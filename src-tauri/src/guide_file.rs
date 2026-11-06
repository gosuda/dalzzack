//! The dalzzack block in an agent's instruction file. `install` writes it,
//! `upgrade` refreshes it, and `uninstall` removes it.

use crate::state::home_dir;
use std::path::PathBuf;

pub const BLOCK_BEGIN: &str = "<!-- dalzzack-begin -->";
pub const BLOCK_END: &str = "<!-- dalzzack-end -->";

pub const DALZZACK_DOCS: &str = concat!(
    "<!-- dalzzack-begin -->\n",
    include_str!("../guide/core.md"),
    "<!-- dalzzack-end -->\n"
);

/// Where Codex keeps its global files: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

/// The instruction files that apply to all projects.
pub fn global_files() -> [PathBuf; 2] {
    [
        home_dir().join(".claude").join("CLAUDE.md"),
        codex_home().join("AGENTS.md"),
    ]
}

/// The text before and after the dalzzack block; `None` when there is no block.
///
/// Anything but one begin marker before one end marker is an error: without an
/// end marker, the text after the begin marker is the user's own.
pub fn split_around_block(text: &str) -> Result<Option<(&str, &str)>, String> {
    let (begins, ends) = (
        text.matches(BLOCK_BEGIN).count(),
        text.matches(BLOCK_END).count(),
    );
    if begins == 0 && ends == 0 {
        return Ok(None);
    }
    match (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) {
        (Some(begin), Some(end)) if begins == 1 && ends == 1 && begin < end => {
            Ok(Some((&text[..begin], &text[end + BLOCK_END.len()..])))
        }
        _ => Err(format!(
            "expected one {BLOCK_BEGIN} before one {BLOCK_END}, found {begins} and {ends}. \
             Fix the markers by hand. The file was not changed."
        )),
    }
}

/// `existing` with its block replaced, or with a block added at the end.
pub fn with_docs(existing: &str) -> Result<String, String> {
    Ok(match split_around_block(existing)? {
        Some((before, after)) => match after.trim_start_matches(['\r', '\n']) {
            "" => format!("{before}{DALZZACK_DOCS}"),
            after => format!("{before}{DALZZACK_DOCS}\n{after}"),
        },
        None => match existing.trim_end() {
            "" => DALZZACK_DOCS.to_string(),
            kept => format!("{kept}\n\n{DALZZACK_DOCS}"),
        },
    })
}

/// `existing` with its block removed, or `None` when it has no block. The result
/// is empty when the file holds nothing else.
pub fn without_docs(existing: &str) -> Result<Option<String>, String> {
    let Some((before, after)) = split_around_block(existing)? else {
        return Ok(None);
    };
    let eol = if before.contains("\r\n") || after.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let before = before.trim_end();
    let after = after.trim_start_matches(['\r', '\n']);
    Ok(Some(match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => after.to_string(),
        (false, true) => format!("{before}{eol}"),
        (false, false) => format!("{before}{eol}{eol}{after}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guide_replaces_its_own_block_and_leaves_the_rest() {
        let file = format!("# Mine\r\n\r\n{BLOCK_BEGIN}\nOLD GUIDE\n{BLOCK_END}\n\n# Also mine\n");
        let updated = with_docs(&file).unwrap();
        assert!(updated.starts_with("# Mine\r\n\r\n"));
        assert!(updated.ends_with("\n# Also mine\n"));
        assert!(updated.contains(DALZZACK_DOCS));
        assert!(!updated.contains("OLD GUIDE"));
        // Running it again changes nothing.
        assert_eq!(with_docs(&updated).unwrap(), updated);
    }

    #[test]
    fn the_guide_goes_at_the_end_of_a_file_without_a_block() {
        assert_eq!(with_docs("").unwrap(), DALZZACK_DOCS);
        assert_eq!(
            with_docs("# Mine\n\n").unwrap(),
            format!("# Mine\n\n{DALZZACK_DOCS}")
        );
    }

    #[test]
    fn a_broken_block_changes_nothing() {
        for file in [
            format!("{BLOCK_BEGIN}\nmy notes, with no end marker\n"),
            format!("{BLOCK_END}\nthe wrong order\n{BLOCK_BEGIN}\n"),
            format!("{BLOCK_BEGIN}\n{BLOCK_END}\n{BLOCK_BEGIN}\n{BLOCK_END}\n"),
            format!("only an end marker {BLOCK_END}\n"),
        ] {
            assert!(with_docs(&file).is_err(), "{file}");
            assert!(without_docs(&file).is_err(), "{file}");
        }
    }

    #[test]
    fn removing_the_block_restores_what_install_found() {
        for original in ["# Mine\n", "one\ntwo\n", "one\r\ntwo\r\n"] {
            let installed = with_docs(original).unwrap();
            assert_eq!(without_docs(&installed).unwrap().unwrap(), original);
        }
    }

    #[test]
    fn removing_the_block_keeps_the_text_after_it() {
        let file = format!("# Mine\n\n{DALZZACK_DOCS}\n# Also mine\n");
        assert_eq!(
            without_docs(&file).unwrap().unwrap(),
            "# Mine\n\n# Also mine\n"
        );
        let file = format!("{DALZZACK_DOCS}\n# Also mine\n");
        assert_eq!(without_docs(&file).unwrap().unwrap(), "# Also mine\n");
    }

    #[test]
    fn a_file_that_holds_only_the_block_becomes_empty() {
        let installed = with_docs("").unwrap();
        assert_eq!(without_docs(&installed).unwrap().unwrap(), "");
    }

    #[test]
    fn a_file_without_a_block_needs_no_change() {
        assert_eq!(without_docs("# Mine\n").unwrap(), None);
        assert_eq!(without_docs("").unwrap(), None);
    }
}
