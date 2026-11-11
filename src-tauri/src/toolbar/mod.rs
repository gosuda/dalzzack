//! The strip above the page: back, forward, reload, the address, and which
//! command the window is running.
//!
//! The bar is a second webview in the same window, docked at the top. It is
//! never part of the page: nothing is injected into the page's DOM, the page
//! cannot see the bar or reach it, and the page keeps the size it was asked for
//! (`--width`/`--height`, `resize`), with the window growing by the bar's
//! height. The bar's own document has no Tauri IPC and no network. Rust tells it
//! what to show, and it asks for things by navigating to a reserved address
//! that [`bar_navigation`] answers and cancels, so a page has nothing to call.
//!
//! The default (WebView) engine only. `--engine chrome` is real Chrome with its
//! own toolbar, and deliberately shows no automation banner.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tauri::webview::{PageLoadEvent, WebviewBuilder};
use tauri::{
    LogicalPosition, LogicalSize, Rect, Url, Webview, WebviewUrl, WebviewWindow, WindowEvent,
};

/// Height of the bar in logical pixels: a 36 px row for history, reload and the
/// address, and a 28 px row for what the window is doing.
const BAR_HEIGHT: f64 = 64.0;

/// Where the bar's links and its address form point. Never resolvable
/// (`.invalid` is reserved), and always cancelled before it leaves the bar.
const ACTION_HOST: &str = "dalzzack.invalid";

const BAR_HTML: &str = include_str!("bar.html");

static ENABLED: AtomicBool = AtomicBool::new(true);
static BAR: OnceLock<Webview> = OnceLock::new();
static INSTANCE: OnceLock<String> = OnceLock::new();
static NAV: Mutex<Nav> = Mutex::new(Nav::new());
static PROBING: AtomicBool = AtomicBool::new(false);
/// Pushes are numbered under a lock so the bar can drop one that arrives late:
/// they are sent from any thread, and from the main thread they overtake the
/// ones still queued.
static PUSH_LOCK: Mutex<()> = Mutex::new(());
static PUSH_SEQ: AtomicU64 = AtomicU64::new(0);
/// The title bar's height as last measured (an `f64`'s bits).
static TITLE_INSET: AtomicU64 = AtomicU64::new(0);

/// What the bar knows about the page.
///
/// Whether Back and Forward have somewhere to go is not something a page can be
/// asked: `history.length` counts entries but not where in them the page is, and
/// the Navigation API only sees entries of the page's own origin. So this keeps
/// its own place in the history: a new entry moves it to the end, and a step
/// back or forward taken by dalzzack's own commands moves it by one.
///
/// A page can also step through history itself (`history.back()`, a site's own
/// back link), and that is not seen. Back therefore asks only for there to be
/// another entry, so it never reads disabled when it would work, and at the
/// oldest entry it does nothing when pressed. Forward follows the place and can
/// read disabled after a page's own step back.
#[derive(Clone)]
struct Nav {
    /// Empty while the landing page shows.
    url: String,
    loading: bool,
    /// `history.length` as last read.
    len: usize,
    /// Which entry of `len` the page is on.
    pos: usize,
}

impl Nav {
    const fn new() -> Self {
        Nav {
            url: String::new(),
            loading: false,
            len: 0,
            pos: 0,
        }
    }

    /// The page's address and `history.length`, as just read.
    fn observe(&mut self, url: String, len: usize) {
        if len > self.len {
            self.pos = len - 1;
        } else if len < self.len {
            // The browser drops the oldest entries past a limit.
            self.pos = self.pos.min(len.saturating_sub(1));
        }
        self.len = len;
        self.url = url;
    }

    /// A step through history that moved the page.
    fn stepped(&mut self, back: bool) {
        self.pos = if back {
            self.pos.saturating_sub(1)
        } else {
            (self.pos + 1).min(self.len.saturating_sub(1))
        };
    }

    fn can_back(&self) -> bool {
        self.len > 1
    }

    fn can_forward(&self) -> bool {
        self.pos + 1 < self.len
    }
}

fn nav() -> MutexGuard<'static, Nav> {
    NAV.lock().unwrap_or_else(|e| e.into_inner())
}

