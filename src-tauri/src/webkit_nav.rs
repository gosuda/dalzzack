//! The navigation log on WKWebView: what the navigation delegate says, as the
//! `Network.*` and `Page.*` events [`crate::netlog`] already reads.
//!
//! WKWebView has no network event stream, so on macOS `nav-log` had nothing to
//! read. Its delegate does say, for the main frame, when a load starts (with the
//! `WKNavigation` it is), when a server redirects it, what response came back
//! (status, headers, type), and when the document committed, finished or failed;
//! for an iframe, that a load is about to start and what response came back.
//! That is a navigation timeline, and netlog builds one from CDP events, so this
//! module only translates: one call per delegate callback in, CDP-shaped events
//! out.
//!
//! What WebKit does not say, and what this therefore cannot: subresource
//! requests, request headers, the status of a redirect's own response
//! (`302`/`301`): only the URL it led to, so a redirect hop shows its `location`
//! and no status. An iframe has no `WKNavigation`, so its redirects are not
//! joined into one request.
//!
//! The translation is plain data, so it is tested without a window; the macOS
//! glue that calls it is `crate::platform::wk_nav`.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// One event, in the shape of the CDP event of the same name.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub params: Value,
}

/// A navigation response, as the delegate reports it.
#[derive(Debug, Clone, Default)]
pub struct Response {
    pub url: String,
    /// `None` for a response that is not HTTP (a `file://` page).
    pub status: Option<i64>,
    pub mime_type: Option<String>,
    pub headers: BTreeMap<String, String>,
}

const MAIN_FRAME: &str = "main";

/// Iframe loads kept waiting for their response. A page that keeps starting
/// iframes that never answer cannot grow the tracker past this.
const FRAME_CAP: usize = 64;

/// Why an iframe load is closed without a response: WebKit says nothing more
/// about an iframe than its start and its response.
const NO_FRAME_RESPONSE: &str =
    "no response was seen for this frame (it redirected, or the page stopped it)";

#[derive(Debug, Clone)]
struct InFlight {
    request_id: String,
    loader_id: String,
    frame_id: String,
    /// The main frame's `WKNavigation` (its address), which the redirect and the
    /// failure callbacks name. `0` for an iframe, and for a load the log saw
    /// only from its response.
    navigation: usize,
    /// The URL this request is at now: the last hop.
    url: String,
    responded: bool,
}

#[derive(Debug, Default)]
pub struct Tracker {
    next: u64,
    /// The last main-frame navigation action, `(url, method)`: the method the
    /// load it leads to is sent with (a form's POST).
    last_action: Option<(String, String)>,
    main: Option<InFlight>,
    frames: Vec<InFlight>,
}

impl Tracker {
    pub const fn new() -> Self {
        Tracker {
            next: 0,
            last_action: None,
            main: None,
            frames: Vec::new(),
        }
    }

    fn id(&mut self, kind: &str) -> String {
        self.next += 1;
        format!("wk-{kind}-{}", self.next)
    }

    fn request_event(request: &InFlight, method: &str, redirect_from: Option<&str>) -> Event {
        let mut params = json!({
            "requestId": request.request_id,
            "loaderId": request.loader_id,
            "type": "Document",
            "frameId": request.frame_id,
            "documentURL": request.url,
            "request": { "url": request.url, "method": method, "headers": {} },
            "initiator": { "type": "other" },
        });
        if let Some(previous) = redirect_from {
            // The redirected response, as WebKit says it: where it came from and
            // where it led. It reports no status for it.
            params["redirectResponse"] = json!({
                "url": previous,
                "headers": { "location": request.url },
            });
            params["redirectHasExtraInfo"] = json!(false);
        }
        Event {
            name: "Network.requestWillBeSent",
            params,
        }
    }

    fn closed(request: &InFlight, why: &str, canceled: bool) -> Event {
        Event {
            name: "Network.loadingFailed",
            params: json!({
                "requestId": request.request_id,
                "type": "Document",
                "errorText": why,
                "canceled": canceled,
                // The delegate's failure callback records real failures itself;
                // this one closes the request and must not be recorded as a
                // failed load a second time.
                "alreadyRecorded": true,
            }),
        }
    }

