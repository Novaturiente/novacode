//! Super's context over days (L10.9): the request is rebuilt on every wake from a
//! code-written state block and the newest chat; the facts that matter (charters,
//! open requests) live on the board. Also super's history search and the decision check.

use super::board::{Board, NOTES_CAP};
use super::link::Inform;
use novacode_ai::adapter::{CacheRetention, Request};
use novacode_core::context_window;
use novacode_core::memory::Worker;
use novacode_core::session::Store;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::{AgentEvent, Block, Message, StreamEvent};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Tokens of chat super's model gets with each request (user decision).
pub const CHAT_BUDGET: u64 = 6000;
/// Chars a history search returns.
const HISTORY_CHARS: usize = 2000;

/// Super's Models section, from the session's current (provider, model).
pub type ModelsFn = Arc<dyn Fn(Option<(String, String)>) -> String + Send + Sync>;

/// Super's context builder: the board at `<dir>/board.json` and the session's open todo
/// items as the state block, then the newest chat in [`CHAT_BUDGET`]. A health check
/// answered with exactly `OK` is hidden and left out; `on_ok` runs then (records the check).
/// Plain final text gets one nudge to use `say`, then is sent as a `note`. Each request
/// also sets `trigger`: what started the current run, for `say`'s frame. `models` gets the
/// session's current model and returns the Models section, which ends the state block.
pub fn builder(
    dir: PathBuf,
    on_ok: Arc<dyn Fn() + Send + Sync>,
    models: ModelsFn,
    trigger: super::say::Trigger,
    shown: Shown,
) -> context_window::Builder {
    context_window::Builder {
        budget: CHAT_BUDGET,
        quiet: Some(context_window::Quiet {
            marker: super::link::HEALTH_CHECK.into(),
            on_ok,
        }),
        plain: Some(context_window::Plain {
            tool: "say".into(),
            nudge: super::say::NUDGE.into(),
            args: Arc::new(|text: &str| json!({ "kind": "note", "body": text })),
        }),
        state: Arc::new(move |store: &Store| {
            *trigger.lock().unwrap() = super::say::trigger(&store.branch());
            let open = open_requests(store);
            let mut block = match Board::load(&dir) {
                Ok(b) => b.state_block(&open),
                Err(e) => format!("The board couldn't be read ({e}); call `task list`.\n"),
            };
            block += &models(store.last_model());
            *shown.lock().unwrap() = block.clone();
            block
        }),
    }
}

/// Super's open `todo` items: the user's requests that are not (yet) tasks.
fn open_requests(store: &Store) -> Vec<String> {
    use novacode_tools::todo::Status;
    let Some((items, _)) = store.todo() else { return vec![] };
    items
        .into_iter()
        .filter(|i| matches!(i.status, Status::Pending | Status::Active))
        .map(|i| match i.note {
            Some(n) => format!("{} ({n})", i.text),
            None => i.text,
        })
        .collect()
}

/// One plain text call to the helper model.
pub async fn helper_text(w: &Worker, system: &str, user: String) -> Result<String, String> {
    let req = Request {
        tail: Vec::new(),
        model: w.model.clone(),
        messages: vec![
            Message::System { text: system.into() },
            Message::User {
                content: vec![Block::text(user)],
            },
        ],
        tools: vec![],
        max_tokens: 1024,
        cache: CacheRetention::Short,
        thinking: None,
        images: false,
        session: None,
    };
    let mut rx = w.adapter.stream(req, CancellationToken::new());
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Done(m) => return Ok(m.text().trim().to_string()),
            StreamEvent::Error(m) => return Err(m.error_message.unwrap_or_else(|| format!("{:?}", m.stop))),
            _ => {}
        }
    }
    Err("the helper model's stream ended without a result".into())
}

const SUMMARISE: &str = "You condense notes about one task that a coding agent runs for a user. Keep every \
decision, limit, preference and answer, with names, paths and numbers exact; drop repetition. Reply with the \
condensed notes only, as short sentences on one paragraph, no preamble.";

