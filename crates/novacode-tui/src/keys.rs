//! Named-action keybindings (L9.10). Names and defaults follow pi, so a pi
//! `keybindings.json` works as `~/.novacode/keybindings.json` unchanged.
//! Novacode-only actions use the `app.` prefix too.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    App,
    Editor,
    Scroll,
    Select,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A {
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DeleteBack,
    DeleteForward,
    DeleteWordBack,
    DeleteWordForward,
    DeleteToLineStart,
    DeleteToLineEnd,
    Yank,
    YankPop,
    Undo,
    NewLine,
    Submit,
    Tab,
    Interrupt,
    Clear,
    Exit,
    Suspend,
    ModelSelect,
    ToolsExpand,
    TurnsToggle,
    ExternalEditor,
    FollowUp,
    MessageCopy,
    InputCopy,
    Dequeue,
    Reload,
    PasteImage,
    Background,
    PanelToggle,
    PanelFocus,
    SessionNew,
    SessionResume,
    KeysHelp,
    HistorySearch,
    ThinkingToggle,
    ThinkingCycle,
    PageUp,
    PageDown,
    Top,
    Bottom,
    PrevPrompt,
    NextPrompt,
    Search,
    SelUp,
    SelDown,
    SelPageUp,
    SelPageDown,
    SelConfirm,
    SelCancel,
}

pub struct Def {
    pub action: A,
    pub name: &'static str,
    pub scope: Scope,
    pub defaults: &'static [&'static str],
    pub desc: &'static str,
}

macro_rules! defs {
    ($( $a:ident, $name:literal, $scope:ident, [$($k:literal),*], $desc:literal; )*) => {
        pub const DEFS: &[Def] = &[ $( Def { action: A::$a, name: $name, scope: Scope::$scope, defaults: &[$($k),*], desc: $desc } ),* ];
    };
}

