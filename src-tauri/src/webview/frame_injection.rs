//! WebView2 frame injection — utilities for cross-origin iframe access.
//!
//! Uses the Chrome DevTools Protocol (CDP) to reach into cross-origin iframes;
//! CDP can access every frame regardless of the Same-Origin Policy.
use anyhow::Result;

#[cfg(target_os = "windows")]
use tauri::WebviewWindow;

/// Result of a CDP call.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct CdpResult {
    pub success: bool,
    pub data: Option<String>,
    pub error: Option<String>,
}

/// Fetch the main frame's HTML via CDP.
#[cfg(target_os = "windows")]
pub async fn get_main_frame_html_via_cdp(webview: &WebviewWindow) -> Result<Option<String>> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl,
    };
    use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

    let (tx, rx) = mpsc::channel::<CdpResult>();

    #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
    struct HtmlHandler {
        tx: mpsc::Sender<CdpResult>,
    }

    impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for HtmlHandler_Impl {
        fn Invoke(
            &self,
            errorcode: HRESULT,
            returnobjectasjson: &PCWSTR,
        ) -> windows_core::Result<()> {
            if errorcode.is_err() {
                let _ = self.tx.send(CdpResult {
                    success: false,
                    data: None,
                    error: Some(format!("CDP error: {:?}", errorcode)),
                });
                return Ok(());
            }

            let json_str = unsafe { returnobjectasjson.to_string() }.unwrap_or_default();

            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if let Some(result) = parsed.get("result") {
                    if let Some(value) = result.get("value") {
                        if let Some(html) = value.as_str() {
                            let _ = self.tx.send(CdpResult {
                                success: true,
                                data: Some(html.to_string()),
                                error: None,
                            });
                            return Ok(());
                        }
                    }
                }
            }

            let _ = self.tx.send(CdpResult {
                success: false,
                data: None,
                error: Some("Failed to parse result".to_string()),
            });
            Ok(())
        }
    }

    let tx_clone = tx.clone();

    let cdp_result = webview.with_webview(move |wv| {
        unsafe {
            let controller = wv.controller();
            let core: ICoreWebView2 = match controller.CoreWebView2() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx_clone.send(CdpResult {
                        success: false,
                        data: None,
                        error: Some(format!("CoreWebView2 error: {:?}", e)),
                    });
                    return;
                }
            };

            let handler: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler =
                HtmlHandler { tx: tx_clone.clone() }.into();

            let method = HSTRING::from("Runtime.evaluate");
            let params = HSTRING::from(r#"{
                "expression": "(function() { return document.documentElement ? document.documentElement.outerHTML : ''; })()",
                "returnByValue": true
            }"#);

            if let Err(e) = core.CallDevToolsProtocolMethod(&method, &params, &handler) {
                let _ = tx_clone.send(CdpResult {
                    success: false,
                    data: None,
                    error: Some(format!("CallDevToolsProtocolMethod error: {:?}", e)),
                });
            }
        }
    });

    if let Err(e) = cdp_result {
        tracing::debug!("[CDP] with_webview failed: {:?}", e);
        return Ok(None);
    }

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(result) if result.success => Ok(result.data),
        Ok(result) => {
            tracing::debug!("[CDP] failed to fetch main frame HTML: {:?}", result.error);
            Ok(None)
        }
        Err(e) => {
            tracing::debug!("[CDP] response wait timed out: {:?}", e);
            Ok(None)
        }
    }
}

// ============================================================================
// Cross-origin iframe access via CDP event subscription
// ============================================================================

use std::sync::Arc;
use tokio::sync::RwLock;

/// ExecutionContext info payload (CDP `Runtime.executionContextCreated` event).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ExecutionContextInfo {
    pub context: ExecutionContext,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ExecutionContext {
    pub id: i64,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "auxData")]
    pub aux_data: Option<ExecutionContextAuxData>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ExecutionContextAuxData {
    #[serde(rename = "frameId")]
    pub frame_id: Option<String>,
    #[serde(rename = "isDefault")]
    pub is_default: Option<bool>,
    #[serde(rename = "type")]
    pub context_type: Option<String>,
}

/// Thread-safe store of ExecutionContexts.
pub type ExecutionContextStore = Arc<RwLock<Vec<ExecutionContext>>>;

/// Enable the Runtime domain (required to receive CDP events).
#[cfg(target_os = "windows")]
pub async fn enable_runtime_domain(webview: &WebviewWindow) -> Result<bool> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl,
    };
    use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

    let (tx, rx) = mpsc::channel::<bool>();

    #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
    struct EnableHandler {
        tx: mpsc::Sender<bool>,
    }

    impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for EnableHandler_Impl {
        fn Invoke(&self, errorcode: HRESULT, _: &PCWSTR) -> windows_core::Result<()> {
            let _ = self.tx.send(errorcode.is_ok());
            Ok(())
        }
    }

    let tx_clone = tx.clone();
    let cdp_result = webview.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        if let Ok(core) = controller.CoreWebView2() {
            let core: ICoreWebView2 = core;
            let handler: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler = EnableHandler {
                tx: tx_clone.clone(),
            }
            .into();

            let method = HSTRING::from("Runtime.enable");
            let params = HSTRING::from("{}");

            if let Err(e) = core.CallDevToolsProtocolMethod(&method, &params, &handler) {
                tracing::debug!("[CDP] Runtime.enable call failed: {:?}", e);
                let _ = tx_clone.send(false);
            }
        } else {
            let _ = tx_clone.send(false);
        }
    });

    if let Err(e) = cdp_result {
        tracing::debug!("[CDP] with_webview failed (Runtime.enable): {:?}", e);
        return Ok(false);
    }

    // Wrap the blocking recv_timeout in spawn_blocking so it does not block a tokio worker thread
    let result = tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    Ok(result)
}

/// Subscribe to `Runtime.executionContextCreated` events.
///
/// # Important
/// WebView2's CDP event handler (`Invoke`) runs on the **WebView2 UI thread**,
/// which is unrelated to the tokio runtime. Calling `tokio::spawn` directly from
/// there panics with "No current runtime" or silently fails.
///
/// Workaround: capture a `Handle::current()` from an async context up front and
/// use `handle.spawn(...)` inside the COM handler.
#[cfg(target_os = "windows")]
pub async fn subscribe_execution_context_events(
    webview: &WebviewWindow,
    context_store: ExecutionContextStore,
) -> Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl,
    };
    use windows_core::{implement, Ref, HSTRING, PWSTR};

    let context_store_clone = context_store.clone();

    // ── Capture the tokio runtime handle (only possible in an async context) ──
    // WebView2 event handlers run on the UI thread, so capture the handle here and
    // pass it to the handlers. Without it, handle.spawn(...) does not work there.
    let tokio_handle = tokio::runtime::Handle::current();

    #[implement(ICoreWebView2DevToolsProtocolEventReceivedEventHandler)]
    struct ContextEventHandler {
        context_store: ExecutionContextStore,
        tokio_handle: tokio::runtime::Handle,
    }

    impl ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl for ContextEventHandler_Impl {
        fn Invoke(
            &self,
            _sender: Ref<'_, ICoreWebView2>,
            args: Ref<'_, ICoreWebView2DevToolsProtocolEventReceivedEventArgs>,
        ) -> windows_core::Result<()> {
            unsafe {
                if let Some(args_ref) = args.as_ref() {
                    // Only the page's own contexts belong in this store. Since
                    // `sw-console` auto-attaches workers and cross-site frames,
                    // their `Runtime` events reach this handler too — and a
                    // context id is unique only within its own target, so a
                    // worker's context 1 would have replaced the page's.
                    if crate::sw_console::read_session_id(args_ref).is_some() {
                        return Ok(());
                    }
                    let mut param_json: PWSTR = PWSTR::null();
                    if args_ref.ParameterObjectAsJson(&mut param_json).is_ok()
                        && !param_json.is_null()
                    {
                        {
                            let json_str = param_json.to_string().unwrap_or_default();

                            if let Ok(event_info) =
                                serde_json::from_str::<ExecutionContextInfo>(&json_str)
                            {
                                let store = self.context_store.clone();
                                let context = event_info.context;

                                // Update the sync mirror too. Console capture runs on the UI
                                // thread and cannot await the tokio RwLock, so this path is the
                                // only way it can put a frame_id on a log line.
                                crate::console::note_context_frame(
                                    context.id,
                                    context
                                        .aux_data
                                        .as_ref()
                                        .and_then(|a| a.frame_id.as_deref()),
                                );

                                // Spawn through the captured handle: safe on the WebView2 UI thread too
                                self.tokio_handle.spawn(async move {
                                    let mut contexts = store.write().await;
                                    // A re-announced id gets the newest data (skipping it would keep stale data)
                                    contexts.retain(|c| c.id != context.id);
                                    tracing::debug!(
                                        "[CDP] ExecutionContext received: id={}, origin={}",
                                        context.id,
                                        context.origin
                                    );
                                    contexts.push(context);
                                });
                            }

                            // Free the memory
                            windows::Win32::System::Com::CoTaskMemFree(Some(
                                param_json.as_ptr() as *const _
                            ));
                        }
                    }
                }
            }
            Ok(())
        }
    }

    let cdp_result = webview.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        if let Ok(core) = controller.CoreWebView2() {
            let core: ICoreWebView2 = core;

            let event_name = HSTRING::from("Runtime.executionContextCreated");
            match core.GetDevToolsProtocolEventReceiver(&event_name) {
                Ok(event_receiver) => {
                    let handler: ICoreWebView2DevToolsProtocolEventReceivedEventHandler =
                        ContextEventHandler {
                            context_store: context_store_clone.clone(),
                            tokio_handle: tokio_handle.clone(),
                        }
                        .into();

                    let mut token: i64 = 0;
                    match event_receiver.add_DevToolsProtocolEventReceived(&handler, &mut token) {
                        Ok(()) => {
                            tracing::debug!(
                                "[CDP] subscribed to Runtime.executionContextCreated (token: {})",
                                token
                            );
                        }
                        Err(e) => {
                            tracing::debug!("[CDP] event subscribe failed: {:?}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("[CDP] GetDevToolsProtocolEventReceiver failed: {:?}", e);
                }
            }
        } else {
            tracing::debug!("[CDP] CoreWebView2 fetch failed (subscribe)");
        }
    });

    if let Err(e) = cdp_result {
        tracing::debug!("[CDP] with_webview failed (event subscribe): {:?}", e);
    }

    Ok(())
}

