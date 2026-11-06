# Changelog

All notable changes to dalzzack. The format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[SemVer](https://semver.org).

## [1.1.0] - 2026-11-05

### Added

- `dalzzack uninstall` undoes the install. Alone, it prints what would change and
  changes nothing. `--yes` stops daemons, removes the guide from the global
  Claude Code and Codex files, removes the `PATH` entry, and deletes the binary.
  `--purge` also deletes data and browser logins. `--project` cleans only
  `./AGENTS.md`, so it can run in each project first.
- `DALZZACK_VERSION` selects a release in the install scripts.
- `scripts/verify-release.sh` and `scripts/verify-release.ps1` check a published
  release from install to uninstall.
- One-line install scripts, so installing no longer needs a list of commands:
  `scripts/install.sh` for macOS and `scripts/install.ps1` for Windows. Each
  downloads the latest release, checks its SHA-256 hash, and adds the binary to
  `PATH` without `sudo` or administrator rights. Run the script again to update.
- `dalzzack install codex-project` writes `./AGENTS.md`, as `install codex` did
  before.
- `dalzzack get-text` prints the visible text of the page or one element, the
  cheapest way to read a page.
- `dalzzack guide <topic>` prints a part of the usage guide.
- `dalzzack tools --names` and `dalzzack tools <name>`, so that nobody needs the
  64 KB list to read one schema.
- `dalzzack run -i <id>` runs steps on a live instance and prints one result.
  Steps can be command lines, such as `["click", "--text", "OK"]`, and `--steps -`
  reads them from stdin.
- `dalzzack install --refresh` rewrites the guide in global files that already
  have it, and creates nothing.
- `--pretty`, `--full`, `--max-output`, and `DALZZACK_OUTPUT=full`. See Changed.
- `get-html --no-strip-noise`, because raw HTML could not be read at all.
- `describe` reports `total`, because `elementCount` stops at the cap of 50.

### Changed

- `dalzzack upgrade` now installs the latest release and needs no Rust. It checks
  the hash and the version of the new binary before it stops any daemon, and it
  refreshes the installed guides. `update` is an alias, `--check` prints the
  versions, `--force` reinstalls, and `--tag` installs one tag. `--path`, `--git`,
  `--rev`, and `--no-locked` still build from source with cargo. Version 1.0.0
  builds from source when it upgrades, so to leave it, run the install command once.
- Output in a pipe is shorter, and a terminal is unchanged. JSON is compact, and
  `navigate`, `wait-navigation`, `describe`, `console-logs`, `exec-js`, and `list`
  print a short form: 74% to 92% smaller on a test page. Over 16 KiB, the default
  output goes to a file and a summary prints. Failures are never shortened. `--field` and `--jq`
  read the complete result, and `--full` or `DALZZACK_OUTPUT=full` restore it. A
  script that reads fields of those commands without a filter needs one of these.
- The installed guide is about 2.3 KB and not 53 KB, because every agent session
  pays for it. `dalzzack upgrade` replaces the old block.
- `get-frames --full` is now the global `--full`, with the same meaning there.
- `sites/` and `logs/` moved from next to the binary to a per-user folder:
  `%LOCALAPPDATA%\dalzzack` on Windows, `~/Library/Application Support/dalzzack` on
  macOS.
- Logging is at `info` by default, each line stops at 2,000 bytes, and logs older
  than 7 days are deleted. `DALZZACK_LOG` replaces `RUST_LOG`, and logs no longer
  go to stderr.
- `dalzzack install codex` and `dalzzack install all` now write the global Codex
  file, `~/.codex/AGENTS.md` or `$CODEX_HOME/AGENTS.md`, instead of `./AGENTS.md`
  in the current folder. Codex reads the global file in every project and reads
  all of it. It cuts project files at 32 KiB, which is less than the size of the
  guide, and it does not read them in untrusted projects. If an earlier version
  wrote the guide into a project, delete the block between
  `<!-- dalzzack-begin -->` and `<!-- dalzzack-end -->` from that `AGENTS.md`.

### Fixed

- `kill`, `start`, `upgrade`, and `uninstall` could force-kill another program. A
  pid file outlives its daemon over a restart, and the pid can then belong to
  anything. The pid file now holds the identity of its process, and dalzzack
  force-kills only that process.
- `list` and `stop` deleted the files of a daemon that hung, so `kill` could not
  find it.
- `navigate`, `describe`, and `screenshot` failed with `os error 5` when the binary
  was in a folder that cannot be written.
- `get-html` turned non-ASCII text into mojibake, deleted the text after `style=""`,
  and left part of an attribute value in the tag.
- `describe` printed the value of password fields, and a failed `click` printed it
  in `nearby`. They now print `***`. Hidden inputs left `formValues`.
- `run --steps` reported success and went on after a failed step.
- `dalzzack install` deleted the text after the begin marker if the end marker was
  missing, and replaced a file that it could not read.
- Printed paths no longer start with `\\?\` on Windows.
- A Windows debug build crashed on every command with a stack overflow.
- A release is now public only after all its files exist, and a tag must match the
  version files.

## [1.0.0] - 2026-11-03

First public release.

dalzzack was developed in a private repository under the name TaskWeaver
(0.1.0 to 0.19.0). This repository starts fresh at 1.0.0, so that history is
not here. The commands and their output are the same as TaskWeaver 0.19.0,
except for the changes below.

### Added

- Prebuilt binaries, so installing no longer needs Rust. Each GitHub release
  has `dalzzack-macos-universal.tar.gz` (Apple silicon and Intel) and
  `dalzzack-windows-x64.zip`, each with a `.sha256` file. The names do not
  change between versions, so
  `https://github.com/gosuda/dalzzack/releases/latest/download/<name>` always
  gives the newest one. The macOS binary is not notarized; see the README for
  the one-time `xattr` step after a browser download.

### Changed

- Releases no longer carry the Tauri installers (`.msi`, `.exe`, `.dmg`). They
  did not put `dalzzack` on `PATH`, and their shortcut ran it with no
  arguments, which only prints the help. Use the archives above.

- Renamed from TaskWeaver to dalzzack:

  | What | Before | Now |
  |------|--------|-----|
  | Binary and crate | `taskweaver` | `dalzzack` |
  | Library crate | `taskweaver_lib` | `dalzzack_lib` |
  | Repository | private | <https://github.com/gosuda/dalzzack> |
  | App identifier (on Windows, also the shared WebView2 profile folder) | ends in `.taskweaver` | `org.gosuda.dalzzack` |
  | Chrome path variable | `TASKWEAVER_CHROME` | `DALZZACK_CHROME` |
  | Temp files and folders (hang dumps, symbol cache, Chrome and WebView profiles, CPU profiles, minidumps) | `taskweaver-*` | `dalzzack-*` |
  | Agent-docs block markers | `<!-- taskweaver-begin -->` / `<!-- taskweaver-end -->` | `<!-- dalzzack-begin -->` / `<!-- dalzzack-end -->` |
  | CDP isolated world name | `taskweaver` | `dalzzack` |

- Rust and npm dependencies updated to current releases.
- `upgrade` checks for cargo before it stops any daemon. Without cargo it
  fails with `cargo_not_found` and a `download_url` for this platform's
  prebuilt binary. When the running binary is not the one cargo replaces (a
  prebuilt one, for example), the result carries a `note` that names both.
- The landing page that a new window shows was redesigned. It now lists the
  current command names.

### Fixed

- `screenshot --selector` on Windows cut the wrong region when the display
  scale was above 100%. WebView2 captures in device pixels, but the crop used
  the element's CSS pixels unscaled, so at 125% a 1280×640 element came back
  as its top-left 1024×512. The crop is now scaled the way the macOS path
  already was.
- The macOS release build was Apple silicon only: the workflow named the
  universal target but did not pass it to the build.
- After `upgrade` on macOS, the ad-hoc codesign step signed the running
  binary instead of the one that cargo had just installed.

### Migrating from TaskWeaver

dalzzack does not read any of the old names. To move an existing install:

1. Stop running daemons: `taskweaver list`, then `taskweaver stop --id <id>` for each.
2. Install dalzzack with `cargo install --git https://github.com/gosuda/dalzzack --locked`.
   `taskweaver upgrade --git https://github.com/gosuda/dalzzack` does the same.
   Neither removes the old binary, so run `cargo uninstall taskweaver` afterwards.
3. Replace the agent docs. Delete the block from `<!-- taskweaver-begin -->` to
   `<!-- taskweaver-end -->` in `~/.claude/CLAUDE.md` and in any project `AGENTS.md`,
   then run `dalzzack install claude-code` (or `codex`, or `all`). If you skip the
   delete, the file keeps both blocks.
4. If you set `TASKWEAVER_CHROME`, rename it to `DALZZACK_CHROME`.
5. Windows: the shared WebView2 profile is now
   `%LOCALAPPDATA%\org.gosuda.dalzzack\EBWebView`. It starts empty, so log in to
   your sites again. When no `taskweaver` daemon runs, delete the old folder in
   `%LOCALAPPDATA%` whose name ends in `.taskweaver`.
6. Old temp files named `taskweaver-*` can be deleted. To keep downloaded symbols
   (msedge alone is about 630 MB), rename `taskweaver-symcache` to
   `dalzzack-symcache` in the temp directory instead.
