# Terminal UI

This page covers the novacode screen, the editor, slash commands, key bindings and the look of the UI (themes, status line, icons, images).

novacode runs full screen in your terminal's alternate screen. The mouse is on: the wheel scrolls, a click folds or unfolds a tool block or thinking line, and dragging over text copies it to the clipboard. That works anywhere on screen: the chat, the status line, question and approval dialogs, pickers and the side panel (dialog borders are left out of the copy).

## Layout

From top to bottom:

- **Transcript.** Your messages (in a box), the model's replies, tool calls and their output. Finished turns fold to one line by default (`Alt+O` toggles, `ui.turnCollapse`). `Ctrl+O` shows every tool's output in full or short again. `Ctrl+T` shows or folds thinking (`ui.thinking`: `collapsed`, `full` or `hidden`).
- **Editor.** The input box. It grows up to 10 rows.
- **Status line.** One row under the editor with the activity (`ready`, a spinner, or what novacode waits for) on the left and details on the right. See [Status line](#status-line).
- **Side panel.** An optional column on the right. See [Side panel](#side-panel).

Above the editor you may also see queued messages (`⏵ queued: …`), the completion popup, approval cards and question forms.

## Side panel

The panel has tabs. Switch tabs with `Tab` and `Shift+Tab` or the number keys `1`–`9` while the panel has focus (the tab bar shows the numbers then), or click a tab.

| Tab | Shows |
|---|---|
| Agent | The goal, the todo list and subagents. Subagents show from the oldest prompt that still has one running up to the newest prompt that started any; older finished ones are hidden (the title counts them). |
| Processes | One process at a time: its status, command and live output (the program's own colors, else errors red, warnings yellow, success green, URLs blue). Running ones come first. `/ps` opens the panel here with focus. |
| Usage stats | Bars for context use (and a split into system prompt, tools and messages), cache hits (last request and session) and subscription windows; sparklines of cache hit and cost (or output tokens) per request; token totals and cost; MCP servers with connected or failed state and tool counts; how many skills, prompts, agents, extensions, packages and context files loaded, and running processes and subagents. `Ctrl+Alt+U` opens it. |
| Timing | Where the time of each task went, with a sparkline of task times. |
| Files | Files the model read or edited, edited first, each with `+added −removed` lines since the session started. `Enter` or a click opens the file in your editor: in nvim or vim as a diff against the session start (`-d`), else the file alone. |

Extensions can add tabs too (see [extensions](extensions.md)).

| Key | What it does |
|---|---|
| `Ctrl+Alt+P` | Show or hide the panel. |
| `Ctrl+Alt+U` | Open the panel on Usage stats with focus. Again closes it. |
| `Ctrl+Alt+F` | Give the panel the keys. `Esc` or `q` gives them back. |
| `Up`/`Down` (or `k`/`j`) | Move in the focused panel. |
| `Enter` | Run the action of the selected row (Files: open it; extension tabs: their action). |
| Mouse | Wheel scrolls the panel, a click picks a tab or a row, a drag copies text. |
| `Left`/`Right` (or `h`/`l`) | Processes tab: the previous or next process. `Up`/`Down` scroll its output; `PageUp`/`PageDown` scroll by 10; `End` or `G` follows the tail again. |

The panel needs a terminal at least 100 columns wide. On a narrower screen it stays hidden and open todos show as a short block above the editor instead. Its width is a quarter of the screen, at least 40 columns and at most half.

By default the panel opens by itself while todos or subagents are running and hides a few seconds after the work ends (`ui.panel.auto`).

### Panel in a tmux pane

Inside tmux you can move the panel into its own pane next to novacode:

```json
{ "ui": { "panel": { "host": "tmux", "tmuxWidth": "30%" } } }
```

| `ui.panel.host` | Meaning |
|---|---|
| `inline` (default) | Draw the panel inside novacode. |
| `tmux` | Split a tmux pane that runs the panel. Needs `$TMUX`; outside tmux the panel stays inline. |
| `auto` | `tmux` when you are inside tmux, else `inline`. |

The pane runs `novacode panel --session <id>`, a full-screen panel for a live session. You can run it yourself in any terminal on the same machine. Get the id from `novacode agents`:

```bash
novacode agents
novacode panel --session 0195f3a1c2d4
```

The pane closes when the session's window closes. See [config](config.md) for all `ui.*` settings.

## The editor

| What | How |
|---|---|
| New line | `Shift+Enter`, `Ctrl+J`, or `\` then `Enter` (the backslash is dropped). |
| Send | `Enter`. |
| Move and delete | Emacs-style keys (`Ctrl+A`, `Ctrl+E`, `Ctrl+W`, `Ctrl+K`, `Ctrl+Y`…). Full list below. |
| Undo | `Ctrl+-`. |
| Prompt history | `Up` at the top line, `Down` at the bottom line. History is the prompts of the current session. |
| Search history | `Ctrl+R`, type, `Ctrl+R` again for older matches, `Enter` to use, `Esc` to cancel. |
| Complete | `Tab` or `Enter` in the popup. Works for `/commands` and `@files`. |
| Edit in your editor | `Ctrl+G` opens `$VISUAL`, else `$EDITOR`, else the first of `nvim`, `nano`, `emacs -nw`, `vim`, `vi` found. Save and quit to bring the text back. In a question form it opens what you are typing, or the focused choice's preview (a plan) to read in full or edit. |
| Copy the input | `Alt+C`. |
| Clear | `Ctrl+C` clears the editor. `Esc` twice quickly does too. |

`Shift+Enter` needs a terminal that supports the Kitty keyboard protocol, or tmux with `set -s extended-keys on` (novacode turns on xterm's modifyOtherKeys there). Elsewhere use `Ctrl+J` or `\` + `Enter`, which work in every terminal.

Big pastes (more than 5 lines or 1000 characters) collapse to a marker like `[paste #1 +40 lines]`. The full text is sent when you submit.

### @file completion

Type `@` and a few letters to fuzzy-search files in the project (ignored files are skipped). Folders keep the popup open so you can go deeper. Start with `~/`, `/`, `./` or `../` to browse one folder, also outside the project.

The path stays in your message and the model reads the file with its tools. Image files you mention (`@shot.png`) are attached to the message as images.

### Paste an image

`Ctrl+V` pastes an image from the clipboard. novacode saves it to a file and inserts `@<path>` in the editor. If the clipboard holds text, the text is pasted. This needs `wl-paste` (used when `WAYLAND_DISPLAY` is set) or `xclip` (X11). Copying also goes through the terminal (OSC 52) and, on Wayland, `wl-copy`.

### Typing while the model works

You can type while a turn runs.

| Key | What happens |
|---|---|
| `Enter` | **Steer.** The message is delivered before the next model call of the running turn. |
| `Alt+Enter` | **Follow-up.** The message waits and starts a new turn after this one ends. |
| `Alt+Up` | Take the newest queued message back into the editor. If it already reached the model you get `too late`. |
| `Esc` | Abort the running turn. |
| `Ctrl+C` | With text: clear it. While busy with an empty editor: abort. Idle: press twice to quit. |

### Shell commands with `!`

| Input | What happens |
|---|---|
| `!git status` | Runs the command in your project folder with `bash -c`. You see the output, and the model gets it at its next call. |
| `!!git status` | Same, but the output is not sent to the model. |

The command is killed after 120 seconds. For long jobs use the model's process tools and watch them in the panel or with `/ps`. `Ctrl+Alt+B` moves commands the model is running right now into the process manager.

## Slash commands

Type `/` to see the list. A line that is only `/something` unknown to novacode is rejected as a typo; `/tmp is full` (more words) is sent as a normal prompt.

### Built in

| Command | What it does |
|---|---|
| `/model` | Pick a model, or `/model <provider/model>`. |
| `/thinking` | Pick a thinking level, or `/thinking <level>` (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`). The model may not offer every level. |
| `/resume` | Resume another session from this folder. |
| `/new` | Start a new session. |
| `/name <text>` | Name this session. |
| `/worktree [name \| exit \| remove]` | Git worktrees: list them, enter or create `.worktrees/<name>`, or go back to the main checkout keeping or removing this one. See [sessions](sessions.md#worktrees). |
| `/tree` | Jump to any prompt in the session tree. |
| `/rewind` | Go back to before a prompt of this branch, with files. |
| `/fork` | New session up to before a prompt. |
| `/clone` | New session with this whole branch. |
| `/compact` | Replace older messages with the memory notes. |
| `/export [html]` | Write this branch to a file. |
| `/copy` | Copy the last reply (also `Ctrl+X`). |
| `/btw <question>` | Side question. See [below](#side-questions-with-btw). |
| `/context` | Where the context window goes. |
| `/docs [question]` | Add a hidden note telling the model where novacode's own docs (`~/.novacode/docs/`) and your config files are, then ask the question if you gave one. Use it to have the session set up or customise novacode. |
| `/usage` | Tokens and cost per route, cache warming, memory workers and subscription windows. |
| `/debug` | Route, cache, queues, processes, MCP and log paths, for bug reports. |
| `/ps` | Open the side panel on the Processes tab, one process at a time with its live output. |
| `/mcp` | MCP servers: status and tool counts. See [mcp](mcp.md). |
| `/login [provider]` | Log in from a dialog: no provider = a list of all of them. Shows the sign-in URL (`Ctrl+X` copy, `Ctrl+O` open), a device code, or a box for a pasted code / API key; the session reloads after. Several accounts need `novacode login --account` on the command line. See [providers](providers.md). |
| `/keys` (or `/help`) | Show the key bindings. |
| `/reload` | Re-read settings, keys, theme, skills, extensions, MCP servers and context files. |
| `/restart` | Restart novacode (for example after a rebuild) and resume this session. |
| `/quit` (or `/exit`) | Quit. |

Session commands are explained in [sessions](sessions.md).

In a window opened with `novacode attach`, `/resume`, `/new`, `/tree`, `/rewind`, `/fork`, `/clone`, `/reload`, `/restart` and `/login` work only in the session's own window, and so do the `Alt+Up` and `Alt+Shift+R` keys. Over `--rpc` more of them are limited, see [automation](automation.md#slash-commands-over-rpc).

### Modes and goals

| Command | What it does |
|---|---|
| `/plan` | Plan mode on or off: read-only until you approve a plan. `/plan <task>` turns it on and sends the task. |
| `/mode <name>` | Switch to a mode preset. `/mode off` leaves it. `/mode` alone lists them. |
| `/goal <objective>` | Work until the objective is done. `/goal` shows the current goal. Also `/goal pause`, `resume`, `clear` and `audit on` / `audit off`. See [subagents](subagents.md#goals). |

An active mode shows in the status line (`⏸ <name> mode`).

### Skills, templates and extensions

- `/skill:<name> [args]` inlines a skill into your message. See [skills](skills.md).
- `/<name> [args]` runs a prompt template with that name.
- Script extensions can add their own commands. See [extensions](extensions.md).

All of them show up in the `/` popup.

### Side questions with /btw

`/btw <question>` asks the model something without touching the conversation. Your main turn keeps running. The answer is not saved in the session. A short answer appears in the transcript; a longer one opens in a page (`Esc` or `q` closes it).

## Key bindings

Press `Alt+/` or type `/keys` to see the live table, including your changes. Type to filter it. These are the defaults.

### App

| Action name | Default | What it does |
|---|---|---|
| `app.interrupt` | `Esc` | Abort the running turn. Before the model's first word or tool call, this also takes the prompt back: it leaves the conversation and returns to the editor. Twice, when idle: clear the editor. |
| `app.clear` | `Ctrl+C` | Clear editor, abort, or quit on second press. |
| `app.exit` | `Ctrl+D` | Quit when the editor is empty. |
| `app.suspend` | `Ctrl+Z` | Suspend to the shell. |
| `app.model.select` | `Ctrl+L` | Pick a model. |
| `app.tools.expand` | `Ctrl+O` | Show every tool's output in full, or short again. |
| `app.turns.toggle` | `Alt+O` | Expand or collapse finished turns. |
| `app.editor.external` | `Ctrl+G` | Edit the prompt, or a question form's text or plan, in your editor. |
| `app.message.followUp` | `Alt+Enter` | Queue as a follow-up. |
| `app.message.copy` | `Ctrl+X` | Copy the last reply. |
| `app.input.copy` | `Alt+C` | Copy what is in the input box. |
| `app.message.dequeue` | `Alt+Up` | Take back the last queued message. |
| `app.reload` | `Alt+Shift+R` | Reload settings, keys, theme, skills, extensions, MCP servers and context files. |
| `app.clipboard.pasteImage` | `Ctrl+V` | Paste an image (or text). |
| `app.process.background` | `Ctrl+Alt+B` | Move running commands to the process manager. |
| `app.panel.toggle` | `Ctrl+Alt+P` | Show or hide the side panel. |
| `app.panel.focus` | `Ctrl+Alt+F` | Move keys to the side panel. |
| `app.panel.usage` | `Ctrl+Alt+U` | Open the side panel on Usage stats (again: close). |
| `app.session.new` | none | Start a new session. |
| `app.session.resume` | none | Resume a session. |
| `app.keys.help` | `Alt+/` | Show key bindings. |
| `app.history.search` | `Ctrl+R` | Search prompt history. |
| `app.thinking.toggle` | `Ctrl+T` | Show or fold thinking. |
| `app.thinking.cycle` | `Shift+Tab` | Next thinking level. |

### Transcript

| Action name | Default | What it does |
|---|---|---|
| `tui.altScreen.pageUp` | `PageUp` | Scroll up a page. |
| `tui.altScreen.pageDown` | `PageDown` | Scroll down a page. |
| `tui.altScreen.top` | `Ctrl+Home` | Scroll to the top. |
| `tui.altScreen.bottom` | `Ctrl+End` | Scroll to the bottom. |
| `tui.altScreen.previousPrompt` | `Ctrl+Up`, `Ctrl+Shift+Up` | Jump to the previous prompt. |
| `tui.altScreen.nextPrompt` | `Ctrl+Down`, `Ctrl+Shift+Down` | Jump to the next prompt. |
| `tui.altScreen.search` | `Ctrl+F`, `Ctrl+Shift+F` | Search the transcript. `Enter` older, `Shift+Enter` newer, `Esc` closes. |

### Editor

| Action name | Default | What it does |
|---|---|---|
| `tui.editor.cursorUp` | `Up` | Cursor up (history at the top). |
| `tui.editor.cursorDown` | `Down` | Cursor down (history at the bottom). |
| `tui.editor.cursorLeft` | `Left`, `Ctrl+B` | Cursor left. |
| `tui.editor.cursorRight` | `Right`, `Ctrl+F` | Cursor right. |
| `tui.editor.cursorWordLeft` | `Alt+Left`, `Ctrl+Left`, `Alt+B` | Word left. |
| `tui.editor.cursorWordRight` | `Alt+Right`, `Ctrl+Right`, `Alt+F` | Word right. |
| `tui.editor.cursorLineStart` | `Home`, `Ctrl+A` | Line start. |
| `tui.editor.cursorLineEnd` | `End`, `Ctrl+E` | Line end. |
| `tui.editor.deleteCharBackward` | `Backspace` | Delete left. |
| `tui.editor.deleteCharForward` | `Delete`, `Ctrl+D` | Delete right. |
| `tui.editor.deleteWordBackward` | `Ctrl+W`, `Alt+Backspace` | Delete word left. |
| `tui.editor.deleteWordForward` | `Alt+D`, `Alt+Delete` | Delete word right. |
| `tui.editor.deleteToLineStart` | `Ctrl+U` | Delete to line start. |
| `tui.editor.deleteToLineEnd` | `Ctrl+K` | Delete to line end. |
| `tui.editor.yank` | `Ctrl+Y` | Paste the last deleted text. |
| `tui.editor.yankPop` | `Alt+Y` | Cycle older deleted text. |
| `tui.editor.undo` | `Ctrl+-`, `Ctrl+_` | Undo. |
| `tui.input.newLine` | `Shift+Enter`, `Ctrl+J` | Insert a newline. |
| `tui.input.submit` | `Enter` | Send (steers while busy). |
| `tui.input.tab` | `Tab` | Complete. |

### Lists and pickers

| Action name | Default | What it does |
|---|---|---|
| `tui.select.up` | `Up`, `Ctrl+P` | Selection up. |
| `tui.select.down` | `Down`, `Ctrl+N` | Selection down. |
| `tui.select.pageUp` | `PageUp` | Selection page up. |
| `tui.select.pageDown` | `PageDown` | Selection page down. |
| `tui.select.confirm` | `Enter` | Confirm. |
| `tui.select.cancel` | `Esc`, `Ctrl+C` | Cancel. |

### Change a key

Create `~/.novacode/keybindings.json` (or `$NOVACODE_HOME/keybindings.json`). It is plain JSON, without comments. Each key is an action name; the value is one key or a list. Names and defaults follow pi, so a pi `keybindings.json` works as it is.

```json
{
  "app.message.followUp": "ctrl+enter",
  "app.model.select": ["ctrl+l", "alt+m"],
  "app.suspend": []
}
```

- An empty list removes all keys from that action.
- Key names: modifiers `ctrl`, `alt`, `shift` (also `super`), joined with `+`, then a key: a letter or symbol, `enter`, `escape`, `tab`, `backspace`, `delete`, `home`, `end`, `pageUp`, `pageDown`, `up`, `down`, `left`, `right`, `space` or `f1`…`f12`.
- Unknown action names and unreadable keys show a warning at start. A file that is not valid JSON is ignored with a warning.
- Run `/reload` (or `Alt+Shift+R`) to apply changes. `/keys` marks changed actions with `(custom)` and lists conflicts.

## Themes

`ui.theme` picks the colors.

| Value | Theme |
|---|---|
| `auto` (default) | Dark or light, by asking your terminal for its background color. If it does not answer, `COLORFGBG` is used, else dark. |
| `dark` or `catppuccin-mocha` | Catppuccin Mocha. |
| `light` or `catppuccin-latte` | Catppuccin Latte. |
| any other name | `~/.novacode/themes/<name>.json`, or `themes/<name>.json` in an installed [package](packages.md). |

A theme file only needs the tokens you want to change. The rest come from the built-in theme of the same brightness (set `"dark": false` to build on the light one).

```json
{
  "name": "mine",
  "dark": true,
  "syntax": "base16-ocean.dark",
  "tokens": {
    "accent": "#ff8800",
    "user": { "fg": "#89b4fa", "bold": true },
    "selection": { "bg": "#45475a" }
  }
}
```

A token is `"#rrggbb"` or an object with `fg`, `bg` and the flags `bold`, `italic`, `underline`, `dim`. Tokens:

`text`, `dim`, `accent`, `user`, `heading`, `code`, `code_block`, `link`, `quote`, `thinking`, `ok`, `warn`, `error`, `add`, `del`, `border`, `selection`, `toggle`, `match`, and `thinking_off`, `thinking_minimal`, `thinking_low`, `thinking_medium`, `thinking_high`, `thinking_xhigh`, `thinking_max`: the editor's border color at each thinking level (an extension's `editor` border wins).

`syntax` names the code-highlight theme. Bad tokens are skipped with a warning. Without truecolor (`COLORTERM=truecolor`), colors are mapped to the 256-color palette.

### Your message box

`ui.userMessage` styles how your prompts look in the transcript.

```json
{ "ui": { "userMessage": {
  "border": "single",
  "borderColor": "#585b70",
  "text": { "fg": "#cdd6f4", "bg": "#313244" },
  "paddingX": 2,
  "paddingY": 0
} } }
```

`border` is `rounded` (default), `single`, `heavy`, `double`, `ascii` or `none`. Padding is 0 to 8 (default 1 across, 0 down).

## Status line

`ui.statusline` sets rows of segments. `placement` is `below` (default) or `above` the editor. This example is the default layout:

```json
{ "ui": { "statusline": { "placement": "below", "rows": [
  { "left": ["activity"],
    "right": ["ext", "bg", "ctx", "cache", "cost", "model", "cwd", "git"] }
] } } }
```

Segments:

| Segment | Shows |
|---|---|
| `activity` | `ready`, a spinner while working, or what it waits for. |
| `model` | Model id (without the provider, like pi) and thinking level. |
| `thinking` | Thinking level alone. |
| `cwd`, `git` | Folder and branch. |
| `ctx` | Context window used, in percent. |
| `cache` | Prompt cache hit rate for the whole session: cached prompt tokens over all prompt tokens so far. The Usage tab has the last request's rate. |
| `usage_5h`, `usage_week` | Subscription usage windows. |
| `cost` | Money spent this session on API-key routes. |
| `turn_time`, `session_time` | Time of the current turn and of the session. |
| `bg` | Number of background processes. |
| `queue` | Queued messages. |
| `mcp` | MCP servers ready. |
| `agents` | `agents N` while N subagents are running; hidden otherwise. |
| `wtpath` | Name of the git worktree, when you are in one. |
| `ext` | All status segments from extensions (`ext:<key>` for one). |

A segment can also be an object: `{"id": "ctx", "format": "context {}", "style": "warn", "icon": "▣", "thresholds": [50, 70, 90]}`. `thresholds` colors a percentage: ok below the first number, warn below the second, code color below the third, error above. A row can set `sep` (default two spaces). On narrow screens, right-side segments drop from the end first. A segment with no value is skipped. Unknown segment names give a warning.

## Icons, images, speed and notifications

| Setting | Values | Effect |
|---|---|---|
| `ui.icons` | `nerd` (default), `unicode` | Use `unicode` if your font has no Nerd Font glyphs (panel icons). |
| `ui.images` | `auto` (default), `kitty`, `iterm2`, `sixel`, `halfblocks`, `off` | How images in the transcript are drawn. `auto` picks by terminal: Kitty, Ghostty and Konsole use Kitty graphics; WezTerm and iTerm2 use iTerm2; foot and mlterm use Sixel; anything else uses half-blocks. `off` shows a one-line placeholder. |
| `ui.slow` | bool | Fewer frames and a slower spinner. Default: on over SSH or when the machine load is above its core count. |
| `notifications.enabled` | bool, default true | When a turn ends, or novacode needs you, while you are not looking at it, send a terminal notification (OSC 777). In tmux it goes to every attached client, so it reaches you over SSH too. |

The terminal also decides some things through environment variables:

| Variable | Used for |
|---|---|
| `COLORFGBG` | Light or dark detection under `ui.theme: auto`, when the terminal doesn't answer |
| `COLORTERM` | `truecolor` turns on 24-bit colors |
| `TERM`, `TERM_PROGRAM`, `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `KONSOLE_VERSION`, `WEZTERM_EXECUTABLE` | Picking the image protocol under `ui.images: auto` |
| `WAYLAND_DISPLAY` | Clipboard tools: `wl-copy` / `wl-paste` instead of `xclip` |
| `SSH_CONNECTION`, `SSH_TTY` | Count as a slow terminal (see `ui.slow`) |
| `TMUX`, `TMUX_PANE` | tmux panel host and `tmux.agentState`. See [automation](automation.md#tmux-agent-state) |
| `VISUAL`, `EDITOR` | `Ctrl+G` external editor |

novacode also reports its state to herdr when it runs inside it ([automation](automation.md#herdr)).

See [config](config.md) for the full settings table.

## See also

- [sessions](sessions.md): resume, tree, rewind and export.
- [config](config.md): every `ui.*` setting.
- [extensions](extensions.md): extension commands, panel tabs and status segments.
- [skills](skills.md): `/skill:<name>` and templates.
- [security](security.md): approval cards and modes.
