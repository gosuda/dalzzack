//! Daemon server — runs inside the Tauri process, listens on a TCP port.

use super::{
    daemon_process, pid_file_path, pid_file_text, port_file_path, read_port, DaemonProcess,
    DaemonRequest, DaemonResponse,
};
use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Everything `start` can configure about a daemon.
///
/// None of it can be changed after the WebView exists, which is why `start`
/// against a live id reports the flags it had to ignore instead of pretending
/// they applied.
#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub record: bool,
    pub watch_hang: bool,
    pub auto_dump_dir: Option<String>,
    pub browser_args: Vec<String>,
    /// Skip console capture entirely — no CDP subscription, no WebKit system
    /// logging, no in-page hook. The escape hatch for sites that treat any
    /// console instrumentation as a debugger.
    pub no_console_capture: bool,
    /// macOS: opt in to console capture, accepting that the page can observe it.
    /// macOS has no invisible transport (see [`arm_console_capture`]), so capture
    /// is off there unless this is set. Ignored on Windows, whose CDP transport is
    /// already out-of-band and on by default.
    pub console_capture: bool,
    /// Explicit User-Agent override.
    pub user_agent: Option<String>,
    /// Keep the engine's own User-Agent even where the default is a giveaway
    /// (WKWebView's UA has no browser product token).
    pub raw_user_agent: bool,
    /// Leave the page's Content Security Policy enforced.
    ///
    /// dalzzack bypasses CSP by default so cross-origin iframes can be
    /// inspected. The cost is that the browser stops being a witness for
    /// anything policy-related: nothing is ever blocked, so nothing is ever
    /// reported. Turn this on when the question *is* the policy.
    pub enforce_csp: bool,
    /// Give this daemon its own WebView2 profile — empty at start, deleted by
    /// `stop` — so its cookies, storage and `reset` never touch any other daemon.
    pub private_profile: bool,
    /// Which browser to drive.
    pub engine: crate::engine::Engine,
    /// Was `--engine` given explicitly? Only then can a live daemon on the
    /// other engine be said to have ignored it.
    pub engine_given: bool,
    /// `--engine chrome`: explicit path to the Chrome executable.
    pub chrome_path: Option<String>,
    /// Console ring size (`--console-buffer`); `None` = the default 500.
    pub console_buffer: Option<usize>,
    /// Stop by itself after this many seconds (`--ttl`).
    pub ttl_secs: Option<u64>,
    /// `--trust-cert`: absolute paths of PEM files whose certificates this
    /// WKWebView instance trusts on top of the system roots (macOS only).
    pub trust_certs: Vec<String>,
}

/// `start --ttl`: `(seconds, epoch ms it expires at)`, set once at startup.
static TTL: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();

/// Parse a `--ttl` value: `90`, `90s`, `10m`, `2h`, or a sum such as `1h30m`.
pub fn parse_ttl(raw: &str) -> Result<u64, String> {
    let s = raw.trim().to_ascii_lowercase();
    let bad = || format!("invalid --ttl '{raw}' — use seconds (90), or 90s, 10m, 2h, 1h30m");
    if s.is_empty() {
        return Err(bad());
    }
    if let Ok(n) = s.parse::<u64>() {
        return if n == 0 { Err(bad()) } else { Ok(n) };
    }
    let (mut total, mut num) = (0u64, String::new());
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => return Err(bad()),
        };
        let n: u64 = num.parse().map_err(|_| bad())?;
        total = total.saturating_add(n.saturating_mul(unit));
        num.clear();
    }
    if !num.is_empty() || total == 0 {
        return Err(bad());
    }
    Ok(total)
}

/// Stop this daemon `secs` from now, the way `stop` would.
///
/// The stop runs as a separate `dalzzack stop` process: that is the path
/// which waits for this process to exit and then deletes a `--private-profile`
/// folder, and the Chrome engine's own shutdown is reached through it too.
/// Should that not end the process, it exits by itself (ending a recorded
/// Chrome first).
pub(crate) fn arm_ttl(instance_id: &str, secs: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let _ = TTL.set((secs, now + secs * 1000));
    let id = instance_id.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        tracing::info!("[ttl] {secs}s elapsed — stopping '{id}'");
        let spawned = std::env::current_exe().ok().and_then(|exe| {
            let mut cmd = Command::new(exe);
            cmd.args(["stop", "--id", &id])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            cmd.spawn().ok()
        });
        std::thread::sleep(std::time::Duration::from_secs(if spawned.is_some() {
            30
        } else {
            0
        }));
        tracing::warn!("[ttl] '{id}' still running after the TTL stop — exiting");
        if crate::engine::is_chrome() {
            let _ = crate::chrome::launch::reap(&id);
        }
        let _ = std::fs::remove_file(port_file_path(&id));
        let _ = std::fs::remove_file(pid_file_path(&id));
        std::process::exit(0);
    });
}

/// `(ttl seconds, expires at epoch ms)` when `--ttl` was given.
pub(crate) fn ttl() -> Option<(u64, u64)> {
    TTL.get().copied()
}

/// When any client last ran a tool on this daemon, and which one — so `list`
/// can tell an abandoned instance from one somebody is in the middle of using.
static LAST_USED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_TOOL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static STARTED_AT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_used(tool: &str) {
    LAST_USED_MS.store(
        crate::recording::now_ms(),
        std::sync::atomic::Ordering::Relaxed,
    );
    if let Ok(mut t) = LAST_TOOL.lock() {
        *t = tool.to_string();
    }
}

