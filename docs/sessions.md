# Sessions

This page explains where novacode keeps your conversations and how to resume, branch, rewind, compact, export and run them in the background.

## Where sessions live

Every conversation is one file:

```text
~/.novacode/sessions/<folder path with / replaced by ->/<session id>.jsonl
```

For a project in `/home/me/app` that is `~/.novacode/sessions/-home-me-app/`. Set `NOVACODE_HOME` to move `~/.novacode`.

The file is append-only and holds a tree of messages, not a flat list. That is what makes `/tree`, `/fork` and `/rewind` possible. The file is created at your first real exchange, so an empty session leaves nothing behind.

Session ids are long. You only need the start of one (the first 13 characters are shown in listings).

One novacode process owns a session file at a time. Opening a session that is open elsewhere fails with "session X is open in another novacode process". Use `novacode attach <id>` instead (see [Background sessions](#background-sessions)). If the last line of a file is half written after a crash, novacode repairs it on open and skips lines it cannot read.

## Start, continue and resume

| Command | What it does |
|---|---|
| `novacode` | New session. |
| `novacode -c` | Continue the most recent session in this folder. |
| `novacode -r <id>` | Resume a session by id prefix, or by the path of a `.jsonl` file. It looks in this folder first, then in every folder, and takes the first match. |
| `novacode --no-session` | Don't save this session. |

When you quit, novacode prints `resume with: novacode -r <id>` if the session was saved. If it ended inside a worktree, the hint is `novacode -w <name> -r <id>`, which reopens it there from anywhere in the repo.

**A session remembers its folder.** When it moves (`/worktree`, `-w`, or the model's `enter_worktree`/`exit_worktree`), the new folder is saved in the session. `-r` and `-c` start in the folder the session was last in, wherever you run them from, so trust, project settings and context files are that folder's. If that folder is gone (a removed worktree), it uses the newest earlier folder that still exists, else the one it started in. `/resume` to a session that is in another folder restarts novacode there. With [`shell-init`](#worktrees) your shell ends up in that folder when you quit.

Inside the UI:

| Command | What it does |
|---|---|
| `/resume` | Pick another session from this folder. Sessions with no prompts, and the current one, are not listed. |
| `/new` | Start a new session. The model stays the same. |
| `/name <text>` | Name this session. The name shows in the `/resume` list; without one, the first prompt does. |

`--no-session` also applies to `/new`. With it, no file is written, and file rewind is not available (see below).

## Worktrees

A git worktree is a second checkout of the same repo in another folder, on its own branch. Work there doesn't touch your main checkout. novacode keeps them in `<repo>/.worktrees/<name>` on branch `worktree-<name>`, hides the folder from `git status` (through `.git/info/exclude`), and copies root `.env` files in.

| How | What it does |
|---|---|
| `novacode -w <name>` | Create (or reuse) the worktree and start the session in it. No name: `wt-MMDD-HHMM` (UTC). Works from inside another worktree too. |
| `/worktree` | List the repo's worktrees: branch, uncommitted files, commits not in the main checkout. `*` marks this session's. |
| `/worktree <name>` | Move this session into that worktree, creating it if needed. |
| `/worktree exit` | Move back to the main checkout and keep the worktree. |
| `/worktree remove` | Move back and remove it. Git refuses if there are uncommitted changes; an unmerged branch is kept. |
| `enter_worktree`, `exit_worktree` | The same, as tools the model can call (see [tools](tools.md#enter_worktree-and-exit_worktree)). |

**Quitting inside a worktree** asks `Keep the worktree? [Y/n]` on the terminal. `Enter` keeps it; `n` removes it the same way `/worktree remove` does.

**Your shell can follow.** A program can't change its parent shell's folder, so novacode writes its final folder to `$NOVACODE_CWD_FILE` and a small shell function `cd`s there. Add one line to your shell's config:

```sh
novacode shell-init fish | source          # ~/.config/fish/config.fish
eval "$(novacode shell-init zsh)"          # ~/.zshrc
eval "$(novacode shell-init bash)"         # ~/.bashrc
```

After that, quitting a session that ended in a worktree leaves your shell in that worktree (or in the main checkout if you removed it).

## Branches

You can go back to an earlier prompt without losing anything. Old messages stay in the file as another branch.

| Command | What it does |
|---|---|
| `/tree` | Shows every prompt of the session as a tree (`●` is on your current branch, `○` is not). Pick one to continue from just before it. Its text goes back into the editor. |
| `/rewind` | Like `/tree` but only for this branch, newest first. Can also restore files. See [Rewind](#rewind). |
| `/fork` | Pick a prompt of this branch. novacode makes a new session with everything before it. The prompt text goes into the editor. The old session stays. |
| `/clone` | New session with this whole branch. The old session stays. |

After `/fork` or `/clone` novacode tells you the old session id, so you can go back with `novacode -r <id>`.

You can't go before the first prompt.

## Rewind

`/rewind` shows the prompts of this branch. After you pick one, it asks what to restore:

| Choice | Effect |
|---|---|
| Files and conversation | Files go back to how they were before that prompt, and the conversation moves back to just before it. |
| Conversation only | Same as `/tree` on that prompt. |
| Files only | Only the files change. |

The prompt text comes back into the editor so you can change it and send it again.

### How file checkpoints work

novacode takes a snapshot before each prompt, if the session is saved and `checkpoints.enabled` is true (the default).

**In a git repository.** The whole working tree is snapshot: tracked changes, deletions and untracked files up to 1 MB each. Files that git ignores are not snapshot and rewind does not touch them. Your index, HEAD and branches are never touched. The snapshots are loose objects in the repo's own object store, so `git gc` can remove them after about two weeks. Rewind then tells you the snapshot is gone. Untracked files over 1 MB are left as they are, and the result message lists them.

**Outside git.** There is no folder snapshot. novacode keeps a private copy of each file the first time its own edit or write tools change it, in `~/.novacode/checkpoints/` (a private git repo per folder, so `git` must be installed; nothing prunes it). Only those changes can be rewound. Changes made by shell commands are not covered.

Rewinding files deletes files that did not exist at that point and rewrites the others, so check your work first. The first entry of the `/rewind` list is **Undo the last file rewind** when there is one. That undo list only lives while novacode runs.

## Compact

`/compact` replaces older messages with the memory notes, to free context. It runs at once, or as soon as the running turn ends. novacode also compacts by itself when the context gets large (see [Context](context.md#session-memory-and-compaction) for when). Use `/context` to see where the window goes. Tuning is in `memory.*` settings; see [config](config.md).

## Export

Write the current branch to a file, with secrets masked.

In the UI:

```text
/export          # JSONL
/export html     # one self-contained web page
```

From the shell:

```bash
novacode export                      # newest session in this folder, as JSONL
novacode export 0195f3a1 --html      # by id prefix, as HTML
novacode export -o review.html --html
```

Without `-o` the file is `novacode-<first 13 characters of the id>.jsonl` (or `.html`) in the current folder (not the session folder), and the path is printed. `/export` does the same.

JSONL has one message per line, with a `ts_ms` timestamp added. Only this branch's messages are written: memory notes, compaction and checkpoint entries are left out. HTML shows thinking, tool calls and results, with errors marked. Masking covers your saved logins, and also token patterns if `security.maskSecrets` is on. See [Security](security.md#secrets).

## Background sessions

A background session runs without a window. Start one with an optional first prompt:

```bash
novacode --bg "run the tests and fix what fails"
```

It prints the session id and returns. Other flags such as `-m` work as usual. `--bg` can't be combined with `-p` or `--rpc`.

| Command | What it does |
|---|---|
| `novacode agents` | List live sessions: id, state (`working`, `blocked` when it waits for an answer, or `idle`), whether it is background, model and folder. |
| `novacode attach <id>` | Open a window on a live session. |
| `novacode status` | One-line status of a live session. Shows this tmux pane's session, else the newest in this folder. |
| `novacode status --all` | Every live session. |
| `novacode stop <id>` | End a live session, including a background one. |

`status` also takes `--session <id>`, `--json`, and `--format '<template>'` for scripts and tmux, for example `--format '{state} {ctx}%'`. Fields: `session state detail model level ctx ctx_tokens window hit read write miss cost 5h week branch project cwd bg`.

Every normal window is a live session too, so you can `attach` a second window to it, or see it in `agents`. Leaving an attached window (quit) only detaches. The session keeps running. Close a session for good with `stop`, or quit its own window.

An attached window can't run commands that swap or rebuild the session: `/resume`, `/new`, `/tree`, `/rewind`, `/fork`, `/clone`, `/reload`, `/restart` and `/login`, plus `Alt+Up` and `Alt+Shift+R`. Run those in the session's own window.

## Restart

`/restart` restarts novacode and resumes the same session. Use it after you rebuild or update the binary. Attached windows reconnect by themselves. Your `-c`, `-r` and `-w` flags are replaced by a `-r` for this session, and your other flags are kept.

## See also

- [tui](tui.md): all slash commands and key bindings.
- [config](config.md): `checkpoints.enabled` and `memory.*`.
- [automation](automation.md): print mode, RPC and scripting with background sessions.
- [subagents](subagents.md): sessions started by the model.
- [troubleshooting](troubleshooting.md): when a session won't open.
