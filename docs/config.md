# Settings reference

<!-- Generated from crates/novacode-base/src/settings_doc.rs; edit there. -->

Global settings live in `~/.novacode/settings.json` and project settings in `.novacode/settings.json` (read only when the folder is trusted). Both are JSON with comments; the project wins key by key, except `permissions`, which is global only. Dotted keys are nested objects: `ui.theme` is `{"ui": {"theme": …}}`. A wrong value is reported by `novacode doctor` and treated as unset.

| Key | Type | Default | What it does |
|---|---|---|---|
| `model` | string | picked at first run | Default `provider/model[:level]`, e.g. `claude/claude-opus-5-5:medium`. |
| `thinking` | string | `medium` | Default thinking level: off, minimal, low, medium, high, xhigh, max. |
| `maxTokens` | number | 16384 | Output cap per reply, clamped to the model's limit. |
| `contextWindow` | number | from the catalog | Use a smaller context window than the model has. |
| `cacheRetention` | string | `short` | `long` asks for 1h (Anthropic) or 24h (OpenAI) prompt caching. |
| `cache.warm` | bool | false | Keep the prompt cache alive with 1-token replays while it pays off (L2.17). |
| `providers` | object | {} | Per-provider overrides: `providers.<id>.apiKey`, `baseUrl`, headers, compat flags. |
| `models` | object | {} | Per-model catalog overrides: `models.<provider>/<id>.contextWindow`, costs, … |
| `permissions` | object | auto mode | Global only. `mode` auto|ask|read-only, `allow`/`ask`/`deny` rules like `Bash(git push:*)`, `protected`, `profiles`. |
| `tools.<name>.exposure` | string | per tool | listed, codemode, deferred or hidden: how a tool reaches the model. |
| `hooks` | object | {} | Command hooks: `beforeTool`, `afterTool`, `events.<type>`, `timeoutMs` (L7.1). |
| `hooks.onActivity` | string | unset | Shell command run on working/blocked/idle changes, State JSON on stdin. |
| `processExtensions` | object | {} | Long-running JSON-RPC extensions: `<name>: {command, args, env}`. |
| `modes` | object | {} | Mode presets: `<name>: {prompt, readOnly, tools}`; `plan` is built in. |
| `checkpoints.enabled` | bool | true | File checkpoints per prompt, for `/rewind` (L4.15). |
| `memory.enabled` | bool | true | Observer notes and compaction from notes (L4.5). |
| `memory.model` | string | the profile's small model | Model for the observer, reflector and dropper. |
| `memory.observeAfterTokens` | number | 10000 | New tokens before the observer runs. |
| `memory.keepRecentTokens` | number | 20000 | Tokens kept verbatim after a compaction. |
| `memory.compactAtTokens` | number | min(80% of window, 400k) | Context size that triggers auto-compaction. |
| `memory.reflectAfterTokens` | number | 20000 | New observer tokens before the reflector runs. |
| `memory.poolTargetTokens` | number | 10000 | Size the dropper trims active notes down to. |
| `memory.store` | bool | true | Cross-session memory (SQLite) and its start-of-session index (L10.4). |
| `memory.indexItems` | number | 15 | Memory items in the session-start index. |
| `memory.indexChars` | number | 2000 | Character cap of that index. |
| `agents.stallMinutes` | number | 10 | A subagent silent this long is stopped. |
| `agents.timeoutMinutes` | number | 45 | A subagent running this long is stopped. |
| `goal.audit` | bool | true | `/goal` completion is checked by the `auditor` subagent before it counts. |
| `goal.auditModel` | string | the session's model | Model of the goal auditor (`provider/model[:level]`). |
| `goal.maxRuns` | number | 30 | Automatic continuations of a goal before it pauses (per start or `/goal resume`). |
| `web.native` | bool | false | Use the model's own web search when it has one (L2.23). |
| `web.searxngUrl` | string | unset | SearXNG instance for `web_search`, the last fallback. |
| `debug.metrics` | bool | false | Per-session timing JSONL in `~/.novacode/metrics/` for `novacode stats`. |
| `notifications.enabled` | bool | true | OSC 777 notification when a turn ends unwatched or needs you. |
| `tmux.agentState` | bool | true | Set the `@agent_*` pane options inside tmux. |
| `ui.theme` | string | `auto` | auto, catppuccin-mocha, catppuccin-latte or a file in `~/.novacode/themes`. |
| `ui.turnCollapse` | bool | true | Fold finished turns to one line (Alt+O toggles). |
| `ui.thinking` | string | `collapsed` | How thinking shows: collapsed, full or hidden. |
| `ui.userMessage` | object | rounded box | `border`, `borderColor`, `text`, `paddingX`, `paddingY` of your messages. |
| `ui.statusline` | object | built-in rows | `placement` and `rows` of segments (activity, ctx, cache, model, …). |
| `ui.icons` | string | `nerd` | `unicode` when your font has no Nerd Font glyphs. |
| `ui.images` | string | `auto` | Image protocol: auto, kitty, iterm2, sixel, halfblocks or off. |
| `ui.panel.auto` | bool | true | Open the side panel by itself while todos or subagents run. |
| `ui.panel.host` | string | `inline` | inline, tmux or auto: draw the panel in novacode or in a tmux pane. |
| `ui.panel.tmuxWidth` | string | `30%` | Width of the tmux panel pane. |
| `ui.slow` | bool | detected | Lower frame rate and animations (SSH, busy machine). |
