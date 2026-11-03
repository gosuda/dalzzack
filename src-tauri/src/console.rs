//! Console capture that the page cannot observe.
//!
//! The original implementation wrapped `console.log` in the page and eagerly
//! serialized every argument with `JSON.stringify`. That walks the object graph
//! and **invokes accessors**, which is exactly the signal `disable-devtool` uses
//! to decide DevTools is open — so every protected site treated dalzzack as a
//! debugger and redirected away. A measurement tool that changes what it
//! measures is worse than no tool.
//!
//! So capture now happens outside the page wherever possible:
//!
//! | Platform | Transport | Wraps `console.log`? |
//! |---|---|---|
//! | Windows | CDP `Runtime.consoleAPICalled` (browser process) | no |
//! | macOS | none — opt in to the in-page hook with `--console-capture` | yes |
//!
//! The CDP transport lands in the daemon-side ring buffer below, which makes
//! `console-logs` a daemon-memory read — so it also answers when the renderer is
//! wedged, which the in-page path never could.
//!
//! The buffer also carries what the page itself cannot report: CDP
//! `Log.entryAdded` and `Audits.issueAdded` deliver CSP violations, mixed
//! content and blocked loads, which never reach `Runtime.consoleAPICalled` and
//! (in this environment) do not fire the page's `securitypolicyviolation` event
//! either. Those entries are tagged with a `source`.

use serde::Serialize;
use serde_json::Value;
// BTreeMap rather than HashMap: `BTreeMap::new` is const, so the buffer can be
// a plain `static Mutex<…>` with no lazy-init wrapper.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default ring size. `start --console-buffer N` changes it per daemon.
const LIVE_CAP: usize = 500;
/// Upper bound for `--console-buffer`: entries are small, but a page that logs
/// in a loop should not be able to grow the daemon without limit.
pub const MAX_CAPACITY: usize = 100_000;

static CAPACITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(LIVE_CAP);

/// Set the ring size (clamped to `1..=MAX_CAPACITY`). Called once at daemon
/// start, before anything is captured.
pub fn set_capacity(n: usize) {
    CAPACITY.store(clamp_capacity(n), std::sync::atomic::Ordering::Relaxed);
}

pub fn clamp_capacity(n: usize) -> usize {
    n.clamp(1, MAX_CAPACITY)
}

pub fn capacity() -> usize {
    CAPACITY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where `console-logs` is getting its data from.
///
/// Always reported to the caller. Which transport answered decides how much the
/// answer is worth — an empty buffer from a live CDP subscription means "the
/// page was quiet", the same empty buffer from a transport that never came up
/// means nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// CDP `Runtime.consoleAPICalled` — Windows.
    Cdp,
    /// In-page hook. Detectable; used only on platforms with no out-of-band
    /// transport, and decided before the WebView is created (the hook has to be
    /// a document-start script, so it cannot be added retroactively).
    InPageFallback,
    /// `start --no-console-capture`.
    Disabled,
    /// The transport this platform should have had did not come up. Distinct
    /// from `Disabled` on purpose: the user asked for console capture and is
    /// not getting it, and an empty buffer here means nothing.
    Unavailable,
    /// Daemon has not finished arming a transport yet.
    #[default]
    Unknown,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Cdp => "cdp",
            Transport::InPageFallback => "in_page_fallback",
            Transport::Disabled => "disabled",
            Transport::Unavailable => "unavailable",
            Transport::Unknown => "unknown",
        }
    }

    /// Does this transport read from the daemon-side buffer (rather than the page)?
    pub fn is_out_of_band(self) -> bool {
        matches!(self, Transport::Cdp)
    }
}

/// One captured console line. Field names match the historical `console-logs`
/// response so existing callers keep working; only the rendering of object
/// arguments changes.
#[derive(Serialize, Debug, Clone)]
pub struct LogEntry {
    pub level: String,
    pub message: String,
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// Which V8 context logged this, and which frame that context belongs to.
    ///
    /// On a page whose frames all share one origin — a proxy serving 25 frames
    /// from `proxy.localhost` — `url` cannot tell two frames apart, so without
    /// this there is no way to say which frame a line came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    /// Where the line came from when it is *not* a page `console.*` call:
    /// `csp` | `security` | `network` | `deprecation` | `violation` | `other`.
    ///
    /// Absent means the page logged it itself. Filterable with
    /// `console-logs --source csp`, which is the fast path to "did a policy
    /// block this?".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

struct Live {
    transport: Transport,
    error: Option<String>,
    entries: VecDeque<LogEntry>,
    dropped: usize,
    /// Violation key → last-seen ms, so the same CSP block reported by both
    /// `Log.entryAdded` and `Audits.issueAdded` appears once.
    recent_violations: BTreeMap<String, u64>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    transport: Transport::Unknown,
    error: None,
    entries: VecDeque::new(),
    dropped: 0,
    recent_violations: BTreeMap::new(),
});

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record which transport the daemon armed.
pub fn set_transport(t: Transport) {
    if let Ok(mut live) = LIVE.lock() {
        live.transport = t;
        if t != Transport::Unknown {
            live.error = None;
        }
    }
}