fn last_used() -> (Option<u64>, Option<String>) {
    let ms = LAST_USED_MS.load(std::sync::atomic::Ordering::Relaxed);
    if ms == 0 {
        return (None, None);
    }
    (Some(ms), LAST_TOOL.lock().ok().map(|t| t.clone()))
}

/// Start-time configuration of the running daemon, so `__ping` can report
/// which flags are actually in effect. Set once in [`run_daemon`].
#[derive(Clone, Default)]
pub struct DaemonConfig {
    pub watch_hang: bool,
    pub browser_args: Vec<String>,
    /// The UA actually handed to the WebView; `None` means the engine's own.
    /// `--no-console-capture` and `--raw-user-agent` are not stored: they are
    /// already visible as `console_transport: "disabled"` and
    /// `user_agent_source: "raw"`, and a second copy could disagree.
    pub user_agent: Option<String>,
}

static DAEMON_CONFIG: std::sync::OnceLock<DaemonConfig> = std::sync::OnceLock::new();

fn daemon_config() -> DaemonConfig {
    DAEMON_CONFIG.get().cloned().unwrap_or_default()
}

/// For engines with their own startup (Chrome): record the flags `__ping`
/// reports, so `start` against a live id compares against the truth.
pub(crate) fn set_daemon_config(watch_hang: bool, browser_args: Vec<String>) {
    let _ = DAEMON_CONFIG.set(DaemonConfig {
        watch_hang,
        browser_args,
        user_agent: None,
    });
}

/// Result of the startup renderer health check: `None` until it has run.
static RENDERER_HEALTH: std::sync::Mutex<Option<Result<(), String>>> = std::sync::Mutex::new(None);

/// Run one trivial script on the daemon's own landing page to prove the webview
/// can execute JS at all.
///
/// A webview can fail to attach while the window still exists — Tauri logs the
/// wry error and carries on — and every tool then times out as if the *page*
/// were wedged. That is indistinguishable from a real hang unless something
/// establishes a baseline before any page is loaded. This is that baseline.
async fn check_renderer_health() {
    const PROBE_TIMEOUT_MS: u32 = 8000;
    let result = match crate::webview_execute_js("return 1", Some(PROBE_TIMEOUT_MS)).await {
        Ok(_) => {
            tracing::info!("[health] renderer executes JS");
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                "[health] renderer CANNOT execute JS: {} — the webview is broken, not the page",
                e
            );
            Err(e)
        }
    };
    if let Ok(mut slot) = RENDERER_HEALTH.lock() {
        *slot = Some(result);
    }
}

/// Record the startup health check from an engine that runs its own (Chrome).
pub(crate) fn set_renderer_health(result: Result<(), String>) {
    if let Ok(mut slot) = RENDERER_HEALTH.lock() {
        *slot = Some(result);
    }
}

/// `(renderer_ok, renderer_error)` — `renderer_ok` is null while the check is
/// still pending, so "not yet known" never reads as "healthy".
fn renderer_health() -> (Option<bool>, Option<String>) {
    match RENDERER_HEALTH.lock() {
        Ok(slot) => match slot.as_ref() {
            Some(Ok(())) => (Some(true), None),
            Some(Err(e)) => (Some(false), Some(e.clone())),
            None => (None, None),
        },
        Err(_) => (None, Some("health state lock poisoned".into())),
    }
}

/// Ping a daemon by sending `__ping` and return the state it reports, or
/// `None` if it doesn't answer within 2 s.
///
/// Catches the "kernel-accepts-but-app-is-stuck" case where a bare
/// `TcpStream::connect` succeeds even though the daemon can't process commands.
fn ping_daemon_info(port: u16) -> Option<serde_json::Value> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().ok()?,
        Duration::from_millis(500),
    )
    .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(b"{\"tool\":\"__ping\",\"args\":{}}\n")
        .ok()?;
    stream.flush().ok()?;
    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let resp: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    resp.get("result").cloned()
}

/// Try to kill a possibly-stuck daemon process by PID. Sends SIGTERM first
/// (so any cleanup hooks fire), waits briefly, then SIGKILL if still alive.
/// No-op on platforms without `kill`.
pub(super) fn force_kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        // SIGTERM first so any cleanup hooks can run; SIGKILL after a short
        // grace period in case it ignored TERM. Swallow stderr — if the second
        // kill fires after the process is already gone, "No such process"
        // would otherwise leak to the user.
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(500));
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            // taskkill reports in the console code page; on stdout it would land
            // in front of the JSON a runner is about to parse.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Start a daemon by spawning a background process with the internal `daemon` subcommand.
