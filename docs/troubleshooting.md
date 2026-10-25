# Troubleshooting

This page lists the tools novacode gives you to find out what went wrong, where the logs are, fixes for the problems people hit most, and how to report a bug.

## Where to look first

| Tool | What it tells you |
|---|---|
| `novacode doctor` | Checks every connected provider (one model-list call each), your settings files, your key bindings, and skills, prompts, agents and packages that were skipped or disabled. Exits with a failure code if a provider, setting or key has a problem, if no provider is connected, or if `sandbox.mode` is `on` and the sandbox can't work. |
| `novacode auth status` | Each login and key, where it comes from, and when its stored token expires (read from disk, no network check) |
| `/debug` (in a session) | Session id and file, route, context and cache numbers, queued prompts, processes, MCP server status, the log folder and how many crash logs exist |
| `novacode mcp list` | Connects to every MCP server and shows `ready` or `failed: <reason>` |
| `novacode status` | State of a live session (`--json` for the full record, `--format '{ctx}% {5h}'` for a tmux status line) |

```sh
novacode doctor
```

```text
providers
  ✓ claude  12 models (0.4s)
settings
  ✓ no problems
keys
  ✓ no clashes
skills, prompts, agents
  ✓ nothing shadowed or disabled
sandbox
  ✓ on (bubblewrap)
```

`✓` is fine and `✗` is a problem (exit code 1). `-` is a local provider whose server isn't running, or a sandbox that is off or unavailable while `sandbox.mode` is `auto` (commands then run unsandboxed; not a problem). `·` is a skill, prompt or agent that was skipped or shadowed (not a problem).

## Flags for one run

| Flag | What it does |
|---|---|
| `-v`, `--verbose` | With `-p`: print tool calls, the stop reason, token and cache usage, and the session file to stderr |
| `--trace` | Record the raw request and stream (secrets masked) to `~/.novacode/logs/trace-<id>.jsonl`. It also sets the default log level to `debug` (unless `NOVACODE_LOG` is set). In print mode the file path is printed to stderr. |
| `--profile-startup` | Time each startup phase, print the table and exit before a session starts |

```sh
novacode --profile-startup
```

## Logs

Logs are in `~/.novacode/logs/` (or `$NOVACODE_HOME/logs`).

| File | What it holds |
|---|---|
| `novacode.log.<date>` | The main log, one file per day. Extension `log()`, `console.*` and process-extension `log` lines land here too, tagged `extension=<name>` |
| `crash-<time>-<pid>.log` | Written when novacode panics: the message, a backtrace and the last log lines, with secrets masked. The terminal is restored first and the file's path is printed. |
| `mcp-<server>.log` | The stderr of a local (stdio) MCP server |
| `ext-<name>.log` | The stderr of a process extension, emptied at each start |
| `trace-<id>.jsonl` | Written by `--trace` |
| `cache-misses*` | Notes on why the prompt cache missed |

