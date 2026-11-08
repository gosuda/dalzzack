//! Tool definitions and dispatcher for the browser automation surface.
//!
//! `get_tool_definitions()` returns the JSON schema list consumed by LLM
//! clients (MCP, function calling, etc.). `execute_tool()` is the runtime
//! dispatcher. Every tool defined here must also have a dispatcher arm
//! and vice versa — the `tools` CLI command will surface drift.

use crate::{
    get_sites_path, is_webview_ready, webview_auto_dismiss_modals,
    webview_describe_cross_origin_frames, webview_execute_js, webview_fill_form,
    webview_get_frame_html, webview_get_frame_list, webview_get_html, webview_get_title,
    webview_get_url, webview_keyboard_in_cross_origin_iframe, webview_navigate,
    webview_wait_for_selector, CURRENT_SITE,
};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::Value;

/// Tool definition (used for --help generation and step validation)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

// ============================================================================
// HTML Noise Stripping Helpers
// ============================================================================

/// Elements whose whole content is noise.
const NOISE_BLOCKS: [&[u8]; 4] = [b"script", b"style", b"noscript", b"svg"];

fn is_html_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn ends_tag_name(b: u8) -> bool {
    is_html_space(b) || matches!(b, b'>' | b'/')
}

/// Remove comments, `<script>`, `<style>`, `<noscript>`, `<svg>`, and the
/// attributes that only describe presentation or framework bookkeeping.
///
/// Every cut falls on an ASCII byte, so the text between cuts stays valid UTF-8
/// and is copied unchanged.
fn strip_html_noise(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(offset) = html[i..].find('<') {
        let tag = i + offset;
        out.push_str(&html[i..tag]);
        i = match noise_block_end(html.as_bytes(), tag) {
            Some(end) => end,
            None => copy_tag(html, tag, &mut out),
        };
    }
    out.push_str(&html[i..]);
    out
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Where the comment or noise element that opens at `at` ends. `None` for any
/// other tag, and for a block that never closes: that text is kept.
fn noise_block_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at..];
    if rest.starts_with(b"<!--") {
        return find_bytes(rest, b"-->").map(|end| at + end + 3);
    }
    let name = NOISE_BLOCKS.iter().find(|name| {
        rest.len() > name.len() + 1
            && rest[1..=name.len()].eq_ignore_ascii_case(name)
            && ends_tag_name(rest[name.len() + 1])
    })?;
    closing_tag_end(rest, name).map(|end| at + end)
}

/// Where the first `</name>` ends, in any letter case.
fn closing_tag_end(bytes: &[u8], name: &[u8]) -> Option<usize> {
    let mut i = 0;
    while let Some(offset) = find_bytes(&bytes[i..], b"</") {
        let start = i + offset + 2;
        let after = start + name.len();
        if bytes.len() > after && bytes[start..after].eq_ignore_ascii_case(name) {
            let close = after + bytes[after..].iter().position(|b| !is_html_space(*b))?;
            if bytes[close] == b'>' {
                return Some(close + 1);
            }
        }
        i = start;
    }
    None
}

/// Copy the tag at `at` without its noise attributes, and return where to go on.
/// A `<` that opens no start tag (`</div>`, `<!doctype`, `a < b`) is copied alone.
fn copy_tag(html: &str, at: usize, out: &mut String) -> usize {
    let bytes = html.as_bytes();
    if !bytes.get(at + 1).is_some_and(u8::is_ascii_alphabetic) {
        out.push('<');
        return at + 1;
    }
    let mut i = at + 1;
    while i < bytes.len() && !ends_tag_name(bytes[i]) {
        i += 1;
    }
    out.push_str(&html[at..i]);
    loop {
        let gap = i;
        while i < bytes.len() && is_html_space(bytes[i]) {
            i += 1;
        }
        match bytes.get(i) {
            None => {
                out.push_str(&html[gap..]);
                return i;
            }
            Some(b'>') => {
                out.push_str(&html[gap..=i]);
                return i + 1;
            }
            Some(b'/') => {
                i += 1;
                out.push_str(&html[gap..i]);
                continue;
            }
            Some(_) => {}
        }
        let name_start = i;
        while i < bytes.len() && !ends_tag_name(bytes[i]) && bytes[i] != b'=' {
            i += 1;
        }
        let name = &bytes[name_start..i];
        i = attribute_end(bytes, i);
        if !is_noise_attribute(name) {
            out.push_str(&html[gap..i]);
        }
    }
}

/// Where an attribute ends, given where its name ends. With no `=` these are
/// the same place.
fn attribute_end(bytes: &[u8], name_end: usize) -> usize {
    let skip_space = |from: usize| {
        let spaces = bytes[from..].iter().take_while(|b| is_html_space(**b));
        from + spaces.count()
    };
    let equals = skip_space(name_end);
    if bytes.get(equals) != Some(&b'=') {
        return name_end;
    }
    let value = skip_space(equals + 1);
    match bytes.get(value) {
        Some(quote @ (b'"' | b'\'')) => bytes[value + 1..]
            .iter()
            .position(|b| b == quote)
            .map_or(bytes.len(), |end| value + end + 2),
        _ => bytes[value..]
            .iter()
            .position(|b| is_html_space(*b) || *b == b'>')
            .map_or(bytes.len(), |end| value + end),
    }
}

fn is_noise_attribute(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"style")
        || name.eq_ignore_ascii_case(b"data-reactid")
        || name
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"data-v-"))
}

// ============================================================================
// Tool Definitions
// ============================================================================
//
// `site` is the daemon instance ID. The daemon auto-injects it from the
// `--id` CLI flag, so LLM callers can omit it. It's listed in each schema
// as optional for transparency.

fn site_param() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "Daemon instance ID. Auto-injected from CLI --id; LLMs can omit."
    })
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "navigate".to_string(),
            description: "Navigate the browser to a URL and wait for the page to load. A main-frame load that failed (a refused certificate, DNS, a refused connection) returns `status: failed`, `error: NAVIGATION_FAILED`, with `failing_url` and `error_text` (WebView2, Chrome) or `error_domain`, `error_code`, and `trust_error` (WKWebView).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "url": { "type": "string", "description": "Target URL (http/https)." },
                    "timeout_ms": { "type": "integer", "description": "Page load timeout in milliseconds (default 5000)." }
                },
                "required": ["url"]
            }),
        },

        ToolDefinition {
            name: "get_text".to_string(),
            description: "Return the visible text of the page or one element. Use this to read a page: it is several times smaller than HTML and has the actual words, which a screenshot does not.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector to scope to a single element (default: whole page)." },
                    "max_length": { "type": "integer", "description": "Cut the text to N bytes (0 = unlimited)." }
                }
            }),
        },

        ToolDefinition {
            name: "get_html".to_string(),
            description: "Return the HTML of the current page or a specific element. Prefer `screenshot` or `describe` for visual understanding; use this only when you need a value that cannot be seen on screen (hidden tokens, raw text, attribute data).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector to scope to a single element (default: whole document)." },
                    "save_to": { "type": "string", "description": "Optional path to write the HTML to disk." },
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default 30000)." },
                    "file_only": { "type": "boolean", "description": "When true with save_to, write to disk only and omit HTML from the response (saves tokens)." },
                    "strip_noise": { "type": "boolean", "description": "Remove <script>, <style>, <noscript>, <svg>, comments, and style/data-* attributes." },
                    "max_length": { "type": "integer", "description": "Truncate HTML to N characters (0 = unlimited)." }
                }
            }),
        },

        ToolDefinition {
            name: "execute_js".to_string(),
            description: "Execute JavaScript and return the JSON-serialised result. Use `return <value>` to produce output. Runs in the page's MAIN world — page globals ARE visible — but only in the TOP frame, so a global set inside an iframe reads as `undefined` unless you pass `frame`. Every result reports where it ran (`world`, `frame`, `frame_origin`, `context_id`).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "script": { "type": "string", "description": "JavaScript source. Use `return value;` to send a result back." },
                    "timeout_ms": { "type": "integer", "description": "Execution timeout in milliseconds (default 30000)." },
                    "max_result_length": { "type": "integer", "description": "Truncate stringified result to N characters (0 = use the 5 MB safety cap)." },
                    "await_promise": { "type": "boolean", "description": "Await a promise result instead of failing on it — lets the script use `await` at the top level (default false)." },
                    "frame": { "type": "string", "description": "Run inside a specific frame: `top` (default), an index from `get_frame_list` (same order by construction), a `frame_id` from `get_frame_list`/`known_frames` (the stable handle), or a substring of the frame's origin/name. Out-of-range indexes are rejected. Windows only." },
                    "world": { "type": "string", "enum": ["main", "isolated"], "description": "`main` (default) sees page globals; `isolated` uses Page.createIsolatedWorld — hidden from page hooks, and correspondingly blind to page globals. CSP note: the isolated world is created with `grantUniveralAccess`, which lifts the same-origin policy but NOT the page's Content Security Policy — measured, both worlds are blocked identically. What removes CSP is dalzzack's own default (`Page.setBypassCSP`), which applies to every world; use `start --enforce-csp` before concluding anything about a policy. Windows only." },
                    "target": { "type": "string", "description": "`page` (default) or `sw` — evaluate inside the page's SERVICE WORKER (its own isolate: `self`, `caches`, `clients`, the worker's globals — unreachable from any page-side tool). `sw:<substring>` picks the worker whose script URL contains it; `sw` the most recently attached. `frame`/`world` do not apply. Windows/WebView2 or `--engine chrome`." }
                },
                "required": ["script"]
            }),
        },

        ToolDefinition {
            name: "screenshot".to_string(),
            description: "Capture the visible viewport (or a single element when `selector` is provided) as a PNG. The recommended primitive for visual page understanding.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "output_file": { "type": "string", "description": "Output PNG path (default: auto-generated)." },
                    "selector": { "type": "string", "description": "CSS selector to crop to a single element (e.g. `img.captcha`)." }
                }
            }),
        },

        ToolDefinition {
            name: "wait".to_string(),
            description: "Sleep for a fixed duration. Prefer `wait_for_selector` over this — timer-based waits are flaky.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "ms": { "type": "integer", "description": "Duration in milliseconds." }
                },
                "required": ["ms"]
            }),
        },

        ToolDefinition {
            name: "get_current_url".to_string(),
            description: "Return the current page URL and document title.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },

        ToolDefinition {
            name: "wait_navigation".to_string(),
            description: "Wait until the document is REPLACED — form submit, click-through, redirect. A sentinel planted in the old document vanishes when the new one commits, so same-URL navigations are detected too. Use after click/fill_form; proxied navigations can take 20-30s.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "timeout_ms": { "type": "integer", "description": "Overall timeout in ms (default 30000 — raise for slow proxies)." },
                    "to": { "type": "string", "description": "Only finish when the NEW url contains this substring (redirect chains keep waiting until it matches)." },
                    "interval_ms": { "type": "integer", "description": "Poll interval in ms (default 250)." }
                }
            }),
        },

        ToolDefinition {
            name: "wait_for_selector".to_string(),
            description: "Block until a CSS selector matches a DOM element. State `present` waits for any match, `added` for a NEW element (count increases from baseline), `removed` for disappearance. 100ms polling.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector to wait for." },
                    "state": { "type": "string", "enum": ["present", "added", "removed"], "description": "Condition to wait for (default: present)." },
                    "text": { "type": "string", "description": "Optional text content the match must contain (only with state=present)." },
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default 10000)." }
                },
                "required": ["selector"]
            }),
        },

        ToolDefinition {
            name: "fill_form".to_string(),
            description: "Fill one or more form fields. SPA-safe: uses the native input value setter so React/Vue state stays in sync. Optionally clicks a submit button after filling.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "fields": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "selector": { "type": "string", "description": "CSS selector of the input field." },
                                "value": { "type": "string", "description": "Value to type into the field." }
                            },
                            "required": ["selector", "value"]
                        },
                        "description": "List of fields to fill."
                    },
                    "submit_selector": { "type": "string", "description": "Optional CSS selector of a submit button to click after filling." }
                },
                "required": ["fields"]
            }),
        },

        ToolDefinition {
            name: "preprocess_captcha".to_string(),
            description: "Run image preprocessing on a captcha file (grayscale, contrast, denoise, threshold, morphology) to improve OCR/vision-model accuracy. Does not touch the browser.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "input_file": { "type": "string", "description": "Path to the captcha image to process." },
                    "output_file": { "type": "string", "description": "Output path (default: captcha_processed.png)." },
                    "contrast": { "type": "number", "description": "Contrast multiplier (default 1.5, range 0.5–3.0)." },
                    "median_radius": { "type": "integer", "description": "Median filter radius (default 1, range 0–5; 0 = skip)." },
                    "threshold_block_radius": { "type": "integer", "description": "Adaptive threshold block radius (default 11, range 3–51; 0 = Otsu auto-threshold)." },
                    "morphology_radius": { "type": "integer", "description": "Erosion/dilation radius (default 1, range 0–3; 0 = skip)." }
                },
                "required": ["input_file"]
            }),
        },

        ToolDefinition {
            name: "get_captured_requests".to_string(),
            description: "Return XHR/fetch requests captured during the last N seconds. The interceptor is installed automatically on every navigate. Each entry includes url, method, status, contentType, plus optional body/response previews. Prefer this over scraping HTML when you need API data. NOTE: this is an in-page hook over fetch/XHR — script/CSS/image loads are invisible to it, and it cannot report who initiated a request. For either, use `dump_recording` (start the daemon with --record): its CDP network events cover every request type and carry `initiator` with a JS call stack.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "seconds": { "type": "integer", "description": "Lookback window in seconds (default 180)." },
                    "url_pattern": { "type": "string", "description": "Include filter: only requests whose URL contains this substring." },
                    "url_exclude": { "type": "string", "description": "Exclude filter, pipe-separated (e.g. `analytics|track|pixel`)." },
                    "content_type": { "type": "string", "description": "Content-Type substring filter (e.g. `json`)." },
                    "method": { "type": "string", "description": "HTTP method filter (GET, POST, …)." },
                    "max_count": { "type": "integer", "description": "Cap the number of returned entries (0 = unlimited)." },
                    "include_preview": { "type": "boolean", "description": "When false, omit bodyPreview/resPreview/reqBody fields (default true)." },
                    "include_headers": { "type": "boolean", "description": "Include captured request headers (Authorization etc.). Sensitive values are redacted to a 12-char prefix by default (default false)." },
                    "no_redact": { "type": "boolean", "description": "With include_headers: return sensitive header values in full (default false)." }
                }
            }),
        },

        ToolDefinition {
            name: "get_frame_list".to_string(),
            description: "List every iframe on the page with id, name, src, origin (same/cross), accessibility, and (Windows) its CDP `frame_id` — the stable handle `execute_js`'s `frame` accepts; the listing index is also `frame`'s index space. `name` is truncated to 200 chars (`name_len`/`name_truncated` mark the cut) unless `full`.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "full": { "type": "boolean", "description": "Return complete `name` attributes (SafeFrame ad slots carry multi-KB JSON there; default truncates to 200 chars)." }
                }
            }),
        },

        ToolDefinition {
            name: "get_frame_html".to_string(),
            description: "Return the HTML of a single iframe. Same-origin frames are read via contentDocument; cross-origin frames return only the src URL (navigate to it directly to inspect).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector of the iframe (e.g. `iframe[name='game']`). Either selector or index is required." },
                    "index": { "type": "integer", "description": "Iframe index from `get_frame_list`. Either selector or index is required." }
                }
            }),
        },

        ToolDefinition {
            name: "auto_dismiss_modals".to_string(),
            description: "Detect and close modals/popups/overlays. Tries close buttons, ESC key, then forced hide. Covers Vue, Element UI, Bootstrap, and common cookie banners.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "max_attempts": { "type": "integer", "description": "Maximum attempts (default 3)." },
                    "delay_ms": { "type": "integer", "description": "Delay between attempts in milliseconds (default 500)." }
                }
            }),
        },

        ToolDefinition {
            name: "check_visible".to_string(),
            description: "Instant visibility check for a CSS selector — no polling. Useful for conditional branches.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector to check." }
                },
                "required": ["selector"]
            }),
        },

        ToolDefinition {
            name: "click_element".to_string(),
            description: "Click an element by CSS selector or visible text. More reliable than coordinate-based `click` because it survives layout changes and resize.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector of the element to click." },
                    "text": { "type": "string", "description": "Visible text content to find (XPath-based). Either selector or text is required." },
                    "click_type": { "type": "string", "enum": ["left", "right", "double"], "description": "Click type (default: left)." }
                }
            }),
        },

        ToolDefinition {
            name: "upload_file".to_string(),
            description: "Upload a local file to an `<input type=\"file\">` element via the DataTransfer API. Max 50 MB.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector of the file input element." },
                    "file_path": { "type": "string", "description": "Absolute path to the local file." }
                },
                "required": ["selector", "file_path"]
            }),
        },

        ToolDefinition {
            name: "save_image".to_string(),
            description: "Save a DOM `<img>` element's content (base64 data URI or remote URL) to a local file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector of the img element." },
                    "output_path": { "type": "string", "description": "Local file path to write to." }
                },
                "required": ["selector", "output_path"]
            }),
        },

        ToolDefinition {
            name: "click".to_string(),
            description: "Click at viewport coordinates. Auto-detects iframes and uses native OS mouse events for cross-origin frames. Prefer `click_element` when you have a selector — coordinate clicks break on resize.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "x": { "type": "number", "description": "X coordinate in viewport pixels." },
                    "y": { "type": "number", "description": "Y coordinate in viewport pixels." },
                    "click_type": { "type": "string", "enum": ["left", "right", "double"], "description": "Click type (default: left)." }
                },
                "required": ["x", "y"]
            }),
        },

        ToolDefinition {
            name: "scroll".to_string(),
            description: "Scroll the page or a scrollable container. Use `to` for absolute positions (top/bottom/left-end/right-end), or `direction` + `amount` for relative scrolling.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "to": { "type": "string", "enum": ["top", "bottom", "left-end", "right-end"], "description": "Absolute scroll target. Mutually exclusive with direction/amount." },
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "Relative scroll direction (default: down)." },
                    "amount": { "type": "integer", "description": "Pixels to scroll for relative mode (default 500)." },
                    "selector": { "type": "string", "description": "CSS selector of a scrollable container. Omit to scroll the page." }
                }
            }),
        },

        ToolDefinition {
            name: "keyboard_input".to_string(),
            description: "Type text or press a special key. With `text`, types character-by-character (or pastes if `paste=true`). With `key`, presses a named key (Enter, Tab, Escape, ArrowDown, …) with optional modifiers.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "text": { "type": "string", "description": "Text to type. Either text or key is required." },
                    "key": { "type": "string", "description": "Named key to press (Enter, Tab, Escape, ArrowUp, …). Either text or key is required." },
                    "selector": { "type": "string", "description": "Optional CSS selector to focus before typing." },
                    "modifiers": { "type": "string", "description": "Comma-separated modifiers: ctrl, shift, alt, meta (e.g. `ctrl,shift`)." },
                    "paste": { "type": "boolean", "description": "When true with `text`, paste the entire string at once via the native setter (fast). Default false." }
                }
            }),
        },

        ToolDefinition {
            name: "element_at".to_string(),
            description: "Find the nearest interactive element at the given viewport coordinates. Returns tag, text, CSS selector, bounding rect, and a snippet of outerHTML. Useful for translating a screenshot click into a stable selector.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "x": { "type": "number", "description": "X coordinate in viewport pixels." },
                    "y": { "type": "number", "description": "Y coordinate in viewport pixels." }
                },
                "required": ["x", "y"]
            }),
        },

        ToolDefinition {
            name: "describe".to_string(),
            description: "Take an annotated screenshot: numbered red labels are overlaid on every interactive element (button, link, input, textarea, select), and a JSON index of those elements (id, tag, text, x/y center) is returned. The recommended first action on any new page — gives the agent stable handles to click by id or coordinate.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "output": { "type": "string", "description": "Output PNG path (default: describe.png)." }
                }
            }),
        },

        ToolDefinition {
            name: "resize_window".to_string(),
            description: "Resize the browser window of a running daemon. Minimum 100×100. Useful for responsive testing or fitting the viewport to a target layout.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "width": { "type": "integer", "description": "New window width in pixels (>= 100)." },
                    "height": { "type": "integer", "description": "New window height in pixels (>= 100)." }
                },
                "required": ["width", "height"]
            }),
        },

        ToolDefinition {
            name: "back".to_string(),
            description: "Navigate one step back in browser history (`history.back()`). Returns the URL before and after, plus a `changed` flag — false means there was no history to go back to.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },

        ToolDefinition {
            name: "forward".to_string(),
            description: "Navigate one step forward in browser history (`history.forward()`). Returns the URL before and after, plus a `changed` flag.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },

        ToolDefinition {
            name: "reload".to_string(),
            description: "Reload the current page. Set `hard` to bypass the HTTP cache (the Ctrl+Shift+R equivalent) — pair it with `clear_site_data` to get a true first-visit load.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "hard": { "type": "boolean", "description": "Bypass the HTTP cache (default false). Windows: CDP `Page.reload {ignoreCache:true}`; macOS: `-[WKWebView reloadFromOrigin]`." }
                }
            }),
        },

        ToolDefinition {
            name: "clear_site_data".to_string(),
            description: "Clear an origin's storage out-of-band: service worker registrations, CacheStorage, localStorage, IndexedDB, cookies. Runs outside the page's JS world (Windows: CDP `Storage.clearDataForOrigin`; macOS: WKWebsiteDataStore), so it still works when a proxy/membrane or a policy hides `navigator.serviceWorker`. Follow with `reload` + `hard` for a clean first-visit load. macOS caveat: clears by HOST, not origin — scheme and port are not distinguished.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "origin": { "type": "string", "description": "Origin to clear, e.g. `http://proxy.localhost:18080`. Defaults to the current page's origin." },
                    "types": { "type": "string", "description": "Comma-separated storage types, or `all`. Known: appcache, cache_storage, cookies, file_systems, indexeddb, local_storage, service_workers, shader_cache, websql. Default: service_workers,cache_storage,local_storage,indexeddb,cookies." }
                }
            }),
        },

        ToolDefinition {
            name: "hover".to_string(),
            description: "Hover (fire mouseover/mouseenter/mousemove) over an element by CSS selector or viewport coordinates. Use this to reveal dropdown menus, tooltips, or hover-only UI. Returns `nearby` candidates when the selector misses.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "selector": { "type": "string", "description": "CSS selector of the element to hover. Mutually exclusive with x/y." },
                    "x": { "type": "number", "description": "Viewport X coordinate. Use with y." },
                    "y": { "type": "number", "description": "Viewport Y coordinate. Use with x." }
                }
            }),
        },

        ToolDefinition {
            name: "console_logs".to_string(),
            description: "Read the page's captured console output (log/info/warn/error/debug + uncaught errors + unhandled rejections) — from the top document AND every frame, cross-site (out-of-process) frames included; each line carries its `frame_id`. Ring buffer: 500 entries unless the daemon was started with `--console-buffer N`; the oldest are dropped first and `dropped` counts them. Useful for diagnosing why a click didn't fire, why a request failed, or what a SPA is complaining about. On Windows capture is armed at daemon start and is NOT observable by the page: CDP browser-process events never wrap `console.log`, so getter-trapped objects are never read and anti-debug libraries do not fire, and because it reads daemon memory it answers while the renderer is wedged. On macOS there is NO invisible transport, so nothing is captured unless the daemon was started with `--console-capture` — that installs a page-visible in-page hook. Every response carries `console_transport` — `cdp` / `in_page_fallback` / `disabled` / `unavailable` — so an empty buffer can be told apart from broken capture; `unavailable` also carries the reason.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "level": { "type": "string", "enum": ["log", "info", "warn", "error", "debug"], "description": "Filter by log level (default: all levels)." },
                    "source": { "type": "string", "enum": ["console", "csp", "security", "network", "deprecation", "violation", "javascript", "other"], "description": "Filter by origin. `console` is the page's own console.* calls; `csp` is Content Security Policy violations, which the page's `securitypolicyviolation` event does NOT surface in this environment — this is the only way to see them. Default: everything." },
                    "frame": { "type": "string", "description": "Only lines from this frame: `top` (the top document, plus lines that carry no frame id), an index from `get_frame_list`, or a `frame_id`." },
                    "text": { "type": "string", "description": "Only lines whose message contains this (case-insensitive)." },
                    "max": { "type": "integer", "description": "Maximum entries to return — the newest (default 100, up to the buffer size). `matched` says how many matched before the cut." },
                    "clear": { "type": "boolean", "description": "Clear the buffer after reading (default false)." }
                }
            }),
        },

        // ===================================================================
        // Renderer diagnostics — out-of-band inspection for hung/slow webviews.
        // macOS: sample/vmmap of the WebContent process. Windows: real CDP.
        // ===================================================================
        ToolDefinition {
            name: "diag_pause".to_string(),
            description: "Capture WHERE the renderer is executing right now — the #1 tool when the page is frozen and `execute_js` times out. macOS: external `sample` of the WKWebView WebContent process (works even when the renderer is fully wedged). Windows: out-of-band CDP JS stack PLUS MiniDumpWriteDump of every WebView2 process (kernel-driven, so it answers on a fully wedged renderer). Pass `full_memory: true` when the question is 'what is allocated?' rather than 'where is the stack?' — that includes the heap, so page/JS strings are searchable in the dump.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "duration_ms": { "type": "integer", "description": "Sampling window in ms (macOS, default 1200; rounded up to whole seconds, max 30000)." },
                    "output": { "type": "string", "description": "File path for the full report (auto temp file if omitted)." },
                    "full_memory": { "type": "boolean", "description": "Windows: dump the heap too (MiniDumpWithFullMemory | FullMemoryInfo | HandleData). Dump size ≈ process working set. Without `only_pid`, only the largest WebView2 process is dumped." },
                    "only_pid": { "type": "integer", "description": "Windows: dump only this PID. Pick it from `candidate_processes` in a previous pause result." },
                    "all_processes": { "type": "boolean", "description": "Windows: with full_memory, dump every WebView2 process instead of just the largest (GBs of disk)." },
                    "max_bytes": { "type": "integer", "description": "Windows: warn (never fail) when a dump exceeds this many bytes." },
                    "symbolize": { "type": "boolean", "description": "Windows: resolve native stacks to function names via dbghelp. Threads parked in ntdll (waiting, not working) are excluded — including them buries the signal. Unresolved frames come back as `module+0xRVA` rather than being dropped." },
                    "all_threads": { "type": "boolean", "description": "Windows: with symbolize, keep the ntdll waiters too." },
                    "download_symbols": { "type": "boolean", "description": "Windows: fetch PDBs from the Microsoft symbol server. Off by default — msedge.pdb is hundreds of MB and the first resolve would stall for minutes." },
                    "symbol_cache": { "type": "string", "description": "Windows: symbol cache directory (default <temp>/dalzzack-symcache)." },
                    "samples": { "type": "integer", "description": "Take N dumps of ONE process instead of one dump of every process — the way to tell 'spinning here' from 'happened to be here'. With symbolize, the response includes a frame histogram." },
                    "interval_ms": { "type": "integer", "description": "Delay between samples in ms (default 250)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_console_dump".to_string(),
            description: "Dump the console ring buffer (like console_logs) with explicit hung-renderer detection and a `source` field naming the transport that answered. With the out-of-band transports (cdp / os_log) this is a daemon-memory read and keeps working through a hang; only the in-page fallback can be defeated by one, and that case is labelled `renderer: unresponsive` and points at diag_pause instead of returning an empty console. Portable across platforms.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "level": { "type": "string", "enum": ["log", "info", "warn", "error", "debug"], "description": "Filter by level (default all)." },
                    "max": { "type": "integer", "description": "Max entries (default 500)." },
                    "clear": { "type": "boolean", "description": "Clear the buffer after reading (default false)." }
                }
            }),
        },
        ToolDefinition {
            name: "get_response_headers".to_string(),
            description: "Response headers captured from CDP `Network.responseReceived`, including the NAVIGATION DOCUMENT — which no other tool can see. `get_captured_requests` / `network_log` are built on an in-page fetch/XHR hook that does not exist yet when the document is fetched, and `dump_recording` never stored headers. Use this to answer 'what CSP / CORP / cache policy / cookies was this document served with?'. Capture is armed at daemon start (no `--record` needed) because `Network.responseReceived` fires once at navigation time and cannot be obtained afterwards. Documents are kept in their own ring so a flood of subresources cannot evict them. `set-cookie` values are masked by default (name and attributes are kept); pass `no_redact` for the raw value. Windows-only — there is no fallback, since page script cannot read response headers.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "url_pattern": { "type": "string", "description": "Filter by URL. Without `*` it is a substring match; with `*` it is a glob over the full URL." },
                    "resource_type": { "type": "string", "description": "CDP resource type, case-insensitive: `Document` (the navigation — usually what you want), `XHR`, `Fetch`, `Script`, `Stylesheet`, `Image`, … Omit for all." },
                    "max": { "type": "integer", "description": "Maximum entries — newest last (default 50, cap 500)." },
                    "no_redact": { "type": "boolean", "description": "Return `set-cookie` values unmasked (default false)." }
                }
            }),
        },
        ToolDefinition {
            name: "navigation_log".to_string(),
            description: "The navigation timeline: one entry per document request chain (top frame by default), each with EVERY hop — redirects included — and per hop the status, `location`, `cf-mitigated`, `server`, set-cookie names and a detected `challenge` (e.g. `cloudflare:challenge`). Also says whether the response actually committed. Always on (no `--record` needed), daemon-memory read (answers on a wedged renderer). Use it whenever 'did the site let us through?' matters: a challenge page that solves itself shows up here as a `403 cf-mitigated` entry followed by a second navigation to the same URL, whereas `navigate` alone only shows where it ended. `headers: true` adds the full request headers as sent on the wire (cookies, `sec-fetch-*`; cookie values masked) and the full response headers per hop. Needs CDP: WebView2 on Windows, or `start --engine chrome`.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "since_ms": { "type": "integer", "description": "Only navigations that started within the last N ms." },
                    "url_pattern": { "type": "string", "description": "Keep chains where any hop's URL matches (substring, or glob with `*`)." },
                    "include_frames": { "type": "boolean", "description": "Include iframe documents (default: top frame only)." },
                    "max": { "type": "integer", "description": "Newest N chains (default 20, cap 200)." },
                    "headers": { "type": "boolean", "description": "Add full request (wire) and response headers to every hop." },
                    "no_redact": { "type": "boolean", "description": "Do not mask cookie / set-cookie / authorization values." }
                }
            }),
        },
        ToolDefinition {
            name: "failed_requests".to_string(),
            description: "Every request that failed — of ANY resource type (document, script, stylesheet, image, font, fetch, …) — WITH its URL, which CDP's `loadingFailed` never carries. Each entry is classified: `blocked` (CSP/mixed content/inspector, with `blocked_reason`), `cors` (`cors_error`), `network_error` (DNS/TLS/connection, `error_text` like `net::ERR_CONNECTION_RESET`), `http_error` (status >= 400; omit with `no_http_errors`), `canceled` (hidden unless `include_canceled` — navigating away and ad slots produce lots). Carries initiator and frame. Always on, daemon-memory read. Use it instead of `get_captured_requests` (fetch/XHR only, in-page hook) when a script, image or iframe silently failed to load. On macOS WKWebView, which has no network stream, only main-frame loads that failed (`coverage: main_frame_only`).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "since_ms": { "type": "integer", "description": "Only requests that started within the last N ms." },
                    "url_pattern": { "type": "string", "description": "Substring, or glob with `*`." },
                    "resource_type": { "type": "string", "description": "CDP resource type, case-insensitive: Document, Script, Stylesheet, Image, Font, XHR, Fetch, Media, Other, …" },
                    "no_http_errors": { "type": "boolean", "description": "Network-layer failures only; leave out HTTP >= 400 responses." },
                    "include_canceled": { "type": "boolean", "description": "Also list canceled (net::ERR_ABORTED) requests." },
                    "max": { "type": "integer", "description": "Newest N (default 50, cap 1000)." },
                    "headers": { "type": "boolean", "description": "Add the failing hop's request/response headers." },
                    "no_redact": { "type": "boolean", "description": "Do not mask cookie values." }
                }
            }),
        },
        ToolDefinition {
            name: "record".to_string(),
            description: "Turn the `--record` tape (what `dump_recording` reads) on or off on a LIVE daemon, or report its state. `on` starts capturing from now — events from before are not recoverable; `off` stops appending but keeps the tape readable (`clear` drops it). The always-on views (`navigation_log`, `failed_requests`, `get_response_headers`, `console_logs`) never need this.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "action": { "type": "string", "enum": ["on", "off", "status"], "description": "Default `status`." },
                    "clear": { "type": "boolean", "description": "With `off`: also drop what was captured." }
                }
            }),
        },
        ToolDefinition {
            name: "cookies".to_string(),
            description: "Read or delete the browser's cookie jar through CDP — including HttpOnly cookies (invisible to `document.cookie`) and cookies set on PARENT domains (`.example.com`: `_ga`, consent, cross-subdomain sessions) and on third-party domains, which `clear_site_data` (one origin) leaves behind. Scope: default = exactly what the browser would send to the current page URL; `url` = the same for another URL; `domain` = every cookie on that domain, its subdomains AND its parents; `all` = the whole jar. `name` narrows to one cookie. Values are masked unless `no_redact`. `clear` deletes the selection and then re-reads the jar, reporting `remaining` — a CDP delete whose domain does not match exactly is otherwise a silent no-op. Each cookie reports snake_case fields: `http_only`, `secure`, `same_site`, `host_only`, `session`, `expires`. Plain daemons SHARE one profile, and it outlives them: `clear` on it refuses (`SHARED_PROFILE`) even when no other daemon is running, unless `force_shared`; `dry_run` lists what would be deleted. Needs CDP (WebView2 on Windows, or `--engine chrome`).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "action": { "type": "string", "enum": ["list", "clear"], "description": "Default `list`." },
                    "url": { "type": "string", "description": "Cookies the browser would send to this URL (default: the current page)." },
                    "domain": { "type": "string", "description": "Cookies on this domain, its subdomains and its parent domains." },
                    "all": { "type": "boolean", "description": "The whole cookie jar." },
                    "name": { "type": "string", "description": "Only cookies with this exact name." },
                    "no_redact": { "type": "boolean", "description": "Show cookie values unmasked (list)." },
                    "force_shared": { "type": "boolean", "description": "clear: proceed on the shared profile anyway." },
                    "dry_run": { "type": "boolean", "description": "clear: list what would be deleted, delete nothing (never refused)." }
                }
            }),
        },
        ToolDefinition {
            name: "reset".to_string(),
            description: "Return this daemon to a first-visit state WITHOUT stopping it — the id, port, window and start flags stay. Clears: all browsing data of the profile (cookies, cache, local/session storage, IndexedDB, service workers; WebView2 `ClearBrowsingDataAll`, or a brand-new throw-away profile on `--engine chrome`), intercept rules and injected scripts, and the daemon's buffers (console, nav-log, failed-requests, response headers, recording contents), then navigates to `url` (default about:blank). Refuses with `SHARED_PROFILE` on the shared profile — even when no other daemon is running, since it outlives this one — unless `force_shared` or `keep_browsing_data`; `dry_run` reports what each step would clear (cookie counts per domain) and touches nothing. Use `clear_site_data` for one origin, `cookies` for cookies only.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "keep_browsing_data": { "type": "boolean", "description": "Reset only dalzzack's own state (rules, buffers, page)." },
                    "keep_rules": { "type": "boolean", "description": "Keep intercept rules and injected scripts." },
                    "force_shared": { "type": "boolean", "description": "Clear the shared profile anyway." },
                    "dry_run": { "type": "boolean", "description": "Report what each step would do and do nothing." },
                    "url": { "type": "string", "description": "Where to leave the page afterwards (default about:blank)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_sw_console".to_string(),
            description: "Read the Service Worker's OWN console output — console.* and uncaught exceptions from inside the SW, which never appear in the page console (`console_logs` / `diag_console_dump`). Use it when a SW-side panic or trace is the thing you need to see. Windows-only: requires attaching a CDP session to the service_worker target.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "level": { "type": "string", "enum": ["log", "info", "warn", "error", "debug"], "description": "Filter by level (default all)." },
                    "max": { "type": "integer", "description": "Max entries — newest first (default 100)." },
                    "clear": { "type": "boolean", "description": "Clear the buffer after reading (default false)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_network_log".to_string(),
            description: "List captured fetch/XHR requests and flag ones still PENDING (no response) — a common hang trigger. Note: in-page capture only; HTML-parser requests (<script src>, CSS, img) require CDP Network.* (Windows build).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "since_ms": { "type": "integer", "description": "Only requests newer than this many ms ago (default 60000)." },
                    "url_pattern": { "type": "string", "description": "Substring filter on URL." },
                    "max": { "type": "integer", "description": "Max requests to return." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_profile_start".to_string(),
            description: "Begin a CPU sampling capture. macOS: background `sample` of WebContent. Windows: CDP Profiler. Reproduce the slowness, then call diag_profile_stop.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "max_seconds": { "type": "integer", "description": "macOS: max sampling duration before auto-stop (default 600, max 3600)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_profile_stop".to_string(),
            description: "End the CPU capture started by diag_profile_start and return the report (macOS: sample text; Windows: a Chrome DevTools .cpuprofile JSON at report_path).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "output": { "type": "string", "description": "Windows: path for the .cpuprofile JSON (auto temp file if omitted)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_heap_snapshot".to_string(),
            description: "Capture a memory report. macOS: native process memory via vmmap/footprint (NOT a V8 .heapsnapshot — use attach-devtools for that). Windows: post-GC JS heap usage via CDP.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "output": { "type": "string", "description": "File path for the report (auto temp file if omitted)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_attach_devtools".to_string(),
            description: "Make the renderer inspectable and return how to attach a debugger. macOS: enables WKWebView isInspectable (attach via Safari ▸ Develop). Windows: opens the WebView2 DevTools window directly.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },
        ToolDefinition {
            name: "diag_exec_js_isolated".to_string(),
            description: "Run JS in a world isolated from page globals. macOS: WKContentWorld.defaultClientWorld. Windows: out-of-band CDP Runtime.evaluate. Still the same renderer thread, so a fully hung renderer can still time out — use diag_pause for that.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "script": { "type": "string", "description": "JS to evaluate (use `return` to get a value)." },
                    "timeout_ms": { "type": "integer", "description": "Execution timeout in ms (default 30000)." }
                },
                "required": ["script"]
            }),
        },
        ToolDefinition {
            name: "diag_wait_for_condition".to_string(),
            description: "Poll a JS expression until it's truthy or timeout. Each probe uses a short timeout so a wedged renderer is detected rather than blocking the wait. Portable across platforms.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "script": { "type": "string", "description": "JS expression/body; truthy return ends the wait (use `return`)." },
                    "timeout_ms": { "type": "integer", "description": "Overall timeout in ms (default 30000)." },
                    "interval_ms": { "type": "integer", "description": "Poll interval in ms (default 250, min 50)." }
                },
                "required": ["script"]
            }),
        },
        ToolDefinition {
            name: "dump_recording".to_string(),
            description: "Dump the always-on background recording (network/console/exceptions) buffered daemon-side. Reads from daemon memory — does NOT touch the renderer — so it answers even when the page is wedged. Requires `dalzzack start --record` at daemon launch. Windows captures real CDP Network/Runtime events (browser-process, hang-resistant). macOS polls the in-page __tw_logs / __cr buffers every 500 ms and reports `renderer_wedged_at_ms` when polling stops.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "output": { "type": "string", "description": "If set, write the full snapshot JSON to this path; otherwise return inline." },
                    "filter": { "type": "string", "enum": ["all", "network", "console", "exceptions"], "description": "Restrict the snapshot to one event class (default: all)." },
                    "since_ms": { "type": "integer", "description": "Drop events older than this epoch-ms cutoff." },
                    "clear": { "type": "boolean", "description": "Empty the buffer after snapshotting (default false)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_trace".to_string(),
            description: "Capture a chrome-tracing-compatible timeline for `duration_ms`. Windows uses real CDP Tracing.* (V8 + blink + devtools.timeline) over the browser process, hang-resistant. macOS captures Performance API + paint/longtask via in-page injection (degraded — won't survive a fully hung renderer). Output JSON opens in chrome://tracing or perfetto. Start here when 'where did time go between event X and Y?' matters.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "duration_ms": { "type": "integer", "description": "Capture window in ms (default 2000, max 60000)." },
                    "output": { "type": "string", "description": "Write trace JSON here. Default: temp file." },
                    "categories": { "type": "string", "description": "CDP category list (Windows only)." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_debugger_arm".to_string(),
            description: "Pre-arm V8 so subsequent script execution (or every throw) triggers Debugger.paused — daemon buffers the call frames and auto-resumes. Captures call frames out-of-band on the browser process, so it fires even when the renderer thread is stuck inside the paused frame. `strategy: exceptions` is the only way to observe engine-thrown errors (Proxy invariant TypeErrors, `Illegal invocation`): those never pass through the page's global Error constructors, so wrapping them in-page reports zero. Windows-only (programmatic CDP Debugger access). On macOS use `diag_attach_devtools` and set breakpoints manually.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "strategy": { "type": "string", "enum": ["first-script", "exceptions", "url"], "description": "first-script: Debugger.setInstrumentationBreakpoint(beforeScriptExecution). exceptions: Debugger.setPauseOnExceptions — pauses on every throw (slow, but sees engine-thrown errors). url: Debugger.setBreakpointByUrl with the given pattern." },
                    "url_pattern": { "type": "string", "description": "Regex of script URL to break on (only with strategy=url)." },
                    "uncaught_only": { "type": "boolean", "description": "strategy=exceptions: pause only on UNCAUGHT exceptions (default: all throws)." },
                    "max_frames": { "type": "integer", "description": "Keep at most N call frames per pause (default: all). Use on an exception storm." },
                    "sample_every": { "type": "integer", "description": "Record only every Nth pause (default 1 = all). Use on an exception storm." }
                }
            }),
        },
        ToolDefinition {
            name: "diag_debugger_snapshot".to_string(),
            description: "Pull the call frames buffered by diag_debugger_arm. Includes call stacks at each pause point. Pass `clear: true` to also empty the buffer.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "clear": { "type": "boolean" }
                }
            }),
        },
        ToolDefinition {
            name: "diag_debugger_disarm".to_string(),
            description: "Disable the V8 debugger and drop any buffered frames.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },
        ToolDefinition {
            name: "intercept".to_string(),
            description: "Add a client-side network fault-injection rule for fetch/XHR requests. Actions: `hang` (never respond; honors the app's AbortController), `delay` (hold N ms then pass through), `fail` (immediate network error), `mock` (respond with a given status/body without hitting the network). Rules persist across navigations (document-start re-injection) and are cleared when the daemon stops. Adding a rule with the same pattern+method replaces it.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "pattern": { "type": "string", "description": "URL pattern. Without `*`: substring match. With `*`: glob against the full URL (e.g. `*/portfolio*`)." },
                    "action": { "type": "string", "enum": ["hang", "delay", "fail", "mock"], "description": "What to do with matching requests." },
                    "ms": { "type": "integer", "description": "delay: wait duration (required). mock: response latency. hang: optional auto-error timeout (default: hang forever)." },
                    "status": { "type": "integer", "description": "mock: HTTP status code (default 200)." },
                    "body": { "type": "string", "description": "mock: response body (default empty)." },
                    "content_type": { "type": "string", "description": "mock: Content-Type header (default application/json)." },
                    "method": { "type": "string", "description": "Only intercept this HTTP method (GET, POST, …). Default: all methods." }
                },
                "required": ["pattern", "action"]
            }),
        },
        ToolDefinition {
            name: "intercept_list".to_string(),
            description: "List active network fault-injection rules and the rule-set version currently applied to the page.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },
        ToolDefinition {
            name: "intercept_clear".to_string(),
            description: "Remove network fault-injection rules — all of them, or only those whose pattern equals `pattern`.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "pattern": { "type": "string", "description": "Only remove rules with exactly this pattern (default: remove all)." }
                }
            }),
        },
        ToolDefinition {
            name: "inject_script".to_string(),
            description: "Run a script at DOCUMENT START on every navigation, before any page script. This is the only way to observe what happens before the page's own code runs — `execute_js` needs a loaded document, so by the time it arrives the first API call, the first DOM insertion, the first assignment have already happened. CHOOSING THE WORLD MATTERS: `isolated` (default) shares the DOM but CANNOT see page globals, so it cannot wrap `window.fetch` or any object the page made; use `main` to pre-empt a page API. The isolated world is the same one `execute_js` uses with `world: \"isolated\"`, so values the injected script leaves behind are readable from there. Windows uses CDP; macOS uses WKUserScript. Scripts live in daemon memory and die with it.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "name": { "type": "string", "description": "Label for this script, unique per daemon. Used by inject_clear." },
                    "script": { "type": "string", "description": "JavaScript source to run at document start." },
                    "world": { "type": "string", "enum": ["isolated", "main"], "description": "`isolated` (default): DOM only, page globals invisible, page cannot observe it. `main`: sees page globals — required to wrap or watch an API before page scripts reach it." }
                },
                "required": ["name", "script"]
            }),
        },
        ToolDefinition {
            name: "inject_list".to_string(),
            description: "List the document-start scripts registered by inject_script, with the world each one runs in.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "site": site_param() }
            }),
        },
        ToolDefinition {
            name: "inject_clear".to_string(),
            description: "Unregister document-start scripts — one by `name`, or all of them. On macOS an individual WKUserScript cannot be removed, so the response says plainly that the script keeps running until the daemon restarts rather than implying a clean page.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "site": site_param(),
                    "name": { "type": "string", "description": "Only remove the script with this name (default: remove all)." }
                }
            }),
        },
    ]
}

