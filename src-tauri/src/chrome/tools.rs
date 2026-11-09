//! Tools the Chrome engine implements itself.
//!
//! Everything that is plain page JavaScript (`get-html`, `wait-for`,
//! `fill-form`, `scroll`, `describe`, …) runs through the shared
//! implementation unchanged — `webview_execute_js` routes to Chrome's
//! `Runtime.evaluate` on this engine. What lives here is what the WebView path
//! does through WebView2 COM or OS input, and what real Chrome does better:
//!
//! * input (`click`, `hover`, `keyboard`) goes through `Input.dispatch*Event`,
//!   so events are **trusted** (`isTrusted: true`) and CSS `:hover` fires —
//!   the WebView path dispatches synthetic JS events;
//! * `navigate` waits for the top frame's `load`, not a fixed sleep;
//! * frames come from `Page.getFrameTree`, and out-of-process iframes are
//!   reached through their own auto-attached sessions.
//!
//! Tools that are built on WebView2 internals (minidumps, the WebView2 event
//! plumbing behind profiling/tracing/debugger) answer `UNSUPPORTED_ON_ENGINE`
//! instead of a misleading WebView error.

use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

/// `Some(result)` when this engine handles the tool itself, `None` to fall
/// through to the shared implementation.
pub async fn dispatch(name: &str, args: &Value) -> Option<Result<Value>> {
    let r = match name {
        "navigate" => navigate(args).await,
        // A service worker target is resolved by the shared tool, which already
        // routes to this engine's sessions.
        "execute_js"
            if args
                .get("target")
                .and_then(|t| t.as_str())
                .unwrap_or("page")
                == "page" =>
        {
            exec_js(args).await
        }
        "click" => click_at(args).await,
        "click_element" => click_element(args).await,
        "hover" => hover(args).await,
        "keyboard_input" => keyboard(args).await,
        "resize_window" => resize(args).await,
        "get_frame_list" => frame_list(args).await,
        "get_frame_html" => frame_html(args).await,
        "get_captured_requests" => captured_requests(args).await,
        "diag_network_log" => network_log(args).await,
        "diag_attach_devtools" => Ok(attach_devtools()),
        "diag_pause"
        | "diag_profile_start"
        | "diag_profile_stop"
        | "diag_heap_snapshot"
        | "diag_trace"
        | "diag_debugger_arm"
        | "diag_debugger_snapshot"
        | "diag_debugger_disarm"
        | "diag_exec_js_isolated" => Ok(unsupported(name)),
        _ => return None,
    };
    Some(r)
}

fn unsupported(name: &str) -> Value {
    json!({
        "status": "failed",
        "error": "UNSUPPORTED_ON_ENGINE",
        "engine": "chrome",
        "message": format!(
            "`{name}` is built on WebView2 internals (process minidumps / WebView2 event \
             plumbing) and is not available on --engine chrome"
        ),
        "hint": "This is a real Chrome: open DevTools in its window (F12) for profiling, \
                 tracing and the debugger, or use `exec-js --world isolated` for out-of-page \
                 evaluation.",
    })
}

fn attach_devtools() -> Value {
    let port = super::current().map(|b| b.port);
    json!({
        "status": "completed",
        "engine": "chrome",
        "message": "This daemon drives a real Chrome window — press F12 in it to open DevTools.",
        "devtools_targets": port.map(|p| format!("http://127.0.0.1:{p}/json")),
    })
}

fn arg_str<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// navigate
// ---------------------------------------------------------------------------

async fn navigate(args: &Value) -> Result<Value> {
    let Some(url) = arg_str(args, "url") else {
        return Ok(
            json!({ "status": "failed", "error": "URL_REQUIRED", "message": "url required" }),
        );
    };
    if let Err(e) = url.parse::<url::Url>() {
        return Ok(json!({
            "status": "failed",
            "error": "NAVIGATION_FAILED",
            "message": format!("Invalid URL '{url}': {e}"),
        }));
    }
    let timeout = Duration::from_millis(args["timeout_ms"].as_u64().unwrap_or(5000));
    let seq_before = crate::netlog::last_seq();
    let since = crate::recording::now_ms();

    // `Page.navigate` answers only once the response has arrived and committed
    // (or failed). A server that never answers would hold it far past
    // `--timeout-ms`, so the same budget applies — and running out is not a
    // failure: the navigation is still in flight, and the report says so.
    let budget = timeout.max(Duration::from_secs(3));
    let nav = match super::page_call("Page.navigate", json!({ "url": url }), budget).await {
        Ok(v) => v,
        Err(e) if e.contains("timed out") => {
            let mut out = json!({
                "status": "completed",
                "url": url,
                "engine": "chrome",
                "load_note": format!(
                    "the navigation had not committed within {}ms — the server has not answered \
                     yet; `nav-log` shows it once it does",
                    budget.as_millis()
                ),
            });
            merge(
                &mut out,
                crate::tools::network::navigation_report(seq_before, Some(url)),
            );
            return Ok(out);
        }
        Err(e) => {
            return Ok(json!({ "status": "failed", "error": "NAVIGATION_FAILED", "message": e }));
        }
    };
    if let Some(err) = nav
        .get("errorText")
        .and_then(|e| e.as_str())
        .filter(|s| !s.is_empty())
    {
        crate::main_frame::consume();
        return Ok(crate::tools::definitions::failed_navigation(
            url,
            &json!({ "failing_url": url, "error_text": err, "message": err }),
            &crate::tools::network::navigation_report(seq_before, Some(url)),
        ));
    }
    // No loader id = a same-document navigation (fragment / history): nothing
    // will "load".
    let loader = nav
        .get("loaderId")
        .and_then(|l| l.as_str())
        .map(String::from);
    let loaded_at = if loader.is_some() {
        super::wait_for_load(loader.as_deref(), since, timeout).await
    } else {
        None
    };

    let (final_url, title) = match super::eval_page(
        "return JSON.stringify({u: location.href, t: document.title})",
        Some(3000),
        false,
    )
    .await
    {
        Ok(Value::String(s)) => {
            let p: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
            (
                p["u"].as_str().unwrap_or(url).to_string(),
                p["t"].as_str().map(String::from),
            )
        }
        _ => (url.to_string(), None),
    };
    {
        let mut current = crate::CURRENT_SITE.write().await;
        *current = arg_str(args, "site").map(String::from);
    }

    let mut out = json!({
        "status": "completed",
        "url": final_url,
        "title": title,
        "engine": "chrome",
        "load_event_ms": loaded_at.map(|t| t.saturating_sub(since)),
    });
    if loader.is_some() && loaded_at.is_none() {
        out["load_note"] = json!(format!(
            "the top frame's load event did not fire within {}ms — the page may still be loading",
            timeout.as_millis()
        ));
    }
    merge(
        &mut out,
        crate::tools::network::navigation_report(seq_before, Some(url)),
    );
    // Whatever failed during this call is this call's to report.
    crate::main_frame::consume();
    Ok(out)
}

