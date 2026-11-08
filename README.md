<p align="center"><img src="docs/assets/hero.png" alt="dalzzack with Agent Together: a moon-shaped macaron in the middle of browser windows that an agent clicks, types into, captures and checks"></p>

# dalzzack

> Browser automation CLI for AI agents. Screenshot-first, token-efficient.

dalzzack spawns a real browser window as a daemon and lets an AI agent (Claude, GPT, whatever) drive it through a small CLI. Built on Tauri 2 with native WebView APIs: no Chromium fork, no Playwright runtime, just the OS WebView and one Rust binary (about 37 MB on Windows).

Each `--id` is an isolated browser instance, so you can run agents in parallel.

---

## Why screenshot-first?

Most agent-controlled browsers default to `get_html` for page understanding. That's expensive. Modern pages routinely run into hundreds of thousands of tokens.

We measured seven sites at a 1280×800 viewport. HTML token counts use `chars/3.3` (typical BPE ratio for HTML); screenshot uses Anthropic's vision formula `(W×H)/750` after auto-resize to ~1.15 MP.

| Site | Raw HTML tokens | Stripped HTML tokens | Screenshot tokens | Verdict |
|------|---:|---:|---:|---|
| example.com | 155 | 107 | 1,531 | HTML wins (14×) |
| news.ycombinator.com | 10,547 | 10,297 | 1,531 | screenshot **6.7×** |
| google.com | 26,922 | 1,902 | 1,531 | tie (1.2×) |
| en.wikipedia.org | 31,928 | 26,654 | 1,531 | screenshot **17×** |
| naver.com | 92,379 | 27,948 | 1,531 | screenshot **18×** |
| github.com/trending | 191,923 | 147,859 | 1,531 | screenshot **97×** |
| amazon.com | 298,750 | 123,053 | 1,531 | screenshot **80×** |

**Screenshot cost is a flat ~1,500 tokens regardless of page size. HTML scales linearly with the DOM.** Break-even is around 5,000 chars of HTML. Below that, prefer HTML. Above, prefer a screenshot. Real-world sites are almost always above.

So `describe` and `screenshot` are the recommended primitives, with `get-text` to read the words on a page. Use `get-html` only when a specific value (hidden field, token, API response) can't be seen on screen.

### What an agent pays in 1.1

An agent pays for the usage guide in every session, and for the output of every command. Version 1.1 cuts both with program logic, so an agent saves tokens with no extra flag.

| What the agent reads | 1.0 | 1.1 | Saved |
|---|---:|---:|---:|
| Usage guide, in every session | 16,000 | 700 | 96% |
| Output of an 8-step flow | 3,119 | 851 | 73% |
| `navigate` | 495 | 58 | 88% |
| `describe` | 1,688 | 429 | 75% |
| `console-logs` (88 lines) | 9,533 | 804 | 92% |
| `exec-js` | 57 | 13 | 77% |

These are tokens, estimated with `chars/3.3` as above. The guide row compares the two guides. The other rows come from one local test page of 74 KB, so real pages will differ. The 1.0 column is the complete indented output, which `--full --pretty` still prints. The flow is `navigate`, `describe`, two `fill-form` calls, `click`, `wait-for`, `console-logs --level error`, and `screenshot`, without the cost of opening the image.

- The installed guide is 2.3 KB, not 53 KB. `dalzzack guide <topic>` prints the rest when an agent needs it.
- In a pipe, a command prints compact JSON in a short form. A failure always prints in full, and `--full`, `--field`, and `--jq` read the complete result.
- Output over 16 KiB goes to a file, and a summary prints. A 2 MB `exec-js` result prints 1.4 KB.
- `get-text` reads a page for much less than its HTML: about 900 tokens against 6,500 for stripped HTML on the test page.
- `run` takes several steps in one call, so a flow needs fewer round trips.

---

## Install

macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/gosuda/dalzzack/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/gosuda/dalzzack/main/scripts/install.ps1 | iex
```

Then open a new terminal and give your agents the usage guide:

```bash
dalzzack --version
dalzzack install all
```

The script downloads the latest [release](https://github.com/gosuda/dalzzack/releases), checks its SHA-256 hash, installs the binary, and adds it to your `PATH`. It needs no Rust, no Node.js, and no administrator rights. To read it before you run it, see [install.sh](scripts/install.sh) and [install.ps1](scripts/install.ps1).

| | macOS (universal) | Windows (x64) |
|---|---|---|
| Binary | `~/.local/bin/dalzzack` | `%LOCALAPPDATA%\Programs\dalzzack\dalzzack.exe` |
| `PATH` change | One line in `~/.zshenv`, or in your bash profile | Your user `Path` |

To change this, set these environment variables before you run the script:

- `DALZZACK_INSTALL_DIR`: the folder for the binary.
- `DALZZACK_NO_MODIFY_PATH=1`: do not change `PATH`.
- `DALZZACK_VERSION`: the release tag to install, such as a release candidate.

Windows needs the WebView2 runtime, which Windows 11 already has.

Use a release unless you want to change dalzzack itself. To build from source, see [Development](#development).

Screenshots and logs go to `%LOCALAPPDATA%\dalzzack` on Windows and `~/Library/Application Support/dalzzack` on macOS.

### Manual download

Each [GitHub release](https://github.com/gosuda/dalzzack/releases) has `dalzzack-macos-universal.tar.gz` and `dalzzack-windows-x64.zip`, each with a `.sha256` file. Extract the binary into a folder on your `PATH`.

To check a download, fetch the `.sha256` file with the same name. On macOS, run `shasum -a 256 -c dalzzack-macos-universal.tar.gz.sha256`. On Windows, compare its hash with `(Get-FileHash dalzzack-windows-x64.zip).Hash`.

The macOS binary is not notarized by Apple. A file that `curl` downloads runs as it is. If you downloaded the archive in a browser, macOS blocks the binary until you clear its quarantine flag, once for each download:

```bash
xattr -d com.apple.quarantine <path to dalzzack>
```

If you downloaded the zip in a browser and Windows blocks the program, run `Unblock-File <path to dalzzack.exe>` once.

### Updating

```bash
dalzzack upgrade           # install the latest release (`dalzzack update` is the same)
dalzzack upgrade --check   # print the current and latest versions, and change nothing
```

`upgrade` reads the latest release on GitHub and does nothing if you already have it. Otherwise it downloads the archive, checks its SHA-256 hash and the new binary's version, stops running daemons, replaces the binary, and refreshes the guide in your agents' files. A failed download changes nothing. It needs no Rust.

- `--force`: reinstall, also at the same version.
- `--tag v1.2.3`: install one release, for a rollback. A release candidate reports the version of its release, so going from a candidate to that release needs `--force`.
- `--keep-daemons`: refuse to upgrade if any daemons are running (default is to stop them).

Version 1.0.0 builds from source when it upgrades. To leave it, run the install command once.

Coming from TaskWeaver, the private predecessor? Follow the [migration steps](CHANGELOG.md#migrating-from-taskweaver).

---

## Quick start

```bash
dalzzack start --id b                                           # spawn browser window
dalzzack navigate -i b --url "https://example.com"
dalzzack describe -i b                                          # annotated screenshot + element list
dalzzack click -i b --text "Sign In"                            # click by visible text
dalzzack fill-form -i b --selector "#email" --value "you@x.com"
dalzzack wait-for -i b --selector ".result" --state added
dalzzack screenshot -i b
dalzzack stop --id b
```

`start` opens a landing screen showing the instance id and ready status. From that point, every command talks to the daemon over a local TCP socket.

---

## Recommended agent loop

```
describe → click/fill → wait-for → screenshot → next
```

Rules of thumb:

- `describe` first on any new page. It labels every interactive element with a number on the screenshot, and lists each element's tag, text and position.
- Prefer `click --text` or `click --selector` over coordinate clicks. Coordinates break on resize.
- Use `wait-for --state added` (event-driven) instead of `wait --ms` (timer-driven).
- Use `get-requests` to read API responses instead of scraping rendered HTML.
- Use `get-text` to read a page, and `run` for several steps in one call.
- Use `get-html` only when a value (hidden token, raw text) cannot be seen on screen.

This is `describe` on a sign-in page. Every interactive element gets a number, and the JSON result lists the same numbers with each element's tag, text and position:

<p align="center"><img src="docs/assets/describe.png" width="720" alt="describe output: a sign-in page with numbered red boxes around its 9 interactive elements"></p>

---

## Documentation

- [Command reference](docs/commands.md): every command and flag, with platform notes.
- [Architecture](docs/architecture.md): how the CLI talks to the daemon, the platform matrix, and the design decisions.
- [AGENTS.md](AGENTS.md): the usage guide that `dalzzack install` writes for agents.
- [SECURITY.md](SECURITY.md): the threat model. Read it before you point a daemon at sites you do not trust.
- [CHANGELOG.md](CHANGELOG.md): release notes.

---

## Agent integration

Write the short usage guide into your agent's instruction file:

```bash
dalzzack install claude-code     # ~/.claude/CLAUDE.md (global, all projects)
dalzzack install codex           # ~/.codex/AGENTS.md or $CODEX_HOME/AGENTS.md (global, all projects)
dalzzack install all             # both
dalzzack install codex-project   # ./AGENTS.md (current project)
```

Use `codex-project` only when you want the guide in one repository.

The guide is about 2.3 KB, because an agent pays for it in every session. It has the main commands and rules, and `dalzzack guide <topic>` prints the rest when an agent needs it.

The guide sits between `<!-- dalzzack-begin -->` and `<!-- dalzzack-end -->`. `dalzzack upgrade` keeps that block up to date, and `dalzzack install --refresh` does the same by hand. For other agents, copy [AGENTS.md](AGENTS.md) into their instruction file.

## Uninstall

```bash
dalzzack uninstall                # print what would change, and change nothing
dalzzack uninstall --yes          # apply it
dalzzack uninstall --yes --purge  # also delete the data
```

`--yes` stops running daemons, removes the guide from the global Claude Code and Codex files, removes the `PATH` entry, and deletes this binary. The rest of each file stays. If a guide file holds nothing else, it goes too.

| Item | `--yes` | `--yes --purge` |
|---|---|---|
| Guides, `PATH` entry, binary | Removed | Removed |
| Screenshots, logs, and browser logins | Kept | Deleted |
| Symbol cache and dumps in the temp folder | Kept | Deleted |

The report names what stays, and why:

- **Project guides.** dalzzack keeps no list of projects. Before the full uninstall, run `dalzzack uninstall --project --yes` in each project. It removes the guide from `./AGENTS.md` and changes nothing else.
- **A `PATH` entry in a shared folder.** If other programs are in the folder, such as `~/.local/bin`, the entry stays, because they need it.
- **Other copies.** The command deletes only the binary that runs it. `other_copies` lists any others on `PATH`.
- **A binary in a protected folder.** The report gives the command to delete it as an administrator.

If a step fails, the binary stays, so you can run `dalzzack uninstall --yes` again. dalzzack 1.0 has no `uninstall`, so run the install script first.

---

## Development

```bash
# Build & install CLI
cd src-tauri && cargo install --path . --force