// ============================================================================
// Tool Execution
// ============================================================================

pub async fn execute_tool(name: &str, args: &Value) -> Result<Value> {
    if crate::main_frame::starts_navigation(name) {
        crate::main_frame::note_action();
    }
    // The Chrome engine implements a few tools itself (input, frames,
    // navigation, WebView2-only diagnostics); everything else is shared and
    // reaches Chrome through the engine-aware JS executor.
    if crate::engine::is_chrome() {
        if let Some(r) = crate::chrome::tools::dispatch(name, args).await {
            return r;
        }
    }
    match name {
        "navigate" => tool_navigate(args).await,
        "get_html" => tool_get_html(args).await,
        "get_text" => tool_get_text(args).await,
        "execute_js" => tool_execute_js(args).await,
        "screenshot" => tool_screenshot(args).await,
        "wait" => tool_wait(args).await,
        "wait_navigation" => tool_wait_navigation(args).await,
        "get_current_url" => tool_get_current_url(args).await,
        "wait_for_selector" => tool_wait_for_selector(args).await,
        "fill_form" => tool_fill_form(args).await,
        "preprocess_captcha" => tool_preprocess_captcha(args).await,
        "get_captured_requests" => tool_get_captured_requests(args).await,
        "get_response_headers" => tool_get_response_headers(args).await,
        "navigation_log" => crate::tools::network::tool_navigation_log(args).await,
        "failed_requests" => crate::tools::network::tool_failed_requests(args).await,
        "record" => crate::tools::network::tool_record(args).await,
        "cookies" => crate::cookies::tool_cookies(args).await,
        "reset" => crate::reset::tool_reset(args).await,
        "get_frame_list" => tool_get_frame_list(args).await,
        "get_frame_html" => tool_get_frame_html(args).await,
        "auto_dismiss_modals" => tool_auto_dismiss_modals(args).await,
        "click" => tool_click(args).await,
        "scroll" => tool_scroll(args).await,
        "keyboard_input" => tool_keyboard_input(args).await,
        "element_at" => tool_element_at(args).await,
        "check_visible" => tool_check_visible(args).await,
        "click_element" => tool_click_element(args).await,
        "upload_file" => tool_upload_file(args).await,
        "save_image" => tool_save_image(args).await,
        "describe" => tool_describe(args).await,
        "resize_window" => tool_resize_window(args).await,
        "back" => tool_history(args, "back").await,
        "forward" => tool_history(args, "forward").await,
        "reload" => tool_reload(args).await,
        "clear_site_data" => crate::site_data::clear_site_data(args).await,
        "hover" => tool_hover(args).await,
        "console_logs" => tool_console_logs(args).await,
        // Renderer diagnostics (out-of-band inspection for hung/slow webviews).
        "diag_pause" => crate::diagnostics::pause(args).await,
        "diag_console_dump" => crate::diagnostics::console_dump(args).await,
        "diag_sw_console" => crate::diagnostics::sw_console(args).await,
        "diag_network_log" => crate::diagnostics::network_log(args).await,
        "diag_profile_start" => crate::diagnostics::profile_start(args).await,
        "diag_profile_stop" => crate::diagnostics::profile_stop(args).await,
        "diag_heap_snapshot" => crate::diagnostics::heap_snapshot(args).await,
        "diag_attach_devtools" => crate::diagnostics::attach_devtools(args).await,
        "diag_exec_js_isolated" => crate::diagnostics::exec_js_isolated(args).await,
        "diag_wait_for_condition" => crate::diagnostics::wait_for_condition(args).await,
        "diag_trace" => crate::diagnostics::trace(args).await,
        "diag_debugger_arm" => crate::diagnostics::debugger_arm(args).await,
        "diag_debugger_snapshot" => crate::diagnostics::debugger_snapshot(args).await,
        "diag_debugger_disarm" => crate::diagnostics::debugger_disarm(args).await,
        "dump_recording" => tool_dump_recording(args).await,
        "intercept" => tool_intercept(args).await,
        "intercept_list" => tool_intercept_list(args).await,
        "intercept_clear" => tool_intercept_clear(args).await,
        "inject_script" => tool_inject_script(args).await,
        "inject_list" => tool_inject_list(args).await,
        "inject_clear" => tool_inject_clear(args).await,
        _ => Err(anyhow!("Unknown tool: {}", name)),
    }
}

/// dump_recording — snapshot the daemon-side recording buffer.
///
/// This is a pure in-memory read. It deliberately does NOT call the renderer:
/// the whole point of the recording is to survive a renderer wedge, so this
/// tool must keep working when every CDP-/JS-based tool would time out.
async fn tool_dump_recording(args: &Value) -> Result<Value> {
    let filter = args.get("filter").and_then(|v| v.as_str());
    let since_ms = args.get("since_ms").and_then(|v| v.as_u64());
    let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
    // A tape stopped by `record off` is still evidence: read it, and say it
    // stopped, rather than refusing as if nothing had ever been recorded.
    let stopped_with_events = !crate::recording::is_enabled() && crate::recording::has_events();
    let snap = crate::recording::snapshot(filter, since_ms, clear);

    // Guard rail: if recording was never enabled, say so explicitly. Otherwise
    // the caller might think the page is silent when really `--record` is off.
    if snap.get("enabled").and_then(|v| v.as_bool()) != Some(true) && !stopped_with_events {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "RECORDING_NOT_ENABLED",
            "message": "Background recording is off. Turn it on for this live daemon with \
                        `dalzzack record on -i <id>` (events from before that moment are not \
                        recoverable), or start the daemon with `--record`.",
            "transport": snap.get("transport").cloned().unwrap_or(Value::Null),
        }));
    }
    let mut snap = snap;
    if stopped_with_events {
        if let Some(obj) = snap.as_object_mut() {
            obj.insert(
                "note".into(),
                serde_json::json!(
                    "recording is OFF (`record off`); this is what was captured while it was on — \
                     nothing after stopped_at_ms is on the tape"
                ),
            );
        }
    }

    // Enabled but never armed: the capture transport failed to come up, so an
    // empty tape here would be a lie ("the page was silent") rather than an
    // answer. Say which it is.
    if snap.get("armed").and_then(|v| v.as_bool()) != Some(true) {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "RECORDING_NOT_ARMED",
            "message": format!(
                "Recording was requested but the capture transport never came up: {}. \
                 The buffer is empty because nothing was ever captured — not because the page was quiet.",
                snap.get("arm_error").and_then(|v| v.as_str()).unwrap_or("reason unknown (see daemon log)")
            ),
            "transport": snap.get("transport").cloned().unwrap_or(Value::Null),
            "arm_error": snap.get("arm_error").cloned().unwrap_or(Value::Null),
        }));
    }

    if let Some(output) = args.get("output").and_then(|v| v.as_str()) {
        let bytes = serde_json::to_vec_pretty(&snap)?;
        std::fs::write(output, &bytes)?;
        return Ok(serde_json::json!({
            "status": "completed",
            "output_path": output,
            "bytes_written": bytes.len(),
            "enabled": snap.get("enabled"),
            "transport": snap.get("transport"),
            "counts": snap.get("counts"),
            "dropped": snap.get("dropped"),
            "renderer_wedged_at_ms": snap.get("renderer_wedged_at_ms"),
            "last_poll_ok_ms": snap.get("last_poll_ok_ms"),
        }));
    }

    Ok(snap)
}

/// back / forward — drive `history.back()` / `history.forward()`.
///
/// `history.back/forward()` triggers an async navigation. We can't `await` from
/// inside the page (the sync JS bridge rejects Promise returns), so we poll the
/// URL for up to ~3 s and report whichever of these outcomes happens first:
///   - URL changed → `changed: true`
///   - Timed out, URL still same → `changed: false` (likely no history to go to)
async fn tool_history(_args: &Value, dir: &str) -> Result<Value> {
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    let trigger = format!("var u = location.href; history.{dir}(); return u;");
    let before = match webview_execute_js(&trigger, Some(3000)).await {
        Ok(v) => v.as_str().unwrap_or("").to_string(),
        Err(e) => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "HISTORY_ERROR",
                "message": e,
            }));
        }
    };

    // Poll for navigation to settle. We sample 15 × 200 ms = 3 s total.
    // First sample at 100 ms catches the common fast case; the rest catch slow
    // SPAs and full-document navigations.
    const POLL_INTERVAL_MS: u64 = 200;
    const POLL_MAX_ITERS: usize = 15;
    let probe = "return JSON.stringify({ url: location.href, title: document.title, ready: document.readyState });".to_string();

    let mut last: Value = serde_json::json!({});
    let mut after_url = before.clone();
    let mut elapsed_ms: u64 = 0;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    elapsed_ms += 100;

    for _ in 0..POLL_MAX_ITERS {
        last = webview_execute_js(&probe, Some(2000))
            .await
            .ok()
            .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or(serde_json::json!({}));
        after_url = last["url"].as_str().unwrap_or("").to_string();
        let ready = last["ready"].as_str().unwrap_or("");
        if !before.is_empty() && after_url != before && ready == "complete" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        elapsed_ms += POLL_INTERVAL_MS;
    }

    Ok(serde_json::json!({
        "status": "completed",
        "direction": dir,
        "before_url": before,
        "after_url": after_url,
        "title": last["title"],
        "ready_state": last["ready"],
        "elapsed_ms": elapsed_ms,
        "changed": !before.is_empty() && before != after_url,
    }))
}

