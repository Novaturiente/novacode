//! One live session shared by its windows (L8.13): the hub mirrors what a new
//! client needs to catch up (messages, the in-flight partial, open approvals)
//! and fans events out to every connected client in order.

use crate::wire::{ActState, ServerMsg, State, capabilities};
use futures_util::future::BoxFuture;
use novacode_core::agent::SessionHandle;
use novacode_core::policy::{Approval, ApprovalRequest, Approver};
use novacode_tui::app::UiEvent;
use novacode_tui::ask::AskReq;
use novacode_tui::dialog::{Answer, ApproveReq};
use novacode_types::{AgentEvent, BlockKind, Message, Question, Reply, StreamEvent};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Events since the last stored message, enough to rebuild a live block.
/// shortcut: capped by count, oldest tool output dropped first; a client that
/// attaches mid-way through a huge tool output sees only its tail.
const TAIL_MAX: usize = 4000;

/// Where an `ask_user` answer goes (`None` = dismissed).
type AskReply = oneshot::Sender<Option<Vec<Reply>>>;

struct Pending {
    msg: ServerMsg,
    reply: oneshot::Sender<Answer>,
}

struct Inner {
    state: State,
    history: Vec<Message>,
    tail: Vec<AgentEvent>,
    /// The current extension UI, latest op per slot (in arrival order), for late windows.
    ui: BTreeMap<String, (u64, AgentEvent)>,
    ui_seq: u64,
    clients: BTreeMap<u64, mpsc::UnboundedSender<ServerMsg>>,
    next_client: u64,
    approvals: BTreeMap<u64, Pending>,
    /// Open `ask_user` forms (L5.21): the message and the waiting tool.
    asks: BTreeMap<u64, (ServerMsg, AskReply)>,
    /// The owner's own TUI, if any: gets approvals and remote state changes.
    local: Option<mpsc::UnboundedSender<UiEvent>>,
    closed: bool,
}

pub struct Hub {
    inner: Mutex<Inner>,
    pub handle: SessionHandle,
    next_approval: std::sync::atomic::AtomicU64,
    /// Fires after every state change (the run file and sinks follow it).
    pub changed: tokio::sync::watch::Sender<State>,
    focused: std::sync::atomic::AtomicBool,
    /// Print mode: no window will ever answer, so approvals and questions are refused at once.
    pub unattended: std::sync::atomic::AtomicBool,
}

/// Compactions on a session branch (L9.19): the hub state's `compactions` at start.
pub fn compactions(branch: &[&novacode_core::session::Entry]) -> u32 {
    branch
        .iter()
        .filter(|e| matches!(e.kind, novacode_core::session::EntryKind::Compaction { .. }))
        .count() as u32
}

fn activity_state(busy: bool, blocked: bool) -> ActState {
    if blocked {
        ActState::Blocked
    } else if busy {
        ActState::Working
    } else {
        ActState::Idle
    }
}

impl Hub {
    /// Start mirroring `handle`'s events. `history` = the session's messages now.
    pub fn start(
        handle: SessionHandle,
        history: Vec<Message>,
        state: State,
        local: Option<mpsc::UnboundedSender<UiEvent>>,
    ) -> Arc<Self> {
        let mut events = handle.subscribe();
        let hub = Arc::new(Hub {
            inner: Mutex::new(Inner {
                state: state.clone(),
                history,
                tail: Vec::new(),
                ui: BTreeMap::new(),
                ui_seq: 0,
                clients: BTreeMap::new(),
                next_client: 0,
                approvals: BTreeMap::new(),
                asks: BTreeMap::new(),
                local,
                closed: false,
            }),
            handle,
            next_approval: 1.into(),
            changed: tokio::sync::watch::channel(state).0,
            focused: true.into(),
            unattended: false.into(),
        });
        let h = Arc::downgrade(&hub);
        tokio::spawn(async move {
            loop {
                let ev = match events.recv().await {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "hub missed events");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(hub) = h.upgrade() else { break };
                hub.on_event(ev);
            }
        });
        hub
    }

