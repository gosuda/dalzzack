//! `screencast`: record what the page shows as an mp4.
//!
//! `screencast start`, run any commands (scroll, click, navigate), then
//! `screencast stop --output a.mp4`. Frames come from the engine itself, page
//! only (never the window, the toolbar or the rest of the screen): CDP's
//! `Page.startScreencast` on the Chrome engine, and WebKit's own snapshot of the
//! page on macOS. They go to disk as they arrive, each with the time it arrived,
//! and `stop` encodes them so the video runs in real time. The encoder that the
//! OS has makes the mp4 ([`native`]: Media Foundation on Windows, AVFoundation on
//! macOS), and ffmpeg ([`encode`]) is the fallback when the OS has none or it
//! fails.
//!
//! One recording per daemon. It stops capturing by itself at `--max-seconds`
//! (the frames are kept until `stop`), so a forgotten recording cannot fill a
//! disk. When no encoder works `stop` fails *without* discarding the frames: fix
//! that (install ffmpeg) and call `stop` again.

pub mod encode;
pub mod native;
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
        // Only the macOS source (and the tests') run a task that `stop` ends.
        #[cfg(not(any(target_os = "macos", test)))]
        let _ = stop;
        match self {
            Source::Chrome => chrome_start(options).await.map(|_| None),
            #[cfg(target_os = "macos")]
            Source::WebKit => webkit::start(options, stop.clone()).await.map(Some),
            #[cfg(test)]
            Source::Scripted => {
                let _ = (options, stop);
                // A source that gives up at once, as the macOS one does when
                // WebKit stops giving snapshots.
                Ok(SCRIPTED_DIES
                    .load(Ordering::SeqCst)
                    .then(|| tokio::spawn(async {})))
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
        } else {
            // A full disk: the frame is lost like one the queue had no room for.
            counters.dropped.fetch_add(1, Ordering::Relaxed);
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
#[cfg(test)]
static SCRIPTED_DIES: AtomicBool = AtomicBool::new(false);

/// The folder of the recording in progress, for [`discard_files`]: an exit that
/// cannot wait for [`abort`] still throws the frames away.
static FILES: Mutex<Option<PathBuf>> = Mutex::new(None);

fn set_files(dir: Option<PathBuf>) {
    *FILES.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// Throw the frames of an unfinished recording away now, without waiting for
/// anything: the process is about to exit (the window was closed, the TTL ran
/// out) and nothing else would remove them.
pub fn discard_files() {
    let dir = FILES.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(dir) = dir else { return };
    // No more frames: a source may still be producing them, and the writer
    // would put one in the folder while it is being removed.
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    for _ in 0..3 {
        if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

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
            // The macOS source stops by itself when WebKit gives no snapshots.
            let source_gone = session
                .source_task
                .as_ref()
                .is_some_and(|task| task.is_finished());
            if over_time || over_size || source_gone {
                let reason = if over_time {
                    "max_seconds"
                } else if over_size {
                    "max_bytes"
                } else {
                    "source_failed"
                };
                end_capture(session, reason).await;
                tracing::info!("[screencast] capture ended: {reason}");
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

    // From here an exit that cannot wait still removes the folder.
    set_files(Some(dir.clone()));
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
            // Chrome may have started sending frames before the call timed out.
            source.stop().await;
            *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = writer.await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            set_files(None);
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
        set_files(None);
        return Err(Failure::new(
            "NO_FRAMES",
            "The recording captured no frames: the page did not paint while it ran. Nothing was written.",
        ));
    }
    // From here a failure keeps the frames, so the caller can fix it and retry.
    let choice = EncoderChoice::from_env()?;
    let output_path = resolve_output(site, output).await?;
    if let Some(parent) = output_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let fps = session.options.fps;
    let (encoder, ffmpeg_encoder) = make_video(
        choice,
        &frames,
        end_ms,
        fps,
        &output_path,
        &session.dir.join("frames.txt"),
    )
    .await?;

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
    set_files(None);
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
        "encoder": encoder,
        "ffmpeg_encoder": ffmpeg_encoder,
        "ended_because": reason,
    }))
}

/// Which encoder `stop` may use. `DALZZACK_ENCODER` is `native` (the OS encoder
/// only), `ffmpeg` (ffmpeg only) or not set: the OS encoder first, and ffmpeg
/// when the OS has none or it fails.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EncoderChoice {
    Auto,
    Native,
    Ffmpeg,
}

