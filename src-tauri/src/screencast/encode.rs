//! Turning timestamped frames into an mp4: the ffmpeg side of `screencast`.
//!
//! Frames arrive when the page paints, not at a steady rate (a still page sends
//! almost none, a scroll sends many), so a video that played them one after the
//! other would run at the wrong speed. Each frame is therefore given the time it
//! was on screen (until the next one), through ffmpeg's concat demuxer, and the
//! result is resampled to a constant frame rate. That keeps the video in real
//! time whatever the capture rate was.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One captured frame: where it is and when it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMeta {
    /// Milliseconds after the recording started.
    pub offset_ms: u64,
    pub file: PathBuf,
}

/// An ffmpeg concat list: every frame with the time it stays on screen. The last
/// frame stays until `end_ms` (the moment capture ended, so a page that stopped
/// changing is held, not cut), and at least one frame's time at `fps`.
pub fn concat_list(frames: &[FrameMeta], end_ms: u64, fps: u32) -> String {
    let min_last = (1000 / u64::from(fps.max(1))).max(1);
    let mut out = String::from("ffconcat version 1.0\n");
    let quoted = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
    for (i, frame) in frames.iter().enumerate() {
        let next = frames.get(i + 1).map(|f| f.offset_ms).unwrap_or(end_ms);
        let hold_ms = next.saturating_sub(frame.offset_ms);
        let hold_ms = if i + 1 == frames.len() {
            hold_ms.max(min_last)
        } else {
            hold_ms.max(1)
        };
        out.push_str(&format!("file '{}'\n", quoted(&frame.file)));
        out.push_str("option framerate 1000\n");
        out.push_str(&format!("duration {:.3}\n", hold_ms as f64 / 1000.0));
    }
    // The concat demuxer ignores the duration of the last entry unless its file
    // is named once more.
    if let Some(last) = frames.last() {
        out.push_str(&format!("file '{}'\n", quoted(&last.file)));
        out.push_str("option framerate 1000\n");
    }
    out
}

/// For every tick of a constant frame rate, the index of the frame that is on
/// screen: the latest one that arrived at or before that tick. The video starts at
/// the first frame and ends when capture ended, and a page that stopped changing
/// is held to the end. It is never shorter than one frame time.
///
/// Both encoders follow this plan: the OS encoder writes exactly these ticks, and
/// ffmpeg is limited to as many frames (`-frames:v`). Without that limit ffmpeg
/// ran one hold of the last frame too long.
pub fn plan(frames: &[FrameMeta], end_ms: u64, fps: u32) -> Vec<usize> {
    let Some(first) = frames.first() else {
        return Vec::new();
    };
    let fps = u64::from(fps.max(1));
    let one_frame_ms = (1000 / fps).max(1);
    let span_ms = end_ms.saturating_sub(first.offset_ms).max(one_frame_ms);
    let count = (span_ms * fps).div_ceil(1000).max(1);
    let mut at = 0usize;
    (0..count)
        .map(|tick| {
            let now = first.offset_ms + tick * 1000 / fps;
            while at + 1 < frames.len() && frames[at + 1].offset_ms <= now {
                at += 1;
            }
            at
        })
        .collect()
}

/// An H.264 encoder that an ffmpeg build can have. A full (GPL) build has
/// libx264. A small (LGPL) build has no libx264 and brings libopenh264, or uses the
/// encoder of the OS, so a small build is enough when we pick from what it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264 {
    X264,
    OpenH264,
    VideoToolbox,
    MediaFoundation,
}

impl H264 {
    /// In the order of preference: smallest file first, and the OS encoders last.
    /// Hardware encoders (NVENC, QSV, AMF) are left out: they need the hardware.
    const ALL: [H264; 4] = [
        H264::X264,
        H264::OpenH264,
        H264::VideoToolbox,
        H264::MediaFoundation,
    ];

    pub fn name(self) -> &'static str {
        match self {
            H264::X264 => "libx264",
            H264::OpenH264 => "libopenh264",
            H264::VideoToolbox => "h264_videotoolbox",
            H264::MediaFoundation => "h264_mf",
        }
    }

    fn options(self) -> &'static [&'static str] {
        match self {
            H264::X264 => &["-preset", "veryfast", "-crf", "23"],
            H264::OpenH264 => &["-b:v", "4M"],
            H264::VideoToolbox => &["-b:v", "4M"],
            H264::MediaFoundation => &["-rate_control", "quality", "-quality", "70"],
        }
    }
}

