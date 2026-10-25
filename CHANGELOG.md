# Changelog

All notable changes to novacode. Versions follow [semantic versioning](https://semver.org); before 1.0, a minor version can change default behaviour. Releases before 0.3.0 have generated notes on the [releases page](https://github.com/Novaturiente/novacode/releases).

## [Unreleased]

### Added

- **Usage per account.** Subscription usage is cached per login in `~/.novacode/cache/usage/<provider>-<account>.json`, fetched with that account's token, so two Claude accounts no longer share one set of numbers. `<provider>.json` is still written, for the account a session uses.
- **Which account a session is on.** The tmux pane option `@agent_account`, `account` in `novacode status --json`, the `{account}` field for `--format`, and `account` in the RPC state. They follow account rotation, not only the start.
- **`/account`** lists the provider's accounts; `/account <name>` switches to one from the next request. Rotation still moves on when that account runs out.

## [0.3.3] - 2026-10-26

### Fixed

- **Background processes stop when a headless session ends.** Commands started with the `process` tool in a `--rpc` or `--bg` session, which includes every subagent, kept running after the session closed. These sessions now stop them on exit, as the interactive window already did.

## [0.3.2] - 2026-10-26

### Fixed

- **Symlinked config files stay symlinks.** When novacode saved `settings.json` (for example after an "Always" approval), `mcp.json` (`novacode mcp add`/`remove`) or `packages.lock.json` (`novacode install`, `uninstall`, `packages update`), it replaced a symlink with a regular file, so a dotfiles repo deployed with GNU Stow silently stopped getting updates. novacode now writes through the link to its target; the `.bak` copy sits next to the target. A dangling link is still replaced, as before.

## [0.3.1] - 2026-10-26

### Fixed

- **novacode no longer gets suspended after an unsandboxed command.** A command such as `zsh -i` could take over the terminal, and novacode then stopped at the next key press (`zsh: suspended (tty input) novacode`). On Linux, unsandboxed `bash` commands and `process` starts now run in their own session (`setsid`), with no terminal, like sandboxed ones already did. A `/dev/tty` prompt (for example `sudo` asking for a password) now fails at once instead of hanging until the timeout.

## [0.3.0] - 2026-10-25

### Changed: new defaults you will notice

- **Commands run in a sandbox (Linux).** When bubblewrap (`bwrap`) is installed, `bash` commands and `process` starts run inside it (`sandbox.mode`, default `auto`). They can read your files but write only in the project, `/tmp` and tool download caches (pip, uv, npm, pnpm, yarn, bun, cargo, go, …). Credentials, token files, shell history, browser and mail profiles and the keyring are hidden, secret environment variables are removed, and Unix sockets (Docker, ssh-agent, D-Bus) and io_uring are blocked. A command that needs one of these runs with `unsandboxed: true`, which is checked more strictly. Without bubblewrap, and on macOS, commands run as before. `novacode doctor` shows the sandbox state. Settings: `sandbox.mode` (`auto`, `on`, `off`), `sandbox.allowWrite`, `sandbox.denyRead`.
- **Command judge in `auto` mode.** A small model checks risky commands (network access, inline code, computed command names, bulk deletes, paths outside the project, unsandboxed runs) and file writes outside the project against what you asked for. When it blocks one, you get an approval card; in `-p` runs the call is refused. If the judge can't be reached, the command runs and you see a notice. Pick its model with `permissions.judge`, or set it to `off`.
- **Project trust re-asks when project config changes.** Trust now records a fingerprint of the folder's config (`.novacode/` settings, extensions, skills, agents, packages and lock file, `.mcp.json` here and in parent folders, `.claude/` skills, agents and commands). When it changes, for example after a `git pull`, novacode lists what changed and asks again. Non-interactive runs ignore the changed config and print a note. Installing the project's own packages with `novacode packages install` doesn't trigger the question.
- **More file changes ask first.** The `write` and `edit` tools ask before they change novacode's `settings.json` (it holds the permission rules), your skills, extensions, agents, prompts and packages under `~/.novacode`, `~/.claude/skills`, and shell startup files such as `~/.zshrc`. They refuse credential files and protected paths, also through symlinks.
- **Update check.** At most once a day, interactive sessions check novacode's latest release and your packages' latest releases in the background, and every start shows a notice while an update is available. Nothing is installed. Turn it off with `updates.check: false`.

### Added

- **Visible compactions.** While the context is compacted, the activity line says `compacting context`. When it is done, a full-width divider (`━━ context compacted · 182k → 41k tokens · 37 notes ━━`) stays in the transcript and never folds away. A new status-line segment, `compactions`, shows `compacted ×N`. JSON and RPC clients get a `compacting` event and a `compactions` state field.
- **Codemode checks and feedback.**
  - Before a script runs, it is checked for unknown tools, MCP servers and argument names, undefined variables and Node or browser APIs. A failed check replies `Script not run:` with line numbers and the tool's declaration, and no call runs.
  - MCP calls check their arguments against the tool's schema before reaching the server.
  - Errors carry a `Hint:` line for common mistakes. Every result ends with a footer (`[3 calls · 1.2 s · stored: a, b]`), and a failed script lists the calls that already ran.
  - Images from nested calls reach the script and can be passed on with `image()`.
  - MCP `outputSchema` is kept and shown as the return type. Each server's tool signatures are cached (`~/.novacode/cache/mcp-tools/`) and listed in the next session's codemode description.
  - A refused call stops the script's tool use, including calls already running.
  - Time spent waiting on you no longer counts toward the 300 s limit. `structuredClone` exists. The store is capped at 8 MiB.
- **Prompt history across sessions.** Up and Ctrl+R also reach prompts from this folder's recent sessions (subagent sessions skipped, up to 500 prompts).
- **Ask forms use the full editor.** Free-text and note fields wrap, show a real cursor, and take paste and the editor keys. Shift+Backspace deletes to the left everywhere.
- **Eval task `codemode-traps`**, and `evals/run.sh` reports codemode runs and failures.

### Fixed

- **Risk gate.** Wrapper options and their values (`nice -n`, `timeout`, `xargs`, `env -S`, `stdbuf`, `ionice`, `sudo`, …) no longer hide the real command. `tee`, `sed -i`, `perl -i` and `awk` output count as writes. Inline code that names a credential folder asks first. Git global options and `git -c` settings that run programs are checked, and force-like pushes ask. `deny` and `ask` rules match nested commands (`sh -c`, `$(…)`, wrappers).
- **Secret masking** also catches lower-case names such as `api_key="…"`, and masked values aren't masked twice.
- **TUI.**
  - Typing a no-break or ideographic space no longer crashes the session.
  - Text wraps at word boundaries, and tabs count as 4 cells.
  - Paste can be undone, Ctrl+R reaches older matches, and parked drafts are kept.
  - The approval card ignores keys for 400 ms after it appears, and Enter never approves.
  - The `@` file list refreshes, and a UI-thread panic restores the terminal.
  - Esc or Ctrl+C puts queued follow-ups back in the editor (not in attached windows).
- **Providers.**
  - A 120 s first-response timeout. The agent loop retries 4 times (2, 4, 8, 16 s), also after an error mid-reply.
  - Google per-minute 429s are retried with the server's delay instead of being treated as a used-up quota.
  - A token refresh that fails on a network error stays on the same account; a refused refresh switches account.
  - Whitespace-only tool output is sent as `(no output)`, and broken `\u` escapes in tool arguments are repaired.
  - Memory notes cut off at the output limit count as errors instead of empty notes.
- **Subagents.** The stall watchdog counts tool progress and doesn't stop a subagent that is waiting on its own subagents or on your answer. Subagents stop when the parent ends, worktree names never repeat, and open approval cards close when a subagent stops.
- **Tools.**
  - `edit` and `write` follow symlinks and leave your own `.bak` files alone.
  - `read` refuses files over 64 MiB and hints at clipped lines.
  - `grep` skips binary files and honours `.gitignore` outside git.
  - MCP calls cancelled with Esc tell the server.
  - One extension error no longer disables the extension for the session.
- **Commands and goals.** A line like `/tmp is full` is sent as a prompt, not rejected as an unknown command. `goal.judge` set to a model now uses that model.
- **Docs.** Installed docs refresh when a page changes, not only when the version does. Two full reviews of every page against the code.
- **Packages.**
  - `packages update` never moves a package to an older release.
  - A damaged lock file is an error and is never rewritten.
  - Pre-release tags order below their release.
  - The update check falls back to tags when a GitHub repo has no release.
- **`novacode auth status`** exits with code 1 when only keyless local servers (`ollama`, `lmstudio`) are listed.
- **Print mode** notes on stderr when the reply was cut at the output token limit or had no text.

[0.3.3]: https://github.com/Novaturiente/novacode/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/Novaturiente/novacode/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/Novaturiente/novacode/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Novaturiente/novacode/compare/v0.2.6...v0.3.0
