//! Platform-specific JavaScript execution implementations.

use crate::state::{get_main_window, next_execution_id};

/// Wrap a user script so the result comes back as a JSON envelope.
///
/// Shared by every execution path (ExecuteScript, CDP `Runtime.evaluate` in a
/// chosen frame or world) so a script behaves identically wherever it runs.
pub(crate) fn wrap_script(script: &str) -> String {
    format!(
        r#"
        (function() {{
            try {{
                var __result__ = (function() {{ {script} }})();
                if (__result__ && typeof __result__.then === 'function') {{
                    return JSON.stringify({{ ok: false, error: 'Promise result not supported in sync mode. Use await in your script.' }});
                }}
                return JSON.stringify({{ ok: true, value: __result__ }});
            }} catch(e) {{
                return JSON.stringify({{ ok: false, error: e.toString() }});
            }}
        }})()
    "#,
        script = script
    )
}

/// As [`wrap_script`], but for scripts that may use top-level `await`.
///
/// The sync wrapper is a plain `(function(){…})()`, so `await` inside the user's
/// script is a *syntax* error — the script never runs, and passing
/// `awaitPromise: true` cannot help because there is nothing to await. That is
/// why `exec-js --await --world isolated` failed with "await is only valid in
/// async functions". An async IIFE returns a promise, which CDP's `awaitPromise`
/// then resolves.
///
/// Used wherever the transport is CDP `Runtime.evaluate`: WebView2, and the
/// Chrome engine on every platform. (WKWebView's own `--await` path goes
/// through `callAsyncJavaScript`, which takes an async body directly.)
pub(crate) fn wrap_script_async(script: &str) -> String {
    format!(
        r#"
        (async function() {{
            try {{
                var __result__ = await (async function() {{ {script} }})();
                return JSON.stringify({{ ok: true, value: __result__ }});
            }} catch(e) {{
                return JSON.stringify({{ ok: false, error: e.toString() }});
            }}
        }})()
    "#,
        script = script
    )
}

/// Decode WebView2's `resultObjectAsJson` into the value the script returned.
///
/// The parameter name is literal: WebView2 hands back the **JSON encoding** of
/// the result, so a script returning the string `hi` arrives as `"hi"` (with
/// quotes) and every inner character is JSON-escaped. The correct inverse is a
/// single `serde_json` decode.
///
/// This used to be a chain of five ordered `replace` calls, and the ordering was
/// wrong in two ways that only showed up on large, heavily escaped payloads:
///
/// * `\\r\\n` was restored to `\r\n` by the backslash rule, and the *next* two
///   rules immediately turned those into raw CR/LF bytes — illegal inside a JSON
///   string, so the envelope no longer parsed.
/// * `\"` was unescaped *before* `\\`, so a literal backslash before a quote
///   terminated the string early. Escaped `</script>` blobs are exactly that
///   shape.
///
/// `\uXXXX`, `\/`, `\b` and `\f` were not handled at all. A NAVER SafeFrame ad
/// puts multi-KB escaped JSON (with CRLF and escaped `<script>`) in an iframe's
/// `name`, hitting both bugs at once — which is how `get-frames` came to report
/// "No iframes on the page" for a page with twelve of them.
///
/// Non-string results (numbers, objects, `null`) are returned as-is, matching
/// the previous behaviour.
///
/// Only the WebView2 path decodes; the tests run everywhere, so the `test` cfg
/// keeps this from being dead code on macOS (where clippy runs in CI).
#[cfg(any(target_os = "windows", test))]
fn decode_result_json(json_str: &str) -> String {
    match serde_json::from_str::<String>(json_str) {
        Ok(decoded) => decoded,
        Err(_) => json_str.to_string(),
    }
}

/// Execute JavaScript and return result as JSON.
///
/// macOS: WKWebView evaluateJavaScript (direct result)
/// Windows: WebView2 ExecuteScript (direct result)
/// Other: Event-based fallback via __TAURI__
/// timeout_ms: timeout in milliseconds (default 30000ms = 30s)
pub async fn webview_execute_js(
    script: &str,
    timeout_ms: Option<u32>,
) -> Result<serde_json::Value, String> {
    webview_execute_js_logged(script, timeout_ms, true).await
}

/// [`webview_execute_js`] that leaves no line in the log. For what runs on a
/// timer, which would otherwise write thousands of lines an hour.
pub async fn webview_execute_js_quiet(
    script: &str,
    timeout_ms: Option<u32>,
) -> Result<serde_json::Value, String> {
    webview_execute_js_logged(script, timeout_ms, false).await
}

async fn webview_execute_js_logged(
    script: &str,
    timeout_ms: Option<u32>,
    log: bool,
) -> Result<serde_json::Value, String> {
    // Real Chrome: same wrapper and decoding, delivered by Runtime.evaluate.
    // Every JS-based tool reaches Chrome through here.
    if crate::engine::is_chrome() {
        return crate::chrome::eval_page(script, timeout_ms, false).await;
    }
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    let timeout = timeout_ms.unwrap_or(30000);
    let execution_id = next_execution_id();

    if log {
        tracing::info!(
            "execute_js: id={}, timeout={}ms, script_len={}",
            execution_id,
            timeout,
            script.len()
        );
    }

    let wrapped_script = wrap_script(script);

    // macOS: WKWebView evaluateJavaScript
    #[cfg(target_os = "macos")]
    {
        webview_execute_js_macos(&window, &wrapped_script, timeout).await
    }

    // Windows: WebView2 ExecuteScript
    #[cfg(target_os = "windows")]
    {
        webview_execute_js_windows(&window, &wrapped_script, timeout).await
    }

    // Linux: webkit2gtk run_javascript
    #[cfg(target_os = "linux")]
    {
        webview_execute_js_linux(&window, &wrapped_script, timeout).await
    }

    // Other platforms: event-based fallback
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        webview_execute_js_event_based(&window, script, timeout).await
    }
}

