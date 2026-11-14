//! `screencast`: record what the page shows as an mp4.
//!
//! `screencast start`, run any commands (scroll, click, navigate), then
//! `screencast stop --output a.mp4`. Frames come from the engine itself, page
//! only (never the window, the toolbar or the rest of the screen): CDP's
//! `Page.startScreencast` on the Chrome engine, and WebKit's own snapshot of the
//! page on macOS. They go to disk as they arrive, each with the time it arrived,
//! and `stop` encodes them with ffmpeg so the video runs in real time (see
//! [`encode`]).
//!
//! One recording per daemon. It stops capturing by itself at `--max-seconds`
//! (the frames are kept until `stop`), so a forgotten recording cannot fill a
//! disk. Without ffmpeg `stop` fails *without* discarding the frames: install it
//! and call `stop` again.

pub mod encode;
#[cfg(target_os = "macos")]
mod webkit;

use anyhow::Result;
use encode::FrameMeta;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const DEFAULT_FPS: u32 = 30;
const DEFAULT_MAX_SECONDS: u64 = 120;
const DEFAULT_QUALITY: u8 = 80;
/// A recording that reaches this much on disk stops capturing.
const MAX_BYTES: u64 = 1_500_000_000;
/// Frames waiting to be written. A source that outruns the disk drops frames
/// (and counts them) instead of growing memory.
const QUEUE: usize = 32;

/// A refusal, as the `error` code and the `message` an agent reads.
#[derive(Debug, Clone)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
        }
    }

    pub fn json(&self) -> Value {
        json!({ "status": "failed", "error": self.code, "message": self.message })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub fps: u32,
    pub max_seconds: u64,
    pub quality: u8,
}

impl Options {
    pub fn from_args(args: &Value) -> Result<Options, Failure> {
        let number = |key: &str, default: u64, min: u64, max: u64| -> Result<u64, Failure> {
            match args.get(key).filter(|v| !v.is_null()) {
                None => Ok(default),
                Some(v) => v
                    .as_u64()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| {
                        Failure::new(
                            "INVALID_OPTION",
                            format!("{key} must be a whole number from {min} to {max}"),
                        )
                    }),
            }
        };
        Ok(Options {
            fps: number("fps", u64::from(DEFAULT_FPS), 1, 60)? as u32,
            max_seconds: number("max_seconds", DEFAULT_MAX_SECONDS, 1, 600)?,
            quality: number("quality", u64::from(DEFAULT_QUALITY), 1, 100)? as u8,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    /// The Chrome engine: CDP `Page.startScreencast`.
    Chrome,
    /// macOS WKWebView: snapshots of the page, on a timer.
    #[cfg(target_os = "macos")]
    WebKit,
    /// Scripted frames, for the tests.
    #[cfg(test)]
    Scripted,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Chrome => "cdp_screencast",
            #[cfg(target_os = "macos")]
            Source::WebKit => "webkit_snapshot",
            #[cfg(test)]
            Source::Scripted => "scripted",
        }
    }

    fn pick() -> Result<Source, Failure> {
        #[cfg(test)]
        if SCRIPTED.load(Ordering::SeqCst) {
            return Ok(Source::Scripted);
        }
        if crate::engine::is_chrome() {
            return Ok(Source::Chrome);
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Source::WebKit)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(Failure::new(
                "UNSUPPORTED_PLATFORM",
                "screencast is not built for this platform's default engine yet (macOS has it). \
                 Start the instance with `--engine chrome` to record here.",
            ))
        }
    }

    async fn start(
        self,
        options: &Options,
        stop: &Arc<AtomicBool>,
    ) -> Result<Option<JoinHandle<()>>, Failure> {
        match self {
            Source::Chrome => chrome_start(options).await.map(|_| None),
            #[cfg(target_os = "macos")]
            Source::WebKit => webkit::start(options, stop.clone()).await.map(Some),
            #[cfg(test)]
            Source::Scripted => {
                let _ = (options, stop);
                Ok(None)
            }
        }
    }

    async fn stop(self) {
        if self == Source::Chrome {
            let _ =
                crate::chrome::page_call("Page.stopScreencast", json!({}), Duration::from_secs(5))
                    .await;
        }
    }
}

