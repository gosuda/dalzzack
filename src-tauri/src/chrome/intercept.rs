//! `intercept` on the Chrome engine, through CDP's `Fetch` domain.
//!
//! The WebView engine enforces rules with a document-start hook around
//! `fetch`/`XMLHttpRequest`. Real Chrome gets nothing injected into the page —
//! that is the point of the engine — so the browser enforces them instead:
//! `Fetch.enable` pauses matching requests and the daemon answers each one
//! (`continueRequest`, `failRequest`, `fulfillRequest`, or nothing at all for
//! `hang`). The page cannot observe this; `fetch.toString()` stays native.
//!
//! Scope matches the WebView engine: fetch and XHR only (resource types `Fetch`
//! and `XHR`, plus their CORS preflights), so the same rule does the same thing
//! on both engines.

use super::conn::Conn;
use super::Browser;
use crate::intercept::{InterceptRule, INTERCEPT_RULES};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Resource types a rule applies to. A `mock` of a cross-origin request
/// answers its CORS preflight too, instead of the preflight reaching the real
/// server first; Chrome labels that preflight `Fetch` (or `Preflight`).
const IN_SCOPE: [&str; 3] = ["Fetch", "XHR", "Preflight"];

/// A CDP `Fetch.RequestPattern` list for the current rules. By URL only: the
/// filter rejects `Preflight` as a resource type (measured, Chrome 154:
/// "Unknown resource type in fetch filter"), so the scope is checked when a
/// request is paused instead — see [`IN_SCOPE`].
fn patterns(rules: &[InterceptRule]) -> Vec<Value> {
    rules
        .iter()
        .map(|r| {
            let url_pattern = if r.pattern.contains('*') {
                r.pattern.clone()
            } else {
                format!("*{}*", r.pattern)
            };
            json!({ "urlPattern": url_pattern, "requestStage": "Request" })
        })
        .collect()
}

/// Apply the current rule set to one session (the page, or an OOPIF).
pub async fn enable_on_session(conn: &Arc<Conn>, session: &str) -> Result<(), String> {
    let rules = INTERCEPT_RULES.read().await.clone();
    let t = Duration::from_secs(5);
    let call = if rules.is_empty() {
        conn.call(Some(session), "Fetch.disable", json!({}), t)
            .await
    } else {
        conn.call(
            Some(session),
            "Fetch.enable",
            json!({ "patterns": patterns(&rules) }),
            t,
        )
        .await
    };
    call.map(|_| ())
}

/// Push the rule set to the page and every attached frame session.
pub async fn sync(version: u64) -> Result<Value, String> {
    let b = super::current().ok_or_else(super::not_running)?;
    let rules = INTERCEPT_RULES.read().await.clone();
    // The page session is where every rule takes effect; if Chrome refused it,
    // no rule is enforced anywhere and "completed" would be a lie.
    enable_on_session(&b.conn, &b.page_session)
        .await
        .map_err(|e| format!("Chrome refused the rule set (Fetch domain): {e}"))?;
    let children: Vec<String> = super::CHILDREN
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|m| m.keys().cloned().collect()))
        .unwrap_or_default();
    let mut frame_errors = Vec::new();
    for s in &children {
        if let Err(e) = enable_on_session(&b.conn, s).await {
            frame_errors.push(json!({ "session": s, "error": e }));
        }
    }
    Ok(json!({
        "version": version,
        "rule_count": rules.len(),
        "transport": "cdp_fetch",
        "persists_across_navigation": true,
        "applied_to_current_page": true,
        "sessions": 1 + children.len(),
        "frame_errors": frame_errors,
        "note": "Enforced by Chrome itself (CDP Fetch domain) — nothing is injected into the \
                 page. Scope: fetch and XHR (and their CORS preflights), as on the WebView engine.",
    }))
}

fn matches(rule: &InterceptRule, url: &str, method: &str) -> bool {
    crate::response_headers::url_matches(url, &rule.pattern)
        && rule
            .method
            .as_deref()
            .is_none_or(|m| m.eq_ignore_ascii_case(method))
}

