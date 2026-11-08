//! Main-frame loads that failed, on every engine.
//!
//! `navigate` used to report `completed` whenever its wait ran out, so a page
//! that never loaded (a refused certificate, a name that does not resolve)
//! read as a success while the window still showed something else. This store
//! answers one question for `navigate`, `wait-navigation`, and
//! `failed-requests`: did a top-level document fail to load?
//!
//! Two sources feed it. A CDP engine (WebView2, Chrome) reports the failure in
//! its network stream, and [`crate::netlog`] records it here. WKWebView has no
//! network stream, so its navigation delegate records the failure
//! (`platform::wk_delegate`).

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// How many failures are kept. A test run that needs more uses `nav-log`.
const KEEP: usize = 50;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Failure {
    pub seq: u64,
    pub ts_ms: u64,
    /// The WebKit navigation (`WKNavigation` pointer). 0 from a CDP engine.
    pub navigation: usize,
    pub failing_url: Option<String>,
    /// `NSURLErrorDomain` and the like (WebKit), or `None` (CDP).
    pub error_domain: Option<String>,
    pub error_code: Option<i64>,
    /// `localizedDescription` (WebKit) or `errorText` such as
    /// `net::ERR_CERT_AUTHORITY_INVALID` (CDP).
    pub message: String,
    /// What Security.framework said about the certificate, when known.
    pub trust_error: Option<Value>,
    /// A trust evaluation for this failure is still running.
    pub trust_pending: bool,
    /// Another load replaced this one, or it became a download.
    pub canceled: bool,
}

impl Failure {
    /// The fields a failed `navigate` or `wait-navigation` result carries.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "failing_url": self.failing_url,
            "message": self.message,
        });
        match (&self.error_domain, self.error_code) {
            (Some(domain), Some(code)) => {
                v["error_domain"] = json!(domain);
                v["error_code"] = json!(code);
            }
            _ => v["error_text"] = json!(self.message),
        }
        if let Some(t) = &self.trust_error {
            v["trust_error"] = t.clone();
        }
        v
    }
}

/// Is a WebKit error a cancellation, not a page that failed to load?
/// `NSURLErrorCancelled` comes when another load replaces this one, and
/// `WebKitErrorFrameLoadInterruptedByPolicyChange` when it became a download.
pub fn webkit_canceled(domain: &str, code: i64) -> bool {
    (domain == "NSURLErrorDomain" && code == -999) || (domain == "WebKitErrorDomain" && code == 102)
}

/// Which command a failure belongs to is decided by sequence numbers, never
/// by the clock: a click and the `wait-navigation` after it can be seconds
/// apart, and a freed `WKNavigation` address can come back for a later load.
#[derive(Default)]
struct Store {
    next: u64,
    items: VecDeque<Failure>,
    /// Failures up to this number were reported by a `navigate` or a
    /// `wait-navigation`, so no later call reports them again.
    reported: u64,
    /// The number when the last command that can start a navigation (a click,
    /// a key, a script, back) began. What came before it is not its doing.
    action: u64,
}

impl Store {
    fn record(&mut self, mut f: Failure) -> u64 {
        self.next += 1;
        f.seq = self.next;
        if self.items.len() == KEEP {
            self.items.pop_front();
        }
        self.items.push_back(f);
        self.next
    }

    fn get_mut(&mut self, seq: u64) -> Option<&mut Failure> {
        self.items.iter_mut().find(|f| f.seq == seq)
    }

    /// The newest failure recorded after `after` that is not a cancellation.
    fn newest_after(&self, after: u64) -> Option<&Failure> {
        self.items
            .iter()
            .rev()
            .find(|f| !f.canceled && f.seq > after)
    }

    /// The failure of one WebKit navigation, recorded after `after`.
    fn for_navigation(&self, navigation: usize, after: u64) -> Option<&Failure> {
        self.items
            .iter()
            .rev()
            .find(|f| navigation != 0 && f.navigation == navigation && f.seq > after && !f.canceled)
    }

