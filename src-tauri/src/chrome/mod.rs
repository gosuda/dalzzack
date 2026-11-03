//! Real Google Chrome as a dalzzack engine (`start --engine chrome`).
//!
//! # Why
//!
//! WebView2 is Chromium, but it is not Chrome, and the differences are exactly
//! the ones bot defences measure: the TLS ClientHello (Edge's WebView2 build
//! negotiates a different extension set — one missing extension was the whole
//! cause of a Cloudflare challenge in a proxy investigation), the client hints,
//! the brand list, the automation surface. "Does Chrome get challenged here?"
//! cannot be answered by WebView2, and an automation-framework Chrome launched
//! with its usual flags is judged differently again.
//!
//! So this engine launches the installed Chrome with a fresh throw-away profile
//! and *only* `--remote-debugging-port` (see [`launch`]), attaches over the
//! DevTools protocol, and serves the same TCP tool protocol as the WebView
//! daemon. The always-on capture (console, `nav-log`, `failed-requests`,
//! `response-headers`, `--record`) is fed from the same CDP events through the
//! same ingest path, so its output means the same thing on both engines.
//!
//! # Shape
//!
//! One WebSocket to the *browser* target ([`conn`]), flattened sessions on it:
//! the page, its out-of-process iframes (auto-attached from the page session),
//! and service workers (auto-attached from the browser session, held at start so
//! their first log lines are not lost). The profile is deleted when the daemon
//! stops; `kill` reaps Chrome and the profile from a record file, because it
//! bypasses the daemon.

pub mod conn;
pub mod intercept;
pub mod launch;
pub mod tools;

use conn::{Conn, Event};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Events the page session delivers into the `--record` tape — no console
/// events on a `--no-console-capture` daemon, which never enables `Runtime`.
pub fn tape_events() -> Vec<&'static str> {
    let mut v = vec![
        "Network.requestWillBeSent",
        "Network.requestWillBeSentExtraInfo",
        "Network.responseReceived",
        "Network.responseReceivedExtraInfo",
        "Network.loadingFailed",
        "Network.loadingFinished",
        "Page.frameNavigated",
    ];
    if OPTIONS.get().is_none_or(|o| o.console) {
        v.extend([
            "Runtime.consoleAPICalled",
            "Runtime.exceptionThrown",
            "Log.entryAdded",
        ]);
    }
    v
}

/// Everything about the running browser. Replaced wholesale by `reset`,
/// which launches a fresh Chrome on a fresh profile.
pub struct Browser {
    pub conn: Arc<Conn>,
    pub page_session: String,
    pub target_id: String,
    pub version: Value,
    pub pid: u32,
    pub port: u16,
    pub profile_dir: PathBuf,
    generation: u64,
    /// Chrome's own process handle. Holding it is what makes killing by pid
    /// safe: the OS will not hand this pid to another process while the
    /// handle is open (Windows) or un-reaped (Unix).
    child: Mutex<Option<std::process::Child>>,
}

