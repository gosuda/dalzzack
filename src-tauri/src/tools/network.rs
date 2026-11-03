//! Tools over the always-on CDP network stream: the navigation timeline
//! (`nav-log`), `failed-requests`, and turning the `--record` tape on or off
//! on a live daemon (`record on|off|status`).
//!
//! All three are daemon-memory reads or switches — none of them calls into the
//! renderer, so they answer while the page is wedged.

use anyhow::Result;
use serde_json::{json, Value};

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// "Is there any stream feeding this at all?" — an empty timeline from a
/// capture that never came up must not read as "nothing navigated".
fn unarmed_payload(tool: &str) -> Value {
    json!({
        "status": "failed",
        "error": "NETWORK_CAPTURE_UNAVAILABLE",
        "message": format!(
            "`{tool}` reads CDP Network/Page events, and no event stream is armed for this daemon. \
             That needs CDP: WebView2 on Windows, or `start --engine chrome` anywhere. On this \
             daemon nothing has been recorded — an empty answer would mean nothing at all."
        ),
        "engine": crate::engine::current().as_str(),
    })
}

/// `nav-log` — every document request chain, with each redirect hop's status
/// and the headers that explain it (`location`, `cf-mitigated`, …).
pub async fn tool_navigation_log(args: &Value) -> Result<Value> {
    if !crate::netlog::is_armed() {
        return Ok(unarmed_payload("nav-log"));
    }
    let q = crate::netlog::NavQuery {
        after_seq: None,
        since_ms: args
            .get("since_ms")
            .and_then(|v| v.as_u64())
            .map(since_to_epoch),
        include_frames: arg_bool(args, "include_frames"),
        url_pattern: arg_str(args, "url_pattern").map(String::from),
        max: args
            .get("max")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .clamp(1, 200) as usize,
        with_headers: arg_bool(args, "headers"),
        redact: !arg_bool(args, "no_redact"),
    };
    let navs = crate::netlog::navigations(&q);
    let (current_url, current_url_ms) = crate::netlog::current_url();
    let challenges: Vec<&Value> = navs.iter().filter(|n| !n["challenge"].is_null()).collect();
    let hint = if navs.is_empty() {
        Some(
            "No document requests matched. The timeline starts when the daemon starts; navigate \
             first. Iframe documents are hidden unless --frames is given.",
        )
    } else if !challenges.is_empty() {
        Some(
            "A bot challenge was served (see `challenge`). A challenge that solves itself shows up \
             as a later navigation to the same URL — compare `status` across entries before \
             concluding the site let you through on the first request.",
        )
    } else {
        None
    };
    Ok(json!({
        "status": "completed",
        "engine": crate::engine::current().as_str(),
        "current_url": current_url,
        "current_url_ms": current_url_ms,
        "returned": navs.len(),
        "headers_included": q.with_headers,
        "redacted": q.redact,
        "navigations": navs,
        "hint": hint,
    }))
}

/// `--since-ms` is "this many ms ago" on the network tools (same as
/// `network-log`); the store wants an epoch.
fn since_to_epoch(ago_ms: u64) -> u64 {
    crate::recording::now_ms().saturating_sub(ago_ms)
}