async fn chrome_start(options: &Options) -> Result<(), Failure> {
    // Frames at the size the page has, not larger: the viewport in CSS pixels.
    let metrics =
        crate::chrome::page_call("Page.getLayoutMetrics", json!({}), Duration::from_secs(5))
            .await
            .map_err(|e| Failure::new("SCREENCAST_FAILED", e))?;
    let size = |key: &str| {
        metrics
            .pointer(&format!("/cssVisualViewport/{key}"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .round() as u64
    };
    let (width, height) = (size("clientWidth"), size("clientHeight"));
    let mut params = json!({
        "format": "jpeg",
        "quality": options.quality,
        "everyNthFrame": 1,
    });
    if width > 0 && height > 0 {
        params["maxWidth"] = json!(width);
        params["maxHeight"] = json!(height);
    }
    crate::chrome::page_call("Page.startScreencast", params, Duration::from_secs(10))
        .await
        .map(|_| ())
        .map_err(|e| Failure::new("SCREENCAST_FAILED", e))
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

struct Frame {
    offset_ms: u64,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
}

struct Sink {
    tx: mpsc::Sender<Frame>,
    started: Instant,
    counters: Arc<Counters>,
}

/// Where sources put frames. `None` between recordings, so a late frame from a
/// source that has not stopped yet goes nowhere.
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Hand a JPEG frame to the recording in progress. Safe from any thread, never
/// blocks: a frame that does not fit the queue is dropped and counted.
pub fn push_frame(bytes: Vec<u8>) {
    let guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sink) = guard.as_ref() else { return };
    sink.counters.received.fetch_add(1, Ordering::Relaxed);
    let frame = Frame {
        offset_ms: sink.started.elapsed().as_millis() as u64,
        bytes,
    };
    if sink.tx.try_send(frame).is_err() {
        sink.counters.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

async fn write_frames(
    dir: PathBuf,
    mut rx: mpsc::Receiver<Frame>,
    counters: Arc<Counters>,
) -> Vec<FrameMeta> {
    let mut written = Vec::new();
    while let Some(frame) = rx.recv().await {
        let file = dir.join(format!("{:06}.jpg", written.len() + 1));
        let len = frame.bytes.len() as u64;
        if tokio::fs::write(&file, &frame.bytes).await.is_ok() {
            counters.bytes.fetch_add(len, Ordering::Relaxed);
            written.push(FrameMeta {
                offset_ms: frame.offset_ms,
                file,
            });
        }
    }
    written
}

// ---------------------------------------------------------------------------
// The recording
// ---------------------------------------------------------------------------

enum Phase {
    Capturing,
    /// Capture has ended (`stop`, or a limit); the frames wait for `stop` to encode them.
    Ended {
        reason: &'static str,
        end_ms: u64,
    },
}

struct Session {
    dir: PathBuf,
    options: Options,
    source: Source,
    started: Instant,
    counters: Arc<Counters>,
    phase: Phase,
    stop_source: Arc<AtomicBool>,
    source_task: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<Vec<FrameMeta>>>,
    frames: Vec<FrameMeta>,
}

static SESSION: tokio::sync::Mutex<Option<Session>> = tokio::sync::Mutex::const_new(None);

#[cfg(test)]
static SCRIPTED: AtomicBool = AtomicBool::new(false);

/// When capture began (epoch ms), or 0: what the toolbar's REC badge counts from.
static RECORDING_SINCE: AtomicU64 = AtomicU64::new(0);

/// Epoch ms at which the capture in progress began, if one is.
pub fn recording_since() -> Option<u64> {
    match RECORDING_SINCE.load(Ordering::SeqCst) {
        0 => None,
        ms => Some(ms),
    }
}

fn set_recording_since(ms: u64) {
    RECORDING_SINCE.store(ms, Ordering::SeqCst);
    crate::toolbar::push();
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn root_dir() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join("dalzzack-screencast-test");
    }
    crate::state::data_dir().join("screencast")
}

/// Capture ends: the source stops, the queue drains to disk. Idempotent, and an
/// earlier reason (a limit) is kept when `stop` follows it.
async fn end_capture(session: &mut Session, reason: &'static str) {
    if matches!(session.phase, Phase::Ended { .. }) {
        return;
    }
    session.stop_source.store(true, Ordering::SeqCst);
    session.source.stop().await;
    if let Some(task) = session.source_task.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }
    let end_ms = session.started.elapsed().as_millis() as u64;
    // Dropping the sink closes the queue, which is what lets the writer finish.
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Some(writer) = session.writer.take() {
        session.frames = writer.await.unwrap_or_default();
    }
    session.phase = Phase::Ended { reason, end_ms };
    set_recording_since(0);
}

/// Ends capture at the limits, so a recording nobody stopped cannot fill a disk.
fn spawn_watchdog() -> JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut slot = SESSION.lock().await;
            let Some(session) = slot.as_mut() else { return };
            if !matches!(session.phase, Phase::Capturing) {
                return;
            }
            let over_time = session.started.elapsed().as_secs() >= session.options.max_seconds;
            let over_size = session.counters.bytes.load(Ordering::Relaxed) >= MAX_BYTES;
            if over_time || over_size {
                let reason = if over_time {
                    "max_seconds"
                } else {
                    "max_bytes"
                };
                end_capture(session, reason).await;
                tracing::info!("[screencast] capture ended by its limit: {reason}");
                return;
            }
        }
    })
}

