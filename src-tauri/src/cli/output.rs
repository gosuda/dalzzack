//! `--field` / `--jq` / `--raw`: shape a command's JSON output before it is
//! printed, so a runner can take one value without piping through another
//! tool (and without depending on a `jq` binary being installed).
//!
//! `--jq` is real jq (the `jaq` implementation). `--field` is the common case
//! made safe to type: a dotted path such as `instances[].id` or
//! `document.hops[0].status`, translated to jq with optional-iteration so a
//! missing branch yields `null` rather than an error.

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;
use serde_json::Value;

/// What the user asked for.
#[derive(Debug, Clone, Default)]
pub struct OutputFilter {
    pub fields: Vec<String>,
    pub jq: Option<String>,
    /// Print string results without JSON quotes (`jq -r`).
    pub raw: bool,
}

impl OutputFilter {
    pub fn is_active(&self) -> bool {
        !self.fields.is_empty() || self.jq.is_some()
    }
}

/// Translate a `--field` path into a jq expression.
///
/// `a.b` → `.a.b`, `a[0]` → `.a[0]`, `a[].b` → `.a[]?.b`, `"x-y"` keys via
/// `a["x-y"]`. A leading `.` is accepted and ignored.
pub fn field_to_jq(path: &str) -> String {
    let p = path.trim();
    let p = p.strip_prefix('.').unwrap_or(p);
    if p.is_empty() {
        return ".".to_string();
    }
    let mut out = String::new();
    let mut chars = p.chars().peekable();
    let mut segment = String::new();
    let flush = |segment: &mut String, out: &mut String| {
        if segment.is_empty() {
            return;
        }
        // Plain identifiers can be written bare; anything else (a header name
        // like `cf-mitigated`) needs the quoted form.
        if segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !segment.starts_with(|c: char| c.is_ascii_digit())
        {
            out.push('.');
            out.push_str(segment);
        } else {
            if out.is_empty() {
                out.push('.');
            }
            out.push_str(&format!("[{}]", serde_json::Value::String(segment.clone())));
        }
        segment.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '.' => flush(&mut segment, &mut out),
            '[' => {
                flush(&mut segment, &mut out);
                // A path that STARTS with an index (`[0].name`) indexes the
                // input itself; bare `[0]` in jq is an array literal.
                if out.is_empty() {
                    out.push('.');
                }
                let mut inner = String::new();
                for c2 in chars.by_ref() {
                    if c2 == ']' {
                        break;
                    }
                    inner.push(c2);
                }
                if inner.is_empty() {
                    out.push_str("[]?");
                } else if inner.starts_with('"') || inner.parse::<i64>().is_ok() {
                    out.push_str(&format!("[{inner}]"));
                } else {
                    out.push_str(&format!("[{}]", serde_json::Value::String(inner)));
                }
            }
            _ => segment.push(c),
        }
    }
    flush(&mut segment, &mut out);
    if out.is_empty() {
        ".".to_string()
    } else {
        out
    }
}

/// Run a jq program over one JSON value. Every output becomes one element.
pub fn run_jq(program: &str, input: &Value) -> Result<Vec<Value>, String> {
    let text = serde_json::to_string(input).map_err(|e| e.to_string())?;
    let val = jaq_json::read::parse_single(text.as_bytes())
        .map_err(|e| format!("input is not JSON: {e:?}"))?;

    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: program,
                path: (),
            },
        )
        .map_err(|errs| {
            let detail: Vec<String> = errs.iter().map(|(_, e)| format!("{e:?}")).collect();
            explain_parse_error(program, &detail.join("; "))
        })?;
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| {
            let detail: Vec<String> = errs
                .iter()
                .flat_map(|(_, es)| es.iter().map(|(name, _)| format!("undefined: {name}")))
                .collect();
            format!(
                "cannot compile jq program `{program}`: {}",
                detail.join("; ")
            )
        })?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut out = Vec::new();
    for r in filter.id.run((ctx, val)).map(unwrap_valr) {
        match r {
            Ok(v) => {
                let s = v.to_string();
                out.push(serde_json::from_str(&s).unwrap_or(Value::String(s)));
            }
            Err(e) => return Err(explain_runtime_error(&e.to_string())),
        }
    }
    Ok(out)
}

/// What the regex functions (`test`, `match`, `capture`, `scan`, `split`,
/// `sub`, `gsub`) accept here. jq proper uses Oniguruma; jaq uses Rust's
/// `regex`, which has neither look-around nor backreferences.
pub const REGEX_NOTE: &str = "Regexes use Rust `regex` syntax: no look-around (`(?=…)`, \
                              `(?!…)`, `(?<=…)`) and no backreferences (`\\1`). Named groups \
                              `(?<name>…)` and flags (`\"g\"`, `\"i\"`, `\"x\"`) work.";