/// `start --no-toolbar` turns the bar off for the whole run. Set before the
/// window is built, because the window's size depends on it.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// How much taller than the page the window is: the bar's height, or nothing
/// when there is no bar. Every size and every screen position that used to
/// assume "the page fills the window" adds this.
pub fn height() -> f64 {
    if is_enabled() {
        BAR_HEIGHT
    } else {
        0.0
    }
}

/// A rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Split a window's content area into the bar (full width, below `top`) and the
/// page (everything under the bar). `top` is what the window's own title bar
/// covers of the content area: nothing on Windows and Linux, where the content
/// area starts below it, and about 32 px on macOS, where it does not.
fn split(width: f64, height: f64, top: f64, bar: f64) -> (Frame, Frame) {
    let top = top.clamp(0.0, height.max(0.0));
    let bar = bar.clamp(0.0, (height - top).max(0.0));
    (
        Frame {
            x: 0.0,
            y: top,
            width,
            height: bar,
        },
        Frame {
            x: 0.0,
            y: top + bar,
            width,
            height: (height - top - bar).max(0.0),
        },
    )
}

fn rect(frame: Frame) -> Rect {
    Rect {
        position: LogicalPosition::new(frame.x, frame.y).into(),
        size: LogicalSize::new(frame.width, frame.height).into(),
    }
}

/// The title bar's height as last measured, in logical pixels. Readable from any
/// thread; see [`measure_title_inset`].
pub fn title_inset() -> f64 {
    f64::from_bits(TITLE_INSET.load(Ordering::SeqCst))
}

/// How much of the top of the window's content area its title bar covers, asked
/// of the window and remembered for [`title_inset`]. **Main thread only**: it
/// reads `NSWindow` geometry, which AppKit allows only there. The window is
/// built and resized on the main thread, which is where this runs.
///
/// On macOS the content view reaches under the title bar, so a webview at the
/// top of it has its first ~32 px hidden (WebKit pushes the page down by that
/// much, which is why `--height` has always given a viewport that short there).
/// The bar has to start below the title bar, and it has to be measured: the
/// height differs between macOS versions. Windows and Linux start the content
/// below the title bar, so it covers nothing there.
#[cfg(target_os = "macos")]
pub fn measure_title_inset(window: &WebviewWindow) -> f64 {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSRect;

    let Ok(ns_window) = window.ns_window() else {
        return title_inset();
    };
    let ns_window: *mut AnyObject = ns_window.cast();
    if ns_window.is_null() {
        return title_inset();
    }
    // SAFETY: a live NSWindow, asked on the main thread for two plain geometry
    // properties, and the pointer is used before this function returns.
    let inset = unsafe {
        let frame: NSRect = msg_send![ns_window, frame];
        let layout: NSRect = msg_send![ns_window, contentLayoutRect];
        (frame.size.height - layout.size.height).max(0.0)
    };
    TITLE_INSET.store(inset.to_bits(), Ordering::SeqCst);
    inset
}

#[cfg(not(target_os = "macos"))]
pub fn measure_title_inset(_window: &WebviewWindow) -> f64 {
    0.0
}

/// How much taller the window is than the page it shows: the bar, and on macOS
/// the title bar the bar must clear. Zero with `--no-toolbar`, which keeps the
/// window exactly as it was.
pub fn extra_height() -> f64 {
    if is_enabled() {
        BAR_HEIGHT + title_inset()
    } else {
        0.0
    }
}

fn bar_url() -> Url {
    let encoded = B64.encode(BAR_HTML);
    Url::parse(&format!("data:text/html;charset=utf-8;base64,{encoded}"))
        .expect("the inline toolbar data URL must be valid")
}

/// What the bar's webview must share with the page's on Windows. WebView2
/// allows one set of browser arguments per profile folder, and a second webview
/// in the same folder with different ones fails outright, leaving a window whose
/// webview is silently missing (see the note above `run_with_instance_id`).
#[derive(Debug, Clone, Default)]
pub struct BarEnv {
    /// The page's `--browser-arg`s, joined as they were given to it.
    pub browser_args: Option<String>,
    /// The page's WebView2 profile folder.
    pub data_directory: Option<std::path::PathBuf>,
}