/// A request header, whatever its capitalization.
fn header<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params
        .pointer("/request/headers")
        .and_then(|h| h.as_object())
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .and_then(|(_, v)| v.as_str())
        })
}

/// CORS headers for a locally answered request. A credentialed request is
/// rejected by the browser if the answer says `*`, so the request's own origin
/// is echoed back with credentials allowed — the mock should succeed or fail
/// for the page's reasons, not because of how it was mocked.
fn cors_headers(params: &Value) -> Vec<Value> {
    let origin = header(params, "Origin");
    let mut h = vec![json!({
        "name": "Access-Control-Allow-Origin",
        "value": origin.unwrap_or("*"),
    })];
    if origin.is_some() {
        h.push(json!({ "name": "Access-Control-Allow-Credentials", "value": "true" }));
        h.push(json!({ "name": "Vary", "value": "Origin" }));
    }
    h
}

async fn resume(conn: &Arc<Conn>, session: &str, id: &str, why: &str) {
    if let Err(e) = conn
        .call(
            Some(session),
            "Fetch.continueRequest",
            json!({ "requestId": id }),
            Duration::from_secs(10),
        )
        .await
    {
        // Nothing else can release a paused request; say so where it is seen.
        tracing::warn!("[chrome/intercept] continueRequest ({why}) failed: {e}");
    }
}

/// The reason phrase for a mocked status. Chrome refuses `fulfillRequest` for
/// a code it has no standard phrase for unless one is given (measured: 299).
fn reason_phrase(code: u16) -> &'static str {
    reqwest::StatusCode::from_u16(code)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Mocked")
}

/// Answer a paused request locally. If Chrome refuses the answer the request
/// is failed instead: a paused request nobody answers stays pending forever,
/// which looks exactly like a `hang` rule nobody asked for.
async fn fulfill(
    conn: &Arc<Conn>,
    session: &str,
    id: &str,
    code: u16,
    headers: Vec<Value>,
    body: Option<String>,
) {
    let mut p = json!({
        "requestId": id,
        "responseCode": code,
        "responsePhrase": reason_phrase(code),
        "responseHeaders": headers,
    });
    if let Some(body) = body {
        p["body"] = json!(body);
    }
    let call = conn
        .call(
            Some(session),
            "Fetch.fulfillRequest",
            p,
            Duration::from_secs(10),
        )
        .await;
    if let Err(e) = call {
        tracing::warn!(
            "[chrome/intercept] fulfillRequest ({code}) refused, failing the request: {e}"
        );
        fail(conn, session, id, "Failed").await;
    }
}

async fn fail(conn: &Arc<Conn>, session: &str, id: &str, reason: &str) {
    if let Err(e) = conn
        .call(
            Some(session),
            "Fetch.failRequest",
            json!({ "requestId": id, "errorReason": reason }),
            Duration::from_secs(10),
        )
        .await
    {
        tracing::warn!("[chrome/intercept] failRequest ({reason}) failed: {e}");
    }
}

