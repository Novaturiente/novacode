# Extensions

novacode runs extensions in three forms. All of them reach files and commands through the same safety rules as the model (risk gate, trust).

| Form | Where | Good for |
|---|---|---|
| **Script** (TS or JS, QuickJS) | `~/.novacode/extensions/<name>.ts` or `<name>/index.ts`; project `.novacode/extensions/` (trusted folders only) | Tools, commands, hooks and UI, with no build step |
| **Process** (any language, JSON-RPC on stdio) | settings `processExtensions.<name>: {command, args, env}` | Bigger plugins, or code in Python/Go/Rust |
| **Command hooks** | settings `hooks` | One shell command per event (see `docs/config.md`) |

## Packages

A package is a folder or git repo with any of `extensions/`, `skills/`, `prompts/`, `agents/`, `themes/` and an `mcp.json` (Claude Code format; your own and the project's servers win a name).

```sh
novacode install https://github.com/you/pack.git   # cloned; its commit is pinned
novacode install ~/code/my-pack                     # linked: edits apply at once
novacode packages                                   # list (disabled ones say why)
novacode packages update [name]                     # pull and move the pin
novacode uninstall pack
```

Pins live in `~/.novacode/packages.lock.json` and record which commit each package is at. `novacode install` always takes the newest commit; only project packages (below) install at their locked commits.

An optional `novacode-package.json` says which extension API the package was written for:

```json
{ "name": "pack", "version": "1.2.0", "apiVersion": [1, 2] }
```

`apiVersion` is a number or `[min, max]`. If this novacode's API isn't in the range, the package is disabled with a notice instead of breaking at runtime.

**Project packages.** A repo can list the packages its team uses in `.novacode/packages.json` (`{"packages": {"name": "git-url"}}`) and commit `.novacode/packages.lock.json`. In a trusted folder, novacode asks once whether to install the missing ones into `.novacode/packages/` (add that folder to `.gitignore`); `novacode packages install` does it any time. Nothing is installed without asking.

## Skills, agents and commands from other tools

Skills (`SKILL.md`), agent files and prompt templates load from these folders. The first folder that has a name wins:

1. the project's `.novacode/` (trusted folders only, except `prompts/`, which is plain text);
2. the project's `.claude/` (trusted folders only);
3. `~/.novacode/`;
4. `~/.claude/`;
5. `~/.agents/skills` (skills only);
6. installed packages.

Under `.claude/`, templates are read from `commands/`, as in Claude Code. In agent files, Claude Code's tool names (`Read`, `Glob`, `Task`, …) are mapped to novacode's, and `model: sonnet` or `inherit` uses the parent's model. Set `compat.claude: false` to skip the `.claude` folders.

Nothing loads twice. The same file reached through two folders or a symlink counts once. A different file with a name that's already taken is skipped, and `novacode doctor` lists it. Claude Code hooks, MCP servers and permissions are not read from `.claude/settings.json`; `novacode import claude` copies them once, after showing you the changes.

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

The default export gets the frozen `novacode` object (also `globalThis.novacode`). Types are stripped, so TypeScript works as is.

**Imports.** An extension can import its own files with relative paths (`import { x } from "./lib/util"`). The extension can be `.ts`, `.js` or a folder with `index.ts`. Imports resolve inside the folder that holds the entry file and can't reach outside it. Bare package names (`import "lodash"`) are refused: npm packages expect Node, which QuickJS doesn't have. Write a process extension for those. Setup has 5 s of JS time. Each extension has its own memory limit.

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
- **`spawnAgent(task, opts?)`**: a subagent through the `agent` tool, with the same depth limit, file claims and approvals. `opts` takes the tool's spawn fields (`title`, `agent`, `model`, `cwd`, `after`, `notify`, `worktree`, `tools`, `mcp`). The Subagents panel shows `[ext:<name>]` before its title. Resolves to `{id}`; the report arrives the way `notify` says (default: it wakes the model).
- **`notify(text)`**: a notice in every window. The model doesn't see it.
- **`remind(text, {wake?})`**: a note for the model, saved and sent with the next model call. `wake` starts a run when the session is idle. Use it for anything the model must remember.
- **`sendMessage(text, {deliverAs?})`**: a real user message, saved like a typed one. `deliverAs: "steer"` puts it into the running turn; `"followUp"` (the default) sends it after the turn, or at once when the session is idle.
- **`settings.get(path?)`**: the merged settings at a dotted `path` (all of them when left out). Keep your own under `extensions.<your name>`.
- **`state.append(kind, data)`** and **`state.entries(kind)`**: your extension's data, kept in the session file and never sent to the model. `entries` returns what you appended under `kind` on the current branch, oldest first, so state follows `/rewind` and `/tree`.
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
| `after_tool` | `{tool, args, output, isError, callId, durationMs, touched}` | A string is appended to the result as feedback. `{append?, replace?, isError?}` also replaces the result text or sets its error flag. `touched` lists the absolute paths the call changed or read: the `path` of edit/write/read, the files of an apply_patch, and the files a `bash` command changed (seen from git status before and after; outside a git repo, none). |
| `input` | `{text, source: prompt\|steer}` | `{text}` rewrites the prompt before it is saved, `{handled: true}` swallows it. |
| `before_request` | `{provider, model, cwd, contextTokens, lastUser}` | A string, a list of strings or `{append}`: unsaved text sent after the cached prefix, for current state such as the time or git status. |
| `ui_key` | `{tab, node, key, width, height}` | Handled by your code. |
| any session event (`agent_start`, `tool_end`, `settled`, `compacted`, …) | the event JSON | Observe only. |

Handlers fail open: an error or a timeout (10 s) leaves things as they were.

## Process extensions

A long-running program that reads and writes line-delimited JSON-RPC 2.0 on stdio. Turn one on in settings:

```json
{ "processExtensions": { "lsp": { "command": "novacode-lsp", "args": [], "env": {} } },
  "extensions": { "lsp": { "waitMs": 3000 } } }
```

- **Start.** novacode sends `initialize {apiVersion, cwd, session, home, settings}` (`settings` is your `extensions.<name>` block) and expects `{tools, commands, events, timeoutMs?}` back within 10 s. `timeoutMs` (up to 60 000) replaces the 10 s limit for your event handlers, for work like waiting on a language server.
- **Calls.** After that it sends `tool`, `command` and `event` requests with the same payloads as above.
- **Notifications you may send:** `notify {text}`, `log {text}`, `remind {text, wake}` and `ui {op, …}`. The `ui` ops are `status`, `widget`, `footer`, `header`, `editor`, `open_tab`, `show_tab`, `close_tab`, `source`, `publish`, `unpublish`, `region` and `shortcut`, with the fields listed in `api-v1.snap`.
- **Calling novacode.** Send a request `host {op, args}` (with your own `id`) to use anything a script can: `exec {command, timeout?}`, `readFile {path}`, `writeFile {path, text}`, `ask {questions}`, `messages`, `complete {…}`, `spawnAgent {task, opts}`, `settings {path?}`, `stateAppend {kind, data}`, `stateEntries {kind}`, `sendMessage {text, deliverAs?}`. The answer is a normal JSON-RPC result or error.
- **Stop.** Before the process group is killed it gets a `shutdown` notification and 2 s to exit.
- stderr goes to `~/.novacode/logs/ext-<name>.log`.

**Example: image generation.** [`extensions/image-gen`](../extensions/image-gen) adds a `generate_image` tool (Gemini image models through `novacode login antigravity`, or a key in `extensions.image-gen.apiKey` / `GEMINI_API_KEY`). Install with `cargo install --locked --path extensions/image-gen` and add `"processExtensions": {"image-gen": {"command": "novacode-image-gen"}}`.

**Example: language-server diagnostics.** [`extensions/lsp`](../extensions/lsp) is a process extension that reports the errors an edit (or a `bash` command) left in the touched files. It isn't part of novacode: install it with `cargo install --locked --path extensions/lsp`, then add the `processExtensions.lsp` entry above.

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