/// Record why the preferred transport could not be armed. The daemon still
/// degrades to a working transport; this is so the answer says which.
pub fn set_transport_error(t: Transport, error: impl Into<String>) {
    if let Ok(mut live) = LIVE.lock() {
        live.transport = t;
        live.error = Some(error.into());
    }
}

pub fn transport() -> Transport {
    LIVE.lock().map(|l| l.transport).unwrap_or_default()
}

pub fn transport_error() -> Option<String> {
    LIVE.lock().ok().and_then(|l| l.error.clone())
}

/// How many context→frame mappings to remember.
///
/// Deliberately generous and never pruned on context *destruction*: a log line
/// captured at time T must still resolve to the frame it came from, even after
/// that context is gone.
const CONTEXT_FRAME_CAP: usize = 2000;

/// `executionContextId` → CDP `frameId`, readable from a **synchronous**
/// context.
///
/// The authoritative store (`CDP_CONTEXT_STORE`) is behind a tokio `RwLock`,
/// and CDP events are delivered on the WebView2 UI thread with no runtime to
/// await on — so console capture cannot read it. This mirror is written from
/// the same `Runtime.executionContextCreated` handler and read at push time.
static CONTEXT_FRAMES: Mutex<(BTreeMap<i64, String>, VecDeque<i64>)> =
    Mutex::new((BTreeMap::new(), VecDeque::new()));

/// Remember which frame an execution context belongs to.
pub fn note_context_frame(context_id: i64, frame_id: Option<&str>) {
    let Some(frame_id) = frame_id else { return };
    let Ok(mut guard) = CONTEXT_FRAMES.lock() else {
        return;
    };
    let (map, order) = &mut *guard;
    if map.insert(context_id, frame_id.to_string()).is_none() {
        order.push_back(context_id);
    }
    while order.len() > CONTEXT_FRAME_CAP {
        if let Some(old) = order.pop_front() {
            map.remove(&old);
        }
    }
}

/// The frame an execution context belongs to, if we ever saw it announced.
pub fn frame_for_context(context_id: i64) -> Option<String> {
    CONTEXT_FRAMES
        .lock()
        .ok()
        .and_then(|g| g.0.get(&context_id).cloned())
}

#[cfg(test)]
pub fn reset_context_frames() {
    if let Ok(mut g) = CONTEXT_FRAMES.lock() {
        g.0.clear();
        g.1.clear();
    }
}

/// Append a captured line to the daemon-side buffer.
pub fn push(level: &str, message: String, url: Option<String>, line: Option<i64>) {
    push_with_source(level, message, url, line, None);
}

/// As [`push`], tagging where a non-`console.*` line came from.
pub fn push_with_source(
    level: &str,
    message: String,
    url: Option<String>,
    line: Option<i64>,
    source: Option<String>,
) {
    push_entry(level, message, url, line, source, None);
}

/// As [`push`], recording which execution context (and hence which frame) the
/// line came from.
pub fn push_with_context(
    level: &str,
    message: String,
    url: Option<String>,
    line: Option<i64>,
    context_id: Option<i64>,
) {
    push_entry(level, message, url, line, None, context_id);
}

/// A line from a target whose context ids are its own (an out-of-process
/// iframe's session): the frame is known directly, and its context id must not
/// be looked up in the page's map, where the same number means another frame.
pub fn push_from_frame(
    level: &str,
    message: String,
    url: Option<String>,
    line: Option<i64>,
    frame_id: Option<String>,
) {
    push_resolved(level, message, url, line, None, None, frame_id);
}

fn push_entry(
    level: &str,
    message: String,
    url: Option<String>,
    line: Option<i64>,
    source: Option<String>,
    context_id: Option<i64>,
) {
    // Resolve the frame BEFORE taking the LIVE lock — two independent mutexes,
    // always in this order, so they can never deadlock against each other.
    let frame_id = context_id.and_then(frame_for_context);
    push_resolved(level, message, url, line, source, context_id, frame_id);
}

fn push_resolved(
    level: &str,
    message: String,
    url: Option<String>,
    line: Option<i64>,
    source: Option<String>,
    context_id: Option<i64>,
    frame_id: Option<String>,
) {
    let Ok(mut live) = LIVE.lock() else { return };
    if live.transport == Transport::Disabled {
        return;
    }
    let cap = capacity();
    while live.entries.len() >= cap {
        live.entries.pop_front();
        live.dropped += 1;
    }
    live.entries.push_back(LogEntry {
        level: level.to_string(),
        message,
        ts: now_ms(),
        url,
        line,
        source,
        context_id,
        frame_id,
    });
}

