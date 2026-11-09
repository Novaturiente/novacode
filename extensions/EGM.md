# extensions

## Overview
Optional novacode process extensions, each a separate Rust crate with its own `[workspace]` and `Cargo.lock`. They are not built by the main workspace, `scripts/check.sh` or the release. Each runs as a child process of novacode and speaks line-delimited JSON-RPC 2.0 on stdin/stdout (`initialize`, `tool` / `event`, `shutdown`), as described in `docs/extensions.md`. Both read `NOVACODE_HOME` (else `<home>/.novacode`) inherited from novacode.

## Subdirectories & Boundaries
- image-gen: [./image-gen/EGM.md](./image-gen/EGM.md) - `novacode-image-gen`, the `generate_image` tool via Gemini image models (Antigravity login or Gemini API key).
- lsp: [./lsp/EGM.md](./lsp/EGM.md) - `novacode-lsp`, language-server diagnostics appended to `after_tool` results.

Verify each crate with `cargo test --manifest-path extensions/<name>/Cargo.toml`.
