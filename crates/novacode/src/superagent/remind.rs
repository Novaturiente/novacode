//! Re-asks (L10.9 "Super restarts after upgrade, re-asks"): every card super opens for the
//! user (its `say` questions, `ask_user`, code-made cards, relayed task questions and
//! approvals) is tracked while it waits; the links' tick re-announces each one every
//! `super.watch.remindMinutes` with a desktop notification and a short notice. No wake:
//! super's model is never run for a reminder.

use futures_util::future::BoxFuture;
use novacode_core::policy::{Approval, ApprovalRequest, Approver};
use novacode_tools::ask::Asker;
use novacode_types::{Question, Reply};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many words of the decision a reminder shows.
const DECISION_WORDS: usize = 15;

/// The cards open now: id → (task, decision, last announced, unix seconds).
#[derive(Default)]
pub struct Pending {
    open: Mutex<HashMap<u64, (String, String, u64)>>,
    next: AtomicU64,
}

/// Takes its card off [`Pending`] when the wait ends (answered, dismissed or dropped).
struct Guard(Arc<Pending>, u64);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.open.lock().unwrap().remove(&self.1);
    }
}

impl Pending {
    fn open(self: &Arc<Self>, task: String, decision: String, now: u64) -> Guard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.open.lock().unwrap().insert(id, (task, decision, now));
        Guard(self.clone(), id)
    }

    /// The reminder lines due at `now` (one per card left unanswered `every` seconds since
    /// it opened or was last announced); each due card counts as announced now. `every` 0: none.
    pub fn due(&self, now: u64, every: u64) -> Vec<String> {
        if every == 0 {
            return vec![];
        }
        let mut g = self.open.lock().unwrap();
        let mut ids: Vec<u64> = g
            .iter()
            .filter(|(_, (_, _, last))| now.saturating_sub(*last) >= every)
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| {
                let (task, decision, last) = g.get_mut(&id)?;
                *last = now;
                Some(format!("{task}: still waiting for your answer: {decision}"))
            })
            .collect()
    }

    /// `asker`, with each of its cards tracked here while it waits.
    pub fn asker(self: &Arc<Self>, asker: Arc<dyn Asker>) -> Arc<dyn Asker> {
        Arc::new(Tracked(self.clone(), asker))
    }

    /// `approver`, with each approval card tracked here while it waits.
    pub fn approver(self: &Arc<Self>, approver: Arc<dyn Approver>) -> Arc<dyn Approver> {
        Arc::new(TrackedApprovals(self.clone(), approver))
    }
}

struct Tracked(Arc<Pending>, Arc<dyn Asker>);

impl Asker for Tracked {
    fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
        let (task, decision) = questions.first().map(describe).unwrap_or_default();
        let guard = self.0.open(task, decision, super::board::now());
        Box::pin(async move {
            let _guard = guard;
            self.1.ask(questions).await
        })
    }
}

struct TrackedApprovals(Arc<Pending>, Arc<dyn Approver>);

impl Approver for TrackedApprovals {
    fn approve(&self, req: ApprovalRequest) -> BoxFuture<'_, Approval> {
        let (task, rest) = labelled(&req.reason);
        let task = task.unwrap_or_else(|| "super".into());
        let decision = words(&format!("approve {}: {rest}", req.tool));
        let guard = self.0.open(task, decision, super::board::now());
        Box::pin(async move {
            let _guard = guard;
            self.1.approve(req).await
        })
    }
}

/// `[label] rest` → (label, rest).
fn labelled(s: &str) -> (Option<String>, &str) {
    match s.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((l, rest)) => (Some(l.to_string()), rest),
        None => (None, s),
    }
}

/// A card's task and its one decision: the label of a relayed card (`[T1-x] …`), else the
/// header's `<label>: …` part, else the header; the decision is a `say` question's
/// `Decision:` field, else the question's last paragraph; at most 15 words.
pub(super) fn describe(q: &Question) -> (String, String) {
    let (label, text) = labelled(&q.question);
    let header = q.header.as_deref().unwrap_or_default();
    let task = label
        .or_else(|| header.split_once(": ").map(|(l, _)| l.to_string()))
        .or_else(|| (!header.is_empty()).then(|| header.to_string()))
        .unwrap_or_else(|| "super".into());
    let decision = match text.split_once("\nDecision:\n- ") {
        Some((_, d)) => d.lines().next().unwrap_or_default(),
        None => text
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .last()
            .unwrap_or_default(),
    };
    (task, words(decision.trim_start_matches("- ")))
}

/// The first [`DECISION_WORDS`] words of `s`, with `…` when there were more.
fn words(s: &str) -> String {
    let all: Vec<&str> = s.split_whitespace().collect();
    let mut out = all[..all.len().min(DECISION_WORDS)].join(" ");
    if all.len() > DECISION_WORDS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(header: Option<&str>, text: &str) -> Question {
        Question {
            question: text.into(),
            header: header.map(String::from),
            options: vec![],
            multi: false,
        }
    }

    #[test]
    fn a_card_names_its_task_and_one_decision_cut_to_15_words() {
        let relayed = q(Some("db"), "[T1-x] which db?");
        assert_eq!(describe(&relayed), ("T1-x".into(), "which db?".into()));
        let code = q(
            Some("T2-api: merge"),
            "T2-api ready 100% · tests pass\n\nMerge T2-api into main? The task rebases first.",
        );
        assert_eq!(
            describe(&code),
            (
                "T2-api".into(),
                "Merge T2-api into main? The task rebases first.".into()
            )
        );
        let said = q(
            Some("T3-ui, T4-db: question"),
            "T3-ui\nline\n\nRisk:\n- low\n\nDecision:\n- one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen\n",
        );
        assert_eq!(
            describe(&said),
            (
                "T3-ui, T4-db".into(),
                "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen…".into()
            )
        );
        assert_eq!(describe(&q(Some("Switch models"), "Switch?")).0, "Switch models");
        assert_eq!(describe(&q(None, "Name it?")).0, "super");
    }

    #[test]
    fn an_open_card_is_due_every_interval_until_it_closes() {
        let p = Arc::new(Pending::default());
        let g = p.open("T1-x".into(), "which db?".into(), 100);
        assert!(p.due(100 + 1799, 1800).is_empty());
        assert_eq!(
            p.due(100 + 1800, 1800),
            ["T1-x: still waiting for your answer: which db?"]
        );
        assert!(
            p.due(100 + 1801, 1800).is_empty(),
            "announced again only after another interval"
        );
        assert_eq!(p.due(100 + 3600, 1800).len(), 1);
        assert!(p.due(100 + 9999, 0).is_empty(), "0 turns reminders off");
        drop(g);
        assert!(p.due(100 + 99999, 1800).is_empty(), "an answered card is no longer due");
    }

    struct Never;
    impl Asker for Never {
        fn ask(&self, _: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn a_tracked_card_is_pending_while_its_wait_runs_and_gone_when_dropped() {
        let p = Arc::new(Pending::default());
        let a = p.asker(Arc::new(Never));
        let wait = a.ask(vec![q(Some("T1-x: close"), "Close T1-x?")]);
        let far = super::super::board::now() + 10_000;
        assert_eq!(p.due(far, 60), ["T1-x: still waiting for your answer: Close T1-x?"]);
        drop(wait);
        assert!(p.due(far + 10_000, 60).is_empty());
    }
}
