# Quickstart

This page takes you from a fresh install to your first prompt, and shows the basic keys and flags.

## First run

Go to a project folder and start novacode:

```sh
cd ~/code/my-project
novacode
```

novacode needs a terminal. For pipes and scripts use `-p` (see below).

### 1. Connect

If you have no default model yet, a picker asks how novacode should reach a model:

- **Subscriptions** (Claude, Codex, Antigravity, Copilot) are always listed. Pick one and novacode starts the login (a browser sign-in). Details are in [providers](providers.md).
- **API-key providers** show up in the list only when their key is already set, for example `OPENROUTER_API_KEY` in your environment. `novacode models --all` lists every provider and its env var.

Press Esc to cancel. To skip the picker, pass a model: `novacode -m provider/model`.

### 2. Pick a model

novacode asks the provider for its model list. Pick one. It is saved to `~/.novacode/settings.json` as `"model"`, so you won't be asked again. Change it any time with `/model`.

### 3. Trust prompt

If the folder has project config (`.novacode/settings.json`, `.novacode/extensions`, `.novacode/skills`, `.novacode/agents`, `.mcp.json`, or `.claude/skills`, `.claude/agents`, `.claude/commands`), novacode asks whether to trust it. Project config can change settings and run code, so it only loads after you say yes.

| Choice | Effect |
|---|---|
| Trust this folder | Remembered for this folder and below |
| Trust the parent folder | Remembered for the parent and below |
| Trust for this session only | Not saved |
| Don't trust | Remembered; project config is ignored |

`AGENTS.md` and `CLAUDE.md` load either way. See [security](security.md).

## Your first prompt

Type a request and press Enter:

```text
explain what this project does and how to run its tests
```

novacode reads files and runs tools on its own. In the default `auto` mode it asks you before commands that touch shared state, like a force push, and refuses destructive ones. See [security](security.md) for the stricter `ask` and `read-only` modes. Add files to a prompt with `@`:

```text
@src/main.rs why does this return early?
```

Start a line with `!` to run a shell command yourself. The model sees its output on its next call; use `!!` to keep the output from the model:

```text
!git status
```

## Keys and commands

| Key | Does |
|---|---|
| Enter | Send (while the model works, it steers the current turn) |
| Alt+Enter | Queue the message as a follow-up |
| Shift+Enter or Ctrl+J | New line |
| Esc | Stop the running turn |
| Ctrl+C | Clear the input, stop the turn, or quit on a second press |
| Ctrl+D | Quit when the input is empty |
| Ctrl+L | Pick a model |
| Shift+Tab | Next thinking level |
| Ctrl+R | Search prompt history |
| Ctrl+G | Edit the prompt in `$EDITOR` |
| Alt+/ | Show all key bindings |

Type `/` to see the slash commands. The ones you will use first:

| Command | Does |
|---|---|
| `/model` | Pick a model, or `/model <id>` |
| `/thinking` | Pick a thinking level, or `/thinking <level>` |
| `/resume` | Resume another session in this folder |
| `/new` | Start a new session |
| `/login <provider>` | Log in without leaving novacode |
| `/keys` | Show key bindings |
| `/quit` | Quit |

See [tui](tui.md) for the full list.

## Continue or resume a session

novacode saves every session. Pick up where you left off:

```sh
novacode -c           # the most recent session in this folder
novacode -r 0192      # a session by id prefix
```

Inside novacode, `/resume` shows the sessions of this folder. See [sessions](sessions.md). Use `--no-session` to run without saving.

## One-shot prompts

`-p` sends one prompt, prints the final answer to stdout and exits:

```sh
novacode -p "list the public functions in src/lib.rs"
git diff | novacode -p "write a commit message for this diff"
```

Piped input is added after the prompt. There is nobody to approve actions in this mode, so requests that would ask you are refused. Add `-m provider/model` to choose a model, and see [automation](automation.md) for `--json` and other options.

## Check your setup

```sh
novacode auth status   # which logins and keys novacode sees
novacode doctor        # checks every connected provider and your settings
```

## Next steps

- Connect more providers: [providers](providers.md)
- Control what the agent may do: [security](security.md)
- Tune settings: [config](config.md)
- Add instructions for your project: [context](context.md)

## See also

- [Install](install.md)
- [Providers](providers.md)
- [TUI](tui.md)
- [Sessions](sessions.md)
- [Security](security.md)
