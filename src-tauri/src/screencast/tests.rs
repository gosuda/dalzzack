use super::*;

/// A frame of a normal size: an H.264 encoder may refuse a picture of a few pixels.
fn jpeg() -> Vec<u8> {
    let image = image::RgbImage::from_fn(320, 240, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
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
    let kept = std::env::temp_dir().join(format!(
        "dalzzack-screencast-kept-{}.mp4",
        std::process::id()
    ));
    let kept_text = kept.to_string_lossy().to_string();
    if can_encode() {
        // The kept frames are what `stop` encodes, and it says why capture ended.
        let done = stop("t", Some(&kept_text)).await.unwrap();
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(done["ended_because"], "max_seconds");
        assert_eq!(done["frames"], 1);
        assert!(kept.metadata().unwrap().len() > 0);
        let _ = std::fs::remove_file(&kept);
    } else {
        abort().await;
    }
    assert_eq!(status().await["state"], "idle");

    // A source that gives up (WebKit stopped giving snapshots) ends capture,
    // and `status` says so instead of reporting a recording that is not.
    SCRIPTED_DIES.store(true, Ordering::SeqCst);
    start("t", defaults).await.unwrap();
    push_frame(jpeg());
    tokio::time::sleep(Duration::from_millis(600)).await;
    let s = status().await;
    assert_eq!(s["state"], "capture_ended", "{s}");
    assert_eq!(s["ended_because"], "source_failed");
    assert!(!is_recording());
    SCRIPTED_DIES.store(false, Ordering::SeqCst);
    abort().await;

    // An exit that cannot wait (the window closed) still removes the frames.
    start("t", defaults).await.unwrap();
    push_frame(jpeg());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let dir = FILES
        .lock()
        .unwrap()
        .clone()
        .expect("a recording has a folder");
    assert!(dir.exists());
    discard_files();
    assert!(!dir.exists(), "the frames are gone");
    abort().await;
    assert_eq!(status().await["state"], "idle");

    // Frames in, an mp4 out (when an encoder is there: the OS has one, or ffmpeg).
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

    // Forced to ffmpeg, with no ffmpeg to find: the frames stay for another try.
    std::env::set_var("DALZZACK_ENCODER", "ffmpeg");
    std::env::set_var("DALZZACK_FFMPEG", out.with_extension("no-ffmpeg"));
    let e = stop("t", Some(&out_text)).await.unwrap_err();
    assert_eq!(e.code, "FFMPEG_NOT_FOUND", "{}", e.message);
    assert_eq!(
        status().await["state"],
        "capture_ended",
        "the frames are kept"
    );
    assert!(!out.exists());
    // A misspelt choice is refused by name, and also keeps the frames.
    std::env::set_var("DALZZACK_ENCODER", "ffmepg");
    let e = stop("t", Some(&out_text)).await.unwrap_err();
    assert_eq!(e.code, "INVALID_OPTION");
    assert!(e.message.contains("ffmepg"), "{}", e.message);
    assert_eq!(status().await["state"], "capture_ended");
    std::env::remove_var("DALZZACK_ENCODER");
    std::env::remove_var("DALZZACK_FFMPEG");

    if !can_encode() {
        let e = stop("t", Some(&out_text)).await.unwrap_err();
        assert_eq!(e.code, "FFMPEG_NOT_FOUND");
        assert_eq!(status().await["state"], "capture_ended");
        abort().await;
    } else {
        let done = stop("t", Some(&out_text)).await.unwrap();
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(done["frames"], 5);
        assert!(done["duration_ms"].as_u64().unwrap() >= 450, "{done}");
        let expected = if native::SUPPORTED {
            "native"
        } else {
            "ffmpeg"
        };
        assert_eq!(done["encoder"], expected, "{done}");
        assert!(out.metadata().unwrap().len() > 0);
        assert_eq!(status().await["state"], "idle");
        let _ = std::fs::remove_file(&out);
    }
    SCRIPTED.store(false, Ordering::SeqCst);
}

/// Can `stop` make a video here: the OS has an encoder, or ffmpeg is found.
fn can_encode() -> bool {
    native::SUPPORTED || encode::find_ffmpeg().is_some()
}

#[test]
fn the_encoder_choice_is_native_ffmpeg_or_automatic_and_nothing_else() {
    use super::EncoderChoice::*;
    for (value, want) in [
        (None, Auto),
        (Some(""), Auto),
        (Some("auto"), Auto),
        (Some("native"), Native),
        (Some(" ffmpeg "), Ffmpeg),
    ] {
        assert_eq!(
            super::EncoderChoice::parse(value).unwrap(),
            want,
            "{value:?}"
        );
    }
    let e = super::EncoderChoice::parse(Some("x264")).unwrap_err();
    assert_eq!(e.code, "INVALID_OPTION");
    assert!(e.message.contains("x264"));
}

#[test]
fn the_os_encoder_goes_first_and_ffmpeg_takes_over_when_it_cannot() {
    use super::{after_native, AfterNative, EncoderChoice::*};
    // The OS encoder worked: done, whatever the choice but ffmpeg-only.
    assert!(matches!(
        after_native(Auto, true, Some(Ok(()))),
        AfterNative::Done
    ));
    assert!(matches!(
        after_native(Native, true, Some(Ok(()))),
        AfterNative::Done
    ));
    // It failed: automatic goes on with ffmpeg and remembers why.
    match after_native(Auto, true, Some(Err("no encoder".into()))) {
        AfterNative::UseFfmpeg { native_failure } => {
            assert_eq!(native_failure.as_deref(), Some("no encoder"))
        }
        other => panic!("expected ffmpeg to take over, got {other:?}"),
    }
    // No OS encoder on this platform: ffmpeg, with nothing to explain.
    assert!(matches!(
        after_native(Auto, false, None),
        AfterNative::UseFfmpeg {
            native_failure: None
        }
    ));
    // ffmpeg only: the OS encoder is never asked, and its absence is no failure.
    assert!(matches!(
        after_native(Ffmpeg, true, None),
        AfterNative::UseFfmpeg {
            native_failure: None
        }
    ));
}

#[test]
fn native_only_never_falls_back_to_ffmpeg() {
    use super::{after_native, AfterNative, EncoderChoice::Native};
    for (supported, attempt) in [(true, Some(Err("boom".to_string()))), (false, None)] {
        match after_native(Native, supported, attempt) {
            AfterNative::Fail(f) => {
                assert_eq!(f.code, "ENCODE_FAILED");
                assert!(f.message.contains("frames are kept"), "{}", f.message);
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}

#[test]
fn with_no_encoder_at_all_the_message_says_what_failed_and_how_many_frames_stay() {
    let f = super::ffmpeg_not_found(25, Some("cannot start the H.264 encoder"));
    assert_eq!(f.code, "FFMPEG_NOT_FOUND");
    assert!(f
        .message
        .contains("The built-in encoder failed (cannot start the H.264 encoder)"));
    assert!(
        f.message.contains("25 captured frames are kept"),
        "{}",
        f.message
    );
    let plain = super::ffmpeg_not_found(3, None);
    assert!(
        plain.message.starts_with("ffmpeg is needed"),
        "{}",
        plain.message
    );
    assert!(!plain.message.contains("built-in"), "{}", plain.message);
}