defs! {
    Interrupt, "app.interrupt", App, ["escape"], "Abort the running turn (double: clear editor)";
    Clear, "app.clear", App, ["ctrl+c"], "Clear editor, abort, or quit on second press";
    Exit, "app.exit", App, ["ctrl+d"], "Quit when the editor is empty";
    Suspend, "app.suspend", App, ["ctrl+z"], "Suspend to the shell";
    ModelSelect, "app.model.select", App, ["ctrl+l"], "Pick a model";
    ToolsExpand, "app.tools.expand", App, ["ctrl+o"], "Show every tool's output in full, or short again";
    TurnsToggle, "app.turns.toggle", App, ["alt+o"], "Expand or collapse finished turns";
    ExternalEditor, "app.editor.external", App, ["ctrl+g"], "Edit the prompt in $EDITOR";
    FollowUp, "app.message.followUp", App, ["alt+enter"], "Queue as a follow-up";
    MessageCopy, "app.message.copy", App, ["ctrl+x"], "Copy the last reply";
    InputCopy, "app.input.copy", App, ["alt+c"], "Copy what's in the input box";
    Dequeue, "app.message.dequeue", App, ["alt+up"], "Take back the last queued message into the editor";
    Reload, "app.reload", App, ["alt+shift+r"], "Reload settings, keys, theme, skills, extensions, MCP servers and context files";
    PasteImage, "app.clipboard.pasteImage", App, ["ctrl+v"], "Paste an image (or text) from the clipboard";
    Background, "app.process.background", App, ["ctrl+alt+b"], "Move running commands to the process manager";
    PanelToggle, "app.panel.toggle", App, ["ctrl+alt+p"], "Show or hide the side panel";
    PanelFocus, "app.panel.focus", App, ["ctrl+alt+f"], "Move keys to the side panel (Esc returns)";
    SessionNew, "app.session.new", App, [], "Start a new session";
    SessionResume, "app.session.resume", App, [], "Resume a session";
    KeysHelp, "app.keys.help", App, ["alt+/"], "Show keybindings";
    HistorySearch, "app.history.search", App, ["ctrl+r"], "Search prompt history";
    ThinkingToggle, "app.thinking.toggle", App, ["ctrl+t"], "Show or fold thinking";
    ThinkingCycle, "app.thinking.cycle", App, ["shift+tab"], "Next thinking level";
    PageUp, "tui.altScreen.pageUp", Scroll, ["pageUp"], "Scroll up a page";
    PageDown, "tui.altScreen.pageDown", Scroll, ["pageDown"], "Scroll down a page";
    Top, "tui.altScreen.top", Scroll, ["ctrl+home"], "Scroll to the top";
    Bottom, "tui.altScreen.bottom", Scroll, ["ctrl+end"], "Scroll to the bottom";
    PrevPrompt, "tui.altScreen.previousPrompt", Scroll, ["ctrl+up", "ctrl+shift+up"], "Jump to the previous prompt";
    NextPrompt, "tui.altScreen.nextPrompt", Scroll, ["ctrl+down", "ctrl+shift+down"], "Jump to the next prompt";
    Search, "tui.altScreen.search", Scroll, ["ctrl+f", "ctrl+shift+f"], "Search the transcript";
    CursorUp, "tui.editor.cursorUp", Editor, ["up"], "Cursor up (history at the top)";
    CursorDown, "tui.editor.cursorDown", Editor, ["down"], "Cursor down (history at the bottom)";
    CursorLeft, "tui.editor.cursorLeft", Editor, ["left", "ctrl+b"], "Cursor left";
    CursorRight, "tui.editor.cursorRight", Editor, ["right", "ctrl+f"], "Cursor right";
    WordLeft, "tui.editor.cursorWordLeft", Editor, ["alt+left", "ctrl+left", "alt+b"], "Word left";
    WordRight, "tui.editor.cursorWordRight", Editor, ["alt+right", "ctrl+right", "alt+f"], "Word right";
    LineStart, "tui.editor.cursorLineStart", Editor, ["home", "ctrl+a"], "Line start";
    LineEnd, "tui.editor.cursorLineEnd", Editor, ["end", "ctrl+e"], "Line end";
    DeleteBack, "tui.editor.deleteCharBackward", Editor, ["backspace"], "Delete left";
    DeleteForward, "tui.editor.deleteCharForward", Editor, ["delete", "ctrl+d"], "Delete right";
    DeleteWordBack, "tui.editor.deleteWordBackward", Editor, ["ctrl+w", "alt+backspace"], "Delete word left";
    DeleteWordForward, "tui.editor.deleteWordForward", Editor, ["alt+d", "alt+delete"], "Delete word right";
    DeleteToLineStart, "tui.editor.deleteToLineStart", Editor, ["ctrl+u"], "Delete to line start";
    DeleteToLineEnd, "tui.editor.deleteToLineEnd", Editor, ["ctrl+k"], "Delete to line end";
    Yank, "tui.editor.yank", Editor, ["ctrl+y"], "Paste the last deleted text";
    YankPop, "tui.editor.yankPop", Editor, ["alt+y"], "Cycle older deleted text";
    Undo, "tui.editor.undo", Editor, ["ctrl+-", "ctrl+_"], "Undo";
    NewLine, "tui.input.newLine", Editor, ["shift+enter", "ctrl+j"], "Insert a newline";
    Submit, "tui.input.submit", Editor, ["enter"], "Send (steers while busy)";
    Tab, "tui.input.tab", Editor, ["tab"], "Complete";
    SelUp, "tui.select.up", Select, ["up", "ctrl+p"], "Selection up";
    SelDown, "tui.select.down", Select, ["down", "ctrl+n"], "Selection down";
    SelPageUp, "tui.select.pageUp", Select, ["pageUp"], "Selection page up";
    SelPageDown, "tui.select.pageDown", Select, ["pageDown"], "Selection page down";
    SelConfirm, "tui.select.confirm", Select, ["enter"], "Confirm";
    SelCancel, "tui.select.cancel", Select, ["escape", "ctrl+c"], "Cancel";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Combo {
    code: KeyCode,
    mods: KeyModifiers,
}

/// `ctrl+shift+f`, `alt+enter`, `pageUp`, `ctrl+-`, `alt+/`.
pub fn parse(s: &str) -> Option<Combo> {
    let s = s.trim().to_lowercase();
    let mut mods = KeyModifiers::NONE;
    let mut rest = s.as_str();
    loop {
        let (m, r) = match rest.split_once('+') {
            Some((m, r)) if !r.is_empty() => (m, r),
            _ => break,
        };
        mods |= match m {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" | "cmd" => KeyModifiers::SUPER,
            _ => return None,
        };
        rest = r;
    }
    let code = match rest {
        "enter" | "return" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        f if f.len() > 1 && f.starts_with('f') => KeyCode::F(f[1..].parse().ok()?),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next()?),
        _ => return None,
    };
    Some(normalize(code, mods))
}

