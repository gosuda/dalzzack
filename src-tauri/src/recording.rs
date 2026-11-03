//! Always-on background event recording.
//!
//! Rationale: every other diagnostics tool (`pause`, `console_dump`, `network_log`,
//! `exec_js_isolated`, …) reaches into the renderer at the moment you call it.
//! When the renderer is wedged, that call wedges too — the data you want is on
//! the wrong side of the freeze.
//!
//! The fix is to pre-arm capture *before* the renderer can hang and buffer the
//! events daemon-side, so `dump_recording` is a pure in-memory read with no
//! IPC into the page. Sources by platform:
//!
//! - **Windows**: real CDP `Network.*` / `Runtime.consoleAPICalled` /
//!   `Runtime.exceptionThrown` over WebView2. These are emitted by the browser
//!   process, not the renderer's V8 isolate, so the stream keeps flowing while
//!   page JS is stuck.
//! - **macOS**: WKWebView has no programmatic CDP, so the network side is
//!   polled out of the in-page capture buffer every 500 ms. When the renderer
//!   wedges this freezes too, but the last successful poll survives in daemon
//!   memory — and the wedge timestamp gets recorded, so the agent can see
//!   exactly when capture stopped. (Console does *not* go through this path;
//!   see [`crate::console`] for the out-of-band transports.)
//!
//! Either way, `dump_recording` is a daemon-memory read, not a CDP call.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const NETWORK_CAP: usize = 5000;
const CONSOLE_CAP: usize = 5000;
const EXCEPTION_CAP: usize = 1000;

/// Treat the renderer as wedged after this many consecutive poll failures.
pub const WEDGE_POLL_THRESHOLD: u32 = 5;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NetworkEvent {
    /// `request` | `response` | `failed` | `finished`
    pub kind: String,
    pub ts_ms: u64,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoded_data_length: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// CDP `Network.requestWillBeSent.initiator` — who caused this request,
    /// with a JS call stack when a script caused it.
    ///
    /// Only the CDP path can answer this. The in-page hook behind
    /// `get-requests` sees `fetch`/XHR only, so a `<script src>` — the usual
    /// thing you're tracing back — never appears there at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConsoleEvent {
    pub ts_ms: u64,
    /// `log` | `info` | `warn` | `error` | `debug`
    pub level: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExceptionEvent {
    pub ts_ms: u64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

pub struct RecordingBuffer {
    enabled: bool,
    /// `cdp` | `in_page_poll` | `none`
    transport: String,
    started_at_ms: u64,
    network: VecDeque<NetworkEvent>,
    console: VecDeque<ConsoleEvent>,
    exceptions: VecDeque<ExceptionEvent>,
    dropped_network: usize,
    dropped_console: usize,
    dropped_exceptions: usize,
    last_event_ms: u64,
    last_poll_ok_ms: Option<u64>,
    consecutive_poll_failures: u32,
    renderer_wedged_at_ms: Option<u64>,
    last_wedge_error: Option<String>,
    /// Did the capture transport actually come up? `enabled` only means the
    /// flag was set at startup; without this, a failed subscription is
    /// indistinguishable from "the page was silent" — an empty dump that lies.
    armed: bool,
    arm_error: Option<String>,
    /// Which event sources were successfully subscribed (Windows/CDP).
    armed_events: Vec<String>,
    /// Did a synthetic event actually make it through the pipeline at startup?
    /// `armed` only says the subscription calls returned success; this says
    /// capture was observed working end to end.
    arm_verified: Option<bool>,
    arm_probe_ms: Option<u64>,
    /// Set by `record off`. The tape is kept — it is evidence — but nothing
    /// after this moment is on it.
    stopped_at_ms: Option<u64>,
    /// How the tape was turned on: `start --record` or `record on`.
    enabled_by: Option<String>,
    /// When the tape was first turned on. `started_at_ms` is the latest `on`.
    first_started_at_ms: Option<u64>,
    /// `(stopped_at_ms, resumed_at_ms)` of every `record off` → `record on`:
    /// the tape keeps the earlier events, so without these the gap is invisible.
    off_periods: Vec<(u64, u64)>,
}

impl RecordingBuffer {
    fn new() -> Self {
        Self {
            enabled: false,
            transport: "none".into(),
            started_at_ms: now_ms(),
            network: VecDeque::with_capacity(NETWORK_CAP),
            console: VecDeque::with_capacity(CONSOLE_CAP),
            exceptions: VecDeque::with_capacity(EXCEPTION_CAP),
            dropped_network: 0,
            dropped_console: 0,
            dropped_exceptions: 0,
            last_event_ms: 0,
            last_poll_ok_ms: None,
            consecutive_poll_failures: 0,
            renderer_wedged_at_ms: None,
            last_wedge_error: None,
            armed: false,
            arm_error: None,
            armed_events: Vec::new(),
            arm_verified: None,
            arm_probe_ms: None,
            stopped_at_ms: None,
            enabled_by: None,
            first_started_at_ms: None,
            off_periods: Vec::new(),
        }
    }
}

fn buffer() -> &'static Mutex<RecordingBuffer> {
    static BUF: OnceLock<Mutex<RecordingBuffer>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(RecordingBuffer::new()))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mark recording active. Must be called from `run_daemon` BEFORE the renderer
/// gets a chance to load anything we care about.
pub fn enable(transport: &str) {
    enable_by(transport, "start --record");
}

/// [`enable`], recording who turned the tape on — a tape started mid-session
/// by `record on` has no events from before that moment, and saying so is
/// the difference between "the page was quiet" and "we weren't listening".
pub fn enable_by(transport: &str, by: &str) {
    if let Ok(mut b) = buffer().lock() {
        let now = now_ms();
        if let Some(stopped) = b.stopped_at_ms {
            b.off_periods.push((stopped, now));
        }
        b.enabled = true;
        b.transport = transport.to_string();
        b.started_at_ms = now;
        b.last_event_ms = now;
        b.stopped_at_ms = None;
        b.first_started_at_ms.get_or_insert(now);
        b.enabled_by = Some(by.to_string());
        tracing::info!(
            "[recording] enabled by {} (transport: {}, started_at_ms: {})",
            by,
            transport,
            b.started_at_ms
        );
    }
}

/// `record off`: stop appending, keep the tape. `clear` also drops it.
pub fn disable(clear: bool) {
    if let Ok(mut b) = buffer().lock() {
        b.enabled = false;
        b.stopped_at_ms = Some(now_ms());
        if clear {
            b.network.clear();
            b.console.clear();
            b.exceptions.clear();
            b.dropped_network = 0;
            b.dropped_console = 0;
            b.dropped_exceptions = 0;
            // Nothing earlier is left on the tape, so there is no gap to show.
            b.off_periods.clear();
            b.first_started_at_ms = None;
        }
    }
    tracing::info!("[recording] disabled (clear: {})", clear);
}

pub fn is_enabled() -> bool {
    buffer().lock().map(|b| b.enabled).unwrap_or(false)
}

/// The tape's state in one word: `on`, `off` (was on, stopped by `record
/// off`; what it caught is still readable) or `never`.
///
/// `armed` is a different question — did the capture transport come up — and
/// stays `true` after `record off` so `record on` can resume at once. Reading
/// `armed` as "is it recording" is the mistake this exists to prevent.
pub fn state_word() -> &'static str {
    buffer()
        .lock()
        .map(|b| {
            state_of(
                b.enabled,
                b.first_started_at_ms.is_some() || b.stopped_at_ms.is_some(),
            )
        })
        .unwrap_or("never")
}

