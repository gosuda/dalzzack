//! A command line as the window's toolbar shows it.
//!
//! The toolbar puts the command a window is running where a person can read it,
//! so it can end up in a screenshot or a screen recording. Arguments can hold
//! what the agent typed into a page: a password, a token, a card number. Logs
//! already keep the tool name only for that reason. The toolbar shows more,
//! because the name alone does not say *which* element a `click` is waiting on,
//! but it works from an allow-list: a value is shown only for a flag that is
//! known to name a place or a setting. Every other value, and every word after
//! the command name, becomes dots.

const MASK: &str = "••••";
const MAX_VALUE_CHARS: usize = 48;
const MAX_LABEL_CHARS: usize = 160;

/// Flags whose value says where to act or how long to wait. None of them is
/// something the agent typed.
const SHOWN: &[&str] = &[
    "selector",
    "text",
    "url",
    "to",
    "url-pattern",
    "key",
    "state",
    "x",
    "y",
    "index",
    "frame",
    "target",
    "level",
    "source",
    "width",
    "height",
    "timeout-ms",
    "ms",
    "interval-ms",
    "click-type",
    "modifiers",
    "max",
    "method",
    "resource-type",
];

/// A URL can carry a token in its query or fragment, so only up to there shows.
const URL_FLAGS: &[&str] = &["url", "to", "url-pattern"];

/// Output flags and the instance id say nothing about what the window is doing.
/// These take a value, which goes with them.
const DROPPED_WITH_VALUE: &[&str] = &["i", "id", "field", "jq"];
const DROPPED_ALONE: &[&str] = &["raw", "pretty", "full"];

/// `-5` is a value, not a flag.
fn flag_of(word: &str) -> Option<&str> {
    let rest = word.strip_prefix('-')?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let starts_like_a_number = rest.chars().next().is_some_and(|c| c.is_ascii_digit());
    (!rest.is_empty() && !starts_like_a_number).then_some(rest)
}

fn clip(value: &str) -> String {
    if value.chars().count() <= MAX_VALUE_CHARS {
        return value.to_string();
    }
    let head: String = value.chars().take(MAX_VALUE_CHARS - 1).collect();
    format!("{head}…")
}

fn quoted(value: &str) -> String {
    if value.is_empty() || value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn shown_value(flag: &str, value: &str, command: Option<&str>) -> String {
    // `keyboard --text` is what gets typed; everywhere else `--text` is the
    // visible text to look for.
    if !SHOWN.contains(&flag) || (flag == "text" && command == Some("keyboard")) {
        return MASK.to_string();
    }
    if URL_FLAGS.contains(&flag) {
        return match value.find(['?', '#']) {
            Some(cut) => format!("{}?…", clip(&value[..cut])),
            None => quoted(&clip(value)),
        };
    }
    quoted(&clip(value))
}

/// The label for one command line, given as the words after `dalzzack`.
pub fn command_label(words: &[String]) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut command: Option<&str> = None;
    let mut at = 0;
    while at < words.len() {
        let word = words[at].as_str();
        at += 1;

        let Some(flag) = flag_of(word) else {
            // The first plain word is the command; any later one is an argument.
            if command.is_none() {
                command = Some(word);
                shown.push(word.to_string());
            } else {
                shown.push(MASK.to_string());
            }
            continue;
        };

        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        if DROPPED_ALONE.contains(&name) {
            continue;
        }
        let value = match inline {
            Some(value) => Some(value),
            None => match words.get(at) {
                Some(next) if flag_of(next).is_none() => {
                    at += 1;
                    Some(next.as_str())
                }
                _ => None,
            },
        };
        if DROPPED_WITH_VALUE.contains(&name) {
            continue;
        }
        shown.push(format!("--{name}"));
        if let Some(value) = value {
            shown.push(shown_value(name, value, command));
        }
    }

    let label = shown.join(" ");
    if label.chars().count() <= MAX_LABEL_CHARS {
        return label;
    }
    let head: String = label.chars().take(MAX_LABEL_CHARS - 1).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::command_label;

    fn label(line: &str) -> String {
        // A test line has no quoted spaces except where a test builds the words.
        let words: Vec<String> = line.split(' ').map(str::to_string).collect();
        command_label(&words)
    }

    #[test]
    fn what_was_typed_into_a_page_becomes_dots() {
        assert_eq!(
            label("fill-form -i b --selector #email --value hunter2"),
            "fill-form --selector #email --value ••••"
        );
        assert_eq!(
            label("keyboard --text hunter2"),
            "keyboard --text ••••",
            "keyboard --text is what gets typed"
        );
        assert_eq!(
            label("exec-js --script document.cookie"),
            "exec-js --script ••••"
        );
    }

    #[test]
    fn a_locator_is_shown_and_text_with_spaces_is_quoted() {
        let words = ["click", "--text", "Sign In"].map(String::from);
        assert_eq!(command_label(&words), "click --text \"Sign In\"");
        assert_eq!(label("click -i b --selector #pay"), "click --selector #pay");
    }

    #[test]
    fn an_unknown_flag_is_masked_until_someone_allows_it() {
        assert_eq!(
            label("cookies --name sid --value s3cret"),
            "cookies --name •••• --value ••••"
        );
        assert_eq!(label("cookies set sid"), "cookies •••• ••••");
    }

    #[test]
    fn a_url_loses_its_query_and_fragment() {
        assert_eq!(
            label("navigate --url https://x.test/a?token=abc#frag"),
            "navigate --url https://x.test/a?…"
        );
        assert_eq!(
            label("navigate --url=https://x.test/login"),
            "navigate --url https://x.test/login"
        );
    }

    #[test]
    fn the_instance_id_and_output_flags_are_left_out() {
        assert_eq!(
            label("get-text --id=b --jq .text --raw --selector main"),
            "get-text --selector main"
        );
    }

    #[test]
    fn a_negative_number_is_a_value() {
        assert_eq!(label("click --x -5 --y 40"), "click --x -5 --y 40");
    }

    #[test]
    fn a_long_value_is_cut() {
        let long = "a".repeat(200);
        let shown = label(&format!("click --selector {long}"));
        assert!(shown.ends_with('…'));
        assert!(shown.chars().count() < 80);
    }
}
