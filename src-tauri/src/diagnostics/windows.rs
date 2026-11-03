//! Windows renderer diagnostics via WebView2 CDP.
//!
//! WebView2's `CallDevToolsProtocolMethod` is a genuine Chrome DevTools Protocol
//! transport that runs out-of-band from the renderer's script execution, so these
//! request/response CDP calls answer even while page JS is busy.
//!
//! Scope note: this build sticks to *request/response* CDP (and a couple of direct
//! WebView2 methods). The domains that only deliver data via CDP *events*
//! (`HeapProfiler.addHeapSnapshotChunk` streaming, `Debugger.paused` call frames,
//! `Console.messageAdded`, `Network.*`) need an `add_DevToolsProtocolEventReceived`
//! sink, which is a follow-up. Where that bites, each tool says so honestly and the
//! portable in-page paths (`console-dump`, `network-log`) still apply.

use anyhow::Result;
use serde_json::{json, Value};
use tauri::WebviewWindow;

use super::symbols;

/// Call a CDP method and return the parsed `returnObjectAsJson` payload.
///
/// Thin wrapper over [`crate::cdp::call`] with this module's historical 10s
/// timeout, so the existing diagnostics call sites stay unchanged.
async fn call_cdp(
    window: &WebviewWindow,
    method: &str,
    params_json: &str,
) -> Result<Value, String> {
    crate::cdp::call(window, method, params_json, crate::cdp::DEFAULT_TIMEOUT).await
}

async fn main_window() -> Result<WebviewWindow> {
    crate::get_main_window()
        .await
        .ok_or_else(|| anyhow::anyhow!("WEBVIEW_NOT_FOUND: WebView window not available"))
}

// ---------------------------------------------------------------------------
// pause — current JS stack (non-destructive). Hung-renderer caveat documented.
// ---------------------------------------------------------------------------