    /// What `wait-navigation` reports: the newest failure since the last
    /// action, unless a call already reported it.
    fn unreported(&self) -> Option<&Failure> {
        self.newest_after(self.reported.max(self.action))
    }
}

static STORE: Mutex<Store> = Mutex::new(Store {
    next: 0,
    items: VecDeque::new(),
    reported: 0,
    action: 0,
});

/// True on a WKWebView daemon whose navigation delegate records failures.
static WEBKIT_CAPTURE: AtomicBool = AtomicBool::new(false);

fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Keep a failure. Returns its sequence number.
pub fn record(f: Failure) -> u64 {
    with_store(|s| s.record(f))
}

/// Fill in the certificate reason once the evaluation has finished.
pub fn set_trust_error(seq: u64, trust_error: Option<Value>) {
    with_store(|s| {
        if let Some(f) = s.get_mut(seq) {
            f.trust_error = trust_error;
            f.trust_pending = false;
        }
    })
}

/// The sequence number of the newest failure. Take it before a load starts:
/// only what comes after it can be that load's failure.
pub fn last_seq() -> u64 {
    with_store(|s| s.next)
}

/// The failure of one WebKit navigation, recorded after `after_seq`.
pub fn for_navigation(navigation: usize, after_seq: u64) -> Option<Failure> {
    with_store(|s| s.for_navigation(navigation, after_seq).cloned())
}

/// The newest failure recorded after `after_seq`, whatever its navigation.
pub fn newest_after(after_seq: u64) -> Option<Failure> {
    with_store(|s| s.newest_after(after_seq).cloned())
}

/// How long a failure may wait for its certificate reason, which is evaluated
/// off the main thread.
const TRUST_SETTLE_MS: u64 = 1500;

/// `f`, once its certificate reason is in, or after [`TRUST_SETTLE_MS`].
pub async fn settled(mut f: Failure) -> Failure {
    use tokio::time::{sleep, Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(TRUST_SETTLE_MS);
    while f.trust_pending && Instant::now() < deadline {
        sleep(Duration::from_millis(50)).await;
        let seq = f.seq;
        if let Some(newer) = with_store(|s| s.get_mut(seq).cloned()) {
            f = newer;
        }
    }
    f
}

/// Every failure, oldest first.
pub fn all() -> Vec<Failure> {
    with_store(|s| s.items.iter().cloned().collect())
}

/// The newest load that failed since the last action, unless a `navigate`
/// or a `wait-navigation` already reported it.
pub fn unreported() -> Option<Failure> {
    with_store(|s| s.unreported().cloned())
}

/// Mark every failure so far as reported.
pub fn consume() {
    with_store(|s| s.reported = s.next);
}

/// A command that can start a navigation begins. What failed before it is
/// not its doing.
pub fn note_action() {
    with_store(|s| s.action = s.next);
}

/// Can this tool start a navigation? Then [`note_action`] runs before it.
pub fn starts_navigation(tool: &str) -> bool {
    matches!(
        tool,
        "click"
            | "click_element"
            | "keyboard_input"
            | "fill_form"
            | "execute_js"
            | "diag_exec_js_isolated"
            | "auto_dismiss_modals"
            | "back"
            | "forward"
            | "reload"
    )
}

pub fn set_webkit_capture(on: bool) {
    WEBKIT_CAPTURE.store(on, Ordering::Relaxed);
}

pub fn webkit_capture() -> bool {
    WEBKIT_CAPTURE.load(Ordering::Relaxed)
}

/// What sees a failed main-frame load on this daemon, for `__ping`.
pub fn capture_word() -> &'static str {
    if crate::netlog::is_armed() {
        "netlog"
    } else if webkit_capture() {
        "webkit_delegate"
    } else {
        "unavailable"
    }
}