/// `notes` within [`NOTES_CAP`]: the newest that fit half the cap stay; the older ones become
/// one summary by the helper model. `None`: unchanged (within the cap, or the helper failed,
/// so nothing is lost and the next note tries again).
pub async fn fit_notes(notes: &[String], helper: Option<&Worker>) -> Option<Vec<String>> {
    let total: usize = notes.iter().map(String::len).sum();
    if total <= NOTES_CAP {
        return None;
    }
    let mut keep = 0;
    let mut used = 0;
    for n in notes.iter().rev() {
        if keep > 0 && used + n.len() > NOTES_CAP / 2 {
            break;
        }
        used += n.len();
        keep += 1;
    }
    let (old, new) = notes.split_at(notes.len() - keep);
    if old.is_empty() {
        return None;
    }
    let head = match helper {
        Some(w) => {
            let text = helper_text(w, SUMMARISE, old.join("\n")).await.ok()?;
            let text: String = text.chars().take(NOTES_CAP / 2).collect();
            format!("[summary of {} earlier notes] {text}", old.len())
        }
        // shortcut: without a helper model the oldest notes are cut, marked; add a summary
        // by the session model if super ever runs without one.
        None => format!("[older notes trimmed: {}]", old.len()),
    };
    Some(std::iter::once(head).chain(new.iter().cloned()).collect())
}

/// `history_search`: super's own session file, read-only, for chat older than its window.
pub struct HistorySearch {
    pub file: PathBuf,
}

/// Matching messages of `file`, newest first, at most [`HISTORY_CHARS`] chars.
pub fn search_history(file: &std::path::Path, query: &str) -> String {
    let hits = novacode_core::export::search(&[file.to_path_buf()], query, None, 1000);
    if hits.is_empty() {
        return format!("nothing in your past conversation contains all of: {query}");
    }
    let mut out = String::new();
    let total = hits.len();
    let mut shown = 0;
    for h in hits.iter().rev() {
        let line = format!(
            "[{}Z {}] {}\n",
            novacode_core::memory::utc_minute(h.ts_ms),
            h.role,
            h.snippet
        );
        if out.len() + line.len() > HISTORY_CHARS {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < total {
        out.push_str(&format!(
            "({shown} of {total} matches shown, newest first; add words to narrow it)\n"
        ));
    }
    out
}

impl Tool for HistorySearch {
    fn name(&self) -> &str {
        "history_search"
    }
    fn description(&self) -> &str {
        "Search your own past conversation with the user (older than what you see now) for messages containing \
all the given words. Read-only; newest matches first, capped."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "query": { "type": "string", "description": "Words that must all appear (case-insensitive)." } },
            "required": ["query"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(q) = args["query"]
                .as_str()
                .filter(|q| !q.trim().is_empty())
                .map(String::from)
            else {
                return ToolOutput::error("`query` is missing");
            };
            let file = self.file.clone();
            let out = tokio::task::spawn_blocking(move || search_history(&file, &q))
                .await
                .unwrap_or_else(|e| format!("search failed: {e}"));
            ToolOutput::text(out)
        })
    }
}

const DECISION: &str = "You check a coding agent's bookkeeping. Given: the user's message to the agent, what \
the agent recorded during its reply (task notes, memories, todo items, tasks started), and the agent's state \
block (its task charters and open requests, as code shows them to it). A decision counts only when it lasts: a \
task's scope or limits, a preference or a rule that should still matter in later turns (e.g. \"use port 8080 for \
T1\", \"never deploy on Fridays\", \"also check the logs tomorrow\"). Not decisions: questions, thanks, status \
checks; one-off requests (\"just this once\", \"do X now\"); instructions already carried out in this reply; \
anything about how the agent's own tools work internally (card types, argument fields, bookkeeping mechanics). \
A decision already covered by the charters, open requests, memory or anything recorded is not missing. If the \
message holds a lasting decision nothing covers, reply with one line naming it. Otherwise reply exactly NONE.";

/// The helper's verdict on one user turn: the unrecorded decision, if any. `state`: super's
/// state block (charters, open requests), so what is on record already doesn't count.
pub async fn unrecorded_decision(w: &Worker, user: &str, recorded: &[String], state: &str) -> Option<String> {
    let rec = match recorded.is_empty() {
        true => "(nothing)".to_string(),
        false => recorded.join("\n"),
    };
    let state = match state.trim().is_empty() {
        true => "(not available)",
        false => state,
    };
    let text = helper_text(
        w,
        DECISION,
        format!("User's message:\n{user}\n\nRecorded during the reply:\n{rec}\n\nState block:\n{state}"),
    )
    .await
    .ok()?;
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim().to_string();
    (!line.eq_ignore_ascii_case("none") && !line.to_lowercase().starts_with("none")).then_some(line)
}