impl Browser {
    /// Wait up to `budget` for Chrome to exit on its own.
    fn wait_exit(&self, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let running = match self.child.lock() {
                Ok(mut c) => c
                    .as_mut()
                    .is_some_and(|ch| matches!(ch.try_wait(), Ok(None))),
                Err(_) => false,
            };
            if !running {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// End Chrome's whole process tree, then release the handle. Blocking.
    fn kill(&self) {
        if let Ok(mut c) = self.child.lock() {
            if let Some(ch) = c.as_mut() {
                if matches!(ch.try_wait(), Ok(None)) {
                    launch::kill_tree(self.pid);
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while matches!(ch.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            *c = None;
        }
    }
}

/// `reset` and `shutdown` replace or end the browser; never both at once.
static LIFECYCLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the daemon was started with — needed again by `reset`.
#[derive(Debug, Clone)]
struct EngineOptions {
    instance_id: String,
    exe: PathBuf,
    width: Option<u32>,
    height: Option<u32>,
    browser_args: Vec<String>,
    console: bool,
}

static STATE: RwLock<Option<Arc<Browser>>> = RwLock::new(None);
static OPTIONS: OnceLock<EngineOptions> = OnceLock::new();
/// Bumped per launched browser, so the old connection's close during `reset`
/// is not mistaken for the user closing Chrome.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Execution contexts, from `Runtime.executionContextCreated`. `session` is
/// `None` for the page itself, `Some` for an out-of-process iframe.
#[derive(Debug, Clone)]
pub struct Context {
    pub id: i64,
    pub frame_id: String,
    pub is_default: bool,
    pub origin: String,
    pub name: String,
    pub session: Option<String>,
}

static CONTEXTS: Mutex<Vec<Context>> = Mutex::new(Vec::new());

/// `(target id, target type, url)` of an auto-attached child target.
type ChildTarget = (String, String, String);

/// Targets auto-attached under the page (OOPIFs, dedicated workers), by
/// session id. For an OOPIF the target id IS the frame id, which is how a
/// frame is routed to its own session.
static CHILDREN: Mutex<Option<HashMap<String, ChildTarget>>> = Mutex::new(None);

/// Service worker targets discovered at browser level: target id → url.
static SW_TARGETS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Top-frame `load` lifecycle events: (loader id, epoch ms), newest last.
static LOADS: Mutex<VecDeque<(String, u64)>> = Mutex::new(VecDeque::new());
static LOAD_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn current() -> Option<Arc<Browser>> {
    STATE.read().ok().and_then(|s| s.clone())
}

pub fn is_ready() -> bool {
    current().is_some_and(|b| !b.conn.is_closed())
}

/// `Chrome/154.0.8037.98` — what `list` and `start` report.
pub fn browser_version() -> Option<String> {
    current().and_then(|b| {
        b.version
            .get("product")
            .and_then(|p| p.as_str())
            .map(String::from)
    })
}

/// The Chrome executable this daemon launched.
pub fn exe_path() -> Option<String> {
    OPTIONS.get().map(|o| o.exe.to_string_lossy().to_string())
}

pub fn browser_info() -> Value {
    match current() {
        Some(b) => json!({
            "product": b.version.get("product"),
            "user_agent": b.version.get("userAgent"),
            "protocol": b.version.get("protocolVersion"),
            "pid": b.pid,
            "devtools_port": b.port,
            "profile_dir": b.profile_dir,
        }),
        None => Value::Null,
    }
}

fn not_running() -> String {
    "CHROME_GONE: the Chrome engine is not connected".to_string()
}

/// A command on the page session.
pub async fn page_call(method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
    let b = current().ok_or_else(not_running)?;
    b.conn
        .call(Some(&b.page_session), method, params, timeout)
        .await
}

/// A command on any session (`None` = the browser target).
pub async fn session_call(
    session: Option<&str>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let b = current().ok_or_else(not_running)?;
    b.conn.call(session, method, params, timeout).await
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// `Runtime.evaluate` with dalzzack's wrapper, so `return` and `--await`
/// behave exactly as on the WebView path. `context_id` = `None` means the
/// session's default (main-world) context.
pub async fn eval_wrapped(
    session: Option<&str>,
    context_id: Option<i64>,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<Value, String> {
    let b = current().ok_or_else(not_running)?;
    let session = session.unwrap_or(&b.page_session).to_string();
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(30000) as u64);
    let mut params = json!({
        "expression": if await_promise {
            crate::platform::wrap_script_async(script)
        } else {
            crate::platform::wrap_script(script)
        },
        "returnByValue": true,
        "awaitPromise": await_promise,
        "userGesture": true,
    });
    if let Some(ctx) = context_id {
        params["contextId"] = json!(ctx);
    }
    let v = b
        .conn
        .call(Some(&session), "Runtime.evaluate", params, timeout)
        .await
        .map_err(|e| {
            if e.contains("timed out") {
                format!("TIMEOUT: {e}")
            } else {
                e
            }
        })?;
    if let Some(exc) = v.get("exceptionDetails") {
        let msg = exc
            .pointer("/exception/description")
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("uncaught exception");
        return Err(format!("JS_EXECUTION_ERROR: {msg}"));
    }
    let raw = v
        .pointer("/result/value")
        .and_then(|x| x.as_str())
        .unwrap_or("null");
    crate::platform::parse_js_result(raw)
}

/// Main world, top frame — the engine behind `webview_execute_js`.
pub async fn eval_page(
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<Value, String> {
    eval_wrapped(None, None, script, timeout_ms, await_promise).await
}

/// Fire-and-forget evaluation (`webview_eval`).
pub async fn eval_nowait(script: &str) -> Result<(), String> {
    let b = current().ok_or_else(not_running)?;
    b.conn.send_nowait(
        Some(&b.page_session),
        "Runtime.evaluate",
        json!({ "expression": script, "userGesture": true }),
    );
    Ok(())
}

/// `exec-js --target sw` on this engine.
pub async fn eval_in_session(
    session_id: &str,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<Value, String> {
    eval_wrapped(Some(session_id), None, script, timeout_ms, await_promise).await
}

/// The main-world context of a frame: `(session, context id)`. An OOPIF lives
/// in its own session, where its default context needs no id at all.
pub fn main_context_for(frame_id: &str) -> Option<(Option<String>, Option<i64>)> {
    if let Some(s) = child_session_for_frame(frame_id) {
        return Some((Some(s), None));
    }
    CONTEXTS.lock().ok().and_then(|c| {
        c.iter()
            .rev()
            .find(|x| x.frame_id == frame_id && x.is_default && x.session.is_none())
            .map(|x| (None, Some(x.id)))
    })
}

pub fn context_origin(frame_id: &str) -> Option<String> {
    CONTEXTS.lock().ok().and_then(|c| {
        c.iter()
            .rev()
            .find(|x| x.frame_id == frame_id && x.is_default)
            .map(|x| x.origin.clone())
    })
}

pub fn child_session_for_frame(frame_id: &str) -> Option<String> {
    CHILDREN.lock().ok().and_then(|c| {
        c.as_ref().and_then(|m| {
            m.iter()
                .find(|(_, (tid, ty, _))| tid == frame_id && ty == "iframe")
                .map(|(s, _)| s.clone())
        })
    })
}

/// The URL of the page target as the browser reports it — no page script.
pub async fn page_url() -> Option<String> {
    let b = current()?;
    let v = b
        .conn
        .call(
            None,
            "Target.getTargetInfo",
            json!({ "targetId": b.target_id }),
            Duration::from_secs(3),
        )
        .await
        .ok()?;
    v.pointer("/targetInfo/url")
        .and_then(|u| u.as_str())
        .map(String::from)
}

/// PNG of the page (the viewport, or a `clip` given in VIEWPORT CSS pixels —
/// what `getBoundingClientRect` returns).
pub async fn capture_png(clip: Option<(f64, f64, f64, f64)>) -> Result<Vec<u8>, String> {
    let mut params = json!({ "format": "png" });
    if let Some((x, y, w, h)) = clip {
        // CDP's clip is in DOCUMENT coordinates; on a scrolled page a
        // viewport rect would capture whatever sits that far from the top.
        let (dx, dy) = match eval_page(
            "var v = window.visualViewport; \
             return JSON.stringify(v ? [v.pageLeft, v.pageTop] : [window.scrollX, window.scrollY])",
            Some(3000),
            false,
        )
        .await
        {
            Ok(Value::String(s)) => serde_json::from_str::<(f64, f64)>(&s).unwrap_or((0.0, 0.0)),
            _ => (0.0, 0.0),
        };
        params["clip"] = json!({ "x": x + dx, "y": y + dy, "width": w, "height": h, "scale": 1 });
    }
    let v = page_call("Page.captureScreenshot", params, Duration::from_secs(20)).await?;
    let data = v
        .get("data")
        .and_then(|d| d.as_str())
        .ok_or("Page.captureScreenshot returned no data")?;
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("bad screenshot base64: {e}"))
}

/// Service workers that can be evaluated in: `(session id, script url)`,
/// attaching on demand to any discovered worker the auto-attach missed.
pub async fn service_worker_targets() -> Vec<(String, String)> {
    let attached = crate::sw_console::sessions();
    let discovered: Vec<(String, String)> = SW_TARGETS
        .lock()
        .ok()
        .and_then(|m| {
            m.as_ref()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        })
        .unwrap_or_default();
    let attached_urls: Vec<String> = attached.iter().map(|(_, u)| u.clone()).collect();
    for (target_id, url) in discovered {
        if attached_urls.contains(&url) {
            continue;
        }
        if let Ok(v) = session_call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            Duration::from_secs(5),
        )
        .await
        {
            if let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) {
                crate::sw_console::register_session(sid, &url);
            }
        }
    }
    crate::sw_console::sessions()
}

// ---------------------------------------------------------------------------
// Load tracking (for `navigate`)
// ---------------------------------------------------------------------------

fn note_load(loader_id: &str) {
    if let Ok(mut l) = LOADS.lock() {
        l.push_back((loader_id.to_string(), crate::recording::now_ms()));
        while l.len() > 50 {
            l.pop_front();
        }
    }
    LOAD_NOTIFY.notify_waiters();
}

fn load_seen(loader_id: Option<&str>, since_ms: u64) -> Option<u64> {
    LOADS.lock().ok().and_then(|l| {
        l.iter()
            .rev()
            .find(|(id, ts)| *ts >= since_ms && loader_id.is_none_or(|want| want == id))
            .map(|(_, ts)| *ts)
    })
}

/// Wait for the top frame's `load` (for `loader_id`, or any load since
/// `since_ms`). Returns when it fired, or `None` on timeout.
pub async fn wait_for_load(
    loader_id: Option<&str>,
    since_ms: u64,
    timeout: Duration,
) -> Option<u64> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Register before checking, so a load landing in between still wakes us.
        let notified = LOAD_NOTIFY.notified();
        if let Some(ts) = load_seen(loader_id, since_ms) {
            return Some(ts);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return load_seen(loader_id, since_ms);
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

fn on_event(ev: Event) {
    let Some(b) = current() else { return };
    match ev.session_id.as_deref() {
        Some(s) if s == b.page_session => on_page_event(&b, &ev),
        Some(s) => on_child_event(&b, s, &ev),
        None => on_browser_event(&b, &ev),
    }
}

fn track_context(ev: &Event, session: Option<&str>) {
    match ev.method.as_str() {
        "Runtime.executionContextCreated" => {
            let c = &ev.params["context"];
            let Some(id) = c.get("id").and_then(|x| x.as_i64()) else {
                return;
            };
            let frame_id = c
                .pointer("/auxData/frameId")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if session.is_none() {
                crate::console::note_context_frame(id, Some(&frame_id));
            }
            if let Ok(mut all) = CONTEXTS.lock() {
                all.push(Context {
                    id,
                    frame_id,
                    is_default: c
                        .pointer("/auxData/isDefault")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false),
                    origin: c
                        .get("origin")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: c
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    session: session.map(String::from),
                });
                if all.len() > 2000 {
                    all.drain(..500);
                }
            }
        }
        "Runtime.executionContextDestroyed" => {
            if let Some(id) = ev.params.get("executionContextId").and_then(|x| x.as_i64()) {
                if let Ok(mut all) = CONTEXTS.lock() {
                    all.retain(|c| !(c.id == id && c.session.as_deref() == session));
                }
            }
        }
        "Runtime.executionContextsCleared" => {
            if let Ok(mut all) = CONTEXTS.lock() {
                all.retain(|c| c.session.as_deref() != session);
            }
        }
        _ => {}
    }
}

fn on_page_event(b: &Arc<Browser>, ev: &Event) {
    track_context(ev, None);
    match ev.method.as_str() {
        "Page.lifecycleEvent" => {
            let top = ev.params.get("frameId").and_then(|f| f.as_str()) == Some(&b.target_id);
            if top && ev.params.get("name").and_then(|n| n.as_str()) == Some("load") {
                if let Some(l) = ev.params.get("loaderId").and_then(|l| l.as_str()) {
                    note_load(l);
                }
            }
        }
        "Target.attachedToTarget" => attach_child(b, &ev.params),
        "Target.detachedFromTarget" => {
            if let Some(sid) = ev.params.get("sessionId").and_then(|s| s.as_str()) {
                if let Ok(mut c) = CHILDREN.lock() {
                    if let Some(m) = c.as_mut() {
                        m.remove(sid);
                    }
                }
                if let Ok(mut all) = CONTEXTS.lock() {
                    all.retain(|c| c.session.as_deref() != Some(sid));
                }
            }
        }
        "Fetch.requestPaused" => intercept::on_request_paused(b, None, &ev.params),
        "ServiceWorker.workerErrorReported" => crate::sw_console::ingest_worker_error(&ev.params),
        "ServiceWorker.workerVersionUpdated" => crate::sw_console::note_worker_version(),
        "Inspector.targetCrashed" => {
            crate::console::push_with_source(
                "error",
                "[dalzzack] the page's renderer process crashed (Inspector.targetCrashed)".into(),
                None,
                None,
                Some("dalzzack".into()),
            );
        }
        _ => {}
    }
    crate::recording::ingest_cdp_event(&ev.method, &ev.params);
}

/// An OOPIF or dedicated worker attached under the page.
fn attach_child(b: &Arc<Browser>, params: &Value) {
    let Some(sid) = params.get("sessionId").and_then(|s| s.as_str()) else {
        return;
    };
    let info = &params["targetInfo"];
    let s = |k: &str| {
        info.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let waiting = params
        .get("waitingForDebugger")
        .and_then(|w| w.as_bool())
        .unwrap_or(false);
    if s("type") == "service_worker" {
        // The page's auto-attach hands over the service worker that controls
        // it — the same worker the browser-level auto-attach already holds and
        // owns. Two holds on one worker stall both sessions until each
        // resumes (measured, Chrome 154: Runtime.enable and Network.enable
        // time out on both copies and the worker sits in `parsed` for
        // seconds). This copy is released at once; it never runs any calls.
        if waiting {
            b.conn
                .send_nowait(Some(sid), "Runtime.runIfWaitingForDebugger", json!({}));
        }
        return;
    }
    if let Ok(mut c) = CHILDREN.lock() {
        c.get_or_insert_with(HashMap::new)
            .insert(sid.to_string(), (s("targetId"), s("type"), s("url")));
    }
    let console = OPTIONS.get().map(|o| o.console).unwrap_or(true);
    let conn = b.conn.clone();
    let sid = sid.to_string();
    let is_frame = s("type") == "iframe";
    tokio::spawn(async move {
        // The target is held (`waitForDebuggerOnStart`) until the resume
        // below, so the frame's first requests are seen — and intercepted —
        // like every later one. Short timeouts: the target waits on us.
        let t = Duration::from_secs(3);
        let _ = conn.call(Some(&sid), "Network.enable", json!({}), t).await;
        if console {
            let _ = conn.call(Some(&sid), "Runtime.enable", json!({}), t).await;
        }
        // Rules apply to every frame's requests, not only the top document's.
        let _ = intercept::enable_on_session(&conn, &sid).await;
        if is_frame {
            // A cross-site frame inside a cross-site frame is attached from
            // here, not from the page.
            let _ = conn
                .call(
                    Some(&sid),
                    "Target.setAutoAttach",
                    json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
                    t,
                )
                .await;
        }
        // Unconditional for a held target: one never resumed never runs.
        if waiting {
            conn.send_nowait(Some(&sid), "Runtime.runIfWaitingForDebugger", json!({}));
        }
    });
}

fn on_child_event(b: &Arc<Browser>, session: &str, ev: &Event) {
    if crate::sw_console::is_sw_session(session) {
        crate::sw_console::ingest(session, &ev.method, &ev.params);
        return;
    }
    track_context(ev, Some(session));
    match ev.method.as_str() {
        // A frame's own Page events would be mistaken for the top document's.
        m if m.starts_with("Page.") => {}
        "Fetch.requestPaused" => intercept::on_request_paused(b, Some(session), &ev.params),
        // Nested frames and workers of this frame.
        "Target.attachedToTarget" => attach_child(b, &ev.params),
        "Target.detachedFromTarget" => {
            if let Some(sid) = ev.params.get("sessionId").and_then(|s| s.as_str()) {
                if let Ok(mut c) = CHILDREN.lock() {
                    if let Some(m) = c.as_mut() {
                        m.remove(sid);
                    }
                }
            }
        }
        // Context ids are only unique within one target: this frame's
        // context 3 is not the page's context 3. Its console lines are tagged
        // with the frame directly instead of through the page's context map.
        "Runtime.consoleAPICalled" | "Runtime.exceptionThrown" => {
            let frame = CHILDREN
                .lock()
                .ok()
                .and_then(|c| c.as_ref().and_then(|m| m.get(session).cloned()))
                .filter(|(_, ty, _)| ty == "iframe")
                .map(|(tid, _, _)| tid);
            crate::recording::ingest_cdp_event_in_frame(&ev.method, &ev.params, frame.as_deref());
        }
        _ => crate::recording::ingest_cdp_event(&ev.method, &ev.params),
    }
}

/// A service worker a web page registered, as opposed to one of the browser's
/// own (`chrome-extension://`, `chrome://`).
fn is_web_worker(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn on_browser_event(b: &Arc<Browser>, ev: &Event) {
    let info = &ev.params["targetInfo"];
    let ty = info.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let tid = info.get("targetId").and_then(|t| t.as_str()).unwrap_or("");
    let url = info.get("url").and_then(|t| t.as_str()).unwrap_or("");
    match ev.method.as_str() {
        "Target.targetCreated" | "Target.targetInfoChanged" if ty == "service_worker" => {
            if let Ok(mut m) = SW_TARGETS.lock() {
                let m = m.get_or_insert_with(HashMap::new);
                if is_web_worker(url) {
                    m.insert(tid.to_string(), url.to_string());
                } else {
                    m.remove(tid);
                }
            }
        }
        // An OOPIF attaches before it navigates; its URL arrives here.
        "Target.targetInfoChanged" if ty == "iframe" => {
            if let Ok(mut c) = CHILDREN.lock() {
                if let Some(m) = c.as_mut() {
                    for (t, _, u) in m.values_mut() {
                        if t == tid {
                            *u = url.to_string();
                        }
                    }
                }
            }
        }
        "Target.targetDestroyed" => {
            let gone = ev
                .params
                .get("targetId")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if let Ok(mut m) = SW_TARGETS.lock() {
                if let Some(m) = m.as_mut() {
                    m.remove(gone);
                }
            }
            // `reset` closes the old browser on purpose; it bumps the
            // generation first, so its page going away is not "the user
            // closed the tab".
            let current_generation = b.generation == GENERATION.load(Ordering::SeqCst);
            if gone == b.target_id && current_generation && !SHUTTING_DOWN.load(Ordering::SeqCst) {
                // The tab dalzzack drives was closed. Without it every tool
                // would fail; closing the window means "stop", as it does for
                // the WebView daemon.
                tracing::warn!("[chrome] the page target was closed — stopping the daemon");
                tokio::spawn(async {
                    shutdown().await;
                    exit_daemon();
                });
            }
        }
        "Target.attachedToTarget" => {
            let Some(sid) = ev.params.get("sessionId").and_then(|s| s.as_str()) else {
                return;
            };
            let waiting = ev
                .params
                .get("waitingForDebugger")
                .and_then(|w| w.as_bool())
                .unwrap_or(false);
            let conn = b.conn.clone();
            let sid = sid.to_string();
            if ty == "service_worker" && !is_web_worker(url) {
                // Chrome's own component extensions run service workers too
                // (measured: `chrome-extension://fignfifo…/service_worker.js`
                // on a fresh profile). They are not the page's, and listing
                // them would make `exec-js --target sw` pick the wrong one.
                if waiting {
                    conn.send_nowait(Some(&sid), "Runtime.runIfWaitingForDebugger", json!({}));
                }
                conn.send_nowait(None, "Target.detachFromTarget", json!({ "sessionId": sid }));
            } else if ty == "service_worker" {
                tracing::info!("[chrome] service worker attached: {url}");
                crate::sw_console::register_session(&sid, url);
                let console = OPTIONS.get().map(|o| o.console).unwrap_or(true);
                tokio::spawn(async move {
                    if console {
                        // Held by waitForDebuggerOnStart, so the worker's own
                        // first console lines are captured. The resume below is
                        // unconditional: a held worker that is never resumed
                        // never runs.
                        let _ = conn
                            .call(
                                Some(&sid),
                                "Runtime.enable",
                                json!({}),
                                Duration::from_secs(3),
                            )
                            .await;
                    }
                    if waiting {
                        conn.send_nowait(Some(&sid), "Runtime.runIfWaitingForDebugger", json!({}));
                    }
                });
            } else if waiting {
                conn.send_nowait(Some(&sid), "Runtime.runIfWaitingForDebugger", json!({}));
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(sid) = ev.params.get("sessionId").and_then(|s| s.as_str()) {
                crate::sw_console::unregister_session(sid);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

fn exit_daemon() -> ! {
    if let Some(o) = OPTIONS.get() {
        let _ = std::fs::remove_file(crate::daemon::port_file_path(&o.instance_id));
        let _ = std::fs::remove_file(crate::daemon::pid_file_path(&o.instance_id));
        let _ = std::fs::remove_file(launch::record_path(&o.instance_id));
    }
    std::process::exit(0);
}

/// Launch Chrome, attach, and arm every capture. Used at start and by `reset`.
async fn launch_and_attach(o: &EngineOptions) -> Result<Arc<Browser>, String> {
    let profile_dir = launch::new_profile_dir(&o.instance_id);
    let (exe, dir, w, h, args) = (
        o.exe.clone(),
        profile_dir.clone(),
        o.width,
        o.height,
        o.browser_args.clone(),
    );
    let launched =
        match tokio::task::spawn_blocking(move || launch::launch(&exe, &dir, w, h, &args))
            .await
            .map_err(|e| format!("launch task: {e}"))
            .and_then(|r| r)
        {
            Ok(l) => l,
            Err(e) => {
                // launch() already killed whatever it started; the folder is ours.
                let dir = profile_dir.clone();
                let _ = tokio::task::spawn_blocking(move || launch::remove_profile(&dir)).await;
                return Err(e);
            }
        };
    let pid = launched.child.id();
    launch::write_record(&o.instance_id, pid, &profile_dir);
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let mut child = Some(launched.child);

    match attach(
        o,
        generation,
        pid,
        launched.port,
        &launched.ws_path,
        &profile_dir,
        &mut child,
    )
    .await
    {
        Ok(b) => Ok(b),
        Err(e) => {
            // Whatever this launch started must not outlive the failure, and
            // its socket closing must not be taken for the user closing
            // Chrome (that would exit the daemon with the reason lost).
            GENERATION.fetch_add(1, Ordering::SeqCst);
            let adopted = current().filter(|b| b.generation == generation);
            if adopted.is_some() {
                if let Ok(mut s) = STATE.write() {
                    *s = None;
                }
            }
            let dir = profile_dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(mut ch) = child {
                    // Still ours (handle held), so the pid is still Chrome.
                    if matches!(ch.try_wait(), Ok(None)) {
                        launch::kill_tree(pid);
                    }
                    let _ = ch.wait();
                } else if let Some(b) = adopted {
                    b.kill();
                }
                launch::remove_profile(&dir)
            })
            .await;
            let _ = std::fs::remove_file(launch::record_path(&o.instance_id));
            Err(e)
        }
    }
}

/// Connect to a just-launched Chrome, attach to its page and arm capture.
/// Takes `child` into the `Browser` once there is one.
async fn attach(
    o: &EngineOptions,
    generation: u64,
    pid: u32,
    port: u16,
    ws_path: &str,
    profile_dir: &std::path::Path,
    child: &mut Option<std::process::Child>,
) -> Result<Arc<Browser>, String> {
    let ws_url = format!("ws://127.0.0.1:{port}{ws_path}");
    let conn = Conn::connect(&ws_url, on_event, move || {
        // Chrome went away under a live daemon (window closed, crash): stop.
        // A planned close — shutdown, reset, a failed start — bumps the
        // generation or sets SHUTTING_DOWN first, and lands here as a no-op.
        if GENERATION.load(Ordering::SeqCst) == generation
            && !SHUTTING_DOWN.swap(true, Ordering::SeqCst)
        {
            tracing::warn!("[chrome] DevTools connection closed — Chrome exited; stopping");
            if let Some(b) = current() {
                b.kill();
                let _ = launch::remove_profile(&b.profile_dir);
            }
            exit_daemon();
        }
    })
    .await?;

    let version = conn
        .call(None, "Browser.getVersion", json!({}), CALL_TIMEOUT)
        .await?;
    let targets = conn
        .call(None, "Target.getTargets", json!({}), CALL_TIMEOUT)
        .await?;
    let page = targets
        .get("targetInfos")
        .and_then(|t| t.as_array())
        .and_then(|list| {
            list.iter()
                .find(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
                .and_then(|t| t.get("targetId").and_then(|x| x.as_str()))
                .map(String::from)
        });
    let target_id = match page {
        Some(t) => t,
        None => conn
            .call(
                None,
                "Target.createTarget",
                json!({ "url": "about:blank" }),
                CALL_TIMEOUT,
            )
            .await?
            .get("targetId")
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or("Target.createTarget returned no targetId")?,
    };
    let attached = conn
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            CALL_TIMEOUT,
        )
        .await?;
    let page_session = attached
        .get("sessionId")
        .and_then(|s| s.as_str())
        .ok_or("Target.attachToTarget returned no sessionId")?
        .to_string();

    let browser = Arc::new(Browser {
        conn,
        page_session,
        target_id,
        version,
        pid,
        port,
        profile_dir: profile_dir.to_path_buf(),
        generation,
        child: Mutex::new(child.take()),
    });
    // Visible to the event handler BEFORE any domain is enabled, so the very
    // first events are routed rather than dropped.
    if let Ok(mut s) = STATE.write() {
        *s = Some(browser.clone());
    }
    arm(&browser, o).await?;
    Ok(browser)
}

async fn arm(b: &Arc<Browser>, o: &EngineOptions) -> Result<(), String> {
    let s = Some(b.page_session.as_str());
    let c = &b.conn;
    let t = CALL_TIMEOUT;
    c.call(s, "Page.enable", json!({}), t).await?;
    c.call(
        s,
        "Page.setLifecycleEventsEnabled",
        json!({ "enabled": true }),
        t,
    )
    .await?;
    c.call(s, "Network.enable", json!({}), t).await?;
    if o.console {
        c.call(s, "Runtime.enable", json!({}), t).await?;
        c.call(s, "Log.enable", json!({}), t).await?;
        // Structured CSP issues; Log.entryAdded already carries the text.
        let _ = c.call(s, "Audits.enable", json!({}), t).await;
        crate::console::set_transport(crate::console::Transport::Cdp);
    } else {
        crate::console::set_transport(crate::console::Transport::Disabled);
    }
    // Worker errors reachable from the page session (sw-console's second source).
    let _ = c.call(s, "ServiceWorker.enable", json!({}), t).await;
    // Out-of-process iframes and dedicated workers get their own sessions,
    // held at start until `attach_child` has enabled Network/Fetch on them —
    // otherwise a frame's first requests escape both capture and `intercept`.
    let _ = c
        .call(
            s,
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            t,
        )
        .await;
    // Service workers belong to the browser target, not the page — attach to
    // them from there, held at start so their boot logs are captured.
    let _ = c
        .call(
            None,
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            t,
        )
        .await;
    if let Err(e) = c
        .call(
            None,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                "filter": [{ "type": "service_worker", "exclude": false }],
            }),
            t,
        )
        .await
    {
        tracing::warn!(
            "[chrome] browser-level auto-attach for service workers failed ({e}); workers will be \
             attached on demand"
        );
    }
    crate::sw_console::set_arm_state(crate::sw_console::ArmState::Armed);

    crate::netlog::set_main_frame_id(&b.target_id);
    crate::netlog::set_armed(true);
    crate::response_headers::set_armed(true);
    intercept::enable_on_session(c, &b.page_session).await?;
    Ok(())
}

/// Daemon entry point for `--engine chrome`.
pub fn run_daemon(instance_id: String, opts: crate::daemon::DaemonOptions) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => fail_startup(&instance_id, &format!("cannot create a tokio runtime: {e}")),
    };
    rt.block_on(async move {
        let exe = match launch::find_chrome(opts.chrome_path.as_deref()) {
            Ok(p) => p,
            Err(e) => fail_startup(&instance_id, &e),
        };
        let o = EngineOptions {
            instance_id: instance_id.clone(),
            exe,
            width: opts.width,
            height: opts.height,
            browser_args: opts.browser_args.clone(),
            console: !opts.no_console_capture,
        };
        let _ = OPTIONS.set(o.clone());
        crate::daemon::set_daemon_config(opts.watch_hang, opts.browser_args.clone());
        if opts.user_agent.is_some() || opts.enforce_csp || opts.private_profile {
            tracing::info!(
                "[chrome] --user-agent / --enforce-csp / --private-profile do not apply: Chrome \
                 keeps its own UA, enforces CSP, and always runs on a throw-away profile"
            );
        }

        // A previous daemon for this id that died without cleaning up (crash,
        // `start` respawning an unresponsive one) may have left Chrome running.
        if let Some(r) = launch::reap(&instance_id) {
            tracing::warn!("[chrome] reaped a previous Chrome for this id: {r}");
        }
        let browser = match launch_and_attach(&o).await {
            Ok(b) => b,
            // launch_and_attach has already ended whatever it started.
            Err(e) => fail_startup(&instance_id, &e),
        };
        crate::profile::set(crate::profile::Profile {
            kind: crate::profile::Kind::Ephemeral,
            path: Some(browser.profile_dir.clone()),
            // Deleted by the daemon itself (`shutdown`), not by `stop`.
            disposable: true,
        });
        tracing::info!(
            "[chrome] attached: {} (pid {}, devtools port {}, profile {})",
            browser_version().unwrap_or_default(),
            browser.pid,
            browser.port,
            browser.profile_dir.display()
        );

        if opts.record {
            crate::recording::enable("cdp");
            crate::recording::mark_armed(&tape_events());
        }
        verify_and_health(o.console, opts.record).await;

        if opts.watch_hang {
            let dir = opts
                .auto_dump_dir
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(crate::recording::watchdog::default_dir);
            let id = instance_id.clone();
            tokio::spawn(async move {
                crate::recording::watchdog::run(dir, id).await;
            });
        }

        if let Err(e) = crate::daemon::run_tcp_server(&instance_id).await {
            tracing::error!("[chrome] TCP server error: {e}");
        }
        shutdown().await;
    });
}

/// `navigator.webdriver` as the page saw it at startup. `true` would mean the
/// browser announces automation — the one thing this engine exists to avoid —
/// so it is measured on every start rather than assumed.
static WEBDRIVER: Mutex<Option<bool>> = Mutex::new(None);

pub fn webdriver_flag() -> Option<bool> {
    WEBDRIVER.lock().ok().and_then(|w| *w)
}

/// The same two proofs the WebView daemon gives before it accepts a command:
/// the page runs JS, and console events actually reach the buffer — plus the
/// fidelity check above.
async fn verify_and_health(console: bool, record: bool) {
    let health = eval_page("return 1", Some(8000), false).await.map(|_| ());
    crate::daemon::set_renderer_health(health);
    let wd = eval_page("return navigator.webdriver === true", Some(3000), false)
        .await
        .ok()
        .and_then(|v| v.as_bool());
    if wd == Some(true) {
        tracing::warn!(
            "[chrome] navigator.webdriver is TRUE — this Chrome announces automation; check \
             --browser-arg for --enable-automation / --headless / --remote-debugging-pipe"
        );
    }
    if let Ok(mut w) = WEBDRIVER.lock() {
        *w = wd;
    }
    if !console {
        return;
    }
    let started = std::time::Instant::now();
    let probe = format!(
        "console.debug('{}'); return 1",
        crate::recording::PROBE_MARKER
    );
    let _ = eval_page(&probe, Some(3000), false).await;
    let deadline = started + Duration::from_secs(3);
    loop {
        let live = crate::console::consume_probe(crate::recording::PROBE_MARKER);
        let taped = crate::recording::consume_probe();
        if live || taped {
            if record {
                crate::recording::mark_arm_verified(true, started.elapsed().as_millis() as u64);
            }
            return;
        }
        if std::time::Instant::now() >= deadline {
            if record {
                crate::recording::mark_arm_verified(false, started.elapsed().as_millis() as u64);
            }
            crate::console::set_transport_error(
                crate::console::Transport::Unavailable,
                "a console line logged at startup never reached the daemon",
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn fail_startup(instance_id: &str, message: &str) -> ! {
    tracing::error!("[chrome] startup failed: {message}");
    let _ = std::fs::write(
        crate::daemon::startup_error_path(instance_id),
        message.as_bytes(),
    );
    std::process::exit(3);
}

static SHUTDOWN: tokio::sync::OnceCell<Value> = tokio::sync::OnceCell::const_new();

/// Close Chrome and delete its profile. Every caller — `__shutdown`, the tab
/// being closed — waits for the same single cleanup to finish and gets its
/// result, so nobody exits the process halfway through deleting the profile.
pub async fn shutdown() -> Value {
    SHUTDOWN
        .get_or_init(|| async {
            SHUTTING_DOWN.store(true, Ordering::SeqCst);
            // An in-flight `reset` finishes first; it may be holding the browser.
            let _guard = LIFECYCLE.lock().await;
            let out = match current() {
                Some(b) => close_browser(&b).await,
                None => json!({ "browser": "not running" }),
            };
            if let Some(o) = OPTIONS.get() {
                let _ = std::fs::remove_file(launch::record_path(&o.instance_id));
            }
            out
        })
        .await
        .clone()
}

/// End one browser: ask it to close, wait for it to exit, kill it only if it
/// will not, then delete its profile. The process handle is held throughout,
/// so a kill can only ever reach this Chrome.
async fn close_browser(b: &Arc<Browser>) -> Value {
    // Graceful first, so Chrome flushes and exits on its own terms.
    let _ = b
        .conn
        .call(None, "Browser.close", json!({}), Duration::from_secs(3))
        .await;
    let b = b.clone();
    tokio::task::spawn_blocking(move || {
        let exited_by_itself = b.wait_exit(Duration::from_secs(3));
        b.kill();
        let removed = launch::remove_profile(&b.profile_dir);
        if let Err(e) = &removed {
            tracing::warn!("[chrome] {e}");
        }
        json!({
            "browser_pid": b.pid,
            "closed_gracefully": exited_by_itself,
            "profile_removed": removed.is_ok(),
            "profile_error": removed.err(),
        })
    })
    .await
    .unwrap_or_else(|e| json!({ "error": format!("close task: {e}") }))
}

/// `reset` on this engine: a brand-new Chrome on a brand-new profile. Nothing
/// a site stored survives, because nothing of the old profile exists any more.
pub async fn reset_profile() -> Result<Value, String> {
    let _guard = LIFECYCLE.lock().await;
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        return Err("the daemon is shutting down".into());
    }
    let o = OPTIONS.get().cloned().ok_or_else(not_running)?;
    let old = current().ok_or_else(not_running)?;
    let old_dir = old.profile_dir.clone();
    // Bump the generation BEFORE closing, so the old socket's close is not
    // taken for the user closing Chrome.
    GENERATION.fetch_add(1, Ordering::SeqCst);
    close_browser(&old).await;
    if let Ok(mut s) = STATE.write() {
        *s = None;
    }
    if let Ok(mut c) = CONTEXTS.lock() {
        c.clear();
    }
    if let Ok(mut c) = CHILDREN.lock() {
        *c = None;
    }
    if let Ok(mut m) = SW_TARGETS.lock() {
        *m = None;
    }
    for (sid, _) in crate::sw_console::sessions() {
        crate::sw_console::unregister_session(&sid);
    }
    let fresh = launch_and_attach(&o).await.map_err(|e| {
        format!(
            "the old Chrome was closed but a new one could not be started ({e}); this daemon has \
             no browser now — stop and start it again"
        )
    })?;
    crate::profile::set_path(fresh.profile_dir.clone());
    verify_and_health(o.console, crate::recording::is_enabled()).await;
    // Document-start scripts lived in the old browser; scripts that are still
    // registered (`reset --keep-rules`) must run in the new one too.
    let reinjected = crate::inject::reregister_cdp().await;
    Ok(json!({
        "cleared": "everything — Chrome was restarted on a new, empty profile",
        "old_profile_removed": !old_dir.exists(),
        "profile_dir": fresh.profile_dir,
        "browser_pid": fresh.pid,
        "generation": fresh.generation,
        "scripts_reinjected": reinjected,
    }))
}
