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

## Start, continue and resume

| Command | What it does |
|---|---|
| `novacode` | New session. |
| `novacode -c` | Continue the most recent session in this folder. |
| `novacode -r <id>` | Resume a session by id prefix. It also finds sessions from other folders. |
| `novacode --no-session` | Don't save this session. |

When you quit, novacode prints `resume with: novacode -r <id>` if the session was saved.

Inside the UI:

| Command | What it does |
|---|---|
| `/resume` | Pick another session from this folder. Sessions with no prompts, and the current one, are not listed. |
| `/new` | Start a new session. The model stays the same. |
| `/name <text>` | Name this session. The name shows in the `/resume` list; without one, the first prompt does. |

`--no-session` also applies to `/new`. With it, no file is written, and file rewind is not available (see below).

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

**In a git repository.** The whole working tree is snapshot: tracked changes, deletions and untracked files up to 1 MB each. Your index, HEAD and branches are never touched. The snapshots are loose objects in the repo's own object store, so `git gc` can remove them after about two weeks. Rewind then tells you the snapshot is gone. Untracked files over 1 MB are left as they are, and the result message lists them.

**Outside git.** There is no folder snapshot. novacode keeps a private copy of each file the first time its own edit or write tools change it, in `~/.novacode/checkpoints/`. Only those changes can be rewound. Changes made by shell commands are not covered.

Rewinding files deletes files that did not exist at that point and rewrites the others, so check your work first. The first entry of the `/rewind` list is **Undo the last file rewind** when there is one. That undo list only lives while novacode runs.

## Compact

`/compact` replaces older messages with the memory notes, to free context. It runs at once, or as soon as the running turn ends. novacode also compacts by itself when the context gets large. Use `/context` to see where the window goes. Tuning is in `memory.*` settings; see [config](config.md).

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

Without `-o` the file is `novacode-<first 13 characters of the id>.jsonl` (or `.html`) in the current folder, and the path is printed. JSONL has one message per line.

## Background sessions

A background session runs without a window. Start one with an optional first prompt:

```bash
novacode --bg "run the tests and fix what fails"
```

It prints the session id and returns. Other flags such as `-m` work as usual. `--bg` can't be combined with `-p` or `--rpc`.

| Command | What it does |
|---|---|
| `novacode agents` | List live sessions: id, state (`working` or `idle`), whether it is background, model and folder. |
| `novacode attach <id>` | Open a window on a live session. |
| `novacode status` | One-line status of a live session. Shows this tmux pane's session, else the newest in this folder. |
| `novacode status --all` | Every live session. |
| `novacode stop <id>` | End a live session, including a background one. |

`status` also takes `--session <id>`, `--json`, and `--format '<template>'` for scripts and tmux, for example `--format '{state} {ctx}%'`. Fields: `session state detail model level ctx ctx_tokens window hit read write miss cost 5h week branch project cwd bg`.

Every normal window is a live session too, so you can `attach` a second window to it, or see it in `agents`. Leaving an attached window (quit) only detaches. The session keeps running. Close a session for good with `stop`, or quit its own window.

An attached window can't run commands that swap or rebuild the session: `/resume`, `/new`, `/tree`, `/rewind`, `/fork`, `/clone`, `/reload`, `/restart` and `/login`, plus `Alt+Up` and `Alt+Shift+R`. Run those in the session's own window.

## Restart

`/restart` restarts novacode and resumes the same session. Use it after you rebuild or update the binary. Attached windows reconnect by themselves. Your `-c` or `-r` flags are replaced by a `-r` for this session, and your other flags are kept.

## See also

- [tui](tui.md): all slash commands and key bindings.
- [config](config.md): `checkpoints.enabled` and `memory.*`.
- [automation](automation.md): print mode, RPC and scripting with background sessions.
- [subagents](subagents.md): sessions started by the model.
- [troubleshooting](troubleshooting.md): when a session won't open.