/// macOS: WKWebView.evaluateJavaScript
#[cfg(target_os = "macos")]
async fn webview_execute_js_macos(
    window: &tauri::WebviewWindow,
    script: &str,
    timeout: u32,
) -> Result<serde_json::Value, String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use std::sync::{Arc, Condvar, Mutex};

    let result_holder: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let condvar = Arc::new(Condvar::new());
    let script_owned = script.to_string();

    let result_clone = Arc::clone(&result_holder);
    let condvar_clone = Arc::clone(&condvar);

    window
        .with_webview(move |wv| unsafe {
            let webview_ptr: *mut AnyObject = wv.inner().cast();

            let result_for_block = Arc::clone(&result_clone);
            let condvar_for_block = Arc::clone(&condvar_clone);

            let block =
                block2::StackBlock::new(move |result: *mut AnyObject, error: *mut AnyObject| {
                    let stored_result = if !error.is_null() {
                        let desc: *mut AnyObject = msg_send![error, localizedDescription];
                        let utf8: *const std::ffi::c_char = msg_send![desc, UTF8String];
                        let error_str = if utf8.is_null() {
                            "Unknown error".to_string()
                        } else {
                            std::ffi::CStr::from_ptr(utf8).to_string_lossy().to_string()
                        };
                        Err(error_str)
                    } else if result.is_null() {
                        Ok("null".to_string())
                    } else {
                        let utf8: *const std::ffi::c_char = msg_send![result, UTF8String];
                        if utf8.is_null() {
                            let desc: *mut AnyObject = msg_send![result, description];
                            let desc_utf8: *const std::ffi::c_char = msg_send![desc, UTF8String];
                            if desc_utf8.is_null() {
                                Ok("null".to_string())
                            } else {
                                Ok(std::ffi::CStr::from_ptr(desc_utf8)
                                    .to_string_lossy()
                                    .to_string())
                            }
                        } else {
                            Ok(std::ffi::CStr::from_ptr(utf8).to_string_lossy().to_string())
                        }
                    };

                    if let Ok(mut guard) = result_for_block.lock() {
                        *guard = Some(stored_result);
                    }
                    condvar_for_block.notify_one();
                });
            let block = block.copy();

            let ns_string_cstr = c"NSString";
            let ns_string_class: *mut AnyObject = msg_send![
                objc2::runtime::AnyClass::get(ns_string_cstr).unwrap(),
                alloc
            ];
            let script_cstr = std::ffi::CString::new(script_owned.as_str()).unwrap_or_default();
            let ns_script: *mut AnyObject =
                msg_send![ns_string_class, initWithUTF8String: script_cstr.as_ptr()];

            let _: () =
                msg_send![webview_ptr, evaluateJavaScript: ns_script, completionHandler: &*block];
        })
        .map_err(|e| format!("with_webview error: {:?}", e))?;

    let timeout_duration = std::time::Duration::from_millis(timeout as u64);
    let result_str = {
        let guard = result_holder.lock().map_err(|_| "Lock poisoned")?;
        let (guard, timeout_result) = condvar
            .wait_timeout_while(guard, timeout_duration, |result| result.is_none())
            .map_err(|_| "Condvar wait failed")?;

        if timeout_result.timed_out() {
            return Err(format!(
                "JS_EXECUTION_ERROR: Execution timeout ({}ms)",
                timeout
            ));
        }

        guard
            .clone()
            .ok_or("JS_EXECUTION_ERROR: No result received")?
    }
    .map_err(|e| format!("JS_EXECUTION_ERROR: {}", e))?;

    tracing::debug!("macOS JS result: {}", result_str);

    parse_js_result(&result_str)
}

/// Windows: WebView2.ExecuteScript
#[cfg(target_os = "windows")]
async fn webview_execute_js_windows(
    window: &tauri::WebviewWindow,
    script: &str,
    timeout: u32,
) -> Result<serde_json::Value, String> {
    use std::sync::{Arc, Condvar, Mutex};
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2ExecuteScriptCompletedHandler,
        ICoreWebView2ExecuteScriptCompletedHandler_Impl,
    };
    use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

    let result_holder: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let condvar = Arc::new(Condvar::new());
    let script_owned = script.to_string();

    let result_clone = Arc::clone(&result_holder);
    let condvar_clone = Arc::clone(&condvar);

    #[implement(ICoreWebView2ExecuteScriptCompletedHandler)]
    struct ScriptHandler {
        result_holder: Arc<Mutex<Option<Result<String, String>>>>,
        condvar: Arc<Condvar>,
    }

    impl ICoreWebView2ExecuteScriptCompletedHandler_Impl for ScriptHandler_Impl {
        fn Invoke(
            &self,
            errorcode: HRESULT,
            resultobjectasjson: &PCWSTR,
        ) -> windows_core::Result<()> {
            let stored = if errorcode.is_err() {
                Err(format!("ExecuteScript error: {:?}", errorcode))
            } else {
                let json_str = unsafe { resultobjectasjson.to_string() }.unwrap_or_default();
                Ok(decode_result_json(&json_str))
            };

            if let Ok(mut guard) = self.result_holder.lock() {
                *guard = Some(stored);
            }
            self.condvar.notify_one();
            Ok(())
        }
    }

    window
        .with_webview(move |wv| unsafe {
            let controller = wv.controller();
            let core: ICoreWebView2 = match controller.CoreWebView2() {
                Ok(c) => c,
                Err(e) => {
                    if let Ok(mut guard) = result_clone.lock() {
                        *guard = Some(Err(format!("CoreWebView2 error: {:?}", e)));
                    }
                    condvar_clone.notify_one();
                    return;
                }
            };

            let handler: ICoreWebView2ExecuteScriptCompletedHandler = ScriptHandler {
                result_holder: result_clone.clone(),
                condvar: condvar_clone.clone(),
            }
            .into();

            let script_hstring = HSTRING::from(script_owned.as_str());

            if let Err(e) = core.ExecuteScript(&script_hstring, &handler) {
                if let Ok(mut guard) = result_clone.lock() {
                    *guard = Some(Err(format!("ExecuteScript call error: {:?}", e)));
                }
                condvar_clone.notify_one();
            }
        })
        .map_err(|e| format!("with_webview error: {:?}", e))?;

    let timeout_duration = std::time::Duration::from_millis(timeout as u64);
    let result_str = {
        let guard = result_holder.lock().map_err(|_| "Lock poisoned")?;
        let (guard, timeout_result) = condvar
            .wait_timeout_while(guard, timeout_duration, |result| result.is_none())
            .map_err(|_| "Condvar wait failed")?;

        if timeout_result.timed_out() {
            return Err(format!(
                "JS_EXECUTION_ERROR: Execution timeout ({}ms)",
                timeout
            ));
        }

        guard
            .clone()
            .ok_or("JS_EXECUTION_ERROR: No result received")?
    }
    .map_err(|e| format!("JS_EXECUTION_ERROR: {}", e))?;

    tracing::debug!("Windows JS result: {}", result_str);

    parse_js_result(&result_str)
}

