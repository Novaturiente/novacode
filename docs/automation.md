# Automation

This page covers running novacode without a person at the keyboard: print mode for scripts and CI, a JSON event stream, the `--rpc` protocol for building your own front end, background sessions, and hooks that run your commands on events.

## Print mode: `-p`

`-p` sends one prompt, prints the final answer to stdout and exits.

```sh
novacode -p "explain what src/main.rs does"
novacode -m claude/claude-opus-5-5 -p "fix the failing test"
```

If stdin is not a terminal, what you pipe in is added after the prompt:

```sh
git diff | novacode -p "review this diff"
cat error.log | novacode -p "what went wrong?"
```

Only the answer goes to stdout, so you can pipe it. Notices and retries go to stderr. Add `-v` to also print each tool call, the token and cache usage and the session file path to stderr.

An empty prompt with nothing piped fails with `-p needs a prompt`. Print mode needs a model: pass `-m provider/model` or set `model` in settings. Without one it stops with `no model: pass -m provider/model or set "model" in settings`. `--bg` and `--rpc` do the same. Only interactive runs pick one for you.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | The run finished (stopped normally, hit the length limit, or stopped to use tools) |
| `1` | The model request failed, or novacode could not start (a bad flag, no model, no login) |
| `130` | The run was aborted (for example Ctrl+C) |

```sh
if novacode -p "does the build pass? answer yes or no" > answer.txt; then
  cat answer.txt
else
  echo "novacode failed" >&2
fi
```

### Nobody to approve

In print mode there is no one to answer a permission question. Anything that would ask is refused, and the model is told to use an allow rule or run interactively. Questions to the user (`ask_user`) get no answer either. Plan for it:

- Add allow rules for the commands you want in your global settings (see [security](security.md)). Permission rules are read from your global settings only.
- Or limit what the run can do: `--mode read-only` for a review that must not change files, or `--profile scout` for read-only search tools.

```sh
novacode --mode read-only -p "list the TODOs in this repo"
```

### Useful flags for scripts

| Flag | What it does |
|---|---|
| `-m provider/model[:level]` | Choose the model, e.g. `codex/gpt-5.5:high` |
| `--api-key KEY` | API key for the model's provider, this run only. Needs a model: `-m provider/model` or `model` in settings. It comes ahead of saved and environment keys. |
| `--trace` | Record the raw request and stream (secrets removed) to `~/.novacode/logs/trace-<id>.jsonl`. See [troubleshooting](troubleshooting.md). |
| `--no-session` | Don't save this run as a session |
| `-w`, `--worktree [NAME]` | Start in git worktree `.worktrees/NAME` (made if missing) |
| `-c` / `-r ID` | Continue the newest session in this folder / resume one by id prefix |
| `--mode auto\|ask\|read-only` | Permission mode for this run |
| `--profile NAME` | Restrict tools to a named profile, e.g. `scout` |
| `--trust` | Trust this folder's project config for this run only |
| `--no-context-files` | Don't load `AGENTS.md` / `CLAUDE.md` |
| `-v`, `--verbose` | Tool calls, stop reason and token usage on stderr |

Project config (project settings, extensions, hooks, project MCP servers) loads only in [trusted](security.md) folders. A script never gets the trust question, so in CI either run `novacode trust` once on the checkout or pass `--trust` when you want the repo's config to apply. Without it, novacode prints a note to stderr and ignores project config.

`novacode "prompt"` without `-p` starts the interactive window with that first prompt.

Your `AGENTS.md` and `CLAUDE.md` files still load. `--api-key` is hidden from crash logs.

## In CI

```sh
# GitHub Actions style
export ANTHROPIC_API_KEY=...     # or pass --api-key
novacode --no-session --mode read-only \
  -m anthropic/claude-opus-5-5 \
  -p "review the changes in this PR for bugs" < pr.diff > review.md
```

The environment variable depends on the provider. If a key is missing, the error names the variable to set. See [providers](providers.md). On a headless box you can also store a key once by piping it in: `echo "$KEY" | novacode login openrouter`.

