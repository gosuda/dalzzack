//! The short form of a result: what an agent reads when stdout is a pipe.
//!
//! It stays one JSON document, and it only leaves out what the common case does
//! not need. A failure is never shortened, and `--full`, `--field`, and `--jq`
//! always read the complete result.

use serde_json::{json, Map, Value};

/// A `data:` URL longer than this is shortened in the output.
const LONG_DATA_URL: usize = 200;

/// The short form of `result`, from the command `tool` called with `args`.
pub fn brief(tool: &str, args: &Value, result: &Value) -> Value {
    if result["status"] == "failed" {
        return result.clone();
    }
    clean(match tool {
        "navigate" | "wait_navigation" => plain_document(result),
        "describe" => describe(result),
        "console_logs" | "diag_console_dump" => console_logs(args, result),
        "execute_js" => execute_js(args, result),
        "list" => list(result),
        _ => result.clone(),
    })
}

/// Remove `null` fields and shorten long `data:` URLs. What a script returned
/// in `result` is the agent's own data, so it stays as it is.
fn clean(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, v)| !v.is_null() || key == "result")
                .map(|(key, v)| {
                    let v = match key.as_str() {
                        "result" => v,
                        k if k.contains("url") => shorten_data_url(v),
                        _ => clean(v),
                    };
                    (key, v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(clean).collect()),
        other => other,
    }
}

fn shorten_data_url(value: Value) -> Value {
    match value.as_str() {
        Some(url) if url.starts_with("data:") && url.len() > LONG_DATA_URL => {
            let start: String = url.chars().take(64).collect();
            json!(format!("{start}... [{} chars]", url.len()))
        }
        _ => clean(value),
    }
}

/// One normal page load repeats its URL five times in `document`. Only a load
/// with something to explain keeps the details.
fn plain_document(result: &Value) -> Value {
    let document = &result["document"];
    let plain = document["committed"] == true
        && document["redirects"] == 0
        && document["challenge"].is_null()
        && document["status"].as_i64().is_some_and(|s| s < 400)
        && result["navigations_during_wait"].as_u64().unwrap_or(1) <= 1;
    let mut out = result.clone();
    if plain {
        out["http"] = document["status"].clone();
        if let Some(fields) = out.as_object_mut() {
            fields.remove("document");
            fields.remove("navigations_during_wait");
        }
    }
    out
}

fn text_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

/// Elements as `<id> <tag> <x>,<y> <text>` rows.
fn describe(result: &Value) -> Value {
    let mut out = result.clone();
    let Some(elements) = result["elements"].as_array() else {
        return out;
    };
    let rows: Vec<String> = elements
        .iter()
        .map(|e| {
            let words: Vec<&str> = text_of(e, "text").split_whitespace().collect();
            let row = format!(
                "{} {} {},{} {}",
                e["id"],
                text_of(e, "tag"),
                e["x"],
                e["y"],
                words.join(" ")
            );
            row.trim_end().to_string()
        })
        .collect();
    out["elements"] = json!(rows);

    // A value that a row already shows adds nothing.
    let shown: Vec<&str> = elements.iter().map(|e| text_of(e, "text")).collect();
    if let Some(fields) = out.as_object_mut() {
        fields.remove("elementCount");
        fields.remove("size");
        let empty = match fields.get_mut("formValues").and_then(Value::as_object_mut) {
            Some(values) => {
                values.retain(|_, v| !v.as_str().is_some_and(|s| shown.contains(&s)));
                values.is_empty()
            }
            None => false,
        };
        if empty {
            fields.remove("formValues");
        }
    }
    out
}

/// The URL most lines share. Those lines then do not repeat it.
fn most_common_url(logs: &[Value]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for url in logs.iter().map(|log| text_of(log, "url")) {
        match counts.iter_mut().find(|(known, _)| *known == url) {
            Some((_, count)) => *count += 1,
            None => counts.push((url, 1)),
        }
    }
    let (url, _) = counts.into_iter().max_by_key(|(_, count)| *count)?;
    (!url.is_empty()).then(|| url.to_string())
}

