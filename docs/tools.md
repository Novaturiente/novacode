# Tools

This page lists the tools the model can call, what each one does, how output is capped, and how tools that are not shown up front are reached.

## Built-in tools

| Tool | What it does |
|---|---|
| `read` | Reads a text file, or shows an image. |
| `write` | Creates or overwrites a file. |
| `edit` | Replaces exact text in a file. |
| `bash` | Runs a shell command. |
| `grep` | Searches file contents with a regex. |
| `find` | Finds files by glob. |
| `ls` | Lists a directory. |
| `process` | Runs and manages long-running commands. |
| `todo` | Keeps a task list for the session. |
| `ask_user` | Asks you structured questions. |
| `skill` | Activates a [skill](skills.md) by name: returns its text and applies its `allowed-tools` / `disallowed-tools` for the turn, or runs it as a subagent (`context: fork`). Only there when you have skills. |
| `agent` | Starts and manages subagents. |
| `goal` | Works toward a goal until it is done. |
| `enter_plan_mode`, `plan_complete` | Plan mode: propose it, then submit the plan. |
| `memory` | Long-term memory across sessions. |
| `recall` | Reads the source behind a compaction note. |
| `codemode` | Runs a short JavaScript/TypeScript script that calls other tools. |
| `tool_search`, `call` | Find and call tools that are not listed up front. |
| `web_fetch`, `web_search` | Fetch a page; search the web. (deferred) |
| `enter_worktree`, `exit_worktree` | Work in a separate git worktree. (deferred) |
| `schedule` | Message yourself later. (deferred) |
| `session_search` | Search earlier sessions. (deferred) |
| `classify` | Classifier models, reached from scripts only. |

Some tools only exist when they make sense. `agent` and `goal` need a live window or RPC client and are not available below the subagent depth limit. `ask_user`, `enter_plan_mode` and `plan_complete` need someone to answer. `schedule` needs a live window or RPC client. `session_search` needs a saved session. `recall` needs `memory.enabled` and a saved session (so not with `--no-session`).

Every call, including calls from `codemode` scripts, goes through the same permission check and hooks. See [Security](security.md).

### How calls run

- Calls in one reply run at the same time, and their results are added in order. If any call in the reply is one that must run alone (`ask_user`, `todo`, `goal`, `tool_search`, `schedule`, the worktree tools, the plan tools), the whole reply runs one call at a time.
- Permission checks and approval cards happen first, one card at a time.
- An unknown tool name gets a "did you mean" hint. String arguments are fixed to the type the schema wants (`"5"` becomes 5, `"true"` becomes true). Arguments that are not valid JSON are not run; the model is told to resend.

### Output caps

