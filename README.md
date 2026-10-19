# novacode

A coding agent for the terminal, written in Rust. It is one fast binary that works with your subscriptions (Claude Pro/Max, ChatGPT/Codex, Antigravity, GitHub Copilot) and with about 50 API-key providers. It is built to keep prompt-cache hits high and to stay out of your way.

The design and the reasons behind it live in `Progress/` (`ARCHITECTURE.md`, `DECISIONS.md`, one document per layer).

## Install

```sh
cargo build --release -p novacode          # target/release/novacode (needs glibc ≥ the build machine's)
scripts/build-musl.sh                      # static binary: target/x86_64-unknown-linux-musl/release/novacode
scripts/build-musl.sh myserver             # …and copy it to ~/.local/bin on another machine over ssh
```

The static build needs the Rust musl std (`rustup target add x86_64-unknown-linux-musl`) and `musl-gcc` (or `MUSL_CC`).

## First run

```sh
novacode                     # login picker, then a model, then the trust prompt for this folder
novacode login claude        # or codex, antigravity, copilot; or any API-key provider, e.g. openrouter
novacode -m codex/gpt-5.6-luna:high
novacode -c                  # continue the newest session here (-r ID for another)
novacode -p "fix the failing test"   # print mode: only the final answer on stdout
```

A model is `provider/model[:thinking]`. `novacode models` lists what your logins and keys can use.

Coming from Claude Code? Your skills, agents and commands in `~/.claude` load as they are. Run `novacode import claude` once to copy your hooks, MCP servers, permission rules and model. It shows the changes and asks before writing anything.

## In the TUI

| Key or command | What it does |
|---|---|
| Enter / Alt+Enter | Send; while it works, Enter steers and Alt+Enter queues a follow-up |
| Esc, Ctrl+C | Stop the turn; Esc Esc clears the editor |
| Ctrl+L, `/model` · Shift+Tab, `/thinking` | Switch model · thinking level |
| Alt+O · Ctrl+O · Ctrl+T | Fold turns · full tool output · show thinking |
| Ctrl+Alt+P · Ctrl+Alt+F | Side panel (todos, subagents, processes, usage) · focus it |
| `@path`, Ctrl+V | Attach a file or image · paste a clipboard image |
| `!cmd` / `!!cmd` | Run a shell command; its output goes to the model / stays out |
| `/plan`, `/mode` | Read-only plan mode, mode presets |
| `/rewind`, `/tree`, `/fork`, `/clone` | Go back (files, conversation or both), branch |
| `/context`, `/usage`, `/debug` | Token cost per prompt section · usage and quota windows · state for bug reports |
| `/compact`, `/resume`, `/new`, `/export`, `/ps`, `/mcp`, `/reload`, `/restart` | … |
| Alt+/ | Every key binding |

Key bindings use pi's names in `~/.novacode/keybindings.json`.

## Files

| Path | What |
|---|---|
| `~/.novacode/settings.json`, `.novacode/settings.json` | Settings, global and per project (see [`docs/config.md`](docs/config.md)) |
| `~/.novacode/auth.json` | Logins and keys (0600) |
| `~/.novacode/AGENTS.md`, `AGENTS.md`/`CLAUDE.md` up the tree | Instructions for the model |
| `~/.novacode/skills`, `~/.agents/skills`, `.novacode/skills` | Agent Skills |
| `~/.novacode/agents`, `.novacode/agents` | Subagent definitions (scout, worker and reviewer are built in) |
| `~/.novacode/extensions`, `.novacode/extensions` | Script extensions (see [`docs/extensions.md`](docs/extensions.md)) |
| `~/.novacode/mcp.json`, `.mcp.json` | MCP servers (`novacode mcp add`) |
| `~/.novacode/sessions/` | Sessions (JSONL trees) |
| `~/.novacode/logs/` | Logs, crash logs, cache-miss traces |

Project settings, extensions, skills and MCP servers load only in folders you trust (`novacode trust`).

## Commands

`login`, `logout`, `auth status`, `models`, `doctor`, `export`, `attach`, `panel`, `agents`, `stop`, `status`, `debug bundle`, `stats`, `install`/`uninstall`/`update`/`packages`, `mcp`, `import claude`, `trust`. Use `novacode <command> --help` for details.

## Reporting a bug

`novacode debug bundle` writes `novacode-debug-<time>.tar.gz` with logs, crash logs, settings and the newest session here, secrets masked. `--profile-startup` prints where startup time goes.

## Developing

```sh
scripts/check.sh                                   # fmt, clippy, tests
UPDATE_SNAPSHOTS=1 cargo test                      # accept TUI, wire and docs snapshots
evals/run.sh claude/claude-opus-5-5:low            # real tasks against a real model (costs tokens)
```

`docs/config.md` is generated from `crates/novacode-base/src/settings_doc.rs`. Debug builds panic when code reads a setting that isn't listed there.
