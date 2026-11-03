//! Platform-specific web security configuration.

/// Disable Same-Origin Policy on macOS via WKWebView Private API.
/// Required for cross-origin iframe access. Not App Store compatible.
#[cfg(target_os = "macos")]
pub fn disable_web_security(webview: &tauri::WebviewWindow) -> Result<(), String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    #[link(name = "WebKit", kind = "framework")]
    extern "C" {
        fn WKPreferencesSetWebSecurityEnabled(preferences: *mut AnyObject, enabled: bool);
    }

    webview
        .with_webview(|wv| unsafe {
            let webview_ptr: *mut AnyObject = wv.inner().cast();
            let config: *mut AnyObject = msg_send![webview_ptr, configuration];
            let prefs: *mut AnyObject = msg_send![config, preferences];
            WKPreferencesSetWebSecurityEnabled(prefs, false);
            tracing::info!("[macOS] Web Security disabled (Same-Origin Policy bypassed)");
        })
        .map_err(|e| format!("with_webview error: {:?}", e))
}

/// Disable web security on Linux via WebKitSettings.
/// Enables cross-origin iframe access by allowing universal access from file URLs
/// and disabling XSS auditing.
#[cfg(target_os = "linux")]
pub fn disable_web_security(webview: &tauri::WebviewWindow) -> Result<(), String> {
    use webkit2gtk::{SettingsExt, WebViewExt};

    webview
        .with_webview(|wv| {
            let webview: webkit2gtk::WebView = wv.inner();
            if let Some(settings) = webview.settings() {
                settings.set_allow_universal_access_from_file_urls(true);
                settings.set_allow_file_access_from_file_urls(true);
                tracing::info!("[Linux] Web Security relaxed (universal file access enabled)");
            } else {
                tracing::warn!("[Linux] Could not access WebKit settings");
            }
        })
        .map_err(|e| format!("with_webview error: {:?}", e))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn disable_web_security(_webview: &tauri::WebviewWindow) -> Result<(), String> {
    Ok(())
}
