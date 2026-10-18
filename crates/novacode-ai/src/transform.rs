//! Cross-model transform (L2.15): make stored history valid for the model
//! about to receive it. Pure and deterministic (L1.7 rule 3), so the same
//! history always gives the same bytes and the cached prefix holds.
//!
//! - Assistant replies that errored or were aborted are left out, with their tool results.
//! - Thinking from another provider or model is dropped; so is replay data.
//! - Tool-call ids are rewritten when the target's id rules need it.
//! - Every tool call gets a result (a stand-in if none was stored).
//! - Images become a placeholder for text-only models.

use novacode_types::{Block, Message, StopReason, ToolCall};
use std::collections::{BTreeMap, BTreeSet};

/// Who is about to receive the history.
pub struct Target<'a> {
    /// Replay namespace of the adapter (e.g. `anthropic`, or the vendor id for Chat Completions).
    pub provider: &'a str,
    pub model: &'a str,
    pub images: bool,
    /// Rewrites a foreign tool-call id into one the target accepts (`None` = keep ids).
    pub normalize_id: Option<fn(&str) -> String>,
}

pub const NO_RESULT: &str = "No result provided";
pub const IMAGE_OMITTED: &str = "(image omitted: model does not support images)";

pub fn for_target(messages: &[Message], t: &Target) -> Vec<Message> {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: BTreeSet<String> = BTreeSet::new();
    let close = |out: &mut Vec<Message>, pending: &mut Vec<ToolCall>, answered: &mut BTreeSet<String>| {
        for c in pending.drain(..) {
            if !answered.contains(&c.id) {
                out.push(Message::ToolResult {
                    call_id: c.id,
                    tool_name: c.name,
                    content: vec![Block::text(NO_RESULT)],
                    is_error: true,
                    details: None,
                });
            }
        }
        answered.clear();
    };
    for m in messages {
        match m {
            Message::Assistant(a) => {
                close(&mut out, &mut pending, &mut answered);
                if matches!(a.stop, StopReason::Error { .. } | StopReason::Aborted) {
                    dropped.extend(a.tool_calls().map(|c| c.id.clone()));
                    continue;
                }
                let same = a.provider == t.provider && a.model == t.model;
                let mut a = a.clone();
                a.content = a
                    .content
                    .into_iter()
                    .filter_map(|b| match b {
                        Block::Thinking { .. } if !same => None,
                        Block::Thinking { ref text, ref replay } if replay.is_none() && text.trim().is_empty() => None,
                        Block::Text { text, replay } => Some(Block::Text {
                            text,
                            replay: replay.filter(|_| same),
                        }),
                        Block::ToolCall(mut c) => {
                            if !same {
                                c.replay = None;
                                if let Some(f) = t.normalize_id {
                                    let new = f(&c.id);
                                    if new != c.id {
                                        ids.insert(c.id.clone(), new.clone());
                                        c.id = new;
                                    }
                                }
                            }
                            Some(Block::ToolCall(c))
                        }
                        b => Some(b),
                    })
                    .collect();
                pending = a.tool_calls().cloned().collect();
                out.push(Message::Assistant(a));
            }
            Message::ToolResult {
                call_id,
                tool_name,
                content,
                is_error,
                details,
            } => {
                if dropped.contains(call_id) {
                    continue;
                }
                let call_id = ids.get(call_id).cloned().unwrap_or_else(|| call_id.clone());
                answered.insert(call_id.clone());
                out.push(Message::ToolResult {
                    call_id,
                    tool_name: tool_name.clone(),
                    content: images(content, t.images),
                    is_error: *is_error,
                    details: details.clone(),
                });
            }
            Message::User { content } => {
                close(&mut out, &mut pending, &mut answered);
                out.push(Message::User {
                    content: images(content, t.images),
                });
            }
            // System and custom messages between a call and its result wait until the results are in.
            other => {
                if pending.is_empty() {
                    out.push(other.clone());
                } else {
                    close(&mut out, &mut pending, &mut answered);
                    out.push(other.clone());
                }
            }
        }
    }
    close(&mut out, &mut pending, &mut answered);
    out
}