/// Subscribe to `Runtime.executionContextDestroyed` / `Runtime.executionContextsCleared`
/// so the context store forgets contexts that no longer exist.
///
/// Without this the store only ever grows within a page's life (iframe reloads,
/// ad-slot recycling), and a stale context id handed to `Runtime.evaluate` comes
/// back as E_INVALIDARG (0x80070057). Same COM-handler pattern and threading
/// caveats as [`subscribe_execution_context_events`].
#[cfg(target_os = "windows")]
pub async fn subscribe_execution_context_lifecycle(
    webview: &WebviewWindow,
    context_store: ExecutionContextStore,
) -> Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl,
    };
    use windows_core::{implement, Ref, HSTRING, PWSTR};

    let tokio_handle = tokio::runtime::Handle::current();

    #[implement(ICoreWebView2DevToolsProtocolEventReceivedEventHandler)]
    struct LifecycleEventHandler {
        context_store: ExecutionContextStore,
        tokio_handle: tokio::runtime::Handle,
        // true = executionContextsCleared (clear all), false = executionContextDestroyed
        clear_all: bool,
    }

    impl ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl for LifecycleEventHandler_Impl {
        fn Invoke(
            &self,
            _sender: Ref<'_, ICoreWebView2>,
            args: Ref<'_, ICoreWebView2DevToolsProtocolEventReceivedEventArgs>,
        ) -> windows_core::Result<()> {
            unsafe {
                if let Some(args_ref) = args.as_ref() {
                    // A worker or cross-site frame session clearing ITS
                    // contexts must not clear the page's (see above).
                    if crate::sw_console::read_session_id(args_ref).is_some() {
                        return Ok(());
                    }
                    let mut param_json: PWSTR = PWSTR::null();
                    if args_ref.ParameterObjectAsJson(&mut param_json).is_ok()
                        && !param_json.is_null()
                    {
                        let json_str = param_json.to_string().unwrap_or_default();
                        let store = self.context_store.clone();

                        if self.clear_all {
                            self.tokio_handle.spawn(async move {
                                let mut contexts = store.write().await;
                                let n = contexts.len();
                                contexts.clear();
                                tracing::debug!(
                                    "[CDP] executionContextsCleared: dropped {n} contexts"
                                );
                            });
                        } else if let Some(id) =
                            serde_json::from_str::<serde_json::Value>(&json_str)
                                .ok()
                                .and_then(|v| v.get("executionContextId").and_then(|x| x.as_i64()))
                        {
                            self.tokio_handle.spawn(async move {
                                let mut contexts = store.write().await;
                                contexts.retain(|c| c.id != id);
                                tracing::debug!("[CDP] executionContextDestroyed: id={id} removed");
                            });
                        }

                        windows::Win32::System::Com::CoTaskMemFree(Some(
                            param_json.as_ptr() as *const _
                        ));
                    }
                }
            }
            Ok(())
        }
    }

    let store_clone = context_store.clone();
    let handle_clone = tokio_handle.clone();

    let cdp_result = webview.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        if let Ok(core) = controller.CoreWebView2() {
            let core: ICoreWebView2 = core;

            for (event, clear_all) in [
                ("Runtime.executionContextDestroyed", false),
                ("Runtime.executionContextsCleared", true),
            ] {
                let event_name = HSTRING::from(event);
                match core.GetDevToolsProtocolEventReceiver(&event_name) {
                    Ok(event_receiver) => {
                        let handler: ICoreWebView2DevToolsProtocolEventReceivedEventHandler =
                            LifecycleEventHandler {
                                context_store: store_clone.clone(),
                                tokio_handle: handle_clone.clone(),
                                clear_all,
                            }
                            .into();

                        let mut token: i64 = 0;
                        match event_receiver.add_DevToolsProtocolEventReceived(&handler, &mut token)
                        {
                            Ok(()) => {
                                tracing::debug!("[CDP] subscribed to {event}");
                            }
                            Err(e) => {
                                tracing::debug!("[CDP] event subscribe failed ({event}): {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            "[CDP] GetDevToolsProtocolEventReceiver failed ({event}): {:?}",
                            e
                        );
                    }
                }
            }
        } else {
            tracing::debug!("[CDP] CoreWebView2 fetch failed (lifecycle subscribe)");
        }
    });

    if let Err(e) = cdp_result {
        tracing::debug!("[CDP] with_webview failed (lifecycle subscribe): {:?}", e);
    }

    Ok(())
}

/// Execute JavaScript in a specific ExecutionContext.
#[cfg(target_os = "windows")]
pub async fn evaluate_in_context(
    webview: &WebviewWindow,
    context_id: i64,
    expression: &str,
) -> Result<Option<String>> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl,
    };
    use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

    let (tx, rx) = mpsc::channel::<Option<String>>();

    #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
    struct EvalHandler {
        tx: mpsc::Sender<Option<String>>,
    }

    impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for EvalHandler_Impl {
        fn Invoke(&self, errorcode: HRESULT, result: &PCWSTR) -> windows_core::Result<()> {
            if errorcode.is_err() {
                let _ = self.tx.send(None);
                return Ok(());
            }

            let json_str = unsafe { result.to_string() }.unwrap_or_default();

            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if let Some(result) = parsed.get("result") {
                    if let Some(value) = result.get("value") {
                        if let Some(html) = value.as_str() {
                            let _ = self.tx.send(Some(html.to_string()));
                            return Ok(());
                        }
                    }
                }
            }

            let _ = self.tx.send(None);
            Ok(())
        }
    }

    let expression = expression.to_string();
    let tx_clone = tx.clone();

    let cdp_result = webview.with_webview(move |wv| {
        unsafe {
            let controller = wv.controller();
            if let Ok(core) = controller.CoreWebView2() {
                let core: ICoreWebView2 = core;
                let handler: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler =
                    EvalHandler {
                        tx: tx_clone.clone(),
                    }
                    .into();

                let method = HSTRING::from("Runtime.evaluate");
                // Build the JSON with serde_json (hand escaping can produce invalid JSON)
                let params_json = serde_json::json!({
                    "expression": expression,
                    "contextId": context_id,
                    "returnByValue": true
                });
                let params = HSTRING::from(params_json.to_string());

                if let Err(e) = core.CallDevToolsProtocolMethod(&method, &params, &handler) {
                    tracing::debug!(
                        "[CDP] Runtime.evaluate failed (context {}): {:?}",
                        context_id,
                        e
                    );
                    let _ = tx_clone.send(None);
                }
            } else {
                let _ = tx_clone.send(None);
            }
        }
    });

    if let Err(e) = cdp_result {
        tracing::debug!("[CDP] with_webview failed (evaluate): {:?}", e);
        return Ok(None);
    }

    let result = tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .ok()
            .flatten()
    })
    .await
    .unwrap_or(None);

    Ok(result)
}

