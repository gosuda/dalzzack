//! Daemon module — manages persistent browser instances via TCP IPC.
//!
//! Architecture:
//! - `start_daemon(id)` spawns a background process running the Tauri WebView + TCP server
//! - `send_command(id, tool, args)` connects via TCP and sends a tool request
//! - `stop_daemon(id)` sends a shutdown command
//! - `list_instances()` scans for active port files

mod client;
mod server;

pub use client::{
    kill_daemon, list_instances, list_instances_json, live_instance_ids, ping_info, send_command,
    stop_daemon,
};
pub use server::{parse_ttl, run_daemon, start_daemon, DaemonOptions};
// For engines that run their own startup (Chrome) but serve the same protocol.
pub(crate) use server::{
    run_tcp_server, set_daemon_config, set_renderer_health, startup_error_path,
};

use crate::chrome::launch::process_identity;
use std::path::PathBuf;
use std::sync::OnceLock;

static INSTANCE: OnceLock<String> = OnceLock::new();

/// Set once by the daemon process itself.
pub fn set_instance_id(id: &str) {
    let _ = INSTANCE.set(id.to_string());
}

/// This daemon's id — `None` in the CLI process.
pub fn instance_id() -> Option<String> {
    INSTANCE.get().cloned()
}

/// Get the port file path for a given instance ID.
/// The file stores the TCP port number the daemon is listening on.
pub fn port_file_path(instance_id: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    dir.join(format!("dalzzack-{}.port", instance_id))
}

/// Get the PID file path for a given instance ID.
/// Written by the daemon process so we can force-kill it when it stops
/// responding to `__ping`/`__shutdown`.
pub fn pid_file_path(instance_id: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    dir.join(format!("dalzzack-{}.pid", instance_id))
}

