# crates

## Overview

The Cargo workspace members of novacode: six library crates and one binary crate. Dependencies run one way, from foundations up to the binary:

- `novacode-base` (L0) and `novacode-types` (L1) depend on no other novacode crate.
- `novacode-ai` (L2) and `novacode-tools` (L5) build on `novacode-types` and `novacode-base`.
- `novacode-script` (L7.5) is the embedded JavaScript engine and reaches the outside world only through host traits.
- `novacode-core` is the harness core and SDK. It builds on all of the crates above.
- `novacode-tui` (L9) depends only on `novacode-types` among novacode crates.
- `novacode` is the binary that wires everything together.

## Subdirectories & Boundaries

- novacode-base: [./novacode-base/EGM.md](./novacode-base/EGM.md) - Paths, crash-safe atomic/secret/JSONL storage with locks, layered JSONC settings, ids, logging, secret redaction, pinned git commands, platform helpers.
- novacode-types: [./novacode-types/EGM.md](./novacode-types/EGM.md) - Shared plain data: `Message`, `Block`, `ToolCall`, `StreamEvent`, `AgentEvent`, the extension UI protocol, token estimates. No I/O.
- novacode-ai: [./novacode-ai/EGM.md](./novacode-ai/EGM.md) - Provider layer: `Adapter` and wire adapters, `Providers`/`Route`, `AuthStore` and account rotation, model `Catalog`, retry, schema repair, cross-model transform.
- novacode-tools: [./novacode-tools/EGM.md](./novacode-tools/EGM.md) - `Tool` trait, `ExecutionEnv`, output guards, per-file mutation queue, built-in tools, process manager, bubblewrap sandbox, images.
- novacode-script: [./novacode-script/EGM.md](./novacode-script/EGM.md) - QuickJS runtime for codemode and script extensions, TypeScript stripping and pre-run checks.
- novacode-core: [./novacode-core/EGM.md](./novacode-core/EGM.md) - Agent loop, single tool pipeline, policy/risk gate/judge/trust, append-only JSONL sessions, checkpoints, memory, hooks, extensions, MCP, packages, Claude Code plugins.
- novacode-tui: [./novacode-tui/EGM.md](./novacode-tui/EGM.md) - Fullscreen ratatui terminal UI: event loop, transcript, editor, dialogs, side panel, themes.
- novacode: [./novacode/EGM.md](./novacode/EGM.md) - The binary: CLI and run modes, live-session host/hub, wire protocol, subagents, goals, schedules, super mode.
