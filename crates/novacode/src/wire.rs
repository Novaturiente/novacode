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

/// What every client is told about the session, at `hello` and on change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub session: String,
    pub cwd: String,
    /// `provider/model`.
    pub model: String,
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
    /// The session's state changed (model, level, busy…).
    State { state: State },
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
    /// Starts a run, or is queued as a follow-up when one is running.
    /// Response data: `{"disposition": "started" | "queued"}`.
    Prompt {
        text: String,
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
    /// Response data: [`State`].
    GetState,
    /// Response data: `{"messages": [...]}`.
    GetMessages,
    /// Answer approval `approval`.
    Approval {
        approval: u64,
        answer: Answer,
    },
    /// End the session (a `--bg` one included).
    Shutdown,
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
            ClientCmd::Approval { .. } => "approval",
            ClientCmd::Shutdown => "shutdown",
        }
    }
}

pub fn capabilities() -> Vec<String> {
    [
        "events",
        "history",
        "approvals",
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
            ServerMsg::State { state },
            ServerMsg::Bye {
                reason: "session ended".into(),
            },
        ];
        let client = vec![
            Request {
                id: Some("1".into()),
                cmd: ClientCmd::Prompt { text: "fix it".into() },
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
