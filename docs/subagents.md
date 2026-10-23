# Subagents, plan mode and goals

This page covers three ways to get more done with less hand-holding: subagents (other novacode sessions that work in the background), plan mode (think first, read-only), and goals (keep going until a checkable result is met). It also mentions the `todo` and `schedule` tools.

## Subagents

A subagent is a separate novacode session started by your main session. The model starts it with the `agent` tool, gives it a task, and keeps working. When the subagent finishes, its final message comes back as a report.

A subagent does not see your conversation. The model must give it a complete task. It dies with its parent.

### Built-in types

| Type | What it is for | Tools | Model |
|---|---|---|---|
| `scout` | Read-only recon: find where something is done, map a folder, summarise long files | `read`, `grep`, `find`, `ls`, `todo` | the provider's small model (falls back to the parent's) |
| `worker` | A bounded change: edit the named files, run the check, report | all of the parent's tools | the parent's |
| `reviewer` | Review a change for bugs, risk and missing tests; changes nothing | `read`, `grep`, `find`, `ls`, `bash`, `todo` | the parent's |
| `auditor` | Checks that a goal is really done (used by `/goal`); changes nothing | `read`, `grep`, `find`, `ls`, `bash` | the parent's (or `goal.auditModel`) |

If the model doesn't name a type, it gets `worker`.

### The `agent` tool

One tool, several actions:

| Action | What it does |
|---|---|
| `spawn` | Start a subagent. Returns an id like `a1`. |
| `list` | One line per subagent: id, status, type, title, model, time, tokens. |
| `status {id}` | One agent's status, session id, working folder and last activity. |
| `log {id, limit?}` | Its recent activity lines (default 20). |
| `read {id, offset?, limit?}` | The full report (`limit` default 20000 characters). Long reports reach the model cut at 6000 characters, with a pointer to `read`. |
| `send {id, message}` | Steer a running agent, or resume a finished one with a new message. |
| `wait {ids?, any?, timeout?}` | Block until reports arrive. Default: all started agents, 600 s (max 3600). |
| `stop {id}` | Stop it. |

There is no separate `resume` action: `send` to a finished agent resumes its session.

`spawn` fields:

| Field | Meaning |
|---|---|
| `task` | Complete, self-contained instructions. Required. |
| `title` | A few words; shown in the panel. Defaults to the first 40 characters of the task. |
| `agent` | Type name. Default `worker`. |
| `model` | `provider/model` or `provider/model:level`. Overrides the type's model. |
| `cwd` | Working folder. |
| `after` | Ids of agents that must finish first. The new agent waits in `queued`. If one of them fails or is stopped, it fails too. |
| `notify` | `wake` (default): the report starts a turn. `next`: rides along with the next turn. `quiet`: panel only. |
| `worktree` | `true` runs it in a new git worktree under `.worktrees/` on branch `worktree-<title-slug>-<id>` (for example `worktree-fix-parser-a2`). You merge the branch yourself. |
| `tools` | Tools for it, picked from the parent's own. Default: the type's tools. |
| `mcp` | MCP servers it may use. Default: none. |

`tools` and `mcp` are only honoured while `agents.parentPicksTools` is on (the default). A subagent never gets a tool its parent doesn't have. See [MCP](mcp.md) for how a subagent reaches MCP servers.

### Limits and safety

- **Depth.** Your session can start subagents, and those can start their own. Theirs cannot. Two levels, no more.
- **Permission mode.** A subagent starts in your session's permission mode. While plan mode (or another read-only preset) is on, it starts read-only, and a preset's tool list limits its tools too.
- **File claims.** Two agents never edit the same file. A subagent that tries `edit`, `write` or `apply_patch` on a file another live agent holds is refused with a message naming the holder. A claim passes down from parent to child. Claims of dead processes are dropped.
- **Stall and timeout.** A subagent with no activity for `agents.stallMinutes` (default 10) is stopped. One that runs longer than `agents.timeoutMinutes` (default 45) is stopped.
- **Questions and approvals.** If a subagent needs to ask you something or needs an approval, it shows up in your session.
- **Print mode.** In `-p` runs nobody can approve, so every subagent's `notify` is forced to `wake`.