fn state_of(enabled: bool, ever_started: bool) -> &'static str {
    match (enabled, ever_started) {
        (true, _) => "on",
        (false, true) => "off",
        (false, false) => "never",
    }
}

/// Compact state for `record status` and `__ping`.
pub fn status() -> Value {
    let Ok(b) = buffer().lock() else {
        return serde_json::json!({ "error": "RECORDING_LOCK_POISONED" });
    };
    serde_json::json!({
        "recording": state_of(b.enabled, b.first_started_at_ms.is_some() || b.stopped_at_ms.is_some()),
        "enabled": b.enabled,
        "enabled_by": b.enabled_by,
        "armed": b.armed,
        "arm_error": b.arm_error,
        "arm_verified": b.arm_verified,
        "transport": b.transport,
        "started_at_ms": if b.enabled_by.is_some() { Some(b.started_at_ms) } else { None },
        "stopped_at_ms": b.stopped_at_ms,
        "first_started_at_ms": b.first_started_at_ms,
        "off_periods": b.off_periods.iter().map(|(s, r)| serde_json::json!({ "stopped_at_ms": s, "resumed_at_ms": r })).collect::<Vec<_>>(),
        "counts": {
            "network": b.network.len(),
            "console": b.console.len(),
            "exceptions": b.exceptions.len(),
        },
    })
}

/// Does the tape hold anything? A stopped tape with events is still worth
/// reading; an empty one that was never on is not.
pub fn has_events() -> bool {
    buffer()
        .lock()
        .map(|b| !b.network.is_empty() || !b.console.is_empty() || !b.exceptions.is_empty())
        .unwrap_or(false)
}

/// Record that the capture transport came up, and which event sources it got.
pub fn mark_armed(events: &[&str]) {
    if let Ok(mut b) = buffer().lock() {
        b.armed = true;
        b.arm_error = None;
        b.armed_events = events.iter().map(|e| e.to_string()).collect();
    }
    tracing::info!("[recording] armed on {} event source(s)", events.len());
}

/// Record that arming failed. `dump_recording` surfaces this instead of
/// returning a plausible-looking empty tape.
pub fn mark_arm_failed(reason: impl Into<String>) {
    let reason = reason.into();
    if let Ok(mut b) = buffer().lock() {
        b.armed = false;
        b.arm_error = Some(reason.clone());
    }
    tracing::error!("[recording] arming FAILED: {}", reason);
}

pub fn is_armed() -> bool {
    buffer().lock().map(|b| b.armed).unwrap_or(false)
}

/// Marker string for the startup self-test. Deliberately unmistakable: it is
/// logged once, on the daemon's own landing page, before any navigation.
pub const PROBE_MARKER: &str = "__dalzzack_recording_probe__";

/// Look for the self-test console entry and **remove it** from the tape.
///
/// The probe exists to prove capture works; leaving it in the buffer would put
/// dalzzack's own noise into the user's evidence. Returns whether it landed.
pub fn consume_probe() -> bool {
    let Ok(mut b) = buffer().lock() else {
        return false;
    };
    let before = b.console.len();
    b.console.retain(|e| !e.text.contains(PROBE_MARKER));
    before != b.console.len()
}

/// Record the outcome of the startup self-test.
pub fn mark_arm_verified(ok: bool, elapsed_ms: u64) {
    if let Ok(mut b) = buffer().lock() {
        b.arm_verified = Some(ok);
        b.arm_probe_ms = Some(elapsed_ms);
        if !ok {
            // Subscription succeeded but nothing came through: the tape would
            // otherwise look like a quiet page forever.
            b.armed = false;
            b.arm_error = Some(format!(
                "capture self-test failed: a console event logged at startup never reached the \
                 buffer within {elapsed_ms}ms, so the CDP event stream is not actually flowing"
            ));
        }
    }
    if ok {
        tracing::info!("[recording] capture self-test passed in {}ms", elapsed_ms);
    } else {
        tracing::error!(
            "[recording] capture self-test FAILED after {}ms",
            elapsed_ms
        );
    }
}

pub fn arm_error() -> Option<String> {
    buffer().lock().ok().and_then(|b| b.arm_error.clone())
}

pub fn push_network(e: NetworkEvent) {
    let Ok(mut b) = buffer().lock() else { return };
    if !b.enabled {
        return;
    }
    b.last_event_ms = e.ts_ms;
    if b.network.len() == NETWORK_CAP {
        b.network.pop_front();
        b.dropped_network += 1;
    }
    b.network.push_back(e);
}

pub fn push_console(e: ConsoleEvent) {
    let Ok(mut b) = buffer().lock() else { return };
    if !b.enabled {
        return;
    }
    b.last_event_ms = e.ts_ms;
    if b.console.len() == CONSOLE_CAP {
        b.console.pop_front();
        b.dropped_console += 1;
    }
    b.console.push_back(e);
}

pub fn push_exception(e: ExceptionEvent) {
    let Ok(mut b) = buffer().lock() else { return };
    if !b.enabled {
        return;
    }
    b.last_event_ms = e.ts_ms;
    if b.exceptions.len() == EXCEPTION_CAP {
        b.exceptions.pop_front();
        b.dropped_exceptions += 1;
    }
    b.exceptions.push_back(e);
}

/// Called by the macOS poller after a successful read of the in-page buffers.
pub fn mark_poll_ok(at_ms: u64) {
    let Ok(mut b) = buffer().lock() else { return };
    // A successful poll IS the macOS transport working — nothing else arms it.
    if !b.armed {
        b.armed = true;
        b.arm_error = None;
        b.armed_events = vec!["poll:console".into(), "poll:network".into()];
    }
    b.last_poll_ok_ms = Some(at_ms);
    b.consecutive_poll_failures = 0;
    b.renderer_wedged_at_ms = None;
    b.last_wedge_error = None;
}

/// Called by the macOS poller after a failed read. Once the failures cross the
/// `WEDGE_POLL_THRESHOLD` we stop counting and freeze the wedge timestamp at
/// the last good poll — that's the moment automation effectively stopped seeing
/// the page.
pub fn mark_poll_failed(error: &str) {
    let Ok(mut b) = buffer().lock() else { return };
    if !b.enabled {
        return;
    }
    b.consecutive_poll_failures = b.consecutive_poll_failures.saturating_add(1);
    if b.consecutive_poll_failures >= WEDGE_POLL_THRESHOLD && b.renderer_wedged_at_ms.is_none() {
        b.renderer_wedged_at_ms = Some(b.last_poll_ok_ms.unwrap_or(b.started_at_ms));
        b.last_wedge_error = Some(error.to_string());
        tracing::warn!(
            "[recording] renderer wedged at {:?} (after {} failed polls): {}",
            b.renderer_wedged_at_ms,
            b.consecutive_poll_failures,
            error
        );
    }
}

pub fn consecutive_poll_failures() -> u32 {
    buffer()
        .lock()
        .map(|b| b.consecutive_poll_failures)
        .unwrap_or(0)
}

