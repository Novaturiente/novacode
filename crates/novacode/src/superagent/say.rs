//! Super's `say` tool (L10.9 "Structured messages to the user"): every message to the
//! user is a typed block. The model writes kind, status, body and choices; code checks
//! them and adds the frame (time, trigger, a board status line per task), saved in the
//! result's `details.say` so every window draws the same block, now and on reattach.

use super::board::Board;
use super::task::Super;
use novacode_core::session::{Entry, EntryKind};
use novacode_tools::ask::Asker;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::{Block, Choice, Message, Question, Say};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const KINDS: [&str; 6] = ["update", "question", "warning", "done", "note", "reply"];

/// What the code's plain-text nudge starts with (one per run, then a note block).
pub const NUDGE: &str = "[say] Your last reply was plain text, which the user doesn't get as a message. Send it \
again with `say` (pick the kind; name the tasks; for a question or warning give the status). If you meant \
nothing for the user, reply with nothing.";

/// The trigger of super's current run, set by the context builder at each model call.
pub type Trigger = Arc<Mutex<String>>;

/// What started the run the newest entry belongs to: `reply to you` for a user message,
/// `health check`, or `after <the wake's first words>`. A run is the entries after the
/// last assistant reply without tool calls; what started it is what came before its first
/// reply: a user message first, else the last wake (notes queued earlier ride along with
/// it). Runs started only by code's nudges belong to the run before them.
pub fn trigger(entries: &[&Entry]) -> String {
    let ends_run = |e: &Entry| matches!(&e.kind, EntryKind::Message { message: Message::Assistant(a) } if a.tool_calls().next().is_none());
    let starter = |e: &Entry| match &e.kind {
        EntryKind::Message {
            message: Message::User { content },
        } => {
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            (!text.starts_with("<system-reminder>")).then(|| "reply to you".to_string())
        }
        EntryKind::CustomMessage { kind, text, .. } if kind == "super" => Some(describe(text)),
        EntryKind::CustomMessage { kind, .. } if !matches!(kind.as_str(), "nudge" | "todo" | "hook") => {
            Some(format!("after a {kind} note"))
        }
        _ => None,
    };
    let mut end = entries.len();
    loop {
        let start = entries[..end].iter().rposition(|e| ends_run(e)).map_or(0, |i| i + 1);
        let head: Vec<String> = entries[start..end]
            .iter()
            .take_while(|e| {
                !matches!(
                    &e.kind,
                    EntryKind::Message {
                        message: Message::Assistant(_)
                    }
                )
            })
            .filter_map(|e| starter(e))
            .collect();
        if let Some(t) = head.iter().find(|t| *t == "reply to you").or(head.last()) {
            return t.clone();
        }
        if start == 0 {
            return "your own run".into();
        }
        end = start - 1;
    }
}

/// A wake's text as a trigger.
fn describe(text: &str) -> String {
    if text.starts_with(super::link::HEALTH_CHECK) {
        return "health check".into();
    }
    if text.starts_with("Decision check") {
        return "decision check".into();
    }
    let first = text.lines().next().unwrap_or("").trim();
    let cut = [" (", ": ", ". "]
        .iter()
        .filter_map(|p| first.find(p))
        .min()
        .unwrap_or(first.len());
    let mut s: String = first[..cut].trim_end_matches(['.', ':']).chars().take(60).collect();
    if first[..cut].chars().count() > 60 {
        s.push('…');
    }
    format!("after {s}")
}

