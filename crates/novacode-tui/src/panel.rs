//! The side panel (L9.16): tabs of node sources or extension-painted regions.
//! Built-in tabs (Agent, Processes, Usage, Timing, Files) are fed from the same
//! [`AgentEvent`]s every window sees, so an attached window or the tmux panel pane
//! (L9.18) shows the same thing as the session's own window.

use crate::theme::Theme;
use novacode_types::ui::{Line as ULine, Node, UiOp};
use novacode_types::{AgentEvent, Message, StopReason, Usage};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Hide the panel this long after everything finished (unless pinned).
const HIDE_AFTER: Duration = Duration::from_secs(5);
pub const MIN_WIDTH: u16 = 40;
/// Below this terminal width the panel stays hidden (pi's status-panel rule).
pub const MIN_SCREEN: u16 = 100;

/// Session facts from the host for the Usage stats tab.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct HostInfo {
    /// MCP servers: (name, `ready` / `connecting` / `failed: why`, tools).
    #[serde(default)]
    pub servers: Vec<(String, String, usize)>,
    /// What loaded: (`skills`, 67), (`extensions`, 1), …
    #[serde(default)]
    pub loaded: Vec<(String, usize)>,
    /// Where the context goes, tokens: (`system prompt`, …), (`tools`, …), (`messages`, …).
    #[serde(default)]
    pub parts: Vec<(String, u64)>,
}

impl HostInfo {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub title: Option<String>,
    pub order: f64,
    pub newest_first: bool,
    /// (done, total): drawn as a bar after the title.
    pub progress: Option<(usize, usize)>,
    /// Insertion order; publishing an existing id replaces it in place.
    pub nodes: Vec<Node>,
}

/// One drawn node row with its tree guides (pi panel viewer style).
struct Row<'a> {
    node: &'a Node,
    /// Section heading drawn above it (first row of a titled source).
    title: Option<(&'a str, Option<(usize, usize)>)>,
    /// First row of a later source: a rule goes above it.
    rule: bool,
    /// `├─`/`└─` with the ancestors' `│ ` before it.
    guide: String,
    /// The guide for its detail lines.
    below: String,
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub name: String,
    pub title: String,
    pub priority: i32,
    pub width: Option<u16>,
    /// The extension that owns it (`None` = built in): keys and actions go there.
    pub owner: Option<String>,
    pub sources: Vec<Source>,
    /// A region tab: lines the extension paints.
    pub region: Option<Vec<ULine>>,
}

impl Tab {
    fn new(name: &str, title: &str, priority: i32) -> Self {
        Tab {
            name: name.into(),
            title: title.into(),
            priority,
            width: None,
            owner: None,
            sources: Vec::new(),
            region: None,
        }
    }

    fn source(&mut self, id: &str) -> &mut Source {
        if let Some(i) = self.sources.iter().position(|s| s.id == id) {
            return &mut self.sources[i];
        }
        self.sources.push(Source {
            id: id.into(),
            title: None,
            order: 0.0,
            newest_first: false,
            progress: None,
            nodes: Vec::new(),
        });
        self.sources.last_mut().unwrap()
    }

    fn empty(&self) -> bool {
        self.region.is_none() && self.sources.iter().all(|s| s.nodes.is_empty())
    }
}

/// What a key in the focused panel asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelAction {
    /// Run a `/command`.
    Command(String),
    /// Open a file from the Files tab in the editor, diffed against the session start.
    OpenFile(String),
    /// Show a process's output in the `/ps` popup.
    OpenProc(String),
    /// Tell the owning extension: (ext, tab, node id, action or key).
    Ext {
        ext: String,
        tab: String,
        node: Option<String>,
        action: String,
    },
}

pub struct Panel {
    pub tabs: Vec<Tab>,
    pub shown: Option<String>,
    /// Toggled by hand: stays as the user left it until toggled again.
    pub pinned: Option<bool>,
    pub visible: bool,
    pub focused: bool,
    /// Selected row in the shown node tab.
    pub sel: usize,
    idle_since: Option<Instant>,
    /// Nerd Font glyphs (`ui.icons`, default nerd) instead of plain Unicode.
    pub nerd: bool,
    /// Inner size at the last draw (sent with region keys).
    pub size: (u16, u16),
    stats: Stats,
    /// Agent-tab work was unfinished at the last tick: new work re-opens a panel the
    /// user closed by hand.
    was_pending: bool,
    /// `ui.panel.auto: false` (or the tmux pane hosts it): never opens by itself.
    pub never_auto: bool,
    /// Processes from the host (L5.11): the Processes tab lists them, the `/ps` popup shows one.
    pub procs: Vec<crate::procs::ProcView>,
    /// Session facts from the host (Usage stats tab).
    pub host: HostInfo,
    /// Runs started so far: a subagent belongs to the run (user prompt) it was spawned in.
    run: u32,
    agent_set: BTreeMap<String, u32>,
    /// Files tab: (+added, -removed) lines since the session started, from the host.
    diffstat: BTreeMap<String, (u64, u64)>,
    /// Where the last draw put things, for the mouse: tab labels (x from, x to, name),
    /// the body, and which row each body line shows.
    tab_spans: Vec<(u16, u16, u16, String)>,
    tab_row: u16,
    body: Rect,
    line_rows: Vec<Option<usize>>,
}

#[derive(Default)]
struct Stats {
    started: Option<Instant>,
    turn_at: Option<Instant>,
    replied_at: Option<Instant>,
    task: Task,
    tasks: Vec<Task>,
    settled_at: Option<Instant>,
    usage_total: Usage,
    last: Option<Usage>,
    /// Per request: (% of the prompt from cache, $ cost or output tokens without a price).
    requests: Vec<(f64, f64)>,
    window: u64,
    price: Option<novacode_types::Cost>,
    procs: BTreeMap<String, (String, Option<String>)>,
    files: BTreeMap<String, &'static str>,
}

#[derive(Default, Clone)]
struct Task {
    model: Duration,
    tools: Duration,
    turns: u32,
    idle: Duration,
    wall: Duration,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn node(id: &str, label: impl Into<String>) -> Node {
    Node {
        id: id.into(),
        parent: None,
        label: label.into(),
        tag: None,
        status: None,
        icon: None,
        dim: false,
        detail: Vec::new(),
        time: None,
        order: None,
        actions: BTreeMap::new(),
    }
}

pub fn secs_pub(d: Duration) -> String {
    secs(d)
}

fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 60.0 {
        format!("{}m{:02}s", s as u64 / 60, s as u64 % 60)
    } else {
        format!("{s:.1}s")
    }
}

impl Default for Panel {
    fn default() -> Self {
        let mut agent = Tab::new("agent", "Agent", 20);
        for (id, title, order, newest) in [
            ("goal", "Goal", 1.0, false),
            ("todos", "Todos", 5.0, false),
            ("subagents", "Subagents", 10.0, true),
        ] {
            let s = agent.source(id);
            s.title = Some(title.into());
            s.order = order;
            s.newest_first = newest;
        }
        Panel {
            tabs: vec![
                agent,
                Tab::new("processes", "Processes", 15),
                Tab::new("usage", "Usage stats", 5),
                Tab::new("timing", "Timing", 4),
                Tab::new("files", "Files", 3),
            ],
            shown: Some("agent".into()),
            pinned: None,
            visible: false,
            focused: false,
            sel: 0,
            idle_since: None,
            nerd: true,
            size: (0, 0),
            stats: Stats::default(),
            was_pending: false,
            never_auto: false,
            procs: Vec::new(),
            host: HostInfo::default(),
            run: 0,
            agent_set: BTreeMap::new(),
            diffstat: BTreeMap::new(),
            tab_spans: Vec::new(),
            tab_row: 0,
            body: Rect::default(),
            line_rows: Vec::new(),
        }
    }
}

