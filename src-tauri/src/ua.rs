//! User-Agent resolution and engine-version detection.
//!
//! # Why this is not a constant
//!
//! The obvious implementation — pick a realistic UA string and hardcode it —
//! is worse than doing nothing. The engine also emits `Sec-CH-UA` and
//! `Sec-CH-UA-Full-Version-List` request headers and populates
//! `navigator.userAgentData`, and **none of those can be overridden**. A UA
//! string that disagrees with them is a stronger bot signal than an unusual UA,
//! and in practice produces an endless challenge loop.
//!
//! So the engine's real version is read at runtime and the same value is used
//! everywhere it appears.
//!
//! # Platform rules
//!
//! | | Engine | Default UA | What we do |
//! |---|---|---|---|
//! | Windows | WebView2 = Edge | already correct Edge branding at the live runtime version | **nothing** unless `--user-agent` |
//! | macOS | WKWebView = Safari | ends at `(KHTML, like Gecko)` — no product token, a plain embedded-browser signature | complete it with the installed Safari's version |
//! | Linux | WebKitGTK | — | Chrome UA (pragmatic) |
//!
//! Touching a UA that is already consistent can only break it, which is why
//! Windows is left alone by default.
//!
//! # Reduced vs full version
//!
//! Chromium ≥ 107 freezes minor/build/patch to `0.0.0` in the UA string; the
//! real build number appears only in
//! `userAgentData.getHighEntropyValues().uaFullVersion`. Leaking the full build
//! into the UA is itself anomalous, and putting the reduced form into
//! high-entropy values contradicts `Sec-CH-UA-Full-Version-List`. Both forms are
//! kept, and used in exactly one place each.

use std::sync::OnceLock;

/// Fallbacks for when engine detection fails. Only ever used to fill a UA we
/// were already going to override.
#[cfg(target_os = "windows")]
const FALLBACK_VERSION: &str = "131.0.0.0";
#[cfg(target_os = "macos")]
const FALLBACK_VERSION: &str = "17.6";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const FALLBACK_VERSION: &str = "131.0.0.0";

/// Outcome of UA resolution.
#[derive(Debug, Clone, Default)]
pub struct ResolvedUserAgent {
    /// UA to hand the WebView builder. `None` = keep the engine default.
    pub applied: Option<String>,
    /// `explicit` | `detected` | `fallback` | `engine_default` | `raw`
    ///
    /// The first thing to look at when a site blocks one machine and not
    /// another: `detected` and `fallback` are very different situations.
    pub source: &'static str,
    /// Full engine build (e.g. `149.0.4022.98`) for high-entropy client hints.
    pub full_version: Option<String>,
}

static SOURCE: OnceLock<&'static str> = OnceLock::new();

/// The `user_agent_source` reported by `__ping` / `list`.
pub fn source() -> &'static str {
    SOURCE.get().copied().unwrap_or("engine_default")
}

/// Decide the UA for this daemon.
pub fn resolve(explicit: Option<String>, raw: bool) -> ResolvedUserAgent {
    let resolved = resolve_inner(explicit, raw);
    let _ = SOURCE.set(resolved.source);
    resolved
}

fn resolve_inner(explicit: Option<String>, raw: bool) -> ResolvedUserAgent {
    let full_version = detect_full_version();

    if let Some(ua) = explicit {
        return ResolvedUserAgent {
            applied: Some(ua),
            source: "explicit",
            full_version,
        };
    }
    if raw {
        return ResolvedUserAgent {
            applied: None,
            source: "raw",
            full_version,
        };
    }

    // Windows: WebView2's own UA is Edge branding at the live runtime version,
    // so it already agrees with the Sec-CH-UA headers it generates. Overriding
    // it can only introduce drift.
    #[cfg(target_os = "windows")]
    {
        ResolvedUserAgent {
            applied: None,
            source: "engine_default",
            full_version,
        }
    }

    // macOS: the default UA has no `Version/x Safari/y` product token at all,
    // which is the WKWebView signature. Complete it at the installed Safari's
    // version. No `userAgentData` / `window.chrome` stubs — Safari has neither,
    // and inventing them would contradict the UA we just set.
    #[cfg(target_os = "macos")]
    {
        let detected = detect_engine_version();
        let source = if detected.is_some() {
            "detected"
        } else {
            "fallback"
        };
        let v = detected.unwrap_or_else(|| FALLBACK_VERSION.to_string());
        ResolvedUserAgent {
            applied: Some(format!(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                 (KHTML, like Gecko) Version/{v} Safari/605.1.15"
            )),
            source,
            full_version,
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        ResolvedUserAgent {
            applied: None,
            source: "engine_default",
            full_version,
        }
    }
}

/// `"149.0.4022.98"` → `"149.0.0.0"`.
///
/// Chromium reduces everything below the major version in the UA string.
pub fn reduce_version(full: &str) -> String {
    match full
        .split('.')
        .next()
        .map(str::trim)
        .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()))
    {
        Some(major) => format!("{major}.0.0.0"),
        None => FALLBACK_VERSION.to_string(),
    }
}

