//! The Windows sink: Media Foundation's H.264 encoder through a sink writer.
//!
//! Software only (`MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` off): a GPU encoder
//! depends on the driver and on the session, and this must work the same on a
//! build server and on a remote desktop.
//!
//! The index (`moov`) is written after the data. `MF_MPEG4SINK_MOOV_BEFORE_MDAT`
//! is not used: with a file URL it fails with access denied at `Finalize`, and with
//! a read-write byte stream it "succeeds" and writes a file that no decoder reads.
//!
//! Windows N editions ship without the encoder until the Media Feature Pack is
//! installed. That shows up as `MF_E_TOPO_CODEC_NOT_FOUND` and is reported as such,
//! so the caller can fall back to ffmpeg.

use super::{nv12_len, Sink};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// What a Media Foundation call said, with its HRESULT.
fn explain(what: &str, e: windows::core::Error) -> String {
    if e.code() == MF_E_TOPO_CODEC_NOT_FOUND {
        return format!(
            "{what}: the Windows H.264 encoder is not installed (a Windows N edition needs the \
             Media Feature Pack) (0x{:08X})",
            e.code().0 as u32
        );
    }
    format!(
        "{what}: {} (0x{:08X})",
        e.message().trim(),
        e.code().0 as u32
    )
}

fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// The time of tick `index` at `fps`, in the 100 ns units Media Foundation uses.
fn tick_time(index: u64, fps: u32) -> i64 {
    (u128::from(index) * 10_000_000 / u128::from(fps.max(1))) as i64
}

/// COM for this thread, undone when it goes out of scope.
struct Com(bool);

impl Com {
    fn start() -> Com {
        // Fails with RPC_E_CHANGED_MODE when the thread is already set up another
        // way, which is fine: Media Foundation works in that mode too.
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Media Foundation for this process, shut down when it goes out of scope.
struct Platform;

impl Platform {
    fn start() -> Result<Platform, String> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|e| explain("cannot start Media Foundation", e))?;
        Ok(Platform)
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

struct MfSink {
    // Declared first so that it is dropped before Media Foundation and COM.
    writer: IMFSinkWriter,
    stream: u32,
    fps: u32,
    len: usize,
    _platform: Platform,
    _com: Com,
}

pub fn open(output: &Path, width: u32, height: u32, fps: u32) -> Result<Box<dyn Sink>, String> {
    let com = Com::start();
    let platform = Platform::start()?;
    let fail = |what: &str| {
        let what = what.to_string();
        move |e| explain(&what, e)
    };
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2).map_err(fail("cannot create attributes"))?;
        let attributes: IMFAttributes = attributes.ok_or("no attributes")?;
        attributes
            .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
            .map_err(fail("cannot choose mp4"))?;
        attributes
            .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)
            .map_err(fail("cannot choose the software encoder"))?;

        let wide: Vec<u16> = output.as_os_str().encode_wide().chain(Some(0)).collect();
        let writer = MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attributes)
            .map_err(fail("cannot create the mp4 file"))?;

        // What goes into the file: H.264, High profile, tagged BT.709 so that a
        // player does not have to guess. The bit rate is a ceiling for the
        // quality-based mode below.
        let bitrate = (u64::from(width) * u64::from(height) * u64::from(fps) / 10)
            .clamp(500_000, 20_000_000) as u32;
        let target = MFCreateMediaType().map_err(fail("cannot describe the video"))?;
        let set = |r: windows::core::Result<()>| r.map_err(fail("cannot describe the video"));
        set(target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
        set(target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))?;
        set(target.SetUINT32(&MF_MT_AVG_BITRATE, bitrate))?;
        set(target.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
        set(target.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)))?;
        set(target.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)))?;
        set(target.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)))?;
        set(target.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32))?;
        set(target.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32))?;
        set(target.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32))?;
        set(target.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32))?;
        set(target.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32))?;
        let stream = writer
            .AddStream(&target)
            .map_err(fail("cannot add the video stream"))?;

        // What comes in: NV12, the same size and rate, in the same colour space.
        let source = MFCreateMediaType().map_err(fail("cannot describe the frames"))?;
        let set = |r: windows::core::Result<()>| r.map_err(fail("cannot describe the frames"));
        set(source.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
        set(source.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))?;
        set(source.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
        set(source.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)))?;
        set(source.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)))?;
        set(source.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)))?;
        set(source.SetUINT32(&MF_MT_DEFAULT_STRIDE, width))?;
        set(source.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32))?;
        set(source.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32))?;

        // Quality-based rate control: the encoder spends bits where the picture
        // changes, which suits a screen that is mostly still.
        let mut parameters = None;
        MFCreateAttributes(&mut parameters, 2).map_err(fail("cannot create attributes"))?;
        let parameters: IMFAttributes = parameters.ok_or("no attributes")?;
        parameters
            .SetUINT32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_Quality.0 as u32,
            )
            .map_err(fail("cannot choose the rate control"))?;
        parameters
            .SetUINT32(&CODECAPI_AVEncCommonQuality, 70)
            .map_err(fail("cannot choose the quality"))?;
        writer
            .SetInputMediaType(stream, &source, &parameters)
            .map_err(fail("cannot start the H.264 encoder"))?;
        writer
            .BeginWriting()
            .map_err(fail("cannot start writing"))?;

        Ok(Box::new(MfSink {
            writer,
            stream,
            fps,
            len: nv12_len(width, height),
            _platform: platform,
            _com: com,
        }))
    }
}

