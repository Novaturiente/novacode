//! Relaying a child session's cards (L10.1 subagents, L10.9 super tasks): an
//! `ask_user` form or an approval that arrived on a child's wire is re-opened
//! in the parent's windows with a label in front, and the user's answer goes
//! back as the wire command that answers it. Shared by `agents.rs` and
//! `superagent/link.rs`, so there is one relay.

use crate::wire::{ClientCmd, ServerMsg};
use novacode_core::policy::{Approval, ApprovalRequest, Approver};
use novacode_tools::ask::Asker;
use novacode_tui::dialog::Answer;
use novacode_types::{Question, Reply};
use serde_json::json;

/// A card a child opened.
#[derive(Debug, Clone, PartialEq)]
pub enum Card {
    Ask {
        id: u64,
        questions: Vec<Question>,
    },
    Approve {
        id: u64,
        tool: String,
        detail: String,
        reason: String,
        rule: String,
        risky: bool,
    },
}

impl Card {
    pub fn from_msg(m: &ServerMsg) -> Option<Card> {
        match m {
            ServerMsg::Ask { id, questions } => Some(Card::Ask {
                id: *id,
                questions: questions.clone(),
            }),
            ServerMsg::Approve {
                id,
                tool,
                detail,
                reason,
                rule,
                risky,
            } => Some(Card::Approve {
                id: *id,
                tool: tool.clone(),
                detail: detail.clone(),
                reason: reason.clone(),
                rule: rule.clone(),
                risky: *risky,
            }),
            _ => None,
        }
    }

    /// The child's card id.
    pub fn id(&self) -> u64 {
        match self {
            Card::Ask { id, .. } | Card::Approve { id, .. } => *id,
        }
    }

    /// What it asks, in a few words.
    pub fn what(&self) -> String {
        match self {
            Card::Ask { questions, .. } => questions
                .iter()
                .map(|q| q.question.as_str())
                .collect::<Vec<_>>()
                .join(" / "),
            Card::Approve { tool, detail, .. } => format!("to run {tool} {detail}"),
        }
    }
}

/// How the parent's copy is labelled: `ask` goes in front of every question,
/// `approval` in front of the approval's reason.
pub struct Label<'a> {
    pub ask: &'a str,
    pub approval: &'a str,
}

/// Open `card` in the parent's windows and wait for the answer. Returns the
/// command that answers the child's card and the answer in a few words.
pub async fn relay(asker: &dyn Asker, approver: &dyn Approver, card: Card, label: Label<'_>) -> (ClientCmd, String) {
    match card {
        Card::Ask { id, mut questions } => {
            for q in &mut questions {
                q.question = format!("{}{}", label.ask, q.question);
            }
            let replies = asker.ask(questions).await;
            let said = replies.as_deref().map_or("dismissed".to_string(), said);
            (ClientCmd::Answer { ask: id, replies }, said)
        }
        Card::Approve {
            id,
            tool,
            detail,
            reason,
            rule,
            risky,
        } => {
            let key = if tool == "bash" { "command" } else { "path" };
            let req = ApprovalRequest {
                tool,
                args: json!({ key: detail }),
                reason: format!("{}{reason}", label.approval),
                rule,
                risky,
            };
            let answer = match approver.approve(req).await {
                Approval::Once => Answer::Once,
                Approval::Session => Answer::Session,
                Approval::Always => Answer::Always,
                Approval::Deny(why) => Answer::Deny(why),
            };
            let said = match &answer {
                Answer::Once => "allowed once".to_string(),
                Answer::Session => "allowed for the session".to_string(),
                Answer::Always => "allowed always".to_string(),
                Answer::Deny(None) => "denied".to_string(),
                Answer::Deny(Some(w)) => format!("denied: {w}"),
            };
            (ClientCmd::Approval { approval: id, answer }, said)
        }
    }
}

fn said(replies: &[Reply]) -> String {
    replies
        .iter()
        .map(|r| {
            let mut parts = r.chosen.clone();
            parts.extend(r.text.clone());
            parts.join(", ")
        })
        .collect::<Vec<_>>()
        .join(" / ")
}