impl Panel {
    fn tab(&mut self, name: &str) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.name == name)
    }

    /// New process list from the host; keeps the shown process if it is still there.
    pub fn set_procs(&mut self, mut v: Vec<crate::procs::ProcView>) {
        crate::procs::order(&mut v);
        self.procs = v;
        self.refresh_procs();
    }

    /// Processes tab: one row per process, the host's list when it has sent one.
    /// Enter or a click opens its output.
    fn refresh_procs(&mut self) {
        let now = now_ms();
        let known: Vec<&str> = self.procs.iter().map(|p| p.id.as_str()).collect();
        // Started or ended since the host's last list: from the events.
        let mut nodes: Vec<Node> = self
            .stats
            .procs
            .iter()
            .rev()
            .filter(|(id, _)| !known.contains(&id.as_str()))
            .map(|(id, (name, end))| {
                let mut n = node(id, name.clone());
                n.tag = Some(id.clone());
                n.status = Some(end.clone().unwrap_or_else(|| "running".into()));
                n.dim = end.is_some();
                n.actions.insert("enter".into(), "output".into());
                n
            })
            .collect::<Vec<_>>();
        nodes.extend(self.procs.iter().enumerate().map(|(i, p)| {
            let mut n = node(&p.id, p.name.clone());
            n.tag = Some(p.id.clone());
            n.status = Some(p.status.clone());
            n.dim = p.status != "running";
            n.order = Some(i as f64);
            let mut state = p.state.clone();
            if p.status == "running" {
                state += &format!(" · {}", clock(now.saturating_sub(p.started_ms)));
            }
            n.detail = vec![format!("{state} · $ {}", p.command)];
            let last = p
                .output
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty() && *l != "[stderr]");
            if let Some(l) = last {
                let plain: String = crate::procs::plain(l);
                n.detail.push(format!("› {}", plain.trim()));
            }
            n.actions.insert("enter".into(), "output".into());
            n
        }));
        self.set_nodes("processes", "procs", nodes);
    }

    /// The Processes tab is on screen: the host's list should be kept fresh.
    pub fn procs_shown(&self) -> bool {
        self.visible && self.shown.as_deref() == Some("processes")
    }

    /// Subagents still running (the `agents` status segment).
    pub fn live_subagents(&self) -> usize {
        let tab = self.tabs.iter().find(|t| t.name == "agent");
        tab.and_then(|t| t.sources.iter().find(|s| s.id == "subagents"))
            .map_or(0, |s| s.nodes.iter().filter(|n| !n.dim).count())
    }

    /// The oldest prompt whose subagents are shown: the oldest one with a subagent
    /// still running, else the newest that spawned any.
    fn agent_floor(&self) -> u32 {
        let tab = self.tabs.iter().find(|t| t.name == "agent");
        let nodes = tab
            .and_then(|t| t.sources.iter().find(|s| s.id == "subagents"))
            .map(|s| &s.nodes);
        let set = |n: &Node| self.agent_set.get(&n.id).copied().unwrap_or(0);
        let all = nodes.into_iter().flatten();
        let newest = all.clone().map(set).max().unwrap_or(0);
        all.filter(|n| !n.dim).map(set).min().unwrap_or(newest).min(newest)
    }

    /// Open the panel on `name`, focused (Ctrl+Alt+U for Usage stats); again closes it.
    pub fn show_tab(&mut self, name: &str) {
        if self.visible && self.focused && self.shown.as_deref() == Some(name) {
            self.toggle();
            return;
        }
        self.pinned = Some(true);
        self.visible = true;
        self.focused = true;
        self.shown = Some(name.into());
        self.sel = 0;
    }

    /// Edited files, for the host to count changed lines.
    pub fn edited_files(&self) -> Vec<String> {
        self.stats
            .files
            .iter()
            .filter(|(_, k)| **k == "edited")
            .map(|(p, _)| p.clone())
            .collect()
    }

    pub fn set_diffstat(&mut self, v: Vec<(String, u64, u64)>) {
        self.diffstat = v.into_iter().map(|(p, a, d)| (p, (a, d))).collect();
        self.refresh_files();
    }

    /// The panel's part of the screen (as last drawn) holds this cell.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        self.visible
            && (self.tab_row..self.body.bottom()).contains(&row)
            && col >= self.body.x.saturating_sub(2)
            && col < self.body.right()
    }

    /// A click at a cell of the panel: a tab label switches to it; a row selects it
    /// (and opens it, in the Files tab).
    pub fn click(&mut self, col: u16, row: u16) -> Option<PanelAction> {
        self.focused = true;
        if row < self.body.y {
            // A tab's name row or the bar row under it.
            let hit = self
                .tab_spans
                .iter()
                .find(|(y, a, b, _)| (*y..=*y + 1).contains(&row) && (*a..*b).contains(&col));
            if let Some((.., name)) = hit.cloned() {
                self.shown = Some(name);
                self.sel = 0;
            }
            return None;
        }
        let line = row.checked_sub(self.body.y)? as usize;
        let r = (*self.line_rows.get(line)?)?;
        self.sel = r;
        let tab = self.current()?;
        if tab.owner.is_none() && matches!(tab.name.as_str(), "files" | "processes") {
            let files = tab.name == "files";
            let id = self.rows().get(r)?.node.id.clone();
            return Some(if files {
                PanelAction::OpenFile(id)
            } else {
                PanelAction::OpenProc(id)
            });
        }
        None
    }

    /// The mouse wheel over the panel.
    pub fn wheel(&mut self, up: bool) {
        if self.interactive() {
            let n = self.rows().len();
            self.sel = if up {
                self.sel.saturating_sub(1)
            } else {
                (self.sel + 1).min(n.saturating_sub(1))
            };
        } else {
            self.sel = if up { self.sel.saturating_sub(3) } else { self.sel + 3 };
        }
    }

    pub fn current(&self) -> Option<&Tab> {
        let name = self.shown.as_deref()?;
        self.tabs.iter().find(|t| t.name == name)
    }

    /// Show `name` if nothing is shown or it ranks at least as high as what is.
    fn offer(&mut self, name: &str) {
        let p = self.tabs.iter().find(|t| t.name == name).map(|t| t.priority);
        let cur = self.current().filter(|t| !t.empty()).map(|t| t.priority);
        if let (Some(p), cur) = (p, cur)
            && cur.is_none_or(|c| p >= c)
        {
            self.shown = Some(name.into());
        }
    }

    /// An extension's op. Unknown tabs are created on first use.
    pub fn apply(&mut self, ext: &str, op: &UiOp) {
        match op {
            UiOp::OpenTab {
                name,
                title,
                priority,
                width,
                region,
                show,
            } => {
                let t = match self.tab(name) {
                    Some(t) => t,
                    None => {
                        self.tabs.push(Tab::new(name, name, *priority));
                        self.tabs.last_mut().unwrap()
                    }
                };
                t.title = title.clone().unwrap_or_else(|| name.clone());
                t.priority = *priority;
                t.width = *width;
                t.owner = Some(ext.into());
                if *region && t.region.is_none() {
                    t.region = Some(Vec::new());
                }
                if *show {
                    self.shown = Some(name.clone());
                    self.visible = true;
                } else {
                    self.offer(name);
                }
            }
            UiOp::ShowTab { name } => {
                if self.tab(name).is_some() {
                    self.shown = Some(name.clone());
                    self.visible = true;
                }
            }
            UiOp::CloseTab { name } => {
                self.tabs.retain(|t| &t.name != name);
                if self.shown.as_ref() == Some(name) {
                    self.shown = self
                        .tabs
                        .iter()
                        .filter(|t| !t.empty())
                        .max_by_key(|t| t.priority)
                        .map(|t| t.name.clone());
                }
            }
            UiOp::Source {
                tab,
                id,
                title,
                order,
                newest_first,
            } => {
                let t = self.ensure(tab, ext);
                let s = t.source(id);
                s.title = title.clone();
                s.order = *order;
                s.newest_first = *newest_first;
            }
            UiOp::Publish { tab, source, node } => {
                self.publish(tab, source, (**node).clone(), Some(ext));
                if tab == "agent" && source == "subagents" {
                    self.agent_set.entry(node.id.clone()).or_insert(self.run);
                    let floor = self.agent_floor();
                    let sets = &self.agent_set;
                    if let Some(s) = self
                        .tabs
                        .iter_mut()
                        .find(|t| t.name == "agent")
                        .map(|t| t.source("subagents"))
                    {
                        let shown: Vec<&Node> = s
                            .nodes
                            .iter()
                            .filter(|n| sets.get(&n.id).is_none_or(|&r| r >= floor))
                            .collect();
                        let (n, live) = (shown.len(), shown.iter().filter(|n| !n.dim).count());
                        let earlier = s.nodes.len() - n;
                        let mut t = match live {
                            0 => format!("Subagents · {n}"),
                            _ => format!("Subagents · {live} running of {n}"),
                        };
                        if earlier > 0 {
                            t += &format!(" · {earlier} earlier hidden");
                        }
                        s.title = Some(t);
                    }
                }
                self.offer(tab);
            }
            UiOp::Remove { tab, source, id } => {
                if let Some(t) = self.tab(tab) {
                    let s = t.source(source);
                    match id {
                        Some(id) => s.nodes.retain(|n| &n.id != id),
                        None => s.nodes.clear(),
                    }
                }
            }
            UiOp::Region { tab, lines } => {
                let t = self.ensure(tab, ext);
                t.region = Some(lines.clone());
            }
            UiOp::Status { .. }
            | UiOp::Widget { .. }
            | UiOp::Footer { .. }
            | UiOp::Header { .. }
            | UiOp::Editor { .. }
            | UiOp::Shortcut { .. } => {}
        }
    }

    fn ensure(&mut self, tab: &str, ext: &str) -> &mut Tab {
        if self.tab(tab).is_none() {
            let mut t = Tab::new(tab, tab, 0);
            t.owner = Some(ext.into());
            self.tabs.push(t);
        }
        self.tab(tab).unwrap()
    }

    fn publish(&mut self, tab: &str, source: &str, n: Node, ext: Option<&str>) {
        let t = match ext {
            Some(e) => self.ensure(tab, e),
            None => match self.tab(tab) {
                Some(t) => t,
                None => return,
            },
        };
        let s = t.source(source);
        match s.nodes.iter_mut().find(|x| x.id == n.id) {
            Some(x) => *x = n,
            None => s.nodes.push(n),
        }
    }

    fn set_nodes(&mut self, tab: &str, source: &str, nodes: Vec<Node>) {
        if let Some(t) = self.tab(tab) {
            t.source(source).nodes = nodes;
        }
    }

    /// Feed the built-in tabs.
    pub fn on_agent(&mut self, ev: &AgentEvent, window_tokens: u64, price: Option<&novacode_types::Cost>) {
        let now = Instant::now();
        if matches!(ev, AgentEvent::AgentStart) {
            self.run += 1;
        }
        let st = &mut self.stats;
        st.window = window_tokens;
        st.price = price.copied();
        match ev {
            AgentEvent::AgentStart => {
                st.task = Task {
                    idle: st.settled_at.map(|s| now - s).unwrap_or_default(),
                    ..Default::default()
                };
                st.started = Some(now);
            }
            AgentEvent::TurnStart => {
                if let Some(r) = st.replied_at.take() {
                    st.task.tools += now - r;
                }
                st.turn_at = Some(now);
                st.task.turns += 1;
            }
            AgentEvent::Message(Message::Assistant(a)) => {
                if let Some(t) = st.turn_at.take() {
                    st.task.model += now - t;
                }
                st.replied_at = (a.stop == StopReason::ToolUse).then_some(now);
                if a.usage.prompt_total() > 0 {
                    st.last = Some(a.usage);
                    let hit = a.usage.cache_read as f64 * 100.0 / a.usage.prompt_total() as f64;
                    let spend = price.map_or(a.usage.output as f64, |p| a.usage.cost(p));
                    st.requests.push((hit, spend));
                    let u = &mut st.usage_total;
                    u.input += a.usage.input;
                    u.output += a.usage.output;
                    u.cache_read += a.usage.cache_read;
                    u.cache_write += a.usage.cache_write;
                }
            }
            AgentEvent::ToolStart { call } => {
                if let Some(p) = call.arguments["path"].as_str() {
                    let kind = match call.name.as_str() {
                        "edit" | "write" | "apply_patch" => "edited",
                        "read" => "read",
                        _ => "",
                    };
                    if !kind.is_empty() {
                        let e = st.files.entry(p.to_string()).or_insert(kind);
                        if kind == "edited" {
                            *e = "edited";
                        }
                    }
                }
            }
            AgentEvent::ProcessStarted { id, name } => {
                st.procs.insert(id.clone(), (name.clone(), None));
            }
            AgentEvent::ProcessEnded {
                id,
                name,
                code,
                stopped,
            } => {
                let how = match (stopped, code) {
                    (true, _) => "stopped",
                    (_, Some(0)) => "done",
                    _ => "failed",
                };
                st.procs.insert(id.clone(), (name.clone(), Some(how.into())));
            }
            AgentEvent::Settled => {
                if let Some(s) = st.started.take() {
                    st.task.wall = now - s;
                    st.tasks.push(st.task.clone());
                }
                st.settled_at = Some(now);
            }
            _ => {}
        }
        if let AgentEvent::Message(Message::ToolResult {
            tool_name,
            details: Some(d),
            ..
        }) = ev
            && tool_name == "todo"
        {
            self.todos(&d["todo"]);
        }
        self.refresh(window_tokens, price);
    }

    fn todos(&mut self, v: &serde_json::Value) {
        let items: Vec<&serde_json::Value> = v.as_array().into_iter().flatten().collect();
        let nodes: Vec<Node> = items
            .iter()
            .map(|i| {
                let id = i["id"].as_u64().unwrap_or(0);
                let mut n = node(&format!("t{id}"), i["text"].as_str().unwrap_or_default());
                let status = i["status"].as_str().unwrap_or_default();
                n.status = Some(
                    match status {
                        "active" => "running",
                        "done" => "done",
                        "dropped" => "stopped",
                        _ => "queued",
                    }
                    .into(),
                );
                // Checkboxes, not the play/stop glyphs other nodes use.
                n.icon = Some(format!("todo:{status}"));
                n.dim = status == "dropped" || status == "done";
                n.order = Some(id as f64);
                let (start, end) = (i["started_ms"].as_u64(), i["done_ms"].as_u64());
                n.time = match (status, start, end) {
                    ("active", Some(s), _) => Some(novacode_types::ui::Time {
                        ms: None,
                        since: Some(s),
                    }),
                    ("done", Some(s), Some(e)) => Some(novacode_types::ui::Time {
                        ms: Some(e.saturating_sub(s)),
                        since: None,
                    }),
                    _ => None,
                };
                if status == "active" {
                    n.detail = i["note"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default();
                }
                n
            })
            .collect();
        let done = items.iter().filter(|i| i["status"] == "done").count();
        let live = items.iter().filter(|i| i["status"] != "dropped").count();
        // A finished plan leaves the panel (pi): only open work is shown.
        let open = items
            .iter()
            .any(|i| matches!(i["status"].as_str(), Some("pending" | "active")));
        let nodes = if open { nodes } else { Vec::new() };
        if let Some(t) = self.tab("agent") {
            t.source("todos").progress = Some((done, live));
        }
        self.set_nodes("agent", "todos", nodes);
        self.offer("agent");
    }

    /// Rebuild the computed tabs from the stats.
    fn refresh(&mut self, window_tokens: u64, price: Option<&novacode_types::Cost>) {
        let st = &self.stats;
        let mut usage = Vec::new();
        if let Some(u) = st.last {
            let ctx = u.prompt_total() + u.output;
            usage.push({
                let mut n = node(
                    "ctx",
                    format!("context {:.0}%", ctx as f64 * 100.0 / window_tokens.max(1) as f64),
                );
                n.tag = Some(format!("{}k / {}k", ctx / 1000, window_tokens / 1000));
                n
            });
            let hit = u.cache_read as f64 * 100.0 / u.prompt_total().max(1) as f64;
            usage.push({
                let mut n = node("cache", format!("last request {hit:.0}% from cache"));
                n.detail = vec![format!(
                    "read {} · write {} · new {}",
                    u.cache_read, u.cache_write, u.input
                )];
                n
            });
        }
        let t = st.usage_total;
        if t.prompt_total() > 0 {
            let hit = t.cache_read as f64 * 100.0 / t.prompt_total() as f64;
            let mut n = node("session", format!("session {hit:.0}% from cache"));
            n.detail = vec![format!("prompt {} · output {}", t.prompt_total(), t.output)];
            if let Some(p) = price {
                n.tag = Some(format!("${:.4}", t.cost(p)));
            }
            usage.push(n);
        }
        let timing: Vec<Node> = st
            .tasks
            .iter()
            .enumerate()
            .rev()
            .take(20)
            .map(|(i, k)| {
                let mut n = node(&i.to_string(), format!("task {}", i + 1));
                n.tag = Some(secs(k.wall));
                n.detail = vec![format!(
                    "model {} · tools {} · {} turns · idle before {}",
                    secs(k.model),
                    secs(k.tools),
                    k.turns,
                    secs(k.idle)
                )];
                n
            })
            .collect();
        let mut timing = timing;
        if st.tasks.len() >= 2 {
            let walls: Vec<f64> = st.tasks.iter().map(|k| k.wall.as_secs_f64()).collect();
            let mut n = node("trend", format!("per task {}", sparkline(&walls, 24)));
            n.order = Some(-1.0);
            timing.insert(0, n);
        }
        let had_procs = self.tabs.iter().any(|t| t.name == "processes" && !t.empty());
        self.refresh_procs();
        self.set_nodes("usage", "usage", usage);
        self.set_nodes("timing", "tasks", timing);
        self.refresh_files();
        if !had_procs && !self.stats.procs.is_empty() {
            self.offer("processes");
        }
    }

    /// Files tab: edited first, each with its +/- lines; Enter or a click opens it.
    fn refresh_files(&mut self) {
        let mut v: Vec<(&String, &&str)> = self.stats.files.iter().collect();
        v.sort_by_key(|(p, k)| (**k != "edited", (*p).clone()));
        let files: Vec<Node> = v
            .into_iter()
            .map(|(p, k)| {
                let mut n = node(p, p.clone());
                n.tag = Some(match self.diffstat.get(p) {
                    Some((a, d)) => format!("+{a} −{d}"),
                    None => (*k).into(),
                });
                n.dim = *k == "read";
                n.actions.insert("enter".into(), "open".into());
                n
            })
            .collect();
        self.set_nodes("files", "files", files);
    }

    /// The user's quota windows, from the host (L8.15).
    pub fn quota(&mut self, five_hour: Option<f64>, weekly: Option<f64>) {
        let Some(t) = self.tab("usage") else { return };
        let s = t.source("quota");
        s.title = Some("Subscription".into());
        s.order = 1.0;
        s.nodes = [("5h", five_hour), ("week", weekly)]
            .into_iter()
            .filter_map(|(k, v)| {
                let mut n = node(k, format!("{k} window {:.0}% used", v?));
                n.status = Some(if v? >= 90.0 { "failed" } else { "idle" }.into());
                Some(n)
            })
            .collect();
    }

    /// Auto show/hide (L9.16): only the Agent tab opens the panel by itself, while
    /// a todo or subagent is unfinished; it closes 5 s after they all finish.
    /// Every other tab is opened by hand (Ctrl+Alt+P).
    pub fn tick(&mut self) {
        let pending = self.tabs.iter().find(|t| t.name == "agent").is_some_and(|t| {
            t.sources
                .iter()
                .flat_map(|s| &s.nodes)
                .any(|n| !matches!(n.status.as_deref(), Some("done" | "stopped" | "failed")))
        });
        // Closed by hand: stays closed for this batch of work, opens again for the next.
        if pending && !self.was_pending && self.pinned == Some(false) && !self.never_auto {
            self.pinned = None;
        }
        self.was_pending = pending;
        if let Some(p) = self.pinned {
            self.visible = p;
            return;
        }
        if pending {
            if !self.visible {
                self.shown = Some("agent".into());
            }
            self.idle_since = None;
            self.visible = true;
        } else if self.visible {
            let since = *self.idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= HIDE_AFTER && !self.focused {
                self.visible = false;
            }
        }
    }

    pub fn toggle(&mut self) {
        let v = !self.visible;
        self.pinned = Some(v);
        self.visible = v;
        if !v {
            self.focused = false;
        }
    }

    /// Tab names that have something to show, in priority order.
    fn shown_tabs(&self) -> Vec<&Tab> {
        let usage = !self.host.loaded.is_empty() || !self.host.servers.is_empty();
        let mut v: Vec<&Tab> = self
            .tabs
            .iter()
            .filter(|t| !t.empty() || t.owner.is_some() || (t.name == "usage" && usage))
            .collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.priority));
        v
    }

    pub fn cycle(&mut self, back: bool) {
        let names: Vec<String> = self.shown_tabs().iter().map(|t| t.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        let i = self
            .shown
            .as_ref()
            .and_then(|s| names.iter().position(|n| n == s))
            .unwrap_or(0);
        let j = if back {
            (i + names.len() - 1) % names.len()
        } else {
            (i + 1) % names.len()
        };
        self.shown = Some(names[j].clone());
        self.sel = 0;
    }

    /// Open todos in two or three lines: progress, the active one, what's next.
    pub fn todo_summary(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let Some(src) = self
            .tabs
            .iter()
            .find(|x| x.name == "agent")
            .and_then(|x| x.sources.iter().find(|s| s.id == "todos"))
        else {
            return Vec::new();
        };
        let open = |s: &str| matches!(s, "running" | "queued");
        if !src.nodes.iter().any(|n| n.status.as_deref().is_some_and(open)) {
            return Vec::new();
        }
        let (done, total) = src.progress.unwrap_or((0, 0));
        let filled = (done * 10 + total / 2).checked_div(total).unwrap_or(0);
        let mut out = vec![Line::from(vec![
            Span::styled(
                format!(" Todos {done}/{total} "),
                t.heading.add_modifier(Modifier::BOLD),
            ),
            Span::styled("▰".repeat(filled), t.ok),
            Span::styled("▱".repeat(10 - filled), t.dim),
        ])];
        let row = |n: &Node, lead: &str| {
            let (g, gs) = glyph(n.icon.as_deref(), n.status.as_deref(), self.nerd, t);
            let mut v = vec![
                Span::styled(format!(" {lead}{g} "), gs),
                Span::styled(n.label.clone(), t.text),
            ];
            if let Some(d) = n.detail.first() {
                v.push(Span::styled(format!("  {d}"), t.dim));
            }
            crate::wrap::truncate(v, w)
        };
        if let Some(n) = src.nodes.iter().find(|n| n.status.as_deref() == Some("running")) {
            out.push(row(n, ""));
        }
        if let Some(n) = src.nodes.iter().find(|n| n.status.as_deref() == Some("queued")) {
            out.push(row(n, "next "));
        }
        out
    }

    /// Rows of the shown node tab in display order, with tree guides.
    fn rows(&self) -> Vec<Row<'_>> {
        let Some(t) = self.current() else { return Vec::new() };
        let mut srcs: Vec<&Source> = t.sources.iter().filter(|s| !s.nodes.is_empty()).collect();
        srcs.sort_by(|a, b| a.order.total_cmp(&b.order));
        let mut out = Vec::new();
        let floor = self.agent_floor();
        for s in srcs {
            let mut nodes: Vec<&Node> = s.nodes.iter().collect();
            if t.name == "agent" && s.id == "subagents" {
                nodes.retain(|n| self.agent_set.get(&n.id).is_none_or(|&r| r >= floor));
            }
            nodes.sort_by(|a, b| a.order.unwrap_or(0.0).total_cmp(&b.order.unwrap_or(0.0)));
            if s.newest_first {
                nodes.reverse();
            }
            let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
            fn walk<'a>(all: &[&'a Node], ids: &[&str], parent: Option<&str>, prefix: &str, out: &mut Vec<Row<'a>>) {
                // A parent that isn't here makes the node a root.
                let kids: Vec<&&Node> = all
                    .iter()
                    .filter(|n| n.parent.as_deref().filter(|p| ids.contains(p)) == parent)
                    .collect();
                for (i, n) in kids.iter().enumerate() {
                    let last = i + 1 == kids.len();
                    out.push(Row {
                        node: n,
                        title: None,
                        rule: false,
                        guide: format!("{prefix}{}", if last { "└─" } else { "├─" }),
                        below: format!("{prefix}{}", if last { "  " } else { "│ " }),
                    });
                    let below = format!("{prefix}{}", if last { "  " } else { "│ " });
                    walk(all, ids, Some(&n.id), &below, out);
                }
            }
            let first = out.len();
            walk(&nodes, &ids, None, "  ", &mut out);
            if let Some(r) = out.get_mut(first) {
                r.title = s.title.as_deref().map(|t| (t, s.progress));
                r.rule = first > 0;
            }
        }
        out
    }

    /// The shown node tab has rows with actions: a selection is drawn and moved.
    /// Otherwise up/down just scroll.
    fn interactive(&self) -> bool {
        self.current()
            .is_some_and(|t| t.sources.iter().flat_map(|s| &s.nodes).any(|n| !n.actions.is_empty()))
    }

    /// A key while focused. `None` = not handled here.
    pub fn key(&mut self, key: &str) -> Option<Option<PanelAction>> {
        let tab = self.current()?.clone();
        let rows = self.rows();
        let sel_id = rows.get(self.sel).map(|r| r.node.id.clone());
        let sel_actions = rows.get(self.sel).map(|r| r.node.actions.clone()).unwrap_or_default();
        let n = rows.len();
        // Region tabs get every key except the ones that leave the panel.
        if tab.region.is_some() && !matches!(key, "esc" | "tab" | "shift+tab") {
            return Some(tab.owner.map(|ext| PanelAction::Ext {
                ext,
                tab: tab.name.clone(),
                node: None,
                action: key.into(),
            }));
        }
        if let Some(d) = key.parse::<usize>().ok().filter(|d| (1..=9).contains(d)) {
            if let Some(name) = self.shown_tabs().get(d - 1).map(|t| t.name.clone()) {
                self.shown = Some(name);
                self.sel = 0;
            }
            return Some(None);
        }
        match key {
            "esc" | "q" => self.focused = false,
            "tab" => self.cycle(false),
            "shift+tab" => self.cycle(true),
            "up" | "k" => self.sel = self.sel.saturating_sub(1),
            // Without a selection this scrolls; render clamps it to the text.
            "down" | "j" if !self.interactive() => self.sel += 1,
            "down" | "j" => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            k => {
                let act = if k == "enter" {
                    sel_actions.get("enter").or(sel_actions.values().next())
                } else {
                    sel_actions.get(k)
                };
                if let Some(a) = act {
                    if a == "open" && tab.name == "files" && tab.owner.is_none() {
                        return Some(sel_id.map(PanelAction::OpenFile));
                    }
                    if a == "output" && tab.name == "processes" && tab.owner.is_none() {
                        return Some(sel_id.map(PanelAction::OpenProc));
                    }
                    return Some(Some(if a.starts_with('/') {
                        PanelAction::Command(a.clone())
                    } else {
                        PanelAction::Ext {
                            ext: tab.owner.clone().unwrap_or_default(),
                            tab: tab.name.clone(),
                            node: sel_id,
                            action: a.clone(),
                        }
                    }));
                }
                return None;
            }
        }
        Some(None)
    }

    pub fn width(&self, screen: u16) -> u16 {
        let w = self.current().and_then(|t| t.width).unwrap_or(screen / 4);
        w.max(MIN_WIDTH).min(screen / 2)
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, theme: &Theme) {
        let border = if self.focused { theme.accent } else { theme.border };
        let block = Block::default().borders(Borders::LEFT).border_style(border);
        let inner = block.inner(area);
        let inner = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(1),
            ..inner
        };
        f.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        // Accent-bar look: icon + name, the shown tab in accent with a ━ bar under it.
        // Tabs that don't fit go on another row, so every name stays visible.
        let tabs = self.shown_tabs();
        let mut rows: Vec<(Vec<Span>, Vec<Span>)> = vec![(Vec::new(), Vec::new())];
        let mut spans_at: Vec<(u16, u16, u16, String)> = Vec::new();
        let mut x = 0u16;
        for (i, t) in tabs.iter().enumerate() {
            let on = self.shown.as_deref() == Some(t.name.as_str());
            let icon = match (self.nerd, t.name.as_str()) {
                (true, "agent") => "\u{f06a9} ",
                (true, "processes") => "\u{f013} ",
                (true, "usage") => "\u{f080} ",
                (true, "timing") => "\u{f017} ",
                (true, "files") => "\u{f15b} ",
                _ => "",
            };
            // Focused: the number that jumps to it.
            let num = if self.focused && i < 9 {
                format!("{} ", i + 1)
            } else {
                String::new()
            };
            let label = format!(" {num}{icon}{} ", t.title);
            let lw = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
            if x > 0 && x + lw > inner.width {
                rows.push((Vec::new(), Vec::new()));
                x = 0;
            }
            let y = inner.y + 2 * (rows.len() as u16 - 1);
            spans_at.push((y, inner.x + x, inner.x + x + lw, t.name.clone()));
            x += lw;
            let st = if on {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.dim
            };
            let (head, under) = rows.last_mut().unwrap();
            head.push(Span::styled(label, st));
            under.push(if on {
                Span::styled("━".repeat(lw as usize), theme.accent)
            } else {
                Span::raw(" ".repeat(lw as usize))
            });
        }
        self.tab_spans = spans_at;
        self.tab_row = area.y;
        let lines: Vec<Line> = rows
            .into_iter()
            .flat_map(|(h, u)| {
                [
                    crate::wrap::truncate(h, inner.width as usize),
                    crate::wrap::truncate(u, inner.width as usize),
                ]
            })
            .collect();
        let top = (lines.len() as u16).min(inner.height);
        self.size = (inner.width, inner.height - top);
        f.render_widget(Paragraph::new(lines), Rect { height: top, ..inner });
        let body = Rect {
            y: inner.y + top,
            height: inner.height - top,
            ..inner
        };
        let w = body.width as usize;
        self.body = body;
        self.line_rows.clear();
        if self.shown.as_deref() == Some("usage") {
            let all = self.usage_lines(w, theme);
            let footer = self
                .focused
                .then(|| footer_line("↑↓ scroll  1-9 tab  ⇥ tab  esc back", w, theme));
            let room = (body.height as usize).saturating_sub(footer.is_some() as usize);
            self.sel = self.sel.min(all.len().saturating_sub(room));
            let mut out: Vec<Line> = all.into_iter().skip(self.sel).take(room).collect();
            if let Some(f) = footer {
                out.resize(room, Line::raw(""));
                out.push(f);
            }
            f.render_widget(Paragraph::new(out), body);
            return;
        }
        let lines: Vec<Line> = match self.current().and_then(|t| t.region.clone()) {
            Some(region) => region
                .iter()
                .map(|l| crate::wrap::truncate(spans(l, theme), w))
                .collect(),
            None => {
                let n = self.rows().len();
                let interactive = self.interactive();
                if interactive {
                    self.sel = self.sel.min(n.saturating_sub(1));
                }
                let (rows, nerd, sel) = (self.rows(), self.nerd, self.sel);
                let focused = self.focused && interactive;
                let now = now_ms();
                let mut out = Vec::new();
                let mut owner: Vec<Option<usize>> = Vec::new();
                let mut sel_line = 0;
                for (i, r) in rows.iter().enumerate() {
                    let n = r.node;
                    if r.rule {
                        out.push(Line::raw(""));
                    }
                    owner.resize(out.len(), None);
                    if let Some((title, progress)) = r.title {
                        let icon = match (nerd, title) {
                            (true, "Goal") => "\u{f140} ",
                            (true, "Todos") => "\u{f0ae} ",
                            (true, "Subagents") => "\u{f06a9} ",
                            _ => "",
                        };
                        let h = vec![
                            Span::styled("▍", theme.accent),
                            Span::styled(
                                format!(" {icon}{}", title.to_uppercase()),
                                theme.heading.add_modifier(Modifier::BOLD),
                            ),
                        ];
                        let count = progress.map(|(d, t)| format!("{d}/{t}")).unwrap_or_default();
                        let cw = count.chars().count();
                        let l = crate::wrap::truncate(h, w.saturating_sub(cw + 1).max(1));
                        let pad = w.saturating_sub(crate::wrap::width(&l.spans) + cw);
                        let mut sp = l.spans;
                        sp.push(Span::raw(" ".repeat(pad)));
                        sp.push(Span::styled(count, theme.dim));
                        out.push(Line::from(sp));
                        if let Some((done, total)) = progress {
                            let filled = (done * 10 + total / 2).checked_div(total).unwrap_or(0);
                            out.push(Line::from(vec![
                                Span::raw("   "),
                                Span::styled("▰".repeat(filled), theme.ok),
                                Span::styled("▱".repeat(10 - filled), theme.border),
                            ]));
                        }
                    }
                    let (g, gs) = glyph(n.icon.as_deref(), n.status.as_deref(), nerd, theme);
                    let todo = n.icon.as_deref().is_some_and(|i| i.starts_with("todo:"));
                    let label_style = if n.dim && todo {
                        theme.dim.add_modifier(Modifier::CROSSED_OUT)
                    } else if n.dim {
                        theme.text
                    } else {
                        theme.text.add_modifier(Modifier::BOLD)
                    };
                    let mut left = vec![
                        Span::styled(r.guide.clone(), theme.border),
                        Span::styled(format!("{g} "), gs),
                        Span::styled(n.label.clone(), label_style),
                    ];
                    if let Some(t) = &n.tag {
                        left.push(Span::styled(format!(" {t}"), theme.dim));
                    }
                    let (right, rs) = match n.time {
                        Some(t) => {
                            let ms = t.ms.unwrap_or(0) + t.since.map(|s| now.saturating_sub(s)).unwrap_or(0);
                            (clock(ms), if t.since.is_some() { theme.warn } else { theme.dim })
                        }
                        None => (String::new(), theme.dim),
                    };
                    let rw = unicode_width::UnicodeWidthStr::width(right.as_str());
                    let l = crate::wrap::truncate(left, w.saturating_sub(rw + 1).max(1));
                    let pad = w.saturating_sub(crate::wrap::width(&l.spans) + rw);
                    let mut sp = l.spans;
                    sp.push(Span::raw(" ".repeat(pad)));
                    sp.push(Span::styled(right, rs));
                    if focused && i == sel {
                        sel_line = out.len();
                        sp = sp
                            .into_iter()
                            .map(|s| {
                                let st = s.style.add_modifier(Modifier::REVERSED);
                                s.style(st)
                            })
                            .collect();
                    }
                    owner.resize(out.len(), None);
                    out.push(Line::from(sp));
                    owner.push(Some(i));
                    // Details hang under the label; the guide keeps going to the children.
                    let has_kids = rows
                        .get(i + 1)
                        .is_some_and(|x| x.node.parent.as_deref() == Some(n.id.as_str()));
                    let pad = format!("{}{}", r.below, if has_kids { "│ " } else { "  " });
                    for (k, d) in n.detail.iter().enumerate() {
                        let mut v = vec![Span::styled(pad.clone(), theme.border)];
                        if k == 0
                            && n.status.as_deref() == Some("running")
                            && n.icon.as_deref().is_some_and(|i| i.starts_with("todo:"))
                        {
                            v.push(Span::styled(if nerd { "\u{f0da} " } else { "▸ " }, theme.warn));
                        }
                        v.push(Span::styled(d.clone(), theme.dim));
                        out.push(crate::wrap::truncate(v, w));
                        owner.push(Some(i));
                    }
                }
                if out.is_empty() {
                    out.push(Line::styled(" (empty)", theme.dim));
                }
                // Keys for the selected row while focused (pi viewer footer).
                let footer = self.focused.then(|| {
                    let mut k: Vec<String> = rows
                        .get(sel)
                        .map(|r| {
                            r.node
                                .actions
                                .iter()
                                .map(|(k, a)| format!("{} {a}", if k == "enter" { "⏎" } else { k }))
                                .collect()
                        })
                        .unwrap_or_default();
                    k.push(
                        if interactive {
                            "1-9 ⇥ tab"
                        } else {
                            "↑↓ scroll  1-9 ⇥ tab"
                        }
                        .into(),
                    );
                    k.push("esc back".into());
                    k.join("  ")
                });
                let room = body.height as usize - footer.is_some() as usize;
                let max_start = out.len().saturating_sub(room);
                let start = if interactive {
                    sel_line.saturating_sub(room / 2).min(max_start)
                } else {
                    self.sel = self.sel.min(max_start);
                    self.sel
                };
                owner.resize(out.len(), None);
                self.line_rows = owner.into_iter().skip(start).take(room).collect();
                let mut out: Vec<Line> = out.into_iter().skip(start).take(room).collect();
                if let Some(f) = footer {
                    out.resize(room, Line::raw(""));
                    out.push(footer_line(&f, w, theme));
                }
                out
            }
        };
        f.render_widget(Paragraph::new(lines), body);
    }
}