/// Build an Edge-branded UA at `full_version`'s major. Windows only uses this
/// for the consistency script; the header itself stays the engine's.
pub fn edge_user_agent(full_version: &str) -> String {
    let v = reduce_version(full_version);
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{v} Safari/537.36 Edg/{v}"
    )
}

/// The engine's full build number, cached for the process.
pub fn detect_full_version() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(detect_engine_version).clone()
}

/// Read the real engine version from the OS.
#[cfg(target_os = "windows")]
fn detect_engine_version() -> Option<String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::GetAvailableCoreWebView2BrowserVersionString;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows_core::{PCWSTR, PWSTR};

    // `GetAvailableCoreWebView2BrowserVersionString` is a standalone
    // WebView2Loader export: no COM apartment, no CoInitialize, no WebView
    // instance. Safe to call from whatever thread first touches the cache.
    //
    // Deliberately NOT falling back to the Edge/Chrome `BLBeacon` registry keys
    // the way some implementations do. The process that generates `Sec-CH-UA` is
    // the *runtime*, and the browser's registry version drifts from it; if this
    // API fails there is no WebView2 at all and the daemon has bigger problems.
    unsafe {
        let mut version: PWSTR = PWSTR::null();
        if GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version).is_err()
            || version.is_null()
        {
            return None;
        }
        let raw = version.to_string().ok();
        CoTaskMemFree(Some(version.as_ptr() as *const _));
        // Non-stable channels append a suffix, e.g. "120.0.2210.91 beta".
        raw.and_then(|v| v.split_whitespace().next().map(str::to_string))
            .filter(|v| !v.is_empty())
    }
}

