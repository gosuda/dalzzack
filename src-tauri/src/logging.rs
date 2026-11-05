//! File logging and panic hook setup.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The longest log line, in bytes. A tool result can be megabytes, and the log
/// is not the place for page content.
const MAX_LOG_LINE: usize = 2_000;

/// Logs older than this are deleted when an instance starts.
const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Writes each log line cut to `MAX_LOG_LINE`. The fmt layer writes one event
/// in one call, so one call is one line.
struct ClippedLines<W>(W);

impl<W: Write> Write for ClippedLines<W> {
    fn write(&mut self, line: &[u8]) -> std::io::Result<usize> {
        if line.len() <= MAX_LOG_LINE {
            self.0.write_all(line)?;
        } else {
            self.0.write_all(&line[..MAX_LOG_LINE])?;
            self.0.write_all(b" [cut]\n")?;
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn delete_old_logs(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if path.extension().is_some_and(|e| e == "log") && age.is_some_and(|age| age > max_age) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The folder for daemon logs and crash reports.
pub fn get_log_path() -> PathBuf {
    crate::state::data_dir().join("logs")
}

/// Initialize logging to a file. Nothing goes to stderr: a daemon has none, and
/// an agent that runs `dalzzack run` would read every log line.
/// Returns a WorkerGuard that flushes logs when dropped.
pub fn init_file_logging(instance_id: Option<&str>) -> Option<WorkerGuard> {
    let log_dir = get_log_path();

    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("[dalzzack] Failed to create log directory: {}", e);
        return None;
    }
    delete_old_logs(&log_dir, LOG_RETENTION);

    let now = chrono::Local::now();
    let log_filename = match instance_id {
        Some(id) => format!("dalzzack_{}_{}.log", id, now.format("%Y%m%d")),
        None => format!("dalzzack_{}.log", now.format("%Y%m%d")),
    };

    let file_appender = tracing_appender::rolling::never(&log_dir, &log_filename);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(move || ClippedLines(non_blocking.clone()))
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_target(true);

    let env_filter = tracing_subscriber::EnvFilter::try_from_env("DALZZACK_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("dalzzack=info,dalzzack_lib=info"));

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .try_init();

    tracing::info!("=== dalzzack Started ===");
    tracing::info!("Log file: {:?}", log_dir.join(&log_filename));
    tracing::info!("Instance ID: {:?}", instance_id);

    Some(guard)
}

/// Set up panic hook for crash logging
pub fn setup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic".to_string()
        };

        tracing::error!("!!! PANIC !!! at {}: {}", location, message);

        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!("Backtrace:\n{}", backtrace);

        let crash_file = get_log_path().join(format!(
            "crash_{}.log",
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        ));
        let crash_content = format!(
            "=== dalzzack CRASH ===\nTime: {}\nLocation: {}\nMessage: {}\n\nBacktrace:\n{}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            location,
            message,
            backtrace
        );
        let _ = std::fs::write(&crash_file, crash_content);
        eprintln!("[dalzzack] Crash log saved to: {:?}", crash_file);

        default_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_files_live_in_the_data_folder() {
        assert!(get_log_path().starts_with(crate::state::data_dir()));
    }

    #[test]
    fn a_long_line_is_cut_and_a_short_line_is_not() {
        let mut out = ClippedLines(Vec::new());
        assert_eq!(out.write(b"short\n").unwrap(), 6);
        let long = vec![b'x'; MAX_LOG_LINE * 2];
        assert_eq!(out.write(&long).unwrap(), long.len());
        let text = String::from_utf8(out.0).unwrap();
        assert!(text.starts_with("short\nxxx"));
        assert!(text.ends_with(" [cut]\n"));
        assert_eq!(text.len(), 6 + MAX_LOG_LINE + " [cut]\n".len());
    }

    #[test]
    fn only_old_log_files_are_deleted() {
        let dir = std::env::temp_dir().join(format!("dalzzack-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let week_ago = SystemTime::now() - LOG_RETENTION - Duration::from_secs(60);
        for (name, modified) in [
            ("old.log", week_ago),
            ("new.log", SystemTime::now()),
            ("old.txt", week_ago),
        ] {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_modified(modified).unwrap();
        }

        delete_old_logs(&dir, LOG_RETENTION);

        assert!(!dir.join("old.log").exists());
        assert!(dir.join("new.log").exists());
        assert!(dir.join("old.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
