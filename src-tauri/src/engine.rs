//! Which browser engine this daemon drives.
//!
//! `webview` is the original backend: a Tauri window hosting the platform
//! webview (WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux).
//! `chrome` launches a real Google Chrome with a throw-away profile and drives
//! it over the DevTools protocol — because an embedded WebView2 is *not* what a
//! site sees from Chrome: it differs in the TLS ClientHello, in its client
//! hints, and in how bot defences score it. See [`crate::chrome`].

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    #[default]
    WebView,
    Chrome,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::WebView => "webview",
            Engine::Chrome => "chrome",
        }
    }

    pub fn parse(s: &str) -> Option<Engine> {
        match s.trim().to_ascii_lowercase().as_str() {
            "webview" | "webview2" | "wkwebview" | "default" => Some(Engine::WebView),
            "chrome" => Some(Engine::Chrome),
            _ => None,
        }
    }
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Set once at daemon start.
pub fn set(e: Engine) {
    let _ = ENGINE.set(e);
}

pub fn current() -> Engine {
    ENGINE.get().copied().unwrap_or(Engine::WebView)
}

pub fn is_chrome() -> bool {
    current() == Engine::Chrome
}

/// Call a CDP method on the page session of whichever engine this daemon
/// drives. `Err` carries `UNSUPPORTED_PLATFORM` where there is no CDP at all
/// (WKWebView / WebKitGTK), so callers can map it to an honest failure.
pub async fn page_cdp(
    method: &str,
    params: &serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, String> {
    if is_chrome() {
        return crate::chrome::page_call(method, params.clone(), timeout).await;
    }
    #[cfg(target_os = "windows")]
    {
        let window = crate::get_main_window()
            .await
            .ok_or_else(|| "WEBVIEW_NOT_FOUND: WebView not ready".to_string())?;
        crate::cdp::call(&window, method, &params.to_string(), timeout).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (method, params, timeout);
        Err(format!(
            "UNSUPPORTED_PLATFORM: `{method}` needs CDP — WebView2 on Windows, or start the \
             daemon with `--engine chrome`"
        ))
    }
}

/// Read a field out of a CDP result regardless of transport. WebView2 has been
/// seen to wrap results in an extra `result` envelope; a raw WebSocket reply
/// never does. `path` is a JSON pointer without the leading slash.
pub fn cdp_result<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    v.pointer(&format!("/{path}"))
        .or_else(|| v.pointer(&format!("/result/{path}")))
}

/// Is there a CDP page session on this daemon at all?
pub fn has_cdp() -> bool {
    is_chrome() || cfg!(target_os = "windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for e in [Engine::WebView, Engine::Chrome] {
            assert_eq!(Engine::parse(e.as_str()), Some(e));
        }
        assert_eq!(Engine::parse("WebView2"), Some(Engine::WebView));
        assert_eq!(Engine::parse("firefox"), None);
    }
}
