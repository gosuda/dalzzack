//! Per-daemon page-scope namespace name.
//!
//! Every global dalzzack needs inside the page lives under a single object
//! whose name is generated once per daemon process. The point is not
//! cryptographic hiding — a page can still enumerate `window` — but to defeat
//! the *static* detection that anti-bot and anti-debug scripts actually use
//! (`Object.keys(window).some(k => /DALZZACK|__TW/.test(k))`).
//!
//! The name is stable for the life of the process, so the daemon-side readers
//! (`tool_console_logs`, `get_captured_requests`, `intercept`) and the injected
//! scripts always agree.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

static NAMESPACE: OnceLock<String> = OnceLock::new();

/// Placeholder token used inside the injected JS templates. Replaced with
/// [`namespace()`] when a script is rendered.
pub const NS_TOKEN: &str = "NS_PLACEHOLDER";

/// The page-scope namespace identifier for this process, e.g. `_a3f19c72b4e8`.
///
/// Shaped like a minifier artifact on purpose. Guaranteed to be a valid JS
/// identifier and to contain neither `TW` nor `DALZZACK`.
pub fn namespace() -> &'static str {
    NAMESPACE.get_or_init(|| {
        let mut h = DefaultHasher::new();
        std::process::id().hash(&mut h);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .hash(&mut h);
        // Address of a static — varies with ASLR, so two daemons started in the
        // same nanosecond with the same pid still differ.
        (&NAMESPACE as *const _ as usize).hash(&mut h);
        format!("_{:012x}", h.finish() & 0xffff_ffff_ffff)
    })
}

/// Render a JS template, substituting [`NS_TOKEN`] with [`namespace()`].
pub fn render(template: &str) -> String {
    template.replace(NS_TOKEN, namespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_is_a_valid_js_identifier() {
        let ns = namespace();
        assert!(!ns.is_empty());
        let mut chars = ns.chars();
        let first = chars.next().unwrap();
        assert!(
            first == '_' || first == '$' || first.is_ascii_alphabetic(),
            "identifier must not start with a digit: {ns}"
        );
        assert!(
            ns.chars()
                .all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()),
            "identifier has invalid characters: {ns}"
        );
    }

    #[test]
    fn namespace_carries_no_detectable_brand() {
        let ns = namespace().to_ascii_uppercase();
        // The exact regexes an anti-bot script would use.
        assert!(!ns.contains("DALZZACK"), "brand leaked: {ns}");
        assert!(!ns.contains("TW"), "brand leaked: {ns}");
        assert!(!ns.contains("WEAVER"), "brand leaked: {ns}");
    }

    #[test]
    fn namespace_is_stable_within_the_process() {
        assert_eq!(namespace(), namespace());
    }

    #[test]
    fn render_substitutes_every_occurrence() {
        let out = render("window.NS_PLACEHOLDER.cr = []; window.NS_PLACEHOLDER.v = 0;");
        assert!(!out.contains(NS_TOKEN));
        assert_eq!(out.matches(namespace()).count(), 2);
    }
}