fn merge(out: &mut Value, extra: Value) {
    if let (Some(o), Some(e)) = (out.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// frames
// ---------------------------------------------------------------------------

/// Every frame below the top document — the index space shared by
/// `get-frames`, `--frame N` and `get-frame-html --index N`.
///
/// Indexes follow the top document's `<iframe>`/`<frame>` elements in DOM
/// order, exactly as on the WebView engine, so `--frame 2` means the same
/// element on both. Each element is mapped to its frame id with
/// `DOM.describeNode` — the only route that also reaches cross-site iframes:
/// Chrome runs those out of process and leaves them out of the parent's
/// `Page.getFrameTree` altogether (measured on Chrome 154). Nested frames, which
/// have no element in the top document, are appended after.
async fn frames() -> Result<Vec<Value>, String> {
    let t = Duration::from_secs(10);
    let tree = super::page_call("Page.getFrameTree", json!({}), t).await?;
    let mut by_id: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut tree_order: Vec<String> = Vec::new();
    fn walk(
        node: &Value,
        by_id: &mut std::collections::HashMap<String, Value>,
        order: &mut Vec<String>,
    ) {
        if let Some(children) = node.get("childFrames").and_then(|c| c.as_array()) {
            for c in children {
                let f = c["frame"].clone();
                if let Some(id) = f["id"].as_str() {
                    order.push(id.to_string());
                    by_id.insert(id.to_string(), f.clone());
                }
                walk(c, by_id, order);
            }
        }
    }
    walk(&tree["frameTree"], &mut by_id, &mut tree_order);
    // Out-of-process iframes: known only through their own targets.
    let oopifs: Vec<(String, String)> = super::CHILDREN
        .lock()
        .ok()
        .and_then(|c| {
            c.as_ref().map(|m| {
                m.values()
                    .filter(|(_, ty, _)| ty == "iframe")
                    .map(|(tid, _, url)| (tid.clone(), url.clone()))
                    .collect()
            })
        })
        .unwrap_or_default();

    let count = match super::eval_page(
        "return document.querySelectorAll('iframe,frame').length",
        Some(5000),
        false,
    )
    .await
    {
        Ok(v) => v.as_u64().unwrap_or(0),
        Err(_) => 0,
    };

    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 0..count {
        let el = super::page_call(
            "Runtime.evaluate",
            json!({ "expression": format!("document.querySelectorAll('iframe,frame')[{i}]") }),
            t,
        )
        .await?;
        let Some(object_id) = el.pointer("/result/objectId").and_then(|o| o.as_str()) else {
            continue;
        };
        let node = super::page_call("DOM.describeNode", json!({ "objectId": object_id }), t).await;
        let _ =
            super::page_call("Runtime.releaseObject", json!({ "objectId": object_id }), t).await;
        let node = node.unwrap_or(Value::Null);
        let attrs = node
            .pointer("/node/attributes")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let attr = |name: &str| {
            attrs
                .chunks(2)
                .find(|kv| kv.first().and_then(|k| k.as_str()) == Some(name))
                .and_then(|kv| kv.get(1).and_then(|v| v.as_str()))
                .map(String::from)
        };
        let id = node
            .pointer("/node/frameId")
            .and_then(|f| f.as_str())
            .unwrap_or_default()
            .to_string();
        let mut f = by_id.get(&id).cloned().unwrap_or_else(|| {
            json!({
                "id": id,
                "url": oopifs
                    .iter()
                    .find(|(t, u)| *t == id && !u.is_empty())
                    .map(|(_, u)| u.clone())
                    .or_else(|| attr("src")),
            })
        });
        if f["name"].as_str().is_none_or(|n| n.is_empty()) {
            f["name"] = json!(attr("name").unwrap_or_default());
        }
        f["dom_index"] = json!(i);
        seen.insert(id);
        out.push(f);
    }
    for id in tree_order {
        if !seen.contains(&id) {
            if let Some(f) = by_id.get(&id) {
                seen.insert(id.clone());
                out.push(f.clone());
            }
        }
    }
    for (tid, url) in oopifs {
        if !seen.contains(&tid) {
            out.push(json!({ "id": tid, "url": url, "name": "" }));
        }
    }
    Ok(out)
}

struct FrameTarget {
    frame_id: String,
    label: String,
    url: Option<String>,
}

async fn resolve_frame(frame: Option<&str>) -> Result<FrameTarget, Value> {
    let b =
        super::current().ok_or_else(|| json!({ "status": "failed", "error": "CHROME_GONE" }))?;
    let spec = frame.unwrap_or("top");
    // The top frame's own id is what every result reports as `frame_id`, so
    // feeding it back must work too.
    if spec == "top" || spec == b.target_id {
        return Ok(FrameTarget {
            frame_id: b.target_id.clone(),
            label: "top".into(),
            url: None,
        });
    }
    let list = frames().await.map_err(
        |e| json!({ "status": "failed", "error": "FRAME_ENUMERATION_FAILED", "message": e }),
    )?;
    let known = || {
        list.iter()
            .enumerate()
            .map(|(i, f)| json!({ "index": i, "frame_id": f["id"], "url": f["url"], "name": f["name"] }))
            .collect::<Vec<_>>()
    };
    if let Ok(idx) = spec.parse::<usize>() {
        return list
            .get(idx)
            .map(|f| FrameTarget {
                frame_id: f["id"].as_str().unwrap_or_default().to_string(),
                label: format!("frame[{idx}]"),
                url: f["url"].as_str().map(String::from),
            })
            .ok_or_else(|| {
                json!({
                    "status": "failed",
                    "error": "FRAME_INDEX_OUT_OF_RANGE",
                    "message": format!("frame index {idx} out of range — get-frames lists {} frame(s)", list.len()),
                    "known_frames": known(),
                })
            });
    }
    let pick = list
        .iter()
        .find(|f| f["id"].as_str() == Some(spec))
        .or_else(|| {
            list.iter().find(|f| {
                f["url"].as_str().is_some_and(|u| u.contains(spec))
                    || f["name"].as_str().is_some_and(|n| n.contains(spec))
            })
        });
    match pick {
        Some(f) => Ok(FrameTarget {
            frame_id: f["id"].as_str().unwrap_or_default().to_string(),
            label: spec.to_string(),
            url: f["url"].as_str().map(String::from),
        }),
        None => Err(json!({
            "status": "failed",
            "error": "FRAME_NOT_FOUND",
            "message": format!("no frame has id '{spec}' or a url/name containing it"),
            "known_frames": known(),
        })),
    }
}

async fn frame_list(args: &Value) -> Result<Value> {
    let full = args.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
    let list = match frames().await {
        Ok(l) => l,
        Err(e) => {
            return Ok(
                json!({ "status": "failed", "error": "FRAME_ENUMERATION_FAILED", "message": e }),
            )
        }
    };
    let rendered: Vec<Value> = list
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let id = f["id"].as_str().unwrap_or_default();
            let name = f["name"].as_str().unwrap_or_default();
            let (name_shown, name_len) = if full || name.chars().count() <= 200 {
                (name.to_string(), None)
            } else {
                (name.chars().take(200).collect(), Some(name.chars().count()))
            };
            json!({
                "index": i,
                "frame_id": id,
                "parent_id": f["parentId"],
                "url": f["url"],
                "name": name_shown,
                "name_len": name_len,
                "name_truncated": name_len.is_some(),
                "security_origin": f["securityOrigin"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| super::context_origin(id)),
                "dom_index": f["dom_index"],
                // Cross-site iframes run in their own renderer process; they are
                // reached through their own CDP session.
                "out_of_process": super::child_session_for_frame(id).is_some(),
                "main_world_context": super::main_context_for(id).is_some(),
            })
        })
        .collect();
    Ok(json!({
        "status": "completed",
        "engine": "chrome",
        "count": rendered.len(),
        "frames": rendered,
    }))
}

