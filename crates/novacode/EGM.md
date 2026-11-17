# crates/novacode

## Overview

`novacode` is the binary crate: the CLI entry point and run modes (interactive TUI, print, JSON, RPC, background). It wires together the workspace libraries `novacode-base`, `novacode-core`, `novacode-ai`, `novacode-tools`, `novacode-tui` and `novacode-types`.

- **Default features:** `images` and `highlight`.
- **Windows only:** the `self-replace` crate, used by `novacode update`.

## Subdirectories & Boundaries

- src: [src](./src/EGM.md) - The CLI, the live-session host, hub and wire protocol, subagents, goals, schedules, logins and maintenance subcommands.

## Verification

- Run: `cargo test -p novacode`