pub async fn pause(args: &Value) -> Result<Value> {
    let window = main_window().await?;
    let site = args
        .get("site")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let opts = DumpOptions {
        full_memory: args
            .get("full_memory")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        only_pid: args
            .get("only_pid")
            .and_then(|v| v.as_u64())
            .map(|p| p as u32),
        all_processes: args
            .get("all_processes")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        max_bytes: args.get("max_bytes").and_then(|v| v.as_u64()),
    };

    // Sampling: several dumps of ONE process, spaced out. A single sample
    // cannot tell "spinning here" from "happened to be here", and dumping every
    // process each round leaves dozens of files to sort through by hand.
    let samples = args
        .get("samples")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 64);
    let interval_ms = args
        .get("interval_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(250)
        .min(60_000);
    let symbolize = args
        .get("symbolize")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let all_threads = args
        .get("all_threads")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sym_opts = symbols::SymbolOptions {
        download: args
            .get("download_symbols")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        cache_dir: args
            .get("symbol_cache")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(symbols::SymbolOptions::default_cache),
    };

    // ALWAYS capture renderer minidumps out-of-band first. This is the real
    // out-of-band stack capture — driven by Windows kernel debug APIs, NOT by
    // V8 / the renderer's main thread — so it works on a fully wedged renderer.
    // Runs on a blocking thread so the async runtime stays free.
    let safe: String = site
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let dump_dir = std::env::temp_dir().join(format!("dalzzack-{safe}-pause"));

    // Ask WebView2 which browser process is ours before walking the tree.
    let root_pid = browser_process_id().await;

    // Target selection resolves symbols too, but never downloads: picking a
    // process to sample must not stall on a symbol server.
    let sym_opts_probe = symbols::SymbolOptions {
        download: false,
        cache_dir: sym_opts.cache_dir.clone(),
    };

    let mut capture = CaptureResult::default();
    let mut sample_dumps: Vec<std::path::PathBuf> = Vec::new();
    // Which process the sampling loop settles on, and why.
    let mut sampled_pid: Option<u32> = opts.only_pid;
    let mut sample_target_reason: Option<String> = None;

    for round in 0..samples {
        let mut round_opts = opts.clone();
        // After the first round, sampling follows ONE process — that's what
        // keeps the file count sane. Which one is decided by evidence below
        // rather than by size: several daemons share a WebView2 browser
        // process, so the biggest renderer is often somebody else's.
        if samples > 1 {
            round_opts.only_pid = sampled_pid;
        }
        let dump_dir_clone = dump_dir.clone();
        let this = tokio::task::spawn_blocking(move || {
            capture_renderer_minidumps(&dump_dir_clone, &round_opts, root_pid)
        })
        .await
        .unwrap_or_default();

        sample_dumps.extend(this.dumps.iter().map(|d| d.path.clone()));

        // First round with no explicit target: pick the process that is
        // actually working. Symbols are resolved here too, cheaply (exports
        // only): classifying by module alone would count an allocating thread
        // as idle, which is the opposite of what we're hunting for.
        if samples > 1 && sampled_pid.is_none() {
            let mut best: Option<(u32, usize)> = None;
            for d in &this.dumps {
                if let Ok(contents) = symbols::read_dump(&d.path) {
                    let addrs: Vec<u64> = contents.threads.iter().map(|t| t.rip).collect();
                    let resolved =
                        symbols::symbolize(&contents, &addrs, &sym_opts_probe).unwrap_or_default();
                    let busy = contents
                        .threads
                        .iter()
                        .zip(resolved.iter())
                        .filter(|(t, r)| !contents.is_waiting(t, r.symbol.as_deref()))
                        .count();
                    if busy > 0 && best.is_none_or(|(_, b)| busy > b) {
                        best = Some((d.pid, busy));
                    }
                }
            }
            match best {
                Some((pid, busy)) => {
                    sampled_pid = Some(pid);
                    sample_target_reason = Some(format!(
                        "PID {pid} had the most busy threads ({busy}) in the first sample"
                    ));
                }
                None => {
                    sampled_pid = this.suggested_pid;
                    sample_target_reason = Some(format!(
                        "no process had a busy thread in the first sample; fell back to the \
                         suggested renderer ({:?})",
                        this.suggested_pid
                    ));
                }
            }
        }

        capture = this;

        if round + 1 < samples {
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    }
    let dumps = &capture.dumps;

    // Then try the cheap CDP path for a JS-level stack. If responsive we get
    // both signals; if not, we still have the minidumps.
    let params = json!({
        "expression": "(function(){ try { throw new Error('tw-stack'); } catch (e) { return e.stack; } })()",
        "returnByValue": true,
    })
    .to_string();

    let (js_stack, cdp_status, cdp_err) = match call_cdp(&window, "Runtime.evaluate", &params).await
    {
        Ok(v) => (
            crate::cdp::result_at(&v, "result/value")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            "ok",
            None,
        ),
        Err(e) => (String::new(), "wedged", Some(e)),
    };

    let dump_paths: Vec<String> = dumps.iter().map(|d| d.path.display().to_string()).collect();
    let dump_details: Vec<Value> = dumps
        .iter()
        .map(|d| {
            json!({
                "pid": d.pid,
                "type": d.process_type,
                "path": d.path.display().to_string(),
                "bytes": d.bytes,
                "working_set_bytes": d.working_set_bytes,
                "over_max_bytes": d.over_max,
            })
        })
        .collect();
    let candidates: Vec<Value> = capture
        .candidates
        .iter()
        .map(|c| {
            json!({
                "pid": c.pid,
                "type": c.process_type,
                "working_set_bytes": c.working_set_bytes,
            })
        })
        .collect();

    let note = if opts.full_memory {
        "Full-memory minidumps: heap pages are included, so page/JS strings are searchable in the \
         dump (`strings`, WinDbg `s -a`). Expect a file roughly the size of the process working \
         set. Open .dmp in WinDbg or Visual Studio."
    } else {
        "Minidumps are TRUE out-of-band: captured via Windows kernel debug APIs so they work even \
         when the renderer's main thread is fully wedged. This is a STACKS-ONLY dump — no heap. \
         Pass `--full-memory` (with `--only-pid`) when you need to know what is in memory. Open \
         .dmp in WinDbg, Visual Studio, or DebugView."
    };

    // Symbolization: turn the dumps into named frames rather than a path to a
    // file the caller has to open in WinDbg.
    let (symbolized, symbol_histogram, symbol_error) = if symbolize {
        let paths = sample_dumps.clone();
        let cache = sym_opts.cache_dir.clone();
        let download = sym_opts.download;
        match tokio::task::spawn_blocking(move || {
            let opts = symbols::SymbolOptions {
                download,
                cache_dir: cache,
            };
            let mut per_dump = Vec::new();
            let mut first_err: Option<String> = None;
            for p in &paths {
                match symbols::analyze_dump(p, all_threads, &opts) {
                    Ok(v) => per_dump.push(v),
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            let hist = symbols::histogram(&per_dump);
            (per_dump, hist, first_err)
        })
        .await
        {
            Ok((per_dump, hist, err)) => (Some(per_dump), Some(hist), err),
            Err(e) => (None, None, Some(format!("symbolization task failed: {e}"))),
        }
    } else {
        (None, None, None)
    };

    Ok(json!({
        "status": "completed",
        "tool": "cdp:Runtime.evaluate + MiniDumpWriteDump",
        "js_stack": js_stack,
        "samples": samples,
        "interval_ms": if samples > 1 { Some(interval_ms) } else { None },
        "sampled_pid": sampled_pid,
        "sample_target_reason": sample_target_reason,
        "symbolized": symbolized,
        "symbol_histogram": symbol_histogram,
        "symbol_error": symbol_error,
        "symbol_cache": sym_opts.cache_dir.display().to_string(),
        "symbols_downloaded": sym_opts.download,
        "suggested_pid": capture.suggested_pid,
        "cdp_status": cdp_status,
        "cdp_error": cdp_err,
        "minidump_dir": dump_dir.display().to_string(),
        "minidump_paths": dump_paths,
        "minidump_count": dumps.len(),
        "minidumps": dump_details,
        "full_memory": opts.full_memory,
        "dump_type": if opts.full_memory { "full_memory" } else { "stacks_only" },
        "candidate_processes": candidates,
        "auto_selected_pid": capture.auto_selected_pid,
        "warnings": capture.warnings,
        "note": note,
    }))
}

// ---------------------------------------------------------------------------
// profile-start / profile-stop — CDP Profiler (CPU sampling)
// ---------------------------------------------------------------------------

pub async fn profile_start(_args: &Value) -> Result<Value> {
    let window = main_window().await?;
    if let Err(e) = call_cdp(&window, "Profiler.enable", "{}").await {
        return Ok(json!({"status":"failed","error":"PROFILER_ENABLE_FAILED","message":e}));
    }
    // ~1kHz sampling.
    let _ = call_cdp(
        &window,
        "Profiler.setSamplingInterval",
        r#"{"interval":1000}"#,
    )
    .await;
    match call_cdp(&window, "Profiler.start", "{}").await {
        Ok(_) => Ok(json!({
            "status": "started",
            "tool": "cdp:Profiler",
            "hint": "Reproduce the slowness, then run profile-stop.",
        })),
        Err(e) => Ok(json!({"status":"failed","error":"PROFILER_START_FAILED","message":e})),
    }
}

pub async fn profile_stop(args: &Value) -> Result<Value> {
    let window = main_window().await?;
    match call_cdp(&window, "Profiler.stop", "{}").await {
        Ok(v) => {
            let profile = crate::cdp::result_at(&v, "profile")
                .cloned()
                .unwrap_or(Value::Null);
            let out = args
                .get("output")
                .and_then(|s| s.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("dalzzack-cpu-profile.json"));
            let _ = std::fs::write(&out, serde_json::to_vec(&profile).unwrap_or_default());
            let samples = profile
                .get("samples")
                .and_then(|s| s.as_array())
                .map(|a| a.len());
            Ok(json!({
                "status": "completed",
                "tool": "cdp:Profiler",
                "report_path": out.display().to_string(),
                "sample_count": samples,
                "note": "Open in Chrome DevTools ▸ Performance ▸ Load profile.",
            }))
        }
        Err(e) => Ok(json!({"status":"failed","error":"PROFILER_STOP_FAILED","message":e})),
    }
}

// ---------------------------------------------------------------------------
// heap-snapshot — request/response heap usage (full snapshot needs events)
// ---------------------------------------------------------------------------

pub async fn heap_snapshot(_args: &Value) -> Result<Value> {
    let window = main_window().await?;
    let _ = call_cdp(&window, "HeapProfiler.enable", "{}").await;
    let _ = call_cdp(&window, "HeapProfiler.collectGarbage", "{}").await;
    match call_cdp(&window, "Runtime.getHeapUsage", "{}").await {
        Ok(v) => {
            let used = crate::cdp::result_at(&v, "usedSize")
                .cloned()
                .unwrap_or(Value::Null);
            let total = crate::cdp::result_at(&v, "totalSize")
                .cloned()
                .unwrap_or(Value::Null);
            Ok(json!({
                "status": "completed",
                "report_type": "heap_usage",
                "used_size": used,
                "total_size": total,
                "note": "Post-GC JS heap usage. A full retained-object .heapsnapshot streams via \
                         HeapProfiler.addHeapSnapshotChunk events (follow-up); use `attach-devtools` \
                         ▸ Memory tab to capture one interactively.",
            }))
        }
        Err(e) => Ok(json!({"status":"failed","error":"HEAP_USAGE_FAILED","message":e})),
    }
}

// ---------------------------------------------------------------------------
// attach-devtools — open the WebView2 DevTools window
// ---------------------------------------------------------------------------

pub async fn attach_devtools(_args: &Value) -> Result<Value> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;

    let window = main_window().await?;
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let tx_clone = tx.clone();

    let cb = window.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        let core: ICoreWebView2 = match controller.CoreWebView2() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx_clone.send(Err(format!("CoreWebView2 error: {:?}", e)));
                return;
            }
        };
        match core.OpenDevToolsWindow() {
            Ok(_) => {
                let _ = tx_clone.send(Ok(()));
            }
            Err(e) => {
                let _ = tx_clone.send(Err(format!("OpenDevToolsWindow error: {:?}", e)));
            }
        }
    });

    if let Err(e) = cb {
        return Ok(
            json!({"status":"failed","error":"WITH_WEBVIEW_FAILED","message":format!("{e:?}")}),
        );
    }

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(json!({
            "status": "completed",
            "transport": "webview2_devtools",
            "opened": true,
            "instructions": "A Chrome DevTools window opened for the renderer (Console/Sources/Performance/Memory).",
        })),
        Ok(Err(e)) => Ok(json!({"status":"failed","error":"OPEN_DEVTOOLS_FAILED","message":e})),
        Err(_) => Ok(
            json!({"status":"failed","error":"OPEN_DEVTOOLS_TIMEOUT","message":"timed out opening DevTools"}),
        ),
    }
}

