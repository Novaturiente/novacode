//! Plain data shared by every crate (L1). No I/O, no logic beyond helpers.
//!
//! Serde policy (L1.6): enums are tagged with `"type"`, optional fields default
//! and are skipped when empty, unknown fields are ignored, unknown blocks load
//! as [`Block::Unknown`]. Maps keep insertion order (`preserve_order`) so the
//! bytes we store are the bytes we send (L1.7 rule 3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opaque provider data kept byte-exact and sent back only to `provider`
/// (thinking signatures, encrypted reasoning, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replay {
    pub provider: String,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed arguments; `{}` when the model sent invalid JSON.
    pub arguments: Value,
    /// Raw text when the arguments were not a JSON object, so the model can fix it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<Replay>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<Replay>,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<Replay>,
    },
    Image {
        mime_type: String,
        /// Base64.
        data: String,
    },
    ToolCall(ToolCall),
    /// A vendor-run item (e.g. server-side web search), replayed verbatim to `provider` only.
    ProviderNative {
        provider: String,
        item: Value,
    },
    #[serde(other)]
    Unknown,
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Block::Text {
            text: text.into(),
            replay: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON schema of the arguments, in a fixed key order.
    pub schema: Value,
}

/// Error kinds (L0.3, L2a). Providers map status/codes to these; nothing
/// downstream matches on error strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ErrorKind {
    /// Context window exceeded.
    Overflow,
    RateLimit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    QuotaExhausted,
    Auth,
    Transient,
    InvalidRequest,
    Aborted,
    Other,
}

impl ErrorKind {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ErrorKind::RateLimit { .. } | ErrorKind::Transient)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    ToolUse,
    Length,
    Aborted,
    Error { kind: ErrorKind },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

/// API prices, USD per million tokens (L2.21).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

impl Usage {
    /// What this usage costs at `price`, in USD.
    pub fn cost(&self, price: &Cost) -> f64 {
        (self.input as f64 * price.input
            + self.output as f64 * price.output
            + self.cache_read as f64 * price.cache_read
            + self.cache_write as f64 * price.cache_write)
            / 1e6
    }