/// Event-based fallback for platforms without native WebView API
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
async fn webview_execute_js_event_based(
    window: &tauri::WebviewWindow,
    script: &str,
    timeout: u32,
) -> Result<serde_json::Value, String> {
    use tauri::Listener;

    let execution_id = next_execution_id();
    let event_name = format!("js-result-{}", execution_id);

    let wrapped_script = format!(
        r#"
        (async function() {{
            const eventName = "{event_name}";
            try {{
                var __result__ = (function() {{ {script} }})();
                if (__result__ && typeof __result__.then === 'function') {{
                    __result__ = await __result__;
                }}
                window.__TAURI__.event.emit(eventName, JSON.stringify({{ ok: true, value: __result__ }}));
            }} catch(e) {{
                window.__TAURI__.event.emit(eventName, JSON.stringify({{ ok: false, error: e.toString() }}));
            }}
        }})();
    "#,
        event_name = event_name,
        script = script
    );

    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let tx = std::sync::Mutex::new(Some(tx));

    let unlisten = window.once(&event_name, move |event| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(event.payload().to_string());
        }
    });

    if let Err(e) = window.eval(&wrapped_script) {
        window.unlisten(unlisten);
        return Err(format!("JS_EXECUTION_ERROR: {}", e));
    }

    let rust_timeout = std::time::Duration::from_millis(timeout as u64 + 1000);

    match tokio::time::timeout(rust_timeout, rx).await {
        Ok(Ok(result_str)) => {
            let cleaned = if result_str.starts_with('"') && result_str.ends_with('"') {
                let inner = &result_str[1..result_str.len() - 1];
                inner.replace("\\\"", "\"").replace("\\\\", "\\")
            } else {
                result_str
            };

            match serde_json::from_str::<serde_json::Value>(&cleaned) {
                Ok(parsed) => {
                    if parsed["ok"].as_bool() == Some(true) {
                        Ok(parsed["value"].clone())
                    } else {
                        let error_msg = parsed["error"].as_str().unwrap_or("Unknown error");
                        Err(format!("JS_EXECUTION_ERROR: {}", error_msg))
                    }
                }
                Err(e) => Err(format!("JS_EXECUTION_ERROR: Failed to parse result: {}", e)),
            }
        }
        Ok(Err(_)) => {
            window.unlisten(unlisten);
            Err("JS_EXECUTION_ERROR: Event channel closed".to_string())
        }
        Err(_) => {
            window.unlisten(unlisten);
            Err(format!(
                "JS_EXECUTION_ERROR: Execution timeout ({}ms)",
                timeout
            ))
        }
    }
}

/// Linux: webkit2gtk run_javascript with native callback
///
/// Uses webkit2gtk's `WebViewExt::run_javascript()` to execute JS natively,
/// bypassing the event-based __TAURI__ fallback. This enables JS execution
/// on external websites that don't expose the Tauri API.
#[cfg(target_os = "linux")]
async fn webview_execute_js_linux(
    window: &tauri::WebviewWindow,
    script: &str,
    timeout: u32,
) -> Result<serde_json::Value, String> {
    use std::sync::{Arc, Condvar, Mutex};
    use webkit2gtk::WebViewExt;

    let result_holder: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let condvar = Arc::new(Condvar::new());
    let script_owned = script.to_string();

    let result_clone = Arc::clone(&result_holder);
    let condvar_clone = Arc::clone(&condvar);

    window
        .with_webview(move |wv| {
            let webview: webkit2gtk::WebView = wv.inner();

            let result_for_cb = Arc::clone(&result_clone);
            let condvar_for_cb = Arc::clone(&condvar_clone);

            webview.run_javascript(&script_owned, gio::Cancellable::NONE, move |result| {
                let stored = match result {
                    Ok(js_result) => {
                        use javascriptcore_rs::ValueExt;
                        match js_result.js_value() {
                            Some(js_value) => {
                                if js_value.is_string() {
                                    match js_value.to_string() {
                                        Some(s) => Ok(s.to_string()),
                                        None => Ok("null".to_string()),
                                    }
                                } else {
                                    match js_value.to_json(0) {
                                        Some(json_str) => Ok(json_str.to_string()),
                                        None => Ok("null".to_string()),
                                    }
                                }
                            }
                            None => Ok("null".to_string()),
                        }
                    }
                    Err(e) => Err(format!("JS execution error: {}", e)),
                };

                if let Ok(mut guard) = result_for_cb.lock() {
                    *guard = Some(stored);
                }
                condvar_for_cb.notify_one();
            });
        })
        .map_err(|e| format!("with_webview error: {:?}", e))?;

    let timeout_duration = std::time::Duration::from_millis(timeout as u64);
    let result_str = {
        let guard = result_holder.lock().map_err(|_| "Lock poisoned")?;
        let (guard, timeout_result) = condvar
            .wait_timeout_while(guard, timeout_duration, |result| result.is_none())
            .map_err(|_| "Condvar wait failed")?;

        if timeout_result.timed_out() {
            return Err(format!(
                "JS_EXECUTION_ERROR: Execution timeout ({}ms)",
                timeout
            ));
        }

        guard
            .clone()
            .ok_or("JS_EXECUTION_ERROR: No result received")?
    }
    .map_err(|e| format!("JS_EXECUTION_ERROR: {}", e))?;

    tracing::debug!("Linux JS result: {}", result_str);

    parse_js_result(&result_str)
}

/// Execute JavaScript that may `await`, resolving the promise before returning.
///
/// [`webview_execute_js`] wraps the script in a *synchronous* IIFE, so a script
/// using `await` (or returning a promise) gets rejected with "Promise result not
/// supported in sync mode". This is the opt-in async counterpart, reached via
/// `exec-js --await`.
///
/// - **Windows**: CDP `Runtime.evaluate {awaitPromise: true}`. This runs on the
///   WebView2 browser thread rather than through `ExecuteScript`, which also
///   means the caller's timeout is enforced by the CDP transport.
/// - **macOS**: `-[WKWebView callAsyncJavaScript:…]`, whose body is an async
///   function body — `await` works natively.
/// - **Elsewhere**: unsupported; the caller should fall back or report it.
pub async fn webview_execute_js_async(
    script: &str,
    timeout_ms: Option<u32>,
) -> Result<serde_json::Value, String> {
    let timeout = timeout_ms.unwrap_or(30000);
    let execution_id = next_execution_id();
    tracing::info!(
        "execute_js(await): id={}, timeout={}ms, script_len={}",
        execution_id,
        timeout,
        script.len()
    );

    if crate::engine::is_chrome() {
        return crate::chrome::eval_page(script, Some(timeout), true).await;
    }

    #[cfg(target_os = "windows")]
    {
        let window = get_main_window()
            .await
            .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;
        execute_js_await_windows(&window, script, timeout).await
    }

    #[cfg(target_os = "macos")]
    {
        let window = get_main_window()
            .await
            .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;
        execute_js_await_macos(&window, script, timeout).await
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = script;
        Err(
            "UNSUPPORTED_PLATFORM: `--await` needs CDP (Windows) or callAsyncJavaScript (macOS)"
                .to_string(),
        )
    }
}

