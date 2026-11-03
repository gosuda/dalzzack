//! Network fault injection — intercept rule state + cross-navigation persistence.
//!
//! Rules live in daemon memory (cleared automatically on `stop`) and are pushed
//! into the WebView two ways on every change:
//!
//! 1. A **document-start script** registered at runtime (Windows:
//!    `AddScriptToExecuteOnDocumentCreated`, macOS: `WKUserScript` at
//!    `AtDocumentStart`), so rules survive navigations and are in place before
//!    the app bundle captures its `fetch` reference.
//! 2. An immediate `exec-js` into the already-loaded page, so rules apply
//!    without requiring a navigation.
//!
//! The JS engine that consumes the rules lives in [`crate::injection`]'s
//! network interceptor (`window.__TW_INTERCEPT_RULES`).

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterceptRule {
    /// Substring match, or glob against the full URL when it contains `*`.
    pub pattern: String,
    /// One of: hang | delay | fail | mock.
    pub action: String,
    /// delay: wait duration. mock: response latency. hang: optional auto-error timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// mock: HTTP status (default 200).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// mock: response body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// mock: Content-Type (default application/json).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Restrict the rule to one HTTP method (default: all).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

pub const VALID_ACTIONS: &[&str] = &["hang", "delay", "fail", "mock"];

lazy_static::lazy_static! {
    /// Active fault-injection rules. Daemon memory only — gone on stop/kill.
    pub static ref INTERCEPT_RULES: Arc<RwLock<Vec<InterceptRule>>> =
        Arc::new(RwLock::new(Vec::new()));
    /// Windows: id returned by AddScriptToExecuteOnDocumentCreated, kept so the
    /// previous sync script can be removed when rules change.
    static ref DOCSTART_SCRIPT_ID: Arc<RwLock<Option<String>>> =
        Arc::new(RwLock::new(None));
}

/// Monotonic version so stale document-start scripts (macOS can't remove user
/// scripts individually) never overwrite a newer rule set.
static INTERCEPT_VERSION: AtomicU64 = AtomicU64::new(0);

pub fn current_version() -> u64 {
    INTERCEPT_VERSION.load(Ordering::SeqCst)
}

/// Build the JS that installs `rules` into a page. Version-guarded so an older
/// script running after a newer one is a no-op.
pub fn build_sync_script(rules: &[InterceptRule], version: u64) -> String {
    let rules_json = serde_json::to_string(rules).unwrap_or_else(|_| "[]".to_string());
    let ns = crate::pagens::namespace();
    format!(
        r#"(function() {{
    var v = {version};
    var NS = window.{ns} = window.{ns} || {{}};
    if ((NS.v || 0) >= v) return;
    NS.v = v;
    NS.rules = {rules_json};
}})();"#
    )
}

/// Push the current rule set into the WebView. Returns sync status details;
/// only hard-fails when the WebView itself is unavailable.
pub async fn sync_rules_to_webview() -> Result<serde_json::Value, String> {
    // Real Chrome has no in-page hook to feed; its rules are enforced by the
    // browser itself through CDP's Fetch domain.
    if crate::engine::is_chrome() {
        let version = INTERCEPT_VERSION.fetch_add(1, Ordering::SeqCst) + 1;
        return crate::chrome::intercept::sync(version).await;
    }
    let rules = INTERCEPT_RULES.read().await.clone();
    let version = INTERCEPT_VERSION.fetch_add(1, Ordering::SeqCst) + 1;
    let script = build_sync_script(&rules, version);

    let window = crate::state::get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available. Is the daemon running?")?;

    // 1. Persist across future navigations via a document-start script.
    let persist_result = register_document_start_script(&window, &script).await;
    if let Err(e) = &persist_result {
        tracing::warn!("[intercept] document-start registration failed: {}", e);
    }

    // 2. Apply to the page that is loaded right now (best effort — the landing
    //    page or a wedged renderer may reject this without affecting rule state).
    let apply_script = format!(
        "{script}\nreturn (window.{ns} || {{}}).v;",
        ns = crate::pagens::namespace()
    );
    let applied_now = crate::platform::webview_execute_js(&apply_script, Some(3000))
        .await
        .is_ok();

    Ok(serde_json::json!({
        "version": version,
        "rule_count": rules.len(),
        "persists_across_navigation": persist_result.is_ok(),
        "persist_error": persist_result.err(),
        "applied_to_current_page": applied_now,
    }))
}