/// reload — refresh the current page.
///
/// `hard: true` bypasses the HTTP cache via the platform's native path, which
/// also means it doesn't depend on the page's JS world being cooperative.
/// Without it, the historical `location.reload()` behaviour is unchanged.
async fn tool_reload(args: &Value) -> Result<Value> {
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    if args.get("hard").and_then(|v| v.as_bool()).unwrap_or(false) {
        return crate::site_data::hard_reload().await;
    }

    let script = "var u = location.href; location.reload(); return u;";
    match webview_execute_js(script, Some(5000)).await {
        Ok(v) => Ok(serde_json::json!({
            "status": "completed",
            "mode": "normal",
            "url": v,
        })),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "RELOAD_ERROR",
            "message": e,
        })),
    }
}

/// hover — move the OS cursor over an element (when possible) and dispatch JS hover events.
///
/// Two-phase to do this right:
///   1. Resolve the target's viewport center (selector → bounding rect, or use the given x/y).
///   2. macOS: move the native cursor via CGEvent so the WebView's CSS `:hover` pseudo-class
///      actually fires — this is what makes dropdown menus open. Then also dispatch JS
///      `mouseover`/`mouseenter`/`mousemove` for sites whose handlers listen for them.
///      Windows/Linux: dispatch JS events only and warn via `native_hover: false` so the
///      caller knows CSS `:hover` may not have triggered.
async fn tool_hover(args: &Value) -> Result<Value> {
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }
    let selector = args["selector"].as_str();
    let req_x = args["x"].as_f64();
    let req_y = args["y"].as_f64();

    if selector.is_none() && (req_x.is_none() || req_y.is_none()) {
        return Err(anyhow!("hover requires either selector or x+y"));
    }

    // Phase 1: resolve target center.
    let resolve_script = if let Some(sel) = selector {
        let sel_escaped = sel
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('"', "\\\"");
        format!(
            r#"var el = document.querySelector("{sel}");
            if (!el) return JSON.stringify({{ found: false }});
            el.scrollIntoView({{ block: 'center', behavior: 'instant' }});
            var r = el.getBoundingClientRect();
            return JSON.stringify({{
                found: true,
                tag: el.tagName.toLowerCase(),
                cx: r.x + r.width/2,
                cy: r.y + r.height/2,
                rect: {{ x: Math.round(r.x), y: Math.round(r.y), width: Math.round(r.width), height: Math.round(r.height) }}
            }});"#,
            sel = sel_escaped,
        )
    } else {
        let x = req_x.unwrap();
        let y = req_y.unwrap();
        format!(
            r#"var el = document.elementFromPoint({x}, {y});
            if (!el) return JSON.stringify({{ found: false }});
            return JSON.stringify({{
                found: true,
                tag: el.tagName.toLowerCase(),
                cx: {x}, cy: {y}
            }});"#,
            x = x,
            y = y,
        )
    };

    let target: Value = match webview_execute_js(&resolve_script, Some(5000)).await {
        Ok(v) => v
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::json!({})),
        Err(e) => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "HOVER_ERROR",
                "message": e,
            }));
        }
    };

    if target["found"] != serde_json::Value::Bool(true) {
        let nearby = collect_nearby_interactive(8).await;
        let err = if selector.is_some() {
            "ELEMENT_NOT_FOUND"
        } else {
            "NO_ELEMENT_AT_POINT"
        };
        return Ok(serde_json::json!({
            "status": "failed",
            "error": err,
            "nearby": nearby,
            "hint": "Nothing matched. See `nearby` for visible interactive elements.",
        }));
    }

    let cx = target["cx"].as_f64().unwrap_or(0.0);
    let cy = target["cy"].as_f64().unwrap_or(0.0);

    // Phase 2a: native cursor move (macOS only). Tells the WebView's CSS engine
    // to flip `:hover` on the underlying element.
    let native_hover_ok = crate::platform::native_mouse_move(cx, cy).await.is_ok();

    // Phase 2b: JS event dispatch — covers handlers that listen to mouseover/etc
    // directly, and is the only signal on Windows/Linux right now.
    let js_dispatch = format!(
        r#"var el = document.elementFromPoint({cx}, {cy});
        if (!el) return 'no-element';
        ['mouseover','mouseenter','mousemove'].forEach(function(t) {{
            el.dispatchEvent(new MouseEvent(t, {{ bubbles: true, cancelable: true, clientX: {cx}, clientY: {cy} }}));
        }});
        return el.tagName.toLowerCase();"#,
        cx = cx,
        cy = cy,
    );
    let _ = webview_execute_js(&js_dispatch, Some(3000)).await;

    let mut response = serde_json::json!({
        "status": "completed",
        "tag": target["tag"],
        "center": { "x": cx.round() as i64, "y": cy.round() as i64 },
        "native_hover": native_hover_ok,
    });
    if let Some(rect) = target.get("rect") {
        response["rect"] = rect.clone();
    }
    if !native_hover_ok {
        response["note"] = Value::String(
            "Native cursor move unavailable on this platform — CSS `:hover` styles may not apply. JS mouseover/enter events were still dispatched.".into(),
        );
    }
    Ok(response)
}

/// console_logs — read captured console output.
///
/// Reads the daemon-side buffer when an out-of-band transport is armed (CDP on
/// Windows, WebKit system-console logging on macOS), which also means it keeps
/// answering after the renderer wedges. Only the in-page fallback still needs a
/// live page.
/// `console-logs --frame X` → the frame ids to keep, and whether lines without
/// a frame id (the top document's network/policy messages) count. Same
/// vocabulary as `exec-js --frame`: `top`, a `get-frames` index, or a frame id.
async fn console_frame_ids(frame: &str) -> Result<(Vec<String>, bool), Value> {
    if frame.eq_ignore_ascii_case("top") || frame.eq_ignore_ascii_case("main") {
        let top = crate::platform::describe_default_target().await.frame_id;
        return Ok((top.into_iter().collect(), true));
    }
    if let Ok(idx) = frame.parse::<usize>() {
        let listing = Box::pin(execute_tool(
            "get_frame_list",
            &serde_json::json!({ "site": crate::daemon::instance_id().unwrap_or_default() }),
        ))
        .await
        .unwrap_or_else(|e| serde_json::json!({ "status": "failed", "message": e.to_string() }));
        let frames = listing["frames"].as_array().cloned().unwrap_or_default();
        return match frames.get(idx) {
            Some(f) => match f["frame_id"].as_str() {
                Some(id) => Ok((vec![id.to_string()], false)),
                None => Err(serde_json::json!({
                    "status": "failed",
                    "error": "FRAME_ID_UNKNOWN",
                    "message": format!("get-frames index {idx} has no frame id"),
                })),
            },
            None => Err(serde_json::json!({
                "status": "failed",
                "error": "FRAME_INDEX_OUT_OF_RANGE",
                "message": format!(
                    "frame index {idx} out of range — get-frames lists {} iframe(s)",
                    frames.len()
                ),
            })),
        };
    }
    Ok((vec![frame.to_string()], false))
}

pub(crate) async fn tool_console_logs(args: &Value) -> Result<Value> {
    use crate::console::Transport;

    // Whitelist the level filter so a caller can't inject JS via the format!
    // template used by the fallback path. Anything else (including empty) means
    // "all levels".
    let level_filter = match args["level"].as_str().unwrap_or("") {
        l @ ("log" | "info" | "warn" | "error" | "debug") => l,
        "" => "",
        other => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "INVALID_LEVEL",
                "message": format!("level must be one of: log, info, warn, error, debug (got: {})", other),
            }));
        }
    };
    // Same whitelist rationale as `level`.
    let source_filter = match args["source"].as_str().unwrap_or("") {
        s @ ("console" | "csp" | "security" | "network" | "deprecation" | "violation"
        | "javascript" | "other") => s,
        "" => "",
        other => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "INVALID_SOURCE",
                "message": format!(
                    "source must be one of: console, csp, security, network, deprecation, \
                     violation, javascript, other (got: {other})"
                ),
            }));
        }
    };
    // Up to the whole ring: `--console-buffer` can make it larger than 500.
    let max = (args["max"].as_u64().unwrap_or(100) as usize).min(crate::console::capacity());
    let clear = args["clear"].as_bool().unwrap_or(false);
    let text_filter = args["text"].as_str().filter(|s| !s.is_empty());
    let frame_arg = args["frame"].as_str().filter(|s| !s.is_empty());
    let transport = crate::console::transport();

    if transport == Transport::Disabled {
        return Ok(serde_json::json!({
            "status": "completed",
            "count": 0,
            "logs": [],
            "cleared": false,
            "console_transport": transport.as_str(),
            "csp_bypassed": crate::state::csp_bypassed(),
            "hint": "Console capture is off — the daemon was started with --no-console-capture. \
                     Restart it without that flag (stop, then start) to collect console output.",
        }));
    }

    // The transport this platform should have had never came up. Reading the
    // in-page buffer here would return `[]` — indistinguishable from a quiet
    // page, which is the exact confusion `Unavailable` exists to prevent.
    if transport == Transport::Unavailable {
        return Ok(serde_json::json!({
            "status": "completed",
            "count": 0,
            "logs": [],
            "cleared": false,
            "console_transport": transport.as_str(),
            "csp_bypassed": crate::state::csp_bypassed(),
            "console_transport_error": crate::console::transport_error(),
            "hint": "No console capture is armed, so this empty list says NOTHING about \
                     whether the page logged anything. See `console_transport_error` for why. \
                     On macOS, restart with `--console-capture` to accept a page-visible \
                     in-page hook.",
        }));
    }

    // Out-of-band transports: daemon memory, no renderer involvement.
    if transport.is_out_of_band() {
        let level = (!level_filter.is_empty()).then_some(level_filter);
        let source = (!source_filter.is_empty()).then_some(source_filter);
        let frame_ids = match frame_arg {
            Some(f) => match console_frame_ids(f).await {
                Ok(ids) => Some(ids),
                Err(problem) => return Ok(problem),
            },
            None => None,
        };
        let filter = crate::console::Filter {
            level,
            source,
            frames: frame_ids
                .as_ref()
                .map(|(ids, top)| crate::console::FrameSet {
                    ids: ids.iter().map(String::as_str).collect(),
                    include_untagged: *top,
                }),
            text: text_filter,
        };
        let (logs, matched, dropped) = crate::console::snapshot_filtered(&filter, max, clear);

        // Zero CSP violations means nothing at all while enforcement is off:
        // no load is ever blocked, so no violation can ever be reported. Saying
        // so is the difference between "this page's policy is fine" and "we
        // were not in a position to tell".
        let csp_bypassed = crate::state::csp_bypassed();
        let hint = (source_filter == "csp" && logs.is_empty() && csp_bypassed).then(|| {
            "This daemon bypasses Content Security Policy (CDP Page.setBypassCSP), so nothing \
             is ever blocked and no violation can be reported — an empty result here says \
             nothing about the page's policy. Restart with `dalzzack start --id <ID> \
             --enforce-csp` to observe real violations (it costs cross-origin iframe access). \
             To read the policy itself instead, use `response-headers --resource-type document`."
                .to_string()
        });

        // A ring that has overflowed has lost its OLDEST lines — the page's
        // first messages, usually the ones that matter.
        let hint = hint.or_else(|| {
            (dropped > 0).then(|| {
                format!(
                    "The ring buffer holds {} entries and has dropped the {dropped} oldest. \
                     Read sooner, --clear between reads, or restart with `start \
                     --console-buffer N` (up to {}).",
                    crate::console::capacity(),
                    crate::console::MAX_CAPACITY
                )
            })
        });

        return Ok(serde_json::json!({
            "status": "completed",
            "count": logs.len(),
            // Before `--max` cut it down.
            "matched": matched,
            "logs": logs,
            "cleared": clear,
            "console_transport": transport.as_str(),
            "csp_bypassed": csp_bypassed,
            "dropped": dropped,
            "buffer_size": crate::console::capacity(),
            "frame_ids": frame_ids.as_ref().map(|(ids, _)| ids),
            "hint": hint,
        }));
    }

    // Fallback: the in-page ring buffer. Needs a live renderer — and it only
    // ever holds page `console.*` calls, so any other source filter is empty
    // by construction rather than by accident.
    if !source_filter.is_empty() && source_filter != "console" {
        return Ok(serde_json::json!({
            "status": "completed",
            "count": 0,
            "logs": [],
            "cleared": false,
            "console_transport": transport.as_str(),
            "csp_bypassed": crate::state::csp_bypassed(),
            "hint": format!(
                "source={source_filter} is only available from an out-of-band transport \
                 (CDP / os_log); the in-page fallback captures the page's own console.* only."
            ),
        }));
    }
    // The in-page hook lives in the top document only, so it has no other
    // frame's lines to give — an empty answer must not read as "that frame was
    // quiet".
    if frame_arg.is_some_and(|f| !f.eq_ignore_ascii_case("top") && !f.eq_ignore_ascii_case("main"))
    {
        return Ok(serde_json::json!({
            "status": "completed",
            "count": 0,
            "logs": [],
            "cleared": false,
            "console_transport": transport.as_str(),
            "hint": "--frame needs an out-of-band transport (CDP: Windows, or `--engine \
                     chrome`); the in-page fallback only ever sees the top document's console.",
        }));
    }
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready",
            "console_transport": transport.as_str(),
        }));
    }
    let script = format!(
        r#"var NS = window.{ns} || {{}};
        var logs = NS.logs || [];
        var filtered = logs;
        var lvl = "{level}";
        if (lvl) filtered = filtered.filter(function(l) {{ return l.level === lvl; }});
        var txt = {text};
        if (txt) filtered = filtered.filter(function(l) {{ return String(l.message || '').toLowerCase().indexOf(txt) >= 0; }});
        var slice = filtered.slice(-{max});
        if ({clear}) NS.logs = [];
        return JSON.stringify(slice);"#,
        ns = crate::pagens::namespace(),
        level = level_filter,
        // A JSON string literal is a JS string literal: no injection.
        text = serde_json::to_string(&text_filter.unwrap_or("").to_lowercase())
            .unwrap_or_else(|_| "\"\"".into()),
        max = max,
        clear = clear,
    );

    match webview_execute_js(&script, Some(3000)).await {
        Ok(v) => {
            let logs: Vec<Value> = v
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            Ok(serde_json::json!({
                "status": "completed",
                "count": logs.len(),
                "logs": logs,
                "cleared": clear,
                "console_transport": transport.as_str(),
                "csp_bypassed": crate::state::csp_bypassed(),
                "transport_error": crate::console::transport_error(),
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "CONSOLE_LOGS_ERROR",
            "message": e,
            "console_transport": transport.as_str(),
        })),
    }
}

/// get_response_headers — response headers captured from CDP.
///
/// The navigation document is the point of this tool: it is where CSP, CORP,
/// cache policy and cookies are decided, and no other tool can see it. The
/// in-page hook behind `get-requests` cannot — it does not exist yet when the
/// document is fetched — so there is no fallback to offer on platforms without
/// CDP.
pub(crate) async fn tool_get_response_headers(args: &Value) -> Result<Value> {
    if !crate::engine::has_cdp() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "UNSUPPORTED_PLATFORM",
            "message": "`response-headers` needs CDP Network events (WebView2 on Windows, or \
                        `start --engine chrome`). There is no in-page fallback: a page script \
                        cannot read response headers, and the navigation document is fetched \
                        before any script runs.",
        }));
    }
    {
        let url_pattern = args
            .get("url_pattern")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let resource_type = args
            .get("resource_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let max = args
            .get("max")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .min(500) as usize;
        let redact = !args
            .get("no_redact")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (mut entries, matched) =
            crate::response_headers::snapshot(url_pattern, resource_type, max);
        if redact {
            crate::response_headers::redact(&mut entries);
        }

        let armed = crate::response_headers::is_armed();
        // An empty result from a live capture means "nothing matched"; from a
        // capture that never came up it means nothing at all.
        let hint = if !armed {
            Some(
                "CDP Network capture is not armed for this daemon, so nothing was recorded. \
                 Check `dalzzack list` for renderer_ok / console_transport, then stop and \
                 start the daemon."
                    .to_string(),
            )
        } else if entries.is_empty() {
            Some(
                "Capture is armed but nothing matched. Response headers are recorded from the \
                 moment the daemon starts, so navigate first; note that `--resource-type document` \
                 only returns navigations."
                    .to_string(),
            )
        } else {
            None
        };

        Ok(serde_json::json!({
            "status": "completed",
            "network_armed": armed,
            "returned": entries.len(),
            "matched": matched,
            "redacted": redact,
            "responses": entries,
            "hint": hint,
        }))
    }
}

/// resize_window — resize the WebView window.
async fn tool_resize_window(args: &Value) -> Result<Value> {
    use crate::APP_HANDLE;
    use tauri::{LogicalSize, Manager};

    let width = args["width"]
        .as_u64()
        .ok_or_else(|| anyhow!("width required (positive integer)"))?;
    let height = args["height"]
        .as_u64()
        .ok_or_else(|| anyhow!("height required (positive integer)"))?;

    if width < 100 || height < 100 {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_SIZE",
            "message": "width and height must be >= 100",
        }));
    }

    let app_handle = APP_HANDLE.read().await;
    let app = app_handle
        .as_ref()
        .ok_or_else(|| anyhow!("AppHandle not available"))?;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| anyhow!("Main window not found"))?;

    window
        .set_size(LogicalSize::new(width as f64, height as f64))
        .map_err(|e| anyhow!("set_size failed: {}", e))?;

    tracing::info!("[resize_window] Resized to {}x{}", width, height);

    Ok(serde_json::json!({
        "status": "completed",
        "width": width,
        "height": height,
    }))
}

// ============================================================================
// Tool Implementations
// ============================================================================

/// Collect up to `limit` visible interactive elements (button/link/input/textarea/select/[role=button]).
///
/// Used to enrich selector-not-found errors: when the agent's CSS selector or
/// text query misses, we return a small list of nearby candidates so the LLM
/// can self-correct without burning a full `describe` call (~1500 image tokens)
/// just to re-discover the page.
pub(crate) async fn collect_nearby_interactive(limit: usize) -> Vec<Value> {
    let script = format!(
        r#"var els = Array.from(document.querySelectorAll('button,a[href],input,textarea,select,[role="button"]'))
            .filter(function(el) {{
                var r = el.getBoundingClientRect();
                if (r.width === 0 || r.height === 0) return false;
                var s = window.getComputedStyle(el);
                if (s.display === 'none' || s.visibility === 'hidden' || s.opacity === '0') return false;
                if (r.bottom < 0 || r.top > window.innerHeight) return false;
                if (r.right < 0 || r.left > window.innerWidth) return false;
                return true;
            }})
            .slice(0, {limit});
        function pickSelector(el) {{
            if (el.id) return '#' + el.id;
            if (el.name) return el.tagName.toLowerCase() + '[name="' + el.name + '"]';
            var cls = Array.from(el.classList).filter(function(c) {{ return !c.startsWith('css-') && c.length < 30; }}).slice(0, 2);
            if (cls.length) return el.tagName.toLowerCase() + '.' + cls.join('.');
            var tp = el.getAttribute('type');
            return el.tagName.toLowerCase() + (tp ? '[type="' + tp + '"]' : '');
        }}
        return JSON.stringify(els.map(function(el) {{
            var r = el.getBoundingClientRect();
            var t = (el.innerText || (el.type==='password'&&el.value?'***':el.value) || el.getAttribute('aria-label') || el.getAttribute('placeholder') || '').trim();
            return {{
                tag: el.tagName.toLowerCase(),
                text: t.substring(0, 60),
                selector: pickSelector(el),
                x: Math.round(r.left + r.width/2),
                y: Math.round(r.top + r.height/2)
            }};
        }}));"#,
        limit = limit,
    );

    match webview_execute_js(&script, Some(3000)).await {
        Ok(v) => v
            .as_str()
            .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The folder for one instance's files, created when missing.
async fn site_dir(site: &str) -> Result<std::path::PathBuf> {
    let dir = get_sites_path().join(site);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

/// navigate — go to a URL.
async fn tool_navigate(args: &Value) -> Result<Value> {
    let site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow!("url required"))?;
    let timeout_ms = args["timeout_ms"].as_u64().map(|v| v as u32);

    tracing::info!(
        "navigate: site={}, url={}, timeout_ms={:?}",
        site,
        url,
        timeout_ms
    );

    // Check that the WebView is ready
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready. Make sure the daemon is running (dalzzack start --id <ID>)."
        }));
    }

    // Everything the timeline records after this point belongs to this
    // navigation (or to what it set off).
    let seq_before = crate::netlog::last_seq();

    // Navigate the WebView to the URL
    let navigated = webview_navigate(url, timeout_ms).await;
    // Whatever failed during this call is this call's to report.
    crate::main_frame::consume();
    match navigated {
        Ok(result) => {
            // Update CURRENT_SITE
            {
                let mut current = CURRENT_SITE.write().await;
                *current = Some(site.to_string());
            }

            if let Some(failure) = result.failure {
                return Ok(failed_navigation(url, &failure, &Value::Null));
            }
            let report = crate::tools::network::navigation_report(seq_before, Some(url));
            if crate::main_frame::document_failed(&report["document"]) {
                let failure = serde_json::json!({
                    "failing_url": report["document"]["final_url"],
                    "error_text": report["document"]["error_text"],
                    "message": report["document"]["error_text"],
                });
                return Ok(failed_navigation(url, &failure, &report));
            }

            let mut out = serde_json::json!({
                "status": "completed",
                "url": result.url,
                "title": result.title
            });
            // The main document's status and every hop it went through — a
            // challenge that solved itself used to come back as a plain
            // `completed` with only the final page to show for it.
            if let (Some(obj), Some(report)) = (out.as_object_mut(), report.as_object()) {
                for (k, v) in report {
                    obj.insert(k.clone(), v.clone());
                }
            }
            Ok(out)
        }
        Err(e) => {
            let (error_code, message) = if e.contains("NAVIGATION_TIMEOUT") {
                ("NAVIGATION_TIMEOUT", "Page load timed out")
            } else if e.contains("WEBVIEW_NOT_FOUND") {
                ("WEBVIEW_NOT_FOUND", "WebView not found")
            } else {
                ("NAVIGATION_FAILED", e.as_str())
            };

            Ok(serde_json::json!({
                "status": "failed",
                "error": error_code,
                "message": message
            }))
        }
    }
}

/// The result of a navigation whose main-frame load failed. `failure` holds
/// the fields of [`crate::main_frame::Failure::to_json`]; `report` is the
/// navigation report, when there is one.
pub(crate) fn failed_navigation(url: &str, failure: &Value, report: &Value) -> Value {
    let mut out = serde_json::json!({
        "status": "failed",
        "error": "NAVIGATION_FAILED",
        "url": url,
        "engine": crate::engine::current().as_str(),
    });
    if let (Some(obj), Some(report)) = (out.as_object_mut(), report.as_object()) {
        for (k, v) in report.iter().filter(|(k, _)| k.as_str() != "hint") {
            obj.insert(k.clone(), v.clone());
        }
    }
    if let (Some(obj), Some(failure)) = (out.as_object_mut(), failure.as_object()) {
        for (k, v) in failure.iter().filter(|(_, v)| !v.is_null()) {
            obj.insert(k.clone(), v.clone());
        }
    }
    if out["message"].as_str().is_none_or(str::is_empty) {
        out["message"] = serde_json::json!("the page did not load");
    }
    out["hint"] = serde_json::json!(
        "The page did not load. The window does not show this URL: it still shows the page \
         before it, or the browser's own error page. `get-text` and `describe` read that page, \
         not this site."
    );
    out
}

/// get_html — fetch page HTML.
/// `file_only`: when combined with `save_to`, omit HTML from the response (saves tokens).
/// `strip_noise`: strip `<script>`/`<style>`/`<noscript>`/`<svg>`/comments and noisy attributes.
/// `max_length`: cap the length of the returned HTML.
async fn tool_get_html(args: &Value) -> Result<Value> {
    let site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let selector = args["selector"].as_str();
    let save_to = args["save_to"].as_str();
    let timeout_ms = args["timeout_ms"].as_u64().map(|v| v as u32);
    let file_only = args["file_only"].as_bool().unwrap_or(false);
    let do_strip_noise = args["strip_noise"].as_bool().unwrap_or(false);
    let max_length = args["max_length"].as_u64().unwrap_or(0) as usize;

    tracing::info!(
        "get_html: site={}, selector={:?}, save_to={:?}, file_only={}, strip_noise={}, max_length={}",
        site, selector, save_to, file_only, do_strip_noise, max_length
    );

    // Check the WebView
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    // Get the HTML
    match webview_get_html(selector, timeout_ms).await {
        Ok(html_opt) => match html_opt {
            Some(html) => {
                let original_length = html.len();

                // 1. Save to the file (always the original HTML)
                let saved_to = match save_to {
                    Some(filename) => {
                        let path = site_dir(site).await?.join(filename);
                        tokio::fs::write(&path, &html)
                            .await
                            .with_context(|| format!("cannot write {}", path.display()))?;
                        Some(path.display().to_string())
                    }
                    None => None,
                };

                // 2. file_only: together with save_to, do not return the HTML
                if file_only && saved_to.is_some() {
                    return Ok(serde_json::json!({
                        "status": "completed",
                        "html": null,
                        "length": original_length,
                        "saved_to": saved_to,
                        "file_only": true
                    }));
                }

                // 3. strip_noise: remove the tags and attributes that add nothing
                let processed = if do_strip_noise {
                    strip_html_noise(&html)
                } else {
                    html
                };

                // 4. max_length: limit the character count
                let (final_html, truncated) = if max_length > 0 && processed.len() > max_length {
                    (cut_text(&processed, max_length).to_string(), true)
                } else {
                    (processed, false)
                };

                let mut response = serde_json::json!({
                    "status": "completed",
                    "html": final_html,
                    "length": original_length
                });
                if truncated {
                    response["truncated"] = serde_json::json!(true);
                    response["truncated_at"] = serde_json::json!(max_length);
                }
                if do_strip_noise {
                    response["stripped_length"] = serde_json::json!(final_html.len());
                }
                if saved_to.is_some() {
                    response["saved_to"] = serde_json::json!(saved_to);
                }
                Ok(response)
            }
            None => Ok(serde_json::json!({
                "status": "completed",
                "html": null,
                "message": "No element matched the selector"
            })),
        },
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "JS_EXECUTION_ERROR",
            "message": e
        })),
    }
}

/// Text without trailing spaces, and with one blank line where a page has many.
fn tidy_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_blank = true;
    for line in text.lines().map(str::trim_end) {
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        } else if !after_blank {
            out.push('\n');
        }
        after_blank = line.is_empty();
    }
    out.truncate(out.trim_end().len());
    out
}

