//! Site data control — `clear-site-data` and hard reload.
//!
//! Both exist because the page-JS route is untrustworthy on hardened pages: a
//! proxy/membrane can hide `navigator.serviceWorker.controller` and a policy can
//! block `getRegistrations()`, so "unregister the SW from JS" silently no-ops.
//!
//! - **Windows**: CDP (`Storage.clearDataForOrigin`, `ServiceWorker.unregister`,
//!   `Page.reload {ignoreCache:true}`) — runs in the browser process, entirely
//!   outside the page's JS world, so the membrane can't interfere.
//! - **macOS**: no CDP. `WKWebsiteDataStore` / `-[WKWebView reloadFromOrigin]`
//!   are the native equivalents and likewise bypass page JS. One honest caveat:
//!   `WKWebsiteDataRecord` is keyed by *host*, not origin — scheme and port are
//!   not distinguished — so the response reports `scope: "host"`.

use anyhow::Result;
use serde_json::{json, Value};

/// Storage buckets `clear-site-data` understands. These are the CDP
/// `Storage.clearDataForOrigin` `storageTypes` names; the macOS backend maps
/// them onto `WKWebsiteDataType*`.
const KNOWN_TYPES: &[&str] = &[
    "appcache",
    "cache_storage",
    "cookies",
    "file_systems",
    "indexeddb",
    "local_storage",
    "service_workers",
    "shader_cache",
    "websql",
];

/// What `--types` defaults to: the set that actually matters for "make this
/// origin behave like a first visit".
const DEFAULT_TYPES: &str = "service_workers,cache_storage,local_storage,indexeddb,cookies";

/// Extract `scheme://host[:port]` from a URL.
///
/// Deliberately hand-rolled rather than pulling in a URL crate: we only need
/// the origin prefix, and this keeps the failure mode obvious (`None` for
/// anything that isn't an absolute hierarchical URL, e.g. `about:blank`).
pub(crate) fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    // Strip userinfo — it is not part of an origin.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if authority.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme.to_ascii_lowercase(), authority))
}

/// Host portion of an origin (`http://x.test:18080` → `x.test`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn host_of(origin: &str) -> Option<String> {
    let authority = origin.split_once("://")?.1;
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 literal: keep the brackets off but the address intact.
        stripped.split_once(']').map(|(h, _)| h)?
    } else {
        authority.split(':').next()?
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Normalize the `--types` argument into a deduped, validated list.
///
/// Returns `(accepted, unknown)`. Unknown names are reported back rather than
/// silently dropped — a typo'd type would otherwise look like a successful clear.
pub(crate) fn parse_types(raw: &str) -> (Vec<String>, Vec<String>) {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("all") {
        return (KNOWN_TYPES.iter().map(|s| s.to_string()).collect(), vec![]);
    }
    let mut accepted: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let t = part.trim().to_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        if KNOWN_TYPES.contains(&t.as_str()) {
            if !accepted.contains(&t) {
                accepted.push(t);
            }
        } else if !unknown.contains(&t) {
            unknown.push(t);
        }
    }
    (accepted, unknown)
}

/// Resolve the origin to operate on: explicit `--origin`, else the current page.
async fn resolve_origin(args: &Value) -> Result<String, Value> {
    if let Some(o) = args.get("origin").and_then(|v| v.as_str()) {
        let o = o.trim();
        if !o.is_empty() {
            return origin_of(o).map(Ok).unwrap_or_else(|| {
                Err(json!({
                    "status": "failed",
                    "error": "INVALID_ORIGIN",
                    "message": format!("`{o}` is not an absolute origin (expected scheme://host[:port])"),
                }))
            });
        }
    }

    let url = current_url().await.unwrap_or_default();
    origin_of(&url).ok_or_else(|| {
        json!({
            "status": "failed",
            "error": "ORIGIN_UNRESOLVED",
            "message": format!(
                "could not derive an origin from the current URL ({}). Pass --origin explicitly.",
                if url.is_empty() { "unavailable" } else { url.as_str() }
            ),
        })
    })
}

