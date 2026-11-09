# novacode-tools

## Overview

Crate `novacode-tools`: the tool layer (L5) of novacode. It depends on
`novacode-types` and `novacode-base`, uses ripgrep's library crates for search,
and has one feature, `images` (default on), which enables image conversion
through the `image` crate. Unix builds use `nix` for signals; Windows builds
use `win32job` for process-tree kills.

## Subdirectories & Boundaries

- src: [./src/EGM.md](./src/EGM.md) - `Tool` trait, `ToolCtx`,
  `ExecutionEnv`, output guards, per-file mutation queue, built-in tools
  (read, write, edit, apply_patch, bash, grep, find, ls, process, todo,
  ask_user, tool_search, call, worktrees), bubblewrap sandbox and image
  normalisation.

## Verification

- Run: `cargo test -p novacode-tools`
