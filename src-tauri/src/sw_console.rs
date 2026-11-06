//! Service Worker console capture (Windows / WebView2 only).
//!
//! A service worker runs in its own target with its own V8 isolate, so nothing
//! it logs ever reaches the page's console — `console_logs` and
//! `diag_console_dump` are structurally blind to it. When the interesting
//! failure (a panic, a trace) happens *inside* the SW, that's the one place you
//! need to look.
//!
//! CDP reaches it via `Target.setAutoAttach`, which hands us a `sessionId` per
//! non-page target. `Runtime.consoleAPICalled` / `Runtime.exceptionThrown`
//! events then arrive tagged with that session id (readable through
//! `ICoreWebView2DevToolsProtocolEventReceivedEventArgs2::SessionId`), and
//! session-scoped calls go out through [`crate::cdp::call_for_session`].
//!
//! Arming happens once at daemon start (see `daemon::server::run_daemon`)
//! rather than on first read: a service worker starts up with the first
//! navigation, and its most interesting logs are the startup ones.
//!
//! If `Target.setAutoAttach` is unavailable the tool says so explicitly
//! (`SW_ATTACH_UNAVAILABLE`) instead of returning an empty list, which would be
//! indistinguishable from "the SW logged nothing".

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

/// Ring buffer size — matches the in-page console capture.
const MAX_ENTRIES: usize = 500;

#[derive(Clone, Debug)]
pub struct SwLogEntry {
    pub ts_ms: u128,
    pub session_id: String,
    pub target_url: String,
    pub level: String,
    pub text: String,
    /// `console` or `exception`.
    pub source: String,
}

impl SwLogEntry {
    fn to_json(&self) -> Value {
        json!({
            "ts_ms": self.ts_ms,
            "session_id": self.session_id,
            "target_url": self.target_url,
            "level": self.level,
            "text": self.text,
            "source": self.source,
        })
    }
}

/// Whether `Target.setAutoAttach` succeeded, so reads can distinguish
/// "nothing logged" from "we were never listening".
#[derive(Clone, Debug)]
pub enum ArmState {
    /// Arming hasn't been attempted (non-Windows, or daemon startup skipped it).
    NotAttempted,
    Armed,
    Failed(String),
}

struct State {
    entries: VecDeque<SwLogEntry>,
    /// sessionId → target URL, for sessions we've identified as service workers.
    sessions: HashMap<String, String>,
    /// The same ids in attach order — "the" service worker for `exec-js
    /// --target sw` is the most recently attached one.
    session_order: Vec<String>,
    /// sessionId → (target id, URL at attach) for cross-site (out-of-process)
    /// iframes. Such a frame is its own target: the page session sees none of
    /// its contexts or console calls, so this session is the only way in. For
    /// an iframe target the target id IS its frame id.
    frames: HashMap<String, (String, String)>,
    arm: ArmState,
    dropped: u64,
    /// Why each transport is or isn't available. `SW_ATTACH_UNAVAILABLE` used to
    /// be the whole answer, which left no way to tell "no service worker
    /// exists" from "this runtime refuses to attach" — and so no way to decide
    /// whether a workaround was even needed.
    diag: Diagnostics,
}

/// Everything a failed attach should have told you the first time.
#[derive(Clone, Debug, Default)]
struct Diagnostics {
    /// Raw CDP error from `Target.setAutoAttach`, if it failed.
    auto_attach_error: Option<String>,
    /// Raw CDP error from `ServiceWorker.enable`, if it failed.
    sw_domain_error: Option<String>,
    /// Did `ServiceWorker.enable` actually succeed? Tracked separately because
    /// "no error recorded" also describes a call that was never made — treating
    /// that as a live transport would suppress the very failure report this
    /// module exists to give.
    sw_domain_ok: bool,
    /// Is `ICoreWebView2_11` (session-scoped CDP) present? Without it, the
    /// auto-attach path cannot talk to a worker session at all.
    session_cdp_available: Option<bool>,
    /// Service worker versions the `ServiceWorker` domain has reported —
    /// non-empty means a worker exists even if we never attached to it.
    versions_seen: usize,
    /// A worker session attached but `Runtime.enable` on it failed, so that
    /// worker's `console.*` is unavailable. Degrades the console source only —
    /// worker errors still arrive via the `ServiceWorker` domain.
    session_runtime_enable_error: Option<String>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            entries: VecDeque::new(),
            sessions: HashMap::new(),
            session_order: Vec::new(),
            frames: HashMap::new(),
            arm: ArmState::NotAttempted,
            dropped: 0,
            diag: Diagnostics::default(),
        })
    })
}

