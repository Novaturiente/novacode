# Settings reference

<!-- Generated from crates/novacode-base/src/settings_doc.rs; edit there. -->

Global settings live in `~/.novacode/settings.json` and project settings in `.novacode/settings.json` (read only when the folder is trusted). Both are JSON with comments; the project wins key by key, except `permissions`, which is global only. Dotted keys are nested objects: `ui.theme` is `{"ui": {"theme": …}}`. A wrong value is reported by `novacode doctor` and treated as unset.

| Key | Type | Default | What it does |
|---|---|---|---|
| `compat.claude` | bool | true | Also load skills, agents and commands from `~/.claude` and a trusted project's `.claude`. novacode's own folders win a name. |
| `model` | string | picked at first run | Default `provider/model[:level]`, e.g. `claude/claude-opus-5-5:medium`. |
| `thinking` | string | `medium` | Default thinking level: off, minimal, low, medium, high, xhigh, max. |
| `maxTokens` | number | 16384 | Output cap per reply, clamped to the model's limit. |
| `contextWindow` | number | from the catalog | Override the model's context window (normally a smaller one). |
| `cacheRetention` | string | `short` | `long` asks Anthropic (Claude) for 1h prompt caching instead of 5 min. Other providers ignore it. |
| `cache.warm` | bool | false | Keep the prompt cache alive with 1-token replays while it pays off. |
| `providers` | object | {} | Add a provider or change a bundled one: `providers.<id>.{api, name, baseUrl, auth, env, apiKey, keyOptional, catalog, headers, compat}`, merged field by field over the bundled profile. See docs/providers.md. |
| `models` | object | {} | Per-model catalog overrides, keyed `"<provider>/<id>"`: `contextWindow`, `maxTokens`, `images`, `reasoning` (bools), `name`, `cost`. See docs/providers.md. |
| `permissions` | object | auto mode | Global only. `mode` auto\|ask\|read-only; `allow`/`ask`/`deny` lists of rules like `Bash(git push:*)`; `protected: {commands, paths}`; `profiles.<name>.tools`, named tool sets. See docs/security.md. |
| `tools.<name>.exposure` | string | per tool | How a tool reaches the model: `listed` (declared to it), `codemode` (alias `codemode-only`: only codemode scripts call it), `deferred` (found through `tool_search`) or `hidden` (user and UI only). |
| `hooks` | object | {} | Command hooks: `beforeTool`, `afterTool` and `events.<type>`, each a command or a list; `timeoutMs` (default 5000) limits `beforeTool`, `afterTool` gets twice that. See docs/automation.md. |
| `hooks.onActivity` | string | unset | Shell command run on working/blocked/idle changes, State JSON on stdin. |
| `processExtensions` | object | {} | Long-running JSON-RPC extensions in any language: `<name>: {command, args, env}`. See docs/extensions.md. |
| `modes` | object | {} | Mode presets: `<name>: {prompt, readOnly, tools}`; `plan` is built in. |
| `checkpoints.enabled` | bool | true | File checkpoints per prompt, for `/rewind`. |
| `extensions.<name>` | object | unset | Settings of extension `<name>`: a process extension gets them in `initialize`, any extension through `novacode.settings.get("extensions.<name>")`. |
| `security.maskSecrets` | bool | false | Also mask well-known secret shapes in tool output (GitHub/AWS/OpenAI/Anthropic/Slack/Google tokens, private keys, `*_TOKEN=`/`*_SECRET=` values) before the model sees them. Your own logins are always masked. |
| `memory.enabled` | bool | true | Observer notes, compaction from notes and the `recall` tool. |
| `memory.model` | string | the profile's small model | Model for the observer, reflector and dropper. |
| `memory.observeAfterTokens` | number | 10000 | New tokens before the observer runs. |
| `memory.keepRecentTokens` | number | 20000 | Tokens kept verbatim after a compaction. |
| `memory.compactAtTokens` | number | min(80% of window, 400k) | Context size that triggers auto-compaction. |
| `memory.reflectAfterTokens` | number | 20000 | New conversation tokens before the reflector runs. |
| `memory.poolTargetTokens` | number | 10000 | Size the dropper trims active notes down to. |
| `memory.store` | bool | true | Cross-session memory (SQLite) and its start-of-session index. |
| `memory.indexItems` | number | 15 | Memory items in the session-start index. |
| `memory.indexChars` | number | 2000 | Character cap of that index. |
| `agents.stallMinutes` | number | 10 | A subagent silent this long is stopped. |
| `agents.timeoutMinutes` | number | 45 | A subagent running this long is stopped. |
| `agents.maxConcurrent` | number | 4 | Subagents running at once; more wait as `queued`, in spawn order. A subagent's own subagents run one at a time. |
| `agents.parentPicksTools` | bool | true | The model may pick a subagent's tools (`tools`) and MCP servers (`mcp`) when it spawns one, from its own. Off: only the type's tools. |
| `agents.env` | object | {} | Environment variables every subagent starts with, e.g. `{"CAVEMAN_DEFAULT_MODE": "off", "PONYTAIL_SUBAGENT_MATCHER": "^$"}` keeps those plugins' `SubagentStart` text out of subagents. |
| `agents.<name>.tools` | list | the agent file's | Default tools of subagent type `<name>` (built-ins too); a subagent sees only its tools. |
| `agents.<name>.mcp` | list | [] | MCP servers subagent type `<name>` gets by default (subagents start none otherwise). |
| `goal.audit` | bool | true | `/goal` completion is checked by the `auditor` subagent before it counts. |
| `goal.auditModel` | string | the session's model | Model of the goal auditor (`provider/model[:level]`). |
| `goal.maxRuns` | number | 30 | Automatic continuations of a goal before it pauses (per start or `/goal resume`). |
| `goal.maxTokens` | number | none | Tokens (prompt + output) a goal may spend; then the agent summarises its progress and the goal pauses. |
| `goal.judge` | string | the observer's model | Checks after each turn whether the goal is met: a `provider/model`, a classifier model (e.g. `typesafe/jev-latest`: calibrated probabilities, met needs 0.8), or `off` (only the agent's `complete` ends it). |
| `web.native` | bool | false | Use the model's own web search when it has one. |
| `web.searxngUrl` | string | unset | SearXNG instance for `web_search`, the last fallback. |
| `debug.metrics` | bool | false | Per-session timing JSONL in `~/.novacode/metrics/` for `novacode stats`. |
| `notifications.enabled` | bool | true | OSC 777 notification when a turn ends unwatched or needs you. |
| `tmux.agentState` | bool | true | Set the `@agent_*` pane options inside tmux. |
| `ui.theme` | string | `auto` | auto, dark (catppuccin-mocha), light (catppuccin-latte) or a file in `~/.novacode/themes`. |
| `ui.turnCollapse` | bool | true | Fold finished turns to one line (Alt+O toggles). |
| `ui.thinking` | string | `collapsed` | How thinking shows: collapsed, full or hidden. |
| `ui.userMessage` | object | rounded box | Your messages' box: `border` (rounded, single, heavy, double, ascii, none), `borderColor` and `text` (`#rrggbb` or `{fg, bg, bold, italic, …}`), `paddingX` (1), `paddingY` (0). |
| `ui.statusline` | object | built-in rows | `placement` and `rows` of segments (activity, ctx, cache, model, …). Segment names are in docs/tui.md. |
| `ui.icons` | string | `nerd` | `unicode` when your font has no Nerd Font glyphs; any other value means `nerd` (`novacode doctor` flags it). |
| `ui.images` | string | `auto` | Image protocol: auto, kitty, iterm2, sixel, halfblocks or off. |
| `ui.panel.auto` | bool | true | Open the side panel by itself while todos or subagents run. |
| `ui.panel.host` | string | `inline` | inline, tmux or auto: draw the panel in novacode or in a tmux pane. `tmux` and `auto` only take effect inside tmux. |
| `ui.panel.tmuxWidth` | string | `30%` | Width of the tmux panel pane. |
| `ui.slow` | bool | on over SSH or when the load average is above the core count | Lower frame rate and animations. |

Other files in `~/.novacode`: logins and keys in `auth.json`, MCP servers in `mcp.json` ([mcp](mcp.md)), trusted folders in `trust.json` ([security](security.md)). The full list is in [install](install.md).

## Environment variables

| Variable | What it does |
|---|---|
| `NOVACODE_HOME` | Use this folder instead of `~/.novacode`. |
| `NOVACODE_LOG` | Log level or filter, e.g. `debug` ([troubleshooting](troubleshooting.md)). |
| `NOVACODE_HOOKS_DISABLED` | Any value turns command hooks off ([automation](automation.md)). |
| Provider keys (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, …) | API keys, see [providers](providers.md). |
| `BRAVE_API_KEY`, `GEMINI_API_KEY` | Web search engines ([tools](tools.md)). |
| `VISUAL`, `EDITOR` | The editor `Ctrl+G` opens. Terminal variables are in [tui](tui.md). |