#[cfg(test)]
mod context_frame_tests {
    use super::*;

    #[test]
    fn a_log_line_resolves_to_the_frame_that_logged_it() {
        reset_context_frames();
        note_context_frame(170, Some("FRAME-A"));
        note_context_frame(171, Some("FRAME-B"));
        // A context announced without a frame id is not invented.
        note_context_frame(172, None);

        assert_eq!(frame_for_context(170).as_deref(), Some("FRAME-A"));
        assert_eq!(frame_for_context(171).as_deref(), Some("FRAME-B"));
        assert_eq!(frame_for_context(172), None);
        assert_eq!(frame_for_context(999), None);

        // A destroyed context must still resolve: the line was logged before
        // the frame went away, and that history is the answer being asked for.
        note_context_frame(170, Some("FRAME-A"));
        assert_eq!(frame_for_context(170).as_deref(), Some("FRAME-A"));
        reset_context_frames();
    }
}

/// Classify a CDP `Log.entryAdded` source into the label `console-logs` reports.
///
/// CSP violations arrive as `source: "security"` with a `Refused to …` text, so
/// they are promoted to their own `csp` label — that is the thing people
/// actually filter for.
pub fn log_entry_source(cdp_source: &str, text: &str) -> &'static str {
    if is_csp_text(text) {
        return "csp";
    }
    match cdp_source {
        "security" => "security",
        "network" => "network",
        "deprecation" => "deprecation",
        "violation" => "violation",
        "javascript" => "javascript",
        _ => "other",
    }
}

fn is_csp_text(text: &str) -> bool {
    text.contains("Content Security Policy") || text.contains("Content-Security-Policy")
}

/// Build a stable key for a CSP violation so the same block reported through
/// two channels collapses to one line.
///
/// One violation produces up to three differently-worded messages — the engine's
/// `Connecting to '…' violates …`, the Audits rendering, and a subsystem note
/// like `Fetch API cannot load …`. Only two things are stable across all of
/// them: the blocked URL, and the **directive name** (`connect-src`). The
/// directive *value* is not — the engine inlines the whole allow-list, and
/// splitting it on `.` truncates mid-hostname.
///
/// Deliberately tolerant: the engine's wording can change between versions, and
/// a key that fails to parse must degrade to "keep the entry" rather than drop
/// it. Worst case is a duplicate; losing the only record of a block is not an
/// acceptable failure mode.
pub fn violation_key(text: &str) -> String {
    // The engine quotes the URL (`'https://…'`) while the Audits rendering does
    // not, so strip the punctuation before comparing — otherwise the two
    // channels key differently and the dedup silently does nothing.
    const PUNCT: [char; 6] = ['\'', '"', '(', ')', ',', '.'];
    let url = text
        .split_whitespace()
        .map(|w| w.trim_matches(PUNCT))
        .find(|w| w.starts_with("http://") || w.starts_with("https://"))
        .unwrap_or("")
        .to_string();
    let directive = text
        .split("directive:")
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_start_matches('"')
        // The directive NAME only. Everything after it is the allow-list, which
        // each message renders differently (or truncates).
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    if url.is_empty() && directive.is_empty() {
        // Nothing reliable to key on — fall back to the whole line so the
        // entry is still recorded (and only an exact repeat is suppressed).
        return text.to_string();
    }
    format!("{url}|{directive}")
}

/// Dedup window for a violation reported through both `Log` and `Audits`.
const VIOLATION_DEDUP_MS: u64 = 2000;

/// Should this violation be recorded, or did the other channel already report it?
pub fn should_record_violation(key: &str, ts_ms: u64) -> bool {
    let Ok(mut live) = LIVE.lock() else {
        return true;
    };
    let fresh = |seen: u64| ts_ms.saturating_sub(seen) < VIOLATION_DEDUP_MS;

    // A message that names the URL but not the directive (`Fetch API cannot
    // load …`) is the same event as one that names both, so it matches any
    // recent violation on that URL.
    if let Some(url) = key.strip_suffix('|') {
        if !url.is_empty() {
            let prefix = format!("{url}|");
            if live
                .recent_violations
                .iter()
                .any(|(k, &seen)| k.starts_with(&prefix) && fresh(seen))
            {
                return false;
            }
        }
    } else if let Some(&seen) = live.recent_violations.get(key) {
        if fresh(seen) {
            return false;
        }
    }
    // Bound the map: a page can violate a policy thousands of times.
    if live.recent_violations.len() > 256 {
        live.recent_violations
            .retain(|_, seen| ts_ms.saturating_sub(*seen) < VIOLATION_DEDUP_MS);
    }
    live.recent_violations.insert(key.to_string(), ts_ms);
    true
}

