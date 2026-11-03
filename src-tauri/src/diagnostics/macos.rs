//! macOS renderer diagnostics.
//!
//! WKWebView renders out-of-process (`com.apple.WebKit.WebContent`). We grab the
//! renderer's PID via the private `-[WKWebView _webProcessIdentifier]` selector —
//! consistent with this project already relying on WKWebView private APIs — and
//! then read that process externally with `sample` / `vmmap`. Because the sampler
//! is a separate process, these work even when the renderer's main thread is fully
//! wedged (exactly the case `execute_js` can't observe).

use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::ProfileSession;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn temp_file(site: &str, kind: &str, ext: &str) -> PathBuf {
    // Keep the site id filesystem-safe.
    let safe: String = site
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("dalzzack-{safe}-{kind}-{}.{ext}", now_ms()))
}

/// Truncate a long report to a JSON-friendly excerpt (char-boundary safe).
fn excerpt(s: &str, max: usize) -> Value {
    if s.chars().count() <= max {
        return json!(s);
    }
    let head: String = s.chars().take(max).collect();
    json!(format!(
        "{head}\n…[truncated; full report at report_path, {} bytes total]",
        s.len()
    ))
}

/// Run a diagnostic CLI tool to completion off the async runtime.
async fn run_tool(prog: &'static str, args: Vec<String>) -> Result<std::process::Output, String> {
    tokio::task::spawn_blocking(move || std::process::Command::new(prog).args(&args).output())
        .await
        .map_err(|e| format!("join error: {e}"))?
        .map_err(|e| format!("failed to spawn {prog}: {e}"))
}

/// Resolve the WKWebView's WebContent PID via `_webProcessIdentifier`.
///
/// Returns `Err` if the webview isn't up, the selector is unavailable, or the
/// renderer process hasn't been spawned yet (pid <= 0).
pub(crate) async fn web_process_pid() -> Result<i32, String> {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};
    use std::sync::{Arc, Condvar, Mutex};

    let window = crate::get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    let holder: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let condvar = Arc::new(Condvar::new());
    let holder_cb = Arc::clone(&holder);
    let condvar_cb = Arc::clone(&condvar);

    window
        .with_webview(move |wv| unsafe {
            let ptr: *mut AnyObject = wv.inner().cast();
            // Guard against an OS/runtime where the private selector is gone:
            // an unrecognized-selector send would abort the process.
            let responds: bool = msg_send![ptr, respondsToSelector: sel!(_webProcessIdentifier)];
            let pid: i32 = if responds {
                msg_send![ptr, _webProcessIdentifier]
            } else {
                -2 // sentinel: selector unavailable
            };
            if let Ok(mut g) = holder_cb.lock() {
                *g = Some(pid);
            }
            condvar_cb.notify_one();
        })
        .map_err(|e| format!("with_webview error: {e:?}"))?;

    let guard = holder.lock().map_err(|_| "lock poisoned")?;
    let (guard, timed_out) = condvar
        .wait_timeout_while(guard, std::time::Duration::from_secs(3), |v| v.is_none())
        .map_err(|_| "condvar wait failed")?;
    if timed_out.timed_out() {
        return Err("timed out querying _webProcessIdentifier (UI thread busy?)".into());
    }
    match guard.unwrap_or(-1) {
        -2 => Err("_webProcessIdentifier unavailable on this WKWebView".into()),
        p if p <= 0 => Err("WebContent process not running yet (navigate first)".into()),
        p => Ok(p),
    }
}

fn pid_error(site_tool: &str, e: String) -> Value {
    json!({
        "status": "failed",
        "error": "WEBCONTENT_PID",
        "tool": site_tool,
        "message": e,
    })
}

// ---------------------------------------------------------------------------
// pause — instant stack capture of the renderer
// ---------------------------------------------------------------------------