/// Read the port number from the port file for a given instance ID.
pub fn read_port(instance_id: &str) -> Option<u16> {
    let path = port_file_path(instance_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// Read the PID from the PID file for a given instance ID.
pub fn read_pid(instance_id: &str) -> Option<u32> {
    read_pid_file(instance_id).map(|record| record.0)
}

fn read_pid_file(instance_id: &str) -> Option<(u32, Option<String>)> {
    let text = std::fs::read_to_string(pid_file_path(instance_id)).ok()?;
    parse_pid_file(&text)
}

/// The pid, then on a second line the identity of that process. A file from
/// before 1.1 has only the pid.
fn parse_pid_file(text: &str) -> Option<(u32, Option<String>)> {
    let mut lines = text.lines().map(str::trim);
    let pid = lines.next()?.parse().ok()?;
    let identity = lines.next().filter(|line| !line.is_empty());
    Some((pid, identity.map(str::to_string)))
}

/// What this process writes to its pid file.
pub(crate) fn pid_file_text(pid: u32) -> String {
    match process_identity(pid) {
        Some(identity) => format!("{pid}\n{identity}\n"),
        None => pid.to_string(),
    }
}

/// Whether the daemon that wrote a pid file still runs.
///
/// A pid alone proves nothing: the file outlives its daemon over a restart, and
/// the pid can then belong to another program. So nothing force-kills a pid
/// unless this answers `Running`.
#[derive(Debug, PartialEq)]
pub enum DaemonProcess {
    /// The process that wrote the file still runs.
    Running { pid: u32, identity: String },
    /// There is no file, or its pid names no process or another process.
    Gone,
    /// A process has this pid, but the file has no identity to compare.
    Unverified(u32),
}

pub fn daemon_process(instance_id: &str) -> DaemonProcess {
    classify(read_pid_file(instance_id), process_identity)
}

fn classify(
    record: Option<(u32, Option<String>)>,
    identity_of: impl Fn(u32) -> Option<String>,
) -> DaemonProcess {
    let Some((pid, written)) = record else {
        return DaemonProcess::Gone;
    };
    match (written, identity_of(pid)) {
        (Some(written), Some(current)) if written == current => DaemonProcess::Running {
            pid,
            identity: written,
        },
        (None, Some(_)) => DaemonProcess::Unverified(pid),
        _ => DaemonProcess::Gone,
    }
}

/// True once `pid` no longer names the process with this identity.
pub(crate) fn process_ended(pid: u32, identity: &str) -> bool {
    process_identity(pid).as_deref() != Some(identity)
}

/// The command for a user to end a daemon that dalzzack cannot end.
pub(crate) fn force_end_hint(pid: u32) -> String {
    if cfg!(windows) {
        format!("From an administrator terminal, run: taskkill /F /PID {pid}")
    } else {
        format!("Run: sudo kill -9 {pid}")
    }
}

/// For a pid that may belong to another program, so the check comes first.
pub(crate) fn unverified_hint(pid: u32) -> String {
    format!(
        "Check that pid {pid} is dalzzack before you end it. {} `dalzzack list` clears this record.",
        force_end_hint(pid)
    )
}

/// JSON protocol: request sent over socket
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct DaemonRequest {
    pub tool: String,
    pub args: serde_json::Value,
}

/// JSON protocol: response sent over socket
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct DaemonResponse {
    pub ok: bool,
    pub result: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_is(current: &'static str) -> impl Fn(u32) -> Option<String> {
        move |_| Some(current.to_string())
    }

    fn no_process(_: u32) -> Option<String> {
        None
    }

    #[test]
    fn a_pid_file_has_a_pid_and_an_identity() {
        assert_eq!(
            parse_pid_file("42\n133912\n"),
            Some((42, Some("133912".to_string())))
        );
        assert_eq!(
            parse_pid_file("42\r\nMon Nov  6 10:00:00 2026\r\n"),
            Some((42, Some("Mon Nov  6 10:00:00 2026".to_string())))
        );
    }

    #[test]
    fn a_pid_file_from_before_1_1_has_only_a_pid() {
        assert_eq!(parse_pid_file("42"), Some((42, None)));
        assert_eq!(parse_pid_file("42\n"), Some((42, None)));
        assert_eq!(parse_pid_file("42\n\n"), Some((42, None)));
        assert_eq!(parse_pid_file("not a pid"), None);
        assert_eq!(parse_pid_file(""), None);
    }

    #[test]
    fn only_the_same_process_counts_as_running() {
        assert_eq!(
            classify(Some((42, Some("a".to_string()))), identity_is("a")),
            DaemonProcess::Running {
                pid: 42,
                identity: "a".to_string()
            }
        );
    }

    #[test]
    fn a_reused_pid_is_not_the_daemon() {
        assert_eq!(
            classify(Some((42, Some("a".to_string()))), identity_is("b")),
            DaemonProcess::Gone
        );
    }

    #[test]
    fn a_pid_with_no_process_is_gone() {
        assert_eq!(
            classify(Some((42, Some("a".to_string()))), no_process),
            DaemonProcess::Gone
        );
        assert_eq!(classify(Some((42, None)), no_process), DaemonProcess::Gone);
        assert_eq!(classify(None, identity_is("a")), DaemonProcess::Gone);
    }

    #[test]
    fn a_pid_with_no_identity_cannot_be_proved() {
        assert_eq!(
            classify(Some((42, None)), identity_is("a")),
            DaemonProcess::Unverified(42)
        );
    }

    #[test]
    fn test_port_file_path_format() {
        let path = port_file_path("my-instance");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("dalzzack-my-instance.port"));
    }

    #[test]
    fn test_daemon_request_serde() {
        let req = DaemonRequest {
            tool: "navigate".to_string(),
            args: serde_json::json!({"url": "https://example.com"}),
        };

        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tool, "navigate");
        assert_eq!(deserialized.args["url"], "https://example.com");
    }

    #[test]
    fn test_daemon_response_serde_ok() {
        let resp = DaemonResponse {
            ok: true,
            result: serde_json::json!({"status": "completed"}),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(deserialized.ok);
        assert_eq!(deserialized.result["status"], "completed");
    }

    #[test]
    fn test_daemon_response_serde_error() {
        let resp = DaemonResponse {
            ok: false,
            result: serde_json::json!({"error": "WEBVIEW_NOT_FOUND"}),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(!deserialized.ok);
    }
}
