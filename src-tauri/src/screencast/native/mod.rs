//! The encoder that the OS has: timestamped JPEG frames in, an H.264 mp4 out, with
//! no ffmpeg to install.
//!
//! Windows uses the Media Foundation H.264 encoder (the software one, so a machine
//! with no GPU, or a remote session, works the same) and macOS uses AVFoundation.
//! Everything that does not depend on the OS is here: which frame is on screen at
//! each tick of a constant frame rate ([`plan`]), the colour conversion
//! ([`rgb_to_nv12`]) and the loop that feeds a [`Sink`]. A sink is the part that
//! is different on each OS.
//!
//! The picture is converted with the BT.709 matrix, limited range. That is what
//! players assume for an untagged HD video, and the frames are sRGB screen content.

use super::encode::{plan, FrameMeta};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
mod av;
mod faststart;
#[cfg(windows)]
mod mf;

/// Does this platform have an OS encoder that dalzzack can drive?
pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

/// How many frames are decoded and converted at once (each on its own thread).
const BATCH: usize = 8;

/// Where the converted pictures go, one at a time, in order. `index` is the
/// frame's place at the constant rate: its time is `index / fps`.
trait Sink {
    fn write(&mut self, index: u64, nv12: &[u8]) -> Result<(), String>;
    /// Close the file. A sink that is dropped before this leaves a broken file.
    fn finish(self: Box<Self>) -> Result<(), String>;
}

pub struct Job<'a> {
    pub frames: &'a [FrameMeta],
    /// The moment capture ended, in the same milliseconds as the frames.
    pub end_ms: u64,
    pub fps: u32,
    pub output: &'a Path,
}

/// Make the video. Blocking: call it from a blocking thread. `cancel` stops it
/// between batches. A failure leaves no half-written file behind.
pub fn encode(job: &Job, cancel: &AtomicBool) -> Result<(), String> {
    encode_with(job, cancel, open_sink)
}

fn open_sink(output: &Path, width: u32, height: u32, fps: u32) -> Result<Box<dyn Sink>, String> {
    #[cfg(windows)]
    {
        mf::open(output, width, height, fps)
    }
    #[cfg(target_os = "macos")]
    {
        av::open(output, width, height, fps)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (output, width, height, fps);
        Err("this platform has no built-in encoder".to_string())
    }
}

fn encode_with<F>(job: &Job, cancel: &AtomicBool, open: F) -> Result<(), String>
where
    F: FnOnce(&Path, u32, u32, u32) -> Result<Box<dyn Sink>, String>,
{
    let result = run(job, cancel, open);
    if result.is_err() {
        let _ = std::fs::remove_file(job.output);
    }
    result
}

fn run<F>(job: &Job, cancel: &AtomicBool, open: F) -> Result<(), String>
where
    F: FnOnce(&Path, u32, u32, u32) -> Result<Box<dyn Sink>, String>,
{
    let first = job.frames.first().ok_or("there are no frames to encode")?;
    let (src_w, src_h) = image::image_dimensions(&first.file)
        .map_err(|e| format!("cannot read frame {}: {e}", first.file.display()))?;
    // H.264 in yuv420p needs both sides even.
    let (w, h) = (src_w & !1, src_h & !1);
    if w == 0 || h == 0 {
        return Err(format!(
            "the frames are {src_w}x{src_h}: too small to encode"
        ));
    }

    let ticks = plan(job.frames, job.end_ms, job.fps);
    // Each picture is converted once, however many ticks it stays on screen.
    let mut shown: Vec<usize> = ticks.clone();
    shown.dedup();

    let mut sink = open(job.output, w, h, job.fps)?;
    let mut next_tick = 0usize;
    for batch in shown.chunks(BATCH) {
        if cancel.load(Ordering::Relaxed) {
            return Err("the encoding was cancelled".to_string());
        }
        let files: Vec<&Path> = batch
            .iter()
            .map(|&i| job.frames[i].file.as_path())
            .collect();
        let pictures = convert_batch(&files, w, h);
        for (&frame, picture) in batch.iter().zip(pictures) {
            let picture = picture?;
            while next_tick < ticks.len() && ticks[next_tick] == frame {
                sink.write(next_tick as u64, &picture)?;
                next_tick += 1;
            }
        }
    }
    sink.finish()?;
    // Best effort: a file with its index at the end plays everywhere. It only cannot
    // start before the whole file is there.
    if let Err(e) = faststart::faststart(job.output) {
        tracing::warn!("cannot move the mp4 index to the front: {e}");
    }
    Ok(())
}

