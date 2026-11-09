# novacode-base

## Overview
Crate `novacode-base` (layer L0): foundations every other novacode crate builds on — paths, storage, settings, ids, logging, secret masking, git hardening and platform helpers. Dependencies: `serde`, `serde_json`, `thiserror`, `tracing`, `uuid`, `regex`, `jsonc-parser`, `tracing-appender`, `tracing-subscriber`.

## Subdirectories & Boundaries
- src: [./src/EGM.md](./src/EGM.md) - `Paths`, atomic/secret/JSONL storage with file locks, layered JSONC `Settings` with a documented key table, `logging::init`, `Redactor`, pinned `git` command builders, and `platform` helpers. Owns all crash-safe disk writes and novacode's own git invocations.

## Verification
- Run: `cargo test -p novacode-base`
