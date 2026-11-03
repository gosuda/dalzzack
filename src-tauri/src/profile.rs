//! Which browser profile (cookie jar, storage, cache) this daemon uses, and
//! which other live daemons share it.
//!
//! Plain WebView2 daemons all share ONE user data folder — that is what lets a
//! login made in one id carry over to the next. It also means that clearing
//! cookies or browsing data in one daemon clears them in every other daemon on
//! the machine, including ones that belong to somebody else's session. Every
//! destructive operation (`cookies clear`, `reset`) therefore checks this first
//! and refuses on a shared profile unless told otherwise.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The WebView2 user data folder every plain daemon uses.
    Shared,
    /// This daemon's own folder (`--private-profile` or `--browser-arg`).
    Private,
    /// A throw-away folder that is deleted when the daemon stops
    /// (`--engine chrome`).
    Ephemeral,
    /// WKWebView / WebKitGTK default data store — process-wide, not a folder
    /// dalzzack chose.
    Platform,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Shared => "shared",
            Kind::Private => "private",
            Kind::Ephemeral => "ephemeral",
            Kind::Platform => "platform",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub kind: Kind,
    pub path: Option<PathBuf>,
    /// Throw-away: removed when the daemon stops (`--private-profile`,
    /// `--engine chrome`). A `--browser-arg` folder is private but kept.
    pub disposable: bool,
}

/// Replaceable: the Chrome engine's `reset` moves to a brand-new profile.
static PROFILE: RwLock<Option<Profile>> = RwLock::new(None);

pub fn set(p: Profile) {
    if let Ok(mut slot) = PROFILE.write() {
        *slot = Some(p);
    }
}

/// Keep the kind, point at a new folder.
pub fn set_path(path: PathBuf) {
    if let Ok(mut slot) = PROFILE.write() {
        if let Some(p) = slot.as_mut() {
            p.path = Some(path);
        }
    }
}

pub fn current() -> Profile {
    PROFILE
        .read()
        .ok()
        .and_then(|p| p.clone())
        .unwrap_or(Profile {
            kind: Kind::Platform,
            path: None,
            disposable: false,
        })
}

pub fn to_json() -> Value {
    let p = current();
    json!({
        "kind": p.kind.as_str(),
        "path": p.path.map(|p| p.to_string_lossy().to_string()),
        "disposable": p.disposable,
    })
}

/// `stop`, from the CLI: remove a disposable WebView2 profile once the daemon
/// and its WebView2 processes have let go of it (they outlive the daemon by a
/// second or two). Only ever a folder this module names.
pub fn remove_disposable_webview_dir(profile: &Value) -> Option<bool> {
    if profile.get("disposable").and_then(|d| d.as_bool()) != Some(true)
        || profile.get("kind").and_then(|k| k.as_str()) != Some("private")
    {
        return None;
    }
    let path = PathBuf::from(profile.get("path")?.as_str()?);
    let ours = path.parent() == Some(std::env::temp_dir().as_path())
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("dalzzack-webview-"));
    if !ours {
        return Some(false);
    }
    for _ in 0..40 {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return Some(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(true),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(250)),
        }
    }
    Some(false)
}

/// Where Tauri points WebView2 when no data directory is given:
/// `%LOCALAPPDATA%\<bundle identifier>\EBWebView`.
pub fn default_webview_dir() -> Option<PathBuf> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        dirs::data_local_dir().map(|d| d.join("org.gosuda.dalzzack").join("EBWebView"))
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        None
    }
}

fn sanitized(instance_id: &str) -> String {
    instance_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Folder for a `--browser-arg` daemon's own WebView2 profile — kept per id
/// across restarts, as it always was.
pub fn private_webview_dir(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("dalzzack-webview-{}", sanitized(instance_id)))
}

/// FNV-1a — stable across builds and platforms, which `DefaultHasher` is not.
fn id_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) as u32 ^ h as u32)
}

/// Folder for a `--private-profile` daemon. Unlike [`private_webview_dir`] it
/// is wiped at start and deleted at stop, so two ids must never share it: the
/// sanitized name alone maps `a-b`, `a.b` and `a_b` (and any two Korean ids of
/// the same length) to one folder, hence the hash of the real id.
pub fn disposable_webview_dir(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "dalzzack-webview-p-{}-{}",
        sanitized(instance_id),
        id_hash(instance_id)
    ))
}

/// Present in a profile folder that may be deleted without asking: `kill`
/// bypasses the daemon and has no other way to know.
pub const DISPOSABLE_MARKER: &str = ".dalzzack-disposable";

/// `kill`: remove the id's `--private-profile` folder once its WebView2
/// processes let go. Only a folder carrying the marker is touched.
pub fn remove_disposable_after_kill(instance_id: &str) -> Option<bool> {
    let dir = disposable_webview_dir(instance_id);
    if !dir.join(DISPOSABLE_MARKER).exists() {
        return None;
    }
    remove_disposable_webview_dir(&json!({
        "kind": "private",
        "disposable": true,
        "path": dir.to_string_lossy(),
    }))
}