/// Tokio handle captured while arming.
///
/// CDP event callbacks are invoked by WebView2 on its own UI thread, which has
/// no tokio runtime — `Handle::try_current()` there always fails. Session-scoped
/// CDP calls are async, so the callback needs a handle it can `spawn` onto, and
/// the only place with one is `arm()`.
#[cfg(target_os = "windows")]
fn runtime_handle() -> &'static OnceLock<tokio::runtime::Handle> {
    static H: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    &H
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn set_arm_state(s: ArmState) {
    if let Ok(mut st) = state().lock() {
        st.arm = s;
    }
}

/// Record a service worker session discovered via `Target.attachedToTarget`.
pub fn register_session(session_id: &str, target_url: &str) {
    if let Ok(mut st) = state().lock() {
        st.sessions
            .insert(session_id.to_string(), target_url.to_string());
        st.session_order.retain(|s| s != session_id);
        st.session_order.push(session_id.to_string());
    }
}

pub fn unregister_session(session_id: &str) {
    if let Ok(mut st) = state().lock() {
        st.sessions.remove(session_id);
        st.session_order.retain(|s| s != session_id);
        st.frames.remove(session_id);
    }
}

/// Record a cross-site iframe session discovered via `Target.attachedToTarget`.
pub fn register_frame_session(session_id: &str, target_id: &str, url: &str) {
    if let Ok(mut st) = state().lock() {
        st.frames.insert(
            session_id.to_string(),
            (target_id.to_string(), url.to_string()),
        );
    }
}

/// The frame id (= target id) a cross-site frame session belongs to.
pub fn frame_for_session(session_id: &str) -> Option<String> {
    state()
        .lock()
        .ok()?
        .frames
        .get(session_id)
        .map(|(t, _)| t.clone())
}

/// The session attached to the out-of-process frame `frame_id`, if any.
pub fn session_for_frame(frame_id: &str) -> Option<String> {
    state()
        .lock()
        .ok()?
        .frames
        .iter()
        .find(|(_, (t, _))| t == frame_id)
        .map(|(s, _)| s.clone())
}

