//! Document-start script injection.
//!
//! # Why this exists
//!
//! Everything else observes the page *after* it exists. `exec-js` needs a loaded
//! document, `--record` captures network and console but not the moment a script
//! first touches an object, and the initialization script
//! ([`crate::injection`]) is fixed at window creation. So a question like "what
//! ran before my hook installed?" was **structurally unanswerable** — by the
//! time the observer arrived, the event was already history.
//!
//! This registers a script that runs at document start on every navigation,
//! before any page script.
//!
//! # Choosing a world
//!
//! | World | Sees | Use when |
//! |---|---|---|
//! | `main` | page globals **and** the DOM | you must wrap or watch an API *before* page scripts reach it |
//! | `isolated` (default) | the DOM only — **page globals are invisible** | the page must not be able to observe the instrumentation |
//!
//! The default is `isolated` because this project's instrumentation is supposed
//! to be unobservable. But an isolated world genuinely cannot see
//! `window.fetch` or any object the page created, so *pre-empting* a page API
//! requires `main`. Getting this backwards produces a script that runs and
//! silently observes nothing, so both the CLI help and the tool response say
//! which world was used.
//!
//! The isolated world name matches [`crate::platform::isolated_context_for`]
//! (`dalzzack`), so values an injected script leaves behind are readable with
//! `exec-js --world isolated`.

use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Isolated world name, shared with `exec-js --world isolated` so the two ends
/// of an investigation meet in the same world.
pub const ISOLATED_WORLD: &str = "dalzzack";

#[derive(Debug, Clone, Serialize)]
pub struct InjectedScript {
    /// Caller-supplied label, unique per daemon. Used by `inject-clear`.
    pub name: String,
    /// `main` | `isolated`
    pub world: String,
    pub source_len: usize,
    pub registered_at_ms: u64,
    /// Windows: the CDP identifier needed to remove it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    /// Kept so a script can be registered again in a browser that replaced
    /// the one it was registered in (Chrome engine `reset --keep-rules`).
    #[serde(skip)]
    pub source: String,
}

lazy_static::lazy_static! {
    static ref INJECTED: Arc<RwLock<Vec<InjectedScript>>> = Arc::new(RwLock::new(Vec::new()));
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn list() -> Vec<InjectedScript> {
    INJECTED.read().await.clone()
}

/// Validate a world name, so a typo cannot silently become "main".
pub fn normalize_world(world: Option<&str>) -> Result<&'static str, String> {
    match world.unwrap_or("isolated") {
        "isolated" => Ok("isolated"),
        "main" => Ok("main"),
        other => Err(format!(
            "unknown world '{other}' — use 'isolated' (default; DOM only, page globals invisible) \
             or 'main' (sees page globals, required to pre-empt a page API)"
        )),
    }
}

/// Register a script to run at document start on every navigation.
pub async fn add(name: &str, source: &str, world: &str) -> Result<InjectedScript, String> {
    if name.trim().is_empty() {
        return Err("INVALID_NAME: name must not be empty".to_string());
    }
    if INJECTED.read().await.iter().any(|s| s.name == name) {
        return Err(format!(
            "DUPLICATE_NAME: a script named '{name}' is already registered — \
             run `inject-clear --name {name}` first"
        ));
    }

    let platform_id = if crate::engine::is_chrome() {
        register_cdp(source, world).await?
    } else {
        register_platform(source, world).await?
    };
    let entry = InjectedScript {
        name: name.to_string(),
        world: world.to_string(),
        source_len: source.len(),
        registered_at_ms: now_ms(),
        platform_id,
        source: source.to_string(),
    };
    INJECTED.write().await.push(entry.clone());
    Ok(entry)
}

/// Remove one script by name, or all of them.
///
/// Returns `(removed, warning)`. The warning is where a platform cannot
/// actually unregister — saying so is the point, since a script that keeps
/// running after `inject-clear` would quietly contaminate every later
/// observation.
pub async fn clear(name: Option<&str>) -> Result<(Vec<String>, Option<String>), String> {
    let mut guard = INJECTED.write().await;
    let (removed, kept): (Vec<_>, Vec<_>) = guard
        .drain(..)
        .partition(|s| name.is_none_or(|n| s.name == n));
    *guard = kept;
    drop(guard);

    if removed.is_empty() {
        return Ok((Vec::new(), None));
    }

    let mut warning = None;
    for s in &removed {
        let r = if crate::engine::is_chrome() {
            unregister_cdp(s).await
        } else {
            unregister_platform(s).await
        };
        if let Err(e) = r {
            warning = Some(e);
        }
    }
    Ok((removed.into_iter().map(|s| s.name).collect(), warning))
}