/// Answer one paused request. Runs on its own task: `delay` and `hang` must not
/// hold up the socket reader.
pub fn on_request_paused(b: &Arc<Browser>, session: Option<&str>, params: &Value) {
    let conn = b.conn.clone();
    let session = session.unwrap_or(&b.page_session).to_string();
    let params = params.clone();
    tokio::spawn(async move {
        let Some(id) = params.get("requestId").and_then(|x| x.as_str()) else {
            return;
        };
        let url = params
            .pointer("/request/url")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        let resource_type = params
            .get("resourceType")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if !IN_SCOPE.contains(&resource_type) {
            // A document, script or image whose URL happens to match: the
            // patterns are by URL only, the scope is fetch/XHR.
            resume(&conn, &session, id, "out of scope").await;
            return;
        }
        // Identified by what it is, not by its resource type: Chrome pauses a
        // fetch()'s preflight as resource type `Fetch` (measured, Chrome 154).
        let preflight = resource_type == "Preflight"
            || (params.pointer("/request/method").and_then(|x| x.as_str()) == Some("OPTIONS")
                && header(&params, "Access-Control-Request-Method").is_some());
        // A preflight is matched as the request it announces.
        let method = if preflight {
            header(&params, "Access-Control-Request-Method").unwrap_or("GET")
        } else {
            params
                .pointer("/request/method")
                .and_then(|x| x.as_str())
                .unwrap_or("GET")
        };
        let rule = INTERCEPT_RULES
            .read()
            .await
            .iter()
            .find(|r| matches(r, url, method))
            .cloned();
        let Some(rule) = rule else {
            // Matched a Fetch pattern but not the rule's method filter, or the
            // rule was removed while the request was paused.
            resume(&conn, &session, id, "no rule").await;
            return;
        };
        tracing::info!(
            "[chrome/intercept] {} {}{} → {}",
            method,
            url,
            if preflight { " (preflight)" } else { "" },
            rule.action
        );
        match rule.action.as_str() {
            "mock" if preflight => {
                let mut headers = cors_headers(&params);
                headers.push(json!({ "name": "Access-Control-Allow-Methods", "value": method }));
                if let Some(req) = header(&params, "Access-Control-Request-Headers") {
                    headers.push(json!({ "name": "Access-Control-Allow-Headers", "value": req }));
                }
                fulfill(&conn, &session, id, 204, headers, None).await;
            }
            "fail" => fail(&conn, &session, id, "Failed").await,
            "delay" => {
                tokio::time::sleep(Duration::from_millis(rule.ms.unwrap_or(0))).await;
                resume(&conn, &session, id, "delay").await;
            }
            "hang" => {
                // Never answered: the request stays pending until the page
                // gives up (its own AbortController / timeout), exactly like a
                // black-holed connection. With --ms it errors out instead.
                if let Some(ms) = rule.ms {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    fail(&conn, &session, id, "TimedOut").await;
                }
            }
            "mock" => {
                if let Some(ms) = rule.ms {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                use base64::Engine;
                let body = base64::engine::general_purpose::STANDARD
                    .encode(rule.body.clone().unwrap_or_default().as_bytes());
                let mut headers = vec![json!({
                    "name": "Content-Type",
                    "value": rule
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "application/json".into()),
                })];
                headers.extend(cors_headers(&params));
                fulfill(
                    &conn,
                    &session,
                    id,
                    rule.status.unwrap_or(200),
                    headers,
                    Some(body),
                )
                .await;
            }
            _ => resume(&conn, &session, id, "unknown action").await,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(p: &str, method: Option<&str>) -> InterceptRule {
        InterceptRule {
            pattern: p.into(),
            action: "fail".into(),
            ms: None,
            status: None,
            body: None,
            content_type: None,
            method: method.map(String::from),
        }
    }

    #[test]
    fn substring_rules_become_wildcard_patterns() {
        let p = patterns(&[rule("/api/", None), rule("*://x.test/*", None)]);
        assert_eq!(p.len(), 2, "one pattern per rule");
        assert_eq!(p[0]["urlPattern"], "*/api/*");
        assert_eq!(p[1]["urlPattern"], "*://x.test/*");
        assert!(p.iter().all(|x| x["requestStage"] == "Request"));
        // Chrome rejects the whole Fetch.enable over a `Preflight` resource
        // type, so no pattern may carry one.
        assert!(p.iter().all(|x| x.get("resourceType").is_none()));
    }

    #[test]
    fn method_filter_is_honoured() {
        let r = rule("/orders", Some("POST"));
        assert!(matches(&r, "https://a.test/orders", "post"));
        assert!(!matches(&r, "https://a.test/orders", "GET"));
    }

    #[test]
    fn credentialed_mocks_echo_the_origin() {
        let with_origin = json!({ "request": { "headers": { "origin": "https://app.test" } } });
        let h = cors_headers(&with_origin);
        assert_eq!(h[0]["value"], "https://app.test");
        assert!(h
            .iter()
            .any(|x| x["name"] == "Access-Control-Allow-Credentials"));
        let none = json!({ "request": { "headers": {} } });
        assert_eq!(cors_headers(&none)[0]["value"], "*");
    }

    #[test]
    fn every_mocked_status_carries_a_phrase() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(503), "Service Unavailable");
        // No standard phrase: Chrome refuses the answer without one.
        assert_eq!(reason_phrase(299), "Mocked");
    }
}