/// Current page URL. On Windows this goes through CDP `Target.getTargetInfo`
/// so a wedged or membrane-hardened page can't lie about (or block) it; the
/// page-JS path is only the fallback.
pub(crate) async fn current_url() -> Option<String> {
    if crate::engine::has_cdp() {
        if let Ok(v) = crate::engine::page_cdp(
            "Target.getTargetInfo",
            &json!({}),
            std::time::Duration::from_secs(10),
        )
        .await
        {
            if let Some(u) =
                crate::engine::cdp_result(&v, "targetInfo/url").and_then(|u| u.as_str())
            {
                if !u.is_empty() {
                    return Some(u.to_string());
                }
            }
        }
    }
    crate::webview_get_url().await.ok()
}

// ===========================================================================
// clear-site-data
// ===========================================================================

pub async fn clear_site_data(args: &Value) -> Result<Value> {
    let raw_types = args
        .get("types")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_TYPES);
    let (types, unknown) = parse_types(raw_types);

    if types.is_empty() {
        return Ok(json!({
            "status": "failed",
            "error": "NO_VALID_TYPES",
            "message": format!("no recognized storage types in `{raw_types}`"),
            "unknown_types": unknown,
            "known_types": KNOWN_TYPES,
        }));
    }

    let origin = match resolve_origin(args).await {
        Ok(o) => o,
        Err(failure) => return Ok(failure),
    };

    // CDP wherever there is one (WebView2, or the Chrome engine on any OS);
    // the platform data store otherwise.
    let mut result = if crate::engine::is_chrome() || cfg!(target_os = "windows") {
        cdp_clear_impl(&origin, &types).await?
    } else {
        clear_impl(&origin, &types).await?
    };
    if !unknown.is_empty() {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("unknown_types".into(), json!(unknown));
            obj.insert("known_types".into(), json!(KNOWN_TYPES));
        }
    }
    Ok(result)
}

async fn cdp_clear_impl(origin: &str, types: &[String]) -> Result<Value> {
    let timeout = std::time::Duration::from_secs(10);
    let params = json!({ "origin": origin, "storageTypes": types.join(",") });
    if let Err(e) = crate::engine::page_cdp("Storage.clearDataForOrigin", &params, timeout).await {
        return Ok(json!({
            "status": "failed",
            "error": "CLEAR_SITE_DATA_FAILED",
            "message": e,
            "origin": origin,
        }));
    }

    // `Storage.clearDataForOrigin` drops the registration *records*, but an
    // already-running service worker can survive until the origin is
    // re-navigated. `ServiceWorker.unregister` tears the live one down too.
    // Best-effort: report the outcome rather than failing the whole call.
    let sw = if types.iter().any(|t| t == "service_workers") {
        let _ = crate::engine::page_cdp("ServiceWorker.enable", &json!({}), timeout).await;
        let scope = format!("{origin}/");
        match crate::engine::page_cdp(
            "ServiceWorker.unregister",
            &json!({ "scopeURL": scope }),
            timeout,
        )
        .await
        {
            Ok(_) => json!({ "ok": true, "scope_url": scope }),
            Err(e) => json!({ "ok": false, "scope_url": scope, "message": e }),
        }
    } else {
        json!({ "ok": false, "skipped": "service_workers not in --types" })
    };

    Ok(json!({
        "status": "completed",
        "transport": "cdp",
        "origin": origin,
        "scope": "origin",
        "types_cleared": types,
        "service_worker_unregister": sw,
        "note": "Storage is cleared. Follow with `reload --hard` to drop the HTTP cache \
                 and load without a controlling service worker.",
    }))
}

