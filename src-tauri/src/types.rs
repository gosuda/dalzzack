//! Result types for WebView operations.

/// Navigation result
#[derive(Debug)]
pub struct NavigateResult {
    pub url: String,
    pub title: Option<String>,
    /// The engine reported that the main-frame load failed: the fields of
    /// [`crate::main_frame::Failure::to_json`].
    pub failure: Option<serde_json::Value>,
}

/// Wait for selector result
#[derive(Debug)]
pub struct WaitForSelectorResult {
    pub found: bool,
    pub elapsed_ms: u64,
}

/// Form fill result
#[derive(Debug)]
pub struct FillFormResult {
    pub fields_filled: usize,
    pub total_fields: usize,
    pub submitted: bool,
}

/// iframe information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrameInfo {
    pub index: usize,
    pub id: Option<String>,
    /// Truncated to 200 chars unless `--full` (SafeFrame ad slots put multi-KB
    /// JSON here); `name_len`/`name_truncated` make the cut visible.
    pub name: Option<String>,
    #[serde(rename = "nameLen", default, skip_serializing_if = "Option::is_none")]
    pub name_len: Option<usize>,
    #[serde(
        rename = "nameTruncated",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub name_truncated: Option<bool>,
    /// CDP frame id (Windows only) — the stable handle `exec-js --frame`
    /// accepts. Filled in by the tool layer, not the in-page JS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    /// `true` for a cross-site frame running in its own renderer process (its
    /// own CDP target): `exec-js --frame` and `console-logs` reach it through
    /// that target's session. Windows only, filled in by the tool layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_of_process: Option<bool>,
    /// The document the frame shows now, from its own target — the parent
    /// cannot read a cross-site frame's location, and `src` is only where it
    /// started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub src: Option<String>,
    pub origin: String,
    #[serde(rename = "canAccess")]
    pub can_access: bool,
    pub visible: bool,
    pub width: u32,
    pub height: u32,
}

/// iframe HTML result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrameHtmlResult {
    pub html: Option<String>,
    pub src: Option<String>,
    pub origin: String,
    pub id: Option<String>,
    pub name: Option<String>,
    pub is_cross_origin: bool,
    /// Access method: "contentDocument" | "native_sop_disabled" | "reqwest"
    pub method: String,
    /// Error message on access failure
    pub error: Option<String>,
}

/// CDP frame search result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CdpFrameResult {
    pub found: bool,
    pub html: Option<String>,
    pub frames: Option<Vec<CdpFrameInfo>>,
    pub method: String,
}

/// CDP frame info
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CdpFrameInfo {
    pub context_id: i64,
    pub origin: String,
    pub html: String,
    pub is_main_frame: bool,
}

/// Modal dismiss result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModalDismissResult {
    pub dismissed_count: u32,
    pub remaining_count: u32,
    pub attempts: u32,
    pub modals_found: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_info_serde() {
        let info = FrameInfo {
            index: 0,
            id: Some("frame1".to_string()),
            name: None,
            name_len: None,
            name_truncated: None,
            frame_id: None,
            out_of_process: None,
            url: None,
            src: Some("https://example.com".to_string()),
            origin: "cross-origin".to_string(),
            can_access: false,
            visible: true,
            width: 800,
            height: 600,
        };

        let json = serde_json::to_string(&info).unwrap();
        // canAccess should be serialized with camelCase rename
        assert!(json.contains("\"canAccess\""));
        assert!(!json.contains("\"can_access\""));

        let deserialized: FrameInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.index, 0);
        assert!(!deserialized.can_access);
        assert_eq!(deserialized.width, 800);
    }

    #[test]
    fn test_frame_html_result_serde() {
        let result = FrameHtmlResult {
            html: Some("<html><body>Test</body></html>".to_string()),
            src: Some("https://example.com/frame".to_string()),
            origin: "cross-origin".to_string(),
            id: None,
            name: None,
            is_cross_origin: true,
            method: "cdp".to_string(),
            error: None,
        };

        let json = serde_json::to_string(&result).unwrap();
        let deserialized: FrameHtmlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.html.unwrap(), "<html><body>Test</body></html>");
        assert!(deserialized.is_cross_origin);
        assert_eq!(deserialized.method, "cdp");
    }

    #[test]
    fn test_cdp_frame_result_serde() {
        let result = CdpFrameResult {
            found: true,
            html: Some("<div>content</div>".to_string()),
            frames: None,
            method: "cdp".to_string(),
        };

        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CdpFrameResult = serde_json::from_str(&json).unwrap();
        assert!(deserialized.found);
        assert!(deserialized.frames.is_none());
    }

    #[test]
    fn test_modal_dismiss_result_serde() {
        let result = ModalDismissResult {
            dismissed_count: 2,
            remaining_count: 0,
            attempts: 3,
            modals_found: vec![".modal .close".to_string(), ".el-dialog__close".to_string()],
        };

        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ModalDismissResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.dismissed_count, 2);
        assert_eq!(deserialized.modals_found.len(), 2);
    }
}