// ---------------------------------------------------------------------------
// exec-js-isolated — Runtime.evaluate out-of-band
// ---------------------------------------------------------------------------

pub async fn exec_js_isolated(args: &Value) -> Result<Value> {
    let script = args
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`script` is required"))?;
    let window = main_window().await?;

    let params = json!({
        "expression": script,
        "returnByValue": true,
        "userGesture": false,
        "awaitPromise": false,
    })
    .to_string();

    match call_cdp(&window, "Runtime.evaluate", &params).await {
        Ok(v) => {
            if let Some(exc) = crate::cdp::result_at(&v, "exceptionDetails") {
                return Ok(json!({
                    "status": "failed",
                    "error": "JS_EXCEPTION",
                    "details": exc.clone(),
                }));
            }
            let value = crate::cdp::result_at(&v, "result/value")
                .cloned()
                .unwrap_or(Value::Null);
            Ok(json!({
                "status": "completed",
                "world": "cdp_default",
                "value": value,
                "note": "Evaluated via out-of-band CDP. (True isolated world via \
                         Page.createIsolatedWorld is a follow-up; this uses the default context.)",
            }))
        }
        Err(e) => Ok(json!({"status":"failed","error":"ISOLATED_EXEC_ERROR","message":e})),
    }
}

// ===========================================================================
// `trace` (C) — CDP Tracing.* via browser process. Real out-of-band tracing:
// events flow on the WebView2 browser thread, so a wedged renderer doesn't
// stop the firehose.
// ===========================================================================

use std::sync::{Mutex, OnceLock};

struct TraceState {
    events: Vec<Value>,
    complete: bool,
}

fn trace_state() -> &'static Mutex<TraceState> {
    static S: OnceLock<Mutex<TraceState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(TraceState {
            events: Vec::new(),
            complete: false,
        })
    })
}

pub async fn trace(args: &Value) -> Result<Value> {
    let duration_ms = args
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000)
        .min(60_000);
    let categories = args.get("categories").and_then(|v| v.as_str()).unwrap_or(
        "devtools.timeline,v8,blink,blink.user_timing,disabled-by-default-v8.cpu_profiler",
    );
    let site = args
        .get("site")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let out = args
        .get("output")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let safe: String = site
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            std::env::temp_dir().join(format!(
                "dalzzack-{}-trace-{}.json",
                safe,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ))
        });

    let window = main_window().await?;

    // Reset accumulator.
    {
        let mut s = trace_state().lock().unwrap();
        s.events.clear();
        s.complete = false;
    }

    // Subscribe to the two events we care about. Both are owned by the
    // browser process, so the renderer can be fully wedged without losing
    // the stream.
    if let Err(e) = subscribe_trace_events(&window).await {
        return Ok(json!({
            "status": "failed",
            "error": "TRACE_SUBSCRIBE_FAILED",
            "message": e,
        }));
    }

    // Start tracing. ReportEvents transferMode delivers via
    // `Tracing.dataCollected` events instead of a stream handle.
    let start_params = format!(
        r#"{{"transferMode":"ReportEvents","traceConfig":{{"includedCategories":["{}"]}}}}"#,
        categories
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(',', "\",\"")
    );
    if let Err(e) = call_cdp(&window, "Tracing.start", &start_params).await {
        return Ok(json!({
            "status": "failed",
            "error": "TRACING_START_FAILED",
            "message": e,
        }));
    }

    tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
    let _ = call_cdp(&window, "Tracing.end", "{}").await;

    // Wait for tracingComplete event — capped so a misbehaving CDP doesn't
    // hang `trace` itself.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if trace_state().lock().unwrap().complete {
            break;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!("[trace] tracingComplete didn't fire within 10s");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let events = std::mem::take(&mut trace_state().lock().unwrap().events);
    let payload = json!({
        "traceEvents": events,
        "displayTimeUnit": "ms",
        "otherData": {
            "transport": "cdp_tracing",
            "platform": "windows",
            "duration_ms": duration_ms,
            "categories": categories,
        },
    });

    let bytes = serde_json::to_vec_pretty(&payload).unwrap_or_default();
    std::fs::write(&out, &bytes).ok();

    Ok(json!({
        "status": "completed",
        "transport": "cdp_tracing",
        "duration_ms": duration_ms,
        "event_count": events.len(),
        "output_path": out.display().to_string(),
        "bytes_written": bytes.len(),
        "note": "Open in chrome://tracing or perfetto.",
    }))
}

async fn subscribe_trace_events(window: &WebviewWindow) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl,
    };
    use windows_core::{implement, Ref, HSTRING, PWSTR};

    #[implement(ICoreWebView2DevToolsProtocolEventReceivedEventHandler)]
    struct TraceHandler {
        kind: &'static str,
    }

    impl ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl for TraceHandler_Impl {
        fn Invoke(
            &self,
            _sender: Ref<'_, ICoreWebView2>,
            args: Ref<'_, ICoreWebView2DevToolsProtocolEventReceivedEventArgs>,
        ) -> windows_core::Result<()> {
            unsafe {
                if let Some(args_ref) = args.as_ref() {
                    let mut p: PWSTR = PWSTR::null();
                    if args_ref.ParameterObjectAsJson(&mut p).is_ok() && !p.is_null() {
                        let s = p.to_string().unwrap_or_default();
                        if let Ok(v) = serde_json::from_str::<Value>(&s) {
                            match self.kind {
                                "data" => {
                                    if let Some(arr) = v.get("value").and_then(|x| x.as_array()) {
                                        if let Ok(mut st) = trace_state().lock() {
                                            st.events.extend(arr.iter().cloned());
                                        }
                                    }
                                }
                                "complete" => {
                                    if let Ok(mut st) = trace_state().lock() {
                                        st.complete = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                        windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as *const _));
                    }
                }
            }
            Ok(())
        }
    }

    let res = window.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        let Ok(core) = controller.CoreWebView2() else {
            return;
        };
        let core: ICoreWebView2 = core;
        for (ev, kind) in [
            ("Tracing.dataCollected", "data"),
            ("Tracing.tracingComplete", "complete"),
        ] {
            let name = HSTRING::from(ev);
            if let Ok(rx) = core.GetDevToolsProtocolEventReceiver(&name) {
                let h: ICoreWebView2DevToolsProtocolEventReceivedEventHandler =
                    TraceHandler { kind }.into();
                let mut tok = Default::default();
                let _ = rx.add_DevToolsProtocolEventReceived(&h, &mut tok);
                // The registration lives on the receiver; dropping it can take
                // the subscription with it. Hold it for the process lifetime.
                std::mem::forget(rx);
            }
        }
    });
    res.map_err(|e| format!("with_webview failed: {e:?}"))
}