    fn on_event(&self, ev: AgentEvent) {
        let mut g = self.inner.lock().unwrap();
        let before = (g.state.busy, g.state.activity.clone());
        match &ev {
            AgentEvent::AgentStart => g.state.busy = true,
            AgentEvent::Settled => g.state.busy = false,
            _ => {}
        }
        let detail = match &ev {
            AgentEvent::AgentStart => Some("working".to_string()),
            AgentEvent::TurnStart => Some("waiting for the model".into()),
            AgentEvent::Stream(StreamEvent::BlockStart { kind, name, .. }) => Some(match kind {
                BlockKind::Thinking => "thinking".into(),
                BlockKind::ToolCall => format!("writing {} call", name.as_deref().unwrap_or("tool")),
                _ => "writing".into(),
            }),
            AgentEvent::Compacting { .. } => Some("compacting".into()),
            // Back to plain work; an idle session's detail is cleared below.
            AgentEvent::Compacted { .. } => Some("working".into()),
            AgentEvent::ToolStart { call } => Some(format!("tool:{}", call.name)),
            AgentEvent::Retry { .. } => Some("retrying".into()),
            _ => None,
        };
        // While waiting on a person, the detail says what for.
        if let Some(d) = detail
            && g.approvals.is_empty()
            && g.asks.is_empty()
        {
            g.state.activity.detail = d;
        }
        if matches!(ev, AgentEvent::Settled) || matches!(ev, AgentEvent::Compacted { .. }) && !g.state.busy {
            g.state.activity.detail.clear();
        }
        let blocked = !g.approvals.is_empty() || !g.asks.is_empty();
        g.state.activity.state = activity_state(g.state.busy, blocked);
        match &ev {
            AgentEvent::AgentStart => g.state.task_started_ms = Some(novacode_core::session::now_ms()),
            AgentEvent::Settled => g.state.task_started_ms = None,
            AgentEvent::Compacted { tokens_after, .. } => {
                g.state.context_tokens = *tokens_after;
                g.state.compactions += 1;
            }
            AgentEvent::Stream(StreamEvent::Notice { text }) if text.starts_with("cache miss:") => {
                g.state.cache.last_miss = Some(text.trim_start_matches("cache miss:").trim().to_string());
            }
            AgentEvent::Message(m) => g.state.add(m),
            AgentEvent::CwdChanged { cwd, branch } => {
                g.state.cwd = cwd.clone();
                g.state.branch = branch.clone();
            }
            AgentEvent::ProcessStarted { id, .. } => g.state.processes.push(id.clone()),
            AgentEvent::ProcessEnded { id, .. } => g.state.processes.retain(|p| p != id),
            _ => {}
        }
        match &ev {
            AgentEvent::Message(m) => {
                g.history.push(m.clone());
                // Running tools stay; everything else is now in the message.
                g.tail
                    .retain(|e| matches!(e, AgentEvent::ToolStart { .. } | AgentEvent::ToolUpdate { .. }));
            }
            AgentEvent::ToolEnd { call_id, .. } => g.tail.retain(|e| match e {
                AgentEvent::ToolStart { call } => call.id != *call_id,
                AgentEvent::ToolUpdate { call_id: c, .. } => c != call_id,
                _ => true,
            }),
            AgentEvent::Settled => g.tail.clear(),
            AgentEvent::Ui { ext, op } => {
                g.ui_seq += 1;
                let (slot, seq) = (op.slot(ext), g.ui_seq);
                g.ui.insert(slot, (seq, ev.clone()));
            }
            _ => {
                g.tail.push(ev.clone());
                if g.tail.len() > TAIL_MAX
                    && let Some(i) = g.tail.iter().position(|e| matches!(e, AgentEvent::ToolUpdate { .. }))
                {
                    g.tail.remove(i);
                }
            }
        }
        let figures = matches!(
            ev,
            AgentEvent::Message(_)
                | AgentEvent::Compacted { .. }
                | AgentEvent::CwdChanged { .. }
                | AgentEvent::ProcessStarted { .. }
                | AgentEvent::ProcessEnded { .. }
        );
        let msg = ServerMsg::Event { event: ev };
        g.clients.retain(|_, tx| tx.send(msg.clone()).is_ok());
        if figures || (g.state.busy, g.state.activity.clone()) != before {
            let s = g.state.clone();
            drop(g);
            self.state_changed(s, false);
        }
    }