// ===========================================================================
// Windows — CDP `Page.addScriptToEvaluateOnNewDocument`.
//
// This is the only primitive that gives document-start *and* a chosen world in
// one call. The WebView2 native `AddScriptToExecuteOnDocumentCreated` that
// `intercept` uses has no world parameter and is main-world only.
// ===========================================================================

/// Register every known script again, in a page session that has never seen
/// them (a Chrome restarted by `reset`). Returns one line per script.
pub async fn reregister_cdp() -> Vec<serde_json::Value> {
    let mut guard = INJECTED.write().await;
    let mut out = Vec::new();
    for s in guard.iter_mut() {
        match register_cdp(&s.source, &s.world).await {
            Ok(id) => {
                s.platform_id = id;
                out.push(serde_json::json!({ "name": s.name, "status": "registered" }));
            }
            Err(e) => {
                out.push(serde_json::json!({ "name": s.name, "status": "failed", "error": e }))
            }
        }
    }
    out
}

/// Register over CDP. Engine-neutral: WebView2's page session and the Chrome
/// engine's both answer `Page.addScriptToEvaluateOnNewDocument`.
async fn register_cdp(source: &str, world: &str) -> Result<Option<String>, String> {
    let timeout = std::time::Duration::from_secs(10);
    // Harmless if the domain is already enabled; some runtimes require it
    // before the Page.* script APIs answer.
    let _ = crate::engine::page_cdp("Page.enable", &serde_json::json!({}), timeout).await;

    // Omitting `worldName` means the main world. `runImmediately: false` keeps
    // this a document-start hook rather than also firing on the current page,
    // so "did it run at document start?" stays an honest question.
    let params = if world == "isolated" {
        serde_json::json!({
            "source": source,
            "worldName": ISOLATED_WORLD,
            "runImmediately": false,
        })
    } else {
        serde_json::json!({ "source": source, "runImmediately": false })
    };

    let v =
        crate::engine::page_cdp("Page.addScriptToEvaluateOnNewDocument", &params, timeout).await?;

    let id = crate::engine::cdp_result(&v, "identifier")
        .and_then(|x| x.as_str())
        .ok_or_else(|| {
            format!("Page.addScriptToEvaluateOnNewDocument returned no identifier: {v}")
        })?
        .to_string();
    Ok(Some(id))
}

async fn unregister_cdp(script: &InjectedScript) -> Result<(), String> {
    let Some(id) = script.platform_id.as_deref() else {
        return Ok(());
    };
    crate::engine::page_cdp(
        "Page.removeScriptToEvaluateOnNewDocument",
        &serde_json::json!({ "identifier": id }),
        std::time::Duration::from_secs(10),
    )
    .await
    .map(|_| ())
}

#[cfg(target_os = "windows")]
async fn register_platform(source: &str, world: &str) -> Result<Option<String>, String> {
    register_cdp(source, world).await
}

#[cfg(target_os = "windows")]
async fn unregister_platform(script: &InjectedScript) -> Result<(), String> {
    unregister_cdp(script).await
}

// ===========================================================================
// macOS — `WKUserScript` in a named `WKContentWorld`.
//
// The 4-argument initializer is what `intercept` is missing; it takes the
// content world, which is how an isolated world is reached here.
// ===========================================================================

