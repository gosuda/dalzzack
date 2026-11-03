//! `reset` — return a live daemon to a first-visit state without giving up its
//! id, port, window or start flags.
//!
//! Stopping and restarting is the other way to get there, and it costs more
//! than time: the id's port changes under any script that cached it, and every
//! start-time flag has to be repeated exactly. `reset` clears what a site can
//! remember (cookies, storage, cache, service workers), what dalzzack itself
//! was told to do (intercept rules, injected scripts) and what it has buffered
//! (console, timeline, headers, tape) — and says which of those it did.

use anyhow::Result;
use serde_json::{json, Map, Value};

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub async fn tool_reset(args: &Value) -> Result<Value> {
    let keep_browsing_data = arg_bool(args, "keep_browsing_data");
    let keep_rules = arg_bool(args, "keep_rules");
    let force_shared = arg_bool(args, "force_shared");
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("about:blank")
        .to_string();

    let mut steps = Map::new();

    // 0. Refuse before touching anything: browsing data is the only step that
    // can reach beyond this daemon. On the shared profile that is always so —
    // it outlives this daemon — not only while another daemon happens to run.
    let refusal = if keep_browsing_data {
        None
    } else {
        crate::profile::guard("reset", force_shared)
            .await
            .map(|mut refusal| {
                refusal["hint"] = json!(
                    "Pass --dry-run to see what would be cleared. Start this instance with \
                     `--private-profile` or `--engine chrome` so it has a profile of its own, \
                     pass --keep-browsing-data to reset only dalzzack's own state (rules, \
                     buffers), or --force-shared to clear the shared profile anyway."
                );
                refusal
            })
    };
    if arg_bool(args, "dry_run") {
        return Ok(plan(keep_browsing_data, keep_rules, &url, refusal).await);
    }
    if let Some(refusal) = refusal {
        return Ok(refusal);
    }

    // 1. What dalzzack was told to do to every page. First, while the
    // browser session the rules were registered on still exists — the Chrome
    // engine replaces the whole browser in the next step.
    if keep_rules {
        steps.insert("rules".into(), json!("kept (--keep-rules)"));
    } else {
        let intercept = crate::tools::definitions::tool_intercept_clear(&json!({}))
            .await
            .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        let inject = crate::tools::definitions::tool_inject_clear(&json!({}))
            .await
            .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        steps.insert(
            "rules".into(),
            json!({ "intercept": intercept, "inject": inject }),
        );
    }

    // Where there is no profile-wide clear, step 3 can only clear the origin
    // the page is on — and after step 2 the page is about:blank, which has
    // none. Read it now, while the old document is still there.
    let page_before = if keep_browsing_data {
        None
    } else {
        crate::site_data::current_url().await
    };

    // 2. Leave the old document BEFORE clearing anything. Its unload /
    // pagehide handlers, pending timers and in-flight responses would
    // otherwise write cookies and storage straight back into the profile that
    // was just emptied. (The Chrome engine replaces the whole browser, so
    // there it is moot — but harmless.)
    if !keep_browsing_data && !crate::engine::is_chrome() {
        let away = navigate_to("about:blank").await;
        steps.insert("leave_page".into(), away);
    }

    // 3. Browsing data.
    if keep_browsing_data {
        steps.insert("browsing_data".into(), json!("kept (--keep-browsing-data)"));
    } else {
        steps.insert(
            "browsing_data".into(),
            match clear_browsing_data(page_before.as_deref()).await {
                Ok(v) => v,
                Err(e) => json!({ "error": e }),
            },
        );
    }

    // 4. What dalzzack has buffered about earlier pages. The tape keeps its
    // on/off state — only its contents go.
    crate::console::snapshot(None, None, 0, true);
    crate::recording::snapshot(None, None, true);
    crate::response_headers::clear();
    crate::netlog::clear();
    crate::sw_console::snapshot(None, 0, true);
    steps.insert(
        "buffers".into(),
        json!([
            "console",
            "recording",
            "response_headers",
            "nav_log",
            "failed_requests",
            "sw_console"
        ]),
    );

    // 5. Where to leave the page.
    steps.insert("page".into(), navigate_to(&url).await);

    let failed: Vec<&String> = steps
        .iter()
        .filter(|(_, v)| step_failed(v))
        .map(|(k, _)| k)
        .collect();
    Ok(json!({
        "status": if failed.is_empty() { "completed" } else { "partial" },
        "id": crate::daemon::instance_id(),
        "engine": crate::engine::current().as_str(),
        "profile": crate::profile::to_json(),
        "steps": steps,
        "failed_steps": failed,
        "kept": ["id", "port", "window", "start flags", "recording on/off state"],
    }))
}

