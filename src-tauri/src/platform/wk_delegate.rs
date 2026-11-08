//! macOS: what the WKWebView navigation delegate says about failed loads and
//! server certificates.
//!
//! wry 0.57 (`WryNavigationDelegate`) implements neither
//! `webView:didFailProvisionalNavigation:withError:` nor
//! `webView:didReceiveAuthenticationChallenge:completionHandler:`. Without the
//! first, a refused certificate left the window on the previous page and
//! `navigate` said `completed`. [`install`] adds both to the delegate's class
//! before the first navigation; the second only with `start --trust-cert`, so
//! without the flag WebKit handles every challenge exactly as before.
//!
//! Certificates are evaluated on a worker thread, never on the main thread,
//! as Apple asks: an evaluation can fetch over the network.

use crate::main_frame::{self, Failure};
use block2::{Block, RcBlock};
use core_foundation::base::TCFType;
use objc2::encode::{Encoding, RefEncode};
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::NSString;
use security_framework::certificate::SecCertificate;
use security_framework::policy::SecPolicy;
use security_framework::secure_transport::SslProtocolSide;
use security_framework::trust::SecTrust;
use serde_json::{json, Value};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

/// `NSURLSessionAuthChallengeDisposition`.
const USE_CREDENTIAL: isize = 0;
const PERFORM_DEFAULT_HANDLING: isize = 1;

/// `SecTrustRef`, with the type encoding Foundation's methods declare, so
/// `msg_send!` can check the call.
#[repr(C)]
struct SecTrustOpaque {
    _private: [u8; 0],
}

unsafe impl RefEncode for SecTrustOpaque {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__SecTrust", &[]));
}

static ANCHORS: OnceLock<Vec<SecCertificate>> = OnceLock::new();

fn anchors() -> &'static [SecCertificate] {
    ANCHORS.get().map(Vec::as_slice).unwrap_or_default()
}

/// Turn the `--trust-cert` certificates into `SecCertificate`s and record
/// what `start` and `list` report. `Err` stops the daemon with the reason.
pub fn load_anchors() -> Result<(), String> {
    let mut certs = Vec::new();
    let mut report = Vec::new();
    for anchor in crate::trust_cert::anchors() {
        let cert = SecCertificate::from_der(&anchor.der).map_err(|e| {
            format!(
                "--trust-cert {}: a block is not an X.509 certificate ({e})",
                anchor.file
            )
        })?;
        // security-framework panics on a certificate without a summary.
        let subject = catch_unwind(AssertUnwindSafe(|| cert.subject_summary()))
            .unwrap_or_else(|_| "(no subject summary)".to_string());
        report.push(json!({
            "subject": subject,
            "sha256": anchor.sha256,
            "file": anchor.file,
        }));
        certs.push(cert);
    }
    let _ = ANCHORS.set(certs);
    crate::trust_cert::set_report(report);
    Ok(())
}

