### Video — `screencast`

Record the page as an mp4: `screencast start`, run commands (scroll, click, navigate), then `screencast stop --output FILE`. (`record` is a network/console event tape; it makes no video.)

```bash
dalzzack start --id b --width 1280 --height 720
dalzzack navigate -i b --url https://example.com/docs
dalzzack screencast start -i b
dalzzack scroll -i b --amount 120          # repeat, pausing between steps
dalzzack screencast stop -i b --output docs.mp4
```

- `screencast start|stop|status [--fps 30] [--max-seconds 120] [--quality 80] [--output FILE]` — `--fps` 1-60 is the video's rate, `--max-seconds` 1-600, `--quality` 1-100 is the frames' JPEG quality. A relative `--output` goes under the instance's folder (`list`/results print the full path).
- **Page only.** Not the window, the toolbar or the screen, and no sound or mouse pointer. The video is the page's size in CSS pixels (a 1280x720 page gives 1280x720), H.264 `yuv420p` (BT.709 colour), in real time: a scroll every 0.4 s makes a video that scrolls every 0.4 s.
- **Engines.** macOS default engine: snapshots of the page, 30 a second by default (measured up to about 50), with the window in front, behind other windows or in the background. `--engine chrome`: CDP `Page.startScreencast`, which sends a frame only when the page paints, so a page that sits still makes few frames and is held for as long as it stayed still. The Windows default engine answers `UNSUPPORTED_PLATFORM`: use `--engine chrome`.
- **Encoder.** `stop` makes the mp4 with the encoder that the OS has (Media Foundation on Windows, AVFoundation on macOS), so no ffmpeg is needed. When the OS has none (a Windows N edition without the Media Feature Pack) or it fails, `stop` uses ffmpeg. The daemon finds ffmpeg at `DALZZACK_FFMPEG` (set before `start`), on the `PATH` it was started with, or in `/opt/homebrew/bin`, `/usr/local/bin` and `/usr/bin`. Any ffmpeg with `libx264`, `libopenh264`, `h264_videotoolbox` or `h264_mf` works, so a small LGPL build is enough. With neither encoder `stop` answers `FFMPEG_NOT_FOUND` and **keeps the frames**: install ffmpeg and run `stop` again; a new `PATH` reaches only a daemon started after it, and stopping the daemon discards the frames. `DALZZACK_ENCODER=native` or `ffmpeg` (set before `start`) allows only that one; a misspelt value is `INVALID_OPTION` and keeps the frames.
- **Limits.** One recording per instance (`ALREADY_RECORDING`). Capture ends by itself at `--max-seconds` or 1.5 GB on disk; the frames wait for `stop` (`status` shows `state: capture_ended` and `ended_because`). `NO_FRAMES` when the page never painted. A recording is thrown away when the daemon stops, or its window is closed.
- `status`: `state` (`idle`, `recording`, `capture_ended`), `frames` (kept, as in `stop`), `frames_received`, `frames_dropped` (frames the disk could not keep up with), `elapsed_ms`. `ended_because` is `stop`, `max_seconds`, `max_bytes` or `source_failed` (macOS: WebKit stopped giving snapshots; the frames so far are kept). `stop`: `file`, `duration_ms`, `frames`, `frames_dropped`, `capture_fps`, `encoder` (`native` or `ffmpeg`), `ffmpeg_encoder` (the H.264 encoder that ffmpeg used, else `null`), `ended_because`.
- While it records, the window's toolbar shows `REC` and the time.
