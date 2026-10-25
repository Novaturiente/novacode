# Security

This page explains what novacode does and does not protect you from: permission modes, the command risk gate, the sandbox, permission rules, project trust, and how secrets are handled.

## The threat model

Commands the agent runs with `bash` run as you. On Linux with bubblewrap installed, they run in a [sandbox](#sandbox): they can read your files but write only in the project and a few other places, they can't reach desktop services or Docker, and your credentials and secret environment variables are hidden. Without bubblewrap (and on macOS) they run with your files, your network and your credentials. The protections below are guard rails that catch mistakes. They are not a wall against a determined attacker or a hostile model.

If you need real isolation, run novacode inside a container or VM.

What the guard rails do:

- A permission policy checks every tool call before it runs (including calls made from `codemode` scripts).
- A command risk gate looks at each `bash` command and holds or refuses destructive ones.
- Project config that can run code only loads from folders you trust.
- Your own logins are masked in tool output before the model sees it.

What they do not do: stop a command the gate does not recognise, or stop code that a program you allowed runs on its own.

**Extensions are trusted code.** Tools an [extension](extensions.md) adds go through the rules below like any tool. But the extension's own actions are not held to them. Its `exec` call only refuses Catastrophic commands: no approval cards, no `allow`/`deny` rules, no read-only mode, and your `protected` lists do not apply. Its file read and write calls are not checked at all, and absolute paths work. Only install extensions you trust.

## Permission modes

| Mode | What happens |
|---|---|
| `auto` (default) | Everything runs, except what the risk gate or your rules stop. |
| `ask` | You approve every file change (`write`, `edit`) and every `bash` command that is not read-only. |
| `read-only` | No file changes. `bash` and `process` start only run read-only commands such as `ls`, `cat`, `grep`, `git status`. MCP, extension and client tools that don't declare themselves read-only need your approval. |

Set the mode for one run:

```sh
novacode --mode ask
novacode --mode read-only
```

Or set it for good in `~/.novacode/settings.json`:

```json
{
  "permissions": { "mode": "ask" }
}
```

Plan mode (a built-in mode preset) is read-only too. Your own `modes.<name>` presets can also set `readOnly: true` or limit the tools a mode may use. Switch with `/plan` and `/mode`; see [TUI](tui.md).

Use `--api-key` to give a key for one run only. It is masked like your saved logins.

## The command risk gate

Every `bash` command is split into simple commands (including ones inside `$(...)`, backticks, `sh -c '...'` and `sudo`) and classified. What it cannot parse is treated as risky, not waved through.

| Level | Meaning | Examples |
|---|---|---|
| Safe | Reads only. Allowed in read-only mode. | `ls`, `git status` |
| Low | Changes something ordinary. | writing a file, running a program |
| Confirm | Destroys data or history in bulk. | `rm -rf target`, `git reset --hard`, `git clean -fd`, `git branch -D`, `find -delete`, `curl ... \| sh`, `sudo ...`, `dd of=<file>`, `chmod -R` |
| Shared | Rewrites or deletes history on a remote, which no local undo can fix. Also inline code that names a credential folder or protected path: novacode can't see what it does. | `python3 -c "open('~/.ssh/...')"`, `git push --force` (and `--force-with-lease`, `--force-if-includes`, `-uf`), `git push origin +main`, `git push --delete`, `git push --mirror`, `git push --prune` |
| Catastrophic | Targets the system, your home folder or credentials. **Never runs.** | `rm -rf /`, `rm -rf ~`, writing to `/dev/sda`, `mkfs` on a device, `kill -1` (as the last argument), `pkill -f` with a pattern that matches its own command line, fork bombs, a redirect (`>`), `tee`, `sed -i`, `perl -i`, `awk -i inplace`, an `awk` `print > "file"`, `rm`, `cp`, `mv`, `ln`, `install`, `rsync`, `dd of=`, `chmod`/`chown`, `truncate` or `shred` aimed at `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.config/gcloud`, `~/.password-store` or `~/.novacode` |

Only the commands named in that row are checked against these folders. So `tee -a ~/.ssh/authorized_keys` and `sed -i ... ~/.novacode/settings.json` never run.

Code the gate can't read is checked by name only. Inline code (`python -c`, `node -e`/`--eval`/`-p`, `deno eval`, `bun -e`, `perl -e`, `ruby -e`, `php -r`), or a heredoc or here-string fed to one of these, that names a credential folder or one of your protected paths is Shared: it asks you. A name counts as `~/.ssh`, `$HOME/.ssh`, `${HOME}/.ssh`, the full path, or `HOME` followed by `/.ssh` (as in `process.env.HOME + '/.ssh'`). It is not refused, because reading a public key there can be fine.

What the gate can't see still runs: a script on disk (`python3 fix.py`), a path built another way inside the code, or any other program that writes files. That is what the sandbox is for.

System folders count too: `/`, `/bin`, `/boot`, `/dev`, `/etc`, `/home`, `/lib`, `/lib64`, `/opt`, `/proc`, `/root`, `/sbin`, `/srv`, `/sys`, `/usr`, `/var`, and your home folder itself.

More commands that count as Confirm: `git checkout -- .` and `git checkout -f`, `git restore` (unless only `--staged`), `git stash drop` and `clear`, `git reflog expire` and `delete`, `git filter-branch`, `git worktree remove -f`, `truncate`, `shred`, `wipefs`, `mkfs`, `fdisk`, `sfdisk`, `parted`, `gdisk`, `mkswap` and `blkdiscard` (when the target is not a device or protected), `shutdown`, `reboot`, `poweroff`, `halt`, `systemctl poweroff`/`reboot`/`halt`/`suspend`, `crontab -r`, `docker`/`podman` prune or `volume rm`, and `find -exec rm`. `truncate`, `shred`, `wipefs`, `mkfs`, `fdisk`, `sfdisk`, `parted`, `gdisk`, `mkswap`, `blkdiscard`, `chmod`, `rmdir` or `dd of=` aimed at a device such as `/dev/sda` is Catastrophic. A bare `dd` without `of=` is Low. A command it cannot parse, or one nested more than 4 levels deep, is Confirm too. `git push origin :branch` is Shared, like `--delete`.

How the gate reads a command:

- **Wrappers and their options.** `nohup`, `time`, `nice`, `ionice`, `command`, `exec`, `builtin`, `unbuffer`, `stdbuf`, `timeout`, `xargs`, `env` and `sudo`/`doas`/`pkexec` are skipped together with their options (and option values, such as `nice -n 10` or `timeout -s KILL 60`), and the command they run is judged. `env -S '...'` judges the string, plus the words after it (env appends them), as a command: `env -S rm -rf ~` is Catastrophic.
- **Runners.** The command that `watch`, `flock`, `setsid`, `nsenter`, `unshare`, `script -c`, `su -c`, `busybox`, `systemd-run`, `parallel`, `find -exec`/`-execdir`/`-ok` and `git submodule foreach` run is judged too: `setsid rm -rf ~` and `find . -exec rm -rf ~ \;` are Catastrophic.
- **Shells.** `sh -c` finds its script after any options (`bash -ce '...'`, `bash -c -- '...'`, `bash -o pipefail -c '...'`). A heredoc or here-string fed to a shell with no script file (`sh <<'EOF'`, `bash -s <<EOF`, `bash <<< '...'`) is judged as commands. Shell keywords (`if`, `then`, `while`, `do`, `!`, …) don't hide the command after them.
- **Read-only commands that can write or run things** are Low, not Safe: `git -c <key>=...` and `--config-env` (except `color.*`, `user.*`, `advice.*`, `core.quotepath`, `safe.directory`), `--exec-path`, `git diff --output`, `--ext-diff`, `git grep -O`, `--upload-pack`, `ext::` URLs, a bare `git stash`, and a leading `GIT_*`, `LD_*`, `PAGER`, `LESSOPEN`, `LESSCLOSE`, `BASH_ENV`, `ENV`, `PATH`, `EDITOR` or `VISUAL` assignment. The same goes for `sed` with `-i` (its files are checked like a redirect), `-f` or a `w`/`W`/`e` command; `awk` with `system()`, `getline`, pipes, `print > file`, `-f`/`-i`/`-E` or `--file`/`--include`/`--exec`; `fd -x`/`-X`/`--exec` (also inside a cluster such as `-Hx`); `find -fprint`; `sort -o` and `--compress-program`; `tree -o`; `uniq`/`xxd` with an output file; `rg --pre`; `ss -K`; `date -s`/`--set` (`date -Iseconds` stays Safe).
- **Copying into your home folder is allowed.** For `cp`, `ln`, `install` and `mv` only the destination counts: `cp file.txt ~/` and `cp ~/.ssh/id_ed25519.pub .` are Low. Replacing the home folder itself (`cp -T dir ~`) or writing under a credential folder (`cp k ~/.ssh/authorized_keys`) is Catastrophic. So is copying a source whose name lands on a protected path inside home, such as `cp -r payload/.ssh ~` or `rsync -a dir/.ssh ~/`, because it merges into `~/.ssh`. `mv` also counts its sources, so `mv ~/.gnupg /tmp` is Catastrophic. `rsync` into your home or a system folder is Catastrophic only with `--delete`; with `--remove-source-files` its sources count too. An rsync source ending in `/` copies its contents, which the gate cannot see: `rsync -a src/ ~/` is Low unless `src/` itself is protected.

What each level does depends on the mode:

- **Catastrophic**: refused in every mode. The model is told to do it another way or ask you to run it yourself.
- **Shared**: asks you, even in `auto` mode; refused in `read-only` mode. Without anyone to ask, it needs a `justification`.
- **Confirm**: asks you in `ask` mode. In `auto` mode it runs without asking (it is logged). In `read-only` mode (and plan mode) it is refused without asking: read-only means no changes.
- **Safe / Low**: no gate action.

You can add folders to the protected set and ban commands outright:

```json
{
  "permissions": {
    "protected": {
      "commands": ["sudo"],
      "paths": ["~/work/secrets"]
    }
  }
}
```

A protected command never runs. It matches the command name (so `/usr/bin/sudo` too) and any other word of the command (so `env sudo ls`). Protected paths are treated like credentials: destroying or overwriting them is Catastrophic.

The file tools follow the same list. `write`, `edit` and `apply_patch` are refused, in every mode and past any `allow` rule, when the file is in a credential folder, a protected path, or is your home or a system folder itself. `~/.novacode` is the exception: there the file tools refuse only your logins and trust decisions (`auth.json`, `auth-*.json`, `mcp-auth.json`, `trust.json`), so the agent can still edit `~/.novacode/AGENTS.md` and skills. A change to `settings.json` (in `~/.novacode` or a project's `.novacode`) always asks you first, past any `allow` rule, because it holds the permission rules; with nobody to ask it is refused. This ask covers the file tools only. Shell commands treat the whole `~/.novacode` folder as credentials, so a redirect or `cp` onto `~/.novacode/settings.json` is refused, but `sed -i`, `tee` or an interpreter editing it runs with no ask in `auto` mode. A project's `.novacode/settings.json` is not protected from shell commands at all. The path is checked as given and with symlinks resolved, so a project link into `~/.ssh` is refused too. Reads are not affected. Path rules such as `Read(...)` also match the resolved path. A `deny` or `ask` rule matches when either path does. An `allow` rule needs both: a link inside an allowed folder that points outside it is not auto-allowed, and the approval card says where the link resolves to.

When a tool writes a file, it writes through a symlink to its target (the link stays a link), keeps the file's mode, and never creates or deletes a `<file>.bak` next to it: a `.bak` file of your own is left alone.

### When nobody can approve

In print mode (`-p`) and other runs with no one to answer a question, a held command cannot be approved. novacode gives the model one way forward: call `bash` again with the same command and a `justification` field (at least 12 characters) that says why it is safe. If the justification is there, the command runs and the reason is logged. Without it, the command is refused. This only works for commands held by the risk gate (Confirm and Shared). It does not work for a call held by an `ask` rule or by `ask` mode.

## Command judge

The risk gate reads command text. It can't tell whether a `curl` or a `git push` is what you asked for. In `auto` mode a small model, the **command judge**, checks the risky commands the gate lets through.

It looks at `bash` commands and `process` starts that:

- reach another machine or run remote code: `curl`, `wget`, `ssh`, `scp`, `sftp`, `nc`, `ncat`, `socat`, `telnet`, `xh`, `http`/`https` (HTTPie), `openssl s_client`, `rsync` to or from `host:path`, `git push`, `gh`/`glab` commands that change something, `npm`/`cargo`/`twine`/`gem` publish, `docker`/`podman` push or login, `npx`, `pnpx`, `bunx`, `uvx`, and an `awk` program that calls `system()` or pipes to a command;
- run inline code (`python -c`, `node -e`, a heredoc fed to an interpreter, and the rest of the gate's list);
- have a computed command name (`$CMD`, `$(...)`, backticks);
- are Confirm level (bulk deletes that `auto` mode runs without asking);
- name a path outside the project and `/tmp`, as an argument or as a redirect target (`echo x >> ~/.bashrc`, `> /dev/tcp/...`; `> /dev/null` and `2>&1` don't count);
- are a `process` start whose `cwd` is outside the project and `/tmp`;
- run with `unsandboxed: true`.

It sees through wrappers, runners, `sh -c`, `$(...)` and shell keywords the same way the gate does. Builds, tests and read-only commands never go to it. Nor does a command your own `allow` rules or this session's approvals cover. An `allow` rule must cover every command inside the line too: `Bash(git push:*)` does not cover `git push $(curl x)`. In `ask` and `read-only` mode it is off: you already decide there. An `unsandboxed: true` call asks you in `ask` mode, even for a read-only command, and is refused in `read-only` and plan mode, because outside the sandbox even `cat` can read what the sandbox hides.

The judge sees your messages of this session and the agent's recent tool calls (name and arguments, without the agent's `intent` and `justification`). The command itself is given as a quoted JSON string. It never sees the agent's replies or any tool output, so neither the agent nor text in a file can talk it round. In a subagent, the "user" messages it sees are the task the parent agent wrote, not your own words. It answers `ALLOW` or `BLOCK` with a reason, in any case and with or without markdown around it.

- **Block, with you there:** an approval card shows the judge's reason. `y` runs it; `a` allows that exact command for the session.
- **Block, nobody there** (print mode): the command is refused, and the model is told why.
- **An answer that is neither ALLOW nor BLOCK, or empty:** counts as a block ("the judge gave no clear answer").
- **A command over 4000 characters:** blocked without asking the model ("too long to judge").
- **The judge can't be reached or takes more than 20 s:** the gate's verdict stands and the command runs. You see a short notice, and the log has the error.

Esc while it thinks cancels the call. Pick its model with `permissions.judge`: unset uses the observer's model (the provider's small model), `off` turns it off, or name a `provider/model[:level]`. Like all of `permissions`, this is global only. Its calls are logged but not counted in `/usage`.

## Sandbox

On Linux, the commands the model runs (the `bash` tool and `process` starts) run inside [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`). Install it with your package manager (`bubblewrap`). It adds about 10 ms per command.

Inside the sandbox:

- The whole file system is **read-only**.
- These places are **writable**: the project (the git top level of the session folder, or the folder itself outside git; for a git worktree also the repository's shared `.git` folder), `/tmp`, `$TMPDIR`, the tool caches that exist (`~/.cache`, `~/.cargo`, `~/.rustup`, `~/.npm`, `~/.bun`, `~/go`) and the folders in `sandbox.allowWrite`.
- These stay **read-only** even inside a writable place, because a program novacode or you run later outside the sandbox would execute them: the git `config`, `config.worktree` and `hooks` of the project's git folder (and of a worktree's shared one), the project's `.novacode/`, `.claude/` and `.mcp.json`, and the tool binaries and config `~/.cargo/bin`, `~/.cargo/config`, `~/.cargo/config.toml`, `~/.cargo/env`, `~/.rustup`, `~/.bun/bin` and `~/go/bin`.
- These are **hidden** (an empty folder or an empty file): the credential folders the risk gate protects (`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.config/gcloud`, `~/.password-store`), novacode's logins and trust list (`auth.json`, `auth-*.json`, `mcp-auth.json`, `trust.json` in `~/.novacode`), token files (`~/.git-credentials`, `~/.netrc`, `~/.cargo/credentials`, `~/.cargo/credentials.toml`, `~/.docker/config.json`, `~/.config/gh/hosts.yml`, `~/.pypirc`, `~/.npmrc`, `~/.claude/.credentials.json`, `~/.codex/auth.json`, `~/.config/hub`, `~/.azure`, `~/.terraform.d/credentials.tfrc.json`, `~/.pgpass`, `~/.vault-token`, `~/.config/rclone`), your `permissions.protected.paths` and the paths in `sandbox.denyRead`. They stay hidden even inside a writable folder.
- **Unix sockets are blocked.** A seccomp filter makes creating one fail with `Operation not permitted`. Through such sockets a command could ask a program outside to act for it: D-Bus and systemd can start a command outside the sandbox, Docker can mount your home folder as root, and agents such as ssh-agent and gpg-agent sign with your keys. The filter also blocks typing into the terminal (`TIOCSTI`). Network sockets and `socketpair` still work. The filter exists for x86_64 and aarch64; on another CPU the sandbox runs without it and logs a warning.
- **Secret environment variables are dropped:** the key variables of every provider, any name containing `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `API_KEY`, `APIKEY`, `ACCESS_KEY`, `PRIVATE_KEY` or `CREDENTIAL` (in any case), and `SSH_AUTH_SOCK`, `GPG_AGENT_INFO`, `DBUS_SESSION_BUS_ADDRESS` and `DOCKER_HOST`.
- The network stays on. The working folder is the same as outside.
- Timeouts, Esc and `process stop` still end everything the command started.

The layout is worked out when the session starts; a path that doesn't exist then is skipped. A path deleted later is skipped from then on, so commands keep working and the path is not created again.

**What needs `unsandboxed: true`:** anything that talks to a unix socket or needs a secret variable. Examples: Docker and Podman, `ssh` or `git push` through an ssh agent, `gpg` signing, `systemctl --user`, and anything else that talks to a desktop service.

**What stays open:**

- The tool caches stay writable so downloads work. A command can plant a file there (a poisoned package or build cache) that a tool you run later outside the sandbox executes.
- The project stays writable. The model can create files in it that novacode or another tool runs later, for example a missing `.novacode/` or `.mcp.json` (read-only only once it exists), or code that your tests or build run.

When a sandboxed command fails with `Read-only file system`, the result adds one line that names the writable places and says how to run the command outside.

**Running one command outside.** `bash` and `process` `start` take `unsandboxed: true` for a command that needs what the sandbox hides or blocks, for example `git push` over SSH. Such a command gets your full environment. There is no approval card for it, but the risk gate still applies, the [command judge](#command-judge) checks every such call (its block asks you), and the result says `[ran outside the sandbox]`.

**What it doesn't cover:** your own `!` commands, hooks, extensions' `exec`, MCP servers and provider programs. You set those up yourself, so they run as you.

**Settings** (global only, so a project can't turn the sandbox off):

| Key | Meaning |
|---|---|
| `sandbox.mode` | `auto` (default), `on` or `off`. |
| `sandbox.allowWrite` | More writable folders. |
| `sandbox.denyRead` | More hidden files and folders. |

With `auto`, novacode checks once whether bubblewrap works. If it doesn't, it logs one warning and commands run unsandboxed. With `on`, commands are refused instead (except `unsandboxed: true` ones). `novacode doctor` shows the state under `sandbox`: `on (bubblewrap)`, `off (setting)`, or `unavailable (bwrap not found), commands run unsandboxed`.

## Approval dialogs

When something needs your approval, a card shows the tool, the command or path, and why. Answer with:

| Key | Answer |
|---|---|
| `y` | Allow once. Enter alone does not approve. |
| `a` | Allow for the rest of this session. |
| `A` | Always allow: saves a rule to your global settings. Not offered for commands held by the risk gate. |
| `n` | Deny. You can type a reason, then Enter; the model sees it. |
| Esc or Ctrl+C | Deny without a reason. |

Keys are ignored for 400 ms after a card appears, so a key you were already typing doesn't answer it.

"Session" for a risk-gated command means that exact command. For other calls, `a` and `A` use a rule such as `Bash(git push:*)` (the first two words of the command) or `Edit(<path>)` / `Write(<path>)`; `A` also saves it to `permissions.allow`.

In print mode there is nobody to approve. A call that would need approval is not run, and the model is told to use an allow rule or run interactively (or, for a risk-gate hold, to add a `justification`).

## Permission rules

Rules go in `permissions.allow`, `permissions.ask` and `permissions.deny`. A rule is `Tool` or `Tool(pattern)`. Tool names are not case sensitive; `*` matches every tool.

```json
{
  "permissions": {
    "allow": ["Bash(git status)", "Bash(cargo test:*)", "Read(**/notes/**)"],
    "ask":   ["Bash(git push:*)", "Edit(src/secret/**)"],
    "deny":  ["Bash(curl:*)", "Read(**/.env)"]
  }
}
```

| Rule | Matches |
|---|---|
| `Bash` | Every bash command. |
| `Bash(git status)` | Exactly that command. |
| `Bash(git push:*)` | `git push` and anything starting with `git push `. |
| `Read(path)`, `Write(path)`, `Edit(path)`, `Grep(path)`, `Find(path)`, `Ls(path)` | The tool's `path` argument, as a glob (`*`, `**`, `?`). The glob is matched against the path as given and against its absolute form, but the pattern itself is not expanded: `~/` in a pattern only matches a path the model wrote with `~/`. Use `**/name` to match anywhere. |
| Any other tool name | The whole tool (no pattern). A rule with a pattern, such as `Mcp(x)`, never matches these tools. |

`process` with `action: start` is checked as a `bash` call with its `command`: `Bash(...)` rules, protected commands, the risk gate and read-only mode all apply.

Bash commands are matched per simple command. Commands are split at `;`, `&&`, `||`, `|`, `&`, newlines and group brackets, a redirect counts as a `>` word (so `Bash(git status)` does not match `git status > out`), and compared with quotes removed (`git commit -m "a b"` is compared as `git commit -m a b`). A leading `FOO=1` stays in the text, so `Bash(git status)` does not match `FOO=1 git status`. A `deny` or `ask` rule fires if **any** part of a chained command matches. An `allow` rule only fires if **all** parts match, so `Bash(git status)` does not allow `git status && rm -rf x`.

Order of checks for each call:

1. Profile, the active skill's `disallowed-tools` for this turn, or the tool list of a mode preset (tool not available: refused).
2. `deny` rules.
3. Protected commands, then the risk gate.
4. File tools writing into a credential folder, a protected path, or your home or a system folder itself: refused.
5. Read-only mode (or a read-only mode preset). `allow` rules do not get around it. File changes, `enter_worktree`, `exit_worktree`, `process` input and commands that aren't read-only are refused. A tool whose effect novacode can't tell (an MCP tool without `readOnlyHint`, an extension tool without `readOnly`, a client tool) asks you, or is refused when nobody can answer; once you allow it, an `allow` rule or a session approval lets it run.
6. A file tool changing a `settings.json` in `~/.novacode` or a project's `.novacode`: asks you, or is refused when nobody can answer.
7. `allow` rules (and what you approved this session, and the active skill's allowed tools for this turn), then a plugin `PreToolUse` hook's `"allow"`.
8. `ask` rules.
9. `ask` mode.
10. Otherwise allow.

So a Catastrophic command is refused before any `allow` rule is looked at. `ask` rules still ask in `auto` mode.

A plugin hook's `"ask"` turns an allowed call into one that needs your approval; it never makes a refused call run.

Malformed rules are ignored with a warning in the log.

## Profiles

A profile limits which tools a run may use.

```sh
novacode --profile scout
```

`scout` and `reviewer` are built in and identical: they allow `read`, `grep`, `find` and `ls` only. These profiles apply to `--profile` runs. Subagent types have their own tool lists, which are a little different (see [Subagents](subagents.md)). Define your own under `permissions.profiles`:

```json
{
  "permissions": {
    "profiles": {
      "docs": { "tools": ["read", "grep", "find", "ls", "write"] },
      "nobash": { "deny": ["bash"] }
    }
  }
}
```

`tools` is an allow-list (leave it out for all tools); `deny` removes tools. An unknown profile name is an error.

## Permissions are global only

`permissions` and `sandbox` are read from `~/.novacode/settings.json` only. (Which folders you trust is kept in `~/.novacode/trust.json`, never in settings.) A project's `.novacode/settings.json` cannot widen what the agent may do, even when the folder is trusted.

## Project trust

A project folder can ship config that changes settings and runs code. novacode only loads it from folders you trust. These are gated:

- `.novacode/settings.json` (and with it, hooks)
- `.novacode/extensions`
- `.novacode/skills`
- `.novacode/agents`
- `.mcp.json`
- project-level Claude Code config under `.claude`: `skills`, `agents` and `commands` (when `compat.claude` is on)

`AGENTS.md`, `CLAUDE.md` and the project's `.novacode/prompts` are plain text and always load.

The first time you open a folder that has any of these, an interactive session asks:

- Trust this folder
- Trust the parent folder
- Trust for this session only
- Don't trust

Decisions are stored in `~/.novacode/trust.json`. The nearest folder entry up the tree wins, and trusting a folder covers everything below it.

Manage trust from the command line:

```sh
novacode trust                 # trust the current folder
novacode trust ~/work          # trust another folder and everything below it
novacode trust --revoke        # distrust the current folder (stored as an explicit "no")
novacode --trust               # trust this folder for this run only
```

`--revoke` writes a "no" for that folder, which also beats trust inherited from a parent folder. Without a stored decision at the folder or any parent, a folder is untrusted and the prompt appears. In print mode there is no prompt: the project config is ignored, and a note on stderr says so. Run `/reload` in a session to pick up a `novacode trust` you ran meanwhile.

## Secrets

- **Your own logins are always masked.** API keys, OAuth tokens and similar secrets novacode holds are replaced with `[redacted: <source>]` in tool output before the model sees it. Values shorter than 8 characters are skipped.
- **Pattern masking is opt-in.** With `security.maskSecrets` on, well-known token shapes are masked too, wherever they come from: GitHub, AWS, OpenAI, Anthropic, Slack and Google keys, private key blocks, and the values (8 characters or more) of `NAME=value` or `NAME: value` assignments whose name, in any case (`api_key=` too), contains `SECRET`, `TOKEN`, `PASSWORD`, `PASSWD`, `API_KEY`, `APIKEY`, `PRIVATE_KEY` or `ACCESS_KEY`.

```json
{
  "security": { "maskSecrets": true }
}
```

- Logins are stored in `~/.novacode/auth.json`, created with mode `0600` (owner read and write only).
- MCP OAuth tokens are stored in `~/.novacode/mcp-auth.json`, also `0600`.
- Spill files are masked the same way. Exports and debug bundles always mask both your saved logins and the token patterns, whatever `security.maskSecrets` says. See [Sessions](sessions.md).

Masking changes what the model reads. It does not stop a command you allow from sending a secret somewhere.

## Network and privacy

- novacode has no telemetry and sends no usage data of its own.
- It talks to the model provider you chose, plus anything you or the agent ask for: MCP servers, `web_fetch`, `web_search`, and `git` for packages.
- On start (interactive sessions, when the cached copy is old) and with `novacode models --refresh`, it may refresh the model catalog from `https://models.dev/api.json`. The refreshed copy is cached in `~/.novacode/cache/models-dev.json`. This request carries no prompts or session data.

## Reporting a security issue

Open an issue on the project's GitHub repository (Novaturiente/novacode). Do not paste real keys or tokens. `novacode debug bundle` collects logs and settings with secrets masked, which is safe to attach.

## See also

- [Tools](tools.md)
- [Config](config.md)
- [Subagents](subagents.md)
- [Extensions](extensions.md)
- [MCP](mcp.md)
- [Automation](automation.md)
- [Migrating](migrating.md): `novacode import claude` converts Claude Code permission rules.
- [Troubleshooting](troubleshooting.md)
