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

`novacode.apiVersion` is `1`. Names and arguments listed here stay stable within API v1.

### Registering

- **`registerTool(t)`**: `{name, description, parameters (JSON Schema), exposure?, execute(args, {cwd, session})}`. `execute` returns a string or `{content, isError?, details?}`. `details.ui` (styled lines) replaces the tool output in the transcript. `exposure` is listed (the default), codemode, deferred or hidden.
- **`registerCommand(name, {description, handler(args)})`**: a `/name` command. The returned string is shown as a notice.
- **`registerShortcut(key, handler | {handler, description})`**: a key such as `ctrl+alt+d`, shown in Alt+/ help.
- **`on(event, handler)`**: see Events below.

### Acting

- **`exec(command, {timeout?})`**: runs `bash -c` in the session folder and resolves to `{code, stdout, stderr}`. Catastrophic commands are refused. The timeout is in seconds (default 60, at most 600).
- **`readFile(path)`** / **`writeFile(path, text)`**: paths are relative to the session folder, or start with `~/`.
- **`notify(text)`**: a notice in every window. The model doesn't see it.
- **`remind(text, {wake?})`**: a note for the model, saved and sent with the next model call. `wake` starts a run when the session is idle. Use it for anything the model must remember.
- **`log(...args)`**: writes to `~/.novacode/logs/ext-<name>.log`. `console.*` goes there too.

### UI (`novacode.ui`, version 1)

Every call sends data; novacode draws it in every window, including attached ones and the tmux panel.

- **`setStatus(key, text | segments, {style?})`**: a status-line segment, shown as `ext:<key>`. `null` text removes it.
- **`setWidget(key, lines, {below?})`**: lines above the editor, or below it with `below`.
- **`setHeader(lines)`** / **`setFooter(lines)`**: lines above the transcript, or in place of the status line.
- **`setEditor({title?, border?})`**: a title and border colour for the editor box.
- **`openTab(name, {title, priority, width, region, show})`**, **`showTab(name)`**, **`closeTab(name)`**: a side-panel tab.
- **`setSource(tab, id, {title, order, newestFirst})`**, **`publish(tab, source, node)`**, **`remove(tab, source, id?)`**: tree rows in a tab. A node is `{id, parent?, label, tag?, status?, icon?, dim?, detail?, time?, order?, actions?}`, where `actions` maps a key to a `/command`.
- **`region(tab, lines)`**: a region tab drawn with your own lines. Keys come back as `ui_key` events with `{tab, node, key, width, height}`.
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

A long-running program that reads and writes line-delimited JSON-RPC 2.0 on stdio. novacode sends `initialize {apiVersion, cwd, session}` and expects `{tools, commands, events}` back within 10 s. After that it sends `tool`, `command` and `event` requests with the same payloads as above. The extension may send the notifications `notify {text}`, `log {text}`, `remind {text, wake}` and `ui {op, …}` (the same ops as `novacode.ui`). stderr goes to `~/.novacode/logs/ext-<name>.log`.