/// Did this `nav-log` entry fail to load? A canceled load (another navigation
/// replaced it) is not a failure of the page.
pub fn document_failed(nav: &Value) -> bool {
    nav["outcome"] == "failed" && nav["canceled"] != true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(ts_ms: u64, navigation: usize) -> Failure {
        Failure {
            ts_ms,
            navigation,
            failing_url: Some("https://bad.test/".into()),
            message: "net::ERR_CERT_AUTHORITY_INVALID".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_store_keeps_the_newest_failures_only() {
        let mut s = Store::default();
        for i in 0..(KEEP as u64 + 5) {
            s.record(failure(i, 1));
        }
        assert_eq!(s.items.len(), KEEP);
        assert_eq!(s.items.front().map(|f| f.seq), Some(6));
        assert_eq!(s.items.back().map(|f| f.seq), Some(KEEP as u64 + 5));
    }

    #[test]
    fn a_wait_reports_the_newest_failure_since_the_action_once() {
        let mut s = Store::default();
        s.record(failure(100, 1));
        // A click: what failed before it is not its doing.
        s.action = s.next;
        assert!(s.unreported().is_none());
        s.record(failure(200, 2));
        s.record(Failure {
            canceled: true,
            ..failure(300, 3)
        });
        // Seconds may pass between the click and the wait: no clock decides.
        assert_eq!(s.unreported().map(|f| f.navigation), Some(2));
        s.record(failure(400, 4));
        assert_eq!(s.unreported().map(|f| f.navigation), Some(4));
        // Once reported, never again.
        s.reported = s.next;
        assert!(s.unreported().is_none());
    }

    #[test]
    fn a_reused_navigation_address_cannot_bring_back_an_old_failure() {
        let mut s = Store::default();
        s.record(failure(100, 0xbeef));
        // A later load whose WKNavigation got the same address.
        let before_load = s.next;
        assert!(s.for_navigation(0xbeef, before_load).is_none());
        s.record(failure(200, 0xbeef));
        assert_eq!(
            s.for_navigation(0xbeef, before_load).map(|f| f.ts_ms),
            Some(200)
        );
        assert!(s.for_navigation(0, 0).is_none());
    }

    #[test]
    fn the_commands_that_can_start_a_navigation_mark_an_action() {
        for tool in [
            "click",
            "click_element",
            "keyboard_input",
            "execute_js",
            "back",
            "reload",
        ] {
            assert!(starts_navigation(tool), "{tool}");
        }
        for tool in [
            "get_text",
            "describe",
            "screenshot",
            "wait_navigation",
            "navigate",
        ] {
            assert!(!starts_navigation(tool), "{tool}");
        }
    }

    #[test]
    fn a_cdp_failure_reports_its_error_text_and_a_webkit_failure_its_code() {
        let cdp = failure(1, 0).to_json();
        assert_eq!(cdp["error_text"], "net::ERR_CERT_AUTHORITY_INVALID");
        assert!(cdp.get("error_code").is_none());

        let webkit = Failure {
            error_domain: Some("NSURLErrorDomain".into()),
            error_code: Some(-1202),
            message: "The certificate for this server is invalid.".into(),
            trust_error: Some(json!({ "code": -67843, "message": "not trusted" })),
            ..failure(1, 7)
        }
        .to_json();
        assert_eq!(webkit["error_domain"], "NSURLErrorDomain");
        assert_eq!(webkit["error_code"], -1202);
        assert_eq!(webkit["trust_error"]["code"], -67843);
        assert!(webkit.get("error_text").is_none());
    }

    #[test]
    fn cancellations_are_not_page_failures() {
        assert!(webkit_canceled("NSURLErrorDomain", -999));
        assert!(webkit_canceled("WebKitErrorDomain", 102));
        assert!(!webkit_canceled("NSURLErrorDomain", -1202));
        assert!(document_failed(&json!({ "outcome": "failed" })));
        assert!(!document_failed(
            &json!({ "outcome": "failed", "canceled": true })
        ));
        assert!(!document_failed(
            &json!({ "outcome": "response", "status": 200 })
        ));
    }
}
