# TUI style guide

Every screen in the terminal UI follows these rules: the transcript, the side panel, dialogs, pickers, the pager and the status line. A screen that breaks one is a bug. The novacode look stays as it is; this guide only makes it consistent.

## Focus

- Exactly one part of the screen has focus: the **input**, the **side panel** or an open **dialog**.
- Only the focused part draws its frame in the `accent` colour. Every other frame uses `border` grey.
- The input frame is the one exception: while it has focus it keeps its thinking-level colour (`levels`). It turns `border` grey when focus is elsewhere.
- A click moves focus. Esc always returns focus to the input.
- An unfocused panel still shows its selected row, dimly.

## Spacing

- One blank line between transcript items. No blank lines inside an item.
- Detail lines sit under their main line, indented 2 cells.
- Frames have 1 cell of padding on the left and right.
- Panel rows use fixed columns: status icon (2 cells reserved), title (shrinks first), project tag (has a maximum width), age (right-aligned, at least a 2-cell gap before it).
- The selected row has an `accent` pointer `▸` in the cell right before its status icon, on its main line only, and a bold title. No background. Every row reserves that cell (other rows show the guide's `─` there), so nothing shifts when the selection moves. Unfocused, the pointer is `dim`, the title stays bold and no other text dims.

## Glyphs

Plain Unicode is the default. Nerd Font icons are opt-in through the `ui.icons` setting. Every screen must work with the plain set on a stock terminal.

- List cursor `▸`, tool marker `●`, separator `·`, cut text `…`.
- Status icons differ by shape *and* colour:

| State | Plain | Colour |
|---|---|---|
| running | `◐` | `warn` |
| starting | `◌` | `warn` |
| waiting | `⏸` | `accent` |
| queued | `○` | `dim` |
| done | `✓` | `ok` |
| failed | `✗` | `error` |
| stopped | `■` | `dim` |

- Nerd Font icons are drawn 2 cells wide by some terminals. Layouts reserve 2 cells for any icon.
- Don't add `[x]`, `why:` or similar prefixes where an icon or colour already says it.

## Colour roles

Use the roles from `Theme` (`crates/novacode-tui/src/theme.rs`), never raw colours.

- `text`: main lines. `dim`: detail lines, hints, fold lines, the unfocused selection pointer.
- `accent`: the focused frame, the list cursor, links to act on. `border`: unfocused frames.
- `selection`: mouse text selection and picker highlights. `matched`: the current search match.
- `ok`, `warn`, `error`: state only (done, running or caution, failed). `add`, `del`: diffs.
- `heading`, `code`, `code_block`, `link`, `quote`, `thinking`, `user`, `toggle`: their content type.
- Every screen must stay readable on a light background and with the 256-colour fallback.

## Key hints

- Format: `key action · key action`, in `dim`, on one line. Example: `↑↓ move · enter open · esc back`.
- All hints are drawn by the one shared hint helper from (key, action) pairs. Key labels come from the keymap, so rebound keys show the user's key.
- A hint shows only when its key works at that moment. No "back" on the first question; the note key only on the selected choice.
- The approval card's letters `y`, `a`, `A`, `n` are fixed and also go through the helper.
- When the line does not fit, drop the least important hints from the end. Never wrap a hint line.

## Text lengths

- Labels: 3 words or fewer.
- Hints: one line.
- Dialog sentences: 12 words or fewer.

## Complete but structured

Shortening never drops information; it only restructures it.

- Each item has one bright main line, with dim, indented detail lines under it that wrap at word breaks. Never cut a word in the middle.
- Panel detail lines wrap to at most two lines.
- Fold only where novacode already folds: tool output, thinking and turns. A fold line always says what it hides, e.g. `… (42 more lines)`.

## Mouse

- A click selects a row in the ask form and pickers. A double-click confirms.
- The wheel scrolls the pager, the process view, the side chat and an ask form that does not fit.
- A click in the input moves the cursor there.
- The approval card ignores the mouse, so a stray click never grants a permission.
- Text selection and copy keep working everywhere else.