/// The longest start of `text` that fits in `max` bytes.
fn cut_text(text: &str, max: usize) -> &str {
    let end = (0..=max.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    &text[..end]
}

/// get_text — the visible text of the page or one element.
async fn tool_get_text(args: &Value) -> Result<Value> {
    let selector = args["selector"].as_str();
    let max_length = args["max_length"].as_u64().unwrap_or(0) as usize;

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    let element = match selector {
        Some(selector) => format!("document.querySelector({})", Value::from(selector)),
        None => "document.body".to_string(),
    };
    let script = format!("var el = {element}; return el ? el.innerText : null;");
    match webview_execute_js(&script, Some(10000)).await {
        Ok(Value::Null) => Ok(serde_json::json!({
            "status": "failed",
            "error": "SELECTOR_NOT_FOUND",
            "message": format!("No element matched the selector: {}", selector.unwrap_or("body"))
        })),
        Ok(value) => {
            let text = tidy_text(value.as_str().unwrap_or_default());
            let truncated = max_length > 0 && text.len() > max_length;
            let mut response = serde_json::json!({
                "status": "completed",
                "text": if truncated { cut_text(&text, max_length) } else { &text },
                "length": text.len()
            });
            if truncated {
                response["truncated"] = serde_json::json!(true);
            }
            Ok(response)
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "JS_EXECUTION_ERROR",
            "message": e
        })),
    }
}

/// execute_js — run JavaScript.
/// `max_result_length`: cap the length of the stringified result (0 = use safety cap).
/// A hard safety cap (`JS_RESULT_SAFETY_CAP`) always applies on top, to prevent
/// a malicious or buggy script from returning multi-megabyte payloads that
/// blow up daemon memory or the LLM context.
pub(crate) const JS_RESULT_SAFETY_CAP: usize = 5_000_000;

async fn tool_execute_js(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let script = args["script"]
        .as_str()
        .ok_or_else(|| anyhow!("script required"))?;
    let timeout_ms = args["timeout_ms"].as_u64().map(|v| v as u32);
    let await_promise = args["await_promise"].as_bool().unwrap_or(false);
    let max_result_length = args["max_result_length"].as_u64().unwrap_or(0) as usize;
    let effective_cap = if max_result_length == 0 {
        JS_RESULT_SAFETY_CAP
    } else {
        max_result_length.min(JS_RESULT_SAFETY_CAP)
    };

    tracing::info!(
        "execute_js: script length={}, timeout_ms={:?}, max_result_length={}, effective_cap={}",
        script.len(),
        timeout_ms,
        max_result_length,
        effective_cap
    );

    // A service worker is its own target with its own isolate: nothing the
    // page can reach (frames, worlds) applies, so it is resolved separately.
    let target_arg = args
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("page");
    if target_arg != "page" {
        return exec_in_service_worker(
            target_arg,
            script,
            timeout_ms,
            await_promise,
            effective_cap,
        )
        .await;
    }

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    // Where to run. Default = main world, top frame (what ExecuteScript does).
    // A global set inside an iframe is invisible from the top frame and reads
    // exactly like instrumentation that never ran, so the target is always
    // reported back with the result.
    let frame_arg = args.get("frame").and_then(|v| v.as_str());
    let world_arg = args.get("world").and_then(|v| v.as_str()).unwrap_or("main");
    if world_arg != "main" && world_arg != "isolated" {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_WORLD",
            "message": format!("unknown world '{world_arg}' — use 'main' or 'isolated'"),
        }));
    }

    let (execution, target) = match resolve_execution(frame_arg, world_arg).await {
        Ok(Some(target)) => {
            #[cfg(target_os = "windows")]
            let out = match target.session_id.as_deref() {
                // A cross-site frame: its own target, its own context numbering.
                Some(session) => {
                    crate::platform::execute_js_in_session_context(
                        session,
                        target.context_id,
                        script,
                        timeout_ms,
                        await_promise,
                    )
                    .await
                }
                None => {
                    let ctx = target.context_id.expect("targeted execution has a context");
                    crate::platform::execute_js_in_context(ctx, script, timeout_ms, await_promise)
                        .await
                }
            };
            // `resolve_execution` only returns a target where CDP exists, so
            // this arm is unreachable off Windows; keep it honest anyway.
            #[cfg(not(target_os = "windows"))]
            let out = Err::<Value, String>("UNSUPPORTED_PLATFORM".to_string());
            (out, target)
        }
        // No targeting requested — the fast default path.
        Ok(None) => {
            // Resolved BEFORE the script runs: this is where it is about to
            // run, and a script that navigates must not make us report the
            // context of the document that replaced it.
            let target = crate::platform::describe_default_target().await;
            let out = if await_promise {
                crate::platform::webview_execute_js_async(script, timeout_ms).await
            } else {
                webview_execute_js(script, timeout_ms).await
            };
            (out, target)
        }
        Err(problem) => return Ok(problem),
    };

    match execution {
        Ok(result) => {
            let result_str = match &result {
                Value::String(s) => s.clone(),
                _ => serde_json::to_string(&result).unwrap_or_default(),
            };
            if result_str.len() > effective_cap {
                let total = result_str.len();
                // Slice at a char boundary to avoid splitting a multi-byte UTF-8 codepoint.
                let mut cut = effective_cap.min(result_str.len());
                while cut > 0 && !result_str.is_char_boundary(cut) {
                    cut -= 1;
                }
                let mut out = serde_json::json!({
                    "status": "completed",
                    "result": &result_str[..cut],
                    "truncated": true,
                    "total_length": total,
                    "safety_cap": JS_RESULT_SAFETY_CAP,
                });
                // Truncated results still report where they ran — same
                // contract as every other execute_js result.
                merge_target(&mut out, &target);
                return Ok(out);
            }
            let mut out = serde_json::json!({
                "status": "completed",
                "result": result
            });
            merge_target(&mut out, &target);
            Ok(out)
        }
        Err(e) => {
            // A targeted run failing on the CDP layer usually means the
            // context id went stale between resolution and evaluate (the
            // frame's document was replaced). Name it, and hand back what IS
            // known so the caller can re-target instead of staring at an
            // HRESULT.
            let targeted = frame_arg.is_some() || world_arg != "main";
            let context_gone =
                e.contains("Cannot find context") || (targeted && e.contains("CDP error"));

            // The frame was found and the call reached it; its renderer just
            // did not answer — a page computing a challenge looks exactly like
            // this. Not to be confused with FRAME_NOT_FOUND, which is decided
            // before anything is sent.
            let busy = e.contains("timed out") || e.starts_with("TIMEOUT");

            let error_code = if e.contains("UNSUPPORTED_PLATFORM") {
                "UNSUPPORTED_PLATFORM"
            } else if e.contains("WEBVIEW_NOT_FOUND") {
                "WEBVIEW_NOT_FOUND"
            } else if busy {
                "RENDERER_BUSY"
            } else if targeted && context_gone {
                "CONTEXT_NOT_FOUND"
            } else {
                "JS_EXECUTION_ERROR"
            };

            let mut out = serde_json::json!({
                "status": "failed",
                "error": error_code,
                "message": e
            });
            if busy {
                out["hint"] = serde_json::json!(
                    "The target exists — the evaluate was delivered and its renderer did not \
                     answer in time (a long-running script: a challenge computing, a busy loop). \
                     Retry later or with a larger --timeout-ms; `console-logs` and `nav-log` are \
                     read from daemon memory and keep answering meanwhile."
                );
            }
            #[cfg(target_os = "windows")]
            if error_code == "CONTEXT_NOT_FOUND" {
                if let Some(obj) = out.as_object_mut() {
                    let top_id = match crate::get_main_window().await {
                        Some(w) => crate::platform::top_frame_id(&w).await.ok(),
                        None => None,
                    };
                    obj.insert(
                        "known_frames".into(),
                        crate::platform::frame_candidates_json(
                            &crate::platform::list_frame_contexts().await,
                            top_id.as_deref(),
                        ),
                    );
                    obj.insert(
                        "hint".into(),
                        serde_json::json!(
                            "the frame's document was likely replaced since the context was \
                             recorded — re-run get-frames and target again"
                        ),
                    );
                }
            }
            merge_target(&mut out, &target);
            Ok(out)
        }
    }
}

/// Pick a service worker from `(session_id, script_url)` pairs: `sw` = the most
/// recently attached, `sw:<substring>` = the newest whose script URL contains it.
pub(crate) fn pick_service_worker(
    target: &str,
    sessions: &[(String, String)],
) -> Result<(String, String), String> {
    let filter = target
        .strip_prefix("sw:")
        .or_else(|| target.strip_prefix("service_worker:"))
        .filter(|s| !s.is_empty());
    if filter.is_none() && !matches!(target, "sw" | "service_worker") {
        return Err(format!(
            "unknown target '{target}' — use `page` (default), `sw`, or `sw:<script-url substring>`"
        ));
    }
    sessions
        .iter()
        .rev()
        .find(|(_, url)| filter.is_none_or(|f| url.contains(f)))
        .cloned()
        .ok_or_else(|| match filter {
            Some(f) => format!("no attached service worker's script URL contains '{f}'"),
            None => "no service worker is attached".to_string(),
        })
}

async fn eval_in_worker(
    session_id: &str,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
) -> Result<Value, String> {
    if crate::engine::is_chrome() {
        return crate::chrome::eval_in_session(session_id, script, timeout_ms, await_promise).await;
    }
    #[cfg(target_os = "windows")]
    {
        crate::platform::execute_js_in_session(session_id, script, timeout_ms, await_promise).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (session_id, script, timeout_ms, await_promise);
        Err("UNSUPPORTED_PLATFORM".to_string())
    }
}

/// `exec-js --target sw` — evaluate inside the page's service worker.
async fn exec_in_service_worker(
    target: &str,
    script: &str,
    timeout_ms: Option<u32>,
    await_promise: bool,
    effective_cap: usize,
) -> Result<Value> {
    let sessions: Vec<(String, String)> = if crate::engine::is_chrome() {
        crate::chrome::service_worker_targets().await
    } else {
        crate::sw_console::sessions()
    };
    let (session_id, script_url) = match pick_service_worker(target, &sessions) {
        Ok(s) => s,
        Err(message) => {
            let versions = crate::sw_console::worker_versions_seen();
            return Ok(serde_json::json!({
                "status": "failed",
                "error": if message.starts_with("unknown target") { "INVALID_TARGET" } else { "NO_SERVICE_WORKER" },
                "message": message,
                "service_workers": sessions
                    .iter()
                    .map(|(id, url)| serde_json::json!({ "session_id": id, "url": url }))
                    .collect::<Vec<_>>(),
                "hint": if crate::engine::has_cdp() {
                    if versions > 0 && sessions.is_empty() {
                        "The ServiceWorker domain has seen a worker, but no session is attached to \
                         it (attach can be refused for workers owned by the browser target). \
                         Reload the page so the worker restarts under auto-attach."
                    } else {
                        "Navigate to a page that registers a service worker first. Workers attach \
                         when they start; `sw-console` lists what is attached."
                    }
                } else {
                    "Service worker targets need CDP: WebView2 on Windows, or `--engine chrome`."
                },
            }));
        }
    };

    // A worker that has only just started can be evaluated in before its own
    // script has run — it is held at start so its first console lines are
    // captured. Measured: `self.serviceWorker.state === "parsed"` and every
    // global the script sets still undefined, for a call made right after the
    // page that registered it loaded. Give it a moment to get past that.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let probe = "return self.serviceWorker ? self.serviceWorker.state : ''";
    while let Ok(Value::String(state)) = eval_in_worker(&session_id, probe, Some(1000), false).await
    {
        if state != "parsed" || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let result = eval_in_worker(&session_id, script, timeout_ms, await_promise).await;

    let mut out = match result {
        Ok(v) => {
            let s = match &v {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            if s.len() > effective_cap {
                let mut cut = effective_cap;
                while cut > 0 && !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                serde_json::json!({
                    "status": "completed",
                    "result": &s[..cut],
                    "truncated": true,
                    "total_length": s.len(),
                })
            } else {
                serde_json::json!({ "status": "completed", "result": v })
            }
        }
        Err(e) => serde_json::json!({
            "status": "failed",
            "error": if e.contains("JS_EXECUTION_ERROR") { "JS_EXECUTION_ERROR" } else { "SESSION_ERROR" },
            "message": e,
            "hint": "A worker that was stopped (idle workers are terminated after ~30 s) detaches \
                     its session; trigger it again (reload, fetch) and retry.",
        }),
    };
    if let Some(obj) = out.as_object_mut() {
        obj.insert("target".into(), serde_json::json!("service_worker"));
        obj.insert("session_id".into(), serde_json::json!(session_id));
        obj.insert("script_url".into(), serde_json::json!(script_url));
    }
    Ok(out)
}

/// Stamp every `execute_js` result with where it actually ran.
fn merge_target(out: &mut Value, target: &crate::platform::ExecutionTarget) {
    if let (Some(obj), Some(extra)) = (out.as_object_mut(), target.to_json().as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
}

/// Work out which execution context to use, if any.
///
/// `Ok(None)` means "no targeting requested" — use the default ExecuteScript
/// path. `Err(v)` is a ready-to-return failure payload that lists the frames we
/// *do* know about, so a missed match is actionable rather than a dead end.
#[cfg(target_os = "windows")]
async fn resolve_execution(
    frame: Option<&str>,
    world: &str,
) -> Result<Option<crate::platform::ExecutionTarget>, Value> {
    use crate::platform::{
        frame_candidates_json, isolated_context_for, list_frame_contexts, main_context_for,
        resolve_frame_id, top_frame_id, ExecutionTarget, FrameResolveError,
    };

    if frame.is_none() && world == "main" {
        return Ok(None);
    }

    let window = match crate::get_main_window().await {
        Some(w) => w,
        None => {
            return Err(serde_json::json!({
                "status": "failed",
                "error": "WEBVIEW_NOT_FOUND",
                "message": "WebView not ready",
            }))
        }
    };
    // The top frame's id is the only reliable way to tell the main document
    // apart from an iframe — a context's `isDefault` is per-frame.
    let top_id = top_frame_id(&window).await.ok();

    // Frame identity first (a CDP frame id), execution context second.
    // Numeric indexes address the DOM iframe list — the SAME order get-frames
    // reports — so `--frame N` and `get-frames` index N agree by construction.
    let resolution = match resolve_frame_id(&window, frame.unwrap_or("top"), top_id.as_deref())
        .await
    {
        Ok(r) => r,
        Err(err) => {
            // A cross-site frame has no context in the page's store, so a name
            // or origin never matched it there (measured on 0.18.0: `--frame
            // spin` → FRAME_NOT_FOUND for a frame `get-frames` listed as
            // `spin`). Its own target knows its URL; the parent knows its name.
            if matches!(err, FrameResolveError::NotFound { .. }) {
                if let Some((fid, _)) =
                    crate::platform::out_of_process_frame_matching(&window, frame.unwrap_or(""))
                        .await
                {
                    return Box::pin(resolve_execution(Some(&fid), world)).await;
                }
            }
            let known = frame_candidates_json(&list_frame_contexts().await, top_id.as_deref());
            return Err(match err {
                FrameResolveError::IndexOutOfRange {
                    idx,
                    dom_frame_count,
                } => serde_json::json!({
                    "status": "failed",
                    "error": "FRAME_INDEX_OUT_OF_RANGE",
                    "message": format!(
                        "frame index {idx} out of range — get-frames lists {dom_frame_count} iframe(s)"
                    ),
                    "dom_frame_count": dom_frame_count,
                    "known_frames": known,
                    "hint": "`--frame N` indexes the get-frames list (same order). The top \
                             document is `--frame top` (the default), not an index.",
                }),
                FrameResolveError::NoFrameId { idx } => serde_json::json!({
                    "status": "failed",
                    "error": "FRAME_ID_UNKNOWN",
                    "message": format!(
                        "iframe[{idx}] exists in the DOM but no CDP frame id could be derived for it"
                    ),
                    "known_frames": known,
                }),
                FrameResolveError::NoTopFrame => serde_json::json!({
                    "status": "failed",
                    "error": "FRAME_NOT_FOUND",
                    "message": "Page.getFrameTree reported no top frame id — navigate first",
                    "known_frames": known,
                }),
                FrameResolveError::DomEnumerationFailed { message } => serde_json::json!({
                    "status": "failed",
                    "error": "FRAME_ENUMERATION_FAILED",
                    "message": message,
                    "known_frames": known,
                }),
                FrameResolveError::NotFound { message } => serde_json::json!({
                    "status": "failed",
                    "error": "FRAME_NOT_FOUND",
                    "message": message,
                    "known_frames": known,
                    "hint": "Pass --frame with an index from get-frames (same order), a \
                             frame_id from get-frames/known_frames, a substring of the \
                             frame's origin or name, or `top`. Navigate first — frames only \
                             exist once a document loads.",
                }),
            });
        }
    };

    // A cross-site frame runs in its own renderer: it is its own target, and
    // its session — not the page's context store — is the way in.
    if let Some(session) = crate::sw_console::session_for_frame(&resolution.frame_id) {
        let origin = crate::platform::frame_target_url(&window, &resolution.frame_id)
            .await
            .and_then(|u| url::Url::parse(&u).ok())
            .map(|u| u.origin().ascii_serialization());
        let context_id = if world == "main" {
            // The session's default context is its main world.
            None
        } else {
            match crate::platform::isolated_context_in_session(
                &window,
                &session,
                &resolution.frame_id,
            )
            .await
            {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    return Err(serde_json::json!({
                        "status": "failed",
                        "error": "ISOLATED_WORLD_FAILED",
                        "message": e,
                        "frame_id": resolution.frame_id,
                        "out_of_process": true,
                    }))
                }
            }
        };
        return Ok(Some(ExecutionTarget {
            world: world.into(),
            context_id,
            frame_id: Some(resolution.frame_id),
            frame_origin: origin,
            frame_label: resolution.label,
            session_id: Some(session),
        }));
    }

    if world == "main" {
        // The main world of a frame is its default execution context. No
        // silent fallback to the parent: writing to the wrong frame and
        // reporting success is worse than an error.
        return match main_context_for(&resolution.frame_id).await {
            Some(ctx) => Ok(Some(ExecutionTarget {
                world: "main".into(),
                context_id: Some(ctx.context_id),
                frame_id: Some(resolution.frame_id),
                frame_origin: Some(ctx.origin),
                frame_label: resolution.label,
                session_id: None,
            })),
            None => Err(serde_json::json!({
                "status": "failed",
                "error": "NO_MAIN_WORLD_CONTEXT",
                "message": format!(
                    "no default (main-world) execution context is known for frame {} — \
                     srcless/about:blank frames may never announce one",
                    resolution.frame_id
                ),
                "frame_id": resolution.frame_id,
                "known_frames": frame_candidates_json(&list_frame_contexts().await, top_id.as_deref()),
                "hint": "try --world isolated (it only needs the frame id), or navigate/reload \
                         so the frame announces a context",
            })),
        };
    }

    // Isolated world: created directly from the frame id, no store dependency.
    match isolated_context_for(&window, &resolution.frame_id).await {
        Ok(ctx) => {
            let origin = main_context_for(&resolution.frame_id)
                .await
                .map(|c| c.origin);
            Ok(Some(ExecutionTarget {
                world: "isolated".into(),
                context_id: Some(ctx),
                frame_id: Some(resolution.frame_id),
                frame_origin: origin,
                frame_label: resolution.label,
                session_id: None,
            }))
        }
        Err(e) => Err(serde_json::json!({
            "status": "failed",
            "error": "ISOLATED_WORLD_FAILED",
            "message": e,
        })),
    }
}

#[cfg(not(target_os = "windows"))]
async fn resolve_execution(
    frame: Option<&str>,
    world: &str,
) -> Result<Option<crate::platform::ExecutionTarget>, Value> {
    if frame.is_none() && world == "main" {
        return Ok(None);
    }
    Err(serde_json::json!({
        "status": "failed",
        "error": "UNSUPPORTED_PLATFORM",
        "message": "--frame and --world need CDP (Windows/WebView2 only)",
    }))
}

/// screenshot — capture the page (WebView2 CapturePreview when available).
///
/// Windows: capture WebView contents directly via WebView2's CapturePreview API.
/// Other OSes: fall back to `xcap`.
/// - No same-process limitation (uses WebView internals).
/// - Captures cross-origin iframes too.
/// - When `selector` is provided, the element rect is resolved via JS and the image is cropped.
async fn tool_screenshot(args: &Value) -> Result<Value> {
    let site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let output_file = args["output_file"].as_str().unwrap_or("screenshot.png");
    let selector = args["selector"].as_str();

    tracing::info!(
        "screenshot: site={}, output={}, selector={:?}",
        site,
        output_file,
        selector
    );

    // Check the WebView
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    let file_path = site_dir(site).await?.join(output_file);

    // With a selector, get the element's bounding rect through JS
    //
    // Every engine captures only what is on screen, so an element outside the
    // viewport came back as a blank image (Chrome engine) or a zero-height crop
    // that failed to save (macOS WebView). Scroll it into view first — but only
    // when it is not already fully visible, so a visible element's page is
    // left exactly as it was.
    let crop_rect = if let Some(sel) = selector {
        let script = format!(
            r#"
            var el = document.querySelector('{}');
            if (!el) return null;
            var rect = el.getBoundingClientRect();
            if (rect.top < 0 || rect.left < 0 || rect.bottom > window.innerHeight || rect.right > window.innerWidth) {{
                el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }});
                rect = el.getBoundingClientRect();
            }}
            return JSON.stringify({{
                x: Math.round(rect.x),
                y: Math.round(rect.y),
                width: Math.round(rect.width),
                height: Math.round(rect.height),
                vw: window.innerWidth,
                vh: window.innerHeight
            }});
            "#,
            sel.replace('\'', "\\'").replace('"', "\\\"")
        );
        match webview_execute_js(&script, Some(5000)).await {
            Ok(val) => {
                if val.is_null() {
                    return Ok(serde_json::json!({
                        "status": "failed",
                        "error": "SELECTOR_NOT_FOUND",
                        "message": format!("No element matched the selector: {}", sel)
                    }));
                }
                let rect_str = val.as_str().unwrap_or("");
                serde_json::from_str::<serde_json::Value>(rect_str).ok()
            }
            Err(_) => None,
        }
    } else {
        None
    };

    // Wait for rendering to stabilize (double rAF)
    let _ = webview_execute_js(
        "return new Promise(function(r) { requestAnimationFrame(function() { requestAnimationFrame(function() { r('ok'); }); }); });",
        Some(2000),
    )
    .await;
    // rAF may not resolve in sync exec-js, add small delay as fallback
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let capture_result = capture_page(&file_path, &crop_rect).await;

    match capture_result {
        Ok(()) => {
            let metadata = tokio::fs::metadata(&file_path).await;
            let size = metadata.map(|m| m.len()).unwrap_or(0);
            let current_url = webview_get_url().await.unwrap_or_default();

            Ok(serde_json::json!({
                "status": "completed",
                "file": crate::state::printable_path(&file_path),
                "size": size,
                "current_url": current_url,
                "selector": selector
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "SCREENSHOT_FAILED",
            "message": e
        })),
    }
}

/// Capture the page to `file_path`, cropped to `crop_rect` (CSS pixels) if
/// given: Chrome's `Page.captureScreenshot` on the Chrome engine, WebView2's
/// CapturePreview on Windows, the window pixels (`xcap`) elsewhere.
async fn capture_page(
    file_path: &std::path::Path,
    crop_rect: &Option<Value>,
) -> Result<(), String> {
    if crate::engine::is_chrome() {
        // The clip is applied by Chrome itself, in CSS pixels — no device
        // pixel ratio arithmetic to get wrong.
        let clip = crop_rect.as_ref().and_then(|r| {
            let (w, h) = (r["width"].as_f64()?, r["height"].as_f64()?);
            (w > 0.0 && h > 0.0).then(|| {
                (
                    r["x"].as_f64().unwrap_or(0.0),
                    r["y"].as_f64().unwrap_or(0.0),
                    w,
                    h,
                )
            })
        });
        let png = crate::chrome::capture_png(clip).await?;
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {:?}", e))?;
        }
        return std::fs::write(file_path, &png).map_err(|e| format!("Failed to save image: {e}"));
    }
    #[cfg(target_os = "windows")]
    {
        capture_webview_screenshot(file_path, crop_rect).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        capture_with_xcap(file_path, crop_rect).await
    }
}

/// Windows: take a screenshot via the WebView2 CapturePreview API.
#[cfg(target_os = "windows")]
async fn capture_webview_screenshot(
    file_path: &std::path::Path,
    crop_rect: &Option<Value>,
) -> Result<(), String> {
    use crate::APP_HANDLE;
    use std::sync::mpsc;
    use tauri::Manager;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2CapturePreviewCompletedHandler,
        ICoreWebView2CapturePreviewCompletedHandler_Impl,
        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
    };
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Com::IStream;
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows_core::{implement, HRESULT};

    let file_path = file_path.to_path_buf();
    let crop_rect = crop_rect.clone();

    // Get the WebviewWindow from the AppHandle
    let app_handle = APP_HANDLE.read().await;
    let app = app_handle.as_ref().ok_or("AppHandle not available")?;
    let window = app
        .get_webview_window("main")
        .ok_or("Main window not found")?;

    // Receive the result through a channel
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>();

    window
        .with_webview(move |webview: tauri::webview::PlatformWebview| {
            unsafe {
                // Create an in-memory IStream
                let stream: IStream = match CreateStreamOnHGlobal(HGLOBAL::default(), true) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(Err(format!("CreateStreamOnHGlobal failed: {:?}", e)));
                        return;
                    }
                };

                // CapturePreview completion handler
                #[implement(ICoreWebView2CapturePreviewCompletedHandler)]
                struct CaptureHandler {
                    tx: mpsc::Sender<Result<Vec<u8>, String>>,
                    stream: IStream,
                }

                impl ICoreWebView2CapturePreviewCompletedHandler_Impl for CaptureHandler_Impl {
                    fn Invoke(&self, errorcode: HRESULT) -> windows_core::Result<()> {
                        if errorcode.is_ok() {
                            // Read the data from the IStream
                            match read_stream_data(&self.stream) {
                                Ok(data) => {
                                    let _ = self.tx.send(Ok(data));
                                }
                                Err(e) => {
                                    let _ = self.tx.send(Err(e));
                                }
                            }
                        } else {
                            let _ = self
                                .tx
                                .send(Err(format!("CapturePreview failed: {:?}", errorcode)));
                        }
                        Ok(())
                    }
                }

                let handler: ICoreWebView2CapturePreviewCompletedHandler = CaptureHandler {
                    tx,
                    stream: stream.clone(),
                }
                .into();

                // Get ICoreWebView2 from the WebView2 controller
                let controller = webview.controller();
                let core: ICoreWebView2 = match controller.CoreWebView2() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("[screenshot] CoreWebView2 failed: {:?}", e);
                        return;
                    }
                };

                // Call CapturePreview
                if let Err(e) = core.CapturePreview(
                    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                    &stream,
                    &handler,
                ) {
                    tracing::error!("[screenshot] CapturePreview failed: {:?}", e);
                }
            }
        })
        .map_err(|e| format!("with_webview failed: {:?}", e))?;

    // Wait for the result (5 s timeout)
    let png_data = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "CapturePreview timeout".to_string())??;

    tracing::info!(
        "[screenshot] CapturePreview succeeded, {} bytes",
        png_data.len()
    );

    // Process the image (apply the crop)
    let img =
        image::load_from_memory(&png_data).map_err(|e| format!("Failed to load image: {:?}", e))?;

    // CapturePreview returns device pixels; the rect is CSS pixels. Cropping
    // with the rect as-is cut the wrong region at any display scale above
    // 100% (at 125%, a 1280x640 element came back as its top-left 1024x512).
    let final_img = match crop_rect
        .as_ref()
        .and_then(|r| window_crop(r, img.width(), img.height()))
    {
        Some((x, y, w, h)) => img.crop_imm(x, y, w, h),
        None => img,
    };

    // Save
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory: {:?}", e))?;
    }
    final_img
        .save(&file_path)
        .map_err(|e| format!("Failed to save image: {:?}", e))?;

    tracing::info!("[screenshot] Saved to {:?}", file_path);
    Ok(())
}

