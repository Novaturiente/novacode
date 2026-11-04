# Super mode

Super mode is for running many tasks at once without switching between sessions yourself. You talk to one session, the **super agent**. For each task you give it, it starts a separate novacode session, keeps track of it on a board, checks the work before calling it done, and tells you when something needs you.

## Starting it

```sh
cd ~/Projects
novacode --super
```

The folder you start in is the super agent's **root**. Tasks can only run in that folder or its subfolders, so starting in `~/Projects` gives it every project below it.

The super agent runs in the background. `novacode --super` starts it if it isn't running and opens a window on it. Closing the window doesn't stop it; run `novacode --super` in the same folder again to reattach. Without a terminal (in a script), `novacode --super` prints the super agent's session id instead of opening a window. `novacode stop <session>` ends it. `-m <model>` picks the super agent's model; other session flags (`--bg`, `-c`, `-r`, `--trust`, `-w`, …) can't be combined with `--super`.

There is one super agent per root, held by a lock file (`super.lock` in its folder, see [The board](#the-board)). Two launches at the same time give one super agent and both attach to it. You can't start one inside, or above, the root of a running one.

## The super agent itself

The super agent is a session with a small tool set: `task` (start and steer tasks), `ask_user`, `memory`, `todo` and `agent` (subagents, such as a scout that looks something up in a project). `super.tools` adds more. Its prompt is its own plus your global `AGENTS.md`; it loads no project `AGENTS.md`, skills or MCP servers, since it doesn't work in any one project. Scouts and tasks it starts get the regular tool set, and each runs with its own folder's `AGENTS.md`, MCP servers and trust.

When its context fills up, its compaction keeps the board (every task, its state and last report) rather than the chat, so it never loses track of a task.

## How it works

- You describe a task. The super agent writes 1–5 **acceptance criteria**: checkable statements of what "done" means. For a non-trivial task it shows them to you first; if you ask to see them first, it asks you on a form and waits for your answer before it starts the task.
- It starts a **task**: a separate background novacode session, labelled like `T7-fix-parser` (`T<n>-` plus a short slug of 1–4 words). For open-ended work it can give the task a goal made from the criteria you approved; the goal starts without another approval card.
- A task that changes code gets its own git worktree (`.worktrees/<label>`, branch `worktree-<label>`), started from the branch your main checkout has checked out, and its edit tools can't change files outside it (shell commands aren't fenced; this guards against mistakes, not against a task set on doing it). A task that only reads or answers a question runs in place. In a folder without git, a task that changes files runs in place, one at a time per folder, and is still checked and reviewed; it can't be merged, so once it is ready the super agent offers to close it.
- Tasks are regular sessions in their project: they load the project's `AGENTS.md`, skills and MCP servers (for example codegraph) and follow your normal permissions. If a project isn't trusted yet, the super agent asks you once before its first task there.
- Untracked project files listed in `worktree.copy` (by default `AGENTS.md`, `CLAUDE.md`, `.mcp.json`, `.codegraph/`, `.claude/`, `.novacode/`) are copied into each new worktree, so a gitignored `AGENTS.md` isn't lost. Codegraph's `daemon.*` lock files and sockets are never copied, so codegraph starts its own daemon for the worktree. Copied files never count as the task's changes.
- The super agent doesn't read code or task transcripts. It only gets each task's short reports, which keeps its own context small, even after days.
- Every message about a task names its label.

## Questions and approvals

When a task asks you something or needs an approval, the form appears in the super agent's window with the task's label in front, like `[T7-fix-parser] Which config file?`. You answer it there. You can also attach to the task and answer there; the super window's copy then closes. (If you answer in the task's window while the super agent is down, the super window's copy may stay open until you dismiss it.) Stopping the super agent never answers a task's card: the card stays open in the task, and you can answer it in the task's own window or in the super window after the super agent starts again.

An unanswered question waits until you answer it. You get a desktop notification (off with `notifications.enabled: false`).

The super agent is woken by code, not by polling: a task's terminal report (`done`, `blocked`, `failed`), a review verdict, a merge result, or a turn that ended without a report each start a turn of the super agent's with a one-line note. For a turn that ended without a report, the note carries the task's last message (cut to 600 characters), so an answer the task gave in plain text isn't lost. Smaller news (an answered card, a reconnect) rides along with the next wake.

## Done means proven and reviewed

A task can't call its work done on its own word:

- **Proof is checked by code.** novacode logs every command a task runs and marks each result `[proof #N]`. A task that changes code cites those numbers as its checks, so their real exit codes go into the report. A check that ran before the last change, or is still running in the background, doesn't count. Besides proof numbers, checks may only name files a check produced (screenshots, logs), which must exist and be newer than the last edit, and URLs; never source files, commands or results, which go in the report's summary. The log lives in memory, so a task that restarts has to run its checks again.
- **Committed work only.** A task can't report done with uncommitted changes. The review checks exactly the commit the task reported; if the task changes its branch during the review, that review is thrown away and a new one runs later.
- **Every code change is reviewed.** An independent review task, on a stronger model, gets the request (with every follow-up the super agent sent the task later, such as "also add a README line"), the criteria and the changes, but not the task's own summary. It shows on the board as its own row, labelled like `T8-review-T7-fix-parser`, and is archived once its verdict is in. It works in its own scratch copy of the task's commit (`/tmp/novacode-review-<label>-<id>/work`, with the `worktree.copy` files), so nothing it runs leaves files in the task's worktree; its edit tools can change files only there. It:
  - judges each acceptance criterion as met or not, with proof;
  - re-runs the checks and opens screenshots and URLs;
  - runs the tests and builds for the changed code and the code that uses it;
  - reads the changes for new bugs and for anything the request didn't ask for;
  - for a bug fix, confirms a test fails before the fix and passes after it.
- A test that fails twice on the commit the task started from is reported as **pre-existing**, and one that fails there but passes on a re-run as **flaky**; neither is blamed on the task. These results are kept per repo, base commit, folder and command (in `baseline/` in the super agent's folder), and reused, so there's no full test run at every task start.
- The reviewer can ask for more proof (a browser check with a screenshot, a test, a reproduction). Findings and proof requests go back to the task word for word; together they get 2 rounds before you are asked. If you want the work as it is after that, the super agent's `task accept` opens a card made by novacode, "Accept <label> without an approved review?"; on yes the task becomes `ready`, marked `accepted without review`. Without your yes on that card it stays `blocked`.
- If the reviewer itself crashes, a new one starts once; if that crashes too, you are asked, with the reason. If the reviewer's model can't be used (denied, unknown, or not logged in), the task is blocked with that reason.
- Only an approved review, or your yes on that card, makes a task that changes code `ready`. If the super agent doubts an approval, it orders a second review, which takes the task out of `ready` again without using up a round.
- A **read task** (a question, nothing to change) reports its answer; the super agent judges it and marks it `ready` (`task accept`), or orders a review task for it too. While no review runs, the task shows `reviewing (your call)`: nothing else will move it, and the super agent is told to judge it right away.

The super agent can tell a task to use named skills.

## The board

The super agent keeps a board of all tasks in its folder, `~/.novacode/super/<root-hash>/`:

- `board.json`: the machine copy, written only by novacode.
- `board.md`: a readable copy.
- `inbox/`: reports tasks write for the super agent to apply (bad or unknown ones go to `inbox/rejected/`).
- `baseline/`: the test results of base commits (see above).
- `root`, `session` and `super.lock`: which root this is, the super agent's session id, and its lock.

The **Tasks** tab in the side panel of a super window shows each task's label, repo, state, model, tokens, age and last summary, refreshed every 2 seconds. While a task is running, the panel opens by itself on the Tasks tab, and it becomes the shown tab when the first tasks appear. Press Enter on a row to open that task in a new tmux window (outside tmux, the attach command is printed). The new window gets your `NOVACODE_HOME`, so it finds the task; if tmux fails, a notice shows tmux's error and the attach command.

| State | Meaning |
|---|---|
| `queued` | Waiting for a free slot |
| `running` | Working |
| `waiting-you` | A question or approval is open |
| `blocked` | Can't go on without you (or waiting for a quota or rate limit to clear) |
| `failed` | Stopped with an error |
| `stopped` | Stopped by you; can continue |
| `interrupted` | Died while super was down |
| `reviewing` | Being reviewed; `reviewing (your call)` for a read task the super agent must judge |
| `ready` | Reviewed (or `accepted without review`); waiting for merge or close (or merged or PR opened, waiting for close) |
| `merging` | Rebasing and merging, or opening a PR |
| `archived` | Closed |

`waiting-you`, `running` and `merging` are the working states. A task that ended up `ready` or `reviewing` keeps that state even if its process exits, since the reviewed commit is what counts.

At most 5 tasks run at once (`super.maxRunning`). New tasks over the limit wait, and reviews start before new work; a message to an existing task never waits. Finished tasks that you haven't closed are not limited: an idle session costs almost nothing, and closing is your call.

If a task runs out of quota, or a provider's rate limit lasts longer than the normal retries, the task shows `blocked` with the provider and, when known, the reset time. No new task or review starts on that provider until it clears (tasks on other providers keep starting), and you get one notice. When the limit clears, the super agent tells the blocked tasks to continue by itself.

A task whose turn ends on a model error, or that stops without reporting, shows `blocked` with the reason. A turn that leaves the task's subagents or background commands still running isn't counted as stopped.

## Controlling tasks

Ask the super agent in plain words, for example "stop T3", "tell T5 to also update the docs", "close T2". It can:

- **stop** a task: the run ends and its queued messages are dropped, but the session stays; a later message continues it, including a goal it was working toward (pressing Esc in an attached task window still keeps queued messages, as today). Stopping a queued task takes it out of the queue; stopping a task under review stops the review;
- **send** a follow-up to any task (the review of that task sees it as part of your request);
- **close** it, only after you confirm on a card: the session is kept, the board row is archived, and the worktree is removed only if its branch is merged into its base;
- **merge** it, only after you confirm on a card: the task rebases, re-runs its checks if the base moved, and merges its branch locally, or opens a PR if you ask (the merge is then finished once the PR is open, and the task shows "PR opened"). A conflict or a dirty main checkout comes back as `blocked`. If your main checkout has a different branch checked out than the one the task started from, the merge is refused and your checkout is left alone. Only one merge per repo runs at a time; a second one is refused, naming the task that holds the repo. If a merge is stopped or dies halfway, the repo is freed and, if your main checkout was left with a merge or rebase in progress or with uncommitted changes to tracked files, the task shows `blocked` with the reason. A task in a folder without git can't be merged; once it is ready, the super agent offers to close it. After a successful merge, the super agent asks whether to close the task; if you already said to close it after the merge, it calls close right away, and the close card is the confirmation. The super agent acts on instructions you already gave and doesn't ask again in text before a merge or close card. A PR merged on GitHub by squash or rebase leaves the branch looking unmerged, so close asks before removing its worktree.

The merge and close cards are made by novacode itself, not written by the super agent's model, so a task's summary can't talk the super agent into skipping them.

## Models

The super agent picks a model tier for each task (`super.tiers`):

| Tier | Default model |
|---|---|
| `quick` | `claude/claude-haiku-5-5` |
| `standard` | `claude/claude-sonnet-5-5` |
| `deep` | `claude/claude-opus-5-5:high` |
| `review` | `codex/gpt-6.1-sol:high` |

The tiers go from weakest to strongest: `quick`, `standard`, `deep`, `review`. You can name a model when you ask for a task. Reviewers run one tier above the task; a task on the `review` tier or on a model you named is reviewed by the `review` tier. A task keeps its model; for steps on other models it uses its own subagents.

`models.deny` lists models that are never used by any session, subagent or judge, for example `["claude-fable*"]`.

## If the super agent restarts

Tasks keep running when the super agent stops (`/restart` in the super window restarts it in place). When it starts again, it reconnects to the live tasks, marks those that died while working `interrupted`, and asks you once whether to resume them. Review tasks that died are restarted by themselves. A resumed task checks its folder and git first, so it doesn't repeat work, and re-runs its checks. If a turn ended while the super agent was away, it is reported only when novacode can tell the turn was the super agent's; otherwise the super agent just gets a note.
