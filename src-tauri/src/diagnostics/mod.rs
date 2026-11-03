//! Renderer diagnostics — out-of-band inspection for hung/slow webviews.
//!
//! The browser automation surface (`execute_js`, `console_logs`, …) all funnels
//! through the renderer's main thread. When that thread wedges, every one of
//! those calls times out — so they can't tell you *why* it wedged. This module
//! adds tools that look at the renderer from the outside:
//!
//! - **macOS**: `_webProcessIdentifier` gives us the WKWebView's WebContent PID;
//!   `sample`/`vmmap` then read that process externally. These work even when the
//!   renderer is fully hung, because the sampler is a separate process.
//! - **Windows**: real CDP domains (Debugger / Profiler / Runtime) over WebView2's
//!   `CallDevToolsProtocolMethod`. CDP request/response runs out-of-band from the
//!   renderer's script execution, so `Debugger.pause` / `Profiler.stop` answer
//!   even while page JS is busy.
//! - **console-dump / network-log / wait-for-condition** are portable: they read
//!   the in-page capture buffers, with explicit hung-renderer detection.
//!
//! Anything that can't be done honestly on a platform returns a clear
//! `UNSUPPORTED_*` status instead of pretending.

use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
pub mod symbols;
#[cfg(target_os = "windows")]
mod windows;

// ===========================================================================
// Shared helpers
// ===========================================================================

/// Instance id, used to key per-instance state (e.g. profiling sessions).
/// Only referenced by the macOS diagnostics backend; dead on Windows/Linux.
#[allow(dead_code)]
fn site_key(args: &Value) -> String {
    args.get("site")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string()
}

#[allow(dead_code)]
fn unsupported(tool: &str) -> Result<Value> {
    Ok(json!({
        "status": "failed",
        "error": "UNSUPPORTED_PLATFORM",
        "message": format!(
            "`{tool}` is only implemented on macOS (sample/vmmap) and Windows (CDP)."
        ),
    }))
}

/// A live background sampling session started by `profile-start`.
#[cfg(target_os = "macos")]
pub(crate) struct ProfileSession {
    /// PID of the spawned `sample` helper (SIGINT it to flush the report).
    pub sample_pid: u32,
    /// PID of the WebContent process being sampled.
    pub target_pid: i32,
    /// File the report is written to.
    pub file: std::path::PathBuf,
    /// Wall-clock start, used only for reporting elapsed time.
    pub started: std::time::Instant,
    /// Handle to the `sample` child, kept so `profile-stop` can reap it.
    pub child: std::process::Child,
}

#[cfg(target_os = "macos")]
lazy_static::lazy_static! {
    pub(crate) static ref PROFILE_SESSIONS:
        std::sync::Mutex<std::collections::HashMap<String, ProfileSession>> =
        std::sync::Mutex::new(std::collections::HashMap::new());
}

// ===========================================================================
// Tool entry points (one per CLI subcommand). Each dispatches per-platform.
// ===========================================================================