The default log level is `info`. Set `NOVACODE_LOG` to change it with [`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):

```sh
NOVACODE_LOG=debug novacode
NOVACODE_LOG=novacode_core=debug,info novacode
```

## Timing numbers: `debug.metrics` and `novacode stats`

To see where time and tokens go, turn on metrics in `~/.novacode/settings.json`:

```json
{ "debug": { "metrics": true } }
```

Each session then writes a timing file in `~/.novacode/metrics/`. Read it with:

```sh
novacode stats              # the latest session
novacode stats 01jabc       # one session, by id prefix
novacode stats --turns      # every turn, not just the slowest
novacode stats --all        # all sessions together
```

Without metrics files, `stats` tells you to turn the setting on first.

## Common problems

In print mode (`-p`) novacode exits `0` on success, `1` on an error, `2` on a bad flag and `130` when aborted. See [automation](automation.md).

### "Account is exhausted" or rate limit errors

If you have a subscription and it hits its limit, the message says when it resets if the provider told novacode (otherwise it says `quota used up`), for example `claude account `default`: … (resets in 3h 20m). Add another account with `novacode login claude --account <name>`, or switch model with /model.`

You have these options:

- Wait for the reset. `/usage` shows your quota windows.
- Add a second account. When one runs out, novacode switches to the next one on its own and tells you (`account `default` is exhausted (…); switched to account `work``). It remembers which accounts are out, so a new session doesn't start with a spent one. novacode never falls back to a metered API key by itself.
- Switch to another model or provider with `/model`.

```sh
novacode login claude --account work
novacode logout claude --account work   # remove one account later
```

### Login on a machine with no browser

`novacode login` always prints the URL, so you can open it on another machine (in `/login`, `Ctrl+X` copies it).

| Provider | What to do |
|---|---|
| `codex`, `copilot` | You get a short code and a URL. Open the URL on any device, enter the code and approve. It works over SSH. |
| `claude` | Pick **Copy a code** (`--method code`), open the URL anywhere, sign in, then paste the code you're shown. With **Browser**, paste the `localhost` address the browser fails to load instead, or forward port 53692. |
| `codex` (browser) | Same as antigravity below; or pick **Device code** (`--method device`). |
| `antigravity` | Open the URL anywhere. When the browser ends on a `localhost` page that fails to load, copy that full address and paste it into the terminal. |
| API-key providers (`openrouter`, `openai`, …) | `novacode login <provider>` asks for the key. You can pipe it: `echo "$KEY" \| novacode login openrouter`. novacode first checks the key with a model-list call. If the check fails it asks "save it anyway? [y/N]"; with piped input it saves nothing. Or skip the login and set the provider's environment variable. |

For one-off scripted runs, `--api-key` with `-m provider/model` needs no stored login at all.

### "The login has expired or was revoked" (`invalid_grant`)

The provider refused the saved refresh token, so only a new login helps. The message names the command, for example `novacode login claude --account work`. If you have more than one account for that provider, novacode skips the broken one and uses the next, with a notice.

Subscription logins use a refresh token that the provider replaces each time it is used. If you copy or share one `auth.json` between several `NOVACODE_HOME` folders (or machines), whichever copy refreshes first makes the others' token invalid. They then fail with a token error such as `invalid_grant`.

Fix it by logging in again where it fails (`novacode login <provider>`). To avoid it, give each home its own login instead of sharing `auth.json`. Inside one home, parallel sessions are fine: they take a lock and share the refreshed token.

### An MCP server doesn't connect

```sh
novacode mcp list
```

This starts every configured server and prints `ready`, `connecting` or `failed: <reason>` with its tools. It exits with `1` if any failed. Then check:

- A local server writes its stderr to `~/.novacode/logs/mcp-<name>.log`. Read it.
- The command must be on your `PATH` when novacode starts. Try running it by hand.
- A project `.mcp.json` loads only in a [trusted folder](security.md). `novacode mcp list` says so when it ignores one.
- Server names can't contain `__` (two underscores). novacode warns and skips them.
- An HTTP server that needs OAuth needs `novacode mcp login <name>`.
- `/mcp` in a session shows the same status.

See [mcp](mcp.md).

### Project config isn't loading

Project settings, extensions, hooks, skills, packages and `.mcp.json` load only in folders you trust. The first time you start novacode interactively in an untrusted folder that has project config, it asks whether to trust it. If you already answered, or in a non-interactive run, it prints:

```text
note: this folder's project config is ignored (not trusted). Run `novacode trust` or pass --trust.
```

```sh
novacode trust            # trust this folder and everything below it
novacode trust --revoke   # take it back
novacode --trust          # trust it for this run only
```

If the project config changed since you trusted the folder, novacode asks again; a non-interactive run ignores it with a note that lists what changed. Run `novacode trust` again to accept the change.

Non-interactive runs (`-p`, `--rpc`, `--bg`) never ask, so they always need `novacode trust` or `--trust`. `AGENTS.md` and `CLAUDE.md` load either way. If you changed trust or settings in a running session, use `/reload`. See [security](security.md).

### Icons show as boxes or question marks

Your terminal font has no Nerd Font glyphs. Either install a Nerd Font, or switch to plain Unicode icons:

```json
{ "ui": { "icons": "unicode" } }
```

### Images don't show

novacode picks an image protocol on its own (`ui.images` is `auto`). If your terminal can't show images, or picks the wrong protocol, set it yourself:

```json
{ "ui": { "images": "kitty" } }
```

Valid values are `auto`, `kitty`, `iterm2`, `sixel`, `halfblocks` and `off`. `halfblocks` draws images with plain block characters, so it needs no special terminal support.

### Slow or flickery over SSH

novacode lowers its frame rate and animations by itself when it sees an SSH session or a busy machine. If it guessed wrong, set it:

```json
{ "ui": { "slow": true } }
```

Use `false` to force the full frame rate.

### A package won't load

`novacode packages` shows `(disabled: …)` with the reason, and `novacode doctor` lists it. The usual cause is an `apiVersion` in `novacode-package.json` that this novacode doesn't match. See [packages](packages.md).

### "session not saved"

novacode couldn't write the session file (disk full, permissions, a read-only mount). Nothing is lost yet: the entries stay in memory, and no further model call or batch of tool calls starts until they are written (calls already running in a batch finish). Fix the cause, then send any message: the missing entries are written first, in order, and a "session saved again" note confirms it. If you quit before that, the unsaved part is gone.

### Something looks stuck

Press Esc to abort the turn. Open another terminal and run `novacode agents` to see live sessions (ids and prefixes work) and `novacode status` to see what the session is doing (without a session id it picks this tmux pane's session, else the newest in this folder, else the newest live one anywhere; it exits 1 with "no live session" only when none is running; `--all` lists every one). `novacode stop <id>` ends one. `/debug` shows running processes and queued prompts.

A provider that sends nothing for 120 s (no response headers, or a reply that stalls mid-stream) fails with a transient error and is retried ([providers](providers.md#errors-and-retries)), so a silent server can't hang a turn forever. A subagent with no activity for `agents.stallMinutes` (default 10) is stopped ([subagents](subagents.md#limits-and-safety)).

## Report a bug

Open an issue at <https://github.com/Novaturiente/novacode/issues>.

1. Make a debug bundle:

   ```sh
   novacode debug bundle
   ```

   It writes `novacode-debug-<ms>.tar.gz` (Unix milliseconds) in the current folder and prints the path. It needs `tar` on your `PATH`. It holds `info.txt` (version, OS, terminal, the names of your logins), your settings (global and project), key bindings and `mcp.json`, the recent logs, crash logs, cache-miss and MCP/extension logs, and the newest session in this folder with its metrics file, if any. Known keys and tokens are masked, and anything shaped like a credential is masked too. Still, look inside before you share it. The session contains your conversation.

   | Flag | What it does |
   |---|---|
   | `--session ID` | Use this session instead of the newest here |
   | `--no-session` | Leave the session out |
   | `-o`, `--out FILE` | Where to write the archive |

2. Describe what you did, what you expected and what happened. Include the version (`novacode --version`), the output of `/debug` if you can, and the `crash-*.log` file if there was a crash.
3. Attach the bundle, or paste the relevant parts.

## See also

- [install](install.md): installing and updating
- [config](config.md): `debug.metrics`, `ui.icons`, `ui.images`, `ui.slow` and the rest
- [providers](providers.md): logins, accounts and API keys
- [mcp](mcp.md): configuring MCP servers
- [security](security.md): trust and permissions
- [automation](automation.md): print mode, RPC and background sessions