pub async fn pause(args: &Value) -> Result<Value> {
    let site = super::site_key(args);
    let dur_ms = args
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(1200);
    // sample wants whole seconds; clamp to a sane 1..=30s window.
    let secs = (dur_ms.div_ceil(1000).clamp(1, 30)) as u32;

    let pid = match web_process_pid().await {
        Ok(p) => p,
        Err(e) => return Ok(pid_error("pause", e)),
    };

    let out = args
        .get("output")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| temp_file(&site, "sample", "txt"));

    // `sample <pid> <secs> -file <out> -mayDie` — -mayDie lets it bail cleanly if
    // the target exits mid-sample. For a wedged renderer every sample lands on the
    // same frame, which is exactly the stuck location we want.
    let res = run_tool(
        "/usr/bin/sample",
        vec![
            pid.to_string(),
            secs.to_string(),
            "-file".into(),
            out.display().to_string(),
            "-mayDie".into(),
        ],
    )
    .await;

    match res {
        Ok(output) => {
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            if text.is_empty() {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                return Ok(json!({
                    "status": "failed",
                    "error": "SAMPLE_EMPTY",
                    "target_pid": pid,
                    "message": format!("sample produced no report. stderr: {stderr}"),
                }));
            }
            Ok(json!({
                "status": "completed",
                "tool": "sample",
                "target_pid": pid,
                "duration_ms": secs as u64 * 1000,
                "report_path": out.display().to_string(),
                "report_excerpt": excerpt(&text, 8000),
            }))
        }
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "SAMPLE_FAILED",
            "target_pid": pid,
            "message": e,
        })),
    }
}

// ---------------------------------------------------------------------------
// profile-start / profile-stop — CPU sampling over a window
// ---------------------------------------------------------------------------

pub async fn profile_start(args: &Value) -> Result<Value> {
    let site = super::site_key(args);

    // Refuse to silently clobber an in-flight session.
    if super::PROFILE_SESSIONS
        .lock()
        .map(|m| m.contains_key(&site))
        .unwrap_or(false)
    {
        return Ok(json!({
            "status": "failed",
            "error": "PROFILE_ALREADY_RUNNING",
            "message": "A profile is already running for this instance. Call profile-stop first.",
        }));
    }

    let pid = match web_process_pid().await {
        Ok(p) => p,
        Err(e) => return Ok(pid_error("profile-start", e)),
    };

    let max_secs = args
        .get("max_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(600)
        .clamp(1, 3600);
    let file = temp_file(&site, "profile", "txt");

    // Background `sample` writing to a file; profile-stop SIGINTs it to flush.
    let child = std::process::Command::new("/usr/bin/sample")
        .arg(pid.to_string())
        .arg(max_secs.to_string())
        .arg("-file")
        .arg(&file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match child {
        Ok(c) => {
            let sample_pid = c.id();
            if let Ok(mut sessions) = super::PROFILE_SESSIONS.lock() {
                sessions.insert(
                    site,
                    ProfileSession {
                        sample_pid,
                        target_pid: pid,
                        file: file.clone(),
                        started: Instant::now(),
                        child: c,
                    },
                );
            }
            Ok(json!({
                "status": "started",
                "tool": "sample",
                "target_pid": pid,
                "sample_pid": sample_pid,
                "max_seconds": max_secs,
                "report_path": file.display().to_string(),
                "hint": "Reproduce the slowness, then run profile-stop.",
            }))
        }
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "SAMPLE_SPAWN_FAILED",
            "message": e.to_string(),
        })),
    }
}

