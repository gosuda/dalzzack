use super::*;

fn jpeg() -> Vec<u8> {
    let image = image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([(x * 4) as u8, (y * 5) as u8, 128])
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80)
        .encode_image(&image)
        .unwrap();
    bytes
}

#[test]
fn options_have_defaults_and_refuse_nonsense() {
    let o = Options::from_args(&json!({})).unwrap();
    assert_eq!((o.fps, o.max_seconds, o.quality), (30, 120, 80));
    let o = Options::from_args(&json!({ "fps": 12, "max_seconds": 600, "quality": 100 })).unwrap();
    assert_eq!((o.fps, o.max_seconds, o.quality), (12, 600, 100));
    // A null is "not given", as a CLI that sends every option sends it.
    assert_eq!(Options::from_args(&json!({ "fps": null })).unwrap().fps, 30);

    for bad in [
        json!({ "fps": 0 }),
        json!({ "fps": 61 }),
        json!({ "max_seconds": 0 }),
        json!({ "max_seconds": 601 }),
        json!({ "quality": 0 }),
        json!({ "quality": 101 }),
        json!({ "fps": "fast" }),
        json!({ "fps": -3 }),
    ] {
        let e = Options::from_args(&bad).unwrap_err();
        assert_eq!(e.code, "INVALID_OPTION", "{bad}");
    }
}

#[test]
fn a_failure_reads_as_an_error_code_and_a_message() {
    let j = Failure::new("NOT_RECORDING", "nothing").json();
    assert_eq!(j["status"], "failed");
    assert_eq!(j["error"], "NOT_RECORDING");
    assert_eq!(j["message"], "nothing");
}

#[tokio::test]
async fn an_unknown_action_is_refused_by_name() {
    let r = tool(&json!({ "action": "pause" })).await.unwrap();
    assert_eq!(r["error"], "INVALID_ACTION");
    assert!(r["message"].as_str().unwrap().contains("pause"));
}

/// The whole life of a recording, in one test because it uses the process-wide
/// recorder: the other tests here touch none of it.
#[tokio::test]
async fn a_recording_runs_from_start_to_a_video() {
    SCRIPTED.store(true, Ordering::SeqCst);
    let defaults = Options::from_args(&json!({})).unwrap();

    // Nothing recording.
    assert_eq!(status().await["state"], "idle");
    assert_eq!(stop("t", None).await.unwrap_err().code, "NOT_RECORDING");

    // Start; a second start is refused; a recording with no frames is not saved.
    assert_eq!(start("t", defaults).await.unwrap()["status"], "started");
    assert!(is_recording());
    assert_eq!(status().await["state"], "recording");
    assert_eq!(
        start("t", defaults).await.unwrap_err().code,
        "ALREADY_RECORDING"
    );
    assert_eq!(stop("t", None).await.unwrap_err().code, "NO_FRAMES");
    assert!(!is_recording());
    assert_eq!(
        status().await["state"],
        "idle",
        "an empty recording leaves nothing behind"
    );

    // A limit ends capture and keeps the frames for `stop`.
    let short = Options::from_args(&json!({ "max_seconds": 1 })).unwrap();
    start("t", short).await.unwrap();
    push_frame(jpeg());
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let s = status().await;
    assert_eq!(s["state"], "capture_ended", "{s}");
    assert_eq!(s["ended_because"], "max_seconds");
    assert_eq!(s["frames"], 1);
    assert!(!is_recording(), "no frame is taken after the limit");
    assert_eq!(
        start("t", defaults).await.unwrap_err().code,
        "ALREADY_RECORDING"
    );
    abort().await;
    assert_eq!(status().await["state"], "idle");

    // Frames in, an mp4 out (when ffmpeg is installed).
    start("t", defaults).await.unwrap();
    for _ in 0..5 {
        push_frame(jpeg());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let out = std::env::temp_dir().join(format!(
        "dalzzack-screencast-test-{}.mp4",
        std::process::id()
    ));
    let out_text = out.to_string_lossy().to_string();
    if encode::find_ffmpeg().is_none() {
        let e = stop("t", Some(&out_text)).await.unwrap_err();
        assert_eq!(e.code, "FFMPEG_NOT_FOUND");
        assert_eq!(
            status().await["state"],
            "capture_ended",
            "the frames are kept"
        );
        abort().await;
    } else {
        let done = stop("t", Some(&out_text)).await.unwrap();
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(done["frames"], 5);
        assert!(done["duration_ms"].as_u64().unwrap() >= 450, "{done}");
        assert!(out.metadata().unwrap().len() > 0);
        assert_eq!(status().await["state"], "idle");
        let _ = std::fs::remove_file(&out);
    }
    SCRIPTED.store(false, Ordering::SeqCst);
}
