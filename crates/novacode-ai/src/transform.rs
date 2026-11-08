//! Cross-model transform (L2.15): make stored history valid for the model
//! about to receive it. Pure and deterministic (L1.7 rule 3), so the same
//! history always gives the same bytes and the cached prefix holds.
//!
//! - Assistant replies that errored or were aborted are left out, with their tool results.
//! - Thinking from another provider or model is dropped; so is replay data.
//! - Tool-call ids are rewritten when the target's id rules need it.
//! - Every tool call gets a result (a stand-in if none was stored).
//! - Images become a placeholder for text-only models, and all but the newest few for every model.

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

/// Only a call novacode stopped during (crash, kill, power loss) has no stored result (L4.17).
pub const NO_RESULT: &str = "No result: novacode stopped while this tool ran, so it may have partly run. Check its effects before running it again.";
pub const IMAGE_OMITTED: &str = "(image omitted: model does not support images)";
/// How many images (the newest, across the request) are sent; each older one
/// is re-sent with every request otherwise (L5).
pub const KEEP_IMAGES: usize = 4;
pub const OLD_IMAGE: &str = "[older image omitted to save context; read the file again to see it]";

pub fn for_target(messages: &[Message], t: &Target) -> Vec<Message> {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: BTreeSet<String> = BTreeSet::new();
    // System and custom messages between a call and its results, put after the results.
    let mut held: Vec<Message> = Vec::new();
    let close = |out: &mut Vec<Message>,
                 pending: &mut Vec<ToolCall>,
                 answered: &mut BTreeSet<String>,
                 held: &mut Vec<Message>| {
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
        out.append(held);
    };
    for m in messages {
        match m {
            Message::Assistant(a) => {
                close(&mut out, &mut pending, &mut answered, &mut held);
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
                // A result whose call isn't in the reply just before it (or a second
                // result for one call) makes providers reject the whole request.
                if !pending.iter().any(|c| c.id == call_id) || !answered.insert(call_id.clone()) {
                    continue;
                }
                out.push(Message::ToolResult {
                    call_id,
                    tool_name: tool_name.clone(),
                    content: images(content, t.images),
                    is_error: *is_error,
                    details: details.clone(),
                });
            }
            Message::User { content } => {
                close(&mut out, &mut pending, &mut answered, &mut held);
                out.push(Message::User {
                    content: images(content, t.images),
                });
            }
            // System and custom messages between a call and its result wait until the results are in.
            other => {
                if pending.is_empty() {
                    out.push(other.clone());
                } else {
                    held.push(other.clone());
                }
            }
        }
    }
    close(&mut out, &mut pending, &mut answered, &mut held);
    drop_old_images(&mut out);
    out
}

/// Only the newest [`KEEP_IMAGES`] images are sent. The cut moves only when
/// a new image arrives, so the cached prefix changes once per image.
fn drop_old_images(out: &mut [Message]) {
    let contents = || {
        out.iter().filter_map(|m| match m {
            Message::User { content } | Message::ToolResult { content, .. } => Some(content),
            _ => None,
        })
    };
    let total = contents()
        .flatten()
        .filter(|b| matches!(b, Block::Image { .. }))
        .count();
    let mut old = total.saturating_sub(KEEP_IMAGES);
    for m in out.iter_mut() {
        if old == 0 {
            return;
        }
        let (Message::User { content } | Message::ToolResult { content, .. }) = m else {
            continue;
        };
        for b in content.iter_mut() {
            if old > 0 && matches!(b, Block::Image { .. }) {
                *b = Block::text(OLD_IMAGE);
                old -= 1;
            }
        }
    }
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
    fn results_stay_next_to_their_calls() {
        let msgs = vec![
            Message::user("hi"),
            asst("openai", "gpt-5", StopReason::ToolUse, vec![call("c1")]),
            Message::System { text: "note".into() },
            result("c1"),
            result("c1"),     // duplicate
            result("orphan"), // its call was never stored
        ];
        let t = Target {
            provider: "openai",
            model: "gpt-5",
            images: true,
            normalize_id: None,
        };
        let out = for_target(&msgs, &t);
        assert_eq!(out.len(), 4, "{out:#?}");
        assert!(matches!(&out[2], Message::ToolResult { call_id, is_error: false, .. } if call_id == "c1"));
        assert!(matches!(&out[3], Message::System { .. }), "note after the result");
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
    fn only_the_newest_images_are_sent() {
        let img = || Block::Image {
            mime_type: "image/png".into(),
            data: "AA".into(),
        };
        let mut msgs = vec![Message::User {
            content: vec![Block::text("look"), img()],
        }];
        for i in 0..KEEP_IMAGES {
            let id = format!("c{i}");
            msgs.push(asst("openai", "gpt-5", StopReason::ToolUse, vec![call(&id)]));
            msgs.push(result(&id));
        }
        let t = Target {
            provider: "openai",
            model: "gpt-5",
            images: true,
            normalize_id: None,
        };
        let out = for_target(&msgs, &t);
        let Message::User { content } = &out[0] else { panic!() };
        assert_eq!(content[1], Block::text(OLD_IMAGE), "the oldest image is replaced");
        let kept: usize = out
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .flatten()
            .filter(|b| matches!(b, Block::Image { .. }))
            .count();
        assert_eq!(kept, KEEP_IMAGES);
        assert_eq!(&out[1..], &msgs[1..], "the newest images are untouched");
    }

    #[test]
    fn safe_ids() {
        assert_eq!(safe_id("toolu_01AB-c"), "toolu_01AB-c");
        let s = safe_id(&"x|".repeat(300));
        assert!(s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        assert_ne!(safe_id("a|b"), safe_id("a/b"));
    }
}