pub async fn start(site: &str, options: Options) -> Result<Value, Failure> {
    let mut slot = SESSION.lock().await;
    if let Some(existing) = slot.as_ref() {
        return Err(match existing.phase {
            Phase::Capturing => Failure::new(
                "ALREADY_RECORDING",
                "A screencast is already recording on this instance. Run `screencast stop \
                 --output FILE` to end it.",
            ),
            Phase::Ended { .. } => Failure::new(
                "ALREADY_RECORDING",
                "A screencast has ended capture and is waiting to be saved. Run `screencast \
                 stop --output FILE` to encode it.",
            ),
        });
    }
    let source = Source::pick()?;
    remove_stale_recordings().await;

    let dir = root_dir().join(format!("{site}-{}", wall_ms()));
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        Failure::new(
            "SCREENCAST_FAILED",
            format!("cannot create {}: {e}", dir.display()),
        )
    })?;

    let counters = Arc::new(Counters::default());
    let (tx, rx) = mpsc::channel(QUEUE);
    let started = Instant::now();
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink {
        tx,
        started,
        counters: counters.clone(),
    });
    let writer = tokio::spawn(write_frames(dir.clone(), rx, counters.clone()));
    let stop_source = Arc::new(AtomicBool::new(false));

    let source_task = match source.start(&options, &stop_source).await {
        Ok(task) => task,
        Err(failure) => {
            *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = writer.await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(failure);
        }
    };
    spawn_watchdog();
    set_recording_since(wall_ms());
    *slot = Some(Session {
        dir,
        options,
        source,
        started,
        counters,
        phase: Phase::Capturing,
        stop_source,
        source_task,
        writer: Some(writer),
        frames: Vec::new(),
    });
    Ok(json!({
        "status": "started",
        "recording": true,
        "source": source.as_str(),
        "fps": options.fps,
        "max_seconds": options.max_seconds,
        "hint": "Run your commands, then `screencast stop --output FILE.mp4`. Only the page is recorded.",
    }))
}

