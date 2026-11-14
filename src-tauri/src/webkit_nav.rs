//! The navigation log on WKWebView: what the navigation delegate says, as the
//! `Network.*` and `Page.*` events [`crate::netlog`] already reads.
//!
//! WKWebView has no network event stream, so on macOS `nav-log` had nothing to
//! read. Its delegate does say, for the main frame and for iframes, that a
//! navigation is about to start, what response came back (status, headers,
//! type), and when the document committed, finished or failed. That is a
//! navigation timeline, and netlog builds one from CDP events, so this module
//! only translates: one call per delegate callback in, CDP-shaped events out.
//!
//! What WebKit does not say, and what this therefore cannot: subresource
//! requests, and the status of a redirect's own response (`302`/`301`): only the
//! URL it led to. A redirect hop shows its `location` and no status.
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
    pub status: i64,
    pub mime_type: Option<String>,
    pub headers: BTreeMap<String, String>,
}

/// WKNavigationTypeOther. A server redirect arrives as one; so does a
/// navigation a script or `navigate` started.
const NAVIGATION_TYPE_OTHER: i64 = -1;

const MAIN_FRAME: &str = "main";

#[derive(Debug, Clone)]
struct InFlight {
    request_id: String,
    loader_id: String,
    frame_id: String,
    /// Which iframe, for telling two apart; `0` for the main frame.
    frame_key: usize,
    /// The URL this request is at now: the last hop.
    url: String,
    responded: bool,
}

#[derive(Debug, Default)]
pub struct Tracker {
    next: u64,
    main: Option<InFlight>,
    frames: Vec<InFlight>,
}

impl Tracker {
    pub const fn new() -> Self {
        Tracker {
            next: 0,
            main: None,
            frames: Vec::new(),
        }
    }

    fn id(&mut self, kind: &str) -> String {
        self.next += 1;
        format!("wk-{kind}-{}", self.next)
    }

    fn request_event(
        request: &InFlight,
        method: &str,
        redirect_from: Option<&str>,
        main: bool,
    ) -> Event {
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
        let _ = main;
        Event {
            name: "Network.requestWillBeSent",
            params,
        }
    }

    fn canceled(request: &InFlight, why: &str) -> Event {
        Event {
            name: "Network.loadingFailed",
            params: json!({
                "requestId": request.request_id,
                "type": "Document",
                "errorText": why,
                "canceled": true,
                // The delegate's failure callback has recorded failures
                // already; this one is bookkeeping and must not be recorded
                // as a failed load a second time.
                "alreadyRecorded": true,
            }),
        }
    }

    /// A navigation is about to start: `decidePolicyForNavigationAction`.
    ///
    /// For a redirect WebKit asks about the next URL first and says
    /// `didReceiveServerRedirect` after, so a navigation that arrives while the
    /// frame's previous one has had no response, and is not one the user made
    /// (a link, a form, back/forward), is the next hop of that request.
    pub fn action(
        &mut self,
        url: &str,
        method: &str,
        main: bool,
        frame_key: usize,
        navigation_type: i64,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let slot = if main {
            self.main.as_ref()
        } else {
            self.frames.iter().find(|f| f.frame_key == frame_key)
        };
        if let Some(open) = slot.filter(|f| !f.responded).cloned() {
            if navigation_type == NAVIGATION_TYPE_OTHER {
                let previous = open.url.clone();
                let hop = InFlight {
                    url: url.to_string(),
                    ..open
                };
                events.push(Self::request_event(&hop, method, Some(&previous), main));
                self.store(hop, main);
                return events;
            }
            // Something else was started: the open one will never answer.
            events.push(Self::canceled(&open, "superseded by a newer navigation"));
        }
        let request_id = self.id("req");
        let loader_id = self.id("loader");
        let frame_id = if main {
            MAIN_FRAME.to_string()
        } else {
            format!("frame-{frame_key:x}")
        };
        let request = InFlight {
            request_id,
            loader_id,
            frame_id,
            frame_key,
            url: url.to_string(),
            responded: false,
        };
        events.push(Self::request_event(&request, method, None, main));
        self.store(request, main);
        events
    }

    fn store(&mut self, request: InFlight, main: bool) {
        if main {
            self.main = Some(request);
        } else if let Some(at) = self
            .frames
            .iter()
            .position(|f| f.frame_key == request.frame_key)
        {
            self.frames[at] = request;
        } else {
            self.frames.push(request);
        }
    }

    /// A response came back: `decidePolicyForNavigationResponse`.
    pub fn response(&mut self, response: &Response, main: bool) -> Vec<Event> {
        let mut events = Vec::new();
        let found = if main {
            self.main.clone()
        } else {
            // A response names no frame; the one asking for this URL is it.
            self.frames
                .iter()
                .find(|f| !f.responded && f.url == response.url)
                .cloned()
        };
        let mut request = match found {
            Some(r) => r,
            None => {
                // The navigation began before the log was watching.
                let key = 0;
                let synthetic = InFlight {
                    request_id: self.id("req"),
                    loader_id: self.id("loader"),
                    frame_id: if main {
                        MAIN_FRAME.to_string()
                    } else {
                        "frame-unknown".to_string()
                    },
                    frame_key: key,
                    url: response.url.clone(),
                    responded: false,
                };
                events.push(Self::request_event(&synthetic, "GET", None, main));
                synthetic
            }
        };
        let mut headers = Map::new();
        for (name, value) in &response.headers {
            headers.insert(name.clone(), Value::String(value.clone()));
        }
        events.push(Event {
            name: "Network.responseReceived",
            params: json!({
                "requestId": request.request_id,
                "loaderId": request.loader_id,
                "type": "Document",
                "frameId": request.frame_id,
                "hasExtraInfo": false,
                "response": {
                    "url": response.url,
                    "status": response.status,
                    "mimeType": response.mime_type,
                    "headers": headers,
                },
            }),
        });
        request.url = response.url.clone();
        request.responded = true;
        self.store(request, main);
        events
    }