    /// The method a main-frame load at `url` is sent with: its action's, when
    /// the last action was for this URL.
    fn method_for(&self, url: &str) -> String {
        match &self.last_action {
            Some((action_url, method)) if action_url == url => method.clone(),
            _ => "GET".to_string(),
        }
    }

    /// A navigation is being decided: `decidePolicyForNavigationAction`.
    ///
    /// For the main frame this only notes the method: the load itself begins at
    /// [`Tracker::started`], which a fragment link, a download and a navigation
    /// the host cancels never reach. An iframe has no later start to wait for,
    /// so its request begins here.
    pub fn action(&mut self, url: &str, method: &str, main: bool) -> Vec<Event> {
        if main {
            self.last_action = Some((url.to_string(), method.to_string()));
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.frames.len() >= FRAME_CAP {
            let oldest = self.frames.remove(0);
            if !oldest.responded {
                events.push(Self::closed(&oldest, NO_FRAME_RESPONSE, true));
            }
        }
        let request_id = self.id("req");
        let request = InFlight {
            frame_id: format!("frame-{}", self.next),
            loader_id: self.id("loader"),
            request_id,
            navigation: 0,
            url: url.to_string(),
            responded: false,
        };
        events.push(Self::request_event(&request, method, None));
        self.frames.push(request);
        events
    }

    /// The main frame began a load: `didStartProvisionalNavigation`. A load
    /// still waiting for its response is replaced by it; WebKit's own failure
    /// for that one comes later and names the old navigation, so it closes
    /// nothing here.
    pub fn started(&mut self, navigation: usize, url: &str) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(open) = &self.main {
            if open.navigation == navigation && navigation != 0 {
                return events;
            }
            if !open.responded {
                events.push(Self::closed(open, "superseded by a newer navigation", true));
            }
        }
        let request = InFlight {
            request_id: self.id("req"),
            loader_id: self.id("loader"),
            frame_id: MAIN_FRAME.to_string(),
            navigation,
            url: url.to_string(),
            responded: false,
        };
        events.push(Self::request_event(&request, &self.method_for(url), None));
        self.main = Some(request);
        events
    }

    /// A server redirected the main frame's load:
    /// `didReceiveServerRedirectForProvisionalNavigation`. `url` is where it
    /// goes now. The same request gets a hop, as CDP reports a redirect.
    pub fn redirected(&mut self, navigation: usize, url: &str) -> Vec<Event> {
        let open = self
            .main
            .clone()
            .filter(|m| m.navigation == navigation && !m.responded);
        let Some(open) = open else {
            // The log did not see this load start.
            return self.started(navigation, url);
        };
        if open.url == url {
            return Vec::new();
        }
        let hop = InFlight {
            url: url.to_string(),
            ..open.clone()
        };
        let events = vec![Self::request_event(
            &hop,
            &self.method_for(url),
            Some(&open.url),
        )];
        self.main = Some(hop);
        events
    }

    /// A response came back: `decidePolicyForNavigationResponse`.
    pub fn response(&mut self, response: &Response, main: bool) -> Vec<Event> {
        let mut events = Vec::new();
        let found = if main {
            self.main.clone().filter(|m| !m.responded)
        } else {
            // A response names no frame; the newest load asking for this URL is
            // it (an older one that never answered was stopped).
            self.frames
                .iter()
                .rev()
                .find(|f| !f.responded && f.url == response.url)
                .cloned()
        };
        let mut request = match found {
            Some(r) => r,
            None => {
                // The load began before the log was watching.
                let synthetic = InFlight {
                    request_id: self.id("req"),
                    loader_id: self.id("loader"),
                    frame_id: if main {
                        MAIN_FRAME.to_string()
                    } else {
                        format!("frame-{}", self.next)
                    },
                    navigation: 0,
                    url: response.url.clone(),
                    responded: false,
                };
                events.push(Self::request_event(&synthetic, "GET", None));
                if !main {
                    self.frames.push(synthetic.clone());
                }
                synthetic
            }
        };
        let mut headers = Map::new();
        for (name, value) in &response.headers {
            headers.insert(name.clone(), Value::String(value.clone()));
        }
        let mut answer = json!({
            "url": response.url,
            "mimeType": response.mime_type,
            "headers": headers,
        });
        if let Some(status) = response.status {
            answer["status"] = json!(status);
        }
        events.push(Event {
            name: "Network.responseReceived",
            params: json!({
                "requestId": request.request_id,
                "loaderId": request.loader_id,
                "type": "Document",
                "frameId": request.frame_id,
                "hasExtraInfo": false,
                "response": answer,
            }),
        });
        request.url = response.url.clone();
        request.responded = true;
        if main {
            self.main = Some(request);
        } else if let Some(slot) = self
            .frames
            .iter_mut()
            .find(|f| f.request_id == request.request_id)
        {
            *slot = request;
        }
        events
    }

