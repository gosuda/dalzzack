# dalzzack

Browser automation CLI for AI agents, written in Rust on Tauri 2. The contributor
guide is [CONTRIBUTING.md](CONTRIBUTING.md).

- Build: `cd src-tauri && cargo build`
- Test: `cd src-tauri && cargo test --lib --bins`
- Lint: `cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings`
- `AGENTS.md` is generated, and a test keeps it identical to the installed guide. Edit
  `src-tauri/guide/core.md`, then run `dalzzack install codex-project` in the repo root.
  The core stays under 4 KB: details go in a topic file in `src-tauri/guide/`.

How an agent uses the CLI:

@AGENTS.md
