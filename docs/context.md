# Context and memory

This page covers what novacode puts in the system prompt, how you add your own instructions, how prompt caching works, and how memory keeps long sessions going.

## What goes into the system prompt

The prompt is built from named sections, always in this order. A section is left out when it is empty.

| # | Section | What it holds |
|---|---|---|
| 1 | `preamble` | The built-in instructions, or your `~/.novacode/SYSTEM.md` if it exists. |
| 2 | `agent` | The role prompt of a [subagent](subagents.md), when this session is one. |
| 3 | `tool_guidelines` | One-line usage hints from the tools that are enabled. |
| 4 | `addendum` | Your `~/.novacode/APPEND_SYSTEM.md`. |
| 5 | `project_context` | Your context files (see below). |
| 6 | `skills` | A compact list of [skills](skills.md). |
| 7 | `memory` | The saved-memory index from earlier sessions. |
| 8 | `environment` | Working directory, date, git branch and HEAD, and how many files were changed at session start. |
| 9 | `mode` | The active mode preset, if any. |

`SYSTEM.md` replaces the built-in preamble. `APPEND_SYSTEM.md` adds to it. Both live in novacode's home folder (`~/.novacode`, or `$NOVACODE_HOME` if you set it). Empty files are ignored. They are read again whenever the prompt is rebuilt, so an edit shows up as a section update.

A subagent definition can either replace the preamble or add an `agent` section. See [subagents](subagents.md).

### The prompt is stable

The model gets the whole prompt once, at the start of a session. Later, if a section changes (you edit `AGENTS.md`, add a skill, switch mode), novacode appends a short message with only the changed section. It never rewrites the start of the conversation, so the provider's prompt cache stays valid.

The date and git state are written once at session start. If the date or the git branch/HEAD changes during the session, novacode adds a short hidden reminder such as `Git branch is now fix-login (HEAD a1b2c3d).`

## Context files

Context files hold instructions for your project: build commands, code style, things to avoid. novacode reads them at session start.

It looks in these folders, in this order:

1. `~/.novacode` (the global one)
2. every folder from `/` down to the folder you started novacode in

In each folder it loads **one** file, the first that exists:

1. `AGENTS.override.md`
2. `AGENTS.md`
3. `CLAUDE.md`

(`AGENTS.MD` and `CLAUDE.MD` are also accepted.) Files that are empty are skipped. The same real file is never loaded twice, even if it is reachable through a symlink or a worktree. Folders closer to your working directory come later in the prompt, so they are read last.

Each file is wrapped like this in the prompt, so the model knows where it came from:

```
<project_instructions path="/home/you/work/app/AGENTS.md">
...
</project_instructions>
```

Context files are plain text, so they load whether or not the folder is trusted. See [security](security.md) for what trust controls.

To start without any context files:

```
novacode --no-context-files
```

## Seeing where the context goes

Type `/context` in the TUI. It shows an estimated token count for the system prompt (split by section), the tools, and the messages, as a share of the model's context window. It also shows the state of the cache warmer.

## Prompt caching

Providers can reuse the start of a request instead of reading it again, which is faster and cheaper. novacode keeps the start of the request stable so this works, and it tells you when the cache missed and why.

| Setting | Default | Effect |
|---|---|---|
| `cacheRetention` | `short` | Set to `long` to ask for a longer cache lifetime: 1 hour on Anthropic, 24 hours on OpenAI. Any other value means `short`. |
| `cache.warm` | `false` | Keep the cache alive between messages by sending 1-token replays, only while that is expected to save more than it costs. |

```json
{
  "cacheRetention": "long",
  "cache.warm": true
}
```