/// Windows: `Runtime.evaluate` with `awaitPromise`.
#[cfg(target_os = "windows")]
async fn execute_js_await_windows(
    window: &tauri::WebviewWindow,
    script: &str,
    timeout: u32,
) -> Result<serde_json::Value, String> {
    // The script is a *function body* (callers use `return`), but
    // `Runtime.evaluate` wants an expression — so wrap it in an async IIFE and
    // let `awaitPromise` resolve the result.
    let expression = format!("(async () => {{ {script} }})()");
    let params = serde_json::json!({
        "expression": expression,
        "awaitPromise": true,
        "returnByValue": true,
        "userGesture": true,
    })
    .to_string();

    // Give CDP a little headroom over the caller's timeout so we surface the
    // page's own timeout error rather than a transport timeout.
    let transport_timeout = std::time::Duration::from_millis(timeout as u64 + 2000);
    let v = crate::cdp::call(window, "Runtime.evaluate", &params, transport_timeout)
        .await
        .map_err(|e| format!("JS_EXECUTION_ERROR: {e}"))?;

    // WebView2 has been observed to hand back the method result both bare and
    // nested under an extra `result` envelope, so accept either shape.
    let exception = v
        .pointer("/exceptionDetails")
        .or_else(|| v.pointer("/result/exceptionDetails"));
    if let Some(exc) = exception {
        let msg = exc
            .pointer("/exception/description")
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("uncaught exception");
        return Err(format!("JS_EXECUTION_ERROR: {msg}"));
    }

    Ok(v.pointer("/result/value")
        .or_else(|| v.pointer("/result/result/value"))
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// macOS: `callAsyncJavaScript` — the body is an async function body, so the
/// caller's `await` works as written.
#[cfg(target_os = "macos")]
async fn execute_js_await_macos(
    window: &tauri::WebviewWindow,
    script: &str,
    timeout: u32,
) -> Result<serde_json::Value, String> {
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{class, msg_send};
    use std::sync::{Arc, Condvar, Mutex};

    // Same `{ok, value|error}` envelope as the sync path so `parse_js_result`
    // handles both identically.
    let wrapped = format!(
        r#"try {{
            var __v = await (async function() {{ {script} }})();
            return JSON.stringify({{ ok: true, value: __v }});
        }} catch (e) {{
            return JSON.stringify({{ ok: false, error: e.toString() }});
        }}"#,
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

            let ns_string_cstr = c"NSString";
            let ns_string_class: *mut AnyObject =
                msg_send![AnyClass::get(ns_string_cstr).unwrap(), alloc];
            let body_cstr = std::ffi::CString::new(wrapped.as_str()).unwrap_or_default();
            let ns_body: *mut AnyObject =
                msg_send![ns_string_class, initWithUTF8String: body_cstr.as_ptr()];

            // nil arguments / nil frame (main frame) / the page's own world, so
            // the script sees the same globals `exec-js` normally sees.
            let nil_args: *mut AnyObject = std::ptr::null_mut();
            let nil_frame: *mut AnyObject = std::ptr::null_mut();
            let world: *mut AnyObject = msg_send![class!(WKContentWorld), pageWorld];

            let _: () = msg_send![
                webview_ptr,
                callAsyncJavaScript: ns_body,
                arguments: nil_args,
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
            return Err(format!(
                "JS_EXECUTION_ERROR: Execution timeout ({timeout}ms)"
            ));
        }
        guard
            .clone()
            .ok_or("JS_EXECUTION_ERROR: No result received")?
    }
    .map_err(|e| format!("JS_EXECUTION_ERROR: {e}"))?;

    parse_js_result(&result_str)
}

/// Parse JS execution result JSON
pub(crate) fn parse_js_result(result_str: &str) -> Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(result_str) {
        Ok(parsed) => {
            if parsed["ok"].as_bool() == Some(true) {
                Ok(parsed["value"].clone())
            } else {
                let error_msg = parsed["error"].as_str().unwrap_or("Unknown error");
                Err(format!("JS_EXECUTION_ERROR: {}", error_msg))
            }
        }
        Err(e) => {
            // Every caller feeds this the output of `wrap_script`, so a payload
            // that will not parse means the bridge mangled it — not that the
            // page returned bare text. Returning the raw envelope as a "value"
            // is how `{"ok":true,"value":"[…]"}` reached `get-frames`, which
            // then failed to parse it as a frame list and reported an empty
            // page. Say what happened instead.
            Err(format!(
                "JS_RESULT_DECODE_ERROR: the bridge returned a payload that is not a valid \
                 result envelope ({e}). raw[{} bytes]: {}",
                result_str.len(),
                truncate_for_error(result_str)
            ))
        }
    }
}

/// Keep an error message readable when the payload is multi-KB.
fn truncate_for_error(s: &str) -> String {
    const LIMIT: usize = 300;
    if s.len() <= LIMIT {
        return s.to_string();
    }
    let mut cut = LIMIT;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[truncated]", &s[..cut])
}

/// Execute a script and deserialize its result, reporting failure as failure.
///
/// The idiom this replaces was duplicated at six call sites:
///
/// ```ignore
/// let result_str = val.as_str().unwrap_or("{}");
/// let parsed = serde_json::from_str(result_str).unwrap_or(<default>);
/// ```
///
/// Both `unwrap_or`s turn a decoding problem into plausible data — an empty
/// frame list, a form that filled zero fields, "all modals dismissed". The
/// caller then reports that fabricated default as fact. Requiring `T` and
/// returning `Err` with the raw payload makes that shape unrepresentable.
pub async fn exec_js_parse<T: serde::de::DeserializeOwned>(
    script: &str,
    timeout_ms: Option<u32>,
) -> Result<T, String> {
    let val = webview_execute_js(script, timeout_ms).await?;
    let result_str = val.as_str().ok_or_else(|| {
        format!(
            "JS_RESULT_TYPE_ERROR: expected a JSON string from the script, got {}",
            truncate_for_error(&val.to_string())
        )
    })?;
    serde_json::from_str(result_str).map_err(|e| {
        format!(
            "JS_RESULT_PARSE_ERROR: {e}. raw[{} bytes]: {}",
            result_str.len(),
            truncate_for_error(result_str)
        )
    })
}

// ===========================================================================
// Targeted execution — a specific frame and/or JS world (Windows/CDP only).
//
// The default path (`ICoreWebView2::ExecuteScript`) runs in the page's MAIN
// world but only in the TOP frame. That second half is easy to misread: a
// global set inside an iframe comes back `undefined`, which looks exactly like
// instrumentation that failed to run. Everything here exists so the caller can
// say where to run — and so every result says where it ran.
// ===========================================================================

/// Where a script ran, reported back with every result.
#[derive(Debug, Clone)]
pub struct ExecutionTarget {
    /// `main` | `isolated`
    pub world: String,
    pub context_id: Option<i64>,
    pub frame_id: Option<String>,
    pub frame_origin: Option<String>,
    /// `top` for the main document.
    pub frame_label: String,
    /// Set for a cross-site (out-of-process) frame: it is its own CDP target,
    /// reached through this session. `context_id` is then numbered by that
    /// target (or `None` for its default context).
    pub session_id: Option<String>,
}

impl ExecutionTarget {
    /// The default: main world, top frame — what `ExecuteScript` does.
    pub fn top_main() -> Self {
        Self {
            world: "main".into(),
            context_id: None,
            frame_id: None,
            frame_origin: None,
            frame_label: "top".into(),
            session_id: None,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "world": self.world,
            "frame": self.frame_label,
            "frame_id": self.frame_id,
            "frame_origin": self.frame_origin,
            "context_id": self.context_id,
        });
        if self.session_id.is_some() {
            v["out_of_process"] = serde_json::json!(true);
        }
        v
    }
}

