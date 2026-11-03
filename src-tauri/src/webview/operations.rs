//! WebView operation functions (HTML access, form fill, frame access, modals, CDP).

use crate::state::{get_main_window, CDP_CONTEXT_STORE};
use crate::types::*;

/// Get current page HTML.
/// If selector is None, returns full page HTML.
/// If selector is Some, returns that element's outerHTML.
pub async fn webview_get_html(
    selector: Option<&str>,
    timeout_ms: Option<u32>,
) -> Result<Option<String>, String> {
    let script = match selector {
        Some(sel) => {
            format!(
                r#"
                var el = document.querySelector('{}');
                return el ? el.outerHTML : null;
                "#,
                sel.replace('\'', "\\'").replace('"', "\\\"")
            )
        }
        None => "return document.documentElement.outerHTML".to_string(),
    };

    let result = crate::platform::webview_execute_js(&script, timeout_ms).await?;

    if result.is_null() {
        Ok(None)
    } else {
        Ok(result.as_str().map(|s| s.to_string()))
    }
}

/// Get current URL via JS (window.url() panics on macOS)
pub async fn webview_get_url() -> Result<String, String> {
    match crate::platform::webview_execute_js("return window.location.href", Some(3000)).await {
        Ok(val) => Ok(val.as_str().unwrap_or("about:blank").to_string()),
        Err(e) => Err(format!("Failed to get URL: {}", e)),
    }
}

/// Get current page title via JS
pub async fn webview_get_title() -> Result<String, String> {
    match crate::platform::webview_execute_js("return document.title", Some(3000)).await {
        Ok(val) => Ok(val.as_str().unwrap_or("").to_string()),
        Err(e) => Err(format!("Failed to get title: {}", e)),
    }
}

/// Execute JavaScript without return value (fire-and-forget)
pub async fn webview_eval(script: &str) -> Result<(), String> {
    if crate::engine::is_chrome() {
        return crate::chrome::eval_nowait(script).await;
    }
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    window
        .eval(script)
        .map_err(|e| format!("JS_EXECUTION_ERROR: {}", e))
}

/// Check if WebView is ready (or, on the Chrome engine, that Chrome is
/// connected).
pub async fn is_webview_ready() -> bool {
    if crate::engine::is_chrome() {
        return crate::chrome::is_ready();
    }
    get_main_window().await.is_some()
}

