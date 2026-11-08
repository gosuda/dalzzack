//! Always-on request index and navigation timeline, fed by CDP `Network.*` and
//! `Page.*` events.
//!
//! # Why this exists
//!
//! Three questions kept costing whole sessions because nothing could answer
//! them without `--record` (which has to be chosen before the problem shows
//! up):
//!
//! * **"What did the navigation actually go through?"** A redirect hop never
//!   produces `Network.responseReceived` — its response only arrives inside the
//!   *next* hop's `requestWillBeSent.redirectResponse` — so `response-headers`
//!   showed the final document and silently dropped the `302` in front of it.
//!   And a challenge page that solves itself (Cloudflare's `cf-mitigated:
//!   challenge` → script → reload) looked like a plain successful load, because
//!   `navigate` only ever reported where it ended up.
//! * **"Which requests failed?"** `Network.loadingFailed` carries no URL — only
//!   a request id — so a failure is useless unless the request that preceded it
//!   was kept. The in-page `get-requests` hook sees fetch/XHR only.
//! * **"What URL is that instance on?"** — for `list`, without touching the
//!   renderer.
//!
//! The events are the same ones `recording` already subscribes to; this module
//! keeps a small, bounded, *structured* view of them regardless of `--record`.
//! It is pure JSON in, JSON out, so the WebView2 event handler and the real
//! Chrome engine feed it identically.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// Requests kept for `failed-requests` and URL attribution. A heavy page makes
/// a few hundred; this holds several navigations' worth.
const REQUEST_CAP: usize = 3000;
/// Document request chains kept for the timeline. Documents are rare, so a
/// flood of subresources can never push a navigation out.
const NAVIGATION_CAP: usize = 200;
/// `requestWillBeSentExtraInfo` / `responseReceivedExtraInfo` that arrived
/// before the event they belong to.
const PENDING_EXTRA_CAP: usize = 300;

/// Response headers lifted onto every hop, because they are what tells you
/// *why* a hop went where it went. The full set is available with `headers`.
const HOP_HEADERS: &[&str] = &[
    "location",
    "cf-mitigated",
    "cf-ray",
    "server",
    "content-type",
    "content-length",
    "cache-control",
    "x-cache",
    "via",
    "refresh",
];

#[derive(Serialize, Debug, Clone, Default)]
pub struct Hop {
    pub ts_ms: u64,
    pub url: String,
    /// `redirect` | `response` | `failed` | `pending`
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_service_worker: Option<bool>,
    /// The subset of response headers in [`HOP_HEADERS`] that were present.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub key_headers: BTreeMap<String, String>,
    /// Names of cookies this response set — the values are never kept here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub set_cookie_names: Vec<String>,
    /// Detected bot-challenge marker, e.g. `cloudflare:challenge`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
    #[serde(skip)]
    pub response_headers: BTreeMap<String, String>,
    #[serde(skip)]
    pub request_headers: BTreeMap<String, String>,
    /// Did `request_headers` come from the wire (`requestWillBeSentExtraInfo`)
    /// rather than the renderer's view (which lacks `cookie` and friends)?
    #[serde(skip)]
    pub request_headers_from_wire: bool,
    #[serde(skip)]
    pub response_headers_from_wire: bool,
    /// `Some(false)` when CDP said no wire headers will come for this
    /// response (`hasExtraInfo` / `redirectHasExtraInfo` false — an HSTS
    /// internal redirect, a cache hit). Such a hop must never be handed
    /// another hop's wire headers.
    #[serde(skip)]
    pub expects_wire: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestRecord {
    pub seq: u64,
    pub request_id: String,
    pub ts_ms: u64,
    pub method: Option<String>,
    pub resource_type: Option<String>,
    pub frame_id: Option<String>,
    pub loader_id: Option<String>,
    pub initiator_type: Option<String>,
    pub initiator_url: Option<String>,
    /// Every URL this request went through, in order. The last hop is the
    /// one that is still pending, answered or failed.
    pub hops: Vec<Hop>,
    pub finished_ms: Option<u64>,
    pub encoded_data_length: Option<i64>,
    pub canceled: bool,
    pub cors_error: Option<String>,
    /// Set from `Page.frameNavigated` with this request's loader id. Kept on
    /// the record, not in a separate ring, so a navigation that is still in
    /// the timeline never loses its commit to a flood of iframe commits.
    pub committed_ms: Option<u64>,
    pub committed_url: Option<String>,
}

impl RequestRecord {
    fn is_document(&self) -> bool {
        self.resource_type.as_deref() == Some("Document")
    }

    fn last_hop(&self) -> Option<&Hop> {
        self.hops.last()
    }

    fn last_hop_mut(&mut self) -> Option<&mut Hop> {
        self.hops.last_mut()
    }

