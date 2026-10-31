# Changelog

All notable changes to novacode. Versions follow [semantic versioning](https://semver.org); before 1.0, a minor version can change default behaviour. Releases before 0.3.0 have generated notes on the [releases page](https://github.com/Novaturiente/novacode/releases).

## [0.6.6] - 2026-10-31

### Fixed

- **Worktrees no longer load the main checkout's `AGENTS.md`/`CLAUDE.md` too.** Inside a git worktree nested in its main checkout (such as `repo/.worktrees/feat`), only the worktree's own copy loads. Before, both copies went into the system prompt, doubling it and mixing two versions of the rules. Folders above the repo still load. See [context](docs/context.md).
- **Subagent reports are written in plain, complete sentences.** The old "terse fragments" style spread into the parent's own messages in long sessions, until words ran together without spaces.

## [0.6.5] - 2026-10-29

### Added

- **`novacode restore`** lists sessions whose process died mid-run (crash, kill, power loss) by name, id, age and folder, and resumes the one you pick. A subagent shows the title its parent gave it. See [sessions](docs/sessions.md).

### Changed

- **Startup shows one line** for interrupted sessions (`2 sessions were cut off mid-run: novacode restore`) instead of one line per session.

## [0.6.4] - 2026-10-29

### Added

- **Sessions name themselves.** After the first reply, the observer's small model gives an interactive session a short name, and `/resume` shows it instead of the first prompt. After each compaction it renames the session if the work moved on. It is a separate request, so the session model's cache is untouched. `/name` pins your own name. Not for `-p`, `--rpc` or `memory.enabled: false`. See [sessions](docs/sessions.md).
- **The name is visible outside:** tmux pane option `@agent_name`, `name` in `get_state` and the run file, `novacode status --format '{name}'`, and a `session_named` event.

### Fixed

- **Todo lists no longer freeze** when the work changes track: an open list the model hasn't touched for 10 replies, or that was compacted away, is shown to it again with a request to update it. A resumed session keeps its list past a compaction.

## [0.6.3] - 2026-10-29

0.6.2 was tagged but never published (a release build failed); its changes ship in this release too.

### Added

- **`"raw": true` for RPC `prompt`** sends the text to the model as written: no `!cmd`, slash command, skill, template, extension input or `@image`. Use it for generated prompts. See [automation](docs/automation.md).
- **Model loops are stopped.** A reply whose text or thinking keeps repeating the same block is cancelled and asked for again.

### Changed

- **`maxTokens` is the room for the answer.** Reasoning models get 16384 more for thinking on top (every provider counts thinking against the cap), clamped to the model's limit. Before, thinking could use up the whole 16384 and cut long answers off.
- **A cut-off reply is continued, not restarted:** the nudge quotes where the reply stopped and asks for the next character on.
- **Print mode exits `3`** when the reply is still cut off after asking the model to continue (was `0`). The TUI shows an error in that case.
- **Every agent-level retry sends `retry_rollback`**, so clients drop the text the failed attempt streamed.
- **Output cap keeps 4096 tokens spare** below the room left in the context window.
- **Folded turns keep every message** and fold each run of tool calls and notes on its own line. Notes from schedules, subagents and cache misses fold too, also when they arrive after the turn ended.
- **An inlined skill shows as `/name` plus your text** in the transcript, prompt history, session list and `/tree`, not as the whole skill.

### Fixed

- **Windows:** saving a session failed with `Access is denied.` (since the fsync change in 0.6.2).

## [0.6.2] - 2026-10-29

### Added

- **Skills as `/name`.** Type `/release-notes` instead of `/skill:release-notes`; `/skill:name` still works. Built-in, mode and extension commands and templates of the same name win; such a skill is completed as `/skill:name`. See [skills](docs/skills.md#using-a-skill-yourself).
- **Interrupted sessions.** When novacode dies mid-run (crash, kill, power loss), the session is listed at the next start and in `novacode agents` with its `novacode -r <id>` command, for 7 days or until resumed. A resumed one says it was cut off.

### Changed

- **Folded turns keep every message.** A finished turn shows all of the agent's messages, not only the last; each run of tool calls and notes between them folds to its own line.
- **Session saves are fsynced**, so a power loss keeps every saved entry.
- **A tool call cut off by a crash** tells the model it may have partly run (was: `No result provided`).

## [0.6.1] - 2026-10-27

### Changed

- **Lenient by default.** New setting `permissions.security`: `lenient` (the new default) or `hardened` (how 0.6.0 behaved). At `lenient`, `auto` mode almost never asks: no command judge, and Shared commands and most config-file writes run, logged. Catastrophic commands, file writes into credentials, and your own rules still stop, and changes to novacode's `settings.json`, `extensions`, `packages` and `mcp.json` still ask. On Linux with bubblewrap, commands run in a credentials-only sandbox: everything is writable except credential folders, token files and `~/.novacode` (read-only, still readable). A `permissions` value that doesn't parse, or a settings file with a syntax error, now means `hardened`. To keep the old behaviour, set `"permissions": {"security": "hardened"}`. `novacode doctor` shows the level. See [security levels](docs/security.md#security-levels).
- **Hardened is less noisy.** The command judge sees your `ask_user` answers. An `a` on a judge card for `git push` to a named remote or `ssh` to a host also lets the same push or host past the judge for the rest of the session, and the card says so.

## [0.6.0] - 2026-10-27

### Added

- **Windows x86_64 (experimental).** A release `.zip` for `x86_64-pc-windows-msvc` and `scripts/install.ps1` (`irm …/install.ps1 | iex`). Commands run in Git Bash, which must be installed (Git for Windows; `NOVACODE_BASH` picks another `bash.exe`). There is no command sandbox on Windows: `sandbox.mode` `auto` warns once and runs commands unsandboxed, `on` refuses them; the approval policy still applies. Process trees are stopped with Job Objects, live sessions talk over named pipes, `novacode update` replaces the running `.exe`, and the clipboard paste reads Windows' clipboard. Tested in CI and by hand on Windows 11. See [install](docs/install.md#windows).
- **`NOVACODE_VERSION`** for `install.ps1` installs a given tag.

### Changed

- **The risk gate reads `\` inside `"…"` as bash does:** it is kept unless it escapes `$`, `` ` ``, `"`, `\` or a newline. Before, every backslash there was dropped, so `"C:\Users"` was read as `C:Users`.
- **On Windows, protected paths match whatever the spelling:** case, `/` or `\`, trailing dots and spaces, `name:stream`, `\\?\` and `\\.\` prefixes, admin shares (`\\host\C$`), Git Bash `/c/…`, drive-relative `C:…`, 8.3 short names and `$USERPROFILE`/`$APPDATA`-style variables. `rm.exe`, `RM` and `C:\…\rm.exe` count as `rm`; `cmd`, `powershell` and `pwsh` ask before running. Permission rules match without regard to case on Windows, and `\` in a rule is a folder separator there.
- **Release tags with a `-` (`v0.6.1-rc.1`) publish as pre-releases**, so `novacode update` and the install scripts never pick one up.

### Known issues

- On Windows, capturing `novacode --bg`'s output in PowerShell (`$id = novacode --bg …`) waits until the background session ends. Use `cmd /c 'novacode --bg "…" > id.txt'` (see [automation](docs/automation.md#background-sessions---bg)).

## [0.5.0] - 2026-10-26

### Added

- **`/websearch`** picks the web search engine tried first (`auto`, `antigravity`, `gemini`, `brave`, `searxng`); `/websearch <engine>` sets it directly. Saved as the setting `web.search`, read on every search. The other engines stay as fallbacks, and an answer from a fallback says which engine failed first.

### Changed

- **`/account` opens a picker** of the current provider's accounts, with the active one marked. The pick is saved per provider in `~/.novacode/selected.json`: new sessions start on it, and Antigravity web search and `/usage` use it instead of always `default`. Automatic rotation still moves on when an account runs out, without changing the saved pick.

### Fixed

- **Commands the host answers itself no longer leave a stuck prompt box.** `/account`, `/mode`, `/goal ...` and `/usage` left their own text shown as a pending prompt below the reply until the next turn ended.

## [0.4.0] - 2026-10-26

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

[0.6.5]: https://github.com/Novaturiente/novacode/compare/v0.6.4...v0.6.5
[0.6.4]: https://github.com/Novaturiente/novacode/compare/v0.6.3...v0.6.4
[0.6.3]: https://github.com/Novaturiente/novacode/compare/v0.6.1...v0.6.3
[0.6.2]: https://github.com/Novaturiente/novacode/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/Novaturiente/novacode/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/Novaturiente/novacode/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/Novaturiente/novacode/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Novaturiente/novacode/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/Novaturiente/novacode/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/Novaturiente/novacode/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/Novaturiente/novacode/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Novaturiente/novacode/compare/v0.2.6...v0.3.0