    /// The main frame's document committed: `didCommitNavigation`. Every iframe
    /// the log holds belongs to the document this replaces: WebKit reports a new
    /// document's iframes only after its commit, so they are all gone now.
    pub fn commit(&mut self, url: &str) -> Vec<Event> {
        let mut events = Vec::new();
        for f in self.frames.drain(..) {
            if !f.responded {
                events.push(Self::closed(&f, NO_FRAME_RESPONSE, true));
            }
        }
        let loader_id = match &self.main {
            Some(m) => m.loader_id.clone(),
            None => self.id("loader"),
        };
        events.push(Event {
            name: "Page.frameNavigated",
            params: json!({ "frame": { "id": MAIN_FRAME, "url": url, "loaderId": loader_id } }),
        });
        events
    }

    /// The main frame finished loading: `didFinishNavigation`.
    pub fn finished(&mut self) -> Vec<Event> {
        self.frames.retain(|f| !f.responded);
        match self.main.take() {
            Some(m) => vec![Event {
                name: "Network.loadingFinished",
                params: json!({ "requestId": m.request_id }),
            }],
            None => Vec::new(),
        }
    }

    /// A main-frame load failed: `didFailProvisionalNavigation`, which names the
    /// navigation. It closes that request only: the failure of a load that a
    /// newer one already replaced closes nothing. The failure itself is
    /// recorded by the delegate.
    pub fn failed(&mut self, navigation: usize, error: &str, canceled: bool) -> Vec<Event> {
        if !self
            .main
            .as_ref()
            .is_some_and(|m| m.navigation == navigation)
        {
            return Vec::new();
        }
        match self.main.take() {
            Some(m) => vec![Self::closed(&m, error, canceled)],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: usize = 0xa0;
    const B: usize = 0xb0;

    fn names(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(|e| e.name).collect()
    }

    fn id(event: &Event) -> String {
        event.params["requestId"].as_str().unwrap().to_string()
    }

    fn page(url: &str, status: i64) -> Response {
        Response {
            url: url.to_string(),
            status: Some(status),
            mime_type: Some("text/html".into()),
            headers: BTreeMap::from([("Content-Type".into(), "text/html".into())]),
        }
    }

    #[test]
    fn a_redirect_chain_is_one_request_with_a_hop_per_url() {
        let mut t = Tracker::default();
        t.action("http://h/", "GET", true);
        let first = t.started(A, "http://h/");
        t.action("http://h/hop2", "GET", true);
        let hop2 = t.redirected(A, "http://h/hop2");
        t.action("http://h/page", "GET", true);
        let hop3 = t.redirected(A, "http://h/page");
        let done = t.response(&page("http://h/page", 200), true);

        assert_eq!(first.len(), 1);
        assert_eq!(id(&first[0]), id(&hop2[0]));
        assert_eq!(id(&hop2[0]), id(&hop3[0]));
        assert_eq!(id(&hop3[0]), id(&done[0]));

        // The first hop has no redirect in front of it; the next two say where
        // they came from and where that led.
        assert!(first[0].params.get("redirectResponse").is_none());
        assert_eq!(hop2[0].params["redirectResponse"]["url"], "http://h/");
        assert_eq!(
            hop2[0].params["redirectResponse"]["headers"]["location"],
            "http://h/hop2"
        );
        assert_eq!(hop3[0].params["redirectResponse"]["url"], "http://h/hop2");
        // WebKit says nothing of a redirect's own status, so none is claimed.
        assert!(hop2[0].params["redirectResponse"].get("status").is_none());
        assert_eq!(done[0].params["response"]["status"], 200);
    }

    #[test]
    fn a_form_post_answered_with_a_redirect_is_one_chain_from_post_to_get() {
        // A redirect arrives with the navigation type of what started it (a
        // form, a link), so only WebKit's redirect callback can say it is one.
        let mut t = Tracker::default();
        t.action("http://h/submit", "POST", true);
        let post = t.started(A, "http://h/submit");
        t.action("http://h/done", "GET", true);
        let get = t.redirected(A, "http://h/done");
        assert_eq!(post[0].params["request"]["method"], "POST");
        assert_eq!(get[0].params["request"]["method"], "GET");
        assert_eq!(id(&post[0]), id(&get[0]));
        assert_eq!(get[0].params["redirectResponse"]["url"], "http://h/submit");
    }

    #[test]
    fn a_navigation_started_while_one_waits_replaces_it_and_the_late_failure_closes_nothing() {
        let mut t = Tracker::default();
        let slow = t.started(A, "http://h/slow");
        let other = t.started(B, "http://h/other");
        assert_eq!(
            names(&other),
            ["Network.loadingFailed", "Network.requestWillBeSent"]
        );
        assert_eq!(id(&other[0]), id(&slow[0]));
        assert_eq!(other[0].params["canceled"], true);
        assert_ne!(id(&other[1]), id(&slow[0]));

        // WebKit cancels the slow load (-999) after the new one started: that
        // failure names the slow navigation, not the one now open.
        assert!(t.failed(A, "cancelled", true).is_empty());
        let answer = t.response(&page("http://h/other", 200), true);
        assert_eq!(names(&answer), ["Network.responseReceived"]);
        assert_eq!(id(&answer[0]), id(&other[1]));
    }

    #[test]
    fn a_fragment_link_or_a_cancelled_navigation_opens_no_request() {
        // A fragment link, a download, a navigation the host refuses: decided,
        // never started. The next load is a request of its own, not a hop.
        let mut t = Tracker::default();
        t.started(A, "http://h/page");
        t.response(&page("http://h/page", 200), true);
        t.finished();
        assert!(t.action("http://h/page#section", "GET", true).is_empty());
        let next = t.started(B, "http://other/");
        assert_eq!(names(&next), ["Network.requestWillBeSent"]);
        assert!(next[0].params.get("redirectResponse").is_none());
    }

    #[test]
    fn a_navigation_after_a_response_is_a_new_request() {
        // The challenge page that reloads itself: 403, then a script navigates.
        let mut t = Tracker::default();
        let a = t.started(A, "http://h/challenge");
        t.response(&page("http://h/challenge", 403), true);
        let b = t.started(B, "http://h/challenge?solved=1");
        assert_ne!(id(&a[0]), id(&b[0]));
        assert!(b[0].params.get("redirectResponse").is_none());
        assert_eq!(names(&b), ["Network.requestWillBeSent"]);
    }

    #[test]
    fn an_iframe_is_tracked_apart_from_the_main_frame() {
        let mut t = Tracker::default();
        let main = t.started(A, "http://h/");
        let frame = t.action("http://other/ad", "GET", false);
        assert_ne!(id(&main[0]), id(&frame[0]));
        assert_ne!(frame[0].params["frameId"], "main");
        // The iframe's response finds its own request by URL.
        let resp = t.response(&page("http://other/ad", 200), false);
        assert_eq!(id(&resp[0]), id(&frame[0]));
        // The main frame finishing closes itself and lets the answered iframe go.
        let done = t.finished();
        assert_eq!(names(&done), ["Network.loadingFinished"]);
        assert_eq!(id(&done[0]), id(&main[0]));
        assert!(t.frames.is_empty(), "the answered iframe is not kept");
    }

    #[test]
    fn an_iframe_reloaded_after_a_load_that_never_answered_gets_its_own_response() {
        let mut t = Tracker::default();
        t.started(A, "http://h/");
        let dead = t.action("http://ad/x", "GET", false);
        let again = t.action("http://ad/x", "GET", false);
        let resp = t.response(&page("http://ad/x", 200), false);
        assert_eq!(id(&resp[0]), id(&again[0]));
        assert_ne!(id(&resp[0]), id(&dead[0]));
    }

    #[test]
    fn a_new_document_closes_the_iframes_of_the_one_before_and_keeps_its_own() {
        let mut t = Tracker::default();
        t.started(A, "http://h/one");
        let old = t.action("http://ad/old", "GET", false);
        // A load of the new document starts; its iframes are not reported yet.
        t.started(B, "http://h/two");
        let commit = t.commit("http://h/two");
        assert_eq!(
            names(&commit),
            ["Network.loadingFailed", "Page.frameNavigated"]
        );
        assert_eq!(id(&commit[0]), id(&old[0]));
        assert_eq!(commit[0].params["alreadyRecorded"], true);
        assert!(t.frames.is_empty());
        // The new document's iframes come after its commit, and are kept.
        let new = t.action("http://ad/new", "GET", false);
        assert_eq!(t.frames.len(), 1);
        assert_eq!(t.frames[0].request_id, id(&new[0]));
    }

    #[test]
    fn iframes_that_never_answer_cannot_grow_the_tracker_without_bound() {
        let mut t = Tracker::default();
        for i in 0..FRAME_CAP * 3 {
            t.action(&format!("http://ad/{i}"), "GET", false);
        }
        assert_eq!(t.frames.len(), FRAME_CAP);
    }

    #[test]
    fn a_response_with_no_navigation_before_it_still_makes_a_request() {
        let mut t = Tracker::default();
        let events = t.response(&page("http://h/late", 200), true);
        assert_eq!(
            names(&events),
            ["Network.requestWillBeSent", "Network.responseReceived"]
        );
        assert_eq!(id(&events[0]), id(&events[1]));
    }

    #[test]
    fn a_failed_load_closes_its_own_request_once() {
        let mut t = Tracker::default();
        let open = t.started(A, "http://h/down");
        let events = t.failed(A, "could not connect", false);
        assert_eq!(names(&events), ["Network.loadingFailed"]);
        assert_eq!(id(&events[0]), id(&open[0]));
        assert_eq!(events[0].params["alreadyRecorded"], true);
        assert_eq!(events[0].params["errorText"], "could not connect");
        // Nothing open any more: a second failure has nothing to close.
        assert!(t.failed(A, "again", false).is_empty());
    }

    #[test]
    fn commit_and_finish_carry_the_loader_and_the_request() {
        let mut t = Tracker::default();
        let req = t.started(A, "http://h/");
        t.response(&page("http://h/", 200), true);
        let commit = t.commit("http://h/");
        assert_eq!(commit[0].params["frame"]["id"], "main");
        assert_eq!(
            commit[0].params["frame"]["loaderId"],
            req[0].params["loaderId"]
        );
        let done = t.finished();
        assert_eq!(id(&done[0]), id(&req[0]));
    }

    #[test]
    fn a_response_that_is_not_http_claims_no_status() {
        let mut t = Tracker::default();
        t.started(A, "file:///tmp/a.html");
        let file = Response {
            url: "file:///tmp/a.html".into(),
            status: None,
            mime_type: Some("text/html".into()),
            headers: BTreeMap::new(),
        };
        let events = t.response(&file, true);
        assert!(events[0].params["response"].get("status").is_none());
    }

    #[test]
    fn response_headers_pass_through_for_the_log_to_redact() {
        let mut t = Tracker::default();
        t.started(A, "http://h/");
        let mut r = page("http://h/", 200);
        r.headers.insert("Set-Cookie".into(), "sid=secret".into());
        let events = t.response(&r, true);
        // This layer keeps what WebKit said; redaction is netlog's.
        assert_eq!(
            events[0].params["response"]["headers"]["Set-Cookie"],
            "sid=secret"
        );
    }
}
