# superagent

## Overview

Super mode (L10.9): `novacode --super` runs one long-lived "super" session per root folder. Super's model starts and steers tasks, and each task is a detached `novacode --bg` session. Code (not the model) owns the task board, the state machine, the worktrees, merges, reviews and the cards shown to the user. The spec is `Progress/specs/super-mode.md`.

## Interface

- `mod.rs`: super's own session setup.
  - `prompt(home, root)` builds the system prompt.
  - `TOOLS` lists the model's tools: `task`, `say`, `ask_user`, `memory`, `todo`, `agent`, `history_search`.
  - `tools(settings)` filters that list by settings.
  - `COMPACTION` and `compaction_guide(dir)` are the compaction instructions.
  - `feed_board(slot, sup, paths)` pushes board rows to the hub panel.
  - `Watch`, `watch(settings)`, `max_running(settings)` and `describes_deploy(root)` read the `super.*` settings.
- `entry.rs`: one super per root, stored under `<home>/super/<root-hash>/`.
  - `dir`, `check_root`, `claim` (takes `super.lock` as a `FileLock`), `locked`.
  - `write_session`, `last_session`, `mark_fresh`, `running`, `wait`.
- `board.rs`: the board and task state machine.
  - `State` has these states: `queued`, `running`, `waiting-you`, `blocked`, `failed`, `stopped`, `interrupted`, `reviewing`, `ready`, `merging`, `archived`.
  - `Task` is one board row, `Event` an input to the state machine, `Effect` a resulting action, and `Next` the transition result.
  - Types: `Kind`, `TaskType`, `Limit`/`LimitKind`, `Block`, `Done`, `Asked`, `Flight`, `Held`, `Hold`, `Health`, `Need`, `Train`, `ReportState`, `Verdict`, `MergeResult`, `AfterState`, `Landed`.
  - `next(task, event, lock_holder)` is the pure transition function.
  - `Board::load` and `Board::save` read and write the board. `Board::apply` applies a transition in memory only; `save` persists it.
  - Caps: `FILES_KEPT`, `NOTES_CAP`, `ASKED_KEPT`, `ASKED_TEXT`, `STATUS_SUMMARY`, `RATE_LIMIT_HOLD_MAX`, `GENERAL_KEPT`, `REQUEST_KEPT`.
  - `ROUNDS_USED_UP` is the review-rounds-exhausted reason. `prompt_hash` and `now` are helpers.
- `task.rs`: the `task` tool (`TaskTool`, wrapping `Arc<Super>`) and the `Super` struct.
  - `Super::run(args)` handles these actions: `spawn`, `info`, `send`, `stop`, `close`, `accept`, `merge`, `review`, `note`, `hold`, `resume`, `scope`, `model`, `list`, `read`.
  - Other types and functions: `Config`, `Start`, the `Launcher` trait (`Procs` is the real one), `check_slug`, `session_name`, `preamble` and `DEPLOY_STEP`.
- `link.rs`: one socket link per live task.
  - `Links::start`/`kick`/`open`, the `Conn` type and the `Inform` trait. `HubInform` is the real `Inform`.
  - `Watcher` turns task events, goals and catch-up into board events.
  - Types `Goal` and `CatchUp`, plus constants `LIMIT_CLEARED` and `HEALTH_CHECK`.
- `inbox.rs`: `read(super_dir, board)` returns `Pending` reports in order. `Pending::done` deletes each file. `Report` is the parsed report.
- `report.rs`: the task-side half of the inbox.
  - `Identity`, `ProofLog` (an `after_tool` proof recorder), `Check`, `Edit` and `ReportTool` (the `report` tool).
  - `write_inbox`, `spawn_writer`, `rewrite_unapplied`, `last_seq`.
  - Caps: `MAX_WORDS`, `MAX_VERDICT_CHARS`, `MAX_FINDINGS`, `TAIL_LINES`, `TAIL_CHARS`.