pub async fn profile_stop(args: &Value) -> Result<Value> {
    let site = super::site_key(args);

    let session = super::PROFILE_SESSIONS
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&site));
    let Some(session) = session else {
        return Ok(json!({
            "status": "failed",
            "error": "NO_ACTIVE_PROFILE",
            "message": "No profile is running for this instance. Call profile-start first.",
        }));
    };

    let ProfileSession {
        sample_pid,
        target_pid,
        file,
        started,
        mut child,
    } = session;
    let elapsed_ms = started.elapsed().as_millis();

    // SIGINT makes `sample` finish early and flush its report to the file.
    let _ = std::process::Command::new("kill")
        .arg("-INT")
        .arg(sample_pid.to_string())
        .status();

    // Reap the child so it doesn't linger as a zombie.
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;

    // The flush races the read; retry briefly until the file has content.
    let mut text = String::new();
    for _ in 0..10 {
        text = std::fs::read_to_string(&file).unwrap_or_default();
        if !text.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    if text.is_empty() {
        return Ok(json!({
            "status": "failed",
            "error": "PROFILE_EMPTY",
            "target_pid": target_pid,
            "elapsed_ms": elapsed_ms,
            "report_path": file.display().to_string(),
            "message": "sample wrote no report (renderer may have exited).",
        }));
    }

    Ok(json!({
        "status": "completed",
        "tool": "sample",
        "target_pid": target_pid,
        "elapsed_ms": elapsed_ms,
        "report_path": file.display().to_string(),
        "report_excerpt": excerpt(&text, 8000),
    }))
}

// ---------------------------------------------------------------------------
// heap-snapshot — native memory report (NOT a V8 .heapsnapshot)
// ---------------------------------------------------------------------------

pub async fn heap_snapshot(args: &Value) -> Result<Value> {
    let site = super::site_key(args);
    let pid = match web_process_pid().await {
        Ok(p) => p,
        Err(e) => return Ok(pid_error("heap-snapshot", e)),
    };

    let out = args
        .get("output")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| temp_file(&site, "memory", "txt"));

    // vmmap --summary is reliable for same-user processes; footprint adds a
    // dirty/physical rollup but may not always be permitted — best-effort.
    let vmmap = run_tool("/usr/bin/vmmap", vec!["--summary".into(), pid.to_string()]).await;
    let footprint = run_tool("/usr/bin/footprint", vec![pid.to_string()]).await;

    let mut combined = String::new();
    if let Ok(o) = &vmmap {
        combined.push_str("===== vmmap --summary =====\n");
        combined.push_str(&String::from_utf8_lossy(&o.stdout));
        combined.push('\n');
    }
    if let Ok(o) = &footprint {
        if o.status.success() {
            combined.push_str("\n===== footprint =====\n");
            combined.push_str(&String::from_utf8_lossy(&o.stdout));
        }
    }

    if combined.trim().is_empty() {
        let err = vmmap.err().unwrap_or_else(|| "no output".into());
        return Ok(json!({
            "status": "failed",
            "error": "MEMORY_REPORT_FAILED",
            "target_pid": pid,
            "message": err,
        }));
    }

    let _ = std::fs::write(&out, &combined);

    Ok(json!({
        "status": "completed",
        "report_type": "native_memory",
        "target_pid": pid,
        "report_path": out.display().to_string(),
        "report_excerpt": excerpt(&combined, 8000),
        "note": "This is native process memory (vmmap/footprint), NOT a V8 JS .heapsnapshot. \
                 WKWebView can't export a JS heap snapshot out-of-band — use `attach-devtools` \
                 (Safari Web Inspector) or the Windows build (CDP HeapProfiler) for that.",
    }))
}

// ---------------------------------------------------------------------------
// attach-devtools — make the renderer inspectable for Safari Web Inspector
// ---------------------------------------------------------------------------