## JSON event stream: `--json`

With `-p`, `--json` writes the protocol's events to stdout as JSON lines instead of the answer. Each line is one JSON object with a `type`.

```sh
novacode -p "what is 2+2?" --json
```

The first line is `hello`, then one `event` line for each thing the session does:

```json
{"type":"hello","protocol":1,"version":"0.1.0","capabilities":["events"],"state":{"session":"01abc…","cwd":"/work","model":"claude/claude-opus-5-5","level":"medium","window_tokens":200000,"busy":false,…}}
{"type":"event","event":{"type":"agent_start"}}
{"type":"event","event":{"type":"stream","stream":"delta","index":0,"text":"4"}}
{"type":"event","event":{"type":"agent_end","stop":{"type":"stop"}}}
{"type":"event","event":{"type":"settled"}}
```

(The lines are shortened here. A real run has more events, such as `turn_start`, `message`, `tool_start` and `tool_end`.) The exit codes are the same as for plain `-p`.

Pick out what you need with `jq`:

```sh
novacode -p "list the files here" --json | jq -r 'select(.type=="event" and .event.type=="tool_start") | .event.call.name'
```

## The `--rpc` protocol

`--rpc` runs a session with no window and speaks the same JSON lines on stdin and stdout. One command per line in, messages out. Use it to put novacode behind your own editor plugin, web page or script. It can't be combined with `-p`.

```sh
novacode --rpc -m claude/claude-opus-5-5
```

The protocol version is `1` (the `protocol` field in `hello`). A client should read it first and refuse a version it doesn't know. Closing stdin ends the session.

### Messages from novacode

Every message has a `type`.

| `type` | What it is |
|---|---|
| `hello` | Always first: `protocol`, `version`, `capabilities`, and the session `state` |
| `history` | The session's messages so far (sent right after `hello`) |
| `event` | A loop event in `event`: `agent_start`, `turn_start`, `stream`, `message`, `tool_start`, `tool_update`, `tool_end`, `retry`, `agent_end`, `settled`, `compacted`, `prompt_taken_back` (Esc before the first reply took the prompt off the branch; `text` holds it), … Streamed text arrives as `stream` events with `"stream":"delta"` |
| `response` | The answer to a command that had an `id`: `command`, `ok`, `data`, `error` |
| `ui` | A notice, page or list for the user, tagged by `ui.kind` |
| `approve` | A permission question (see below) |
| `approval_done` | Approval `id` was answered |
| `ask` | Questions for the user from the model |
| `ask_done` | Questions `id` were answered or dismissed |
| `state` | The session's state changed (model, thinking level, busy, context use, …) |
| `tool_call` | The model called a tool that you registered |
| `bye` | The session is closing |

### Commands to novacode

Send one JSON object per line. Add an `id` (any string) to a command to get a `response` with the same `id`. Without an `id` you only get a response when the command fails.