/// Windows: (re-)register the sync script to run at document creation in every
/// future navigation. Replaces the previously registered script, if any.
#[cfg(target_os = "windows")]
async fn register_document_start_script(
    window: &tauri::WebviewWindow,
    script: &str,
) -> Result<(), String> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler,
        ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler_Impl,
    };
    use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

    let old_id = DOCSTART_SCRIPT_ID.read().await.clone();
    let (tx, rx) = mpsc::channel::<Result<String, String>>();

    #[implement(ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler)]
    struct AddScriptHandler {
        tx: mpsc::Sender<Result<String, String>>,
    }

    impl ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler_Impl
        for AddScriptHandler_Impl
    {
        fn Invoke(&self, errorcode: HRESULT, id: &PCWSTR) -> windows_core::Result<()> {
            if errorcode.is_err() {
                let _ = self
                    .tx
                    .send(Err(format!("AddScript callback error: {:?}", errorcode)));
            } else {
                let script_id = unsafe { id.to_string() }.unwrap_or_default();
                let _ = self.tx.send(Ok(script_id));
            }
            Ok(())
        }
    }

    let script_owned = script.to_string();
    let tx_clone = tx.clone();
    window
        .with_webview(move |wv| unsafe {
            let controller = wv.controller();
            let core: ICoreWebView2 = match controller.CoreWebView2() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx_clone.send(Err(format!("CoreWebView2 error: {:?}", e)));
                    return;
                }
            };

            if let Some(oid) = &old_id {
                let _ = core.RemoveScriptToExecuteOnDocumentCreated(&HSTRING::from(oid.as_str()));
            }

            let handler: ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler =
                AddScriptHandler {
                    tx: tx_clone.clone(),
                }
                .into();

            if let Err(e) = core.AddScriptToExecuteOnDocumentCreated(
                &HSTRING::from(script_owned.as_str()),
                &handler,
            ) {
                let _ = tx_clone.send(Err(format!(
                    "AddScriptToExecuteOnDocumentCreated error: {:?}",
                    e
                )));
            }
        })
        .map_err(|e| format!("with_webview error: {:?}", e))?;

    let script_id =
        tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(5)))
            .await
            .map_err(|e| format!("join error: {}", e))?
            .map_err(|e| format!("AddScript callback timeout: {}", e))??;

    *DOCSTART_SCRIPT_ID.write().await = Some(script_id);
    Ok(())
}

/// macOS: add the sync script as a WKUserScript at document start. Individual
/// user scripts can't be removed, so superseded scripts stay registered but are
/// neutralized by the version guard inside the script itself.
#[cfg(target_os = "macos")]
async fn register_document_start_script(
    window: &tauri::WebviewWindow,
    script: &str,
) -> Result<(), String> {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_foundation::NSString;
    use objc2_web_kit::{WKUserContentController, WKUserScript, WKUserScriptInjectionTime};

    let script_owned = script.to_string();
    window
        .with_webview(move |wv| unsafe {
            let mtm =
                MainThreadMarker::new().expect("with_webview callback must run on main thread");
            let controller: &WKUserContentController = &*wv.controller().cast();
            let user_script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(&script_owned),
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            controller.addUserScript(&user_script);
        })
        .map_err(|e| format!("with_webview error: {:?}", e))?;
    Ok(())
}

/// Other platforms: no runtime document-start hook wired up. Rules still apply
/// to the current page via exec-js but do not survive navigation.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn register_document_start_script(
    _window: &tauri::WebviewWindow,
    _script: &str,
) -> Result<(), String> {
    Err("document-start persistence is not supported on this platform; rules apply to the current page only".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, action: &str) -> InterceptRule {
        InterceptRule {
            pattern: pattern.to_string(),
            action: action.to_string(),
            ms: None,
            status: None,
            body: None,
            content_type: None,
            method: None,
        }
    }

    #[test]
    fn test_sync_script_contains_rules_and_version() {
        let script = build_sync_script(&[rule("*/portfolio*", "hang")], 7);
        assert!(script.contains("var v = 7;"));
        assert!(script.contains("*/portfolio*"));
        assert!(script.contains("\"action\":\"hang\""));
        assert!(script.contains("NS.rules = "));
        // The rule table must land under the randomized namespace, never under a
        // name a page could grep for.
        assert!(script.contains(crate::pagens::namespace()));
        assert!(!script.contains("__TW_INTERCEPT_RULES"));
    }

    #[test]
    fn test_sync_script_empty_rules() {
        let script = build_sync_script(&[], 1);
        assert!(script.contains("NS.rules = []"));
    }

    #[test]
    fn test_rule_serialization_skips_none_fields() {
        let json = serde_json::to_string(&rule("/api", "fail")).unwrap();
        assert!(!json.contains("\"ms\""));
        assert!(!json.contains("\"status\""));
        assert!(json.contains("\"pattern\":\"/api\""));
    }

    #[test]
    fn test_rule_serialization_includes_set_fields() {
        let mut r = rule("*/markets*", "delay");
        r.ms = Some(8000);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"ms\":8000"));
    }
}