/// Set up CDP event subscriptions immediately after WebView creation.
/// Must be called before the first navigate.
///
/// # Arguments
/// * `webview` - WebView reference.
/// * `context_store` - ExecutionContextStore that receives events (created by the caller).
#[cfg(target_os = "windows")]
pub async fn setup_cdp_for_webview(
    webview: &WebviewWindow,
    context_store: ExecutionContextStore,
) -> Result<()> {
    // 1. Page.setBypassCSP — bypass CSP (replaces --disable-web-security)
    //
    // On by default because cross-origin iframe inspection depends on it, but
    // it silently makes the browser a poor witness for anything policy-related:
    // with enforcement off there are no violations to observe, and a page that
    // would be blocked in a real browser loads fine here. `--enforce-csp` keeps
    // the policy live; either way the mode is reported by `list` and by the
    // CSP-facing tools so "no violations" is never mistaken for "no policy".
    let bypass = crate::state::csp_bypassed();
    let params = if bypass {
        r#"{"enabled": true}"#
    } else {
        r#"{"enabled": false}"#
    };
    let bypass_ok = call_cdp_method(webview, "Page.setBypassCSP", params).await?;
    if bypass_ok {
        tracing::info!("[CDP] Page.setBypassCSP enabled={bypass}");
    } else {
        tracing::warn!("[CDP] Page.setBypassCSP failed to set enabled={bypass}");
    }

    // 2. Security.setIgnoreCertificateErrors — ignore certificate errors
    let cert_ok = call_cdp_method(
        webview,
        "Security.setIgnoreCertificateErrors",
        r#"{"ignore": true}"#,
    )
    .await?;
    if cert_ok {
        tracing::debug!("[CDP] Security.setIgnoreCertificateErrors enabled");
    } else {
        tracing::debug!("[CDP] Security.setIgnoreCertificateErrors failed (ignored)");
    }

    // 3. Subscribe to events first (Runtime.enable re-emits existing contexts, so order matters)
    subscribe_execution_context_events(webview, context_store.clone()).await?;
    subscribe_execution_context_lifecycle(webview, context_store).await?;
    tracing::debug!("[CDP] ExecutionContext event subscriptions set up");

    // 4. Enable the Runtime domain → existing contexts get re-emitted as executionContextCreated
    let enabled = enable_runtime_domain(webview).await?;
    if enabled {
        tracing::debug!("[CDP] Runtime domain enabled (existing contexts re-emitted)");
    } else {
        tracing::debug!("[CDP] Runtime domain enable failed");
    }

    Ok(())
}

/// Generic CDP method caller (Windows)
#[cfg(target_os = "windows")]
async fn call_cdp_method(webview: &WebviewWindow, method: &str, params: &str) -> Result<bool> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
        ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl,
    };
    use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

    let (tx, rx) = mpsc::channel::<bool>();

    #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
    struct GenericHandler {
        tx: mpsc::Sender<bool>,
    }

    impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for GenericHandler_Impl {
        fn Invoke(&self, errorcode: HRESULT, _: &PCWSTR) -> windows_core::Result<()> {
            let _ = self.tx.send(errorcode.is_ok());
            Ok(())
        }
    }

    let tx_clone = tx.clone();
    let method = method.to_string();
    let params = params.to_string();

    let cdp_result = webview.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        if let Ok(core) = controller.CoreWebView2() {
            let core: ICoreWebView2 = core;
            let handler: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler = GenericHandler {
                tx: tx_clone.clone(),
            }
            .into();

            let method_h = HSTRING::from(&method);
            let params_h = HSTRING::from(&params);

            if let Err(e) = core.CallDevToolsProtocolMethod(&method_h, &params_h, &handler) {
                tracing::debug!("[CDP] {} call failed: {:?}", method, e);
                let _ = tx_clone.send(false);
            }
        } else {
            let _ = tx_clone.send(false);
        }
    });

    if let Err(e) = cdp_result {
        tracing::debug!("[CDP] with_webview failed: {:?}", e);
        return Ok(false);
    }

    let result = tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    Ok(result)
}

/// Search stored ExecutionContexts for HTML containing the marker.
#[cfg(target_os = "windows")]
pub async fn search_marker_in_all_frames(
    webview: &WebviewWindow,
    context_store: &ExecutionContextStore,
    marker: &str,
) -> Result<Option<String>> {
    let contexts = context_store.read().await.clone();

    if contexts.is_empty() {
        tracing::debug!("[CDP] no ExecutionContexts collected; searching main frame only");
        // Search the main frame too
        if let Ok(Some(html)) = get_main_frame_html_via_cdp(webview).await {
            if html.contains(marker) {
                tracing::debug!("[CDP] marker found in main frame!");
                return Ok(Some(html));
            } else {
                tracing::debug!("[CDP] marker not in main frame");
            }
        }
        return Ok(None);
    }

    tracing::debug!(
        "[CDP] searching marker across {} ExecutionContext(s)",
        contexts.len()
    );

    for context in contexts {
        // Skip about:blank and similar
        if context.origin.starts_with("about:") || context.origin.is_empty() {
            continue;
        }

        tracing::debug!(
            "[CDP] searching Context {} (origin: {})",
            context.id,
            context.origin
        );

        let expression = "(function() { return document.documentElement ? document.documentElement.outerHTML : ''; })()";

        match evaluate_in_context(webview, context.id, expression).await {
            Ok(Some(html)) if html.contains(marker) => {
                tracing::debug!("[CDP] marker found! Context ID: {}", context.id);
                return Ok(Some(html));
            }
            Ok(Some(_)) => {
                tracing::debug!("[CDP] marker not in Context {}", context.id);
            }
            Ok(None) => {
                tracing::debug!("[CDP] failed to fetch HTML from Context {}", context.id);
            }
            Err(e) => {
                tracing::debug!("[CDP] Context {} evaluate error: {:?}", context.id, e);
            }
        }
    }

    tracing::debug!("[CDP] marker not found in any context");
    Ok(None)
}

/// Collect HTML from every ExecutionContext (no marker filter).
#[cfg(target_os = "windows")]
pub async fn get_all_frames_html(
    webview: &WebviewWindow,
    context_store: &ExecutionContextStore,
) -> Result<Vec<FrameHtml>> {
    let contexts = context_store.read().await.clone();
    let mut results = Vec::new();

    // Main frame
    if let Ok(Some(html)) = get_main_frame_html_via_cdp(webview).await {
        results.push(FrameHtml {
            context_id: 0,
            origin: "main".to_string(),
            html,
            is_main_frame: true,
        });
    }

    // Each ExecutionContext
    for context in contexts {
        if context.origin.starts_with("about:") || context.origin.is_empty() {
            continue;
        }

        let expression = "(function() { return document.documentElement ? document.documentElement.outerHTML : ''; })()";

        if let Ok(Some(html)) = evaluate_in_context(webview, context.id, expression).await {
            results.push(FrameHtml {
                context_id: context.id,
                origin: context.origin.clone(),
                html,
                is_main_frame: false,
            });
        }
    }

    Ok(results)
}