async fn frame_html(args: &Value) -> Result<Value> {
    let target = if let Some(idx) = args.get("index").and_then(|v| v.as_u64()) {
        resolve_frame(Some(&idx.to_string())).await
    } else if let Some(sel) = arg_str(args, "selector") {
        match frame_id_for_selector(sel).await {
            Ok(id) => resolve_frame(Some(&id)).await,
            Err(e) => Err(e),
        }
    } else {
        resolve_frame(Some("0")).await
    };
    let target = match target {
        Ok(t) => t,
        Err(v) => return Ok(v),
    };
    let (session, ctx) = match main_world(&target.frame_id).await {
        Ok(x) => x,
        Err(v) => return Ok(v),
    };
    match super::eval_wrapped(
        session.as_deref(),
        ctx,
        "return document.documentElement ? document.documentElement.outerHTML : ''",
        Some(10000),
        false,
    )
    .await
    {
        Ok(v) => {
            let html = v.as_str().unwrap_or_default().to_string();
            Ok(json!({
                "status": "completed",
                "frame": target.label,
                "frame_id": target.frame_id,
                "url": target.url,
                "length": html.len(),
                "html": html,
            }))
        }
        Err(e) => Ok(json!({ "status": "failed", "error": "FRAME_HTML_FAILED", "message": e })),
    }
}

/// The frame id of the `<iframe>` a selector names in the top document.
async fn frame_id_for_selector(sel: &str) -> Result<String, Value> {
    let fail = |m: String| json!({ "status": "failed", "error": "FRAME_NOT_FOUND", "message": m });
    let expr = format!(
        "document.querySelector({})",
        serde_json::Value::String(sel.to_string())
    );
    let v = super::page_call(
        "Runtime.evaluate",
        json!({ "expression": expr }),
        Duration::from_secs(5),
    )
    .await
    .map_err(fail)?;
    let object_id = v
        .pointer("/result/objectId")
        .and_then(|o| o.as_str())
        .ok_or_else(|| fail(format!("no element matches '{sel}'")))?;
    let node = super::page_call(
        "DOM.describeNode",
        json!({ "objectId": object_id }),
        Duration::from_secs(5),
    )
    .await;
    // The remote object pins the element in the page until released.
    let _ = super::page_call(
        "Runtime.releaseObject",
        json!({ "objectId": object_id }),
        Duration::from_secs(5),
    )
    .await;
    let node = node.map_err(fail)?;
    node.pointer("/node/frameId")
        .and_then(|f| f.as_str())
        .map(String::from)
        .ok_or_else(|| fail(format!("'{sel}' is not a frame element")))
}