/// The live URL of a cross-site frame target. Asked of the browser
/// (`Target.getTargetInfo`), not of the frame's renderer — so it answers while
/// that frame is busy computing a challenge.
#[cfg(target_os = "windows")]
pub async fn frame_target_url(window: &tauri::WebviewWindow, target_id: &str) -> Option<String> {
    let v = crate::cdp::call(
        window,
        "Target.getTargetInfo",
        &serde_json::json!({ "targetId": target_id }).to_string(),
        std::time::Duration::from_secs(3),
    )
    .await
    .ok()?;
    crate::cdp::result_at(&v, "targetInfo/url")
        .and_then(|u| u.as_str())
        .filter(|u| !u.is_empty())
        .map(String::from)
}

/// Find an attached cross-site frame by a substring of its URL or of its
/// `<iframe name>`: `(frame id, session id)`.
#[cfg(target_os = "windows")]
pub async fn out_of_process_frame_matching(
    window: &tauri::WebviewWindow,
    needle: &str,
) -> Option<(String, String)> {
    let sessions = crate::sw_console::frame_sessions();
    if sessions.is_empty() {
        return None;
    }
    let lower = needle.to_lowercase();
    // URL first — what the frame actually is.
    for (sid, fid, attach_url) in &sessions {
        let url = frame_target_url(window, fid)
            .await
            .unwrap_or_else(|| attach_url.clone());
        if url.to_lowercase().contains(&lower) {
            return Some((fid.clone(), sid.clone()));
        }
    }
    // Then the `name` the parent gave it, read from the parent's DOM in the
    // same order `dom_iframe_frame_ids` enumerates.
    let names = crate::cdp::call(
        window,
        "Runtime.evaluate",
        &serde_json::json!({
            "expression": "JSON.stringify(Array.from(document.querySelectorAll('iframe'), f => f.name || ''))",
            "returnByValue": true,
        })
        .to_string(),
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await
    .ok()
    .and_then(|v| {
        crate::cdp::result_at(&v, "result/value")
            .and_then(|x| x.as_str())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
    })?;
    let ids = dom_iframe_frame_ids(window).await.ok()?;
    names.iter().zip(ids.iter()).find_map(|(name, fid)| {
        let fid = fid.as_ref()?;
        if !name.to_lowercase().contains(&lower) {
            return None;
        }
        crate::sw_console::session_for_frame(fid).map(|sid| (fid.clone(), sid))
    })
}

/// The top frame's CDP id, fetched once per navigation.
///
/// `Page.getFrameTree` is a round trip; the answer only changes when the top
/// document is replaced, and the cache is cleared with the context store at
/// exactly that moment (see `clear_cdp_context_store`).
#[cfg(target_os = "windows")]
pub async fn cached_top_frame_id(window: &tauri::WebviewWindow) -> Option<String> {
    if let Some(id) = crate::state::CDP_TOP_FRAME_ID.read().await.clone() {
        return Some(id);
    }
    let id = top_frame_id(window).await.ok()?;
    *crate::state::CDP_TOP_FRAME_ID.write().await = Some(id.clone());
    Some(id)
}

/// Describe where the DEFAULT (untargeted) `exec-js` path runs.
///
/// `ExecuteScript` needs no context id to work, so this path used to report
/// `context_id: null` / `frame_origin: null` — breaking the "every result says
/// where it ran" contract precisely where it is most useful: telling a live
/// context apart from one left over from a replaced document. The values are
/// looked up rather than invented, so an unknown stays null.
#[cfg(target_os = "windows")]
pub async fn describe_default_target() -> ExecutionTarget {
    let mut target = ExecutionTarget::top_main();
    if crate::engine::is_chrome() {
        return chrome_default_target(target);
    }
    let Some(window) = crate::get_main_window().await else {
        return target;
    };
    let Some(frame_id) = cached_top_frame_id(&window).await else {
        return target;
    };
    if let Some(ctx) = main_context_for(&frame_id).await {
        target.context_id = Some(ctx.context_id);
        target.frame_origin = Some(ctx.origin);
    }
    target.frame_id = Some(frame_id);
    target
}

/// Off Windows there is no CDP, so there is nothing to look up — the honest
/// answer stays null rather than a guess.
#[cfg(not(target_os = "windows"))]
pub async fn describe_default_target() -> ExecutionTarget {
    let target = ExecutionTarget::top_main();
    if crate::engine::is_chrome() {
        return chrome_default_target(target);
    }
    target
}

/// The Chrome engine knows its top frame (the page target id) and that
/// frame's default context from its own event stream.
fn chrome_default_target(mut target: ExecutionTarget) -> ExecutionTarget {
    if let Some(b) = crate::chrome::current() {
        target.frame_origin = crate::chrome::context_origin(&b.target_id);
        if let Some((_, ctx)) = crate::chrome::main_context_for(&b.target_id) {
            target.context_id = ctx;
        }
        target.frame_id = Some(b.target_id.clone());
    }
    target
}

/// A frame we could run in, as CDP sees it.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct FrameCandidate {
    pub context_id: i64,
    pub frame_id: Option<String>,
    pub origin: String,
    pub name: String,
    pub is_default: bool,
}

/// Snapshot the execution contexts CDP has told us about.
///
/// Populated by the `Runtime.executionContextCreated` subscription set up in
/// `setup_cdp_for_webview`. Only **default (main-world) contexts** are listed:
/// the raw store also carries dalzzack's own "dalzzack" isolated worlds —
/// `Page.createIsolatedWorld` announces them like any other context — and
/// indexing those alongside the real frames is exactly what made two `--frame`
/// indexes land in the same DOM (measured on naver: pairs 0↔1, 2↔8, 5↔9).
/// Sorted by context id so the order is deterministic (insertion order is
/// per-event task-completion order, i.e. noise). The raw store stays unfiltered
/// for consumers that need isolated worlds (e.g. cross-origin iframe work).
#[cfg(target_os = "windows")]
pub async fn list_frame_contexts() -> Vec<FrameCandidate> {
    let store = crate::state::CDP_CONTEXT_STORE.read().await;
    let mut frames: Vec<FrameCandidate> = store
        .iter()
        .filter(|c| {
            c.aux_data
                .as_ref()
                .and_then(|a| a.is_default)
                .unwrap_or(false)
        })
        .map(|c| FrameCandidate {
            context_id: c.id,
            frame_id: c.aux_data.as_ref().and_then(|a| a.frame_id.clone()),
            origin: c.origin.clone(),
            name: c.name.clone(),
            is_default: true,
        })
        .collect();
    frames.sort_by_key(|f| f.context_id);
    frames
}

/// Truncate a string for display at a char boundary. Returns the (possibly
/// truncated) string and `Some(original_char_len)` when truncation happened.
///
/// Ad frames carry SafeFrame JSON in their `name` attribute (measured 4.4/6.1/
/// 9.8 KB on naver) — echoing that verbatim into every listing and error
/// payload buries the useful fields.
pub fn truncate_display(s: &str, cap: usize) -> (String, Option<usize>) {
    let char_len = s.chars().count();
    if char_len <= cap {
        return (s.to_string(), None);
    }
    (s.chars().take(cap).collect(), Some(char_len))
}

/// How much of a frame `name` survives into listings and error payloads.
pub const NAME_DISPLAY_CAP: usize = 200;