/// WKWebView is always the Safari engine, so the default browser is irrelevant.
#[cfg(target_os = "macos")]
fn detect_engine_version() -> Option<String> {
    let out = std::process::Command::new("defaults")
        .args([
            "read",
            "/Applications/Safari.app/Contents/Info",
            "CFBundleShortVersionString",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn detect_engine_version() -> Option<String> {
    None
}

/// JS that makes `navigator.userAgentData` agree with an overridden UA.
///
/// Returns `None` when nothing should be injected — which is the common case
/// and the safe one. Injecting a fingerprint that contradicts the UA is worse
/// than injecting nothing:
///
/// * Windows without `--user-agent`: the engine is already self-consistent.
/// * macOS at all times: Safari has no `userAgentData` and no `window.chrome`.
///   Stubbing them while sending a Safari UA is instant contradiction.
pub fn consistency_script(ua_overridden: bool, full_version: Option<&str>) -> Option<String> {
    let _ = (ua_overridden, full_version);

    #[cfg(target_os = "windows")]
    {
        if !ua_overridden {
            return None;
        }
        // Refuse to guess. Overwriting with an invented version would create the
        // exact mismatch this module exists to avoid.
        let full = full_version?;
        let major = full.split('.').next().unwrap_or("").to_string();
        if major.is_empty() {
            return None;
        }
        Some(windows_consistency_script(&major, full))
    }

    #[cfg(target_os = "macos")]
    {
        Some(MACOS_CONSISTENCY_SCRIPT.to_string())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

#[cfg(target_os = "windows")]
fn windows_consistency_script(major: &str, full: &str) -> String {
    // Defined on `Navigator.prototype`, not the instance, so
    // `'userAgentData' in Navigator.prototype` also agrees. Silent throughout:
    // writing to the console would undo the work done in `injection.rs`.
    format!(
        r#"
(function() {{
    try {{
        var major = {major};
        var full = {full};
        var brands = [
            {{ brand: 'Microsoft Edge', version: major }},
            {{ brand: 'Chromium',       version: major }},
            {{ brand: 'Not_A Brand',    version: '24'  }}
        ];
        var data = {{
            brands: brands, mobile: false, platform: 'Windows',
            getHighEntropyValues: function() {{
                return Promise.resolve({{
                    brands: brands, mobile: false, platform: 'Windows',
                    platformVersion: '15.0.0', architecture: 'x86', bitness: '64', model: '',
                    uaFullVersion: full,
                    fullVersionList: brands.map(function(b) {{
                        return {{ brand: b.brand,
                                 version: b.brand === 'Not_A Brand' ? '24.0.0.0' : full }};
                    }})
                }});
            }},
            toJSON: function() {{ return {{ brands: brands, mobile: false, platform: 'Windows' }}; }}
        }};
        Object.defineProperty(Navigator.prototype, 'userAgentData', {{
            get: function() {{ return data; }}, configurable: true
        }});
    }} catch(e) {{}}
}})();
"#,
        major = serde_json::to_string(major).unwrap_or_else(|_| "\"\"".into()),
        full = serde_json::to_string(full).unwrap_or_else(|_| "\"\"".into()),
    )
}

/// Values that must not contradict a Safari UA.
///
/// `navigator.webdriver` is left at `false` (real Safari reports exactly that —
/// forcing `undefined` here would itself be the anomaly), and no
/// `userAgentData` / `window.chrome` is invented.
#[cfg(target_os = "macos")]
const MACOS_CONSISTENCY_SCRIPT: &str = r#"
(function() {
    try {
        // A `/Safari/` UA with no `window.safari` object reads as a spoof.
        if (typeof window.safari === 'undefined') {
            Object.defineProperty(window, 'safari', {
                get: function() { return { pushNotification: { toString: function() {
                    return '[object SafariRemoteNotification]';
                } } }; },
                configurable: true
            });
        }
        // WKWebView reports 0x0 outer dimensions; a real browser never does,
        // and it is a standard headless heuristic.
        if (window.outerWidth === 0 || window.outerHeight === 0) {
            Object.defineProperty(window, 'outerWidth', {
                get: function() { return window.innerWidth; }, configurable: true
            });
            Object.defineProperty(window, 'outerHeight', {
                get: function() { return window.innerHeight; }, configurable: true
            });
        }
    } catch(e) {}
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_version_keeps_only_the_major() {
        assert_eq!(reduce_version("149.0.4022.98"), "149.0.0.0");
        assert_eq!(reduce_version("150"), "150.0.0.0");
        assert_eq!(reduce_version("131.0.0.0"), "131.0.0.0");
    }

    #[test]
    fn reduce_version_falls_back_on_garbage() {
        assert_eq!(reduce_version(""), FALLBACK_VERSION);
        assert_eq!(reduce_version("   "), FALLBACK_VERSION);
        assert_eq!(reduce_version("beta"), FALLBACK_VERSION);
    }

    #[test]
    fn full_build_number_never_leaks_into_the_ua_string() {
        // Chromium freezes everything below the major in the UA; a full build
        // number there is itself an anomaly.
        let ua = edge_user_agent("149.0.4022.98");
        assert!(ua.contains("Chrome/149.0.0.0"), "{ua}");
        assert!(ua.contains("Edg/149.0.0.0"), "{ua}");
        assert!(!ua.contains("4022"), "full build leaked into UA: {ua}");
    }

    #[test]
    fn explicit_user_agent_wins() {
        let r = resolve_inner(Some("custom/1.0".into()), false);
        assert_eq!(r.applied.as_deref(), Some("custom/1.0"));
        assert_eq!(r.source, "explicit");
    }

    #[test]
    fn raw_keeps_the_engine_default() {
        let r = resolve_inner(None, true);
        assert!(r.applied.is_none());
        assert_eq!(r.source, "raw");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_default_leaves_a_consistent_engine_ua_alone() {
        // WebView2 already sends Edge branding at the live runtime version, so
        // it agrees with the Sec-CH-UA headers it generates. Overriding could
        // only introduce drift.
        let r = resolve_inner(None, false);
        assert!(r.applied.is_none());
        assert_eq!(r.source, "engine_default");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn no_consistency_script_without_an_override() {
        assert!(consistency_script(false, Some("149.0.4022.98")).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn consistency_script_refuses_to_guess_a_version() {
        // Better to leave the engine's native values than overwrite them with
        // something invented.
        assert!(consistency_script(true, None).is_none());
        assert!(consistency_script(true, Some("")).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn consistency_script_agrees_with_the_ua_major() {
        let s = consistency_script(true, Some("149.0.4022.98")).expect("script");
        assert!(s.contains("\"149\""), "{s}");
        assert!(s.contains("\"149.0.4022.98\""), "{s}");
        assert!(s.contains("Navigator.prototype"));
        // Silent — see injection.rs.
        assert!(!s.contains("console."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_completes_the_safari_product_token() {
        let r = resolve_inner(None, false);
        let ua = r
            .applied
            .expect("macOS always overrides the truncated default UA");
        assert!(ua.contains("Version/"), "{ua}");
        assert!(ua.ends_with("Safari/605.1.15"), "{ua}");
        assert!(matches!(r.source, "detected" | "fallback"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_never_invents_chromium_fingerprints() {
        let s = consistency_script(false, None).expect("script");
        assert!(!s.contains("userAgentData"), "Safari has none: {s}");
        assert!(!s.contains("window.chrome"), "Safari has none: {s}");
        assert!(!s.contains("console."));
    }
}