/// Render a CDP `Audits.issueAdded` CSP issue as one line, with its location.
///
/// Returns `None` for any other issue kind — this is not a general audits feed.
pub fn csp_issue_text(params: &Value) -> Option<(String, Option<String>, Option<i64>)> {
    let issue = params.get("issue")?;
    if issue.get("code").and_then(|c| c.as_str())? != "ContentSecurityPolicyIssue" {
        return None;
    }
    let d = issue
        .get("details")?
        .get("contentSecurityPolicyIssueDetails")?;

    let kind = d
        .get("contentSecurityPolicyViolationType")
        .and_then(|x| x.as_str())
        .unwrap_or("kCSPViolation");
    let directive = d
        .get("violatedDirective")
        .and_then(|x| x.as_str())
        .unwrap_or("(unknown directive)");
    let blocked = d
        .get("blockedURL")
        .and_then(|x| x.as_str())
        .unwrap_or("(inline)");

    let loc = d.get("sourceCodeLocation");
    let url = loc
        .and_then(|l| l.get("url"))
        .and_then(|x| x.as_str())
        // An empty `url` is not a location; reporting `"url": ""` reads as a bug.
        .filter(|s| !s.is_empty())
        .map(String::from);
    let line = loc
        .and_then(|l| l.get("lineNumber"))
        .and_then(|x| x.as_i64());

    Some((
        format!(
            "[csp] {kind}: {blocked} blocked by Content Security Policy directive: {directive}"
        ),
        url,
        line,
    ))
}

/// Remove the startup self-test entry, reporting whether it had landed.
///
/// The probe proves capture works end to end; leaving it in the buffer would
/// put dalzzack's own noise into the user's evidence.
pub fn consume_probe(marker: &str) -> bool {
    let Ok(mut live) = LIVE.lock() else {
        return false;
    };
    let before = live.entries.len();
    live.entries.retain(|e| !e.message.contains(marker));
    before != live.entries.len()
}

/// Read back the buffer, newest `max` entries last.
///
/// `source` filters by origin: `csp` for policy violations, `console` for the
/// page's own `console.*` calls, or any other tag from [`log_entry_source`].
pub fn snapshot(
    level: Option<&str>,
    source: Option<&str>,
    max: usize,
    clear: bool,
) -> (Vec<LogEntry>, usize) {
    let filter = Filter {
        level,
        source,
        ..Default::default()
    };
    let (out, _, dropped) = snapshot_filtered(&filter, max, clear);
    (out, dropped)
}

/// What `console-logs` narrows the buffer to. All conditions must hold.
#[derive(Default)]
pub struct Filter<'a> {
    pub level: Option<&'a str>,
    pub source: Option<&'a str>,
    /// Entries from these frame ids. An entry without a `frame_id` counts as
    /// the top document's (network and policy messages carry none).
    pub frames: Option<FrameSet<'a>>,
    /// Substring of the message, case-insensitive.
    pub text: Option<&'a str>,
}

pub struct FrameSet<'a> {
    pub ids: Vec<&'a str>,
    /// Also match entries that carry no frame id (`--frame top`).
    pub include_untagged: bool,
}

impl Filter<'_> {
    fn matches(&self, e: &LogEntry) -> bool {
        self.level.is_none_or(|l| e.level == l)
            && self.source.is_none_or(|s| match e.source.as_deref() {
                // Untagged entries are the page's own console calls.
                None => s == "console",
                Some(tag) => tag == s,
            })
            && self
                .frames
                .as_ref()
                .is_none_or(|f| match e.frame_id.as_deref() {
                    Some(id) => f.ids.iter().any(|want| want.eq_ignore_ascii_case(id)),
                    None => f.include_untagged,
                })
            && self
                .text
                .is_none_or(|t| e.message.to_lowercase().contains(&t.to_lowercase()))
    }
}

/// Read back the buffer through `filter`: `(newest max matches, how many
/// matched in all, how many the ring has dropped)`.
pub fn snapshot_filtered(
    filter: &Filter,
    max: usize,
    clear: bool,
) -> (Vec<LogEntry>, usize, usize) {
    let Ok(mut live) = LIVE.lock() else {
        return (Vec::new(), 0, 0);
    };
    let filtered: Vec<LogEntry> = live
        .entries
        .iter()
        .filter(|e| filter.matches(e))
        .cloned()
        .collect();
    let matched = filtered.len();
    let start = filtered.len().saturating_sub(max);
    let out = filtered[start..].to_vec();
    let dropped = live.dropped;
    if clear {
        live.entries.clear();
        live.dropped = 0;
        live.recent_violations.clear();
    }
    (out, matched, dropped)
}