#[cfg(target_os = "windows")]
pub fn frame_candidates_json(
    frames: &[FrameCandidate],
    top_frame_id: Option<&str>,
) -> serde_json::Value {
    serde_json::Value::Array(
        frames
            .iter()
            .map(|f| {
                let (name, name_len) = truncate_display(&f.name, NAME_DISPLAY_CAP);
                // Deliberately no "index" here: `--frame N` indexes the DOM
                // iframe list (same order as get-frames), NOT this context
                // listing — an index here would teach the wrong coordinate.
                // The stable handle is `frame_id`.
                serde_json::json!({
                    "context_id": f.context_id,
                    "frame_id": f.frame_id,
                    "origin": f.origin,
                    "name": name,
                    "name_len": name_len,
                    // `isDefault` marks the default context OF ITS OWN FRAME —
                    // every frame has one — so it says nothing about being the
                    // top frame. Only the frame id can answer that.
                    "is_default_context": f.is_default,
                    "is_top": match (top_frame_id, f.frame_id.as_deref()) {
                        (Some(top), Some(fid)) => Some(top == fid),
                        _ => None,
                    },
                })
            })
            .collect(),
    )
}

/// A resolved `--frame` argument: the CDP frame id plus a display label.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct FrameResolution {
    pub frame_id: String,
    pub label: String,
}

/// Why a `--frame` argument could not be resolved. The caller turns each
/// variant into a distinct, actionable error payload.
#[cfg(target_os = "windows")]
#[derive(Debug)]
pub enum FrameResolveError {
    /// Numeric index outside the DOM iframe list (out of range is an ERROR —
    /// it used to silently hit whatever context happened to sit at that slot).
    IndexOutOfRange { idx: usize, dom_frame_count: usize },
    /// The iframe exists in the DOM but no CDP frame id could be derived.
    NoFrameId { idx: usize },
    /// Frame-id / substring lookup missed.
    NotFound { message: String },
    /// `top` requested but `Page.getFrameTree` gave no id.
    NoTopFrame,
    /// The DOM enumeration itself failed (CDP/renderer problem, not a miss).
    DomEnumerationFailed { message: String },
}

/// Map each DOM iframe to its CDP frame id, in `document.querySelectorAll('iframe')`
/// order — the SAME enumeration get-frames uses, so `--frame N` and
/// `get-frames` index N refer to the same frame by construction.
///
/// Per iframe: `Runtime.evaluate` (returnByValue:false) → objectId →
/// `DOM.describeNode` → `node.frameId` (no `DOM.enable` needed). A per-slot
/// failure yields `None` for that slot rather than failing the whole list, so
/// numeric targeting of a broken slot errors honestly instead of retargeting.
#[cfg(target_os = "windows")]
pub async fn dom_iframe_frame_ids(
    window: &tauri::WebviewWindow,
) -> Result<Vec<Option<String>>, String> {
    let v = crate::cdp::call(
        window,
        "Runtime.evaluate",
        &serde_json::json!({
            "expression": "document.querySelectorAll('iframe').length",
            "returnByValue": true,
        })
        .to_string(),
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await?;
    let count = crate::cdp::result_at(&v, "result/value")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| format!("iframe count query returned no number: {v}"))?
        as usize;

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let object_id = crate::cdp::call(
            window,
            "Runtime.evaluate",
            &serde_json::json!({
                "expression": format!("document.querySelectorAll('iframe')[{i}]"),
                "returnByValue": false,
            })
            .to_string(),
            crate::cdp::DEFAULT_TIMEOUT,
        )
        .await
        .ok()
        .and_then(|v| {
            crate::cdp::result_at(&v, "result/objectId").and_then(|x| x.as_str().map(String::from))
        });
        let Some(object_id) = object_id else {
            out.push(None);
            continue;
        };
        let frame_id = crate::cdp::call(
            window,
            "DOM.describeNode",
            &serde_json::json!({ "objectId": object_id }).to_string(),
            crate::cdp::DEFAULT_TIMEOUT,
        )
        .await
        .ok()
        .and_then(|v| {
            crate::cdp::result_at(&v, "node/frameId").and_then(|x| x.as_str().map(String::from))
        });
        out.push(frame_id);
    }
    Ok(out)
}

/// Pure core: the default (main-world) context recorded for `frame_id`.
#[cfg(target_os = "windows")]
pub fn context_for_frame_id<'a>(
    frames: &'a [FrameCandidate],
    frame_id: &str,
) -> Option<&'a FrameCandidate> {
    frames
        .iter()
        .find(|f| f.is_default && f.frame_id.as_deref() == Some(frame_id))
}

/// Pure core: case-insensitive substring match against origin or name.
#[cfg(target_os = "windows")]
pub fn pick_by_substring<'a>(
    frames: &'a [FrameCandidate],
    needle: &str,
) -> Option<&'a FrameCandidate> {
    let lower = needle.to_lowercase();
    frames.iter().find(|f| {
        f.origin.to_lowercase().contains(&lower) || f.name.to_lowercase().contains(&lower)
    })
}

/// Resolve a `--frame` argument to a CDP frame id.
///
/// Accepts `top`/`main` (the top document), a numeric index into the DOM
/// iframe list (same order as `get-frames`), an exact CDP frame id, or a
/// substring of a frame's origin or name. Frame identity is the frame id;
/// which execution context to use is decided separately per world.
#[cfg(target_os = "windows")]
pub async fn resolve_frame_id(
    window: &tauri::WebviewWindow,
    frame: &str,
    top_frame_id: Option<&str>,
) -> Result<FrameResolution, FrameResolveError> {
    if frame.eq_ignore_ascii_case("top") || frame.eq_ignore_ascii_case("main") {
        return match top_frame_id {
            Some(fid) => Ok(FrameResolution {
                frame_id: fid.to_string(),
                label: "top".to_string(),
            }),
            None => Err(FrameResolveError::NoTopFrame),
        };
    }

    if let Ok(idx) = frame.parse::<usize>() {
        let ids = dom_iframe_frame_ids(window)
            .await
            .map_err(|e| FrameResolveError::DomEnumerationFailed { message: e })?;
        return match ids.get(idx) {
            None => Err(FrameResolveError::IndexOutOfRange {
                idx,
                dom_frame_count: ids.len(),
            }),
            Some(None) => Err(FrameResolveError::NoFrameId { idx }),
            Some(Some(fid)) => Ok(FrameResolution {
                frame_id: fid.clone(),
                label: format!("iframe[{idx}]"),
            }),
        };
    }

    let candidates = list_frame_contexts().await;

    // Exact frame id — accept when anything corroborates it exists: the top
    // frame, the context store, or the DOM iframe list. Unknown ids are
    // rejected rather than passed through to fail later as a raw CDP error.
    if top_frame_id == Some(frame)
        || candidates
            .iter()
            .any(|f| f.frame_id.as_deref() == Some(frame))
    {
        return Ok(FrameResolution {
            frame_id: frame.to_string(),
            label: frame.to_string(),
        });
    }
    if let Ok(ids) = dom_iframe_frame_ids(window).await {
        if ids.iter().flatten().any(|fid| fid == frame) {
            return Ok(FrameResolution {
                frame_id: frame.to_string(),
                label: frame.to_string(),
            });
        }
    }

    if let Some(hit) = pick_by_substring(&candidates, frame) {
        if let Some(fid) = hit.frame_id.clone() {
            return Ok(FrameResolution {
                frame_id: fid,
                label: frame.to_string(),
            });
        }
    }

    Err(FrameResolveError::NotFound {
        message: format!("no frame whose id, origin or name matches '{frame}'"),
    })
}

