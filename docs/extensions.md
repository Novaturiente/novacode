# Extensions

novacode runs extensions in three forms. All of them reach files and commands through the same safety rules as the model (risk gate, trust).

| Form | Where | Good for |
|---|---|---|
| **Script** (TS or JS, QuickJS) | `~/.novacode/extensions/<name>.ts` or `<name>/index.ts`; project `.novacode/extensions/` (trusted folders only) | Tools, commands, hooks and UI, with no build step |
| **Process** (any language, JSON-RPC on stdio) | settings `processExtensions.<name>: {command, args, env}` | Bigger plugins, or code in Python/Go/Rust |
| **Command hooks** | settings `hooks` | One shell command per event (see `docs/config.md`) |

Packages (`novacode install <git-url|path>`) bundle `extensions/`, `skills/` and `prompts/` folders.

`/reload` loads changes. An extension that hangs, runs out of memory or throws 3 times in a row is disabled with a notice until the next `/reload`.

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

The default export gets the frozen `novacode` object (also `globalThis.novacode`). Types are stripped, so TypeScript works as is. Setup has 5 s of JS time. Each extension has its own memory limit.

## API v1

`novacode.apiVersion` is `1`.

**Stability promise.** Within v1, changes are additive only: nothing listed here is renamed or removed, and payloads only gain fields. A breaking change becomes API v2, and v1 keeps working for at least one release after that, with a deprecation notice. The whole surface (this object, hook payloads, UI ops, the process protocol) is snapshot-tested in `crates/novacode-core/tests/api-v1.snap`, so an accidental change fails the build.

### Registering

- **`registerTool(t)`**: `{name, description, parameters (JSON Schema), exposure?, execute(args, {cwd, session})}`. `execute` returns a string or `{content, isError?, details?}`. `details.ui` (styled lines) replaces the tool output in the transcript. `exposure` is listed (the default), codemode, deferred or hidden.
- **`registerCommand(name, {description, handler(args)})`**: a `/name` command. The returned string is shown as a notice.
- **`registerShortcut(key, handler | {handler, description})`**: a key such as `ctrl+alt+d`, shown in Alt+/ help.
- **`on(event, handler)`**: see Events below.

### Acting

- **`exec(command, {timeout?})`**: runs `bash -c` in the session folder and resolves to `{code, stdout, stderr}`. Catastrophic commands are refused. The timeout is in seconds (default 60, at most 600).
- **`fs.read(path)`** / **`fs.write(path, text)`**: paths are relative to the session folder, or start with `~/`.
- **`session.messages()`**: the conversation as of the last finished run, without the system prompt. Each message is `{role: user|assistant|tool_result, …}` as stored in the session file.
- **`complete({messages, system?, maxTokens?})`**: one model call on the session's provider, model and login. `messages` is a list of `{role: user|assistant, content: text}`. No tools are sent, and nothing is stored in the session. Resolves to `{text, usage, model}`. It counts against your subscription or key like any call, so don't call it in a loop.
- **`spawnAgent(task, opts?)`**: a subagent through the `agent` tool, with the same depth limit, file claims and approvals. `opts` takes the tool's spawn fields (`title`, `agent`, `model`, `cwd`, `after`, `notify`, `worktree`). The Subagents panel shows `[ext:<name>]` before its title. Resolves to `{id}`; the report arrives the way `notify` says (default: it wakes the model).
- **`notify(text)`**: a notice in every window. The model doesn't see it.
- **`remind(text, {wake?})`**: a note for the model, saved and sent with the next model call. `wake` starts a run when the session is idle. Use it for anything the model must remember.
- **`log(...args)`**: writes to `~/.novacode/logs/ext-<name>.log`. `console.*` goes there too.

### UI (`novacode.ui`, version 1)

Every call sends data; novacode draws it in every window, including attached ones and the tmux panel.