fn images(content: &[Block], allowed: bool) -> Vec<Block> {
    if allowed {
        return content.to_vec();
    }
    let mut out: Vec<Block> = Vec::new();
    for b in content {
        match b {
            Block::Image { .. } => {
                if !matches!(out.last(), Some(Block::Text { text, .. }) if text == IMAGE_OMITTED) {
                    out.push(Block::text(IMAGE_OMITTED));
                }
            }
            b => out.push(b.clone()),
        }
    }
    out
}

/// Ids matching `^[a-zA-Z0-9_-]{1,64}$` (Anthropic, most Chat Completions vendors).
/// Deterministic: the same foreign id always maps to the same new id.
pub fn safe_id(id: &str) -> String {
    let ok =
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        return id.to_string();
    }
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    // FNV-1a keeps it stable across runs and versions (no random hasher).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{clean}_{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_types::{AssistantMessage, ErrorKind, Replay, Usage};
    use serde_json::json;

    fn asst(provider: &str, model: &str, stop: StopReason, content: Vec<Block>) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            stop,
            usage: Usage::default(),
            provider: provider.into(),
            model: model.into(),
            response_id: None,
            error_message: None,
            thinking_level: None,
        })
    }
    fn call(id: &str) -> Block {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({}),
            arguments_error: None,
            replay: Some(Replay {
                provider: "openai".into(),
                data: json!("sig"),
            }),
        })
    }
    fn result(id: &str) -> Message {
        Message::ToolResult {
            call_id: id.into(),
            tool_name: "read".into(),
            content: vec![
                Block::text("ok"),
                Block::Image {
                    mime_type: "image/png".into(),
                    data: "AA".into(),
                },
            ],
            is_error: false,
            details: None,
        }
    }
    fn thinking(text: &str) -> Block {
        Block::Thinking {
            text: text.into(),
            replay: Some(Replay {
                provider: "openai".into(),
                data: json!({}),
            }),
        }
    }

    #[test]
    fn foreign_history_becomes_valid() {
        let foreign = "fc_abc|def/+==";
        let msgs = vec![
            Message::user("hi"),
            asst(
                "openai",
                "gpt-5",
                StopReason::ToolUse,
                vec![thinking("hmm"), call(foreign), call("call_2")],
            ),
            result(foreign), // call_2 never got a result
            asst(
                "openai",
                "gpt-5",
                StopReason::Error {
                    kind: ErrorKind::Transient,
                },
                vec![call("lost")],
            ),
            result("lost"),
            Message::user("next"),
        ];
        let t = Target {
            provider: "anthropic",
            model: "claude-x",
            images: false,
            normalize_id: Some(safe_id),
        };
        let out = for_target(&msgs, &t);
        let Message::Assistant(a) = &out[1] else { panic!() };
        assert_eq!(a.content.len(), 2, "thinking dropped");
        let new_id = a.tool_calls().next().unwrap().id.clone();
        assert_eq!(new_id, safe_id(foreign));
        assert!(a.tool_calls().all(|c| c.replay.is_none()));
        let Message::ToolResult { call_id, content, .. } = &out[2] else {
            panic!()
        };
        assert_eq!(call_id, &new_id);
        assert_eq!(content[1], Block::text(IMAGE_OMITTED));
        let Message::ToolResult {
            call_id,
            content,
            is_error,
            ..
        } = &out[3]
        else {
            panic!()
        };
        assert_eq!((call_id.as_str(), *is_error), ("call_2", true));
        assert_eq!(content[0], Block::text(NO_RESULT));
        assert_eq!(out.len(), 5, "errored reply and its result left out: {out:#?}");
        assert_eq!(for_target(&msgs, &t), out, "deterministic");
    }

    #[test]
    fn same_model_keeps_everything() {
        let msgs = vec![
            Message::user("hi"),
            asst(
                "openai",
                "gpt-5",
                StopReason::ToolUse,
                vec![thinking("hmm"), call("c1")],
            ),
            result("c1"),
        ];
        let t = Target {
            provider: "openai",
            model: "gpt-5",
            images: true,
            normalize_id: Some(safe_id),
        };
        assert_eq!(for_target(&msgs, &t), msgs);
    }

    #[test]
    fn safe_ids() {
        assert_eq!(safe_id("toolu_01AB-c"), "toolu_01AB-c");
        let s = safe_id(&"x|".repeat(300));
        assert!(s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        assert_ne!(safe_id("a|b"), safe_id("a/b"));
    }
}