/// Wait for a CSS selector to appear in the DOM.
/// Polls every 100ms, returns found=false on timeout.
pub async fn webview_wait_for_selector(
    selector: &str,
    timeout_ms: Option<u32>,
) -> Result<WaitForSelectorResult, String> {
    let timeout = timeout_ms.unwrap_or(10000) as u64;
    let poll_script = format!(
        r#"return document.querySelector('{}') !== null;"#,
        selector.replace('\'', "\\'").replace('"', "\\\"")
    );

    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_millis(timeout);

    loop {
        if let Ok(val) = crate::platform::webview_execute_js(&poll_script, Some(5000)).await {
            if val.as_bool() == Some(true) {
                let elapsed = start.elapsed().as_millis() as u64;
                return Ok(WaitForSelectorResult {
                    found: true,
                    elapsed_ms: elapsed,
                });
            }
        }

        if start.elapsed() >= deadline {
            let elapsed = start.elapsed().as_millis() as u64;
            return Ok(WaitForSelectorResult {
                found: false,
                elapsed_ms: elapsed,
            });
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

/// Fill form fields and optionally submit.
/// Uses nativeInputValueSetter to bypass React/Vue frameworks.
pub async fn webview_fill_form(
    fields: &[(String, String)],
    submit_selector: Option<&str>,
) -> Result<FillFormResult, String> {
    let mut field_scripts = Vec::new();
    for (selector, value) in fields {
        let sel_escaped = selector.replace('\'', "\\'").replace('"', "\\\"");
        let val_escaped = value
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('"', "\\\"");
        field_scripts.push(format!(
            r#"
            (function() {{
                var el = document.querySelector("{sel}");
                if (!el) return false;
                el.focus();
                var proto = el.tagName === 'TEXTAREA'
                    ? window.HTMLTextAreaElement.prototype
                    : window.HTMLInputElement.prototype;
                var setter = Object.getOwnPropertyDescriptor(proto, 'value');
                // React 16+ installs a hidden `_valueTracker` that caches the last
                // value it observed. onChange only fires when that cached value
                // differs from the DOM value — so reset it first to guarantee React
                // (and Vue/Angular) always see a real change. Without this, controlled
                // inputs stay "empty" in framework state and the form blocks submit.
                if (el._valueTracker) {{ el._valueTracker.setValue(''); }}
                if (setter && setter.set) {{
                    setter.set.call(el, "{val}");
                }} else {{
                    el.value = "{val}";
                }}
                // Fire a realistic keystroke sequence. Real keydown/keyup + an
                // InputEvent (not a bare Event) matter for libraries that gate
                // validation on actual key events (react-hook-form, Formik, etc.).
                el.dispatchEvent(new KeyboardEvent('keydown', {{bubbles: true}}));
                el.dispatchEvent(new InputEvent('input', {{bubbles: true, inputType: 'insertText', data: "{val}"}}));
                el.dispatchEvent(new KeyboardEvent('keyup', {{bubbles: true}}));
                el.dispatchEvent(new Event('change', {{bubbles: true}}));
                el.blur();
                return true;
            }})()"#,
            sel = sel_escaped,
            val = val_escaped
        ));
    }

    let fields_count = fields.len();
    let submit_js = if let Some(submit_sel) = submit_selector {
        let sel_escaped = submit_sel.replace('\'', "\\'").replace('"', "\\\"");
        format!(
            r#"
            var btn = document.querySelector("{}");
            if (btn) {{ btn.click(); true; }} else {{ false; }}
            "#,
            sel_escaped
        )
    } else {
        "true".to_string()
    };

    let full_script = format!(
        r#"
        return (function() {{
            var filled = 0;
            var results = [];
            {}
            {}
            return JSON.stringify({{ filled: filled, submitted: {} }});
        }})();
        "#,
        field_scripts
            .iter()
            .enumerate()
            .map(|(i, s)| { format!("results[{}] = {}; if (results[{}]) filled++;", i, s, i) })
            .collect::<Vec<_>>()
            .join("\n"),
        submit_js,
        submit_selector.is_some()
    );

    // A decode failure used to be reported as "0 fields filled, not submitted"
    // — indistinguishable from a form that really did nothing, when the fill may
    // well have succeeded.
    match crate::platform::exec_js_parse::<serde_json::Value>(&full_script, Some(10000)).await {
        Ok(parsed) => Ok(FillFormResult {
            fields_filled: parsed["filled"].as_u64().unwrap_or(0) as usize,
            total_fields: fields_count,
            submitted: parsed["submitted"].as_bool().unwrap_or(false),
        }),
        Err(e) => Err(format!("FILL_FORM_ERROR: {}", e)),
    }
}

/// List all iframes on the page.
///
/// `full = false` truncates each frame's `name` to 200 chars in-page (SafeFrame
/// ad slots carry multi-KB JSON in `name` — measured 4.4/6.1/9.8 KB on naver,
/// over 20 KB per get-frames response) and reports `nameLen`/`nameTruncated` so
/// the cut is visible, same philosophy as set-cookie masking. Truncating in
/// the JS shrinks the bridge payload too, not just the output.
pub async fn webview_get_frame_list(full: bool) -> Result<Vec<FrameInfo>, String> {
    // `return` is load-bearing: `wrap_script` embeds this inside
    // `(function(){ <here> })()`, so a bare IIFE expression is evaluated and
    // discarded — the envelope then carries `value: undefined`. That is why
    // this tool reported an empty frame list on every page, not just ones with
    // awkward frame names.
    let script = r#"
        return (function() {
            const FULL = __FULL__;
            const NAME_CAP = 200;
            const frames = Array.from(document.querySelectorAll('iframe'));
            return JSON.stringify(frames.map((f, idx) => {
                let origin = 'unknown';
                let canAccess = false;

                try {
                    if (f.contentDocument) {
                        origin = 'same-origin';
                        canAccess = true;
                    }
                } catch(e) {
                    origin = 'cross-origin';
                    canAccess = false;
                }

                const rawName = f.name || null;
                const truncated = !FULL && !!rawName && rawName.length > NAME_CAP;
                return {
                    index: idx,
                    id: f.id || null,
                    name: truncated ? rawName.slice(0, NAME_CAP) : rawName,
                    nameLen: rawName ? rawName.length : null,
                    nameTruncated: truncated ? true : null,
                    src: f.src || null,
                    origin: origin,
                    canAccess: canAccess,
                    visible: f.offsetWidth > 0 && f.offsetHeight > 0,
                    width: f.offsetWidth,
                    height: f.offsetHeight
                };
            }));
        })()
    "#
    .replace("__FULL__", if full { "true" } else { "false" });
    let script = script.as_str();

    // A decode failure here used to become `Vec::new()`, which the tool reported
    // as "No iframes on the page" — a confident lie about the page rather than
    // an admission about the bridge.
    crate::platform::exec_js_parse::<Vec<FrameInfo>>(script, Some(5000))
        .await
        .map_err(|e| format!("GET_FRAME_LIST_ERROR: {e}"))
}

/// Get HTML of a specific iframe.
/// macOS: SOP disabled, cross-origin iframes accessible via contentDocument.
/// Windows: Falls back to CDP for cross-origin iframes.
pub async fn webview_get_frame_html(
    frame_selector: Option<&str>,
    frame_index: Option<usize>,
) -> Result<FrameHtmlResult, String> {
    let selector_js = match (frame_selector, frame_index) {
        (Some(sel), _) => format!("document.querySelector('{}')", sel),
        (None, Some(idx)) => format!("document.querySelectorAll('iframe')[{}]", idx),
        (None, None) => return Err("frame_selector or frame_index is required".into()),
    };

    let script = format!(
        r#"
        return (function() {{
            const frame = {selector_js};
            if (!frame) {{
                return JSON.stringify({{ error: 'FRAME_NOT_FOUND' }});
            }}

            let result = {{
                error: null,
                html: null,
                src: frame.src || null,
                origin: 'same-origin',
                id: frame.id || null,
                name: frame.name || null,
                isCrossOrigin: false,
                method: 'contentDocument'
            }};

            try {{
                const frameOrigin = new URL(frame.src).origin;
                const pageOrigin = window.location.origin;
                if (frameOrigin !== pageOrigin) {{
                    result.isCrossOrigin = true;
                    result.origin = 'cross-origin';
                }}
            }} catch(e) {{
            }}

            try {{
                if (frame.contentDocument && frame.contentDocument.documentElement) {{
                    result.html = frame.contentDocument.documentElement.outerHTML;
                    if (result.isCrossOrigin) {{
                        result.method = 'native_sop_disabled';
                    }}
                    return JSON.stringify(result);
                }} else {{
                    result.error = 'CONTENT_NOT_ACCESSIBLE';
                    return JSON.stringify(result);
                }}
            }} catch(e) {{
                result.error = 'ACCESS_ERROR: ' + e.message;
                return JSON.stringify(result);
            }}
        }})()
    "#
    );

    // Previously a decode failure became `{"error":"PARSE_ERROR"}`, which left
    // `html` as `None` and made the tool report a cross-origin access problem
    // (`action_required: "navigate"`) for what was really a bridge bug.
    let parsed: serde_json::Value = crate::platform::exec_js_parse(&script, Some(10000)).await?;

    if let Some(err) = parsed["error"].as_str() {
        if err == "FRAME_NOT_FOUND" {
            return Err("FRAME_NOT_FOUND: specified iframe not found".into());
        }
    }

    let src = parsed["src"].as_str().map(|s| s.to_string());
    let id = parsed["id"].as_str().map(|s| s.to_string());
    let name = parsed["name"].as_str().map(|s| s.to_string());
    let is_cross_origin = parsed["isCrossOrigin"].as_bool().unwrap_or(false);
    let html = parsed["html"].as_str().map(|s| s.to_string());
    let js_error = parsed["error"].as_str().map(|s| s.to_string());

    if html.is_some() {
        let method = parsed["method"]
            .as_str()
            .unwrap_or("contentDocument")
            .to_string();
        let origin = parsed["origin"]
            .as_str()
            .unwrap_or("same-origin")
            .to_string();
        return Ok(FrameHtmlResult {
            html,
            src,
            origin,
            id,
            name,
            is_cross_origin,
            method,
            error: None,
        });
    }

    // CDP/Frame Context fallback for Windows/macOS
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        tracing::info!(
            "[get_frame_html] JS access failed ({}), trying frame context fallback...",
            js_error.as_deref().unwrap_or("unknown")
        );

        if let Some(ref frame_src) = src {
            if let Ok(url) = url::Url::parse(frame_src) {
                let origin_marker = url.origin().ascii_serialization();

                let contexts = CDP_CONTEXT_STORE.read().await;
                for context in contexts.iter() {
                    if context.origin.contains(&origin_marker)
                        || origin_marker.contains(&context.origin)
                    {
                        tracing::debug!(
                            "[Frame Context] origin match: frame_src={}, context_origin={}",
                            frame_src,
                            context.origin
                        );

                        let window = match get_main_window().await {
                            Some(w) => w,
                            None => {
                                tracing::warn!("[Frame Context] WebView window not available");
                                break;
                            }
                        };

                        let expression = "(function() { return document.documentElement ? document.documentElement.outerHTML : ''; })()";
                        if let Ok(Some(html_result)) =
                            crate::webview::evaluate_in_context(&window, context.id, expression)
                                .await
                        {
                            tracing::info!(
                                "[Frame Context] cross-origin iframe HTML retrieved successfully!"
                            );
                            return Ok(FrameHtmlResult {
                                html: Some(html_result),
                                src,
                                origin: "cross-origin".to_string(),
                                id,
                                name,
                                is_cross_origin: true,
                                method: "frame_context".to_string(),
                                error: None,
                            });
                        }
                    }
                }

                tracing::debug!(
                    "[Frame Context] exact origin match failed, trying marker search..."
                );
                drop(contexts);

                let marker = url.host_str().unwrap_or("");
                if !marker.is_empty() {
                    let window = match get_main_window().await {
                        Some(w) => w,
                        None => {
                            tracing::warn!(
                                "[Frame Context] WebView window not available for marker search"
                            );
                            return Ok(FrameHtmlResult {
                                html: None,
                                src,
                                origin: "cross-origin".to_string(),
                                id,
                                name,
                                is_cross_origin: true,
                                method: "failed".to_string(),
                                error: js_error,
                            });
                        }
                    };

                    match crate::webview::search_marker_in_all_frames(
                        &window,
                        &CDP_CONTEXT_STORE,
                        marker,
                    )
                    .await
                    {
                        Ok(Some(html_result)) => {
                            tracing::info!(
                                "[Frame Context] marker search HTML retrieved successfully!"
                            );
                            return Ok(FrameHtmlResult {
                                html: Some(html_result),
                                src,
                                origin: "cross-origin".to_string(),
                                id,
                                name,
                                is_cross_origin: true,
                                method: "frame_context_marker_search".to_string(),
                                error: None,
                            });
                        }
                        _ => {
                            tracing::debug!("[Frame Context] marker search failed");
                        }
                    }
                }
            }
        }

        tracing::warn!("[Frame Context] fallback failed, returning JS error");
    }

    Ok(FrameHtmlResult {
        html: None,
        src,
        origin: if is_cross_origin {
            "cross-origin".to_string()
        } else {
            "unknown".to_string()
        },
        id,
        name,
        is_cross_origin,
        method: "failed".to_string(),
        error: js_error,
    })
}

// ============================================================================
// CDP-based Frame Access
// ============================================================================

/// Get cross-origin iframe HTML via CDP (Windows only)
#[cfg(target_os = "windows")]
pub async fn webview_get_frame_html_via_cdp(
    marker: Option<&str>,
) -> Result<CdpFrameResult, String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    if let Some(marker_str) = marker {
        match crate::webview::search_marker_in_all_frames(&window, &CDP_CONTEXT_STORE, marker_str)
            .await
        {
            Ok(Some(html)) => Ok(CdpFrameResult {
                found: true,
                html: Some(html),
                frames: None,
                method: "cdp".to_string(),
            }),
            Ok(None) => Ok(CdpFrameResult {
                found: false,
                html: None,
                frames: None,
                method: "cdp".to_string(),
            }),
            Err(e) => Err(format!("CDP_ERROR: {}", e)),
        }
    } else {
        match crate::webview::get_all_frames_html(&window, &CDP_CONTEXT_STORE).await {
            Ok(frames) => {
                let frame_results: Vec<CdpFrameInfo> = frames
                    .into_iter()
                    .map(|f| CdpFrameInfo {
                        context_id: f.context_id,
                        origin: f.origin,
                        html: f.html,
                        is_main_frame: f.is_main_frame,
                    })
                    .collect();

                Ok(CdpFrameResult {
                    found: !frame_results.is_empty(),
                    html: None,
                    frames: Some(frame_results),
                    method: "cdp".to_string(),
                })
            }
            Err(e) => Err(format!("CDP_ERROR: {}", e)),
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub async fn webview_get_frame_html_via_cdp(
    _marker: Option<&str>,
) -> Result<CdpFrameResult, String> {
    Err("CDP is only supported on Windows".to_string())
}

/// Get main frame HTML via CDP (Windows only)
#[cfg(target_os = "windows")]
pub async fn webview_get_main_frame_html_cdp() -> Result<Option<String>, String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    crate::webview::get_main_frame_html_via_cdp(&window)
        .await
        .map_err(|e| format!("CDP_ERROR: {}", e))
}

#[cfg(not(target_os = "windows"))]
pub async fn webview_get_main_frame_html_cdp() -> Result<Option<String>, String> {
    Err("CDP is only supported on Windows".to_string())
}

/// Initialize CDP after WebView creation (must be called before navigate)
#[cfg(target_os = "windows")]
pub async fn setup_cdp() -> Result<(), String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    {
        let mut store = CDP_CONTEXT_STORE.write().await;
        store.clear();
    }

    crate::webview::setup_cdp_for_webview(&window, CDP_CONTEXT_STORE.clone())
        .await
        .map_err(|e| format!("CDP_SETUP_ERROR: {}", e))
}

/// macOS: WKUserScript-based Frame Context collection
#[cfg(target_os = "macos")]
pub async fn setup_cdp() -> Result<(), String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    {
        let mut store = CDP_CONTEXT_STORE.write().await;
        store.clear();
    }

    crate::webview::setup_cdp_for_webview(&window, CDP_CONTEXT_STORE.clone())
        .await
        .map_err(|e| format!("MACOS_FRAME_SETUP_ERROR: {}", e))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub async fn setup_cdp() -> Result<(), String> {
    tracing::warn!("[CDP] CDP is only supported on Windows/macOS, skipping setup");
    Ok(())
}

/// Collect interactive elements inside cross-origin iframes (for `describe`).
///
/// macOS: broadcasts via the Native Bridge. Windows: uses CDP `evaluate_in_context`.
pub async fn webview_describe_cross_origin_frames(
) -> Result<Vec<crate::webview::FrameDescribeResult>, String> {
    let window = get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

    crate::webview::describe_elements_in_all_frames(&window, &CDP_CONTEXT_STORE)
        .await
        .map_err(|e| format!("DESCRIBE_FRAMES_ERROR: {}", e))
}

/// Click an element inside a cross-origin iframe.
pub async fn webview_click_in_cross_origin_iframe(
    local_x: f64,
    local_y: f64,
    click_type: &str,
    iframe_index: usize,
) -> Result<serde_json::Value, String> {
    crate::webview::click_in_cross_origin_iframe(
        local_x,
        local_y,
        click_type,
        iframe_index,
        &CDP_CONTEXT_STORE,
    )
    .await
    .map_err(|e| format!("IFRAME_CLICK_ERROR: {}", e))
}

/// Send keyboard input into a cross-origin iframe.
pub async fn webview_keyboard_in_cross_origin_iframe(
    text: Option<&str>,
    key: Option<&str>,
    paste: bool,
    modifiers: &serde_json::Value,
    iframe_index: usize,
) -> Result<serde_json::Value, String> {
    crate::webview::keyboard_in_cross_origin_iframe(
        text,
        key,
        paste,
        modifiers,
        iframe_index,
        &CDP_CONTEXT_STORE,
    )
    .await
    .map_err(|e| format!("IFRAME_KEYBOARD_ERROR: {}", e))
}

/// Clear CDP Context Store (called on new page load)
pub async fn clear_cdp_context_store() {
    {
        let mut store = CDP_CONTEXT_STORE.write().await;
        store.clear();
    }
    // The cached top frame id belongs to the document being replaced. Taken
    // after the store guard is dropped — never two locks at once.
    *crate::state::CDP_TOP_FRAME_ID.write().await = None;
    tracing::debug!("[CDP] Context store cleared");
}

// ============================================================================
// Modal/Popup Auto Dismiss
// ============================================================================

/// Auto-detect and dismiss modals/popups.
/// Supports Vue.js, Element UI, Bootstrap and common patterns.
pub async fn webview_auto_dismiss_modals(
    max_attempts: Option<u32>,
    delay_ms: Option<u32>,
) -> Result<ModalDismissResult, String> {
    let max_attempts = max_attempts.unwrap_or(3);
    let delay_ms = delay_ms.unwrap_or(500);

    // Same missing `return` as `webview_get_frame_list` — the modals were
    // actually dismissed (side effects run), but the counts never came back, so
    // the retry loop always saw `remaining: 0` and stopped after one pass.
    let script = r#"
        return (function dismissModals() {
            const CLOSE_SELECTORS = [
                '.v-dialog__content .close', '.v-dialog__content [class*="close"]',
                '.v-dialog .v-btn--icon', '.v-dialog__content .mdi-close',
                '.el-dialog__close', '.el-message-box__close', '.el-dialog__headerbtn',
                '.modal .close', '.modal .btn-close', '.modal-header .close',
                '[data-dismiss="modal"]', '[data-bs-dismiss="modal"]',
                '[class*="modal"] .close', '[class*="modal"] [class*="close"]',
                '.popup .close', '[class*="popup"] .close', '[class*="popup"] [class*="close"]',
                'button[aria-label="Close"]', 'button[aria-label="\uB2EB\uAE30"]' /* Korean "Close" */,
                '.notice_modal .close', '#modal-notice .close',
                '.alert .close', '.alert-dismissible .close',
                '.modal .icon-close', '.modal .ico-close',
                '[class*="close-btn"]', '[class*="closeBtn"]'
            ];

            const HIDE_SELECTORS = [
                '.v-dialog__content', '.v-overlay--active',
                '.el-overlay', '.el-dialog__wrapper',
                '.modal.show', '.modal.fade.show', '.modal-backdrop',
                '.popup:not(.hidden)', '[class*="popup"]:not([style*="none"])',
                '.notice_modal', '#modal-notice',
                '[role="dialog"][aria-modal="true"]'
            ];

            let dismissed = 0;
            let modalsFound = [];

            for (const sel of CLOSE_SELECTORS) {
                const btns = document.querySelectorAll(sel);
                btns.forEach(btn => {
                    if (btn.offsetParent !== null || getComputedStyle(btn).display !== 'none') {
                        try {
                            btn.click();
                            dismissed++;
                            modalsFound.push(sel);
                        } catch(e) {}
                    }
                });
            }

            document.dispatchEvent(new KeyboardEvent('keydown', {
                key: 'Escape',
                keyCode: 27,
                code: 'Escape',
                bubbles: true
            }));

            for (const sel of HIDE_SELECTORS) {
                const els = document.querySelectorAll(sel);
                els.forEach(el => {
                    if (el.offsetParent !== null || getComputedStyle(el).display !== 'none') {
                        el.style.display = 'none';
                        el.style.visibility = 'hidden';
                        modalsFound.push(sel + ' (hidden)');
                    }
                });
            }

            document.body.style.overflow = '';
            document.body.classList.remove('modal-open', 'overflow-hidden');

            const remaining = document.querySelectorAll(
                '.modal.show, .v-dialog__content:not([style*="none"]), .el-overlay:not([style*="none"]), [role="dialog"][aria-modal="true"]:not([style*="none"])'
            ).length;

            return JSON.stringify({
                dismissed: dismissed,
                remaining: remaining,
                modalsFound: [...new Set(modalsFound)]
            });
        })()
    "#;

    let mut total_dismissed = 0;
    let mut final_remaining = 0;
    let mut all_modals_found: Vec<String> = Vec::new();

    for attempt in 1..=max_attempts {
        // The worst of the four: a decode failure produced `remaining: 0`, which
        // broke the retry loop early and reported "All modals dismissed
        // (0 closed)" — a silent success for work that never happened.
        match crate::platform::exec_js_parse::<serde_json::Value>(script, Some(5000)).await {
            Ok(parsed) => {
                let dismissed = parsed["dismissed"].as_u64().unwrap_or(0) as u32;
                let remaining = parsed["remaining"].as_u64().unwrap_or(0) as u32;

                total_dismissed += dismissed;
                final_remaining = remaining;

                if let Some(modals) = parsed["modalsFound"].as_array() {
                    for m in modals {
                        if let Some(s) = m.as_str() {
                            all_modals_found.push(s.to_string());
                        }
                    }
                }

                if remaining == 0 {
                    break;
                }

                if attempt < max_attempts {
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms as u64)).await;
                }
            }
            Err(e) => {
                return Err(format!("AUTO_DISMISS_MODALS_ERROR: {}", e));
            }
        }
    }

    Ok(ModalDismissResult {
        dismissed_count: total_dismissed,
        remaining_count: final_remaining,
        attempts: max_attempts,
        modals_found: all_modals_found
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect(),
    })
}
