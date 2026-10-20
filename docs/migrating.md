# Migrating from other tools

This page tells you what novacode picks up from Claude Code, pi and Codex without any work, what you can copy over, and what you have to set up again.

## From Claude Code

### What loads as it is

novacode reads these Claude Code files directly. You don't copy anything.

| Claude Code | In novacode |
|---|---|
| `~/.claude/skills`, `.claude/skills` | Skills |
| `~/.claude/agents`, `.claude/agents` | Subagent definitions. Claude tool names (`Read`, `Glob`, `Task`, …) are mapped to novacode's. `model: sonnet` or `inherit` is understood. |
| `~/.claude/commands`, `.claude/commands` | Prompt templates, used as `/name` |
| `.mcp.json` in the project | MCP servers (same format; trusted folders only) |
| `CLAUDE.md` | Instructions for the model, when the folder has no `AGENTS.md` |

Rules for these folders:

- novacode's own folders win. If `~/.novacode/skills/review` and `~/.claude/skills/review` both exist, the novacode one is used. `novacode doctor` lists the ones that were skipped.
- A project's `.claude/` folder (skills, agents, commands) loads only when you [trust](security.md) the folder. `~/.claude` always loads.
- To turn all of this off, set `compat.claude` to `false`. See [config](config.md).

### CLAUDE.md and AGENTS.md

In each folder from `/` down to your working folder, novacode loads one instruction file. The order of preference is `AGENTS.override.md`, then `AGENTS.md`, then `CLAUDE.md`. So a repo with only `CLAUDE.md` works, and a repo with both uses `AGENTS.md`. The same real file is never loaded twice.

The global file is `~/.novacode/AGENTS.md` (or `CLAUDE.md` in that folder). `~/.claude/CLAUDE.md` is not read. Copy it if you want it:

```sh
cp ~/.claude/CLAUDE.md ~/.novacode/AGENTS.md
```

These files load even in folders you haven't trusted. `--no-context-files` skips them for one run.

### Copy your settings: `novacode import claude`

Hooks, MCP servers, permission rules and the default model live in Claude Code's settings files, which novacode does not read on its own. Copy them once:

```sh
novacode import claude          # show the changes, then ask [y/N]
novacode import claude --yes    # apply without asking
```

It reads `~/.claude/settings.json` and `~/.claude.json`. It prints every change, plus a list of what it left out and why. Without `--yes` it asks; if input isn't a terminal it only prints the changes and says "run with --yes to apply". Before writing, it saves a backup next to each file it changes (`settings.json.bak`, `mcp.json.bak`). Running it again adds nothing that is already there.

What it copies:

| From Claude Code | To novacode |
|---|---|
| `hooks` `PreToolUse` | `hooks.beforeTool` in `~/.novacode/settings.json` |
| `hooks` `PostToolUse` | `hooks.afterTool` |
| `hooks` `SessionStart` | `hooks.events.session_start` |
| `hooks` `SessionEnd` | `hooks.events.session_end` |
| `hooks` `Stop` | `hooks.events.agent_end` |
| `permissions.allow`, `ask`, `deny` | `permissions.allow`, `ask`, `deny`, with tool names changed to novacode's (`Bash(npm test:*)` becomes `bash(npm test:*)`, `Glob` becomes `find`, …). Rule tool names are not case sensitive |
| `mcpServers` in `~/.claude.json` | `~/.novacode/mcp.json` |
| `model` | `model` in settings, only when you have no default model yet and the name maps to a `claude/claude-<name>` model (for example `sonnet`) |

How hooks work after the copy: each hook is stored as a call to `novacode claude-hook <event> <matcher> <command>`. That wrapper applies the Claude Code matcher (a regex on the tool name) and gives your script the input in Claude Code's shape (`tool_name`, `tool_input`, `session_id`, `cwd`, `hook_event_name`), and sets `CLAUDE_PROJECT_DIR`. A block from your script (exit code 2, `"decision": "block"` or a `permissionDecision` of `deny`) blocks the tool call in novacode. Keep `novacode` on your `PATH` so the hooks can run.

What it leaves out, and says so:

- Hook events other than the five above (`UserPromptSubmit`, `PreCompact`, …), and hooks that are not plain commands. A script extension may cover them (see [extensions](extensions.md)).
- `Stop` is copied as an observer. It can't keep the agent going as it can in Claude Code.
- `SessionStart` output is not added to the model's context.
- Permission rules for `WebFetch(domain:…)`. novacode doesn't match web fetches by domain.
- `permissions.defaultMode`. novacode keeps its own `permissions.mode` (`auto`, `ask` or `read-only`).
- An MCP server whose name you already have in `~/.novacode/mcp.json` (yours is kept).
- A `model` when you already have one, or when no novacode model matches.

