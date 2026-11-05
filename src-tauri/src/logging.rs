//! File logging and panic hook setup.

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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

    let now = chrono::Local::now();
    let log_filename = match instance_id {
        Some(id) => format!("dalzzack_{}_{}.log", id, now.format("%Y%m%d")),
        None => format!("dalzzack_{}.log", now.format("%Y%m%d")),
    };

    let file_appender = tracing_appender::rolling::never(&log_dir, &log_filename);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_target(true);

    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("dalzzack=debug".parse().unwrap())
        .add_directive("dalzzack_lib=debug".parse().unwrap());

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