/// Where a frame's main world can be evaluated: `(session, context)`.
async fn main_world(frame_id: &str) -> Result<(Option<String>, Option<i64>), Value> {
    let b =
        super::current().ok_or_else(|| json!({ "status": "failed", "error": "CHROME_GONE" }))?;
    if frame_id == b.target_id {
        return Ok((None, None));
    }
    super::main_context_for(frame_id).ok_or_else(|| {
        json!({
            "status": "failed",
            "error": "NO_MAIN_WORLD_CONTEXT",
            "message": format!(
                "no main-world execution context is known for frame {frame_id} (no-console-capture \
                 daemons do not track contexts; srcless frames may never announce one)"
            ),
            "frame_id": frame_id,
            "hint": "try --world isolated — it only needs the frame id",
        })
    })
}

/// An isolated world in a frame: `(session, context)`.
async fn isolated_world(frame_id: &str) -> Result<(Option<String>, Option<i64>), Value> {
    let session = super::child_session_for_frame(frame_id);
    let v = super::session_call(
        Some(
            session.as_deref().unwrap_or(
                &super::current()
                    .map(|b| b.page_session.clone())
                    .unwrap_or_default(),
            ),
        ),
        "Page.createIsolatedWorld",
        json!({
            "frameId": frame_id,
            "worldName": crate::inject::ISOLATED_WORLD,
            "grantUniveralAccess": true,
        }),
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| json!({ "status": "failed", "error": "ISOLATED_WORLD_FAILED", "message": e }))?;
    let ctx = v
        .get("executionContextId")
        .and_then(|c| c.as_i64())
        .ok_or_else(|| json!({ "status": "failed", "error": "ISOLATED_WORLD_FAILED", "message": format!("no context id in {v}") }))?;
    Ok((session, Some(ctx)))
}

// ---------------------------------------------------------------------------
// exec-js
// ---------------------------------------------------------------------------

