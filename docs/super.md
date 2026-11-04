# Super mode

> **Status: designed, not built yet.** This page describes how super mode will work. The commands and settings below do not exist in the current release.

Super mode is for running many tasks at once without switching between sessions yourself. You talk to one session, the **super agent**. For each task you give it, it starts a separate novacode session, keeps track of it on a board, checks the work before calling it done, and tells you when something needs you.

## Starting it

```sh
cd ~/Projects
novacode --super
```

The folder you start in is the super agent's **root**. Tasks can only run in that folder or its subfolders, so starting in `~/Projects` gives it every project below it.

The super agent runs in the background. `novacode --super` starts it if it isn't running and opens a window on it. Closing the window doesn't stop it; run `novacode --super` in the same folder again to reattach. `novacode stop <session>` ends it. `-m <model>` picks the super agent's model; other session flags can't be combined with `--super`.

There is one super agent per root. You can't start one inside, or above, the root of a running one.

## How it works

- You describe a task. The super agent writes 1–5 **acceptance criteria**: checkable statements of what "done" means. For a non-trivial task it shows them to you first.
- It starts a **task**: a separate background novacode session, labelled like `T7-fix-parser`.
- A task that changes code gets its own git worktree (`.worktrees/<label>`, branch `worktree-<label>`), started from the branch your main checkout has checked out, and its edit tools can't change files outside it (shell commands aren't fenced; this guards against mistakes, not against a task set on doing it). A task that only reads or answers a question runs in place. In a folder without git, a task that changes files runs in place, one at a time per folder, and is still checked and reviewed; it can't be merged, so once it is ready the super agent offers to close it.
- Tasks are regular sessions in their project: they load the project's `AGENTS.md`, skills and MCP servers (for example codegraph) and follow your normal permissions. If a project isn't trusted yet, the super agent asks you once before its first task there.
- Untracked project files listed in `worktree.copy` (by default `AGENTS.md`, `CLAUDE.md`, `.mcp.json`, `.codegraph/`, `.claude/`, `.novacode/`) are copied into each new worktree, so a gitignored `AGENTS.md` isn't lost. Codegraph's `daemon.*` lock files and sockets are never copied, so codegraph starts its own daemon for the worktree. Copied files never count as the task's changes.
- The super agent doesn't read code or task transcripts. It only gets each task's short reports, which keeps its own context small, even after days.
- Every message about a task names its label.

## Questions and approvals

When a task asks you something or needs an approval, the form appears in the super agent's window with the task's label in front. You answer it there. You can also attach to the task and answer there; the super window's copy then closes.

An unanswered question waits until you answer it. You get a desktop notification.

## Done means proven and reviewed

A task can't call its work done on its own word:

- **Proof is checked by code.** novacode logs every command a task runs and marks each result `[proof #N]`. A task that changes code cites those numbers as its checks, so their real exit codes go into the report. A check that ran before the last change, or is still running in the background, doesn't count. Screenshots and files must exist and be newer than the last edit. The log lives in memory, so a task that restarts has to run its checks again.
- **Committed work only.** A task can't report done with uncommitted changes. The review checks exactly the commit the task reported; if the task changes its branch during the review, that review is thrown away and a new one runs later.
- **Every code change is reviewed.** An independent review task, on a stronger model, gets the request, the criteria and the changes, but not the task's own summary. It works in its own scratch copy of the task's commit, so nothing it runs leaves files in the task's worktree. It:
  - judges each acceptance criterion as met or not, with proof;
  - re-runs the checks and opens screenshots and URLs;
  - runs the tests and builds for the changed code and the code that uses it;
  - reads the changes for new bugs and for anything the request didn't ask for;
  - for a bug fix, confirms a test fails before the fix and passes after it.