/// `Ok` is the JSON to print on stdout; `Err` is a startup failure the CLI
/// prints on stderr with exit code 1.
pub fn start_daemon(
    instance_id: &str,
    opts: DaemonOptions,
) -> Result<serde_json::Value, serde_json::Value> {
    let DaemonOptions {
        width,
        height,
        record,
        watch_hang,
        auto_dump_dir,
        browser_args,
        no_console_capture,
        console_capture,
        user_agent,
        raw_user_agent,
        enforce_csp,
        private_profile,
        engine,
        engine_given,
        chrome_path,
        console_buffer,
        ttl_secs,
        trust_certs,
    } = opts;
    let port_file = port_file_path(instance_id);
    let pid_file = pid_file_path(instance_id);

    // Detect a previously-spawned daemon for this id and decide what to do.
    // The old check used a bare TcpStream::connect, which lies: the kernel
    // completes the 3-way handshake even when the app is wedged on a previous
    // tool call. Ping with a 2 s read timeout instead, and force-kill the old
    // PID if it doesn't answer.
    if port_file.exists() {
        if let Some(port) = read_port(instance_id) {
            if let Some(info) = ping_daemon_info(port) {
                // A live daemon keeps its ORIGINAL start flags — none of these
                // can be applied after WebView creation. Silently returning
                // `already_running` is how a session ends up debugging with
                // `--record` it never actually got.
                let live_record = info
                    .get("record")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let live_watch = info
                    .get("watch_hang")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let live_args: Vec<String> = info
                    .get("browser_args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                let live_no_console = info
                    .get("console_transport")
                    .and_then(|v| v.as_str())
                    .map(|t| t == "disabled")
                    .unwrap_or(false);
                let live_in_page = info
                    .get("console_transport")
                    .and_then(|v| v.as_str())
                    .map(|t| t == "in_page_fallback")
                    .unwrap_or(false);
                let live_ua = info
                    .get("user_agent")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let mut ignored: Vec<&str> = Vec::new();
                if record && !live_record {
                    ignored.push("--record");
                }
                if watch_hang && !live_watch {
                    ignored.push("--watch-hang");
                }
                if !browser_args.is_empty() && browser_args != live_args {
                    ignored.push("--browser-arg");
                }
                if no_console_capture && !live_no_console {
                    ignored.push("--no-console-capture");
                }
                // The in-page hook is a document-start script, so this can only
                // ever have applied at the live daemon's own WebView creation.
                if console_capture && !live_in_page {
                    ignored.push("--console-capture");
                }
                if user_agent.is_some() && user_agent != live_ua {
                    ignored.push("--user-agent");
                }
                if raw_user_agent && live_ua.is_some() {
                    ignored.push("--raw-user-agent");
                }
                let live_csp_bypassed = info
                    .get("csp_bypassed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if enforce_csp && live_csp_bypassed {
                    ignored.push("--enforce-csp");
                }
                if width.is_some() || height.is_some() {
                    ignored.push("--width/--height");
                }
                // Pre-0.18 daemons don't report an engine; they are all webview.
                let live_engine = info
                    .get("engine")
                    .and_then(|v| v.as_str())
                    .unwrap_or("webview");
                // Only a flag that was given can have been ignored: `--engine`
                // defaults to webview, and a plain `start` against a live
                // Chrome id asked for nothing different.
                if engine_given && live_engine != engine.as_str() {
                    ignored.push("--engine");
                }
                let live_private = info
                    .pointer("/profile/kind")
                    .and_then(|v| v.as_str())
                    .is_some_and(|k| k != "shared");
                if private_profile && !live_private {
                    ignored.push("--private-profile");
                }
                if ttl_secs.is_some() && info.get("ttl_s").and_then(|v| v.as_u64()) != ttl_secs {
                    ignored.push("--ttl");
                }
                if console_buffer.is_some()
                    && info.get("console_buffer").and_then(|v| v.as_u64())
                        != console_buffer.map(|n| crate::console::clamp_capacity(n) as u64)
                {
                    ignored.push("--console-buffer");
                }
                if let Some(p) = chrome_path.as_deref() {
                    let live_exe = info.get("chrome_path").and_then(|v| v.as_str());
                    let same = live_exe.is_some_and(|l| {
                        std::fs::canonicalize(l).ok() == std::fs::canonicalize(p).ok()
                    });
                    if !same {
                        ignored.push("--chrome-path");
                    }
                }

                if !trust_certs.is_empty()
                    && crate::trust_cert::hashes_of(&trust_certs)
                        != crate::trust_cert::reported_hashes(&info)
                {
                    ignored.push("--trust-cert");
                }

                let hint = if ignored.is_empty() {
                    None
                } else {
                    Some(format!(
                        "This daemon is already running and keeps the flags it was started with; \
                         {} had no effect. Run `dalzzack stop --id {}` and start again to apply \
                         them.",
                        ignored.join(", "),
                        instance_id
                    ))
                };

                return Ok(serde_json::json!({
                    "status": "already_running",
                    "id": instance_id,
                    "port": port,
                    "version": info.get("version"),
                    "record": live_record,
                    "recording_armed": info.get("recording_armed"),
                    "recording_arm_error": info.get("recording_arm_error"),
                    "watch_hang": live_watch,
                    "browser_args": live_args,
                    "renderer_ok": info.get("renderer_ok"),
                    "renderer_error": info.get("renderer_error"),
                    "console_transport": info.get("console_transport"),
                    "user_agent": live_ua,
                    "user_agent_source": info.get("user_agent_source"),
                    "engine": live_engine,
                    "profile": info.get("profile"),
                    "trusted_certs": info.get("trusted_certs"),
                    "ignored_flags": ignored,
                    "hint": hint,
                }));
            }
            tracing::warn!(
                "[start] Daemon for '{}' on port {} is unresponsive — killing and respawning",
                instance_id,
                port
            );
            if let DaemonProcess::Running { pid, .. } = daemon_process(instance_id) {
                force_kill_pid(pid);
            }
            // A Chrome-engine daemon killed this way leaves Chrome behind,
            // and the new daemon would overwrite its record.
            let _ = crate::chrome::launch::reap(instance_id);
        }
        let _ = std::fs::remove_file(&port_file);
        let _ = std::fs::remove_file(&pid_file);
    }

    // Get our own binary path
    let exe = std::env::current_exe().expect("Failed to get current executable path");

    // Build daemon command args
    let mut args: Vec<String> = vec![
        "daemon".to_string(),
        "--id".to_string(),
        instance_id.to_string(),
    ];
    if let Some(w) = width {
        args.push("--width".to_string());
        args.push(w.to_string());
    }
    if let Some(h) = height {
        args.push("--height".to_string());
        args.push(h.to_string());
    }
    if record {
        args.push("--record".to_string());
    }
    if watch_hang {
        args.push("--watch-hang".to_string());
    }
    if let Some(dir) = auto_dump_dir.as_ref() {
        args.push("--auto-dump-dir".to_string());
        args.push(dir.clone());
    }
    for arg in &browser_args {
        args.push("--browser-arg".to_string());
        args.push(arg.clone());
    }
    if no_console_capture {
        args.push("--no-console-capture".to_string());
    }
    if console_capture {
        args.push("--console-capture".to_string());
    }
    if let Some(ua) = user_agent.as_ref() {
        args.push("--user-agent".to_string());
        args.push(ua.clone());
    }
    if raw_user_agent {
        args.push("--raw-user-agent".to_string());
    }
    if enforce_csp {
        args.push("--enforce-csp".to_string());
    }
    if private_profile {
        args.push("--private-profile".to_string());
    }
    if engine != crate::engine::Engine::WebView {
        args.push("--engine".to_string());
        args.push(engine.as_str().to_string());
    }
    if let Some(p) = chrome_path.as_ref() {
        args.push("--chrome-path".to_string());
        args.push(p.clone());
    }
    if let Some(n) = console_buffer {
        args.push("--console-buffer".to_string());
        args.push(n.to_string());
    }
    if let Some(s) = ttl_secs {
        args.push("--ttl-secs".to_string());
        args.push(s.to_string());
    }
    for path in &trust_certs {
        args.push("--trust-cert".to_string());
        args.push(path.clone());
    }

    // Windows: CreateProcessW(bInheritHandles=TRUE) duplicates EVERY inheritable
    // parent handle into the child, not just the three set via Stdio below. The
    // CLI's own stdout — the caller's shell pipe — went along for the ride, so
    // `dalzzack start … | cat` never saw EOF until the daemon died (measured:
    // the CLI exits in ~2s; killing the daemon alone, WebView2 children still
    // alive, closed the pipe 4s later). Clear the inherit flag on our std
    // handles before spawning — this changes nothing about our own writes, only
    // what children may inherit. rust-lang/rust#38227.
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::Foundation::{SetHandleInformation, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
        use windows::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if let Ok(h) = GetStdHandle(std_handle) {
                if !h.is_invalid() {
                    let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
                }
            }
        }
    }

    // Spawn the daemon process (intentionally not waited — it's a background daemon)
    let mut child = Command::new(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn daemon process");

    let pid = child.id();
    let error_file = startup_error_path(instance_id);
    let _ = std::fs::remove_file(&error_file);

    // Wait briefly for port file to appear — or for the daemon to die trying.
    // A daemon that cannot start (no Chrome installed, say) used to leave the
    // caller staring at a 10-second "no port file" with no reason given.
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(if engine == crate::engine::Engine::Chrome {
        30
    } else {
        10
    });
    loop {
        if port_file.exists() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let reason = std::fs::read_to_string(&error_file).ok();
            let _ = std::fs::remove_file(&error_file);
            return Err(serde_json::json!({
                "status": "failed",
                "error": "DAEMON_EXITED",
                "id": instance_id,
                "engine": engine.as_str(),
                "exit_code": status.code(),
                "message": reason.unwrap_or_else(|| {
                    "the daemon process exited before it was ready; see its log".to_string()
                }),
                "log_dir": crate::get_log_path().to_string_lossy().to_string(),
            }));
        }
        if start.elapsed() > timeout {
            // A daemon whose webview failed to attach still starts, but every
            // CDP setup step burns its own timeout first, so it binds the port
            // long after this deadline. Don't leave the caller thinking nothing
            // happened — the process is coming up, and it may be broken.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Err(serde_json::json!({
                "status": "failed",
                "error": format!(
                    "Daemon did not create port file within {} seconds",
                    timeout.as_secs()
                ),
                "id": instance_id,
                "hint": format!(
                    "The daemon process may still be starting — a webview that fails to \
                     attach makes startup this slow. Run `dalzzack list` and check \
                     `renderer_ok` for id '{instance_id}': if it is false, the webview is \
                     broken (tool timeouts are the daemon's, not the page's){}.",
                    if browser_args.is_empty() {
                        String::new()
                    } else {
                        format!(" — suspect --browser-arg ({}) first", browser_args.join(" "))
                    }
                ),
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Detach: avoid zombie process by spawning a thread to wait
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let port = read_port(instance_id).unwrap_or(0);

    // The daemon runs its own JS baseline check before binding the port, so by
    // the time we can ping it the answer is ready. Report it here rather than
    // handing back a "started" that only means "a process exists".
    let live = ping_daemon_info(port).unwrap_or(serde_json::Value::Null);
    let field = |k: &str| live.get(k).cloned().unwrap_or(serde_json::Value::Null);
    let (renderer_ok, renderer_error) = (field("renderer_ok"), field("renderer_error"));
    let hint = if renderer_ok == serde_json::Value::Bool(false) {
        Some(if browser_args.is_empty() {
            "The webview cannot execute JavaScript. Every tool will time out as if the page were \
             wedged — it is not. Check the daemon log for a webview creation error."
                .to_string()
        } else {
            format!(
                "The webview cannot execute JavaScript, and this daemon was started with \
                 --browser-arg ({}). That is the first thing to suspect: restart without it to \
                 confirm. Tool timeouts from here are the daemon's, not the page's.",
                browser_args.join(" ")
            )
        })
    } else {
        None
    };

    Ok(serde_json::json!({
        "status": "started",
        "id": instance_id,
        "pid": pid,
        "port": port,
        "renderer_ok": renderer_ok,
        "renderer_error": renderer_error,
        "engine": field("engine"),
        "profile": field("profile"),
        "browser_version": field("browser_version"),
        "navigator_webdriver": field("navigator_webdriver"),
        "ttl_s": field("ttl_s"),
        "expires_at_ms": field("expires_at_ms"),
        "console_buffer": field("console_buffer"),
        "trusted_certs": field("trusted_certs"),
        "hint": hint,
    }))
}

/// Where a daemon that fails during startup leaves its reason for `start`.
pub(crate) fn startup_error_path(instance_id: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("dalzzack-{instance_id}.start-error"))
}

/// Run the daemon process (called by the internal `daemon` subcommand).
/// This starts the Tauri WebView + TCP server.
pub fn run_daemon(instance_id: String, opts: DaemonOptions) {
    crate::daemon::set_instance_id(&instance_id);
    STARTED_AT_MS.store(
        crate::recording::now_ms(),
        std::sync::atomic::Ordering::Relaxed,
    );
    crate::engine::set(opts.engine);
    // Both engines, before anything is captured or anyone can connect.
    if let Some(n) = opts.console_buffer {
        crate::console::set_capacity(n);
    }
    if let Some(secs) = opts.ttl_secs {
        arm_ttl(&instance_id, secs);
    }

    if opts.engine == crate::engine::Engine::Chrome {
        crate::setup_panic_hook();
        let _log_guard = crate::init_file_logging(Some(&instance_id));
        crate::chrome::run_daemon(instance_id, opts);
        return;
    }

    let DaemonOptions {
        width,
        height,
        record,
        watch_hang,
        auto_dump_dir,
        browser_args,
        no_console_capture,
        console_capture,
        user_agent,
        raw_user_agent,
        enforce_csp,
        private_profile,
        engine: _,
        engine_given: _,
        chrome_path: _,
        console_buffer: _,
        ttl_secs: _,
        trust_certs,
    } = opts;

    // Must be set before `setup_cdp` runs — it decides Page.setBypassCSP.
    crate::state::set_enforce_csp(enforce_csp);

    // Setup logging and panic hook
    crate::setup_panic_hook();
    let _log_guard = crate::init_file_logging(Some(&instance_id));

    // `start` checked these files already. One that changed since then stops
    // the daemon with the reason, which `start` prints.
    let mut anchors = Vec::new();
    for path in &trust_certs {
        match crate::trust_cert::read(std::path::Path::new(path)) {
            Ok(mut found) => anchors.append(&mut found),
            Err(message) => {
                tracing::error!("[trust-cert] {message}");
                let _ = std::fs::write(startup_error_path(&instance_id), &message);
                std::process::exit(1);
            }
        }
    }
    crate::trust_cert::set_anchors(anchors);
    #[cfg(target_os = "macos")]
    if let Err(message) = crate::platform::wk_delegate::load_anchors() {
        tracing::error!("[trust-cert] {message}");
        let _ = std::fs::write(startup_error_path(&instance_id), &message);
        std::process::exit(1);
    }

    // Whose cookie jar this is. WebView2 permits one set of browser arguments
    // per user data folder, so `--browser-arg` has always implied a folder of
    // its own; `--private-profile` asks for one outright, and starts it empty.
    let data_directory = if cfg!(target_os = "windows") {
        if private_profile || !browser_args.is_empty() {
            let dir = if private_profile {
                crate::profile::disposable_webview_dir(&instance_id)
            } else {
                crate::profile::private_webview_dir(&instance_id)
            };
            if private_profile {
                // Fresh at every start: a "private" profile that silently kept
                // yesterday's cookies would defeat the point.
                if dir.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        tracing::warn!("[profile] could not wipe {}: {}", dir.display(), e);
                    }
                }
                // How `kill`, which never talks to the daemon, knows this
                // folder may be deleted.
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join(crate::profile::DISPOSABLE_MARKER), b"");
            }
            crate::profile::set(crate::profile::Profile {
                kind: crate::profile::Kind::Private,
                path: Some(dir.clone()),
                // `--private-profile` folders are throw-away (wiped at the next
                // start anyway) and `stop` removes them; a `--browser-arg`
                // folder is kept per id, as it always was.
                disposable: private_profile,
            });
            Some(dir)
        } else {
            crate::profile::set(crate::profile::Profile {
                kind: crate::profile::Kind::Shared,
                path: crate::profile::default_webview_dir(),
                disposable: false,
            });
            None
        }
    } else {
        crate::profile::set(crate::profile::Profile {
            kind: crate::profile::Kind::Platform,
            path: None,
            disposable: false,
        });
        None
    };

    tracing::info!(
        "=== dalzzack Daemon Starting (id: {}, size: {:?}x{:?}, record: {}) ===",
        instance_id,
        width,
        height,
        record
    );

    // Resolve the User-Agent before the WebView is built — it is a construction
    // parameter, not something that can be set later.
    let ua = crate::ua::resolve(user_agent.clone(), raw_user_agent);
    tracing::info!(
        "[ua] source={} value={:?}",
        ua.source,
        ua.applied.as_deref().unwrap_or("<engine default>")
    );

    let _ = DAEMON_CONFIG.set(DaemonConfig {
        watch_hang,
        browser_args: browser_args.clone(),
        user_agent: ua.applied.clone(),
    });

    if no_console_capture {
        crate::console::set_transport(crate::console::Transport::Disabled);
    }

    if record {
        // Pre-arm the buffer NOW so the very first events of the first page
        // load make it in. Transport gets a platform-honest label.
        #[cfg(target_os = "windows")]
        crate::recording::enable("cdp");
        #[cfg(target_os = "macos")]
        crate::recording::enable("in_page_poll");
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        crate::recording::enable("unsupported");
    }

    let id_for_socket = instance_id.clone();

    // Spawn the TCP server in a background thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        rt.block_on(async {
            // Wait for the window itself, not a fixed sleep. Window creation is
            // async; a 500 ms guess loses the race on a cold/slow start and then
            // every boot-time arming step below silently no-ops (that was the
            // `dump-recording` always-empty bug).
            if crate::wait_for_main_window(tokio::time::Duration::from_secs(20))
                .await
                .is_none()
            {
                tracing::error!(
                    "[Daemon] WebView window never appeared within 20s — CDP, recording and \
                     sw-console arming will all be unavailable"
                );
            }

            // Setup CDP
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                if let Err(e) = crate::setup_cdp().await {
                    tracing::warn!("[Daemon] CDP setup failed: {}", e);
                } else {
                    tracing::info!("[Daemon] CDP setup completed");
                }
            }

            // Arm console capture AFTER CDP is up but BEFORE the TCP server
            // accepts the first navigate, so the first page's own startup logs
            // are captured. Unlike `--record` this is on by default: console
            // output is table stakes, and the whole point of this transport is
            // that collecting it no longer costs an in-page hook the site can
            // see. `--record` upgrades the same subscription to also carry
            // Network events into the recording tape.
            arm_console_capture(no_console_capture, console_capture, record).await;

            // Baseline health check BEFORE the TCP server accepts anything, so
            // the very first `list`/`start` answer already knows whether this
            // daemon can run JS at all.
            check_renderer_health().await;

            // Arm service worker console capture. Independent of `--record`:
            // a SW starts with the first navigation and its startup logs are
            // usually the interesting ones, so attaching lazily on first
            // `sw-console` read would miss them. Failure is recorded and
            // surfaced by the tool itself, never fatal to startup.
            crate::sw_console::arm().await;

            // Watchdog. Independent of `--record` — without recording the hang
            // dump will only contain the marker file, but the *detection* still
            // works. Tying them together would force users to pay the recording
            // cost (cheap, but real) for hang alerts alone.
            if watch_hang {
                let dir = auto_dump_dir
                    .clone()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(crate::recording::watchdog::default_dir);
                let id_for_watchdog = id_for_socket.clone();
                tokio::spawn(async move {
                    crate::recording::watchdog::run(dir, id_for_watchdog).await;
                });
            }

            // Start TCP server
            if let Err(e) = run_tcp_server(&id_for_socket).await {
                tracing::error!("[Daemon] TCP server error: {}", e);
            }
        });
    });

    // Run the Tauri GUI (blocks until window closes)
    crate::run_with_instance_id(crate::app::AppOptions {
        instance_id: Some(instance_id),
        width,
        height,
        browser_args,
        user_agent: ua.applied,
        console_fallback: console_fallback_wanted(no_console_capture, console_capture),
        ua_full_version: ua.full_version,
        data_directory,
    });
}

