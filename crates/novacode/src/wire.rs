//! The wire protocol (L8.7): one JSONL protocol for `--json`, `--rpc` and the
//! session socket that `attach` uses (L8.5, L8.6, L8.13). Versioned: a client
//! reads `hello.protocol` and refuses a major it doesn't know. Public surface,
//! snapshot-tested (principle #7): `tests/wire.snap.jsonl`.

use novacode_tui::dialog::Answer;
use novacode_types::{AgentEvent, Cost, Message, ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bump on any breaking change to the messages below.
pub const PROTOCOL: u32 = 1;

/// What the session is doing (L8.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActState {
    Working,
    /// Waiting on a person: a permission card (later also ask_user).
    Blocked,
    #[default]
    Idle,
}

impl ActState {
    pub fn as_str(self) -> &'static str {
        match self {
            ActState::Working => "working",
            ActState::Blocked => "blocked",
            ActState::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Activity {
    pub state: ActState,
    /// thinking, writing, tool:<name>, compacting, retrying, permission:<tool>…
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CacheStats {
    /// Prompt tokens read from / written to the cache by the last reply.
    pub last_read: u64,
    pub last_write: u64,
    /// Session: cache reads over all prompt tokens, 0–100.
    pub hit_pct: f64,
    /// The last miss the detector named (L2.17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_miss: Option<String>,
}

/// What every client is told about the session, at `hello` and on change.
/// Also the status record (L8.15): `get_state`/`get_status`, the run file and
/// `novacode status`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct State {
    pub session: String,
    /// The session's name (`/name` or automatic, L4.18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    /// `provider/model`.
    pub model: String,
    /// The subscription account requests go to (`default`, `mili`, …); absent on API-key routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub level: ThinkingLevel,
    pub window_tokens: u64,
    /// A run is in progress (between `agent_start` and `settled`).
    pub busy: bool,
    /// API prices of the route; absent on subscriptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Cost>,
    /// USD spent so far on API-key routes.
    #[serde(default)]
    pub spent: f64,
    #[serde(default)]
    pub activity: Activity,
    /// The session file; absent for `--no-session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Context in use (last reply's usage plus estimates since), in tokens.
    #[serde(default)]
    pub context_tokens: u64,
    /// Compactions on the session's current branch (L9.19).
    #[serde(default)]
    pub compactions: u32,
    #[serde(default)]
    pub cache: CacheStats,
    /// Session token totals.
    #[serde(default)]
    pub tokens: novacode_types::Usage,
    /// Subscription quota windows, when the route has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<novacode_ai::quota::Quota>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// When this process took the session up, and when the running task started (unix ms).
    #[serde(default)]
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_started_ms: Option<u64>,
    /// Running processes in the process manager (L5.11): tmux `@agent_bg`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<String>,
    /// MCP servers: (ready, configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<(usize, usize)>,
    /// MCP servers, what loaded and the context split, for the Usage stats tab.
    #[serde(default, skip_serializing_if = "novacode_tui::panel::HostInfo::is_empty")]
    pub host: novacode_tui::panel::HostInfo,
}

impl State {
    /// What the status line and Usage tab need from the host (L9.12).
    pub fn info(&self) -> novacode_tui::UiEvent {
        let q = self.quota.as_ref();
        let (mcp_ready, mcp_total) = self.mcp.unwrap_or_default();
        novacode_tui::UiEvent::Info {
            five_hour: q.and_then(|q| q.five_hour.as_ref()).map(|w| w.used_pct),
            weekly: q.and_then(|q| q.weekly.as_ref()).map(|w| w.used_pct),
            mcp_ready,
            mcp_total,
            host: self.host.clone(),
        }
    }

    /// What [`info`](Self::info) is built from, to tell when to send it again.
    pub fn info_key(
        &self,
    ) -> (
        Option<novacode_ai::quota::Quota>,
        Option<(usize, usize)>,
        novacode_tui::panel::HostInfo,
    ) {
        (self.quota.clone(), self.mcp, self.host.clone())
    }

    /// Context use, 0–100.
    pub fn context_pct(&self) -> u64 {
        (self.context_tokens * 100).checked_div(self.window_tokens).unwrap_or(0)
    }

    /// Fold one stored message into the token, cache and context figures.
    pub fn add(&mut self, m: &Message) {
        match m {
            Message::Assistant(a) if a.usage.prompt_total() > 0 => {
                let u = &a.usage;
                self.context_tokens = u.prompt_total() + u.output;
                self.tokens.input += u.input;
                self.tokens.output += u.output;
                self.tokens.cache_read += u.cache_read;
                self.tokens.cache_write += u.cache_write;
                self.cache.last_read = u.cache_read;
                self.cache.last_write = u.cache_write;
                let prompt = self.tokens.input + self.tokens.cache_read + self.tokens.cache_write;
                self.cache.hit_pct =
                    (self.tokens.cache_read as f64 * 100.0 / prompt.max(1) as f64 * 10.0).round() / 10.0;
                if let Some(p) = &self.price {
                    self.spent += u.cost(p);
                }
            }
            other => self.context_tokens += novacode_types::estimate::message(other),
        }
    }
}

/// Server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Always the first line.
    Hello {
        protocol: u32,
        version: String,
        capabilities: Vec<String>,
        state: State,
    },
    /// The session's messages so far (attach and RPC catch-up).
    History { messages: Vec<Message> },
    /// A loop event (L1.5); streamed text arrives as deltas.
    Event { event: AgentEvent },
    /// The reply to a command that carried an `id`.
    Response {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        command: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        data: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Something the host tells this client: a notice, a page, a model list…
    /// (a serialised `UiEvent`, tagged by `kind`).
    Ui { ui: Value },
    /// A permission question (L6.1). Every client gets it; the first answer wins.
    Approve {
        id: u64,
        tool: String,
        detail: String,
        reason: String,
        rule: String,
        risky: bool,
    },
    /// Approval `id` was answered (here or elsewhere).
    ApprovalDone { id: u64 },
    /// `ask_user` (L5.21): questions for the user; every client gets them, the
    /// first `answer` wins.
    Ask {
        id: u64,
        questions: Vec<novacode_types::Question>,
    },
    /// Questions `id` were answered or dismissed (here or elsewhere).
    AskDone { id: u64 },
    /// The session's state changed (model, level, busy…).
    State { state: State },
    /// Run a client-registered tool (L7.8); answer with `tool_result`.
    ToolCall { id: u64, name: String, args: Value },
    /// The server is closing this connection.
    Bye { reason: String },
}

/// Client → server: a command, with an optional `id` echoed in its response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub cmd: ClientCmd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCmd {
    /// Starts a run, or is queued as a follow-up when one is running. A
    /// session slash command (`/compact`, `/model x`…) runs instead of being sent.
    /// Response data: `{"disposition": "started" | "queued" | "handled"}`.
    Prompt {
        text: String,
        /// Sent to the model as written: no `!cmd`, slash command, skill, template,
        /// extension input or image attachment (for pipelines whose text may start so).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        raw: bool,
    },
    /// Delivered before the next model call of the running turn (L3.7).
    Steer {
        text: String,
    },
    Abort,
    /// `provider/model[:level]`.
    SetModel {
        model: String,
    },
    SetThinking {
        level: ThinkingLevel,
    },
    Compact,
    /// Response data: the `/context` report as text.
    GetContext,
    /// The host answers with a `ui` `models` message.
    ListModels,
    /// Write the session to a file. Response data: `{"path": …}`.
    Export {
        #[serde(default)]
        html: bool,
    },
    /// Response data: [`State`], the status record (L8.15).
    #[serde(alias = "get_status")]
    GetState,
    /// Response data: `{"messages": [...]}`.
    GetMessages,
    /// Response data: `{"commands": [{"name", "description"}]}` (L8.9).
    GetCommands,
    /// Answer `ask_user` form `ask`, one reply per question; `null` = dismissed.
    Answer {
        ask: u64,
        replies: Option<Vec<novacode_types::Reply>>,
    },
    /// Answer approval `approval`.
    Approval {
        approval: u64,
        answer: Answer,
    },
    /// Tools this client runs (L7.8): the model sees them from the next run;
    /// calls arrive as `tool_call` and are answered with `tool_result`.
    RegisterTools {
        tools: Vec<ClientToolDef>,
    },
    /// Answer `tool_call` `call`.
    ToolResult {
        call: u64,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A key or panel action for extension `ext` (L9 UI API).
    ExtKey {
        ext: String,
        #[serde(default)]
        tab: Option<String>,
        #[serde(default)]
        node: Option<String>,
        key: String,
        #[serde(default)]
        width: u16,
        #[serde(default)]
        height: u16,
    },
    /// End the session (a `--bg` one included).
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
}

impl ClientCmd {
    pub fn name(&self) -> &'static str {
        match self {
            ClientCmd::Prompt { .. } => "prompt",
            ClientCmd::Steer { .. } => "steer",
            ClientCmd::Abort => "abort",
            ClientCmd::SetModel { .. } => "set_model",
            ClientCmd::SetThinking { .. } => "set_thinking",
            ClientCmd::Compact => "compact",
            ClientCmd::GetContext => "get_context",
            ClientCmd::ListModels => "list_models",
            ClientCmd::Export { .. } => "export",
            ClientCmd::GetState => "get_state",
            ClientCmd::GetMessages => "get_messages",
            ClientCmd::GetCommands => "get_commands",
            ClientCmd::Approval { .. } => "approval",
            ClientCmd::Answer { .. } => "answer",
            ClientCmd::Shutdown => "shutdown",
            ClientCmd::RegisterTools { .. } => "register_tools",
            ClientCmd::ToolResult { .. } => "tool_result",
            ClientCmd::ExtKey { .. } => "ext_key",
        }
    }
}

pub fn capabilities() -> Vec<String> {
    [
        "events",
        "history",
        "approvals",
        "ask_user",
        "prompt",
        "steer",
        "models",
        "thinking",
        "compact",
        "export",
    ]
    .map(String::from)
    .to_vec()
}

pub fn line<T: Serialize>(m: &T) -> String {
    let mut s = serde_json::to_string(m).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_tui::app::UiEvent;
    use novacode_types::{Block, StopReason, StreamEvent};

    fn samples() -> (Vec<ServerMsg>, Vec<Request>) {
        let state = State {
            session: "01abc".into(),
            cwd: "/w".into(),
            model: "claude/claude-opus-5-5".into(),
            level: ThinkingLevel::Medium,
            window_tokens: 200_000,
            busy: false,
            price: None,
            spent: 0.0,
            activity: Activity {
                state: ActState::Working,
                detail: "tool:bash".into(),
            },
            file: Some("/s/01abc.jsonl".into()),
            ..Default::default()
        };
        let server = vec![
            ServerMsg::Hello {
                protocol: PROTOCOL,
                version: "x".into(),
                capabilities: capabilities(),
                state: state.clone(),
            },
            ServerMsg::History {
                messages: vec![Message::user("hi")],
            },
            ServerMsg::Event {
                event: AgentEvent::Stream(StreamEvent::Delta {
                    index: 0,
                    text: "he".into(),
                }),
            },
            ServerMsg::Event {
                event: AgentEvent::AgentEnd { stop: StopReason::Stop },
            },
            ServerMsg::Response {
                id: Some("1".into()),
                command: "prompt".into(),
                ok: true,
                data: serde_json::json!({"disposition": "started"}),
                error: None,
            },
            ServerMsg::Response {
                id: None,
                command: "set_model".into(),
                ok: false,
                data: Value::Null,
                error: Some("unknown model".into()),
            },
            ServerMsg::Ui {
                ui: serde_json::to_value(UiEvent::Notice {
                    text: "n".into(),
                    error: false,
                })
                .unwrap(),
            },
            ServerMsg::Approve {
                id: 3,
                tool: "bash".into(),
                detail: "rm -rf build".into(),
                reason: "deletes files".into(),
                rule: "Bash(rm:*)".into(),
                risky: true,
            },
            ServerMsg::ApprovalDone { id: 3 },
            ServerMsg::Ask {
                id: 4,
                questions: vec![novacode_types::Question {
                    question: "Which db?".into(),
                    header: Some("Database".into()),
                    options: vec![novacode_types::Choice {
                        label: "SQLite".into(),
                        description: Some("zero setup".into()),
                        preview: None,
                        recommended: true,
                    }],
                    multi: false,
                }],
            },
            ServerMsg::AskDone { id: 4 },
            ServerMsg::State { state },
            ServerMsg::Bye {
                reason: "session ended".into(),
            },
        ];
        let client = vec![
            Request {
                id: Some("1".into()),
                cmd: ClientCmd::Prompt {
                    text: "fix it".into(),
                    raw: false,
                },
            },
            Request {
                id: Some("1b".into()),
                cmd: ClientCmd::Prompt {
                    text: "/home/x is a path, !not a command".into(),
                    raw: true,
                },
            },
            Request {
                id: None,
                cmd: ClientCmd::Steer {
                    text: "also tests".into(),
                },
            },
            Request {
                id: None,
                cmd: ClientCmd::Abort,
            },
            Request {
                id: Some("2".into()),
                cmd: ClientCmd::SetModel {
                    model: "codex/gpt-5.5:high".into(),
                },
            },
            Request {
                id: None,
                cmd: ClientCmd::SetThinking {
                    level: ThinkingLevel::Low,
                },
            },
            Request {
                id: None,
                cmd: ClientCmd::Export { html: true },
            },
            Request {
                id: None,
                cmd: ClientCmd::Approval {
                    approval: 3,
                    answer: Answer::Deny(Some("not now".into())),
                },
            },
            Request {
                id: Some("9".into()),
                cmd: ClientCmd::GetState,
            },
            Request {
                id: None,
                cmd: ClientCmd::Answer {
                    ask: 4,
                    replies: Some(vec![novacode_types::Reply {
                        question: "Which db?".into(),
                        chosen: vec!["SQLite".into()],
                        text: None,
                        preview: None,
                    }]),
                },
            },
        ];
        let _ = Block::text("");
        (server, client)
    }

    /// Principle #7: the wire format only changes on purpose.
    /// `UPDATE_SNAPSHOTS=1` rewrites the file.
    #[test]
    fn wire_snapshot() {
        let (server, client) = samples();
        let mut out = String::new();
        for m in &server {
            out += &line(m);
        }
        for r in &client {
            out += &line(r);
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire.snap.jsonl");
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &out).unwrap();
        }
        let want = std::fs::read_to_string(&path).expect("run with UPDATE_SNAPSHOTS=1 once");
        assert_eq!(
            out, want,
            "wire format changed: bump PROTOCOL if it breaks clients, then UPDATE_SNAPSHOTS=1"
        );
        // Every line reads back.
        for l in want.lines().take(server.len()) {
            serde_json::from_str::<ServerMsg>(l).unwrap();
        }
        for l in want.lines().skip(server.len()) {
            serde_json::from_str::<Request>(l).unwrap();
        }
        // Minimal hand-written commands parse too.
        let r: Request = serde_json::from_str(r#"{"type":"abort"}"#).unwrap();
        assert_eq!(r.cmd.name(), "abort");
    }
}