/// The best H.264 encoder that the output of `ffmpeg -hide_banner -encoders` lists.
pub fn pick_h264(listing: &str) -> Option<H264> {
    let names: Vec<&str> = listing
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let flags = words.next()?;
            let name = words.next()?;
            // The first letter of the flags is the kind: `V` is a video encoder.
            flags.starts_with('V').then_some(name)
        })
        .collect();
    H264::ALL.into_iter().find(|e| names.contains(&e.name()))
}

/// ffmpeg's arguments: H.264 in yuv420p (what every player and editor takes),
/// both sides even (yuv420p needs it), a constant `fps` of exactly `ticks` frames
/// (see [`plan`]), and BT.709 colour, the same as the OS encoder makes.
pub fn ffmpeg_args(
    list: &Path,
    output: &Path,
    fps: u32,
    ticks: usize,
    encoder: H264,
) -> Vec<String> {
    // `out_range=tv`: JPEG is full-range, and without the conversion ffmpeg keeps
    // it as yuvj420p, which some players and editors misread. `out_color_matrix`
    // and the tags say which colours these are: without them a player guesses BT.709
    // for HD video, and ffmpeg's own default matrix (BT.601) shifts the colours.
    let filter = format!(
        "fps={fps},scale=trunc(iw/2)*2:trunc(ih/2)*2:out_range=tv:out_color_matrix=bt709,\
         format=yuv420p"
    );
    let mut args: Vec<String> = [
        "-y",
        "-v",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &list.to_string_lossy(),
        "-vf",
        &filter,
        "-frames:v",
        &ticks.to_string(),
        "-c:v",
        encoder.name(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(encoder.options().iter().map(|s| s.to_string()));
    args.extend(
        [
            // JPEG frames are full-range, which ffmpeg would keep as yuvj420p; players
            // and editors expect plain yuv420p.
            "-pix_fmt",
            "yuv420p",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
            "-movflags",
            "+faststart",
            &output.to_string_lossy(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args
}

/// The best H.264 encoder in this ffmpeg, or `None` when it has none (or cannot
/// be run).
pub async fn probe_h264(ffmpeg: &Path) -> Option<H264> {
    let run = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), run)
        .await
        .ok()?
        .ok()?;
    pick_h264(&String::from_utf8_lossy(&output.stdout))
}

/// Where ffmpeg is: `DALZZACK_FFMPEG`, else `PATH`, else winget's links folder on
/// Windows, else the usual install folders.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("DALZZACK_FFMPEG").map(PathBuf::from) {
        return explicit.is_file().then_some(explicit);
    }
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let local_app_data = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        None
    };
    candidate_folders(std::env::var_os("PATH"), local_app_data)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The folders ffmpeg may be in, in the order to look. A daemon's `PATH` is only as
/// new as the daemon, so an ffmpeg installed after it started is found through
/// winget's links folder, where winget puts the command's shim.
fn candidate_folders(path: Option<OsString>, local_app_data: Option<PathBuf>) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(local) = local_app_data {
        folders.push(local.join("Microsoft").join("WinGet").join("Links"));
    }
    for usual in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        folders.push(PathBuf::from(usual));
    }
    folders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winget_links_are_searched_after_the_path_and_before_the_usual_folders() {
        let folders = candidate_folders(Some(OsString::from("/a")), Some(PathBuf::from("/local")));
        let links = PathBuf::from("/local")
            .join("Microsoft")
            .join("WinGet")
            .join("Links");
        assert_eq!(folders[0], PathBuf::from("/a"));
        assert_eq!(folders[1], links);
        assert_eq!(folders[2], PathBuf::from("/opt/homebrew/bin"));
    }

    #[test]
    fn no_local_app_data_means_no_winget_folder() {
        let folders = candidate_folders(Some(OsString::from("/a")), None);
        assert_eq!(folders[0], PathBuf::from("/a"));
        assert_eq!(folders[1], PathBuf::from("/opt/homebrew/bin"));
    }

    fn frame(offset_ms: u64, name: &str) -> FrameMeta {
        FrameMeta {
            offset_ms,
            file: PathBuf::from(format!("/tmp/f/{name}.jpg")),
        }
    }

    #[test]
    fn every_frame_stays_until_the_next_one() {
        let list = concat_list(
            &[frame(0, "a"), frame(400, "b"), frame(1000, "c")],
            1500,
            30,
        );
        assert_eq!(
            list,
            "ffconcat version 1.0\n\
             file '/tmp/f/a.jpg'\noption framerate 1000\nduration 0.400\n\
             file '/tmp/f/b.jpg'\noption framerate 1000\nduration 0.600\n\
             file '/tmp/f/c.jpg'\noption framerate 1000\nduration 0.500\n\
             file '/tmp/f/c.jpg'\noption framerate 1000\n"
        );
    }

    #[test]
    fn a_page_that_stopped_changing_is_held_to_the_end() {
        // One frame, then nothing for 4 seconds: the video is 4 seconds long.
        let list = concat_list(&[frame(100, "only")], 4100, 30);
        assert!(list.contains("duration 4.000"), "{list}");
    }

    #[test]
    fn the_last_frame_is_never_shorter_than_one_frame_time() {
        let list = concat_list(&[frame(0, "a"), frame(500, "b")], 500, 25);
        assert!(
            list.contains("file '/tmp/f/b.jpg'\noption framerate 1000\nduration 0.040"),
            "{list}"
        );
    }

    #[test]
    fn two_frames_at_the_same_instant_do_not_make_a_zero_length_entry() {
        let list = concat_list(&[frame(10, "a"), frame(10, "b")], 600, 30);
        assert!(
            list.contains("file '/tmp/f/a.jpg'\noption framerate 1000\nduration 0.001"),
            "{list}"
        );
    }

    #[test]
    fn a_quote_in_a_path_cannot_end_the_file_line() {
        let f = FrameMeta {
            offset_ms: 0,
            file: PathBuf::from("/tmp/it's/a.jpg"),
        };
        assert!(concat_list(&[f], 100, 30).contains(r"file '/tmp/it'\''s/a.jpg'"));
    }

    #[test]
    fn no_frames_make_a_list_with_no_entries() {
        assert_eq!(concat_list(&[], 1000, 30), "ffconcat version 1.0\n");
    }

    #[test]
    fn the_video_is_even_sided_h264_at_a_constant_rate() {
        let args = ffmpeg_args(
            Path::new("/t/list.txt"),
            Path::new("/t/out.mp4"),
            24,
            96,
            H264::X264,
        );
        let joined = args.join(" ");
        assert!(
            joined.contains("-f concat -safe 0 -i /t/list.txt"),
            "{joined}"
        );
        assert!(
            joined.contains(
                "fps=24,scale=trunc(iw/2)*2:trunc(ih/2)*2:out_range=tv:out_color_matrix=bt709,\
                 format=yuv420p"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("-frames:v 96 -c:v libx264 -preset veryfast -crf 23"),
            "{joined}"
        );
        assert!(
            joined.ends_with("-movflags +faststart /t/out.mp4"),
            "{joined}"
        );
        assert!(joined.contains("-pix_fmt yuv420p"), "{joined}");
    }

    #[test]
    fn the_colours_are_converted_and_tagged_as_bt709() {
        // The matrix of the conversion and the tag must agree, or the colours shift.
        let joined = ffmpeg_args(
            Path::new("/t/l.txt"),
            Path::new("/t/o.mp4"),
            30,
            1,
            H264::OpenH264,
        )
        .join(" ");
        assert!(joined.contains("out_color_matrix=bt709"), "{joined}");
        assert!(
            joined.contains(
                "-colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv"
            ),
            "{joined}"
        );
    }

    #[test]
    fn each_encoder_gets_its_own_options_and_no_other() {
        let args = |e| ffmpeg_args(Path::new("l"), Path::new("o"), 30, 1, e).join(" ");
        assert!(args(H264::OpenH264).contains("-c:v libopenh264 -b:v 4M"));
        assert!(args(H264::VideoToolbox).contains("-c:v h264_videotoolbox -b:v 4M"));
        assert!(
            args(H264::MediaFoundation).contains("-c:v h264_mf -rate_control quality -quality 70")
        );
        for other in [H264::OpenH264, H264::VideoToolbox, H264::MediaFoundation] {
            assert!(!args(other).contains("-crf"), "{other:?} has no -crf");
            assert!(!args(other).contains("-preset"), "{other:?} has no -preset");
        }
    }

    // The lines of `ffmpeg -encoders`, cut down.
    const FULL_BUILD: &str = " V....D libx264              libx264 H.264 / AVC (codec h264)\n \
         V....D h264_mf              H264 via MediaFoundation (codec h264)\n \
         V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)\n \
         A....D aac                  AAC (Advanced Audio Coding)\n";
    const SMALL_BUILD: &str = " V....D libopenh264          OpenH264 H.264 / AVC (codec h264)\n \
         V....D h264_amf             AMD AMF H.264 Encoder (codec h264)\n \
         V....D h264_mf              H264 via MediaFoundation (codec h264)\n";
    const OS_ONLY_BUILD: &str = " V....D h264_nvenc           NVIDIA NVENC H.264 encoder\n \
         V....D h264_videotoolbox    VideoToolbox H.264 Encoder (codec h264)\n";

    #[test]
    fn the_best_encoder_that_the_build_has_is_picked() {
        assert_eq!(pick_h264(FULL_BUILD), Some(H264::X264));
        assert_eq!(pick_h264(SMALL_BUILD), Some(H264::OpenH264));
        assert_eq!(pick_h264(OS_ONLY_BUILD), Some(H264::VideoToolbox));
        assert_eq!(
            pick_h264(" V....D h264_mf  x\n"),
            Some(H264::MediaFoundation)
        );
    }

    #[test]
    fn a_build_with_no_usable_h264_encoder_gives_none() {
        // Hardware encoders alone are not enough, an audio line named like an
        // encoder is not a video encoder, and an empty listing has nothing.
        assert_eq!(
            pick_h264(" V....D h264_nvenc  NVENC\n V....D h264_qsv  QSV\n"),
            None
        );
        assert_eq!(pick_h264(" A....D libx264  not video\n"), None);
        assert_eq!(pick_h264(""), None);
    }

    #[test]
    fn every_tick_shows_the_latest_frame_that_has_arrived() {
        // 10 fps, a tick every 100 ms: frame 1 arrives at 400, frame 2 at 1000.
        let ticks = plan(
            &[frame(0, "a"), frame(400, "b"), frame(1000, "c")],
            1500,
            10,
        );
        assert_eq!(ticks, [0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn a_page_that_stopped_changing_is_held_to_the_end_of_the_plan() {
        // One frame, then nothing for 4 seconds: 4 seconds of that frame.
        let ticks = plan(&[frame(100, "only")], 4100, 30);
        assert_eq!(ticks.len(), 120);
        assert!(ticks.iter().all(|&i| i == 0));
    }

    #[test]
    fn the_video_runs_from_the_first_frame_not_from_the_start_of_capture() {
        // Capture began at 0 but the page first painted at 2000: no empty lead-in.
        assert_eq!(plan(&[frame(2000, "a")], 3000, 25).len(), 25);
    }

    #[test]
    fn the_video_is_never_shorter_than_one_frame() {
        assert_eq!(plan(&[frame(500, "a")], 500, 25).len(), 1);
        assert_eq!(
            plan(&[frame(500, "a")], 100, 25).len(),
            1,
            "an end before the frame"
        );
    }

    #[test]
    fn of_two_frames_at_the_same_instant_the_later_one_is_shown() {
        assert_eq!(plan(&[frame(10, "a"), frame(10, "b")], 110, 10), [1]);
    }

    #[test]
    fn no_frames_make_no_ticks() {
        assert!(plan(&[], 1000, 30).is_empty());
    }

    #[test]
    fn the_length_follows_the_rate_not_the_number_of_frames() {
        // 947 irregular frames in 20 s are still 600 ticks at 30 fps.
        let frames: Vec<FrameMeta> = (0..947).map(|i| frame(i * 21, "f")).collect();
        assert_eq!(plan(&frames, 20_000, 30).len(), 600);
    }
}