// ===========================================================================
// `debugger_arm` / `debugger_snapshot` / `debugger_disarm` (D) — pre-arm V8
// via CDP Debugger so we can capture call frames the moment any script runs.
// Subscribed events live on the browser process, so Debugger.paused fires
// even if the renderer's own JS thread is wedged inside script execution.
// ===========================================================================

/// What `Debugger.scriptParsed` told us about one script.
///
/// Call frames carry a `scriptId` and, for anything that isn't a plain
/// `<script src>`, an empty `url` — so "which script is this function in?"
/// is unanswerable from the pause payload alone. The daemon buffers
/// `scriptParsed` alongside the pauses and joins them at snapshot time.
#[derive(Clone, Debug)]
struct ScriptInfo {
    /// For a `<script src>` this is the real URL; for an eval'd script that
    /// used `//# sourceURL=`, CDP puts that name here instead.
    url: String,
    /// Whether `url` came from `//# sourceURL=` rather than a real fetch.
    has_source_url: bool,
    /// WebView2's own label for the script (often useful when `url` is empty).
    embedder_name: String,
    /// For `eval`/`new Function`, where the eval was called from.
    origin_stack: Option<String>,
    start_line: i64,
}

impl ScriptInfo {
    /// Best available human-facing name for the script.
    fn best_name(&self) -> Option<String> {
        for candidate in [&self.url, &self.embedder_name] {
            if !candidate.is_empty() {
                return Some(candidate.clone());
            }
        }
        None
    }
}

struct DebuggerState {
    armed: bool,
    strategy: String,
    /// Captured frames per `Debugger.paused` event.
    pauses: Vec<Value>,
    /// `scriptId` → script identity, from `Debugger.scriptParsed`.
    scripts: std::collections::HashMap<String, ScriptInfo>,
    subscribed: bool,
    /// Truncate each pause's call frames to this many (0 = keep all).
    max_frames: usize,
    /// Record only every Nth pause (1 = record all). An exception storm can
    /// produce thousands of pauses a second; without this the buffer is just
    /// the last N of a firehose and the daemon pays to serialize all of it.
    sample_every: u64,
    /// Total pauses seen, including ones dropped by sampling.
    seen: u64,
    dropped_by_sampling: u64,
    /// Ring cap — the newest `PAUSE_CAP` recorded pauses are kept.
    dropped_by_cap: u64,
}

/// Upper bound on buffered pauses. Exception storms are the expected input
/// here, so the buffer must be bounded or the daemon grows without limit.
const PAUSE_CAP: usize = 2000;

/// Upper bound on the `scriptId` → URL table. A page that evals in a loop
/// creates a script per iteration.
const SCRIPT_CAP: usize = 20_000;

fn debugger_state() -> &'static Mutex<DebuggerState> {
    static S: OnceLock<Mutex<DebuggerState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(DebuggerState {
            armed: false,
            strategy: String::new(),
            pauses: Vec::new(),
            scripts: std::collections::HashMap::new(),
            subscribed: false,
            max_frames: 0,
            sample_every: 1,
            seen: 0,
            dropped_by_sampling: 0,
            dropped_by_cap: 0,
        })
    })
}