Nothing else is read. Project-level `.claude/settings.json`, plugins, output styles and status line settings are not imported. Project MCP servers need no import because `.mcp.json` is read as it is.

### Tool names

| Claude Code | novacode |
|---|---|
| `Read` | `read` |
| `Write` | `write` |
| `Edit`, `MultiEdit` | `edit` |
| `Bash` | `bash` |
| `Grep` | `grep` |
| `Glob` | `find` |
| `LS` | `ls` |
| `WebFetch` | `web_fetch` |
| `WebSearch` | `web_search` |
| `TodoWrite` | `todo` |
| `Task` | `agent` |

### Logging in

Your Claude Pro or Max subscription works directly: `novacode login claude`. Model names look like `claude/claude-opus-5-5`. See [providers](providers.md).

## From pi

novacode follows pi in several places on purpose.

| Same as pi | Notes |
|---|---|
| Key binding names | Actions like `app.model.select` and `tui.editor.cursorLeft`, and their defaults, follow pi's. Copy your pi `keybindings.json` to `~/.novacode/keybindings.json`. `novacode doctor` reports unknown or clashing keys. |
| `AGENTS.md` | Read the same way, in every folder up the tree. |
| `~/.agents/skills` | Scanned for skills. |
| Skills (`SKILL.md`) | Same format. |
| Print, JSON and RPC modes | `-p`, `--json`, `--rpc` exist. The messages are novacode's own (see [automation](automation.md)); don't assume a pi client works unchanged. |

### What you set up again

- **Your pi folder isn't scanned.** `~/.pi/agent/skills`, `~/.pi/agent/prompts` and `~/.pi/agent/AGENTS.md` are not read. Link or copy them:

  ```sh
  ln -s ~/.pi/agent/skills ~/.novacode/skills
  ln -s ~/.pi/agent/prompts ~/.novacode/prompts
  cp ~/.pi/agent/AGENTS.md ~/.novacode/AGENTS.md
  cp ~/.pi/agent/keybindings.json ~/.novacode/keybindings.json
  ```

  If `~/.novacode/skills` already exists, link the individual skill folders into it instead.
- **pi extensions don't run.** They are written for pi's API. novacode has its own extension API (script extensions in TypeScript or JavaScript, and process extensions in any language). You need to port them. See [extensions](extensions.md).
- **npm packages are not supported.** novacode packages come from git or a local folder. See [packages](packages.md).
- **Settings differ.** Setting names are novacode's; see [config](config.md).
- **Logins.** Run `novacode login <provider>` again. See [providers](providers.md).
- **Sessions** are not imported.

### Concept mapping

| pi | novacode |
|---|---|
| Skills | Skills (`~/.novacode/skills`, `~/.agents/skills`, `.novacode/skills`) |
| Prompt templates | Prompt templates (`prompts/`, used as `/name`) |
| Extensions | Extensions (different API) |
| Packages | Packages (git or local only) |
| `keybindings.json` | `~/.novacode/keybindings.json` |
| `AGENTS.md` | `AGENTS.md` (global one in `~/.novacode/`) |
| `settings.json` | `~/.novacode/settings.json` and `.novacode/settings.json` |

## From Codex

| Codex | novacode |
|---|---|
| `AGENTS.md`, `AGENTS.override.md` | Both are read in every folder up the tree. `AGENTS.override.md` wins over `AGENTS.md` in the same folder. |
| `~/.codex/AGENTS.md` | Not read. Copy it to `~/.novacode/AGENTS.md`. |
| `codex login` (ChatGPT sign-in) | `novacode login codex`. It uses a device code that works over SSH. This is a separate login; novacode doesn't read Codex's own credentials. Use models like `codex/gpt-5.5:high`. |
| `codex exec "…"` | `novacode -p "…"` (see [automation](automation.md)) |
| `~/.agents/skills` | Scanned for skills. |
| MCP servers in `config.toml` | Add them again: `novacode mcp add name -- command args…` (see [mcp](mcp.md)). |
| Approval modes | `--mode auto`, `ask` or `read-only`, or `permissions.mode` in settings (see [security](security.md)). |

`config.toml` and `~/.codex/skills` are not read.

## See also

- [quickstart](quickstart.md): first run
- [providers](providers.md): logins and API keys
- [skills](skills.md): where skills, agents and prompts load from
- [mcp](mcp.md): MCP servers
- [extensions](extensions.md): porting hooks and plugins
- [packages](packages.md): sharing what you build
- [config](config.md): every setting
