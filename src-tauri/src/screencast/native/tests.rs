use super::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The reference conversion in floating point: BT.709, limited range.
fn reference(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    (16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr)
}

fn solid(r: u8, g: u8, b: u8, width: usize, height: usize) -> Vec<u8> {
    let mut rgb = Vec::new();
    for _ in 0..width * height {
        rgb.extend_from_slice(&[r, g, b]);
    }
    let mut out = vec![0u8; nv12_len(width as u32, height as u32)];
    rgb_to_nv12(&rgb, width, width, height, &mut out);
    out
}

#[test]
fn a_colour_converts_like_the_bt709_limited_range_definition() {
    for (r, g, b) in [
        (255, 255, 255),
        (0, 0, 0),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (0, 0, 255),
        (250, 250, 40),
        (20, 90, 200),
    ] {
        let out = solid(r, g, b, 4, 4);
        let (y, u, v) = reference(r, g, b);
        let (luma, chroma) = (f64::from(out[0]), (f64::from(out[16]), f64::from(out[17])));
        assert!((luma - y).abs() <= 1.0, "Y of {r},{g},{b}: {luma} vs {y}");
        assert!(
            (chroma.0 - u).abs() <= 1.5,
            "U of {r},{g},{b}: {} vs {u}",
            chroma.0
        );
        assert!(
            (chroma.1 - v).abs() <= 1.5,
            "V of {r},{g},{b}: {} vs {v}",
            chroma.1
        );
    }
}

#[test]
fn white_black_and_grey_stay_inside_the_video_range_with_neutral_chroma() {
    assert_eq!(
        &solid(255, 255, 255, 2, 2)[..],
        [235, 235, 235, 235, 128, 128]
    );
    assert_eq!(&solid(0, 0, 0, 2, 2)[..], [16, 16, 16, 16, 128, 128]);
    let grey = solid(128, 128, 128, 2, 2);
    assert_eq!(grey[4], 128);
    assert_eq!(grey[5], 128);
}

#[test]
fn a_chroma_sample_is_the_average_of_its_four_pixels() {
    // Left column white, right column black: each pixel keeps its own brightness,
    // the colour is the average, which is grey (no colour).
    let rgb = [255, 255, 255, 0, 0, 0, 255, 255, 255, 0, 0, 0];
    let mut out = vec![0u8; nv12_len(2, 2)];
    rgb_to_nv12(&rgb, 2, 2, 2, &mut out);
    assert_eq!(&out[..4], [235, 16, 235, 16]);
    assert_eq!(&out[4..], [128, 128]);
}

#[test]
fn the_planes_are_luma_first_then_u_and_v_interleaved() {
    // 4x2 picture: a red pair of columns, then a blue pair.
    let mut rgb = Vec::new();
    for _row in 0..2 {
        for px in [[255, 0, 0], [255, 0, 0], [0, 0, 255], [0, 0, 255]] {
            rgb.extend_from_slice(&px);
        }
    }
    let mut out = vec![0u8; nv12_len(4, 2)];
    rgb_to_nv12(&rgb, 4, 4, 2, &mut out);
    assert_eq!(out.len(), 12);
    let (red, blue) = (reference(255, 0, 0), reference(0, 0, 255));
    // Chroma plane: U0 V0 U1 V1 after the 8 luma bytes.
    assert!((f64::from(out[8]) - red.1).abs() <= 1.5 && (f64::from(out[9]) - red.2).abs() <= 1.5);
    assert!(
        (f64::from(out[10]) - blue.1).abs() <= 1.5 && (f64::from(out[11]) - blue.2).abs() <= 1.5
    );
}

#[test]
fn a_wider_source_is_cropped_by_its_stride_not_skewed() {
    // 3 pixels a row, converted as 2: the third column must not shift the rows.
    let rgb = [
        255, 0, 0, 255, 0, 0, 0, 255, 0, //
        255, 0, 0, 255, 0, 0, 0, 255, 0,
    ];
    let mut out = vec![0u8; nv12_len(2, 2)];
    rgb_to_nv12(&rgb, 3, 2, 2, &mut out);
    let red = reference(255, 0, 0);
    assert!(
        (f64::from(out[2]) - red.0).abs() <= 1.0,
        "second row starts at its own start"
    );
    assert!((f64::from(out[5]) - red.2).abs() <= 1.5);
}

/// A folder of frame files that goes away with the test.
struct Folder(PathBuf);