/// Snapshot the buffer into a JSON payload. `filter` is one of
/// `all` | `network` | `console` | `exceptions` (default: all). `since_ms`
/// drops events older than the given epoch millisecond. `clear` empties the
/// underlying ring buffers after snapshotting.
pub fn snapshot(filter: Option<&str>, since_ms: Option<u64>, clear: bool) -> Value {
    let Ok(mut b) = buffer().lock() else {
        return serde_json::json!({
            "status": "failed",
            "error": "RECORDING_LOCK_POISONED",
        });
    };

    let want_net = matches!(filter, None | Some("all") | Some("network"));
    let want_con = matches!(filter, None | Some("all") | Some("console"));
    let want_exc = matches!(filter, None | Some("all") | Some("exceptions"));

    let net: Vec<&NetworkEvent> = if want_net {
        b.network
            .iter()
            .filter(|e| since_ms.is_none_or(|s| e.ts_ms >= s))
            .collect()
    } else {
        vec![]
    };
    let con: Vec<&ConsoleEvent> = if want_con {
        b.console
            .iter()
            .filter(|e| since_ms.is_none_or(|s| e.ts_ms >= s))
            .collect()
    } else {
        vec![]
    };
    let exc: Vec<&ExceptionEvent> = if want_exc {
        b.exceptions
            .iter()
            .filter(|e| since_ms.is_none_or(|s| e.ts_ms >= s))
            .collect()
    } else {
        vec![]
    };

    let payload = serde_json::json!({
        "status": "completed",
        "recording": state_of(b.enabled, b.first_started_at_ms.is_some() || b.stopped_at_ms.is_some()),
        "enabled": b.enabled,
        "armed": b.armed,
        "arm_error": b.arm_error,
        "armed_events": b.armed_events,
        "arm_verified": b.arm_verified,
        "arm_probe_ms": b.arm_probe_ms,
        "dalzzack_version": env!("CARGO_PKG_VERSION"),
        "transport": b.transport,
        "enabled_by": b.enabled_by,
        "started_at_ms": b.started_at_ms,
        "stopped_at_ms": b.stopped_at_ms,
        "first_started_at_ms": b.first_started_at_ms,
        "off_periods": b.off_periods.iter().map(|(s, r)| serde_json::json!({ "stopped_at_ms": s, "resumed_at_ms": r })).collect::<Vec<_>>(),
        "now_ms": now_ms(),
        "last_event_ms": b.last_event_ms,
        "last_poll_ok_ms": b.last_poll_ok_ms,
        "renderer_wedged_at_ms": b.renderer_wedged_at_ms,
        "wedge_error": b.last_wedge_error,
        "counts": {
            "network": b.network.len(),
            "console": b.console.len(),
            "exceptions": b.exceptions.len(),
        },
        "dropped": {
            "network": b.dropped_network,
            "console": b.dropped_console,
            "exceptions": b.dropped_exceptions,
        },
        "returned": {
            "network": net.len(),
            "console": con.len(),
            "exceptions": exc.len(),
        },
        "filter": filter.unwrap_or("all"),
        "network": net,
        "console": con,
        "exceptions": exc,
    });

    if clear {
        b.network.clear();
        b.console.clear();
        b.exceptions.clear();
        b.dropped_network = 0;
        b.dropped_console = 0;
        b.dropped_exceptions = 0;
    }

    payload
}

// ---------------------------------------------------------------------------
// Platform-specific drivers
// ---------------------------------------------------------------------------

/// Cross-platform hang watchdog. Pings the renderer on a short interval; when
/// it stops answering, freezes a recording snapshot to disk so the agent has
/// post-mortem data without having to remember to dump manually.
///
/// This is the answer to feature E: a daemon-side ping every 500 ms via a
/// trivial `webview_execute_js("return 1")`. Five consecutive timeouts → "hang
/// detected" → auto-dump the recording buffer (if `--record` is on) plus a
/// lightweight `hang.json` marker file even when recording is off. Auto-dump
/// fires once per hang; resets after the renderer recovers.
pub mod watchdog {
    use super::{is_enabled, mark_poll_failed, now_ms, snapshot};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const PING_INTERVAL_MS: u64 = 500;
    const PING_TIMEOUT_MS: u32 = 800;
    const HANG_THRESHOLD: u32 = 5;

