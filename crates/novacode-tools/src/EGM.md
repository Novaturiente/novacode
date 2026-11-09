# novacode-tools (src)

## Overview

The tool layer (L5). It defines the `Tool` trait, the per-call `ToolCtx`, the
`ExecutionEnv` seam most file and process access goes through, output guards,
the per-file mutation queue, and the built-in tools (read, write, edit,
apply_patch, bash, grep, find, ls, process, todo, ask_user, tool_search, call,
enter/exit worktree). It also owns the bubblewrap sandbox and image
normalisation.

## Interface

- `lib.rs`: `Tool` trait (`name`, `description`, `schema`, `execute`,
  `sequential`, `large_output`, `keep`, `exposure`, `read_only`, `guidelines`,
  `output_schema`, `def`), `ToolCtx` (`path`, `update`), `ToolOutput`
  (`text`, `error`), `Exposure` (`Listed`, `CodemodeOnly`, `Deferred`,
  `Hidden`), `Exposed` (`scriptable`), `exposed`, `Nested` trait, `Keep`, `Tools`,
  `builtins`, `guidelines`, `schema_for`, `parse`; re-exports `ExecutionEnv`,
  `LocalEnv`, `SandboxedEnv`, `FileState`.
- `env.rs`: `ExecutionEnv` trait (`read`, `write`, `remove`, `stat`,
  `canonical`, `exec`, `sandbox`), `LocalEnv`, `SandboxedEnv`, `ExecSpec`,
  `ExecResult`, `FileMeta`, `TASK_VARS`.
- `files.rs`: `FileState` (`lock`, `saw`, `check_stale`, `record`; stale-file
  guard), `FileGuard`.
- `guard.rs`: `truncate`, `check_budget`, `estimate_tokens`, `Budget`,
  `MAX_LINES` (2000), `MAX_BYTES` (50 KB), `MAX_LINE` (2000), `ACCEPT_LARGE`.
- Built-in tools: `read::Read`, `write::Write`, `edit::Edit` (+ `apply_edits`),
  `patch::ApplyPatch`, `bash::Bash` (`DEFAULT_TIMEOUT` 120 s, `MAX_TIMEOUT`
  600 s), `search::{Grep, Find, Ls}`, `process::ProcessTool`,
  `todo::Todo`, `ask::AskUser`, `toolsearch::{ToolSearch, Call}`,
  `worktree::{EnterWorktree, ExitWorktree}`.
- `process.rs`: `ProcessManager`, `Event`, `Info`, `End`, `Watch`,
  `Attention`, `adopt`, `release`, `stop_inside`, `end_group`,
  `kill_group_now`, `pid_alive`.
- `sandbox.rs`: `Sandbox` (`new`, `mode`, `light`, `available`, `wraps`,
  `args`, `git_snapshot`, `hint`, …), `Config`, `Mode`, `command`, `hint_for`,
  `close_inherited_fds`, `git_dirs`, `project_dirs`, `GitSnap`.
- `ask.rs`: `Asker` trait, `AskUser`, `render`.
- `toolsearch.rs`: `rank` (BM25), `CallArgs`.
- `todo.rs`: `Todo` (`new`, `from_history`), `Status`, `Item`, `render`, `stale_note`, `STALE_AFTER`,
  `MAX_STALE_NOTES`.
- `worktree.rs`: `create`, `create_new`, `add_detached`, `here`, `Here`,
  `remove`, `list`, `copies`, `COPY_DEFAULT`.
- `image.rs`: `normalize`, `sniff`, `is_image`, `mentions`, `attach`, `Image`,
  `MAX_SIDE` (1568 px), `MAX_B64` (4,718,592).

## Invariants

- File and exec access of read, write, edit, apply_patch and bash goes through
  `ExecutionEnv`, so another backend (SSH, sandbox) can replace `LocalEnv`.
  Exceptions use the local disk directly: grep/find/ls (`ignore::WalkBuilder`,
  `std::fs::read_dir`), output spill files (`guard.rs`), `image::mentions` /
  `attach`, the worktree code (`std::fs` and git subprocesses), and
  `ProcessManager::start`, which spawns through `sandbox::command`.
- `ToolOutput::details` is UI-only and never sent to a model.
- Mutations of one real path run one at a time (`FileState::lock`); write and
  edit refuse a file changed since the agent last read it.
- `edit` and `apply_patch` are all-or-nothing: nothing is written if any
  replacement or hunk fails. `edit` preserves CRLF and BOM.
- Output guards apply once, when the result is created: cut at 2000 lines /
  50 KB, full text spilled to a file; output too big for the context is refused
  unless `accept_large_output` is set. Codemode-script callers (`ctx.script`)
  are not clipped.
- `ToolCtx::update` never blocks; a slow UI drops progress.
- `builtins(apply_patch)` returns exactly seven tools; `apply_patch` replaces
  `edit` for model families that use it.
- Settings (`tools.<name>.exposure`) override a tool's declared `Exposure`.
- `sandbox::command` builds every model command, so the bash and process paths
  can't drift.
- Every process the session's `ProcessManager` started is killed when it drops.
- Image normalisation keeps a fitting non-PNG file byte-for-byte. A fitting
  PNG is decoded and kept as PNG unless it is opaque and its JPEG is under
  half its size, in which case the JPEG is sent.

## Side Effects

- Disk: reads and atomic writes of project files, output spill files, process
  log files.
- Processes: shell commands in their own process group (bash, process tool),
  killed as a group on timeout or cancel; on Linux, model commands run under
  bubblewrap with a seccomp filter blocking unix sockets, unless
  `sandbox.mode` is off, bwrap is unavailable in auto mode (then
  unsandboxed), or the light level is used (bubblewrap with no seccomp
  filter, no env stripping, no git pins).
- Git: `enter_worktree` / `exit_worktree` create and remove
  `<repo>/.worktrees/<name>` on branch `worktree-<name>`; the cwd change is
  returned as `details.set_cwd` for the agent to apply.
- User interaction: `ask_user` blocks on an `Asker` until answered or dismissed.

## Verification

- Run: `cargo test -p novacode-tools`