pub async fn attach_devtools(_args: &Value) -> Result<Value> {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};
    use std::sync::{Arc, Condvar, Mutex};

    let window = crate::get_main_window()
        .await
        .ok_or_else(|| anyhow::anyhow!("WEBVIEW_NOT_FOUND"))?;

    let holder: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let condvar = Arc::new(Condvar::new());
    let holder_cb = Arc::clone(&holder);
    let condvar_cb = Arc::clone(&condvar);

    let cb_result = window.with_webview(move |wv| unsafe {
        let ptr: *mut AnyObject = wv.inner().cast();
        // -[WKWebView setInspectable:] exists on macOS 13.3+.
        let supported: bool = msg_send![ptr, respondsToSelector: sel!(setInspectable:)];
        if supported {
            let _: () = msg_send![ptr, setInspectable: true];
        }
        if let Ok(mut g) = holder_cb.lock() {
            *g = Some(supported);
        }
        condvar_cb.notify_one();
    });

    if let Err(e) = cb_result {
        return Ok(json!({
            "status": "failed",
            "error": "WITH_WEBVIEW_FAILED",
            "message": format!("{e:?}"),
        }));
    }

    let supported = {
        let guard = holder
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;
        let (guard, timed) = condvar
            .wait_timeout_while(guard, std::time::Duration::from_secs(3), |v| v.is_none())
            .map_err(|_| anyhow::anyhow!("condvar wait failed"))?;
        if timed.timed_out() {
            return Ok(
                json!({"status":"failed","error":"UI_BUSY","message":"timed out enabling inspectable"}),
            );
        }
        guard.unwrap_or(false)
    };

    let pid = web_process_pid().await.ok();

    if !supported {
        return Ok(json!({
            "status": "failed",
            "error": "INSPECTABLE_UNSUPPORTED",
            "web_process_pid": pid,
            "message": "setInspectable: is unavailable (needs macOS 13.3+).",
        }));
    }

    Ok(json!({
        "status": "completed",
        "inspectable": true,
        "web_process_pid": pid,
        "transport": "safari_web_inspector",
        "instructions": [
            "Enable Safari's Develop menu: Safari ▸ Settings ▸ Advanced ▸ 'Show features for web developers'.",
            "In Safari, open: Develop ▸ <this Mac> ▸ look for the dalzzack window/page, and click it.",
            "WKWebView exposes Apple's Web Inspector protocol, not a CDP ws:// URL — there is no remote-debugging port to attach Chrome DevTools to."
        ],
    }))
}

// ---------------------------------------------------------------------------
// exec-js-isolated — run JS in a content world isolated from page globals
// ---------------------------------------------------------------------------

pub async fn exec_js_isolated(args: &Value) -> Result<Value> {
    let script = args
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`script` is required"))?;
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(30000) as u32;

    match evaluate_in_content_world(script, timeout_ms).await {
        Ok(value) => Ok(json!({
            "status": "completed",
            "world": "default_client_world",
            "value": value,
        })),
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "ISOLATED_EXEC_ERROR",
            "world": "default_client_world",
            "message": e,
            "note": "Isolated from page globals, but still the same renderer thread — \
                     a fully hung renderer will still time out. Use `pause` for that.",
        })),
    }
}