#[cfg(target_os = "macos")]
async fn clear_impl(origin: &str, types: &[String]) -> Result<Value> {
    let Some(host) = host_of(origin) else {
        return Ok(json!({
            "status": "failed",
            "error": "INVALID_ORIGIN",
            "message": format!("could not extract a host from `{origin}`"),
        }));
    };

    let wk_types = macos_data_types(types);
    match macos_impl::remove_for_host(&host, &wk_types).await {
        Ok(removed) => Ok(json!({
            "status": "completed",
            "transport": "wkwebsitedatastore",
            "origin": origin,
            "scope": "host",
            "host": host,
            "types_cleared": types,
            "wk_types": wk_types,
            "records_removed": removed,
            "warning": format!(
                "macOS clears by HOST, not origin: WKWebsiteDataRecord does not distinguish \
                 scheme or port, so data for every scheme/port on `{host}` was removed, not just \
                 `{origin}`."
            ),
            "note": "Follow with `reload --hard` to drop the HTTP cache and load without a \
                     controlling service worker.",
        })),
        Err(e) => Ok(json!({
            "status": "failed",
            "error": "CLEAR_SITE_DATA_FAILED",
            "message": e,
            "origin": origin,
        })),
    }
}

/// Windows always goes through [`cdp_clear_impl`].
#[cfg(target_os = "windows")]
async fn clear_impl(origin: &str, types: &[String]) -> Result<Value> {
    cdp_clear_impl(origin, types).await
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn clear_impl(origin: &str, types: &[String]) -> Result<Value> {
    let _ = (origin, types);
    Ok(json!({
        "status": "failed",
        "error": "UNSUPPORTED_PLATFORM",
        "message": "clear-site-data needs CDP (Windows/WebView2) or WKWebsiteDataStore (macOS).",
    }))
}

/// Map our CDP-flavoured type names onto `WKWebsiteDataType*` constants.
///
/// The constants' string *values* are identical to their symbol names, so we
/// build them as plain strings and avoid linking against WebKit's globals.
#[cfg(target_os = "macos")]
fn macos_data_types(types: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for t in types {
        match t.as_str() {
            "service_workers" => push("WKWebsiteDataTypeServiceWorkerRegistrations"),
            "cache_storage" => {
                push("WKWebsiteDataTypeFetchCache");
                push("WKWebsiteDataTypeDiskCache");
                push("WKWebsiteDataTypeMemoryCache");
            }
            "local_storage" => push("WKWebsiteDataTypeLocalStorage"),
            "indexeddb" => push("WKWebsiteDataTypeIndexedDBDatabases"),
            "cookies" => push("WKWebsiteDataTypeCookies"),
            "websql" => push("WKWebsiteDataTypeWebSQLDatabases"),
            "appcache" => push("WKWebsiteDataTypeOfflineWebApplicationCache"),
            // No WebKit equivalent — CDP-only buckets. Skipped silently here;
            // the caller still sees them in `types_cleared` vs `wk_types`.
            _ => {}
        }
    }
    out
}

#[cfg(target_os = "macos")]
mod macos_impl {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::{NSArray, NSSet, NSString};
    use std::sync::mpsc;
    use std::time::Duration;

    const OP_TIMEOUT: Duration = Duration::from_secs(15);

    /// Fetch every data record of `types`, keep the ones whose display name
    /// matches `host`, and remove those.
    ///
    /// `WKWebsiteDataRecord.displayName` is the registrable domain (e.g.
    /// `example.com` for `a.b.example.com`), so we accept an exact match or a
    /// suffix match on a label boundary.
    pub(super) async fn remove_for_host(host: &str, types: &[String]) -> Result<usize, String> {
        let window = crate::get_main_window()
            .await
            .ok_or("WEBVIEW_NOT_FOUND: WebView window not available")?;

        let host_owned = host.to_string();
        let types_owned: Vec<String> = types.to_vec();
        let (tx, rx) = mpsc::channel::<Result<usize, String>>();

        window
            .with_webview(move |_wv| unsafe {
                let Some(cls) = AnyClass::get(c"WKWebsiteDataStore") else {
                    let _ = tx.send(Err("WKWebsiteDataStore class unavailable".into()));
                    return;
                };
                let store: *mut AnyObject = msg_send![cls, defaultDataStore];
                if store.is_null() {
                    let _ = tx.send(Err("defaultDataStore returned nil".into()));
                    return;
                }

                let type_strs: Vec<objc2::rc::Retained<NSString>> =
                    types_owned.iter().map(|t| NSString::from_str(t)).collect();
                let type_refs: Vec<&NSString> = type_strs.iter().map(|s| &**s).collect();
                let type_set = NSSet::from_slice(&type_refs);

                let tx_fetch = tx.clone();
                let host_for_block = host_owned.clone();

                let fetch_block = block2::StackBlock::new(move |records: *mut AnyObject| {
                    let records = records as *mut NSArray<AnyObject>;
                    if records.is_null() {
                        let _ = tx_fetch.send(Ok(0));
                        return;
                    }
                    let count: usize = msg_send![records, count];
                    let mut matched: Vec<*mut AnyObject> = Vec::new();
                    for i in 0..count {
                        let record: *mut AnyObject = msg_send![records, objectAtIndex: i];
                        if record.is_null() {
                            continue;
                        }
                        let name: *mut AnyObject = msg_send![record, displayName];
                        if name.is_null() {
                            continue;
                        }
                        let utf8: *const std::ffi::c_char = msg_send![name, UTF8String];
                        if utf8.is_null() {
                            continue;
                        }
                        let display = std::ffi::CStr::from_ptr(utf8)
                            .to_string_lossy()
                            .to_ascii_lowercase();
                        if host_matches(&host_for_block, &display) {
                            matched.push(record);
                        }
                    }

                    if matched.is_empty() {
                        let _ = tx_fetch.send(Ok(0));
                        return;
                    }

                    let removed = matched.len();
                    let match_refs: Vec<&AnyObject> = matched.iter().map(|r| &**r).collect();
                    let match_array = NSArray::from_slice(&match_refs);

                    // Rebuild the type set inside this block — the outer one is
                    // captured by the enclosing closure, which has already returned.
                    let inner_type_strs: Vec<objc2::rc::Retained<NSString>> =
                        types_owned.iter().map(|t| NSString::from_str(t)).collect();
                    let inner_type_refs: Vec<&NSString> =
                        inner_type_strs.iter().map(|s| &**s).collect();
                    let inner_type_set = NSSet::from_slice(&inner_type_refs);

                    let tx_remove = tx_fetch.clone();
                    let remove_block = block2::StackBlock::new(move || {
                        let _ = tx_remove.send(Ok(removed));
                    });
                    let remove_block = remove_block.copy();

                    // Re-fetch the singleton rather than capturing the outer
                    // pointer: the enclosing `with_webview` closure (and its
                    // autorelease scope) has already returned by the time this
                    // completion block runs.
                    let Some(cls) = AnyClass::get(c"WKWebsiteDataStore") else {
                        let _ = tx_fetch.send(Err("WKWebsiteDataStore class unavailable".into()));
                        return;
                    };
                    let store: *mut AnyObject = msg_send![cls, defaultDataStore];
                    if store.is_null() {
                        let _ = tx_fetch.send(Err("defaultDataStore returned nil".into()));
                        return;
                    }
                    let _: () = msg_send![
                        store,
                        removeDataOfTypes: &*inner_type_set,
                        forDataRecords: &*match_array,
                        completionHandler: &*remove_block,
                    ];
                });
                let fetch_block = fetch_block.copy();

                let _: () = msg_send![
                    store,
                    fetchDataRecordsOfTypes: &*type_set,
                    completionHandler: &*fetch_block,
                ];
            })
            .map_err(|e| format!("with_webview error: {e:?}"))?;

        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(format!(
                "WKWebsiteDataStore did not complete within {}s",
                OP_TIMEOUT.as_secs()
            )),
        }
    }

    /// `displayName` is the registrable domain, so `app.example.com` shows up
    /// as `example.com`. Match exactly or on a label boundary.
    fn host_matches(host: &str, display: &str) -> bool {
        host == display
            || host.ends_with(&format!(".{display}"))
            || display.ends_with(&format!(".{host}"))
    }
}