impl EncoderChoice {
    fn from_env() -> Result<EncoderChoice, Failure> {
        let value = std::env::var("DALZZACK_ENCODER").ok();
        EncoderChoice::parse(value.as_deref())
    }

    fn parse(value: Option<&str>) -> Result<EncoderChoice, Failure> {
        match value.map(str::trim) {
            None | Some("") | Some("auto") => Ok(EncoderChoice::Auto),
            Some("native") => Ok(EncoderChoice::Native),
            Some("ffmpeg") => Ok(EncoderChoice::Ffmpeg),
            Some(other) => Err(Failure::new(
                "INVALID_OPTION",
                format!(
                    "DALZZACK_ENCODER is `{other}`; use `native`, `ffmpeg` or `auto` (or leave \
                     it unset). The frames are kept."
                ),
            )),
        }
    }
}

const ENCODE_TIMEOUT: Duration = Duration::from_secs(600);

/// Make the mp4 and say which encoder made it: `native`, or `ffmpeg` and the name
/// of the H.264 encoder that it used.
async fn make_video(
    choice: EncoderChoice,
    frames: &[FrameMeta],
    end_ms: u64,
    fps: u32,
    output: &std::path::Path,
    list: &std::path::Path,
) -> Result<(&'static str, Option<&'static str>), Failure> {
    let attempt = if choice != EncoderChoice::Ffmpeg && native::SUPPORTED {
        Some(encode_native(frames, end_ms, fps, output).await)
    } else {
        None
    };
    let native_failure = match after_native(choice, native::SUPPORTED, attempt) {
        AfterNative::Done => return Ok(("native", None)),
        AfterNative::Fail(failure) => return Err(failure),
        AfterNative::UseFfmpeg { native_failure } => native_failure,
    };

    let Some(ffmpeg) = encode::find_ffmpeg() else {
        return Err(ffmpeg_not_found(frames.len(), native_failure.as_deref()));
    };
    match encode_ffmpeg(&ffmpeg, frames, end_ms, fps, output, list).await {
        Ok(codec) => Ok(("ffmpeg", Some(codec))),
        Err(mut failure) => {
            if let Some(detail) = native_failure {
                failure.message.push_str(&format!(
                    " (The built-in encoder had failed first: {detail}.)"
                ));
            }
            Err(failure)
        }
    }
}

/// What `stop` does once the OS encoder has been tried (or skipped).
#[derive(Debug)]
enum AfterNative {
    /// The OS encoder made the file.
    Done,
    /// Go on with ffmpeg. `native_failure` is why the OS encoder did not do it.
    UseFfmpeg {
        native_failure: Option<String>,
    },
    Fail(Failure),
}

/// `attempt` is the result of the OS encoder, or `None` when it was not tried.
fn after_native(
    choice: EncoderChoice,
    supported: bool,
    attempt: Option<Result<(), String>>,
) -> AfterNative {
    match (choice, attempt) {
        (EncoderChoice::Ffmpeg, _) => AfterNative::UseFfmpeg {
            native_failure: None,
        },
        (_, Some(Ok(()))) => AfterNative::Done,
        (EncoderChoice::Native, Some(Err(detail))) => AfterNative::Fail(Failure::new(
            "ENCODE_FAILED",
            format!(
                "the built-in encoder failed: {detail}. The frames are kept; `screencast stop` \
                 retries."
            ),
        )),
        (EncoderChoice::Native, None) => {
            debug_assert!(!supported);
            AfterNative::Fail(Failure::new(
                "ENCODE_FAILED",
                "DALZZACK_ENCODER=native, but this platform has no built-in encoder. The frames \
                 are kept.",
            ))
        }
        (EncoderChoice::Auto, Some(Err(detail))) => AfterNative::UseFfmpeg {
            native_failure: Some(detail),
        },
        (EncoderChoice::Auto, None) => AfterNative::UseFfmpeg {
            native_failure: None,
        },
    }
}

