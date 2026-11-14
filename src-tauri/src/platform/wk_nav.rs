//! macOS: the navigation log's source. It wraps two methods of wry's WKWebView
//! navigation delegate, to see each navigation that starts and each response
//! that comes back, and always calls the method it wrapped, so wry (and the
//! `on_navigation` it serves) behaves exactly as before. What the delegate says
//! goes through [`crate::webkit_nav`], which turns it into the events
//! [`crate::netlog`] reads.
//!
//! No page JavaScript is involved: this is the browser telling its host about
//! navigations, and the page cannot observe it.
//!
//! The wrapping is the fragile part, since it relies on wry's delegate having
//! those two methods. [`install`] checks that both took effect and only then
//! arms the log; otherwise `nav-log` says it is unavailable instead of showing
//! an empty timeline that would read as "nothing navigated".

use crate::webkit_nav::{Event, Response, Tracker};
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send, sel};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static ORIGINAL_ACTION: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_RESPONSE: AtomicUsize = AtomicUsize::new(0);
/// The page's WKWebView. The toolbar's webview shares the delegate class and is
/// not part of the log.
static PAGE: AtomicUsize = AtomicUsize::new(0);
static TRACKER: Mutex<Tracker> = Mutex::new(Tracker::new());

/// The signature of both wrapped methods:
/// `(self, _cmd, webView, navigationAction | navigationResponse, decisionHandler)`.
type PolicyFn = unsafe extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *mut AnyObject,
    *mut AnyObject,
    *mut AnyObject,
);

fn feed(step: impl FnOnce(&mut Tracker) -> Vec<Event>) {
    let events = {
        let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        step(&mut tracker)
    };
    for event in events {
        crate::netlog::ingest(event.name, &event.params);
    }
}

/// The page's document committed (Tauri's page-load `Started`).
pub fn page_committed(url: &str) {
    if PAGE.load(Ordering::SeqCst) != 0 && is_logged(url) {
        feed(|t| t.commit(url));
    }
}

/// The page finished loading (Tauri's page-load `Finished`).
pub fn page_finished() {
    if PAGE.load(Ordering::SeqCst) != 0 {
        feed(|t| t.finished());
    }
}

/// A main-frame load failed (the delegate's own failure callback saw it).
pub fn load_failed(error: &str, canceled: bool) {
    if PAGE.load(Ordering::SeqCst) != 0 {
        feed(|t| t.failed(error, canceled));
    }
}

/// The log is for pages. The landing page and the toolbar are `data:` URLs.
fn is_logged(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://")
}

unsafe fn text(obj: *mut AnyObject) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let is_string: bool = msg_send![obj, isKindOfClass: class!(NSString)];
    is_string
        .then(|| super::wk_delegate::ns_string(obj))
        .flatten()
}

/// An `NSDictionary` of strings, as a map. Other keys and values are skipped.
unsafe fn string_pairs(dict: *mut AnyObject) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if dict.is_null() {
        return out;
    }
    let keys: *mut AnyObject = msg_send![dict, allKeys];
    let count: usize = msg_send![keys, count];
    for i in 0..count {
        let key: *mut AnyObject = msg_send![keys, objectAtIndex: i];
        let value: *mut AnyObject = msg_send![dict, objectForKey: key];
        if let (Some(k), Some(v)) = (text(key), text(value)) {
            out.insert(k, v);
        }
    }
    out
}

unsafe fn on_action(webview: *mut AnyObject, action: *mut AnyObject) {
    if webview as usize != PAGE.load(Ordering::SeqCst) {
        return;
    }
    let request: *mut AnyObject = msg_send![action, request];
    if request.is_null() {
        return;
    }
    let url_object: *mut AnyObject = msg_send![request, URL];
    let Some(url) = text(msg_send![url_object, absoluteString]).filter(|u| is_logged(u)) else {
        return;
    };
    let method = text(msg_send![request, HTTPMethod]).unwrap_or_else(|| "GET".to_string());
    // A navigation with no target frame opens a new window: not this page's.
    let target: *mut AnyObject = msg_send![action, targetFrame];
    if target.is_null() {
        return;
    }
    let main: bool = msg_send![target, isMainFrame];
    let kind: isize = msg_send![action, navigationType];
    feed(|t| t.action(&url, &method, main, target as usize, kind as i64));
}

