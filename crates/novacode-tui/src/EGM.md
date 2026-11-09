# novacode-tui/src

## Overview
Fullscreen terminal UI (layer L9) built on `ratatui` and `crossterm`. It speaks protocol types only: `AgentEvent`s and `UiEvent`s come in, `Cmd`s go out, so the same UI can later sit on an attach socket (L8.13). It draws the transcript, prompt editor, dialogs, side panel and status line.

## Interface
- `lib.rs`: `async run(init, events, ui, cmds) -> io::Result<Exit>` (enters and restores the terminal); `async run_in(&mut Term, ...)` (keeps the terminal between sessions); re-exports `App`, `Cmd`, `Exit`, `Init`, `UiEvent`.
- `app.rs`: `App` (`new`, `on_agent`, `on_ui`, `on_event`, `on_key`, `tick`, `pace`, `draw`, `finish_frame`, `editor_text`, `set_editor_text`, ...), `Init`, `Cmd`, `UiEvent`, `Exit` (`Quit`, `Resume`, `New`, `Branch`, `Rewind`, `Reload`, `Restart`), `BranchAction`, `COMMANDS` (slash commands).
- `transcript.rs` (L9.1, L9.7, L9.8): `Transcript`, `Turn`, `Item`, `Tool`, `Img`, `View`, `Thinking`, `SPINNER`, `NEW_SINCE`, `summary(name, args)`, `lay_say`.
- `editor.rs` (L9.5): `Editor` (history, kill ring, undo, paste markers, history search), `Action`.
- `keys.rs` (L9.10): `A` (actions), `Scope`, `Def`/`DEFS`, `Combo`, `parse`, `combo`, `name`, `is`, `Binding`, `Keymap` (`new(overrides)`, `lookup`, `keys`, `conflicts`).
- `dialog.rs` (L9.13, L6.1): `Picker`, `PickItem`, `Picked`, `fuzzy`, `ApprovalCard`, `ApproveReq`, `Answer`, `Pager` (incl. `keys_help`), `frame`, `centered`.
- `ask.rs` (L5.21): `AskForm`, `AskReq`, `Done`. `btw.rs`: `BtwChat`, `BtwKey`. `login.rs`: `LoginView`, `LoginStep`, `LoginKey`. `procs.rs` (L5.11): `ProcView`, `ProcPopup`, `order`, `plain`, `output_lines`, `header`.
- `panel.rs` (L9.16): `Panel` (`on_agent`, `apply(ext, &UiOp)`, `click`, `wheel`, `tick`, `toggle`, `set_procs`, `set_tasks`, `set_needs`, `set_diffstat`, `quota`, ...), `Tab`, `Source`, `HostInfo`, `NeedRow`, `TaskRow`, `PanelAction`, `MIN_WIDTH = 40`, `MIN_SCREEN = 100`, `now_ms`, `secs_pub`, `status_glyph`, `clock`, `spans`.
- `statusline.rs` (L9.12): `Config::parse` (`ui.statusline` setting), `SEGMENTS`, `Seg`, `Row`, `Value_`, `render`, `compactions`.
- `markdown.rs`: `render(md, width, theme, highlight)`. `highlight.rs` (feature `highlight`): `lines(code, lang, theme, ansi256)`. `images.rs` (L9.14): `dimensions`, `Images` (feature `images`: `detect`, `halfblocks`, `render`). `wrap.rs`: `sanitize`, `width`, `wrap`, `truncate`, `plain`.
- `theme.rs` (L9.11): `Theme` (`load(name, dir, dark_bg)`, `token`, `apply_user_message`), `UserBox`, `to_256`, `dark_from_osc11`.
- `term.rs` (L9.2): `Term`, `enter`, `restore`, `install_panic_hook`, `suspend`, `mark_ui_thread`, `query_background`, `editor_command`, `external_editor`, `open_in_editor`, `open(url)`, `copy(text)`, `paste() -> Option<Pasted>`.
- `prompt.rs` (L6.3, L11.6): `select(title, body, items)` — blocking fullscreen choice (trust prompt, onboarding) before the main UI.

## Invariants
- The UI depends only on `novacode-types`; it never calls the session directly, only sends `Cmd`s.
- The run loop runs on the thread that called it (the runtime's `block_on`), never on a worker task. Input is handled first; redraw only when something changed, at most 60 fps (16 ms frame), 3× slower over SSH or on a loaded machine (L9.17).
- Terminal restore runs on exit, on panic and around Ctrl+Z, so the shell is never left in raw mode.
- A finished turn is laid out once per width and cached; only the running turn is re-laid out. Code blocks are highlighted once per width.
- Keybinding names and defaults follow pi, so a pi `keybindings.json` works unchanged; novacode-only actions use the `app.` prefix.
- A user theme only needs the tokens it changes; the rest come from the bundled theme of the same brightness (catppuccin mocha / latte).
- Without the `images` feature, images are one-line `[image W×H]` placeholders. Default features: `highlight`, `images`.
- `/btw` questions are never stored in the session.

## Side Effects
- Terminal: raw mode, alternate screen, OSC 11 background query (150 ms timeout), image protocol queries (Kitty, iTerm2, Sixel), OSC 52 clipboard writes.
- Processes: clipboard helpers (`wl-copy` / `wl-paste` on Wayland, `xclip` for paste on X11, `powershell` for paste on Windows), `xdg-open` / `open` / `rundll32` (Windows) for URLs, `$VISUAL`/`$EDITOR` (else the first of nvim, nano, `emacs -nw`, vim found on `PATH`; if none is found, `notepad` on Windows or `vi` elsewhere) for external editing (temp file written then removed), `tmux` commands, `sh`/`cmd` on suspend.
- Disk: reads theme JSON from `~/.novacode/themes/`, reads directories for path completion, writes and removes temp files for the external editor and pasted images.
- Reads environment: `NOVACODE_HOME`, `TMUX`, `HOME`, `WAYLAND_DISPLAY`, `COLORFGBG`, `COLORTERM`, `TERM`, `PATH`, `VISUAL`, `EDITOR`.

## Verification
- Run: `cargo test -p novacode-tui`