/// Evaluate JS in `WKContentWorld.defaultClientWorld` (isolated from page JS).
async fn evaluate_in_content_world(script: &str, timeout: u32) -> Result<Value, String> {
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{class, msg_send};
    use std::sync::{Arc, Condvar, Mutex};

    let window = crate::get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    // Same JSON-wrapping contract as webview_execute_js so results parse uniformly.
    let wrapped = format!(
        r#"(function() {{
            try {{
                var __r = (function() {{ {script} }})();
                if (__r && typeof __r.then === 'function') {{
                    return JSON.stringify({{ ok: false, error: 'Promise result not supported in sync mode.' }});
                }}
                return JSON.stringify({{ ok: true, value: __r }});
            }} catch (e) {{
                return JSON.stringify({{ ok: false, error: e.toString() }});
            }}
        }})()"#,
        script = script
    );

    let result_holder: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let condvar = Arc::new(Condvar::new());
    let result_cb = Arc::clone(&result_holder);
    let condvar_cb = Arc::clone(&condvar);

    window
        .with_webview(move |wv| unsafe {
            let webview_ptr: *mut AnyObject = wv.inner().cast();

            let result_block = Arc::clone(&result_cb);
            let condvar_block = Arc::clone(&condvar_cb);
            let block =
                block2::StackBlock::new(move |result: *mut AnyObject, error: *mut AnyObject| {
                    let stored = if !error.is_null() {
                        let desc: *mut AnyObject = msg_send![error, localizedDescription];
                        let utf8: *const std::ffi::c_char = msg_send![desc, UTF8String];
                        if utf8.is_null() {
                            Err("Unknown error".to_string())
                        } else {
                            Err(std::ffi::CStr::from_ptr(utf8).to_string_lossy().to_string())
                        }
                    } else if result.is_null() {
                        Ok("null".to_string())
                    } else {
                        let utf8: *const std::ffi::c_char = msg_send![result, UTF8String];
                        if utf8.is_null() {
                            Ok("null".to_string())
                        } else {
                            Ok(std::ffi::CStr::from_ptr(utf8).to_string_lossy().to_string())
                        }
                    };
                    if let Ok(mut g) = result_block.lock() {
                        *g = Some(stored);
                    }
                    condvar_block.notify_one();
                });
            let block = block.copy();

            // Build the NSString script argument.
            let ns_string_cstr = c"NSString";
            let ns_string_class: *mut AnyObject =
                msg_send![AnyClass::get(ns_string_cstr).unwrap(), alloc];
            let script_cstr = std::ffi::CString::new(wrapped.as_str()).unwrap_or_default();
            let ns_script: *mut AnyObject =
                msg_send![ns_string_class, initWithUTF8String: script_cstr.as_ptr()];

            // The isolated client world + nil frame (main frame).
            let world: *mut AnyObject = msg_send![class!(WKContentWorld), defaultClientWorld];
            let nil_frame: *mut AnyObject = std::ptr::null_mut();

            let _: () = msg_send![
                webview_ptr,
                evaluateJavaScript: ns_script,
                inFrame: nil_frame,
                inContentWorld: world,
                completionHandler: &*block
            ];
        })
        .map_err(|e| format!("with_webview error: {e:?}"))?;

    let result_str = {
        let guard = result_holder.lock().map_err(|_| "lock poisoned")?;
        let (guard, timed_out) = condvar
            .wait_timeout_while(
                guard,
                std::time::Duration::from_millis(timeout as u64),
                |r| r.is_none(),
            )
            .map_err(|_| "condvar wait failed")?;
        if timed_out.timed_out() {
            return Err(format!("execution timeout ({timeout}ms)"));
        }
        guard.clone().ok_or("no result received")?
    }?;

    // Unwrap the { ok, value | error } envelope.
    match serde_json::from_str::<Value>(&result_str) {
        Ok(parsed) => {
            if parsed["ok"].as_bool() == Some(true) {
                Ok(parsed["value"].clone())
            } else {
                Err(parsed["error"]
                    .as_str()
                    .unwrap_or("Unknown error")
                    .to_string())
            }
        }
        Err(_) => Ok(Value::String(result_str)),
    }
}

// ---------------------------------------------------------------------------
// `trace` — chrome-tracing-shaped timeline capture.
//
// WKWebView has no programmatic CDP, so true browser-process Tracing isn't
// available on macOS. The next-best signal is what the page already collected
// in the Performance buffers: navigation timing, paint timing, long tasks,
// resource timing, user marks/measures. We assemble those into chrome-tracing
// `traceEvents` format so the same downstream viewer (`chrome://tracing`,
// perfetto) opens both Windows and macOS captures.
//
// Limitations vs Windows CDP Tracing: no V8 internals, no GC events, no layout
// pass details — and the read itself happens through the renderer, so a fully
// hung renderer will time out. Documented in the response.
// ---------------------------------------------------------------------------

