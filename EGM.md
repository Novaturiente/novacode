# novacode Blueprint

## System Overview

novacode is a coding agent for the terminal, shipped as one Rust binary. It talks to model providers (subscriptions or API keys), runs tools against the user's project, and loads Claude Code skills, agents, commands and plugins. The human-facing introduction is [README.md](./README.md); design decisions live in `Progress/` (not tracked here).

The code is a Cargo workspace under `crates/`, plus optional extension crates under `extensions/`, maintenance scripts under `scripts/` and manual model evals under `evals/`. `docs/`, `packaging/` and `logos/` hold no code and have no contract.

## Subdirectories & System Boundaries

- crates: [./crates/EGM.md](./crates/EGM.md) - The workspace: `novacode-base`, `novacode-types`, `novacode-ai`, `novacode-tools`, `novacode-script`, `novacode-core`, `novacode-tui` and the `novacode` binary.
- extensions: [./extensions/EGM.md](./extensions/EGM.md) - Optional process extensions (`image-gen`, `lsp`), each its own crate and workspace, speaking line-delimited JSON-RPC 2.0 on stdio.
- scripts: [./scripts/EGM.md](./scripts/EGM.md) - `check.sh` (the pass/fail gate), `build-musl.sh`, `bench.sh`, `gen-catalog.py`, `install.sh`, `install.ps1`.
- evals: [./evals/EGM.md](./evals/EGM.md) - `run.sh` harness and task fixtures for manual, token-costing evals against real models.

## Global Invariants

- Crate dependencies run one way: `novacode-base`/`novacode-types` at the bottom, `novacode` binary at the top. `novacode-tui` depends only on `novacode-types` among novacode crates.
- Every tool call goes through the single pipeline in `novacode-core` (permissions, hooks, output guards, redaction). All file and process access from tools goes through `ExecutionEnv`.
- Sessions are append-only JSONL; the cached system prompt prefix never changes, and history transforms are deterministic so provider prompt caches hold.
- Crash-safe writes (`write_atomic`, `write_secret`, locks) come from `novacode-base`; credential files are 0600.
- Project executable config loads only for trusted folders; `AGENTS.md`/`CLAUDE.md` always load.
- The wire protocol (`crates/novacode/src/wire.rs`) and the core public API are snapshot-tested public surfaces.
- Extensions under `extensions/` are not built by the workspace, `scripts/check.sh` or the release.

## Global Side Effects

- Reads and writes the novacode home (`NOVACODE_HOME`, else `~/.novacode`): settings, `auth.json`, sessions, checkpoints, memory SQLite databases, trust store, caches, logs, run files and sockets.
- Network calls to model providers, OAuth and quota endpoints, models.dev, MCP servers, web search engines and GitHub releases.
- Spawns child processes: shell commands (optionally under bubblewrap), hooks, process extensions, MCP stdio servers, language servers, subagents and `--bg` sessions.
- Writes git objects for checkpoints and creates git worktrees under `<repo>/.worktrees/`.

## System Verification

- Run: `scripts/check.sh` (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -q -- -D warnings`, `cargo test --workspace -q`).
- Per crate: `cargo test -p <crate>`; extensions: `cargo test --manifest-path extensions/<name>/Cargo.toml`.