async fn exec_js(args: &Value) -> Result<Value> {
    let Some(script) = args.get("script").and_then(|v| v.as_str()) else {
        return Ok(
            json!({ "status": "failed", "error": "SCRIPT_REQUIRED", "message": "script required" }),
        );
    };
    let timeout_ms = args["timeout_ms"].as_u64().map(|v| v as u32);
    let await_promise = args["await_promise"].as_bool().unwrap_or(false);
    let max_len = args["max_result_length"].as_u64().unwrap_or(0) as usize;
    let cap = if max_len == 0 {
        crate::tools::definitions::JS_RESULT_SAFETY_CAP
    } else {
        max_len.min(crate::tools::definitions::JS_RESULT_SAFETY_CAP)
    };
    let world = args.get("world").and_then(|v| v.as_str()).unwrap_or("main");
    if world != "main" && world != "isolated" {
        return Ok(json!({
            "status": "failed",
            "error": "INVALID_WORLD",
            "message": format!("unknown world '{world}' — use 'main' or 'isolated'"),
        }));
    }
    let target = match resolve_frame(arg_str(args, "frame")).await {
        Ok(t) => t,
        Err(v) => return Ok(v),
    };
    let located = if world == "isolated" {
        isolated_world(&target.frame_id).await
    } else {
        main_world(&target.frame_id).await
    };
    let (session, ctx) = match located {
        Ok(x) => x,
        Err(v) => return Ok(v),
    };

    let stamp = json!({
        "world": world,
        "frame": target.label,
        "frame_id": target.frame_id,
        "frame_origin": super::context_origin(&target.frame_id),
        "context_id": ctx,
        "session": if session.is_some() { "out_of_process_frame" } else { "page" },
        "engine": "chrome",
    });

    let mut out =
        match super::eval_wrapped(session.as_deref(), ctx, script, timeout_ms, await_promise).await
        {
            Ok(v) => {
                let s = match &v {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                if s.len() > cap {
                    let mut cut = cap;
                    while cut > 0 && !s.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    json!({
                        "status": "completed",
                        "result": &s[..cut],
                        "truncated": true,
                        "total_length": s.len(),
                        "safety_cap": crate::tools::definitions::JS_RESULT_SAFETY_CAP,
                    })
                } else {
                    json!({ "status": "completed", "result": v })
                }
            }
            Err(e) => {
                // Same vocabulary as the WebView engine: the frame was found
                // and the evaluate delivered; its renderer did not answer.
                let busy = e.contains("timed out") || e.starts_with("TIMEOUT");
                let code = if e.contains("Cannot find context") {
                    "CONTEXT_NOT_FOUND"
                } else if e.starts_with("CHROME_GONE") {
                    "CHROME_GONE"
                } else if busy {
                    "RENDERER_BUSY"
                } else {
                    "JS_EXECUTION_ERROR"
                };
                let mut out = json!({ "status": "failed", "error": code, "message": e });
                if busy {
                    out["hint"] = json!(
                        "The target exists — the evaluate was delivered and its renderer did not \
                         answer in time (a long-running script: a challenge computing, a busy \
                         loop). Retry later or with a larger --timeout-ms; `console-logs` and \
                         `nav-log` keep answering meanwhile."
                    );
                }
                out
            }
        };
    merge(&mut out, stamp);
    Ok(out)
}

// ---------------------------------------------------------------------------
// input
// ---------------------------------------------------------------------------

async fn key_event(params: Value) -> Result<(), String> {
    super::page_call("Input.dispatchKeyEvent", params, Duration::from_secs(10))
        .await
        .map(|_| ())
}

async fn mouse(kind: &str, x: f64, y: f64, button: &str, click_count: u32) -> Result<(), String> {
    let buttons = match (kind, button) {
        ("mousePressed", "left") => 1,
        ("mousePressed", "right") => 2,
        _ => 0,
    };
    super::page_call(
        "Input.dispatchMouseEvent",
        json!({
            "type": kind,
            "x": x,
            "y": y,
            "button": if kind == "mouseMoved" { "none" } else { button },
            "buttons": buttons,
            "clickCount": click_count,
        }),
        Duration::from_secs(10),
    )
    .await
    .map(|_| ())
}

/// A human click at viewport coordinates: move, press, release (twice for a
/// double click, with the click count CDP needs to synthesize `dblclick`).
async fn trusted_click(x: f64, y: f64, click_type: &str) -> Result<(), String> {
    let button = if click_type == "right" {
        "right"
    } else {
        "left"
    };
    mouse("mouseMoved", x, y, "none", 0).await?;
    let clicks = if click_type == "double" { 2 } else { 1 };
    for n in 1..=clicks {
        mouse("mousePressed", x, y, button, n).await?;
        mouse("mouseReleased", x, y, button, n).await?;
    }
    Ok(())
}

async fn element_at(x: f64, y: f64) -> Value {
    let script = format!(
        "var el = document.elementFromPoint({x}, {y}); if (!el) return null; \
         var r = el.getBoundingClientRect(); \
         return JSON.stringify({{ tag: el.tagName.toLowerCase(), text: (el.textContent || '').trim().substring(0, 100), \
         rect: {{ x: r.x, y: r.y, width: r.width, height: r.height }} }});"
    );
    match super::eval_page(&script, Some(3000), false).await {
        Ok(Value::String(s)) => serde_json::from_str(&s).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

async fn click_at(args: &Value) -> Result<Value> {
    let (Some(x), Some(y)) = (args["x"].as_f64(), args["y"].as_f64()) else {
        return Ok(
            json!({ "status": "failed", "error": "COORDINATES_REQUIRED", "message": "x and y required" }),
        );
    };
    let click_type = args["click_type"].as_str().unwrap_or("left");
    let target = element_at(x, y).await;
    if let Err(e) = trusted_click(x, y, click_type).await {
        return Ok(json!({ "status": "failed", "error": "CLICK_ERROR", "message": e }));
    }
    let mut out = json!({
        "status": "completed",
        "method": "cdp_input",
        "trusted": true,
        "x": x,
        "y": y,
    });
    if let Some(o) = target.as_object() {
        for (k, v) in o {
            out[k] = v.clone();
        }
    } else {
        out["note"] = json!("no element at these coordinates (the click was still delivered)");
    }
    Ok(out)
}

/// Scroll an element into view and return its centre, using the same element
/// matching as the WebView click.
async fn locate(selector: Option<&str>, text: Option<&str>) -> Result<Value, String> {
    let finder = crate::tools::definitions::element_finder_js(selector, text);
    let script = format!(
        "var el = {finder}; if (!el) return JSON.stringify({{ found: false }}); \
         el.scrollIntoView({{ block: 'center', behavior: 'instant' }}); \
         var r = el.getBoundingClientRect(); \
         return JSON.stringify({{ found: true, tag: el.tagName.toLowerCase(), \
           text: (el.textContent || '').trim().substring(0, 100), \
           rect: {{ x: Math.round(r.x), y: Math.round(r.y), width: Math.round(r.width), height: Math.round(r.height) }}, \
           center: {{ x: r.x + r.width / 2, y: r.y + r.height / 2 }} }});"
    );
    match super::eval_page(&script, Some(10000), false).await? {
        Value::String(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
        other => Ok(other),
    }
}

async fn not_found(selector: Option<&str>, text: Option<&str>) -> Value {
    let nearby = crate::tools::definitions::collect_nearby_interactive(8).await;
    json!({
        "status": "failed",
        "error": "ELEMENT_NOT_FOUND",
        "message": format!("No element found for {}", selector.map(|s| format!("selector '{s}'")).or(text.map(|t| format!("text '{t}'"))).unwrap_or_default()),
        "nearby": nearby,
        "hint": "Retry with one of the selectors in `nearby`, or call `describe` for the full element index.",
    })
}

async fn click_element(args: &Value) -> Result<Value> {
    let selector = arg_str(args, "selector");
    let text = arg_str(args, "text");
    if selector.is_none() && text.is_none() {
        return Ok(
            json!({ "status": "failed", "error": "TARGET_REQUIRED", "message": "selector or text required" }),
        );
    }
    let click_type = args["click_type"].as_str().unwrap_or("left");
    let found = match locate(selector, text).await {
        Ok(v) => v,
        Err(e) => {
            return Ok(json!({ "status": "failed", "error": "CLICK_ELEMENT_ERROR", "message": e }))
        }
    };
    if found["found"] != true {
        return Ok(not_found(selector, text).await);
    }
    let (x, y) = (
        found["center"]["x"].as_f64().unwrap_or(0.0),
        found["center"]["y"].as_f64().unwrap_or(0.0),
    );
    if let Err(e) = trusted_click(x, y, click_type).await {
        return Ok(json!({ "status": "failed", "error": "CLICK_ERROR", "message": e }));
    }
    Ok(json!({
        "status": "completed",
        "method": "cdp_input",
        "trusted": true,
        "tag": found["tag"],
        "text": found["text"],
        "rect": found["rect"],
        "center": { "x": x.round(), "y": y.round() },
    }))
}

async fn hover(args: &Value) -> Result<Value> {
    let selector = arg_str(args, "selector");
    let (x, y, info) = if let (Some(x), Some(y)) = (args["x"].as_f64(), args["y"].as_f64()) {
        (x, y, element_at(x, y).await)
    } else if selector.is_some() {
        let found = match locate(selector, None).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(json!({ "status": "failed", "error": "HOVER_ERROR", "message": e }))
            }
        };
        if found["found"] != true {
            return Ok(not_found(selector, None).await);
        }
        (
            found["center"]["x"].as_f64().unwrap_or(0.0),
            found["center"]["y"].as_f64().unwrap_or(0.0),
            found,
        )
    } else {
        return Ok(
            json!({ "status": "failed", "error": "TARGET_REQUIRED", "message": "selector or x/y required" }),
        );
    };
    if let Err(e) = mouse("mouseMoved", x, y, "none", 0).await {
        return Ok(json!({ "status": "failed", "error": "HOVER_ERROR", "message": e }));
    }
    Ok(json!({
        "status": "completed",
        "x": x.round(),
        "y": y.round(),
        // The browser's own pointer moved, so CSS :hover applies.
        "native_hover": true,
        "method": "cdp_input",
        "tag": info["tag"],
        "text": info["text"],
    }))
}

/// DOM key name → (code, Windows virtual key, text it types).
/// What one key press needs to look like to Blink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyInfo {
    /// The DOM `KeyboardEvent.key`, canonically spelled — Blink's default
    /// actions compare it exactly (`"Tab"` moves focus, `"tab"` does nothing;
    /// a button activates on `" "`, not `"Space"`).
    key: String,
    code: String,
    vk: i64,
    /// What the key types, if anything.
    text: Option<String>,
}

/// Key name (any case; `Space` accepted for `" "`) or a single character →
/// the event fields CDP needs.
pub(crate) fn key_info(key: &str) -> Option<KeyInfo> {
    let named: &[(&str, &str, i64, Option<&str>)] = &[
        ("Enter", "Enter", 13, Some("\r")),
        ("Tab", "Tab", 9, None),
        ("Escape", "Escape", 27, None),
        ("Backspace", "Backspace", 8, None),
        ("Delete", "Delete", 46, None),
        ("Insert", "Insert", 45, None),
        ("ArrowLeft", "ArrowLeft", 37, None),
        ("ArrowUp", "ArrowUp", 38, None),
        ("ArrowRight", "ArrowRight", 39, None),
        ("ArrowDown", "ArrowDown", 40, None),
        ("Home", "Home", 36, None),
        ("End", "End", 35, None),
        ("PageUp", "PageUp", 33, None),
        ("PageDown", "PageDown", 34, None),
    ];
    if let Some((name, code, vk, text)) = named.iter().find(|(k, ..)| k.eq_ignore_ascii_case(key)) {
        return Some(KeyInfo {
            key: name.to_string(),
            code: code.to_string(),
            vk: *vk,
            text: text.map(String::from),
        });
    }
    if key == " " || key.eq_ignore_ascii_case("space") {
        return Some(KeyInfo {
            key: " ".into(),
            code: "Space".into(),
            vk: 32,
            text: Some(" ".into()),
        });
    }
    if let Some(n) = key
        .strip_prefix(['F', 'f'])
        .and_then(|n| n.parse::<i64>().ok())
    {
        if (1..=12).contains(&n) {
            return Some(KeyInfo {
                key: format!("F{n}"),
                code: format!("F{n}"),
                vk: 111 + n,
                text: None,
            });
        }
    }
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        // Typing a newline is pressing Enter.
        if c == '\n' || c == '\r' {
            return key_info("Enter");
        }
        let (code, vk) = if c.is_ascii_alphabetic() {
            let up = c.to_ascii_uppercase();
            (format!("Key{up}"), up as i64)
        } else if c.is_ascii_digit() {
            (format!("Digit{c}"), c as i64)
        } else {
            (String::new(), 0)
        };
        return Some(KeyInfo {
            key: c.to_string(),
            code,
            vk,
            text: Some(c.to_string()),
        });
    }
    None
}