/// Reduce a `Debugger.paused` payload to what's worth buffering: the pause
/// reason, the exception's own description/message, and (optionally truncated)
/// call frames without their scope chains — the scope chain is where the bulk
/// of the JSON is, and it's rarely what you're reading a storm for.
fn condense_pause(
    v: &Value,
    max_frames: usize,
    scripts: &std::collections::HashMap<String, ScriptInfo>,
) -> Value {
    let reason = v.get("reason").cloned().unwrap_or(Value::Null);
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let exception_text = v
        .pointer("/data/description")
        .or_else(|| v.pointer("/data/value"))
        .or_else(|| v.pointer("/data/className"))
        .cloned();

    let frames: Vec<Value> = v
        .get("callFrames")
        .and_then(|f| f.as_array())
        .map(|arr| {
            let take = if max_frames == 0 {
                arr.len()
            } else {
                max_frames.min(arr.len())
            };
            arr[..take]
                .iter()
                .map(|f| {
                    let script_id = f
                        .pointer("/location/scriptId")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let info = scripts.get(&script_id);

                    // The frame's own `url` is empty for eval, `new Function`,
                    // inline module scripts and anything a bundler injected —
                    // exactly the frames worth identifying. Fall back to what
                    // `scriptParsed` said about this scriptId.
                    let frame_url = f
                        .get("url")
                        .and_then(|u| u.as_str())
                        .filter(|u| !u.is_empty())
                        .map(String::from);
                    let resolved = frame_url
                        .clone()
                        .or_else(|| info.and_then(|i| i.best_name()));

                    // Line numbers are script-relative; for an inline script the
                    // page-relative line needs scriptParsed's startLine added.
                    let line = f.pointer("/location/lineNumber").and_then(|l| l.as_i64());
                    let page_line = match (line, info.map(|i| i.start_line)) {
                        (Some(l), Some(start)) if start > 0 => Some(l + start),
                        _ => line,
                    };

                    json!({
                        "functionName": f.get("functionName").cloned().unwrap_or(Value::Null),
                        "url": resolved,
                        "url_source": if frame_url.is_some() {
                            "call_frame"
                        } else if resolved.is_some() {
                            "script_parsed"
                        } else {
                            "unknown"
                        },
                        "lineNumber": line,
                        "pageLineNumber": page_line,
                        "columnNumber": f.pointer("/location/columnNumber").cloned().unwrap_or(Value::Null),
                        "scriptId": script_id,
                        // Where an eval'd / dynamically-created script came from.
                        "script_origin_stack": info.and_then(|i| i.origin_stack.clone()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let total_frames = v
        .get("callFrames")
        .and_then(|f| f.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    json!({
        "ts_ms": crate::recording::now_ms(),
        "reason": reason,
        "exception_text": exception_text,
        "data": data,
        "call_frames": frames,
        "total_frames": total_frames,
        "frames_truncated": max_frames != 0 && total_frames > frames.len(),
    })
}

/// Called from the `Debugger.scriptParsed` / `scriptFailedToParse` handler.
/// Cheap by construction — it runs on the browser thread for every script the
/// page parses.
fn record_script(v: &Value) {
    let Some(script_id) = v.get("scriptId").and_then(|s| s.as_str()) else {
        return;
    };
    let Ok(mut s) = debugger_state().lock() else {
        return;
    };
    // Bound it: a page that evals in a loop would otherwise grow this forever.
    if s.scripts.len() >= SCRIPT_CAP && !s.scripts.contains_key(script_id) {
        return;
    }
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    s.scripts.insert(
        script_id.to_string(),
        ScriptInfo {
            url: get("url"),
            has_source_url: v
                .get("hasSourceURL")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            embedder_name: get("embedderName"),
            // `stackTrace` is present when the script was created by eval /
            // new Function / document.write — i.e. exactly when `url` is empty.
            origin_stack: v
                .pointer("/stackTrace/callFrames/0")
                .and_then(format_origin_frame),
            start_line: v.get("startLine").and_then(|x| x.as_i64()).unwrap_or(0),
        },
    );
}

/// "who called eval" as one line. Returns `None` rather than a string of
/// separators when the frame carries nothing worth printing.
fn format_origin_frame(f: &Value) -> Option<String> {
    let name = f
        .get("functionName")
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    let url = f.get("url").and_then(|x| x.as_str()).unwrap_or_default();
    let line = f.get("lineNumber").and_then(|x| x.as_i64());

    let name = if name.is_empty() { "(anonymous)" } else { name };
    match (url.is_empty(), line) {
        (true, None) => None,
        (true, Some(l)) => Some(format!("{name} @ line {l}")),
        (false, None) => Some(format!("{name} @ {url}")),
        (false, Some(l)) => Some(format!("{name} @ {url}:{l}")),
    }
}

/// Called from the `Debugger.paused` handler. Applies sampling + caps and
/// returns nothing — the handler must stay cheap, it's on the browser thread.
fn record_pause(v: &Value) {
    let Ok(mut s) = debugger_state().lock() else {
        return;
    };
    s.seen = s.seen.saturating_add(1);
    if s.sample_every > 1 && (s.seen - 1) % s.sample_every != 0 {
        s.dropped_by_sampling = s.dropped_by_sampling.saturating_add(1);
        return;
    }
    let max_frames = s.max_frames;
    // Join against the script table now, while the pause is fresh: a later
    // navigation clears V8's script ids and the mapping would be gone.
    let scripts = std::mem::take(&mut s.scripts);
    let condensed = condense_pause(v, max_frames, &scripts);
    s.scripts = scripts;
    if s.pauses.len() >= PAUSE_CAP {
        s.pauses.remove(0);
        s.dropped_by_cap = s.dropped_by_cap.saturating_add(1);
    }
    s.pauses.push(condensed);
}

pub async fn debugger_arm(args: &Value) -> Result<Value> {
    let strategy = args
        .get("strategy")
        .and_then(|v| v.as_str())
        .unwrap_or("first-script");
    let window = main_window().await?;

    // Subscribe to Debugger.paused once (idempotent).
    let needs_subscribe = !debugger_state().lock().unwrap().subscribed;
    if needs_subscribe {
        if let Err(e) = subscribe_debugger_paused(&window).await {
            return Ok(json!({
                "status": "failed",
                "error": "DEBUGGER_SUBSCRIBE_FAILED",
                "message": e,
            }));
        }
        debugger_state().lock().unwrap().subscribed = true;
    }

    if let Err(e) = call_cdp(&window, "Debugger.enable", "{}").await {
        return Ok(json!({"status":"failed","error":"DEBUGGER_ENABLE_FAILED","message":e}));
    }

    // Sampling knobs — an exception storm is the expected input for the
    // `exceptions` strategy, so record-every-Nth and frame truncation are part
    // of arming, not an afterthought.
    {
        let mut s = debugger_state().lock().unwrap();
        s.max_frames = args
            .get("max_frames")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(1000) as usize;
        s.sample_every = args
            .get("sample_every")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1);
        s.seen = 0;
        s.dropped_by_sampling = 0;
        s.dropped_by_cap = 0;
    }

    let armed_params = match strategy {
        "first-script" => r#"{"instrumentation":"beforeScriptExecution"}"#.to_string(),
        "exceptions" => {
            // `uncaught` vs `all`: engine-thrown TypeErrors (Proxy invariant
            // violations, `Illegal invocation`) never pass through the page's
            // global Error constructors, so wrapping them in-page reports zero.
            // This is the only way to see them.
            let uncaught_only = args
                .get("uncaught_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let state = if uncaught_only { "uncaught" } else { "all" };
            format!(r#"{{"state":"{state}"}}"#)
        }
        "url" => {
            let pat = args
                .get("url_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or(".*");
            // setBreakpointByUrl needs a urlRegex and a line; line 0 = the first executable line.
            format!(
                r#"{{"lineNumber":0,"urlRegex":"{}"}}"#,
                pat.replace('\\', "\\\\").replace('"', "\\\"")
            )
        }
        other => {
            return Ok(json!({
                "status": "failed",
                "error": "UNKNOWN_STRATEGY",
                "message": format!(
                    "Unknown debugger strategy '{other}'. Use 'first-script', 'exceptions', or 'url'."
                ),
            }));
        }
    };

    let method = match strategy {
        "first-script" => "Debugger.setInstrumentationBreakpoint",
        "exceptions" => "Debugger.setPauseOnExceptions",
        _ => "Debugger.setBreakpointByUrl",
    };

    match call_cdp(&window, method, &armed_params).await {
        Ok(_) => {
            let (sample_every, max_frames) = {
                let mut s = debugger_state().lock().unwrap();
                s.armed = true;
                s.strategy = strategy.to_string();
                (s.sample_every, s.max_frames)
            };
            Ok(json!({
                "status": "armed",
                "strategy": strategy,
                "method": method,
                "params": armed_params,
                "sample_every": sample_every,
                "max_frames": max_frames,
                "pause_cap": PAUSE_CAP,
                "note": "Debugger.paused events are buffered daemon-side and auto-resumed, so \
                         `debugger-snapshot` is a memory read and answers even when the renderer \
                         is wedged. With strategy=exceptions every throw pauses V8 — the page runs \
                         much slower, and on a storm you want --sample-every / --max-frames.",
            }))
        }
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "DEBUGGER_ARM_FAILED",
            "message": e,
        })),
    }
}

pub async fn debugger_snapshot(args: &Value) -> Result<Value> {
    let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut s = debugger_state().lock().unwrap();
    let pauses = if clear {
        std::mem::take(&mut s.pauses)
    } else {
        s.pauses.clone()
    };
    // Which scripts the returned frames actually reference — the whole table
    // can be thousands of entries on a real page, and only these matter here.
    let mut referenced: std::collections::BTreeSet<String> = Default::default();
    for p in &pauses {
        if let Some(frames) = p.get("call_frames").and_then(|f| f.as_array()) {
            for f in frames {
                if let Some(id) = f.get("scriptId").and_then(|x| x.as_str()) {
                    referenced.insert(id.to_string());
                }
            }
        }
    }
    let scripts: Vec<Value> = referenced
        .iter()
        .filter_map(|id| {
            s.scripts.get(id).map(|i| {
                json!({
                    "scriptId": id,
                    "url": i.best_name(),
                    // true ⇒ `url` is a `//# sourceURL=` label the script chose
                    // for itself, not something that was actually fetched.
                    "from_source_url_comment": i.has_source_url,
                    "embedder_name": i.embedder_name,
                    "start_line": i.start_line,
                    "origin_stack": i.origin_stack,
                })
            })
        })
        .collect();
    let unresolved = referenced.len() - scripts.len();

    let out = json!({
        "status": "completed",
        "armed": s.armed,
        "strategy": s.strategy,
        "pause_count": pauses.len(),
        "seen_total": s.seen,
        "dropped_by_sampling": s.dropped_by_sampling,
        "dropped_by_cap": s.dropped_by_cap,
        "sample_every": s.sample_every,
        "max_frames": s.max_frames,
        "scripts_known": s.scripts.len(),
        "scripts_unresolved": unresolved,
        "scripts": scripts,
        "pauses": pauses,
        "note": "Daemon-memory read — no IPC into the page, so this answers on a wedged renderer. \
                 `seen_total` counts every pause including the ones sampling dropped. Call frames \
                 are joined against buffered `Debugger.scriptParsed`, so each frame carries a \
                 resolved `url` (`url_source` says whether it came from the frame itself or the \
                 script table) plus `script_origin_stack` for eval'd scripts.",
    });
    if clear {
        s.dropped_by_cap = 0;
    }
    Ok(out)
}

pub async fn debugger_disarm(_args: &Value) -> Result<Value> {
    let window = main_window().await?;
    // Turn pause-on-exception off explicitly: `Debugger.disable` alone has been
    // seen to leave the setting live on re-enable.
    let _ = call_cdp(
        &window,
        "Debugger.setPauseOnExceptions",
        r#"{"state":"none"}"#,
    )
    .await;
    let _ = call_cdp(&window, "Debugger.disable", "{}").await;
    let mut s = debugger_state().lock().unwrap();
    s.armed = false;
    s.strategy.clear();
    s.pauses.clear();
    s.scripts.clear();
    s.seen = 0;
    s.dropped_by_sampling = 0;
    s.dropped_by_cap = 0;
    Ok(json!({
        "status": "disarmed",
    }))
}

async fn subscribe_debugger_paused(window: &WebviewWindow) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl,
    };
    use windows_core::{implement, Ref, HSTRING, PWSTR};

    #[implement(ICoreWebView2DevToolsProtocolEventReceivedEventHandler)]
    struct PausedHandler {
        /// `paused` | `script`. Only `paused` may resume — sending
        /// `Debugger.resume` for a `scriptParsed` event would resume a debugger
        /// that isn't paused and log a protocol error per script.
        kind: &'static str,
    }

    impl ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl for PausedHandler_Impl {
        fn Invoke(
            &self,
            sender: Ref<'_, ICoreWebView2>,
            args: Ref<'_, ICoreWebView2DevToolsProtocolEventReceivedEventArgs>,
        ) -> windows_core::Result<()> {
            unsafe {
                if let Some(args_ref) = args.as_ref() {
                    let mut p: PWSTR = PWSTR::null();
                    if args_ref.ParameterObjectAsJson(&mut p).is_ok() && !p.is_null() {
                        let s = p.to_string().unwrap_or_default();
                        if let Ok(v) = serde_json::from_str::<Value>(&s) {
                            match self.kind {
                                "script" => record_script(&v),
                                _ => record_pause(&v),
                            }
                        }
                        windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as *const _));
                    }
                }
                if self.kind != "paused" {
                    return Ok(());
                }
                // CRITICAL: auto-resume so the page keeps running. Otherwise
                // the first paused script wedges the renderer permanently and
                // the whole point of pre-arm (passive observation) is lost.
                if let Some(core_ref) = sender.as_ref() {
                    let method = HSTRING::from("Debugger.resume");
                    let params = HSTRING::from("{}");
                    // Fire-and-forget — we don't have a handler chain here.
                    use webview2_com::Microsoft::Web::WebView2::Win32::{
                        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
                        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl,
                    };
                    use windows_core::{HRESULT, PCWSTR};
                    #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
                    struct NoopHandler;
                    impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for NoopHandler_Impl {
                        fn Invoke(&self, _ec: HRESULT, _ret: &PCWSTR) -> windows_core::Result<()> {
                            Ok(())
                        }
                    }
                    let h: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler =
                        NoopHandler.into();
                    let _ = core_ref.CallDevToolsProtocolMethod(&method, &params, &h);
                }
            }
            Ok(())
        }
    }

    let res = window.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        let Ok(core) = controller.CoreWebView2() else {
            return;
        };
        let core: ICoreWebView2 = core;
        // `scriptParsed` is what makes a call frame identifiable: frames carry
        // a scriptId, not a URL. `Debugger.enable` re-emits it for every script
        // already parsed, so subscribing before enabling (see `debugger_arm`)
        // yields the full table, not just scripts loaded after arming.
        for (ev, kind) in [
            ("Debugger.paused", "paused"),
            ("Debugger.scriptParsed", "script"),
            ("Debugger.scriptFailedToParse", "script"),
        ] {
            let name = HSTRING::from(ev);
            if let Ok(rx) = core.GetDevToolsProtocolEventReceiver(&name) {
                let h: ICoreWebView2DevToolsProtocolEventReceivedEventHandler =
                    PausedHandler { kind }.into();
                let mut tok = Default::default();
                let _ = rx.add_DevToolsProtocolEventReceived(&h, &mut tok);
                std::mem::forget(rx);
            }
        }
    });
    res.map_err(|e| format!("with_webview failed: {e:?}"))
}

// ===========================================================================
// `pause` enhancement (B) — pair the CDP JS stack with an out-of-band
// minidump of the WebView2 renderer processes. The minidump is captured via
// `MiniDumpWriteDump` against each child msedgewebview2.exe process, which
// works even when the renderer's main thread is fully wedged (the dump is
// driven by Windows kernel debug APIs, not by the renderer's V8 isolate).
// ===========================================================================

/// How to take the dump. Defaults reproduce the historical stacks-only behaviour.
#[derive(Clone, Debug, Default)]
pub struct DumpOptions {
    /// `MiniDumpWithFullMemory` (+ full memory info + handle data): the dump
    /// carries the heap, so "what is allocated" becomes answerable. Costs a
    /// file roughly the size of the target's working set.
    pub full_memory: bool,
    /// Dump only this PID. Required in spirit for `full_memory` — see
    /// [`capture_renderer_minidumps`] for what happens when it's omitted.
    pub only_pid: Option<u32>,
    /// Dump every WebView2 process even in full-memory mode. Off by default
    /// because 6–7 GB-sized dumps fill a disk fast.
    pub all_processes: bool,
    /// Soft ceiling: a dump larger than this is kept but flagged in `warnings`.
    pub max_bytes: Option<u64>,
}

#[derive(Debug)]
pub struct DumpRecord {
    pub pid: u32,
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub working_set_bytes: u64,
    pub process_type: String,
    pub over_max: bool,
}

#[derive(Debug)]
pub struct ProcessCandidate {
    pub pid: u32,
    pub working_set_bytes: u64,
    /// Chromium process type from the command line: `renderer`, `browser`,
    /// `gpu-process`, `utility`, `crashpad-handler`, … The browser process is
    /// routinely the *largest* of the family, so working set alone picks the
    /// wrong target for "what is the page allocating?".
    pub process_type: String,
}

#[derive(Debug, Default)]
pub struct CaptureResult {
    pub dumps: Vec<DumpRecord>,
    /// Every WebView2 process we could have dumped, with its working set —
    /// this is how the agent picks a `--only-pid` for the next run.
    pub candidates: Vec<ProcessCandidate>,
    /// Set when full-memory mode picked the fattest process on its own.
    pub auto_selected_pid: Option<u32>,
    /// The process worth looking at (renderer first, then working set),
    /// reported in **every** mode. `auto_selected_pid` is only filled in when
    /// full-memory mode actually chose for you, which made it read as "we could
    /// not decide" during a plain `pause`.
    pub suggested_pid: Option<u32>,
    pub warnings: Vec<String>,
}

/// Candidates rooted at the WebView2 browser process when we know it, else at
/// our own process.
fn list_webview_processes(root_pid: Option<u32>) -> Vec<ProcessCandidate> {
    let root = root_pid.unwrap_or_else(std::process::id);
    let mut found = list_webview_processes_from(root);
    // The browser process itself is a legitimate dump target and is not its own
    // descendant, so add it explicitly.
    if let Some(pid) = root_pid {
        if !found.iter().any(|c| c.pid == pid) {
            found.push(ProcessCandidate {
                pid,
                working_set_bytes: working_set_bytes(pid),
                process_type: process_type_of(pid),
            });
        }
    }
    found
}

pub fn capture_renderer_minidumps(
    out_dir: &std::path::Path,
    opts: &DumpOptions,
    root_pid: Option<u32>,
) -> CaptureResult {
    let mut result = CaptureResult::default();

    let _ = std::fs::create_dir_all(out_dir);

    let mut candidates = list_webview_processes(root_pid);
    // Renderers first, then by working set. The page's heap lives in a
    // renderer; the browser process is usually the biggest of the family and
    // dumping it answers the wrong question.
    candidates.sort_by_key(|c| {
        (
            c.process_type != "renderer",
            std::cmp::Reverse(c.working_set_bytes),
        )
    });
    result.suggested_pid = candidates.first().map(|c| c.pid);

    // Which processes actually get dumped.
    let targets: Vec<u32> = match opts.only_pid {
        Some(pid) => {
            if !candidates.iter().any(|c| c.pid == pid) {
                result.warnings.push(format!(
                    "--only-pid {pid} is not a WebView2 process of this daemon; nothing dumped. \
                     See candidate_processes for valid PIDs."
                ));
                result.candidates = candidates;
                return result;
            }
            vec![pid]
        }
        None if opts.full_memory && !opts.all_processes => {
            // A full-memory dump of all 6–7 WebView2 processes is GBs of disk.
            // Default to the fattest one (the renderer, in every wedge we've
            // seen) and say so loudly instead of silently filling the disk.
            match candidates.first() {
                Some(c) => {
                    result.auto_selected_pid = Some(c.pid);
                    result.warnings.push(format!(
                        "--full-memory without --only-pid: dumped only PID {} (type={}, working \
                         set {} bytes). Pass --only-pid to choose, or --all-processes to dump \
                         every WebView2 process.",
                        c.pid, c.process_type, c.working_set_bytes
                    ));
                    vec![c.pid]
                }
                None => vec![],
            }
        }
        None => candidates.iter().map(|c| c.pid).collect(),
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    for pid in targets {
        let (ws, ptype) = candidates
            .iter()
            .find(|c| c.pid == pid)
            .map(|c| (c.working_set_bytes, c.process_type.clone()))
            .unwrap_or((0, "unknown".to_string()));
        let Some(path) = write_minidump(pid, out_dir, ts, opts.full_memory) else {
            result
                .warnings
                .push(format!("minidump for PID {pid} failed — see daemon log"));
            continue;
        };
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let over_max = opts.max_bytes.is_some_and(|max| bytes > max);
        if over_max {
            result.warnings.push(format!(
                "PID {pid} dump is {bytes} bytes, over --max-bytes {}. Kept anyway.",
                opts.max_bytes.unwrap_or(0)
            ));
        }
        result.dumps.push(DumpRecord {
            pid,
            path,
            bytes,
            working_set_bytes: ws,
            process_type: ptype,
            over_max,
        });
    }

    result.candidates = candidates;
    result
}

/// PID of the WebView2 browser process backing this window, straight from the
/// runtime.
///
/// Not necessarily a child of ours: WebView2 keeps one browser process per user
/// data folder, so with several daemons sharing that folder the process tree
/// belongs to whichever started first. Walking our own descendants therefore
/// finds nothing at all in that case — asking the runtime is the only reliable
/// way to know which processes to dump.
pub async fn browser_process_id() -> Option<u32> {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;

    let window = crate::get_main_window().await?;
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    let cb = window.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        let Ok(core) = controller.CoreWebView2() else {
            return;
        };
        let core: ICoreWebView2 = core;
        let mut pid: u32 = 0;
        if core.BrowserProcessId(&mut pid).is_ok() && pid != 0 {
            let _ = tx.send(pid);
        }
    });
    if cb.is_err() {
        return None;
    }
    rx.recv_timeout(std::time::Duration::from_secs(3)).ok()
}

