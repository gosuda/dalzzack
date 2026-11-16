//! The macOS sink: AVFoundation's `AVAssetWriter` writing H.264 into an mp4.
//!
//! Pictures go in as NV12 pixel buffers taken from the adaptor's own pool. The
//! writer is not told that the data is real time, so it takes as long as it needs
//! for each frame (`isReadyForMoreMediaData` is waited on) and drops nothing.
//!
//! The H.264 profile and level are chosen by the system, and the colour is tagged
//! BT.709 like the Windows sink and the ffmpeg path.

use super::{nv12_len, Sink};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::Message;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey,
    AVVideoCodecTypeH264, AVVideoColorPrimariesKey, AVVideoColorPrimaries_ITU_R_709_2,
    AVVideoColorPropertiesKey, AVVideoCompressionPropertiesKey, AVVideoHeightKey,
    AVVideoProfileLevelH264HighAutoLevel, AVVideoProfileLevelKey, AVVideoTransferFunctionKey,
    AVVideoTransferFunction_ITU_R_709_2, AVVideoWidthKey, AVVideoYCbCrMatrixKey,
    AVVideoYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::CMTime;
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The longest that the writer may take to be ready for one more frame, or to
/// finish the file.
const PATIENCE: Duration = Duration::from_secs(60);

struct AvSink {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    fps: i32,
    width: usize,
    height: usize,
}

fn text(s: &CFString) -> &NSString {
    // CFString and NSString are the same object (toll-free bridged).
    unsafe { &*(s as *const CFString as *const NSString) }
}

fn number(n: usize) -> Retained<AnyObject> {
    let n: Retained<NSNumber> = NSNumber::new_u64(n as u64);
    unsafe { Retained::cast_unchecked(n) }
}

fn string(s: &NSString) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(s.retain()) }
}

fn dictionary(
    entries: &[(&NSString, Retained<AnyObject>)],
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = entries.iter().map(|(k, _)| *k).collect();
    let values: Vec<Retained<AnyObject>> = entries.iter().map(|(_, v)| v.clone()).collect();
    NSDictionary::from_retained_objects(&keys, &values)
}

fn dictionary_object(d: Retained<NSDictionary<NSString, AnyObject>>) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(d) }
}

fn failure(what: &str, writer: &AVAssetWriter) -> String {
    match unsafe { writer.error() } {
        Some(e) => format!("{what}: {}", e.localizedDescription()),
        None => what.to_string(),
    }
}

pub fn open(output: &Path, width: u32, height: u32, fps: u32) -> Result<Box<dyn Sink>, String> {
    // AVAssetWriter refuses to write over a file.
    let _ = std::fs::remove_file(output);
    let path = output.to_str().ok_or("the output path is not text")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let file_type = unsafe { AVFileTypeMPEG4 }.ok_or("AVFoundation has no mp4 file type")?;
    let writer = unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type) }
        .map_err(|e| format!("cannot create the mp4 file: {}", e.localizedDescription()))?;

    let bitrate = (u64::from(width) * u64::from(height) * u64::from(fps) / 10)
        .clamp(500_000, 20_000_000) as usize;
    let settings = unsafe {
        let codec = AVVideoCodecTypeH264.ok_or("this macOS has no H.264 encoder")?;
        let compression = dictionary(&[
            (
                AVVideoAverageBitRateKey.ok_or("no bit rate key")?,
                number(bitrate),
            ),
            (
                AVVideoProfileLevelKey.ok_or("no profile key")?,
                string(AVVideoProfileLevelH264HighAutoLevel.ok_or("no profile")?),
            ),
        ]);
        let colour = dictionary(&[
            (
                AVVideoColorPrimariesKey.ok_or("no colour key")?,
                string(AVVideoColorPrimaries_ITU_R_709_2.ok_or("no colour")?),
            ),
            (
                AVVideoTransferFunctionKey.ok_or("no colour key")?,
                string(AVVideoTransferFunction_ITU_R_709_2.ok_or("no colour")?),
            ),
            (
                AVVideoYCbCrMatrixKey.ok_or("no colour key")?,
                string(AVVideoYCbCrMatrix_ITU_R_709_2.ok_or("no colour")?),
            ),
        ]);
        dictionary(&[
            (AVVideoCodecKey.ok_or("no codec key")?, string(codec)),
            (
                AVVideoWidthKey.ok_or("no width key")?,
                number(width as usize),
            ),
            (
                AVVideoHeightKey.ok_or("no height key")?,
                number(height as usize),
            ),
            (
                AVVideoColorPropertiesKey.ok_or("no colour key")?,
                dictionary_object(colour),
            ),
            (
                AVVideoCompressionPropertiesKey.ok_or("no compression key")?,
                dictionary_object(compression),
            ),
        ])
    };
    let media_type = unsafe { AVMediaTypeVideo }.ok_or("AVFoundation has no video type")?;
    let input = unsafe {
        AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            media_type,
            Some(&settings),
        )
    };
    unsafe { input.setExpectsMediaDataInRealTime(false) };

    let attributes = dictionary(&[
        (
            text(unsafe { kCVPixelBufferPixelFormatTypeKey }),
            number(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as usize),
        ),
        (
            text(unsafe { kCVPixelBufferWidthKey }),
            number(width as usize),
        ),
        (
            text(unsafe { kCVPixelBufferHeightKey }),
            number(height as usize),
        ),
    ]);
    let adaptor = unsafe {
        AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
            &input,
            Some(&attributes),
        )
    };

    unsafe {
        // The index goes in front of the data, so that a player can start early.
        writer.setShouldOptimizeForNetworkUse(true);
        if !writer.canAddInput(&input) {
            return Err("the writer does not take an H.264 video input".to_string());
        }
        writer.addInput(&input);
        if !writer.startWriting() {
            return Err(failure("cannot start writing", &writer));
        }
        writer.startSessionAtSourceTime(CMTime::new(0, fps as i32));
    }
    Ok(Box::new(AvSink {
        writer,
        input,
        adaptor,
        fps: fps as i32,
        width: width as usize,
        height: height as usize,
    }))
}