/// `<level> [source] L<line> <message> @<url>`, with `(xN)` for repeated lines.
fn log_row(log: &Value, shared_url: Option<&str>) -> String {
    let mut row = text_of(log, "level").to_string();
    if let Some(source) = log["source"].as_str() {
        row.push_str(&format!(" [{source}]"));
    }
    if let Some(line) = log["line"].as_u64() {
        row.push_str(&format!(" L{line}"));
    }
    row.push(' ');
    row.push_str(text_of(log, "message"));
    match log["url"].as_str() {
        Some(url) if Some(url) != shared_url => row.push_str(&format!(" @{url}")),
        _ => {}
    }
    row
}

fn console_logs(args: &Value, result: &Value) -> Value {
    let mut out = result.clone();
    let Some(logs) = result["logs"].as_array() else {
        return out;
    };
    let shared_url = most_common_url(logs);

    let mut rows: Vec<(String, usize)> = Vec::new();
    for log in logs {
        let row = log_row(log, shared_url.as_deref());
        match rows.iter_mut().find(|(known, _)| *known == row) {
            Some((_, count)) => *count += 1,
            None => rows.push((row, 1)),
        }
    }
    let rows: Vec<String> = rows
        .into_iter()
        .map(|(row, count)| match count {
            1 => row,
            n => format!("{row} (x{n})"),
        })
        .collect();
    out["logs"] = json!(rows);
    if let Some(url) = shared_url {
        out["url"] = json!(url);
    }

    let Some(fields) = out.as_object_mut() else {
        return out;
    };
    fields.remove("buffer_size");
    fields.remove("frame_ids");
    if fields.get("cleared") == Some(&json!(false)) {
        fields.remove("cleared");
    }
    if fields.get("dropped") == Some(&json!(0)) {
        fields.remove("dropped");
    }
    if fields.get("matched") == fields.get("count") {
        fields.remove("matched");
    }
    // These two say whether an empty answer means anything, so they stay where
    // that is in doubt.
    if fields.get("console_transport") == Some(&json!("cdp")) {
        fields.remove("console_transport");
    }
    if args["source"] != "csp" {
        fields.remove("csp_bypassed");
    }
    out
}

/// The script's value. Where it ran matters only when the caller chose the place.
fn execute_js(args: &Value, result: &Value) -> Value {
    let mut out = result.clone();
    // `world` and `target` always arrive, with these default values.
    let chosen = !args["frame"].is_null()
        || !matches!(args["world"].as_str(), None | Some("main"))
        || !matches!(args["target"].as_str(), None | Some("page"));
    if let (false, Some(fields)) = (chosen, out.as_object_mut()) {
        for key in ["context_id", "frame", "frame_id", "frame_origin", "world"] {
            fields.remove(key);
        }
    }
    out
}