/// Every `msedgewebview2.exe` in the WebView2 process tree, with its working
/// set. `root_pid` is the browser process; when unknown we fall back to our own
/// descendants (correct for a daemon that owns its browser process).
fn list_webview_processes_from(root_pid: u32) -> Vec<ProcessCandidate> {
    let mut found: Vec<ProcessCandidate> = Vec::new();
    let our_pid = root_pid;

    // Walk the process tree via Toolhelp32 and pick msedgewebview2.exe
    // descendants of our own process — the browser/GPU/utility processes plus
    // the renderer we actually care about.
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(_) => return found,
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut parents: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name_w = entry.szExeFile;
                let name = String::from_utf16_lossy(
                    &name_w
                        .iter()
                        .take_while(|&&c| c != 0)
                        .copied()
                        .collect::<Vec<_>>(),
                );
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                names.insert(entry.th32ProcessID, name);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }

        // Transitive descendant set of our PID.
        let mut descendants: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for pid in parents.keys() {
            let mut cur = *pid;
            while let Some(parent) = parents.get(&cur).copied() {
                if parent == our_pid {
                    descendants.insert(*pid);
                    break;
                }
                if parent == 0 || parent == cur {
                    break;
                }
                cur = parent;
            }
        }

        for pid in descendants {
            let name = names.get(&pid).cloned().unwrap_or_default();
            if !name.eq_ignore_ascii_case("msedgewebview2.exe") {
                continue;
            }
            found.push(ProcessCandidate {
                pid,
                working_set_bytes: working_set_bytes(pid),
                process_type: process_type_of(pid),
            });
        }
    }
    found
}