pub(crate) fn modifier_mask(modifiers: &str) -> i64 {
    let mut m = 0;
    for p in modifiers.split(',').map(|s| s.trim().to_ascii_lowercase()) {
        m |= match p.as_str() {
            "alt" => 1,
            "ctrl" | "control" => 2,
            "meta" | "cmd" | "command" => 4,
            "shift" => 8,
            _ => 0,
        };
    }
    m
}

async fn keyboard(args: &Value) -> Result<Value> {
    let text = args["text"].as_str();
    let key = args["key"].as_str();
    if text.is_none() && key.is_none() {
        return Ok(
            json!({ "status": "failed", "error": "INPUT_REQUIRED", "message": "text or key required" }),
        );
    }
    let modifiers = modifier_mask(args["modifiers"].as_str().unwrap_or(""));
    if let Some(sel) = arg_str(args, "selector") {
        let focus = format!(
            "var el = document.querySelector({}); if (!el) return false; el.focus(); \
             if (typeof el.selectionStart === 'number' && el.value !== undefined) {{ \
               el.selectionStart = el.selectionEnd = el.value.length; }} return true;",
            serde_json::Value::String(sel.to_string())
        );
        if super::eval_page(&focus, Some(5000), false).await.ok() != Some(Value::Bool(true)) {
            return Ok(not_found(Some(sel), None).await);
        }
    }

    if let Some(k) = key {
        let Some(info) = key_info(k) else {
            return Ok(
                json!({ "status": "failed", "error": "UNKNOWN_KEY", "message": format!("unknown key '{k}'") }),
            );
        };
        if let Err(e) = press(&info, modifiers).await {
            return Ok(json!({ "status": "failed", "error": "KEYBOARD_ERROR", "message": e }));
        }
        return Ok(
            json!({ "status": "completed", "key": info.key, "modifiers": modifiers, "method": "cdp_input", "trusted": true }),
        );
    }

    let text = text.unwrap_or_default();
    if args["paste"].as_bool().unwrap_or(false) {
        if let Err(e) = super::page_call(
            "Input.insertText",
            json!({ "text": text }),
            Duration::from_secs(10),
        )
        .await
        {
            return Ok(json!({ "status": "failed", "error": "KEYBOARD_ERROR", "message": e }));
        }
        return Ok(
            json!({ "status": "completed", "typed": text.chars().count(), "mode": "paste", "method": "cdp_input" }),
        );
    }
    // One key event per character, like a person typing (a newline is Enter).
    for c in text.chars() {
        let Some(info) = key_info(&c.to_string()) else {
            continue;
        };
        if let Err(e) = press(&info, modifiers).await {
            return Ok(json!({ "status": "failed", "error": "KEYBOARD_ERROR", "message": e }));
        }
    }
    Ok(
        json!({ "status": "completed", "typed": text.chars().count(), "mode": "keys", "method": "cdp_input", "trusted": true }),
    )
}