/// `reset --dry-run`: what each step would do, touching nothing. Never refused
/// — looking is how to decide on --force-shared.
async fn plan(
    keep_browsing_data: bool,
    keep_rules: bool,
    url: &str,
    refusal: Option<Value>,
) -> Value {
    let rules = if keep_rules {
        json!("kept (--keep-rules)")
    } else {
        let inject = crate::tools::definitions::tool_inject_list(&json!({}))
            .await
            .ok();
        json!({
            "intercept_rules": crate::intercept::INTERCEPT_RULES.read().await.len(),
            "inject_scripts": inject
                .as_ref()
                .and_then(|v| v.get("scripts").and_then(|s| s.as_array()).map(|a| a.len())),
        })
    };
    let browsing_data = if keep_browsing_data {
        json!("kept (--keep-browsing-data)")
    } else {
        let scope = if crate::engine::is_chrome() {
            "the whole throw-away Chrome profile (a new browser on a new profile)".to_string()
        } else if cfg!(target_os = "windows") {
            "the whole profile: cookies, cache, local/session storage, IndexedDB, service \
             workers, history (ClearBrowsingDataAll)"
                .to_string()
        } else {
            match crate::site_data::current_url().await {
                Some(u) => format!("the current origin only ({u})"),
                None => "nothing — the page has no origin to clear".to_string(),
            }
        };
        // Cookies are the one part that can be listed; storage and caches are
        // per origin and not enumerable here.
        let cookies = if crate::engine::has_cdp() {
            crate::cookies::tool_cookies(&json!({ "action": "list", "all": true }))
                .await
                .ok()
                .map(|v| json!({ "count": v["count"], "by_domain": v["by_domain"] }))
        } else {
            None
        };
        json!({ "scope": scope, "cookies": cookies })
    };
    json!({
        "status": "completed",
        "dry_run": true,
        "would_be_refused": refusal.is_some(),
        "refusal": refusal,
        "profile": crate::profile::to_json(),
        "steps": {
            "rules": rules,
            "leave_page": !keep_browsing_data && !crate::engine::is_chrome(),
            "browsing_data": browsing_data,
            "buffers": ["console", "recording", "response_headers", "nav_log", "failed_requests", "sw_console"],
            "page": url,
        },
    })
}

async fn navigate_to(url: &str) -> Value {
    // Boxed: `execute_tool` dispatches to this very module's tool.
    let nav = Box::pin(crate::tools::definitions::execute_tool(
        "navigate",
        &json!({
            "url": url,
            "timeout_ms": 500,
            "site": crate::daemon::instance_id().unwrap_or_default(),
        }),
    ))
    .await
    .unwrap_or_else(|e| json!({ "status": "failed", "message": e.to_string() }));
    let mut out =
        json!({ "url": url, "status": nav.get("status").cloned().unwrap_or(Value::Null) });
    if nav.get("status").and_then(|s| s.as_str()) == Some("failed") {
        out["error"] = nav
            .get("message")
            .or_else(|| nav.get("error"))
            .cloned()
            .unwrap_or(Value::Null);
    }
    out
}

/// Did a step fail anywhere inside its result? Steps nest other tools'
/// payloads (`rules.intercept`, `rules.inject`), and those report failure as
/// `status: "failed"` or a non-null `error`. (A `warning` — macOS cannot
/// unregister a user script — is information, not failure.)
fn step_failed(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            m.get("error").is_some_and(|e| !e.is_null())
                || m.get("status").and_then(|s| s.as_str()) == Some("failed")
                || m.values().any(step_failed)
        }
        Value::Array(a) => a.iter().any(step_failed),
        _ => false,
    }
}