- A test that fails twice on the commit the task started from is reported as **pre-existing**, and one that fails there but passes on a re-run as **flaky**; neither is blamed on the task. These results are kept per repo, base commit and folder, and reused, so there's no full test run at every task start.
- The reviewer can ask for more proof (a browser check with a screenshot, a test, a reproduction). Findings and proof requests go back to the task word for word; together they get 2 rounds before you are asked.
- If the reviewer itself crashes, a new one starts once; if that crashes too, you are asked, with the reason. If the reviewer's model can't be used (denied, unknown, or not logged in), the task is blocked with that reason.
- Only an approved review makes a task `ready`. If the super agent doubts an approval, it orders a second review, which takes the task out of `ready` again without using up a round.

The super agent can tell a task to use named skills, and for open-ended work it can give the task a goal with the criteria you approved.

## The board

The super agent keeps a board of all tasks at `~/.novacode/super/<root-hash>/`:

- `board.json`: the machine copy, written only by novacode.
- `board.md`: a readable copy.

The **Tasks** tab in the side panel shows each task's label, repo, state, model, tokens, age and last summary. Press Enter on a row to open that task in a new tmux window (outside tmux, the attach command is printed).

| State | Meaning |
|---|---|
| `queued` | Waiting for a free slot |
| `running` | Working |
| `waiting-you` | A question or approval is open |
| `blocked` | Can't go on without you (or waiting for a quota or rate limit to clear) |
| `failed` | Stopped with an error |
| `stopped` | Stopped by you; can continue |
| `interrupted` | Died while super was down |
| `reviewing` | Being reviewed |
| `ready` | Reviewed; waiting for merge or close (or merged or PR opened, waiting for close) |
| `merging` | Rebasing and merging, or opening a PR |
| `archived` | Closed |

`waiting-you`, `running` and `merging` are the working states. A task that ended up `ready` or `reviewing` keeps that state even if its process exits, since the reviewed commit is what counts.

At most 5 tasks run at once (`super.maxRunning`). New tasks over the limit wait, and reviews start before new work; a message to an existing task never waits. Finished tasks that you haven't closed are not limited: an idle session costs almost nothing, and closing is your call.

If a task runs out of quota, or a provider's rate limit lasts longer than the normal retries, the task shows `blocked` with the provider and, when known, the reset time. No new task or review starts on that provider until it clears (tasks on other providers keep starting), and you get one notice. When the limit clears, the super agent tells the blocked tasks to continue by itself.

A task whose turn ends on a model error, or that stops without reporting, shows `blocked` with the reason. A turn that leaves the task's subagents or background commands still running isn't counted as stopped.

## Controlling tasks

Ask the super agent in plain words, for example "stop T3", "tell T5 to also update the docs", "close T2". It can:

- **stop** a task: the run ends and its queued messages are dropped, but the session stays; a later message continues it, including a goal it was working toward (pressing Esc in an attached task window still keeps queued messages, as today). Stopping a queued task takes it out of the queue; stopping a task under review stops the review;
- **send** a follow-up to any task;
- **close** it, only after you confirm on a card: the session is kept, the board row is archived, and the worktree is removed only if its branch is merged into its base;
- **merge** it, only after you confirm on a card: the task rebases, re-runs its checks if the base moved, and merges its branch locally, or opens a PR if you ask (the merge is then finished once the PR is open, and the task shows "PR opened"). A conflict or a dirty main checkout comes back as `blocked`. If your main checkout has a different branch checked out than the one the task started from, the merge is refused and your checkout is left alone. Only one merge per repo runs at a time. If a merge is stopped or dies halfway, the repo is freed and, if your main checkout was left mid-merge, the task shows `blocked` with the reason. After a successful merge, the super agent asks whether to close the task. A PR merged on GitHub by squash or rebase leaves the branch looking unmerged, so close asks before removing its worktree.

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

Tasks keep running when the super agent stops. When it starts again, it reconnects to the live tasks and asks you once whether to resume any that died while working. A resumed task checks its folder and git first, so it doesn't repeat work, and re-runs its checks. If a turn ended while the super agent was away, it is reported only when novacode can tell the turn was the super agent's; otherwise the super agent just gets a note.
