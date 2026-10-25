# Quickstart

This page takes you from a fresh install to your first prompt, and shows the basic keys and flags.

## First run

Go to a project folder and start novacode:

```sh
cd ~/code/my-project
novacode
```

novacode needs a terminal (stdin and stdout must both be one), or it stops with "interactive mode needs a terminal". For pipes and scripts use `-p` (see below).

### 1. Trust prompt

If the folder has project config (`.novacode/settings.json`, `.novacode/extensions`, `.novacode/skills`, `.novacode/agents`, `.mcp.json`, or `.claude/skills`, `.claude/agents`, `.claude/commands`), novacode first asks whether to trust it. Project config can change settings and run code, so it only loads after you say yes.

| Choice | Effect |
|---|---|
| Trust this folder | Remembered for this folder and below |
| Trust the parent folder | Remembered for the parent and below |
| Trust for this session only | Not saved |
| Don't trust | Remembered; project config is ignored |

`AGENTS.md` and `CLAUDE.md` load either way. See [security](security.md).

### 2. Connect

If you have no default model yet, a picker asks how novacode should reach a model:

- **Subscriptions** (Claude, Codex, Antigravity, Copilot) are always listed. Pick one that you are not logged in to yet and novacode starts the login, then lists its models. novacode prints a sign-in URL and tries to open it with `xdg-open` (Linux) or `open` (macOS). If no browser opens, open the printed URL yourself. Details are in [providers](providers.md).
- **API-key providers** show up in the list only when their key is already set: in your environment (for example `OPENROUTER_API_KEY`) or saved with `novacode login <provider>`. `novacode models --all` lists every provider and its env var. Local servers (Ollama, LM Studio) don't appear here; pass `-m ollama/<model>`.

Press Esc to cancel. novacode then exits with "setup cancelled"; run it again. To skip the picker, pass a model: `novacode -m provider/model`, or `provider/model:thinking` such as `-m codex/gpt-5.5:high`.

### 3. Pick a model

novacode asks the provider for its model list. Pick one. It is saved to `~/.novacode/settings.json` as `"model"`, so you won't be asked again. Change it any time with `/model`.

## Your first prompt

Type a request and press Enter:

```text
explain what this project does and how to run its tests
```

novacode reads files and runs tools on its own. In the default `auto` mode it asks you before commands that touch shared state, like a force push, and refuses catastrophic ones such as `rm -rf ~`. Other risky commands, like `rm -rf target`, run without asking. See [security](security.md) for the stricter `ask` and `read-only` modes. Add files to a prompt with `@`:

```text
@src/main.rs why does this return early?
```

Start a line with `!` to run a shell command yourself. It runs, shows its output, and the model sees that output on its next call. Use `!!` to run it without telling the model:

```text
!git status
```

## Keys and commands

| Key | Does |
|---|---|
| Enter | Send (while the model works, your message is added to the current turn) |
| Alt+Enter | Queue the message as a follow-up |
| Shift+Enter or Ctrl+J | New line |
| Esc | Stop the running turn |
| Ctrl+C | Clear the input, stop the turn, or quit on a second press |
| Ctrl+D | Quit when the input is empty |
| Ctrl+L | Pick a model |
| Shift+Tab | Next thinking level (how much the model reasons before it answers; see [tui](tui.md)) |
| Ctrl+R | Search prompt history |
| Ctrl+G | Edit the prompt in `$VISUAL` or `$EDITOR` (else the first of nvim, nano, emacs, vim, vi) |
| Alt+/ | Show all key bindings |

Type `/` to see the slash commands. The ones you will use first:

| Command | Does |
|---|---|
| `/model` | Pick a model, or `/model <id>` |
| `/thinking` | Pick a thinking level, or `/thinking <level>` |
| `/resume` | Resume another session in this folder |
| `/new` | Start a new session |
| `/login [provider]` | Log in without leaving novacode (no provider = pick from a list) |
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

`-p` needs a prompt argument. Piped input is added after the prompt. There is nobody to approve actions in this mode, so requests that would ask you are refused. An untrusted folder's project config is skipped with a note; run `novacode trust` or pass `--trust`. Add `-m provider/model` to choose a model, and see [automation](automation.md) for `--json` and other options.