/// `pause` — capture where the renderer is right now.
pub async fn pause(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::pause(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::pause(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("pause")
    }
}

/// `profile-start` — begin a CPU sampling capture.
pub async fn profile_start(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::profile_start(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::profile_start(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("profile-start")
    }
}

/// `profile-stop` — end the capture and return the report.
pub async fn profile_stop(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::profile_stop(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::profile_stop(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("profile-stop")
    }
}

/// `heap-snapshot` — capture a memory/heap report.
pub async fn heap_snapshot(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::heap_snapshot(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::heap_snapshot(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("heap-snapshot")
    }
}

/// `attach-devtools` — make the renderer inspectable and return how to attach.
pub async fn attach_devtools(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::attach_devtools(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::attach_devtools(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("attach-devtools")
    }
}

/// `exec-js-isolated` — run JS in a world isolated from page globals.
pub async fn exec_js_isolated(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::exec_js_isolated(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::exec_js_isolated(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("exec-js-isolated")
    }
}

/// `trace` — capture a chrome-tracing-compatible timeline for the given
/// duration. Windows: CDP `Tracing.start` / `Tracing.dataCollected` /
/// `Tracing.tracingComplete` over the WebView2 browser process (so it survives
/// a wedged renderer). macOS: Performance API + paint timing snapshot via the
/// in-page injection (degraded — won't survive a fully hung renderer).
pub async fn trace(args: &Value) -> Result<Value> {
    #[cfg(target_os = "macos")]
    {
        macos::trace(args).await
    }
    #[cfg(target_os = "windows")]
    {
        windows::trace(args).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = args;
        unsupported("trace")
    }
}

/// `debugger-arm` — pre-arm V8 so the next script execution pauses and we can
/// capture call frames out-of-band. Windows-only; macOS WebKit's debugger isn't
/// programmatically accessible from Rust (use `attach-devtools` instead).
pub async fn debugger_arm(args: &Value) -> Result<Value> {
    #[cfg(target_os = "windows")]
    {
        windows::debugger_arm(args).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        Ok(json!({
            "status": "failed",
            "error": "UNSUPPORTED_PLATFORM",
            "message": "debugger-arm needs programmatic CDP Debugger access; macOS WebKit's debugger \
                       is not exposed to native code. Use `attach-devtools` and set breakpoints \
                       interactively in the Web Inspector."
        }))
    }
}

/// `debugger-snapshot` — read out the call frames captured by `debugger-arm`
/// since the last snapshot.
pub async fn debugger_snapshot(args: &Value) -> Result<Value> {
    #[cfg(target_os = "windows")]
    {
        windows::debugger_snapshot(args).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        unsupported("debugger-snapshot")
    }
}

/// `debugger-disarm` — disable the V8 debugger and drop any buffered frames.
pub async fn debugger_disarm(args: &Value) -> Result<Value> {
    #[cfg(target_os = "windows")]
    {
        windows::debugger_disarm(args).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        unsupported("debugger-disarm")
    }
}

/// `sw-console` — read the Service Worker's own console buffer.
///
/// Unlike `console-dump` this is not an in-page read: a service worker has its
/// own isolate, so the page's capture can never see it. Windows attaches a CDP
/// session to the `service_worker` target at daemon start and buffers its
/// `Runtime.consoleAPICalled` / `Runtime.exceptionThrown` events; other
/// platforms have no equivalent hook.
pub async fn sw_console(args: &Value) -> Result<Value> {
    let level = args.get("level").and_then(|v| v.as_str());
    let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
    let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(crate::sw_console::snapshot(level, max, clear))
}

// ===========================================================================
// Portable tools (read in-page buffers; work on every platform)
// ===========================================================================

/// `console-dump` — dump the console ring buffer, with explicit hung-renderer
/// detection.
///
/// Shares its capture with `console_logs`. When an out-of-band transport is
/// armed (CDP on Windows, WebKit system-console logging on macOS) this is a
/// daemon-memory read and keeps answering through a wedged renderer; only the
/// in-page fallback can be defeated by a hang, and that case is labelled as
/// such rather than reported as an empty console.
pub async fn console_dump(args: &Value) -> Result<Value> {
    let forwarded = json!({
        "site": args.get("site").cloned().unwrap_or(Value::Null),
        "level": args.get("level").cloned().unwrap_or(Value::Null),
        "max": args.get("max").and_then(|v| v.as_u64()).unwrap_or(500),
        "clear": args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false),
    });

    let source = crate::console::transport().as_str();

    match crate::tools::definitions::tool_console_logs(&forwarded).await {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("source".into(), json!(source));
                if obj.get("status").and_then(|s| s.as_str()) == Some("failed") {
                    // Only reachable on the in-page fallback: the out-of-band
                    // transports never touch the renderer.
                    obj.insert("renderer".into(), json!("unresponsive"));
                    obj.insert(
                        "hint".into(),
                        json!("The in-page console fallback cannot be read while the renderer is hung. Run `pause` for an out-of-band native stack."),
                    );
                }
            }
            Ok(v)
        }
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "CONSOLE_DUMP_ERROR",
            "source": source,
            "renderer": "unresponsive",
            "message": e.to_string(),
            "hint": "Run `pause` for an out-of-band native stack.",
        })),
    }
}