/// Read all bytes from an `IStream` (Windows only).
#[cfg(target_os = "windows")]
fn read_stream_data(stream: &windows::Win32::System::Com::IStream) -> Result<Vec<u8>, String> {
    use windows::Win32::System::Com::STREAM_SEEK_SET;

    unsafe {
        // Seek to the start of the stream
        stream
            .Seek(0, STREAM_SEEK_SET, None)
            .map_err(|e| format!("Seek failed: {:?}", e))?;

        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let mut read = 0u32;
            // Read returns an HRESULT, so check ok()
            let hr = stream.Read(
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                Some(&mut read),
            );
            if hr.is_err() {
                return Err(format!("Read failed: {:?}", hr));
            }
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read as usize]);
        }
        Ok(data)
    }
}

/// Map a viewport rect in CSS pixels (`x`/`y`/`width`/`height`, plus the
/// viewport's `vw`/`vh`) onto a captured window image, as `(x, y, w, h)` in
/// image pixels. `None` when the rect is empty.
///
/// The window image is in physical pixels and includes the title bar, so the
/// rect cannot be used as-is — that cut the wrong region, half-size and too
/// high on a Retina screen. Measured (macOS, 2x): a 1200x769 viewport comes
/// back as a 2400x1602 image, the extra 64 px being the title bar. So the scale
/// is the image width over the viewport width, and the title bar is whatever
/// sits above the viewport's height. (`outerHeight` cannot stand in for this:
/// it is spoofed to equal `innerHeight`.)
/// Windows' CapturePreview image is the viewport alone, so there the title
/// bar term is zero and only the scale applies.
fn window_crop(rect: &Value, img_w: u32, img_h: u32) -> Option<(u32, u32, u32, u32)> {
    let css = |k: &str| rect[k].as_f64().unwrap_or(0.0);
    let (w, h, vw, vh) = (css("width"), css("height"), css("vw"), css("vh"));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let (scale, top) = if vw > 0.0 && vh > 0.0 {
        let s = img_w as f64 / vw;
        (s, (img_h as f64 - vh * s).max(0.0))
    } else {
        (1.0, 0.0)
    };
    let px = |v: f64| v.max(0.0).round() as u32;
    Some((
        px(css("x") * scale),
        px(top + css("y") * scale),
        px(w * scale),
        px(h * scale),
    ))
}

/// Non-Windows: xcap fallback.
#[cfg(not(target_os = "windows"))]
async fn capture_with_xcap(
    file_path: &std::path::Path,
    crop_rect: &Option<Value>,
) -> Result<(), String> {
    use crate::get_window_title_sync;
    use xcap::Window;

    let file_path = file_path.to_path_buf();
    let crop_rect = crop_rect.clone();

    tokio::task::spawn_blocking(move || -> Result<(), String> {
        // Make sure that the parent directory of the save path exists
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {:?}: {}", parent, e))?;
        }

        // Find the window by its per-instance title
        //
        // Only ever a window this process owns. The old fallback took the first
        // window whose title merely contained "dalzzack" — measured, an editor
        // open on this repository sat above the page in z-order and was captured
        // and reported as the page.
        let expected_title = get_window_title_sync();
        let own_pid = std::process::id();
        let mut ours: Vec<Window> = Window::all()
            .map_err(|e| format!("Failed to list windows: {}", e))?
            .into_iter()
            .filter(|w| w.pid().ok() == Some(own_pid))
            .collect();
        if ours.is_empty() {
            // xcap lists on-screen windows only. Naming every other app's window
            // here (as this used to) told the caller nothing and leaked titles.
            return Err(format!(
                "Window '{expected_title}' is not on screen (minimized, hidden, or on another \
                 Space): window capture only sees on-screen windows"
            ));
        }
        let idx = ours
            .iter()
            .position(|w| w.title().unwrap_or_default() == expected_title)
            .unwrap_or(0);
        let window = ours.swap_remove(idx);

        // Capture the window
        let image = window
            .capture_image()
            .map_err(|e| format!("Window capture failed: {}", e))?;

        // Apply the crop
        let crop = crop_rect
            .as_ref()
            .and_then(|r| window_crop(r, image.width(), image.height()));
        let final_image: image::DynamicImage = match crop {
            Some((x, y, w, h)) => image::DynamicImage::from(image).crop_imm(x, y, w, h),
            None => image::DynamicImage::from(image),
        };

        final_image
            .save(&file_path)
            .map_err(|e| format!("Failed to save: {:?}", e))?;

        Ok(())
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))?
}

/// How long a document that already satisfies `--to` must keep its sentinel
/// before `wait-navigation` concludes nothing is being replaced.
const ALREADY_AT_TARGET_GRACE_MS: u64 = 1500;

/// wait_navigation — wait until the document is replaced.
///
/// A sentinel planted in the current document vanishes when the navigation
/// commits, so this also detects same-URL navigations (a form POSTing back to
/// itself) that a URL comparison misses. Transient eval failures while the
/// document swap is in flight are "keep polling", not errors.
async fn tool_wait_navigation(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let interval_ms = args["interval_ms"].as_u64().unwrap_or(250).max(50);
    let to = args["to"]
        .as_str()
        .map(unquote_arg)
        .filter(|s| !s.is_empty())
        .map(String::from);

    tracing::info!("wait_navigation: timeout_ms={timeout_ms}, to={to:?}");

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    // Plant a sentinel so a same-URL navigation is still detectable. Failing to
    // plant it is not an error: the documented order is click-then-wait, and by
    // then the navigation is usually already in flight and page JS no longer
    // answers. That IS the event being waited for, so fall through to
    // commit detection with whatever the browser process can still tell us.
    let plant = webview_execute_js(
        "window.__tw_nav_sentinel = true; return JSON.stringify({u: location.href, r: document.readyState})",
        Some(2000),
    )
    .await;
    let (before_url, before_ready, sentinel_planted) = match plant {
        Ok(v) => {
            let obj = v
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            (
                obj["u"].as_str().unwrap_or_default().to_string(),
                obj["r"].as_str().unwrap_or_default().to_string(),
                true,
            )
        }
        Err(_) => (
            browser_side_url().await.unwrap_or_default(),
            String::new(),
            false,
        ),
    };

    // `click` returns at 0 s, and a fast navigation can have fully landed
    // before this call even planted its sentinel. Then there is no swap left
    // to witness and only the last-chance check rescues the answer — 45 s
    // late (measured). Returning immediately whenever `--to` already matches
    // would be wrong, though: it breaks waiting for a same-URL re-navigation.
    // The distinguishing signal is the sentinel itself: plant it, and if the
    // document that already satisfies `--to` keeps it through a short grace
    // window, nothing is being replaced — we are already there.
    if sentinel_planted && before_ready == "complete" {
        if let Some(t) = to.as_deref().filter(|t| before_url.contains(t)) {
            let grace_ms = timeout_ms.min(ALREADY_AT_TARGET_GRACE_MS);
            let grace_start = std::time::Instant::now();
            let mut still_here = true;
            while (grace_start.elapsed().as_millis() as u64) < grace_ms {
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms.min(250))).await;
                match webview_execute_js("return !!window.__tw_nav_sentinel", Some(2000)).await {
                    Ok(v) if v.as_bool() == Some(true) => {}
                    // Gone or unanswerable: a swap is in flight after all —
                    // fall through to the normal wait.
                    _ => {
                        still_here = false;
                        break;
                    }
                }
            }
            if still_here {
                let mut out = navigation_result(
                    &before_url,
                    &before_url,
                    &before_ready,
                    grace_start,
                    false,
                    false,
                );
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("already_at_target".into(), serde_json::json!(true));
                    obj.insert("to".into(), serde_json::json!(t));
                    obj.insert(
                        "note".into(),
                        serde_json::json!(format!(
                            "the document already satisfied --to and was complete when this call \
                             started, and its sentinel survived a {grace_ms} ms grace window — no \
                             replacement is in flight. To wait for a same-URL re-navigation, start \
                             wait-navigation before triggering it."
                        )),
                    );
                }
                return Ok(out);
            }
        }
    }

    // With no sentinel, no baseline URL and no destination there is nothing to
    // compare against — say so instead of polling blindly for 30 s.
    if !sentinel_planted && before_url.is_empty() && to.is_none() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "SENTINEL_PLANT_FAILED",
            "message": "the page could not evaluate JS and no baseline URL was available, so a document swap cannot be detected",
            "hint": "pass --to <substring of the destination URL>, which is checked out-of-band and needs no page JS",
        }));
    }

    let start = std::time::Instant::now();
    let mut last_url = before_url.clone();
    let mut ready_state = String::new();
    let mut polls = 0u32;
    let mut probe_failures = 0u32;
    // Latched: once the swap is witnessed it stays witnessed. Previously the
    // `--to` gate was only ever evaluated at the instant a poll happened to see
    // the commit, so a destination that appeared one poll later was never
    // matched again — a timeout whose own `current_url` satisfied `--to`.
    let mut committed = false;

    while (start.elapsed().as_millis() as u64) < timeout_ms {
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        polls += 1;

        let mut url = String::new();
        let mut sentinel: Option<bool> = None;
        match webview_execute_js(
            "return JSON.stringify({u: location.href, s: !!window.__tw_nav_sentinel, r: document.readyState})",
            Some(2000),
        )
        .await
        {
            Ok(v) => match v
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
            {
                Some(obj) => {
                    url = obj["u"].as_str().unwrap_or_default().to_string();
                    sentinel = obj["s"].as_bool();
                    ready_state = obj["r"].as_str().unwrap_or_default().to_string();
                }
                None => probe_failures += 1,
            },
            // Mid-swap the old document stops answering — that is the
            // navigation happening, not a problem.
            Err(_) => probe_failures += 1,
        }

        // The renderer can be unable to answer JS for seconds during a proxied
        // navigation. The browser process still knows the URL, so the `--to`
        // gate keeps working while page JS is unavailable.
        if url.is_empty() {
            if let Some(u) = browser_side_url().await {
                url = u;
            }
        }
        if !url.is_empty() {
            last_url = url.clone();
        }

        if let Some(failed) = failed_wait(&before_url, &last_url, start).await {
            return Ok(failed);
        }

        if !committed {
            let sentinel_gone = sentinel == Some(false);
            let url_changed =
                !last_url.is_empty() && !before_url.is_empty() && last_url != before_url;
            committed = sentinel_gone || url_changed;
        }

        if committed {
            match &to {
                None => {
                    crate::main_frame::consume();
                    return Ok(navigation_result(
                        &before_url,
                        &last_url,
                        &ready_state,
                        start,
                        true,
                        false,
                    ));
                }
                // Re-checked every poll, not only on the poll that saw the swap.
                Some(t) if last_url.contains(t.as_str()) => {
                    crate::main_frame::consume();
                    return Ok(navigation_result(
                        &before_url,
                        &last_url,
                        &ready_state,
                        start,
                        true,
                        false,
                    ));
                }
                // Redirect chain still in flight — keep waiting.
                Some(_) => {}
            }
        }
    }

    // Last chance before declaring a timeout: re-read the URL out-of-band and
    // check `--to` once more. Reporting NAVIGATION_TIMEOUT while printing a
    // `current_url` that satisfies `--to` is the one answer this tool must
    // never give.
    if let Some(u) = browser_side_url().await {
        if !u.is_empty() {
            last_url = u;
        }
    }
    if let Some(failed) = failed_wait(&before_url, &last_url, start).await {
        return Ok(failed);
    }
    crate::main_frame::consume();
    if let Some(t) = &to {
        if last_url.contains(t.as_str()) {
            return Ok(navigation_result(
                &before_url,
                &last_url,
                &ready_state,
                start,
                committed,
                true,
            ));
        }
    }

    Ok(serde_json::json!({
        "status": "failed",
        "error": "NAVIGATION_TIMEOUT",
        "message": format!("document was not replaced within {timeout_ms} ms"),
        "navigated": false,
        "before_url": before_url,
        "current_url": last_url,
        // Echoed so a mis-quoted destination (`--to '"/a/b/"'` — quotes a
        // runner forgot to strip) is visible instead of looking like a bug in
        // the matcher.
        "to": to,
        "document_replaced": committed,
        "sentinel_planted": sentinel_planted,
        "polls": polls,
        "probe_failures": probe_failures,
        "elapsed_ms": start.elapsed().as_millis() as u64,
        "hint": match (&to, committed) {
            (Some(_), true) => "the document WAS replaced but no URL along the way contained --to; compare `to` against `current_url`",
            (Some(_), false) => "nothing replaced the document and --to never matched — did the click/submit fire? Proxied navigations can need a larger --timeout-ms",
            (None, _) => "nothing replaced the document — did the click/submit actually fire? Proxied navigations can need a larger --timeout-ms",
        },
    }))
}

/// Build the success payload for [`tool_wait_navigation`].
///
/// `navigated` says whether the document swap was actually witnessed;
/// `matched_late` marks a destination that was only confirmed by the final
/// out-of-band check, so a caller can tell a clean detection from a rescue.
/// A main-frame load that failed since the last action, as a failed
/// `wait-navigation` result, unless an earlier call reported it. Such a load
/// never replaces the document (WebKit), or replaces it with the browser's
/// error page (WebView2, Chrome).
async fn failed_wait(
    before_url: &str,
    current_url: &str,
    start: std::time::Instant,
) -> Option<Value> {
    let failure = crate::main_frame::unreported()?;
    let failure = crate::main_frame::settled(failure).await;
    crate::main_frame::consume();
    let url = failure.failing_url.clone().unwrap_or_default();
    let mut out = failed_navigation(&url, &failure.to_json(), &Value::Null);
    out["navigated"] = serde_json::json!(false);
    out["before_url"] = serde_json::json!(before_url);
    out["current_url"] = serde_json::json!(current_url);
    out["elapsed_ms"] = serde_json::json!(start.elapsed().as_millis() as u64);
    Some(out)
}

fn navigation_result(
    before_url: &str,
    after_url: &str,
    ready_state: &str,
    start: std::time::Instant,
    navigated: bool,
    matched_late: bool,
) -> Value {
    let mut out = serde_json::json!({
        "status": "completed",
        "navigated": navigated,
        "before_url": before_url,
        "after_url": after_url,
        "ready_state": ready_state,
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    // The top-frame document chain that produced `after_url`: its status and
    // every hop, so "where did we land" comes with "and what did the server
    // say". Only a chain that ended at that URL qualifies — a document that
    // arrived without a request (back/forward cache, about:blank) must not be
    // reported with the previous page's 403.
    if crate::netlog::is_armed() && !after_url.is_empty() {
        let normalize = |u: &str| {
            url::Url::parse(u)
                .map(|p| p.to_string())
                .unwrap_or_else(|_| u.to_string())
        };
        let want = normalize(after_url);
        let doc = crate::netlog::navigations(&crate::netlog::NavQuery {
            max: 5,
            redact: true,
            ..Default::default()
        })
        .into_iter()
        .rev()
        .find(|n| {
            [&n["committed_url"], &n["final_url"]]
                .iter()
                .any(|u| u.as_str().is_some_and(|u| normalize(u) == want))
        });
        if let (Some(doc), Some(obj)) = (doc, out.as_object_mut()) {
            obj.insert("document".into(), doc);
        }
    }
    if matched_late {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("matched_late".into(), serde_json::json!(true));
            obj.insert(
                "note".into(),
                serde_json::json!(
                    "--to matched on the final check; the document swap itself was not witnessed \
                     (page JS was unavailable while it happened)"
                ),
            );
        }
    }
    out
}

/// The top frame's URL as the **browser process** sees it — answers while the
/// renderer is too busy for JS. `None` where there is no CDP.
#[cfg(target_os = "windows")]
async fn browser_side_url() -> Option<String> {
    if crate::engine::is_chrome() {
        return crate::chrome::page_url().await;
    }
    let window = crate::get_main_window().await?;
    crate::platform::top_frame_url(&window).await.ok()
}

#[cfg(not(target_os = "windows"))]
async fn browser_side_url() -> Option<String> {
    if crate::engine::is_chrome() {
        return crate::chrome::page_url().await;
    }
    None
}

/// Strip one layer of surrounding quotes from a CLI-supplied value.
///
/// A runner that forwards `--to "/zp/p/"` without a shell to strip the quotes
/// would otherwise match URLs against `"/zp/p/"` — quote characters included —
/// and never fire. No URL substring legitimately opens and closes with a quote.
fn unquote_arg(s: &str) -> &str {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        return &t[1..t.len() - 1];
    }
    t
}

/// wait — sleep.
async fn tool_wait(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let ms = args["ms"].as_u64().ok_or_else(|| anyhow!("ms required"))?;

    tracing::info!("wait: {}ms", ms);

    // Cap at 60 seconds
    let ms = ms.min(60_000);

    tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;

    Ok(serde_json::json!({
        "status": "completed",
        "waited_ms": ms
    }))
}

/// get_current_url — return the current page URL.
async fn tool_get_current_url(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;

    tracing::info!("get_current_url");

    // Check the WebView
    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    let url = webview_get_url().await.unwrap_or_default();
    let title = webview_get_title().await.unwrap_or_default();

    Ok(serde_json::json!({
        "status": "completed",
        "url": url,
        "title": title
    }))
}

/// wait_for_selector — wait for a CSS selector.
/// Supports state: "present" (default), "added" (count increases), "removed" (disappears).
/// Supports text: wait for an element containing specific text.
async fn tool_wait_for_selector(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let selector = args["selector"]
        .as_str()
        .ok_or_else(|| anyhow!("selector required"))?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10000) as u32;
    let state = args["state"].as_str().unwrap_or("present");
    let text_filter = args["text"].as_str();

    tracing::info!(
        "wait_for_selector: selector={}, state={}, text={:?}, timeout_ms={}",
        selector,
        state,
        text_filter,
        timeout_ms
    );

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    match state {
        "present" => {
            // Original behavior: wait for element to exist (with optional text filter)
            let Some(text) = text_filter else {
                // No text filter: just wait for the selector to exist.
                return match webview_wait_for_selector(selector, Some(timeout_ms)).await {
                    Ok(result) => {
                        if result.found {
                            Ok(serde_json::json!({
                                "status": "completed",
                                "found": true,
                                "elapsed_ms": result.elapsed_ms,
                                "selector": selector
                            }))
                        } else {
                            let nearby = collect_nearby_interactive(8).await;
                            Ok(serde_json::json!({
                                "status": "completed",
                                "found": false,
                                "elapsed_ms": result.elapsed_ms,
                                "selector": selector,
                                "message": format!("Selector '{}' not found within {}ms", selector, result.elapsed_ms),
                                "nearby": nearby,
                                "hint": "Element didn't appear in time. Check `nearby` for what IS visible, or call `describe` for the full element index."
                            }))
                        }
                    }
                    Err(e) => Ok(serde_json::json!({
                        "status": "failed",
                        "error": "WAIT_FOR_SELECTOR_ERROR",
                        "message": e
                    })),
                };
            };
            // Poll for element with matching text
            let sel_escaped = selector.replace('\'', "\\'");
            let text_escaped = text.replace('\'', "\\'");
            let start = std::time::Instant::now();
            loop {
                let script = format!(
                    r#"var els = document.querySelectorAll('{sel}');
                        for (var i = 0; i < els.length; i++) {{
                            if (els[i].textContent && els[i].textContent.indexOf('{text}') >= 0) return true;
                        }}
                        return false;"#,
                    sel = sel_escaped,
                    text = text_escaped
                );
                if let Ok(val) = webview_execute_js(&script, Some(5000)).await {
                    if val.as_bool() == Some(true) {
                        return Ok(serde_json::json!({
                            "status": "completed",
                            "found": true,
                            "elapsed_ms": start.elapsed().as_millis(),
                            "selector": selector,
                            "text": text
                        }));
                    }
                }
                if start.elapsed().as_millis() >= timeout_ms as u128 {
                    return Ok(serde_json::json!({
                        "status": "completed",
                        "found": false,
                        "elapsed_ms": start.elapsed().as_millis(),
                        "selector": selector,
                        "text": text,
                        "message": format!("Element with text '{}' not found within {}ms", text, timeout_ms)
                    }));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        "added" => {
            // Wait for element count to increase
            let sel_escaped = selector.replace('\'', "\\'");
            let count_script = format!(
                "return document.querySelectorAll('{}').length;",
                sel_escaped
            );
            let initial_count = webview_execute_js(&count_script, Some(5000))
                .await
                .ok()
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let start = std::time::Instant::now();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if let Ok(val) = webview_execute_js(&count_script, Some(5000)).await {
                    let current = val.as_u64().unwrap_or(0);
                    if current > initial_count {
                        return Ok(serde_json::json!({
                            "status": "completed",
                            "found": true,
                            "elapsed_ms": start.elapsed().as_millis(),
                            "selector": selector,
                            "initial_count": initial_count,
                            "current_count": current
                        }));
                    }
                }
                if start.elapsed().as_millis() >= timeout_ms as u128 {
                    return Ok(serde_json::json!({
                        "status": "completed",
                        "found": false,
                        "elapsed_ms": start.elapsed().as_millis(),
                        "selector": selector,
                        "initial_count": initial_count,
                        "message": format!("No new elements added within {}ms", timeout_ms)
                    }));
                }
            }
        }
        "removed" => {
            // Wait for element to disappear
            let sel_escaped = selector.replace('\'', "\\'");
            let start = std::time::Instant::now();
            loop {
                let script = format!(
                    "return document.querySelectorAll('{}').length;",
                    sel_escaped
                );
                if let Ok(val) = webview_execute_js(&script, Some(5000)).await {
                    if val.as_u64().unwrap_or(1) == 0 {
                        return Ok(serde_json::json!({
                            "status": "completed",
                            "found": true,
                            "removed": true,
                            "elapsed_ms": start.elapsed().as_millis(),
                            "selector": selector
                        }));
                    }
                }
                if start.elapsed().as_millis() >= timeout_ms as u128 {
                    return Ok(serde_json::json!({
                        "status": "completed",
                        "found": false,
                        "removed": false,
                        "elapsed_ms": start.elapsed().as_millis(),
                        "selector": selector,
                        "message": format!("Element still exists after {}ms", timeout_ms)
                    }));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        _ => Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_STATE",
            "message": format!("Invalid state '{}'. Use: present, added, removed", state)
        })),
    }
}

/// fill_form — fill form fields and optionally submit.
async fn tool_fill_form(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let fields_arr = args["fields"]
        .as_array()
        .ok_or_else(|| anyhow!("fields required (array)"))?;
    let submit_selector = args["submit_selector"].as_str();

    tracing::info!(
        "fill_form: fields={}, submit={:?}",
        fields_arr.len(),
        submit_selector
    );

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    // Parse the fields
    let fields: Vec<(String, String)> = fields_arr
        .iter()
        .filter_map(|f| {
            let selector = f["selector"].as_str()?.to_string();
            let value = f["value"].as_str()?.to_string();
            Some((selector, value))
        })
        .collect();

    if fields.is_empty() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_FIELDS",
            "message": "No valid fields. Each field needs a selector and a value."
        }));
    }

    match webview_fill_form(&fields, submit_selector).await {
        Ok(result) => {
            // Partial-fill recovery: when some fields missed, surface visible inputs
            // so the agent can correct the selectors on the next call.
            let missed = result.total_fields.saturating_sub(result.fields_filled);
            let mut response = serde_json::json!({
                "status": "completed",
                "fields_filled": result.fields_filled,
                "total_fields": result.total_fields,
                "submitted": result.submitted,
            });
            if missed > 0 {
                let inputs_script = r#"return JSON.stringify(Array.from(document.querySelectorAll('input,textarea,select'))
                    .filter(function(el) { var r = el.getBoundingClientRect(); var s = window.getComputedStyle(el);
                        return r.width > 0 && r.height > 0 && s.display !== 'none' && s.visibility !== 'hidden'; })
                    .slice(0, 10)
                    .map(function(el) {
                        var tp = el.getAttribute('type') || '';
                        var sel = el.id ? '#' + el.id : (el.name ? el.tagName.toLowerCase() + '[name="' + el.name + '"]' : el.tagName.toLowerCase() + (tp ? '[type="' + tp + '"]' : ''));
                        return {
                            tag: el.tagName.toLowerCase(),
                            type: tp,
                            selector: sel,
                            name: el.name || '',
                            placeholder: el.placeholder || '',
                            label: (el.getAttribute('aria-label') || '').substring(0, 60)
                        };
                    }));"#;
                // Best-effort recovery hint. Staying silent is acceptable here
                // (the caller already has its answer), but log rather than
                // vanish so a decode bug is not invisible.
                let candidates: Vec<Value> =
                    match crate::platform::exec_js_parse(inputs_script, Some(3000)).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("[fill_form] nearby-input probe failed: {e}");
                            Vec::new()
                        }
                    };
                if let Some(obj) = response.as_object_mut() {
                    obj.insert("missed_count".into(), Value::from(missed));
                    obj.insert("nearby".into(), Value::Array(candidates));
                    obj.insert(
                        "hint".into(),
                        Value::String(
                            "Some selectors didn't match. See `nearby` for visible form inputs."
                                .into(),
                        ),
                    );
                }
            }
            Ok(response)
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "FILL_FORM_ERROR",
            "message": e
        })),
    }
}

