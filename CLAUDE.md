# dalzzack

Browser automation CLI for AI agents, written in Rust on Tauri 2. The contributor
guide is [CONTRIBUTING.md](CONTRIBUTING.md).

- Build: `cd src-tauri && cargo build`
- Test: `cd src-tauri && cargo test --lib --bins`
- Lint: `cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings`
- `AGENTS.md` is generated. It must stay identical to the `DALZZACK_DOCS` block in
  `src-tauri/src/main.rs`. Edit the constant, then run `dalzzack install codex` in
  the repo root.

How an agent uses the CLI:

@AGENTS.md
