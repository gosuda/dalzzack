//! Response headers captured from CDP `Network.responseReceived`.
//!
//! # Why this exists
//!
//! Until now there was **no way at all** to see what headers a document was
//! served with:
//!
//! * `dump-recording` carries CDP network events but never stored `headers`,
//!   and needs `--record`.
//! * `get-requests` / `network-log` are built on an in-page `fetch`/XHR hook,
//!   so the **navigation document is invisible to them by construction** — the
//!   hook does not exist yet when the document is fetched.
//!
//! That document's response headers are exactly where CSP, CORP/COEP, cache
//! policy and cookies are decided, so the blind spot sat on top of the most
//! consequential request of the page.
//!
//! # Why capture is always on
//!
//! `Network.responseReceived` fires once, at navigation time. There is no way
//! to ask for it after the fact, so a "turn it on when you need it" design
//! cannot work — by the time you know you need the document's headers, the
//! event is long gone. The domain is therefore armed on every daemon.
//!
//! # Why two rings
//!
//! The navigation document arrives **first**, then hundreds of subresources
//! follow. A single ring buffer would evict the one entry that matters, which
//! would reproduce the original blind spot in a new form. Documents get their
//! own small ring that subresource traffic cannot push them out of.

use serde::Serialize;
use serde_json::Value;
// BTreeMap throughout rather than HashMap: `BTreeMap::new` is const, so the
// store can be a plain `static Mutex<…>` with no lazy-init wrapper.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// Documents are rare and precious; subresources are many and cheap.
const DOCUMENT_CAP: usize = 50;
const ALL_CAP: usize = 500;
/// `responseReceivedExtraInfo` without a matching `responseReceived` would
/// otherwise accumulate forever (aborted requests, preflights).
const PENDING_EXTRA_CAP: usize = 200;

#[derive(Serialize, Debug, Clone, Default)]
pub struct ResponseHeaderEntry {
    pub ts_ms: u64,
    pub request_id: String,
    pub url: String,
    pub status: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// CDP resource type: `Document` | `XHR` | `Fetch` | `Script` | `Stylesheet` | …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_disk_cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_service_worker: Option<bool>,
    /// Header names lowercased, as the engine reports them to the renderer.
    pub headers: BTreeMap<String, String>,
    /// Headers as they came off the wire (`Network.responseReceivedExtraInfo`).
    ///
    /// Present only when that event arrived. It is the more truthful source —
    /// it survives responses the renderer never surfaces and keeps repeated
    /// `set-cookie` lines that the merged view collapses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_headers: Option<BTreeMap<String, String>>,
    /// `true` for a redirect hop (`3xx` with `location`). Its response never
    /// produces `responseReceived`; it is taken from the next hop's
    /// `requestWillBeSent.redirectResponse`, and shares that request id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<bool>,
    /// CDP's `hasExtraInfo` / `redirectHasExtraInfo`. `Some(false)`: no wire
    /// headers will ever come for this response (HSTS internal redirect, cache
    /// hit), so another response's must never be attached to it.
    #[serde(skip)]
    pub expects_extra: Option<bool>,
}

impl ResponseHeaderEntry {
    fn is_document(&self) -> bool {
        self.resource_type.as_deref() == Some("Document")
    }

    /// Can wire headers reported with `status` belong to this entry?
    ///
    /// A revalidated cache hit is `304` on the wire and `200` in
    /// `responseReceived` (documented CDP behaviour), so that pair matches.
    fn accepts_extra(&self, status: Option<i64>) -> bool {
        self.raw_headers.is_none()
            && self.expects_extra != Some(false)
            && status.is_none_or(|s| s == 0 || s == self.status || (s == 304 && self.status == 200))
    }

    /// Identity of one response across the two rings, which hold copies.
    fn same_response(&self, other: &ResponseHeaderEntry) -> bool {
        self.request_id == other.request_id
            && self.ts_ms == other.ts_ms
            && self.status == other.status
            && self.url == other.url
    }
}