- `review.rs`: review tasks.
  - `scratch_id` and `scratch` locate a review's scratch folder.
  - `Baseline` (`path`/`load`/`find`/`record`), `BaseCheck` and `BaseStatus` hold baseline check results.
- `say.rs`: the `say` tool (`SayTool`) and `ShortAskUser`.
  - `GATES` and `KINDS` list the allowed values, and `NUDGE` is the plain-text nudge.
  - `plain_note`, `trigger` and `card` build the message blocks.
- `context.rs`: the request is rebuilt on every wake.
  - `builder`, `CHAT_BUDGET`, `ModelsFn`.
  - `helper_text` and `fit_notes` call a helper model.
  - `HistorySearch` and `search_history` search past chat.
  - `unrecorded_decision`, `decision_check`, `DECISION_CHECK` and `nudge` check for decisions that were not recorded.
- `models.rs`: worker model tiers, `TIERS` = `quick`/`standard`/`deep`.
  - `Models` (`resolve`, `review`, `tier_of`, `allowed`, `check`, `refresh`, `warm`, `switch_targets`, `usable`, `section`).
  - The `Source` trait with `Real`, plus `Overrides` and `provider`.
- `later.rs`: non-blocking cards.
  - `ASKED`, `answered`, `dismissed`, `keep_general`, `wake`, `note`, `key`, `detach`.
  - `Seen` wraps an `Asker`.
- `remind.rs`: `Pending` tracks open cards. `due`, `asker` and `approver` handle re-announcing them.
- `upgrade.rs`: restart after the binary changes.
  - `request` and `requested` set and read a restart request.
  - `Ident`, `ident(path)` and `ExeWatch` (`current`/`new`/`due`) detect a new binary.

## Invariants

- Only one super runs per root. `entry::claim` holds `super.lock`. A `root` file in the dir catches hash collisions and nested roots.
- Only super's code writes `board.json`, so the board has a single writer. Every transition goes through `board::next`, which is a pure function of the spec's table. The model never edits the board directly.
- Tasks write inbox reports as `<super dir>/inbox/<task id>-<seq>.json` through a temp file and a rename. Super applies them, saves the board, and only then deletes them (`Pending::done`). `inbox::read` itself also deletes reports that were already applied, and moves unparseable, unknown-task, archived-task or wrong-token reports to `inbox/rejected/`.
- The `report` tool exists only in a task's own session (depth 0), never in its subagents. Its `checks` must cite `[proof #N]` entries from `ProofLog`.
- A review brief is reasoning-free. It never includes the task's summary or transcript. Reviews run on super's exact current model and thinking level.
- Super's cards never block its run. The tool returns at once, and the answer arrives later as a wake. Reminders never run super's model.
- Task processes outlive super. An upgrade restart only restarts super, and only when it is idle.

## Side Effects

- Super's state on disk lives under `<home>/super/<root-hash>/`:
  - `root`, `super.lock`, `session`, `last` and `fresh`;
  - `board.json` (atomic write, with a `.bak` fallback) and `board.md`;
  - `inbox/*.json` and `inbox/rejected/` (rejected reports);
  - `briefs/<id>.md` (long review briefs moved out by `Board::compact`);
  - `summaries/<label>.md` (close summaries);
  - `review/<id>/work` scratch worktrees;
  - `baseline/<repo hash>/<commit>.json`.
- Spawns detached `novacode --bg` task processes, sends `kill` to them, and runs `git` (worktree, merge `--ff-only`, status, prune) and `gh` for PRs.
- Opens a Unix socket connection per live task, and relays task cards into super's windows.
- Sends desktop notifications for reminders, task state changes and task questions.
- Saves session entries with `ext` "superagent": `kind` "proof" and `kind` "task" (the task identity, from `report.rs`).
- Re-execs its own binary after an upgrade.

## Verification

- Run: `cargo test -p novacode`