    fn set_blocked(&self) {
        let s = {
            let mut g = self.inner.lock().unwrap();
            let blocked = g
                .approvals
                .values()
                .next()
                .map(|p| match &p.msg {
                    ServerMsg::Approve { tool, .. } => format!("permission:{tool}"),
                    _ => String::new(),
                })
                .or_else(|| (!g.asks.is_empty()).then(|| "ask_user".to_string()));
            let state = activity_state(g.state.busy, blocked.is_some());
            match blocked {
                Some(d) => g.state.activity.detail = d,
                None if g.state.activity.detail.starts_with("permission:") || g.state.activity.detail == "ask_user" => {
                    g.state.activity.detail = "working".into()
                }
                None => {}
            }
            g.state.activity.state = state;
            g.state.clone()
        };
        self.state_changed(s, false);
    }

    /// The owner's terminal has focus (L11.9: notify only when nobody is looking).
    pub fn set_focus(&self, focused: bool) {
        self.focused.store(focused, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn focused(&self) -> bool {
        self.focused.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A new client: what it needs to catch up, then its live feed.
    pub fn connect(
        &self,
    ) -> (
        u64,
        Vec<ServerMsg>,
        mpsc::UnboundedSender<ServerMsg>,
        mpsc::UnboundedReceiver<ServerMsg>,
    ) {
        let mut g = self.inner.lock().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut first = vec![
            ServerMsg::Hello {
                protocol: crate::wire::PROTOCOL,
                version: env!("CARGO_PKG_VERSION").into(),
                capabilities: capabilities(),
                state: g.state.clone(),
            },
            ServerMsg::History {
                messages: g.history.clone(),
            },
        ];
        if g.state.busy {
            first.push(ServerMsg::Event {
                event: AgentEvent::AgentStart,
            });
        }
        let mut ui: Vec<&(u64, AgentEvent)> = g.ui.values().collect();
        ui.sort_by_key(|(seq, _)| *seq);
        let ui: Vec<AgentEvent> = ui.into_iter().map(|(_, e)| e.clone()).collect();
        first.extend(ui.into_iter().map(|event| ServerMsg::Event { event }));
        first.extend(g.tail.iter().map(|e| ServerMsg::Event { event: e.clone() }));
        first.extend(g.approvals.values().map(|p| p.msg.clone()));
        first.extend(g.asks.values().map(|(m, _)| m.clone()));
        if g.closed {
            first.push(ServerMsg::Bye {
                reason: "session ended".into(),
            });
        }
        let id = g.next_client;
        g.next_client += 1;
        g.clients.insert(id, tx.clone());
        (id, first, tx, rx)
    }

    pub fn disconnect(&self, id: u64) {
        self.inner.lock().unwrap().clients.remove(&id);
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    /// Seed the UI log (extension setup ops sent before the hub existed).
    pub fn seed_ui(&self, events: Vec<AgentEvent>) {
        let mut g = self.inner.lock().unwrap();
        for ev in events {
            if let AgentEvent::Ui { ext, op } = &ev {
                g.ui_seq += 1;
                let (slot, seq) = (op.slot(ext), g.ui_seq);
                g.ui.insert(slot, (seq, ev));
            }
        }
    }

    /// The current extension UI, in order, for a window starting up.
    pub fn ui_log(&self) -> Vec<AgentEvent> {
        let g = self.inner.lock().unwrap();
        let mut ui: Vec<&(u64, AgentEvent)> = g.ui.values().collect();
        ui.sort_by_key(|(seq, _)| *seq);
        ui.into_iter().map(|(_, e)| e.clone()).collect()
    }

    pub fn history(&self) -> Vec<Message> {
        self.inner.lock().unwrap().history.clone()
    }

    /// Change the shared state; remote windows hear of it, and the local TUI
    /// too when the change came from elsewhere.
    pub fn update(&self, from_local: bool, f: impl FnOnce(&mut State)) {
        let (s, route_changed) = {
            let mut g = self.inner.lock().unwrap();
            let before = (g.state.model.clone(), g.state.level, g.state.window_tokens);
            let info_before = g.state.info_key();
            f(&mut g.state);
            let after = (g.state.model.clone(), g.state.level, g.state.window_tokens);
            if info_before != g.state.info_key()
                && let Some(l) = &g.local
            {
                let _ = l.send(g.state.info());
            }
            (g.state.clone(), before != after)
        };
        self.state_changed(s, !from_local && route_changed);
    }

    fn state_changed(&self, s: State, tell_local: bool) {
        let mut g = self.inner.lock().unwrap();
        let msg = ServerMsg::State { state: s.clone() };
        g.clients.retain(|_, tx| tx.send(msg.clone()).is_ok());
        if tell_local && let Some(l) = &g.local {
            let _ = l.send(UiEvent::ModelSet {
                model: s.model.clone(),
                window_tokens: s.window_tokens,
                level: s.level,
                price: s.price,
            });
        }
        drop(g);
        self.changed.send_replace(s);
    }

    /// Ask every window; the first answer wins and the others drop the card.
    async fn ask(self: &Arc<Self>, req: ApprovalRequest) -> Answer {
        if self.unattended.load(std::sync::atomic::Ordering::Relaxed) {
            return Answer::Deny(Some("nobody can approve in print mode".into()));
        }
        let id = self.next_approval.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let detail = match req.args.get("command").or_else(|| req.args.get("path")) {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => req.args.to_string(),
        };
        let msg = ServerMsg::Approve {
            id,
            tool: req.tool.clone(),
            detail: detail.clone(),
            reason: req.reason.clone(),
            rule: req.rule.clone(),
            risky: req.risky,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.inner.lock().unwrap();
            g.clients.retain(|_, c| c.send(msg.clone()).is_ok());
            if let Some(l) = &g.local {
                let (ltx, lrx) = oneshot::channel();
                let card = ApproveReq {
                    id,
                    tool: req.tool,
                    detail,
                    reason: req.reason,
                    rule: req.rule,
                    risky: req.risky,
                    reply: ltx,
                };
                if l.send(UiEvent::Approve(card)).is_ok() {
                    let hub = self.clone();
                    tokio::spawn(async move {
                        if let Ok(a) = lrx.await {
                            hub.answer(id, a);
                        }
                    });
                }
            }
            g.approvals.insert(id, Pending { msg, reply: tx });
        }
        let _open = Open {
            hub: self.clone(),
            id,
            ask: false,
        };
        self.set_blocked();
        rx.await.unwrap_or(Answer::Deny(None))
    }

    /// An answer from any window. Late answers are ignored.
    pub fn answer(&self, id: u64, a: Answer) {
        let mut g = self.inner.lock().unwrap();
        let Some(p) = g.approvals.remove(&id) else { return };
        let _ = p.reply.send(a);
        let done = ServerMsg::ApprovalDone { id };
        g.clients.retain(|_, tx| tx.send(done.clone()).is_ok());
        if let Some(l) = &g.local {
            let _ = l.send(UiEvent::ApprovalDone { id });
        }
        drop(g);
        self.set_blocked();
    }

    /// The session is over: tell every client and refuse open approvals.
    pub fn close(&self, reason: &str) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        let bye = ServerMsg::Bye { reason: reason.into() };
        for tx in g.clients.values() {
            let _ = tx.send(bye.clone());
        }
        g.clients.clear();
        g.approvals.clear();
        g.asks.clear();
    }

    /// `ask_user` in every window; the first answer (or dismissal) wins.
    async fn ask_user(self: &Arc<Self>, questions: Vec<Question>) -> Option<Vec<Reply>> {
        if self.unattended.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let id = self.next_approval.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let msg = ServerMsg::Ask {
            id,
            questions: questions.clone(),
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.inner.lock().unwrap();
            g.clients.retain(|_, c| c.send(msg.clone()).is_ok());
            if let Some(l) = &g.local {
                let (ltx, lrx) = oneshot::channel();
                if l.send(UiEvent::Ask(AskReq {
                    id,
                    questions,
                    reply: ltx,
                }))
                .is_ok()
                {
                    let hub = self.clone();
                    tokio::spawn(async move {
                        if let Ok(r) = lrx.await {
                            hub.answer_ask(id, r);
                        }
                    });
                }
            }
            g.asks.insert(id, (msg, tx));
        }
        let _open = Open {
            hub: self.clone(),
            id,
            ask: true,
        };
        self.set_blocked();
        rx.await.ok().flatten()
    }

    /// Replies from any window (`None` = dismissed). Late ones are ignored.
    pub fn answer_ask(&self, id: u64, replies: Option<Vec<Reply>>) {
        let mut g = self.inner.lock().unwrap();
        let Some((_, tx)) = g.asks.remove(&id) else { return };
        let _ = tx.send(replies);
        let done = ServerMsg::AskDone { id };
        g.clients.retain(|_, c| c.send(done.clone()).is_ok());
        if let Some(l) = &g.local {
            let _ = l.send(UiEvent::AskDone { id });
        }
        drop(g);
        self.set_blocked();
    }
}

/// An open approval or question: if its waiter is dropped (a stopped subagent's relay),
/// answer it as denied / dismissed so the card closes in every window. A no-op once answered.
struct Open {
    hub: Arc<Hub>,
    id: u64,
    ask: bool,
}

impl Drop for Open {
    fn drop(&mut self) {
        if self.ask {
            self.hub.answer_ask(self.id, None);
        } else {
            self.hub.answer(self.id, Answer::Deny(None));
        }
    }
}

/// The session's approver: every window of the hub (L6.1, L8.13). The hub is
/// created after the agent (it needs the handle), so it is found through this slot.
#[derive(Clone, Default)]
pub struct HubSlot(pub Arc<std::sync::OnceLock<std::sync::Weak<Hub>>>);

impl Approver for HubSlot {
    fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async move {
            let Some(hub) = self.0.get().and_then(|w| w.upgrade()) else {
                return Approval::Deny(None);
            };
            match hub.ask(req).await {
                Answer::Once => Approval::Once,
                Answer::Session => Approval::Session,
                Answer::Always => Approval::Always,
                Answer::Deny(why) => Approval::Deny(why),
            }
        })
    }
}

impl novacode_tools::ask::Asker for HubSlot {
    fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
        Box::pin(async move {
            let hub = self.0.get().and_then(|w| w.upgrade())?;
            hub.ask_user(questions).await
        })
    }
}

#[cfg(test)]
mod tests {
    use novacode_core::session::{Entry, EntryKind};

    #[test]
    fn compactions_counts_the_branch_compaction_entries() {
        let entry = |id: &str, kind| Entry {
            id: id.into(),
            parent: None,
            ts_ms: 0,
            kind,
        };
        let compaction = || EntryKind::Compaction {
            system: String::new(),
            summary: "s".into(),
            first_kept: "m".into(),
            tokens_before: 1000,
            notes: 3,
        };
        let message = || EntryKind::Message {
            message: novacode_types::Message::user("hi"),
        };
        let entries = [
            entry("a", message()),
            entry("b", compaction()),
            entry("c", message()),
            entry("d", compaction()),
        ];
        let branch: Vec<&Entry> = entries.iter().collect();
        assert_eq!(super::compactions(&branch), 2);
        assert_eq!(super::compactions(&branch[..1]), 0);
    }
}
