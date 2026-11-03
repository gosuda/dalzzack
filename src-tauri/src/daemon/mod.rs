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
    let path = pid_file_path(instance_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
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
