//! One live session shared by its windows (L8.13): the hub mirrors what a new
//! client needs to catch up (messages, the in-flight partial, open approvals)
//! and fans events out to every connected client in order.

use crate::wire::{ServerMsg, State, capabilities};
use futures_util::future::BoxFuture;
use novacode_core::agent::SessionHandle;
use novacode_core::policy::{Approval, ApprovalRequest, Approver};
use novacode_tui::app::UiEvent;
use novacode_tui::dialog::{Answer, ApproveReq};
use novacode_types::{AgentEvent, Message};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Events since the last stored message, enough to rebuild a live block.
/// shortcut: capped by count, oldest tool output dropped first; a client that
/// attaches mid-way through a huge tool output sees only its tail.
const TAIL_MAX: usize = 4000;

struct Pending {
    msg: ServerMsg,
    reply: oneshot::Sender<Answer>,
}

struct Inner {
    state: State,
    history: Vec<Message>,
    tail: Vec<AgentEvent>,
    clients: BTreeMap<u64, mpsc::UnboundedSender<ServerMsg>>,
    next_client: u64,
    approvals: BTreeMap<u64, Pending>,
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
                clients: BTreeMap::new(),
                next_client: 0,
                approvals: BTreeMap::new(),
                local,
                closed: false,
            }),
            handle,
            next_approval: 1.into(),
            changed: tokio::sync::watch::channel(state).0,
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
        let busy = match &ev {
            AgentEvent::AgentStart => Some(true),
            AgentEvent::Settled => Some(false),
            _ => None,
        };
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
            _ => {
                g.tail.push(ev.clone());
                if g.tail.len() > TAIL_MAX
                    && let Some(i) = g.tail.iter().position(|e| matches!(e, AgentEvent::ToolUpdate { .. }))
                {
                    g.tail.remove(i);
                }
            }
        }
        let msg = ServerMsg::Event { event: ev };
        g.clients.retain(|_, tx| tx.send(msg.clone()).is_ok());
        if let Some(b) = busy {
            g.state.busy = b;
            let s = g.state.clone();
            drop(g);
            self.state_changed(s, false);
        }
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
        first.extend(g.tail.iter().map(|e| ServerMsg::Event { event: e.clone() }));
        first.extend(g.approvals.values().map(|p| p.msg.clone()));
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

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
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
            f(&mut g.state);
            let after = (g.state.model.clone(), g.state.level, g.state.window_tokens);
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