/// Chromium's `--type=` for `pid` (`renderer`, `gpu-process`, `utility`, …),
/// or `browser` for the parent process, which carries no `--type` flag.
/// `unknown` if the command line can't be read.
///
/// Read straight out of the target's PEB — the process type is the only way to
/// tell the renderer (where the page heap lives) from the browser process
/// (which is typically much larger, and answers nothing about the page).
fn process_type_of(pid: u32) -> String {
    match read_command_line(pid) {
        Some(cmd) => match cmd.split("--type=").nth(1) {
            Some(rest) => rest
                .split([' ', '"'])
                .next()
                .unwrap_or("unknown")
                .to_string(),
            None => "browser".to_string(),
        },
        None => "unknown".to_string(),
    }
}

/// Read another process's command line via `NtQueryInformationProcess` →
/// PEB → `RTL_USER_PROCESS_PARAMETERS.CommandLine`.
///
/// x64-only offsets: PEB+0x20 = ProcessParameters,
/// RTL_USER_PROCESS_PARAMETERS+0x70 = CommandLine (`UNICODE_STRING`:
/// Length u16 at +0, Buffer ptr at +8).
fn read_command_line(pid: u32) -> Option<String> {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    const PEB_PROCESS_PARAMETERS: usize = 0x20;
    const PARAMS_COMMAND_LINE: usize = 0x70;

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;

        let read = |addr: usize, buf: *mut std::ffi::c_void, len: usize| -> bool {
            let mut got = 0usize;
            ReadProcessMemory(h, addr as *const _, buf, len, Some(&mut got)).is_ok() && got == len
        };

        let result = (|| {
            let mut pbi = PROCESS_BASIC_INFORMATION::default();
            let mut ret_len = 0u32;
            let status = NtQueryInformationProcess(
                h,
                ProcessBasicInformation,
                &mut pbi as *mut _ as *mut _,
                std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                &mut ret_len,
            );
            if status.is_err() || pbi.PebBaseAddress.is_null() {
                return None;
            }

            let mut params_ptr = 0usize;
            if !read(
                pbi.PebBaseAddress as usize + PEB_PROCESS_PARAMETERS,
                &mut params_ptr as *mut _ as *mut _,
                std::mem::size_of::<usize>(),
            ) || params_ptr == 0
            {
                return None;
            }

            // UNICODE_STRING { Length: u16, MaximumLength: u16, _pad, Buffer }
            let mut len_bytes = 0u16;
            if !read(
                params_ptr + PARAMS_COMMAND_LINE,
                &mut len_bytes as *mut _ as *mut _,
                2,
            ) || len_bytes == 0
            {
                return None;
            }
            let mut buf_ptr = 0usize;
            if !read(
                params_ptr + PARAMS_COMMAND_LINE + 8,
                &mut buf_ptr as *mut _ as *mut _,
                std::mem::size_of::<usize>(),
            ) || buf_ptr == 0
            {
                return None;
            }

            let mut wide = vec![0u16; len_bytes as usize / 2];
            if !read(
                buf_ptr,
                wide.as_mut_ptr() as *mut _,
                len_bytes as usize & !1,
            ) {
                return None;
            }
            Some(String::from_utf16_lossy(&wide))
        })();

        let _ = windows::Win32::Foundation::CloseHandle(h);
        result
    }
}