/// Every attached cross-site frame as `(session id, frame id, URL at attach)`.
pub fn frame_sessions() -> Vec<(String, String, String)> {
    state()
        .lock()
        .map(|st| {
            st.frames
                .iter()
                .map(|(s, (t, u))| (s.clone(), t.clone(), u.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Attached service worker sessions as `(session_id, script_url)`, oldest
/// first.
pub fn sessions() -> Vec<(String, String)> {
    state()
        .lock()
        .map(|st| {
            st.session_order
                .iter()
                .filter_map(|id| st.sessions.get(id).map(|u| (id.clone(), u.clone())))
                .collect()
        })
        .unwrap_or_default()
}

/// How many worker versions the `ServiceWorker` domain has reported — "a worker
/// exists" even when no session is attached to it.
pub fn worker_versions_seen() -> usize {
    state().lock().map(|st| st.diag.versions_seen).unwrap_or(0)
}

/// Is this session id one of the service worker targets we attached to?
///
/// Used both to ingest here and to keep the same events out of the page-scoped
/// recording buffer.
pub fn is_sw_session(session_id: &str) -> bool {
    state()
        .lock()
        .map(|st| st.sessions.contains_key(session_id))
        .unwrap_or(false)
}

/// Record a `ServiceWorker.workerErrorReported` event.
///
/// This is the second, independent source. `Target.setAutoAttach` only reaches
/// targets *related to the page session*, and a service worker is owned by the
/// browser target — which is why attaching so often fails here. The
/// `ServiceWorker` domain, by contrast, is answerable on the page session
/// (`clear-site-data` already drives `ServiceWorker.unregister` through it), so
/// errors thrown inside the worker are reachable even when attach is not.
pub fn ingest_worker_error(params: &Value) {
    let err = params.pointer("/errorMessage");
    let text = err
        .and_then(|e| e.get("errorMessage"))
        .and_then(|m| m.as_str())
        .unwrap_or("service worker error")
        .to_string();
    let source = err
        .and_then(|e| e.get("sourceURL"))
        .and_then(|u| u.as_str())
        .unwrap_or_default()
        .to_string();
    let line = err
        .and_then(|e| e.get("lineNumber"))
        .and_then(|l| l.as_i64())
        .unwrap_or(-1);

    let entry = SwLogEntry {
        ts_ms: now_ms(),
        session_id: String::new(),
        target_url: source.clone(),
        level: "error".to_string(),
        text: if source.is_empty() {
            text
        } else {
            format!("{text} ({source}:{line})")
        },
        source: "service_worker_error".to_string(),
    };

    if let Ok(mut st) = state().lock() {
        if st.entries.len() >= MAX_ENTRIES {
            st.entries.pop_front();
            st.dropped += 1;
        }
        st.entries.push_back(entry);
    }
}

/// Note that the `ServiceWorker` domain reported a worker version — proof a
/// worker exists, which turns "no logs" into a meaningful answer.
pub fn note_worker_version() {
    if let Ok(mut st) = state().lock() {
        st.diag.versions_seen = st.diag.versions_seen.saturating_add(1);
    }
}

/// Only the Windows arming path writes diagnostics (and the tests assert on
/// them); off Windows nothing sets them, so the helper would be dead code.
#[cfg(any(target_os = "windows", test))]
fn set_diag(f: impl FnOnce(&mut Diagnostics)) {
    if let Ok(mut st) = state().lock() {
        f(&mut st.diag);
    }
}

/// Ingest a `Runtime.consoleAPICalled` / `Runtime.exceptionThrown` event that
/// arrived on a service worker session.
pub fn ingest(session_id: &str, event_name: &str, params: &Value) {
    let (level, text, source) = match event_name {
        "Runtime.consoleAPICalled" => {
            let level = match params.get("type").and_then(|t| t.as_str()).unwrap_or("log") {
                "warning" => "warn",
                "error" | "assert" => "error",
                "debug" => "debug",
                "info" => "info",
                _ => "log",
            };
            (level.to_string(), format_console_args(params), "console")
        }
        "Runtime.exceptionThrown" => ("error".to_string(), format_exception(params), "exception"),
        _ => return,
    };

    let target_url = state()
        .lock()
        .ok()
        .and_then(|st| st.sessions.get(session_id).cloned())
        .unwrap_or_default();

    let entry = SwLogEntry {
        ts_ms: now_ms(),
        session_id: session_id.to_string(),
        target_url,
        level,
        text,
        source: source.to_string(),
    };

    if let Ok(mut st) = state().lock() {
        if st.entries.len() >= MAX_ENTRIES {
            st.entries.pop_front();
            st.dropped += 1;
        }
        st.entries.push_back(entry);
    }
}

/// Flatten `console.*` arguments into a single line, the way DevTools shows it.
fn format_console_args(params: &Value) -> String {
    let Some(args) = params.get("args").and_then(|a| a.as_array()) else {
        return String::new();
    };
    args.iter()
        .map(|a| {
            if let Some(v) = a.get("value") {
                match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            } else if let Some(d) = a.get("description").and_then(|d| d.as_str()) {
                d.to_string()
            } else if let Some(u) = a.get("unserializableValue").and_then(|u| u.as_str()) {
                u.to_string()
            } else {
                a.get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?")
                    .to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_exception(params: &Value) -> String {
    let d = params.pointer("/exceptionDetails");
    let described = d
        .and_then(|d| d.pointer("/exception/description"))
        .and_then(|v| v.as_str());
    if let Some(s) = described {
        return s.to_string();
    }
    d.and_then(|d| d.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("uncaught exception")
        .to_string()
}

/// Read the buffer, newest first.
pub fn snapshot(level: Option<&str>, max: usize, clear: bool) -> Value {
    let Ok(mut st) = state().lock() else {
        return json!({
            "status": "failed",
            "error": "SW_CONSOLE_LOCK_POISONED",
            "message": "service worker console buffer is unavailable",
        });
    };

    let arm = st.arm.clone();
    let diag = st.diag.clone();
    let diag_json = json!({
        "auto_attach_error": diag.auto_attach_error,
        "service_worker_domain_error": diag.sw_domain_error,
        "session_runtime_enable_error": diag.session_runtime_enable_error,
        "session_cdp_available": diag.session_cdp_available,
        "worker_versions_seen": diag.versions_seen,
    });

    // A live transport, or anything already captured, outranks the arm state:
    // reporting SW_ATTACH_UNAVAILABLE while holding worker errors would throw
    // away the very data the caller asked for.
    let sw_domain_alive = diag.sw_domain_ok;
    let have_data = !st.entries.is_empty();

    if let ArmState::Failed(ref why) = arm {
        if !sw_domain_alive && !have_data {
            return json!({
                "status": "failed",
                "error": "SW_ATTACH_UNAVAILABLE",
                "message": format!("no transport could reach the service worker: {why}"),
                // Which of the three possibilities it is — no target, refused
                // attach, or too old a runtime — is answerable from here.
                "diagnostics": diag_json,
                "hint": if diag.session_cdp_available == Some(false) {
                    "This WebView2 runtime lacks ICoreWebView2_11, so session-scoped CDP (and with it \
                     service worker attach) is unavailable. Update the WebView2 runtime."
                } else if diag.versions_seen == 0 {
                    "No service worker version was ever reported, so most likely this origin has no \
                     service worker running yet — navigate first, then read again."
                } else {
                    "A service worker exists but neither transport reached it. `dump-recording \
                     --filter console` still shows the page's console (start the daemon with --record)."
                },
            });
        }
    }
    if let ArmState::NotAttempted = arm {
        return json!({
            "status": "failed",
            "error": "UNSUPPORTED_PLATFORM",
            "message": "sw-console needs CDP session attach (Windows/WebView2 only).",
        });
    }

    let mut out: Vec<Value> = st
        .entries
        .iter()
        .rev()
        .filter(|e| level.is_none_or(|l| e.level == l))
        .take(max)
        .map(|e| e.to_json())
        .collect();
    out.reverse();

    let sessions: Vec<Value> = st
        .sessions
        .iter()
        .map(|(id, url)| json!({ "session_id": id, "url": url }))
        .collect();
    let total = st.entries.len();
    let dropped = st.dropped;

    if clear {
        st.entries.clear();
        st.dropped = 0;
    }

    // Whether console capture is actually working is a question of what arrived,
    // not of whether a call returned in time — `Runtime.enable` routinely takes
    // effect on a worker held at startup while its *response* misses the
    // deadline. Report the observed state, not the inferred one.
    let console_capture_working = st.entries.iter().any(|e| e.source == "console");
    let note = if diag.session_runtime_enable_error.is_some() && !console_capture_working {
        Some(
            "Degraded: a worker session attached but Runtime.enable on it failed and no console \
             output has arrived, so this worker's console.* is probably not captured. Errors \
             thrown inside the worker still arrive via the ServiceWorker domain.",
        )
    } else if diag.session_runtime_enable_error.is_some() {
        Some(
            "Runtime.enable on the worker session timed out, but console output is arriving — \
             the call took effect even though its response did not come back in time.",
        )
    } else if sessions.is_empty() {
        Some(
            "No service worker target is currently attached. Either the origin has no \
             service worker, or it hasn't started yet — navigate first, then read again.",
        )
    } else {
        None
    };

    json!({
        "status": "completed",
        // Two independent sources: attached worker sessions give console.* and
        // exceptions; the ServiceWorker domain gives errors only.
        "transport": if diag.auto_attach_error.is_none() {
            "cdp_session+service_worker_domain"
        } else {
            "service_worker_domain"
        },
        "entries": out,
        "buffered": total,
        "dropped": dropped,
        "attached_sessions": sessions,
        "diagnostics": diag_json,
        "note": note,
    })
}

// ===========================================================================
// Arming (Windows only)
// ===========================================================================

/// Is `ICoreWebView2_11` present? Session-scoped CDP — and therefore the whole
/// auto-attach path — depends on it, so its absence is a complete explanation
/// rather than a symptom.
#[cfg(target_os = "windows")]
fn session_cdp_available(window: &tauri::WebviewWindow) -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2_11};
    use windows_core::Interface;

    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let cb = window.with_webview(move |wv| unsafe {
        let controller = wv.controller();
        let ok = controller
            .CoreWebView2()
            .ok()
            .map(|c| {
                let core: ICoreWebView2 = c;
                core.cast::<ICoreWebView2_11>().is_ok()
            })
            .unwrap_or(false);
        let _ = tx.send(ok);
    });
    if cb.is_err() {
        return false;
    }
    rx.recv_timeout(std::time::Duration::from_secs(3))
        .unwrap_or(false)
}

/// Subscribe to the CDP events and turn on auto-attach so service worker
/// targets get a session id we can follow.
///
/// Best-effort: a failure is recorded in [`ArmState`] and surfaced by
/// [`snapshot`] rather than propagated, so daemon startup is never blocked by
/// an old WebView2 runtime.
#[cfg(target_os = "windows")]
pub async fn arm() {
    // Window creation is async and arming happens once at startup, so it has to
    // wait for the window itself — every other tool fetches it at call time and
    // never notices. See `wait_for_main_window`.
    let Some(window) = crate::wait_for_main_window(std::time::Duration::from_secs(15)).await else {
        set_arm_state(ArmState::Failed(
            "WebView window did not become available within 15s".into(),
        ));
        return;
    };

    // Stash the runtime handle BEFORE subscribing: once auto-attach is on, a
    // callback can fire at any moment and it needs this to resume the target.
    let _ = runtime_handle().set(tokio::runtime::Handle::current());

    if let Err(e) = subscribe_target_events(&window) {
        set_arm_state(ArmState::Failed(e));
        return;
    }

    // `flatten: true` gives us session ids on the events instead of the
    // deprecated `Target.receivedMessageFromTarget` envelope.
    //
    // `waitForDebuggerOnStart: true` guarantees we hear about every target
    // before it runs, so none slips past between attach and `Runtime.enable`
    // (whose replay then delivers the boot logs). The target is held until we
    // resume it, so every attached target MUST be resumed — and resumed
    // first: a held worker answers no session call on WebView2; see
    // `start_session`.
    let params = r#"{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}"#;
    let auto_attach = crate::cdp::call(
        &window,
        "Target.setAutoAttach",
        params,
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await;

    // Second transport, tried unconditionally. A service worker target belongs
    // to the browser, not to this page session, so auto-attach frequently
    // cannot reach it — but the `ServiceWorker` domain answers here, and its
    // `workerErrorReported` carries exactly the failures worth reading.
    let sw_domain = crate::cdp::call(
        &window,
        "ServiceWorker.enable",
        "{}",
        crate::cdp::DEFAULT_TIMEOUT,
    )
    .await;

    set_diag(|d| {
        d.auto_attach_error = auto_attach.as_ref().err().cloned();
        d.sw_domain_error = sw_domain.as_ref().err().cloned();
        d.sw_domain_ok = sw_domain.is_ok();
        d.session_cdp_available = Some(session_cdp_available(&window));
    });

    match (&auto_attach, &sw_domain) {
        (Ok(_), _) => {
            set_arm_state(ArmState::Armed);
            tracing::info!("[sw-console] auto-attach armed");
        }
        (Err(e), Ok(_)) => {
            // Degraded but useful: worker errors will still arrive.
            set_arm_state(ArmState::Armed);
            tracing::warn!(
                "[sw-console] Target.setAutoAttach failed ({}) — falling back to the \
                 ServiceWorker domain (errors only, no console.log)",
                e
            );
        }
        (Err(a), Err(s)) => {
            tracing::warn!("[sw-console] no transport: attach={a}, ServiceWorker.enable={s}");
            set_arm_state(ArmState::Failed(format!(
                "Target.setAutoAttach: {a}; ServiceWorker.enable: {s}"
            )));
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub async fn arm() {
    set_arm_state(ArmState::NotAttempted);
}

/// Subscribe to target lifecycle + the two Runtime events we care about.
///
/// The Runtime events are filtered by session id here, so this subscription
/// never double-counts the page console that `recording` already captures.
#[cfg(target_os = "windows")]
fn subscribe_target_events(window: &tauri::WebviewWindow) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler,
        ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl,
    };
    use windows_core::{implement, Ref, HSTRING};

    const EVENTS: &[&str] = &[
        "Target.attachedToTarget",
        "Target.detachedFromTarget",
        "Runtime.consoleAPICalled",
        "Runtime.exceptionThrown",
        // Page-session-reachable worker errors — the fallback transport.
        "ServiceWorker.workerErrorReported",
        "ServiceWorker.workerVersionUpdated",
    ];

    #[implement(ICoreWebView2DevToolsProtocolEventReceivedEventHandler)]
    struct SwHandler {
        event_name: String,
    }

    impl ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl for SwHandler_Impl {
        fn Invoke(
            &self,
            _sender: Ref<'_, ICoreWebView2>,
            args: Ref<'_, ICoreWebView2DevToolsProtocolEventReceivedEventArgs>,
        ) -> windows_core::Result<()> {
            unsafe {
                let Some(args_ref) = args.as_ref() else {
                    return Ok(());
                };
                let Some(params) = crate::sw_console::read_params(args_ref) else {
                    return Ok(());
                };
                let session_id = crate::sw_console::read_session_id(args_ref);

                match self.event_name.as_str() {
                    "Target.attachedToTarget" => {
                        let ttype = params
                            .pointer("/targetInfo/type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let sid = params
                            .get("sessionId")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if sid.is_empty() {
                            return Ok(());
                        }
                        let url = params
                            .pointer("/targetInfo/url")
                            .and_then(|u| u.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let kind = match ttype.as_str() {
                            "service_worker" => {
                                tracing::info!("[sw-console] attached to SW target {}", url);
                                crate::sw_console::register_session(&sid, &url);
                                SessionKind::ServiceWorker
                            }
                            "iframe" => {
                                let tid = params
                                    .pointer("/targetInfo/targetId")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_default();
                                tracing::info!(
                                    "[sw-console] attached to cross-site frame {} ({})",
                                    tid,
                                    url
                                );
                                crate::sw_console::register_frame_session(&sid, tid, &url);
                                SessionKind::Frame
                            }
                            _ => SessionKind::Other,
                        };
                        // Every auto-attached target is held by
                        // `waitForDebuggerOnStart`, not just the ones we care
                        // about — all of them must be resumed or they hang.
                        crate::sw_console::start_session(sid, kind);
                    }
                    "Target.detachedFromTarget" => {
                        if let Some(sid) = params.get("sessionId").and_then(|s| s.as_str()) {
                            crate::sw_console::unregister_session(sid);
                        }
                    }
                    "ServiceWorker.workerErrorReported" => {
                        crate::sw_console::ingest_worker_error(&params);
                    }
                    "ServiceWorker.workerVersionUpdated" => {
                        crate::sw_console::note_worker_version();
                    }
                    // Only ingest events that arrived on a service worker
                    // session — page-session events belong to `console_logs`.
                    ev => {
                        if let Some(sid) = session_id {
                            if crate::sw_console::is_sw_session(&sid) {
                                crate::sw_console::ingest(&sid, ev, &params);
                            }
                        }
                    }
                }
            }
            Ok(())
        }
    }

    window
        .with_webview(move |wv| unsafe {
            let controller = wv.controller();
            let Ok(core) = controller.CoreWebView2() else {
                tracing::warn!("[sw-console] CoreWebView2 unavailable");
                return;
            };
            let core: ICoreWebView2 = core;
            for ev in EVENTS {
                let name = HSTRING::from(*ev);
                match core.GetDevToolsProtocolEventReceiver(&name) {
                    Ok(receiver) => {
                        let handler: ICoreWebView2DevToolsProtocolEventReceivedEventHandler =
                            SwHandler {
                                event_name: (*ev).to_string(),
                            }
                            .into();
                        let mut token = Default::default();
                        if let Err(e) =
                            receiver.add_DevToolsProtocolEventReceived(&handler, &mut token)
                        {
                            tracing::warn!("[sw-console] subscribe({}) failed: {:?}", ev, e);
                        }
                        // Registration lives on the receiver — hold it for the
                        // process lifetime rather than letting the drop take the
                        // subscription with it.
                        std::mem::forget(receiver);
                    }
                    Err(e) => {
                        tracing::warn!("[sw-console] event receiver({}) failed: {:?}", ev, e)
                    }
                }
            }
        })
        .map_err(|e| format!("with_webview failed: {e:?}"))
}

/// What an auto-attached target is, as far as bringing it to life goes.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    ServiceWorker,
    /// A cross-site iframe running in its own renderer process.
    Frame,
    Other,
}

/// Bring a freshly attached target to life: resume it in every case, then turn
/// on `Runtime` for a service worker or a cross-site frame — which replays the
/// `console.*` calls its script already made, so the first lines are still
/// captured (measured: `sw booted` arrives with the resume-first order).
///
/// Fire-and-forget on the daemon runtime — we're inside a COM callback and must
/// not block it. **The resume is unconditional**: `waitForDebuggerOnStart`
/// holds every auto-attached target, so a target we fail to resume is a target
/// that never runs.
#[cfg(target_os = "windows")]
fn start_session(session_id: String, kind: SessionKind) {
    // Note: this runs on WebView2's UI thread, so the handle has to come from
    // the one we stashed while arming — there is no ambient runtime here.
    let Some(handle) = runtime_handle().get() else {
        // Without a handle we can't resume the target either. Say so loudly:
        // the worker is now stuck waiting for a debugger that will never come.
        tracing::error!(
            "[sw-console] no runtime handle; target {} left waiting for debugger",
            session_id
        );
        return;
    };
    handle.spawn(async move {
        let Some(window) = crate::get_main_window().await else {
            tracing::error!(
                "[sw-console] no window; target {} left waiting for debugger",
                session_id
            );
            return;
        };

        // Resume FIRST. Measured on WebView2: a session-scoped call to a worker
        // that is still held never answers — `Runtime.enable` timed out (2 s)
        // on every worker start, so enabling first delayed every start of
        // every service worker by 2 s, and `exec-js --target sw` right after a
        // load evaluated in a worker whose script had not run yet. Nothing is
        // lost by resuming first: `Runtime.enable` replays the console
        // messages V8 kept since the worker's context was created.
        if let Err(e) = crate::cdp::call_for_session(
            &window,
            &session_id,
            "Runtime.runIfWaitingForDebugger",
            "{}",
            crate::cdp::DEFAULT_TIMEOUT,
        )
        .await
        {
            tracing::error!(
                "[sw-console] runIfWaitingForDebugger failed for {}: {} — that target is stuck",
                session_id,
                e
            );
        }

        const SESSION_ENABLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);
        match kind {
            SessionKind::ServiceWorker => {
                // Treat failure as "no console from this worker" — NOT as
                // "sw-console is unavailable". Flipping the whole tool to
                // failed on one session's hiccup threw away the
                // ServiceWorker-domain errors we had already collected, and
                // reported a working fallback as a total failure.
                if let Err(e) = crate::cdp::call_for_session(
                    &window,
                    &session_id,
                    "Runtime.enable",
                    "{}",
                    SESSION_ENABLE_TIMEOUT,
                )
                .await
                {
                    tracing::warn!("[sw-console] Runtime.enable for session failed: {}", e);
                    set_diag(|d| d.session_runtime_enable_error = Some(e));
                }
            }
            SessionKind::Frame => {
                // A cross-site frame's console reaches `console-logs` only
                // through its own session (measured on 0.18.0: a 127.0.0.1
                // frame under a localhost page logged 700 lines and none
                // arrived). `--no-console-capture` means no `Runtime.enable`
                // anywhere — `exec-js --frame` does not need it.
                if crate::console::transport() != crate::console::Transport::Disabled {
                    if let Err(e) = crate::cdp::call_for_session(
                        &window,
                        &session_id,
                        "Runtime.enable",
                        "{}",
                        SESSION_ENABLE_TIMEOUT,
                    )
                    .await
                    {
                        tracing::warn!("[sw-console] Runtime.enable for frame session failed: {e}");
                    }
                }
                // A cross-site frame inside this one is attached from here,
                // not from the page.
                if let Err(e) = crate::cdp::call_for_session(
                    &window,
                    &session_id,
                    "Target.setAutoAttach",
                    r#"{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}"#,
                    SESSION_ENABLE_TIMEOUT,
                )
                .await
                {
                    tracing::warn!("[sw-console] nested auto-attach for frame session failed: {e}");
                }
            }
            SessionKind::Other => {}
        }
    });
}

/// Read a CDP event's parameter object as JSON.
#[cfg(target_os = "windows")]
pub(crate) unsafe fn read_params(
    args: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
) -> Option<Value> {
    use windows_core::PWSTR;
    let mut p: PWSTR = PWSTR::null();
    if args.ParameterObjectAsJson(&mut p).is_err() || p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as *const _)) };
    serde_json::from_str::<Value>(&s).ok()
}

/// Read a CDP event's session id, if the runtime is new enough to report one.
///
/// `ICoreWebView2DevToolsProtocolEventReceivedEventArgs2` is where `SessionId`
/// lives; on older runtimes the QueryInterface fails and every event is treated
/// as page-scoped (which is what it was before auto-attach existed).
#[cfg(target_os = "windows")]
pub(crate) unsafe fn read_session_id(
    args: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
) -> Option<String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DevToolsProtocolEventReceivedEventArgs2;
    use windows_core::{Interface, PWSTR};

    let args2 = args
        .cast::<ICoreWebView2DevToolsProtocolEventReceivedEventArgs2>()
        .ok()?;
    let mut p: PWSTR = PWSTR::null();
    if unsafe { args2.SessionId(&mut p) }.is_err() || p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as *const _)) };
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run in parallel, and these share the global arm state.
    static ARM_STATE_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn console_args_flatten_to_one_line() {
        let params = json!({
            "type": "error",
            "args": [
                { "type": "string", "value": "kernel panic:" },
                { "type": "number", "value": 42 },
                { "type": "object", "description": "Error: boom" },
                { "type": "undefined" }
            ]
        });
        assert_eq!(
            format_console_args(&params),
            "kernel panic: 42 Error: boom undefined"
        );
    }

    #[test]
    fn exception_prefers_description() {
        let params = json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": { "description": "TypeError: x is not a function\n  at sw.js:1" }
            }
        });
        assert!(format_exception(&params).starts_with("TypeError: x is not a function"));

        let bare = json!({ "exceptionDetails": { "text": "Uncaught (in promise)" } });
        assert_eq!(format_exception(&bare), "Uncaught (in promise)");
    }

    #[test]
    fn unarmed_read_reports_why_instead_of_empty_list() {
        let _guard = ARM_STATE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_arm_state(ArmState::Failed(
            "Target.setAutoAttach not supported".into(),
        ));
        let v = snapshot(None, 100, false);
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error"], "SW_ATTACH_UNAVAILABLE");
        assert!(v["message"].as_str().unwrap().contains("setAutoAttach"));
        // Leave the global back at its default for other tests.
        set_arm_state(ArmState::NotAttempted);
    }

    /// A working fallback must not be reported as a total failure.
    ///
    /// Auto-attach failing while the `ServiceWorker` domain is up is the common
    /// case (a worker target belongs to the browser, not to the page session).
    /// Reporting `SW_ATTACH_UNAVAILABLE` there discarded the worker errors we
    /// had actually captured — the exact failure this module exists to avoid.
    #[test]
    fn live_fallback_is_not_reported_as_unavailable() {
        let _guard = ARM_STATE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_arm_state(ArmState::Failed("Target.setAutoAttach refused".into()));
        set_diag(|d| {
            d.sw_domain_ok = true;
            d.auto_attach_error = Some("Target.setAutoAttach refused".into());
        });

        let v = snapshot(None, 100, false);
        assert_eq!(v["status"], "completed", "{v}");
        assert_eq!(
            v["diagnostics"]["auto_attach_error"],
            "Target.setAutoAttach refused"
        );

        set_diag(|d| *d = Diagnostics::default());
        set_arm_state(ArmState::NotAttempted);
    }

    #[test]
    fn session_registry_round_trip() {
        register_session("SID-TEST", "https://example.test/sw.js");
        assert!(is_sw_session("SID-TEST"));
        assert!(!is_sw_session("SID-OTHER"));
        unregister_session("SID-TEST");
        assert!(!is_sw_session("SID-TEST"));
    }
}
