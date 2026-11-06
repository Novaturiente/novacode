//! A context rebuilt on every request (L10.9, super's context over days): the request
//! carries the system prompt, a state block written by code and only the newest chat
//! that fits a token budget. The session file keeps everything; only the request shrinks.

use crate::session::Store;
use novacode_types::{Block, Message, estimate};
use std::sync::Arc;

/// Renders the state block from the session (todo list) and whatever the caller owns (a board).
pub type StateFn = Arc<dyn Fn(&Store) -> String + Send + Sync>;

/// An agent option: rebuild each request from the projection (`Agent::with_context_builder`).
#[derive(Clone)]
pub struct Builder {
    /// Tokens of chat resent, the current turn included.
    pub budget: u64,
    pub state: StateFn,
    /// Silent checks (super's health checks): see [`Quiet`].
    pub quiet: Option<Quiet>,
}

/// A reply of exactly `OK` (no tool calls) to a wake note starting with `marker` is
/// hidden: its streamed text is rolled back, no message event goes to the windows, and
/// both the note and the reply are left out of later requests and of the transcript (the
/// session file keeps them). `on_ok` runs after each one.
#[derive(Clone)]
pub struct Quiet {
    pub marker: String,
    pub on_ok: Arc<dyn Fn() + Send + Sync>,
}

impl Builder {
    pub fn build(&self, store: &Store, messages: Vec<Message>) -> Vec<Message> {
        rebuild(messages, self.budget, &(self.state)(store))
    }
}

/// System messages first (moved to the front, in order), then `state` as a user message,
/// then the newest whole chat entries that fit `budget` with the current turn, then the
/// current turn (from the last user message that doesn't sit inside a tool loop).
/// A tool call is never split from its results; finished turns lose their thinking.
pub fn rebuild(messages: Vec<Message>, budget: u64, state: &str) -> Vec<Message> {
    let (systems, chat): (Vec<Message>, Vec<Message>) =
        messages.into_iter().partition(|m| matches!(m, Message::System { .. }));
    let chat: Vec<Message> = chat
        .into_iter()
        .filter(|m| !matches!(m, Message::Custom { .. }))
        .collect();
    let start = (0..chat.len())
        .rev()
        .find(|&i| {
            matches!(chat[i], Message::User { .. }) && (i == 0 || !matches!(chat[i - 1], Message::ToolResult { .. }))
        })
        .unwrap_or(0);
    let mut chat = chat;
    let current = chat.split_off(start);
    // Whole units: a user message alone, an assistant reply with its tool results.
    let mut units: Vec<Vec<Message>> = Vec::new();
    for m in chat {
        match m {
            Message::ToolResult { .. } if !units.is_empty() => units.last_mut().unwrap().push(m),
            Message::ToolResult { .. } => {}
            Message::Assistant(mut a) => {
                a.content.retain(|b| !matches!(b, Block::Thinking { .. }));
                if !a.content.is_empty() {
                    units.push(vec![Message::Assistant(a)]);
                }
            }
            m => units.push(vec![m]),
        }
    }
    let mut used: u64 = current.iter().map(estimate::message).sum();
    let mut kept = Vec::new();
    for u in units.into_iter().rev() {
        let cost: u64 = u.iter().map(estimate::message).sum();
        if used + cost > budget {
            break;
        }
        used += cost;
        kept.push(u);
    }
    let mut out = systems;
    out.push(Message::user(format!("<state>\n{}\n</state>", state.trim_end())));
    out.extend(kept.into_iter().rev().flatten());
    out.extend(current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::{AssistantMessage, StopReason, ToolCall};

    fn reply(blocks: Vec<Block>) -> Message {
        Message::Assistant(AssistantMessage {
            content: blocks,
            stop: StopReason::Stop,
            usage: Default::default(),
            provider: "faux".into(),
            model: "m".into(),
            response_id: None,
            error_message: None,
            thinking_level: None,
        })
    }

    fn call(id: &str) -> Block {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "task".into(),
            arguments: serde_json::json!({}),
            arguments_error: None,
            replay: None,
        })
    }

    fn result(id: &str, text: &str) -> Message {
        Message::ToolResult {
            call_id: id.into(),
            tool_name: "task".into(),
            content: vec![Block::text(text)],
            is_error: false,
            details: None,
        }
    }

    fn thinking(t: &str) -> Block {
        Block::Thinking {
            text: t.into(),
            replay: None,
        }
    }

    fn texts(ms: &[Message]) -> Vec<String> {
        ms.iter()
            .map(|m| match m {
                Message::System { text } => format!("S:{text}"),
                Message::User { content } | Message::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                Message::Assistant(a) => format!("A:{}", a.text()),
                Message::Custom { .. } => "C".into(),
            })
            .collect()
    }

    /// About 1000 tokens of text (3500 chars at 3.5 chars per token).
    fn big(tag: &str) -> String {
        format!("{tag}{}", "x".repeat(3500))
    }

    #[test]
    fn the_newest_whole_entries_that_fit_the_budget_follow_the_state_block() {
        let mut ms = vec![Message::System { text: "sys".into() }];
        for i in 0..20 {
            ms.push(Message::user(big(&format!("u{i}"))));
            ms.push(reply(vec![Block::text(big(&format!("a{i}")))]));
        }
        ms.push(Message::user("wake"));
        let out = rebuild(ms, 6000, "BOARD");
        let t = texts(&out);
        assert_eq!(t[0], "S:sys");
        assert_eq!(t[1], "<state>\nBOARD\n</state>");
        assert_eq!(t.last().unwrap(), "wake");
        let total: u64 = out[2..].iter().map(estimate::message).sum();
        assert!(total <= 6000, "{total}");
        // Newest kept, oldest gone; 5 entries of ~1000 fit beside the wake.
        assert!(t.iter().any(|s| s.starts_with("A:a19")) && !t.iter().any(|s| s.starts_with("u0")));
        assert_eq!(out.len(), 2 + 5 + 1, "{t:?}");
    }

    #[test]
    fn a_tool_call_is_never_split_from_its_result_and_old_thinking_is_dropped() {
        let ms = vec![
            Message::user("old"),
            reply(vec![thinking("old thought"), Block::text("let me look"), call("c1")]),
            result("c1", &big("r1")),
            reply(vec![thinking("only thinking")]),
            Message::user("now"),
            reply(vec![thinking("current thought"), call("c2")]),
            result("c2", "r2"),
        ];
        // Room for the result but not for the call with it: neither is kept.
        let out = rebuild(ms.clone(), 1010, "");
        let t = texts(&out);
        assert!(!t.iter().any(|s| s.starts_with("r1") || s == "A:let me look"), "{t:?}");
        // Enough room: both, without the old thinking; the current turn keeps its own.
        let out = rebuild(ms, 6000, "");
        let ids: Vec<&str> = out
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["c1", "c2"]);
        let thoughts: Vec<&str> = out
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.content.iter().filter_map(|b| match b {
                    Block::Thinking { text, .. } => Some(text.as_str()),
                    _ => None,
                })),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(thoughts, ["current thought"]);
    }

    #[test]
    fn a_reminder_inside_a_tool_loop_does_not_start_a_new_turn() {
        let ms = vec![
            Message::user(big("prompt")),
            reply(vec![thinking("t"), call("c1")]),
            result("c1", "r"),
            Message::user("<system-reminder>\nnote\n</system-reminder>"),
        ];
        // The whole run is the current turn: kept with its thinking even over budget.
        let out = rebuild(ms, 10, "");
        assert_eq!(out.len(), 5);
        assert!(matches!(&out[2], Message::Assistant(a) if a.content.len() == 2));
    }
}
