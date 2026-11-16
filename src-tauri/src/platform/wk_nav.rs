//! macOS: the navigation log's source. It wraps two methods of wry's WKWebView
//! navigation delegate, to see each navigation that is decided and each response
//! that comes back, and always calls the method it wrapped, so wry (and the
//! `on_navigation` it serves) behaves exactly as before. It also adds the two
//! callbacks that say a main-frame load started and that a server redirected it
//! (wry has neither), which carry the `WKNavigation` the failure callback later
//! names. What the delegate says goes through [`crate::webkit_nav`], which turns
//! it into the events [`crate::netlog`] reads.
//!
//! No page JavaScript is involved: this is the browser telling its host about
//! navigations, and the page cannot observe it.
//!
//! The hooking is the fragile part, since it relies on wry's delegate having the
//! two policy methods. [`install`] checks that every hook took effect and only
//! then arms the log; otherwise `nav-log` says it is unavailable instead of
//! showing an empty timeline that would read as "nothing navigated".

use crate::webkit_nav::{Event, Response, Tracker};
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send, sel};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static ORIGINAL_ACTION: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_RESPONSE: AtomicUsize = AtomicUsize::new(0);
/// The delegate's own `didStartProvisionalNavigation` / server-redirect
/// methods, if a later wry has them: called after ours. `0` when we added them.
static ORIGINAL_STARTED: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_REDIRECTED: AtomicUsize = AtomicUsize::new(0);
/// The page's WKWebView. The toolbar's webview shares the delegate class and is
/// not part of the log.
static PAGE: AtomicUsize = AtomicUsize::new(0);
static TRACKER: Mutex<Tracker> = Mutex::new(Tracker::new());
/// The main-frame navigation the tracker names, retained (a +1 this log owns), so
/// WebKit cannot free it and give its address to a later navigation while the
/// tracker still compares against that address. `0` when none.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// The signature of both policy methods:
/// `(self, _cmd, webView, navigationAction | navigationResponse, decisionHandler)`.
type PolicyFn = unsafe extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *mut AnyObject,
    *mut AnyObject,
    *mut AnyObject,
);

/// The signature of `webView:didStartProvisionalNavigation:` and
/// `webView:didReceiveServerRedirectForProvisionalNavigation:`.
type NavigationFn =
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject);

fn feed(step: impl FnOnce(&mut Tracker) -> Vec<Event>) {
    let events = {
        let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        step(&mut tracker)
    };
    for event in events {
        crate::netlog::ingest(event.name, &event.params);
    }
}

fn is_page(webview: *mut AnyObject) -> bool {
    let page = PAGE.load(Ordering::SeqCst);
    page != 0 && webview as usize == page
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
pub fn load_failed(
    webview: *mut AnyObject,
    navigation: *mut AnyObject,
    error: &str,
    canceled: bool,
) {
    if is_page(webview) {
        tracing::debug!("[wk-nav] failed {navigation:?} canceled={canceled}: {error}");
        feed(|t| t.failed(navigation as usize, error, canceled));
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

unsafe fn url_text(url: *mut AnyObject) -> Option<String> {
    if url.is_null() {
        return None;
    }
    text(msg_send![url, absoluteString])
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
    if !is_page(webview) {
        return;
    }
    let request: *mut AnyObject = msg_send![action, request];
    if request.is_null() {
        return;
    }
    let Some(url) = url_text(msg_send![request, URL]).filter(|u| is_logged(u)) else {
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
    tracing::debug!("[wk-nav] action main={main} type={kind} {method} {url}");
    feed(|t| t.action(&url, &method, main));
}

unsafe fn on_response(webview: *mut AnyObject, navigation_response: *mut AnyObject) {
    if !is_page(webview) {
        return;
    }
    let response: *mut AnyObject = msg_send![navigation_response, response];
    if response.is_null() {
        return;
    }
    let Some(url) = url_text(msg_send![response, URL]).filter(|u| is_logged(u)) else {
        return;
    };
    let main: bool = msg_send![navigation_response, isForMainFrame];
    // Only an HTTP response has a status and headers; a file's has neither.
    let is_http: bool = msg_send![response, isKindOfClass: class!(NSHTTPURLResponse)];
    let (status, headers) = if is_http {
        let status: isize = msg_send![response, statusCode];
        (
            Some(status as i64),
            string_pairs(msg_send![response, allHeaderFields]),
        )
    } else {
        (None, BTreeMap::new())
    };
    tracing::debug!("[wk-nav] response main={main} {status:?} {url}");
    let parsed = Response {
        url,
        status,
        mime_type: text(msg_send![response, MIMEType]),
        headers,
    };
    feed(|t| t.response(&parsed, main));
}

/// Where the page's load is now: `WKWebView.URL`, which WebKit moves to the
/// provisional URL when a load starts and to the new one on a redirect.
unsafe fn page_url(webview: *mut AnyObject) -> Option<String> {
    url_text(msg_send![webview, URL]).filter(|u| is_logged(u))
}

/// Runs `step`, the tracker's update for `navigation`, with that navigation kept
/// alive. It is retained first; the one the tracker named before is released only
/// after `step`, so the tracker never compares against an address that a newer,
/// different navigation could have been given.
unsafe fn while_tracked(navigation: *mut AnyObject, step: impl FnOnce()) {
    if navigation.is_null() {
        return step();
    }
    let _: *mut AnyObject = msg_send![navigation, retain];
    step();
    let previous = HELD.swap(navigation as usize, Ordering::SeqCst) as *mut AnyObject;
    if !previous.is_null() {
        let _: () = msg_send![previous, release];
    }
}

unsafe fn on_started(webview: *mut AnyObject, navigation: *mut AnyObject) {
    if !is_page(webview) {
        return;
    }
    let Some(url) = page_url(webview) else { return };
    tracing::debug!("[wk-nav] started {navigation:?} {url}");
    while_tracked(navigation, || {
        feed(|t| t.started(navigation as usize, &url));
    });
}

unsafe fn on_redirected(webview: *mut AnyObject, navigation: *mut AnyObject) {
    if !is_page(webview) {
        return;
    }
    let Some(url) = page_url(webview) else { return };
    tracing::debug!("[wk-nav] redirected {navigation:?} {url}");
    while_tracked(navigation, || {
        feed(|t| t.redirected(navigation as usize, &url));
    });
}

unsafe extern "C-unwind" fn decide_action(
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    action: *mut AnyObject,
    handler: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| on_action(webview, action)));
    call_policy(&ORIGINAL_ACTION, this, cmd, webview, action, handler);
}

unsafe extern "C-unwind" fn decide_response(
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    response: *mut AnyObject,
    handler: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| on_response(webview, response)));
    call_policy(&ORIGINAL_RESPONSE, this, cmd, webview, response, handler);
}