Output that reaches the model is cut at **2000 lines or 50 KB**, and any line longer than 2000 bytes is cut with a `… [+N chars]` marker (a base64 blob or a minified file can't fill the context). When it is cut, the full text is saved to `~/.novacode/spill/<session id>-<call id>.txt` (secrets already masked) and the result tells the model where. `bash` keeps the end of the output; most other tools keep the start.

Two limits protect the context window:

- Output over 30% of the window, or over 50,000 tokens, is refused unless the model passes `accept_large_output: true`.
- Output that would take the context past 90% of the window is refused even with it. The model is told to narrow the query or ask you to compact.

`accept_large_output` is only offered on tools that can return a lot (`read`, `grep`, `find`, `ls`, `bash`, `process`, the web tools and so on), not on `write`, `edit`, `todo`, `ask_user` and similar.

### read

| Param | Meaning |
|---|---|
| `path` | File path, relative to the working directory or absolute. `~/` works. |
| `offset` | First line, 1-based. |
| `limit` | Max lines (capped at 2000). |

Lines longer than 2000 characters are clipped with `… [line clipped at 2000 chars; use grep or bash \`cut -c\` to see the rest]`, since `offset` can't reach the rest of a line. Files over 64 MiB are refused before loading; use `grep`, or `sed -n 'a,bp' file` through `bash`. A read is also cut at 50 KB. When more is left, the result says which `offset` to continue with. An `offset` past the end is an error. Images come back as images, converted and shrunk to fit if needed. Binary files (a NUL byte near the start) are refused. A directory is an error: use `ls`.

### write

Params: `path`, `content`. Creates missing folders. If the file changed on disk since the agent last read or wrote it, the write is refused so it does not overwrite your edit. A file the agent never read in this session is overwritten without that check.

### edit

Params: `path`, `edits` (a list of `old_text` / `new_text`). Each `old_text` must match exactly one place; overlapping matches count too (`}\n}\n` in `}\n}\n}\n` is 2). If an exact match fails, a whole-line match is tried that ignores trailing spaces and reads curly quotes, dash variants and non-breaking spaces as their plain forms. All edits apply together or none do. Two edits whose matches overlap are refused; merge them into one. CRLF line endings and a BOM are kept. If the file changed on disk since the agent last read it, `edit` still applies when every `old_text` matches exactly once, and the result says the file had changed. Only a failed match adds "changed on disk since you last read it". Edits to the same file run one at a time. The result is a one-line summary of where it edited. If the match was fuzzy, it also includes the diff so the model can check it.

`apply_patch` (a patch-format tool that can replace `edit` for some models) exists in the code but is not turned on in any release path yet, so the model does not see it.

### bash

| Param | Meaning |
|---|---|
| `command` | Run with `bash -c` in the working directory. |
| `timeout` | Seconds. Default 120, max 600. |
| `intent` | One short sentence on why; shown to you. |
| `justification` | Only when the safety check asks for it with nobody to approve. See [Security](security.md). |

Each call is a fresh shell: `cd` and `export` don't carry over to the next call. stdout and stderr are merged. stdin is closed, so interactive programs fail instead of hanging. The command sees `NOVACODE_SESSION_ID`. In memory, only the last 4 MB of output is kept.

A command that passes its timeout is not killed: it moves to the process manager and keeps running, and the model gets its id. If it is still running at twice the timeout, you are asked whether to keep it. (Without a process manager, such as in some embedded runs, it is killed at the timeout.)

### grep, find, ls

`grep` and `find` respect `.gitignore` (also outside a git repository) and skip hidden files (dotfiles). `grep` also skips binary files (a NUL byte). `ls` lists everything, dotfiles and ignored files included.

| Tool | Params | Default cap |
|---|---|---|
| `grep` | `pattern` (Rust regex, or literal text with `literal`), `path`, `glob`, `ignore_case`, `context` lines, `limit` | 100 matches; the real total is reported |
| `find` | `pattern` (glob, e.g. `**/*.rs`; a pattern with no `/` matches file names anywhere), `path`, `limit` | 1000 results (limit 1 to 5000) |
| `ls` | `path`, `limit` (1 to 5000) | 500 entries; sorted, directories end in `/`, dotfiles included |

`grep` limit is 1 to 1000, `context` is at most 10, and match lines are clipped at 500 characters. All three only search the local file system.

### todo

A small task list. A call with no fields shows the list. The list is saved with the session and comes back on resume.

| Param | Meaning |
|---|---|
| `set` | Replace the whole list with these strings. |
| `add` | Append strings. |
| `done` | Ids to mark done. |
| `drop` | Ids to cancel. |
| `start` | The id being worked on now (only one is active). |
| `note` | What the started item is doing. |

When every item is done or dropped, an `add` without `set` starts a new list instead of appending to the finished one.

The list shows `done/total` first (dropped items are not counted in the total), then one line per item: `○` pending, `◐` active, `✓` done, `✗` dropped.

### ask_user

Asks you one or more questions in one form. Params: `questions`, each with `question`, an optional `header`, `options` (2 to 6 suggested; one can be marked `recommended`, and each can have a `description` and a `preview`), and `multi` to allow several picks. You can always type your own answer, or press `n` on a choice to add a note that is sent with it (the model sees `choice; note`). If you dismiss the form, the model is told to ask in chat, or to go on with its best judgement and say what it assumed.

### agent and goal

`agent` runs subagents: actions `spawn`, `list`, `status`, `log`, `read`, `send`, `wait`, `stop`. `goal` takes `set` (`objective`, `criteria`, and `ordered` to do the todo steps in order), `update` (change the open goal's `objective`, `criteria` or `ordered`), `complete` (with `evidence`), `block` (with `reason`) and `status`. See [Subagents](subagents.md).

### Plan mode

`enter_plan_mode` asks you to switch to read-only plan mode before non-trivial work. In plan mode the model calls `plan_complete` with a markdown plan. When you approve it, plan mode ends and the model builds it.

### memory and recall

`memory` has three actions: `remember` (save a lasting fact, with optional `scope` of `project` or `global`, `kind` and `pin`), `search` (`query`, `limit` default 10, max 50) and `forget` (by id). `recall` takes one note id from a compaction summary and returns the exact source behind it. See [Context](context.md).

### web_fetch and web_search

`web_fetch` takes `url` and an optional `format` (`markdown` by default, `text`, or `html`). Downloads stop at 5 MB and time out after 30 seconds.

`web_search` takes `query` and optional `urls` to read closely. It tries these engines in order and uses the first that works: an Antigravity login, a Gemini API key (`GEMINI_API_KEY`), Brave (`BRAVE_API_KEY`), then your SearXNG instance (`web.searxngUrl`). With none set up, it says so. Setting `web.native` lets models that have their own web search use it. See [Config](config.md).

### enter_worktree and exit_worktree

`enter_worktree` (params `name`, made of letters, digits, `.`, `_` and `-`, and `base`, a commit or branch, default `HEAD`) creates or reuses `<repo>/.worktrees/<name>` on branch `worktree-<name>`, copies root `.env` files in, and moves the session there. `.worktrees/` is hidden from git through `.git/info/exclude`. Inside a worktree it refuses: call `exit_worktree` first. `exit_worktree` (no params) moves back and asks you whether to keep or remove it; with nobody to ask, it keeps it. Removal refuses uncommitted changes and keeps unmerged branches.

### schedule and session_search

`schedule` has `action` `add` (a `message` plus `in` like `90s`, `10m`, `1h30m`, or `at` as local `HH:MM` for the next time it occurs, and optional `every` for a repeat interval), `list` and `cancel` (with `id`). A due message starts a turn, and fires on resume if the session was closed.

`session_search` finds earlier messages that contain all your words. Params: `query`, `all_projects` (default: this folder only), `limit` (default 20, max 100). The current session is skipped. Hits are grouped by session.

## Deferred tools and tool_search

Every listed tool costs tokens on every request. Rarely used tools are **deferred**: they stay out of the request. Instead the model gets `tool_search`, whose description names the deferred tools. It searches them by what they do and returns their descriptions and schemas (`query`, optional `limit`, default 5, max 20).

Once a tool is found:

- on models that accept tool changes in-line, it becomes directly callable;
- on other models the model calls it through `call` with `{"name": "<tool>", "arguments": {...}}`.

Either way, permissions and hooks apply to the real tool. `tool_search` and `call` only appear when at least one tool is deferred.

By default `web_fetch`, `web_search`, `enter_worktree`, `exit_worktree`, `schedule` and `session_search` are deferred. MCP tools can be deferred too; see [MCP](mcp.md).

## Tool exposure

Change how a tool reaches the model with `tools.<name>.exposure` in settings:

| Value | Meaning |
|---|---|
| `listed` | Declared in every request. |
| `deferred` | Found through `tool_search`. |
| `codemode-only` | Only callable from `codemode` scripts (`codemode` also works). |
| `hidden` | Not available to the model. |

```json
{
  "tools": {
    "web_search": { "exposure": "listed" },
    "schedule": { "exposure": "hidden" }
  }
}
```

`classify` is `codemode` by default: scripts reach it as `models.classify()`. Its providers need their own key: `TYPESAFE_API_KEY` for `typesafe`, or `CLOUDFLARE_API_KEY` (or `CLOUDFLARE_API_TOKEN`) plus `CLOUDFLARE_ACCOUNT_ID` for `cloudflare-workers-ai`.

## The process manager

The `process` tool runs commands that keep going across turns: dev servers, watchers, long builds. Actions (all but `start`, `list` and `clear` need the process `id`, e.g. `proc_3`):

| Action | What it does |
|---|---|
| `start` | Starts `command` (with a `name`, optional `cwd`) in its own process group. Waits 1.5 seconds and reports it if it fails at once. |
| `list` | Shows processes with state, pid, command and folder. |
| `output` | Shows the last `lines` (default 100, max 2000), optionally only lines containing `filter` (searched in the last 1 MB of output). |
| `write` | Sends `input` to its stdin; `end` closes stdin. |
| `stop` | Stops it (SIGTERM, then SIGKILL after 2 seconds). |
| `watch` | Sets what the model is told about: when it exits (`on_exit`: `wake`, `context` or `ignore`), when an output line matches, or when it prints nothing for `stall_seconds` (at least 5). `start` takes these too. |
| `clear` | Forgets finished processes and deletes their logs. |

A watch is `{pattern, regex?, on, repeat?}`: `pattern` is plain text unless `regex` is true, and `on` is `wake`, `context` or `ignore`. An invalid regex is an error: `start` then starts nothing, and `watch` changes nothing. Setting `watches` replaces the earlier ones.

The model is told when a process exits or a watched line appears, so it does not need to poll. Output logs are kept in `~/.novacode/processes/`. Each stream's log rotates at 8 MiB, keeping one older file (`.1`); output older than that is dropped, and `output` marks the gap with `[older output discarded]`. Every process a session started is stopped when the session ends. If novacode crashes, the next start cleans up processes it left behind.

`bash` runs through the same manager. When a command hits its timeout, or you press **Ctrl+Alt+B** while it runs, the call returns at once and the command keeps running with an id. Use `/ps` in the TUI to see processes and their latest output.

## codemode

`codemode` lets the model write a short script instead of making many separate calls. The script body is JavaScript or TypeScript run in a sandboxed interpreter, and top-level `await` and `return` work.

```js
const files = await tools.find({ pattern: "**/*.toml" });
const heads = await Promise.all(
  files.split("\n").filter(Boolean).slice(0, 5).map(p => tools.read({ path: p, limit: 5 }))
);
return heads.join("\n---\n");
```

What a script has:

- `await tools.<name>(args)` calls a tool. It returns the tool's text and throws on error.
- `await mcp.<server>.<tool>(args)` calls an MCP tool (`mcp.server["tool-name"]` for dashed names).
- `searchTools(query)`, `describeTool(name)` and `describeNamespace(server)` show what exists and the argument types.
- `store(key, value)` and `load(key)` keep JSON values between scripts in the same session. They are lost when novacode restarts.
- `models.getAvailableOfType("classifier")` and `models.classify(model, {...})` for classifier models.

Only `text(v)`, `console.log(...)`, `image(dataUrl)` and the `return` value reach the model. Big intermediate results stay in the script.

Set `transaction: true` and file changes (`write`, `edit`) the script made are undone if it throws. `bash` and MCP effects cannot be undone.

Limits per script: 300 seconds, 500 tool calls, 64 MB of memory, 16 MB of output. Each nested call goes through your permission rules and hooks, and its output through the usual caps.

## User shell commands

In the TUI, start a message with `!` to run a shell command yourself:

```text
!git status
!!npm test
```

`!cmd` shows the output and also hands it to the model at its next call, without starting a turn. `!!cmd` shows it only to you. The command runs with `bash -c` in the session's folder, has no stdin, and is killed after 120 seconds. Output sent to the model is the last 30,000 characters.

These commands are yours, not the agent's: they do **not** go through the permission rules or the risk gate.

## See also

- [Security](security.md)
- [Context](context.md)
- [Subagents](subagents.md)
- [MCP](mcp.md)
- [Extensions](extensions.md)
- [TUI](tui.md)
- [Config](config.md)
