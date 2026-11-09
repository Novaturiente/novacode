# novacode-script

## Overview
Crate `novacode-script`: the embedded JavaScript engine for codemode (L7.5) and script extensions (L7.2 tier 1). Dependencies: `rquickjs` (QuickJS), `oxc_*` 0.154 (parse, semantic checks, TS stripping), `serde_json`, `tokio`, `futures-util`.

## Subdirectories & Boundaries
- src: [./src/EGM.md](./src/EGM.md) - `codemode::run` with `Host` and `Limits`, `ext::Extension` with `ExtHost` and `Backend`, `ts::to_js` / `ts::cached`, private pre-run `check`. Sandboxed scripts reach the outside world only through host traits.

## Verification
- Run: `cargo test -p novacode-script`