/// Build an entry from a CDP `Network.Response` object.
pub fn entry_from_cdp(
    request_id: &str,
    resource_type: Option<&str>,
    resp: &Value,
    ts_ms: u64,
) -> ResponseHeaderEntry {
    let s = |k: &str| {
        resp.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    ResponseHeaderEntry {
        ts_ms,
        request_id: request_id.to_string(),
        url: s("url").unwrap_or_default(),
        status: resp.get("status").and_then(|x| x.as_i64()).unwrap_or(0),
        status_text: s("statusText"),
        mime_type: s("mimeType"),
        resource_type: resource_type.map(String::from),
        protocol: s("protocol"),
        remote_ip: s("remoteIPAddress"),
        from_disk_cache: resp.get("fromDiskCache").and_then(|x| x.as_bool()),
        from_service_worker: resp.get("fromServiceWorker").and_then(|x| x.as_bool()),
        headers: headers_from_cdp(resp.get("headers")),
        raw_headers: None,
        redirect: None,
        expects_extra: None,
    }
}

type PendingExtra = Vec<(Option<i64>, BTreeMap<String, String>)>;

struct Store {
    documents: VecDeque<ResponseHeaderEntry>,
    all: VecDeque<ResponseHeaderEntry>,
    /// `responseReceivedExtraInfo` can arrive *before* `responseReceived`.
    /// Several per id for a redirect chain, told apart by status code.
    pending_extra: BTreeMap<String, PendingExtra>,
    pending_order: VecDeque<String>,
    armed: bool,
    dropped: usize,
}

static STORE: Mutex<Store> = Mutex::new(Store {
    documents: VecDeque::new(),
    all: VecDeque::new(),
    pending_extra: BTreeMap::new(),
    pending_order: VecDeque::new(),
    armed: false,
    dropped: 0,
});

/// Record that the `Network` domain came up. Without this an empty result is
/// indistinguishable from "nothing was requested".
pub fn set_armed(armed: bool) {
    if let Ok(mut s) = STORE.lock() {
        s.armed = armed;
    }
}

pub fn is_armed() -> bool {
    STORE.lock().map(|s| s.armed).unwrap_or(false)
}

/// Normalize a CDP headers object into a lowercase-keyed map.
pub fn headers_from_cdp(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(obj) = v.and_then(|h| h.as_object()) {
        for (k, val) in obj {
            let text = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(k.to_ascii_lowercase(), text);
        }
    }
    out
}

/// Forget every captured response (`reset`). The capture stays armed.
pub fn clear() {
    if let Ok(mut s) = STORE.lock() {
        s.documents.clear();
        s.all.clear();
        s.pending_extra.clear();
        s.pending_order.clear();
        s.dropped = 0;
    }
}

/// Ingest `Network.responseReceived` (or a redirect hop).
pub fn record(mut entry: ResponseHeaderEntry) {
    let Ok(mut s) = STORE.lock() else { return };

    // An extraInfo event may already have landed for this response. Match on
    // status: the `302` and the `200` of one redirect chain share an id; the
    // first compatible one is this entry's, since they arrive in hop order.
    if let Some(list) = s.pending_extra.get_mut(&entry.request_id) {
        if let Some(pos) = list.iter().position(|(st, _)| entry.accepts_extra(*st)) {
            let (_, raw) = list.remove(pos);
            entry.raw_headers = Some(raw);
        }
        if list.is_empty() {
            s.pending_extra.remove(&entry.request_id);
            s.pending_order.retain(|id| id != &entry.request_id);
        }
    }

    if entry.is_document() {
        if s.documents.len() >= DOCUMENT_CAP {
            s.documents.pop_front();
        }
        s.documents.push_back(entry.clone());
    }
    if s.all.len() >= ALL_CAP {
        s.all.pop_front();
        s.dropped += 1;
    }
    s.all.push_back(entry);
}

/// Ingest `Network.responseReceivedExtraInfo` — the raw wire headers.
///
/// `status` is the event's `statusCode`. It decides which entry the headers
/// belong to when one request id carries a redirect chain: attaching to "the
/// newest entry with this id" put the `302`'s wire headers on the `200`. Among
/// entries with a matching status the OLDEST one waiting wins — ExtraInfo
/// arrives in hop order, so that is how two `301`s are told apart.
pub fn record_extra_info(request_id: &str, status: Option<i64>, raw: BTreeMap<String, String>) {
    let Ok(mut s) = STORE.lock() else { return };

    // The usual order is extraInfo first, but not always. If the entry is
    // already stored, attach in place — in both rings, to the same response.
    let target = s
        .all
        .iter()
        .find(|e| e.request_id == request_id && e.accepts_extra(status))
        .or_else(|| {
            s.documents
                .iter()
                .find(|e| e.request_id == request_id && e.accepts_extra(status))
        })
        .cloned();
    if let Some(target) = target {
        let Store { all, documents, .. } = &mut *s;
        for e in all.iter_mut().chain(documents.iter_mut()) {
            if e.same_response(&target) && e.raw_headers.is_none() {
                e.raw_headers = Some(raw.clone());
            }
        }
        return;
    }

    s.pending_extra
        .entry(request_id.to_string())
        .or_default()
        .push((status, raw));
    if !s.pending_order.iter().any(|id| id == request_id) {
        s.pending_order.push_back(request_id.to_string());
    }
    // Requests that never produce a `responseReceived` (aborted, blocked)
    // would otherwise leak.
    while s.pending_order.len() > PENDING_EXTRA_CAP {
        if let Some(old) = s.pending_order.pop_front() {
            s.pending_extra.remove(&old);
        }
    }
}

/// Match a URL against the same pattern rules the `intercept` tool uses:
/// no `*` means substring, `*` makes it a glob over the whole URL.
pub fn url_matches(url: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if !pattern.contains('*') {
        return url.contains(pattern);
    }
    glob_match(url, pattern)
}

/// Iterative `*` glob with backtracking — no regex dependency, no catastrophic
/// backtracking.
fn glob_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Query the captured responses.
///
/// `resource_type` is matched case-insensitively; `document` reads the
/// dedicated ring so a flood of subresources cannot hide the navigation.
pub fn snapshot(
    url_pattern: Option<&str>,
    resource_type: Option<&str>,
    max: usize,
) -> (Vec<ResponseHeaderEntry>, usize) {
    let Ok(s) = STORE.lock() else {
        return (Vec::new(), 0);
    };

    let wants_documents = resource_type
        .map(|t| t.eq_ignore_ascii_case("document"))
        .unwrap_or(false);
    let source = if wants_documents {
        &s.documents
    } else {
        &s.all
    };

    let filtered: Vec<ResponseHeaderEntry> = source
        .iter()
        .filter(|e| url_pattern.is_none_or(|p| url_matches(&e.url, p)))
        .filter(|e| {
            // The document ring is already type-filtered.
            wants_documents
                || resource_type.is_none_or(|t| {
                    e.resource_type
                        .as_deref()
                        .is_some_and(|rt| rt.eq_ignore_ascii_case(t))
                })
        })
        .cloned()
        .collect();

    let total = filtered.len();
    let start = total.saturating_sub(max);
    (filtered[start..].to_vec(), total)
}

/// Mask sensitive header values in place. Applied at read time, so the buffer
/// keeps the truth and `--no-redact` can still produce it.
pub fn redact(entries: &mut [ResponseHeaderEntry]) {
    for e in entries.iter_mut() {
        redact_map(&mut e.headers);
        if let Some(raw) = e.raw_headers.as_mut() {
            redact_map(raw);
        }
    }
}

fn redact_map(map: &mut BTreeMap<String, String>) {
    for (name, value) in map.iter_mut() {
        if name == "set-cookie" {
            *value = redact_set_cookie(value);
        } else if crate::tools::definitions::is_sensitive_header(name) {
            *value = crate::tools::definitions::redact_header_value(value);
        }
    }
}

/// Mask a `Set-Cookie` value while keeping everything you actually debug with.
///
/// The generic `redact_header_value` splits on the first space to preserve an
/// auth scheme (`Bearer …`), which is exactly wrong here: it would keep
/// `session=SECRET;` as the "scheme" and redact `HttpOnly; Secure` instead.
/// Cookie **name** and **attributes** (`Path`, `Domain`, `HttpOnly`,
/// `SameSite`, `Secure`, `Max-Age`) are what tell you why a session or a policy
/// misbehaves, so only the value between the first `=` and the first `;` goes.
pub(crate) fn redact_set_cookie(value: &str) -> String {
    let (pair, attrs) = match value.split_once(';') {
        Some((p, rest)) => (p, Some(rest)),
        None => (value, None),
    };
    let masked = match pair.split_once('=') {
        Some((name, secret)) => {
            let len = secret.chars().count();
            format!("{name}=…[REDACTED {len} chars]")
        }
        // Not a name=value pair; nothing safe to keep.
        None => format!("…[REDACTED {} chars]", pair.chars().count()),
    };
    match attrs {
        Some(rest) => format!("{masked};{rest}"),
        None => masked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, url: &str, rt: &str) -> ResponseHeaderEntry {
        ResponseHeaderEntry {
            ts_ms: crate::recording::now_ms(),
            request_id: id.into(),
            url: url.into(),
            status: 200,
            resource_type: Some(rt.into()),
            ..Default::default()
        }
    }

    fn reset() {
        let mut s = STORE.lock().unwrap();
        s.documents.clear();
        s.all.clear();
        s.pending_extra.clear();
        s.pending_order.clear();
        s.dropped = 0;
    }

    #[test]
    fn cdp_headers_are_lowercased() {
        let h = headers_from_cdp(Some(&json!({
            "Content-Type": "text/html",
            "Content-Security-Policy": "default-src 'self'"
        })));
        assert_eq!(h.get("content-type").map(String::as_str), Some("text/html"));
        assert!(h.contains_key("content-security-policy"));
    }

    #[test]
    fn glob_and_substring_patterns() {
        assert!(url_matches("https://a.example.com/x.js", "example.com"));
        assert!(!url_matches("https://a.example.com/x.js", "other.com"));
        assert!(url_matches("https://a.example.com/api/v1", "*/api/*"));
        assert!(!url_matches("https://a.example.com/apx/v1", "*/api/*"));
        assert!(url_matches("https://a.example.com/", "*"));
        // Empty pattern means "no filter", not "match nothing".
        assert!(url_matches("https://a.example.com/", ""));
    }

    #[test]
    fn glob_backtracks_correctly() {
        assert!(glob_match("aaa.js", "*.js"));
        assert!(glob_match("a/b/c/d.js", "*/b/*d.js"));
        assert!(!glob_match("a/b/c/d.css", "*/b/*d.js"));
    }

    // ONE test function for everything that touches the store: it is
    // process-global and the harness runs tests in parallel, so separate
    // functions would pollute each other's counts.
    #[test]
    fn store_behaviour() {
        reset();

        // --- the point of the two rings ---------------------------------
        // Document first, then enough subresources to overflow the main ring.
        record(entry("doc", "https://example.com/", "Document"));
        for i in 0..(ALL_CAP + 10) {
            record(entry(
                &format!("r{i}"),
                &format!("https://example.com/{i}.png"),
                "Image",
            ));
        }
        let (docs, total) = snapshot(None, Some("document"), 10);
        assert_eq!(
            total, 1,
            "the navigation document must survive a subresource flood"
        );
        assert_eq!(docs[0].url, "https://example.com/");
        // …and it really was evicted from the shared ring, so this is not a
        // test that would pass with a single buffer.
        let (all, _) = snapshot(Some("https://example.com/"), None, 500);
        assert!(
            !all.iter().any(|e| e.request_id == "doc"),
            "precondition: the flood should have evicted it from the `all` ring"
        );

        // --- resource-type filter --------------------------------------
        reset();
        record(entry("a", "https://x.test/a.js", "Script"));
        record(entry("b", "https://x.test/b.json", "XHR"));
        let (scripts, _) = snapshot(None, Some("script"), 10);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].request_id, "a");

        // --- url pattern ------------------------------------------------
        let (json_only, _) = snapshot(Some("*.json"), None, 10);
        assert_eq!(json_only.len(), 1);
        assert_eq!(json_only[0].request_id, "b");

        // --- max keeps the NEWEST -------------------------------------
        reset();
        for i in 0..5 {
            record(entry(
                &format!("n{i}"),
                &format!("https://x.test/{i}"),
                "XHR",
            ));
        }
        let (tail, total) = snapshot(None, None, 2);
        assert_eq!(total, 5);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[1].request_id, "n4");

        // --- extraInfo merges in either arrival order -------------------
        reset();
        let mut raw = BTreeMap::new();
        raw.insert("set-cookie".to_string(), "sid=abc".to_string());

        // extraInfo BEFORE responseReceived (the usual order)
        record_extra_info("early", Some(200), raw.clone());
        record(entry("early", "https://x.test/1", "Document"));
        let (docs, _) = snapshot(Some("/1"), Some("document"), 10);
        assert_eq!(
            docs[0]
                .raw_headers
                .as_ref()
                .and_then(|h| h.get("set-cookie")),
            Some(&"sid=abc".to_string())
        );

        // extraInfo AFTER responseReceived
        record(entry("late", "https://x.test/2", "Document"));
        record_extra_info("late", Some(200), raw.clone());
        let (docs, _) = snapshot(Some("/2"), Some("document"), 10);
        assert_eq!(
            docs[0]
                .raw_headers
                .as_ref()
                .and_then(|h| h.get("set-cookie")),
            Some(&"sid=abc".to_string())
        );

        // --- a redirect chain: one id, two responses, matched by status --
        reset();
        let mut wire302 = BTreeMap::new();
        wire302.insert("location".to_string(), "/home".to_string());
        let mut wire200 = BTreeMap::new();
        wire200.insert("content-type".to_string(), "text/html".to_string());
        record_extra_info("chain", Some(302), wire302);
        let mut hop = entry("chain", "https://x.test/", "Document");
        hop.status = 302;
        hop.redirect = Some(true);
        record(hop);
        record(entry("chain", "https://x.test/home", "Document"));
        record_extra_info("chain", Some(200), wire200);
        let (docs, _) = snapshot(None, Some("document"), 10);
        assert_eq!(docs.len(), 2, "the redirect hop must be its own entry");
        assert_eq!(docs[0].status, 302);
        assert_eq!(docs[0].redirect, Some(true));
        assert!(docs[0]
            .raw_headers
            .as_ref()
            .unwrap()
            .contains_key("location"));
        assert!(docs[1]
            .raw_headers
            .as_ref()
            .unwrap()
            .contains_key("content-type"));

        // --- two same-status hops, ExtraInfo late: oldest waiting first --
        reset();
        let wire = |m: &str| {
            let mut h = BTreeMap::new();
            h.insert("x-marker".to_string(), m.to_string());
            h
        };
        for (i, url) in ["https://x.test/1", "https://x.test/2"].iter().enumerate() {
            let mut hop = entry("same", url, "Document");
            hop.status = 302;
            hop.ts_ms += i as u64;
            hop.redirect = Some(true);
            record(hop);
        }
        record_extra_info("same", Some(302), wire("A"));
        record_extra_info("same", Some(302), wire("B"));
        let (docs, _) = snapshot(None, Some("document"), 10);
        let got: Vec<&str> = docs
            .iter()
            .map(|d| d.raw_headers.as_ref().unwrap()["x-marker"].as_str())
            .collect();
        assert_eq!(got, ["A", "B"], "late ExtraInfo must not swap hops");
        let (all, _) = snapshot(None, None, 10);
        assert!(
            all.iter().all(|e| e.raw_headers.is_some()),
            "both rings carry the same attachment"
        );

        // --- a revalidated cache hit: 304 on the wire, 200 reported -------
        reset();
        record_extra_info("reval", Some(304), wire("etag"));
        record(entry("reval", "https://x.test/r", "Document"));
        let (docs, _) = snapshot(Some("/r"), Some("document"), 10);
        assert!(
            docs[0].raw_headers.is_some(),
            "304 ExtraInfo belongs to the 200"
        );

        // --- a response that will get no ExtraInfo is never handed one ----
        reset();
        let mut internal = entry("hsts", "http://x.test/", "Document");
        internal.status = 301;
        internal.expects_extra = Some(false);
        record(internal);
        let mut real = entry("hsts", "https://x.test/", "Document");
        real.status = 301;
        real.ts_ms += 1;
        record(real);
        record_extra_info("hsts", Some(301), wire("real"));
        let (docs, _) = snapshot(None, Some("document"), 10);
        assert!(docs[0].raw_headers.is_none());
        assert!(docs[1].raw_headers.is_some());

        // --- orphaned extraInfo is bounded ------------------------------
        reset();
        // Aborted/blocked requests never produce a `responseReceived`, so the
        // pending map would grow without limit.
        for i in 0..(PENDING_EXTRA_CAP + 50) {
            record_extra_info(&format!("orphan{i}"), None, BTreeMap::new());
        }
        {
            let s = STORE.lock().unwrap();
            assert!(s.pending_extra.len() <= PENDING_EXTRA_CAP);
            assert!(s.pending_order.len() <= PENDING_EXTRA_CAP);
        }

        reset();
    }

    #[test]
    fn set_cookie_redaction_keeps_the_name_and_the_attributes() {
        // These attributes are the whole reason you read Set-Cookie while
        // debugging a session or a policy; blanking them would defeat the tool.
        let out = redact_set_cookie(
            "session=aVeryLongSecretValue123; Path=/; HttpOnly; SameSite=Lax; Secure",
        );
        assert!(!out.contains("aVeryLongSecretValue123"), "{out}");
        assert!(out.starts_with("session=…[REDACTED "), "{out}");
        assert!(out.contains("HttpOnly"), "{out}");
        assert!(out.contains("SameSite=Lax"), "{out}");
        assert!(out.contains("Secure"), "{out}");
        assert!(out.contains("Path=/"), "{out}");
    }

    #[test]
    fn set_cookie_redaction_handles_odd_shapes() {
        // No attributes.
        let out = redact_set_cookie("a=secretvalue");
        assert!(!out.contains("secretvalue"));
        assert!(out.starts_with("a=…[REDACTED "), "{out}");
        // Not a name=value pair at all — keep nothing.
        let out = redact_set_cookie("deleted");
        assert!(!out.contains("deleted"), "{out}");
    }

    #[test]
    fn redaction_leaves_non_sensitive_headers_alone() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "set-cookie".to_string(),
            "sid=aVeryLongSecretValue123".to_string(),
        );
        headers.insert("content-type".to_string(), "text/html".to_string());
        headers.insert(
            "content-security-policy".to_string(),
            "default-src 'self'".to_string(),
        );
        let mut entries = vec![ResponseHeaderEntry {
            headers,
            ..Default::default()
        }];

        redact(&mut entries);
        assert!(!entries[0].headers["set-cookie"].contains("aVeryLongSecretValue123"));
        assert_eq!(entries[0].headers["content-type"], "text/html");
        // CSP is the reason this tool exists — it must never be redacted.
        assert_eq!(
            entries[0].headers["content-security-policy"],
            "default-src 'self'"
        );
    }
}