/// Terminals differ on whether Shift shows on punctuation and capitals; compare without it.
fn normalize(code: KeyCode, mut mods: KeyModifiers) -> Combo {
    let code = match code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => {
            mods |= KeyModifiers::SHIFT;
            KeyCode::Char(c.to_ascii_lowercase())
        }
        KeyCode::Char(c) if !c.is_alphanumeric() && c != ' ' => {
            mods -= KeyModifiers::SHIFT;
            KeyCode::Char(c)
        }
        KeyCode::BackTab => {
            mods |= KeyModifiers::SHIFT;
            KeyCode::Tab
        }
        c => c,
    };
    Combo {
        code,
        mods: mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER),
    }
}

pub fn combo(k: &KeyEvent) -> Combo {
    normalize(k.code, k.modifiers)
}

/// A key as text (`ctrl+x`, `enter`, `a`), for panel keys and extensions.
pub fn name(k: &KeyEvent) -> String {
    let c = combo(k);
    let mut s = String::new();
    for (m, n) in [
        (KeyModifiers::CONTROL, "ctrl+"),
        (KeyModifiers::ALT, "alt+"),
        (KeyModifiers::SHIFT, "shift+"),
    ] {
        if c.mods.contains(m) {
            s.push_str(n);
        }
    }
    s.push_str(&match c.code {
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageUp".into(),
        KeyCode::PageDown => "pageDown".into(),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        _ => "?".into(),
    });
    s
}

/// `k` is the key written as `s` (an extension shortcut).
pub fn is(k: &KeyEvent, s: &str) -> bool {
    parse(s).is_some_and(|c| c == combo(k))
}

pub struct Binding {
    pub def: &'static Def,
    pub keys: Vec<String>,
    combos: Vec<Combo>,
    pub custom: bool,
}

pub struct Keymap {
    pub bindings: Vec<Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new(&BTreeMap::new()).0
    }
}

impl Keymap {
    /// Overrides map an action name to one key or a list; unknown names and keys become warnings.
    pub fn new(overrides: &BTreeMap<String, serde_json::Value>) -> (Self, Vec<String>) {
        let mut warn = Vec::new();
        for name in overrides.keys() {
            if !DEFS.iter().any(|d| d.name == name) {
                warn.push(format!("keybindings: unknown action `{name}`"));
            }
        }
        let bindings = DEFS
            .iter()
            .map(|def| {
                let (keys, custom): (Vec<String>, bool) = match overrides.get(def.name) {
                    Some(serde_json::Value::String(s)) => (vec![s.clone()], true),
                    Some(serde_json::Value::Array(a)) => {
                        (a.iter().filter_map(|v| v.as_str().map(String::from)).collect(), true)
                    }
                    _ => (def.defaults.iter().map(|s| s.to_string()).collect(), false),
                };
                let combos = keys
                    .iter()
                    .filter_map(|k| {
                        let c = parse(k);
                        if c.is_none() {
                            warn.push(format!("keybindings: can't read key `{k}` for {}", def.name));
                        }
                        c
                    })
                    .collect();
                Binding {
                    def,
                    keys,
                    combos,
                    custom,
                }
            })
            .collect();
        (Self { bindings }, warn)
    }

