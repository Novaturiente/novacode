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

impl Usage {
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    Done(AssistantMessage),
    /// Terminal failure. The message holds any partial content and `stop = Error|Aborted`.
    Error(AssistantMessage),
}

/// Loop → consumers (L1.5). One stream feeds TUI, print, JSON, RPC and attach.
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
}

#[cfg(test)]
mod tests {
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