/// Windows: collect interactive elements from every ExecutionContext (for `describe`).
#[cfg(target_os = "windows")]
pub async fn describe_elements_in_all_frames(
    webview: &WebviewWindow,
    context_store: &ExecutionContextStore,
) -> Result<Vec<FrameDescribeResult>> {
    let contexts = context_store.read().await.clone();
    let mut results = Vec::new();

    for context in contexts {
        if context.origin.starts_with("about:") || context.origin.is_empty() {
            continue;
        }

        // Only iframes, not the main frame
        let is_default = context
            .aux_data
            .as_ref()
            .and_then(|a| a.is_default)
            .unwrap_or(false);
        let frame_id = context
            .aux_data
            .as_ref()
            .and_then(|a| a.frame_id.as_deref())
            .unwrap_or("");

        // Skip the main frame (JS already handles it directly)
        if is_default && frame_id.is_empty() {
            continue;
        }

        match evaluate_in_context(webview, context.id, FRAME_DESCRIBE_JS).await {
            Ok(Some(json_str)) => {
                if let Ok(result) = serde_json::from_str::<FrameDescribeResult>(&json_str) {
                    if !result.elements.is_empty() {
                        tracing::debug!(
                            "[CDP] describe: found {} elements (context {})",
                            result.elements.len(),
                            context.id
                        );
                        results.push(result);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("[CDP] describe context {} error: {:?}", context.id, e);
            }
        }
    }

    Ok(results)
}

/// Frame HTML result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrameHtml {
    pub context_id: i64,
    pub origin: String,
    pub html: String,
    pub is_main_frame: bool,
}

/// An interactive element inside a cross-origin iframe.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrameDescribeElement {
    pub tag: String,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Frame describe result (iframe interactive elements + form values).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrameDescribeResult {
    pub url: String,
    pub elements: Vec<FrameDescribeElement>,
    #[serde(rename = "formValues")]
    pub form_values: serde_json::Value,
    #[serde(rename = "isMainFrame")]
    pub is_main_frame: bool,
}

/// Shared JS payload for discovering elements inside cross-origin iframes (used by CDP).
pub const FRAME_DESCRIBE_JS: &str = "(function(){var els=Array.from(document.querySelectorAll('button,a[href],input,textarea,select')).filter(function(el){var r=el.getBoundingClientRect();return r.width>0&&r.height>0&&window.getComputedStyle(el).display!=='none'&&window.getComputedStyle(el).visibility!=='hidden'});var elements=els.map(function(el){var r=el.getBoundingClientRect();var t=(el.innerText||el.value||el.getAttribute('aria-label')||'').trim().substring(0,80);return{tag:el.tagName.toLowerCase(),text:t,x:r.left,y:r.top,w:r.width,h:r.height}});var fv={};document.querySelectorAll('input,textarea,select').forEach(function(el){if(el.value&&el.value.length>0){fv[el.name||el.id||el.getAttribute('aria-label')||el.tagName]=el.value.substring(0,200)}});return JSON.stringify({url:location.href,elements:elements,formValues:fv})})()";

// ============================================================================
// macOS: WKScriptMessageHandler + WKUserScript native bridge.
// Reaches into cross-origin iframes without disabling the SOP.
// ============================================================================

#[cfg(target_os = "macos")]
use std::sync::Mutex as StdMutex;

#[cfg(target_os = "macos")]
struct NativeFrameResult {
    json: String,
}

#[cfg(target_os = "macos")]
static NATIVE_SENDER: std::sync::LazyLock<
    StdMutex<Option<tokio::sync::mpsc::UnboundedSender<NativeFrameResult>>>,
> = std::sync::LazyLock::new(|| StdMutex::new(None));

#[cfg(target_os = "macos")]
fn set_native_sender(sender: Option<tokio::sync::mpsc::UnboundedSender<NativeFrameResult>>) {
    if let Ok(mut guard) = NATIVE_SENDER.lock() {
        *guard = sender;
    }
}

#[cfg(target_os = "macos")]
fn send_native_result(json: String) {
    if let Ok(guard) = NATIVE_SENDER.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(NativeFrameResult { json });
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_native {
    use super::*;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
    use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSObjectProtocol, NSString};
    use objc2_web_kit::{
        WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKUserScript,
        WKUserScriptInjectionTime,
    };

    define_class!(
        /// WKScriptMessageHandler — receives messages from cross-origin iframes.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "dalzzackFrameHandler"]
        struct dalzzackFrameHandler;

        unsafe impl NSObjectProtocol for dalzzackFrameHandler {}

        unsafe impl WKScriptMessageHandler for dalzzackFrameHandler {
            #[unsafe(method(userContentController:didReceiveScriptMessage:))]
            unsafe fn user_content_controller_did_receive_message(
                &self,
                _controller: &WKUserContentController,
                message: &WKScriptMessage,
            ) {
                let body: Retained<AnyObject> = message.body();
                let desc: Retained<NSString> = msg_send![&*body, description];
                let json = desc.to_string();

                if !json.is_empty() {
                    send_native_result(json);
                }
            }
        }
    );

    impl dalzzackFrameHandler {
        fn new_instance(mtm: MainThreadMarker) -> Retained<Self> {
            let alloc = Self::alloc(mtm);
            unsafe { msg_send![alloc, init] }
        }
    }

    /// Install the native frame handler on a WebView.
    ///
    /// Registers a WKScriptMessageHandler plus a WKUserScript(forMainFrameOnly: false).
    /// Must be called after WebView creation but before the first navigation.
    pub fn setup_native_frame_handler(webview: &tauri::WebviewWindow) -> Result<()> {
        let result = webview.with_webview(move |platform_webview| {
            unsafe {
                let mtm =
                    MainThreadMarker::new().expect("with_webview callback must run on main thread");

                let controller: &WKUserContentController = &*platform_webview.controller().cast();

                // 1. Register the WKScriptMessageHandler
                let handler = dalzzackFrameHandler::new_instance(mtm);
                controller.addScriptMessageHandler_name(
                    ProtocolObject::from_ref(&*handler),
                    &NSString::from_str(&format!("{}f", crate::pagens::namespace())),
                );

                // 2. Inject the WKUserScript (forMainFrameOnly: false, so cross-origin iframes too)
                let script_source = crate::pagens::render(&generate_native_responder_script());
                let user_script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    WKUserScript::alloc(mtm),
                    &NSString::from_str(&script_source),
                    WKUserScriptInjectionTime::AtDocumentStart,
                    false, // ← inject into every frame (cross-origin included)
                );
                controller.addUserScript(&user_script);

                tracing::info!("[NativeBridge] native frame handler set up");
            }
        });

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::debug!("[NativeBridge] with_webview failed: {:?}", e);
                Err(anyhow::anyhow!("with_webview failed: {:?}", e))
            }
        }
    }

    /// Search every frame for a marker via the native bridge.
    ///
    /// 1. Check the main frame's HTML.
    /// 2. Broadcast the search command to every iframe via `postMessage`.
    /// 3. The injected responder in each iframe posts results back via `webkit.messageHandlers`.
    /// 4. Rust collects the results (5s timeout).
    pub async fn search_marker_via_native(
        _webview: &tauri::WebviewWindow,
        marker: &str,
    ) -> Result<Option<String>> {
        use std::time::Duration;
        use tokio::sync::mpsc;

        let request_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        // 1. Set up the result channel
        let (tx, mut rx) = mpsc::unbounded_channel::<NativeFrameResult>();
        set_native_sender(Some(tx));

        // 2. Check the main frame and broadcast to every iframe
        let marker_json = serde_json::to_string(marker).unwrap_or_default();
        let search_script = format!(
            r#"(function() {{
                var html = document.documentElement ? document.documentElement.outerHTML : '';
                if (html.indexOf({mj}) !== -1) {{
                    try {{
                        window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                            JSON.stringify({{
                                type: 'search-result',
                                requestId: '{rid}',
                                url: window.location.href,
                                found: true,
                                html: html
                            }})
                        );
                    }} catch(e) {{}}
                    return;
                }}
                var iframes = document.querySelectorAll('iframe');
                for (var i = 0; i < iframes.length; i++) {{
                    try {{
                        iframes[i].contentWindow.postMessage({{
                            type: 'NS_PLACEHOLDERn-search',
                            marker: {mj},
                            requestId: '{rid}'
                        }}, '*');
                    }} catch(e) {{}}
                }}
            }})();"#,
            mj = marker_json,
            rid = request_id,
        );

        if let Err(e) =
            crate::webview_execute_js(&crate::pagens::render(&search_script), Some(5000)).await
        {
            set_native_sender(None);
            return Err(anyhow::anyhow!("eval failed: {:?}", e));
        }

        // 3. Collect the results (5 s timeout)
        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Some(r) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&r.json) {
                                let msg_type = parsed.get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                if msg_type == "search-result" {
                                    let found = parsed.get("found")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);

                                    if found {
                                        if let Some(html) = parsed.get("html").and_then(|v| v.as_str()) {
                                            let url = parsed.get("url")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown");
                                            tracing::info!(
                                                "[NativeBridge] marker found! frame: {}",
                                                url
                                            );
                                            set_native_sender(None);
                                            return Ok(Some(html.to_string()));
                                        }
                                    }
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = &mut timeout => {
                    tracing::debug!("[NativeBridge] search timeout (5s)");
                    break;
                }
            }
        }

        set_native_sender(None);
        Ok(None)
    }

    /// Collect HTML from every frame via the native bridge.
    pub async fn get_all_frames_native(_webview: &tauri::WebviewWindow) -> Result<Vec<FrameHtml>> {
        use std::time::Duration;
        use tokio::sync::mpsc;

        let request_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<NativeFrameResult>();
        set_native_sender(Some(tx));

        // Broadcast the HTML collect command to every frame
        let collect_script = format!(
            r#"(function() {{
                // Send the main frame's HTML
                try {{
                    window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                        JSON.stringify({{
                            type: 'collect-result',
                            requestId: '{rid}',
                            url: window.location.href,
                            origin: window.location.origin,
                            html: document.documentElement ? document.documentElement.outerHTML : '',
                            isMainFrame: true
                        }})
                    );
                }} catch(e) {{}}
                // Broadcast the collect command to the iframes
                var iframes = document.querySelectorAll('iframe');
                for (var i = 0; i < iframes.length; i++) {{
                    try {{
                        iframes[i].contentWindow.postMessage({{
                            type: 'NS_PLACEHOLDERn-collect',
                            requestId: '{rid}'
                        }}, '*');
                    }} catch(e) {{}}
                }}
            }})();"#,
            rid = request_id,
        );

        if let Err(e) =
            crate::webview_execute_js(&crate::pagens::render(&collect_script), Some(5000)).await
        {
            set_native_sender(None);
            return Err(anyhow::anyhow!("eval failed: {:?}", e));
        }

        let mut results = Vec::new();
        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Some(r) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&r.json) {
                                let msg_type = parsed.get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                if msg_type == "collect-result" {
                                    if let Some(html) = parsed.get("html").and_then(|v| v.as_str()) {
                                        let origin = parsed.get("origin")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown")
                                            .to_string();
                                        let is_main = parsed.get("isMainFrame")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);

                                        results.push(FrameHtml {
                                            context_id: results.len() as i64,
                                            origin,
                                            html: html.to_string(),
                                            is_main_frame: is_main,
                                        });
                                    }
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = &mut timeout => {
                    tracing::debug!(
                        "[NativeBridge] HTML collect timeout, collected {}",
                        results.len()
                    );
                    break;
                }
            }
        }

        set_native_sender(None);
        Ok(results)
    }

    /// Collect interactive elements from every frame via the native bridge (for `describe`).
    pub async fn describe_elements_in_all_frames_native(
        _webview: &tauri::WebviewWindow,
    ) -> Result<Vec<FrameDescribeResult>> {
        use std::time::Duration;
        use tokio::sync::mpsc;

        let request_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<NativeFrameResult>();
        set_native_sender(Some(tx));

        // Broadcast the describe command to every iframe (not the main frame: JS handles that directly)
        let describe_script = format!(
            r#"(function() {{
                var iframes = document.querySelectorAll('iframe');
                for (var i = 0; i < iframes.length; i++) {{
                    try {{
                        iframes[i].contentWindow.postMessage({{
                            type: 'NS_PLACEHOLDERn-describe',
                            requestId: '{rid}'
                        }}, '*');
                    }} catch(e) {{}}
                }}
            }})();"#,
            rid = request_id,
        );

        if let Err(e) =
            crate::webview_execute_js(&crate::pagens::render(&describe_script), Some(5000)).await
        {
            set_native_sender(None);
            return Err(anyhow::anyhow!("eval failed: {:?}", e));
        }

        let mut results = Vec::new();
        let timeout = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Some(r) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&r.json) {
                                let msg_type = parsed.get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                if msg_type == "describe-result" {
                                    let is_main = parsed.get("isMainFrame")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);

                                    // Skip the main frame (JS already handles it directly)
                                    if is_main {
                                        continue;
                                    }

                                    if let Ok(frame_result) = serde_json::from_value::<FrameDescribeResult>(parsed) {
                                        if !frame_result.elements.is_empty() {
                                            tracing::debug!(
                                                "[NativeBridge] describe: found {} elements ({})",
                                                frame_result.elements.len(),
                                                frame_result.url
                                            );
                                            results.push(frame_result);
                                        }
                                    }
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = &mut timeout => {
                    tracing::debug!(
                        "[NativeBridge] describe collect timeout, collected {} frames",
                        results.len()
                    );
                    break;
                }
            }
        }

        set_native_sender(None);
        Ok(results)
    }

    /// Click an element inside a cross-origin iframe via the native bridge.
    ///
    /// The main frame posts a message to the target iframe's `contentWindow`;
    /// the iframe's responder performs the click and posts the result back to the native handler.
    pub async fn click_in_iframe_native(
        local_x: f64,
        local_y: f64,
        click_type: &str,
        iframe_index: usize,
    ) -> Result<serde_json::Value> {
        use std::time::Duration;
        use tokio::sync::mpsc;

        let request_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<NativeFrameResult>();
        set_native_sender(Some(tx));

        let click_type_json = serde_json::to_string(click_type).unwrap_or_default();
        let script = format!(
            r#"(function() {{
                var iframes = document.querySelectorAll('iframe');
                var target = iframes[{idx}];
                if (target) {{
                    target.focus();
                    target.contentWindow.postMessage({{
                        type: 'NS_PLACEHOLDERn-click',
                        x: {x},
                        y: {y},
                        clickType: {ct},
                        requestId: '{rid}'
                    }}, '*');
                    return 'sent';
                }}
                return 'no_iframe';
            }})();"#,
            idx = iframe_index,
            x = local_x,
            y = local_y,
            ct = click_type_json,
            rid = request_id,
        );

        let eval_result =
            crate::webview_execute_js(&crate::pagens::render(&script), Some(3000)).await;
        if let Ok(ref v) = eval_result {
            if v.as_str() == Some("no_iframe") {
                set_native_sender(None);
                return Ok(serde_json::json!({"status": "failed", "error": "iframe not found"}));
            }
        }
        if eval_result.is_err() {
            set_native_sender(None);
            return Ok(serde_json::json!({"status": "failed", "error": "eval failed"}));
        }

        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        let result = loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(r) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&r.json) {
                                if parsed.get("type").and_then(|v| v.as_str()) == Some("click-result") {
                                    break parsed;
                                }
                            }
                        }
                        None => break serde_json::json!({"status": "failed", "error": "channel closed"}),
                    }
                }
                _ = &mut timeout => {
                    break serde_json::json!({"status": "failed", "error": "timeout"});
                }
            }
        };

        set_native_sender(None);
        Ok(result)
    }

    /// Send keyboard input into a cross-origin iframe via the native bridge.
    pub async fn keyboard_in_iframe_native(
        text: Option<&str>,
        key: Option<&str>,
        paste: bool,
        modifiers: &serde_json::Value,
        iframe_index: usize,
    ) -> Result<serde_json::Value> {
        use std::time::Duration;
        use tokio::sync::mpsc;

        let request_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<NativeFrameResult>();
        set_native_sender(Some(tx));

        let text_json = text
            .map(|t| serde_json::to_string(t).unwrap_or_default())
            .unwrap_or("null".to_string());
        let key_json = key
            .map(|k| serde_json::to_string(k).unwrap_or_default())
            .unwrap_or("null".to_string());
        let mods_json = modifiers.to_string();

        let script = format!(
            r#"(function() {{
                var iframes = document.querySelectorAll('iframe');
                var target = iframes[{idx}];
                if (target) {{
                    target.focus();
                    target.contentWindow.postMessage({{
                        type: 'NS_PLACEHOLDERn-keyboard',
                        text: {text},
                        key: {key},
                        paste: {paste},
                        modifiers: {mods},
                        requestId: '{rid}'
                    }}, '*');
                    return 'sent';
                }}
                return 'no_iframe';
            }})();"#,
            idx = iframe_index,
            text = text_json,
            key = key_json,
            paste = paste,
            mods = mods_json,
            rid = request_id,
        );

        let eval_result =
            crate::webview_execute_js(&crate::pagens::render(&script), Some(3000)).await;
        if let Ok(ref v) = eval_result {
            if v.as_str() == Some("no_iframe") {
                set_native_sender(None);
                return Ok(serde_json::json!({"status": "failed", "error": "iframe not found"}));
            }
        }
        if eval_result.is_err() {
            set_native_sender(None);
            return Ok(serde_json::json!({"status": "failed", "error": "eval failed"}));
        }

        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        let result = loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(r) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&r.json) {
                                if parsed.get("type").and_then(|v| v.as_str()) == Some("keyboard-result") {
                                    break parsed;
                                }
                            }
                        }
                        None => break serde_json::json!({"status": "failed", "error": "channel closed"}),
                    }
                }
                _ = &mut timeout => {
                    break serde_json::json!({"status": "failed", "error": "timeout"});
                }
            }
        };

        set_native_sender(None);
        Ok(result)
    }

    /// Responder script injected into every frame via WKUserScript.
    ///
    /// Runs in every frame (including cross-origin iframes) and:
    /// 1. Receives `postMessage` commands.
    /// 2. Inspects its own HTML.
    /// 3. Posts results back via `webkit.messageHandlers.NS_PLACEHOLDERf`.
    fn generate_native_responder_script() -> String {
        r#"
(function() {
    if (window.NS_PLACEHOLDERnr) return;
    window.NS_PLACEHOLDERnr = true;

    function relayToChildFrames(data) {
        var frames = document.querySelectorAll('iframe, frame');
        for (var i = 0; i < frames.length; i++) {
            try { frames[i].contentWindow.postMessage(data, '*'); } catch(e) {}
        }
    }

    window.addEventListener('message', function(event) {
        if (!event.data) return;

        // click command (clicks an element inside a cross-origin iframe)
        if (event.data.type === 'NS_PLACEHOLDERn-click') {
            var x = event.data.x;
            var y = event.data.y;
            var requestId = event.data.requestId;
            var clickType = event.data.clickType || 'left';
            try {
                var raw = document.elementFromPoint(x, y);
                if (raw) {
                    var clickTarget = raw;
                    // Walk up the ancestors only for non-HTML elements such as SVG (they have no .click())
                    if (typeof raw.click !== 'function') {
                        var el = raw;
                        while (el && typeof el.click !== 'function') el = el.parentElement;
                        clickTarget = el || raw;
                        // SVG case: find the nearest clickable ancestor (at most 5 levels up)
                        var anc = clickTarget; var depth = 0;
                        while (anc && anc !== document.body && depth < 5) {
                            var t = anc.tagName;
                            if (t === 'BUTTON' || t === 'A' || (anc.getAttribute && (
                                anc.getAttribute('role') === 'button' || anc.getAttribute('role') === 'option' ||
                                anc.getAttribute('role') === 'menuitem' || anc.getAttribute('role') === 'menuitemradio'
                            )) || anc.hasAttribute('onclick')) {
                                clickTarget = anc; break;
                            }
                            anc = anc.parentElement; depth++;
                        }
                    }
                    clickTarget.scrollIntoView({block: 'nearest'});
                    try { clickTarget.focus(); } catch(e) {}
                    if (clickType === 'right') {
                        clickTarget.dispatchEvent(new MouseEvent('contextmenu', {bubbles:true,cancelable:true,clientX:x,clientY:y,button:2}));
                    } else if (clickType === 'double') {
                        clickTarget.dispatchEvent(new MouseEvent('dblclick', {bubbles:true,cancelable:true,clientX:x,clientY:y}));
                    } else {
                        clickTarget.dispatchEvent(new MouseEvent('mousedown', {bubbles:true,cancelable:true,clientX:x,clientY:y}));
                        clickTarget.dispatchEvent(new MouseEvent('mouseup', {bubbles:true,cancelable:true,clientX:x,clientY:y}));
                        if (typeof clickTarget.click === 'function') {
                            clickTarget.click();
                        } else {
                            HTMLElement.prototype.click.call(clickTarget);
                        }
                    }
                    var rect = clickTarget.getBoundingClientRect();
                    window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                        JSON.stringify({
                            type: 'click-result', requestId: requestId, status: 'completed',
                            tag: clickTarget.tagName.toLowerCase(),
                            text: (clickTarget.textContent||'').trim().substring(0,100),
                            rect: {x:rect.x,y:rect.y,width:rect.width,height:rect.height}
                        })
                    );
                } else {
                    window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                        JSON.stringify({type:'click-result',requestId:requestId,status:'failed',error:'No element at coordinates'})
                    );
                }
            } catch(e) {
                try { window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                    JSON.stringify({type:'click-result',requestId:requestId,status:'failed',error:e.message})
                ); } catch(e2) {}
            }
            return;
        }

        // keyboard command (keyboard input inside a cross-origin iframe)
        if (event.data.type === 'NS_PLACEHOLDERn-keyboard') {
            var requestId = event.data.requestId;
            var text = event.data.text;
            var key = event.data.key;
            var paste = event.data.paste || false;
            var mods = event.data.modifiers || {};
            try {
                var el = document.activeElement || document.body;
                if (el === document.body || el === document.documentElement) {
                    var inputs = document.querySelectorAll('input:not([type=hidden]),textarea,[contenteditable=true]');
                    if (inputs.length > 0) { el = inputs[0]; el.focus(); }
                }
                if (text) {
                    if ((el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {
                        var proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
                        var setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
                        if (paste) {
                            setter.call(el, el.value + text);
                            el.dispatchEvent(new Event('input', {bubbles:true}));
                            el.dispatchEvent(new Event('change', {bubbles:true}));
                        } else {
                            for (var i = 0; i < text.length; i++) {
                                var ch = text[i];
                                el.dispatchEvent(new KeyboardEvent('keydown', {key:ch,bubbles:true}));
                                setter.call(el, el.value + ch);
                                el.dispatchEvent(new Event('input', {bubbles:true}));
                                el.dispatchEvent(new KeyboardEvent('keyup', {key:ch,bubbles:true}));
                            }
                            el.dispatchEvent(new Event('change', {bubbles:true}));
                        }
                    } else if (el.getAttribute && el.getAttribute('contenteditable') === 'true') {
                        el.textContent = (el.textContent || '') + text;
                        el.dispatchEvent(new Event('input', {bubbles:true}));
                    }
                    window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                        JSON.stringify({
                            type:'keyboard-result',requestId:requestId,status:'completed',
                            typed:text,length:text.length,element:el.tagName.toLowerCase(),
                            mode:paste?'paste':'type'
                        })
                    );
                } else if (key) {
                    if (key === 'Backspace' && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {
                        var setter = Object.getOwnPropertyDescriptor(
                            el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value'
                        ).set;
                        setter.call(el, el.value.slice(0, -1));
                        el.dispatchEvent(new Event('input', {bubbles:true}));
                    }
                    el.dispatchEvent(new KeyboardEvent('keydown', {key:key,code:key,bubbles:true,cancelable:true,
                        ctrlKey:mods.ctrl||false,shiftKey:mods.shift||false,altKey:mods.alt||false,metaKey:mods.meta||false}));
                    el.dispatchEvent(new KeyboardEvent('keyup', {key:key,code:key,bubbles:true,cancelable:true,
                        ctrlKey:mods.ctrl||false,shiftKey:mods.shift||false,altKey:mods.alt||false,metaKey:mods.meta||false}));
                    // Cmd+A (select all)
                    if (mods.meta && key === 'a' && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {
                        el.select();
                    }
                    window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                        JSON.stringify({type:'keyboard-result',requestId:requestId,status:'completed',key:key,element:el.tagName.toLowerCase()})
                    );
                }
            } catch(e) {
                try { window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                    JSON.stringify({type:'keyboard-result',requestId:requestId,status:'failed',error:e.message})
                ); } catch(e2) {}
            }
            return;
        }

        // search command
        if (event.data.type === 'NS_PLACEHOLDERn-search') {
            var marker = event.data.marker;
            var requestId = event.data.requestId;

            try {
                var html = document.documentElement ? document.documentElement.outerHTML : '';
                var found = marker && html.indexOf(marker) !== -1;

                window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                    JSON.stringify({
                        type: 'search-result',
                        requestId: requestId,
                        url: window.location.href,
                        found: found,
                        html: found ? html : null
                    })
                );
            } catch(e) {
                try {
                    window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                        JSON.stringify({
                            type: 'search-error',
                            requestId: requestId,
                            url: window.location.href,
                            error: e.message
                        })
                    );
                } catch(e2) {}
            }

            relayToChildFrames(event.data);
            return;
        }

        // describe command (finds the interactive elements in a cross-origin iframe)
        if (event.data.type === 'NS_PLACEHOLDERn-describe') {
            var requestId = event.data.requestId;
            try {
                var els = Array.from(document.querySelectorAll('button,a[href],input,textarea,select')).filter(function(el) {
                    var r = el.getBoundingClientRect();
                    return r.width > 0 && r.height > 0
                        && window.getComputedStyle(el).display !== 'none'
                        && window.getComputedStyle(el).visibility !== 'hidden';
                });
                var elements = els.map(function(el) {
                    var r = el.getBoundingClientRect();
                    var t = (el.innerText || el.value || el.getAttribute('aria-label') || '').trim().substring(0, 80);
                    return { tag: el.tagName.toLowerCase(), text: t, x: r.left, y: r.top, w: r.width, h: r.height };
                });
                var fv = {};
                document.querySelectorAll('input,textarea,select').forEach(function(el) {
                    if (el.value && el.value.length > 0) {
                        fv[el.name || el.id || el.getAttribute('aria-label') || el.tagName] = el.value.substring(0, 200);
                    }
                });
                window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                    JSON.stringify({
                        type: 'describe-result',
                        requestId: requestId,
                        url: window.location.href,
                        elements: elements,
                        formValues: fv,
                        isMainFrame: (window === window.top)
                    })
                );
            } catch(e) {}
            relayToChildFrames(event.data);
            return;
        }

        // HTML collect command
        if (event.data.type === 'NS_PLACEHOLDERn-collect') {
            var requestId = event.data.requestId;

            try {
                var html = document.documentElement ? document.documentElement.outerHTML : '';
                window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
                    JSON.stringify({
                        type: 'collect-result',
                        requestId: requestId,
                        url: window.location.href,
                        origin: window.location.origin,
                        html: html,
                        isMainFrame: (window === window.top)
                    })
                );
            } catch(e) {}

            relayToChildFrames(event.data);
            return;
        }
    });

    try {
        window.webkit.messageHandlers.NS_PLACEHOLDERf.postMessage(
            JSON.stringify({
                type: 'frame-ready',
                url: window.location.href,
                isMainFrame: (window === window.top)
            })
        );
    } catch(e) {}
})();
"#
        .to_string()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The responder runs in EVERY frame via `WKUserScript`, so anything it
        /// names is page-visible: a `__<brand>NativeResponder` global showed up in a
        /// plain `Object.keys(window)` sweep, and the `<brand>-native-*`
        /// `postMessage` types are readable by any page that listens for
        /// `message`. Both shipped in 0.12.0 despite the release being about
        /// exactly this, because the brand-leak test only covered `injection.rs`.
        #[test]
        fn responder_leaks_no_brand_names_into_the_page() {
            let rendered = crate::pagens::render(&generate_native_responder_script());
            for needle in ["dalzzack", "Dalzzack", "DALZZACK", "__TW", "__tw", "zzack"] {
                assert!(
                    !rendered.contains(needle),
                    "leaked {needle} into the per-frame responder script"
                );
            }
        }

        #[test]
        fn responder_has_no_unrendered_placeholder() {
            let rendered = crate::pagens::render(&generate_native_responder_script());
            assert!(!rendered.contains(crate::pagens::NS_TOKEN));
        }

        /// The handler name the page can probe must match the one JS posts to.
        #[test]
        fn handler_name_matches_the_script() {
            let ns = crate::pagens::namespace();
            let rendered = crate::pagens::render(&generate_native_responder_script());
            assert!(rendered.contains(&format!("messageHandlers.{ns}f")));
        }
    }
}