/// The tool calls that record something: what the check counts as recorded.
fn recording(m: &Message) -> Vec<String> {
    let Message::Assistant(a) = m else { return vec![] };
    a.tool_calls()
        .filter(|c| {
            let action = c.arguments["action"].as_str().unwrap_or("");
            match c.name.as_str() {
                "memory" | "todo" => true,
                "task" => matches!(action, "note" | "spawn" | "scope" | "send"),
                _ => false,
            }
        })
        .map(|c| format!("{} {}", c.name, c.arguments))
        .collect()
}

/// The state block of super's latest request (its context builder writes it).
pub type Shown = Arc<std::sync::Mutex<String>>;

/// After each run that took a user message, the helper model checks it for a decision
/// super did not record; a miss wakes super with a one-line nudge. No helper: no check.
pub fn decision_check(
    mut events: tokio::sync::broadcast::Receiver<AgentEvent>,
    helper: Option<Worker>,
    inform: Arc<dyn Inform>,
    shown: Shown,
) {
    let Some(w) = helper else { return };
    tokio::spawn(async move {
        let (mut user, mut recorded) = (Vec::<String>::new(), Vec::<String>::new());
        loop {
            match events.recv().await {
                Ok(AgentEvent::Message(m)) => match &m {
                    Message::User { content } => {
                        let text: String = content
                            .iter()
                            .filter_map(|b| match b {
                                Block::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.trim().is_empty() {
                            user.push(text);
                        }
                    }
                    m => recorded.extend(recording(m)),
                },
                Ok(AgentEvent::AgentEnd { .. }) => {
                    let (u, r) = (std::mem::take(&mut user), std::mem::take(&mut recorded));
                    if u.is_empty() {
                        continue;
                    }
                    let state = shown.lock().unwrap().clone();
                    if let Some(d) = unrecorded_decision(&w, &u.join("\n\n"), &r, &state).await {
                        inform.wake(nudge(&d));
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
}

/// What a decision-check nudge starts with (never a `say` trigger).
pub const DECISION_CHECK: &str = "Decision check:";

/// The one-line nudge for an unrecorded decision.
pub fn nudge(decision: &str) -> String {
    format!(
        "{DECISION_CHECK} the user's last message holds a decision you didn't record ({decision}). Record it now: \
`task note` for a task, `memory` for a preference, `todo` for a request; then stop."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use novacode_ai::faux::{Faux, FauxReply};

    fn worker(replies: &[&str]) -> (Worker, Faux) {
        let f = Faux::new(replies.iter().map(|r| FauxReply::text(r)));
        (
            Worker {
                adapter: Arc::new(f.clone()),
                provider: "faux".into(),
                model: "small".into(),
            },
            f,
        )
    }

    #[tokio::test]
    async fn notes_over_the_cap_are_summarised_by_the_helper_never_dropped_silently() {
        let notes: Vec<String> = (0..10).map(|i| format!("n{i} {}", "x".repeat(400))).collect();
        assert!(fit_notes(&notes[..3], None).await.is_none(), "within the cap");
        let (w, f) = worker(&["n0 to n5 condensed"]);
        let out = fit_notes(&notes, Some(&w)).await.unwrap();
        assert!(
            out[0].starts_with("[summary of 7 earlier notes] n0 to n5 condensed"),
            "{out:?}"
        );
        assert_eq!(&out[1..], &notes[7..]);
        assert!(out.iter().map(String::len).sum::<usize>() <= NOTES_CAP);
        let asked = format!("{:?}", f.requests.lock().unwrap()[0].messages);
        assert!(asked.contains("n0 ") && asked.contains("n6 ") && !asked.contains("n7 "));
        // The helper failing: unchanged, so nothing is lost.
        let (w, _) = worker(&[]);
        assert!(fit_notes(&notes, Some(&w)).await.is_none());
        // No helper: the newest stay and the cut is marked.
        let out = fit_notes(&notes, None).await.unwrap();
        assert_eq!(out[0], "[older notes trimmed: 7]");
    }

    #[tokio::test]
    async fn the_decision_check_names_an_unrecorded_decision_and_passes_none() {
        let (w, f) = worker(&["Use port 8080 for T1", "NONE"]);
        let d = unrecorded_decision(&w, "use port 8080 for T1", &[], "").await;
        assert_eq!(d.as_deref(), Some("Use port 8080 for T1"));
        let sent = format!("{:?}", f.requests.lock().unwrap()[0].messages);
        assert!(
            sent.contains("use port 8080 for T1") && sent.contains("(nothing)"),
            "{sent}"
        );
        let rec = vec!["task {\"action\":\"note\"}".to_string()];
        assert_eq!(unrecorded_decision(&w, "port 8080", &rec, "").await, None);
    }

    /// S5 (live test): the check nudged for "just this once" and for how super's tools work
    /// inside. The helper sees super's state block (charters, open requests) and is told to
    /// count only lasting decisions not recorded there.
    #[tokio::test]
    async fn the_decision_check_sees_the_state_block_and_skips_one_offs() {
        let (w, f) = worker(&["NONE"]);
        let state = "## Charters\n- T1-api: port 8080\n## Open requests\n- check logs tomorrow\n";
        assert_eq!(
            unrecorded_decision(&w, "just this once, skip the review", &[], state).await,
            None
        );
        let reqs = f.requests.lock().unwrap();
        let Message::System { text: system } = &reqs[0].messages[0] else {
            panic!()
        };
        for rule in ["one-off", "already carried out", "internally", "charters", "memory"] {
            assert!(system.contains(rule), "{rule}: {system}");
        }
        let user = format!("{:?}", reqs[0].messages[1]);
        assert!(
            user.contains("T1-api: port 8080") && user.contains("check logs tomorrow"),
            "{user}"
        );
    }

    struct Wakes(std::sync::Mutex<Vec<String>>);
    impl Inform for Wakes {
        fn wake(&self, text: String) {
            self.0.lock().unwrap().push(text);
        }
        fn note(&self, _: String) {}
        fn notice(&self, _: String) {}
        fn desktop(&self, _: String, _: String) {}
    }

    #[tokio::test]
    async fn a_user_turn_with_a_missed_decision_wakes_super_with_a_nudge_and_wakes_do_not() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let (w, f) = worker(&["Never deploy on Fridays"]);
        let wakes = Arc::new(Wakes(Default::default()));
        decision_check(rx, Some(w), wakes.clone(), Default::default());
        // A run with no user message (a wake): no check.
        tx.send(AgentEvent::AgentEnd {
            stop: novacode_types::StopReason::Stop,
            prompts: vec![],
        })
        .unwrap();
        tx.send(AgentEvent::Message(Message::user("never deploy on fridays")))
            .unwrap();
        tx.send(AgentEvent::AgentEnd {
            stop: novacode_types::StopReason::Stop,
            prompts: vec![],
        })
        .unwrap();
        for _ in 0..100 {
            if !wakes.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let w = wakes.0.lock().unwrap().clone();
        assert_eq!(w, [nudge("Never deploy on Fridays")]);
        assert_eq!(f.requests.lock().unwrap().len(), 1, "only the user turn is checked");
        // No helper: nothing runs.
        let (_tx, rx) = tokio::sync::broadcast::channel::<AgentEvent>(1);
        decision_check(rx, None, wakes, Default::default());
    }

    /// The state block ends with the Models section, made for the session's current model.
    #[test]
    fn the_state_block_ends_with_the_models_section_of_the_current_model() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut store = Store::create(&novacode_base::paths::Paths::new(home.path()), work.path(), false);
        store
            .append(novacode_core::session::EntryKind::ModelChange {
                provider: "codex".into(),
                model: "gpt-6.1-sol".into(),
            })
            .unwrap();
        let models =
            Arc::new(|last: Option<(String, String)>| format!("\n## Models\nYour provider: {}.\n", last.unwrap().0));
        let b = builder(
            home.path().join("super"),
            Arc::new(|| {}),
            models,
            Default::default(),
            Default::default(),
        );
        let block = (b.state)(&store);
        assert!(block.contains("## Board"), "{block}");
        assert!(block.ends_with("## Models\nYour provider: codex.\n"), "{block}");
    }

    #[test]
    fn history_search_finds_old_chat_newest_first_and_capped() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut store = Store::create(&novacode_base::paths::Paths::new(home.path()), work.path(), false);
        for i in 0..40 {
            store
                .append(novacode_core::session::EntryKind::Message {
                    message: Message::user(format!("deploy window note {i} {}", "z".repeat(100))),
                })
                .unwrap();
        }
        store
            .append(novacode_core::session::EntryKind::Message {
                message: Message::user("unrelated"),
            })
            .unwrap();
        store.flush().unwrap();
        let file = store.path().unwrap().to_path_buf();
        let out = search_history(&file, "deploy window");
        assert!(out.len() <= HISTORY_CHARS + 100, "{}", out.len());
        assert!(out.find("note 39").unwrap() < out.find("note 38").unwrap(), "{out}");
        assert!(
            out.contains("of 40 matches shown") && !out.contains("unrelated"),
            "{out}"
        );
        assert!(search_history(&file, "nothing-like-this").starts_with("nothing in your past"));
    }
}