    pub async fn run(auto_dump_dir: PathBuf, instance_id: String) {
        // Give the webview a moment to come up before the first ping. A
        // first-ping failure during initial blank-page setup would otherwise
        // race the threshold and falsely trip the watchdog.
        tokio::time::sleep(Duration::from_millis(2000)).await;

        if let Err(e) = std::fs::create_dir_all(&auto_dump_dir) {
            tracing::warn!(
                "[watchdog] could not create auto-dump dir {:?}: {} (will still log hangs)",
                auto_dump_dir,
                e
            );
        }

        let mut consecutive_failures: u32 = 0;
        let mut dumped_for_this_hang = false;
        let mut total_hangs: u32 = 0;

        loop {
            // Cheapest possible probe. We don't care about the return value,
            // only whether the renderer answers within PING_TIMEOUT_MS.
            match crate::webview_execute_js("return 1", Some(PING_TIMEOUT_MS)).await {
                Ok(_) => {
                    if consecutive_failures > 0 {
                        tracing::info!(
                            "[watchdog] renderer recovered after {} failed pings",
                            consecutive_failures
                        );
                    }
                    consecutive_failures = 0;
                    dumped_for_this_hang = false;
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!("[watchdog] ping {} failed: {}", consecutive_failures, e);
                    // Mirror the failure into the recording buffer so its own
                    // wedge timestamp logic stays in sync with the watchdog.
                    mark_poll_failed(&e);

                    if consecutive_failures >= HANG_THRESHOLD && !dumped_for_this_hang {
                        total_hangs = total_hangs.saturating_add(1);
                        on_hang(&auto_dump_dir, &instance_id, total_hangs);
                        dumped_for_this_hang = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(PING_INTERVAL_MS)).await;
        }
    }

    fn on_hang(dir: &Path, instance_id: &str, hang_index: u32) {
        let stamp = now_ms();

        // Recording dump: only meaningful when --record is on; otherwise the
        // buffer is empty by design. We still write a hang marker so the agent
        // can prove the watchdog fired.
        let recording_path = if is_enabled() {
            let path = dir.join(format!(
                "dalzzack-hang-{}-{}-recording.json",
                instance_id, stamp
            ));
            let snap = snapshot(None, None, false);
            match serde_json::to_vec_pretty(&snap) {
                Ok(bytes) => match std::fs::write(&path, &bytes) {
                    Ok(()) => {
                        tracing::warn!(
                            "[watchdog] HANG #{} — auto-dumped recording ({} bytes) to {:?}",
                            hang_index,
                            bytes.len(),
                            path
                        );
                        Some(path)
                    }
                    Err(e) => {
                        tracing::error!("[watchdog] recording write failed: {}", e);
                        None
                    }
                },
                Err(e) => {
                    tracing::error!("[watchdog] recording serialize failed: {}", e);
                    None
                }
            }
        } else {
            tracing::warn!(
                "[watchdog] HANG #{} detected, but --record is OFF — only a marker will be written",
                hang_index
            );
            None
        };

        let marker = serde_json::json!({
            "kind": "hang_detected",
            "instance_id": instance_id,
            "ts_ms": stamp,
            "hang_index": hang_index,
            "consecutive_failed_pings": HANG_THRESHOLD,
            "ping_interval_ms": PING_INTERVAL_MS,
            "ping_timeout_ms": PING_TIMEOUT_MS,
            "recording_path": recording_path.as_ref().map(|p| p.display().to_string()),
            "recording_enabled": is_enabled(),
        });
        let marker_path = dir.join(format!(
            "dalzzack-hang-{}-{}-marker.json",
            instance_id, stamp
        ));
        if let Err(e) = std::fs::write(
            &marker_path,
            serde_json::to_vec_pretty(&marker).unwrap_or_default(),
        ) {
            tracing::error!("[watchdog] marker write failed: {}", e);
        } else {
            tracing::warn!("[watchdog] hang marker written to {:?}", marker_path);
        }
    }

    /// Default auto-dump directory (`<temp>/dalzzack-hangs`). Override via
    /// `--auto-dump-dir`.
    pub fn default_dir() -> PathBuf {
        std::env::temp_dir().join("dalzzack-hangs")
    }
}

#[cfg(target_os = "macos")]
pub mod poll {
    //! Poll-based capture for macOS (no programmatic CDP).
    //!
    //! Reads the in-page capture buffers (`NS.logs` for the console fallback,
    //! `NS.cr` for fetch/XHR) at a fixed interval. Each call uses a short per-poll timeout
    //! so a wedged renderer is detected, marked, and skipped instead of
    //! blocking the whole capture loop forever.

    use super::*;
    use std::time::Duration;

    const POLL_INTERVAL_MS: u64 = 500;
    const POLL_TIMEOUT_MS: u32 = 1500;

    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Start the poller once per process — `start --record` and a later
    /// `record on` must not end up with two loops double-ingesting.
    pub fn ensure_running() {
        if !STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tokio::spawn(async {
                run().await;
            });
        }
    }

    pub async fn run() {
        // Wait briefly for the webview to be alive before we start polling —
        // the renderer needs to exist for the capture buffers to exist.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let mut since_logs: u64 = 0;
        let mut since_net: u64 = 0;

        loop {
            // If recording was disabled (e.g. tests), stop polling.
            if !is_enabled() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            // webview_execute_js wraps this in a `function() { <script> }()`,
            // so the script body needs a top-level `return` to deliver a value.
            // No IIFE here — that would only *call* an inner function and the
            // outer wrapper would see undefined.
            // `NS.logs` only exists when the in-page console fallback is active;
            // with WebKit's system-console transport armed it stays empty and
            // console events arrive via `crate::console` instead.
            let script = format!(
                "var sl={since_logs}, sn={since_net}; \
                var NS=window.{ns}||{{}}; \
                var logs=(NS.logs||[]).filter(function(l){{return l.ts>sl;}}); \
                var nets=(NS.cr||[]).filter(function(r){{return r.ts>sn;}}); \
                return {{ now: Date.now(), logs: logs, nets: nets }};",
                ns = crate::pagens::namespace()
            );

            match crate::webview_execute_js(&script, Some(POLL_TIMEOUT_MS)).await {
                Ok(v) => {
                    // Accept either an already-decoded object or a JSON string —
                    // the bridge has flipped between the two across versions.
                    let parsed: Option<Value> = if v.is_object() {
                        Some(v)
                    } else if let Some(s) = v.as_str() {
                        serde_json::from_str::<Value>(s).ok()
                    } else {
                        None
                    };
                    match parsed {
                        Some(p) => {
                            let now = p.get("now").and_then(|x| x.as_u64()).unwrap_or_else(now_ms);
                            ingest_logs(&p, &mut since_logs);
                            ingest_nets(&p, &mut since_net);
                            mark_poll_ok(now);
                        }
                        None => {
                            // Empty / unexpected shape (e.g. about:blank before
                            // first navigate). Soft skip, not a wedge.
                            tracing::debug!("[recording] unexpected poll shape");
                        }
                    }
                }
                Err(e) => {
                    mark_poll_failed(&e);
                }
            }

            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    fn ingest_logs(parsed: &Value, since_logs: &mut u64) {
        let Some(logs) = parsed.get("logs").and_then(|x| x.as_array()) else {
            return;
        };
        for entry in logs {
            let ts = entry
                .get("ts")
                .and_then(|x| x.as_u64())
                .unwrap_or_else(now_ms);
            if ts > *since_logs {
                *since_logs = ts;
            }
            let level = entry
                .get("level")
                .and_then(|x| x.as_str())
                .unwrap_or("log")
                .to_string();
            let text = entry
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            // Uncaught errors and unhandled rejections are captured by the
            // in-page hook with a `[uncaught]` / `[unhandledrejection]`
            // prefix — split them off into the exceptions bucket so the dump
            // has the right shape.
            if text.starts_with("[uncaught]") || text.starts_with("[unhandledrejection]") {
                push_exception(ExceptionEvent {
                    ts_ms: ts,
                    text,
                    url: None,
                    line: None,
                    stack: None,
                });
            } else {
                push_console(ConsoleEvent {
                    ts_ms: ts,
                    level,
                    text,
                    url: None,
                    line: None,
                });
            }
        }
    }

    fn ingest_nets(parsed: &Value, since_net: &mut u64) {
        let Some(nets) = parsed.get("nets").and_then(|x| x.as_array()) else {
            return;
        };
        for r in nets {
            let ts = r.get("ts").and_then(|x| x.as_u64()).unwrap_or_else(now_ms);
            if ts > *since_net {
                *since_net = ts;
            }
            let url = r.get("url").and_then(|x| x.as_str()).map(String::from);
            let method = r.get("method").and_then(|x| x.as_str()).map(String::from);
            let request_id = r
                .get("id")
                .and_then(|x| x.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("xhr-{}", ts));
            let status = r.get("status").and_then(|x| x.as_i64());
            let duration_ms = r.get("dur").and_then(|x| x.as_u64());
            let resource_type = r.get("type").and_then(|x| x.as_str()).map(String::from);
            let error_text = r.get("error").and_then(|x| x.as_str()).map(String::from);

            // The in-page hook records the request and updates it in place when
            // the response arrives. By the time we poll we may see either form;
            // emit a single `finished`-style event if a status is present, a
            // `failed` event if `error` is present, otherwise `request`.
            let kind = if error_text.is_some() {
                "failed"
            } else if status.is_some() {
                "finished"
            } else {
                "request"
            };

            push_network(NetworkEvent {
                kind: kind.to_string(),
                ts_ms: ts,
                request_id,
                url,
                method,
                status,
                mime_type: r.get("ct").and_then(|x| x.as_str()).map(String::from),
                error_text,
                encoded_data_length: None,
                from_cache: None,
                resource_type,
                duration_ms,
                // The in-page hook has no initiator information — only CDP
                // reports who triggered a request.
                initiator: None,
            });
        }
    }
}

#[cfg(target_os = "windows")]
pub mod cdp {
    //! CDP-based capture for Windows.
    //!
    //! Subscribes to `Network.requestWillBeSent`, `Network.responseReceived`,
    //! `Network.loadingFailed`, `Network.loadingFinished`,
    //! `Runtime.consoleAPICalled`, and `Runtime.exceptionThrown` via
    //! `add_DevToolsProtocolEventReceived`. These events are driven by the
    //! browser process (not the renderer's V8 isolate), so the stream survives
    //! a fully hung page.

    use super::*;

    /// Whether this arming also owns the `--record` tape.
    ///
    /// The *domains* are the same either way — `Runtime`, `Log` and `Network`
    /// are all needed for tools that are on by default (`console-logs`,
    /// `response-headers`). `Network.responseReceived` in particular fires once
    /// at navigation time, so the navigation document's headers cannot be
    /// obtained after the fact and the domain has to be up before the first
    /// navigate. Only whether arming failures are reported through the
    /// recording buffer's `armed` / `arm_verified` state differs.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// `--no-console-capture`: only the `Network`/`Page` events behind
        /// `response-headers`, `nav-log` and `failed-requests`. Neither domain
        /// is observable from page script, unlike a console subscription.
        NetworkOnly,
        /// Always-on capture, without `--record`.
        Console,
        /// Also arms the `--record` tape state.
        Full,
    }

    /// Has the event subscription been made? Subscribing twice would deliver
    /// every event twice, so `record on` on a live daemon must know. Set by
    /// the UI-thread closure itself, at the moment the handlers are actually
    /// registered — not by the caller, which may have given up waiting.
    static SUBSCRIBED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Has a subscription closure ever been handed to the UI thread? A caller
    /// that timed out waiting for it cannot know whether it will still run, so
    /// it must never dispatch a second one (that one would double every event).
    static SUBSCRIBE_DISPATCHED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    pub fn is_subscribed() -> bool {
        SUBSCRIBED.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Enable the needed domains and subscribe to events. Must be called after
    /// `setup_cdp` has connected to WebView2 — see `run_daemon`.
    ///
    /// Returns `Err` with the reason when the console transport could not be
    /// armed, so the caller can degrade to the in-page fallback and *say* that
    /// it did.
    pub async fn enable_and_subscribe(mode: Mode) -> Result<(), String> {
        let full = mode == Mode::Full;
        let with_console = mode != Mode::NetworkOnly;
        // Window creation is async. Bailing on the first miss is what made this
        // whole subsystem a no-op: `dump-recording` then reported an honest
        // "0 events" for a firehose that was never turned on.
        let Some(window) = crate::wait_for_main_window(std::time::Duration::from_secs(20)).await
        else {
            let e = "WebView window did not become available within 20s";
            if full {
                super::mark_arm_failed(e);
            }
            return Err(e.to_string());
        };

        let events = events_for(with_console);

        // Subscribe BEFORE enabling the domains: `Network.enable` /
        // `Runtime.enable` start the firehose immediately, and anything emitted
        // between enable and subscribe is simply lost.
        if !SUBSCRIBE_DISPATCHED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            if let Err(e) = subscribe_events(&window, &events).await {
                // A definitive failure (nothing registered) may be retried; a
                // timeout may not — the closure can still run later.
                if !e.contains("timed out") {
                    SUBSCRIBE_DISPATCHED.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                let e = format!("event subscription failed: {e}");
                if full {
                    super::mark_arm_failed(&e);
                }
                return Err(e);
            }
        }

        // Turn the firehose on. These are request/response CDP calls — they
        // don't deliver events themselves. `Page` carries the frame commits the
        // navigation timeline needs to say which document actually loaded.
        let domains: &[&str] = if with_console {
            &[
                "Runtime.enable",
                "Log.enable",
                "Network.enable",
                "Page.enable",
            ]
        } else {
            &["Network.enable", "Page.enable"]
        };
        for domain in domains {
            if let Err(e) = call_cdp_oneshot(&window, domain, "{}").await {
                let e = format!("{domain} failed: {e}");
                if full {
                    super::mark_arm_failed(&e);
                }
                return Err(e);
            }
        }
        crate::response_headers::set_armed(true);
        crate::netlog::set_armed(true);
        // Which frame is the top document — the timeline's default view. A
        // `Page.frameNavigated` would tell us too, but only on the next commit.
        if let Ok(tree) = crate::cdp::call(
            &window,
            "Page.getFrameTree",
            "{}",
            crate::cdp::DEFAULT_TIMEOUT,
        )
        .await
        {
            if let Some(id) =
                crate::cdp::result_at(&tree, "frameTree/frame/id").and_then(|v| v.as_str())
            {
                crate::netlog::set_main_frame_id(id);
            }
        }

        if !with_console {
            return Ok(());
        }

        // Audits carries the structured CSP issue (blockedURL,
        // violatedDirective). Not every WebView2 runtime has the domain, and
        // `Log.entryAdded` already delivers the human-readable violation line,
        // so a failure here degrades rather than kills capture.
        if let Err(e) = call_cdp_oneshot(&window, "Audits.enable", "{}").await {
            tracing::warn!(
                "[recording/cdp] Audits.enable failed ({e}); CSP violations will still arrive \
                 via Log.entryAdded, without the structured fields"
            );
        }

        if full {
            super::mark_armed(&events);
        }

        // Prove it. "The subscribe call returned S_OK" is not the same claim as
        // "events are flowing", and the difference is exactly how this
        // subsystem shipped broken: a tape of zeros reads like a quiet page.
        // The probe runs here, on the daemon's own landing page, before any
        // navigation — so it cannot appear in a real page's evidence — and the
        // entry is removed from the tape once observed.
        verify_capture(&window, full).await
    }

    /// Log one console line via CDP and wait for it to come back through the
    /// event subscription.
    async fn verify_capture(window: &tauri::WebviewWindow, full: bool) -> Result<(), String> {
        let started = std::time::Instant::now();
        let expr = format!(
            r#"{{"expression":"console.debug('{}')"}}"#,
            super::PROBE_MARKER
        );
        if let Err(e) = call_cdp_oneshot(window, "Runtime.evaluate", &expr).await {
            // Can't probe — say so rather than claiming verification passed.
            tracing::warn!("[recording/cdp] self-test could not run: {}", e);
            return Err(format!("self-test could not run: {e}"));
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            // Drain both sinks every pass so the probe never lingers in either.
            let live = crate::console::consume_probe(super::PROBE_MARKER);
            let taped = super::consume_probe();
            if live || taped {
                if full {
                    super::mark_arm_verified(true, started.elapsed().as_millis() as u64);
                }
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                if full {
                    super::mark_arm_verified(false, started.elapsed().as_millis() as u64);
                }
                return Err(
                    "capture self-test failed: a console event logged at startup never reached \
                     the daemon"
                        .to_string(),
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Network and frame lifecycle — none of it observable from page script.
    pub(crate) const NETWORK_EVENTS: &[&str] = &[
        "Network.requestWillBeSent",
        // The headers that actually went out (cookies, `sec-fetch-*`) — the
        // renderer's view in `requestWillBeSent` leaves them out.
        "Network.requestWillBeSentExtraInfo",
        "Network.responseReceived",
        // Raw wire headers — the only place repeated `set-cookie` lines and
        // headers of responses the renderer never surfaces are visible.
        "Network.responseReceivedExtraInfo",
        "Network.loadingFailed",
        "Network.loadingFinished",
        // Which document committed, and the top frame's URL for `list`.
        "Page.frameNavigated",
        "Page.navigatedWithinDocument",
    ];

    pub(crate) const CONSOLE_EVENTS: &[&str] = &[
        "Runtime.consoleAPICalled",
        "Runtime.exceptionThrown",
        // Browser-side messages: CSP violations, mixed content, blocked loads.
        // The page's own `securitypolicyviolation` event does not fire here, so
        // without these a policy block leaves no trace anywhere.
        "Log.entryAdded",
        "Audits.issueAdded",
    ];

    pub(crate) fn events_for(with_console: bool) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = NETWORK_EVENTS.to_vec();
        if with_console {
            v.extend_from_slice(CONSOLE_EVENTS);
        }
        v
    }

    /// `record on` against a live daemon. The subscription made at startup
    /// already carries every event the tape wants, so turning the tape on is a
    /// daemon-memory switch — no new subscription (which would double every
    /// event) and no self-test probe in the user's live page.
    ///
    /// It never claims more than startup proved: a daemon whose capture
    /// self-test FAILED is refused (marking it armed would erase the very
    /// failure `dump-recording` exists to report), and `--no-console-capture`
    /// is honoured — network events only, no `Runtime.enable`.
    pub async fn arm_tape_live() -> Result<Vec<&'static str>, String> {
        use crate::console::Transport;
        let transport = crate::console::transport();
        let with_console = transport == Transport::Cdp;
        if is_subscribed() && transport == Transport::Unavailable {
            return Err(format!(
                "this daemon's CDP event stream failed its startup self-test ({}), so a tape \
                 would prove nothing — restart the daemon",
                crate::console::transport_error().unwrap_or_else(|| "reason unknown".into())
            ));
        }
        if is_subscribed() {
            // Idempotent; gives a domain that failed to come up at start a
            // second chance instead of a tape that silently stays empty.
            if let Some(window) = crate::get_main_window().await {
                let domains: &[&str] = if with_console {
                    &[
                        "Runtime.enable",
                        "Log.enable",
                        "Network.enable",
                        "Page.enable",
                    ]
                } else {
                    &["Network.enable", "Page.enable"]
                };
                for d in domains {
                    let _ = call_cdp_oneshot(&window, d, "{}").await;
                }
            }
        } else {
            // Startup arming never subscribed: do it now, in the mode the
            // daemon was started with. (The Full mode's self-test logs one
            // console.debug line in the live page; it is removed from both
            // buffers once seen.)
            let mode = if transport == Transport::Disabled {
                Mode::NetworkOnly
            } else {
                Mode::Full
            };
            enable_and_subscribe(mode).await?;
        }
        let with_console = with_console || crate::console::transport() == Transport::Cdp;
        let events = events_for(with_console);
        super::mark_armed(&events);
        // The startup self-test proved the stream end to end for the console
        // transport; record that rather than probing the page again. Without
        // console capture there was no probe, and "unverified" is the honest
        // state, not "failed".
        if with_console {
            super::mark_arm_verified(true, 0);
        }
        Ok(events)
    }

    /// Subscribe to the CDP events that feed the recording buffer.
    async fn subscribe_events(
        window: &tauri::WebviewWindow,
        events: &[&'static str],
    ) -> Result<(), String> {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceivedEventArgs,
            ICoreWebView2DevToolsProtocolEventReceivedEventHandler,
            ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl,
        };
        use windows_core::{implement, Ref, HSTRING, PWSTR};

        #[implement(ICoreWebView2DevToolsProtocolEventReceivedEventHandler)]
        struct RecordHandler {
            event_name: String,
        }

        impl ICoreWebView2DevToolsProtocolEventReceivedEventHandler_Impl for RecordHandler_Impl {
            fn Invoke(
                &self,
                _sender: Ref<'_, ICoreWebView2>,
                args: Ref<'_, ICoreWebView2DevToolsProtocolEventReceivedEventArgs>,
            ) -> windows_core::Result<()> {
                unsafe {
                    if let Some(args_ref) = args.as_ref() {
                        // Since `sw-console` turns on `Target.setAutoAttach`,
                        // events from non-page targets also reach this handler.
                        // The recording buffer is page-scoped, so drop anything
                        // belonging to an attached service worker session —
                        // `sw-console` owns those.
                        //
                        // A cross-site frame is its own target too, and its
                        // console lines are the page's business — tagged with
                        // the frame directly, because a context id from that
                        // target means nothing in the page's context map.
                        let mut frame: Option<String> = None;
                        if let Some(sid) = crate::sw_console::read_session_id(args_ref) {
                            if crate::sw_console::is_sw_session(&sid) {
                                return Ok(());
                            }
                            match crate::sw_console::frame_for_session(&sid) {
                                Some(fid)
                                    if matches!(
                                        self.event_name.as_str(),
                                        "Runtime.consoleAPICalled" | "Runtime.exceptionThrown"
                                    ) =>
                                {
                                    frame = Some(fid)
                                }
                                // Anything else from a child session (a worker,
                                // or a frame's own lifecycle events) would be
                                // mistaken for the page's.
                                _ => return Ok(()),
                            }
                        }
                        let mut param_json: PWSTR = PWSTR::null();
                        if args_ref.ParameterObjectAsJson(&mut param_json).is_ok()
                            && !param_json.is_null()
                        {
                            let json_str = param_json.to_string().unwrap_or_default();
                            if let Ok(v) = serde_json::from_str::<Value>(&json_str) {
                                match frame.as_deref() {
                                    Some(fid) => super::ingest_cdp_event_in_frame(
                                        &self.event_name,
                                        &v,
                                        Some(fid),
                                    ),
                                    None => super::ingest_cdp_event(&self.event_name, &v),
                                }
                            }
                            windows::Win32::System::Com::CoTaskMemFree(Some(
                                param_json.as_ptr() as *const _
                            ));
                        }
                    }
                }
                Ok(())
            }
        }

        // `with_webview` hands the closure to the UI thread and returns
        // immediately, so its Ok() says nothing about whether the subscription
        // actually happened. Report the real outcome back over a channel.
        let (tx, rx) = std::sync::mpsc::channel::<Result<usize, String>>();
        let wanted: Vec<&'static str> = events.to_vec();
        let wanted_len = wanted.len();
        let subscribe_result = window.with_webview(move |wv| unsafe {
            let controller = wv.controller();
            let Ok(core) = controller.CoreWebView2() else {
                let _ = tx.send(Err("CoreWebView2 unavailable".into()));
                return;
            };
            let core: ICoreWebView2 = core;

            let mut ok_count = 0usize;
            let mut last_err = String::new();
            for ev in &wanted {
                let name = HSTRING::from(*ev);
                match core.GetDevToolsProtocolEventReceiver(&name) {
                    Ok(receiver) => {
                        let handler: ICoreWebView2DevToolsProtocolEventReceivedEventHandler =
                            RecordHandler {
                                event_name: (*ev).to_string(),
                            }
                            .into();
                        let mut token = Default::default();
                        if let Err(e) =
                            receiver.add_DevToolsProtocolEventReceived(&handler, &mut token)
                        {
                            last_err = format!("add_DevToolsProtocolEventReceived({ev}): {e:?}");
                            tracing::warn!("[recording/cdp] {}", last_err);
                        } else {
                            ok_count += 1;
                            tracing::debug!("[recording/cdp] subscribed to {}", ev);
                        }
                        // Keep the receiver alive for the daemon's lifetime.
                        // The registration lives on the receiver object; drop it
                        // and the subscription can go with it. Leaking one COM
                        // pointer per event name is the cheapest correct answer
                        // here — these are process-lifetime subscriptions.
                        std::mem::forget(receiver);
                    }
                    Err(e) => {
                        last_err = format!("GetDevToolsProtocolEventReceiver({ev}): {e:?}");
                        tracing::warn!("[recording/cdp] {}", last_err);
                    }
                }
            }
            if ok_count > 0 {
                // Here, where it happened: a caller that already gave up
                // waiting must still see that events are now being delivered.
                SUBSCRIBED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = if ok_count == 0 {
                tx.send(Err(format!("no events subscribed; last error: {last_err}")))
            } else {
                tx.send(Ok(ok_count))
            };
        });

        subscribe_result.map_err(|e| format!("with_webview failed: {e:?}"))?;

        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(n)) => {
                tracing::info!("[recording/cdp] subscribed to {}/{} events", n, wanted_len);
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("timed out waiting for the UI thread to subscribe".into()),
        }
    }

    /// Fire-and-forget CDP call used for `Network.enable` / `Runtime.enable`.
    /// We don't care about the response — only that the domain is turned on
    /// before any page navigation happens.
    async fn call_cdp_oneshot(
        window: &tauri::WebviewWindow,
        method: &str,
        params: &str,
    ) -> Result<(), String> {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2, ICoreWebView2CallDevToolsProtocolMethodCompletedHandler,
            ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl,
        };
        use windows_core::{implement, HRESULT, HSTRING, PCWSTR};

        #[implement(ICoreWebView2CallDevToolsProtocolMethodCompletedHandler)]
        struct NoopHandler;

        impl ICoreWebView2CallDevToolsProtocolMethodCompletedHandler_Impl for NoopHandler_Impl {
            fn Invoke(&self, _ec: HRESULT, _ret: &PCWSTR) -> windows_core::Result<()> {
                Ok(())
            }
        }

        let method_owned = method.to_string();
        let params_owned = params.to_string();
        let res = window.with_webview(move |wv| unsafe {
            let controller = wv.controller();
            let Ok(core) = controller.CoreWebView2() else {
                return;
            };
            let core: ICoreWebView2 = core;
            let handler: ICoreWebView2CallDevToolsProtocolMethodCompletedHandler =
                NoopHandler.into();
            let m = HSTRING::from(method_owned.as_str());
            let p = HSTRING::from(params_owned.as_str());
            let _ = core.CallDevToolsProtocolMethod(&m, &p, &handler);
        });
        res.map_err(|e| format!("with_webview failed: {e:?}"))
    }
}

/// Anything before this is not a wall-clock timestamp (2020-09-13).
const EPOCH_MS_FLOOR: f64 = 1_600_000_000_000.0;

/// Epoch-ms for a CDP event, or `now` when the event only carries a monotonic
/// clock.
///
/// CDP mixes three time bases on these events and they are NOT interchangeable:
/// `Network.wallTime` is epoch **seconds**, `Runtime.Timestamp` is epoch
/// **milliseconds**, and `Network.MonotonicTime` counts seconds from an
/// arbitrary process origin. Treating them alike produced timestamps a
/// thousand-fold off (Runtime) or in 1970 (MonotonicTime), which silently broke
/// `dump-recording --since-ms`. Each candidate is range-checked instead of
/// trusted, and anything implausible falls back to capture time — the daemon
/// ingests events live, so that's accurate to within milliseconds.
fn plausible_epoch_ms(v: f64) -> Option<u64> {
    // One day of slack for clock skew; anything beyond that isn't a wall clock.
    let ceiling = now_ms() as f64 + 86_400_000.0;
    if v >= EPOCH_MS_FLOOR && v <= ceiling {
        Some(v as u64)
    } else {
        None
    }
}

pub(crate) fn cdp_event_ts_ms(params: &Value) -> u64 {
    // `wallTime` is epoch seconds when present (Network.requestWillBeSent).
    if let Some(t) = params.get("wallTime").and_then(|x| x.as_f64()) {
        if let Some(ms) = plausible_epoch_ms(t * 1000.0) {
            return ms;
        }
    }
    // `Runtime.Timestamp` is already epoch ms. `Network.MonotonicTime` is not a
    // wall clock at all and gets rejected here, falling through to now.
    if let Some(t) = params.get("timestamp").and_then(|x| x.as_f64()) {
        if let Some(ms) = plausible_epoch_ms(t) {
            return ms;
        }
    }
    now_ms()
}

/// Route one page-session CDP event into every daemon-side buffer that wants
/// it. Engine-neutral: WebView2's event handler and the real Chrome engine's
/// WebSocket reader both call this with the event's JSON params.
pub(crate) fn ingest_cdp_event(event: &str, params: &Value) {
    ingest_inner(event, params, None);
}

/// As [`ingest_cdp_event`], for an event from another target's session (an
/// out-of-process iframe). Its execution context ids are numbered by that
/// renderer, so they say nothing in the page's context→frame map; console
/// lines are tagged with `frame_id` directly.
pub(crate) fn ingest_cdp_event_in_frame(event: &str, params: &Value, frame_id: Option<&str>) {
    ingest_inner(event, params, Some(frame_id));
}

fn ingest_inner(event: &str, params: &Value, frame_override: Option<Option<&str>>) {
    let ts_ms = cdp_event_ts_ms(params);
    // The structured request index / navigation timeline sees every event
    // first; it is always on, independent of `--record`.
    crate::netlog::ingest(event, params);
    match event {
        "Network.requestWillBeSent" => {
            // A redirect hop's response never produces `responseReceived` —
            // it only arrives here, attached to the NEXT hop. Dropping it is
            // how `response-headers` lost the `302` in front of a document.
            if let Some(redirect) = params.get("redirectResponse") {
                if let Some(id) = params.get("requestId").and_then(|x| x.as_str()) {
                    let mut entry = crate::response_headers::entry_from_cdp(
                        id,
                        params.get("type").and_then(|x| x.as_str()),
                        redirect,
                        ts_ms,
                    );
                    entry.redirect = Some(true);
                    entry.expects_extra =
                        params.get("redirectHasExtraInfo").and_then(|x| x.as_bool());
                    crate::response_headers::record(entry);
                }
            }
            let req = params.get("request").cloned().unwrap_or(Value::Null);
            push_network(NetworkEvent {
                kind: "request".into(),
                ts_ms,
                request_id: params
                    .get("requestId")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: req.get("url").and_then(|x| x.as_str()).map(String::from),
                method: req.get("method").and_then(|x| x.as_str()).map(String::from),
                status: None,
                mime_type: None,
                error_text: None,
                encoded_data_length: None,
                from_cache: None,
                resource_type: params
                    .get("type")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                duration_ms: None,
                // "who requested this?" — a script initiator carries the call
                // stack, which is the only way to attribute a `<script src>`
                // load without reverse-engineering the bundle.
                initiator: params.get("initiator").cloned(),
            });
        }
        "Network.responseReceived" => {
            let resp = params.get("response").cloned().unwrap_or(Value::Null);
            let request_id = params
                .get("requestId")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let resource_type = params
                .get("type")
                .and_then(|x| x.as_str())
                .map(String::from);

            // The headers were being dropped on the floor here. They are the
            // only place CSP / CORP / cache policy is visible, and for the
            // navigation document there is no other source at all.
            let mut entry = crate::response_headers::entry_from_cdp(
                &request_id,
                resource_type.as_deref(),
                &resp,
                ts_ms,
            );
            entry.expects_extra = params.get("hasExtraInfo").and_then(|x| x.as_bool());
            crate::response_headers::record(entry);

            push_network(NetworkEvent {
                kind: "response".into(),
                ts_ms,
                request_id,
                url: resp.get("url").and_then(|x| x.as_str()).map(String::from),
                method: None,
                status: resp.get("status").and_then(|x| x.as_i64()),
                mime_type: resp
                    .get("mimeType")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                error_text: None,
                encoded_data_length: None,
                from_cache: resp.get("fromDiskCache").and_then(|x| x.as_bool()),
                resource_type,
                duration_ms: None,
                initiator: None,
            });
        }
        // The raw wire headers — repeated `set-cookie` lines survive here, and
        // it arrives even for responses the renderer never surfaces. Order
        // relative to `responseReceived` is not guaranteed, so the store merges
        // whichever lands second.
        "Network.responseReceivedExtraInfo" => {
            if let Some(id) = params.get("requestId").and_then(|x| x.as_str()) {
                crate::response_headers::record_extra_info(
                    id,
                    params.get("statusCode").and_then(|x| x.as_i64()),
                    crate::response_headers::headers_from_cdp(params.get("headers")),
                );
            }
        }
        // Browser-side messages: CSP violations, mixed content, blocked
        // requests. These never reach `Runtime.consoleAPICalled`, and in this
        // environment the page's own `securitypolicyviolation` event does not
        // fire either — so without this a CSP block is completely silent.
        "Log.entryAdded" => {
            let entry = params.get("entry").cloned().unwrap_or(Value::Null);
            let text = entry
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                return;
            }
            let cdp_source = entry
                .get("source")
                .and_then(|x| x.as_str())
                .unwrap_or("other");
            let source = crate::console::log_entry_source(cdp_source, &text);
            if source == "csp"
                && !crate::console::should_record_violation(
                    &crate::console::violation_key(&text),
                    ts_ms,
                )
            {
                return;
            }
            let level = crate::console::normalize_level(
                entry.get("level").and_then(|x| x.as_str()).unwrap_or("log"),
            );
            crate::console::push_with_source(
                level,
                text,
                entry.get("url").and_then(|x| x.as_str()).map(String::from),
                entry.get("lineNumber").and_then(|x| x.as_i64()),
                Some(source.to_string()),
            );
        }
        // Structured CSP data (blockedURL / violatedDirective), when the
        // runtime supports the Audits domain. Best effort — `Log.entryAdded`
        // above already carries the human-readable line.
        "Audits.issueAdded" => {
            if let Some((text, url, line)) = crate::console::csp_issue_text(params) {
                if crate::console::should_record_violation(
                    &crate::console::violation_key(&text),
                    ts_ms,
                ) {
                    crate::console::push_with_source(
                        "error",
                        text,
                        url,
                        line,
                        Some("csp".to_string()),
                    );
                }
            }
        }
        "Network.loadingFailed" => {
            push_network(NetworkEvent {
                kind: "failed".into(),
                ts_ms,
                request_id: params
                    .get("requestId")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: None,
                method: None,
                status: None,
                mime_type: None,
                error_text: params
                    .get("errorText")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                encoded_data_length: None,
                from_cache: None,
                resource_type: params
                    .get("type")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                initiator: None,
                duration_ms: None,
            });
        }
        "Network.loadingFinished" => {
            push_network(NetworkEvent {
                kind: "finished".into(),
                ts_ms,
                request_id: params
                    .get("requestId")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: None,
                method: None,
                status: None,
                mime_type: None,
                error_text: None,
                encoded_data_length: params.get("encodedDataLength").and_then(|x| x.as_i64()),
                from_cache: None,
                initiator: None,
                resource_type: None,
                duration_ms: None,
            });
        }
        "Runtime.consoleAPICalled" => {
            // CDP calls `console.warn` "warning"; normalize so the documented
            // `--level warn` filter actually matches.
            let level = crate::console::normalize_level(
                params.get("type").and_then(|x| x.as_str()).unwrap_or("log"),
            )
            .to_string();
            // Renders object arguments from V8's preview, which marks accessors
            // without invoking them — see `crate::console`.
            let text = crate::console::render_console_args(params.get("args"));
            let (url, line) = console_call_site(params);
            // Feed the always-on console buffer that `console-logs` reads. This
            // happens whether or not `--record` is on; the recording buffer
            // below keeps its own `--record` gate.
            // executionContextId is what tells two same-origin frames apart —
            // on a proxy every frame's url is identical.
            match frame_override {
                Some(frame) => crate::console::push_from_frame(
                    &level,
                    text.clone(),
                    url.clone(),
                    line,
                    frame.map(String::from),
                ),
                None => crate::console::push_with_context(
                    &level,
                    text.clone(),
                    url.clone(),
                    line,
                    params.get("executionContextId").and_then(|x| x.as_i64()),
                ),
            }
            push_console(ConsoleEvent {
                ts_ms,
                level,
                text,
                url,
                line,
            });
        }
        "Runtime.exceptionThrown" => {
            let exc = params
                .get("exceptionDetails")
                .cloned()
                .unwrap_or(Value::Null);
            let text = exc
                .get("text")
                .and_then(|x| x.as_str())
                .map(String::from)
                .unwrap_or_default();
            let url = exc.get("url").and_then(|x| x.as_str()).map(String::from);
            let line = exc.get("lineNumber").and_then(|x| x.as_i64());
            let stack = exc
                .get("stackTrace")
                .and_then(|t| t.get("description"))
                .and_then(|x| x.as_str())
                .map(String::from);
            // Uncaught errors and unhandled rejections both arrive here, so the
            // in-page `error` / `unhandledrejection` listeners are unnecessary
            // on this transport.
            match frame_override {
                Some(frame) => crate::console::push_from_frame(
                    "error",
                    format!("[uncaught] {text}"),
                    url.clone(),
                    line,
                    frame.map(String::from),
                ),
                None => crate::console::push_with_context(
                    "error",
                    format!("[uncaught] {text}"),
                    url.clone(),
                    line,
                    exc.get("executionContextId").and_then(|x| x.as_i64()),
                ),
            }
            push_exception(ExceptionEvent {
                ts_ms,
                text,
                url,
                line,
                stack,
            });
        }
        _ => {}
    }
}

/// Best-effort call site for a `Runtime.consoleAPICalled` event: the topmost
/// frame of the attached stack trace.
fn console_call_site(params: &Value) -> (Option<String>, Option<i64>) {
    let top = params
        .get("stackTrace")
        .and_then(|s| s.get("callFrames"))
        .and_then(|f| f.as_array())
        .and_then(|f| f.first());
    match top {
        Some(frame) => (
            frame
                .get("url")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            frame.get("lineNumber").and_then(|x| x.as_i64()),
        ),
        None => (None, None),
    }
}

#[cfg(test)]
mod ts_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wall_time_seconds_become_epoch_ms() {
        // Network.requestWillBeSent: epoch SECONDS.
        let ts = cdp_event_ts_ms(&json!({ "wallTime": 1_786_124_775.277, "timestamp": 218_483.3 }));
        assert_eq!(ts, 1_786_124_775_277);
    }

    #[test]
    fn runtime_timestamp_is_already_ms() {
        // Runtime.consoleAPICalled: epoch MILLISECONDS — must not be scaled.
        let ts = cdp_event_ts_ms(&json!({ "timestamp": 1_786_124_775_382.0_f64 }));
        assert_eq!(ts, 1_786_124_775_382);
    }

    #[test]
    fn monotonic_time_falls_back_to_now() {
        // Network.responseReceived carries MonotonicTime only — seconds since an
        // arbitrary origin, i.e. not a wall clock. Used to land in 1970.
        let before = now_ms();
        let ts = cdp_event_ts_ms(&json!({ "timestamp": 218_483.3 }));
        assert!(ts >= before, "expected capture time, got {ts}");
    }
}

#[cfg(test)]
mod state_tests {
    use super::state_of;

    #[test]
    fn the_tape_state_is_one_word() {
        assert_eq!(state_of(true, true), "on");
        assert_eq!(state_of(true, false), "on");
        // `record off`: the transport stays armed, but the tape is off.
        assert_eq!(state_of(false, true), "off");
        assert_eq!(state_of(false, false), "never");
    }
}
