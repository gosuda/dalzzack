//! macOS source: snapshots of the page's WKWebView, taken on a timer.
//!
//! `takeSnapshotWithConfiguration:` renders the web view's own content, so the
//! frame is the page and nothing else: not the toolbar, not the title bar, not
//! whatever is on the screen around the window. It works with the window behind
//! other windows or the app in the background, and it does not bring the window
//! forward. Measured on macOS at 1280x720 (animated page, a debug build): 49 to 53
//! frames a second, about 20 ms a frame, in front, in the background and covered.
//! (Capturing the window with `xcap` managed 1.2 a second, which is why it is not
//! used here.)

use super::{push_frame, Failure, Options};
use block2::RcBlock;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::WebviewWindow;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Snapshots in a row that may fail before capture gives up.
const MAX_MISSES: u32 = 10;

pub async fn start(options: &Options, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>, Failure> {
    let window = crate::state::get_main_window()
        .await
        .ok_or_else(|| Failure::new("SCREENCAST_FAILED", "the window is not available"))?;
    let (width, _) = crate::viewport::page_viewport(&window)
        .filter(|(w, h)| *w >= 1.0 && *h >= 1.0)
        .ok_or_else(|| Failure::new("SCREENCAST_FAILED", "the page has no size to record"))?;
    let quality = options.quality;
    // A snapshot is as wide as `snapshotWidth` points times the screen's scale, so
    // ask for the page's width divided by it: the video is then the page's size in
    // CSS pixels (1280x720 for a 1280x720 page) on a Retina screen too, which is
    // also what the Chrome engine's frames are.
    let width = width / window.scale_factor().unwrap_or(1.0).max(1.0);

    // The first frame, taken now: if WebKit gives none, recording cannot work
    // and the caller hears it at `start`, not after a minute of nothing.
    let first = snapshot(&window, width, quality).await.ok_or_else(|| {
        Failure::new(
            "SCREENCAST_FAILED",
            "WKWebView gave no snapshot of the page, so it cannot be recorded",
        )
    })?;
    push_frame(first);

    let interval = Duration::from_millis(1000 / u64::from(options.fps.max(1)));
    Ok(tokio::spawn(async move {
        let mut misses = 0;
        while !stop.load(Ordering::SeqCst) {
            let began = Instant::now();
            match snapshot(&window, width, quality).await {
                Some(bytes) => {
                    misses = 0;
                    push_frame(bytes);
                }
                None => {
                    misses += 1;
                    if misses >= MAX_MISSES {
                        tracing::warn!(
                            "[screencast] WKWebView stopped giving snapshots; capture ends"
                        );
                        return;
                    }
                }
            }
            if let Some(wait) = interval.checked_sub(began.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
    }))
}

/// One JPEG of the page, or `None` if WebKit gave nothing within two seconds.
async fn snapshot(window: &WebviewWindow, width: f64, quality: u8) -> Option<Vec<u8>> {
    let (tx, rx) = oneshot::channel::<Option<Vec<u8>>>();
    let reply = Mutex::new(Some(tx));
    window
        .with_webview(move |webview| unsafe {
            take(webview.inner().cast(), width, quality, reply);
        })
        .ok()?;
    tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .ok()?
        .ok()?
        .filter(|bytes| !bytes.is_empty())
}

/// Ask WebKit for a snapshot. Runs on the main thread, as the call needs.
unsafe fn take(
    webview: *mut AnyObject,
    width: f64,
    quality: u8,
    reply: Mutex<Option<oneshot::Sender<Option<Vec<u8>>>>>,
) {
    let config: *mut AnyObject = msg_send![class!(WKSnapshotConfiguration), new];
    // In points: the page's width, so a frame is the size the page is.
    let number: *mut AnyObject = msg_send![class!(NSNumber), numberWithDouble: width];
    let _: () = msg_send![config, setSnapshotWidth: number];
    let block = RcBlock::new(move |image: *mut AnyObject, _error: *mut AnyObject| {
        let bytes = (!image.is_null()).then(|| jpeg_bytes(image, quality));
        if let Some(tx) = reply.lock().ok().and_then(|mut r| r.take()) {
            let _ = tx.send(bytes);
        }
    });
    let _: () =
        msg_send![webview, takeSnapshotWithConfiguration: config, completionHandler: &*block];
    // `new` handed over a reference, and WebKit keeps its own while it works.
    let _: () = msg_send![config, release];
}

/// An `NSImage` as JPEG bytes (empty when it cannot be converted).
unsafe fn jpeg_bytes(image: *mut AnyObject, quality: u8) -> Vec<u8> {
    let tiff: *mut AnyObject = msg_send![image, TIFFRepresentation];
    if tiff.is_null() {
        return Vec::new();
    }
    let rep: *mut AnyObject = msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff];
    if rep.is_null() {
        return Vec::new();
    }
    let key = NSString::from_str("NSImageCompressionFactor");
    let factor: *mut AnyObject =
        msg_send![class!(NSNumber), numberWithDouble: f64::from(quality) / 100.0];
    let properties: *mut AnyObject =
        msg_send![class!(NSDictionary), dictionaryWithObject: factor, forKey: &*key];
    // NSBitmapImageFileTypeJPEG
    let data: *mut AnyObject =
        msg_send![rep, representationUsingType: 3usize, properties: properties];
    if data.is_null() {
        return Vec::new();
    }
    let len: usize = msg_send![data, length];
    let bytes: *const u8 = msg_send![data, bytes];
    std::slice::from_raw_parts(bytes, len).to_vec()
}