impl Panel {
    /// The Usage stats tab: bars for context, cache and quota, request sparklines,
    /// then MCP servers and what loaded.
    fn usage_lines(&self, w: usize, t: &Theme) -> Vec<Line<'static>> {
        let st = &self.stats;
        let mut out: Vec<Line<'static>> = Vec::new();
        let head = |out: &mut Vec<Line<'static>>, title: &str, right: String| {
            if !out.is_empty() {
                out.push(Line::raw(""));
            }
            let l = vec![
                Span::styled("▍", t.accent),
                Span::styled(
                    format!(" {}", title.to_uppercase()),
                    t.heading.add_modifier(Modifier::BOLD),
                ),
            ];
            let pad = w.saturating_sub(crate::wrap::width(&l) + right.chars().count());
            let mut l = l;
            l.push(Span::raw(" ".repeat(pad)));
            l.push(Span::styled(right, t.dim));
            out.push(crate::wrap::truncate(l, w));
        };
        let bw = w.saturating_sub(16).clamp(8, 40);
        // A labelled bar: `label ███▌░░░ 42%`. `high_bad`: colour turns warn/error as it fills.
        let meter = |label: &str, pct: f64, high_bad: bool| -> Line<'static> {
            let st = match (high_bad, pct) {
                (true, p) if p >= 85.0 => t.error,
                (true, p) if p >= 60.0 => t.warn,
                (true, _) => t.ok,
                (false, p) if p >= 70.0 => t.ok,
                (false, p) if p >= 40.0 => t.warn,
                (false, _) => t.error,
            };
            let mut v = vec![Span::styled(format!("  {label:<8}"), t.dim)];
            v.extend(bar(pct / 100.0, bw, st, t.border));
            v.push(Span::styled(format!(" {pct:>3.0}%"), st));
            crate::wrap::truncate(v, w)
        };
        let k = |n: u64| -> String {
            match n {
                0..1000 => n.to_string(),
                1000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
                _ => format!("{:.2}M", n as f64 / 1e6),
            }
        };
        if let Some(u) = st.last {
            let ctx = u.prompt_total() + u.output;
            let win = st.window.max(1);
            head(&mut out, "Context", format!("{} / {}", k(ctx), k(win)));
            out.push(meter("used", ctx as f64 * 100.0 / win as f64, true));
            // What fills it, as one stacked bar and a legend.
            let parts: Vec<(String, u64)> = self.host.parts.iter().filter(|(_, n)| *n > 0).cloned().collect();
            if !parts.is_empty() {
                let colors = [t.accent, t.warn, t.ok, t.link];
                let mut v = vec![Span::styled(format!("  {:<8}", "split"), t.dim)];
                let mut used = 0;
                for (i, (_, n)) in parts.iter().enumerate() {
                    let cells = ((*n as f64 / win as f64) * bw as f64).round() as usize;
                    let cells = cells.min(bw - used);
                    used += cells;
                    v.push(Span::styled("█".repeat(cells), colors[i % colors.len()]));
                }
                v.push(Span::styled("░".repeat(bw - used), t.border));
                out.push(crate::wrap::truncate(v, w));
                let mut legend = vec![Span::raw("  ".to_string() + &" ".repeat(8))];
                for (i, (name, n)) in parts.iter().enumerate() {
                    legend.push(Span::styled("■ ", colors[i % colors.len()]));
                    legend.push(Span::styled(format!("{name} {}  ", k(*n)), t.dim));
                }
                out.extend(crate::wrap::wrap(legend, w, &[], &[Span::raw("          ")]));
            }
            head(&mut out, "Cache", String::new());
            out.push(meter(
                "last",
                u.cache_read as f64 * 100.0 / u.prompt_total().max(1) as f64,
                false,
            ));
            let tot = st.usage_total;
            out.push(meter(
                "session",
                tot.cache_read as f64 * 100.0 / tot.prompt_total().max(1) as f64,
                false,
            ));
            if st.requests.len() >= 2 {
                let hits: Vec<f64> = st.requests.iter().map(|r| r.0).collect();
                out.push(Line::from(vec![
                    Span::styled(format!("  {:<8}", "per req"), t.dim),
                    Span::styled(sparkline(&hits, bw), t.ok),
                ]));
            }
            out.push(Line::styled(
                format!(
                    "  read {} · write {} · new {}",
                    k(u.cache_read),
                    k(u.cache_write),
                    k(u.input)
                ),
                t.dim,
            ));
            let right = st.price.map(|p| format!("${:.4}", tot.cost(&p))).unwrap_or_default();
            head(&mut out, "Tokens", right);
            out.push(Line::styled(
                format!(
                    "  prompt {} · output {} · {} requests",
                    k(tot.prompt_total()),
                    k(tot.output),
                    st.requests.len()
                ),
                t.text,
            ));
            if st.requests.len() >= 2 {
                let spend: Vec<f64> = st.requests.iter().map(|r| r.1).collect();
                let what = if st.price.is_some() { "$ / req" } else { "out/req" };
                out.push(Line::from(vec![
                    Span::styled(format!("  {what:<8}"), t.dim),
                    Span::styled(sparkline(&spend, bw), t.accent),
                ]));
            }
        }
        let quota: Vec<(String, f64)> = self
            .tabs
            .iter()
            .find(|x| x.name == "usage")
            .and_then(|x| x.sources.iter().find(|s| s.id == "quota"))
            .map(|s| {
                s.nodes
                    .iter()
                    .filter_map(|n| {
                        let pct = n.label.split_whitespace().nth(2)?.trim_end_matches('%').parse().ok()?;
                        Some((n.id.clone(), pct))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !quota.is_empty() {
            head(&mut out, "Subscription", String::new());
            for (id, pct) in quota {
                out.push(meter(&id, pct, true));
            }
        }
        if !self.host.servers.is_empty() {
            let ready = self.host.servers.iter().filter(|s| s.1 == "ready").count();
            head(
                &mut out,
                "MCP",
                format!("{ready}/{} connected", self.host.servers.len()),
            );
            for (name, status, tools) in &self.host.servers {
                let (g, gs) = match status.as_str() {
                    "ready" => ("●", t.ok),
                    "connecting" => ("◌", t.warn),
                    _ => ("✗", t.error),
                };
                let what = if status == "ready" {
                    format!("{tools} tools")
                } else {
                    status.clone()
                };
                out.push(crate::wrap::truncate(
                    vec![
                        Span::styled(format!("  {g} "), gs),
                        Span::styled(name.clone(), t.text.add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  {what}"), t.dim),
                    ],
                    w,
                ));
            }
        }
        if !self.host.loaded.is_empty() {
            head(&mut out, "Loaded", String::new());
            let running = self
                .procs
                .len()
                .max(st.procs.values().filter(|(_, e)| e.is_none()).count());
            let mut items: Vec<(String, usize)> = self.host.loaded.clone();
            items.push(("processes running".into(), running));
            items.push(("subagents running".into(), self.live_subagents()));
            for (name, n) in items {
                out.push(Line::from(vec![
                    Span::styled(
                        format!("  {n:>4} "),
                        if n > 0 {
                            t.accent.add_modifier(Modifier::BOLD)
                        } else {
                            t.dim
                        },
                    ),
                    Span::styled(name, t.text),
                ]));
            }
        }
        if out.is_empty() {
            out.push(Line::styled(
                " (nothing yet: stats appear after the first reply)",
                t.dim,
            ));
        }
        out
    }
}

/// `key word  key word`: keys in the accent colour, words dim.
fn footer_line(f: &str, w: usize, t: &Theme) -> Line<'static> {
    let mut sp = Vec::new();
    for (i, item) in f.split("  ").filter(|x| !x.is_empty()).enumerate() {
        let (k, rest) = item.split_once(' ').unwrap_or((item, ""));
        sp.push(Span::raw(if i == 0 { " " } else { "  " }));
        sp.push(Span::styled(k.to_string(), t.accent));
        if !rest.is_empty() {
            sp.push(Span::styled(format!(" {rest}"), t.dim));
        }
    }
    crate::wrap::truncate(sp, w)
}

/// A status's glyph and colour (for the process view).
pub fn status_glyph(status: &str, nerd: bool, t: &Theme) -> (String, Style) {
    glyph(None, Some(status), nerd, t)
}

/// The status glyph and its colour: Nerd Font like the pi panel, or plain Unicode.
/// `icon` "todo:<status>" asks for checkboxes; any other icon is drawn as given.
fn glyph(icon: Option<&str>, status: Option<&str>, nerd: bool, t: &Theme) -> (String, Style) {
    let style = match status {
        Some("running" | "starting") => t.warn,
        Some("waiting") => t.accent,
        Some("done") => t.ok,
        Some("failed") => t.error,
        Some("idle") => t.heading,
        _ => t.dim,
    };
    let g = match (icon, nerd) {
        (Some(i), _) if !i.starts_with("todo:") => return (i.to_string(), style),
        (Some(i), true) => match &i[5..] {
            "active" => "\u{f192}",
            "done" => "\u{f14a}",
            "dropped" => "\u{f05e}",
            _ => "\u{f096}",
        },
        (Some(i), false) => match &i[5..] {
            "active" => "◐",
            "done" => "✓",
            "dropped" => "✗",
            _ => "○",
        },
        (None, true) => match status {
            Some("running") => "\u{f04b}",
            Some("starting") => "\u{f110}",
            Some("waiting") => "\u{f128}",
            Some("queued") => "\u{f017}",
            Some("done") => "\u{f00c}",
            Some("idle") => "\u{f10c}",
            Some("failed") => "\u{f00d}",
            Some("stopped") => "\u{f04d}",
            _ => "•",
        },
        (None, false) => match status {
            Some("running") => "◐",
            Some("starting") => "◌",
            Some("waiting") => "⏸",
            Some("queued") => "○",
            Some("done") => "✓",
            Some("failed") => "✗",
            Some("stopped") => "■",
            Some("idle") => "·",
            _ => "•",
        },
    };
    (g.to_string(), style)
}

/// A bar `width` cells long, filled to `frac` with eighth-cell precision.
fn bar(frac: f64, width: usize, on: Style, off: Style) -> Vec<Span<'static>> {
    const PART: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (frac.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let mut fill = "█".repeat(full);
    fill.push_str(PART[part]);
    let rest = width.saturating_sub(full + (part > 0) as usize);
    vec![Span::styled(fill, on), Span::styled("░".repeat(rest), off)]
}

/// The last `width` values as `▁▂▅█`, scaled to their own maximum.
fn sparkline(v: &[f64], width: usize) -> String {
    const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let v = &v[v.len().saturating_sub(width)..];
    let max = v.iter().cloned().fold(0.0_f64, f64::max);
    v.iter()
        .map(|x| match max {
            m if m <= 0.0 => TICKS[0],
            m => TICKS[((x / m) * 7.0).round().clamp(0.0, 7.0) as usize],
        })
        .collect()
}

/// pi viewer clock: 42s, 3m07s, 1h05m.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// An extension line as styled spans.
pub fn spans(l: &ULine, t: &Theme) -> Vec<Span<'static>> {
    match l {
        ULine::Plain(s) => vec![Span::styled(s.clone(), t.text)],
        ULine::Styled(segs) => segs
            .iter()
            .map(|s| Span::styled(s.text.clone(), t.token(s.style.as_deref())))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn closed_by_hand_reopens_for_new_work_and_plain_tabs_scroll() {
        let mut p = Panel::default();
        let op = |v: serde_json::Value| serde_json::from_value::<UiOp>(v).unwrap();
        let sub = |id: &str, status: &str| {
            op(json!({"op": "publish", "tab": "agent", "source": "subagents",
                "node": {"id": id, "label": "scout", "status": status, "dim": status == "done"}}))
        };
        p.apply("agents", &sub("a1", "running"));
        p.tick();
        assert!(p.visible, "a running subagent opens the panel");
        assert_eq!(p.live_subagents(), 1);
        assert_eq!(
            p.tab("agent").unwrap().source("subagents").title.as_deref(),
            Some("Subagents · 1 running of 1")
        );
        p.toggle();
        p.tick();
        assert!(!p.visible, "closed by hand stays closed for this work");
        p.apply("agents", &sub("a1", "done"));
        p.tick();
        p.apply("agents", &sub("a2", "running"));
        p.tick();
        assert!(p.visible, "new work opens it again");
        // Timing has no actions: no selection, j/k scroll.
        p.shown = Some("timing".into());
        p.stats.tasks.push(Task::default());
        p.refresh(0, None);
        assert!(!p.interactive());
        p.focused = true;
        assert_eq!(p.key("j"), Some(None));
        assert_eq!(p.sel, 1, "scroll offset, clamped when drawn");
        // Files: rows select, Enter opens; edited files first, with their line counts.
        p.shown = Some("files".into());
        p.stats.files.insert("b.rs".into(), "read");
        p.stats.files.insert("c.rs".into(), "edited");
        p.set_diffstat(vec![("c.rs".into(), 12, 3)]);
        assert!(p.interactive());
        p.sel = 0;
        let first = &p.rows()[0].node;
        assert_eq!((first.id.as_str(), first.tag.as_deref()), ("c.rs", Some("+12 −3")));
        assert_eq!(p.key("enter"), Some(Some(PanelAction::OpenFile("c.rs".into()))));
        assert_eq!(p.edited_files(), ["c.rs"]);
        // Number keys jump to a tab.
        assert_eq!(p.key("1"), Some(None));
        assert_eq!(p.shown.as_deref(), Some("agent"));
    }

    #[test]
    fn subagents_show_from_the_oldest_prompt_still_running() {
        let mut p = Panel::default();
        let op = |v: serde_json::Value| serde_json::from_value::<UiOp>(v).unwrap();
        let sub = |id: &str, status: &str| {
            op(json!({"op": "publish", "tab": "agent", "source": "subagents",
                "node": {"id": id, "label": "scout", "status": status, "dim": status == "done"}}))
        };
        let start = |p: &mut Panel| p.on_agent(&AgentEvent::AgentStart, 0, None);
        let shown = |p: &Panel| p.rows().iter().map(|r| r.node.id.clone()).collect::<Vec<_>>();
        start(&mut p);
        p.apply("agents", &sub("a1", "done"));
        p.apply("agents", &sub("a2", "done"));
        start(&mut p);
        p.apply("agents", &sub("a3", "running"));
        assert_eq!(shown(&p), ["a3"], "only the newest prompt's set");
        start(&mut p);
        p.apply("agents", &sub("a4", "running"));
        assert_eq!(shown(&p), ["a4", "a3"], "a set still running stays");
        p.apply("agents", &sub("a3", "done"));
        assert_eq!(shown(&p), ["a4"]);
        assert_eq!(
            p.tab("agent").unwrap().source("subagents").title.as_deref(),
            Some("Subagents · 1 running of 1 · 3 earlier hidden")
        );
        start(&mut p);
        assert_eq!(shown(&p), ["a4"], "a prompt without subagents keeps the last set");
    }

    #[test]
    fn tabs_wrap_so_every_name_shows() {
        let mut p = Panel::default();
        p.stats.files.insert("a.rs".into(), "edited");
        p.stats.tasks.push(Task::default());
        p.stats.procs.insert("p1".into(), ("dev".into(), None));
        p.host.loaded = vec![("skills".into(), 3)];
        p.refresh(0, None);
        p.todos(&json!([{"id": 1, "text": "x", "status": "active"}]));
        p.focused = true;
        p.visible = true;
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        term.draw(|f| p.render(f, f.area(), &Theme::default())).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        for name in ["Agent", "Processes", "Usage stats", "Timing", "Files"] {
            assert!(text.contains(name), "{name} hidden:\n{text}");
        }
        // A click on the second row of tabs switches to that tab.
        let (y, x, _, name) = p.tab_spans.last().cloned().unwrap();
        assert!(y > p.tab_row, "wrapped to a second row");
        p.click(x + 1, y);
        assert_eq!(p.shown.as_deref(), Some(name.as_str()));
    }

    #[test]
    fn bars_and_sparklines() {
        let t = Theme::default();
        let text = |v: Vec<Span>| v.iter().map(|s| s.content.to_string()).collect::<String>();
        assert_eq!(text(bar(0.5, 8, t.ok, t.dim)), "████░░░░");
        assert_eq!(text(bar(0.0625, 8, t.ok, t.dim)), "▌░░░░░░░");
        assert_eq!(sparkline(&[0.0, 50.0, 100.0], 10), "▁▅█");
        assert_eq!(sparkline(&[1.0, 2.0, 3.0], 2), "▆█");
    }

    #[test]
    fn tabs_sources_and_priority() {
        let mut p = Panel::default();
        let op = |v: serde_json::Value| serde_json::from_value::<UiOp>(v).unwrap();
        p.apply(
            "x",
            &op(json!({"op": "open_tab", "name": "deploy", "title": "Deploy", "priority": 30})),
        );
        p.apply("x", &op(json!({"op": "publish", "tab": "deploy", "source": "s", "node": {"id": "a", "label": "build", "status": "running"}})));
        p.apply("x", &op(json!({"op": "publish", "tab": "deploy", "source": "s", "node": {"id": "b", "parent": "a", "label": "step", "actions": {"r": "retry", "l": "/ps"}}})));
        assert_eq!(p.shown.as_deref(), Some("deploy"), "higher priority takes the slot");
        p.tick();
        assert!(!p.visible, "extension tabs never open the panel by themselves");
        let rows: Vec<(String, String)> = p.rows().iter().map(|r| (r.node.id.clone(), r.guide.clone())).collect();
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), "  └─".to_string()),
                ("b".to_string(), "    └─".to_string())
            ]
        );
        p.focused = true;
        p.sel = 1;
        assert_eq!(
            p.key("r"),
            Some(Some(PanelAction::Ext {
                ext: "x".into(),
                tab: "deploy".into(),
                node: Some("b".into()),
                action: "retry".into()
            }))
        );
        assert_eq!(p.key("l"), Some(Some(PanelAction::Command("/ps".into()))));
        // Built-ins: todos from a todo result.
        p.on_agent(
            &AgentEvent::Message(Message::ToolResult {
                call_id: "c".into(),
                tool_name: "todo".into(),
                content: vec![],
                is_error: false,
                details: Some(json!({"todo": [{"id": 1, "text": "build", "status": "active", "note": "compiling"}]})),
            }),
            100_000,
            None,
        );
        let agent = p.tabs.iter().find(|t| t.name == "agent").unwrap();
        assert_eq!(
            agent.sources.iter().find(|s| s.id == "todos").unwrap().nodes[0]
                .status
                .as_deref(),
            Some("running")
        );
        p.tick();
        assert!(
            p.visible && p.shown.as_deref() == Some("agent"),
            "an open todo opens the Agent tab"
        );
        p.on_agent(
            &AgentEvent::Message(Message::ToolResult {
                call_id: "c2".into(),
                tool_name: "todo".into(),
                content: vec![],
                is_error: false,
                details: Some(json!({"todo": [{"id": 1, "text": "build", "status": "done"}]})),
            }),
            100_000,
            None,
        );
        p.tick();
        p.focused = false; // a focused panel stays up
        p.idle_since = Some(Instant::now() - HIDE_AFTER);
        p.tick();
        assert!(!p.visible, "closes once every todo is done");
        p.apply("x", &op(json!({"op": "close_tab", "name": "deploy"})));
        assert_eq!(
            p.shown.as_deref(),
            Some("agent"),
            "the next tab with content takes over"
        );
        // A region tab forwards keys to its owner.
        p.apply(
            "y",
            &op(json!({"op": "open_tab", "name": "game", "region": true, "show": true})),
        );
        assert!(
            matches!(p.key("left"), Some(Some(PanelAction::Ext { ext, action, .. })) if ext == "y" && action == "left")
        );
    }
}