// ============================================================================
// macOS platform implementation — native-bridge based (no SOP disable needed).
// ============================================================================

#[cfg(target_os = "macos")]
pub async fn get_main_frame_html_via_cdp(webview: &tauri::WebviewWindow) -> Result<Option<String>> {
    let frames = macos_native::get_all_frames_native(webview).await?;
    Ok(frames.into_iter().find(|f| f.is_main_frame).map(|f| f.html))
}

#[cfg(target_os = "macos")]
pub async fn enable_runtime_domain(_webview: &tauri::WebviewWindow) -> Result<bool> {
    Ok(true)
}

#[cfg(target_os = "macos")]
pub async fn subscribe_execution_context_events(
    _webview: &tauri::WebviewWindow,
    _context_store: ExecutionContextStore,
) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn evaluate_in_context(
    _webview: &tauri::WebviewWindow,
    _context_id: i64,
    _expression: &str,
) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
pub async fn setup_cdp_for_webview(
    webview: &tauri::WebviewWindow,
    _context_store: ExecutionContextStore,
) -> Result<()> {
    tracing::info!("[macOS] setting up Native Bridge frame handler");

    macos_native::setup_native_frame_handler(webview)?;

    tracing::info!("[macOS] Native Bridge set up (no SOP disable needed)");
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn search_marker_in_all_frames(
    webview: &tauri::WebviewWindow,
    _context_store: &ExecutionContextStore,
    marker: &str,
) -> Result<Option<String>> {
    tracing::debug!("[macOS] starting marker search (Native Bridge): {}", marker);
    macos_native::search_marker_via_native(webview, marker).await
}

#[cfg(target_os = "macos")]
pub async fn get_all_frames_html(
    webview: &tauri::WebviewWindow,
    _context_store: &ExecutionContextStore,
) -> Result<Vec<FrameHtml>> {
    macos_native::get_all_frames_native(webview).await
}

#[cfg(target_os = "macos")]
pub async fn describe_elements_in_all_frames(
    webview: &tauri::WebviewWindow,
    _context_store: &ExecutionContextStore,
) -> Result<Vec<FrameDescribeResult>> {
    macos_native::describe_elements_in_all_frames_native(webview).await
}

/// Click an element inside a cross-origin iframe.
#[cfg(target_os = "macos")]
pub async fn click_in_cross_origin_iframe(
    local_x: f64,
    local_y: f64,
    click_type: &str,
    iframe_index: usize,
    _context_store: &ExecutionContextStore,
) -> Result<serde_json::Value> {
    macos_native::click_in_iframe_native(local_x, local_y, click_type, iframe_index).await
}

/// Send keyboard input into a cross-origin iframe.
#[cfg(target_os = "macos")]
pub async fn keyboard_in_cross_origin_iframe(
    text: Option<&str>,
    key: Option<&str>,
    paste: bool,
    modifiers: &serde_json::Value,
    iframe_index: usize,
    _context_store: &ExecutionContextStore,
) -> Result<serde_json::Value> {
    macos_native::keyboard_in_iframe_native(text, key, paste, modifiers, iframe_index).await
}

/// Windows: click an element inside a cross-origin iframe (via CDP `evaluate_in_context`).
#[cfg(target_os = "windows")]
pub async fn click_in_cross_origin_iframe(
    local_x: f64,
    local_y: f64,
    click_type: &str,
    _iframe_index: usize,
    context_store: &ExecutionContextStore,
) -> Result<serde_json::Value> {
    let contexts = context_store.read().await.clone();
    // Use the first iframe context that is not the main one
    for context in &contexts {
        if context.origin.starts_with("about:") || context.origin.is_empty() {
            continue;
        }
        let is_default = context
            .aux_data
            .as_ref()
            .and_then(|a| a.is_default)
            .unwrap_or(false);
        let frame_id = context
            .aux_data
            .as_ref()
            .and_then(|a| a.frame_id.as_deref())
            .unwrap_or("");
        if is_default && frame_id.is_empty() {
            continue;
        }

        let window = crate::get_main_window().await;
        if let Some(ref webview) = window {
            let click_event = match click_type {
                "right" => format!("el.dispatchEvent(new MouseEvent('contextmenu',{{bubbles:true,cancelable:true,clientX:{},clientY:{},button:2}}));", local_x, local_y),
                "double" => format!("el.dispatchEvent(new MouseEvent('dblclick',{{bubbles:true,cancelable:true,clientX:{},clientY:{}}}));", local_x, local_y),
                _ => format!("el.focus();el.dispatchEvent(new MouseEvent('mousedown',{{bubbles:true,cancelable:true,clientX:{x},clientY:{y}}}));el.dispatchEvent(new MouseEvent('mouseup',{{bubbles:true,cancelable:true,clientX:{x},clientY:{y}}}));el.click();", x=local_x, y=local_y),
            };
            let script = format!(
                "var el=document.elementFromPoint({},{});if(!el)return JSON.stringify({{status:'failed',error:'No element'}});var r=el.getBoundingClientRect();{}return JSON.stringify({{status:'completed',tag:el.tagName.toLowerCase(),text:(el.textContent||'').trim().substring(0,100),rect:{{x:r.x,y:r.y,width:r.width,height:r.height}}}});",
                local_x, local_y, click_event,
            );
            if let Ok(Some(result_str)) = evaluate_in_context(webview, context.id, &script).await {
                if let Ok(result) = serde_json::from_str::<serde_json::Value>(&result_str) {
                    return Ok(result);
                }
            }
        }
    }
    Ok(serde_json::json!({"status": "failed", "error": "no iframe context found"}))
}

/// Windows: send keyboard input into a cross-origin iframe (via CDP).
#[cfg(target_os = "windows")]
pub async fn keyboard_in_cross_origin_iframe(
    text: Option<&str>,
    key: Option<&str>,
    _paste: bool,
    _modifiers: &serde_json::Value,
    _iframe_index: usize,
    context_store: &ExecutionContextStore,
) -> Result<serde_json::Value> {
    let contexts = context_store.read().await.clone();
    for context in &contexts {
        if context.origin.starts_with("about:") || context.origin.is_empty() {
            continue;
        }
        let is_default = context
            .aux_data
            .as_ref()
            .and_then(|a| a.is_default)
            .unwrap_or(false);
        let frame_id = context
            .aux_data
            .as_ref()
            .and_then(|a| a.frame_id.as_deref())
            .unwrap_or("");
        if is_default && frame_id.is_empty() {
            continue;
        }

        let window = crate::get_main_window().await;
        if let Some(ref webview) = window {
            let text_json = text
                .map(|t| serde_json::to_string(t).unwrap_or_default())
                .unwrap_or("null".to_string());
            let key_json = key
                .map(|k| serde_json::to_string(k).unwrap_or_default())
                .unwrap_or("null".to_string());
            let script = format!(
                r#"var el=document.activeElement||document.body;if(el===document.body){{var ins=document.querySelectorAll('input:not([type=hidden]),textarea');if(ins.length){{el=ins[0];el.focus();}}}}if({text}!==null){{var t={text};if(el.tagName==='INPUT'||el.tagName==='TEXTAREA'){{var s=Object.getOwnPropertyDescriptor(el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype,'value').set;s.call(el,el.value+t);el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));}}return JSON.stringify({{status:'completed',typed:t,length:t.length,element:el.tagName.toLowerCase(),mode:'paste'}});}}else if({key}!==null){{var k={key};el.dispatchEvent(new KeyboardEvent('keydown',{{key:k,code:k,bubbles:true,cancelable:true}}));el.dispatchEvent(new KeyboardEvent('keyup',{{key:k,code:k,bubbles:true,cancelable:true}}));return JSON.stringify({{status:'completed',key:k,element:el.tagName.toLowerCase()}});}}"#,
                text = text_json,
                key = key_json,
            );
            if let Ok(Some(result_str)) = evaluate_in_context(webview, context.id, &script).await {
                if let Ok(result) = serde_json::from_str::<serde_json::Value>(&result_str) {
                    return Ok(result);
                }
            }
        }
    }
    Ok(serde_json::json!({"status": "failed", "error": "no iframe context found"}))
}

/// Linux stub
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn click_in_cross_origin_iframe(
    _local_x: f64,
    _local_y: f64,
    _click_type: &str,
    _iframe_index: usize,
    _context_store: &ExecutionContextStore,
) -> Result<serde_json::Value> {
    Ok(serde_json::json!({"status": "failed", "error": "not supported on this platform"}))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn keyboard_in_cross_origin_iframe(
    _text: Option<&str>,
    _key: Option<&str>,
    _paste: bool,
    _modifiers: &serde_json::Value,
    _iframe_index: usize,
    _context_store: &ExecutionContextStore,
) -> Result<serde_json::Value> {
    Ok(serde_json::json!({"status": "failed", "error": "not supported on this platform"}))
}

// ============================================================================
// Linux and other-platform stubs.
// ============================================================================

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn get_main_frame_html_via_cdp(
    _webview: &tauri::WebviewWindow,
) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn enable_runtime_domain(_webview: &tauri::WebviewWindow) -> Result<bool> {
    Ok(false)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn subscribe_execution_context_events(
    _webview: &tauri::WebviewWindow,
    _context_store: ExecutionContextStore,
) -> Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn evaluate_in_context(
    _webview: &tauri::WebviewWindow,
    _context_id: i64,
    _expression: &str,
) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn setup_cdp_for_webview(
    _webview: &tauri::WebviewWindow,
    _context_store: ExecutionContextStore,
) -> Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn search_marker_in_all_frames(
    _webview: &tauri::WebviewWindow,
    _context_store: &ExecutionContextStore,
    _marker: &str,
) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn get_all_frames_html(
    _webview: &tauri::WebviewWindow,
    _context_store: &ExecutionContextStore,
) -> Result<Vec<FrameHtml>> {
    Ok(Vec::new())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn describe_elements_in_all_frames(
    _webview: &tauri::WebviewWindow,
    _context_store: &ExecutionContextStore,
) -> Result<Vec<FrameDescribeResult>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_context_deserialize() {
        let json = r#"{
            "id": 42,
            "origin": "https://example.com",
            "name": "main",
            "auxData": {
                "frameId": "F1",
                "isDefault": true,
                "type": "default"
            }
        }"#;

        let ctx: ExecutionContext = serde_json::from_str(json).unwrap();
        assert_eq!(ctx.id, 42);
        assert_eq!(ctx.origin, "https://example.com");
        assert_eq!(ctx.name, "main");

        let aux = ctx.aux_data.unwrap();
        assert_eq!(aux.frame_id.unwrap(), "F1");
        assert!(aux.is_default.unwrap());
        assert_eq!(aux.context_type.unwrap(), "default");
    }

    #[test]
    fn test_execution_context_minimal() {
        let json = r#"{"id": 1}"#;
        let ctx: ExecutionContext = serde_json::from_str(json).unwrap();
        assert_eq!(ctx.id, 1);
        assert_eq!(ctx.origin, ""); // default
        assert_eq!(ctx.name, ""); // default
        assert!(ctx.aux_data.is_none());
    }

    #[test]
    fn test_execution_context_info_deserialize() {
        let json = r#"{
            "context": {
                "id": 5,
                "origin": "https://sub.example.com",
                "name": "iframe-ctx",
                "auxData": null
            }
        }"#;

        let info: ExecutionContextInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.context.id, 5);
        assert_eq!(info.context.origin, "https://sub.example.com");
    }

    #[test]
    fn test_frame_html_struct() {
        let frame = FrameHtml {
            context_id: 10,
            origin: "https://test.com".to_string(),
            html: "<div>frame content</div>".to_string(),
            is_main_frame: false,
        };

        assert_eq!(frame.context_id, 10);
        assert!(!frame.is_main_frame);
    }
}
