# Contributing to dalzzack

Thanks for your interest. dalzzack is small and the surface is well-defined — most contributions land quickly when the change is focused and tested locally.

## Development setup

You need:

- **Rust** (stable, edition 2021) — install via [rustup](https://rustup.rs)
- **Node.js** (≥ 18) and **npm** — used to build the embedded landing page assets and for the deploy script
- Platform tooling:
  - **macOS** — Xcode Command Line Tools (`xcode-select --install`)
  - **Linux** — `libwebkit2gtk-4.1-dev libgtk-3-dev libappindicator3-dev librsvg2-dev` (Debian/Ubuntu)
  - **Windows** — WebView2 runtime (pre-installed on Windows 11; on Windows 10 grab the [Evergreen installer](https://developer.microsoft.com/microsoft-edge/webview2/))

Clone and build:

```bash
git clone https://github.com/gosuda/dalzzack && cd dalzzack
npm install
npm run deploy            # builds frontend + Rust, installs to ~/.cargo/bin, codesigns on macOS
dalzzack --help
```

## Build modes

| Task | Command |
|------|---------|
| Quick iterative build | `cd src-tauri && cargo build` |
| Release build | `cd src-tauri && cargo build --release` |
| Install to PATH | `npm run deploy` (or `cd src-tauri && cargo install --path .`) |
| Run unit tests | `cd src-tauri && cargo test --lib --bins` |
| Run smoke test (real browser) | `bash scripts/smoke.sh` |
| Lint | `cd src-tauri && cargo clippy --all-targets -- -D warnings` |
| Format | `cd src-tauri && cargo fmt` |

## Architecture in one paragraph

The CLI binary (`src/main.rs`) parses args with clap. For instance-management commands (`start`, `stop`, `list`, `tools`, `run`) it acts locally. For tool commands it opens a TCP connection to the running daemon, sends a JSON-RPC-like message, and prints the JSON response. The daemon is the same binary spawned via the internal `daemon` subcommand: it runs a Tauri WebView in the main thread and a tokio TCP server in a background thread. Each tool is dispatched by name in `tools/definitions.rs::execute_tool`. Platform-specific WebView access (navigate, JS eval, mouse) lives under `platform/`; cross-frame coordination and CDP setup live under `webview/`.

## Adding a new tool

A tool needs three things, all in `src-tauri/src/`:

1. **CLI subcommand** in `main.rs` (the `Cli` enum) — clap derives the parser.
2. **`cli_to_tool_call` arm** in `main.rs` — maps the parsed args into `(instance_id, tool_name, json_args)`.
3. **Implementation** in `tools/definitions.rs`:
   - An `async fn tool_<name>(args: &Value) -> Result<Value>`.
   - A dispatcher arm in `execute_tool` (e.g. `"<name>" => tool_<name>(args).await,`).
   - A `ToolDefinition { name, description, input_schema }` entry in `get_tool_definitions()`.

The `dalzzack tools` command will surface drift between dispatcher and schema. Run it after any change in this area and confirm the new tool appears.

**Description guidance for LLM consumers.** Tool descriptions are read by AI agents, not humans. Be specific, lead with the verb, cross-link related tools ("prefer X over Y when…"), and call out non-obvious behaviour (e.g. "iframes resolved via CDP", "fields cleared first").

## Coding standards

- `cargo fmt` before committing. CI rejects unformatted code.
- `cargo clippy --all-targets -- -D warnings` must pass.
- No `unwrap()` or `panic!()` in tool implementation paths — return a structured `failed` JSON response instead. Reserved exceptions: process startup (`run_with_instance_id`) and clearly programmer-error invariants.
- Comments, strings and docs in English. When code must match UI text in another language (for example the Korean "Close" label that `dismiss-modals` looks for), write that text as Unicode escapes and say in a comment what it is.
- New tools must return a JSON envelope with at minimum `{ "status": "completed" | "failed", "error"?: string, "message"?: string }`. See existing tools for the convention.

## Testing

Unit tests live alongside their modules with `#[cfg(test)] mod tests`. Integration tests that need a real WebView go in `src-tauri/tests/` (currently empty — contributions welcome).

The smoke script (`scripts/smoke.sh`) starts a daemon, navigates to `example.com`, takes a screenshot, and stops the daemon. It's the minimum bar for "did I break the happy path" and is wired into CI on macOS.

## PR checklist

- [ ] `cargo fmt` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test --lib --bins` passes
- [ ] `bash scripts/smoke.sh` passes (if you touched the daemon, tools, or platform code)
- [ ] `dalzzack tools` lists any new tool you added
- [ ] `docs/commands.md` updated if you added or renamed a public CLI command
- [ ] CHANGELOG `[Unreleased]` section updated
- [ ] Tested on at least one platform; note which in the PR description

## What we won't merge

- Behaviour that defeats the existing safety relaxations without an opt-out (e.g. *more* SOP disabling).
- New runtime dependencies that aren't justified — dalzzack tries to stay a single binary (about 37 MB on Windows as of 1.0.0).
- Tool names that shadow common shell or browser conventions (`open`, `close`, `kill`).
- Feature flags or backwards-compatibility shims for code paths added during the OSS launch. We will rev the version.

## Reporting bugs

Use GitHub Issues. Include:

- `dalzzack --version`
- OS and OS version
- Steps to reproduce, including the exact CLI command
- What you expected vs what happened
- Daemon log if relevant (logs are at `~/.cargo/bin/logs/dalzzack_<id>_<date>.log` by default on a `npm run deploy` install)

Security issues go to the channel listed in [SECURITY.md](SECURITY.md) instead.
