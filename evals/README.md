# Evals

Small real coding tasks, run by hand against real models (L11.5). They cost tokens, so they aren't part of `scripts/check.sh`. The deterministic tests (faux provider, cache-prefix, replays) run there instead.

```sh
evals/run.sh claude/claude-opus-5-5:low            # every task
evals/run.sh codex/gpt-5.6-luna fix-bug rename     # some tasks
EVAL_RUNS=5 evals/run.sh claude/claude-opus-5-5:low resume   # 5 runs, pass count per task
NOVACODE=target/debug/novacode evals/run.sh …      # a dev build
```

Each task folder has `files/` (copied into a fresh git repo under `/tmp`), `prompt.md` (sent with `-p`) and `check.sh` (run in that repo; exit 0 = pass). The work folder is kept, so you can inspect what the model did.

## What a run prints

One line per run: task, `PASS`/`FAIL`, seconds, tokens and the work folder. Tokens are summed over every assistant message of the run (both prompts for a two-prompt task): `in` input, `out` output, `cache` cache-read. They come from the `--json` event stream, saved as `.events.jsonl` (and `.events2.jsonl`) in the work folder. The last reply's text is in `.reply`, stderr in `.stderr`.

With `EVAL_RUNS=N` every task runs N times, each in a fresh work folder, and a `<task> k/N passed` line follows its runs. Models aren't deterministic, so one run says little about a change; use several runs to compare.

## Isolation

Each run gets its own `NOVACODE_HOME` and `HOME` (a temp folder), which holds only a symlink to your `auth.json` (from `$NOVACODE_HOME`, else `~/.novacode`). Your skills, hooks, extensions, MCP servers, settings and context files don't load, so results don't depend on your setup. A login refreshed during the run is written through the symlink to your real `auth.json`, under the same lock as your other sessions. The temp home is deleted after the run.

## Optional task parts

- `prompt2.md`: a second prompt. The first prompt then runs with a saved session, and `prompt2.md` runs after it with `-c` (continue the latest session) in the same work folder, before `check.sh`. Use it to test resuming.
- `mcp.json`: copied into the isolated home as its `mcp.json`, with every `$EVAL_TASK_DIR` replaced by the task folder's absolute path, so a task can use an MCP server it ships (see `mcp-lookup/server.py`). MCP tools are reached through codemode.

## Tasks

| Task | Tests |
|---|---|
| `fix-bug` | Fix a bug the tests show |
| `add-flag` | Add a CLI flag, keep old behaviour |
| `rename` | Rename a function everywhere |
| `multi-file-bug` | Fix a cause in another module |
| `refactor-callers` | Change a signature, update callers |
| `mcp-lookup` | Get a value from an MCP tool |
| `resume` | Finish work from the saved session |
| `parallel-investigate` | Fix 4 modules, subagents allowed |

To add a task, make a folder with `files/`, `prompt.md` and `check.sh`. Keep the check strict and quick to run. Check that it fails on the starting files and passes once the intended fix is applied by hand.