/// What an agent reads to pick an instance. A field that is not at its normal
/// value stays, because it explains a problem.
fn list(result: &Value) -> Value {
    let instances: Vec<Value> = result["instances"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|instance| {
            let mut short = Map::new();
            for key in [
                "id",
                "status",
                "engine",
                "version",
                "renderer_ok",
                "renderer_error",
                "current_url",
                "last_used",
                "hint",
            ] {
                short.insert(key.to_string(), instance[key].clone());
            }
            short.insert("profile".into(), instance["profile"]["kind"].clone());
            if instance["recording"] != "never" {
                short.insert("recording".into(), instance["recording"].clone());
            }
            if instance["console_transport"] != "cdp" {
                short.insert(
                    "console_transport".into(),
                    instance["console_transport"].clone(),
                );
            }
            Value::Object(short)
        })
        .collect();
    let mut out = result.clone();
    out["instances"] = json!(instances);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://example.com/some/long/path/to/a/page.html";

    fn navigate_result(redirects: u64, status: u64) -> Value {
        json!({
            "status": "completed",
            "url": URL,
            "title": "A page",
            "challenge": null,
            "navigations_during_wait": 1,
            "document": {
                "challenge": null,
                "committed": true,
                "committed_url": URL,
                "final_url": URL,
                "requested_url": URL,
                "hops": [{ "status": status, "url": URL }],
                "redirects": redirects,
                "status": status,
            },
        })
    }

    fn size(v: &Value) -> usize {
        v.to_string().len()
    }

    #[test]
    fn a_normal_page_load_is_a_few_fields() {
        let full = navigate_result(0, 200);
        let short = brief("navigate", &json!({}), &full);
        assert_eq!(
            short,
            json!({ "status": "completed", "url": URL, "title": "A page", "http": 200 })
        );
        assert!(size(&short) * 4 < size(&full));
    }

    #[test]
    fn a_load_with_something_to_explain_keeps_its_details() {
        for full in [navigate_result(1, 200), navigate_result(0, 403)] {
            let short = brief("navigate", &json!({}), &full);
            assert!(short["document"]["hops"].is_array(), "{short}");
        }
        let mut challenged = navigate_result(0, 200);
        challenged["document"]["challenge"] = json!("cloudflare:challenge");
        assert!(brief("navigate", &json!({}), &challenged)["document"].is_object());
        let mut reloaded = navigate_result(0, 200);
        reloaded["navigations_during_wait"] = json!(2);
        assert!(brief("navigate", &json!({}), &reloaded)["document"].is_object());
        // A download answers 200, and the page on screen does not change.
        let mut download = navigate_result(0, 200);
        download["document"]["committed"] = json!(false);
        assert!(brief("navigate", &json!({}), &download)["document"].is_object());
    }

    #[test]
    fn a_failure_is_never_shortened() {
        let failed = json!({
            "status": "failed",
            "error": "ELEMENT_NOT_FOUND",
            "hint": null,
            "nearby": [{ "selector": "#a", "text": null }],
        });
        for tool in ["navigate", "describe", "execute_js", "click_element"] {
            assert_eq!(brief(tool, &json!({}), &failed), failed);
        }
    }

    #[test]
    fn describe_prints_rows_and_no_repeated_values() {
        let full = json!({
            "status": "completed",
            "file": "D:\\a\\describe.png",
            "size": 144104,
            "elementCount": 2,
            "total": 40,
            "elements": [
                { "id": 0, "tag": "input", "text": "alice", "x": 93, "y": 143 },
                { "id": 1, "tag": "button", "text": "", "x": 262, "y": 143 },
                { "id": 2, "tag": "a", "text": "two\n  lines", "x": 1, "y": 2 },
            ],
            "formValues": { "user": "alice", "hidden_elsewhere": "other" },
        });
        let short = brief("describe", &json!({}), &full);
        assert_eq!(
            short["elements"],
            json!([
                "0 input 93,143 alice",
                "1 button 262,143",
                "2 a 1,2 two lines"
            ])
        );
        assert_eq!(short["formValues"], json!({ "hidden_elsewhere": "other" }));
        assert_eq!(short["total"], 40);
        assert!(short.get("elementCount").is_none());

        let mut none_left = full.clone();
        none_left["formValues"] = json!({ "user": "alice" });
        assert!(brief("describe", &json!({}), &none_left)
            .get("formValues")
            .is_none());
    }

    fn console_result() -> Value {
        json!({
            "status": "completed",
            "count": 4,
            "matched": 4,
            "dropped": 0,
            "cleared": false,
            "buffer_size": 500,
            "console_transport": "cdp",
            "csp_bypassed": true,
            "frame_ids": null,
            "logs": [
                { "level": "log", "line": 7, "message": "hi", "url": URL, "ts": 1, "frame_id": "F", "context_id": 1 },
                { "level": "log", "line": 7, "message": "hi", "url": URL, "ts": 2, "frame_id": "F", "context_id": 1 },
                { "level": "error", "source": "network", "message": "failed", "url": "https://x.test/a.js", "ts": 3 },
                { "level": "warn", "message": "no url" },
            ],
        })
    }

    #[test]
    fn console_rows_share_the_url_and_merge_repeats() {
        let short = brief("console_logs", &json!({}), &console_result());
        assert_eq!(
            short,
            json!({
                "status": "completed",
                "count": 4,
                "url": URL,
                "logs": [
                    "log L7 hi (x2)",
                    "error [network] failed @https://x.test/a.js",
                    "warn no url",
                ],
            })
        );
    }

    #[test]
    fn console_keeps_what_says_an_empty_answer_means_nothing() {
        let mut result = console_result();
        result["console_transport"] = json!("unavailable");
        result["dropped"] = json!(3);
        result["matched"] = json!(9);
        let short = brief("console_logs", &json!({ "source": "csp" }), &result);
        assert_eq!(short["console_transport"], "unavailable");
        assert_eq!(short["csp_bypassed"], true);
        assert_eq!(short["dropped"], 3);
        assert_eq!(short["matched"], 9);
    }

    #[test]
    fn exec_js_prints_the_value_unless_the_place_was_chosen() {
        let full = json!({
            "status": "completed",
            "result": null,
            "context_id": 3,
            "frame": "top",
            "frame_id": "F",
            "frame_origin": "https://example.com",
            "world": "main",
        });
        let defaults = json!({ "frame": null, "world": "main", "target": "page" });
        assert_eq!(
            brief("execute_js", &defaults, &full),
            json!({ "status": "completed", "result": null })
        );
        for chosen in [
            json!({ "frame": "1", "world": "main", "target": "page" }),
            json!({ "frame": null, "world": "isolated", "target": "page" }),
            json!({ "frame": null, "world": "main", "target": "sw" }),
        ] {
            assert_eq!(brief("execute_js", &chosen, &full)["frame_id"], "F");
        }
    }

    #[test]
    fn what_a_script_returned_is_left_alone() {
        let data_url = format!("data:image/png;base64,{}", "A".repeat(500));
        let full = json!({
            "status": "completed",
            "result": { "url": data_url, "empty": null },
        });
        assert_eq!(brief("execute_js", &json!({}), &full), full);
    }

    #[test]
    fn list_keeps_the_fields_an_agent_reads() {
        let full = json!({
            "count": 1,
            "instances": [{
                "id": "b",
                "status": "running",
                "pid": 1,
                "port": 2,
                "engine": "webview",
                "version": "1.1.0",
                "renderer_ok": true,
                "renderer_error": null,
                "current_url": URL,
                "last_used": "0s ago",
                "profile": { "kind": "shared", "path": "C:\\p" },
                "recording": "never",
                "console_transport": "cdp",
                "csp_bypassed": true,
            }],
        });
        assert_eq!(
            brief("list", &json!({}), &full)["instances"][0],
            json!({
                "id": "b",
                "status": "running",
                "engine": "webview",
                "version": "1.1.0",
                "renderer_ok": true,
                "current_url": URL,
                "last_used": "0s ago",
                "profile": "shared",
            })
        );

        let mut problem = full.clone();
        problem["instances"][0]["recording"] = json!("on");
        problem["instances"][0]["console_transport"] = json!("unavailable");
        let short = brief("list", &json!({}), &problem);
        assert_eq!(short["instances"][0]["recording"], "on");
        assert_eq!(short["instances"][0]["console_transport"], "unavailable");
    }

    #[test]
    fn long_data_urls_are_shortened_and_nulls_removed_everywhere() {
        let long = format!("data:text/html;base64,{}", "A".repeat(37_000));
        let full = json!({
            "status": "completed",
            "url": long,
            "before_url": "data:text/html,short",
            "nested": { "current_url": long, "gone": null },
            "items": [{ "gone": null, "kept": 1 }],
            "text": long,
        });
        let short = brief("get_current_url", &json!({}), &full);
        let url = short["url"].as_str().unwrap();
        assert!(url.len() < 120 && url.ends_with(&format!("[{} chars]", long.len())));
        assert_eq!(short["before_url"], "data:text/html,short");
        assert_eq!(short["nested"], json!({ "current_url": url }));
        assert_eq!(short["items"], json!([{ "kept": 1 }]));
        // Only URL fields: `text` can be anything.
        assert_eq!(short["text"], json!(long));
    }
}
