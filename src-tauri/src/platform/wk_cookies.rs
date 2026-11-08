//! macOS: the WKWebView cookie jar (`WKHTTPCookieStore`).
//!
//! Each `NSHTTPCookie` comes back in the shape CDP reports a cookie, so
//! `cookies` prints the same fields on every engine. `document.cookie` hides
//! every `HttpOnly` cookie and every attribute; this store has them all.

use super::wk_delegate::ns_string;
use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::AnyObject;
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// The cookie store of the webview's own data store.
unsafe fn cookie_store(webview: *mut AnyObject) -> *mut AnyObject {
    let config: *mut AnyObject = msg_send![webview, configuration];
    let data: *mut AnyObject = msg_send![config, websiteDataStore];
    msg_send![data, httpCookieStore]
}

/// One `NSHTTPCookie` as a CDP `Network.Cookie`.
unsafe fn cookie_json(c: *mut AnyObject) -> Value {
    let text = |v: *mut AnyObject| ns_string(v).unwrap_or_default();
    let name = text(msg_send![c, name]);
    let value = text(msg_send![c, value]);
    let session: bool = msg_send![c, isSessionOnly];
    let http_only: bool = msg_send![c, isHTTPOnly];
    let secure: bool = msg_send![c, isSecure];
    let expires_date: *mut AnyObject = msg_send![c, expiresDate];
    let expires = if session || expires_date.is_null() {
        -1.0
    } else {
        let t: f64 = msg_send![expires_date, timeIntervalSince1970];
        t
    };
    let mut v = json!({
        "name": name,
        "value": value,
        "domain": text(msg_send![c, domain]),
        "path": text(msg_send![c, path]),
        "expires": expires,
        "size": name.len() + value.len(),
        "httpOnly": http_only,
        "secure": secure,
        "session": session,
    });
    // `nil` both for `SameSite=None` and for no attribute: NSHTTPCookie keeps
    // only `lax` and `strict`.
    let responds: bool = msg_send![c, respondsToSelector: objc2::sel!(sameSitePolicy)];
    if responds {
        match ns_string(msg_send![c, sameSitePolicy]).as_deref() {
            Some("lax") => v["sameSite"] = json!("Lax"),
            Some("strict") => v["sameSite"] = json!("Strict"),
            _ => {}
        }
    }
    v
}

/// `name`, `domain`, and `path`: what identifies one cookie.
fn key_of(c: &Value) -> (String, String, String) {
    let s = |k: &str| c[k].as_str().unwrap_or_default().to_string();
    (s("name"), s("domain"), s("path"))
}

async fn main_window() -> Result<tauri::WebviewWindow, String> {
    crate::get_main_window()
        .await
        .ok_or_else(|| "WEBVIEW_NOT_FOUND: WebView window not available".to_string())
}

/// Every cookie in the jar.
pub async fn all() -> Result<Vec<Value>, String> {
    let window = main_window().await?;
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<Value>>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    window
        .with_webview(move |wv| unsafe {
            let store = cookie_store(wv.inner().cast());
            let block = RcBlock::new(move |cookies: *mut AnyObject| {
                // A panic must not unwind into WebKit; the caller then times out.
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let mut list = Vec::new();
                    if !cookies.is_null() {
                        let n: usize = msg_send![cookies, count];
                        for i in 0..n {
                            let c: *mut AnyObject = msg_send![cookies, objectAtIndex: i];
                            list.push(cookie_json(c));
                        }
                    }
                    if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                        let _ = tx.send(list);
                    }
                }));
            });
            let _: () = msg_send![store, getAllCookies: &*block];
        })
        .map_err(|e| format!("with_webview error: {e:?}"))?;
    match tokio::time::timeout(TIMEOUT, rx).await {
        Ok(Ok(list)) => Ok(list),
        Ok(Err(_)) => Err("WKHTTPCookieStore did not answer".into()),
        Err(_) => Err(format!(
            "WKHTTPCookieStore getAllCookies did not answer within {}s",
            TIMEOUT.as_secs()
        )),
    }
}

enum Msg {
    /// How many deletions were started.
    Started(usize),
    Deleted((String, String, String)),
}

/// Delete these cookies (as [`all`] returned them). Returns the deleted ones
/// and an error per cookie that was not deleted.
pub async fn delete(targets: &[Value]) -> Result<(Vec<Value>, Vec<Value>), String> {
    let window = main_window().await?;
    let wanted: Vec<(String, String, String)> = targets.iter().map(key_of).collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    let wanted_in_block = wanted.clone();
    window
        .with_webview(move |wv| unsafe {
            let store = cookie_store(wv.inner().cast());
            let store_ptr = store as usize;
            let block = RcBlock::new(move |cookies: *mut AnyObject| {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let store = store_ptr as *mut AnyObject;
                    let mut started = 0usize;
                    if !cookies.is_null() {
                        let n: usize = msg_send![cookies, count];
                        for i in 0..n {
                            let c: *mut AnyObject = msg_send![cookies, objectAtIndex: i];
                            let key = key_of(&cookie_json(c));
                            if !wanted_in_block.contains(&key) {
                                continue;
                            }
                            let done_tx = tx.clone();
                            let done = RcBlock::new(move || {
                                let _ = done_tx.send(Msg::Deleted(key.clone()));
                            });
                            let _: () =
                                msg_send![store, deleteCookie: c, completionHandler: &*done];
                            started += 1;
                        }
                    }
                    let _ = tx.send(Msg::Started(started));
                }));
            });
            let _: () = msg_send![store, getAllCookies: &*block];
        })
        .map_err(|e| format!("with_webview error: {e:?}"))?;

    let mut expected: Option<usize> = None;
    let mut done: Vec<(String, String, String)> = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while expected.is_none_or(|n| done.len() < n) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Msg::Started(n))) => expected = Some(n),
            Ok(Some(Msg::Deleted(key))) => done.push(key),
            Ok(None) | Err(_) => break,
        }
    }
    let as_json =
        |(name, domain, _): &(String, String, String)| json!({ "name": name, "domain": domain });
    let deleted: Vec<Value> = done.iter().map(as_json).collect();
    let errors: Vec<Value> = wanted
        .iter()
        .filter(|k| !done.contains(k))
        .map(|k| {
            let mut e = as_json(k);
            e["error"] = json!("WKHTTPCookieStore did not confirm the deletion");
            e
        })
        .collect();
    Ok((deleted, errors))
}
