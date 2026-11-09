//! Platform-specific WebView implementations.

pub mod js_exec;
pub mod mouse;
/// `keyboard --native`: key presses WebKit treats as real (macOS).
pub mod native_keys;
pub mod navigation;
pub mod security;
/// macOS: the WKWebView cookie jar for `cookies`.
#[cfg(target_os = "macos")]
pub mod wk_cookies;
/// macOS: failed loads and `--trust-cert` challenges from the navigation delegate.
#[cfg(target_os = "macos")]
pub mod wk_delegate;

pub use js_exec::*;
pub use mouse::*;
pub use native_keys::*;
pub use navigation::*;
pub use security::*;