pub async fn trace(args: &Value) -> Result<Value> {
    let duration_ms = args
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000)
        .min(60_000);

    let site = args
        .get("site")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let out = args
        .get("output")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| temp_file(&site, "trace", "json"));

    // Install a long-task observer so we can capture jank that happened during
    // `duration_ms`. The observer pushes into `window.__tw_trace` which we
    // pull at the end. Cheap to re-install — guard against duplicate run.
    let install_script = r#"
        if (!window.__tw_trace_installed) {
            window.__tw_trace_installed = true;
            window.__tw_trace = [];
            try {
                var lo = new PerformanceObserver(function(list){
                    list.getEntries().forEach(function(e){
                        window.__tw_trace.push({
                            kind: 'longtask',
                            name: e.name,
                            startTime: e.startTime,
                            duration: e.duration,
                        });
                    });
                });
                lo.observe({ type: 'longtask', buffered: true });
            } catch(e){}
        }
        window.__tw_trace_start = performance.now();
        return performance.now();
    "#;

    let trace_started = crate::webview_execute_js(install_script, Some(2000))
        .await
        .map_err(|e| {
            anyhow::anyhow!("Could not install trace observer (renderer may be hung): {e}")
        })?;

    let started_at = trace_started.as_f64().unwrap_or(0.0);
    tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;

    // Pull the buffered events plus the standard Performance entries.
    let collect_script = r#"
        var lt = window.__tw_trace || [];
        var nav = performance.getEntriesByType('navigation').map(function(e){return {kind:'nav', name:e.name, startTime:e.startTime, duration:e.duration, type:e.type};});
        var paint = performance.getEntriesByType('paint').map(function(e){return {kind:'paint', name:e.name, startTime:e.startTime, duration:e.duration||0};});
        var res = performance.getEntriesByType('resource').map(function(e){return {kind:'resource', name:e.name, startTime:e.startTime, duration:e.duration, initiator:e.initiatorType, transfer:e.transferSize};});
        var marks = performance.getEntriesByType('mark').map(function(e){return {kind:'mark', name:e.name, startTime:e.startTime};});
        var measures = performance.getEntriesByType('measure').map(function(e){return {kind:'measure', name:e.name, startTime:e.startTime, duration:e.duration};});
        return { events: lt.concat(nav).concat(paint).concat(res).concat(marks).concat(measures), now: performance.now() };
    "#;

    let collected = crate::webview_execute_js(collect_script, Some(3000))
        .await
        .map_err(|e| {
            anyhow::anyhow!("Could not collect trace events (renderer wedged during trace): {e}")
        })?;

    let events = collected
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let now = collected.get("now").and_then(|v| v.as_f64()).unwrap_or(0.0);

    // Repack into chrome-tracing "traceEvents" format. Each entry becomes a
    // single phase=X (instant) or phase=I (duration) event in the timeline.
    let trace_events: Vec<Value> = events
        .iter()
        .map(|e| {
            let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let start = e.get("startTime").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let dur = e.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            // chrome-tracing uses microseconds; performance.now() is ms.
            let ts = (start * 1000.0) as u64;
            if dur > 0.0 {
                json!({
                    "name": format!("{kind}: {name}"),
                    "cat": kind,
                    "ph": "X",
                    "ts": ts,
                    "dur": (dur * 1000.0) as u64,
                    "pid": 1,
                    "tid": 1,
                })
            } else {
                json!({
                    "name": format!("{kind}: {name}"),
                    "cat": kind,
                    "ph": "i",
                    "ts": ts,
                    "s": "g",
                    "pid": 1,
                    "tid": 1,
                })
            }
        })
        .collect();

    let payload = json!({
        "traceEvents": trace_events,
        "displayTimeUnit": "ms",
        "otherData": {
            "transport": "in_page_performance_api",
            "started_at_perfnow_ms": started_at,
            "ended_at_perfnow_ms": now,
            "platform": "macos",
            "note": "macOS fallback — WKWebView has no programmatic CDP. Capture is in-page, so a fully hung renderer will not return events.",
        },
    });

    let bytes = serde_json::to_vec_pretty(&payload).unwrap_or_default();
    std::fs::write(&out, &bytes).ok();

    Ok(json!({
        "status": "completed",
        "transport": "in_page_performance_api",
        "duration_ms": duration_ms,
        "event_count": trace_events.len(),
        "output_path": out.display().to_string(),
        "bytes_written": bytes.len(),
        "note": "Open in chrome://tracing or perfetto. macOS path is degraded vs Windows CDP Tracing.",
    }))
}
