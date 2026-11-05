//! Global application state and helper accessors.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Manager, WebviewWindow};
use tokio::sync::RwLock;

use crate::webview::ExecutionContextStore;

lazy_static::lazy_static! {
    pub static ref APP_HANDLE: Arc<RwLock<Option<AppHandle>>> = Arc::new(RwLock::new(None));
    pub static ref CURRENT_SITE: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    /// Instance ID for parallel execution (--instance-id option)
    pub static ref INSTANCE_ID: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    /// CDP ExecutionContext store (for cross-origin iframe access)
    pub static ref CDP_CONTEXT_STORE: ExecutionContextStore = Arc::new(RwLock::new(Vec::new()));
    /// Top frame's CDP id, cached for the life of one document.
    ///
    /// Cleared together with `CDP_CONTEXT_STORE` on navigation, so it can never
    /// name a frame from a replaced document.
    pub static ref CDP_TOP_FRAME_ID: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    /// Window title cache (for spawn_blocking access)
    pub static ref WINDOW_TITLE_CACHE: std::sync::Mutex<String> = std::sync::Mutex::new("dalzzack".to_string());
}

/// Execution ID counter (ensures event name uniqueness)
pub static EXECUTION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Is the page's Content Security Policy being enforced?
///
/// On Windows dalzzack disables CSP enforcement by default
/// (`Page.setBypassCSP`) because cross-origin iframe inspection depends on it.
/// That has a consequence which used to be invisible and cost a whole
/// investigation: **a page whose CSP is bypassed produces no violations at
/// all**, so `console-logs --source csp` would sit at zero forever and read as
/// "this page has no policy problems".
///
/// Set once at daemon start from `--enforce-csp`; reported everywhere CSP is
/// observable so that "no violations" can be told apart from "no enforcement".
static ENFORCE_CSP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub fn set_enforce_csp(enforce: bool) {
    let _ = ENFORCE_CSP.set(enforce);
}

/// `true` when CSP enforcement is off — i.e. violations cannot occur.
///
/// **Windows only.** `Page.setBypassCSP` is a CDP call and CDP is a WebView2
/// feature; the macOS and Linux backends never disable CSP, so reporting a
/// bypass there would be exactly the kind of lie this flag exists to prevent.
/// `--enforce-csp` is consequently a no-op off Windows: the policy is already
/// live.
pub fn csp_bypassed() -> bool {
    // The Chrome engine never calls Page.setBypassCSP: it exists to show what
    // Chrome does, and Chrome enforces the policy.
    if crate::engine::is_chrome() {
        return false;
    }
    if cfg!(target_os = "windows") {
        !ENFORCE_CSP.get().copied().unwrap_or(false)
    } else {
        false
    }
}

/// Get the next execution ID
pub fn next_execution_id() -> u64 {
    EXECUTION_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Return window title (async, per-instance unique title)
pub async fn get_window_title() -> String {
    let id = INSTANCE_ID.read().await;
    match id.as_ref() {
        Some(instance_id) => format!("dalzzack-{}", instance_id),
        None => "dalzzack".to_string(),
    }
}

/// Window title (sync version for spawn_blocking)
pub fn get_window_title_sync() -> String {
    WINDOW_TITLE_CACHE.lock().unwrap().clone()
}

/// Get the AppHandle
pub async fn get_app_handle() -> Option<AppHandle> {
    let handle = APP_HANDLE.read().await;
    handle.clone()
}

/// Get the main WebView window
pub async fn get_main_window() -> Option<WebviewWindow> {
    let handle = get_app_handle().await?;
    handle.get_webview_window("main")
}

/// Poll for the main WebView window until it exists or `budget` elapses.
///
/// Every *tool* fetches the window at call time, long after startup, so it
/// never notices that window creation is asynchronous. Daemon startup wiring
/// (CDP setup, recording subscription, service worker auto-attach) runs once,
/// immediately, and on a slow start would otherwise find no window and give up
/// silently — which is exactly how `dump-recording` ended up permanently empty.
/// Anything that arms itself once at boot must wait here first.
pub async fn wait_for_main_window(budget: std::time::Duration) -> Option<WebviewWindow> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(w) = get_main_window().await {
            return Some(w);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Where dalzzack keeps what it writes: screenshots and logs.
///
/// Per-user, and never next to the binary: the install folder may not be
/// writable (`C:\Windows\System32`, `/usr/local/bin`), and `navigate` failed
/// there with a bare `os error 5`.
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("dalzzack")
}

/// The absolute form of `path`, for printing. On Windows, `canonicalize` gives a
/// `\\?\` path, which many programs cannot open, Git Bash among them.
pub fn printable_path(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    without_verbatim_prefix(&absolute.to_string_lossy())
}

fn without_verbatim_prefix(path: &str) -> String {
    match path.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_string(),
        },
        None => path.to_string(),
    }
}

/// The folder that holds one sub-folder of files per instance.
pub fn get_sites_path() -> PathBuf {
    data_dir().join("sites")
}

#[cfg(test)]
mod tests {
    #[test]
    fn printed_paths_have_no_verbatim_prefix() {
        let plain = super::without_verbatim_prefix;
        assert_eq!(plain(r"\\?\D:\a\b.png"), r"D:\a\b.png");
        assert_eq!(plain(r"\\?\UNC\host\share\b.png"), r"\\host\share\b.png");
        assert_eq!(plain("/Users/a/b.png"), "/Users/a/b.png");
    }

    #[test]
    fn files_never_go_next_to_the_binary() {
        let exe = std::env::current_exe().unwrap();
        let exe_dir = exe.parent().unwrap();
        assert!(!super::get_sites_path().starts_with(exe_dir));
        assert!(super::get_sites_path().starts_with(super::data_dir()));
    }

    #[test]
    fn csp_is_never_reported_as_bypassed_off_windows() {
        // `Page.setBypassCSP` needs CDP, which only WebView2 has. Claiming a
        // bypass on a backend that cannot perform one would make
        // `console-logs --source csp` look untrustworthy when it is fine.
        if !cfg!(target_os = "windows") {
            assert!(!super::csp_bypassed());
        }
    }
}