/// `network-log` — list captured network requests, flagging ones still pending
/// (no response seen), which is a common hang trigger.
///
/// Note: this is the in-page fetch/XHR capture, so it does *not* include
/// HTML-parser requests (`<script src>`, stylesheets, images). Those are only
/// visible via real CDP `Network.*` events, which WebView2 request/response
/// can't stream — a documented platform limitation.
pub async fn network_log(args: &Value) -> Result<Value> {
    let since_ms = args.get("since_ms").and_then(|v| v.as_u64());
    let seconds = since_ms.map(|ms| (ms / 1000).max(1)).unwrap_or(60);
    let forwarded = json!({
        "site": args.get("site").cloned().unwrap_or(Value::Null),
        "seconds": seconds,
        "url_pattern": args.get("url_pattern").cloned().unwrap_or(Value::Null),
        "max_count": args.get("max").and_then(|v| v.as_u64()),
    });

    match crate::tools::definitions::tool_get_captured_requests(&forwarded).await {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("source".into(), json!("in_page_fetch_xhr"));
                // Surface pending requests (captured request with no response/status).
                if let Some(reqs) = obj.get("requests").and_then(|r| r.as_array()) {
                    let pending: Vec<Value> = reqs
                        .iter()
                        .filter(|r| {
                            let has_status = r.get("status").map(|s| !s.is_null()).unwrap_or(false);
                            let done = r
                                .get("responseReceived")
                                .or_else(|| r.get("response"))
                                .map(|s| !s.is_null())
                                .unwrap_or(false);
                            !has_status && !done
                        })
                        .cloned()
                        .collect();
                    if !pending.is_empty() {
                        obj.insert("pending_count".into(), json!(pending.len()));
                        obj.insert("pending".into(), json!(pending));
                    }
                }
                obj.insert(
                    "note".into(),
                    json!("Captures fetch/XHR only. HTML-parser requests (<script src>, CSS, img) need CDP Network.* (Windows build)."),
                );
            }
            Ok(v)
        }
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "NETWORK_LOG_ERROR",
            "source": "in_page_fetch_xhr",
            "renderer": "unresponsive",
            "message": e.to_string(),
            "hint": "Run `pause` for an out-of-band native stack.",
        })),
    }
}

/// `wait-for-condition` — poll a JS expression until it's truthy or we time out.
/// Each probe uses a short per-call timeout so a wedged renderer is detected
/// rather than blocking the whole wait.
pub async fn wait_for_condition(args: &Value) -> Result<Value> {
    let script = args
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`script` is required"))?;
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(30000);
    let interval_ms = args
        .get("interval_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(250)
        .max(50);

    // Wrap so any return value collapses to a strict boolean and thrown errors
    // count as "not yet" rather than aborting the wait.
    let probe = format!(
        "try {{ return !!((function() {{ {script} }})()); }} catch (e) {{ return false; }}"
    );

    let start = std::time::Instant::now();
    let mut polls: u64 = 0;
    let mut last_error: Option<String> = None;

    loop {
        polls += 1;
        match crate::webview_execute_js(&probe, Some(2000)).await {
            Ok(v) if v.as_bool() == Some(true) => {
                return Ok(json!({
                    "status": "completed",
                    "met": true,
                    "polls": polls,
                    "elapsed_ms": start.elapsed().as_millis(),
                }));
            }
            Ok(_) => {}
            Err(e) => last_error = Some(e),
        }

        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Ok(json!({
                "status": "completed",
                "met": false,
                "polls": polls,
                "elapsed_ms": start.elapsed().as_millis(),
                "last_error": last_error,
                "hint": last_error.is_some().then_some(
                    "Probes are erroring — the renderer may be hung. Try `pause`."
                ),
            }));
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}
