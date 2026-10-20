# Security

This page explains what novacode does and does not protect you from: permission modes, the command risk gate, permission rules, project trust, and how secrets are handled.

## The threat model

novacode has **no sandbox**. Commands the agent runs with `bash` run as you, with your files, your network and your credentials. The protections below are guard rails that catch mistakes. They are not a wall against a determined attacker or a hostile model.

If you need real isolation, run novacode inside a container or VM.

What the guard rails do:

- A permission policy checks every tool call before it runs (including calls made from `codemode` scripts).
- A command risk gate looks at each `bash` command and holds or refuses destructive ones.
- Project config that can run code only loads from folders you trust.
- Your own logins are masked in tool output before the model sees it.

What they do not do: stop a command the gate does not recognise, or stop code that a program you allowed runs on its own.

## Permission modes

| Mode | What happens |
|---|---|
| `auto` (default) | Everything runs, except what the risk gate or your rules stop. |
| `ask` | You approve every file change (`write`, `edit`, `apply_patch`) and every `bash` command that is not read-only. |
| `read-only` | No file changes. `bash` only runs read-only commands such as `ls`, `cat`, `grep`, `git status`. |

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

Plan mode (a built-in mode preset) is read-only too.

## The command risk gate

Every `bash` command is split into simple commands (including ones inside `$(...)`, backticks, `sh -c '...'` and `sudo`) and classified. What it cannot parse is treated as risky, not waved through.

| Level | Meaning | Examples |
|---|---|---|
| Safe | Reads only. Allowed in read-only mode. | `ls`, `git status` |
| Low | Changes something ordinary. | writing a file, running a program |
| Confirm | Destroys data or history in bulk. | `rm -rf target`, `git reset --hard`, `git clean -fd`, `git branch -D`, `find -delete`, `curl ... \| sh`, `sudo ...`, `dd`, `chmod -R` |
| Shared | Rewrites or deletes history on a remote, which no local undo can fix. | `git push --force`, `git push origin +main`, `git push --delete` |
| Catastrophic | Targets the system, your home folder or credentials. **Never runs.** | `rm -rf /`, `rm -rf ~`, writing to `/dev/sda`, `mkfs`, `kill -1`, fork bombs, anything that deletes or overwrites `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.config/gcloud`, `~/.password-store` or `~/.novacode` |

What each level does depends on the mode:

- **Catastrophic**: refused in every mode. The model is told to do it another way or ask you to run it yourself.
- **Shared**: asks you, even in `auto` mode.
- **Confirm**: asks you in `ask` and `read-only` mode. In `auto` mode it runs without asking (it is logged).
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

A protected command never runs. Protected paths are treated like credentials: destroying or overwriting them is Catastrophic.

### When nobody can approve

In print mode (`-p`) and other runs with no one to answer a question, a held command cannot be approved. novacode gives the model one way forward: call `bash` again with the same command and a `justification` field (at least 12 characters) that says why it is safe. If the justification is there, the command runs and the reason is logged. Without it, the command is refused. This applies to Confirm and Shared commands.

## Approval dialogs

When something needs your approval, a card shows the tool, the command or path, and why. Answer with:

| Key | Answer |
|---|---|
| `y` or Enter | Allow once. |
| `a` | Allow for the rest of this session. |
| `A` | Always allow: saves a rule to your global settings. Not offered for commands held by the risk gate. |
| `n` | Deny. You can type a reason, then Enter; the model sees it. |
| Esc or Ctrl+C | Deny without a reason. |

"Session" for a risk-gated command means that exact command. "Always" saves a rule such as `Bash(git push:*)` (the first two words of the command) or `Edit(<path>)` to `permissions.allow`.

In print mode there is nobody to approve. A call that would need approval is not run, and the model is told to use an allow rule or run interactively.

## Permission rules