impl Sink for MfSink {
    fn write(&mut self, index: u64, nv12: &[u8]) -> Result<(), String> {
        if nv12.len() != self.len {
            return Err(format!(
                "a frame has {} bytes, expected {}",
                nv12.len(),
                self.len
            ));
        }
        let fail = |what: &'static str| move |e| explain(what, e);
        unsafe {
            let buffer =
                MFCreateMemoryBuffer(self.len as u32).map_err(fail("cannot allocate a frame"))?;
            let mut data = std::ptr::null_mut();
            buffer
                .Lock(&mut data, None, None)
                .map_err(fail("cannot reach the frame buffer"))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), data, nv12.len());
            buffer
                .Unlock()
                .map_err(fail("cannot release the frame buffer"))?;
            buffer
                .SetCurrentLength(self.len as u32)
                .map_err(fail("cannot size the frame buffer"))?;
            let sample = MFCreateSample().map_err(fail("cannot create a sample"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(fail("cannot fill a sample"))?;
            let start = tick_time(index, self.fps);
            sample
                .SetSampleTime(start)
                .map_err(fail("cannot time a sample"))?;
            sample
                .SetSampleDuration(tick_time(index + 1, self.fps) - start)
                .map_err(fail("cannot time a sample"))?;
            self.writer
                .WriteSample(self.stream, &sample)
                .map_err(fail("cannot encode a frame"))
        }
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        unsafe { self.writer.Finalize() }.map_err(|e| explain("cannot finish the mp4 file", e))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{encode, Job};
    use crate::screencast::encode::FrameMeta;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_tick_is_its_index_over_the_rate_in_100_ns_units() {
        assert_eq!(super::tick_time(0, 30), 0);
        assert_eq!(super::tick_time(30, 30), 10_000_000);
        // No drift: 1/30 s is not a whole number of units, so the time is rounded
        // from the index, not added up.
        assert_eq!(super::tick_time(3, 30), 1_000_000);
        assert_eq!(super::tick_time(1, 30), 333_333);
        assert_eq!(super::tick_time(900_000, 30), 300_000_000_000);
    }

    /// (timescale, duration, has avc1) read from the mp4 boxes.
    fn inspect(bytes: &[u8]) -> (u64, u64, bool) {
        let find = |tag: &[u8]| bytes.windows(4).position(|w| w == tag);
        assert_eq!(&bytes[4..8], b"ftyp", "an mp4 starts with its file type");
        let mvhd = find(b"mvhd").expect("a movie header") + 4;
        let be32 = |at: usize| u64::from(u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()));
        let (timescale, duration) = if bytes[mvhd] == 1 {
            let d = u64::from_be_bytes(bytes[mvhd + 24..mvhd + 32].try_into().unwrap());
            (be32(mvhd + 20), d)
        } else {
            (be32(mvhd + 12), be32(mvhd + 16))
        };
        let (mdat, moov) = (
            find(b"mdat").expect("a data box"),
            find(b"moov").expect("an index"),
        );
        assert!(moov < mdat, "the index is before the data");
        (timescale, duration, find(b"avc1").is_some())
    }

    #[test]
    fn frames_become_an_h264_mp4_of_the_right_length_with_its_index_first() {
        let dir = std::env::temp_dir().join(format!("dalzzack-mf-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut frames = Vec::new();
        for (i, offset_ms) in [0u64, 150, 400, 900, 1300].into_iter().enumerate() {
            let image = image::RgbImage::from_fn(320, 240, |x, y| {
                image::Rgb([(x as usize * (i + 1)) as u8, y as u8, (i * 50) as u8])
            });
            let file = dir.join(format!("{offset_ms}.jpg"));
            image.save(&file).unwrap();
            frames.push(FrameMeta { offset_ms, file });
        }
        let output = dir.join("out.mp4");
        let job = Job {
            frames: &frames,
            end_ms: 2000,
            fps: 25,
            output: &output,
        };
        let result = encode(&job, &AtomicBool::new(false));
        if let Err(e) = &result {
            // A Windows N edition without the Media Feature Pack has no encoder.
            if e.contains("not installed") {
                eprintln!("skipped: {e}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        }
        result.unwrap();
        let bytes = std::fs::read(&output).unwrap();
        let (timescale, duration, avc1) = inspect(&bytes);
        let seconds = duration as f64 / timescale as f64;
        assert!((seconds - 2.0).abs() < 0.1, "2 s of video, got {seconds}");
        assert!(avc1, "the video is H.264");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_output_path_that_cannot_be_made_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("dalzzack-mf-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("0.jpg");
        image::RgbImage::new(64, 48).save(&file).unwrap();
        let frames = [FrameMeta { offset_ms: 0, file }];
        let output = dir.join("no-such-folder").join("out.mp4");
        let job = Job {
            frames: &frames,
            end_ms: 100,
            fps: 10,
            output: &output,
        };
        assert!(encode(&job, &AtomicBool::new(false)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