pub async fn stop(site: &str, output: Option<&str>) -> Result<Value, Failure> {
    let mut slot = SESSION.lock().await;
    let Some(session) = slot.as_mut() else {
        return Err(Failure::new(
            "NOT_RECORDING",
            "No screencast is recording on this instance. Start one with `screencast start`.",
        ));
    };
    end_capture(session, "stop").await;
    let Phase::Ended { reason, end_ms } = session.phase else {
        unreachable!("end_capture ends the phase");
    };
    let frames = session.frames.clone();
    if frames.is_empty() {
        let dir = session.dir.clone();
        *slot = None;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(Failure::new(
            "NO_FRAMES",
            "The recording captured no frames: the page did not paint while it ran. Nothing was written.",
        ));
    }
    // From here a failure keeps the frames, so the caller can fix it and retry.
    let Some(ffmpeg) = encode::find_ffmpeg() else {
        return Err(Failure::new(
            "FFMPEG_NOT_FOUND",
            format!(
                "ffmpeg is needed to make the mp4 and was not found on PATH. The {} captured frames \
                 are kept: install ffmpeg (macOS: `brew install ffmpeg`; Windows: `winget install \
                 ffmpeg`), or point DALZZACK_FFMPEG at it, then run `screencast stop` again.",
                frames.len()
            ),
        ));
    };
    let output_path = resolve_output(site, output).await?;
    if let Some(parent) = output_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let fps = session.options.fps;
    let list = session.dir.join("frames.txt");
    tokio::fs::write(&list, encode::concat_list(&frames, end_ms, fps))
        .await
        .map_err(|e| Failure::new("ENCODE_FAILED", format!("cannot write the frame list: {e}")))?;

    let run = tokio::process::Command::new(&ffmpeg)
        .args(encode::ffmpeg_args(&list, &output_path, fps))
        .kill_on_drop(true)
        .output();
    let result = tokio::time::timeout(Duration::from_secs(600), run).await;
    let output_ok = matches!(&result, Ok(Ok(o)) if o.status.success());
    if !output_ok {
        let detail = match result {
            Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "ffmpeg did not finish within 10 minutes".to_string(),
        };
        return Err(Failure::new(
            "ENCODE_FAILED",
            format!("ffmpeg failed: {detail}. The frames are kept; `screencast stop` retries."),
        ));
    }

    let size = tokio::fs::metadata(&output_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let received = session.counters.received.load(Ordering::Relaxed);
    let dropped = session.counters.dropped.load(Ordering::Relaxed);
    let first_ms = frames.first().map(|f| f.offset_ms).unwrap_or(0);
    let duration_ms = end_ms.saturating_sub(first_ms);
    let dir = session.dir.clone();
    *slot = None;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(json!({
        "status": "completed",
        "file": crate::state::printable_path(&output_path),
        "size": size,
        "duration_ms": duration_ms,
        "frames": frames.len(),
        "frames_dropped": dropped,
        "frames_received": received,
        "capture_fps": if duration_ms > 0 {
            json!(((frames.len() as f64 / (duration_ms as f64 / 1000.0)) * 10.0).round() / 10.0)
        } else {
            Value::Null
        },
        "video_fps": fps,
        "ended_because": reason,
    }))
}

pub async fn status() -> Value {
    let slot = SESSION.lock().await;
    let Some(session) = slot.as_ref() else {
        return json!({ "status": "completed", "recording": false, "state": "idle" });
    };
    let elapsed_ms = match session.phase {
        Phase::Capturing => session.started.elapsed().as_millis() as u64,
        Phase::Ended { end_ms, .. } => end_ms,
    };
    let received = session.counters.received.load(Ordering::Relaxed);
    let (state, reason) = match session.phase {
        Phase::Capturing => ("recording", Value::Null),
        Phase::Ended { reason, .. } => ("capture_ended", json!(reason)),
    };
    json!({
        "status": "completed",
        "recording": matches!(session.phase, Phase::Capturing),
        "state": state,
        "source": session.source.as_str(),
        "elapsed_ms": elapsed_ms,
        "frames": received,
        "frames_dropped": session.counters.dropped.load(Ordering::Relaxed),
        "bytes": session.counters.bytes.load(Ordering::Relaxed),
        "max_seconds": session.options.max_seconds,
        "ended_because": reason,
    })
}

/// Is a screencast capturing right now? The toolbar shows it.
pub fn is_recording() -> bool {
    SINK.lock().map(|s| s.is_some()).unwrap_or(false)
}

/// Throw an unfinished recording away: the daemon is stopping.
pub async fn abort() {
    let mut slot = SESSION.lock().await;
    if let Some(mut session) = slot.take() {
        end_capture(&mut session, "daemon_stopping").await;
        let _ = tokio::fs::remove_dir_all(&session.dir).await;
    }
}

/// Frames of a recording a crashed daemon left behind: a day old or more.
async fn remove_stale_recordings() {
    let Ok(mut entries) = tokio::fs::read_dir(root_dir()).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let old = entry
            .metadata()
            .await
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if old {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}

async fn resolve_output(site: &str, output: Option<&str>) -> Result<PathBuf, Failure> {
    let name = output.filter(|o| !o.is_empty()).unwrap_or("screencast.mp4");
    let path = PathBuf::from(name);
    if path.is_absolute() {
        return Ok(path);
    }
    crate::tools::definitions::site_dir(site)
        .await
        .map(|dir| dir.join(path))
        .map_err(|e| Failure::new("ENCODE_FAILED", e.to_string()))
}

/// The `screencast` tool: `start`, `stop` or `status`.
pub async fn tool(args: &Value) -> Result<Value> {
    let site = args["site"].as_str().unwrap_or("default");
    let answer = match args["action"].as_str().unwrap_or("status") {
        "start" => match Options::from_args(args) {
            Ok(options) => start(site, options).await,
            Err(failure) => Err(failure),
        },
        "stop" => stop(site, args["output"].as_str()).await,
        "status" => Ok(status().await),
        other => Err(Failure::new(
            "INVALID_ACTION",
            format!("unknown action `{other}`: use start, stop or status"),
        )),
    };
    Ok(answer.unwrap_or_else(|failure| failure.json()))
}

#[cfg(test)]
mod tests;
