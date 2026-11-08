//! `cookies list|clear` — the browser's cookie jar, including what page script
//! can never see.
//!
//! `document.cookie` hides every `HttpOnly` cookie and only shows the current
//! host's view, and `clear-site-data --types cookies` clears one *origin* —
//! which leaves cookies set on a parent domain (`.example.com`: `_ga`, consent,
//! session cookies shared across subdomains) and every third-party ad cookie
//! exactly where they were, with nothing to check that against. This reads and
//! deletes through CDP's `Network` domain instead, which sees the jar as the
//! network stack does. WKWebView has no CDP: there the jar is its
//! `WKHTTPCookieStore` (`platform::wk_cookies`), in the same shape.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Which cookies an operation applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// Exactly what the browser would send to this URL — parent-domain
    /// cookies included, other hosts' excluded.
    Url(String),
    /// Every cookie whose domain is this one, a subdomain of it, or a parent
    /// of it — "everything this site touches".
    Domain(String),
    /// The whole jar.
    All,
}

impl Scope {
    fn to_json(&self) -> Value {
        match self {
            Scope::Url(u) => json!({ "url": u }),
            Scope::Domain(d) => json!({ "domain": d }),
            Scope::All => json!("all"),
        }
    }
}

/// Is a cookie set on `cookie_domain` related to `domain` — the same host, a
/// subdomain of it, or a parent it would be sent from?
pub(crate) fn domain_related(cookie_domain: &str, domain: &str) -> bool {
    let cd = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    if cd.is_empty() || d.is_empty() {
        return false;
    }
    cd == d || cd.ends_with(&format!(".{d}")) || d.ends_with(&format!(".{cd}"))
}

async fn current_page_url() -> Option<String> {
    if let (Some(u), _) = crate::netlog::current_url() {
        if u.starts_with("http") {
            return Some(u);
        }
    }
    crate::webview_execute_js("return location.href", Some(2000))
        .await
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .filter(|u| u.starts_with("http"))
}

async fn resolve_scope(args: &Value) -> Result<Scope, Value> {
    if args.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Ok(Scope::All);
    }
    if let Some(d) = args
        .get("domain")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Ok(Scope::Domain(d.to_string()));
    }
    if let Some(u) = args
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Ok(Scope::Url(u.to_string()));
    }
    match current_page_url().await {
        Some(u) => Ok(Scope::Url(u)),
        None => Err(json!({
            "status": "failed",
            "error": "NO_SCOPE",
            "message": "the current page has no http(s) URL to scope cookies to — pass --url, \
                        --domain or --all",
        })),
    }
}

