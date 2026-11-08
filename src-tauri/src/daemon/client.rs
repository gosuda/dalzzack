//! Daemon client — connects to a running daemon via TCP.

use super::server::force_kill_pid;
use super::{
    daemon_process, force_end_hint, pid_file_path, port_file_path, process_ended, read_pid,
    read_port, unverified_hint, DaemonProcess, DaemonRequest, DaemonResponse,
};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Send a tool command to a running daemon instance.
pub fn send_command(
    instance_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let port = read_port(instance_id).ok_or_else(|| {
        format!(
            "No daemon running with id '{}'. Start one with: dalzzack start --id {}",
            instance_id, instance_id
        )
    })?;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).map_err(|e| {
        format!(
            "Failed to connect to daemon '{}': {}. Try: dalzzack start --id {}",
            instance_id, e, instance_id
        )
    })?;

    // Set read timeout
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| format!("Failed to set timeout: {}", e))?;

    // Send request
    let request = DaemonRequest {
        tool: tool.to_string(),
        args,
    };
    let request_str = serde_json::to_string(&request).map_err(|e| e.to_string())?;

    stream
        .write_all(request_str.as_bytes())
        .map_err(|e| format!("Write failed: {}", e))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("Write failed: {}", e))?;
    stream.flush().map_err(|e| format!("Flush failed: {}", e))?;

    // Read response
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("Read failed: {}", e))?;

    let response: DaemonResponse =
        serde_json::from_str(line.trim()).map_err(|e| format!("Invalid response: {}", e))?;

    if !response.ok {
        let error_msg = response
            .result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        Err(error_msg)
    } else {
        Ok(response.result)
    }
}

/// Stop a running daemon instance via graceful shutdown.
///
/// `Ok` is the JSON to print on stdout; `Err` is a failure the CLI prints on
/// stderr and exits 1 for (unchanged from when this printed itself).
pub fn stop_daemon(instance_id: &str) -> Result<serde_json::Value, serde_json::Value> {
    // Which profile it had — asked before it is gone.
    let profile = ping_info(instance_id).and_then(|i| i.get("profile").cloned());
    match send_command(instance_id, "__shutdown", serde_json::json!({})) {
        Ok(reply) => {
            let mut out = serde_json::json!({
                "status": "stopped",
                "id": instance_id,
            });
            // A Chrome-engine daemon closes Chrome and deletes its profile
            // before it answers; say how that went.
            if let Some(chrome) = reply.get("chrome") {
                out["chrome"] = chrome.clone();
            }
            // A `--private-profile` folder is throw-away; leaving it would
            // turn every `start --id <random> --private-profile` into litter.
            if let Some(removed) = profile
                .as_ref()
                .and_then(crate::profile::remove_disposable_webview_dir)
            {
                out["profile_removed"] = serde_json::json!(removed);
            }
            Ok(out)
        }
        Err(e) => {
            // Clean up after a daemon that is gone. A daemon that only hangs
            // keeps its files: they are how `kill` and the next `uninstall`
            // find it.
            if daemon_process(instance_id) == DaemonProcess::Gone {
                let _ = std::fs::remove_file(port_file_path(instance_id));
                let _ = std::fs::remove_file(pid_file_path(instance_id));
            }
            Err(serde_json::json!({
                "status": "failed",
                "error": e,
                "id": instance_id,
                "hint": "If the daemon is hung, use `dalzzack kill --id <id>` to force-terminate."
            }))
        }
    }
}