- **`setStatus(key, text | segments, {style?})`**: a status-line segment showing the text. Its id is `ext:<key>`, for placing it in `ui.statusline` rows. `null` text removes it.
- **`setWidget(key, lines, {below?})`**: lines above the editor, or below it with `below`.
- **`setHeader(lines)`** / **`setFooter(lines)`**: lines above the transcript, or in place of the status line.
- **`setEditor({title?, border?})`**: a title and border colour for the editor box.
- **`openTab(name, {title, priority, width, region, show})`**, **`showTab(name)`**, **`closeTab(name)`**: a side-panel tab.
- **`setSource(tab, id, {title, order, newestFirst})`**, **`publish(tab, source, node)`**, **`unpublish(tab, source, id?)`**: tree rows in a tab. `unpublish` without an id clears the source. A node is `{id, parent?, label, tag?, status?, icon?, dim?, detail?, time?, order?, actions?}`, where `actions` maps a key to a `/command`.
- **`setRegion(tab, lines)`**: a region tab drawn with your own lines. Keys come back as `ui_key` events with `{tab, node, key, width, height}`.
- **`ask(questions)`**, **`select(question, options)`**, **`confirm(question)`**, **`input(question)`**: dialogs in the `ask_user` form. They resolve to `undefined` when dismissed or when no window is open.

Styles are theme token names (`accent`, `dim`, `error`, …) or `#rrggbb`. A line is a string or a list of `{text, style?}` segments.

### Events

| Event | Payload | Return |
|---|---|---|
| `before_tool` | `{tool, args, cwd}` | `{block: reason}` refuses the call, `{args}` rewrites it. The first block wins. |
| `after_tool` | `{tool, args, output, isError}` | A string is appended to the result as feedback. |
| `input` | `{text, source: prompt\|steer}` | `{text}` rewrites the prompt before it is saved, `{handled: true}` swallows it. |
| `before_request` | `{provider, model, cwd, contextTokens, lastUser}` | A string, a list of strings or `{append}`: unsaved text sent after the cached prefix, for current state such as the time or git status. |
| `ui_key` | `{tab, node, key, width, height}` | Handled by your code. |
| any session event (`agent_start`, `tool_end`, `settled`, `compacted`, …) | the event JSON | Observe only. |

Handlers fail open: an error or a timeout (10 s) leaves things as they were.

## Process extensions

A long-running program that reads and writes line-delimited JSON-RPC 2.0 on stdio. novacode sends `initialize {apiVersion, cwd, session}` and expects `{tools, commands, events}` back within 10 s. After that it sends `tool`, `command` and `event` requests with the same payloads as above. The extension may send the notifications `notify {text}`, `log {text}`, `remind {text, wake}` and `ui {op, …}`. The `ui` ops are `status`, `widget`, `footer`, `header`, `editor`, `open_tab`, `show_tab`, `close_tab`, `source`, `publish`, `unpublish`, `region` and `shortcut`, with the fields listed in `api-v1.snap`. `session.messages`, `complete` and `spawnAgent` are script-only for now. stderr goes to `~/.novacode/logs/ext-<name>.log`.

## Custom providers

A provider that novacode doesn't know can be any program that speaks line-delimited JSON-RPC 2.0 on stdio. Add it to settings:

```jsonc
"providers": {
  "mine": { "api": "process", "command": "python3", "args": ["/path/to/provider.py"], "env": {} }
}
```

Then pick `mine/<model>` like any other model. The program is started on the first call and serves the whole session. It needs no key; it handles its own credentials.

| Direction | Message | Shape |
|---|---|---|
| novacode → program | `initialize` (request) | `{apiVersion, provider}` → `{models?: [id]}` (the list shown for `/model`) |
| novacode → program | `stream` (request) | `{model, messages, tools, maxTokens, thinking, session}` → `{stop: stop\|tool_use\|length}`, or a JSON-RPC error `{message, data?: {kind}}` where `kind` is `rate_limit`, `overflow`, `auth`, `transient`, … (retries follow the kind) |
| program → novacode | `event` (notification) | `{id, event}` with `event` one of `{type: "text", delta}`, `{type: "thinking", delta}`, `{type: "tool_call", id, name, arguments}`, `{type: "usage", input, output, cacheRead?, cacheWrite?}` |
| novacode → program | `cancel` (notification) | `{id}`: the user stopped the call |

`messages` and `tools` are novacode's own JSON, the same as `session.messages()`. stderr goes to novacode's stderr. Project settings only apply in trusted folders, so a repo can't start a provider program on its own.