/// Local `HH:MM` now.
// shortcut: asks `date` for the local zone (no tz crate), as `schedule` does; UTC when it fails.
fn local_time(ms: u64) -> String {
    let out = std::process::Command::new("date")
        .args(["-d", &format!("@{}", ms / 1000), "+%H:%M"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => novacode_core::memory::utc_minute(ms)
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

#[derive(Deserialize)]
struct Args {
    kind: String,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    tasks: Vec<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    choices: Vec<ArgChoice>,
    #[serde(default)]
    exact_answer: bool,
}

#[derive(Deserialize)]
struct ArgChoice {
    label: String,
    #[serde(default)]
    description: Option<String>,
}

/// The checks code makes on a `say` (each refusal goes back to the model).
fn check(a: &Args, board: &Board) -> Result<(), String> {
    if !KINDS.contains(&a.kind.as_str()) {
        return Err(format!("`kind` must be one of {}", KINDS.join(", ")));
    }
    if let Some(p) = &a.priority
        && !matches!(p.as_str(), "normal" | "high")
    {
        return Err("`priority` must be normal or high".into());
    }
    if matches!(a.kind.as_str(), "question" | "warning") && a.status.trim().is_empty() {
        return Err(format!(
            "a {} needs `status`: what is true now and what led here",
            a.kind
        ));
    }
    let unknown: Vec<&str> = a
        .tasks
        .iter()
        .filter(|l| board.by_label(l).is_none())
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = board.tasks.iter().map(|t| t.label.as_str()).collect();
        return Err(format!(
            "unknown task label(s): {}; the board has: {}",
            unknown.join(", "),
            if known.is_empty() {
                "(no tasks)".into()
            } else {
                known.join(", ")
            }
        ));
    }
    if a.kind == "question" && a.choices.is_empty() && !a.exact_answer {
        return Err(
            "a question needs `choices`, or `exact_answer: true` when the body asks for one exact answer (a name, a number, a path)"
                .into(),
        );
    }
    if a.body.trim().is_empty() && a.status.trim().is_empty() {
        return Err("`body` is empty".into());
    }
    Ok(())
}

/// `say`: super's only channel to the user.
pub struct SayTool {
    pub sup: Arc<Super>,
    pub asker: Option<Arc<dyn Asker>>,
    pub trigger: Trigger,
}

impl SayTool {
    /// The block for `a`, framed by code from the board now.
    fn frame(&self, a: &Args, at_ms: u64) -> Say {
        let board = self.sup.board.lock().unwrap();
        frame(a, &board, at_ms, &self.trigger.lock().unwrap())
    }
}

fn frame(a: &Args, board: &Board, at_ms: u64, trigger: &str) -> Say {
    Say {
        kind: a.kind.clone(),
        priority: a.priority.clone().unwrap_or_else(|| "normal".into()),
        at_ms,
        time: local_time(at_ms),
        trigger: trigger.to_string(),
        tasks: a.tasks.clone(),
        lines: a.tasks.iter().filter_map(|l| board.status_line(l)).collect(),
        status: a.status.trim().to_string(),
        body: a.body.trim().to_string(),
        choices: a
            .choices
            .iter()
            .map(|c| Choice {
                label: c.label.clone(),
                description: c.description.clone(),
                preview: None,
                recommended: false,
            })
            .collect(),
        answer: None,
    }
}

/// The question's card: the task labels on its label; status lines, status and body as its text.
pub fn card(s: &Say) -> Question {
    let mut text = String::new();
    for l in &s.lines {
        text.push_str(&format!("{l}\n"));
    }
    if !s.status.is_empty() {
        text.push_str(&format!("Where things stand: {}\n", s.status));
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&s.body);
    let who = match s.tasks.is_empty() {
        true => "super".to_string(),
        false => s.tasks.join(", "),
    };
    Question {
        question: text,
        header: Some(format!("{who}: question")),
        options: s.choices.clone(),
        multi: false,
    }
}

impl Tool for SayTool {
    fn name(&self) -> &str {
        "say"
    }
    fn description(&self) -> &str {
        "Send the user a message: your only channel to them. Code shows it as a block with its kind, the time, what \
started this run and a board status line for each task you name. A question with `choices` opens a card and \
returns the user's answer."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": KINDS, "description": "update (task news), question (you need the user's answer), warning (something is wrong), done (work finished), note (an aside), reply (answer to the user's message)." },
                "priority": { "type": "string", "enum": ["normal", "high"], "description": "high only when the user should look now. Default normal." },
                "tasks": { "type": "array", "items": { "type": "string" }, "description": "Labels of the tasks it is about (code adds each one's board status). May be empty." },
                "status": { "type": "string", "description": "Where things stand now and what led here. Required for question and warning." },
                "body": { "type": "string", "description": "Markdown: what happened, then what you did or need." },
                "choices": { "type": "array", "items": { "type": "object", "properties": { "label": { "type": "string" }, "description": { "type": "string" } }, "required": ["label"] }, "description": "A question's answers (2-6); the user can also type their own." },
                "exact_answer": { "type": "boolean", "description": "A question without choices: the body asks for one exact answer (a name, number or path) the user types." }
            },
            "required": ["kind", "body"]
        })
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match serde_json::from_value(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("bad arguments: {e}")),
            };
            if let Err(e) = check(&a, &self.sup.board.lock().unwrap()) {
                return ToolOutput::error(e);
            }
            let id = format!("say-{}", novacode_core::session::now_ms());
            let mut say = self.frame(&a, novacode_core::session::now_ms());
            let typed = a.kind == "question" && say.choices.is_empty();
            let note = |say: &Say| {
                if let Err(e) = self
                    .sup
                    .edit(|b| b.note_say(&id, &say.kind, &say.tasks, &say.status, typed))
                {
                    tracing::warn!(%e, "super: Needs you not saved");
                }
            };
            note(&say);
            let mut text = "Shown to the user.".to_string();
            if a.kind == "question" && !say.choices.is_empty() {
                // While the card is open, the window draws the framed block from this update.
                ctx.update(serde_json::to_string(&json!({ "say": say })).unwrap_or_default());
                match self.asker.as_ref() {
                    None => text = "Shown to the user; no window can show its card, so ask again later.".into(),
                    Some(asker) => match asker.ask(vec![card(&say)]).await {
                        Some(r) => {
                            let answer = novacode_tools::ask::render(&r);
                            say.answer = Some(
                                answer
                                    .lines()
                                    .filter_map(|l| l.strip_prefix("A: "))
                                    .collect::<Vec<_>>()
                                    .join("; "),
                            );
                            let _ = self.sup.edit(|b| b.drop_need(&id));
                            text = format!("The user answered.\n{answer}");
                        }
                        None => {
                            text = "The user closed the card without answering; it stays on their Needs you list. \
Don't ask again at once."
                                .into()
                        }
                    },
                }
            }
            ToolOutput {
                details: Some(json!({ "say": say, "need": id })),
                ..ToolOutput::text(text)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::board::{State, Task};
    use novacode_types::Reply;

    fn board() -> Board {
        Board {
            tasks: vec![Task {
                id: "T2".into(),
                label: "T2-api".into(),
                state: State::Blocked,
                summary: Some("needs the db url".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn args(v: Value) -> Args {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn each_refusal_goes_back_to_the_model() {
        let b = board();
        let no = |v: Value| check(&args(v), &b).unwrap_err();
        assert!(no(json!({"kind": "shout", "body": "x"})).contains("`kind` must be one of"));
        assert!(no(json!({"kind": "note", "priority": "urgent", "body": "x"})).contains("normal or high"));
        assert!(no(json!({"kind": "question", "body": "x", "exact_answer": true})).contains("needs `status`"));
        assert!(no(json!({"kind": "warning", "body": "x"})).contains("a warning needs `status`"));
        let e = no(json!({"kind": "update", "tasks": ["T9"], "body": "x"}));
        assert!(e.contains("unknown task label(s): T9") && e.contains("T2-api"), "{e}");
        assert!(no(json!({"kind": "question", "status": "s", "body": "Port?"})).contains("`exact_answer: true`"));
        assert!(no(json!({"kind": "note", "body": " "})).contains("`body` is empty"));
        // Accepted: a question with choices, or one exact answer.
        for ok in [
            json!({"kind": "question", "status": "s", "body": "Which?", "choices": [{"label": "A"}]}),
            json!({"kind": "question", "status": "s", "body": "Port?", "exact_answer": true}),
            json!({"kind": "update", "tasks": ["T2-api"], "body": "x"}),
        ] {
            assert_eq!(check(&args(ok.clone()), &b), Ok(()), "{ok}");
        }
    }

    #[test]
    fn the_frame_carries_board_status_lines_time_and_trigger() {
        let a = args(
            json!({"kind": "warning", "priority": "high", "tasks": ["T2-api"], "status": "blocked", "body": "**x**"}),
        );
        let s = frame(&a, &board(), 1_700_000_000_000, "after T2-api reported blocked");
        assert_eq!(s.lines, ["T2-api · blocked · needs the db url · no report yet"]);
        assert_eq!((s.kind.as_str(), s.priority.as_str()), ("warning", "high"));
        assert_eq!(s.trigger, "after T2-api reported blocked");
        assert!(s.time.len() == 5 && s.time.as_bytes()[2] == b':', "{}", s.time);
        let q = card(&Say {
            choices: vec![Choice {
                label: "Wait".into(),
                description: None,
                preview: None,
                recommended: false,
            }],
            ..s
        });
        assert_eq!(q.header.as_deref(), Some("T2-api: question"));
        assert!(q.question.starts_with("T2-api · blocked") && q.question.contains("Where things stand: blocked"));
    }

    fn entry(kind: EntryKind) -> Entry {
        Entry {
            id: String::new(),
            parent: None,
            ts_ms: 0,
            kind,
        }
    }

    fn assistant(calls: bool) -> Entry {
        let mut content = vec![Block::text("t")];
        if calls {
            content.push(Block::ToolCall(novacode_types::ToolCall {
                id: "c".into(),
                name: "say".into(),
                arguments: json!({}),
                arguments_error: None,
                replay: None,
            }));
        }
        entry(EntryKind::Message {
            message: Message::Assistant(novacode_types::AssistantMessage {
                content,
                stop: novacode_types::StopReason::Stop,
                usage: Default::default(),
                provider: "f".into(),
                model: "m".into(),
                response_id: None,
                error_message: None,
                thinking_level: None,
            }),
        })
    }

    fn custom(kind: &str, text: &str) -> Entry {
        entry(EntryKind::CustomMessage {
            kind: kind.into(),
            text: text.into(),
            display: false,
        })
    }

    #[test]
    fn the_trigger_comes_from_the_run_s_first_message() {
        let user = entry(EntryKind::Message {
            message: Message::user("how is T2?"),
        });
        let wake = custom("super", "T2-api reported blocked (now blocked): needs the db url");
        let check = custom("super", &super::super::link::health_check_text());
        let t = |es: Vec<Entry>| trigger(&es.iter().collect::<Vec<_>>());
        assert_eq!(t(vec![assistant(false), user, assistant(true)]), "reply to you");
        assert_eq!(
            t(vec![assistant(false), wake.clone(), assistant(true)]),
            "after T2-api reported blocked"
        );
        assert_eq!(t(vec![check, assistant(true)]), "health check");
        // A note queued before the wake rides along: the wake started the run.
        assert_eq!(
            t(vec![
                assistant(false),
                custom("super", "T1 asked a question; you answered"),
                wake.clone(),
                assistant(true)
            ]),
            "after T2-api reported blocked"
        );
        // A nudge after a plain reply belongs to the run before it.
        assert_eq!(
            t(vec![
                wake,
                custom("todo", "stale"),
                assistant(false),
                custom("nudge", NUDGE),
                assistant(true)
            ]),
            "after T2-api reported blocked"
        );
        assert_eq!(describe("T1 merged (now ready). If the user"), "after T1 merged");
    }

    struct Answer(Mutex<Vec<Question>>, Option<&'static str>);
    impl Asker for Answer {
        fn ask(&self, q: Vec<Question>) -> futures_util::future::BoxFuture<'_, Option<Vec<Reply>>> {
            self.0.lock().unwrap().extend(q.clone());
            Box::pin(async move {
                self.1.map(|a| {
                    vec![Reply {
                        question: q[0].question.clone(),
                        chosen: vec![a.into()],
                        text: None,
                        preview: None,
                    }]
                })
            })
        }
    }

    fn ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            env: Arc::new(novacode_tools::LocalEnv),
            cancel: Default::default(),
            updates: None,
            files: Default::default(),
            session_id: "s".into(),
            processes: None,
            native_tools: false,
            nested: None,
            script: false,
        }
    }

    async fn tool(answer: Option<&'static str>) -> (SayTool, Arc<Answer>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().canonicalize().unwrap();
        let (root, home) = (base.join("root"), base.join("home"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let asker = Arc::new(Answer(Mutex::default(), answer));
        let sup = Arc::new(
            Super::new(
                &root,
                home.join("super/x"),
                crate::superagent::task::tests::test_cfg(&home, 5),
                Some(asker.clone()),
                Arc::new(crate::superagent::task::tests::FakeLauncher::default()),
            )
            .unwrap(),
        );
        sup.edit(|b| b.tasks = board().tasks).unwrap();
        let t = SayTool {
            sup,
            asker: Some(asker.clone()),
            trigger: Arc::new(Mutex::new("reply to you".into())),
        };
        (t, asker, d)
    }

    #[tokio::test]
    async fn a_question_with_choices_opens_a_card_and_returns_the_answer() {
        let (t, asker, d) = tool(Some("Wait")).await;
        let ctx = ctx(d.path());
        let o = t
            .execute(
                json!({"kind": "question", "tasks": ["T2-api"], "status": "T2-api is blocked", "body": "Wait or stop?",
                       "choices": [{"label": "Wait"}, {"label": "Stop"}]}),
                &ctx,
            )
            .await;
        assert!(!o.is_error);
        let q = asker.0.lock().unwrap()[0].clone();
        assert_eq!(q.header.as_deref(), Some("T2-api: question"));
        assert_eq!(q.options.len(), 2);
        let say: Say = serde_json::from_value(o.details.unwrap()["say"].clone()).unwrap();
        assert_eq!(say.answer.as_deref(), Some("Wait"));
        assert_eq!(say.trigger, "reply to you");
        // Answered: off Needs you.
        assert!(t.sup.board.lock().unwrap().needs.is_empty());
        // A warning stays there until an update on its task.
        t.execute(
            json!({"kind": "warning", "tasks": ["T2-api"], "status": "s", "body": "b"}),
            &ctx,
        )
        .await;
        assert_eq!(t.sup.board.lock().unwrap().needs.len(), 1);
        t.execute(json!({"kind": "update", "tasks": ["T2-api"], "body": "fixed"}), &ctx)
            .await;
        assert!(Board::load(&t.sup.dir).unwrap().needs.is_empty());
        // A refusal is an error result.
        let o = t.execute(json!({"kind": "warning", "body": "b"}), &ctx).await;
        assert!(o.is_error);
    }

    #[tokio::test]
    async fn a_dismissed_card_stays_on_needs_you() {
        let (t, _, d) = tool(None).await;
        let ctx = ctx(d.path());
        let o = t
            .execute(
                json!({"kind": "question", "status": "s", "body": "Which?", "choices": [{"label": "A"}]}),
                &ctx,
            )
            .await;
        let text = match &o.content[0] {
            Block::Text { text, .. } => text.clone(),
            _ => String::new(),
        };
        assert!(text.contains("closed the card"), "{text}");
        assert_eq!(t.sup.board.lock().unwrap().needs.len(), 1);
    }
}