    fn url(&self) -> &str {
        self.last_hop().map(|h| h.url.as_str()).unwrap_or("")
    }
}

/// A frame commit whose request has not been seen (yet).
#[derive(Debug, Clone, Default)]
struct PendingCommit {
    loader_id: String,
    url: String,
    ts_ms: u64,
}

/// Commits are matched to records as they happen; this only holds the rare
/// one that arrives before its request.
const PENDING_COMMIT_CAP: usize = 50;

/// Wire response headers waiting for their hop: `(status code, headers)`.
type PendingResponse = (i64, BTreeMap<String, String>);

struct Store {
    seq: u64,
    requests: BTreeMap<String, RequestRecord>,
    order: VecDeque<String>,
    /// Document request ids, newest last — the navigation timeline.
    navigations: VecDeque<String>,
    /// Kept separately from `requests` so a navigation evicted from the request
    /// ring (a page with thousands of subresources) still shows in the timeline.
    nav_records: BTreeMap<String, RequestRecord>,
    pending_commits: VecDeque<PendingCommit>,
    /// Wire request headers that arrived before the hop they belong to — in
    /// order, since a redirect chain produces one per hop.
    pending_req_extra: BTreeMap<String, VecDeque<BTreeMap<String, String>>>,
    pending_resp_extra: BTreeMap<String, Vec<PendingResponse>>,
    pending_order: VecDeque<String>,
    main_frame_id: Option<String>,
    current_url: Option<String>,
    current_url_ms: Option<u64>,
    armed: bool,
    dropped_requests: usize,
}

static STORE: Mutex<Store> = Mutex::new(Store {
    seq: 0,
    requests: BTreeMap::new(),
    order: VecDeque::new(),
    navigations: VecDeque::new(),
    nav_records: BTreeMap::new(),
    pending_commits: VecDeque::new(),
    pending_req_extra: BTreeMap::new(),
    pending_resp_extra: BTreeMap::new(),
    pending_order: VecDeque::new(),
    main_frame_id: None,
    current_url: None,
    current_url_ms: None,
    armed: false,
    dropped_requests: 0,
});

/// Record that the event stream feeding this module came up. Without it an
/// empty timeline is indistinguishable from "nothing was loaded".
pub fn set_armed(armed: bool) {
    if let Ok(mut s) = STORE.lock() {
        s.armed = armed;
    }
}

pub fn is_armed() -> bool {
    STORE.lock().map(|s| s.armed).unwrap_or(false)
}

/// Tell the store which frame is the top document, when it is known from
/// somewhere other than a `Page.frameNavigated` (e.g. `Page.getFrameTree` at
/// arming time, or the target id of a real Chrome page).
pub fn set_main_frame_id(id: &str) {
    if let Ok(mut s) = STORE.lock() {
        s.main_frame_id = Some(id.to_string());
    }
}

/// The last committed top-level URL, and when it committed. Read from daemon
/// memory, so `list` can show it without a round trip into a possibly wedged
/// renderer.
pub fn current_url() -> (Option<String>, Option<u64>) {
    STORE
        .lock()
        .map(|s| (s.current_url.clone(), s.current_url_ms))
        .unwrap_or((None, None))
}

/// Sequence number of the newest request seen. Anything recorded after a
/// caller reads this has a strictly greater `seq`.
pub fn last_seq() -> u64 {
    STORE.lock().map(|s| s.seq).unwrap_or(0)
}

/// Forget everything (`reset`). Keeps `armed` and the main frame id — the
/// event stream itself is unaffected.
pub fn clear() {
    if let Ok(mut s) = STORE.lock() {
        s.requests.clear();
        s.order.clear();
        s.navigations.clear();
        s.nav_records.clear();
        s.pending_commits.clear();
        s.pending_req_extra.clear();
        s.pending_resp_extra.clear();
        s.pending_order.clear();
        s.current_url = None;
        s.current_url_ms = None;
        s.dropped_requests = 0;
    }
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn lower_headers(v: Option<&Value>) -> BTreeMap<String, String> {
    crate::response_headers::headers_from_cdp(v)
}

/// Pull the cookie names out of a (possibly newline-joined) `set-cookie` value.
fn set_cookie_names(headers: &BTreeMap<String, String>) -> Vec<String> {
    let Some(raw) = headers.get("set-cookie") else {
        return Vec::new();
    };
    raw.split('\n')
        .filter_map(|line| {
            let pair = line.split(';').next()?.trim();
            let name = pair.split('=').next()?.trim();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// Recognise a bot-challenge response from its headers. Deliberately narrow:
/// a guess here would send an investigation the wrong way.
fn detect_challenge(status: Option<i64>, headers: &BTreeMap<String, String>) -> Option<String> {
    if let Some(v) = headers.get("cf-mitigated") {
        return Some(format!("cloudflare:{}", v.trim()));
    }
    if headers.contains_key("x-datadome") && matches!(status, Some(403)) {
        return Some("datadome".into());
    }
    None
}

/// Can wire response headers reported with `wire` (the ExtraInfo
/// `statusCode`) belong to a hop whose renderer-visible status is `hop`?
///
/// Unknown status never matches: the hop's status arrives later and the
/// headers wait for it. A revalidated cache hit is `304` on the wire and `200`
/// to the renderer (CDP documents this), so that pair matches too.
fn status_matches(wire: i64, hop: Option<i64>) -> bool {
    match hop {
        Some(h) => wire == 0 || wire == h || (wire == 304 && h == 200),
        None => false,
    }
}

fn wants_response_wire(h: &Hop) -> bool {
    !h.response_headers_from_wire && h.expects_wire != Some(false)
}

/// Build the response half of a hop from a CDP `Response` object.
/// `expects_wire` is CDP's `hasExtraInfo` / `redirectHasExtraInfo`.
fn fill_response(hop: &mut Hop, resp: &Value, kind: &str, expects_wire: Option<bool>) {
    hop.kind = kind.to_string();
    if expects_wire.is_some() {
        hop.expects_wire = expects_wire;
    }
    if let Some(u) = str_at(resp, "url") {
        hop.url = u;
    }
    hop.status = resp.get("status").and_then(|x| x.as_i64());
    hop.status_text = str_at(resp, "statusText");
    hop.mime_type = str_at(resp, "mimeType");
    hop.remote_ip = str_at(resp, "remoteIPAddress");
    hop.protocol = str_at(resp, "protocol");
    hop.from_cache = resp.get("fromDiskCache").and_then(|x| x.as_bool());
    hop.from_service_worker = resp.get("fromServiceWorker").and_then(|x| x.as_bool());
    let headers = lower_headers(resp.get("headers"));
    // Wire headers (responseReceivedExtraInfo) are more truthful; never let the
    // renderer's merged view overwrite them if they already landed.
    if !hop.response_headers_from_wire {
        apply_response_headers(hop, headers, false);
    }
    // The status may only just have become known; some detections need it.
    hop.challenge = detect_challenge(hop.status, &hop.response_headers);
}

fn apply_response_headers(hop: &mut Hop, headers: BTreeMap<String, String>, from_wire: bool) {
    hop.key_headers = HOP_HEADERS
        .iter()
        .filter_map(|h| headers.get(*h).map(|v| (h.to_string(), v.clone())))
        .collect();
    hop.set_cookie_names = set_cookie_names(&headers);
    hop.challenge = detect_challenge(hop.status, &headers);
    hop.response_headers = headers;
    hop.response_headers_from_wire = from_wire;
}

impl Store {
    fn insert_request(&mut self, rec: RequestRecord) {
        let id = rec.request_id.clone();
        if rec.is_document() {
            self.navigations.retain(|n| n != &id);
            self.navigations.push_back(id.clone());
            self.nav_records.insert(id.clone(), rec.clone());
            while self.navigations.len() > NAVIGATION_CAP {
                if let Some(old) = self.navigations.pop_front() {
                    self.nav_records.remove(&old);
                }
            }
        }
        if self.requests.insert(id.clone(), rec).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > REQUEST_CAP {
            if let Some(old) = self.order.pop_front() {
                self.requests.remove(&old);
                self.dropped_requests += 1;
            }
        }
    }

    /// Apply `f` to the live record and to its timeline copy, if it has one,
    /// and return what `f` said. `None` = no such record. Both copies always
    /// receive the same event, so they stay identical and either answer is
    /// the answer.
    fn with_record<R>(
        &mut self,
        request_id: &str,
        f: impl Fn(&mut RequestRecord) -> R,
    ) -> Option<R> {
        let a = self.requests.get_mut(request_id).map(&f);
        let b = self.nav_records.get_mut(request_id).map(&f);
        a.or(b)
    }

    /// Drop `request_id` from the pending order once nothing is pending for it,
    /// so a later re-queue is not evicted early as if it were old.
    fn unpend_if_done(&mut self, request_id: &str) {
        if !self.pending_req_extra.contains_key(request_id)
            && !self.pending_resp_extra.contains_key(request_id)
        {
            self.pending_order.retain(|id| id != request_id);
        }
    }

    /// Record a frame commit on every record of that loader.
    fn mark_committed(&mut self, loader_id: &str, url: &str, ts_ms: u64) -> bool {
        let mut applied = false;
        for r in self
            .requests
            .values_mut()
            .chain(self.nav_records.values_mut())
        {
            if r.loader_id.as_deref() == Some(loader_id) && r.is_document() {
                r.committed_ms = Some(ts_ms);
                r.committed_url = Some(url.to_string());
                applied = true;
            }
        }
        applied
    }

    fn get(&self, request_id: &str) -> Option<&RequestRecord> {
        self.requests
            .get(request_id)
            .or_else(|| self.nav_records.get(request_id))
    }

    fn pend(&mut self, request_id: &str) {
        if !self.pending_order.iter().any(|id| id == request_id) {
            self.pending_order.push_back(request_id.to_string());
        }
        while self.pending_order.len() > PENDING_EXTRA_CAP {
            if let Some(old) = self.pending_order.pop_front() {
                self.pending_req_extra.remove(&old);
                self.pending_resp_extra.remove(&old);
            }
        }
    }

    fn is_main_frame(&self, frame_id: Option<&str>) -> Option<bool> {
        match (frame_id, self.main_frame_id.as_deref()) {
            (Some(f), Some(m)) => Some(f == m),
            _ => None,
        }
    }
}

/// Feed one CDP event. Unknown events are ignored, so callers can pass their
/// whole stream through.
pub fn ingest(event: &str, params: &Value) {
    let ts_ms = crate::recording::cdp_event_ts_ms(params);
    let Ok(mut s) = STORE.lock() else { return };
    match event {
        "Network.requestWillBeSent" => on_request(&mut s, params, ts_ms),
        "Network.requestWillBeSentExtraInfo" => {
            let Some(id) = str_at(params, "requestId") else {
                return;
            };
            let headers = lower_headers(params.get("headers"));
            // One per hop, in hop order: the OLDEST hop still without wire
            // headers is the one this belongs to. If every known hop already
            // has its own, the hop this is for has not been announced yet.
            if !attach_request_extra(&mut s, &id, &headers) {
                s.pending_req_extra
                    .entry(id.clone())
                    .or_default()
                    .push_back(headers);
                s.pend(&id);
            }
        }
        "Network.responseReceived" => {
            let Some(id) = str_at(params, "requestId") else {
                return;
            };
            let resp = params.get("response").cloned().unwrap_or(Value::Null);
            let rtype = str_at(params, "type");
            let has_extra = params.get("hasExtraInfo").and_then(|x| x.as_bool());
            let known = s
                .with_record(&id, |r| {
                    if r.resource_type.is_none() {
                        r.resource_type = rtype.clone();
                    }
                    if let Some(h) = r.last_hop_mut() {
                        fill_response(h, &resp, "response", has_extra);
                    }
                })
                .is_some();
            if known {
                attach_pending_response_extra(&mut s, &id);
            }
        }
        "Network.responseReceivedExtraInfo" => {
            let Some(id) = str_at(params, "requestId") else {
                return;
            };
            let status = params
                .get("statusCode")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let headers = lower_headers(params.get("headers"));
            if !attach_response_extra(&mut s, &id, status, &headers) {
                s.pending_resp_extra
                    .entry(id.clone())
                    .or_default()
                    .push((status, headers));
                s.pend(&id);
            }
        }
        "Network.loadingFailed" => {
            let Some(id) = str_at(params, "requestId") else {
                return;
            };
            let error_text = str_at(params, "errorText");
            let blocked = str_at(params, "blockedReason");
            let canceled = params
                .get("canceled")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let cors = params
                .pointer("/corsErrorStatus/corsError")
                .and_then(|x| x.as_str())
                .map(String::from);
            let rtype = str_at(params, "type");
            s.with_record(&id, |r| {
                r.finished_ms = Some(ts_ms);
                r.canceled = canceled;
                r.cors_error = cors.clone();
                if r.resource_type.is_none() {
                    r.resource_type = rtype.clone();
                }
                if let Some(h) = r.last_hop_mut() {
                    h.kind = "failed".into();
                    h.error_text = error_text.clone();
                    h.blocked_reason = blocked.clone();
                }
            });
            // A top-level document that failed is what `navigate` and
            // `wait-navigation` must report (see `crate::main_frame`).
            let main = s.main_frame_id.clone();
            let failed_document = s
                .nav_records
                .get(&id)
                .filter(|r| main.is_none() || r.frame_id.is_none() || r.frame_id == main)
                .map(|r| r.url().to_string());
            if let Some(url) = failed_document {
                crate::main_frame::record(crate::main_frame::Failure {
                    ts_ms,
                    failing_url: Some(url),
                    message: error_text.clone().unwrap_or_default(),
                    canceled,
                    ..Default::default()
                });
            }
            // A CORS-blocked response never reaches `responseReceived`, but its
            // wire headers — and so its status — did arrive.
            let needs_status = s
                .get(&id)
                .and_then(|r| r.last_hop())
                .is_some_and(|h| h.status.is_none() && !h.response_headers_from_wire);
            if needs_status {
                let first = s
                    .pending_resp_extra
                    .get_mut(&id)
                    .filter(|l| !l.is_empty())
                    .map(|l| l.remove(0));
                if let Some((st, hdrs)) = first {
                    s.with_record(&id, |r| {
                        if let Some(h) = r.last_hop_mut() {
                            if st > 0 {
                                h.status = Some(st);
                            }
                            apply_response_headers(h, hdrs.clone(), true);
                        }
                    });
                }
                if s.pending_resp_extra.get(&id).is_some_and(|l| l.is_empty()) {
                    s.pending_resp_extra.remove(&id);
                }
                s.unpend_if_done(&id);
            }
        }
        "Network.loadingFinished" => {
            let Some(id) = str_at(params, "requestId") else {
                return;
            };
            let len = params.get("encodedDataLength").and_then(|x| x.as_i64());
            s.with_record(&id, |r| {
                r.finished_ms = Some(ts_ms);
                r.encoded_data_length = len;
            });
        }
        "Page.frameNavigated" => {
            let frame = params.get("frame").cloned().unwrap_or(Value::Null);
            let Some(frame_id) = str_at(&frame, "id") else {
                return;
            };
            let url = str_at(&frame, "url").unwrap_or_default();
            let is_top = frame.get("parentId").is_none();
            if is_top {
                s.main_frame_id = Some(frame_id.clone());
                s.current_url = Some(url.clone());
                s.current_url_ms = Some(ts_ms);
            }
            if let Some(loader) = str_at(&frame, "loaderId") {
                if !s.mark_committed(&loader, &url, ts_ms) {
                    // about:blank / srcdoc commits have no request at all;
                    // keep a few in case the request is merely late.
                    s.pending_commits.push_back(PendingCommit {
                        loader_id: loader,
                        url,
                        ts_ms,
                    });
                    while s.pending_commits.len() > PENDING_COMMIT_CAP {
                        s.pending_commits.pop_front();
                    }
                }
            }
        }
        // pushState / hash changes: the document stays, the URL moves.
        "Page.navigatedWithinDocument" => {
            let frame_id = str_at(params, "frameId");
            if frame_id.is_some() && frame_id == s.main_frame_id {
                s.current_url = str_at(params, "url");
                s.current_url_ms = Some(ts_ms);
            }
        }
        _ => {}
    }
}

fn on_request(s: &mut Store, params: &Value, ts_ms: u64) {
    let Some(id) = str_at(params, "requestId") else {
        return;
    };
    let req = params.get("request").cloned().unwrap_or(Value::Null);
    let url = str_at(&req, "url").unwrap_or_default();
    let renderer_headers = lower_headers(req.get("headers"));

    // Same request id again = a redirect. The previous hop's response arrives
    // here and nowhere else.
    if let Some(redirect) = params.get("redirectResponse") {
        let redirect = redirect.clone();
        let redirect_has_extra = params.get("redirectHasExtraInfo").and_then(|x| x.as_bool());
        let new_hop = Hop {
            ts_ms,
            url: url.clone(),
            kind: "pending".into(),
            request_headers: renderer_headers.clone(),
            ..Default::default()
        };
        let known = s
            .with_record(&id, |r| {
                if let Some(h) = r.last_hop_mut() {
                    fill_response(h, &redirect, "redirect", redirect_has_extra);
                }
                r.hops.push(new_hop.clone());
            })
            .is_some();
        if known {
            attach_pending_request_extra(s, &id);
            attach_pending_response_extra(s, &id);
            return;
        }
        // The first hop was never seen (evicted, or before arming): keep what
        // we have rather than dropping the chain.
    }

    s.seq += 1;
    let initiator = params.get("initiator");
    let mut rec = RequestRecord {
        seq: s.seq,
        request_id: id.clone(),
        ts_ms,
        method: str_at(&req, "method"),
        resource_type: str_at(params, "type"),
        frame_id: str_at(params, "frameId"),
        loader_id: str_at(params, "loaderId"),
        initiator_type: initiator.and_then(|i| str_at(i, "type")),
        initiator_url: initiator.and_then(|i| {
            str_at(i, "url").or_else(|| {
                i.pointer("/stack/callFrames/0/url")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            })
        }),
        hops: vec![Hop {
            ts_ms,
            url,
            kind: "pending".into(),
            request_headers: renderer_headers,
            ..Default::default()
        }],
        ..Default::default()
    };
    if let Some(loader) = rec.loader_id.clone().filter(|_| rec.is_document()) {
        if let Some(pos) = s.pending_commits.iter().position(|c| c.loader_id == loader) {
            if let Some(c) = s.pending_commits.remove(pos) {
                rec.committed_ms = Some(c.ts_ms);
                rec.committed_url = Some(c.url);
            }
        }
    }
    s.insert_request(rec);
    attach_pending_request_extra(s, &id);
    attach_pending_response_extra(s, &id);
}

/// Give wire request headers to the oldest hop still without them.
fn attach_request_extra(
    s: &mut Store,
    request_id: &str,
    headers: &BTreeMap<String, String>,
) -> bool {
    s.with_record(request_id, |r| {
        match r.hops.iter_mut().find(|h| !h.request_headers_from_wire) {
            Some(h) => {
                h.request_headers = headers.clone();
                h.request_headers_from_wire = true;
                true
            }
            None => false,
        }
    })
    .unwrap_or(false)
}

fn attach_pending_request_extra(s: &mut Store, request_id: &str) {
    while let Some(front) = s
        .pending_req_extra
        .get(request_id)
        .and_then(|q| q.front().cloned())
    {
        if !attach_request_extra(s, request_id, &front) {
            break;
        }
        if let Some(q) = s.pending_req_extra.get_mut(request_id) {
            q.pop_front();
            if q.is_empty() {
                s.pending_req_extra.remove(request_id);
            }
        }
    }
    s.unpend_if_done(request_id);
}

/// Attach wire response headers to the hop they belong to: the OLDEST hop
/// whose status matches and that is still waiting for wire headers.
///
/// A redirect chain shares one request id, so the status is what tells the
/// `302`'s headers from the `200`'s, and hop order is what tells two `301`s
/// apart — ExtraInfo arrives in hop order. Returns whether THIS call attached
/// (a hop that already had its own headers proves nothing about these).
fn attach_response_extra(
    s: &mut Store,
    request_id: &str,
    status: i64,
    headers: &BTreeMap<String, String>,
) -> bool {
    s.with_record(request_id, |r| {
        match r
            .hops
            .iter_mut()
            .find(|h| wants_response_wire(h) && status_matches(status, h.status))
        {
            Some(h) => {
                apply_response_headers(h, headers.clone(), true);
                true
            }
            None => false,
        }
    })
    .unwrap_or(false)
}

fn attach_pending_response_extra(s: &mut Store, request_id: &str) {
    let Some(list) = s.pending_resp_extra.remove(request_id) else {
        return;
    };
    let mut left = Vec::new();
    for (status, headers) in list {
        if !attach_response_extra(s, request_id, status, &headers) {
            left.push((status, headers));
        }
    }
    if !left.is_empty() {
        s.pending_resp_extra.insert(request_id.to_string(), left);
    }
    s.unpend_if_done(request_id);
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

fn redact_map(map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    map.iter()
        .map(|(k, v)| {
            let v = if k == "cookie" {
                redact_cookie_header(v)
            } else if k == "set-cookie" {
                v.split('\n')
                    .map(crate::response_headers::redact_set_cookie)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if crate::tools::definitions::is_sensitive_header(k) {
                crate::tools::definitions::redact_header_value(v)
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect()
}

/// `a=1; b=secret` → `a=…[1]; b=…[6]`. The names are what tell you which
/// cookies went out; the values are what must not leak into a transcript.
pub fn redact_cookie_header(v: &str) -> String {
    v.split(';')
        .map(|pair| {
            let pair = pair.trim();
            match pair.split_once('=') {
                Some((name, val)) => format!("{name}=…[{}]", val.chars().count()),
                None => format!("…[{}]", pair.chars().count()),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn hop_json(h: &Hop, with_headers: bool, redact: bool) -> Value {
    let mut v = serde_json::to_value(h).unwrap_or(Value::Null);
    if with_headers {
        if let Some(obj) = v.as_object_mut() {
            let (req, resp) = if redact {
                (
                    redact_map(&h.request_headers),
                    redact_map(&h.response_headers),
                )
            } else {
                (h.request_headers.clone(), h.response_headers.clone())
            };
            obj.insert("request_headers".into(), json!(req));
            obj.insert(
                "request_headers_source".into(),
                json!(if h.request_headers_from_wire {
                    "wire"
                } else {
                    "renderer"
                }),
            );
            if !resp.is_empty() {
                obj.insert("response_headers".into(), json!(resp));
                obj.insert(
                    "response_headers_source".into(),
                    json!(if h.response_headers_from_wire {
                        "wire"
                    } else {
                        "renderer"
                    }),
                );
            }
        }
    }
    v
}

/// Options for [`navigations`].
#[derive(Debug, Clone, Default)]
pub struct NavQuery {
    /// Only navigations whose request was first seen after this sequence number.
    pub after_seq: Option<u64>,
    pub since_ms: Option<u64>,
    /// Include iframe documents too (default: top frame only).
    pub include_frames: bool,
    pub url_pattern: Option<String>,
    pub max: usize,
    pub with_headers: bool,
    pub redact: bool,
}

/// The navigation timeline: one entry per document request chain, oldest first.
pub fn navigations(q: &NavQuery) -> Vec<Value> {
    let Ok(s) = STORE.lock() else {
        return Vec::new();
    };
    let main = s.main_frame_id.clone();
    let mut out: Vec<Value> = s
        .navigations
        .iter()
        .filter_map(|id| s.nav_records.get(id))
        .filter(|r| q.after_seq.is_none_or(|seq| r.seq > seq))
        .filter(|r| q.since_ms.is_none_or(|t| r.ts_ms >= t))
        .filter(|r| {
            q.include_frames
                // An unknown main frame (no commit seen yet) must not hide
                // the very navigation being asked about.
                || main.is_none()
                || r.frame_id.is_none()
                || r.frame_id == main
        })
        .filter(|r| {
            q.url_pattern.as_deref().is_none_or(|p| {
                r.hops
                    .iter()
                    .any(|h| crate::response_headers::url_matches(&h.url, p))
            })
        })
        .map(|r| navigation_json(&s, r, q.with_headers, q.redact))
        .collect();
    let start = out.len().saturating_sub(q.max.max(1));
    out.drain(..start);
    out
}

fn navigation_json(s: &Store, r: &RequestRecord, with_headers: bool, redact: bool) -> Value {
    let last = r.last_hop();
    let outcome = last.map(|h| h.kind.as_str()).unwrap_or("pending");
    let challenge = r.hops.iter().find_map(|h| h.challenge.clone());
    let mut v = json!({
        "seq": r.seq,
        "request_id": r.request_id,
        "loader_id": r.loader_id,
        "frame_id": r.frame_id,
        "main_frame": s.is_main_frame(r.frame_id.as_deref()),
        "ts_ms": r.ts_ms,
        "method": r.method,
        "initiator": r.initiator_type,
        "requested_url": r.hops.first().map(|h| h.url.clone()),
        "final_url": r.url(),
        "status": last.and_then(|h| h.status),
        "outcome": outcome,
        "redirects": r.hops.len().saturating_sub(1),
        "challenge": challenge,
        "committed": r.committed_ms.is_some(),
        "committed_ms": r.committed_ms,
        "committed_url": r.committed_url,
        "error_text": last.and_then(|h| h.error_text.clone()),
        "hops": r.hops.iter().map(|h| hop_json(h, with_headers, redact)).collect::<Vec<_>>(),
    });
    // `status` and `challenge` stay even when null: "no status yet" and "no
    // challenge seen" are answers. The rest is noise when empty.
    if let Some(obj) = v.as_object_mut() {
        obj.retain(|k, val| !val.is_null() || k == "status" || k == "challenge");
        // Only a cancellation is worth a field: another load replaced this one.
        if r.canceled {
            obj.insert("canceled".into(), json!(true));
        }
    }
    v
}

/// Options for [`failed_requests`].
#[derive(Debug, Clone, Default)]
pub struct FailQuery {
    pub since_ms: Option<u64>,
    pub url_pattern: Option<String>,
    pub resource_type: Option<String>,
    /// Include HTTP responses with status >= 400 (default true at the tool).
    pub http_errors: bool,
    /// Include `net::ERR_ABORTED`-style cancellations (navigation-away noise).
    pub include_canceled: bool,
    pub max: usize,
    pub with_headers: bool,
    pub redact: bool,
}

/// Every request that failed at the network layer (blocked, CORS, DNS, TLS,
/// aborted) or, optionally, came back with an HTTP error — with its URL, which
/// `Network.loadingFailed` itself never carries.
pub fn failed_requests(q: &FailQuery) -> (Vec<Value>, Value) {
    let Ok(s) = STORE.lock() else {
        return (Vec::new(), json!({}));
    };
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut canceled_hidden = 0usize;
    let mut out = Vec::new();

    // The request ring, plus documents that only the timeline still holds
    // (evicted from the ring by a flood of subresources) — oldest first.
    let mut records: Vec<&RequestRecord> = s
        .order
        .iter()
        .filter_map(|id| s.requests.get(id))
        .chain(
            s.nav_records
                .values()
                .filter(|r| !s.requests.contains_key(&r.request_id)),
        )
        .collect();
    records.sort_by_key(|r| r.seq);

    for r in records {
        let Some(last) = r.last_hop() else { continue };
        let kind = if last.kind == "failed" {
            if r.canceled {
                "canceled"
            } else if last.blocked_reason.is_some() {
                "blocked"
            } else if r.cors_error.is_some() {
                "cors"
            } else {
                "network_error"
            }
        } else if q.http_errors && last.status.is_some_and(|st| st >= 400) {
            "http_error"
        } else {
            continue;
        };
        if q.since_ms.is_some_and(|t| r.ts_ms < t) {
            continue;
        }
        if let Some(p) = q.url_pattern.as_deref() {
            if !crate::response_headers::url_matches(r.url(), p) {
                continue;
            }
        }
        if let Some(t) = q.resource_type.as_deref() {
            if !r
                .resource_type
                .as_deref()
                .is_some_and(|rt| rt.eq_ignore_ascii_case(t))
            {
                continue;
            }
        }
        if kind == "canceled" && !q.include_canceled {
            canceled_hidden += 1;
            continue;
        }
        *by_kind.entry(kind.to_string()).or_default() += 1;
        *by_type
            .entry(r.resource_type.clone().unwrap_or_else(|| "Other".into()))
            .or_default() += 1;

        let mut v = json!({
            "kind": kind,
            "ts_ms": r.ts_ms,
            "request_id": r.request_id,
            "url": r.url(),
            "method": r.method,
            "resource_type": r.resource_type,
            "status": last.status,
            "error_text": last.error_text,
            "blocked_reason": last.blocked_reason,
            "cors_error": r.cors_error,
            "frame_id": r.frame_id,
            "main_frame": s.is_main_frame(r.frame_id.as_deref()),
            "initiator": r.initiator_type,
            "initiator_url": r.initiator_url,
            "redirects": r.hops.len().saturating_sub(1),
            "challenge": last.challenge,
        });
        if let Some(obj) = v.as_object_mut() {
            // Most fields are empty for any one failure kind; a list of
            // fifty entries is read by a model, and nulls are pure noise there.
            obj.retain(|k, val| !val.is_null() || k == "status");
            if q.with_headers {
                obj.insert("hop".into(), hop_json(last, true, q.redact));
            }
        }
        out.push(v);
    }

    let total = out.len();
    let start = total.saturating_sub(q.max.max(1));
    out.drain(..start);
    let summary = json!({
        "matched": total,
        "by_kind": by_kind,
        "by_resource_type": by_type,
        "canceled_hidden": canceled_hidden,
        "requests_indexed": s.requests.len(),
        "requests_evicted": s.dropped_requests,
    });
    (out, summary)
}

/// Indexed requests of the given CDP resource types (case-insensitive; empty =
/// all) that started at or after `since_ms`, oldest first. Headers are raw —
/// the caller decides about redaction.
pub fn requests(types: &[&str], since_ms: u64) -> Vec<Value> {
    let Ok(s) = STORE.lock() else {
        return Vec::new();
    };
    s.order
        .iter()
        .filter_map(|id| s.requests.get(id))
        .filter(|r| r.ts_ms >= since_ms)
        .filter(|r| {
            types.is_empty()
                || r.resource_type
                    .as_deref()
                    .is_some_and(|t| types.iter().any(|w| w.eq_ignore_ascii_case(t)))
        })
        .map(|r| {
            let last = r.last_hop();
            let failed = last.is_some_and(|h| h.kind == "failed");
            json!({
                "request_id": r.request_id,
                "ts_ms": r.ts_ms,
                "method": r.method,
                "url": r.url(),
                "resource_type": r.resource_type,
                "status": last.and_then(|h| h.status),
                "mime_type": last.and_then(|h| h.mime_type.clone()),
                "finished_ms": r.finished_ms,
                "duration_ms": r.finished_ms.map(|f| f.saturating_sub(r.ts_ms)),
                "error_text": last.and_then(|h| h.error_text.clone()),
                "canceled": r.canceled,
                "pending": r.finished_ms.is_none() && !failed,
                "request_headers": last.map(|h| h.request_headers.clone()),
                "frame_id": r.frame_id,
            })
        })
        .collect()
}

/// Redact a header map the way every network view does.
pub fn redact_headers(map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    redact_map(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The store is a process-wide static; tests that touch it run serially
    // under this lock so they cannot see each other's requests.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn rws(id: &str, url: &str, ty: &str, frame: &str, redirect: Option<Value>) -> Value {
        let mut v = json!({
            "requestId": id,
            "loaderId": format!("L-{id}"),
            "request": { "url": url, "method": "GET", "headers": { "User-Agent": "x" } },
            "type": ty,
            "frameId": frame,
            "initiator": { "type": "other" },
        });
        if let Some(r) = redirect {
            v["redirectResponse"] = r;
        }
        v
    }

    #[test]
    fn redirect_hops_are_kept_with_their_own_status_and_headers() {
        let _g = SERIAL.lock().unwrap();
        clear();
        set_main_frame_id("MAIN");
        let before = last_seq();

        ingest(
            "Network.requestWillBeSent",
            &rws("1", "https://a.test/", "Document", "MAIN", None),
        );
        // Wire headers for the 302 arrive before the redirect is announced.
        ingest(
            "Network.responseReceivedExtraInfo",
            &json!({ "requestId": "1", "statusCode": 302,
                     "headers": { "Location": "https://a.test/home", "Set-Cookie": "sid=abc; Path=/" } }),
        );
        ingest(
            "Network.requestWillBeSent",
            &rws(
                "1",
                "https://a.test/home",
                "Document",
                "MAIN",
                Some(json!({ "url": "https://a.test/", "status": 302,
                             "headers": { "location": "https://a.test/home" } })),
            ),
        );
        ingest(
            "Network.responseReceived",
            &json!({ "requestId": "1", "type": "Document",
                     "response": { "url": "https://a.test/home", "status": 403,
                                   "headers": { "cf-mitigated": "challenge", "server": "cloudflare" } } }),
        );
        ingest(
            "Page.frameNavigated",
            &json!({ "frame": { "id": "MAIN", "loaderId": "L-1", "url": "https://a.test/home" } }),
        );

        let navs = navigations(&NavQuery {
            after_seq: Some(before),
            max: 10,
            ..Default::default()
        });
        assert_eq!(navs.len(), 1);
        let n = &navs[0];
        assert_eq!(n["redirects"], 1);
        assert_eq!(n["status"], 403);
        assert_eq!(n["challenge"], "cloudflare:challenge");
        assert_eq!(n["committed"], true);
        assert_eq!(n["main_frame"], true);
        let hops = n["hops"].as_array().unwrap();
        assert_eq!(hops[0]["kind"], "redirect");
        assert_eq!(hops[0]["status"], 302);
        assert_eq!(hops[0]["key_headers"]["location"], "https://a.test/home");
        assert_eq!(hops[0]["set_cookie_names"][0], "sid");
        assert_eq!(hops[1]["kind"], "response");
        assert_eq!(current_url().0.as_deref(), Some("https://a.test/home"));
    }

    #[test]
    fn failures_get_the_url_loading_failed_never_carries() {
        let _g = SERIAL.lock().unwrap();
        clear();
        set_main_frame_id("MAIN");
        ingest(
            "Network.requestWillBeSent",
            &rws("s1", "https://cdn.test/app.js", "Script", "MAIN", None),
        );
        ingest(
            "Network.loadingFailed",
            &json!({ "requestId": "s1", "type": "Script", "errorText": "net::ERR_BLOCKED_BY_CLIENT",
                     "blockedReason": "inspector" }),
        );
        ingest(
            "Network.requestWillBeSent",
            &rws("x1", "https://api.test/q", "Fetch", "MAIN", None),
        );
        ingest(
            "Network.responseReceived",
            &json!({ "requestId": "x1", "type": "Fetch",
                     "response": { "url": "https://api.test/q", "status": 500, "headers": {} } }),
        );
        ingest(
            "Network.requestWillBeSent",
            &rws("c1", "https://ads.test/p", "Image", "MAIN", None),
        );
        ingest(
            "Network.loadingFailed",
            &json!({ "requestId": "c1", "type": "Image", "errorText": "net::ERR_ABORTED", "canceled": true }),
        );

        let (list, summary) = failed_requests(&FailQuery {
            http_errors: true,
            max: 50,
            ..Default::default()
        });
        assert_eq!(list.len(), 2, "{list:?}");
        assert_eq!(list[0]["url"], "https://cdn.test/app.js");
        assert_eq!(list[0]["kind"], "blocked");
        assert_eq!(list[1]["kind"], "http_error");
        assert_eq!(summary["canceled_hidden"], 1);

        let (list, _) = failed_requests(&FailQuery {
            http_errors: false,
            include_canceled: true,
            max: 50,
            ..Default::default()
        });
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|v| v["kind"] == "canceled"));
    }

    #[test]
    fn wire_request_headers_win_over_the_renderer_view() {
        let _g = SERIAL.lock().unwrap();
        clear();
        // ExtraInfo first — the usual order for the very first request.
        ingest(
            "Network.requestWillBeSentExtraInfo",
            &json!({ "requestId": "d1", "headers": { "cookie": "a=1; b=secret", "sec-fetch-mode": "navigate" } }),
        );
        ingest(
            "Network.requestWillBeSent",
            &rws("d1", "https://b.test/", "Document", "F", None),
        );
        let navs = navigations(&NavQuery {
            include_frames: true,
            max: 5,
            with_headers: true,
            redact: true,
            ..Default::default()
        });
        let hop = &navs[0]["hops"][0];
        assert_eq!(hop["request_headers_source"], "wire");
        assert_eq!(hop["request_headers"]["sec-fetch-mode"], "navigate");
        assert_eq!(hop["request_headers"]["cookie"], "a=…[1]; b=…[6]");
    }

    #[test]
    fn iframe_documents_are_hidden_unless_asked_for() {
        let _g = SERIAL.lock().unwrap();
        clear();
        set_main_frame_id("MAIN");
        ingest(
            "Network.requestWillBeSent",
            &rws("t1", "https://top.test/", "Document", "MAIN", None),
        );
        ingest(
            "Network.requestWillBeSent",
            &rws("f1", "https://ad.test/frame", "Document", "CHILD", None),
        );
        let top = navigations(&NavQuery {
            max: 10,
            ..Default::default()
        });
        assert_eq!(top.len(), 1);
        let all = navigations(&NavQuery {
            include_frames: true,
            max: 10,
            ..Default::default()
        });
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["main_frame"], false);
    }

    fn redirect(
        id: &str,
        url: &str,
        prev_url: &str,
        status: i64,
        has_extra: Option<bool>,
    ) -> Value {
        let mut v = rws(
            id,
            url,
            "Document",
            "MAIN",
            Some(json!({ "url": prev_url, "status": status, "headers": {} })),
        );
        if let Some(h) = has_extra {
            v["redirectHasExtraInfo"] = json!(h);
        }
        v
    }

    fn resp_extra(id: &str, status: i64, marker: &str) -> Value {
        json!({ "requestId": id, "statusCode": status, "headers": { "x-marker": marker } })
    }

    fn final_response(id: &str, url: &str, status: i64) -> Value {
        json!({ "requestId": id, "type": "Document",
                "response": { "url": url, "status": status, "headers": {} } })
    }

    fn markers(n: &Value) -> Vec<String> {
        n["hops"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                h["response_headers"]["x-marker"]
                    .as_str()
                    .unwrap_or("-")
                    .to_string()
            })
            .collect()
    }

    fn only_nav() -> Value {
        navigations(&NavQuery {
            include_frames: true,
            max: 5,
            with_headers: true,
            ..Default::default()
        })
        .pop()
        .expect("one navigation")
    }

    #[test]
    fn two_redirects_with_the_same_status_each_keep_their_own_wire_headers() {
        // http → https → www: 301, 301, 200, each ExtraInfo arriving before
        // the next hop is announced (the usual order).
        let _g = SERIAL.lock().unwrap();
        clear();
        ingest(
            "Network.requestWillBeSent",
            &rws("r", "http://a.test/", "Document", "MAIN", None),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("r", 301, "h1"),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect("r", "https://a.test/", "http://a.test/", 301, None),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("r", 301, "h2"),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect("r", "https://www.a.test/", "https://a.test/", 301, None),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("r", 200, "h3"),
        );
        ingest(
            "Network.responseReceived",
            &final_response("r", "https://www.a.test/", 200),
        );
        assert_eq!(markers(&only_nav()), ["h1", "h2", "h3"]);
    }

    #[test]
    fn late_extra_info_is_handed_out_in_hop_order() {
        let _g = SERIAL.lock().unwrap();
        clear();
        ingest(
            "Network.requestWillBeSent",
            &rws("l", "https://a.test/1", "Document", "MAIN", None),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect("l", "https://a.test/2", "https://a.test/1", 302, None),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect("l", "https://a.test/3", "https://a.test/2", 302, None),
        );
        ingest(
            "Network.responseReceived",
            &final_response("l", "https://a.test/3", 200),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("l", 302, "A"),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("l", 302, "B"),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("l", 200, "C"),
        );
        assert_eq!(markers(&only_nav()), ["A", "B", "C"]);
    }

    #[test]
    fn a_revalidated_cache_hit_matches_304_on_the_wire_to_200() {
        let _g = SERIAL.lock().unwrap();
        clear();
        ingest(
            "Network.requestWillBeSent",
            &rws("c", "https://a.test/", "Document", "MAIN", None),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("c", 304, "etag"),
        );
        ingest(
            "Network.responseReceived",
            &final_response("c", "https://a.test/", 200),
        );
        let n = only_nav();
        assert_eq!(markers(&n), ["etag"]);
        assert_eq!(n["hops"][0]["response_headers_source"], "wire");
    }

    #[test]
    fn a_hop_that_will_never_get_wire_headers_is_not_given_anothers() {
        // HSTS internal 301 (no ExtraInfo, ever) followed by a real 301.
        let _g = SERIAL.lock().unwrap();
        clear();
        ingest(
            "Network.requestWillBeSent",
            &rws("h", "http://a.test/", "Document", "MAIN", None),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect("h", "https://a.test/", "http://a.test/", 301, Some(false)),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect(
                "h",
                "https://www.a.test/",
                "https://a.test/",
                301,
                Some(true),
            ),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("h", 301, "real"),
        );
        assert_eq!(markers(&only_nav()), ["-", "real", "-"]);
    }

    #[test]
    fn wire_request_headers_go_to_their_own_hop_in_either_order() {
        let _g = SERIAL.lock().unwrap();
        let req_extra = |id: &str, m: &str| json!({ "requestId": id, "headers": { "x-req": m } });
        let req_markers = |n: &Value| -> Vec<String> {
            n["hops"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    h["request_headers"]["x-req"]
                        .as_str()
                        .unwrap_or("-")
                        .to_string()
                })
                .collect()
        };

        // Late: both hops known before either ExtraInfo.
        clear();
        ingest(
            "Network.requestWillBeSent",
            &rws("q", "https://a.test/1", "Document", "MAIN", None),
        );
        ingest(
            "Network.requestWillBeSent",
            &redirect("q", "https://a.test/2", "https://a.test/1", 302, None),
        );
        ingest("Network.requestWillBeSentExtraInfo", &req_extra("q", "ONE"));
        ingest("Network.requestWillBeSentExtraInfo", &req_extra("q", "TWO"));
        assert_eq!(req_markers(&only_nav()), ["ONE", "TWO"]);

        // Early: each ExtraInfo before the hop it belongs to.
        clear();
        ingest("Network.requestWillBeSentExtraInfo", &req_extra("e", "ONE"));
        ingest(
            "Network.requestWillBeSent",
            &rws("e", "https://a.test/1", "Document", "MAIN", None),
        );
        ingest("Network.requestWillBeSentExtraInfo", &req_extra("e", "TWO"));
        ingest(
            "Network.requestWillBeSent",
            &redirect("e", "https://a.test/2", "https://a.test/1", 302, None),
        );
        assert_eq!(req_markers(&only_nav()), ["ONE", "TWO"]);
    }

    #[test]
    fn a_commit_survives_a_flood_of_iframe_commits() {
        let _g = SERIAL.lock().unwrap();
        clear();
        set_main_frame_id("MAIN");
        ingest(
            "Network.requestWillBeSent",
            &rws("m", "https://a.test/", "Document", "MAIN", None),
        );
        ingest(
            "Page.frameNavigated",
            &json!({ "frame": { "id": "MAIN", "loaderId": "L-m", "url": "https://a.test/" } }),
        );
        for i in 0..(NAVIGATION_CAP * 3) {
            ingest(
                "Page.frameNavigated",
                &json!({ "frame": { "id": format!("F{i}"), "parentId": "MAIN",
                                    "loaderId": format!("X{i}"), "url": "about:blank" } }),
            );
        }
        let n = navigations(&NavQuery {
            max: 5,
            ..Default::default()
        })
        .pop()
        .unwrap();
        assert_eq!(n["committed"], true);
    }

    #[test]
    fn navigate_reports_the_chain_it_requested_even_after_many_others() {
        let _g = SERIAL.lock().unwrap();
        clear();
        set_armed(true);
        set_main_frame_id("MAIN");
        let before = last_seq();
        // A chain from an earlier click lands first, then ours, then twelve
        // client-side navigations.
        ingest(
            "Network.requestWillBeSent",
            &rws("old", "https://a.test/clicked", "Document", "MAIN", None),
        );
        ingest(
            "Network.requestWillBeSent",
            &rws("ours", "https://a.test/want", "Document", "MAIN", None),
        );
        for i in 0..12 {
            ingest(
                "Network.requestWillBeSent",
                &rws(
                    &format!("n{i}"),
                    &format!("https://a.test/{i}"),
                    "Document",
                    "MAIN",
                    None,
                ),
            );
        }
        let r = crate::tools::network::navigation_report(before, Some("https://a.test/want"));
        assert_eq!(r["document"]["requested_url"], "https://a.test/want");
        assert_eq!(r["navigations_during_wait"], 13);
        assert_eq!(r["later_navigations"].as_array().unwrap().len(), 10);
        assert_eq!(r["later_navigations_omitted"], 2);
    }

    #[test]
    fn a_cors_failure_keeps_the_status_its_wire_headers_carried() {
        let _g = SERIAL.lock().unwrap();
        clear();
        ingest(
            "Network.requestWillBeSent",
            &rws("x", "https://api.test/q", "Fetch", "MAIN", None),
        );
        ingest(
            "Network.responseReceivedExtraInfo",
            &resp_extra("x", 200, "cors"),
        );
        ingest(
            "Network.loadingFailed",
            &json!({ "requestId": "x", "type": "Fetch", "errorText": "net::ERR_FAILED",
                     "corsErrorStatus": { "corsError": "MissingAllowOriginHeader" } }),
        );
        let (list, _) = failed_requests(&FailQuery {
            max: 5,
            ..Default::default()
        });
        assert_eq!(list[0]["kind"], "cors");
        assert_eq!(list[0]["status"], 200);
    }

    #[test]
    fn cookie_header_redaction_keeps_names() {
        assert_eq!(redact_cookie_header("a=1; b=xyz"), "a=…[1]; b=…[3]");
        assert_eq!(redact_cookie_header("flag"), "…[4]");
    }
}
