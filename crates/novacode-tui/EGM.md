# novacode-tui

## Overview
Crate `novacode-tui` (layer L9): the fullscreen terminal UI. It takes `AgentEvent`s and `UiEvent`s in and sends `Cmd`s out, depending only on `novacode-types` among novacode crates. Dependencies: `ratatui` 0.30, `crossterm` 0.29, `pulldown-cmark`, `unicode-width`, `unicode-segmentation`, `tokio`, `futures-util`, `serde`, `serde_json`, `tracing`, `ignore`, `base64`; optional `syntect` (feature `highlight`) and `ratatui-image` + `image` (feature `images`); `nix` on unix. Default features: `highlight`, `images`.

## Subdirectories & Boundaries
- src: [./src/EGM.md](./src/EGM.md) - `run` / `run_in` event loop, `App` state, transcript layout, prompt editor, keymap, dialogs (picker, approval card, ask form, login, `/btw`, process popup), side panel, status line, markdown/highlight/images, themes, and terminal setup/restore.

## Verification
- Run: `cargo test -p novacode-tui`