/// Decode and convert the files at the same time, one thread for each. The
/// results are in the order of `files`.
fn convert_batch(files: &[&Path], width: u32, height: u32) -> Vec<Result<Vec<u8>, String>> {
    std::thread::scope(|scope| {
        let workers: Vec<_> = files
            .iter()
            .map(|&file| scope.spawn(move || picture(file, width, height)))
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|_| Err("a frame converter stopped".to_string()))
            })
            .collect()
    })
}

/// One frame file as NV12 at exactly `width` x `height`. A frame of another size
/// (the page was resized while recording) is scaled to it; one that is only one
/// pixel larger (an odd side) is cropped.
fn picture(file: &Path, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let decoded = image::open(file)
        .map_err(|e| format!("cannot read frame {}: {e}", file.display()))?
        .to_rgb8();
    let near = |have: u32, want: u32| have >= want && have - want <= 1;
    let decoded = if near(decoded.width(), width) && near(decoded.height(), height) {
        decoded
    } else {
        image::imageops::resize(
            &decoded,
            width,
            height,
            image::imageops::FilterType::Triangle,
        )
    };
    let mut nv12 = vec![0u8; nv12_len(width, height)];
    rgb_to_nv12(
        decoded.as_raw(),
        decoded.width() as usize,
        width as usize,
        height as usize,
        &mut nv12,
    );
    Ok(nv12)
}

// BT.709, limited range (luma 16-235, chroma 16-240), in 15-bit fixed point. The
// chroma rows add up to exactly zero, so white, grey and black have no colour.
const SHIFT: i32 = 15;
const HALF: i32 = 1 << (SHIFT - 1);
const Y_R: i32 = 5983;
const Y_G: i32 = 20128;
const Y_B: i32 = 2032;
const U_R: i32 = -3299;
const U_G: i32 = -11094;
const U_B: i32 = 14393;
const V_R: i32 = 14393;
const V_G: i32 = -13074;
const V_B: i32 = -1319;

/// Bytes in an NV12 picture: the luma plane, then the interleaved chroma plane
/// at half the width and height.
pub fn nv12_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

/// RGB (3 bytes a pixel, `src_width` pixels a row) to NV12, BT.709, limited
/// range, for a `width` x `height` picture with even sides. A chroma value is the
/// average of its 2x2 pixels.
pub fn rgb_to_nv12(rgb: &[u8], src_width: usize, width: usize, height: usize, out: &mut [u8]) {
    debug_assert!(
        width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && out.len() == nv12_len(width as u32, height as u32)
    );
    let stride = src_width * 3;
    let (luma, chroma) = out.split_at_mut(width * height);
    for pair in 0..height / 2 {
        let top = &rgb[pair * 2 * stride..][..width * 3];
        let bottom = &rgb[(pair * 2 + 1) * stride..][..width * 3];
        let (luma_top, luma_bottom) = luma[pair * 2 * width..][..2 * width].split_at_mut(width);
        for (row, luma_row) in [(top, luma_top), (bottom, luma_bottom)] {
            let (pixels, _) = row.as_chunks::<3>();
            for (px, y) in pixels.iter().zip(luma_row.iter_mut()) {
                let (r, g, b) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
                *y = (((Y_R * r + Y_G * g + Y_B * b + HALF) >> SHIFT) + 16) as u8;
            }
        }
        let chroma_row = &mut chroma[pair * width..][..width];
        let (pairs, _) = chroma_row.as_chunks_mut::<2>();
        for (x, uv) in pairs.iter_mut().enumerate() {
            let (a, b) = (&top[x * 6..][..6], &bottom[x * 6..][..6]);
            let sum = |c: usize| {
                (i32::from(a[c]) + i32::from(a[c + 3]) + i32::from(b[c]) + i32::from(b[c + 3]) + 2)
                    / 4
            };
            let (r, g, b) = (sum(0), sum(1), sum(2));
            uv[0] = (((U_R * r + U_G * g + U_B * b + HALF) >> SHIFT) + 128).clamp(16, 240) as u8;
            uv[1] = (((V_R * r + V_G * g + V_B * b + HALF) >> SHIFT) + 128).clamp(16, 240) as u8;
        }
    }
}

#[cfg(test)]
mod tests;