/// preprocess_captcha — captcha image preprocessing (no WebView needed).
///
/// Pipeline: load → grayscale → contrast → median filter → threshold → erosion → dilation → save.
async fn tool_preprocess_captcha(args: &Value) -> Result<Value> {
    let site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let input_file = args["input_file"]
        .as_str()
        .ok_or_else(|| anyhow!("input_file required"))?;
    let output_file = args["output_file"]
        .as_str()
        .unwrap_or("captcha_processed.png");

    // Read the parameters (with defaults) and clamp them to their ranges
    let contrast = (args["contrast"].as_f64().unwrap_or(1.5) as f32).clamp(0.5, 3.0);
    let median_radius = (args["median_radius"].as_u64().unwrap_or(1) as u32).min(5);
    let threshold_block_radius =
        (args["threshold_block_radius"].as_u64().unwrap_or(11) as u32).min(51);
    let morphology_radius = (args["morphology_radius"].as_u64().unwrap_or(1) as u8).min(3);

    tracing::info!(
        "preprocess_captcha: site={}, input={}, output={}, contrast={}, median_r={}, threshold_br={}, morph_r={}",
        site, input_file, output_file, contrast, median_radius, threshold_block_radius, morphology_radius
    );

    // Build the file paths
    let site_path = get_sites_path().join(site);
    let input_path = site_path.join(input_file);
    let output_path = site_path.join(output_file);

    // Check that the input file exists
    if !input_path.exists() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "FILE_NOT_FOUND",
            "message": format!("Input file not found: {}", input_path.display())
        }));
    }

    // Preprocess the image (CPU-bound, so spawn_blocking)
    let input_path_clone = input_path.clone();
    let output_path_clone = output_path.clone();

    let process_result = tokio::task::spawn_blocking(move || -> Result<(u32, u32), String> {
        use image::{DynamicImage, GrayImage};
        use imageproc::contrast::{adaptive_threshold, otsu_level, threshold, ThresholdType};
        use imageproc::distance_transform::Norm;
        use imageproc::filter::median_filter;
        use imageproc::morphology::{dilate, erode};

        // Step 1: load the image
        let img =
            image::open(&input_path_clone).map_err(|e| format!("Image load failed: {}", e))?;

        // Step 2: convert to grayscale
        let mut gray: GrayImage = img.to_luma8();
        let (width, height) = gray.dimensions();

        // Step 3: adjust the contrast (linear scaling around the midpoint 128)
        if (contrast - 1.0_f32).abs() > 0.01 {
            for pixel in gray.pixels_mut() {
                let val = pixel[0] as f32;
                let adjusted = ((val - 128.0) * contrast + 128.0).clamp(0.0, 255.0);
                pixel[0] = adjusted as u8;
            }
        }

        // Step 4: median filter (removes salt-and-pepper noise)
        if median_radius > 0 {
            gray = median_filter(&gray, median_radius, median_radius);
        }

        // Step 5: binarize
        if threshold_block_radius > 0 {
            // Adaptive threshold: uses the local mean, so it handles uneven backgrounds
            gray = adaptive_threshold(&gray, threshold_block_radius, 0);
        } else {
            // Otsu: computes the best global threshold
            let otsu = otsu_level(&gray);
            gray = threshold(&gray, otsu, ThresholdType::Binary);
        }

        // Step 6: erosion (removes small noise dots)
        if morphology_radius > 0 {
            gray = erode(&gray, Norm::LInf, morphology_radius);
        }

        // Step 7: dilation (restores the characters that the erosion made thin; together an opening)
        if morphology_radius > 0 {
            gray = dilate(&gray, Norm::LInf, morphology_radius);
        }

        // Step 8: save as PNG
        let dyn_img = DynamicImage::ImageLuma8(gray);
        dyn_img
            .save(&output_path_clone)
            .map_err(|e| format!("Image save failed: {}", e))?;

        Ok((width, height))
    })
    .await
    .map_err(|e| anyhow!("Task join error: {}", e))?;

    match process_result {
        Ok((width, height)) => {
            let metadata = tokio::fs::metadata(&output_path).await;
            let size = metadata.map(|m| m.len()).unwrap_or(0);

            Ok(serde_json::json!({
                "status": "completed",
                "input_file": input_path.display().to_string(),
                "output_file": output_path.display().to_string(),
                "size": size,
                "dimensions": { "width": width, "height": height },
                "params": {
                    "contrast": contrast,
                    "median_radius": median_radius,
                    "threshold_block_radius": threshold_block_radius,
                    "morphology_radius": morphology_radius
                }
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "PREPROCESS_FAILED",
            "message": e
        })),
    }
}

/// get_captured_requests — return captured network requests (with server-side filtering).
/// Filter by url_pattern/url_exclude/content_type/method, cap with max_count, drop previews via include_preview.
pub(crate) async fn tool_get_captured_requests(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let seconds = args["seconds"].as_u64().unwrap_or(180);
    let url_pattern = args["url_pattern"].as_str().unwrap_or("");
    let url_exclude = args["url_exclude"].as_str().unwrap_or("");
    let content_type_filter = args["content_type"].as_str().unwrap_or("");
    let method_filter = args["method"].as_str().unwrap_or("");
    let max_count = args["max_count"].as_u64().unwrap_or(0) as usize;
    let include_preview = args["include_preview"].as_bool().unwrap_or(true);
    let include_headers = args["include_headers"].as_bool().unwrap_or(false);
    let no_redact = args["no_redact"].as_bool().unwrap_or(false);

    tracing::info!(
        "get_captured_requests: seconds={}, url_pattern={}, url_exclude={}, ct={}, method={}, max={}, preview={}, headers={}, no_redact={}",
        seconds, url_pattern, url_exclude, content_type_filter, method_filter, max_count, include_preview, include_headers, no_redact
    );

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    // Get all captured data from JS (Rust does the filtering)
    let script = format!(
        r#"var now=Date.now();
            var cutoff=now-({seconds}*1000);
            var reqs=((window.{ns}||{{}}).cr||[]).filter(function(r){{return r.ts>=cutoff;}});
            return JSON.stringify(reqs.map(function(r){{
                return {{
                    id:r.id,method:r.method,url:r.url,status:r.status,
                    contentType:r.ct,duration:r.dur,type:r.type,
                    ts:r.ts,
                    intercepted:r.intercepted||null,
                    pending:(r.status===undefined&&!r.error)||null,
                    hasBody:!!r.resBody,
                    bodyPreview:r.resBody?r.resBody.substring(0,200):null,
                    resPreview:r.resPreview||null,
                    reqBody:r.reqBody?r.reqBody.substring(0,200):null,
                    headers:r.hdrs||null,
                    error:r.error
                }};
            }}))"#,
        seconds = seconds,
        ns = crate::pagens::namespace()
    );

    match webview_execute_js(&script, Some(5000)).await {
        Ok(result) => {
            let all_requests: Vec<Value> = if let Some(s) = result.as_str() {
                serde_json::from_str(s).unwrap_or_default()
            } else if let Some(arr) = result.as_array() {
                arr.clone()
            } else {
                vec![]
            };

            let total_captured = all_requests.len();

            // Filter on the Rust side
            let mut filtered: Vec<Value> = all_requests
                .into_iter()
                .filter(|r| {
                    let url = r["url"].as_str().unwrap_or("");
                    let ct = r["contentType"].as_str().unwrap_or("");
                    let m = r["method"].as_str().unwrap_or("");

                    // URL include filter
                    if !url_pattern.is_empty() && !url.contains(url_pattern) {
                        return false;
                    }
                    // URL exclude filter ('|'-separated)
                    if !url_exclude.is_empty() {
                        for pat in url_exclude.split('|') {
                            let pat = pat.trim();
                            if !pat.is_empty() && url.contains(pat) {
                                return false;
                            }
                        }
                    }
                    // Content-Type filter
                    if !content_type_filter.is_empty()
                        && !ct
                            .to_lowercase()
                            .contains(&content_type_filter.to_lowercase())
                    {
                        return false;
                    }
                    // Method filter
                    if !method_filter.is_empty() && !m.eq_ignore_ascii_case(method_filter) {
                        return false;
                    }
                    true
                })
                .collect();

            // Apply max_count
            if max_count > 0 && filtered.len() > max_count {
                filtered.truncate(max_count);
            }

            // With include_preview=false, remove the preview fields
            if !include_preview {
                for req in filtered.iter_mut() {
                    if let Some(obj) = req.as_object_mut() {
                        obj.remove("bodyPreview");
                        obj.remove("resPreview");
                        obj.remove("reqBody");
                    }
                }
            }

            // Headers: left out by default. With include_headers, sensitive headers keep only a 12-char prefix
            for req in filtered.iter_mut() {
                if let Some(obj) = req.as_object_mut() {
                    if !include_headers {
                        obj.remove("headers");
                    } else if !no_redact {
                        if let Some(Value::Object(headers)) = obj.get_mut("headers") {
                            for (name, value) in headers.iter_mut() {
                                if is_sensitive_header(name) {
                                    if let Some(s) = value.as_str() {
                                        *value = Value::String(redact_header_value(s));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let returned = filtered.len();
            Ok(serde_json::json!({
                "status": "completed",
                "requests": filtered,
                "total_captured": total_captured,
                "returned": returned,
                "seconds": seconds
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "EXECUTE_JS_FAILED",
            "message": e.to_string()
        })),
    }
}

/// Header names whose values are redacted by default in get_captured_requests.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-access-token",
    "x-csrf-token",
];

pub(crate) fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS.contains(&name.to_ascii_lowercase().as_str())
}

/// Redact a sensitive header value while keeping enough of a prefix to tell two
/// tokens apart (e.g. old vs new Authorization after an account switch):
/// `Bearer eyJhbGciOiJIUzI1NiIs…` → `Bearer eyJhbGciOiJI…[REDACTED 843 chars]`.
/// Values of 16 chars or fewer are fully redacted.
pub(crate) fn redact_header_value(value: &str) -> String {
    let (scheme, cred) = match value.split_once(' ') {
        Some((s, c)) => (Some(s), c),
        None => (None, value),
    };
    let cred_len = cred.chars().count();
    let shown: String = if cred_len > 16 {
        cred.chars().take(12).collect()
    } else {
        String::new()
    };
    let prefix = scheme.map(|s| format!("{} ", s)).unwrap_or_default();
    format!("{}{}…[REDACTED {} chars]", prefix, shown, cred_len)
}

/// intercept — add a network fault-injection rule and push it into the WebView.
pub(crate) async fn tool_intercept(args: &Value) -> Result<Value> {
    use crate::intercept::{InterceptRule, INTERCEPT_RULES, VALID_ACTIONS};

    let pattern = match args["pattern"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "PATTERN_REQUIRED",
                "message": "pattern is required (substring, or glob with *)"
            }))
        }
    };
    let action = args["action"].as_str().unwrap_or("").to_lowercase();
    if !VALID_ACTIONS.contains(&action.as_str()) {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_ACTION",
            "message": format!("action must be one of {:?}, got '{}'", VALID_ACTIONS, action)
        }));
    }
    let ms = args["ms"].as_u64();
    if action == "delay" && ms.is_none() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "MS_REQUIRED",
            "message": "action=delay requires --ms (delay duration in milliseconds)"
        }));
    }

    let rule = InterceptRule {
        pattern,
        action,
        ms,
        status: args["status"].as_u64().map(|s| s as u16),
        body: args["body"].as_str().map(String::from),
        content_type: args["content_type"].as_str().map(String::from),
        method: args["method"].as_str().map(|m| m.to_uppercase()),
    };

    tracing::info!(
        "intercept: pattern={}, action={}, ms={:?}",
        rule.pattern,
        rule.action,
        rule.ms
    );

    let rules_snapshot = {
        let mut rules = INTERCEPT_RULES.write().await;
        // Same pattern+method replaces the existing rule instead of shadowing it.
        rules.retain(|r| !(r.pattern == rule.pattern && r.method == rule.method));
        rules.push(rule.clone());
        rules.clone()
    };

    match crate::intercept::sync_rules_to_webview().await {
        Ok(sync) => Ok(serde_json::json!({
            "status": "completed",
            "added": rule,
            "active_rules": rules_snapshot,
            "sync": sync
        })),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "SYNC_FAILED",
            "message": e,
            "active_rules": rules_snapshot
        })),
    }
}

/// intercept_list — list active fault-injection rules.
pub(crate) async fn tool_intercept_list(_args: &Value) -> Result<Value> {
    use crate::intercept::{current_version, INTERCEPT_RULES};

    let rules = INTERCEPT_RULES.read().await.clone();

    // Chrome enforces rules in the browser; there is no in-page copy to read,
    // and reading the namespace anyway would report `rules: 0` for rules that
    // are in force.
    if crate::engine::is_chrome() {
        return Ok(serde_json::json!({
            "status": "completed",
            "rules": rules,
            "rule_count": rules.len(),
            "daemon_version": current_version(),
            "transport": "cdp_fetch",
            "page": Value::Null,
        }));
    }

    // Best effort: report what the live page actually has applied.
    let page_state = webview_execute_js(
        &format!(
            "var NS = window.{ns} || {{}}; \
             return JSON.stringify({{v: NS.v || 0, rules: (NS.rules || []).length}})",
            ns = crate::pagens::namespace()
        ),
        Some(3000),
    )
    .await
    .ok()
    .and_then(|v| {
        v.as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
    });

    Ok(serde_json::json!({
        "status": "completed",
        "rules": rules,
        "rule_count": rules.len(),
        "daemon_version": current_version(),
        "page": page_state
    }))
}

/// intercept_clear — remove all rules, or only those matching `pattern` exactly.
pub(crate) async fn tool_intercept_clear(args: &Value) -> Result<Value> {
    use crate::intercept::INTERCEPT_RULES;

    let pattern = args["pattern"].as_str();
    let (removed, remaining) = {
        let mut rules = INTERCEPT_RULES.write().await;
        let before = rules.len();
        match pattern {
            Some(p) => rules.retain(|r| r.pattern != p),
            None => rules.clear(),
        }
        (before - rules.len(), rules.clone())
    };

    tracing::info!(
        "intercept_clear: pattern={:?}, removed={}",
        pattern,
        removed
    );

    match crate::intercept::sync_rules_to_webview().await {
        Ok(sync) => Ok(serde_json::json!({
            "status": "completed",
            "removed": removed,
            "active_rules": remaining,
            "sync": sync
        })),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "SYNC_FAILED",
            "message": e,
            "removed": removed,
            "active_rules": remaining
        })),
    }
}

/// get_frame_list — list iframes on the page.
/// Similar to CefSharp's `GetFrameIdentifiers()`.
async fn tool_get_frame_list(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;

    let full = args["full"].as_bool().unwrap_or(false);

    tracing::info!("get_frame_list full={full}");

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    match webview_get_frame_list(full).await {
        #[allow(unused_mut)]
        Ok(mut frames) => {
            // Attach each iframe's CDP frame id — the stable handle that
            // `exec-js --frame` accepts. Same DOM enumeration on both sides,
            // so index i here IS `--frame i`. Best-effort: a slot that yields
            // no frame id stays None rather than failing the listing.
            #[cfg(target_os = "windows")]
            if let Some(window) = crate::get_main_window().await {
                if let Ok(ids) = crate::platform::dom_iframe_frame_ids(&window).await {
                    for (frame, fid) in frames.iter_mut().zip(ids) {
                        if let Some(id) = fid.as_deref() {
                            if crate::sw_console::session_for_frame(id).is_some() {
                                frame.out_of_process = Some(true);
                                frame.url = crate::platform::frame_target_url(&window, id).await;
                            }
                        }
                        frame.frame_id = fid;
                    }
                }
            }
            let frame_count = frames.len();
            let visible_count = frames.iter().filter(|f| f.visible).count();
            let cross_origin_count = frames.iter().filter(|f| !f.can_access).count();

            Ok(serde_json::json!({
                "status": "completed",
                "frames": frames,
                "total_count": frame_count,
                "visible_count": visible_count,
                "cross_origin_count": cross_origin_count,
                "message": if frame_count == 0 {
                    "No iframes on the page".to_string()
                } else {
                    format!("Found {} iframe(s) (visible: {}, cross-origin: {})",
                        frame_count, visible_count, cross_origin_count)
                }
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "GET_FRAME_LIST_ERROR",
            "message": e
        })),
    }
}

/// get_frame_html — fetch the HTML of a specific iframe.
/// Same-origin: accessed directly via `contentDocument`.
/// Cross-origin: returns the src URL (call `navigate` to inspect it).
async fn tool_get_frame_html(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let selector = args["selector"].as_str();
    let index = args["index"].as_u64().map(|v| v as usize);

    tracing::info!("get_frame_html: selector={:?}, index={:?}", selector, index);

    if selector.is_none() && index.is_none() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_PARAMS",
            "message": "Either selector or index is required"
        }));
    }

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    match webview_get_frame_html(selector, index).await {
        Ok(result) => {
            let html_len = result.html.as_ref().map(|h| h.len()).unwrap_or(0);

            if result.html.is_some() {
                // HTML read succeeded (SOP is off, so cross-origin frames are readable too)
                Ok(serde_json::json!({
                    "status": "completed",
                    "origin": result.origin,
                    "method": result.method,
                    "is_cross_origin": result.is_cross_origin,
                    "html": result.html,
                    "html_length": html_len,
                    "src": result.src,
                    "id": result.id,
                    "name": result.name,
                    "message": if result.is_cross_origin {
                        "Cross-origin iframe accessed directly via native API (SOP disabled)."
                    } else {
                        "Same-origin iframe accessed via contentDocument."
                    }
                }))
            } else {
                // HTML read failed
                Ok(serde_json::json!({
                    "status": "failed",
                    "origin": result.origin,
                    "method": result.method,
                    "is_cross_origin": result.is_cross_origin,
                    "html": null,
                    "src": result.src,
                    "id": result.id,
                    "name": result.name,
                    "error": result.error,
                    "message": "Failed to access iframe HTML.",
                    "action_required": "navigate",
                    "navigate_url": result.src
                }))
            }
        }
        Err(e) => {
            let error_code = if e.contains("FRAME_NOT_FOUND") {
                "FRAME_NOT_FOUND"
            } else {
                "GET_FRAME_HTML_ERROR"
            };
            Ok(serde_json::json!({
                "status": "failed",
                "error": error_code,
                "message": e
            }))
        }
    }
}

/// auto_dismiss_modals — automatically dismiss modals and popups.
/// Covers modal patterns from Vue.js, Element UI, Bootstrap, and other common frameworks.
async fn tool_auto_dismiss_modals(args: &Value) -> Result<Value> {
    let _site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let max_attempts = args["max_attempts"].as_u64().map(|v| v as u32);
    let delay_ms = args["delay_ms"].as_u64().map(|v| v as u32);

    tracing::info!(
        "auto_dismiss_modals: max_attempts={:?}, delay_ms={:?}",
        max_attempts,
        delay_ms
    );

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND",
            "message": "WebView not ready"
        }));
    }

    match webview_auto_dismiss_modals(max_attempts, delay_ms).await {
        Ok(result) => {
            let message = if result.remaining_count == 0 {
                format!("All modals dismissed ({} closed)", result.dismissed_count)
            } else {
                format!(
                    "Dismiss attempt finished ({} closed, {} remaining)",
                    result.dismissed_count, result.remaining_count
                )
            };

            Ok(serde_json::json!({
                "status": "completed",
                "dismissed_count": result.dismissed_count,
                "remaining_count": result.remaining_count,
                "attempts": result.attempts,
                "modals_found": result.modals_found,
                "message": message
            }))
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "AUTO_DISMISS_MODALS_ERROR",
            "message": e
        })),
    }
}

// ============================================================================
// New Tools: click, scroll, keyboard_input, element_at
// ============================================================================

/// click — coordinate-based clicking via JS (supports cross-origin iframes).
async fn tool_click(args: &Value) -> Result<Value> {
    let x = args["x"]
        .as_f64()
        .ok_or_else(|| anyhow!("x coordinate required"))?;
    let y = args["y"]
        .as_f64()
        .ok_or_else(|| anyhow!("y coordinate required"))?;
    let click_type = args["click_type"].as_str().unwrap_or("left");

    tracing::info!("click: x={}, y={}, type={}", x, y, click_type);

    // Step 1: find out whether the element at the point is an iframe
    let detect_script = format!(
        r#"var el = document.elementFromPoint({x}, {y});
        if (!el) return JSON.stringify({{ type: 'none' }});
        if (el.tagName === 'IFRAME') {{
            var rect = el.getBoundingClientRect();
            var sameOrigin = false;
            try {{ var fd = el.contentDocument; sameOrigin = !!fd; }} catch(e) {{}}
            var iframes = document.querySelectorAll('iframe');
            var idx = -1;
            for (var i = 0; i < iframes.length; i++) {{ if (iframes[i] === el) {{ idx = i; break; }} }}
            return JSON.stringify({{ type: 'iframe', sameOrigin: sameOrigin, iframeIndex: idx,
                localX: {x} - rect.left, localY: {y} - rect.top,
                rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }} }});
        }}
        return JSON.stringify({{ type: 'element' }});"#,
        x = x,
        y = y,
    );

    let detect_info: serde_json::Value = match webview_execute_js(&detect_script, Some(3000)).await
    {
        Ok(val) => val
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::json!({"type": "element"})),
        Err(_) => serde_json::json!({"type": "element"}),
    };

    let target_type = detect_info["type"].as_str().unwrap_or("element");

    // Step 2: for an iframe, use an OS-level native mouse click
    // Real mouse input through CGEvent (macOS) / SendInput (Windows) instead of synthetic JS events
    if target_type == "iframe" {
        let iframe_rect = &detect_info["rect"];
        tracing::info!(
            "click: iframe detected at ({}, {}), using native mouse click",
            x,
            y
        );

        // Focus the iframe first
        let focus_script = format!(
            "var iframes = document.querySelectorAll('iframe'); if (iframes[{}]) iframes[{}].focus();",
            detect_info["iframeIndex"].as_u64().unwrap_or(0),
            detect_info["iframeIndex"].as_u64().unwrap_or(0),
        );
        let _ = webview_execute_js(&focus_script, Some(1000)).await;

        // OS-level native mouse click (the viewport coordinates go through unchanged)
        match crate::platform::native_mouse_click(x, y, click_type).await {
            Ok(()) => {
                // Wait 200 ms after the click so the UI can react
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                return Ok(serde_json::json!({
                    "status": "completed",
                    "method": "native",
                    "tag": "iframe",
                    "text": "",
                    "rect": {
                        "x": iframe_rect["x"],
                        "y": iframe_rect["y"],
                        "width": iframe_rect["width"],
                        "height": iframe_rect["height"],
                    }
                }));
            }
            Err(e) => {
                tracing::warn!("native click failed: {}, falling back to JS", e);
                // Fallback: synthetic JS events
            }
        }
    }

    // Step 3: click a regular element
    let click_event = match click_type {
        "right" => format!("el.dispatchEvent(new MouseEvent('contextmenu', {{ bubbles: true, cancelable: true, clientX: {x}, clientY: {y}, button: 2 }}));", x = x, y = y),
        "double" => format!("el.dispatchEvent(new MouseEvent('dblclick', {{ bubbles: true, cancelable: true, clientX: {x}, clientY: {y} }}));", x = x, y = y),
        _ => format!(
            "el.focus(); el.dispatchEvent(new MouseEvent('mousedown', {{ bubbles: true, cancelable: true, clientX: {x}, clientY: {y} }})); el.dispatchEvent(new MouseEvent('mouseup', {{ bubbles: true, cancelable: true, clientX: {x}, clientY: {y} }})); el.click();",
            x = x, y = y
        ),
    };

    let script = format!(
        r#"var el = document.elementFromPoint({x}, {y});
        if (!el) return {{ status: 'failed', error: 'No element at coordinates' }};
        var rect = el.getBoundingClientRect();
        {click_event}
        return {{ status: 'completed', tag: el.tagName.toLowerCase(), text: (el.textContent || '').trim().substring(0, 100),
            rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }} }};"#,
        x = x,
        y = y,
        click_event = click_event,
    );

    match webview_execute_js(&script, Some(5000)).await {
        Ok(result) => Ok(result),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "CLICK_ERROR",
            "message": e
        })),
    }
}

/// scroll — page or element scrolling
async fn tool_scroll(args: &Value) -> Result<Value> {
    let direction = args["direction"].as_str().unwrap_or("down");
    let amount = args["amount"].as_i64().unwrap_or(500);
    let selector = args["selector"].as_str();
    let scroll_to = args["to"].as_str();

    tracing::info!(
        "scroll: direction={}, amount={}, selector={:?}, to={:?}",
        direction,
        amount,
        selector,
        scroll_to
    );

    // Handle --to absolute positioning
    if let Some(to_pos) = scroll_to {
        let script = match selector {
            Some(sel) => {
                let scroll_expr = match to_pos {
                    "top" => "el.scrollTop = 0; el.scrollLeft = el.scrollLeft;",
                    "bottom" => "el.scrollTop = el.scrollHeight;",
                    "left-end" => "el.scrollLeft = 0;",
                    "right-end" => "el.scrollLeft = el.scrollWidth;",
                    _ => {
                        return Ok(
                            serde_json::json!({ "status": "failed", "error": format!("Invalid --to value: {}. Use: top, bottom, left-end, right-end", to_pos) }),
                        )
                    }
                };
                format!(
                    r#"var el = document.querySelector('{sel}');
                    if (!el) return {{ status: 'failed', error: 'Selector not found: {sel}' }};
                    {scroll_expr}
                    return {{ status: 'completed', to: '{to_pos}', element: '{sel}',
                        scrollTop: el.scrollTop, scrollLeft: el.scrollLeft }};"#,
                    sel = sel,
                    scroll_expr = scroll_expr,
                    to_pos = to_pos
                )
            }
            None => {
                let scroll_expr = match to_pos {
                    "top" => "window.scrollTo(0, 0);",
                    "bottom" => "window.scrollTo(0, document.documentElement.scrollHeight);",
                    "left-end" => "window.scrollTo(0, window.scrollY);",
                    "right-end" => {
                        "window.scrollTo(document.documentElement.scrollWidth, window.scrollY);"
                    }
                    _ => {
                        return Ok(
                            serde_json::json!({ "status": "failed", "error": format!("Invalid --to value: {}. Use: top, bottom, left-end, right-end", to_pos) }),
                        )
                    }
                };
                format!(
                    r#"{scroll_expr}
                    return {{ status: 'completed', to: '{to_pos}',
                        scrollX: window.scrollX, scrollY: window.scrollY,
                        pageHeight: document.documentElement.scrollHeight,
                        viewportHeight: window.innerHeight }};"#,
                    scroll_expr = scroll_expr,
                    to_pos = to_pos
                )
            }
        };

        return match webview_execute_js(&script, Some(5000)).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(serde_json::json!({
                "status": "failed",
                "error": "SCROLL_ERROR",
                "message": e
            })),
        };
    }

    let (dx, dy) = match direction {
        "up" => (0, -amount),
        "down" => (0, amount),
        "left" => (-amount, 0),
        "right" => (amount, 0),
        _ => (0, amount),
    };

    let script = match selector {
        Some(sel) => format!(
            r#"var el = document.querySelector('{sel}');
            if (!el) return {{ status: 'failed', error: 'Selector not found: {sel}' }};
            var beforeTop = el.scrollTop;
            var beforeLeft = el.scrollLeft;
            el.scrollBy({dx}, {dy});
            if (el.scrollTop === beforeTop && {dy} !== 0) {{
                el.scrollTop = beforeTop + {dy};
            }}
            if (el.scrollLeft === beforeLeft && {dx} !== 0) {{
                el.scrollLeft = beforeLeft + {dx};
            }}
            return {{ status: 'completed', scrolled: {{ dx: {dx}, dy: {dy} }}, element: '{sel}',
                scrollTop: el.scrollTop, scrollLeft: el.scrollLeft }};"#,
            sel = sel,
            dx = dx,
            dy = dy
        ),
        None => format!(
            r#"window.scrollBy({dx}, {dy});
            return {{ status: 'completed', scrolled: {{ dx: {dx}, dy: {dy} }},
                scrollX: window.scrollX, scrollY: window.scrollY,
                pageHeight: document.documentElement.scrollHeight,
                viewportHeight: window.innerHeight }};"#,
            dx = dx,
            dy = dy
        ),
    };

    match webview_execute_js(&script, Some(5000)).await {
        Ok(result) => Ok(result),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "SCROLL_ERROR",
            "message": e
        })),
    }
}