/// The parser's own message (`Lex([(Escape, "d)\")])`) names a token, not the
/// mistake. The common one is a regex backslash: jq strings have no `\d`.
fn explain_parse_error(program: &str, detail: &str) -> String {
    if detail.contains("Escape") {
        return format!(
            "cannot parse jq program `{program}`: a string contains a backslash escape jq does \
             not have (`\\d`, `\\s`, `\\w`, `\\1`…). In a jq string a regex backslash is \
             written twice — `test(\"\\\\d+\")` — and shells and some agent tools halve \
             backslashes before jq sees them, so check what arrived above. (parser: {detail})"
        );
    }
    format!("cannot parse jq program `{program}`: {detail}")
}

fn explain_runtime_error(e: &str) -> String {
    if e.contains("regex") {
        return format!("jq error: {e}. {REGEX_NOTE}");
    }
    format!("jq error: {e}")
}

/// Keys the program reads that the output does not have, but a differently
/// spelled one does — `httpOnly` asked of output that says `http_only`.
///
/// A missing key is `null`, not an error, in jq; that is how "every cookie has
/// `httpOnly: null`" was once reported as a bug. Only near-misses are named
/// (same letters ignoring case, `_` and `-`), so an intentional absent key
/// never draws a warning.
pub fn missing_key_hints(program: &str, v: &Value) -> Vec<String> {
    let mut keys = std::collections::BTreeSet::new();
    collect_keys(v, &mut keys);
    let norm = |s: &str| {
        s.chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let mut out = Vec::new();
    for ident in program_keys(program) {
        if keys.contains(ident.as_str()) {
            continue;
        }
        let want = norm(&ident);
        if let Some(k) = keys.iter().find(|k| norm(k) == want) {
            let hint = format!(
                "no key \"{ident}\" in this output — did you mean \"{k}\"? (a missing key reads \
                 as null; dalzzack's fields are snake_case, CDP's are camelCase)"
            );
            if !out.contains(&hint) {
                out.push(hint);
            }
        }
    }
    out
}

fn collect_keys<'a>(v: &'a Value, keys: &mut std::collections::BTreeSet<&'a str>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                keys.insert(k.as_str());
                collect_keys(x, keys);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_keys(x, keys)),
        _ => {}
    }
}

/// Object keys a jq program reads: `.name` accesses and `{name, …}`
/// shorthand. String literals and `$variables` are skipped.
fn program_keys(program: &str) -> Vec<String> {
    let chars: Vec<char> = program.chars().collect();
    let ident_start = |c: char| c.is_ascii_alphabetic() || c == '_';
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut i = 0;
    // `{` opens an object construction; `,` and `{` precede a shorthand key.
    let mut brace_depth = 0usize;
    let mut prev_sig = ' ';
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            // Skip the string, escapes included.
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            prev_sig = '"';
            continue;
        }
        if c == '{' {
            brace_depth += 1;
        } else if c == '}' {
            brace_depth = brace_depth.saturating_sub(1);
        }
        if ident_start(c)
            && (i == 0 || !ident_char(chars[i - 1]))
            && chars.get(i.wrapping_sub(1)) != Some(&'$')
        {
            let start = i;
            while i < chars.len() && ident_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let after = chars[i..].iter().find(|c| !c.is_whitespace()).copied();
            let dotted = start > 0 && chars[start - 1] == '.';
            let shorthand = brace_depth > 0
                && matches!(prev_sig, '{' | ',')
                && matches!(after, Some(',') | Some('}'));
            if dotted || shorthand {
                out.push(word);
            }
            prev_sig = 'a';
            continue;
        }
        if !c.is_whitespace() {
            prev_sig = c;
        }
        i += 1;
    }
    out
}

