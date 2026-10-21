<p align="center"><img src="logos/export/star/logo.svg" alt="novacode" width="480"></p>

<p align="center">A coding agent for your terminal, in one small Rust binary.</p>

novacode reads and edits your code, runs commands, and works through tasks with you, like Claude Code, pi or Codex. It uses the subscriptions you already pay for (Claude Pro/Max, ChatGPT/Codex, Antigravity, GitHub Copilot) or an API key for about 55 providers, including local models. Your existing Claude Code skills, agents and commands load without moving any files, and Claude Code plugins (ponytail, caveman, mattpocock/skills, …) install with `novacode install <repo>`, hooks included.

> **Status:** early release (0.2). Linux is the main platform. macOS builds exist but are not tested yet. Windows is not supported; use WSL. Release binaries and AUR packages arrive with the first release; until then, build from source (below and in [docs/install.md](docs/install.md)).

## Install

Once the first release is out:

```sh
curl -fsSL https://raw.githubusercontent.com/Novaturiente/novacode/main/scripts/install.sh | sh
```

Today, build from source: `cargo install --locked --git https://github.com/Novaturiente/novacode novacode` (needs Rust 1.97+, a C compiler and git). All the ways to install (AUR, building from a checkout) are in [docs/install.md](docs/install.md).

## First run

```sh
cd your-project
novacode
```

The first start asks you to log in, pick a model, and decide whether to trust this folder's project config. Then type what you want done.

```sh
novacode "fix the failing test"        # start with a prompt
novacode -c                            # continue the newest session in this folder
novacode -p "summarise src/lib.rs"     # print mode: answer on stdout, then exit
novacode -m codex/gpt-5.5:high    # pick a model: provider/model[:thinking]
```

The full walk-through and a list of every flag and subcommand are in [docs/quickstart.md](docs/quickstart.md).

## What it does

| Area | What you get |
|---|---|
| **Models** | Subscription logins and API keys, several accounts per provider that rotate when one runs out, custom OpenAI-compatible endpoints and local models, custom providers as a program you write. |
| **Tools** | Read, write, edit, patch, bash, grep, find, a background process manager, web search and fetch, todos, questions to you, and a JavaScript tool (codemode) that chains many tool calls in one step. |
| **Safety** | Permission modes (auto, ask, read-only), a command risk check that asks before force pushes and refuses destructive commands, allow/ask/deny rules, project trust, optional secret masking. |
| **Sessions** | Every session is a tree: resume, branch, fork, rewind the conversation and your files, export to JSONL or HTML. Sessions can run in the background and be attached from another terminal. |
| **Context** | `AGENTS.md`/`CLAUDE.md` files, Agent Skills, prompt templates, notes-based compaction, and memory that carries across sessions. |
| **Subagents** | Background helper sessions (scout, worker, reviewer, or your own) that see only the tools they're given, plus plan mode and `/goal` (keep working until an auditor agrees it's done). |
| **Extending** | MCP servers, TypeScript/JavaScript extensions with no build step, extensions in any language over JSON-RPC, command hooks, and git packages that bundle all of these. Claude Code plugins install as packages (skills, commands, agents, MCP servers, hooks, options), and packages update to their latest release. |
| **Automation** | Print mode, a JSON event stream, and an RPC protocol for driving novacode from scripts, editors and CI. |

## How it compares

| | novacode | pi | Claude Code |
|---|---|---|---|
| Runtime | One native binary | Node.js | Node.js |
| Open source | Yes (MIT or Apache-2.0) | Yes | No |
| Several model providers | Yes | Yes | Claude only |
| Loads Claude Code skills, agents, commands | Yes, as they are | Partly | Yes |
| Installs Claude Code plugins (with hooks) | Yes | No | Yes |
| Subagents, plan mode, MCP | Built in | Extensions | Built in |
| Command risk check and approvals | Built in | Extension | Built in |
| Extensions | TS/JS (no npm) or any language | TS with npm | Plugins and hooks |
| Windows | No (WSL works) | Yes | Yes |

## Documentation

**Start here:** [Install](docs/install.md) · [Quickstart](docs/quickstart.md) · [Providers and login](docs/providers.md) · [Coming from Claude Code, pi or Codex](docs/migrating.md)

**Using it:** [The terminal UI](docs/tui.md) · [Sessions](docs/sessions.md) · [Tools](docs/tools.md) · [Context and memory](docs/context.md) · [Skills and prompt templates](docs/skills.md) · [Subagents, plan mode and goals](docs/subagents.md) · [Security and permissions](docs/security.md)

**Extending it:** [MCP servers](docs/mcp.md) · [Extensions](docs/extensions.md) · [Packages](docs/packages.md) · [Scripting and automation](docs/automation.md)

**Reference:** [Settings](docs/config.md) · [Troubleshooting](docs/troubleshooting.md)

## Files

| Path | What |
|---|---|
| `~/.novacode/settings.json` | Your settings ([reference](docs/config.md)). `.novacode/settings.json` in a trusted project adds to it. |
| `~/.novacode/auth.json` | Logins and keys (only you can read it). |
| `~/.novacode/sessions/` | Your sessions. |
| `~/.novacode/logs/` | Logs and crash logs. |

Set `NOVACODE_HOME` to use a different folder than `~/.novacode`. The full list of files is in [docs/install.md](docs/install.md).

## Reporting a bug

Run `novacode debug bundle`. It writes a `novacode-debug-<time>.tar.gz` with logs, settings and the newest session from this folder, with secrets masked. Attach it to an issue at <https://github.com/Novaturiente/novacode/issues>. See [Troubleshooting](docs/troubleshooting.md) for more.

## Developing

```sh
cargo build --release -p novacode     # target/release/novacode
scripts/check.sh                      # format, clippy, tests
UPDATE_SNAPSHOTS=1 cargo test         # accept UI, protocol and docs snapshots
```

`docs/config.md` is generated from `crates/novacode-base/src/settings_doc.rs`; edit the table there.

## License

MIT or Apache-2.0, at your option.