/// Current working set of `pid`, or 0 if it can't be read. Used to pick the
/// renderer worth a full-memory dump — it's the fat one.
fn working_set_bytes(pid: u32) -> u64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) else {
            return 0;
        };
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let ok = GetProcessMemoryInfo(
            h,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if ok {
            counters.WorkingSetSize as u64
        } else {
            0
        }
    }
}

fn write_minidump(
    pid: u32,
    out_dir: &std::path::Path,
    ts: u128,
    full_memory: bool,
) -> Option<std::path::PathBuf> {
    use windows::Win32::Foundation::{GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        MiniDumpWithFullMemory, MiniDumpWithFullMemoryInfo, MiniDumpWithHandleData,
        MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo,
        MiniDumpWithUnloadedModules, MiniDumpWriteDump,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    use windows_core::HSTRING;

    let suffix = if full_memory { "-full" } else { "" };
    let path = out_dir.join(format!("dalzzack-renderer-{}-{}{}.dmp", pid, ts, suffix));
    unsafe {
        let h_proc: HANDLE =
            match OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("[pause/minidump] OpenProcess({pid}) failed: {e:?}");
                    return None;
                }
            };
        let path_h = HSTRING::from(path.to_string_lossy().as_ref());
        let h_file = match CreateFileW(
            &path_h,
            GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("[pause/minidump] CreateFileW failed: {e:?}");
                return None;
            }
        };
        // Stacks-only by default. `full_memory` swaps in the flags that make
        // the heap itself part of the dump — that's what turns "which
        // functions are on the stack" into "what is actually allocated".
        let dump_type = if full_memory {
            MiniDumpWithFullMemory
                | MiniDumpWithFullMemoryInfo
                | MiniDumpWithHandleData
                | MiniDumpWithThreadInfo
                | MiniDumpWithUnloadedModules
        } else {
            MiniDumpWithThreadInfo
                | MiniDumpWithUnloadedModules
                | MiniDumpWithIndirectlyReferencedMemory
        };
        let dump_result = MiniDumpWriteDump(h_proc, pid, h_file, dump_type, None, None, None);
        // Close the file before the caller stats it, otherwise the size read
        // races the flush of a multi-GB write.
        let _ = windows::Win32::Foundation::CloseHandle(h_file);
        let _ = windows::Win32::Foundation::CloseHandle(h_proc);
        if let Err(e) = dump_result {
            tracing::warn!("[pause/minidump] MiniDumpWriteDump failed: {e:?}");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }
    Some(path)
}