/// Render a CDP `RemoteObject` (a `Runtime.consoleAPICalled` argument).
///
/// The load-bearing property: V8's object *preview* marks accessor properties
/// as `type: "accessor"` and **does not call them**. That is what lets this
/// path report the contents of a logged object without the page being able to
/// tell that anything read it.
pub fn render_remote_object(o: &Value) -> String {
    if let Some(v) = o.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(u) = o.get("unserializableValue").and_then(|x| x.as_str()) {
        return u.to_string();
    }
    match o.get("type").and_then(|t| t.as_str()) {
        Some("undefined") => return "undefined".to_string(),
        Some("function") => {
            let d = o.get("description").and_then(|x| x.as_str()).unwrap_or("");
            return if d.is_empty() {
                "function".to_string()
            } else {
                d.lines().next().unwrap_or(d).to_string()
            };
        }
        _ => {}
    }
    match o.get("subtype").and_then(|x| x.as_str()) {
        Some("null") => return "null".to_string(),
        // An Error's `description` is already the full stack. Appending its
        // preview would just repeat `message` and `stack` after it.
        Some("error") => {
            if let Some(d) = o.get("description").and_then(|x| x.as_str()) {
                return d.to_string();
            }
        }
        _ => {}
    }
    if let Some(p) = o.get("preview") {
        return render_preview(p);
    }
    o.get("description")
        .and_then(|x| x.as_str())
        .or_else(|| o.get("className").and_then(|x| x.as_str()))
        .map(String::from)
        .unwrap_or_else(|| o.to_string())
}

fn render_preview(p: &Value) -> String {
    let is_array = p.get("subtype").and_then(|x| x.as_str()) == Some("array");
    let desc = p.get("description").and_then(|x| x.as_str()).unwrap_or("");

    let mut parts: Vec<String> = Vec::new();
    if let Some(props) = p.get("properties").and_then(|x| x.as_array()) {
        for pr in props {
            let name = pr.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let ty = pr.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let rendered = if ty == "accessor" {
                // Never resolved. Reading it is what tripped anti-debug libraries.
                "[getter]".to_string()
            } else if let Some(sub) = pr.get("valuePreview") {
                render_preview(sub)
            } else {
                let v = pr.get("value").and_then(|x| x.as_str()).unwrap_or("");
                match ty {
                    "string" => format!("\"{v}\""),
                    // Functions arrive with an empty `value`; without this a
                    // method renders as `name: ` and reads like a bug.
                    _ if v.is_empty() => {
                        if ty.is_empty() {
                            "…".to_string()
                        } else {
                            ty.to_string()
                        }
                    }
                    _ => v.to_string(),
                }
            };
            parts.push(if is_array {
                rendered
            } else {
                format!("{name}: {rendered}")
            });
        }
    }
    if p.get("overflow").and_then(|x| x.as_bool()).unwrap_or(false) {
        parts.push("…".to_string());
    }

    if parts.is_empty() {
        return desc.to_string();
    }
    let body = parts.join(", ");
    if is_array {
        format!("[{body}]")
    } else if desc.is_empty() || desc == "Object" {
        format!("{{{body}}}")
    } else {
        format!("{desc} {{{body}}}")
    }
}

/// Map a transport's own level name onto the five levels the CLI documents.
///
/// CDP reports `console.warn` as `"warning"` and has a dozen more types
/// (`assert`, `trace`, `table`, …). Storing those verbatim would silently break
/// `console-logs --level warn`, which only accepts the documented five.
pub fn normalize_level(raw: &str) -> &'static str {
    match raw {
        "info" => "info",
        "warn" | "warning" => "warn",
        "error" | "assert" => "error",
        "debug" => "debug",
        _ => "log",
    }
}