/// `failed-requests` — network failures of every resource type (scripts,
/// images, documents, fetch, …) with their URLs, plus HTTP errors.
pub async fn tool_failed_requests(args: &Value) -> Result<Value> {
    if !crate::netlog::is_armed() {
        return Ok(unarmed_payload("failed-requests"));
    }
    let q = crate::netlog::FailQuery {
        since_ms: args
            .get("since_ms")
            .and_then(|v| v.as_u64())
            .map(since_to_epoch),
        url_pattern: arg_str(args, "url_pattern").map(String::from),
        resource_type: arg_str(args, "resource_type").map(String::from),
        http_errors: !arg_bool(args, "no_http_errors"),
        include_canceled: arg_bool(args, "include_canceled"),
        max: args
            .get("max")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 1000) as usize,
        with_headers: arg_bool(args, "headers"),
        redact: !arg_bool(args, "no_redact"),
    };
    let (failures, summary) = crate::netlog::failed_requests(&q);
    let canceled_hidden = summary["canceled_hidden"].as_u64().unwrap_or(0);
    let hint = if failures.is_empty() && canceled_hidden > 0 {
        Some(format!(
            "{canceled_hidden} canceled request(s) hidden (net::ERR_ABORTED — usually navigating \
             away or an ad slot giving up). Pass --include-canceled to see them."
        ))
    } else if failures.is_empty() {
        Some(
            "Nothing failed among the indexed requests. Indexing starts when the daemon starts; \
             `requests_indexed` says how much was looked at."
                .to_string(),
        )
    } else {
        None
    };
    Ok(json!({
        "status": "completed",
        "engine": crate::engine::current().as_str(),
        "returned": failures.len(),
        "summary": summary,
        "failures": failures,
        "hint": hint,
    }))
}

/// `record on|off|status` — the `--record` tape, switchable on a live daemon.
pub async fn tool_record(args: &Value) -> Result<Value> {
    let action = arg_str(args, "action").unwrap_or("status");
    match action {
        "status" => Ok(json!({
            "status": "completed",
            "recording": crate::recording::status(),
        })),
        "on" => {
            if crate::recording::is_enabled() {
                return Ok(json!({
                    "status": "completed",
                    "already_on": true,
                    "recording": crate::recording::status(),
                }));
            }
            if let Err(e) = arm_tape().await {
                crate::recording::disable(false);
                return Ok(json!({
                    "status": "failed",
                    "error": "RECORDING_ARM_FAILED",
                    "message": e,
                    "recording": crate::recording::status(),
                }));
            }
            let status = crate::recording::status();
            let resumed = status["off_periods"]
                .as_array()
                .is_some_and(|p| !p.is_empty());
            Ok(json!({
                "status": "completed",
                "recording": status,
                "note": if resumed {
                    "The tape resumes now. It still holds what was captured while it was on \
                     before; nothing from the `off_periods` gaps is on it. `nav-log`, \
                     `failed-requests`, `response-headers` and `console-logs` are always on and \
                     do not need this."
                } else {
                    "The tape starts now: events from before `started_at_ms` are not on it. \
                     `nav-log`, `failed-requests`, `response-headers` and `console-logs` are \
                     always on and do not need this."
                },
            }))
        }
        "off" => {
            let was_on = crate::recording::is_enabled();
            crate::recording::disable(arg_bool(args, "clear"));
            Ok(json!({
                "status": "completed",
                "was_on": was_on,
                "cleared": arg_bool(args, "clear"),
                "recording": crate::recording::status(),
                "note": "`recording.recording` is now `off`. The tape is kept (unless --clear) and \
                         `dump-recording` still reads it; nothing after `stopped_at_ms` is added. \
                         `armed` stays true — it says the capture transport is up, not that the \
                         tape is running — so `record on` resumes at once.",
            }))
        }
        other => Ok(json!({
            "status": "failed",
            "error": "INVALID_ACTION",
            "message": format!("unknown record action '{other}' — use on, off or status"),
        })),
    }
}

/// Turn the tape on for whichever engine this daemon drives.
async fn arm_tape() -> Result<(), String> {
    if crate::engine::is_chrome() {
        // The Chrome engine's event stream is always flowing (it is what feeds
        // the timeline), so the tape only needs its switch.
        crate::recording::enable_by("cdp", "record on");
        crate::recording::mark_armed(&crate::chrome::tape_events());
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        crate::recording::enable_by("cdp", "record on");
        crate::recording::cdp::arm_tape_live().await.map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        crate::recording::enable_by("in_page_poll", "record on");
        crate::recording::poll::ensure_running();
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Err("no recording transport on this platform (needs CDP or the macOS poller)".to_string())
    }
}