    /// First action in `scopes` order bound to this key.
    pub fn lookup(&self, k: &KeyEvent, scopes: &[Scope]) -> Option<A> {
        let c = combo(k);
        scopes.iter().find_map(|s| {
            self.bindings
                .iter()
                .find(|b| b.def.scope == *s && b.combos.contains(&c))
                .map(|b| b.def.action)
        })
    }

    /// Keys bound to `a`, for hints, e.g. `ctrl+l`.
    pub fn keys(&self, a: A) -> String {
        self.bindings
            .iter()
            .find(|b| b.def.action == a)
            .and_then(|b| b.keys.first().cloned())
            .unwrap_or_default()
    }

    /// Keys bound to two actions of the same scope (for `doctor` and the help popup).
    pub fn conflicts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, a) in self.bindings.iter().enumerate() {
            for b in &self.bindings[i + 1..] {
                // Select keys only work inside dialogs, where app keys don't.
                let main = |s: Scope| s != Scope::Select;
                let shared = a.def.scope == b.def.scope
                    || (main(a.def.scope)
                        && main(b.def.scope)
                        && (a.def.scope == Scope::App || b.def.scope == Scope::App));
                if shared && let Some(k) = a.combos.iter().position(|c| b.combos.contains(c)) {
                    // Known, intended overlaps: app.exit only fires on an empty editor.
                    if (a.def.action, b.def.action) == (A::Exit, A::DeleteForward) {
                        continue;
                    }
                    out.push(format!(
                        "`{}` is bound to both {} and {}",
                        a.keys[k], a.def.name, b.def.name
                    ));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn parses_and_matches() {
        let km = Keymap::default();
        let all = [Scope::App, Scope::Scroll, Scope::Editor];
        assert_eq!(
            km.lookup(&key(KeyCode::Char('l'), KeyModifiers::CONTROL), &all),
            Some(A::ModelSelect)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Enter, KeyModifiers::ALT), &all),
            Some(A::FollowUp)
        );
        assert_eq!(
            km.lookup(&key(KeyCode::Enter, KeyModifiers::SHIFT), &all),
            Some(A::NewLine)
        );
        // Ctrl+_ arrives with SHIFT on most terminals.
        let undo = key(KeyCode::Char('_'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(km.lookup(&undo, &all), Some(A::Undo));
        assert_eq!(
            km.lookup(&key(KeyCode::Char('/'), KeyModifiers::ALT), &all),
            Some(A::KeysHelp)
        );
        assert_eq!(km.lookup(&key(KeyCode::Char('x'), KeyModifiers::NONE), &all), None);
        assert_eq!(parse("ctrl+shift+F"), parse("ctrl+shift+f"));
        assert!(parse("hyper+x").is_none());
        assert!(km.conflicts().is_empty(), "{:?}", km.conflicts());
    }

    #[test]
    fn pi_overrides_carry_over() {
        // The user's pi keybindings.json.
        let o: BTreeMap<String, serde_json::Value> = serde_json::from_str(
            r#"{"tui.editor.deleteWordBackward":["ctrl+w","alt+backspace","ctrl+backspace"],
                "tui.altScreen.top":["shift+home"],"tui.altScreen.bottom":["shift+end"],"nope":"x"}"#,
        )
        .unwrap();
        let (km, warn) = Keymap::new(&o);
        assert_eq!(warn, ["keybindings: unknown action `nope`"]);
        let s = [Scope::Scroll, Scope::Editor];
        assert_eq!(
            km.lookup(&key(KeyCode::Backspace, KeyModifiers::CONTROL), &s),
            Some(A::DeleteWordBack)
        );
        assert_eq!(km.lookup(&key(KeyCode::Home, KeyModifiers::SHIFT), &s), Some(A::Top));
        assert_eq!(km.lookup(&key(KeyCode::Home, KeyModifiers::CONTROL), &s), None);
        assert!(
            km.bindings
                .iter()
                .any(|b| b.custom && b.def.name == "tui.altScreen.top")
        );
    }
}