/// Bring up the console transport, degrading loudly rather than silently.
///
/// The order of preference is fixed by how observable each option is:
/// out-of-band transports do not touch `console.log` at all, so they are always
/// tried first. The in-page fallback is a last resort *and is reported as one* —
/// a caller must be able to tell that the instrumentation became visible again.
async fn arm_console_capture(no_console_capture: bool, console_capture: bool, record: bool) {
    use crate::console::Transport;

    // Only the macOS branch consults this; the signature stays uniform.
    #[cfg(not(target_os = "macos"))]
    let _ = console_capture;

    if no_console_capture {
        crate::console::set_transport(Transport::Disabled);
        tracing::info!("[console] capture disabled by --no-console-capture");
        if record {
            tracing::warn!(
                "[console] --record was also given; the recording tape will carry network \
                 events but no console events"
            );
        }
        // Network and frame events still flow: nothing about them is visible
        // to page script, and without them `response-headers`, `nav-log` and
        // `failed-requests` would be empty on exactly the sites that needed
        // the quietest instrumentation.
        #[cfg(target_os = "windows")]
        {
            use crate::recording::cdp::Mode;
            if let Err(e) = crate::recording::cdp::enable_and_subscribe(Mode::NetworkOnly).await {
                tracing::warn!("[netlog] network capture unavailable ({e})");
            } else if record {
                crate::recording::mark_armed(&crate::recording::cdp::events_for(false));
            }
        }
        return;
    }

    #[cfg(target_os = "windows")]
    {
        use crate::recording::cdp::Mode;
        let mode = if record { Mode::Full } else { Mode::Console };
        match crate::recording::cdp::enable_and_subscribe(mode).await {
            Ok(()) => {
                crate::console::set_transport(Transport::Cdp);
                tracing::info!("[console] transport: cdp (Runtime.consoleAPICalled)");
            }
            Err(e) => {
                // No retroactive fallback: the in-page hook has to be a
                // document-start script and the WebView already exists. Report
                // the hole rather than let an empty buffer read as a quiet page.
                crate::console::set_transport_error(Transport::Unavailable, &e);
                tracing::warn!("[console] CDP transport unavailable ({e})");
            }
        }
    }

    // macOS has no invisible console transport, so there is nothing to try here.
    //
    // WebKit's `logsPageMessagesToSystemConsoleEnabled` looked like one — it never
    // wraps `console.log` — but turning it on makes WebKit stringify every logged
    // argument a *second* time (once for the console message, once to print it).
    // `disable-devtool`'s DateToString/FuncToString detectors count `toString`
    // calls on an object they hand to `console.log`: one is a normal browser, two
    // means "DevTools is open". Measured on macOS 26.3.1 — 1 call with the setting
    // off (same as Safari), 2 with it on. So the setting reproduces the very bug
    // that removing the in-page hook was meant to fix.
    //
    // It bought nothing either: `WTFLogAlways` writes to the sandboxed WebContent
    // process's stderr, which is discarded — the messages never reach the unified
    // log, so `log stream` had nothing to read no matter which pid it filtered on.
    //
    // That leaves the in-page hook, which the page *can* see. It is therefore
    // opt-in, and reported as `in_page_fallback` so the visibility is on record.
    #[cfg(target_os = "macos")]
    {
        if console_capture {
            crate::console::set_transport(Transport::InPageFallback);
            tracing::warn!(
                "[console] transport: in_page_fallback (--console-capture) — this hook is \
                 visible to the page; anti-debug sites can detect it"
            );
        } else {
            crate::console::set_transport_error(
                Transport::Unavailable,
                "macOS has no console transport the page cannot observe: WebKit's \
                 system-console setting doubles the `toString` calls on logged arguments, \
                 which anti-debug libraries read as an open DevTools, and its output never \
                 reaches the unified log anyway. Restart with `--console-capture` to accept \
                 a page-visible in-page hook.",
            );
            tracing::info!(
                "[console] no transport armed on macOS (use --console-capture to opt in)"
            );
        }
        if record {
            crate::recording::poll::ensure_running();
        }
    }

    // Platforms with no out-of-band transport get the in-page hook, which was
    // already injected at WebView creation (see `console_fallback_wanted`).
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = record;
        crate::console::set_transport(Transport::InPageFallback);
    }
}

