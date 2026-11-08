//! Super's cards never block its run (L10.9 "Super session review fixes"): a card super
//! opens (its `say` questions, its `ask_user`, code's confirm cards from `task`) is shown and
//! the tool returns at once; the user's answer comes back later as a wake, so task wakes
//! never queue behind an open card.

use super::task::Super;
use futures_util::future::BoxFuture;
use novacode_tools::ask::Asker;
use novacode_types::{Question, Reply};
use std::future::Future;
use std::sync::{Arc, Mutex};

/// What a tool returns once its card is open.
pub const ASKED: &str = "Asked the user; the answer arrives as a wake. Don't wait for it: go on with other \
work, or end your turn.";

/// The wake for an answered card: `The user answered <task or General>: Q: <decision> A: <answer>`.
pub fn answered(qs: &[Question], replies: &[Reply]) -> String {
    let who = qs.first().map(who).unwrap_or_else(|| "General".into());
    let qa: Vec<String> = qs
        .iter()
        .zip(replies)
        .map(|(q, r)| format!("Q: {} A: {}", decision(q), answer(r)))
        .collect();
    format!("The user answered {who}: {}", qa.join("; "))
}

/// The line for a card closed without an answer.
pub fn dismissed(qs: &[Question]) -> String {
    let who = qs.first().map(who).unwrap_or_else(|| "General".into());
    let q = qs.first().map(decision).unwrap_or_default();
    format!("The user closed the card without answering {who}: Q: {q}")
}

/// A card's task label, or General.
fn who(q: &Question) -> String {
    match super::remind::describe(q).0 {
        s if s == "super" => "General".into(),
        s => s,
    }
}

/// A card's one decision, short.
fn decision(q: &Question) -> String {
    super::remind::describe(q).1
}

/// A reply as one answer: the choices, the typed text, a rewritten preview.
fn answer(r: &Reply) -> String {
    let all = novacode_tools::ask::render(std::slice::from_ref(r));
    let head = format!("Q: {}\nA: ", r.question);
    all.strip_prefix(&head).unwrap_or(&all).to_string()
}

/// Wake super with `text`, unless its session is ending (a card closing then got no answer).
pub fn wake(sup: &Super, text: String) {
    if let Some(l) = sup.links.get().filter(|l| !l.closing()) {
        l.wake(text);
    }
}

/// One line for super's next wake, unless its session is ending.
pub fn note(sup: &Super, text: String) {
    if let Some(l) = sup.links.get().filter(|l| !l.closing()) {
        l.note(text);
    }
}

/// The cards one `task` call opened, with their answers.
#[derive(Default)]
struct Cards {
    opened: tokio::sync::Notify,
    asked: Mutex<Vec<(Question, Option<Vec<Reply>>)>>,
}

tokio::task_local! {
    static CARDS: Arc<Cards>;
}

/// Super's own asker for code-made cards: inside [`detach`] each card it opens is recorded,
/// with its answer; elsewhere (cards already off the run) it only asks.
pub struct Seen(pub Arc<dyn Asker>);

impl Asker for Seen {
    fn ask(&self, questions: Vec<Question>) -> BoxFuture<'_, Option<Vec<Reply>>> {
        let cards = CARDS.try_with(|c| c.clone()).ok();
        let first = questions.first().cloned();
        let wait = self.0.ask(questions);
        Box::pin(async move {
            if let Some(c) = &cards {
                c.opened.notify_one();
            }
            let r = wait.await;
            if let (Some(c), Some(q)) = (cards, first) {
                c.asked.lock().unwrap().push((q, r.clone()));
            }
            r
        })
    }
}

/// The `task` calls (action and task) whose card is open now: a second call for the same one
/// is refused until the first is answered, as a blocked run once made impossible.
static OPEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The key of a `task` call: its action and task(s).
pub fn key(args: &serde_json::Value) -> String {
    format!("{} {} {}", args["action"], args["task"], args["tasks"])
}

fn claim(key: &str) -> bool {
    let mut open = OPEN.lock().unwrap();
    if open.iter().any(|k| k.as_str() == key) {
        return false;
    }
    open.push(key.to_string());
    true
}

fn release(key: &str) {
    OPEN.lock().unwrap().retain(|k| k != key);
}

/// Aborts the work when the call is dropped before its card opened.
struct Abort(Option<tokio::task::AbortHandle>);

impl Drop for Abort {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Run `work` (a `task` call) as before until it opens a card: a result or refusal before
/// that comes back in the call. Once a card opens, the call returns [`ASKED`] and the work
/// goes on off super's run; when it ends, super is woken with the answer and the outcome.
pub async fn detach(
    sup: &Arc<Super>,
    key: String,
    work: impl Future<Output = Result<String, String>> + Send + 'static,
) -> Result<String, String> {
    if OPEN.lock().unwrap().contains(&key) {
        return Err("its card for this is open already; the answer arrives as a wake".into());
    }
    let cards = Arc::new(Cards::default());
    let mut h = tokio::spawn(CARDS.scope(cards.clone(), work));
    let mut guard = Abort(Some(h.abort_handle()));
    tokio::select! {
        r = &mut h => return r.unwrap_or_else(|e| Err(format!("the task call ended: {e}"))),
        _ = cards.opened.notified() => {}
    }
    guard.0 = None;
    let claimed = claim(&key);
    let sup = Arc::downgrade(sup);
    tokio::spawn(async move {
        let out = h.await.unwrap_or_else(|e| Err(format!("the task call ended: {e}")));
        if claimed {
            release(&key);
        }
        let Some(sup) = sup.upgrade() else { return };
        let mut lines: Vec<String> = cards
            .asked
            .lock()
            .unwrap()
            .iter()
            .map(|(q, r)| match r {
                Some(r) => answered(std::slice::from_ref(q), r),
                None => dismissed(std::slice::from_ref(q)),
            })
            .collect();
        lines.push(match out {
            Ok(t) => t,
            Err(e) => format!("Refused: {e}"),
        });
        wake(&sup, lines.join("\n"));
    });
    Ok(ASKED.into())
}