/// Add the delegate methods. Must run before the first navigation. A method
/// that cannot be added is logged and reported, never skipped in silence: a
/// `--trust-cert` daemon that cannot answer challenges stops.
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

            if !anchors().is_empty() {
                type ChallengeFn = unsafe extern "C-unwind" fn(
                    *mut AnyObject,
                    Sel,
                    *mut AnyObject,
                    *mut AnyObject,
                    *mut Block<dyn Fn(isize, *mut AnyObject)>,
                );
                let added = add_method(
                    class,
                    sel!(webView:didReceiveAuthenticationChallenge:completionHandler:),
                    std::mem::transmute::<ChallengeFn, Imp>(did_receive_challenge as ChallengeFn),
                    c"v@:@@@?",
                );
                if !added {
                    let message = "--trust-cert cannot apply: the webview's navigation delegate \
                                   already answers authentication challenges";
                    tracing::error!("[wk-delegate] {message}");
                    let id = crate::daemon::instance_id().unwrap_or_default();
                    let _ = std::fs::write(crate::daemon::startup_error_path(&id), message);
                    std::process::exit(1);
                }
                tracing::info!(
                    "[wk-delegate] {} trust anchor(s) answer server-trust challenges",
                    anchors().len()
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

/// Why Security.framework refuses this server's certificate for `host`, with
/// the `--trust-cert` anchors added as they are for a challenge.
fn trust_reason(mut trust: SecTrust, host: Option<&str>) -> Option<Value> {
    let _ = trust.set_policy(&SecPolicy::create_ssl(SslProtocolSide::SERVER, host));
    add_anchors(&mut trust);
    match trust.evaluate_with_error() {
        Ok(()) => None,
        Err(e) => Some(json!({
            "domain": e.domain().to_string(),
            "code": e.code(),
            "message": e.description().to_string(),
        })),
    }
}

fn add_anchors(trust: &mut SecTrust) {
    if anchors().is_empty() {
        return;
    }
    if let Err(e) = trust.set_anchor_certificates(anchors()) {
        tracing::warn!("[trust-cert] SecTrustSetAnchorCertificates failed: {e}");
    }
    // Keep the system roots: public sites must still load.
    let _ = trust.set_trust_anchor_certificates_only(false);
}

/// The block WebKit passes, held until it is answered on the main thread.
/// WebKit needs exactly one answer, so a reply dropped unanswered (a panic on
/// the way) answers with the default instead.
struct Reply(Option<RcBlock<dyn Fn(isize, *mut AnyObject)>>);
// SAFETY: the block is only called, and dropped, on the main thread: `answer`
// runs there, and `drop` sends an unanswered block there.
unsafe impl Send for Reply {}

impl Reply {
    fn answer(mut self, disposition: isize, credential: *mut AnyObject) {
        if let Some(block) = self.0.take() {
            block.call((disposition, credential));
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        let Some(block) = self.0.take() else {
            return;
        };
        let unanswered = Reply(Some(block));
        if objc2::MainThreadMarker::new().is_some() {
            unanswered.answer(PERFORM_DEFAULT_HANDLING, std::ptr::null_mut());
        } else {
            on_main(move || unanswered.answer(PERFORM_DEFAULT_HANDLING, std::ptr::null_mut()));
        }
    }
}

unsafe extern "C-unwind" fn did_receive_challenge(
    _this: *mut AnyObject,
    _cmd: Sel,
    _webview: *mut AnyObject,
    challenge: *mut AnyObject,
    handler: *mut Block<dyn Fn(isize, *mut AnyObject)>,
) {
    let Some(handler) = handler.as_ref() else {
        return;
    };
    let reply = Reply(Some(handler.copy()));
    let trust = catch_unwind(AssertUnwindSafe(|| server_trust(challenge)))
        .ok()
        .flatten();
    let Some((trust, host)) = trust else {
        // Not a server-trust challenge (HTTP auth, a client certificate):
        // what WebKit does when the delegate has no such method.
        reply.answer(PERFORM_DEFAULT_HANDLING, std::ptr::null_mut());
        return;
    };
    std::thread::spawn(move || {
        let mut trust = trust;
        add_anchors(&mut trust);
        let trusted = match trust.evaluate_with_error() {
            Ok(()) => true,
            Err(e) => {
                tracing::info!(
                    "[trust-cert] {host}: not trusted with the anchors either: {}",
                    e.description()
                );
                false
            }
        };
        on_main(move || unsafe {
            if trusted {
                let trust_ref: *mut SecTrustOpaque = trust.as_concrete_TypeRef().cast();
                let credential: *mut AnyObject =
                    msg_send![class!(NSURLCredential), credentialForTrust: trust_ref];
                reply.answer(USE_CREDENTIAL, credential);
            } else {
                reply.answer(PERFORM_DEFAULT_HANDLING, std::ptr::null_mut());
            }
        });
    });
}

/// The server trust and host of a server-trust challenge, or `None`.
unsafe fn server_trust(challenge: *mut AnyObject) -> Option<(SecTrust, String)> {
    let space: *mut AnyObject = msg_send![challenge, protectionSpace];
    if space.is_null() {
        return None;
    }
    let method = ns_string(msg_send![space, authenticationMethod])?;
    let server_trust: &NSString = objc2_foundation::NSURLAuthenticationMethodServerTrust;
    if method != server_trust.to_string() {
        return None;
    }
    let trust: *mut SecTrustOpaque = msg_send![space, serverTrust];
    if trust.is_null() {
        return None;
    }
    let host = ns_string(msg_send![space, host]).unwrap_or_default();
    Some((SecTrust::wrap_under_get_rule(trust.cast()), host))
}

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(
        queue: *const DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// Run `f` on the main thread, where WebKit's completion handlers belong.
fn on_main(f: impl FnOnce() + Send + 'static) {
    extern "C" fn run(context: *mut c_void) {
        // SAFETY: `context` is the box `on_main` leaked, and runs only once.
        let f = unsafe { Box::from_raw(context.cast::<Box<dyn FnOnce() + Send>>()) };
        let _ = catch_unwind(AssertUnwindSafe(f));
    }
    let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q),
            Box::into_raw(boxed).cast(),
            run,
        );
    }
}