unsafe fn on_response(webview: *mut AnyObject, navigation_response: *mut AnyObject) {
    if webview as usize != PAGE.load(Ordering::SeqCst) {
        return;
    }
    let response: *mut AnyObject = msg_send![navigation_response, response];
    if response.is_null() {
        return;
    }
    let url_object: *mut AnyObject = msg_send![response, URL];
    let Some(url) = text(msg_send![url_object, absoluteString]).filter(|u| is_logged(u)) else {
        return;
    };
    let main: bool = msg_send![navigation_response, isForMainFrame];
    // Only an HTTP response has a status and headers; a file's has neither.
    let is_http: bool = msg_send![response, isKindOfClass: class!(NSHTTPURLResponse)];
    let (status, headers) = if is_http {
        let status: isize = msg_send![response, statusCode];
        (
            status as i64,
            string_pairs(msg_send![response, allHeaderFields]),
        )
    } else {
        (0, BTreeMap::new())
    };
    let parsed = Response {
        url,
        status,
        mime_type: text(msg_send![response, MIMEType]),
        headers,
    };
    feed(|t| t.response(&parsed, main));
}

unsafe extern "C-unwind" fn decide_action(
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    action: *mut AnyObject,
    handler: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| on_action(webview, action)));
    call_original(&ORIGINAL_ACTION, this, cmd, webview, action, handler);
}

unsafe extern "C-unwind" fn decide_response(
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    response: *mut AnyObject,
    handler: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| on_response(webview, response)));
    call_original(&ORIGINAL_RESPONSE, this, cmd, webview, response, handler);
}

/// wry's own method, which decides what happens to the navigation.
unsafe fn call_original(
    original: &AtomicUsize,
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    subject: *mut AnyObject,
    handler: *mut AnyObject,
) {
    let address = original.load(Ordering::SeqCst);
    if address != 0 {
        let wrapped: PolicyFn = std::mem::transmute::<usize, PolicyFn>(address);
        wrapped(this, cmd, webview, subject, handler);
    }
}

/// Replace `sel` on `class` with `new`, keeping the method it replaces in `keep`.
/// `false` when the class has no such method: there is then nothing to wrap, and
/// nothing has been changed.
unsafe fn wrap(class: *mut AnyClass, sel: Sel, new: PolicyFn, keep: &AtomicUsize) -> bool {
    let method = objc2::ffi::class_getInstanceMethod(class, sel);
    if method.is_null() {
        return false;
    }
    let Some(original) = objc2::ffi::method_getImplementation(method) else {
        return false;
    };
    keep.store(original as usize, Ordering::SeqCst);
    let types = objc2::ffi::method_getTypeEncoding(method);
    objc2::ffi::class_replaceMethod(class, sel, std::mem::transmute::<PolicyFn, Imp>(new), types);
    true
}

/// Start logging the page's navigations. Called once, before the first one, by
/// `wk_delegate::install`. Arms [`crate::netlog`] only if both methods were
/// wrapped.
///
/// # Safety
///
/// `class` must be the live class of the page's WKWebView navigation delegate,
/// and `page_webview` that WKWebView. Call it on the main thread, before the
/// delegate is next set on the webview, as `wk_delegate::install` does.
pub unsafe fn install(class: *mut AnyClass, page_webview: *mut AnyObject) {
    if ORIGINAL_ACTION.load(Ordering::SeqCst) != 0 {
        return;
    }
    let action = wrap(
        class,
        sel!(webView:decidePolicyForNavigationAction:decisionHandler:),
        decide_action,
        &ORIGINAL_ACTION,
    );
    let response = wrap(
        class,
        sel!(webView:decidePolicyForNavigationResponse:decisionHandler:),
        decide_response,
        &ORIGINAL_RESPONSE,
    );
    if !(action && response) {
        // One wrapper may have taken effect. It is harmless (it calls wry's method),
        // but the log is not armed, so nothing claims it works.
        tracing::error!(
            "[wk-nav] the navigation delegate lacks a method this needs (action: {action}, \
             response: {response}); `nav-log` stays unavailable on this daemon"
        );
        return;
    }
    PAGE.store(page_webview as usize, Ordering::SeqCst);
    crate::netlog::arm_webkit_navigations();
    tracing::info!("[wk-nav] the navigation log is fed by the WKWebView delegate");
}