/// The default (main-world) execution context recorded for `frame_id`, if any.
///
/// Srcless / about:blank frames may never announce a default context — that is
/// an honest "no main world reachable", not a cue to fall back to the parent.
#[cfg(target_os = "windows")]
pub async fn main_context_for(frame_id: &str) -> Option<FrameCandidate> {
    let frames = list_frame_contexts().await;
    context_for_frame_id(&frames, frame_id).cloned()
}

/// Create (or reuse) a genuinely isolated world for `frame_id` and return its
/// execution context id.
///
/// This is the real thing, unlike `exec-js-isolated`, which despite its name
/// evaluates in the page's default context.
#[cfg(target_os = "windows")]
pub async fn isolated_context_for(
    window: &tauri::WebviewWindow,
    frame_id: &str,
) -> Result<i64, String> {
    let params = serde_json::json!({
        "frameId": frame_id,
        "worldName": "dalzzack",
        "grantUniveralAccess": true,
    })
    .to_string();
    let v = crate::cdp::call(
        window,
        "Page.createIsolatedWorld",
        &params,
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await?;
    crate::cdp::result_at(&v, "executionContextId")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| format!("Page.createIsolatedWorld returned no context id: {v}"))
}

/// [`isolated_context_for`] for a cross-site frame: the world is created by
/// that frame's own target, and the context id is in its numbering.
#[cfg(target_os = "windows")]
pub async fn isolated_context_in_session(
    window: &tauri::WebviewWindow,
    session_id: &str,
    frame_id: &str,
) -> Result<i64, String> {
    let params = serde_json::json!({
        "frameId": frame_id,
        "worldName": "dalzzack",
        "grantUniveralAccess": true,
    })
    .to_string();
    let v = crate::cdp::call_for_session(
        window,
        session_id,
        "Page.createIsolatedWorld",
        &params,
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await?;
    crate::cdp::result_at(&v, "executionContextId")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| format!("Page.createIsolatedWorld returned no context id: {v}"))
}

/// The top frame's CDP frame id, needed to create an isolated world there.
#[cfg(target_os = "windows")]
pub async fn top_frame_id(window: &tauri::WebviewWindow) -> Result<String, String> {
    let v = crate::cdp::call(
        window,
        "Page.getFrameTree",
        "{}",
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await?;
    crate::cdp::result_at(&v, "frameTree/frame/id")
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| format!("Page.getFrameTree returned no frame id: {v}"))
}

/// The top frame's URL, read from the **browser process**.
///
/// `Page.getFrameTree` is answered outside the renderer, so this keeps working
/// through the seconds-long stretches where a busy (or mid-navigation) page
/// cannot evaluate JS — exactly when "did we get there yet?" is being asked.
#[cfg(target_os = "windows")]
pub async fn top_frame_url(window: &tauri::WebviewWindow) -> Result<String, String> {
    let v = crate::cdp::call(
        window,
        "Page.getFrameTree",
        "{}",
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await?;
    crate::cdp::result_at(&v, "frameTree/frame/url")
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| format!("Page.getFrameTree returned no frame url: {v}"))
}

/// Evaluate in a CDP session that is not the page — a service worker's.
///
/// A worker has one execution context, so no `contextId` is needed: the
/// session itself is the target. Same wrapper and decoding as every other path,
/// so `return` and `--await` behave the same as in the page.
#[cfg(target_os = "windows")]
pub async fn execute_js_in_session(
    session_id: &str,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<serde_json::Value, String> {
    execute_js_in_session_context(session_id, None, script, timeout_ms, await_promise).await
}

/// As [`execute_js_in_session`], optionally in one of that session's own
/// contexts (an isolated world created in a cross-site frame).
#[cfg(target_os = "windows")]
pub async fn execute_js_in_session_context(
    session_id: &str,
    context_id: Option<i64>,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<serde_json::Value, String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;
    let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(30000) as u64);
    let mut params = serde_json::json!({
        "expression": if await_promise {
            wrap_script_async(script)
        } else {
            wrap_script(script)
        },
        "returnByValue": true,
        "awaitPromise": await_promise,
    });
    if let Some(ctx) = context_id {
        params["contextId"] = serde_json::json!(ctx);
    }
    let params = params.to_string();
    let v = crate::cdp::call_for_session(&window, session_id, "Runtime.evaluate", &params, timeout)
        .await?;
    if let Some(exc) = crate::cdp::result_at(&v, "exceptionDetails") {
        return Err(format!("JS_EXECUTION_ERROR: {exc}"));
    }
    let raw = crate::cdp::result_at(&v, "result/value")
        .and_then(|x| x.as_str())
        .unwrap_or("null");
    parse_js_result(raw)
}