// ===========================================================================
// hard reload
// ===========================================================================

/// Reload bypassing the HTTP cache. Used by `reload --hard`.
pub async fn hard_reload() -> Result<Value> {
    if crate::engine::is_chrome() || cfg!(target_os = "windows") {
        let url = current_url().await.unwrap_or_default();
        return match crate::engine::page_cdp(
            "Page.reload",
            &json!({ "ignoreCache": true }),
            std::time::Duration::from_secs(10),
        )
        .await
        {
            Ok(_) => Ok(json!({
                "status": "completed",
                "mode": "hard",
                "transport": "cdp",
                "url": url,
            })),
            Err(e) => Ok(json!({
                "status": "failed",
                "error": "HARD_RELOAD_FAILED",
                "message": e,
            })),
        };
    }

    #[cfg(target_os = "macos")]
    {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let window = crate::get_main_window()
            .await
            .ok_or_else(|| anyhow::anyhow!("WEBVIEW_NOT_FOUND: WebView window not available"))?;
        let url = current_url().await.unwrap_or_default();

        window
            .with_webview(move |wv| unsafe {
                let webview: *mut AnyObject = wv.inner().cast();
                // `reloadFromOrigin` performs end-to-end revalidation — the
                // WebKit equivalent of Cmd-Shift-R.
                let _: *mut AnyObject = msg_send![webview, reloadFromOrigin];
            })
            .map_err(|e| anyhow::anyhow!("with_webview error: {e:?}"))?;

        Ok(json!({
            "status": "completed",
            "mode": "hard",
            "transport": "reloadFromOrigin",
            "url": url,
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(json!({
            "status": "failed",
            "error": "UNSUPPORTED_PLATFORM",
            "message": "hard reload needs CDP (Windows/WebView2, or --engine chrome) or WKWebView \
                        (macOS). Use `reload` without --hard.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_strips_path_query_and_fragment() {
        assert_eq!(
            origin_of("http://proxy.localhost:18080/a/b?c=1#d").as_deref(),
            Some("http://proxy.localhost:18080")
        );
        assert_eq!(
            origin_of("https://example.com").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            origin_of("HTTPS://Example.com/x").as_deref(),
            Some("https://Example.com")
        );
    }

    #[test]
    fn origin_drops_userinfo() {
        assert_eq!(
            origin_of("http://user:pw@host.test:8080/x").as_deref(),
            Some("http://host.test:8080")
        );
    }

    #[test]
    fn origin_rejects_non_hierarchical() {
        assert_eq!(origin_of("about:blank"), None);
        assert_eq!(origin_of(""), None);
        assert_eq!(origin_of("http://"), None);
        assert_eq!(origin_of("http:///path"), None);
    }

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("http://proxy.localhost:18080").as_deref(),
            Some("proxy.localhost")
        );
        assert_eq!(
            host_of("https://Example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:8080").as_deref(), Some("::1"));
    }

    #[test]
    fn types_default_and_all() {
        let (accepted, unknown) = parse_types(DEFAULT_TYPES);
        assert_eq!(accepted.len(), 5);
        assert!(unknown.is_empty());
        assert!(accepted.contains(&"service_workers".to_string()));

        let (all, unknown) = parse_types("ALL");
        assert_eq!(all.len(), KNOWN_TYPES.len());
        assert!(unknown.is_empty());
    }

    #[test]
    fn types_report_unknown_instead_of_dropping() {
        let (accepted, unknown) = parse_types("cookies, sevice_workers ,cookies");
        assert_eq!(accepted, vec!["cookies".to_string()]);
        assert_eq!(unknown, vec!["sevice_workers".to_string()]);
    }
}
