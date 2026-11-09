# extensions/lsp

## Overview
The `novacode-lsp` crate: a standalone novacode process extension that reports language-server diagnostics after edits. It has its own empty `[workspace]`, so it is not part of novacode's workspace or distribution and is built and installed on its own. Dependencies: `serde`, `serde_json`, `tokio` only.

## Subdirectories & Boundaries
- src: [./src/EGM.md](./src/EGM.md) - JSON-RPC stdio loop subscribed to `after_tool`; lazily started, warm language servers; appends only new or changed errors for touched files.