    /// The main frame's document committed: `didCommitNavigation`.
    pub fn commit(&mut self, url: &str) -> Vec<Event> {
        let loader_id = match &self.main {
            Some(m) => m.loader_id.clone(),
            None => self.id("loader"),
        };
        vec![Event {
            name: "Page.frameNavigated",
            params: json!({ "frame": { "id": MAIN_FRAME, "url": url, "loaderId": loader_id } }),
        }]
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

    /// The main frame's load failed: `didFailProvisionalNavigation`. The failure
    /// itself is recorded by the delegate; this closes the request.
    pub fn failed(&mut self, error: &str, canceled: bool) -> Vec<Event> {
        match self.main.take() {
            Some(m) => vec![Event {
                name: "Network.loadingFailed",
                params: json!({
                    "requestId": m.request_id,
                    "type": "Document",
                    "errorText": error,
                    "canceled": canceled,
                    "alreadyRecorded": true,
                }),
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(|e| e.name).collect()
    }

    fn page(url: &str, status: i64) -> Response {
        Response {
            url: url.to_string(),
            status,
            mime_type: Some("text/html".into()),
            headers: BTreeMap::from([("Content-Type".into(), "text/html".into())]),
        }
    }

    #[test]
    fn a_redirect_chain_is_one_request_with_a_hop_per_url() {
        let mut t = Tracker::default();
        let first = t.action("http://h/", "GET", true, 0, -1);
        let hop2 = t.action("http://h/hop2", "GET", true, 0, -1);
        let hop3 = t.action("http://h/page", "GET", true, 0, -1);
        let done = t.response(&page("http://h/page", 200), true);

        let id = |e: &Event| e.params["requestId"].as_str().unwrap().to_string();
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
    fn a_navigation_after_a_response_is_a_new_request() {
        // The challenge page that reloads itself: 403, then a script navigates.
        let mut t = Tracker::default();
        let a = t.action("http://h/challenge", "GET", true, 0, -1);
        t.response(&page("http://h/challenge", 403), true);
        let b = t.action("http://h/challenge?solved=1", "GET", true, 0, -1);
        assert_ne!(a[0].params["requestId"], b[0].params["requestId"]);
        assert!(b[0].params.get("redirectResponse").is_none());
        assert_eq!(names(&b), ["Network.requestWillBeSent"]);
    }

    #[test]
    fn a_navigation_the_user_made_replaces_one_still_waiting() {
        let mut t = Tracker::default();
        let a = t.action("http://h/slow", "GET", true, 0, -1);
        // A link click (type 0) is not a redirect of the slow page.
        let b = t.action("http://h/other", "GET", true, 0, 0);
        assert_eq!(
            names(&b),
            ["Network.loadingFailed", "Network.requestWillBeSent"]
        );
        assert_eq!(b[0].params["requestId"], a[0].params["requestId"]);
        assert_eq!(b[0].params["canceled"], true);
        assert_ne!(b[1].params["requestId"], a[0].params["requestId"]);
    }

    #[test]
    fn an_iframe_is_tracked_apart_from_the_main_frame() {
        let mut t = Tracker::default();
        let main = t.action("http://h/", "GET", true, 0, -1);
        let frame = t.action("http://other/ad", "GET", false, 0xbeef, -1);
        assert_ne!(main[0].params["requestId"], frame[0].params["requestId"]);
        assert_eq!(frame[0].params["frameId"], "frame-beef");
        // The iframe's response finds its own request by URL.
        let resp = t.response(&page("http://other/ad", 200), false);
        assert_eq!(resp[0].params["requestId"], frame[0].params["requestId"]);
        // The main frame finishing leaves the answered iframe out and itself closed.
        let done = t.finished();
        assert_eq!(done[0].params["requestId"], main[0].params["requestId"]);
    }

    #[test]
    fn a_response_with_no_navigation_before_it_still_makes_a_request() {
        let mut t = Tracker::default();
        let events = t.response(&page("http://h/late", 200), true);
        assert_eq!(
            names(&events),
            ["Network.requestWillBeSent", "Network.responseReceived"]
        );
        assert_eq!(events[0].params["requestId"], events[1].params["requestId"]);
    }

    #[test]
    fn a_failed_load_closes_the_request_without_being_recorded_twice() {
        let mut t = Tracker::default();
        t.action("http://h/down", "GET", true, 0, -1);
        let events = t.failed("could not connect", false);
        assert_eq!(names(&events), ["Network.loadingFailed"]);
        assert_eq!(events[0].params["alreadyRecorded"], true);
        assert_eq!(events[0].params["errorText"], "could not connect");
        // Nothing open any more: a second failure has nothing to close.
        assert!(t.failed("again", false).is_empty());
    }

    #[test]
    fn commit_and_finish_carry_the_loader_and_the_request() {
        let mut t = Tracker::default();
        let req = t.action("http://h/", "GET", true, 0, -1);
        t.response(&page("http://h/", 200), true);
        let commit = t.commit("http://h/");
        assert_eq!(commit[0].params["frame"]["id"], "main");
        assert_eq!(
            commit[0].params["frame"]["loaderId"],
            req[0].params["loaderId"]
        );
        let done = t.finished();
        assert_eq!(done[0].params["requestId"], req[0].params["requestId"]);
    }

    #[test]
    fn response_headers_pass_through_for_the_log_to_redact() {
        let mut t = Tracker::default();
        t.action("http://h/", "GET", true, 0, -1);
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