/// Does this platform need the detectable in-page console hook?
///
/// Decided before the WebView is built, because the hook must be a
/// document-start script. Windows (CDP) and macOS (WebKit system console) have
/// out-of-band transports and never want it.
pub fn console_fallback_wanted(no_console_capture: bool, console_capture: bool) -> bool {
    if no_console_capture {
        return false;
    }
    // macOS: only on explicit opt-in — the hook is visible to the page, and there
    // is no invisible alternative to fall back *from* (see `arm_console_capture`).
    if cfg!(target_os = "macos") {
        return console_capture;
    }
    // Windows has CDP, which is out-of-band and always preferred.
    !cfg!(target_os = "windows")
}

/// Run the TCP server that accepts tool commands.
pub(crate) async fn run_tcp_server(instance_id: &str) -> anyhow::Result<()> {
    let port_file = port_file_path(instance_id);
    let pid_file = pid_file_path(instance_id);

    // Remove stale port/pid files if exist
    if port_file.exists() {
        std::fs::remove_file(&port_file)?;
    }
    if pid_file.exists() {
        let _ = std::fs::remove_file(&pid_file);
    }

    // Write our own PID so `start --id` can force-kill us if we go unresponsive.
    let _ = std::fs::write(&pid_file, pid_file_text(std::process::id()));

    // Bind to a random available port on localhost
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    // Write port to file so clients can discover it
    std::fs::write(&port_file, port.to_string())?;

    tracing::info!(
        "[Daemon] TCP server listening on 127.0.0.1:{} (port file: {:?})",
        port,
        port_file
    );

    // Accept loop: spawn each connection in its own task so that a slow or
    // hung tool call NEVER blocks the next request. This is what makes
    // __ping / __shutdown stay responsive even when navigate is wedged.
    loop {
        let (stream, _addr) = listener.accept().await?;
        let instance_id_owned = instance_id.to_string();
        let port_file_owned = port_file.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, instance_id_owned, port_file_owned).await {
                tracing::error!("[Daemon] connection error: {}", e);
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    instance_id: String,
    port_file: std::path::PathBuf,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return Ok(()),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("[Daemon] Read error: {}", e);
            return Ok(());
        }
    }
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    let request: DaemonRequest = match serde_json::from_str(line) {
        Ok(req) => req,
        Err(e) => {
            let err_resp = DaemonResponse {
                ok: false,
                result: serde_json::json!({ "error": format!("Invalid request: {}", e) }),
            };
            let resp_str = serde_json::to_string(&err_resp)?;
            writer.write_all(resp_str.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            return Ok(());
        }
    };

    if request.tool == "__shutdown" {
        tracing::info!("[Daemon] Shutdown requested");
        // Chrome is a separate process tree with a throw-away profile; leaving
        // either behind would turn every `stop` into litter. That cleanup takes
        // seconds, so it happens BEFORE the answer — `stop && start` must not
        // find this daemon still answering pings — and the port file goes
        // first, so nothing new connects meanwhile.
        let mut result = serde_json::json!({ "message": "Shutting down" });
        if crate::engine::is_chrome() {
            let _ = std::fs::remove_file(&port_file);
            result["chrome"] = crate::chrome::shutdown().await;
        }
        let resp = DaemonResponse { ok: true, result };
        let resp_str = serde_json::to_string(&resp)?;
        writer.write_all(resp_str.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let _ = std::fs::remove_file(&port_file);
        let _ = std::fs::remove_file(pid_file_path(&instance_id));
        std::process::exit(0);
    }

    if request.tool == "__ping" {
        // Report the live daemon's identity and capture state. `start` uses
        // this to tell the caller which of its flags an already-running daemon
        // is ignoring, and it makes a stale binary visible immediately.
        let (renderer_ok, renderer_error) = renderer_health();
        let (last_used_ms, last_tool) = last_used();
        // Daemon memory only — `list` pings every instance, and a ping that
        // waited on a wedged renderer would get the daemon declared dead.
        let (current_url, current_url_ms) = crate::netlog::current_url();
        let started = STARTED_AT_MS.load(std::sync::atomic::Ordering::Relaxed);
        let resp = DaemonResponse {
            ok: true,
            result: serde_json::json!({
                "engine": crate::engine::current().as_str(),
                "profile": crate::profile::to_json(),
                "browser_version": crate::chrome::browser_version(),
                // Chrome engine only: what the page reads from
                // navigator.webdriver. `true` = this browser announces automation.
                "navigator_webdriver": crate::chrome::webdriver_flag(),
                "chrome_path": crate::chrome::exe_path(),
                "current_url": current_url,
                "current_url_ms": current_url_ms,
                "last_used_ms": last_used_ms,
                "last_tool": last_tool,
                "started_at_ms": if started == 0 { None } else { Some(started) },
                // `start --ttl`: when this daemon stops itself. Null = never.
                "ttl_s": ttl().map(|(s, _)| s),
                "expires_at_ms": ttl().map(|(_, at)| at),
                "console_buffer": crate::console::capacity(),
                // The tape's on/off state in one word. `recording_armed` is
                // only whether the capture transport came up — it stays true
                // after `record off`, which is what made it misleading here.
                "recording": crate::recording::state_word(),
                "alive": true,
                "id": instance_id,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "renderer_ok": renderer_ok,
                "renderer_error": renderer_error,
                "record": crate::recording::is_enabled(),
                "recording_armed": crate::recording::is_armed(),
                "recording_arm_error": crate::recording::arm_error(),
                "browser_args": daemon_config().browser_args,
                "watch_hang": daemon_config().watch_hang,
                // Which console transport actually came up. `unavailable` means
                // the buffer is empty because capture is broken, not because the
                // page was quiet — the two are worth very different amounts.
                "console_transport": crate::console::transport().as_str(),
                "console_transport_error": crate::console::transport_error(),
                "user_agent": daemon_config().user_agent,
                "user_agent_source": crate::ua::source(),
                // With CSP bypassed nothing is ever blocked, so `--source csp`
                // sits at zero no matter what the page's policy says. Reporting
                // it is the difference between "no violations" and "no
                // enforcement".
                "csp_bypassed": crate::state::csp_bypassed(),
                // `start --trust-cert`: what this WKWebView trusts on top of
                // the system roots, so a test report can name it.
                "trusted_certs": crate::trust_cert::report(),
                // What sees a main-frame load that failed: the CDP network
                // stream, the WKWebView delegate, or nothing.
                "load_failure_capture": crate::main_frame::capture_word(),
            }),
        };
        let resp_str = serde_json::to_string(&resp)?;
        writer.write_all(resp_str.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        return Ok(());
    }

    tracing::info!("[Daemon] Executing tool: {}", request.tool);
    note_used(&request.tool);

    // Auto-inject "site" from instance_id if not present in args
    let mut args = request.args.clone();
    if let Some(obj) = args.as_object_mut() {
        if !obj.contains_key("site") {
            obj.insert(
                "site".to_string(),
                serde_json::Value::String(instance_id.clone()),
            );
        }
    }

    // The window's toolbar shows the CLI's masked label, never the arguments.
    let label = request
        .display
        .clone()
        .unwrap_or_else(|| request.tool.clone());
    let result = crate::activity::track(
        label,
        crate::tools::definitions::execute_tool(&request.tool, &args),
    )
    .await;

    let response = match result {
        Ok(output) => DaemonResponse {
            ok: true,
            result: output,
        },
        Err(e) => DaemonResponse {
            ok: false,
            result: serde_json::json!({ "error": e.to_string() }),
        },
    };

    let resp_str = serde_json::to_string(&response)?;
    writer.write_all(resp_str.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod ttl_tests {
    use super::parse_ttl;

    #[test]
    fn ttl_values() {
        assert_eq!(parse_ttl("90"), Ok(90));
        assert_eq!(parse_ttl("90s"), Ok(90));
        assert_eq!(parse_ttl("10m"), Ok(600));
        assert_eq!(parse_ttl("2h"), Ok(7200));
        assert_eq!(parse_ttl("1h30m"), Ok(5400));
        assert_eq!(parse_ttl(" 5M "), Ok(300));
        for bad in ["", "0", "0s", "m", "10x", "1.5h", "-5", "10m5"] {
            assert!(parse_ttl(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