/// Evaluate in a specific execution context via CDP.
#[cfg(target_os = "windows")]
pub async fn execute_js_in_context(
    context_id: i64,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<serde_json::Value, String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;
    let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(30000) as u64);

    let params = serde_json::json!({
        "expression": if await_promise {
            wrap_script_async(script)
        } else {
            wrap_script(script)
        },
        "contextId": context_id,
        "returnByValue": true,
        "awaitPromise": await_promise,
    })
    .to_string();

    let v = crate::cdp::call(&window, "Runtime.evaluate", &params, timeout).await?;

    if let Some(exc) = crate::cdp::result_at(&v, "exceptionDetails") {
        return Err(format!("JS_EXECUTION_ERROR: {exc}"));
    }
    let raw = crate::cdp::result_at(&v, "result/value")
        .and_then(|x| x.as_str())
        .unwrap_or("null");
    parse_js_result(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression that started this: WebView2 hands back the JSON encoding
    /// of the result, and the old five-`replace` chain corrupted it.
    #[test]
    fn decodes_a_plain_string() {
        assert_eq!(decode_result_json(r#""hi""#), "hi");
        assert_eq!(decode_result_json(r#""""#), "");
    }

    #[test]
    fn crlf_survives_the_round_trip() {
        // The old chain restored `\r\n` from `\r\n` and then immediately
        // rewrote those into raw CR/LF bytes, which are illegal inside a JSON
        // string — the envelope stopped parsing.
        let encoded = serde_json::to_string("line1\r\nline2").unwrap();
        assert_eq!(decode_result_json(&encoded), "line1\r\nline2");
    }

    #[test]
    fn a_backslash_before_a_quote_does_not_terminate_the_string() {
        // Unescaping `\"` before `\` ended the string early. Escaped
        // `</script>` blobs are exactly this shape.
        let raw = r#"path a\b and "quoted" text"#;
        let encoded = serde_json::to_string(raw).unwrap();
        assert_eq!(decode_result_json(&encoded), raw);
    }

    #[test]
    fn unicode_and_solidus_escapes_are_handled() {
        // `\uXXXX`, `\/`, `\b`, `\f` were not handled at all.
        assert_eq!(decode_result_json(r#""<script>""#), "<script>");
        assert_eq!(decode_result_json(r#""a\/b""#), "a/b");
        assert_eq!(decode_result_json(r#""a\bb\fc""#), "a\u{8}b\u{c}c");
    }

    #[test]
    fn the_safeframe_shape_survives() {
        // A multi-KB payload combining every failing condition at once: escaped
        // quotes, an escaped `</script>`, CRLF, and a literal backslash. This is
        // the iframe `name` that made `get-frames` report an empty page.
        let payload = format!(
            r#"{{"html":"<script>var a=1;</script>\r\n<div class="ad">x</div>","path":"a\b","pad":"{}"}}"#,
            "P".repeat(3000)
        );
        let encoded = serde_json::to_string(&payload).unwrap();
        assert_eq!(decode_result_json(&encoded), payload);
        assert!(payload.len() > 3000);
    }

    #[test]
    fn a_whole_envelope_survives_and_still_parses() {
        // End to end: the envelope must come back out of the bridge as valid
        // JSON, because `parse_js_result` has to parse it next.
        let inner = serde_json::json!([{ "name": "a\r\nb\"c", "src": "x" }]).to_string();
        let envelope = serde_json::json!({ "ok": true, "value": inner }).to_string();
        let encoded = serde_json::to_string(&envelope).unwrap();

        let decoded = decode_result_json(&encoded);
        let parsed: serde_json::Value =
            serde_json::from_str(&decoded).expect("envelope must parse");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value"].as_str().unwrap(), inner);
    }

    #[test]
    fn non_string_results_pass_through_unchanged() {
        // Numbers, objects and null are not JSON strings; previous behaviour
        // returned them verbatim and callers depend on that.
        assert_eq!(decode_result_json("null"), "null");
        assert_eq!(decode_result_json("42"), "42");
        assert_eq!(decode_result_json(r#"{"a":1}"#), r#"{"a":1}"#);
    }

    #[test]
    fn envelope_decode_failure_is_an_error_not_a_fake_value() {
        // The swallow that let `{"ok":true,"value":"[…]"}` reach `get-frames`
        // as if it were the frame list.
        let err = parse_js_result(r#"{"ok":true,"value":"[broken"#).unwrap_err();
        assert!(err.contains("JS_RESULT_DECODE_ERROR"), "{err}");
        assert!(err.contains("raw["), "error must carry the payload: {err}");
    }

    #[test]
    fn a_good_envelope_still_unwraps() {
        let ok = parse_js_result(r#"{"ok":true,"value":"[]"}"#).unwrap();
        assert_eq!(ok.as_str(), Some("[]"));

        let err = parse_js_result(r#"{"ok":false,"error":"boom"}"#).unwrap_err();
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn error_payloads_are_truncated_but_marked() {
        let long = "x".repeat(5000);
        let out = truncate_for_error(&long);
        assert!(out.len() < 400, "len {}", out.len());
        assert!(out.ends_with("…[truncated]"));
        // Short payloads are shown whole.
        assert_eq!(truncate_for_error("short"), "short");
    }

    #[test]
    fn async_wrapper_is_used_only_where_await_is_possible() {
        // `--await` failed because the sync wrapper is a plain function, making
        // top-level `await` a syntax error.
        assert!(wrap_script("return 1").contains("(function()"));
        assert!(!wrap_script("return 1").contains("async"));

        let a = wrap_script_async("const r = await fetch('/x'); return r.status");
        assert!(a.contains("(async function()"));
        assert!(a.contains("await (async function()"));
        // The sync-mode promise rejection must not fire in async mode.
        assert!(!a.contains("Promise result not supported"));
    }

    #[cfg(target_os = "windows")]
    fn candidate(
        id: i64,
        frame_id: Option<&str>,
        origin: &str,
        name: &str,
        is_default: bool,
    ) -> FrameCandidate {
        FrameCandidate {
            context_id: id,
            frame_id: frame_id.map(String::from),
            origin: origin.to_string(),
            name: name.to_string(),
            is_default,
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn context_lookup_ignores_isolated_worlds() {
        // The naver bug in miniature: the same frame announces a default
        // context AND a "dalzzack" isolated world. Only the default one is
        // the frame's main world.
        let frames = vec![
            candidate(7, Some("F1"), "https://a.example", "", true),
            candidate(9, Some("F1"), "", "dalzzack", false),
            candidate(3, Some("F2"), "https://b.example", "", true),
        ];
        assert_eq!(context_for_frame_id(&frames, "F1").unwrap().context_id, 7);
        assert_eq!(context_for_frame_id(&frames, "F2").unwrap().context_id, 3);
        assert!(context_for_frame_id(&frames, "F3").is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn substring_match_hits_origin_and_name_case_insensitively() {
        let frames = vec![
            candidate(1, Some("F1"), "https://wtm.pstatic.net", "", true),
            candidate(2, Some("F2"), "https://ads.example", "SafeFrame-slot", true),
        ];
        assert_eq!(pick_by_substring(&frames, "PSTATIC").unwrap().context_id, 1);
        assert_eq!(
            pick_by_substring(&frames, "safeframe").unwrap().context_id,
            2
        );
        assert!(pick_by_substring(&frames, "nomatch").is_none());
    }

    #[test]
    fn truncate_display_is_char_boundary_safe() {
        // Under the cap: untouched, no length reported.
        assert_eq!(truncate_display("short", 200), ("short".to_string(), None));

        // Over the cap with multi-byte chars: cut by CHARS, not bytes.
        let wide = "\u{D55C}".repeat(250); // a 3-byte UTF-8 char
        let (cut, len) = truncate_display(&wide, 200);
        assert_eq!(cut.chars().count(), 200);
        assert_eq!(len, Some(250));

        // Exactly at the cap: untouched.
        let exact = "x".repeat(200);
        assert_eq!(truncate_display(&exact, 200), (exact.clone(), None));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn frame_candidates_json_truncates_names_and_has_no_index() {
        let frames = vec![candidate(
            1,
            Some("F1"),
            "https://ads.example",
            &"{\"safeframe\":1,".repeat(400), // ~6KB, the measured naver shape
            true,
        )];
        let v = frame_candidates_json(&frames, Some("TOP"));
        let entry = &v.as_array().unwrap()[0];
        let name = entry["name"].as_str().unwrap();
        assert!(name.chars().count() <= NAME_DISPLAY_CAP, "{}", name.len());
        assert!(entry["name_len"].as_u64().unwrap() > 200);
        // `--frame N` indexes the DOM iframe list, not this listing — an
        // "index" field here would teach the wrong coordinate.
        assert!(entry.get("index").is_none());
        assert_eq!(entry["is_top"].as_bool(), Some(false));
    }
}
