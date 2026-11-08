//! Platform-specific WebView implementations.

pub mod js_exec;
pub mod mouse;
pub mod navigation;
pub mod security;
/// macOS: failed loads and `--trust-cert` challenges from the navigation delegate.
#[cfg(target_os = "macos")]
pub mod wk_delegate;

pub use js_exec::*;
pub use mouse::*;
pub use navigation::*;
pub use security::*;