/// No ffmpeg, and the OS encoder did not make the file (it failed, or there is none).
fn ffmpeg_not_found(frames: usize, native_failure: Option<&str>) -> Failure {
    let tried = match native_failure {
        Some(detail) => format!("The built-in encoder failed ({detail}) and ffmpeg"),
        None => "ffmpeg is needed to make the mp4 here and it".to_string(),
    };
    Failure::new(
        "FFMPEG_NOT_FOUND",
        format!(
            "{tried} was not found by this daemon (it looks in the PATH and environment it was \
             started with, then in /opt/homebrew/bin, /usr/local/bin and /usr/bin). The {frames} \
             captured frames are kept: put ffmpeg in one of those folders (macOS: `brew install \
             ffmpeg`) and run `screencast stop` again. A new PATH or DALZZACK_FFMPEG reaches \
             only a daemon started after it is set, and stopping the daemon discards the frames."
        ),
    )
}

/// The OS encoder, on a blocking thread. When it is too slow it is told to stop
/// and awaited, so that nothing still writes the file when ffmpeg takes over.
async fn encode_native(
    frames: &[FrameMeta],
    end_ms: u64,
    fps: u32,
    output: &std::path::Path,
) -> Result<(), String> {
    let frames = frames.to_vec();
    let output = output.to_path_buf();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let mut work = tokio::task::spawn_blocking(move || {
        native::encode(
            &native::Job {
                frames: &frames,
                end_ms,
                fps,
                output: &output,
            },
            &flag,
        )
    });
    match tokio::time::timeout(ENCODE_TIMEOUT, &mut work).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(format!("the encoder stopped unexpectedly: {e}")),
        Err(_) => {
            cancel.store(true, Ordering::Relaxed);
            let _ = work.await;
            Err("the encoder did not finish within 10 minutes".to_string())
        }
    }
}

/// ffmpeg, with the best H.264 encoder that this build has. Returns its name.
async fn encode_ffmpeg(
    ffmpeg: &std::path::Path,
    frames: &[FrameMeta],
    end_ms: u64,
    fps: u32,
    output: &std::path::Path,
    list: &std::path::Path,
) -> Result<&'static str, Failure> {
    let Some(h264) = encode::probe_h264(ffmpeg).await else {
        return Err(Failure::new(
            "ENCODE_FAILED",
            format!(
                "{} has no H.264 encoder that dalzzack can use (it needs libx264, libopenh264, \
                 h264_videotoolbox or h264_mf). Use another ffmpeg build. The frames are kept; \
                 `screencast stop` retries.",
                ffmpeg.display()
            ),
        ));
    };
    tokio::fs::write(list, encode::concat_list(frames, end_ms, fps))
        .await
        .map_err(|e| Failure::new("ENCODE_FAILED", format!("cannot write the frame list: {e}")))?;

    let ticks = encode::plan(frames, end_ms, fps).len();
    let run = tokio::process::Command::new(ffmpeg)
        .args(encode::ffmpeg_args(list, output, fps, ticks, h264))
        .kill_on_drop(true)
        .output();
    let result = tokio::time::timeout(ENCODE_TIMEOUT, run).await;
    if matches!(&result, Ok(Ok(o)) if o.status.success()) {
        return Ok(h264.name());
    }
    let detail = match result {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
        Ok(Err(e)) => e.to_string(),
        Err(_) => "ffmpeg did not finish within 10 minutes".to_string(),
    };
    Err(Failure::new(
        "ENCODE_FAILED",
        format!("ffmpeg failed: {detail}. The frames are kept; `screencast stop` retries."),
    ))
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
    let dropped = session.counters.dropped.load(Ordering::Relaxed);
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
        // Kept frames, as `stop` counts them; `frames_received` includes the dropped.
        "frames": received.saturating_sub(dropped),
        "frames_received": received,
        "frames_dropped": dropped,
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
        set_files(None);
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