#[cfg(target_os = "macos")]
async fn register_platform(source: &str, world: &str) -> Result<Option<String>, String> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;

    let window = crate::state::get_main_window()
        .await
        .ok_or("WEBVIEW_NOT_FOUND: WebView window not available. Is the daemon running?")?;

    let source_owned = source.to_string();
    let isolated = world == "isolated";
    let failure: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let failure_cb = std::sync::Arc::clone(&failure);

    window
        .with_webview(move |wv| unsafe {
            // Deliberately `msg_send!` rather than the `objc2-web-kit` bindings.
            // Those bindings only exist in a macOS build, so on the Windows
            // machine this is written on they cannot be type-checked or even
            // downloaded — and a macOS-only compile error is exactly how 0.12.0
            // shipped broken. Dynamic dispatch depends on nothing but Apple's
            // public selectors, which have been stable since macOS 11.
            let Some(world_cls) = AnyClass::get(c"WKContentWorld") else {
                *failure_cb.lock().unwrap() =
                    Some("WKContentWorld is unavailable (needs macOS 11+)".to_string());
                return;
            };
            let Some(script_cls) = AnyClass::get(c"WKUserScript") else {
                *failure_cb.lock().unwrap() = Some("WKUserScript class not found".to_string());
                return;
            };

            let content_world: *mut AnyObject = if isolated {
                let name = NSString::from_str(ISOLATED_WORLD);
                msg_send![world_cls, worldWithName: &*name]
            } else {
                msg_send![world_cls, pageWorld]
            };
            if content_world.is_null() {
                *failure_cb.lock().unwrap() = Some("could not obtain a WKContentWorld".to_string());
                return;
            }

            let src = NSString::from_str(&source_owned);
            let alloc: *mut AnyObject = msg_send![script_cls, alloc];
            // WKUserScriptInjectionTimeAtDocumentStart == 0
            let user_script: *mut AnyObject = msg_send![
                alloc,
                initWithSource: &*src,
                injectionTime: 0isize,
                forMainFrameOnly: false,
                inContentWorld: content_world,
            ];
            if user_script.is_null() {
                *failure_cb.lock().unwrap() = Some("WKUserScript init returned nil".to_string());
                return;
            }

            // Same handle `intercept` uses on macOS.
            let controller: *mut AnyObject = wv.controller().cast();
            let _: () = msg_send![controller, addUserScript: user_script];
        })
        .map_err(|e| format!("with_webview error: {e:?}"))?;

    if let Some(e) = failure.lock().unwrap().take() {
        return Err(format!("INJECT_SCRIPT_ERROR: {e}"));
    }
    Ok(None)
}

/// macOS cannot remove an individual `WKUserScript`.
///
/// `removeAllUserScripts` would also destroy the cross-origin frame responder
/// and `intercept`'s rule script, so it is not an option. The registry is
/// updated (`inject-list` stays accurate) and the caller is told plainly that
/// the script keeps running until the daemon restarts — a cleared script that
/// silently kept executing would poison every later observation.
#[cfg(target_os = "macos")]
async fn unregister_platform(script: &InjectedScript) -> Result<(), String> {
    Err(format!(
        "'{}' was removed from the registry, but macOS cannot unregister an individual \
         WKUserScript — it will keep running at document start until the daemon is restarted. \
         Run `dalzzack stop --id <ID>` and start again for a clean page.",
        script.name
    ))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn register_platform(_source: &str, _world: &str) -> Result<Option<String>, String> {
    Err(
        "UNSUPPORTED_PLATFORM: document-start injection needs CDP (Windows) or WKUserScript \
         (macOS)."
            .to_string(),
    )
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn unregister_platform(_script: &InjectedScript) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_defaults_to_isolated() {
        assert_eq!(normalize_world(None).unwrap(), "isolated");
        assert_eq!(normalize_world(Some("isolated")).unwrap(), "isolated");
        assert_eq!(normalize_world(Some("main")).unwrap(), "main");
    }

    #[test]
    fn an_unknown_world_is_rejected_not_defaulted() {
        // Silently treating a typo as "main" would inject into the wrong world
        // and the script would observe nothing, with no indication why.
        let err = normalize_world(Some("Isolated")).unwrap_err();
        assert!(err.contains("unknown world"), "{err}");
        // The message has to explain the consequence, not just list the values.
        assert!(err.contains("page globals"), "{err}");
    }

    #[test]
    fn the_isolated_world_name_matches_exec_js() {
        // If these ever drift, a script could inject successfully and still be
        // unreadable via `exec-js --world isolated`.
        assert_eq!(ISOLATED_WORLD, "dalzzack");
    }
}
