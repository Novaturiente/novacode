# novacode-core

## Overview

The `novacode-core` crate is the harness core: the session owner task, the agent loop, the tool pipeline, permissions, session storage, memory and all extension points. It sits above `novacode-types`, `novacode-base`, `novacode-ai`, `novacode-tools` and `novacode-script`, and the binary and other frontends embed it as the SDK. Notable external dependencies are `rusqlite` (bundled, for FTS5 memory), `reqwest`, `globset`, `htmd` (HTML to markdown) and `yaml-rust2` (Claude Code frontmatter).

## Subdirectories & Boundaries

- src: [./src/EGM.md](./src/EGM.md) - All crate source. It holds `Agent` and `spawn` → `SessionHandle`, the single tool pipeline, `Policy` with the risk gate and the judge, the append-only JSONL session `Store`, checkpoints, observer memory and the SQLite memory store, hooks, script and process extensions, the MCP `Hub` with OAuth, packages and Claude Code plugins.
- tests: integration tests (`loop.rs`, `api_v1.rs` with the `api-v1.snap` public-API snapshot, `mcp_oauth.rs`) and fixtures. They are not documented here.

## Verification

- Run: `cargo test -p novacode-core`