/// Apply the filter and render the text to print.
///
/// * `--field P` alone: the value at `P` (an array if the path fans out).
/// * several `--field`: an object `{P: value, …}`.
/// * `--jq E`: every output, one document per line, like jq.
pub fn render(v: &Value, f: &OutputFilter) -> Result<String, String> {
    if let Some(program) = &f.jq {
        let outs = run_jq(program, v)?;
        return Ok(outs
            .iter()
            .map(|o| show(o, f.raw))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    if f.fields.is_empty() {
        return Ok(show(v, f.raw));
    }
    let pick = |path: &str| -> Result<Value, String> {
        let outs = run_jq(&field_to_jq(path), v)?;
        Ok(match outs.len() {
            0 => Value::Null,
            1 => outs.into_iter().next().unwrap_or(Value::Null),
            _ => Value::Array(outs),
        })
    };
    if f.fields.len() == 1 {
        return Ok(show(&pick(&f.fields[0])?, f.raw));
    }
    let mut obj = serde_json::Map::new();
    for p in &f.fields {
        obj.insert(p.clone(), pick(p)?);
    }
    Ok(show(&Value::Object(obj), f.raw))
}

fn show(v: &Value, raw: bool) -> String {
    match v {
        Value::String(s) if raw => s.clone(),
        // A fanned-out list of strings under --raw: one per line, like `jq -r .[]`.
        Value::Array(items) if raw && items.iter().all(|i| i.is_string()) => items
            .iter()
            .filter_map(|i| i.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_paths_translate() {
        assert_eq!(field_to_jq("status"), ".status");
        assert_eq!(field_to_jq(".status"), ".status");
        assert_eq!(
            field_to_jq("document.hops[0].status"),
            ".document.hops[0].status"
        );
        assert_eq!(field_to_jq("instances[].id"), ".instances[]?.id");
        assert_eq!(
            field_to_jq("hops[0].key_headers.cf-mitigated"),
            ".hops[0].key_headers[\"cf-mitigated\"]"
        );
        assert_eq!(field_to_jq("x-y"), ".[\"x-y\"]");
        assert_eq!(field_to_jq("[0].name"), ".[0].name");
        assert_eq!(field_to_jq("[].name"), ".[]?.name");
        let tools = json!([{ "name": "navigate" }, { "name": "get_html" }]);
        assert_eq!(
            run_jq(&field_to_jq("[0].name"), &tools).unwrap(),
            vec![json!("navigate")]
        );
        assert_eq!(field_to_jq(""), ".");
    }

    #[test]
    fn fields_pick_values() {
        let v = json!({
            "status": "completed",
            "instances": [{ "id": "a", "port": 1 }, { "id": "b", "port": 2 }],
            "doc": { "hops": [{ "status": 302 }, { "status": 200, "key_headers": { "cf-mitigated": "challenge" } }] },
        });
        let one = |p: &str| {
            render(
                &v,
                &OutputFilter {
                    fields: vec![p.into()],
                    raw: true,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        assert_eq!(one("status"), "completed");
        assert_eq!(one("instances[].id"), "a\nb");
        assert_eq!(one("doc.hops[1].status"), "200");
        assert_eq!(one("doc.hops[1].key_headers.cf-mitigated"), "challenge");
        // Missing is null, not an error.
        assert_eq!(one("nope.deeper"), "null");
        assert_eq!(one("nope[].x"), "null");

        let many = render(
            &v,
            &OutputFilter {
                fields: vec!["status".into(), "instances[].port".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let back: Value = serde_json::from_str(&many).unwrap();
        assert_eq!(back["status"], "completed");
        assert_eq!(back["instances[].port"], json!([1, 2]));
    }

    #[test]
    fn jq_is_real_jq() {
        let v = json!({ "a": [1, 2, 3], "s": "x" });
        assert_eq!(
            run_jq(".a | map(. * 2)", &v).unwrap(),
            vec![json!([2, 4, 6])]
        );
        assert_eq!(
            run_jq(".a[] | select(. > 1)", &v).unwrap(),
            vec![json!(2), json!(3)]
        );
        assert_eq!(run_jq("keys", &v).unwrap(), vec![json!(["a", "s"])]);
        assert_eq!(run_jq(".a | length", &v).unwrap(), vec![json!(3)]);
        assert!(run_jq(".a |||", &v).is_err());
        let out = render(
            &v,
            &OutputFilter {
                jq: Some(".s".into()),
                raw: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out, "x");
    }

    #[test]
    fn a_misspelled_key_is_named_not_silently_null() {
        let v = json!({ "cookies": [{ "name": "sid", "http_only": true, "same_site": "Lax" }] });
        // The jq shorthand and the field path that reported `httpOnly: null`.
        let h = missing_key_hints(".cookies[] | {name, httpOnly}", &v);
        assert_eq!(h.len(), 1, "{h:?}");
        assert!(h[0].contains("\"httpOnly\"") && h[0].contains("\"http_only\""));
        assert_eq!(
            missing_key_hints(&field_to_jq("cookies[].sameSite"), &v).len(),
            1
        );
        // Keys that exist, keys with no near match, strings and variables: silent.
        assert!(missing_key_hints(".cookies[] | {name, http_only}", &v).is_empty());
        assert!(missing_key_hints(".cookies[].expires", &v).is_empty());
        assert!(missing_key_hints(".cookies[] | select(.name == \"httpOnly\")", &v).is_empty());
        assert!(missing_key_hints(". as $httpOnly | .cookies", &v).is_empty());
        // `{a: .b}` reads `.b`, not `a`.
        assert_eq!(missing_key_hints("{httpOnly: .cookies}", &v).len(), 0);
    }

    #[test]
    fn regex_and_escape_errors_say_what_to_do() {
        let v = json!("a1");
        // `\d` inside a jq string: the parser's own message names a token.
        let e = run_jq("test(\"\\d\")", &v).unwrap_err();
        assert!(e.contains("written twice"), "{e}");
        // The doubled form works.
        assert_eq!(run_jq("test(\"\\\\d\")", &v).unwrap(), vec![json!(true)]);
        let e = run_jq("test(\"a(?=1)\")", &v).unwrap_err();
        assert!(e.contains("no look-around"), "{e}");
        assert_eq!(
            run_jq("capture(\"(?<d>[0-9])\")", &v).unwrap(),
            vec![json!({ "d": "1" })]
        );
    }
}
