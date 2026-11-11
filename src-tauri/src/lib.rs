//! dalzzack - Browser Automation Library
//!
//! Provides browser automation via Tauri WebView with platform-specific native APIs.
//! CLI daemon controls the WebView for navigation, JS execution, HTML capture, etc.

/// Which commands a window is running, for its toolbar.
pub mod activity;
pub mod app;
/// Shared CDP transport. Windows-only: WebView2 is the only backend that
/// exposes a DevTools Protocol channel.
#[cfg(target_os = "windows")]
pub mod cdp;
/// Real Google Chrome over the DevTools protocol (`start --engine chrome`).
pub mod chrome;
pub mod cli;
/// Page-unobservable console capture and the daemon-side buffer behind
/// `console-logs` / `console-dump`.
pub mod console;
/// `cookies list|clear` through CDP — HttpOnly and parent-domain cookies too.
pub mod cookies;
pub mod daemon;
pub mod diagnostics;
/// Which browser engine this daemon drives (`webview` or `chrome`).
pub mod engine;
/// The dalzzack block in an agent's instruction file.
pub mod guide_file;
/// Document-start script injection into a chosen JS world.
pub mod inject;
pub mod injection;
pub mod intercept;
pub mod logging;
/// Main-frame loads that failed, on every engine (`navigate`, `wait-navigation`).
pub mod main_frame;
/// Always-on request index and navigation timeline (`nav-log`,
/// `failed-requests`, `list`'s current URL).
pub mod netlog;
/// Per-daemon randomized page-scope namespace for injected state.
pub mod pagens;
pub mod platform;
/// Which browser profile this daemon uses and who else shares it.
pub mod profile;
pub mod recording;
/// `reset` — first-visit state without giving up the instance id.
pub mod reset;
/// Response headers captured from CDP — the only view of the navigation
/// document's headers, where CSP/CORP/cache policy is decided.
pub mod response_headers;
pub mod site_data;
pub mod state;
pub mod sw_console;
pub mod tools;
/// `start --trust-cert`: extra trust anchors for one WKWebView instance.
pub mod trust_cert;
pub mod types;
/// User-Agent resolution and engine-version detection.
pub mod ua;
/// `uninstall` — undo the install and the changes to agent files.
pub mod uninstall;
/// `upgrade` — replace the binary with the latest release.
pub mod upgrade;
pub mod webview;

// Re-export public API for backward compatibility
pub use app::{run, run_with_instance_id};
pub use logging::{get_log_path, init_file_logging, setup_panic_hook};
pub use platform::{disable_web_security, webview_execute_js, webview_navigate};
pub use state::{
    get_app_handle, get_main_window, get_sites_path, get_window_title, get_window_title_sync,
    wait_for_main_window, APP_HANDLE, CDP_CONTEXT_STORE, CURRENT_SITE, EXECUTION_COUNTER,
    INSTANCE_ID, WINDOW_TITLE_CACHE,
};
pub use types::*;
pub use webview::operations::{
    clear_cdp_context_store, is_webview_ready, setup_cdp, webview_auto_dismiss_modals,
    webview_click_in_cross_origin_iframe, webview_describe_cross_origin_frames, webview_eval,
    webview_fill_form, webview_get_frame_html, webview_get_frame_html_via_cdp,
    webview_get_frame_list, webview_get_html, webview_get_main_frame_html_cdp, webview_get_title,
    webview_get_url, webview_keyboard_in_cross_origin_iframe, webview_wait_for_selector,
};
