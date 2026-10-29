# Extensions

novacode runs extensions in three forms. When the model calls a tool an extension adds, that call goes through the same rules, risk gate and approvals as a built-in tool. An extension's own actions (`exec`, `fs`, host calls) do not: an extension is code you installed, and it runs with your permissions. `exec` refuses only catastrophic commands, and `fs` can read and write any file you can. Install only extensions you trust. Extensions from a project folder load only when you [trust](security.md) the folder.

| Form | Where | Good for |
|---|---|---|
| **Script** (TS or JS, QuickJS) | `~/.novacode/extensions/<name>.ts` or `<name>/index.ts`; project `.novacode/extensions/` (trusted folders only) | Tools, commands, hooks and UI, with no build step |
| **Process** (any language, JSON-RPC on stdio) | settings `processExtensions.<name>: {command, args, env}` | Bigger plugins, npm packages, or code in Python/Go/Rust ([details](#process-extensions)) |
| **Command hooks** | settings `hooks` | One shell command per event (see [automation](automation.md)) |

To share extensions with skills, prompts, agents and themes, put them in a [package](packages.md). A package's `extensions/` folder holds script extensions only; process extensions are always turned on in settings. Skills and templates are in [skills](skills.md), subagent files in [subagents](subagents.md).

`/reload` loads changes to any extension. An extension that hangs, runs out of memory, exits, or fails 3 times in a row is disabled with a notice until the next `/reload`. The session carries on without it.

## A script extension

```ts
// ~/.novacode/extensions/hello.ts
export default (nc) => {
  nc.registerTool({
    name: "hello",
    description: "Greet someone.",
    parameters: { type: "object", properties: { who: { type: "string" } }, required: ["who"] },
    execute: async (args) => `hello ${args.who}`,
  });
  nc.registerCommand("/hi", { description: "say hi", handler: async (args) => "hi!" });
  nc.on("before_tool", (e) => (e.tool === "write" && e.args.path.endsWith(".env") ? { block: "no .env edits" } : undefined));
  nc.ui.setStatus("hello", "👋 ready");
};
```

The default export gets the frozen `novacode` object (also `globalThis.novacode`) and may be `async`. Types are stripped, so TypeScript works as is.

**Files.** An extension is `<name>.ts`, `<name>.js` or `<name>.mjs`, or a folder `<name>/` with `index.ts` or `index.js`. The name is the file or folder name. When two folders have an extension with the same name, the first one wins: the project's `.novacode/extensions/`, then `~/.novacode/extensions/`, then packages.

**Imports.** An extension can import its own files with relative paths (`import { x } from "./lib/util"`). Imports resolve inside the folder that holds the entry file and can't reach outside it. Bare package names (`import "lodash"`) are refused: npm packages expect Node, which QuickJS doesn't have. Write a process extension for those.

**Limits.** Setup (running the default export) has 5 s of JS time and 10 s overall. Each extension has 64 MiB of memory. An extension runs one handler at a time, so a slow tool or an open dialog makes its other handlers wait.

**Time limits for handlers.** Tools and commands have 300 s. Event handlers have 10 s. Running past the limit disables the extension (see above).

## API v1

`novacode.apiVersion` is `1`.

**Stability promise.** Within v1, changes are additive only: nothing listed here is renamed or removed, and payloads only gain fields. A breaking change becomes API v2, and v1 keeps working for at least one release after that, with a deprecation notice. The whole surface (this object, hook payloads, UI ops, the process protocol) is snapshot-tested in `crates/novacode-core/tests/api-v1.snap`, so an accidental change fails `cargo test`.

### Registering

- **`registerTool(t)`**: `{name, description, parameters (JSON Schema), exposure?, readOnly?, execute(args, {cwd, session})}`.
  - `readOnly: true` says the tool only reads, so it runs in read-only mode and plan mode. Without it, the tool asks the user there (or is refused when nobody can answer).
  - `execute` returns a string, or `{content: string, isError?, details?}`. `undefined` gives `(no output)`; any other value is shown as pretty-printed JSON. If it throws, the model gets `<name> failed: <message>`.
  - `details.ui` (a list of [lines](#ui-novacodeui-version-1)) replaces the tool output in the transcript.
  - `exposure` is `listed` (the default: the model sees it), `codemode` (or `codemode-only`: only codemode scripts can call it), `deferred` (found through `tool_search`) or `hidden`. An unknown value counts as `listed`. The setting `tools.<name>.exposure` overrides it.
- **`registerCommand(name, {description, handler(args)})`**: a `/name` command (a leading `/` in `name` is optional). `args` is the text after the name, trimmed. A returned string is shown as a notice.
- If two extensions register the same command, the first one loaded wins. Process extensions load first, then script extensions in the folder order above. Built-in commands always win.
- **`registerShortcut(key, handler | {handler, description})`**: a key such as `ctrl+alt+d`. It works when no built-in key takes it. Alt+/ help does not list it.
- **`on(event, handler)`**: see Events below.

### Acting

- **`exec(command, {timeout?})`**: runs `bash -c` in the session folder with no stdin and resolves to `{code, stdout, stderr}`. `code` is `null` when the command was killed. A non-zero exit doesn't throw. Catastrophic commands throw `refused: <reason>`; nothing else is checked or asked, and the command runs outside the sandbox. The timeout is in seconds (default 60, at most 600); running past it throws `timed out after <n>s`.
- **`fs.read(path)`** / **`fs.write(path, text)`**: a path is relative to the session folder, absolute, or starts with `~/` (your home folder). `write` creates missing parent folders.
- **`session.messages()`**: the conversation as of the last finished run, without the system prompt. Each message is `{role: user|assistant|tool_result|custom, …}` as stored in the session file (`custom` holds saved reminders).
- **`complete({messages, system?, maxTokens?})`**: one model call on the session's provider, model and login. `messages` is a list of `{role: user|assistant, content: text}`. No tools are sent, and nothing is stored in the session. Resolves to `{text, usage, model}`, where `usage` is `{input, output, cache_read, cache_write, reasoning?}` in tokens. It counts against your subscription or key like any call, so don't call it in a loop.
- **`spawnAgent(task, opts?)`**: a subagent through the `agent` tool, with the same depth limit, file claims and approvals. `opts` takes the tool's spawn fields (`title`, `agent`, `model`, `cwd`, `after`, `notify`, `worktree`, `tools`, `mcp`). The Subagents panel shows `[ext:<name>]` before its title. Resolves to `{id}`; the report arrives the way `notify` says (default: it wakes the model).
- **`notify(text)`**: a notice in every window. The model doesn't see it.
- **`remind(text, {wake?})`**: a note for the model, saved and sent with the next model call. `wake` starts a run when the session is idle. Use it for anything the model must remember.
- **`sendMessage(text, {deliverAs?})`**: a real user message, saved like a typed one. `deliverAs: "steer"` puts it into the running turn; `"followUp"` (the default) sends it after the turn, or at once when the session is idle.
- **`settings.get(path?)`**: the merged settings at a dotted `path` (all of them when left out), or `null` when the path isn't set. Keep your own under `extensions.<your name>`.
- **`state.append(kind, data)`** and **`state.entries(kind)`**: your extension's data, kept in the session file and never sent to the model. `entries` returns what you appended under `kind` on the current branch, oldest first, so state follows `/rewind` and `/tree`.
- **`log(...args)`**: writes one line to novacode's log, `~/.novacode/logs/novacode.log.<date>`, tagged `extension=<name>`. `console.log`, `info`, `warn`, `error` and `debug` do the same.

Every call above returns a promise (except `log`). A failed call rejects with an `Error` whose message starts with the host operation's name, then the reason: `exec: …`, `readFile: …` for `fs.read`, `writeFile: …`, `messages: …`, `settings: …`, `stateAppend: …`, `ui: …`. `remind`, `sendMessage`, `state.*`, `messages`, `complete` and `spawnAgent` need a session, so they fail during setup: call them from a handler. `notify` during setup is only written to the log.

### UI (`novacode.ui`, version 1)

Every call sends data; novacode draws it in every window, including attached ones and the tmux panel.

- **`setStatus(key, text | segments, {style?})`**: a status-line segment showing the text. Its id is `ext:<key>`, for placing it in `ui.statusline` rows. `null` text removes it.
- **`setWidget(key, lines, {below?})`**: lines above the editor, or below it with `below`.
- **`setHeader(lines)`** / **`setFooter(lines)`**: lines above the transcript, or in place of the status line.
- **`setEditor({title?, border?})`**: a title and border colour for the editor box.
- **`openTab(name, {title, priority, width, region, show})`**, **`showTab(name)`**, **`closeTab(name)`**: a side-panel tab.
- **`setSource(tab, id, {title, order, newestFirst})`**, **`publish(tab, source, node)`**, **`unpublish(tab, source, id?)`**: tree rows in a tab. `unpublish` without an id clears the source. A node is `{id, parent?, label, tag?, status?, icon?, dim?, detail?, time?, order?, actions?}`, where `actions` maps a key to a `/command`.
- **`setRegion(tab, lines)`**: a region tab drawn with your own lines. Keys come back as `ui_key` events with `{tab, node, key, width, height}`.
- **`ask(questions)`**: dialogs in the `ask_user` form. `questions` is a list of `{question, header?, options: [{label, description?, preview?, recommended?}], multi?}`. It resolves to one answer per question, `{question, chosen: [labels], text?}` (`text` is what the user typed instead of picking), or `null` when dismissed or when the session has no window attached. In print mode (`-p`) the call rejects with `ask: this session has no window to ask in`.
- **`select(question, options)`**, **`confirm(question)`**, **`input(question)`**: short forms. `select` resolves to the chosen label (options may be strings), `confirm` to `true` or `false`, `input` to the typed text. Each resolves to `undefined` when dismissed or with no window attached; in print mode they reject like `ask`.

Styles are theme token names (`accent`, `dim`, `error`, …) or `#rrggbb`. A line is a string or a list of `{text, style?}` segments.

### Events

| Event | Payload | Return |
|---|---|---|
| `before_tool` | `{tool, args, cwd}`. MCP tools are named `<server>__<tool>`. It also runs for calls made inside codemode | `{block: reason}` refuses the call; the model sees `Blocked by a hook: <reason> (<extension>)`. `{args}` (an object) rewrites the arguments; the next handler sees the rewrite. The first block wins. |
| `after_tool` | `{tool, args, output, isError, callId, durationMs, touched}`. `output` is cut to 16 000 characters | A string is appended to the result as feedback, shown as `[hook] <text>` and cut to 8 000 characters. `{append?, replace?, isError?}` also replaces the result text or sets its error flag. `touched` lists the absolute paths the call changed or read: the `path` of edit/write/read, the files of an `apply_patch`, and the files a `bash` command changed (seen from git status before and after; outside a git repo, none). |
| `input` | `{text, source}`. `source` is `prompt` for a new prompt, `steer` for a message typed while a run is going | `{text}` rewrites the prompt before it is saved, `{handled: true}` swallows it. Handlers run in load order and see earlier rewrites. |
| `before_request` | `{provider, model, cwd, contextTokens, lastUser}` | A string, a list of strings or `{append: string or list}`: unsaved text sent after the cached prefix, for current state such as the time or git status. |
| `ui_key` | `{tab, node, key, width, height}`. `tab` is `null` for a key from `registerShortcut` | Ignored. |
| Session events: `agent_start`, `turn_start`, `message`, `tool_start`, `tool_update`, `tool_end`, `retry`, `agent_end`, `settled`, `cwd_changed`, `session_named`, `process_started`, `process_ended`, `compacted`, `prompt_taken_back`, `ui` | The event as on the [wire](automation.md), with snake_case fields (`call_id`, `is_error`, `duration_ms`) | Ignored: observe only. |

With several `after_tool` handlers, each sees the original result; the last `replace` or `isError` wins. Handlers fail open: an error leaves things as they were. Running past the 10 s limit also disables the extension.

## Process extensions

A process extension is a separate program, in any language, that novacode starts and talks to over stdin and stdout. Each message is one line of JSON-RPC 2.0. This is the way to write an extension in **Rust**, Go, Python, or with npm packages.

**Why Rust extensions are separate programs.** novacode does not load native plugins (a Rust `cdylib` opened with `dlopen`) into its own process, and it has no WASM plugin host. Rust has no stable ABI, so a plugin built with another compiler version can break in odd ways. A panic or memory bug in a plugin would also take the whole session down with it, and every plugin author would need the same toolchain. A separate process avoids all three: if it crashes, novacode disables it and carries on. The cost is one JSON round trip per call, which is small next to a model call.

### Turning one on

```json
{ "processExtensions": { "lsp": { "command": "novacode-lsp", "args": [], "env": {} } },
  "extensions": { "lsp": { "waitMs": 3000 } } }
```

- `command` is a program on `PATH` or an absolute path. `args` and `env` are optional.
- The key (`lsp` here) is the extension's name. Its own settings go under `extensions.<name>` and arrive in `initialize`.
- A project's `.novacode/settings.json` can declare process extensions too, but only in a [trusted](security.md) folder.
- novacode starts every process extension when the session starts, in the session folder, in its own process group. `/reload` starts them again.
- The environment is novacode's own plus your `env`, with `NOVACODE_HOOKS_DISABLED=1` set so a novacode you start from it doesn't run command hooks or plugin hooks again (it still loads extensions).
- stderr goes to `~/.novacode/logs/ext-<name>.log`, which is emptied at each start. Print debug output there, never on stdout: stdout carries only protocol lines, and a line that isn't JSON is ignored.

### Messages from novacode

Every request has a numeric `id`. Answer it with `{"jsonrpc": "2.0", "id": <same id>, "result": …}`, or `"error": {"code": …, "message": "…"}`.

| Method | `params` | Your `result` | Time limit |
|---|---|---|---|
| `initialize` | `{apiVersion, cwd, session, home, settings}`. `home` is the user's home folder, `settings` your `extensions.<name>` block (or `null`) | `{tools?, commands?, events?, timeoutMs?}`, see below | 10 s |
| `tool` | `{name, args, cwd, session}` | A string; or `{content: string, isError?, details?}`; `null` shows `(no output)`; any other JSON is shown pretty-printed | 300 s |
| `command` | `{name, args}`. `name` has no `/`, `args` is the text after the name (may be `""`) | A string shown as a notice; `null` shows nothing | 300 s |
| `event` | `{event, payload}`. The payloads are in the [Events](#events) table | What that table says for the event; `null` changes nothing | 10 s, or your `timeoutMs` |
| `shutdown` | `{}` (a notification, no `id`) | Exit. After 2 s the process group is killed | |

The `initialize` result declares everything you handle:

```json
{
  "tools": [{ "name": "shout", "description": "…", "parameters": { "type": "object" }, "exposure": "listed" }],
  "commands": [{ "name": "/hello", "description": "…" }],
  "events": ["before_tool", "after_tool"],
  "timeoutMs": 20000
}
```

- `parameters` is a JSON Schema for the tool's arguments. `readOnly: true` lets the tool run in read-only mode, as for script tools. `exposure` is `listed` (the default: the model sees it), `codemode` (only codemode scripts can call it), `deferred` (found through `tool_search`) or `hidden`.
- `events` lists the event names you want, from the [Events](#events) table. You get no other events.
- `timeoutMs` (1 to 60 000) replaces the 10 s limit for your event handlers, for slow work like waiting on a language server. It doesn't change the tool and command limit.

**Requests overlap.** The model can run several tools at once, so a new request can arrive before you answered the last one. Answer each by its `id`, in any order. A slow handler should run in its own thread or task, as both examples below do.

**Failures.** An `error` answer counts as an error, whatever its message says. Three errors in a row, a missed time limit, or the process exiting disables the extension with a notice until the next `/reload`; the session carries on. A tool call the user cancels stops waiting for you, but you aren't told, and a late answer is dropped. If the program can't start or `initialize` fails, novacode shows a notice and runs without it.

### Messages you send

**Notifications** (no `id`, no answer):

| Method | `params` | Effect |
|---|---|---|
| `notify` | `{text}` | A notice in every window, shown as `[<name>] text`. The model doesn't see it. |
| `remind` | `{text, wake?}` | Like `remind()` above: a note saved for the model's next call; `wake: true` starts a run when idle. |
| `ui` | `{op, …}` | A UI change, as in the `novacode.ui` calls above. The ops are listed below. A malformed op is only logged as a warning. |
| `log` | `{text}` | A line in novacode's own log, `~/.novacode/logs/novacode.log.<date>`. |

The `ui` ops use snake_case field names. `lines` is a list of [lines](#ui-novacodeui-version-1); for `widget`, `header` and `footer`, `null` clears them. `region` needs a list.

| `op` | Fields | Script call |
|---|---|---|
| `status` | `key, text, style?` (`text` is a string; `null` removes the segment) | `setStatus` |
| `widget` | `key, lines, below?` | `setWidget` |
| `header` / `footer` | `lines` | `setHeader` / `setFooter` |
| `editor` | `title?, border?` | `setEditor` |
| `open_tab` | `name, title?, priority?, width?, region?, show?` | `openTab` |
| `show_tab` / `close_tab` | `name` | `showTab` / `closeTab` |
| `source` | `tab, id, title?, order?, newest_first?` | `setSource` |
| `publish` | `tab, source, node` | `publish` |
| `unpublish` | `tab, source, id?` | `unpublish` |
| `region` | `tab, lines` | `setRegion` |
| `shortcut` | `key, description?` | `registerShortcut`. Ask for the `ui_key` event in `initialize` to receive the key; it arrives with `tab: null`. |

**Host requests** let you use anything a script extension can. Send `{"jsonrpc": "2.0", "id": <your id>, "method": "host", "params": {"op": …, "args": {…}}}`. Pick ids that can't be mistaken for novacode's numeric ids, such as strings. The answer comes back on stdin as a normal result or error with your id and no `method`, mixed in with novacode's requests.

| `op` | `args` | Same as |
|---|---|---|
| `exec` | `{command, timeout?}` | `exec()` |
| `readFile` / `writeFile` | `{path}` / `{path, text}` | `fs.read()` / `fs.write()` |
| `messages` | `{}` | `session.messages()` |
| `complete` | `{messages, system?, maxTokens?}` | `complete()` |
| `spawnAgent` | `{task, opts}` | `spawnAgent()` |
| `ask` | `{questions}` | `ui.ask()`; `null` when dismissed or with no window attached (an error in print mode) |
| `settings` | `{path?}` | `settings.get()` |
| `stateAppend` / `stateEntries` | `{kind, data}` / `{kind}` | `state.append()` / `state.entries()` |
| `sendMessage` | `{text, deliverAs?}` | `sendMessage()` |

The notifications `notify`, `remind`, `log` and `ui` also work as `host` ops. An unknown `op` gets an error. Like a script's calls, these run with your permissions: `exec` refuses only catastrophic commands, runs outside the sandbox, and nothing asks for approval. Ops that need a session (`messages`, `complete`, `spawnAgent`, `stateAppend`, `sendMessage`, …) fail while you are still answering `initialize`.

### Writing one in Rust

The smallest useful extension has one tool (`shout`), one command (`/hello`) and one hook that refuses `git push`. It answers requests one at a time, which is fine while every handler is quick.

`Cargo.toml`:

```toml
[package]
name = "novacode-hello"
version = "0.1.0"
edition = "2024"

[dependencies]
serde_json = "1"
```

`src/main.rs`:

```rust
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn main() {
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if msg["method"] == "shutdown" {
            break;
        }
        // Only requests carry an id; skip notifications.
        let Some(id) = msg.get("id").cloned() else { continue };
        let p = &msg["params"];
        let result = match msg["method"].as_str() {
            Some("initialize") => json!({
                "tools": [{
                    "name": "shout",
                    "description": "Return the text in upper case.",
                    "parameters": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }],
                "commands": [{ "name": "/hello", "description": "Say hello" }],
                "events": ["before_tool"]
            }),
            Some("tool") if p["name"] == "shout" => {
                json!(p["args"]["text"].as_str().unwrap_or_default().to_uppercase())
            }
            Some("command") if p["name"] == "hello" => {
                json!(format!("hello {}", p["args"].as_str().unwrap_or_default()))
            }
            Some("event") if p["event"] == "before_tool" => {
                let cmd = p["payload"]["args"]["command"].as_str().unwrap_or_default();
                if p["payload"]["tool"] == "bash" && cmd.contains("git push") {
                    json!({ "block": "no pushing while the hello extension is on" })
                } else {
                    Value::Null
                }
            }
            _ => Value::Null,
        };
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}
```

Build it, then try it by hand before novacode runs it. Each line you pipe in is one request:

```sh
cargo install --path .
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tool","params":{"name":"shout","args":{"text":"hi"}}}' \
  | novacode-hello
# {"id":1,"jsonrpc":"2.0","result":{"commands":[…],"events":["before_tool"],"tools":[…]}}
# {"id":2,"jsonrpc":"2.0","result":"HI"}
```

Add `"processExtensions": {"hello": {"command": "novacode-hello"}}` to `~/.novacode/settings.json` and run `/reload`. In the TUI, `/hello nova` now shows `hello nova`. The model can call `shout`, and a `git push` it tries is refused with `Blocked by a hook: no pushing while the hello extension is on (hello)`.

When handlers get slow (network calls, language servers) or you need host requests, answer each request from its own task. The two bundled extensions show how: they read stdin in a loop, `tokio::spawn` a task per request, and send every reply through one channel to a single stdout writer. Start from [`extensions/image-gen/src/main.rs`](../extensions/image-gen/src/main.rs) (about 70 lines) for a tool, or [`extensions/lsp/src/main.rs`](../extensions/lsp/src/main.rs) for an `after_tool` hook with `timeoutMs`. Both are their own crates with their own `[workspace]`, outside novacode's workspace. image-gen also depends on `crates/novacode-ai` by path, so build it from a novacode checkout. Both use `NOVACODE_HOME` if set, else `~/.novacode`: image-gen for its login, lsp for its logs.

**Example: image generation.** [`extensions/image-gen`](../extensions/image-gen) adds a `generate_image` tool (Gemini image models through `novacode login antigravity`, or a key in `extensions.image-gen.apiKey`, `GEMINI_API_KEY` or `GOOGLE_API_KEY`). Install with `cargo install --locked --path extensions/image-gen` and add `"processExtensions": {"image-gen": {"command": "novacode-image-gen"}}`.

**Example: language-server diagnostics.** [`extensions/lsp`](../extensions/lsp) is a process extension that reports the errors an edit (or a `bash` command) left in the touched files. It isn't part of novacode: install it with `cargo install --locked --path extensions/lsp`, then add the `processExtensions.lsp` entry above.

## Custom providers

A provider that novacode doesn't know can be any program that speaks line-delimited JSON-RPC 2.0 on stdio. Add it to settings:

```jsonc
"providers": {
  "mine": { "api": "process", "command": "python3", "args": ["/path/to/provider.py"], "env": {} }
}
```

Then pick `mine/<model>` like any other model. The program is started on the first call and serves the whole session. It needs no key; it handles its own credentials.

To set variables for the program, add `"env": {"MY_VAR": "1"}`.

| Direction | Message | Shape |
|---|---|---|
| novacode → program | `initialize` (request, 10 s limit) | `{apiVersion, provider}` → `{models?: [id]}` (the list shown for `/model`) |
| novacode → program | `stream` (request, no time limit) | `{model, messages, tools, maxTokens, thinking, session}` → `{stop: stop\|tool_use\|length}` (any other value counts as `stop`), or a JSON-RPC error `{message, data?: {kind}}` |
| program → novacode | `event` (notification) | `{id, event}`, where `id` is the numeric id of the `stream` request and `event` is one of `{type: "text", delta}`, `{type: "thinking", delta}`, `{type: "tool_call", id, name, arguments}` (`arguments` an object or a JSON string), `{type: "usage", input, output, cacheRead?, cacheWrite?}`. Unknown types are ignored |
| novacode → program | `cancel` (notification) | `{id}`: the user stopped the call |

- `messages` is novacode's own message JSON, the same as `session.messages()`. `tools` is a list of `{name, description, schema}`. `thinking` is `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` or `null`. `session` is the session id.
- The error `kind` decides what novacode does next: `overflow` (context too long: compact and retry), `rate_limit` and `transient` (retry later), `quota_exhausted`, `auth`, `invalid_request`, `aborted`, or `other` (also used for an unknown kind). Those last ones end the call and show the error.
- If the program exits during a call, the call fails with `the provider program exited`.
- stderr goes to novacode's stderr. Project settings only apply in trusted folders, so a repo can't start a provider program on its own.

A working provider in Python is the test at the bottom of [`crates/novacode-ai/src/process.rs`](../crates/novacode-ai/src/process.rs).
