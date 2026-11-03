//! Platform-specific WebView navigation implementations.

use crate::state::get_main_window;
use crate::types::NavigateResult;
use crate::webview::operations::clear_cdp_context_store;

/// Navigate to a URL.
/// timeout_ms: page load wait time in milliseconds (default 5000ms)
pub async fn webview_navigate(
    url: &str,
    timeout_ms: Option<u32>,
) -> Result<NavigateResult, String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available. Is the daemon running?")?;

    let wait_ms = timeout_ms.unwrap_or(5000) as u64;
    tracing::info!("Navigating to: {} (wait: {}ms)", url, wait_ms);

    let parsed_url: url::Url = url
        .parse()
        .map_err(|e| format!("Invalid URL '{}': {}", url, e))?;

    tracing::debug!(
        "Parsed URL: {} (host: {:?})",
        parsed_url.as_str(),
        parsed_url.host_str()
    );

    // Clear CDP Context Store (clean up previous page's ExecutionContexts)
    clear_cdp_context_store().await;

    // macOS: WKWebView loadRequest
    #[cfg(target_os = "macos")]
    {
        webview_navigate_macos(&window, parsed_url.as_str())?;
    }

    // Windows: WebView2 Navigate
    #[cfg(target_os = "windows")]
    {
        webview_navigate_windows(&window, parsed_url.as_str())?;
    }

    // Linux: webkit2gtk native navigation
    #[cfg(target_os = "linux")]
    {
        webview_navigate_linux(&window, parsed_url.as_str())?;
    }

    // Other platforms: eval fallback
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let nav_script = format!(
            "window.location.href = '{}';",
            parsed_url.as_str().replace('\'', "\\'")
        );
        tracing::debug!("Navigating via eval: {}", nav_script);
        window
            .eval(&nav_script)
            .map_err(|e| format!("NAVIGATION_ERROR: {}", e))?;
    }

    // Wait for page load
    tokio::time::sleep(tokio::time::Duration::from_millis(wait_ms)).await;

    // Windows: CDP re-setup after navigation
    #[cfg(target_os = "windows")]
    {
        tracing::debug!(
            "[CDP] Navigation complete, ExecutionContext events will be collected automatically"
        );
    }

    // Actual URL and title, in one round trip.
    let (current_url, title) = match crate::platform::webview_execute_js(
        "return JSON.stringify({u: window.location.href, t: document.title})",
        Some(3000),
    )
    .await
    {
        Ok(val) => {
            let parsed: Option<serde_json::Value> = match &val {
                serde_json::Value::String(s) => serde_json::from_str(s).ok(),
                v if v.is_object() => Some(v.clone()),
                _ => None,
            };
            match parsed {
                Some(p) => (
                    p["u"].as_str().unwrap_or(parsed_url.as_str()).to_string(),
                    p["t"].as_str().map(String::from),
                ),
                None => (parsed_url.to_string(), None),
            }
        }
        Err(_) => (parsed_url.to_string(), None),
    };

    tracing::info!("Navigation complete: url={}", current_url);

    Ok(NavigateResult {
        url: current_url,
        title,
    })
}

/// macOS: WKWebView native navigation via loadRequest
#[cfg(target_os = "macos")]
fn webview_navigate_macos(window: &tauri::WebviewWindow, url: &str) -> Result<(), String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let url_owned = url.to_string();

    window
        .with_webview(move |wv| unsafe {
            let webview_ptr: *mut AnyObject = wv.inner().cast();

            let nsurl_class_name = c"NSURL";
            let nsurl_class: *mut AnyObject = msg_send![
                objc2::runtime::AnyClass::get(nsurl_class_name).unwrap(),
                alloc
            ];

            let url_cstr = std::ffi::CString::new(url_owned.as_str()).unwrap_or_default();

            let nsstring_class_name = c"NSString";
            let nsstring_class: *mut AnyObject = msg_send![
                objc2::runtime::AnyClass::get(nsstring_class_name).unwrap(),
                alloc
            ];
            let ns_url_string: *mut AnyObject =
                msg_send![nsstring_class, initWithUTF8String: url_cstr.as_ptr()];

            let nsurl: *mut AnyObject = msg_send![nsurl_class, initWithString: ns_url_string];

            if nsurl.is_null() {
                tracing::error!("[macOS] Failed to create NSURL for: {}", url_owned);
                return;
            }

            let nsrequest_class_name = c"NSURLRequest";
            let request: *mut AnyObject = msg_send![
                objc2::runtime::AnyClass::get(nsrequest_class_name).unwrap(),
                requestWithURL: nsurl
            ];

            let _: *mut AnyObject = msg_send![webview_ptr, loadRequest: request];

            tracing::info!("[macOS] Native navigation to: {}", url_owned);
        })
        .map_err(|e| format!("with_webview error: {:?}", e))
}

/// Windows: WebView2 native navigation via Navigate()
#[cfg(target_os = "windows")]
fn webview_navigate_windows(window: &tauri::WebviewWindow, url: &str) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
    use windows_core::HSTRING;

    let url_owned = url.to_string();

    window
        .with_webview(move |wv| unsafe {
            let controller = wv.controller();
            match controller.CoreWebView2() {
                Ok(core) => {
                    let core: ICoreWebView2 = core;
                    let url_hstring = HSTRING::from(url_owned.as_str());

                    match core.Navigate(&url_hstring) {
                        Ok(_) => {
                            tracing::info!("[Windows] Native navigation to: {}", url_owned);
                        }
                        Err(e) => {
                            tracing::error!("[Windows] Navigate failed for {}: {:?}", url_owned, e);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("[Windows] CoreWebView2 error: {:?}", e);
                }
            }
        })
        .map_err(|e| format!("with_webview error: {:?}", e))
}

/// Linux: webkit2gtk native navigation via load_uri()
#[cfg(target_os = "linux")]
fn webview_navigate_linux(window: &tauri::WebviewWindow, url: &str) -> Result<(), String> {
    use webkit2gtk::WebViewExt;

    let url_owned = url.to_string();

    window
        .with_webview(move |wv| {
            let webview: webkit2gtk::WebView = wv.inner();
            webview.load_uri(&url_owned);
            tracing::info!("[Linux] Native navigation to: {}", url_owned);
        })
        .map_err(|e| format!("with_webview error: {:?}", e))
}
