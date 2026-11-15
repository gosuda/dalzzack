//! Turning timestamped frames into an mp4: the ffmpeg side of `screencast`.
//!
//! Frames arrive when the page paints, not at a steady rate (a still page sends
//! almost none, a scroll sends many), so a video that played them one after the
//! other would run at the wrong speed. Each frame is therefore given the time it
//! was on screen (until the next one), through ffmpeg's concat demuxer, and the
//! result is resampled to a constant frame rate. That keeps the video in real
//! time whatever the capture rate was.

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

/// ffmpeg's arguments: H.264 in yuv420p (what every player and editor takes),
/// both sides even (yuv420p needs it), a constant `fps`.
pub fn ffmpeg_args(list: &Path, output: &Path, fps: u32) -> Vec<String> {
    // `out_range=tv`: JPEG is full-range, and without the conversion ffmpeg keeps
    // it as yuvj420p, which some players and editors misread.
    let filter = format!("fps={fps},scale=trunc(iw/2)*2:trunc(ih/2)*2:out_range=tv,format=yuv420p");
    [
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
        "-c:v",
        "libx264",
        // JPEG frames are full-range, which ffmpeg would keep as yuvj420p; players
        // and editors expect plain yuv420p.
        "-pix_fmt",
        "yuv420p",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-movflags",
        "+faststart",
        &output.to_string_lossy(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Where ffmpeg is: `DALZZACK_FFMPEG`, else `PATH`, else the usual install
/// folders (a daemon started from a GUI may not have the shell's `PATH`).
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("DALZZACK_FFMPEG").map(PathBuf::from) {
        return explicit.is_file().then_some(explicit);
    }
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let mut folders: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for usual in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        folders.push(PathBuf::from(usual));
    }
    folders
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let args = ffmpeg_args(Path::new("/t/list.txt"), Path::new("/t/out.mp4"), 24);
        let joined = args.join(" ");
        assert!(
            joined.contains("-f concat -safe 0 -i /t/list.txt"),
            "{joined}"
        );
        assert!(
            joined.contains("fps=24,scale=trunc(iw/2)*2:trunc(ih/2)*2:out_range=tv,format=yuv420p"),
            "{joined}"
        );
        assert!(joined.contains("-c:v libx264") && joined.ends_with("/t/out.mp4"));
        assert!(joined.contains("-pix_fmt yuv420p"), "{joined}");
    }
}