/// Would a browser send this cookie (a CDP `Network.Cookie`) to `url`?
/// RFC 6265 section 5.4: the domain, the path, and `Secure`.
pub(crate) fn would_send(cookie: &Value, url: &url::Url) -> bool {
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let domain = cookie["domain"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let domain_ok = match domain.strip_prefix('.') {
        Some(parent) => host == parent || host.ends_with(&format!(".{parent}")),
        None => host == domain,
    };
    let path = cookie["path"].as_str().unwrap_or("/");
    let request = url.path();
    let path_ok = path.is_empty()
        || request == path
        || (request.starts_with(path)
            && (path.ends_with('/') || request[path.len()..].starts_with('/')));
    let secure_ok = cookie["secure"] != true || matches!(url.scheme(), "https" | "wss");
    domain_ok && path_ok && secure_ok
}

/// Does this daemon read its cookies from the WKWebView store?
fn webkit_jar() -> bool {
    cfg!(target_os = "macos") && !crate::engine::is_chrome()
}

/// The WebKit jar, in scope.
async fn fetch_webkit(scope: &Scope) -> Result<Vec<Value>, String> {
    #[cfg(target_os = "macos")]
    let list = crate::platform::wk_cookies::all().await?;
    #[cfg(not(target_os = "macos"))]
    let list: Vec<Value> = Vec::new();
    Ok(match scope {
        Scope::Url(u) => {
            let url = url::Url::parse(u).map_err(|e| format!("invalid --url '{u}': {e}"))?;
            list.into_iter().filter(|c| would_send(c, &url)).collect()
        }
        Scope::Domain(d) => list
            .into_iter()
            .filter(|c| c["domain"].as_str().is_some_and(|cd| domain_related(cd, d)))
            .collect(),
        Scope::All => list,
    })
}

/// Fetch the cookies in scope, as CDP `Network.Cookie` objects.
async fn fetch(scope: &Scope) -> Result<Vec<Value>, String> {
    if !crate::engine::has_cdp() {
        return fetch_webkit(scope).await;
    }
    let raw = match scope {
        Scope::Url(u) => {
            crate::engine::page_cdp("Network.getCookies", &json!({ "urls": [u] }), TIMEOUT).await?
        }
        Scope::Domain(_) | Scope::All => {
            // `getAllCookies` is deprecated in favour of `Storage.getCookies`,
            // but the latter is a browser-target method that not every page
            // session may call. Try both rather than guess.
            match crate::engine::page_cdp("Network.getAllCookies", &json!({}), TIMEOUT).await {
                Ok(v) => v,
                Err(first) => crate::engine::page_cdp("Storage.getCookies", &json!({}), TIMEOUT)
                    .await
                    .map_err(|second| {
                        format!("Network.getAllCookies: {first}; Storage.getCookies: {second}")
                    })?,
            }
        }
    };
    let list = crate::engine::cdp_result(&raw, "cookies")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(match scope {
        Scope::Domain(d) => list
            .into_iter()
            .filter(|c| {
                c.get("domain")
                    .and_then(|x| x.as_str())
                    .is_some_and(|cd| domain_related(cd, d))
            })
            .collect(),
        _ => list,
    })
}

fn filter_name(list: Vec<Value>, name: Option<&str>) -> Vec<Value> {
    match name {
        Some(n) => list
            .into_iter()
            .filter(|c| c.get("name").and_then(|x| x.as_str()) == Some(n))
            .collect(),
        None => list,
    }
}

/// The shape reported per cookie: the CDP object, flattened, with the value
/// masked unless asked for.
fn render(c: &Value, redact: bool) -> Value {
    let s = |k: &str| c.get(k).and_then(|x| x.as_str()).unwrap_or_default();
    let value = s("value");
    let domain = s("domain");
    let expires = c.get("expires").and_then(|x| x.as_f64()).unwrap_or(-1.0);
    let session = c
        .get("session")
        .and_then(|x| x.as_bool())
        .unwrap_or(expires < 0.0);
    let expires_iso = if session || expires <= 0.0 {
        None
    } else {
        chrono::DateTime::from_timestamp(expires as i64, 0).map(|t| t.to_rfc3339())
    };
    let mut v = json!({
        "name": s("name"),
        "value": if redact {
            format!("…[REDACTED {} chars]", value.chars().count())
        } else {
            value.to_string()
        },
        "value_len": value.chars().count(),
        "domain": domain,
        // A cookie without a leading dot was set without `Domain=`: it goes to
        // that exact host only, never to subdomains.
        "host_only": !domain.starts_with('.'),
        "path": s("path"),
        "http_only": c.get("httpOnly").and_then(|x| x.as_bool()).unwrap_or(false),
        "secure": c.get("secure").and_then(|x| x.as_bool()).unwrap_or(false),
        "same_site": c.get("sameSite").cloned().unwrap_or(Value::Null),
        "session": session,
        "expires": expires_iso,
        "size": c.get("size").cloned().unwrap_or(Value::Null),
    });
    if let Some(obj) = v.as_object_mut() {
        for (k, out) in [
            ("partitionKey", "partition_key"),
            ("sourceScheme", "source_scheme"),
            ("priority", "priority"),
        ] {
            if let Some(x) = c.get(k).filter(|x| !x.is_null()) {
                obj.insert(out.into(), x.clone());
            }
        }
        if obj.get("same_site").is_some_and(|x| x.is_null()) {
            obj.remove("same_site");
        }
    }
    v
}

fn by_domain(list: &[Value]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for c in list {
        let d = c
            .get("domain")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string();
        *m.entry(d).or_insert(0) += 1;
    }
    m
}

fn unsupported(msg: &str) -> Value {
    json!({
        "status": "failed",
        "error": "UNSUPPORTED_PLATFORM",
        "message": msg,
        "hint": "Cookies are read through CDP (WebView2 on Windows, or `start --engine chrome`), \
                 or from the WKWebView cookie store on macOS.",
    })
}

fn cdp_failure(e: String) -> Value {
    if e.starts_with("UNSUPPORTED_PLATFORM") {
        return unsupported(&e);
    }
    let error = if crate::engine::has_cdp() {
        "CDP_ERROR"
    } else {
        "COOKIE_STORE_ERROR"
    };
    json!({ "status": "failed", "error": error, "message": e })
}

/// `cookies` tool: `action` = `list` (default) | `clear`.
pub async fn tool_cookies(args: &Value) -> Result<Value> {
    if !crate::engine::has_cdp() && !webkit_jar() {
        return Ok(unsupported(
            "`cookies` needs CDP's Network domain or the WKWebView cookie store, and this \
             engine has neither",
        ));
    }
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("list");
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let redact = !args
        .get("no_redact")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let scope = match resolve_scope(args).await {
        Ok(s) => s,
        Err(v) => return Ok(v),
    };

    match action {
        "list" => {
            let list = match fetch(&scope).await {
                Ok(l) => filter_name(l, name),
                Err(e) => return Ok(cdp_failure(e)),
            };
            let http_only = list
                .iter()
                .filter(|c| c.get("httpOnly").and_then(|x| x.as_bool()) == Some(true))
                .count();
            Ok(json!({
                "status": "completed",
                "scope": scope.to_json(),
                "count": list.len(),
                // The ones `document.cookie` cannot show.
                "http_only_count": http_only,
                "by_domain": by_domain(&list),
                "redacted": redact,
                "profile": crate::profile::to_json(),
                "cookies": list.iter().map(|c| render(c, redact)).collect::<Vec<_>>(),
            }))
        }
        "clear" => clear(scope, name, args).await,
        other => Ok(json!({
            "status": "failed",
            "error": "INVALID_ACTION",
            "message": format!("unknown cookies action '{other}' — use list or clear"),
        })),
    }
}

async fn clear(scope: Scope, name: Option<&str>, args: &Value) -> Result<Value> {
    let flag = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let refusal = crate::profile::guard("Clearing cookies", flag("force_shared")).await;

    // A dry run is never refused: it deletes nothing, and it is how to look
    // before deciding on --force-shared.
    if flag("dry_run") {
        let targets = match fetch(&scope).await {
            Ok(l) => filter_name(l, name),
            Err(e) => return Ok(cdp_failure(e)),
        };
        return Ok(json!({
            "status": "completed",
            "dry_run": true,
            "scope": scope.to_json(),
            "would_delete": targets.len(),
            "by_domain": by_domain(&targets),
            "cookies": targets
                .iter()
                .map(|c| json!({ "name": c["name"], "domain": c["domain"], "path": c["path"] }))
                .collect::<Vec<_>>(),
            "would_be_refused": refusal.is_some(),
            "refusal": refusal,
            "profile": crate::profile::to_json(),
        }));
    }
    if let Some(refusal) = refusal {
        return Ok(refusal);
    }

    let targets = match fetch(&scope).await {
        Ok(l) => filter_name(l, name),
        Err(e) => return Ok(cdp_failure(e)),
    };

    let mut deleted: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    if !crate::engine::has_cdp() {
        #[cfg(target_os = "macos")]
        match crate::platform::wk_cookies::delete(&targets).await {
            Ok((d, e)) => (deleted, errors) = (d, e),
            Err(e) => return Ok(cdp_failure(e)),
        }
    } else if scope == Scope::All && name.is_none() {
        if let Err(e) =
            crate::engine::page_cdp("Network.clearBrowserCookies", &json!({}), TIMEOUT).await
        {
            return Ok(cdp_failure(e));
        }
        deleted = targets
            .iter()
            .map(|c| json!({ "name": c["name"], "domain": c["domain"] }))
            .collect();
    } else {
        for c in &targets {
            // name + exact domain (leading dot included) + path is what
            // identifies a cookie; `url` would also hit same-named cookies on
            // other paths.
            let mut params = json!({
                "name": c["name"],
                "domain": c["domain"],
                "path": c["path"],
            });
            if let Some(pk) = c.get("partitionKey").filter(|x| !x.is_null()) {
                params["partitionKey"] = pk.clone();
            }
            match crate::engine::page_cdp("Network.deleteCookies", &params, TIMEOUT).await {
                Ok(_) => deleted.push(json!({ "name": c["name"], "domain": c["domain"] })),
                Err(e) => {
                    errors.push(json!({ "name": c["name"], "domain": c["domain"], "error": e }))
                }
            }
        }
    }

    // Read the jar back instead of trusting the calls: a delete with a domain
    // that does not match exactly is a silent no-op in CDP.
    // If the re-read itself fails, nothing is verified — that is not success.
    let (remaining, verify_error) = match fetch(&scope).await {
        Ok(l) => (filter_name(l, name), None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let remaining_names: Vec<Value> = remaining
        .iter()
        .map(|c| json!({ "name": c["name"], "domain": c["domain"] }))
        .collect();
    let clean = remaining.is_empty() && errors.is_empty() && verify_error.is_none();
    Ok(json!({
        "status": if clean { "completed" } else { "partial" },
        "scope": scope.to_json(),
        "deleted": deleted.len(),
        "deleted_cookies": deleted,
        "remaining": if verify_error.is_some() { Value::Null } else { json!(remaining_names) },
        "verify_error": verify_error,
        "errors": errors,
        "profile": crate::profile::to_json(),
        "note": "Cookies only. Storage, caches and service workers are untouched — \
                 `clear-site-data` (one origin) or `reset` (everything) cover those.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_relations() {
        // Same host, with or without the leading dot.
        assert!(domain_related(".example.com", "example.com"));
        assert!(domain_related("example.com", "example.com"));
        // A parent-domain cookie is sent to the subdomain.
        assert!(domain_related(".example.com", "www.example.com"));
        // A subdomain's cookie belongs to the site too.
        assert!(domain_related("api.example.com", "example.com"));
        // Suffix tricks are not relations.
        assert!(!domain_related("badexample.com", "example.com"));
        assert!(!domain_related(".example.com", "example.org"));
        assert!(!domain_related("", "example.com"));
    }

    #[test]
    fn a_cookie_is_sent_where_rfc_6265_sends_it() {
        let url = |u: &str| url::Url::parse(u).unwrap();
        let host_only = json!({ "domain": "app.test", "path": "/", "secure": true });
        assert!(would_send(&host_only, &url("https://app.test/login")));
        // Host-only: never to a subdomain, and Secure: never over http.
        assert!(!would_send(&host_only, &url("https://a.app.test/")));
        assert!(!would_send(&host_only, &url("http://app.test/")));

        let parent = json!({ "domain": ".app.test", "path": "/api" });
        assert!(would_send(&parent, &url("http://app.test/api")));
        assert!(would_send(&parent, &url("http://x.app.test/api/v1")));
        // A path is a prefix only up to a `/`.
        assert!(!would_send(&parent, &url("http://app.test/apiv2")));
        assert!(!would_send(&parent, &url("http://otherapp.test/api")));
    }

    #[test]
    fn rendering_masks_values_and_marks_host_only() {
        let c = json!({
            "name": "sid", "value": "secret", "domain": "a.test", "path": "/",
            "expires": -1, "size": 9, "httpOnly": true, "secure": false, "session": true,
            "sameSite": "Lax"
        });
        let r = render(&c, true);
        assert_eq!(r["value"], "…[REDACTED 6 chars]");
        assert_eq!(r["host_only"], true);
        assert_eq!(r["http_only"], true);
        assert!(r["expires"].is_null());
        assert_eq!(render(&c, false)["value"], "secret");

        let p = json!({ "name": "_ga", "value": "x", "domain": ".a.test", "path": "/",
                        "expires": 2_000_000_000.0, "session": false });
        let r = render(&p, true);
        assert_eq!(r["host_only"], false);
        assert!(r["expires"].as_str().unwrap().starts_with("2033-"));
    }
}