/// Force-kill a daemon process by PID. Use this as an escape hatch when the
/// daemon is wedged on a tool call and `stop` times out. It kills only the
/// process that wrote the pid file.
pub fn kill_daemon(instance_id: &str) -> Result<serde_json::Value, serde_json::Value> {
    let port_file = port_file_path(instance_id);
    let pid_file = pid_file_path(instance_id);

    match daemon_process(instance_id) {
        DaemonProcess::Running { pid, identity } => {
            force_kill_pid(pid);
            // `kill` returns before the system has ended the process.
            let mut ended = process_ended(pid, &identity);
            for _ in 0..20 {
                if ended {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
                ended = process_ended(pid, &identity);
            }
            if !ended {
                return Err(serde_json::json!({
                    "status": "failed",
                    "error": "kill_failed",
                    "id": instance_id,
                    "pid": pid,
                    "message": "The daemon is still running. A process with higher rights can refuse this.",
                    "hint": force_end_hint(pid),
                }));
            }
            let _ = std::fs::remove_file(&port_file);
            let _ = std::fs::remove_file(&pid_file);
            // A Chrome-engine daemon leaves Chrome running if only the daemon dies
            // — and its throw-away profile on disk. `kill` bypasses the daemon, so
            // it reaps both from the record the daemon left.
            let chrome = crate::chrome::launch::reap(instance_id);
            let profile_removed = crate::profile::remove_disposable_after_kill(instance_id);
            Ok(serde_json::json!({
                "status": "killed",
                "id": instance_id,
                "pid": pid,
                "chrome": chrome,
                "profile_removed": profile_removed,
            }))
        }
        DaemonProcess::Unverified(pid) => Err(serde_json::json!({
            "status": "failed",
            "error": "unverified_pid",
            "id": instance_id,
            "pid": pid,
            "message": "This pid file has no process identity, so the pid can belong to another program now. Nothing was killed.",
            "hint": unverified_hint(pid),
        })),
        DaemonProcess::Gone => {
            let _ = std::fs::remove_file(&port_file);
            let _ = std::fs::remove_file(&pid_file);
            // The daemon is gone already (crashed, or `list` cleaned up after
            // it) — its Chrome and its throw-away profile may not be.
            let chrome = crate::chrome::launch::reap(instance_id);
            let profile_removed = crate::profile::remove_disposable_after_kill(instance_id);
            Err(serde_json::json!({
                "status": "no_pid_found",
                "id": instance_id,
                "message": "No daemon for this id. Removed any stale port/pid files. It may have crashed or was never started.",
                "chrome": chrome,
                "profile_removed": profile_removed,
            }))
        }
    }
}

/// Ids of daemons that are actually alive (port file present AND answering
/// `__ping`). Used to default `-i` when exactly one instance is running —
/// a missing `--id` used to be a clap error on stderr only, which a runner
/// with `2>/dev/null` turns into a silent no-op.
pub fn live_instance_ids() -> Vec<String> {
    let tmp_dir = std::env::temp_dir();
    let mut ids = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name
                .strip_prefix("dalzzack-")
                .and_then(|s| s.strip_suffix(".port"))
            {
                if ping_info(id).is_some() {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// `__ping` one daemon and return what it reports, or `None` if it does not
/// answer within ~2.5 s (which is how a wedged daemon looks from outside).
pub fn ping_info(instance_id: &str) -> Option<serde_json::Value> {
    let port = read_port(instance_id)?;
    let addr = format!("127.0.0.1:{}", port).parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok();
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let req = serde_json::json!({"tool": "__ping", "args": {}});
    let req_str = serde_json::to_string(&req).ok()?;
    stream.write_all(req_str.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).ok()?;
    if !resp.contains("\"ok\":true") {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(resp.trim())
        .ok()
        .and_then(|v| v.get("result").cloned())
        .or(Some(serde_json::json!({})))
}

/// Human-readable "how long ago" for an epoch-ms timestamp.
fn ago(ms: Option<u64>) -> Option<String> {
    let ms = ms?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    let s = now.saturating_sub(ms) / 1000;
    Some(if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h{}m ago", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d ago", s / 86_400)
    })
}

/// Collect every live instance's state. Dead ones have their stale port/pid
/// files removed on the way.
pub fn list_instances_json() -> serde_json::Value {
    let tmp_dir = std::env::temp_dir();
    let mut instances = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("dalzzack-") && name.ends_with(".port") {
                let id = name
                    .strip_prefix("dalzzack-")
                    .unwrap()
                    .strip_suffix(".port")
                    .unwrap()
                    .to_string();

                // Check if alive — ping with 2 s timeout so we catch the
                // "kernel-accepts-but-app-is-stuck" case. If it doesn't answer
                // ping, treat it as dead and clean up.
                let info = ping_info(&id);
                let alive = info.is_some();

                if alive {
                    let get = |k: &str| info.as_ref().and_then(|i| i.get(k).cloned());
                    let last_used_ms = get("last_used_ms").and_then(|v| v.as_u64());
                    instances.push(serde_json::json!({
                        "id": id,
                        "port": read_port(&id),
                        "pid": read_pid(&id),
                        "status": "running",
                        // Start-time flags, so "why is my tape empty?" is one
                        // command away and a stale binary is obvious.
                        "version": get("version"),
                        // Can this daemon run JS at all? A broken webview makes
                        // every tool time out as if the page were wedged.
                        "renderer_ok": get("renderer_ok"),
                        "renderer_error": get("renderer_error"),
                        "record": get("record"),
                        // `on` | `off` | `never` — the tape's state. Not the
                        // same as `recording_armed` (did the transport come
                        // up?), which stays true after `record off`.
                        "recording": get("recording"),
                        "recording_armed": get("recording_armed"),
                        "watch_hang": get("watch_hang"),
                        // Which console transport came up. `unavailable` means
                        // console-logs will be empty because capture is broken,
                        // not because the page is quiet.
                        "console_transport": get("console_transport"),
                        "console_transport_error": get("console_transport_error"),
                        // `user_agent: null` means the engine's own UA is in
                        // use; `user_agent_source` says how that was decided.
                        "user_agent": get("user_agent"),
                        "user_agent_source": get("user_agent_source"),
                        // `true` means the page's CSP is NOT enforced, so no
                        // violation can ever be observed. Check this before
                        // concluding a page has no policy problems.
                        "csp_bypassed": get("csp_bypassed"),
                        // Which browser, and whose cookie jar. A `shared`
                        // profile is every plain daemon's — clearing it in
                        // one clears it in all.
                        "engine": get("engine"),
                        "browser_version": get("browser_version"),
                        "navigator_webdriver": get("navigator_webdriver"),
                        "profile": get("profile"),
                        // Where it is and when anyone last touched it — read
                        // from daemon memory, not from the page, so a wedged
                        // renderer still answers. Null on pre-0.18 daemons.
                        "current_url": get("current_url"),
                        "last_used_ms": last_used_ms,
                        "last_used": ago(last_used_ms),
                        "last_tool": get("last_tool"),
                        "started_at_ms": get("started_at_ms"),
                        // `start --ttl`: when it stops itself (null = never).
                        "ttl_s": get("ttl_s"),
                        "expires_at_ms": get("expires_at_ms"),
                        "console_buffer": get("console_buffer"),
                        "load_failure_capture": get("load_failure_capture"),
                    }));
                } else if let DaemonProcess::Running { pid, .. } = daemon_process(&id) {
                    // Not answering, but the same process runs: it hangs. Its
                    // files are how `kill` finds it.
                    instances.push(serde_json::json!({
                        "id": id,
                        "port": read_port(&id),
                        "pid": pid,
                        "status": "unresponsive",
                        "hint": format!("Run: dalzzack kill --id {id}"),
                    }));
                } else {
                    // Anything else is stale.
                    let process = daemon_process(&id);
                    let _ = std::fs::remove_file(port_file_path(&id));
                    let _ = std::fs::remove_file(pid_file_path(&id));
                    // A Chrome-engine daemon that died left Chrome behind; once
                    // its record is the only trace left, this is the last
                    // chance to end it (identity-checked, never a reused pid).
                    if process == DaemonProcess::Gone {
                        let _ = crate::chrome::launch::reap(&id);
                    }
                }
            }
        }
    }

    // A record whose daemon is gone but whose port file is too — an earlier
    // `list` cleaned the files up while the dying daemon could still be
    // opened, so the loop above never sees the id again. The record names its
    // daemon, so this never touches one that is still starting.
    for id in crate::chrome::launch::recorded_ids() {
        if !port_file_path(&id).exists() && crate::chrome::launch::record_owner_gone(&id) {
            let _ = crate::chrome::launch::reap(&id);
        }
    }

    serde_json::json!({
        "instances": instances,
        "count": instances.len(),
    })
}

/// List all running dalzzack daemon instances.
pub fn list_instances() {
    println!(
        "{}",
        serde_json::to_string_pretty(&list_instances_json()).unwrap_or_default()
    );
}
