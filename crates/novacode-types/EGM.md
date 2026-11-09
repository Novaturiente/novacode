# novacode-types

## Overview
Crate `novacode-types` (layer L1): the shared plain-data vocabulary of novacode. Every other crate depends on it for messages, content blocks, tool calls, stream and agent events, the extension UI protocol and token estimates. Dependencies: `serde`, `serde_json` (`preserve_order`), `schemars`.

## Subdirectories & Boundaries
- src: [./src/EGM.md](./src/EGM.md) - All types in `lib.rs`: `Message`, `Block`, `ToolCall`, `Replay`, `StreamEvent`, `AgentEvent`, `ui::UiOp`, `estimate`, `Question`/`Say` forms. No I/O; serde tagged with `"type"`, unknown blocks load as `Block::Unknown`.

## Verification
- Run: `cargo test -p novacode-types`