/// Put the bar above the page. `window` is the window the page was built in,
/// and `page_width` x `page_height` the size the page was asked to have.
pub fn install(
    window: &WebviewWindow,
    instance_id: &str,
    page_width: f64,
    page_height: f64,
    env: &BarEnv,
) -> tauri::Result<()> {
    install_bar(window, instance_id, page_width, page_height, env).inspect_err(|_| {
        // Half an install is a window with a dead strip and no bar. Put the
        // page back where it was, and say there is no toolbar.
        set_enabled(false);
        let _ = window.set_size(LogicalSize::new(page_width, page_height));
        let _ = window.set_min_size(Some(LogicalSize::new(400.0, 300.0)));
        let page: &Webview = window.as_ref();
        let _ = page.set_auto_resize(true);
        let _ = page.set_bounds(rect(Frame {
            x: 0.0,
            y: 0.0,
            width: page_width,
            height: page_height,
        }));
    })
}

fn install_bar(
    window: &WebviewWindow,
    instance_id: &str,
    page_width: f64,
    page_height: f64,
    env: &BarEnv,
) -> tauri::Result<()> {
    let _ = INSTANCE.set(instance_id.to_string());

    let page: &Webview = window.as_ref();
    let host = page.window();

    // The window is built as "page + bar" and, on macOS, also has to clear its
    // title bar, which can only be measured now that the window exists. Its
    // size is set outright rather than adjusted: the size a window is built with
    // comes out a pixel taller than asked, and reading it back to add to it
    // would keep that pixel in the page.
    let inset = measure_title_inset(window);
    let window_height = page_height + BAR_HEIGHT + inset;
    window.set_size(LogicalSize::new(page_width, window_height))?;
    window.set_min_size(Some(LogicalSize::new(400.0, 300.0 + BAR_HEIGHT + inset)))?;
    window.center()?;

    // `set_size` takes effect on the next turn of the event loop, so the first
    // layout works from the size just asked for; later ones read the window.
    let (bar_frame, page_frame) = split(page_width, window_height, inset, BAR_HEIGHT);

    // The page was built to fill the window. It now fills what is left below
    // the bar, and this module keeps it so when the window is resized.
    page.set_auto_resize(false)?;
    page.set_bounds(rect(page_frame))?;

    #[allow(unused_mut)]
    let mut builder = WebviewBuilder::new("bar", WebviewUrl::External(bar_url()))
        .devtools(false)
        .on_navigation(bar_navigation)
        .on_page_load(|_, payload| {
            if matches!(payload.event(), PageLoadEvent::Finished) {
                push();
            }
        });
    #[cfg(target_os = "windows")]
    {
        if let Some(args) = &env.browser_args {
            builder = builder.additional_browser_args(args);
        }
        if let Some(dir) = &env.data_directory {
            builder = builder.data_directory(dir.clone());
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = env;

    let bar = host.add_child(
        builder,
        LogicalPosition::new(bar_frame.x, bar_frame.y),
        LogicalSize::new(bar_frame.width, bar_frame.height),
    )?;
    let _ = BAR.set(bar);

    let for_resize = window.clone();
    window.on_window_event(move |event| {
        if matches!(
            event,
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            if let Err(e) = relayout(&for_resize) {
                tracing::warn!("[toolbar] could not relayout: {e}");
            }
        }
    });

    crate::activity::set_listener(on_activity);
    spawn_watcher(window.clone());
    tracing::info!(
        "[toolbar] installed: bar {BAR_HEIGHT} px, title inset {inset} px, bar {bar_frame:?}, page {page_frame:?}"
    );
    Ok(())
}

fn content_frames(window: &WebviewWindow) -> tauri::Result<(Frame, Frame)> {
    let scale = window.scale_factor()?;
    let size = window.inner_size()?.to_logical::<f64>(scale);
    Ok(split(
        size.width,
        size.height,
        measure_title_inset(window),
        BAR_HEIGHT,
    ))
}

fn relayout(window: &WebviewWindow) -> tauri::Result<()> {
    let (bar_frame, page_frame) = content_frames(window)?;
    let page: &Webview = window.as_ref();
    page.set_bounds(rect(page_frame))?;
    if let Some(bar) = BAR.get() {
        bar.set_bounds(rect(bar_frame))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What the bar shows
// ---------------------------------------------------------------------------

/// Send the bar everything it shows. Safe from any thread, and a no-op until the
/// bar exists.
fn push() {
    let Some(bar) = BAR.get() else { return };
    // The number and the snapshot are taken together, so a higher number is
    // always the newer state, whichever push the bar happens to receive first.
    let order = PUSH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let n = nav().clone();
    let state = json!({
        "seq": PUSH_SEQ.fetch_add(1, Ordering::SeqCst) + 1,
        "id": INSTANCE.get(),
        "nav": {
            "url": n.url,
            "loading": n.loading,
            "canBack": n.can_back(),
            "canForward": n.can_forward(),
        },
        "activity": crate::activity::snapshot(),
    });
    // The number and the snapshot are what the lock is for. Sending can wait on
    // the main thread in some builds, so it happens outside it, and a late
    // arrival is the bar's to drop by its number.
    drop(order);
    // JSON is valid JavaScript, and the bar sets every string with textContent.
    let _ = bar.eval(format!("window.__bar&&window.__bar.set({state})"));
}

fn on_activity() {
    push();
    // A command that just ended may have moved the page.
    probe_when_idle();
}

/// Ask where the page is, unless a command is running: its end asks.
fn probe_when_idle() {
    if crate::activity::snapshot()["state"] != "running" {
        schedule_probe();
    }
}

fn shown_address(url: &str) -> String {
    // The landing page is a data: URL, and an address bar showing it would be
    // a wall of base64.
    if url.starts_with("data:") {
        String::new()
    } else {
        // Like a browser's address bar, everything but the credentials.
        crate::cli::label::without_userinfo(url)
    }
}

/// The page started or finished loading `url` (from the page's own load events).
pub fn note_load(url: &Url, loading: bool) {
    if BAR.get().is_none() {
        return;
    }
    {
        let mut n = nav();
        n.url = shown_address(url.as_str());
        n.loading = loading;
    }
    push();
    if !loading {
        schedule_probe();
    }
}

/// `back` or `forward` took the page one step through history.
pub fn note_history_step(back: bool) {
    if BAR.get().is_none() {
        return;
    }
    nav().stepped(back);
    push();
}

/// Where the page is now, and how many history entries there are. This reads
/// `location` and `history.length` and touches nothing else, so the page can
/// tell no more than it can from any other script evaluation.
const PROBE: &str =
    "return JSON.stringify({u: location.href, n: history.length, r: document.readyState});";

fn schedule_probe() {
    // With `--no-toolbar` nothing here runs: a bare window adds nothing to what
    // the page can observe.
    if BAR.get().is_none() || PROBING.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async {
        probe().await;
        PROBING.store(false, Ordering::SeqCst);
    });
}

async fn probe() {
    let Ok(answer) = crate::platform::js_exec::webview_execute_js_quiet(PROBE, Some(800)).await
    else {
        return;
    };
    let Some(read) = answer
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
    else {
        return;
    };
    let url = shown_address(read["u"].as_str().unwrap_or_default());
    let len = read["n"].as_u64().unwrap_or(0) as usize;
    // A document that is complete is not loading, however the load ended: wry
    // reports a load's start and its success, and nothing when it fails.
    let settled = read["r"].as_str() == Some("complete");
    let changed = {
        let mut n = nav();
        let before = (n.url.clone(), n.loading, n.can_back(), n.can_forward());
        n.observe(url, len);
        if settled {
            n.loading = false;
        }
        before != (n.url.clone(), n.loading, n.can_back(), n.can_forward())
    };
    if changed {
        push();
    }
}

/// A page can change its address without a load: `history.pushState`, a hash
/// link, a single-page app's router. No event says so, so the bar asks now and
/// then, but only while the window has focus: an agent's window that nobody is
/// looking at is asked once per command and once per load, and no more.
fn spawn_watcher(window: WebviewWindow) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            if window.is_focused().unwrap_or(false) {
                probe_when_idle();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// What a person does in the bar
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Action {
    Back,
    Forward,
    Reload,
    Go(String),
}

/// The action a link or the address form in the bar asked for.
fn parse_action(url: &Url) -> Option<Action> {
    match url.path() {
        "/back" => Some(Action::Back),
        "/forward" => Some(Action::Forward),
        "/reload" => Some(Action::Reload),
        "/go" => url
            .query_pairs()
            .find(|(key, _)| key == "u")
            .map(|(_, value)| Action::Go(value.into_owned())),
        _ => None,
    }
}

/// Everything the bar's own webview tries to open goes through here. Its one
/// document is allowed; its actions are done and cancelled; anything else is
/// refused, so the bar can never end up showing a web page.
fn bar_navigation(url: &Url) -> bool {
    if url.scheme() == "data" {
        return true;
    }
    if url.host_str() == Some(ACTION_HOST) {
        if let Some(action) = parse_action(url) {
            tauri::async_runtime::spawn(perform(action));
        }
    }
    false
}

/// `http`, `a+b.c`: the characters a URL scheme may have.
fn is_scheme(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// What a person typed in the address field, as an address to open.
///
/// There is no search: text that is not an address is refused rather than
/// guessed at. `javascript:` and its kin never become an address.
fn normalize_address(input: &str) -> Option<String> {
    let text = input.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    // `example.com/login?next=https://example.com` has a `://` in it and no scheme.
    if let Some((scheme, _)) = lower.split_once("://").filter(|(s, _)| is_scheme(s)) {
        return matches!(scheme, "http" | "https" | "file").then(|| text.to_string());
    }
    if lower == "about:blank" {
        return Some(lower);
    }
    for refused in ["javascript:", "data:", "vbscript:", "blob:"] {
        if lower.starts_with(refused) {
            return None;
        }
    }
    let host = lower.split(['/', '?', '#', ':']).next().unwrap_or_default();
    if host.is_empty() && !lower.starts_with('[') {
        return None;
    }
    let local = host == "localhost"
        || host.ends_with(".local")
        || lower.starts_with('[')
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_private());
    Some(format!(
        "{}://{}",
        if local { "http" } else { "https" },
        text
    ))
}

/// Do what the bar asked, through the same dispatcher a CLI command uses, so it
/// shows in the command line below the address like any other.
async fn perform(action: Action) {
    let site = crate::state::INSTANCE_ID
        .read()
        .await
        .clone()
        .unwrap_or_default();
    let (tool, mut args, words): (&str, Value, Vec<String>) = match action {
        Action::Back => ("back", json!({}), vec!["back".into()]),
        Action::Forward => ("forward", json!({}), vec!["forward".into()]),
        Action::Reload => ("reload", json!({}), vec!["reload".into()]),
        Action::Go(typed) => {
            let Some(url) = normalize_address(&typed) else {
                crate::activity::begin("address not understood").finish(false);
                return;
            };
            (
                "navigate",
                json!({ "url": url }),
                vec!["navigate".into(), "--url".into(), url],
            )
        }
    };
    args["site"] = Value::String(site);
    let label = format!("{} (by hand)", crate::cli::command_label(&words));
    let _ =
        crate::activity::track(label, crate::tools::definitions::execute_tool(tool, &args)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn the_page_gets_what_the_bar_leaves() {
        let (bar, page) = split(1200.0, 864.0, 0.0, BAR_HEIGHT);
        assert_eq!((bar.y, bar.height), (0.0, 64.0));
        assert_eq!((page.y, page.height), (64.0, 800.0));
        assert_eq!((bar.width, page.width), (1200.0, 1200.0));
    }

    #[test]
    fn the_bar_starts_below_a_title_bar_that_covers_the_content() {
        // macOS: a 32 px title bar over the content area.
        let (bar, page) = split(1200.0, 896.0, 32.0, BAR_HEIGHT);
        assert_eq!((bar.y, bar.height), (32.0, 64.0));
        assert_eq!((page.y, page.height), (96.0, 800.0));
        assert_eq!(page.y + page.height, 896.0, "the page reaches the bottom");
    }

    #[test]
    fn a_window_shorter_than_the_bar_leaves_the_page_nothing_rather_than_less_than_nothing() {
        let (bar, page) = split(400.0, 40.0, 0.0, BAR_HEIGHT);
        assert_eq!(bar.height, 40.0);
        assert_eq!(page.height, 0.0);
        let (bar, page) = split(400.0, 20.0, 32.0, BAR_HEIGHT);
        assert_eq!((bar.y, bar.height, page.height), (20.0, 0.0, 0.0));
    }

    #[test]
    fn no_bar_leaves_the_whole_window_to_the_page() {
        let (bar, page) = split(800.0, 600.0, 0.0, 0.0);
        assert_eq!(bar.height, 0.0);
        assert_eq!((page.y, page.height), (0.0, 600.0));
    }

    #[test]
    fn the_bar_links_name_their_actions() {
        let act = |u: &str| parse_action(&url(u));
        assert_eq!(act("https://dalzzack.invalid/back"), Some(Action::Back));
        assert_eq!(
            act("https://dalzzack.invalid/forward"),
            Some(Action::Forward)
        );
        assert_eq!(act("https://dalzzack.invalid/reload"), Some(Action::Reload));
        assert_eq!(
            act("https://dalzzack.invalid/go?u=example.com%2Fa%3Fb%3D1"),
            Some(Action::Go("example.com/a?b=1".into()))
        );
        assert_eq!(act("https://dalzzack.invalid/go"), None, "no address");
        assert_eq!(act("https://dalzzack.invalid/delete"), None);
    }

    #[test]
    fn the_bar_only_allows_its_own_document() {
        // The action host is cancelled, not followed; everything else too.
        assert!(bar_navigation(&url("data:text/html;base64,AAAA")));
        assert!(!bar_navigation(&url("https://example.com/")));
        assert!(!bar_navigation(&url("https://dalzzack.invalid/nothing")));
    }

    #[test]
    fn typed_text_becomes_an_address_or_nothing() {
        let n = normalize_address;
        assert_eq!(n("example.com").as_deref(), Some("https://example.com"));
        assert_eq!(
            n("  example.com/a?b=1 ").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(
            n("https://example.com/x").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(
            n("localhost:3000/app").as_deref(),
            Some("http://localhost:3000/app")
        );
        assert_eq!(
            n("127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(n("[::1]:8080").as_deref(), Some("http://[::1]:8080"));
        assert_eq!(n("about:blank").as_deref(), Some("about:blank"));

        assert_eq!(n(""), None);
        assert_eq!(n("two words"), None, "there is no search");
        assert_eq!(n("javascript:alert(1)"), None);
        assert_eq!(n("JavaScript:alert(1)"), None);
        assert_eq!(n("data:text/html,hi"), None);
        assert_eq!(n("ftp://example.com"), None);
        assert_eq!(n("/just/a/path"), None);

        // A `://` inside the address is not a scheme in front of it.
        assert_eq!(
            n("example.com/login?next=https://example.com/home").as_deref(),
            Some("https://example.com/login?next=https://example.com/home")
        );
        assert_eq!(
            n("web.archive.org/web/https://example.com").as_deref(),
            Some("https://web.archive.org/web/https://example.com")
        );
        // Local means local: loopback and private addresses, not any name that
        // merely starts with 127.
        assert_eq!(
            n("192.168.1.20:3000").as_deref(),
            Some("http://192.168.1.20:3000")
        );
        assert_eq!(n("10.0.0.5").as_deref(), Some("http://10.0.0.5"));
        assert_eq!(
            n("127.attacker.example").as_deref(),
            Some("https://127.attacker.example")
        );
        assert_eq!(n("8.8.8.8").as_deref(), Some("https://8.8.8.8"));
    }

    #[test]
    fn back_needs_another_entry_and_forward_follows_the_place() {
        let mut n = Nav::new();
        assert!(!n.can_back() && !n.can_forward(), "nothing read yet");

        n.observe(String::new(), 1); // the landing page
        assert!(!n.can_back() && !n.can_forward());
        n.observe("https://a.test/".into(), 2);
        n.observe("https://b.test/".into(), 3);
        assert!(n.can_back() && !n.can_forward(), "at the newest entry");

        n.stepped(true);
        n.observe("https://a.test/".into(), 3);
        assert!(n.can_back() && n.can_forward(), "in the middle");
        n.stepped(true);
        assert!(n.can_forward(), "at the oldest entry");
        assert!(
            n.can_back(),
            "Back is never disabled while there is another entry"
        );

        // Stepping past either end stays at the end.
        n.stepped(false);
        n.stepped(false);
        n.stepped(false);
        assert!(!n.can_forward());
    }

    #[test]
    fn back_stays_enabled_when_a_new_page_replaces_the_forward_history() {
        // landing, A; back; open B. The browser drops A, so the history is
        // [landing, B] and its length did not change: nothing here can see that a
        // new entry was made, and a model that kept its place would say there is
        // nothing to go back to.
        let mut n = Nav::new();
        n.observe(String::new(), 1);
        n.observe("https://a.test/".into(), 2);
        n.stepped(true);
        n.observe("https://b.test/".into(), 2);
        assert!(n.can_back());
    }

    #[test]
    fn a_longer_history_puts_the_page_on_the_newest_entry() {
        let mut n = Nav::new();
        n.observe("https://a.test/".into(), 3);
        n.stepped(true);
        assert!(n.can_forward());
        n.observe("https://c.test/".into(), 4);
        assert!(!n.can_forward(), "the page is on the newest entry again");
    }

    #[test]
    fn opening_a_page_after_stepping_back_can_leave_forward_enabled() {
        // The browser drops the forward entries when a new page is opened, so
        // `history.length` does not grow and nothing here can see it. Forward
        // then stays enabled and does nothing when pressed: wrong in the
        // harmless direction. The other way, a disabled Forward that would have
        // worked, is the one this model is built never to show.
        let mut n = Nav::new();
        n.observe("https://a.test/".into(), 3);
        n.stepped(true);
        n.observe("https://c.test/".into(), 3);
        assert!(n.can_forward());
    }

    #[test]
    fn a_reload_or_a_redirect_changes_nothing_about_history() {
        let mut n = Nav::new();
        n.observe("https://a.test/".into(), 2);
        n.observe("https://a.test/".into(), 2);
        n.observe("https://a.test/after-redirect".into(), 2);
        assert!(n.can_back() && !n.can_forward());
    }

    #[test]
    fn the_address_bar_never_shows_credentials() {
        // A browser shows the page's address, path and query included, and
        // never the userinfo.
        assert_eq!(
            shown_address("https://admin:hunter2@staging.test:8443/a?t=1#f"),
            "https://staging.test:8443/a?t=1#f"
        );
        assert_eq!(shown_address("http://user@host.test/"), "http://host.test/");
        assert_eq!(
            shown_address("https://host.test/a@b?x=y@z"),
            "https://host.test/a@b?x=y@z"
        );
    }

    #[test]
    fn a_web_page_is_granted_no_tauri_ipc() {
        // The bar is a second webview in the page's own window, and Tauri's
        // webview commands take a label, so any permission the page holds can
        // reach the bar. A page has no use for IPC (it is driven natively), so
        // the capability names no remote URL at all: nothing granted to a page,
        // and so nothing that can list, hide, move or close the bar.
        let capability: Value =
            serde_json::from_str(include_str!("../../capabilities/default.json")).unwrap();
        assert!(
            capability.get("remote").is_none(),
            "a capability with `remote` grants its permissions to web pages: {capability}"
        );
        let permissions = capability["permissions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for permission in permissions {
            let id = permission["identifier"]
                .as_str()
                .or(permission.as_str())
                .unwrap_or("");
            assert!(
                !id.starts_with("core:webview") && id != "core:default",
                "{id} lets a window's script manage its webviews, the bar included"
            );
        }
    }

    #[test]
    fn the_landing_page_has_no_address() {
        assert_eq!(shown_address("data:text/html;base64,AAAA"), "");
        assert_eq!(
            shown_address("https://example.com/"),
            "https://example.com/"
        );
    }
}
