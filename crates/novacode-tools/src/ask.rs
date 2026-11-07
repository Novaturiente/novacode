//! `ask_user` (L5.21): structured questions for the user, answered in any window
//! of the session or over RPC. `None` from the asker = dismissed.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use novacode_types::{Question, Reply};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Whoever can answer: the host's windows and RPC clients.
pub trait Asker: Send + Sync {
    fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>>;

    /// [`Asker::ask`] that gives up as dismissed (`None`) when `cancel` fires (abort,
    /// `novacode stop`, SIGTERM): the dropped wait closes the card in every window.
    /// A tool waiting on a card must use this, or the run never ends and stop hangs.
    fn ask_until<'a>(
        &'a self,
        questions: Vec<Question>,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Option<Vec<Reply>>> {
        Box::pin(async move {
            tokio::select! {
                r = self.ask(questions) => r,
                _ = cancel.cancelled() => None,
            }
        })
    }
}

pub struct AskUser(pub Arc<dyn Asker>);

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// The questions, asked one after another in a single form.
    questions: Vec<Question>,
}

pub fn render(replies: &[Reply]) -> String {
    replies
        .iter()
        .map(|r| {
            let mut a = r.chosen.join(", ");
            if let Some(t) = &r.text {
                if !a.is_empty() {
                    a += "; ";
                }
                a += t;
            }
            if a.is_empty() {
                a = "(no answer)".into();
            }
            if let Some(p) = &r.preview {
                a += &format!("\n(the user rewrote that choice's preview:)\n{p}");
            }
            format!("Q: {}\nA: {a}", r.question)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl Tool for AskUser {
    fn name(&self) -> &str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the user one or more structured questions when a decision is theirs or the request is ambiguous: each with 2–6 short choices (mark one recommended), an optional description and preview per choice, and `multi` for several picks. The user can always type their own answer. Group related questions into one call."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            if a.questions.is_empty() {
                return ToolOutput::error("`questions` is empty");
            }
            match self.0.ask_until(a.questions, &ctx.cancel).await {
                Some(r) => ToolOutput {
                    details: Some(json!({ "replies": r })),
                    ..ToolOutput::text(render(&r))
                },
                None if ctx.cancel.is_cancelled() => {
                    ToolOutput::error("Cancelled: the run was stopped before the user answered.")
                }
                None => ToolOutput::text(
                    "The user dismissed the questions without answering. Ask in chat, or go on with your best judgement and say what you assumed.",
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};

    struct Fixed(Option<Vec<Reply>>);
    impl Asker for Fixed {
        fn ask(&self, q: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
            assert_eq!(q[0].options.len(), 2);
            Box::pin(async move { self.0.clone() })
        }
    }

    #[tokio::test]
    async fn asks_and_renders() {
        let d = tempfile::tempdir().unwrap();
        let args = json!({"questions": [{"question": "Which db?", "options": [{"label": "Postgres", "recommended": true}, {"label": "SQLite"}]}]});
        let r = Reply {
            question: "Which db?".into(),
            chosen: vec!["Postgres".into()],
            text: Some("with pgvector".into()),
            preview: None,
        };
        let o = AskUser(Arc::new(Fixed(Some(vec![r]))))
            .execute(args.clone(), &ctx(d.path()))
            .await;
        assert_eq!(text(&o), "Q: Which db?\nA: Postgres; with pgvector");
        let o = AskUser(Arc::new(Fixed(None))).execute(args, &ctx(d.path())).await;
        assert!(!o.is_error && text(&o).contains("dismissed"));
    }

    /// An abort ends the wait on a card nobody answers (stop used to hang here).
    #[tokio::test]
    async fn abort_ends_an_unanswered_card() {
        struct Never;
        impl Asker for Never {
            fn ask(&self, _: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
                Box::pin(std::future::pending())
            }
        }
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        c.cancel.cancel();
        let args = json!({"questions": [{"question": "Which?", "options": [{"label": "a"}, {"label": "b"}]}]});
        let o = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            AskUser(Arc::new(Never)).execute(args, &c),
        )
        .await
        .expect("the cancelled ask still waits");
        assert!(o.is_error && text(&o).contains("Cancelled"));
    }
}
