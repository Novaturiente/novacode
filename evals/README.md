# Evals

Small real coding tasks, run by hand against real models (L11.5). They cost tokens, so they aren't part of `scripts/check.sh`. The deterministic tests (faux provider, cache-prefix, replays) run there instead.

```sh
evals/run.sh claude/claude-opus-5-5:low            # every task
evals/run.sh codex/gpt-5.6-luna fix-bug rename     # some tasks
NOVACODE=target/debug/novacode evals/run.sh …      # a dev build
```

Each task folder has `files/` (copied into a fresh git repo under `/tmp`), `prompt.md` (sent with `-p`) and `check.sh` (run in that repo; exit 0 = pass). The work folder is kept, so you can inspect what the model did.

To add a task, make a folder with those three parts. Keep the check strict and quick to run.