/// keyboard_input — type text or press special keys (supports cross-origin iframes).
async fn tool_keyboard_input(args: &Value) -> Result<Value> {
    let text = args["text"].as_str();
    let key = args["key"].as_str();
    let selector = args["selector"].as_str();
    let modifiers = args["modifiers"].as_str().unwrap_or("");

    if text.is_none() && key.is_none() {
        return Err(anyhow!("Either 'text' or 'key' is required"));
    }

    tracing::info!(
        "keyboard_input: text={:?}, key={:?}, selector={:?}, modifiers={}",
        text,
        key,
        selector,
        modifiers
    );

    let modifier_parts: Vec<&str> = if modifiers.is_empty() {
        vec![]
    } else {
        modifiers.split(',').map(|s| s.trim()).collect()
    };
    let ctrl = modifier_parts.contains(&"ctrl");
    let shift = modifier_parts.contains(&"shift");
    let alt = modifier_parts.contains(&"alt");
    let meta = modifier_parts.contains(&"meta");

    let paste = args["paste"].as_bool().unwrap_or(false);

    // Detect a cross-origin iframe: check whether activeElement is an IFRAME
    if selector.is_none() {
        let detect_script = r#"var ae = document.activeElement;
            if (ae && ae.tagName === 'IFRAME') {
                var iframes = document.querySelectorAll('iframe');
                var idx = -1;
                for (var i = 0; i < iframes.length; i++) { if (iframes[i] === ae) { idx = i; break; } }
                var sameOrigin = false;
                try { var fd = ae.contentDocument; sameOrigin = !!fd; } catch(e) {}
                return JSON.stringify({ type: 'iframe', sameOrigin: sameOrigin, iframeIndex: idx });
            }
            return JSON.stringify({ type: 'element' });"#;

        let detect_info: serde_json::Value =
            match webview_execute_js(detect_script, Some(3000)).await {
                Ok(val) => val
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::json!({"type": "element"})),
                Err(_) => serde_json::json!({"type": "element"}),
            };

        if detect_info["type"].as_str() == Some("iframe") {
            let same_origin = detect_info["sameOrigin"].as_bool().unwrap_or(false);
            let iframe_index = detect_info["iframeIndex"].as_u64().unwrap_or(0) as usize;

            if same_origin {
                // Same-origin iframe: use contentDocument
                let inner_script = if let Some(text_val) = text {
                    let text_json = serde_json::to_string(text_val).unwrap_or_default();
                    if paste {
                        format!(
                            r#"var iframe = document.querySelectorAll('iframe')[{idx}];
                            var fd = iframe.contentDocument;
                            var el = fd.activeElement || fd.querySelector('input:not([type=hidden]),textarea');
                            if (!el) return JSON.stringify({{ status: 'failed', error: 'No focusable element in iframe' }});
                            el.focus();
                            var text = {text};
                            if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                                var s = Object.getOwnPropertyDescriptor(el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value').set;
                                s.call(el, el.value + text);
                                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                            }}
                            return JSON.stringify({{ status: 'completed', typed: text, length: text.length, element: el.tagName.toLowerCase(), mode: 'paste' }});"#,
                            idx = iframe_index,
                            text = text_json,
                        )
                    } else {
                        format!(
                            r#"var iframe = document.querySelectorAll('iframe')[{idx}];
                            var fd = iframe.contentDocument;
                            var el = fd.activeElement || fd.querySelector('input:not([type=hidden]),textarea');
                            if (!el) return JSON.stringify({{ status: 'failed', error: 'No focusable element in iframe' }});
                            el.focus();
                            var text = {text};
                            for (var i = 0; i < text.length; i++) {{
                                var ch = text[i];
                                el.dispatchEvent(new KeyboardEvent('keydown', {{ key: ch, bubbles: true }}));
                                if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                                    var s = Object.getOwnPropertyDescriptor(el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value').set;
                                    s.call(el, el.value + ch);
                                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                                }}
                                el.dispatchEvent(new KeyboardEvent('keyup', {{ key: ch, bubbles: true }}));
                            }}
                            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                            return JSON.stringify({{ status: 'completed', typed: text, length: text.length, element: el.tagName.toLowerCase() }});"#,
                            idx = iframe_index,
                            text = text_json,
                        )
                    }
                } else if let Some(key_val) = key {
                    let key_json = serde_json::to_string(key_val).unwrap_or_default();
                    format!(
                        r#"var iframe = document.querySelectorAll('iframe')[{idx}];
                        var fd = iframe.contentDocument;
                        var el = fd.activeElement || fd.body;
                        var k = {key};
                        el.dispatchEvent(new KeyboardEvent('keydown', {{ key: k, code: k, bubbles: true, cancelable: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
                        el.dispatchEvent(new KeyboardEvent('keyup', {{ key: k, code: k, bubbles: true, cancelable: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
                        return JSON.stringify({{ status: 'completed', key: k, element: el.tagName.toLowerCase() }});"#,
                        idx = iframe_index,
                        key = key_json,
                        ctrl = ctrl,
                        shift = shift,
                        alt = alt,
                        meta = meta,
                    )
                } else {
                    unreachable!()
                };

                return match webview_execute_js(&inner_script, Some(10000)).await {
                    Ok(val) => {
                        let parsed = val
                            .as_str()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(val);
                        Ok(parsed)
                    }
                    Err(e) => Ok(
                        serde_json::json!({"status": "failed", "error": "IFRAME_KEYBOARD_ERROR", "message": e}),
                    ),
                };
            } else {
                // Cross-origin: native bridge / CDP
                tracing::info!(
                    "keyboard: cross-origin iframe detected, dispatching via native bridge (idx={})",
                    iframe_index
                );
                let mods_json = serde_json::json!({
                    "ctrl": ctrl, "shift": shift, "alt": alt, "meta": meta,
                });
                return match webview_keyboard_in_cross_origin_iframe(
                    text,
                    key,
                    paste,
                    &mods_json,
                    iframe_index,
                )
                .await
                {
                    Ok(result) => Ok(result),
                    Err(e) => Ok(
                        serde_json::json!({"status": "failed", "error": "IFRAME_KEYBOARD_ERROR", "message": e}),
                    ),
                };
            }
        }
    }

    // Regular keyboard input
    let script = if let Some(text_val) = text {
        if paste {
            format!(
                r#"var el = {focus_script};
                var text = {text_json};
                if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                    var nativeSetter = Object.getOwnPropertyDescriptor(
                        el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value'
                    ).set;
                    nativeSetter.call(el, el.value + text);
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }} else {{
                    el.textContent = text;
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                }}
                return {{ status: 'completed', typed: text, length: text.length, element: el.tagName.toLowerCase(), mode: 'paste' }};"#,
                focus_script = focus_script(selector),
                text_json = serde_json::to_string(text_val).unwrap_or_default(),
            )
        } else {
            format!(
                r#"var el = {focus_script};
                var text = {text_json};
                for (var i = 0; i < text.length; i++) {{
                    var ch = text[i];
                    el.dispatchEvent(new KeyboardEvent('keydown', {{ key: ch, bubbles: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
                    el.dispatchEvent(new KeyboardEvent('keypress', {{ key: ch, bubbles: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
                    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                        var nativeSetter = Object.getOwnPropertyDescriptor(
                            el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value'
                        ).set;
                        nativeSetter.call(el, el.value + ch);
                        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    }}
                    el.dispatchEvent(new KeyboardEvent('keyup', {{ key: ch, bubbles: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
                }}
                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                return {{ status: 'completed', typed: text, length: text.length, element: el.tagName.toLowerCase() }};"#,
                focus_script = focus_script(selector),
                text_json = serde_json::to_string(text_val).unwrap_or_default(),
                ctrl = ctrl,
                shift = shift,
                alt = alt,
                meta = meta,
            )
        }
    } else if let Some(key_val) = key {
        format!(
            r#"var el = {focus_script};
            var key = {key_json};
            el.dispatchEvent(new KeyboardEvent('keydown', {{ key: key, code: key, bubbles: true, cancelable: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
            el.dispatchEvent(new KeyboardEvent('keyup', {{ key: key, code: key, bubbles: true, cancelable: true, ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta} }}));
            return {{ status: 'completed', key: key, element: el.tagName.toLowerCase() }};"#,
            focus_script = focus_script(selector),
            key_json = serde_json::to_string(key_val).unwrap_or_default(),
            ctrl = ctrl,
            shift = shift,
            alt = alt,
            meta = meta,
        )
    } else {
        unreachable!()
    };

    match webview_execute_js(&script, Some(10000)).await {
        Ok(result) => Ok(result),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "KEYBOARD_INPUT_ERROR",
            "message": e
        })),
    }
}

/// Helper: generate JS to focus an element (by selector or active element)
fn focus_script(selector: Option<&str>) -> String {
    match selector {
        Some(sel) => format!(
            r#"(() => {{ const el = document.querySelector('{}'); if (!el) throw new Error('Selector not found: {}'); el.focus(); return el; }})()"#,
            sel, sel
        ),
        None => "document.activeElement || document.body".to_string(),
    }
}

/// element_at — find nearest interactive element at coordinates
async fn tool_element_at(args: &Value) -> Result<Value> {
    let x = args["x"]
        .as_f64()
        .ok_or_else(|| anyhow!("x coordinate required"))?;
    let y = args["y"]
        .as_f64()
        .ok_or_else(|| anyhow!("y coordinate required"))?;

    tracing::info!("element_at: x={}, y={}", x, y);

    let script = format!(
        r#"function __getSelector(el) {{
            if (el.id) return '#' + el.id;
            if (el.name) return el.tagName.toLowerCase() + '[name="' + el.name + '"]';
            var path = el.tagName.toLowerCase();
            var tp = el.getAttribute('type');
            if (tp) path += '[type="' + tp + '"]';
            var cls = Array.from(el.classList).filter(function(c) {{ return !c.startsWith('css-') && c.length < 30; }}).slice(0, 2);
            if (cls.length) path += '.' + cls.join('.');
            return path;
        }}
        function __getInfo(el) {{
            var rect = el.getBoundingClientRect();
            var html = el.outerHTML;
            var truncated = html.length > 500 ? html.substring(0, 500) + '...' : html;
            return {{
                tag: el.tagName.toLowerCase(),
                text: (el.textContent || '').trim().substring(0, 200),
                html: truncated,
                selector: __getSelector(el),
                rect: {{ x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) }},
                center: {{ x: Math.round(rect.x + rect.width / 2), y: Math.round(rect.y + rect.height / 2) }},
                interactive: ['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA'].indexOf(el.tagName) >= 0 ||
                    el.hasAttribute('onclick') || el.hasAttribute('tabindex') ||
                    el.getAttribute('role') === 'button'
            }};
        }}
        var hit = document.elementFromPoint({x}, {y});
        var hitInfo = hit ? __getInfo(hit) : null;
        var interactiveSelectors = 'a, button, input, select, textarea, [role=button], [onclick], [tabindex]';
        var allInteractive = document.querySelectorAll(interactiveSelectors);
        var nearby = [];
        for (var i = 0; i < allInteractive.length; i++) {{
            var el = allInteractive[i];
            var rect = el.getBoundingClientRect();
            if (rect.width === 0 || rect.height === 0) continue;
            var cx = rect.x + rect.width / 2;
            var cy = rect.y + rect.height / 2;
            var dist = Math.sqrt(Math.pow(cx - {x}, 2) + Math.pow(cy - {y}, 2));
            if (dist < 300) {{
                var info = __getInfo(el);
                info.distance = Math.round(dist);
                nearby.push(info);
            }}
        }}
        nearby.sort(function(a, b) {{ return a.distance - b.distance; }});
        return {{
            status: 'completed',
            element: hitInfo,
            nearby: nearby.slice(0, 5)
        }};"#,
        x = x,
        y = y,
    );

    match webview_execute_js(&script, Some(10000)).await {
        Ok(result) => Ok(result),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "ELEMENT_AT_ERROR",
            "message": e
        })),
    }
}

// ============================================================================
// New Tools: check_visible, click_element, upload_file
// ============================================================================

/// check_visible — instant visibility check for conditional branching
async fn tool_check_visible(args: &Value) -> Result<Value> {
    let selector = args["selector"]
        .as_str()
        .ok_or_else(|| anyhow!("selector required"))?;

    tracing::info!("check_visible: selector={}", selector);

    let sel_escaped = selector
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"");

    let script = format!(
        r#"var el = document.querySelector("{sel}");
        if (!el) return {{ status: 'completed', exists: false, visible: false, selector: "{sel}" }};
        var style = window.getComputedStyle(el);
        var rect = el.getBoundingClientRect();
        var visible = style.display !== 'none'
            && style.visibility !== 'hidden'
            && style.opacity !== '0'
            && rect.width > 0
            && rect.height > 0
            && rect.bottom > 0
            && rect.right > 0
            && rect.top < window.innerHeight
            && rect.left < window.innerWidth;
        return {{
            status: 'completed',
            exists: true,
            visible: visible,
            selector: "{sel}",
            rect: {{ x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) }},
            computed: {{ display: style.display, visibility: style.visibility, opacity: style.opacity }}
        }};"#,
        sel = sel_escaped,
    );

    match webview_execute_js(&script, Some(5000)).await {
        Ok(result) => Ok(result),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "CHECK_VISIBLE_ERROR",
            "message": e
        })),
    }
}

/// click_element — selector or text-based clicking
/// A JS *expression* that evaluates to the element `click --selector` /
/// `click --text` targets (or null). Shared by the WebView click path and the
/// Chrome engine's trusted-input click, so both pick the same element.
pub(crate) fn element_finder_js(selector: Option<&str>, text: Option<&str>) -> String {
    if let Some(sel) = selector {
        let sel_escaped = sel
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('"', "\\\"");
        format!(r#"document.querySelector("{}")"#, sel_escaped)
    } else if let Some(txt) = text {
        let txt_escaped = txt
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('"', "\\\"");
        format!(
            r#"(function() {{
                var needle = "{txt}";
                // Collect clickable candidates and rank them so an EXACT text match
                // always beats a substring match. Fixes e.g. "Log in" wrongly hitting
                // "Agent log in" (which merely *contains* the needle).
                var snap = document.evaluate(
                    '//button | //a | //input[@type="submit"] | //input[@type="button"] | //*[@role="button"] | //*[@role="link"]',
                    document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
                var best = null, bestScore = 99, bestLen = Infinity;
                for (var i = 0; i < snap.snapshotLength; i++) {{
                    var el = snap.snapshotItem(i);
                    var label = (el.innerText || el.value || el.getAttribute('aria-label') || '').trim();
                    if (!label) continue;
                    var rect = el.getBoundingClientRect();
                    var visible = rect.width > 0 && rect.height > 0;
                    var score;
                    if (label === needle) score = visible ? 0 : 1;          // exact match
                    else if (label.indexOf(needle) >= 0) score = visible ? 2 : 3; // substring
                    else continue;
                    // Lower score wins; tie-break on shortest label (closest match).
                    if (score < bestScore || (score === bestScore && label.length < bestLen)) {{
                        best = el; bestScore = score; bestLen = label.length;
                    }}
                }}
                if (best) return best;
                // Fallback: any element whose text contains the needle.
                var r = document.evaluate('//*[contains(text(), "' + needle + '")]',
                    document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null);
                return r.singleNodeValue;
            }})()"#,
            txt = txt_escaped,
        )
    } else {
        "null".to_string()
    }
}

async fn tool_click_element(args: &Value) -> Result<Value> {
    let selector = args["selector"].as_str();
    let text = args["text"].as_str();
    let click_type = args["click_type"].as_str().unwrap_or("left");

    if selector.is_none() && text.is_none() {
        return Err(anyhow!("Either 'selector' or 'text' is required"));
    }

    tracing::info!(
        "click_element: selector={:?}, text={:?}, click_type={}",
        selector,
        text,
        click_type
    );

    let find_element_js = element_finder_js(selector, text);

    let click_event_js = match click_type {
        "right" => "el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: cx, clientY: cy, button: 2 }));".to_string(),
        "double" => "el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, cancelable: true, clientX: cx, clientY: cy }));".to_string(),
        _ => "el.focus(); el.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, cancelable: true, clientX: cx, clientY: cy })); el.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, cancelable: true, clientX: cx, clientY: cy })); el.click();".to_string(),
    };

    let script = format!(
        r#"var el = {find_el};
        if (!el) return {{ status: 'failed', error: 'ELEMENT_NOT_FOUND', message: 'No element found' }};
        el.scrollIntoView({{ block: 'center', behavior: 'instant' }});
        var rect = el.getBoundingClientRect();
        var cx = rect.x + rect.width / 2;
        var cy = rect.y + rect.height / 2;
        {click_event}
        return {{
            status: 'completed',
            tag: el.tagName.toLowerCase(),
            text: (el.textContent || '').trim().substring(0, 100),
            rect: {{ x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) }},
            center: {{ x: Math.round(cx), y: Math.round(cy) }}
        }};"#,
        find_el = find_element_js,
        click_event = click_event_js,
    );

    let result = match webview_execute_js(&script, Some(10000)).await {
        Ok(result) => result,
        Err(e) => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "CLICK_ELEMENT_ERROR",
                "message": e,
            }));
        }
    };

    // Enrich ELEMENT_NOT_FOUND with a short list of visible interactive elements
    // so the agent can retry with a working selector without a full describe round-trip.
    if result["status"] == "failed" && result["error"] == "ELEMENT_NOT_FOUND" {
        let nearby = collect_nearby_interactive(8).await;
        let mut enriched = result;
        if let Some(obj) = enriched.as_object_mut() {
            obj.insert("nearby".into(), Value::Array(nearby));
            obj.insert(
                "hint".into(),
                Value::String("Retry with one of the selectors in `nearby`, or call `describe` for the full element index.".into()),
            );
        }
        return Ok(enriched);
    }

    Ok(result)
}

/// upload_file — upload a local file to <input type="file"> via DataTransfer API
async fn tool_upload_file(args: &Value) -> Result<Value> {
    let selector = args["selector"]
        .as_str()
        .ok_or_else(|| anyhow!("selector required"))?;
    let file_path = args["file_path"]
        .as_str()
        .ok_or_else(|| anyhow!("file_path required"))?;

    tracing::info!(
        "upload_file: selector={}, file_path={}",
        selector,
        file_path
    );

    // Read file from disk
    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "FILE_NOT_FOUND",
            "message": format!("File not found: {}", file_path)
        }));
    }

    let file_bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "FILE_READ_ERROR",
                "message": format!("Failed to read file: {}", e)
            }));
        }
    };

    // Size guard: 50MB max
    if file_bytes.len() > 50 * 1024 * 1024 {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "FILE_TOO_LARGE",
            "message": format!("File size {}MB exceeds 50MB limit", file_bytes.len() / 1024 / 1024)
        }));
    }

    // Base64 encode
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&file_bytes);

    // Infer MIME type from extension
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let mime_type = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };

    let sel_escaped = selector
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"");
    let filename_escaped = filename
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"");

    let script = format!(
        r#"var input = document.querySelector("{sel}");
        if (!input) return {{ status: 'failed', error: 'SELECTOR_NOT_FOUND', selector: "{sel}" }};
        if (input.tagName !== 'INPUT' || input.type !== 'file')
            return {{ status: 'failed', error: 'NOT_FILE_INPUT', tag: input.tagName, type: input.type }};
        var b64 = "{b64}";
        var byteChars = atob(b64);
        var byteArray = new Uint8Array(byteChars.length);
        for (var i = 0; i < byteChars.length; i++) byteArray[i] = byteChars.charCodeAt(i);
        var file = new File([byteArray], "{filename}", {{ type: "{mime}" }});
        var dt = new DataTransfer();
        dt.items.add(file);
        input.files = dt.files;
        input.dispatchEvent(new Event('change', {{ bubbles: true }}));
        input.dispatchEvent(new Event('input', {{ bubbles: true }}));
        return {{ status: 'completed', filename: "{filename}", size: byteArray.length, mimeType: "{mime}", selector: "{sel}" }};"#,
        sel = sel_escaped,
        b64 = b64,
        filename = filename_escaped,
        mime = mime_type,
    );

    let result = match webview_execute_js(&script, Some(30000)).await {
        Ok(result) => result,
        Err(e) => {
            return Ok(serde_json::json!({
                "status": "failed",
                "error": "UPLOAD_FILE_ERROR",
                "message": e,
            }));
        }
    };

    if result["status"] == "failed" && result["error"] == "SELECTOR_NOT_FOUND" {
        let inputs_script = r#"return JSON.stringify(Array.from(document.querySelectorAll('input[type="file"]'))
            .filter(function(el) { var r = el.getBoundingClientRect(); return r.width >= 0 && r.height >= 0; })
            .slice(0, 8)
            .map(function(el) {
                return {
                    tag: 'input',
                    type: 'file',
                    selector: el.id ? '#' + el.id : (el.name ? 'input[name="' + el.name + '"]' : 'input[type="file"]'),
                    name: el.name || '',
                    accept: el.accept || '',
                    multiple: !!el.multiple
                };
            }));"#;
        let candidates: Vec<Value> = webview_execute_js(inputs_script, Some(3000))
            .await
            .ok()
            .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or_default();
        let mut enriched = result;
        if let Some(obj) = enriched.as_object_mut() {
            obj.insert("nearby".into(), Value::Array(candidates));
            obj.insert(
                "hint".into(),
                Value::String("No file input matched. See `nearby` for visible <input type=\"file\"> elements.".into()),
            );
        }
        return Ok(enriched);
    }

    Ok(result)
}

/// save_image — save a DOM img element's content to a local file
async fn tool_save_image(args: &Value) -> Result<Value> {
    let selector = args["selector"]
        .as_str()
        .ok_or_else(|| anyhow!("selector required"))?;
    let output_path = args["output_path"]
        .as_str()
        .ok_or_else(|| anyhow!("output_path required"))?;

    tracing::info!("save_image: selector={}, output={}", selector, output_path);

    let index = args["index"].as_i64().unwrap_or(-1);
    let sel_escaped = selector.replace('\'', "\\'").replace('"', "\\\"");

    // Extract base64 data from img element (supports index for multiple matches)
    let script = format!(
        r#"
        var imgs = document.querySelectorAll("{sel}");
        if (imgs.length === 0) return JSON.stringify({{ error: 'SELECTOR_NOT_FOUND' }});
        var idx = {index};
        if (idx < 0) idx = imgs.length + idx;
        if (idx < 0 || idx >= imgs.length) return JSON.stringify({{ error: 'INDEX_OUT_OF_RANGE', count: imgs.length, index: {index} }});
        var img = imgs[idx];
        if (!img) return JSON.stringify({{ error: 'SELECTOR_NOT_FOUND' }});
        if (!img.src) return JSON.stringify({{ error: 'NO_SRC_ATTRIBUTE' }});
        if (img.src.startsWith('data:')) {{
            return JSON.stringify({{ data: img.src }});
        }}
        // For regular URLs, draw to canvas to get base64
        var canvas = document.createElement('canvas');
        canvas.width = img.naturalWidth || img.width;
        canvas.height = img.naturalHeight || img.height;
        var ctx = canvas.getContext('2d');
        try {{
            ctx.drawImage(img, 0, 0);
            return JSON.stringify({{ data: canvas.toDataURL('image/png') }});
        }} catch(e) {{
            return JSON.stringify({{ error: 'CROSS_ORIGIN: ' + e.message }});
        }}
        "#,
        sel = sel_escaped,
        index = index
    );

    match crate::platform::exec_js_parse::<serde_json::Value>(&script, Some(15000)).await {
        Ok(parsed) => {
            if let Some(error) = parsed["error"].as_str() {
                let mut response = serde_json::json!({
                    "status": "failed",
                    "error": error,
                    "message": format!("Failed to extract image: {}", error),
                });
                // Selector-miss path: surface nearby <img> candidates so the agent
                // can retry without re-running `describe`.
                if error == "SELECTOR_NOT_FOUND" {
                    let imgs_script = r#"return JSON.stringify(Array.from(document.querySelectorAll('img'))
                        .filter(function(el) { var r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; })
                        .slice(0, 8)
                        .map(function(el) {
                            var src = el.currentSrc || el.src || '';
                            return {
                                tag: 'img',
                                selector: el.id ? '#' + el.id : (el.className ? 'img.' + String(el.className).trim().split(/\s+/).slice(0,2).join('.') : 'img'),
                                src: src.length > 120 ? src.substring(0, 117) + '...' : src,
                                alt: (el.alt || '').substring(0, 60),
                                width: Math.round(el.naturalWidth || el.width),
                                height: Math.round(el.naturalHeight || el.height)
                            };
                        }));"#;
                    let candidates: Vec<Value> = webview_execute_js(imgs_script, Some(3000))
                        .await
                        .ok()
                        .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
                        .unwrap_or_default();
                    if let Some(obj) = response.as_object_mut() {
                        obj.insert("nearby".into(), Value::Array(candidates));
                        obj.insert(
                            "hint".into(),
                            Value::String(
                                "No <img> matched. See `nearby` for visible images on the page."
                                    .into(),
                            ),
                        );
                    }
                }
                return Ok(response);
            }

            if let Some(data_uri) = parsed["data"].as_str() {
                // Parse data URI: data:image/png;base64,XXXXX
                let base64_data = if let Some(pos) = data_uri.find(",") {
                    &data_uri[pos + 1..]
                } else {
                    return Ok(serde_json::json!({
                        "status": "failed",
                        "error": "INVALID_DATA_URI",
                        "message": "Could not parse data URI"
                    }));
                };

                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(base64_data) {
                    Ok(bytes) => {
                        let path = std::path::Path::new(output_path);
                        if let Some(parent) = path.parent() {
                            tokio::fs::create_dir_all(parent).await.ok();
                        }
                        match tokio::fs::write(path, &bytes).await {
                            Ok(()) => Ok(serde_json::json!({
                                "status": "completed",
                                "file": crate::state::printable_path(path),
                                "size": bytes.len(),
                                "selector": selector
                            })),
                            Err(e) => Ok(serde_json::json!({
                                "status": "failed",
                                "error": "WRITE_ERROR",
                                "message": format!("Failed to write file: {}", e)
                            })),
                        }
                    }
                    Err(e) => Ok(serde_json::json!({
                        "status": "failed",
                        "error": "BASE64_DECODE_ERROR",
                        "message": format!("Failed to decode base64: {}", e)
                    })),
                }
            } else {
                Ok(serde_json::json!({
                    "status": "failed",
                    "error": "NO_DATA",
                    "message": "No image data extracted"
                }))
            }
        }
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": "JS_EXECUTION_ERROR",
            "message": e
        })),
    }
}

