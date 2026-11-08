//! macOS: what the WKWebView navigation delegate says about failed loads.
//!
//! wry 0.57 (`WryNavigationDelegate`) does not implement
//! `webView:didFailProvisionalNavigation:withError:`. Without it, a refused
//! certificate left the window on the previous page and `navigate` said
//! `completed`. [`install`] adds it to the delegate's class before the first
//! navigation.
//!
//! Certificates are evaluated on a worker thread, never on the main thread,
//! as Apple asks: an evaluation can fetch over the network.

use crate::main_frame::{self, Failure};
use core_foundation::base::TCFType;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::NSString;
use security_framework::policy::SecPolicy;
use security_framework::secure_transport::SslProtocolSide;
use security_framework::trust::SecTrust;
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Add the delegate method. Must run before the first navigation. A method
/// that cannot be added is logged and reported, never skipped in silence.
pub fn install(webview: &tauri::WebviewWindow) -> Result<(), String> {
    webview
        .with_webview(|wv| unsafe {
            let webview: *mut AnyObject = wv.inner().cast();
            let delegate: *mut AnyObject = msg_send![webview, navigationDelegate];
            if delegate.is_null() {
                tracing::error!("[wk-delegate] the webview has no navigation delegate");
                return;
            }
            let class = (*delegate).class() as *const AnyClass as *mut AnyClass;

            type FailFn = unsafe extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut AnyObject,
                *mut AnyObject,
            );
            let failed = add_method(
                class,
                sel!(webView:didFailProvisionalNavigation:withError:),
                std::mem::transmute::<FailFn, Imp>(did_fail_provisional as FailFn),
                c"v@:@@@",
            );
            main_frame::set_webkit_capture(failed);
            if !failed {
                tracing::error!(
                    "[wk-delegate] could not add didFailProvisionalNavigation: the delegate \
                     already has it. Failed loads are not seen."
                );
            }

            // WebKit reads which methods a delegate has when it is set.
            let _: () =
                msg_send![webview, setNavigationDelegate: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![webview, setNavigationDelegate: delegate];
        })
        .map_err(|e| format!("with_webview error: {e:?}"))
}

unsafe fn add_method(class: *mut AnyClass, sel: Sel, imp: Imp, types: &std::ffi::CStr) -> bool {
    objc2::ffi::class_addMethod(class, sel, imp, types.as_ptr()).as_bool()
}

/// The text of an `NSString`, or `None` for `nil`.
pub(super) unsafe fn ns_string(obj: *mut AnyObject) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    Some((*(obj as *const NSString)).to_string())
}

unsafe extern "C-unwind" fn did_fail_provisional(
    _this: *mut AnyObject,
    _cmd: Sel,
    _webview: *mut AnyObject,
    navigation: *mut AnyObject,
    error: *mut AnyObject,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| record_failure(navigation, error)));
}

unsafe fn record_failure(navigation: *mut AnyObject, error: *mut AnyObject) {
    if error.is_null() {
        return;
    }
    let domain = ns_string(msg_send![error, domain]).unwrap_or_default();
    let code: isize = msg_send![error, code];
    let message = ns_string(msg_send![error, localizedDescription]).unwrap_or_default();
    let info: *mut AnyObject = msg_send![error, userInfo];
    let (failing_url, peer) = if info.is_null() {
        (None, std::ptr::null_mut())
    } else {
        let url_key: &NSString = objc2_foundation::NSURLErrorFailingURLErrorKey;
        let trust_key: &NSString = objc2_foundation::NSURLErrorFailingURLPeerTrustErrorKey;
        let url: *mut AnyObject = msg_send![info, objectForKey: url_key];
        let url = if url.is_null() {
            None
        } else {
            ns_string(msg_send![url, absoluteString])
        };
        let peer: *mut AnyObject = msg_send![info, objectForKey: trust_key];
        (url, peer)
    };
    let canceled = main_frame::webkit_canceled(&domain, code as i64);
    let wants_trust = !peer.is_null() && !canceled;
    tracing::info!(
        "[wk-delegate] main-frame load failed: {domain} {code} {:?}",
        failing_url.as_deref().unwrap_or("")
    );
    let seq = main_frame::record(Failure {
        ts_ms: crate::recording::now_ms(),
        navigation: navigation as usize,
        failing_url: failing_url.clone(),
        error_domain: Some(domain),
        error_code: Some(code as i64),
        message,
        trust_pending: wants_trust,
        canceled,
        ..Default::default()
    });
    if !wants_trust {
        return;
    }
    let peer = SecTrust::wrap_under_get_rule(peer.cast());
    let host = failing_url
        .as_deref()
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(String::from));
    std::thread::spawn(move || {
        let reason = trust_reason(peer, host.as_deref());
        main_frame::set_trust_error(seq, reason);
    });
}

/// Why Security.framework refuses this server's certificate for `host`.
fn trust_reason(mut trust: SecTrust, host: Option<&str>) -> Option<Value> {
    let _ = trust.set_policy(&SecPolicy::create_ssl(SslProtocolSide::SERVER, host));
    match trust.evaluate_with_error() {
        Ok(()) => None,
        Err(e) => Some(json!({
            "domain": e.domain().to_string(),
            "code": e.code(),
            "message": e.description().to_string(),
        })),
    }
}