# Or full deploy (frontend + Rust + install + codesign)
npm run deploy

# Tests
cd src-tauri && cargo test --lib --bins

# Help
dalzzack --help
dalzzack <command> --help
```

### Build from source (needs Rust)

Pick whichever fits your setup. All three install the same binary to a directory on your `PATH`.

```bash
# 1. From git (fastest — needs Rust only, no Node.js)
cargo install --git https://github.com/gosuda/dalzzack --locked

# 2. From a local clone (best if you want to hack on it)
git clone https://github.com/gosuda/dalzzack && cd dalzzack
cargo install --path src-tauri --locked

# 3. Full developer build (rebuilds the React landing too)
git clone https://github.com/gosuda/dalzzack && cd dalzzack
npm install && npm run deploy   # frontend + cargo + ad-hoc codesign (macOS)

dalzzack --help
```

Platforms: macOS (WKWebView) and Windows (WebView2). Linux (WebKitGTK) code exists, but CI does not build or test it and releases do not include it. On macOS the deploy script ad-hoc codesigns; the cargo paths leave codesigning to you (run `codesign --force --sign - $(which dalzzack)` if you hit Gatekeeper).

To update a build from source, `upgrade` can run cargo for you. It stops any running daemons first.

```bash
dalzzack upgrade --path ~/src/dalzzack/src-tauri   # local clone (dev mode)
dalzzack upgrade --git https://github.com/<you>/dalzzack   # use a fork
dalzzack upgrade --rev v1.0.0                      # pin to a tag or sha
```

`--no-locked` drops the `--locked` flag passed to cargo install. Use it only when you've intentionally moved past the committed `Cargo.lock`.

A future release will be published to crates.io so `cargo install dalzzack` works without the `--git` URL.

### CI / release

GitHub Actions workflows:
- **CI** (`ci.yml`): format check, Clippy (macOS), unit tests and build (macOS, Windows), smoke test and the WKWebView certificate and cookie checks (macOS)
- **Release** (`release.yml`): on `v*` tags, checks the tag against the version files, creates a draft, and publishes it after all builds pass. It builds the binary with `tauri build --no-bundle` and attaches `dalzzack-macos-universal.tar.gz` and `dalzzack-windows-x64.zip`, each with a `.sha256` file, to the GitHub release. It publishes no installers: they did not put `dalzzack` on `PATH`. A tag with a hyphen, such as `v1.0.0-rc.1`, makes a pre-release, which the `releases/latest` links skip.

---

## Contributing

Issues and PRs are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first. Report security issues privately, as [SECURITY.md](SECURITY.md) describes.

---

## License

MIT. See [LICENSE](LICENSE).
