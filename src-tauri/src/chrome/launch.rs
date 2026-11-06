//! Find, launch and clean up a real Google Chrome.
//!
//! The launch line is deliberately the one a person would type to get a
//! debuggable Chrome with a throw-away profile — and nothing else:
//!
//! ```text
//! chrome --user-data-dir=<tmp> --remote-debugging-port=<free port> --no-first-run
//!        --no-default-browser-check about:blank
//! ```
//!
//! No `--enable-automation` (that is what sets `navigator.webdriver` and the
//! "controlled by automated test software" bar), no `--headless`, none of the
//! dozens of feature switches automation frameworks add. Each of those is a
//! difference a bot defence can score, and the whole point of this engine is to
//! see what *Chrome* sees.
//!
//! **The port is chosen here, never `0`.** Measured on Chrome 154:
//! `--remote-debugging-port=0` turns `navigator.webdriver` to `true` before any
//! CDP client has even connected, while a fixed port leaves it `false` — through
//! attach, `Runtime`/`Network`/`Page.enable`, auto-attach and navigation. Port 0
//! would be the convenient choice (Chrome writes the port it picked to
//! `DevToolsActivePort`); it is also the one that announces automation.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where Chrome is usually installed, in the order it is looked for.
fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Ok(base) = std::env::var(var) {
                v.push(
                    PathBuf::from(base)
                        .join("Google")
                        .join("Chrome")
                        .join("Application")
                        .join("chrome.exe"),
                );
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        if let Some(home) = dirs::home_dir() {
            v.push(home.join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ] {
            if let Some(path) = std::env::var_os("PATH") {
                for dir in std::env::split_paths(&path) {
                    v.push(dir.join(name));
                }
            }
        }
    }
    v
}

/// The Chrome executable: `--chrome-path`, else `DALZZACK_CHROME`, else the
/// usual install locations.
pub fn find_chrome(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("--chrome-path {} does not exist", p.display()))
        };
    }
    if let Ok(p) = std::env::var("DALZZACK_CHROME") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("DALZZACK_CHROME={} does not exist", p.display()));
    }
    let tried = candidates();
    tried.iter().find(|p| p.is_file()).cloned().ok_or_else(|| {
        format!(
            "Google Chrome was not found (looked in: {}). Install it, or pass --chrome-path / set \
             DALZZACK_CHROME.",
            tried
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// A fresh, empty profile folder for one daemon.
pub fn new_profile_dir(instance_id: &str) -> PathBuf {
    let safe: String = instance_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!(
        "dalzzack-chrome-{safe}-{}",
        crate::recording::now_ms()
    ))
}

pub struct Launched {
    pub child: Child,
    pub port: u16,
    /// `/devtools/browser/<uuid>`
    pub ws_path: String,
}

/// A port nothing is listening on right now. (Not `0` — see the module docs.)
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot find a free local port: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("cannot read the free port: {e}"))
}

/// `GET /json/version` → the browser's WebSocket path, or `None` while Chrome
/// is not listening yet. Plain HTTP over a socket: this runs on a blocking
/// thread and needs nothing more. Chrome keeps the connection open whatever
/// the request says, so the body is read by `Content-Length`, not to EOF.
fn browser_ws_path(port: u16) -> Option<String> {
    use std::io::{Read, Write};
    let addr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let body = loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            break None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if body.len() >= len {
                break Some(body.to_string());
            }
        }
    }?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let url = v.get("webSocketDebuggerUrl")?.as_str()?;
    // ws://127.0.0.1:<port>/devtools/browser/<uuid> → the path
    url.find("/devtools/").map(|i| url[i..].to_string())
}

/// Start Chrome and wait until its DevTools endpoint answers.
pub fn launch(
    exe: &Path,
    profile_dir: &Path,
    width: Option<u32>,
    height: Option<u32>,
    extra_args: &[String],
) -> Result<Launched, String> {
    std::fs::create_dir_all(profile_dir).map_err(|e| {
        format!(
            "cannot create profile folder {}: {e}",
            profile_dir.display()
        )
    })?;
    let port = free_port()?;

    let mut args: Vec<String> = vec![
        format!("--user-data-dir={}", profile_dir.display()),
        format!("--remote-debugging-port={port}"),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
    ];
    if width.is_some() || height.is_some() {
        args.push(format!(
            "--window-size={},{}",
            width.unwrap_or(1200),
            height.unwrap_or(800)
        ));
    }
    args.extend(extra_args.iter().cloned());
    args.push("about:blank".into());
    tracing::info!("[chrome] launching {} {}", exe.display(), args.join(" "));

    let mut child = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(ws_path) = browser_ws_path(port) {
            return Ok(Launched {
                child,
                port,
                ws_path,
            });
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "Chrome exited before it opened its DevTools port (exit {:?}). Chrome refuses \
                 remote debugging on its default profile, but this launch uses a fresh one — check \
                 that {} runs at all, and that no policy disables remote debugging \
                 (RemoteDebuggingAllowed).",
                status.code(),
                exe.display()
            ));
        }
        if Instant::now() >= deadline {
            kill_tree(child.id());
            return Err(format!(
                "Chrome's DevTools endpoint on port {port} did not answer within 20 s"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kill Chrome and every process it started. Chrome is a process *tree*
/// (renderers, GPU, utility); killing only the browser process leaves the rest
/// holding the profile folder open.
pub fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(300));
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stderr(Stdio::null())
            .status();
    }
}