/// Join rendered `Runtime.consoleAPICalled` arguments into one message.
pub fn render_console_args(args: Option<&Value>) -> String {
    args.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .map(render_remote_object)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(message: &str, frame_id: Option<&str>, source: Option<&str>) -> LogEntry {
        LogEntry {
            level: "log".into(),
            message: message.into(),
            ts: 0,
            url: None,
            line: None,
            source: source.map(String::from),
            context_id: None,
            frame_id: frame_id.map(String::from),
        }
    }

    #[test]
    fn filters_by_frame_and_text() {
        let top = entry("page says hi", Some("TOP"), None);
        let child = entry("Turnstile ERROR 600010", Some("CHILD"), None);
        let net = entry("Failed to load resource", None, Some("network"));
        let f = Filter {
            frames: Some(FrameSet {
                ids: vec!["child"],
                include_untagged: false,
            }),
            ..Default::default()
        };
        // Frame ids compare case-insensitively; untagged lines are not a frame's.
        assert!(f.matches(&child) && !f.matches(&top) && !f.matches(&net));
        // `--frame top` keeps the top document's lines and the untagged ones.
        let t = Filter {
            frames: Some(FrameSet {
                ids: vec!["TOP"],
                include_untagged: true,
            }),
            ..Default::default()
        };
        assert!(t.matches(&top) && t.matches(&net) && !t.matches(&child));
        let x = Filter {
            text: Some("error 6000"),
            ..Default::default()
        };
        assert!(x.matches(&child) && !x.matches(&top));
    }

    #[test]
    fn capacity_is_clamped() {
        // Pure: other tests push into the global ring concurrently.
        assert_eq!(clamp_capacity(0), 1);
        assert_eq!(clamp_capacity(10_000_000), MAX_CAPACITY);
        assert_eq!(clamp_capacity(2000), 2000);
        assert_eq!(capacity(), LIVE_CAP, "the default is 500");
    }

    #[test]
    fn primitives_render_plainly() {
        assert_eq!(
            render_remote_object(&json!({"type":"string","value":"hi"})),
            "hi"
        );
        assert_eq!(
            render_remote_object(&json!({"type":"number","value":42})),
            "42"
        );
        assert_eq!(
            render_remote_object(&json!({"type":"boolean","value":true})),
            "true"
        );
        assert_eq!(
            render_remote_object(&json!({"type":"undefined"})),
            "undefined"
        );
        assert_eq!(
            render_remote_object(&json!({"type":"object","subtype":"null","value":null})),
            "null"
        );
    }

    #[test]
    fn unserializable_values_survive() {
        assert_eq!(
            render_remote_object(&json!({"type":"number","unserializableValue":"NaN"})),
            "NaN"
        );
    }

    #[test]
    fn accessors_are_reported_not_resolved() {
        // This is the whole point: a getter-trapped property must show up as a
        // placeholder. If this ever renders a value, something invoked the
        // getter and the page can detect us.
        let obj = json!({
            "type": "object",
            "className": "Object",
            "description": "Object",
            "preview": {
                "type": "object",
                "description": "Object",
                "overflow": false,
                "properties": [
                    {"name": "a", "type": "number", "value": "1"},
                    {"name": "trap", "type": "accessor"}
                ]
            }
        });
        assert_eq!(render_remote_object(&obj), "{a: 1, trap: [getter]}");
    }

    #[test]
    fn dom_nodes_keep_their_tag() {
        let node = json!({
            "type": "object",
            "subtype": "node",
            "className": "HTMLDivElement",
            "description": "div",
            "preview": {
                "type": "object",
                "subtype": "node",
                "description": "div",
                "overflow": false,
                "properties": [{"name": "id", "type": "accessor"}]
            }
        });
        assert_eq!(render_remote_object(&node), "div {id: [getter]}");
    }

    #[test]
    fn errors_render_as_their_stack_not_a_property_dump() {
        // The description already IS the stack; appending the preview would
        // repeat `message` and `stack` after it.
        let err = json!({
            "type": "object",
            "subtype": "error",
            "className": "Error",
            "description": "Error: boom\n    at <anonymous>:1:1",
            "preview": {
                "description": "Error: boom",
                "overflow": false,
                "properties": [
                    {"name": "stack", "type": "string", "value": "Error: boom"},
                    {"name": "message", "type": "string", "value": "boom"}
                ]
            }
        });
        assert_eq!(
            render_remote_object(&err),
            "Error: boom\n    at <anonymous>:1:1"
        );
    }

    #[test]
    fn function_properties_are_named_not_blank() {
        // CDP sends functions with an empty `value`; rendering that verbatim
        // produced `{toJSON: }`, which reads like a bug in the tool.
        let obj = json!({
            "type": "object",
            "description": "Object",
            "preview": {
                "description": "Object",
                "overflow": false,
                "properties": [{"name": "toJSON", "type": "function", "value": ""}]
            }
        });
        assert_eq!(render_remote_object(&obj), "{toJSON: function}");
    }

    #[test]
    fn cdp_level_names_map_onto_the_documented_five() {
        // `console.warn` arrives as "warning"; storing that verbatim silently
        // broke `console-logs --level warn`.
        assert_eq!(normalize_level("warning"), "warn");
        assert_eq!(normalize_level("warn"), "warn");
        assert_eq!(normalize_level("assert"), "error");
        assert_eq!(normalize_level("error"), "error");
        assert_eq!(normalize_level("info"), "info");
        assert_eq!(normalize_level("debug"), "debug");
        // Anything else (trace, table, dir, …) degrades to `log` rather than
        // becoming an unfilterable level.
        assert_eq!(normalize_level("trace"), "log");
        assert_eq!(normalize_level("table"), "log");
    }

    #[test]
    fn arrays_render_as_lists() {
        let arr = json!({
            "type": "object",
            "subtype": "array",
            "description": "Array(2)",
            "preview": {
                "type": "object",
                "subtype": "array",
                "description": "Array(2)",
                "overflow": false,
                "properties": [
                    {"name": "0", "type": "number", "value": "1"},
                    {"name": "1", "type": "string", "value": "x"}
                ]
            }
        });
        assert_eq!(render_remote_object(&arr), "[1, \"x\"]");
    }

    #[test]
    fn overflow_is_marked_rather_than_silently_truncated() {
        let obj = json!({
            "type": "object",
            "description": "Object",
            "preview": {
                "description": "Object",
                "overflow": true,
                "properties": [{"name": "a", "type": "number", "value": "1"}]
            }
        });
        assert_eq!(render_remote_object(&obj), "{a: 1, …}");
    }

    #[test]
    fn falls_back_to_description_when_no_preview() {
        // No preview available is a fidelity loss, not a lie — say "Object",
        // don't invent contents.
        let obj = json!({"type":"object","className":"Object","description":"Object"});
        assert_eq!(render_remote_object(&obj), "Object");
    }

    #[test]
    fn multiple_arguments_are_space_joined() {
        let args = json!([
            {"type":"string","value":"count:"},
            {"type":"number","value":3}
        ]);
        assert_eq!(render_console_args(Some(&args)), "count: 3");
    }

    #[test]
    fn csp_violations_are_labelled_regardless_of_cdp_source() {
        let refused = "Refused to load the image 'https://example.com/x.png' because it \
                       violates the following Content Security Policy directive: \"default-src 'none'\".";
        assert_eq!(log_entry_source("security", refused), "csp");
        // The label is driven by the text, so a runtime that files the same
        // violation under a different source still lands in `--source csp`.
        assert_eq!(log_entry_source("other", refused), "csp");
        // Non-CSP security messages keep their own label.
        assert_eq!(
            log_entry_source("security", "Mixed content blocked"),
            "security"
        );
        assert_eq!(
            log_entry_source("network", "Failed to load resource"),
            "network"
        );
        assert_eq!(
            log_entry_source("deprecation", "X is deprecated"),
            "deprecation"
        );
        assert_eq!(log_entry_source("something-new", "hello"), "other");
    }

    #[test]
    fn violation_key_pairs_the_blocked_url_with_the_directive_name() {
        let refused = "Refused to load the image 'https://example.com/x.png' because it \
                       violates the following Content Security Policy directive: \"default-src 'none'\".";
        let key = violation_key(refused);
        assert_eq!(key, "https://example.com/x.png|default-src");

        // The Audits rendering of the same violation must key identically, or
        // the dedup does nothing.
        let audits = "[csp] kURLViolation: https://example.com/x.png blocked by Content \
                      Security Policy directive: default-src 'none'";
        assert_eq!(violation_key(refused), violation_key(audits));
    }

    #[test]
    fn violation_key_survives_an_inlined_allow_list() {
        // Observed on github.com: the engine inlines the entire connect-src
        // allow-list. Keying on the directive *value* truncated it mid-hostname
        // (`connect-src 'self' uploads.github`) and the dedup silently did
        // nothing — three lines for one violation.
        let engine = "Connecting to 'https://example.com/csp-probe' violates the following \
                      Content Security Policy directive: \"connect-src 'self' uploads.github.com \
                      api.github.com\". The action has been blocked.";
        let audits = "[csp] kURLViolation: https://example.com/csp-probe blocked by Content \
                      Security Policy directive: connect-src";
        assert_eq!(
            violation_key(engine),
            "https://example.com/csp-probe|connect-src"
        );
        assert_eq!(violation_key(engine), violation_key(audits));
    }

    #[test]
    fn a_message_without_a_directive_keys_on_the_url_alone() {
        // `Fetch API cannot load …` is the same event, worded by a different
        // subsystem and naming no directive.
        let fetch_msg = "Fetch API cannot load https://example.com/csp-probe. Refused to \
                         connect because it violates the document's Content Security Policy.";
        assert_eq!(violation_key(fetch_msg), "https://example.com/csp-probe|");
    }

    #[test]
    fn unparseable_violation_text_still_gets_recorded() {
        // Engine wording changes between versions. A key we cannot build must
        // degrade to "keep the entry" — a duplicate is survivable, losing the
        // only record of a blocked request is not.
        let odd = "some future wording with no url and no directive";
        assert_eq!(violation_key(odd), odd);
    }

    #[test]
    fn audits_csp_issue_renders_with_its_location() {
        let params = json!({
            "issue": {
                "code": "ContentSecurityPolicyIssue",
                "details": { "contentSecurityPolicyIssueDetails": {
                    "blockedURL": "https://example.com/x.png",
                    "violatedDirective": "default-src 'none'",
                    "contentSecurityPolicyViolationType": "kURLViolation",
                    "sourceCodeLocation": { "url": "https://site.test/", "lineNumber": 12 }
                }}
            }
        });
        let (text, url, line) = csp_issue_text(&params).expect("csp issue");
        assert!(text.contains("https://example.com/x.png"), "{text}");
        assert!(text.contains("default-src 'none'"), "{text}");
        assert!(text.contains("kURLViolation"), "{text}");
        assert_eq!(url.as_deref(), Some("https://site.test/"));
        assert_eq!(line, Some(12));
    }

    #[test]
    fn audits_ignores_non_csp_issues() {
        // This is not a general audits feed; anything else must be dropped
        // rather than rendered as a mystery console line.
        assert!(csp_issue_text(&json!({"issue": {"code": "MixedContentIssue"}})).is_none());
        assert!(csp_issue_text(&json!({})).is_none());
    }

    #[test]
    fn transport_labels_are_stable() {
        assert_eq!(Transport::Cdp.as_str(), "cdp");
        assert_eq!(Transport::InPageFallback.as_str(), "in_page_fallback");
        assert_eq!(Transport::Disabled.as_str(), "disabled");
        assert!(Transport::Cdp.is_out_of_band());
        assert!(!Transport::InPageFallback.is_out_of_band());
    }

    // The live buffer is process-global, so the tests that mutate it share one
    // test function rather than racing each other under the parallel harness.
    #[test]
    fn live_buffer_behaviour() {
        set_transport(Transport::Cdp);
        snapshot(None, None, 0, true);
        push("log", "a".into(), None, None);
        push("error", "b".into(), None, None);
        push("log", "c".into(), None, None);

        let (all, _) = snapshot(None, None, 100, false);
        assert_eq!(all.len(), 3);

        let (errs, _) = snapshot(Some("error"), None, 100, false);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].message, "b");

        // `max` keeps the NEWEST entries — the tail is what a caller wants.
        let (tail, _) = snapshot(None, None, 2, true);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].message, "b");
        assert_eq!(tail[1].message, "c");

        let (after_clear, _) = snapshot(None, None, 100, false);
        assert!(after_clear.is_empty());

        // --- source filtering ------------------------------------------
        push("log", "page said this".into(), None, None);
        push_with_source(
            "error",
            "csp blocked that".into(),
            None,
            None,
            Some("csp".into()),
        );
        push_with_source(
            "warn",
            "mixed content".into(),
            None,
            None,
            Some("security".into()),
        );

        let (csp, _) = snapshot(None, Some("csp"), 100, false);
        assert_eq!(csp.len(), 1);
        assert_eq!(csp[0].message, "csp blocked that");
        // Untagged entries are the page's own console calls.
        let (page, _) = snapshot(None, Some("console"), 100, false);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].message, "page said this");
        // Level and source compose.
        let (none, _) = snapshot(Some("log"), Some("csp"), 100, true);
        assert!(none.is_empty());

        // --- violation dedup -------------------------------------------
        // The same block arrives from Log.entryAdded and Audits.issueAdded.
        assert!(should_record_violation("https://u/x|connect-src", 1_000));
        assert!(!should_record_violation("https://u/x|connect-src", 1_500));
        // The directive-less wording of the same event collapses into it.
        assert!(!should_record_violation("https://u/x|", 1_600));
        // …but a genuine repeat later is not suppressed forever.
        assert!(should_record_violation(
            "https://u/x|connect-src",
            1_000 + VIOLATION_DEDUP_MS + 1
        ));
        // A different URL is never suppressed.
        assert!(should_record_violation(
            "https://other/y|connect-src",
            1_500
        ));
        // Nor is a different directive on the same URL.
        assert!(should_record_violation("https://u/x|img-src", 1_600));

        snapshot(None, None, 0, true);

        // `--no-console-capture` must not accumulate anything at all.
        set_transport(Transport::Disabled);
        push("log", "should not be kept".into(), None, None);
        let (out, _) = snapshot(None, None, 100, true);
        assert!(out.is_empty());

        // An arming failure is recorded alongside the degraded transport.
        set_transport_error(Transport::InPageFallback, "CDP subscribe failed");
        assert_eq!(transport(), Transport::InPageFallback);
        assert_eq!(transport_error().as_deref(), Some("CDP subscribe failed"));

        set_transport(Transport::Unknown);
    }
}
