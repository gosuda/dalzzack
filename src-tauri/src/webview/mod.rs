//! WebView Module — WebView access, CDP (Chrome DevTools Protocol), and high-level operations.
//!
//! ## Modules
//! - `frame_injection`: CDP-based cross-origin iframe access
//! - `operations`: WebView operation functions (HTML, forms, frames, modals)

pub mod frame_injection;
pub mod operations;

// Re-export commonly used items
pub use frame_injection::{
    ExecutionContext, ExecutionContextStore, FrameDescribeElement, FrameDescribeResult, FrameHtml,
};

// CDP functions - available on all platforms (non-Windows returns stubs)
pub use frame_injection::{
    click_in_cross_origin_iframe, describe_elements_in_all_frames, enable_runtime_domain,
    evaluate_in_context, get_all_frames_html, get_main_frame_html_via_cdp,
    keyboard_in_cross_origin_iframe, search_marker_in_all_frames, setup_cdp_for_webview,
    subscribe_execution_context_events,
};

// Re-export operations for backward compatibility
pub use operations::*;