/// Delete a throw-away profile. Chrome's children can hold files for a moment
/// after the browser process exits, so retry briefly before giving up.
pub fn remove_profile(dir: &Path) -> Result<(), String> {
    // Only ever delete what this module created.
    let is_ours = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("dalzzack-chrome-"));
    if !is_ours {
        return Err(format!(
            "refusing to delete {}: not a dalzzack Chrome profile",
            dir.display()
        ));
    }
    let mut last = String::new();
    for _ in 0..25 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("could not delete {}: {last}", dir.display()))
}

/// The record a daemon leaves so `kill` — which bypasses the daemon entirely —
/// can still take Chrome and its profile down with it.
pub fn record_path(instance_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("dalzzack-{instance_id}.chrome.json"))
}

/// Something that identifies one process *instance*, not just its pid: the
/// creation time on Windows, the start time `ps` reports elsewhere. A pid
/// alone is reused within seconds, and `taskkill /T` on a reused one would
/// take down somebody else's process tree.
pub fn process_identity(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            // A process that was just killed (or that something still holds a
            // handle to) opens fine but is not running: measured, `list` right
            // after a kill saw the dead daemon as alive and skipped the reap.
            let mut code = 0u32;
            let running = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user).is_ok();
            let _ = CloseHandle(h);
            (ok && running).then(|| {
                (((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64).to_string()
            })
        }
    }
    #[cfg(unix)]
    {
        // The text must match across processes, so it cannot depend on the
        // locale or the time zone of the one that asks.
        // By absolute path: a `ps` that cannot run would read as "no process".
        let out = Command::new("/bin/ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        None
    }
}

pub fn write_record(instance_id: &str, pid: u32, profile_dir: &Path) {
    let daemon_pid = std::process::id();
    let _ = std::fs::write(
        record_path(instance_id),
        serde_json::json!({
            "pid": pid,
            "identity": process_identity(pid),
            "profile_dir": profile_dir,
            // Who owns this Chrome, so `list` can tell an orphan (daemon gone,
            // its port and pid files already cleaned up) from a daemon that is
            // still starting.
            "daemon_pid": daemon_pid,
            "daemon_identity": process_identity(daemon_pid),
        })
        .to_string(),
    );
}

/// Is the daemon that wrote this id's record gone? `false` when it is still
/// running or the record cannot say (no record, or no daemon identity in it).
pub fn record_owner_gone(instance_id: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(record_path(instance_id)) else {
        return false;
    };
    let Ok(rec) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let pid = rec.get("daemon_pid").and_then(|p| p.as_u64());
    let identity = rec.get("daemon_identity").and_then(|i| i.as_str());
    match (pid, identity) {
        (Some(pid), Some(identity)) => process_identity(pid as u32).as_deref() != Some(identity),
        _ => false,
    }
}

/// Ids that have a Chrome record in the temp folder.
pub fn recorded_ids() -> Vec<String> {
    std::fs::read_dir(std::env::temp_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_prefix("dalzzack-")?
                        .strip_suffix(".chrome.json")
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `kill` on a Chrome-engine id: end the browser tree and delete its profile.
/// Returns what was done, or `None` if the id never ran Chrome.
///
/// The record can be days old (a daemon that crashed never removes it), so the
/// pid is only killed if it is still the same process that was recorded.
pub fn reap(instance_id: &str) -> Option<serde_json::Value> {
    let path = record_path(instance_id);
    let text = std::fs::read_to_string(&path).ok()?;
    let rec: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pid = rec.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32);
    let recorded = rec
        .get("identity")
        .and_then(|i| i.as_str())
        .map(String::from);
    let killed = match (pid, recorded) {
        (Some(pid), Some(id)) if process_identity(pid).as_deref() == Some(id.as_str()) => {
            kill_tree(pid);
            true
        }
        // No identity, or a different process now owns the pid: never kill.
        _ => false,
    };
    let removed = rec
        .get("profile_dir")
        .and_then(|d| d.as_str())
        .map(|d| remove_profile(Path::new(d)));
    let _ = std::fs::remove_file(&path);
    Some(serde_json::json!({
        "browser_pid": pid,
        "browser_killed": killed,
        "profile_removed": removed.as_ref().map(|r| r.is_ok()),
        "profile_error": removed.and_then(|r| r.err()),
    }))
}
