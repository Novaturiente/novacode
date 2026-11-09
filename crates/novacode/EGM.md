# crates/novacode

## Overview

`novacode` is the binary crate: the CLI entry point and run modes (interactive TUI, print, JSON, RPC, background, super). It wires together the workspace libraries `novacode-base`, `novacode-core`, `novacode-ai`, `novacode-tools`, `novacode-tui` and `novacode-types`.

- **Default features:** `images` and `highlight`.
- **Windows only:** the `self-replace` crate, used by `novacode update`.

## Subdirectories & Boundaries

- src: [src](./src/EGM.md) - The CLI, the live-session host, hub and wire protocol, subagents, goals, schedules, logins and maintenance subcommands.
- src/superagent: [superagent](./src/superagent/EGM.md) - Super mode, where one super session per root runs tasks as detached sessions. Code owns the board and state machine, the inbox, reviews and merges.

## Verification

- Run: `cargo test -p novacode`