impl Drop for AvSink {
    /// A writer that is still writing when it is released (a frame failed, or the
    /// file did not finish in time) must be cancelled: AVFoundation does not accept
    /// that it is just let go, and an exception from it would end the process.
    fn drop(&mut self) {
        if unsafe { self.writer.status() } == AVAssetWriterStatus::Writing {
            unsafe { self.writer.cancelWriting() };
        }
    }
}

impl AvSink {
    /// Wait until the writer takes one more frame.
    fn wait_ready(&self) -> Result<(), String> {
        let started = Instant::now();
        while !unsafe { self.input.isReadyForMoreMediaData() } {
            if unsafe { self.writer.status() } == AVAssetWriterStatus::Failed {
                return Err(failure("the writer failed", &self.writer));
            }
            if started.elapsed() > PATIENCE {
                return Err("the writer did not take a frame within 60 seconds".to_string());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

impl Sink for AvSink {
    fn write(&mut self, index: u64, nv12: &[u8]) -> Result<(), String> {
        if nv12.len() != nv12_len(self.width as u32, self.height as u32) {
            return Err(format!(
                "a frame has {} bytes, expected {}",
                nv12.len(),
                nv12_len(self.width as u32, self.height as u32)
            ));
        }
        self.wait_ready()?;
        let pool = unsafe { self.adaptor.pixelBufferPool() }
            .ok_or("the writer has no pixel buffer pool")?;
        let mut raw = std::ptr::null_mut();
        let made =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        let raw = NonNull::new(raw)
            .filter(|_| made == kCVReturnSuccess)
            .ok_or_else(|| format!("cannot get a pixel buffer (CVReturn {made})"))?;
        // The pool hands the buffer over with one reference that we own.
        let buffer = unsafe { CFRetained::from_raw(raw) };

        let none = CVPixelBufferLockFlags(0);
        if unsafe { CVPixelBufferLockBaseAddress(&buffer, none) } != kCVReturnSuccess {
            return Err("cannot reach the pixel buffer".to_string());
        }
        let (luma, chroma) = nv12.split_at(self.width * self.height);
        unsafe {
            copy_plane(&buffer, 0, luma, self.width, self.height);
            copy_plane(&buffer, 1, chroma, self.width, self.height / 2);
            CVPixelBufferUnlockBaseAddress(&buffer, none);
        }
        let appended = unsafe {
            self.adaptor.appendPixelBuffer_withPresentationTime(
                &buffer,
                CMTime::new(index as i64, self.fps),
            )
        };
        if appended {
            Ok(())
        } else {
            Err(failure("cannot append a frame", &self.writer))
        }
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        unsafe { self.input.markAsFinished() };
        let (done, wait) = mpsc::channel();
        let block = RcBlock::new(move || {
            let _ = done.send(());
        });
        unsafe { self.writer.finishWritingWithCompletionHandler(&block) };
        wait.recv_timeout(PATIENCE)
            .map_err(|_| "the writer did not finish within 60 seconds".to_string())?;
        if unsafe { self.writer.status() } == AVAssetWriterStatus::Completed {
            Ok(())
        } else {
            Err(failure("cannot finish the mp4 file", &self.writer))
        }
    }
}

/// Copy `rows` rows of `row_bytes` bytes into plane `plane`, whose rows may be
/// padded to a longer stride.
unsafe fn copy_plane(
    buffer: &objc2_core_video::CVPixelBuffer,
    plane: usize,
    from: &[u8],
    row_bytes: usize,
    rows: usize,
) {
    let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane) as *mut u8;
    let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
    for row in 0..rows {
        std::ptr::copy_nonoverlapping(
            from.as_ptr().add(row * row_bytes),
            base.add(row * stride),
            row_bytes,
        );
    }
}