/// What `navigate` saw of the main document between `after_seq` and now.
///
/// The point is the hop sequence: a navigation that was answered with a
/// challenge and then replaced by a second navigation used to come back as a
/// plain `completed` with the final page's URL, which made "the site let us
/// through" indistinguishable from "the site challenged us and a script got
/// past it".
/// `requested` is the URL `navigate` was given: the chain that requested it is
/// "the document", even when another chain — set off by an earlier click whose
/// request was processed late — slipped in after `after_seq` ahead of it.
pub fn navigation_report(after_seq: u64, requested: Option<&str>) -> Value {
    if !crate::netlog::is_armed() {
        return json!({
            "navigation_log": "unavailable — no CDP network stream on this daemon",
        });
    }
    // Every chain since `after_seq`, not the newest few: with more than a
    // handful of navigations during the wait, the newest N would not contain
    // the one `navigate` started.
    let all = crate::netlog::navigations(&crate::netlog::NavQuery {
        after_seq: Some(after_seq),
        max: 200,
        redact: true,
        ..Default::default()
    });
    let normalize = |u: &str| {
        url::Url::parse(u)
            .map(|p| p.to_string())
            .unwrap_or_else(|_| u.to_string())
    };
    let start = requested
        .map(normalize)
        .and_then(|want| {
            all.iter().position(|n| {
                n["requested_url"]
                    .as_str()
                    .is_some_and(|r| normalize(r) == want)
            })
        })
        .unwrap_or(0);
    let navs: Vec<Value> = all.into_iter().skip(start).collect();
    let Some(first) = navs.first().cloned() else {
        return json!({
            "document": Value::Null,
            "hint": "no main-frame document request was observed during the wait (a data:/about: \
                     URL, a same-document navigation, or nothing has been sent yet)",
        });
    };
    let challenge = navs
        .iter()
        .find_map(|n| n["challenge"].as_str().map(String::from));
    let last = navs.last().cloned().unwrap_or(Value::Null);

    let mut hints: Vec<String> = Vec::new();
    if let Some(c) = &challenge {
        if navs.len() > 1 {
            hints.push(format!(
                "a bot challenge ({c}) was served, then {} more main-frame navigation(s) \
                 followed — the page now shown is from the last one (status {}), not from the \
                 first request",
                navs.len() - 1,
                last["status"]
            ));
        } else {
            hints.push(format!(
                "a bot challenge ({c}) was served and nothing has replaced it yet — a challenge \
                 that solves itself navigates again; `nav-log` will show it"
            ));
        }
    } else if navs.len() > 1 {
        hints.push(format!(
            "{} main-frame navigations happened during the wait (client-side redirect or \
             reload); the page now shown is from the last one",
            navs.len()
        ));
    }
    match first["outcome"].as_str() {
        Some("pending") => hints.push(
            "the main document had not been answered when the wait ended — `nav-log` shows it \
             once it lands"
                .into(),
        ),
        Some("failed") => hints.push(format!(
            "the main document request failed: {}",
            first["error_text"].as_str().unwrap_or("unknown error")
        )),
        _ => {}
    }
    if let Some(st) = first["status"].as_i64() {
        if st >= 400 && challenge.is_none() {
            hints.push(format!("the main document returned HTTP {st}"));
        }
    }

    let mut out = json!({
        "document": first,
        "navigations_during_wait": navs.len(),
        "challenge": challenge,
    });
    if navs.len() > 1 {
        // The newest ten are what explains the page now shown; the full list
        // is in `nav-log`.
        let later = &navs[1..];
        let keep = later.len().saturating_sub(10);
        out["later_navigations"] = json!(later[keep..].to_vec());
        if keep > 0 {
            out["later_navigations_omitted"] = json!(keep);
        }
    }
    if !hints.is_empty() {
        out["hint"] = json!(hints.join("; "));
    }
    out
}