/// Does another daemon's `__ping` describe the same profile as ours?
///
/// Daemons from before 0.18 don't report a profile; for those the rule they
/// were built with applies — no `--browser-arg` meant the shared folder.
fn shares_with_me(me: &Profile, other: &Value) -> bool {
    if let Some(p) = other.get("profile") {
        let kind = p.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let path = p.get("path").and_then(|k| k.as_str());
        return match me.kind {
            Kind::Shared => kind == "shared",
            Kind::Private | Kind::Ephemeral => {
                path.is_some()
                    && me
                        .path
                        .as_ref()
                        .map(|m| m.to_string_lossy().to_string())
                        .as_deref()
                        == path
            }
            Kind::Platform => kind == "platform",
        };
    }
    let other_engine = other
        .get("engine")
        .and_then(|e| e.as_str())
        .unwrap_or("webview");
    let no_args = other
        .get("browser_args")
        .and_then(|a| a.as_array())
        .is_none_or(|a| a.is_empty());
    match me.kind {
        Kind::Shared => other_engine == "webview" && no_args,
        Kind::Platform => true,
        _ => false,
    }
}

/// Live daemons, other than this one, that use the same profile.
///
/// Pings run on a blocking thread — they are synchronous TCP round trips with
/// their own short timeouts.
pub async fn others_sharing() -> Vec<String> {
    let me = current();
    if me.kind == Kind::Ephemeral {
        return Vec::new();
    }
    let my_id = crate::daemon::instance_id();
    tokio::task::spawn_blocking(move || {
        crate::daemon::live_instance_ids()
            .into_iter()
            .filter(|id| Some(id.as_str()) != my_id.as_deref())
            .filter(|id| {
                crate::daemon::ping_info(id)
                    .map(|info| shares_with_me(&me, &info))
                    .unwrap_or(false)
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Must a destructive browsing-data operation be refused? `None` = go ahead.
///
/// The shared profile (and on macOS the platform's one data store) is refused
/// whether or not another daemon is up right now: it outlives this daemon and
/// every plain daemon started later — other sessions' included — uses it. 0.18
/// only refused while another daemon was live, so with one daemon running
/// `cookies clear` and `reset` wiped it unasked (reported from use). A private
/// profile is refused only if, against the odds, a live daemon shares it.
pub async fn guard(op: &str, force_shared: bool) -> Option<Value> {
    if force_shared {
        return None;
    }
    let me = current();
    if me.kind == Kind::Ephemeral {
        return None;
    }
    let others = others_sharing().await;
    (persists_for_others(me.kind) || !others.is_empty()).then(|| shared_refusal(op, &others))
}

/// Profiles every plain daemon uses, now and later.
fn persists_for_others(kind: Kind) -> bool {
    matches!(kind, Kind::Shared | Kind::Platform)
}

/// The refusal every destructive browsing-data operation returns on a profile
/// that is not this daemon's alone.
pub fn shared_refusal(op: &str, others: &[String]) -> Value {
    let live = if others.is_empty() {
        "No other daemon is running right now, but".to_string()
    } else {
        format!(
            "{} other live daemon(s) use it right now ({}), and",
            others.len(),
            others.join(", ")
        )
    };
    json!({
        "status": "failed",
        "error": "SHARED_PROFILE",
        "message": format!(
            "{op} would clear the shared browser profile (cookie jar, storage, cache). {live} \
             it outlives this daemon: every plain daemon started later — other sessions' \
             included — uses it, and what it held cannot be inspected afterwards."
        ),
        "profile": to_json(),
        "shared_with": others,
        "hint": "Pass --dry-run to see what would be deleted. Start this instance with \
                 `--private-profile` (its own fresh profile) or `--engine chrome` (a throw-away \
                 one) to work without this guard, or pass --force-shared if clearing the shared \
                 profile is really what you want.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_rules() {
        let shared = Profile {
            kind: Kind::Shared,
            path: None,
            disposable: false,
        };
        // A new daemon that says so.
        assert!(shares_with_me(
            &shared,
            &json!({ "profile": { "kind": "shared" } })
        ));
        assert!(!shares_with_me(
            &shared,
            &json!({ "profile": { "kind": "private", "path": "C:/x" } })
        ));
        // A pre-0.18 daemon: no profile field, decided by its browser args.
        assert!(shares_with_me(&shared, &json!({ "browser_args": [] })));
        assert!(!shares_with_me(
            &shared,
            &json!({ "browser_args": ["--foo"] })
        ));

        let private = Profile {
            kind: Kind::Private,
            path: Some(PathBuf::from("C:/p")),
            disposable: true,
        };
        assert!(!shares_with_me(&private, &json!({ "browser_args": [] })));
        assert!(shares_with_me(
            &private,
            &json!({ "profile": { "kind": "private", "path": "C:/p" } })
        ));
    }

    #[test]
    fn the_shared_profile_is_guarded_even_when_nobody_else_is_up() {
        assert!(persists_for_others(Kind::Shared));
        assert!(persists_for_others(Kind::Platform));
        assert!(!persists_for_others(Kind::Private));
        assert!(!persists_for_others(Kind::Ephemeral));
        // The refusal says why with no other daemon to point at.
        let r = shared_refusal("Clearing cookies", &[]);
        assert_eq!(r["error"], "SHARED_PROFILE");
        assert!(r["message"]
            .as_str()
            .unwrap()
            .contains("No other daemon is running right now"));
        assert!(r["hint"].as_str().unwrap().contains("--dry-run"));
    }
}
