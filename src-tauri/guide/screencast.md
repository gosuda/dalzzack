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
- **Page only.** Not the window, the toolbar or the screen, and no sound or mouse pointer. The video is the page's size in CSS pixels (a 1280x720 page gives 1280x720), H.264 `yuv420p`, in real time: a scroll every 0.4 s makes a video that scrolls every 0.4 s.
- **Engines.** macOS default engine: snapshots of the page, 30 a second by default (measured up to about 50), with the window in front, behind other windows or in the background. `--engine chrome`: CDP `Page.startScreencast`, which sends a frame only when the page paints, so a page that sits still makes few frames and is held for as long as it stayed still. The Windows default engine answers `UNSUPPORTED_PLATFORM`: use `--engine chrome`.
- **ffmpeg** makes the mp4 at `stop`. It is found on `PATH` and the usual install folders, or at `DALZZACK_FFMPEG`. Without it `stop` answers `FFMPEG_NOT_FOUND` and **keeps the frames**: install it (`brew install ffmpeg`, `winget install ffmpeg`) and run `stop` again.
- **Limits.** One recording per instance (`ALREADY_RECORDING`). Capture ends by itself at `--max-seconds` or 1.5 GB on disk; the frames wait for `stop` (`status` shows `state: capture_ended` and `ended_because`). `NO_FRAMES` when the page never painted. A recording is thrown away when the daemon stops.
- `status`: `state` (`idle`, `recording`, `capture_ended`), `frames`, `frames_dropped` (frames the disk could not keep up with), `elapsed_ms`. `stop`: `file`, `duration_ms`, `frames`, `frames_dropped`, `capture_fps`, `ended_because`.
- While it records, the window's toolbar shows `REC` and the time.
