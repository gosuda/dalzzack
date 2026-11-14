//! The size of the page a window shows, as the window has it, for `resize` and
//! `start` to report.
//!
//! A window may refuse a size (a minimum, a screen edge), and what `--width` and
//! `resize` ask for is the page's size, so a caller who asks for 375 and gets 400
//! has to be told. The size is read natively from the page's webview, with no
//! page JavaScript, and is therefore what the layout really gave it.

use serde_json::{json, Value};
use tauri::{Webview, WebviewWindow};

/// The page's width and height in logical pixels, or `None` without a window.
///
/// With the toolbar the page's webview is exactly the page. Without it, on macOS,
/// the webview reaches under the title bar and WebKit keeps the page out of that
/// strip, so the page is the webview less the title bar.
pub fn page_viewport(window: &WebviewWindow) -> Option<(f64, f64)> {
    let webview: &Webview = window.as_ref();
    let scale = window.scale_factor().ok()?;
    let size = webview.size().ok()?.to_logical::<f64>(scale);
    let under_title_bar = if crate::toolbar::is_enabled() {
        0.0
    } else {
        crate::toolbar::title_inset()
    };
    Some((size.width, (size.height - under_title_bar).max(0.0)))
}

/// [`page_viewport`] of the daemon's own window.
pub async fn current() -> Option<(f64, f64)> {
    let window = crate::state::get_main_window().await?;
    page_viewport(&window)
}

/// What a size request came to: the size asked for, the page's size now, and
/// whether they are the same to within a pixel. When they are not, `note` says
/// which way it went, because the usual reason is a window minimum.
pub fn size_report(requested: (u64, u64), viewport: Option<(f64, f64)>) -> Value {
    let (rw, rh) = requested;
    let Some((vw, vh)) = viewport else {
        return json!({
            "requested": { "width": rw, "height": rh },
            "viewport": null,
            "exact": null,
        });
    };
    let (vw, vh) = (vw.round() as i64, vh.round() as i64);
    let exact = (vw - rw as i64).abs() <= 1 && (vh - rh as i64).abs() <= 1;
    let mut report = json!({
        "requested": { "width": rw, "height": rh },
        "viewport": { "width": vw, "height": vh },
        "exact": exact,
    });
    if !exact {
        report["note"] = json!(format!(
            "The page is {vw}x{vh}, not {rw}x{rh}: the window limits its own size (a minimum, \
             or the screen). Width and height are the page's, so this is what it really is."
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_asked_for_is_exact_to_within_a_pixel() {
        let r = size_report((375, 812), Some((375.0, 812.4)));
        assert_eq!(r["exact"], true);
        assert_eq!(r["viewport"], json!({ "width": 375, "height": 812 }));
        assert!(r.get("note").is_none());
        // Windows can land half a pixel either way.
        assert_eq!(size_report((375, 812), Some((374.6, 811.6)))["exact"], true);
    }

    #[test]
    fn a_size_the_window_would_not_give_is_said_so_with_the_real_one() {
        let r = size_report((300, 812), Some((320.0, 812.0)));
        assert_eq!(r["exact"], false);
        assert_eq!(r["viewport"]["width"], 320);
        let note = r["note"].as_str().unwrap();
        assert!(
            note.contains("320x812") && note.contains("300x812"),
            "{note}"
        );
    }

    #[test]
    fn a_height_off_by_more_than_a_pixel_is_not_exact() {
        // The bare macOS window, whose title bar covers the top of the page.
        assert_eq!(
            size_report((900, 500), Some((900.0, 468.0)))["exact"],
            false
        );
    }

    #[test]
    fn without_a_window_nothing_is_claimed() {
        let r = size_report((375, 812), None);
        assert_eq!(r["exact"], Value::Null);
        assert_eq!(r["viewport"], Value::Null);
    }
}
