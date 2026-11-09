# evals

## Overview
Manual evals (L11.5): small real coding tasks run against a real model through the `novacode` binary. They cost tokens, so they are not part of `scripts/check.sh`. `run.sh` is the harness; each task folder is a fixture. Human-facing usage notes live in `evals/README.md`.

## Interface
- `run.sh MODEL [TASK...]`: runs the given tasks (default: every folder with a `prompt.md`) and prints one line per run: task, `PASS`/`FAIL`, seconds, `in=N out=N cache=N codemode=N cm_fail=N`, work folder. With `EVAL_RUNS` above 1, also prints `<task> <pass>/<runs> passed` after each task's runs. Ends with `<pass>/<n> passed with <model>`.
- Env: `NOVACODE` (binary, default `novacode`; relative paths resolve from the caller's directory), `NOVACODE_HOME` (source of `auth.json`, default `~/.novacode`), `EVAL_RUNS` (repeats per task, default 1).
- Task folder contract: `files/` (the starting repo), `prompt.md` (sent with `-p`), `check.sh` (run in the work repo; exit 0 = pass). Optional: `prompt2.md` (second prompt sent with `-c` to test resume), `mcp.json` (copied into the isolated home with `$EVAL_TASK_DIR` replaced by the task's absolute path).
- Task folders (fixtures, not documented separately):
  - `add-flag`
  - `codemode-traps`
  - `fix-bug`
  - `mcp-lookup`
  - `multi-file-bug`
  - `parallel-investigate`
  - `refactor-callers`
  - `rename`
  - `resume`

## Invariants
- Each run uses a fresh work repo (`git init` + one commit of `files/`) and an isolated `HOME`/`NOVACODE_HOME` holding only a symlink to the real `auth.json` (if it exists) and the task's rewritten `mcp.json` (if any), so personal skills, hooks, extensions, MCP servers and context files never load.
- The agent runs with `--json` and stdin from `/dev/null`. Single-prompt tasks use `--no-session`; two-prompt tasks save a session and continue it with `-c`.
- Exit status is 0 only when every run passed.

## Side Effects
- Network and model cost: real model calls through the given `MODEL`.
- Disk: creates `/tmp/nc-eval-<task>.XXXX` work folders (kept for inspection, with `.events.jsonl`, `.events2.jsonl`, `.reply`, `.stderr`, `.stderr2`) and `/tmp/nc-eval-home-<task>.XXXX` homes (deleted after each run).
- A login refreshed during a run is written through the symlink to the real `auth.json`.

## Verification
- Run: `evals/run.sh <provider/model> [TASK...]` (manual; needs a login and costs tokens).