/// keyDown (or rawKeyDown for keys that type nothing) + keyUp.
async fn press(info: &KeyInfo, modifiers: i64) -> Result<(), String> {
    for params in key_event_params(info, modifiers) {
        key_event(params).await?;
    }
    Ok(())
}

/// The two `Input.dispatchKeyEvent` calls of one key press. `keyboard
/// --native` on WebView2 sends the same ones over its own CDP channel.
pub(crate) fn key_event_params(info: &KeyInfo, modifiers: i64) -> [Value; 2] {
    // Ctrl/Meta chords type nothing; a plain key with text produces input.
    let typed = info.text.clone().filter(|_| modifiers & (2 | 4) == 0);
    let mut down = json!({
        "type": if typed.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": info.key,
        "code": info.code,
        "windowsVirtualKeyCode": info.vk,
        "nativeVirtualKeyCode": info.vk,
        "modifiers": modifiers,
    });
    if let Some(t) = &typed {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    let up = json!({
        "type": "keyUp", "key": info.key, "code": info.code,
        "windowsVirtualKeyCode": info.vk, "nativeVirtualKeyCode": info.vk, "modifiers": modifiers,
    });
    [down, up]
}

async fn resize(args: &Value) -> Result<Value> {
    let (Some(w), Some(h)) = (args["width"].as_u64(), args["height"].as_u64()) else {
        return Ok(
            json!({ "status": "failed", "error": "INVALID_SIZE", "message": "width and height required" }),
        );
    };
    if w < 100 || h < 100 {
        return Ok(
            json!({ "status": "failed", "error": "INVALID_SIZE", "message": "width and height must be >= 100" }),
        );
    }
    let Some(b) = super::current() else {
        return Ok(json!({ "status": "failed", "error": "CHROME_GONE" }));
    };
    let t = Duration::from_secs(10);
    let win = match b
        .conn
        .call(
            None,
            "Browser.getWindowForTarget",
            json!({ "targetId": b.target_id }),
            t,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return Ok(json!({ "status": "failed", "error": "RESIZE_FAILED", "message": e })),
    };
    let Some(window_id) = win.get("windowId").and_then(|x| x.as_i64()) else {
        return Ok(
            json!({ "status": "failed", "error": "RESIZE_FAILED", "message": format!("no windowId in {win}") }),
        );
    };
    // A maximized window ignores width/height until it is normal again.
    let _ = b
        .conn
        .call(
            None,
            "Browser.setWindowBounds",
            json!({ "windowId": window_id, "bounds": { "windowState": "normal" } }),
            t,
        )
        .await;
    if let Err(e) = b
        .conn
        .call(
            None,
            "Browser.setWindowBounds",
            json!({ "windowId": window_id, "bounds": { "width": w, "height": h } }),
            t,
        )
        .await
    {
        return Ok(json!({ "status": "failed", "error": "RESIZE_FAILED", "message": e }));
    }
    let viewport = super::eval_page(
        "return JSON.stringify({w: innerWidth, h: innerHeight})",
        Some(3000),
        false,
    )
    .await
    .ok()
    .and_then(|v| {
        v.as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
    });
    Ok(json!({
        "status": "completed",
        "width": w,
        "height": h,
        "applies_to": "the Chrome window (outer size)",
        "viewport": viewport,
    }))
}

// ---------------------------------------------------------------------------
// requests — served from the CDP request index, not an in-page hook
// ---------------------------------------------------------------------------

async fn captured_requests(args: &Value) -> Result<Value> {
    let seconds = args["seconds"].as_u64().unwrap_or(180);
    let since = crate::recording::now_ms().saturating_sub(seconds * 1000);
    let url_pattern = args["url_pattern"].as_str().unwrap_or("");
    let url_exclude = args["url_exclude"].as_str().unwrap_or("");
    let ct_filter = args["content_type"].as_str().unwrap_or("").to_lowercase();
    let method_filter = args["method"].as_str().unwrap_or("");
    let max_count = args["max_count"].as_u64().unwrap_or(0) as usize;
    let include_preview = args["include_preview"].as_bool().unwrap_or(true);
    let include_headers = args["include_headers"].as_bool().unwrap_or(false);
    let no_redact = args["no_redact"].as_bool().unwrap_or(false);

    let all = crate::netlog::requests(&["XHR", "Fetch"], since);
    let total = all.len();
    let mut out: Vec<Value> = all
        .into_iter()
        .filter(|r| {
            let url = r["url"].as_str().unwrap_or("");
            let ct = r["mime_type"].as_str().unwrap_or("").to_lowercase();
            let m = r["method"].as_str().unwrap_or("");
            (url_pattern.is_empty() || url.contains(url_pattern))
                && !url_exclude
                    .split('|')
                    .map(|p| p.trim())
                    .any(|p| !p.is_empty() && url.contains(p))
                && (ct_filter.is_empty() || ct.contains(&ct_filter))
                && (method_filter.is_empty() || m.eq_ignore_ascii_case(method_filter))
        })
        .collect();
    if max_count > 0 && out.len() > max_count {
        out.truncate(max_count);
    }

    let mut shaped = Vec::with_capacity(out.len());
    for r in out {
        let id = r["request_id"].as_str().unwrap_or_default().to_string();
        let mut v = json!({
            "id": id,
            "method": r["method"],
            "url": r["url"],
            "status": r["status"],
            "contentType": r["mime_type"],
            "duration": r["duration_ms"],
            "type": r["resource_type"],
            "ts": r["ts_ms"],
            "pending": if r["pending"] == true { json!(true) } else { Value::Null },
            "error": r["error_text"],
        });
        if include_preview && r["status"].is_i64() && r["finished_ms"].is_u64() {
            // Bodies stay in Chrome's buffer for a while after loading; past
            // that this simply comes back empty.
            if let Ok(body) = super::page_call(
                "Network.getResponseBody",
                json!({ "requestId": id }),
                Duration::from_secs(3),
            )
            .await
            {
                let text = if body["base64Encoded"] == true {
                    None
                } else {
                    body["body"].as_str().map(String::from)
                };
                v["hasBody"] = json!(text.is_some());
                v["bodyPreview"] = json!(text.map(|t| t.chars().take(200).collect::<String>()));
            }
        }
        if include_headers {
            let hdrs: std::collections::BTreeMap<String, String> =
                serde_json::from_value(r["request_headers"].clone()).unwrap_or_default();
            v["headers"] = json!(if no_redact {
                hdrs
            } else {
                crate::netlog::redact_headers(&hdrs)
            });
        }
        shaped.push(v);
    }
    Ok(json!({
        "status": "completed",
        "engine": "chrome",
        "source": "cdp",
        "total_captured": total,
        "returned": shaped.len(),
        "requests": shaped,
        "note": "Served from CDP Network events — nothing is injected into the page on this \
                 engine. `failed-requests` and `nav-log` cover every resource type.",
    }))
}

async fn network_log(args: &Value) -> Result<Value> {
    let ago = args["since_ms"].as_u64().unwrap_or(60_000);
    let since = crate::recording::now_ms().saturating_sub(ago);
    let pattern = args["url_pattern"].as_str().unwrap_or("");
    let max = args["max"].as_u64().unwrap_or(200) as usize;
    let now = crate::recording::now_ms();
    let list: Vec<Value> = crate::netlog::requests(&["XHR", "Fetch"], since)
        .into_iter()
        .filter(|r| pattern.is_empty() || r["url"].as_str().unwrap_or("").contains(pattern))
        .map(|r| {
            json!({
                "id": r["request_id"],
                "method": r["method"],
                "url": r["url"],
                "status": r["status"],
                "type": r["resource_type"],
                "ts": r["ts_ms"],
                "duration": r["duration_ms"],
                "pending": r["pending"],
                "age_ms": now.saturating_sub(r["ts_ms"].as_u64().unwrap_or(now)),
                "error": r["error_text"],
            })
        })
        .collect();
    let pending: Vec<&Value> = list.iter().filter(|r| r["pending"] == true).collect();
    let start = list.len().saturating_sub(max);
    Ok(json!({
        "status": "completed",
        "engine": "chrome",
        "source": "cdp",
        "count": list.len(),
        "pending_count": pending.len(),
        "pending": pending,
        "requests": list[start..].to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_cdp_codes() {
        let enter = key_info("Enter").unwrap();
        assert_eq!(
            (enter.code.as_str(), enter.vk, enter.text.as_deref()),
            ("Enter", 13, Some("\r"))
        );
        let a = key_info("a").unwrap();
        assert_eq!((a.key.as_str(), a.code.as_str(), a.vk), ("a", "KeyA", 65));
        assert_eq!(key_info("7").unwrap().code, "Digit7");
        assert_eq!(key_info("F5").unwrap().vk, 116);
        assert!(key_info("NotAKey").is_none());
    }

    #[test]
    fn key_names_are_sent_canonically_spelled() {
        // Blink's default actions compare `key` exactly.
        assert_eq!(key_info("tab").unwrap().key, "Tab");
        assert_eq!(key_info("ENTER").unwrap().key, "Enter");
        assert_eq!(key_info("escape").unwrap().key, "Escape");
        assert_eq!(key_info("Space").unwrap().key, " ");
        assert_eq!(key_info("f5").unwrap().key, "F5");
        // A newline typed in --text is the Enter key.
        assert_eq!(key_info("\n").unwrap().key, "Enter");
    }

    #[test]
    fn modifiers_build_the_cdp_mask() {
        assert_eq!(modifier_mask("ctrl,shift"), 10);
        assert_eq!(modifier_mask("alt"), 1);
        assert_eq!(modifier_mask(""), 0);
    }
}