Put this in `~/.novacode/settings.json` (or the project's `.novacode/settings.json`). The full list of settings is in [config](config.md).

When a reply reads much less from the cache than expected, novacode shows a `cache miss` notice with the likely cause: the model changed, the tool list changed, an earlier message changed, the cache expired, or the provider dropped it. A miss right after compaction is expected and is labelled that way.

## Memory

There are two kinds of memory. They solve different problems.

| | Session memory | Cross-session memory |
|---|---|---|
| Purpose | Keep one long session going after the window fills up. | Remember facts between sessions. |
| Stored in | The session file. | SQLite databases under `~/.novacode/memory/`. |
| Setting | `memory.enabled` | `memory.store` |

### Session memory and compaction

While you work, a small background "observer" reads the new part of the conversation and writes short notes: decisions, fixes, preferences. The notes are saved in the session file. They are not sent to the main model while the context still fits.

When the context gets close to full, novacode **compacts**. It keeps your most recent messages as they are and replaces everything older with a summary built from the notes (lasting facts first, then notes, then the files that were read and changed). If the notes lag behind the cut point, the observer first catches up with extra calls to the worker model (`memory.model`, else the provider's small model, else the session model). If that fails, the session model summarises the gap instead. If both fail, the older messages are dropped and the summary says "No notes were available"; the model is told to ask you if it needs what they held.

Each note has an id. The model can call the `recall` tool with a note id to get the original messages behind it.

Compaction runs by itself in three cases:

- After a turn ends, when the context has reached `memory.compactAtTokens`.
- In the middle of a turn, when the context reaches 95% of the window (only if `compactAtTokens` is below the window).
- When the provider rejects a request as too long: novacode compacts and retries once. If nothing can be compacted, you get "the context is too long and nothing could be compacted".

You can also run it by hand:

```
/compact
```

If everything already fits in the part kept as is, `/compact` says so and does nothing.

Two more background steps keep the notes small. They run in the same background pass as the observer, after it. A "reflector" turns notes into lasting facts, and a "dropper" removes notes that those facts already cover. The dropper only runs when the reflector made new facts and the active notes are over `memory.poolTargetTokens`.

| Setting | Default | Meaning |
|---|---|---|
| `memory.enabled` | `true` | Turn session memory and compaction on or off. |
| `memory.model` | the provider's small model, else the session model | Model used for the observer, reflector and dropper. |
| `memory.observeAfterTokens` | `10000` | New tokens before the observer runs. |
| `memory.keepRecentTokens` | `20000` | Tokens kept word for word after a compaction. |
| `memory.compactAtTokens` | 80% of the window, at most 400000 | Context size that triggers automatic compaction. |
| `memory.reflectAfterTokens` | `20000` | New conversation tokens since the reflector last looked, before it runs again. |
| `memory.poolTargetTokens` | `10000` | Size the dropper trims active notes down to. |

### Cross-session memory

The model has a `memory` tool with three actions:

| Action | What it does |
|---|---|
| `remember` | Save a lasting fact. Optional: `scope` (`project` or `global`), `kind`, `pin`. Saving the same text twice keeps one copy. |
| `search` | Find saved facts. |
| `forget` | Delete one by id (`p12` is a project item, `g3` is a global one). |

Project memories belong to the project (the main repo, so worktrees share them). Global memories apply everywhere. Lasting facts from the reflector are also saved as project memories.

At session start, novacode writes a short index of saved memories into the prompt. In each store, pinned items come first, then the most used, then the newest. Project items are listed before global ones. Each line is cut at 300 characters. It is written once and does not change during the session.

`search` looks in both stores; a word of one character is ignored, and any other word may match. Project items win ties.

| Setting | Default | Meaning |
|---|---|---|
| `memory.store` | `true` | Turn cross-session memory and its index on or off. |
| `memory.indexItems` | `15` | Most items in the start-of-session index. |
| `memory.indexChars` | `2000` | Character cap for that index. |

The files are `~/.novacode/memory/global.db` and `~/.novacode/memory/projects/<key>.db`. `<key>` is a 16-digit hash of the project's root folder. You can delete the files to wipe memory.

### Searching old sessions

The `session_search` tool finds words in messages of earlier sessions. It searches the current folder's sessions, or every project's with `all_projects`. It is only available when the session is saved to a file, and the model finds it through tool search rather than seeing it in every request. See [sessions](sessions.md).

### Turning memory off

```json
{
  "memory.enabled": false,
  "memory.store": false
}
```

- `memory.enabled: false` stops the observer, reflector and compaction, and removes the `recall` tool. `/compact` then says compaction is off, and a full context is no longer compacted for you. Facts from the reflector are not saved either. `recall` is also missing with `--no-session`.
- `memory.store: false` removes the `memory` tool and the start-of-session index.

You can turn off one without the other.

## See also

- [Skills and prompt templates](skills.md)
- [Sessions](sessions.md)
- [Tools](tools.md)
- [Subagents](subagents.md)
- [Config](config.md)