| Command | Fields | What it does |
|---|---|---|
| `prompt` | `text` | Start a run, or queue it as a follow-up if one is running. A `!cmd` line runs a shell command. A built-in `/command` runs that command instead (see [Slash commands](#slash-commands-over-rpc)). Response data: `{"disposition": "started" \| "queued" \| "handled"}` |
| `steer` | `text` | Send a message to the running turn before its next model call |
| `abort` | | Stop the running turn |
| `set_model` | `model` | `provider/model[:level]` |
| `set_thinking` | `level` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max` |
| `compact` | | Compact the context |
| `get_context` | | Response data: the `/context` report as `{"text": …}` |
| `list_models` | | Answered with a `ui` message of models |
| `export` | `html` (optional) | Write the session to a file. Response data: `{"path": …}` |
| `get_state` | | Response data: the session state, the same record as `hello.state` (also `get_status`). See [State](#the-state-record). |
| `get_messages` | | Response data: `{"messages": […]}` |
| `get_commands` | | Response data: `{"commands": [{"name", "description"}]}` |
| `approval` | `approval`, `answer` | Answer a permission question |
| `answer` | `ask`, `replies` | Answer questions from the model |
| `ext_key` | `ext`, `key`, `tab?`, `node?`, `width?`, `height?` | Send a key or panel action to a script [extension](extensions.md). No data comes back. |
| `register_tools` | `tools` | Tools your client runs (see below) |
| `tool_result` | `call`, `content`, `is_error` | The result of a `tool_call` |
| `shutdown` | | End the session |

A line that isn't a valid command gets `{"type":"response","command":"parse","ok":false,"error":"bad command: …"}`.

### Slash commands over RPC

`prompt` text starting with `/` runs the command in the session instead of going to the model. These work: `/compact`, `/context`, `/mcp`, `/name <text>`, `/worktree [name|exit|remove]`, `/btw <q>`, `/docs`, `/export [html]`, `/model <id>`, `/thinking <level>`, `/plan`, `/mode`, `/goal`, `/usage`, `/debug`, plus skills, prompt templates and extension commands. `get_commands` lists them.

These need the session's own window and answer with an error such as `/resume works only in the session's own window`: `/resume`, `/new`, `/tree`, `/rewind`, `/fork`, `/clone`, `/reload`, `/restart`, `/login`, `/keys`, `/copy`, `/ps`, `/quit`. `/model` and `/thinking` without an argument (the pickers) fail the same way. The session-changing ones also don't work in a window opened with `novacode attach`.

### The state record

`hello.state`, `get_state` and `state` messages carry the same record:

| Field | Meaning |
|---|---|
| `session`, `cwd`, `model`, `level` | Session id, folder, `provider/model`, thinking level |
| `window_tokens`, `context_tokens` | Context window size and tokens in use |
| `busy` | A run is in progress |
| `activity` | `{state, detail}`: `working`, `blocked` or `idle`, and what it is doing |
| `spent`, `price?` | USD spent on API-key routes, and the route's prices |
| `tokens` | Token totals for the session |
| `cache` | `{last_read, last_write, hit_pct, last_miss?}` |
| `quota?` | Subscription usage windows, when the route has them |
| `file?` | Session file (absent with `--no-session`) |
| `branch?`, `project?` | Git branch and repo name |
| `started_ms`, `task_started_ms?` | Unix milliseconds: session start, running task start |
| `processes` | Commands running in the process manager |
| `mcp?` | `[ready, configured]` MCP servers |

Fields marked `?` are left out when they have no value.

### A short session

You type the lines marked `>`. The rest comes back. Lines are shortened with `…`.

```text
< {"type":"hello","protocol":1,"version":"0.1.0","capabilities":["events","history","approvals","ask_user","prompt","steer","models","thinking","compact","export"],"state":{…}}
< {"type":"history","messages":[]}
> {"id":"1","type":"prompt","text":"create hello.txt containing hi"}
< {"type":"response","id":"1","command":"prompt","ok":true,"data":{"disposition":"started"}}
< {"type":"event","event":{"type":"agent_start"}}
< {"type":"event","event":{"type":"stream","stream":"delta","index":0,"text":"I'll create"}}
< {"type":"event","event":{"type":"tool_start","call":{"id":"…","name":"write","arguments":{"path":"hello.txt","content":"hi"}}}}
< {"type":"event","event":{"type":"tool_end","call_id":"…","is_error":false,"duration_ms":3}}
< {"type":"event","event":{"type":"agent_end","stop":{"type":"stop"}}}
< {"type":"event","event":{"type":"settled"}}
> {"id":"2","type":"get_state"}
< {"type":"response","id":"2","command":"get_state","ok":true,"data":{…}}
> {"type":"shutdown"}
< {"type":"bye","reason":"session ended"}
```

`settled` means the run is over and nothing is queued. Wait for it before you treat a run as finished.

### Answering approvals and questions

Unlike print mode, an RPC client can answer. When a tool needs permission, every connected client gets an `approve` message. The first answer wins; the others get `approval_done`.

```json
{"type":"approve","id":3,"tool":"bash","detail":"rm -rf build","reason":"deletes files","rule":"Bash(rm:*)","risky":true}
```

Answer with the same number in `approval`:

```json
{"type":"approval","approval":3,"answer":"once"}
{"type":"approval","approval":3,"answer":"session"}
{"type":"approval","approval":3,"answer":"always"}
{"type":"approval","approval":3,"answer":{"deny":"not now"}}
```

`once` allows this call, `session` allows the same rule until the session ends, `always` also saves the rule to your global settings, and `deny` refuses (the text is the reason the model sees). The session waits, with its state set to `blocked`, until someone answers.

When the model asks the user something (`ask`), reply with one entry per question, or `null` to dismiss:

```json
{"type":"ask","id":4,"questions":[{"question":"Which db?","header":"Database","options":[{"label":"SQLite","description":"zero setup","recommended":true}],"multi":false}]}
{"type":"answer","ask":4,"replies":[{"question":"Which db?","chosen":["SQLite"]}]}
{"type":"answer","ask":4,"replies":null}
```

Each reply is `{question, chosen: [labels], text?}`. `text` is what the user typed instead of, or as well as, picking.

### Tools that your client runs

Send `register_tools` with a list of `{name, description, parameters}` (parameters is a JSON Schema). The model sees them from the next run. When the model calls one, you get `{"type":"tool_call","id":7,"name":"…","args":{…}}`. Reply with `{"type":"tool_result","call":7,"content":"…","is_error":false}`.

## Background sessions: `--bg`

`--bg` starts a session with no window and gives you its id.

```sh
novacode --bg "run the full test suite and fix what breaks"
# started 01jabc…xyz in the background
# attach with: novacode attach 01jabc…
```

The id is printed to stdout; the other lines go to stderr. The session runs in its own process group, so closing your terminal doesn't stop it. `--bg` with no prompt starts an idle session you can attach to. Other flags such as `-m` and `--mode` apply to it too.

| Command | What it does |
|---|---|
| `novacode agents` | List live sessions: id, state (`working`, `blocked` when it waits for an answer, or `idle`), whether it's `bg`, model and folder |
| `novacode attach ID` | Open a window on a live session. Leaving only detaches; the session keeps running. An id prefix is enough. |
| `novacode status` | Status of this tmux pane's session, else this folder's newest one |
| `novacode status --session ID` | One live session, by id prefix |
| `novacode status --all` | Every live session |
| `novacode status --json` | The full record as JSON lines |
| `novacode status --format TEMPLATE` | A template, e.g. `'{state} {ctx}% {5h}'` |
| `novacode stop ID` | End a live session, a background one included |

`status` fields are: `session state detail model level ctx ctx_tokens window hit read write miss cost 5h week branch project cwd bg`. It exits with `1` when there is no live session, which makes it easy to use in scripts and in a tmux status line (`#(novacode status --format '{state}')`).

A background session that needs an approval waits in the `blocked` state until you attach and answer. Give it allow rules or `--mode read-only` if you want it to run on its own.

## Reacting to what the agent does

### `hooks.onActivity`

A shell command that runs each time the session changes between `working`, `blocked` and `idle`. It also runs in `--bg` and `--rpc` sessions. See [config](config.md).

```json
{ "hooks": { "onActivity": "notify-send \"novacode: $NOVACODE_STATE\"" } }
```

The command gets the full state as JSON on stdin, and these environment variables:

| Variable | Value |
|---|---|
| `NOVACODE_STATE` | `working`, `blocked` or `idle` |
| `NOVACODE_PREV_STATE` | The state before |
| `NOVACODE_DETAIL` | What it's doing, e.g. `thinking` or `tool:bash` |
| `NOVACODE_SESSION` | Session id |
| `NOVACODE_SESSION_FILE` | Session file path (empty with `--no-session`) |
| `NOVACODE_CWD` | Working folder |
| `NOVACODE_MODEL` | `provider/model` |

The command is killed after 10 seconds. Its output is discarded.

### tmux agent state

Inside tmux, a session in its own window sets these options on its pane, so your status line or scripts can read them (`tmux show -p -v @agent_state`):

| Option | Value |
|---|---|
| `@agent` | `novacode` |
| `@agent_state` | `working`, `blocked`, `idle`, or `done` (it finished while you weren't looking at the pane) |
| `@agent_cwd`, `@agent_model` | Folder and model |
| `@agent_session` | Session file |
| `@agent_project` | The repo name |
| `@agent_bg` | How many background processes are running (unset when none) |

These need `$TMUX_PANE`. Turn it off with `tmux.agentState: false`. When a turn ends or needs you while the pane is unwatched, novacode also sends a terminal notification (`notifications.enabled`). `--bg` and `--rpc` sessions have no pane, so they don't set these options; use `novacode status` or `hooks.onActivity` for them.

### herdr

If novacode runs inside herdr (a terminal workspace manager for agents), it reports its state there too. It does this when `HERDR_ENV=1` and `HERDR_SOCKET_PATH` and `HERDR_PANE_ID` are set. You don't configure anything.

### Command hooks

The `hooks` setting runs shell commands around tool calls and on session events. Each command runs with `sh -c`, gets JSON on stdin, and has `NOVACODE_HOOK_EVENT`, `NOVACODE_SESSION_ID`, `NOVACODE_CWD` and (for tool hooks) `NOVACODE_TOOL` set. `NOVACODE_HOOKS_DISABLED=1` is set too, see below.

```json
{
  "hooks": {
    "beforeTool": "jq -e '.tool == \"bash\" and (.args.command | test(\"rm -rf\"))' >/dev/null && { echo 'no rm -rf' >&2; exit 2; }; exit 0",
    "afterTool": ["./scripts/lint-changed.sh"],
    "events": {
      "agent_end": "./scripts/notify-done.sh",
      "session_end": ["./scripts/cleanup.sh"]
    },
    "timeoutMs": 5000
  }
}
```

| Hook | When | What your command can do |
|---|---|---|
| `beforeTool` | Before each tool call. Stdin: `{event, tool, args, session, cwd}` | Exit `2` to block the call; stderr is the reason the model sees. Print `{"args": {…}}` on stdout (exit `0`) to change the arguments. Any other exit code, or a timeout, lets the call through. |
| `afterTool` | After each tool call. Stdin adds `is_error`, `output`, `call_id`, `duration_ms`, `touched` | Whatever it prints on stdout is added to the tool result the model sees. |
| `events.<type>` | On each session event, such as `agent_end`, `turn_start`, `tool_end`, `compacted`, plus `session_start` and `session_end`. Stdin: the event JSON (events have a `type` field; `session_start` and `session_end` are `{event, session, cwd}`). `NOVACODE_TOOL` is not set | Observe only. Streaming text and live tool output (`stream`, `tool_update`) don't trigger hooks. |

Each of these takes one command or a list. `timeoutMs` (default `5000`) is the limit for `beforeTool`; `afterTool` gets twice that. Event hooks run in the background (up to 30 seconds each), and `session_end` is waited for up to 5 seconds. A novacode started inside a hook runs no command hooks or plugin hooks of its own (`NOVACODE_HOOKS_DISABLED` is set). It still loads extensions and runs `hooks.onActivity`.

Hooks from a project's settings run only when the folder is [trusted](security.md). If you moved from Claude Code, `novacode import claude` converts your hooks (see [migrating](migrating.md)).

## See also

- [config](config.md): `hooks`, `tmux.agentState`, `notifications.enabled` and every other setting
- [security](security.md): permission modes, rules and trust
- [sessions](sessions.md): `-c`, `-r`, `--no-session`, export
- [providers](providers.md): API keys and environment variables
- [troubleshooting](troubleshooting.md): `-v`, `--trace` and logs