/// describe — annotated screenshot with numbered interactive elements + text map.
///
/// Cross-origin iframe support: macOS (Native Bridge), Windows (CDP).
async fn tool_describe(args: &Value) -> Result<Value> {
    let site = args["site"]
        .as_str()
        .ok_or_else(|| anyhow!("site required"))?;
    let output_file = args["output"].as_str().unwrap_or("describe.png");

    tracing::info!("describe: site={}", site);

    if !is_webview_ready().await {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "WEBVIEW_NOT_FOUND"
        }));
    }

    // Step 1: Inject numbered label overlays (main + same-origin iframes)
    // Returns the count of labeled elements
    let inject_result = webview_execute_js(
        "document.querySelectorAll('[data-tw-label]').forEach(function(e){e.remove()});function getInts(doc,ox,oy){var els=Array.from(doc.querySelectorAll('button,a[href],input,textarea,select')).filter(function(el){var r=el.getBoundingClientRect();return r.width>0&&r.height>0&&(r.bottom+oy)>0&&(r.top+oy)<window.innerHeight&&(r.right+ox)>0&&(r.left+ox)<window.innerWidth&&window.getComputedStyle(el).display!=='none'&&window.getComputedStyle(el).visibility!=='hidden'});return els.map(function(el){var r=el.getBoundingClientRect();return{el:el,x:r.left+ox,y:r.top+oy,w:r.width,h:r.height,doc:doc}})}var all=getInts(document,0,0);try{var iframes=document.querySelectorAll('iframe');for(var f=0;f<iframes.length;f++){try{var fd=iframes[f].contentDocument;if(fd){var ir=iframes[f].getBoundingClientRect();all=all.concat(getInts(fd,ir.left,ir.top))}}catch(e){}}}catch(e){}all=all.slice(0,50);all.forEach(function(item,i){var lb=document.createElement('div');lb.setAttribute('data-tw-label','true');lb.style.cssText='position:fixed;z-index:999999;background:rgba(255,0,0,0.85);color:white;font-size:10px;font-weight:bold;font-family:monospace;padding:1px 3px;border-radius:2px;pointer-events:none;line-height:12px;min-width:14px;text-align:center;';lb.style.left=Math.max(0,Math.round(item.x))+'px';lb.style.top=Math.max(0,Math.round(item.y-14))+'px';lb.textContent=i.toString();document.body.appendChild(lb);var bd=document.createElement('div');bd.setAttribute('data-tw-label','true');bd.style.cssText='position:fixed;z-index:999998;border:2px solid rgba(255,0,0,0.6);pointer-events:none;border-radius:2px;';bd.style.left=Math.round(item.x)+'px';bd.style.top=Math.round(item.y)+'px';bd.style.width=Math.round(item.w)+'px';bd.style.height=Math.round(item.h)+'px';document.body.appendChild(bd)});return all.length.toString()",
        Some(10000),
    ).await;

    let same_origin_count: usize = inject_result
        .ok()
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v.as_f64().map(|n| n as usize))
        })
        .unwrap_or(0);

    // Step 1b: Cross-origin iframe elements via Native Bridge (macOS) / CDP (Windows)
    // Concurrently: get iframe bounding rects + cross-origin elements
    let (iframe_rects_result, cross_origin_result) = tokio::join!(
        webview_execute_js(
            "JSON.stringify(Array.from(document.querySelectorAll('iframe')).map(function(f){var r=f.getBoundingClientRect();return{src:f.src,x:r.left,y:r.top,w:r.width,h:r.height}}))",
            Some(5000),
        ),
        webview_describe_cross_origin_frames()
    );

    let cross_origin_frames = cross_origin_result.unwrap_or_else(|e| {
        tracing::debug!("cross-origin describe: {}", e);
        Vec::new()
    });

    // Step 1c: Inject additional overlays for cross-origin iframe elements
    let mut cross_origin_elements: Vec<serde_json::Value> = Vec::new();
    let mut cross_origin_form_values = serde_json::Map::new();

    if !cross_origin_frames.is_empty() {
        let iframe_rects: Vec<serde_json::Value> = iframe_rects_result
            .ok()
            .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or_default();

        let mut next_id = same_origin_count;

        for frame in &cross_origin_frames {
            // Match the iframe src against the frame URL to find the bounding rect offset
            let (ox, oy) = iframe_rects
                .iter()
                .find(|r| {
                    r.get("src")
                        .and_then(|s| s.as_str())
                        .map(|src| {
                            !src.is_empty()
                                && (frame.url.contains(src)
                                    || src.contains(&frame.url)
                                    || frame.url.split('?').next() == src.split('?').next())
                        })
                        .unwrap_or(false)
                })
                .map(|rect| {
                    (
                        rect["x"].as_f64().unwrap_or(0.0),
                        rect["y"].as_f64().unwrap_or(0.0),
                    )
                })
                .unwrap_or((0.0, 0.0));

            // Merge the form values of cross-origin frames
            if let Some(fv) = frame.form_values.as_object() {
                for (k, v) in fv {
                    cross_origin_form_values.insert(k.clone(), v.clone());
                }
            }

            for elem in &frame.elements {
                if next_id >= 50 {
                    break;
                }

                let abs_x = elem.x + ox;
                let abs_y = elem.y + oy;

                cross_origin_elements.push(serde_json::json!({
                    "id": next_id,
                    "tag": elem.tag,
                    "text": elem.text,
                    "x": (abs_x + elem.w / 2.0).round() as i64,
                    "y": (abs_y + elem.h / 2.0).round() as i64,
                }));

                next_id += 1;
            }
        }

        // Inject the extra overlays in one call
        if !cross_origin_elements.is_empty() {
            let overlay_data: Vec<serde_json::Value> = cross_origin_frames
                .iter()
                .flat_map(|frame| {
                    let (ox, oy) = iframe_rects
                        .iter()
                        .find(|r| {
                            r.get("src")
                                .and_then(|s| s.as_str())
                                .map(|src| {
                                    !src.is_empty()
                                        && (frame.url.contains(src)
                                            || src.contains(&frame.url)
                                            || frame.url.split('?').next() == src.split('?').next())
                                })
                                .unwrap_or(false)
                        })
                        .map(|rect| {
                            (
                                rect["x"].as_f64().unwrap_or(0.0),
                                rect["y"].as_f64().unwrap_or(0.0),
                            )
                        })
                        .unwrap_or((0.0, 0.0));

                    frame.elements.iter().map(move |elem| {
                        serde_json::json!({
                            "x": elem.x + ox,
                            "y": elem.y + oy,
                            "w": elem.w,
                            "h": elem.h,
                        })
                    })
                })
                .take(50 - same_origin_count)
                .collect();

            let overlay_json = serde_json::to_string(&overlay_data).unwrap_or_default();
            let inject_extra = format!(
                "var items={};var base={};items.forEach(function(item,i){{var idx=base+i;var lb=document.createElement('div');lb.setAttribute('data-tw-label','true');lb.style.cssText='position:fixed;z-index:999999;background:rgba(0,100,255,0.85);color:white;font-size:10px;font-weight:bold;font-family:monospace;padding:1px 3px;border-radius:2px;pointer-events:none;line-height:12px;min-width:14px;text-align:center;';lb.style.left=Math.max(0,Math.round(item.x))+'px';lb.style.top=Math.max(0,Math.round(item.y-14))+'px';lb.textContent=idx.toString();document.body.appendChild(lb);var bd=document.createElement('div');bd.setAttribute('data-tw-label','true');bd.style.cssText='position:fixed;z-index:999998;border:2px solid rgba(0,100,255,0.6);pointer-events:none;border-radius:2px;';bd.style.left=Math.round(item.x)+'px';bd.style.top=Math.round(item.y)+'px';bd.style.width=Math.round(item.w)+'px';bd.style.height=Math.round(item.h)+'px';document.body.appendChild(bd)}});return 'injected'",
                overlay_json, same_origin_count,
            );
            let _ = webview_execute_js(&inject_extra, Some(5000)).await;

            tracing::info!(
                "describe: {} cross-origin elements from {} frames",
                cross_origin_elements.len(),
                cross_origin_frames.len()
            );
        }
    }

    // Step 2: Take annotated screenshot
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let file_path = site_dir(site).await?.join(output_file);

    let capture_result = capture_page(&file_path, &None).await;

    // Step 3: Collect element data + remove overlays (main + same-origin iframes)
    let collect_result = webview_execute_js(
        "function getInts(doc,ox,oy){return Array.from(doc.querySelectorAll('button,a[href],input,textarea,select')).filter(function(el){var r=el.getBoundingClientRect();return r.width>0&&r.height>0&&(r.bottom+oy)>0&&(r.top+oy)<window.innerHeight&&(r.right+ox)>0&&(r.left+ox)<window.innerWidth&&window.getComputedStyle(el).display!=='none'&&window.getComputedStyle(el).visibility!=='hidden'}).map(function(el){var r=el.getBoundingClientRect();return{el:el,x:r.left+ox,y:r.top+oy,w:r.width,h:r.height}})}var all=getInts(document,0,0);try{var iframes=document.querySelectorAll('iframe');for(var f=0;f<iframes.length;f++){try{var fd=iframes[f].contentDocument;if(fd){var ir=iframes[f].getBoundingClientRect();all=all.concat(getInts(fd,ir.left,ir.top))}}catch(e){}}}catch(e){}var total=all.length;all=all.slice(0,50);var elems=[];all.forEach(function(item,i){var t=(item.el.innerText||(item.el.type==='password'&&item.el.value?'***':item.el.value)||item.el.getAttribute('aria-label')||'').trim().substring(0,80);elems.push({id:i,tag:item.el.tagName.toLowerCase(),text:t,x:Math.round(item.x+item.w/2),y:Math.round(item.y+item.h/2)})});document.querySelectorAll('[data-tw-label]').forEach(function(e){e.remove()});var fv={};document.querySelectorAll('input:not([type=hidden]),textarea,select').forEach(function(el){if(el.value&&el.value.length>0){fv[el.name||el.id||el.getAttribute('aria-label')||el.tagName]=(el.type==='password'&&el.value?'***':el.value).substring(0,200)}});try{var iframes=document.querySelectorAll('iframe');for(var f=0;f<iframes.length;f++){try{var fd=iframes[f].contentDocument;if(fd){fd.querySelectorAll('input:not([type=hidden]),textarea,select').forEach(function(el){if(el.value&&el.value.length>0){fv[el.name||el.id||el.getAttribute('aria-label')||el.tagName]=(el.type==='password'&&el.value?'***':el.value).substring(0,200)}})}}catch(e){}}}catch(e){}return JSON.stringify({url:location.href,title:document.title,total:total,elementCount:elems.length,elements:elems,formValues:fv})",
        Some(10000),
    ).await;

    // Step 4: Build response — merge main frame + cross-origin elements
    let metadata = tokio::fs::metadata(&file_path).await;
    let size = metadata.map(|m| m.len()).unwrap_or(0);
    let capture_ok = capture_result.is_ok();

    let mut parsed: serde_json::Value = match collect_result {
        Ok(ref val) => {
            if let Some(s) = val.as_str() {
                serde_json::from_str(s).unwrap_or(serde_json::json!({}))
            } else {
                val.clone()
            }
        }
        Err(ref e) => serde_json::json!({"_error": e.to_string()}),
    };

    // Merge cross-origin elements into the result
    if !cross_origin_elements.is_empty() {
        if let Some(elements) = parsed.get_mut("elements").and_then(|v| v.as_array_mut()) {
            elements.extend(cross_origin_elements.iter().cloned());
        }
        if let Some(fv) = parsed.get_mut("formValues").and_then(|v| v.as_object_mut()) {
            for (k, v) in &cross_origin_form_values {
                fv.insert(k.clone(), v.clone());
            }
        }
        // Update element count
        let total = parsed["elements"].as_array().map(|a| a.len()).unwrap_or(0);
        parsed["elementCount"] = serde_json::json!(total);
    }

    Ok(serde_json::json!({
        "status": if capture_ok { "completed" } else { "partial" },
        "file": crate::state::printable_path(&file_path),
        "size": size,
        "url": parsed["url"],
        "title": parsed["title"],
        "elementCount": parsed["elementCount"],
        "total": parsed["total"].as_u64().unwrap_or(0)
            + cross_origin_frames
                .iter()
                .map(|frame| frame.elements.len() as u64)
                .sum::<u64>(),
        "elements": parsed["elements"],
        "formValues": parsed["formValues"]
    }))
}

/// inject_script — register a document-start script in a chosen JS world.
pub(crate) async fn tool_inject_script(args: &Value) -> Result<Value> {
    let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_PARAMS",
            "message": "`name` is required — it is how inject_clear identifies the script.",
        }));
    };
    let Some(script) = args.get("script").and_then(|v| v.as_str()) else {
        return Ok(serde_json::json!({
            "status": "failed",
            "error": "INVALID_PARAMS",
            "message": "`script` is required.",
        }));
    };

    let world = match crate::inject::normalize_world(args.get("world").and_then(|v| v.as_str())) {
        Ok(w) => w,
        Err(message) => {
            return Ok(serde_json::json!({
                "status": "failed", "error": "INVALID_WORLD", "message": message,
            }))
        }
    };

    match crate::inject::add(name, script, world).await {
        Ok(entry) => Ok(serde_json::json!({
            "status": "completed",
            "registered": entry,
            // State the consequence of the world choice every time: a script in
            // the isolated world that expected to see page globals runs fine and
            // observes nothing, which is indistinguishable from "nothing
            // happened" unless the tool says so.
            "world_note": if world == "isolated" {
                "Runs in the isolated world: the DOM is shared but page globals are NOT visible. \
                 To wrap or watch a page API before page scripts reach it, re-register with \
                 world=\"main\". Read anything it leaves behind with `exec-js --world isolated`."
            } else {
                "Runs in the page's main world: page globals are visible and can be pre-empted. \
                 The page can also observe this script."
            },
            "applies_from": "the next navigation (document start); the currently loaded page is unaffected",
        })),
        Err(e) => Ok(serde_json::json!({
            "status": "failed",
            "error": if e.starts_with("DUPLICATE_NAME") { "DUPLICATE_NAME" } else { "INJECT_SCRIPT_ERROR" },
            "message": e,
        })),
    }
}

/// inject_list — show the registered document-start scripts.
pub(crate) async fn tool_inject_list(_args: &Value) -> Result<Value> {
    let scripts = crate::inject::list().await;
    Ok(serde_json::json!({
        "status": "completed",
        "count": scripts.len(),
        "scripts": scripts,
    }))
}

/// inject_clear — unregister document-start scripts.
pub(crate) async fn tool_inject_clear(args: &Value) -> Result<Value> {
    let name = args.get("name").and_then(|v| v.as_str());
    match crate::inject::clear(name).await {
        Ok((removed, warning)) => Ok(serde_json::json!({
            "status": "completed",
            "removed": removed,
            "removed_count": removed.len(),
            // A platform that cannot really unregister must say so — a script
            // still running after `inject-clear` would contaminate every later
            // observation while the tool claimed a clean page.
            "warning": warning,
        })),
        Err(e) => Ok(serde_json::json!({
            "status": "failed", "error": "INJECT_CLEAR_ERROR", "message": e,
        })),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    #[test]
    fn window_crop_maps_css_pixels_onto_a_retina_window_with_a_title_bar() {
        // Measured on macOS at 2x: viewport 1200x769, window image 2400x1602
        // (64 px of title bar), a 200x100 box at viewport y=500 found at
        // image y=1064 as 400x200.
        let rect = serde_json::json!({
            "x": 0, "y": 500, "width": 200, "height": 100, "vw": 1200, "vh": 769
        });
        assert_eq!(
            super::window_crop(&rect, 2400, 1602),
            Some((0, 1064, 400, 200))
        );
        // Without a title bar (full screen) the viewport starts at the top.
        assert_eq!(
            super::window_crop(&rect, 2400, 1538),
            Some((0, 1000, 400, 200))
        );
    }

    #[test]
    fn window_crop_maps_css_pixels_onto_a_scaled_webview2_capture() {
        // Measured on Windows at 125%: viewport 1440x900 CSS px, CapturePreview
        // image 1800x1125 (viewport only, no title bar). A 1280x640 element at
        // the top-left must come back as 1600x800.
        let rect = serde_json::json!({
            "x": 0, "y": 0, "width": 1280, "height": 640, "vw": 1440, "vh": 900
        });
        assert_eq!(
            super::window_crop(&rect, 1800, 1125),
            Some((0, 0, 1600, 800))
        );
        let offset = serde_json::json!({
            "x": 100, "y": 40, "width": 200, "height": 80, "vw": 1440, "vh": 900
        });
        assert_eq!(
            super::window_crop(&offset, 1800, 1125),
            Some((125, 50, 250, 100))
        );
    }

    #[test]
    fn window_crop_without_viewport_size_keeps_the_old_one_to_one_mapping() {
        let rect = serde_json::json!({ "x": 10, "y": 20, "width": 30, "height": 40 });
        assert_eq!(super::window_crop(&rect, 800, 600), Some((10, 20, 30, 40)));
        let empty = serde_json::json!({ "x": 10, "y": 20, "width": 0, "height": 40 });
        assert_eq!(super::window_crop(&empty, 800, 600), None);
    }

    use super::*;

    // --- strip_html_noise ---

    #[test]
    fn test_strip_script_tags() {
        let html = r#"<div>Hello</div><script>alert('xss')</script><p>World</p>"#;
        let result = strip_html_noise(html);
        assert!(result.contains("<div>Hello</div>"));
        assert!(result.contains("<p>World</p>"));
        assert!(!result.contains("alert"));
        assert!(!result.contains("<script>"));
    }

    #[test]
    fn test_strip_style_tags() {
        let html = r#"<div>Content</div><style>.cls { color: red; }</style>"#;
        let result = strip_html_noise(html);
        assert!(result.contains("<div>Content</div>"));
        assert!(!result.contains("<style>"));
        assert!(!result.contains("color: red"));
    }

    #[test]
    fn test_strip_html_comments() {
        let html = "<!-- comment --><div>Visible</div><!-- another -->";
        let result = strip_html_noise(html);
        assert!(result.contains("<div>Visible</div>"));
        assert!(!result.contains("comment"));
        assert!(!result.contains("another"));
    }

    #[test]
    fn test_strip_noscript() {
        let html = "<noscript>Enable JS</noscript><p>Content</p>";
        let result = strip_html_noise(html);
        assert!(result.contains("<p>Content</p>"));
        assert!(!result.contains("Enable JS"));
    }

    #[test]
    fn test_strip_svg() {
        let html =
            r#"<div>Text</div><svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0"/></svg>"#;
        let result = strip_html_noise(html);
        assert!(result.contains("<div>Text</div>"));
        assert!(!result.contains("<svg"));
        assert!(!result.contains("<path"));
    }

    #[test]
    fn test_strip_style_attributes() {
        let html = r#"<div style="color: red; font-size: 12px;">Styled</div>"#;
        let result = strip_html_noise(html);
        assert!(result.contains("Styled"));
        assert!(!result.contains("style="));
        assert!(!result.contains("color: red"));
    }

    #[test]
    fn test_strip_data_v_attributes() {
        let html = r#"<div data-v-abc123 class="content">Vue</div>"#;
        let result = strip_html_noise(html);
        assert!(result.contains("Vue"));
        assert!(!result.contains("data-v-"));
    }

    #[test]
    fn test_strip_empty_input() {
        assert_eq!(strip_html_noise(""), "");
    }

    #[test]
    fn test_strip_no_noise() {
        let html = "<div><p>Clean HTML</p></div>";
        let result = strip_html_noise(html);
        assert_eq!(result, html);
    }

    #[test]
    fn to_survives_a_runner_that_forgot_to_strip_quotes() {
        // The reported failure shape: --to matched nothing while the reported
        // current_url plainly contained it.
        assert_eq!(unquote_arg("\"/zp/p/\""), "/zp/p/");
        assert_eq!(unquote_arg("'/zp/p/'"), "/zp/p/");
        assert_eq!(unquote_arg("  /zp/p/  "), "/zp/p/");
        // A bare value is untouched, and a lone quote is not a pair.
        assert_eq!(unquote_arg("/zp/p/"), "/zp/p/");
        assert_eq!(unquote_arg("\"unbalanced"), "\"unbalanced");
        assert_eq!(unquote_arg(""), "");
        assert_eq!(unquote_arg("\""), "\"");
    }

    #[test]
    fn navigation_result_marks_a_late_match_instead_of_claiming_a_clean_one() {
        let start = std::time::Instant::now();
        let clean = navigation_result("a", "b", "complete", start, true, false);
        assert_eq!(clean["status"], "completed");
        assert_eq!(clean["navigated"], true);
        assert!(clean.get("matched_late").is_none());

        // Rescued by the final out-of-band check: the destination is confirmed,
        // but the swap itself was never witnessed — both facts reported.
        let late = navigation_result("a", "b", "", start, false, true);
        assert_eq!(late["status"], "completed");
        assert_eq!(late["navigated"], false);
        assert_eq!(late["matched_late"], true);
        assert!(late["note"].as_str().unwrap().contains("not witnessed"));
    }

    #[test]
    fn test_strip_nested_script() {
        let html = r#"<div>Before</div><script type="text/javascript">var x = 1;</script><div>After</div>"#;
        let result = strip_html_noise(html);
        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("var x"));
    }

    #[test]
    fn page_text_loses_only_empty_space() {
        assert_eq!(
            tidy_text("\n\n제목  \n\n\n\n본문\t\n  둘째 줄\n\n"),
            "제목\n\n본문\n  둘째 줄"
        );
        assert_eq!(tidy_text(""), "");
    }

    #[test]
    fn text_is_cut_between_characters() {
        assert_eq!(cut_text("가나다", 4), "가");
        assert_eq!(cut_text("가나다", 6), "가나");
        assert_eq!(cut_text("abc", 2), "ab");
        assert_eq!(cut_text("abc", 9), "abc");
    }

    #[test]
    fn strip_keeps_text_that_is_not_ascii() {
        let html = "<h1>토큰 측정</h1><p>가격 100원 — ok</p>";
        assert_eq!(strip_html_noise(html), html);
    }

    #[test]
    fn strip_removes_a_whole_attribute_and_nothing_else() {
        // An empty value ate the text up to the next quote; a value with a
        // space left its second half inside the tag.
        assert_eq!(
            strip_html_noise(r#"<td style="">가격</td><td><button class="b">구매</button></td>"#),
            r#"<td>가격</td><td><button class="b">구매</button></td>"#
        );
        assert_eq!(
            strip_html_noise(
                r#"<tr data-v-7ba data-reactid="0" style="color: red; font-size: 12px"><td>x</td></tr>"#
            ),
            "<tr><td>x</td></tr>"
        );
        assert_eq!(
            strip_html_noise(r#"<p style='a: b' id="x">t</p>"#),
            r#"<p id="x">t</p>"#
        );
        assert_eq!(
            strip_html_noise(r#"<img src="a.png" style="b"/>"#),
            r#"<img src="a.png"/>"#
        );
    }

    #[test]
    fn strip_reads_tags_and_not_text() {
        // Looks like an attribute, but it is text or part of another value.
        for html in [
            r#"<pre> style="x" </pre>"#,
            r#"<a title=' style="x"'>t</a>"#,
            r#"<a title="a > b">t</a>"#,
            "<p>1 < 2 and <styled-thing>x</styled-thing></p>",
            "<!doctype html><html></html>",
        ] {
            assert_eq!(strip_html_noise(html), html);
        }
    }

    #[test]
    fn strip_handles_case_and_unfinished_input() {
        assert_eq!(strip_html_noise("a<SCRIPT>x</Script >b"), "ab");
        // Nothing to close the block with: keep the text, do not lose the page.
        assert_eq!(strip_html_noise("a<script>x"), "a<script>x");
        assert_eq!(strip_html_noise("a<!-- x"), "a<!-- x");
        assert_eq!(strip_html_noise("<div class=\"x"), "<div class=\"x");
        assert_eq!(strip_html_noise("<div style=\"x"), "<div");
        assert_eq!(strip_html_noise("<div "), "<div ");
        assert_eq!(strip_html_noise("<"), "<");
    }

    #[test]
    fn test_focus_script_with_selector() {
        let result = focus_script(Some("input#email"));
        assert!(result.contains("input#email"));
        assert!(result.contains("focus"));
    }

    #[test]
    fn test_focus_script_none() {
        let result = focus_script(None);
        assert!(result.contains("document.activeElement"));
    }

    // --- execute_tool dispatch ---

    #[tokio::test]
    async fn test_execute_tool_unknown() {
        let result = execute_tool("nonexistent_tool", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown tool"));
    }

    /// `get_tool_definitions()` (what `dalzzack tools` advertises to MCP/LLM
    /// clients) and `execute_tool`'s match arms are maintained by hand and can
    /// silently drift — an advertised tool that dispatches to "Unknown tool" is
    /// only discovered at call time. Checked statically rather than by calling
    /// each tool, because calling them would drive a real webview.
    #[test]
    fn every_tool_definition_has_a_dispatch_arm() {
        const SRC: &str = include_str!("definitions.rs");
        let dispatch = SRC
            .split("pub async fn execute_tool")
            .nth(1)
            .expect("execute_tool not found in source");

        for def in get_tool_definitions() {
            let arm = format!("\"{}\" =>", def.name);
            assert!(
                dispatch.contains(&arm),
                "tool `{}` is advertised by get_tool_definitions() but has no execute_tool arm",
                def.name
            );
        }
    }

    #[tokio::test]
    async fn test_execute_tool_navigate_missing_url() {
        let result = execute_tool("navigate", &serde_json::json!({"site": "test"})).await;
        // Should fail because URL is required
        assert!(
            result.is_err() || {
                let val = result.unwrap();
                val["status"] == "failed" || val["error"].is_string()
            }
        );
    }

    // --- intercept / header redaction ---

    #[test]
    fn test_sensitive_header_detection() {
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("authorization"));
        assert!(is_sensitive_header("Cookie"));
        assert!(is_sensitive_header("x-api-key"));
        assert!(!is_sensitive_header("content-type"));
        assert!(!is_sensitive_header("accept"));
    }

    #[test]
    fn test_redact_keeps_scheme_and_12_char_prefix() {
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";
        let redacted = redact_header_value(&format!("Bearer {}", token));
        assert!(redacted.starts_with("Bearer eyJhbGciOiJI…"));
        assert!(redacted.contains("[REDACTED"));
        assert!(!redacted.contains("signature"));
    }

    #[test]
    fn test_redact_short_value_shows_nothing() {
        let redacted = redact_header_value("Bearer short");
        assert!(!redacted.contains("short"));
        assert!(redacted.starts_with("Bearer …[REDACTED"));
    }

    #[test]
    fn test_redact_schemeless_value() {
        let redacted = redact_header_value("sessionid=abcdef0123456789deadbeef");
        assert!(redacted.starts_with("sessionid=ab…[REDACTED"));
        assert!(!redacted.contains("deadbeef"));
    }

    #[tokio::test]
    async fn test_intercept_rejects_invalid_action() {
        let result = execute_tool(
            "intercept",
            &serde_json::json!({"site": "test", "pattern": "/api", "action": "explode"}),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["error"], "INVALID_ACTION");
    }

    #[tokio::test]
    async fn test_intercept_delay_requires_ms() {
        let result = execute_tool(
            "intercept",
            &serde_json::json!({"site": "test", "pattern": "/api", "action": "delay"}),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["error"], "MS_REQUIRED");
    }

    #[tokio::test]
    async fn test_intercept_requires_pattern() {
        let result = execute_tool(
            "intercept",
            &serde_json::json!({"site": "test", "action": "hang"}),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["error"], "PATTERN_REQUIRED");
    }
}