### Watching them

Subagents appear in the **Agent** tab of the side panel: type, id, title, status, tokens, tool calls, and what each is doing now. `ui.panel.auto` (default on) opens the panel by itself while subagents or todos run. To watch one in its own window, `agent status` prints a `novacode attach <session>` line.

### Agent definition files

Add your own types as Markdown files. One file per agent, directly in an `agents/` folder (not in subfolders).

```markdown
---
name: docs
description: Writes and updates documentation
tools: read, grep, find, write, edit
model: small
thinking: low
---
You write documentation. Keep it short and accurate. Never touch source code.
```

Folders searched, first match for a name wins (built-ins come last):

1. `<project>/.novacode/agents/` (trusted folders only)
2. `<project>/.claude/agents/` (trusted folders only, needs `compat.claude`, default on)
3. `~/.novacode/agents/`
4. `~/.claude/agents/` (needs `compat.claude`)
5. `agents/` inside installed [packages](packages.md)

So a file named `scout.md` replaces the built-in `scout`. `novacode doctor` lists shadowed files.

Frontmatter fields:

| Field | Meaning |
|---|---|
| `name` | Type name. Default: the file name without `.md`. |
| `description` | Shown to the model so it can pick the type. |
| `tools` | Comma-separated tool names, or a list like `[Read, Grep]`. Leave it out to give all of the parent's tools. |
| `model` | `provider/model` or `small`. Any other value (such as Claude Code's `inherit` or `sonnet`) is ignored and the parent's model is used. |
| `thinking` | Thinking level, added to the model as `model:level`. |
| `mode` | `replace`: the file's text replaces the built-in subagent preamble instead of being added after it. `systemPromptMode: replace` also works. |

The text after the frontmatter is the agent's role prompt.

**Claude Code agent files** work as they are. Tool names are mapped:

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

Other names (for example `mcp__server__tool`) pass through unchanged.

novacode sets a few variables for its subagent processes (`NOVACODE_AGENT_DEPTH`, `NOVACODE_AGENT_CHAIN`, `NOVACODE_AGENT_TITLE`, `NOVACODE_BG_CHILD`). They are internal; don't set them.

### Settings

| Key | Default | Meaning |
|---|---|---|
| `agents.stallMinutes` | `10` | Stop a subagent silent this long. |
| `agents.timeoutMinutes` | `45` | Stop a subagent running this long. |
| `agents.parentPicksTools` | `true` | The model may pick `tools` and `mcp` when it spawns. Off: only the type's tools, and `tools`/`mcp` in a spawn are ignored. |
| `agents.<name>.tools` | the type's | Replace the tools of type `<name>` (built-ins too). |
| `agents.<name>.mcp` | `[]` | MCP servers type `<name>` gets by default. |

```json
{
  "agents": {
    "scout": { "tools": ["read", "grep", "find", "ls"] },
    "worker": { "mcp": ["github"] }
  }
}
```

A subagent that gets MCP servers also gets `codemode`, because MCP tools run through it. The full settings table is in [Settings](config.md).

## Plan mode

In plan mode the model can read, search and run read-only commands. Every change (edits, writes, mutating commands) is refused. The model investigates, asks you about real ambiguities, then submits a plan with the `plan_complete` tool.

| You type | What happens |
|---|---|
| `/plan` | Turn plan mode on. Typed again while on, it turns it off. |
| `/plan <task>` | Turn it on and start planning that task. |

The status line shows `⏸ plan mode` while it is on.

The model can also ask for plan mode itself with `enter_plan_mode` before bigger work. You get a question with two answers: "Plan first" or "Just do it".

When the plan is ready, `plan_complete` shows it to you with two answers:

- **Approve**: plan mode ends and the model builds it.
- **Keep planning**: type what to change; it stays read-only and revises.

The form shows the first 24 lines of the plan. `Ctrl+G` opens the whole plan in your editor. Save and quit to bring your edits back: the form marks the plan "edited", and whichever answer you pick sends the edited plan to the model as the plan to build (or to revise).

### Mode presets

Plan mode is one preset. You can define your own in settings under `modes`:

```json
{
  "modes": {
    "teach": {
      "prompt": "Explain each step as you go. Don't change files.",
      "readOnly": true
    },
    "docs-only": {
      "prompt": "Only touch Markdown files.",
      "tools": ["read", "grep", "find", "edit", "write"]
    }
  }
}
```

| Field | Meaning |
|---|---|
| `prompt` | Added to the system prompt while the mode is on. |
| `readOnly` | `true` refuses all changes. |
| `tools` | Only these tools run; others are refused. |

A preset named `plan` replaces the built-in one.

| Command | Meaning |
|---|---|
| `/mode` | List presets and show the active one. |
| `/mode <name>` | Switch to a preset. |
| `/mode off` | Turn the active preset off. |

Tool rules apply at once. The prompt text arrives from the next step.

## Goals

A goal is an objective with completion criteria you can check. novacode keeps working on it by itself, run after run, until it's met.

```text
/goal make the test suite pass on Node 22 and Node 20
```

The model drafts one clear objective and 1–5 criteria (a test passes, a command prints X, a file has Y). It asks you only if something essential is unclear, writes its steps in the `todo` list, and starts. After each finished turn it is woken to continue.

| Command | Meaning |
|---|---|
| `/goal` or `/goal status` | Show the goal, its state and criteria. |
| `/goal <objective>` | Start a goal. |
| `/goal pause` | Stop the loop (only while the goal is active or blocked). |
| `/goal resume` | Continue a paused or blocked goal (resets the run counter). |
| `/goal clear` | Drop the goal. |
| `/goal audit on` / `off` | Turn the completion audit on or off for this goal. |

The goal stops by itself when:

- you press Esc (paused);
- two runs in a row make no tool call (paused as stalled);
- it reaches `goal.maxRuns` automatic runs (paused);
- the model calls `goal` with `block` because it needs you (blocked).

`/goal resume` continues after any of these. The goal is saved per session in `~/.novacode/goals/<session id>.json`, so it survives a restart.

### The auditor

When the model says the goal is done, an `auditor` subagent re-checks every criterion in the workspace. It reads files and runs read-only commands, and doesn't trust the model's evidence. Its first line must be `VERDICT: approved` or `VERDICT: rejected`. If rejected, the goal goes back to work with the auditor's list of what's missing. If the audit can't run, the goal stays open.

### Settings

| Key | Default | Meaning |
|---|---|---|
| `goal.audit` | `true` | Check completion with the `auditor` subagent. |
| `goal.auditModel` | the session's model | Model for the auditor, `provider/model[:level]`. |
| `goal.maxRuns` | `30` | Automatic runs before it pauses (per start or resume). |

The model drives a goal with the `goal` tool: `set` (objective, criteria, optional `ordered` to do the todo steps in order, one per run), `complete` (with evidence), `block` (with a reason) and `status`.

Goals need a window or an RPC client. They're not available in a subagent at maximum depth.

## Todo and schedule

**`todo`** is the model's task list. One call can combine these fields: `set` (replace the list), `add`, `start` (number of the item being worked on), `note`, `done` and `drop` (item numbers). The list shows in the side panel and is kept in the session.

**`schedule`** lets the model message itself later, inside the current session: `add` with `message` and `in` (`90s`, `10m`, `2h`, `1h30m`) or `at` (`HH:MM`, local time), optionally `every` to repeat (at least `1m`; `every` alone starts after one interval); `list`; `cancel` with `id`. A due message starts a turn. If the session was closed, due items fire when you resume it. There is no system cron. `schedule` is found through `tool_search`, so it costs nothing until needed.

## See also

- [Tools](tools.md)
- [MCP](mcp.md)
- [Sessions](sessions.md)
- [Security and trust](security.md)
- [Settings](config.md)
- [Packages](packages.md)
- [Migrating](migrating.md)