    /// Prompt tokens including cached ones.
    pub fn prompt_total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// How hard the model thinks (L2.16): pi's seven levels. Adapters clamp a
/// level to the nearest one the model supports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<Block>,
    pub stop: StopReason,
    #[serde(default)]
    pub usage: Usage,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Human-readable error text when `stop` is `Error`/`Aborted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Provider effort value this reply was made with, for adapters that resend it
    /// inside the conversation (Anthropic mid-conversation effort, L2.16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

impl AssistantMessage {
    /// All text blocks joined, the final answer for print mode.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|b| match b {
            Block::ToolCall(c) => Some(c),
            _ => None,
        })
    }
}

/// Conversation roles (L1.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// The first is the system prompt; later ones are appended deltas (L4.7).
    System {
        text: String,
    },
    User {
        content: Vec<Block>,
    },
    Assistant(AssistantMessage),
    /// Result of one tool call. `details` is UI-only and never sent to a model.
    ToolResult {
        call_id: String,
        tool_name: String,
        content: Vec<Block>,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Harness-only entry; converted or dropped before sending.
    Custom {
        kind: String,
        data: Value,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: vec![Block::text(text)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Text,
    Thinking,
    ToolCall,
    ProviderNative,
}

/// Streamed by an adapter for one model call (L1.4). Consumers build their own
/// view from deltas; the finished message comes once in `Done`/`Error`.
/// Tagged `stream` (not `type`) so it nests inside `AgentEvent::Stream` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum StreamEvent {
    BlockStart {
        index: usize,
        kind: BlockKind,
        /// Tool name for tool calls, so a UI can label it before arguments arrive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Text, thinking text, or raw tool-argument JSON fragment.
    Delta {
        index: usize,
        text: String,
    },
    BlockEnd {
        index: usize,
        block: Block,
    },
    Usage(Usage),
    /// Discard everything streamed so far; a retry follows.
    RetryRollback,
    /// Something the user should know about this call (e.g. a switch to another
    /// account). Shown, never sent to the model or stored.
    Notice {
        text: String,
    },
    Done(AssistantMessage),
    /// Terminal failure. The message holds any partial content and `stop = Error|Aborted`.
    Error(AssistantMessage),
}

/// Loop → consumers (L1.5). One stream feeds TUI, print, JSON, RPC and attach.
/// Must round-trip through JSON (the wire protocol, L8.7): see the test below.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    /// Live model output for the message being streamed.
    Stream(StreamEvent),
    /// A message was appended to the session (user, assistant, tool result, reminder).
    Message(Message),
    ToolStart {
        call: ToolCall,
    },
    ToolUpdate {
        call_id: String,
        text: String,
    },
    ToolEnd {
        call_id: String,
        is_error: bool,
        duration_ms: u64,
    },
    /// A failed model call will be retried after `delay_ms`.
    Retry {
        attempt: u32,
        delay_ms: u64,
        error: ErrorKind,
        message: String,
    },
    /// The loop stopped. `stop` is the last assistant message's stop reason.
    AgentEnd {
        stop: StopReason,
    },
    /// Stopped and nothing is queued (L3.10).
    Settled,
    /// The session moved to another folder (a worktree, L5.22).
    CwdChanged {
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// A long-running command is in the process manager (L5.11).
    ProcessStarted {
        id: String,
        name: String,
    },
    ProcessEnded {
        id: String,
        name: String,
        /// `None` = killed by a signal.
        code: Option<i32>,
        /// Stopped on purpose (the `process` tool, session exit).
        stopped: bool,
    },
    /// The context was compacted (L4.5): earlier messages replaced by notes.
    Compacted {
        tokens_before: u64,
        tokens_after: u64,
        notes: usize,
        /// No notes were available: earlier messages were dropped without a summary.
        fallback: bool,
    },
    /// A UI change from an extension (L9 UI API): panels, status segments, widgets.
    /// Never stored or sent to the model; every window applies it.
    Ui {
        /// The extension that sent it (keys and actions go back to it).
        ext: String,
        op: ui::UiOp,
    },
}

/// The extension UI API, version 1 (L9). Extensions send data; novacode draws it with
/// its own components, so internals stay private and customisations survive updates.
pub mod ui {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    pub const API_VERSION: u32 = 1;

    /// A styled piece of text; `style` is a theme token (`accent`, `dim`, `ok`, …).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Seg {
        pub text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub style: Option<String>,
    }

    /// A line: plain text, or styled segments.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum Line {
        Plain(String),
        Styled(Vec<Seg>),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
    pub struct Time {
        /// A fixed duration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ms: Option<u64>,
        /// Unix ms start: shown ticking.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub since: Option<u64>,
    }

    /// One row of a panel source (the user's pi panel node schema).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Node {
        pub id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent: Option<String>,
        pub label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tag: Option<String>,
        /// running, starting, waiting, queued, done, idle, failed or stopped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub icon: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub dim: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub detail: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub time: Option<Time>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub order: Option<f64>,
        /// Key → action: a `/command`, or a name sent back to the extension.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub actions: BTreeMap<String, String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum UiOp {
        /// A status-line segment `ext:<key>`; `text: None` removes it.
        Status {
            key: String,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            style: Option<String>,
        },
        /// Lines above (default) or below the editor; `lines: None` removes them.
        Widget {
            key: String,
            #[serde(default)]
            lines: Option<Vec<Line>>,
            #[serde(default)]
            below: bool,
        },
        /// Replace the status line; `None` gives it back.
        Footer {
            #[serde(default)]
            lines: Option<Vec<Line>>,
        },
        /// Lines above the transcript; `None` removes them.
        Header {
            #[serde(default)]
            lines: Option<Vec<Line>>,
        },
        /// Editor decoration: a title in the border and the border's style.
        Editor {
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            border: Option<String>,
        },
        OpenTab {
            name: String,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            priority: i32,
            #[serde(default)]
            width: Option<u16>,
            /// A raw region the extension paints (and gets keys for) instead of nodes.
            #[serde(default)]
            region: bool,
            /// Bring it to the front now.
            #[serde(default)]
            show: bool,
        },
        ShowTab {
            name: String,
        },
        CloseTab {
            name: String,
        },
        Source {
            tab: String,
            id: String,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            order: f64,
            #[serde(default)]
            newest_first: bool,
        },
        /// Add or replace a node (by id) in a source.
        Publish {
            tab: String,
            source: String,
            node: Box<Node>,
        },
        /// Remove a node, or (no id) clear the source.
        #[serde(rename = "unpublish")]
        Remove {
            tab: String,
            source: String,
            #[serde(default)]
            id: Option<String>,
        },
        /// Paint a region tab: lines clipped to its rectangle.
        Region {
            tab: String,
            lines: Vec<Line>,
        },
        /// A key the extension handles (when no built-in binding takes it).
        Shortcut {
            key: String,
            #[serde(default)]
            description: String,
        },
    }

    impl UiOp {
        /// Later ops with the same key replace earlier ones, so a window joining late
        /// gets the current UI without the whole history.
        pub fn slot(&self, ext: &str) -> String {
            match self {
                UiOp::Status { key, .. } => format!("status:{ext}:{key}"),
                UiOp::Widget { key, .. } => format!("widget:{ext}:{key}"),
                UiOp::Footer { .. } => "footer".into(),
                UiOp::Header { .. } => "header".into(),
                UiOp::Editor { .. } => "editor".into(),
                UiOp::OpenTab { name, .. } | UiOp::CloseTab { name } => format!("tab:{name}"),
                UiOp::ShowTab { name } => format!("show:{name}"),
                UiOp::Source { tab, id, .. } => format!("source:{tab}:{id}"),
                UiOp::Publish { tab, source, node } => format!("node:{tab}:{source}:{}", node.id),
                UiOp::Remove {
                    tab,
                    source,
                    id: Some(id),
                } => format!("node:{tab}:{source}:{id}"),
                UiOp::Remove { tab, source, id: None } => format!("clear:{tab}:{source}"),
                UiOp::Region { tab, .. } => format!("region:{tab}"),
                UiOp::Shortcut { key, .. } => format!("shortcut:{key}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_events_round_trip() {
        use super::*;
        let evs = vec![
            AgentEvent::Stream(StreamEvent::BlockStart {
                index: 0,
                kind: BlockKind::ToolCall,
                name: Some("bash".into()),
            }),
            AgentEvent::Stream(StreamEvent::Delta {
                index: 0,
                text: "x".into(),
            }),
            AgentEvent::Stream(StreamEvent::Usage(Usage::default())),
            AgentEvent::Stream(StreamEvent::RetryRollback),
            AgentEvent::Stream(StreamEvent::Notice { text: "n".into() }),
            AgentEvent::Settled,
        ];
        for e in evs {
            let j = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<AgentEvent>(&j).unwrap(), e, "{j}");
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_block_and_fields_load() {
        let m: Message = serde_json::from_value(json!({
            "role": "user", "future": 1,
            "content": [{"type": "text", "text": "hi", "x": 2}, {"type": "hologram", "z": 1}]
        }))
        .unwrap();
        assert_eq!(
            m,
            Message::User {
                content: vec![Block::text("hi"), Block::Unknown]
            }
        );
    }

    #[test]
    fn round_trip_keeps_key_order() {
        let call = ToolCall {
            id: "t1".into(),
            name: "read".into(),
            arguments: serde_json::from_str(r#"{"z":1,"a":2}"#).unwrap(),
            arguments_error: None,
            replay: None,
        };
        let s = serde_json::to_string(&Block::ToolCall(call)).unwrap();
        assert_eq!(
            s,
            r#"{"type":"tool_call","id":"t1","name":"read","arguments":{"z":1,"a":2}}"#
        );
        let stop = StopReason::Error {
            kind: ErrorKind::RateLimit {
                retry_after_ms: Some(5),
            },
        };
        let s = serde_json::to_string(&stop).unwrap();
        assert_eq!(s, r#"{"type":"error","kind":{"type":"rate_limit","retry_after_ms":5}}"#);
        assert_eq!(serde_json::from_str::<StopReason>(&s).unwrap(), stop);
    }
}

/// Token estimates (L2.21): the provider's last reported usage is exact for
/// everything up to that reply; only what came after it is estimated, at
/// pi's ~3.5 chars per token. No tokenizer: it's for budgets and display.
pub mod estimate {
    use super::{Block, Message};

    /// One image, as pi counts it (4800 chars ÷ 3.5).
    pub const IMAGE_TOKENS: u64 = 1372;

    pub fn text(s: &str) -> u64 {
        (s.len() as u64 * 2).div_ceil(7)
    }

    fn blocks(b: &[Block]) -> u64 {
        b.iter()
            .map(|b| match b {
                Block::Text { text: t, .. } | Block::Thinking { text: t, .. } => text(t),
                Block::Image { .. } => IMAGE_TOKENS,
                Block::ToolCall(c) => text(&c.name) + text(&c.arguments.to_string()),
                _ => 0,
            })
            .sum()
    }

    pub fn message(m: &Message) -> u64 {
        match m {
            Message::System { text: t } => text(t),
            Message::User { content } | Message::ToolResult { content, .. } => blocks(content),
            Message::Assistant(a) => blocks(&a.content),
            Message::Custom { .. } => 0,
        }
    }

    /// Context size of a projection: the last real usage plus estimates for
    /// the messages after it, or an estimate of everything when no reply has usage yet.
    pub fn context(messages: &[Message]) -> u64 {
        let last = messages
            .iter()
            .rposition(|m| matches!(m, Message::Assistant(a) if a.usage.prompt_total() > 0));
        match last {
            Some(i) => {
                let Message::Assistant(a) = &messages[i] else {
                    unreachable!()
                };
                a.usage.prompt_total() + a.usage.output + messages[i + 1..].iter().map(message).sum::<u64>()
            }
            None => messages.iter().map(message).sum(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{AssistantMessage, StopReason, Usage};

        #[test]
        fn hybrid() {
            let reply = |input| {
                Message::Assistant(AssistantMessage {
                    content: vec![Block::text("ok")],
                    stop: StopReason::ToolUse,
                    usage: Usage {
                        input,
                        output: 10,
                        ..Default::default()
                    },
                    provider: String::new(),
                    model: String::new(),
                    response_id: None,
                    error_message: None,
                    thinking_level: None,
                })
            };
            let seven = Message::user("x".repeat(70)); // 20 tokens
            assert_eq!(context(std::slice::from_ref(&seven)), 20, "no usage yet: all estimated");
            let msgs = [
                seven.clone(),
                reply(1000),
                seven.clone(),
                Message::User {
                    content: vec![Block::Image {
                        mime_type: "image/png".into(),
                        data: String::new(),
                    }],
                },
            ];
            assert_eq!(context(&msgs), 1000 + 10 + 20 + IMAGE_TOKENS);
            assert_eq!(
                context(&[reply(1000), reply(0)]),
                1010 + 1,
                "a reply without usage is estimated"
            );
        }
    }
}

/// `ask_user` (L5.21): one question for the user. Any number per call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Question {
    /// The question, as one clear sentence.
    pub question: String,
    /// A short label for it (a few words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The choices. The user can always type their own answer instead, so don't add an "other" choice.
    #[serde(default)]
    pub options: Vec<Choice>,
    /// More than one choice may be picked.
    #[serde(default)]
    pub multi: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Choice {
    /// 1–5 words.
    pub label: String,
    /// What picking it means, or its trade-off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Text shown beside the choice when it is focused: a code snippet, a mockup, a config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Your recommendation (at most one per question).
    #[serde(default)]
    pub recommended: bool,
}

/// The user's answer to one [`Question`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub question: String,
    /// Labels of the picked choices.
    #[serde(default)]
    pub chosen: Vec<String>,
    /// What the user typed instead of (or as well as) picking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}