unsafe extern "C-unwind" fn did_start(
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    navigation: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| on_started(webview, navigation)));
    call_navigation(&ORIGINAL_STARTED, this, cmd, webview, navigation);
}

unsafe extern "C-unwind" fn did_redirect(
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    navigation: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| on_redirected(webview, navigation)));
    call_navigation(&ORIGINAL_REDIRECTED, this, cmd, webview, navigation);
}

/// wry's own policy method, which decides what happens to the navigation.
unsafe fn call_policy(
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

unsafe fn call_navigation(
    original: &AtomicUsize,
    this: *mut AnyObject,
    cmd: Sel,
    webview: *mut AnyObject,
    navigation: *mut AnyObject,
) {
    let address = original.load(Ordering::SeqCst);
    if address != 0 {
        let wrapped: NavigationFn = std::mem::transmute::<usize, NavigationFn>(address);
        wrapped(this, cmd, webview, navigation);
    }
}

/// Replace `sel` on `class` with `new`, keeping the method it replaces in `keep`.
/// `false` when the class has no such method: there is then nothing to wrap, and
/// nothing has been changed.
unsafe fn wrap(class: *mut AnyClass, sel: Sel, new: Imp, keep: &AtomicUsize) -> bool {
    let method = objc2::ffi::class_getInstanceMethod(class, sel);
    if method.is_null() {
        return false;
    }
    let Some(original) = objc2::ffi::method_getImplementation(method) else {
        return false;
    };
    keep.store(original as usize, Ordering::SeqCst);
    let types = objc2::ffi::method_getTypeEncoding(method);
    objc2::ffi::class_replaceMethod(class, sel, new, types);
    true
}

/// Give `class` the optional delegate method `sel`: added when the class lacks
/// it, wrapped (its own still called after) when it has one.
unsafe fn hook(class: *mut AnyClass, sel: Sel, new: NavigationFn, keep: &AtomicUsize) -> bool {
    let imp = std::mem::transmute::<NavigationFn, Imp>(new);
    objc2::ffi::class_addMethod(class, sel, imp, c"v@:@@".as_ptr()).as_bool()
        || wrap(class, sel, imp, keep)
}

/// Start logging the page's navigations. Called once, before the first one, by
/// `wk_delegate::install`. Arms [`crate::netlog`] only if every hook took effect.
///
/// # Safety
///
/// `class` must be the live class of the page's WKWebView navigation delegate,
/// and `page_webview` that WKWebView. Call it on the main thread, before the
/// delegate is next set on the webview, as `wk_delegate::install` does.
pub unsafe fn install(class: *mut AnyClass, page_webview: *mut AnyObject) {
    // Once per process: wrapping twice would make a wrapper its own original.
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let action = wrap(
        class,
        sel!(webView:decidePolicyForNavigationAction:decisionHandler:),
        std::mem::transmute::<PolicyFn, Imp>(decide_action as PolicyFn),
        &ORIGINAL_ACTION,
    );
    let response = wrap(
        class,
        sel!(webView:decidePolicyForNavigationResponse:decisionHandler:),
        std::mem::transmute::<PolicyFn, Imp>(decide_response as PolicyFn),
        &ORIGINAL_RESPONSE,
    );
    let started = hook(
        class,
        sel!(webView:didStartProvisionalNavigation:),
        did_start,
        &ORIGINAL_STARTED,
    );
    let redirected = hook(
        class,
        sel!(webView:didReceiveServerRedirectForProvisionalNavigation:),
        did_redirect,
        &ORIGINAL_REDIRECTED,
    );
    if !(action && response && started && redirected) {
        // What did take effect is harmless (it calls wry's method, or nothing),
        // but the log is not armed, so nothing claims it works.
        tracing::error!(
            "[wk-nav] the navigation delegate could not be hooked (action: {action}, response: \
             {response}, started: {started}, redirected: {redirected}); `nav-log` stays \
             unavailable on this daemon"
        );
        return;
    }
    PAGE.store(page_webview as usize, Ordering::SeqCst);
    crate::netlog::arm_webkit_navigations();
    tracing::info!("[wk-nav] the navigation log is fed by the WKWebView delegate");
}