## Check your setup

```sh
novacode auth status   # which logins and keys novacode sees
novacode doctor        # checks every connected provider and your settings
```

Both exit with `1` when nothing is connected (`doctor` also when it finds a problem). Without a model, a `-p` run fails with `no model: pass -m provider/model or set "model" in settings`. More in [troubleshooting](troubleshooting.md).

## Command line reference

`novacode [OPTIONS] [PROMPT] [COMMAND]`. `novacode --help` and `novacode <command> --help` print the same from the binary.

| Flag | Does |
|---|---|
| `PROMPT` | Start the interactive session with this prompt |
| `-p`, `--print PROMPT` | Print mode: answer on stdout, then exit |
| `--json` | With `-p`: write JSON lines instead of the answer: one `hello` line, then one event per line |
| `--rpc` | Speak the JSON protocol on stdin/stdout (not with `-p`) |
| `--bg [PROMPT]` | Run in the background and print the session id |
| `-m`, `--model provider/model[:thinking]` | Model for this run |
| `-c`, `--continue` | Continue the newest session in this folder |
| `-r`, `--resume ID` | Resume a session by id prefix, in the folder it was last in |
| `--no-session` | Don't save this session |
| `-w`, `--worktree [NAME]` | Start in git worktree `.worktrees/NAME` on branch `worktree-NAME` (made if missing; no NAME: `wt-MMDD-HHMM`) |
| `--mode auto\|ask\|read-only` | Permission mode |
| `--profile NAME` | Limit tools to a named profile, e.g. `scout` |
| `--trust` | Trust this folder's project config for this run |
| `--no-context-files` | Don't load `AGENTS.md` / `CLAUDE.md` |
| `--api-key KEY` | API key for this run only. Needs a model (`-m` or the `model` setting) |
| `-v`, `--verbose` | With `-p`: tool calls and token usage on stderr |
| `--trace` | Record the raw request and stream to a log file |
| `--profile-startup` | Print startup timings and exit |
| `-V`, `--version`, `-h`, `--help` | Version and help |

| Command | Does |
|---|---|
| `login [PROVIDER] [--account NAME] [--method browser\|code\|device]` | Log in to a subscription, or save an API key (no provider: pick from a list). `--account` is for subscriptions only |
| `logout PROVIDER [--account NAME]` | Remove a login |
| `models [FILTER] [--all] [--refresh]` | List models; `--refresh` updates the catalog |
| `auth status` | Show logins and keys |
| `doctor` | Check providers, settings and key bindings |
| `export [SESSION] [--html] [-o FILE]` | Export a session as JSONL or HTML |
| `agents` | List live sessions |
| `attach ID` | Open a window on a live session |
| `panel --session ID` | The side panel of a live session, full screen |
| `status [--session ID] [--all] [--json] [--format T]` | State of a live session |
| `stop ID` | End a live session |
| `debug bundle [--session ID \| --no-session] [-o FILE]` | Pack logs, settings and a session for a bug report |
| `stats [SESSION] [--all] [--turns]` | Timing report (needs `debug.metrics`) |
| `install SOURCE [--name N] [--ref REF]` | Install a package from a git URL or folder |
| `uninstall NAME` | Remove a package |
| `packages [update [NAME] [--commits] \| config NAME \| install]` | List, update, configure or install packages |
| `mcp list \| add \| remove \| login \| logout` | Manage MCP servers. `mcp add NAME [--url URL] [--env K=V] [-- CMD ARGS…]` |
| `import claude [--yes]` | Copy Claude Code settings once |
| `trust [PATH] [--revoke]` | Trust (or distrust) a folder's project config |
| `update` | Update novacode itself |
| `shell-init fish\|zsh\|bash` | Print a shell function that follows the session's folder on exit |

Details for `-p`, `--json`, `--rpc`, `--bg` and exit codes are in [automation](automation.md); the rest are on the pages linked from [Next steps](#next-steps).

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