/// Clear cookies, storage, cache and service workers for the whole profile.
/// `page_before` is the URL the page was on when reset began — the only
/// origin there is to clear on a platform without a profile-wide clear.
async fn clear_browsing_data(page_before: Option<&str>) -> Result<Value, String> {
    if crate::engine::is_chrome() {
        return crate::chrome::reset_profile().await;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = page_before;
        clear_webview2_profile().await?;
        Ok(json!({
            "cleared": "all profile data (ICoreWebView2Profile2::ClearBrowsingDataAll): cookies, \
                        cache, local/session storage, IndexedDB, service workers, history",
        }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // No profile-wide API is wired up for WKWebView/WebKitGTK; clearing the
        // current origin is what `clear-site-data` can do. It is passed
        // explicitly: by now the page has left for about:blank, so letting
        // `clear-site-data` read "the current page" failed on every reset.
        let Some(origin) = page_before.and_then(crate::site_data::origin_of) else {
            return Ok(json!({
                "cleared": format!(
                    "nothing — only the current origin can be cleared on this platform, and \
                     the page had none when reset began ({})",
                    page_before.unwrap_or("URL unavailable")
                ),
            }));
        };
        let r = crate::site_data::clear_site_data(&json!({ "types": "all", "origin": origin }))
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "cleared": "current origin only (no profile-wide clear on this platform)",
            "origin": origin,
            "clear_site_data": r,
        }))
    }
}

#[cfg(target_os = "windows")]
async fn clear_webview2_profile() -> Result<(), String> {
    use webview2_com::ClearBrowsingDataCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2Profile2, ICoreWebView2_13,
    };
    use windows_core::Interface;

    let window = crate::get_main_window()
        .await
        .ok_or_else(|| "WEBVIEW_NOT_FOUND: WebView not ready".to_string())?;
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let tx_ui = tx.clone();
    window
        .with_webview(move |wv| unsafe {
            let started = (|| -> Result<(), String> {
                let core: ICoreWebView2 = wv
                    .controller()
                    .CoreWebView2()
                    .map_err(|e| format!("CoreWebView2: {e:?}"))?;
                let profile = core
                    .cast::<ICoreWebView2_13>()
                    .map_err(|e| format!("ICoreWebView2_13 unavailable (runtime too old): {e:?}"))?
                    .Profile()
                    .map_err(|e| format!("Profile: {e:?}"))?
                    .cast::<ICoreWebView2Profile2>()
                    .map_err(|e| {
                        format!("ICoreWebView2Profile2 unavailable (runtime too old): {e:?}")
                    })?;
                let done = tx_ui.clone();
                let handler = ClearBrowsingDataCompletedHandler::create(Box::new(move |r| {
                    let _ = done.send(r.map_err(|e| format!("ClearBrowsingDataAll: {e:?}")));
                    Ok(())
                }));
                profile
                    .ClearBrowsingDataAll(&handler)
                    .map_err(|e| format!("ClearBrowsingDataAll: {e:?}"))
            })();
            if let Err(e) = started {
                let _ = tx_ui.send(Err(e));
            }
        })
        .map_err(|e| format!("with_webview failed: {e:?}"))?;
    drop(tx);
    tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|_| "ClearBrowsingDataAll did not complete within 30s".to_string())?
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_nested_inside_a_step_is_still_a_failure() {
        assert!(!step_failed(
            &json!({ "url": "about:blank", "status": "completed" })
        ));
        assert!(step_failed(
            &json!({ "url": "x", "status": "failed", "error": "boom" })
        ));
        // `rules` nests two tool payloads.
        assert!(step_failed(&json!({
            "intercept": { "status": "completed" },
            "inject": { "status": "failed", "error": "CDP error" },
        })));
        assert!(!step_failed(&json!({
            "intercept": { "status": "completed", "persist_error": null },
            "inject": { "status": "completed", "warning": "macOS keeps the script" },
        })));
        // Chrome's reset re-registers kept scripts; one failing is a failure.
        assert!(step_failed(&json!({
            "cleared": "everything",
            "scripts_reinjected": [{ "name": "a", "status": "registered" }, { "name": "b", "status": "failed" }],
        })));
    }
}