Rules go in `permissions.allow`, `permissions.ask` and `permissions.deny`. A rule is `Tool` or `Tool(pattern)`. Tool names are not case sensitive; `*` matches every tool.

```json
{
  "permissions": {
    "allow": ["Bash(git status)", "Bash(cargo test:*)", "Read(~/notes/**)"],
    "ask":   ["Bash(git push:*)", "Edit(src/secret/**)"],
    "deny":  ["Bash(curl:*)", "Read(.env)"]
  }
}
```

| Rule | Matches |
|---|---|
| `Bash` | Every bash command. |
| `Bash(git status)` | Exactly that command. |
| `Bash(git push:*)` | `git push` and anything starting with `git push `. |
| `Read(path)`, `Write(path)`, `Edit(path)`, `Grep(path)`, `Find(path)`, `Ls(path)` | The tool's `path` argument, as a glob (`*`, `**`, `?`). Matched against the path as given and against its absolute form. `~/` expands to your home folder. |
| Any other tool name | The whole tool (no pattern). |

Bash commands are matched per simple command. A `deny` or `ask` rule fires if **any** part of a chained command matches. An `allow` rule only fires if **all** parts match, so `Bash(git status)` does not allow `git status && rm -rf x`.

Order of checks for each call:

1. Profile (tool not available: refused).
2. `deny` rules.
3. Protected commands, then the risk gate.
4. Read-only mode.
5. `allow` rules (and what you approved this session).
6. `ask` rules.
7. `ask` mode.
8. Otherwise allow.

So a Catastrophic command is refused before any `allow` rule is looked at. `ask` rules still ask in `auto` mode.

Malformed rules are ignored with a warning in the log.

## Profiles

A profile limits which tools a run may use.

```sh
novacode --profile scout
```

`scout` and `reviewer` are built in: they allow `read`, `grep`, `find` and `ls` only. Define your own under `permissions.profiles`:

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

`permissions` is read from `~/.novacode/settings.json` only. A project's `.novacode/settings.json` cannot widen what the agent may do, even when the folder is trusted.

## Project trust

A project folder can ship config that changes settings and runs code. novacode only loads it from folders you trust. These are gated:

- `.novacode/settings.json` (and with it, hooks)
- `.novacode/extensions`
- `.novacode/skills`
- `.mcp.json`
- project-level Claude Code config under `.claude` (when `compat.claude` is on)

`AGENTS.md` and `CLAUDE.md` are plain text and always load.

The first time you open a folder that has such config, an interactive session asks:

- Trust this folder
- Trust the parent folder
- Trust for this session only
- Don't trust

Decisions are stored in `~/.novacode/trust.json`. The nearest folder entry up the tree wins, and trusting a folder covers everything below it.

Manage trust from the command line:

```sh
novacode trust                 # trust the current folder
novacode trust ~/work          # trust another folder and everything below it
novacode trust --revoke        # distrust the current folder
novacode --trust               # trust this folder for this run only
```

Without a stored decision, a folder is untrusted. In print mode there is no prompt: the project config is ignored, and a note on stderr says so. Run `/reload` in a session to pick up a `novacode trust` you ran meanwhile.

## Secrets

- **Your own logins are always masked.** API keys, OAuth tokens and similar secrets novacode holds are replaced with `[redacted: <source>]` in tool output before the model sees it. Values shorter than 8 characters are skipped.
- **Pattern masking is opt-in.** With `security.maskSecrets` on, well-known token shapes are masked too, wherever they come from: GitHub, AWS, OpenAI, Anthropic, Slack and Google keys, private key blocks, and values of `*_TOKEN=` / `*_SECRET=`-style assignments.

```json
{
  "security": { "maskSecrets": true }
}
```

- Logins are stored in `~/.novacode/auth.json`, created with mode `0600` (owner read and write only).
- MCP OAuth tokens are stored in `~/.novacode/mcp-auth.json`, also `0600`.

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
- [Troubleshooting](troubleshooting.md)