impl Folder {
    fn new(name: &str) -> Folder {
        let dir =
            std::env::temp_dir().join(format!("dalzzack-native-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Folder(dir)
    }

    fn jpeg(&self, name: &str, width: u32, height: u32, colour: [u8; 3]) -> PathBuf {
        let image = image::RgbImage::from_fn(width, height, |_, _| image::Rgb(colour));
        let path = self.0.join(name);
        let file = std::fs::File::create(&path).unwrap();
        image::codecs::jpeg::JpegEncoder::new_with_quality(file, 95)
            .encode_image(&image)
            .unwrap();
        path
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Seen {
    opened: Option<(u32, u32, u32)>,
    written: Vec<(u64, Vec<u8>)>,
    finished: bool,
}

struct Fake {
    seen: Arc<Mutex<Seen>>,
    fail_at: Option<u64>,
}

impl Sink for Fake {
    fn write(&mut self, index: u64, nv12: &[u8]) -> Result<(), String> {
        if self.fail_at == Some(index) {
            return Err("disk full".to_string());
        }
        self.seen
            .lock()
            .unwrap()
            .written
            .push((index, nv12.to_vec()));
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        self.seen.lock().unwrap().finished = true;
        Ok(())
    }
}

fn run_fake(
    frames: &[FrameMeta],
    end_ms: u64,
    fps: u32,
    output: &Path,
    fail_at: Option<u64>,
) -> (Result<(), String>, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let handle = seen.clone();
    let job = Job {
        frames,
        end_ms,
        fps,
        output,
    };
    let result = encode_with(&job, &AtomicBool::new(false), move |_, w, h, fps| {
        handle.lock().unwrap().opened = Some((w, h, fps));
        Ok(Box::new(Fake {
            seen: handle,
            fail_at,
        }))
    });
    (result, seen)
}

fn meta(file: PathBuf, offset_ms: u64) -> FrameMeta {
    FrameMeta { offset_ms, file }
}

#[test]
fn the_sink_gets_every_tick_in_order_and_one_picture_for_a_held_frame() {
    let dir = Folder::new("ticks");
    let frames = [
        meta(dir.jpeg("a.jpg", 64, 48, [250, 20, 20]), 0),
        meta(dir.jpeg("b.jpg", 64, 48, [20, 20, 250]), 400),
        meta(dir.jpeg("c.jpg", 64, 48, [20, 250, 20]), 1000),
    ];
    let (result, seen) = run_fake(&frames, 1500, 10, &dir.0.join("out.mp4"), None);
    result.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.opened, Some((64, 48, 10)));
    assert!(seen.finished);
    let indexes: Vec<u64> = seen.written.iter().map(|(i, _)| *i).collect();
    assert_eq!(indexes, (0..15).collect::<Vec<u64>>());
    assert!(seen
        .written
        .iter()
        .all(|(_, p)| p.len() == nv12_len(64, 48)));
    // Ticks 0-3 are one picture, 4-9 another, 10-14 a third.
    assert_eq!(seen.written[0].1, seen.written[3].1);
    assert_ne!(seen.written[3].1, seen.written[4].1);
    assert_eq!(seen.written[4].1, seen.written[9].1);
    assert_ne!(seen.written[9].1, seen.written[10].1);
}

#[test]
fn an_odd_sized_page_is_cropped_to_even_sides() {
    let dir = Folder::new("odd");
    let frames = [meta(dir.jpeg("a.jpg", 65, 49, [90, 90, 90]), 0)];
    let (result, seen) = run_fake(&frames, 100, 10, &dir.0.join("out.mp4"), None);
    result.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.opened, Some((64, 48, 10)));
    assert_eq!(seen.written[0].1.len(), nv12_len(64, 48));
}

#[test]
fn a_frame_of_another_size_is_scaled_to_the_size_of_the_first() {
    let dir = Folder::new("resized");
    let frames = [
        meta(dir.jpeg("a.jpg", 64, 48, [200, 200, 200]), 0),
        meta(dir.jpeg("b.jpg", 32, 24, [200, 200, 200]), 200),
    ];
    let (result, seen) = run_fake(&frames, 400, 10, &dir.0.join("out.mp4"), None);
    result.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.written.len(), 4);
    assert!(seen
        .written
        .iter()
        .all(|(_, p)| p.len() == nv12_len(64, 48)));
}

#[test]
fn a_failure_leaves_no_half_written_file() {
    let dir = Folder::new("failure");
    let frames = [
        meta(dir.jpeg("a.jpg", 64, 48, [10, 10, 10]), 0),
        meta(dir.jpeg("b.jpg", 64, 48, [200, 10, 10]), 100),
    ];
    let output = dir.0.join("out.mp4");
    std::fs::write(&output, b"partial").unwrap();
    let (result, seen) = run_fake(&frames, 300, 10, &output, Some(1));
    assert_eq!(result.unwrap_err(), "disk full");
    assert!(!seen.lock().unwrap().finished);
    assert!(!output.exists(), "the partial file is removed");
}

#[test]
fn a_frame_that_cannot_be_read_is_an_error_that_names_it() {
    let dir = Folder::new("unreadable");
    let bad = dir.0.join("bad.jpg");
    std::fs::write(&bad, b"not a jpeg").unwrap();
    let (result, _) = run_fake(&[meta(bad, 0)], 100, 10, &dir.0.join("out.mp4"), None);
    assert!(result.unwrap_err().contains("bad.jpg"));
}

#[test]
fn a_cancel_stops_before_the_next_batch_and_writes_nothing() {
    let dir = Folder::new("cancel");
    let frames = [meta(dir.jpeg("a.jpg", 64, 48, [10, 10, 10]), 0)];
    let seen = Arc::new(Mutex::new(Seen::default()));
    let handle = seen.clone();
    let job = Job {
        frames: &frames,
        end_ms: 100,
        fps: 10,
        output: &dir.0.join("out.mp4"),
    };
    let result = encode_with(&job, &AtomicBool::new(true), move |_, _, _, _| {
        Ok(Box::new(Fake {
            seen: handle,
            fail_at: None,
        }))
    });
    assert!(result.unwrap_err().contains("cancelled"));
    assert!(seen.lock().unwrap().written.is_empty());
}

#[test]
fn nothing_to_encode_is_an_error() {
    let dir = Folder::new("empty");
    let (result, _) = run_fake(&[], 100, 10, &dir.0.join("out.mp4"), None);
    assert!(result.is_err());
}
