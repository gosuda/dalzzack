//! Shared Chrome DevTools Protocol transport (Windows / WebView2).
//!
//! WebView2's `CallDevToolsProtocolMethod` is a genuine CDP transport that runs
//! out-of-band from the renderer's script execution, so these request/response
//! calls answer even while page JS is busy. Several subsystems need it
//! (`diagnostics`, `clear-site-data`, `exec-js --await`, `sw-console`), so the
//! transport lives here instead of being copy-pasted per call site.
//!
//! Two entry points:
//! - [`call`] — the default (page) session.
//! - [`call_for_session`] — a specific CDP session id, via `ICoreWebView2_11`.
//!   Needed to talk to non-page targets such as service workers, which the
//!   default session cannot reach.

use serde_json::Value;
use std::time::Duration;
use tauri::WebviewWindow;

/// Default timeout for a CDP request/response round trip.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Read a field out of a CDP method result, tolerating both response shapes.
///
/// WebView2's `returnObjectAsJson` is normally the method's result object
/// *bare* (`Page.getFrameTree` → `{"frameTree":…}`), but it has been observed
/// wrapped in an extra `result` envelope. Reaching for only one shape silently
/// yields `None` on the other — which is how `pause` came to report an empty
/// `js_stack` even on a perfectly healthy renderer. Always go through here.
///
/// `path` is a JSON pointer *without* the leading slash, e.g. `"frameTree/frame/id"`.
pub fn result_at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    v.pointer(&format!("/{path}"))
        .or_else(|| v.pointer(&format!("/result/{path}")))
}

struct CdpRaw {
    ok: bool,
    json: String,
    error: Option<String>,
}

/// Call a CDP method on the default (page) session and return the parsed
/// `returnObjectAsJson` payload.
pub async fn call(
    window: &WebviewWindow,
    method: &str,
    params_json: &str,
    timeout: Duration,
) -> Result<Value, String> {
    call_inner(window, None, method, params_json, timeout).await
}

/// Call a CDP method scoped to `session_id` — used for targets the default
/// session can't reach (service workers, workers, OOPIFs).
///
/// Requires the `ICoreWebView2_11` interface (WebView2 runtime 1.0.1185.39+).
pub async fn call_for_session(
    window: &WebviewWindow,
    session_id: &str,
    method: &str,
    params_json: &str,
    timeout: Duration,
) -> Result<Value, String> {
    call_inner(window, Some(session_id), method, params_json, timeout).await
}

async fn call_inner(
    window: &WebviewWindow,
    session_id: Option<&str>,
    method: &str,
    params_json: &str,
    timeout: Duration,
) -> Result<Value, String> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl, ICoreWebView2_11,
    };
    use windows_core::{implement, Interface, HRESULT, HSTRING, PCWSTR};

    let (tx, rx) = mpsc::channel::<CdpRaw>();

    #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
    struct Handler {
        tx: mpsc::Sender<CdpRaw>,
    }

    impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for Handler_Impl {
        fn Invoke(
            &self,
            errorcode: HRESULT,
            returnobjectasjson: &PCWSTR,
        ) -> windows_core::Result<()> {
            if errorcode.is_err() {
                // WebView2 puts the CDP error object ({"code":-32000,"message":...})
                // in returnobjectasjson even on a failed HRESULT — dropping it turns
                // "Cannot find context with specified id" into an opaque 0x80070057.
                let detail = unsafe { returnobjectasjson.to_string() }.unwrap_or_default();
                let detail = if detail.trim().is_empty() || detail.trim() == "null" {
                    "<no CDP error payload>".to_string()
                } else {
                    detail
                };
                let _ = self.tx.send(CdpRaw {
                    ok: false,
                    json: String::new(),
                    error: Some(format!("CDP error: {:?}: {}", errorcode, detail)),
                });
                return Ok(());
            }
            let json_str = unsafe { returnobjectasjson.to_string() }.unwrap_or_default();
            let _ = self.tx.send(CdpRaw {
                ok: true,
                json: json_str,
                error: None,
            });
            Ok(())
        }
    }

    let method_owned = method.to_string();
    let params_owned = params_json.to_string();
    let session_owned = session_id.map(|s| s.to_string());
    let tx_clone = tx.clone();

    let cb = window.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        let core: ICoreWebView2 = match controller.CoreWebView2() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx_clone.send(CdpRaw {
                    ok: false,
                    json: String::new(),
                    error: Some(format!("CoreWebView2 error: {:?}", e)),
                });
                return;
            }
        };
        let handler: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler = Handler {
            tx: tx_clone.clone(),
        }
        .into();
        let method_h = HSTRING::from(method_owned.as_str());
        let params_h = HSTRING::from(params_owned.as_str());

        let call_result = match &session_owned {
            None => core.CallDevToolsProtocolMethod(&method_h, &params_h, &handler),
            Some(sid) => match core.cast::<ICoreWebView2_11>() {
                Ok(cw11) => {
                    let sid_h = HSTRING::from(sid.as_str());
                    cw11.CallDevToolsProtocolMethodForSession(
                        &sid_h, &method_h, &params_h, &handler,
                    )
                }
                Err(e) => {
                    let _ = tx_clone.send(CdpRaw {
                        ok: false,
                        json: String::new(),
                        error: Some(format!(
                            "session-scoped CDP needs ICoreWebView2_11 (WebView2 runtime \
                             1.0.1185.39+); QueryInterface failed: {e:?}"
                        )),
                    });
                    return;
                }
            },
        };

        if let Err(e) = call_result {
            let _ = tx_clone.send(CdpRaw {
                ok: false,
                json: String::new(),
                error: Some(format!("CallDevToolsProtocolMethod error: {:?}", e)),
            });
        }
    });

    if let Err(e) = cb {
        return Err(format!("with_webview failed: {e:?}"));
    }

    match rx.recv_timeout(timeout) {
        Ok(r) if r.ok => serde_json::from_str::<Value>(&r.json)
            .map_err(|e| format!("CDP returned non-JSON: {e}")),
        Ok(r) => Err(r.error.unwrap_or_else(|| "CDP call failed".into())),
        Err(_) => Err(format!(
            "CDP response timed out ({}ms) — renderer may be wedged",
            timeout.as_millis()
        )),
    }
}
