# novacode binary sources

## Overview

This folder is the `novacode` binary crate. It holds the clap CLI, the print/JSON/RPC/background/super run modes, the live-session host and hub, the wire protocol, subagents, goals, logins, and the maintenance subcommands. The engine, tools and TUI live in the sibling crates `novacode-core`, `novacode-tools`, `novacode-tui`, `novacode-ai` and `novacode-base`.

## Interface

- `main.rs`: the `Cli` struct (clap) and `fn main`/`run`.
  - Flags: `-p/--print`, `-m/--model`, `-c/--continue`, `-r/--resume`, `-w/--worktree`, `--mode`, `--profile`, `--json`, `--rpc`, `--bg`, `--super`, `--trust`, `-v/--verbose`, `--no-session`, `--no-context-files`, `--api-key`, `--trace` and `--profile-startup`. Hidden: `--agent`, `--tools`, `--mcp`, `--super-serve`, `--super-resume`, `--super-upgraded`.
  - Subcommands: `login`, `logout`, `models`, `auth status`, `doctor`, `export`, `attach`, `panel`, `agents`, `restore`, `stop`, `status`, `debug bundle`, `stats`, `install`, `uninstall`, `update`, `packages`, `mcp`, `import claude`, `trust`, `shell-init`. Hidden: `claude-hook`, `debug crash`.
- `wire.rs`: the JSONL wire protocol (L8.7), `PROTOCOL = 1`.
  - Types: `ServerMsg`, `ClientCmd`, `Request`, `State`, `Activity`/`ActState`, `CacheStats`, `ClientToolDef`.
  - `capabilities()` lists the server's capabilities, and `line(msg)` serializes one JSONL line.
  - Snapshot: `../tests/wire.snap.jsonl`.
- `host.rs`: `Host` carries out window commands.
  - `handle`/`handle_as`, `slash`, `ext_command`, `commands`, `user_shell`, `export`, `follow_account`.
  - `MODE_COMMANDS` lists the mode slash commands.
- `hub.rs`: `Hub` is one live session shared by many windows.
  - `start`, `connect`/`disconnect`, `set_board`, `state`, `history`, `stop`, `seed_ui`, `ui_log`.
  - `update`, `answer`/`answer_ask`, `close`, `set_focus`/`focused`.
  - `compactions` counts compaction entries.
  - `HubSlot` holds the hub, and `Dismiss` is the card-dismiss callback type.
- `serve.rs`: the session socket, RPC on stdio and run files.
  - `serve`, `go_live`, `Live`, `RunFile`, `run_dir`.
  - `live_sessions`, `interrupted_sessions`, `find_live`, `closing`, `agents`.
  - `stop`, `attach` and `attach_loop` back `novacode stop` and `novacode attach`.
  - `RELOADING` is the reload message.
- `agents.rs`: subagents (L10.1), the `agent` tool, definitions and file claims.
  - `Def`, `defs`, `configure`, `grant`, `Claims`, `Supervisor`, `Setup`, `Place`.
  - `AgentTool` is the `agent` tool, `AgentCfg` its config, and `ModelLookup`/`ModelCheck` are model callback types.
  - `MAX_DEPTH = 2`.
- `relay.rs`: `Card`, `Label` and `relay(asker, approver, card, label)` re-open a child's cards in the parent. Shared by `agents.rs` and `superagent/link.rs`.
- `goals.rs`: `/goal`.
  - `Goals`, `Goal`, `State`, `Judge`, `Config`.
  - `GoalTool` is the `goal` tool, and `START_NOTE` is the start note.
- `schedule.rs`: the `schedule` tool (L10.5): `Schedule`, `Item`, `ScheduleTool`, `parse_duration`.
- `models.rs`: model choice.
  - `Limits`, `price`, `spent`, `usable`, `session_route`, `spawn_check`.
  - `switch`, `helper`, `list`, `command`.
- `login.rs`: `oauth`, `check_key`, `methods`, `providers`, `key_note`, `print_step`, `catch_or_input`, `stdin_lines`, `Io`.
- `activity.rs`: `Sinks` (`for_window`/`for_attach`/`headless`) with `spawn`, `follow`, `desktop` and `project`. These cover tmux, herdr, `hooks.onActivity` and notifications.
- Maintenance commands, one per file:
  - `doctor.rs`: `auth_status`, `doctor`.
  - `diag.rs`: `install_crash_hook`, `bundle`, `gather`, `scrub`, `mark`, `profile`.
  - `docs.rs`: `install`, `dir`, `brief`.
  - `import.rs`: `plan`, `run`, `hook`, `nova_tool`, `Plan`, `Change`.
  - `stats.rs`: `run`, `files`, `session_report`, `all_report`.
  - `status.rs`: `run`, `render`.
  - `update.rs`: `run`, `check`, `newer`, `install_kind`, `REPO`.
  - `updates.rs`: `at_start`, `enabled`, `path`, `Cache`.
  - `usage.rs`: `report`, `quotas`.
- `superagent/`: super mode (L10.9). See [superagent](./superagent/EGM.md).

## Invariants

- The wire protocol is a public surface. Changes must keep `tests/wire.snap.jsonl` passing. `PROTOCOL` is a plain number. `attach` refuses any `hello.protocol` other than `PROTOCOL`; the subagent supervisor and the super task links read `Hello` but don't check `protocol`.
- The hub fans events out to every client in order. Activity sinks run in one task each, so updates stay in order.
- Subagent depth is capped at `MAX_DEPTH = 2`. A child (`novacode --rpc`, piped stdio) dies with its parent.
- Card relay has a single implementation (`relay.rs`), shared by subagents and super tasks.
- `novacode update` only replaces release installs. It never overwrites AUR or source builds, and never touches `~/.novacode`.
- The update check asks at most once per 24 h, cached in `~/.novacode/cache/updates.json`. It never installs anything.
- Schedules live in tool-result details in the session file. There is no system cron.

## Side Effects

- Writes under `~/.novacode`: the docs at `docs/` (rewritten when the `<version> <hash of shipped pages>` stamp changes, so on version or page change), run files and session sockets (`run_dir`), crash logs and traces (`logs/trace-<id>.jsonl`), the update cache (novacode release and package updates), settings, logins and keys, and imported Claude Code config.
- Spawns child `novacode --rpc` subagents and detached `--bg` sessions.
- Network: provider OAuth and keys, model catalog refresh, GitHub release checks, package remote checks, and downloads (self-replace on Windows).
- Sets tmux pane options, runs the `hooks.onActivity` command and sends desktop notifications.
- `debug bundle` writes `novacode-debug-<ms>.tar.gz` with secrets masked. `export` writes `novacode-<id>.jsonl|html`.

## Verification

- Run: `cargo test -p novacode`
