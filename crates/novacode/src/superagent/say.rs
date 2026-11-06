//! Super's `say` tool (L10.9 "Structured messages to the user"): every message to the
//! user is a typed block. The model writes kind, task sections and choices; code checks
//! them and adds the frame (time, trigger, a board status line per task), saved in the
//! result's `details.say` so every window draws the same block, now and on reattach.

use super::board::Board;
use super::task::Super;
use novacode_core::session::{Entry, EntryKind};
use novacode_tools::ask::Asker;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use novacode_types::{Block, Choice, Message, Question, Say, SayQuestion, SaySection};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const KINDS: [&str; 6] = ["update", "question", "warning", "done", "note", "reply"];

/// What the code's plain-text nudge starts with (one per run, then a note block).
pub const NUDGE: &str = "[say] Your last reply was plain text, which the user doesn't get as a message. Send it \
again with `say` (pick the kind; use sections with task labels or General; questions need structured question context, warnings need status). If you meant \
nothing for the user, reply with nothing.";

/// The trigger of super's current run, set by the context builder at each model call.
pub type Trigger = Arc<Mutex<String>>;

/// What the newest entry answers: `reply to you` for a user message, `after <the wake's
/// first words>` for a task event, or `health check`. It is the newest batch of messages
/// between two assistant replies that holds such an event; within a batch (queued wakes)
/// a user message wins, then a task event, then a health check. Code's nudges (decision
/// checks, the plain-text `[say]` nudge, todo and hook notes) are never triggers: a run
/// they start belongs to the event before them.
pub fn trigger(entries: &[&Entry]) -> String {
    let is_reply = |e: &&&Entry| {
        matches!(
            &e.kind,
            EntryKind::Message {
                message: Message::Assistant(_)
            }
        )
    };
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
        EntryKind::CustomMessage { kind, text, .. } if kind == "super" => describe(text),
        EntryKind::CustomMessage { kind, .. } if !matches!(kind.as_str(), "nudge" | "todo" | "hook") => {
            Some(format!("after a {kind} note"))
        }
        _ => None,
    };
    let rank = |t: &String| match t.as_str() {
        "reply to you" => 2,
        "health check" => 0,
        _ => 1,
    };
    entries
        .split(|e| is_reply(&e))
        .rev()
        .find_map(|batch| {
            // `max_by_key` keeps the last of equals: the newest task event.
            batch.iter().filter_map(|e| starter(e)).max_by_key(rank)
        })
        .unwrap_or_else(|| "your own run".into())
}

/// A wake's text as a trigger; `None` for code's decision-check nudge.
fn describe(text: &str) -> Option<String> {
    if text.starts_with(super::link::HEALTH_CHECK) {
        return Some("health check".into());
    }
    if text.starts_with(super::context::DECISION_CHECK) {
        return None;
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
    Some(format!("after {s}"))
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
    sections: Vec<ArgSection>,
    #[serde(default)]
    choices: Vec<ArgChoice>,
    #[serde(default)]
    exact_answer: bool,
}

#[derive(Deserialize)]
struct ArgSection {
    task: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    question: Option<SayQuestion>,
}

#[derive(Deserialize)]
struct ArgChoice {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    action: Option<String>,
}

/// A question's free-text fields and each blocker field: at most this many words.
const FIELD_WORDS: usize = 40;
/// A question section: at most this many blockers.
const MAX_BLOCKERS: usize = 5;

/// `Err("<field>: N words, at most <max>")` when `text` is longer than `max` words.
fn words(field: &str, text: &str, max: usize) -> Result<(), String> {
    match text.split_whitespace().count() {
        n if n > max => Err(format!("{field}: {n} words, at most {max}; write short points")),
        _ => Ok(()),
    }
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
    if a.sections.is_empty() {
        return Err("`sections` must not be empty".into());
    }
    if a.kind == "warning" && a.sections.iter().any(|s| s.status.trim().is_empty()) {
        return Err(format!(
            "a {} needs `status`: what is true now and what led here",
            a.kind
        ));
    }
    let unknown: Vec<&str> = a
        .sections
        .iter()
        .map(|s| s.task.as_str())
        .filter(|l| *l != "General" && board.by_label(l).is_none())
        .collect();
    if !unknown.is_empty() {
        let known: Vec<String> = board.tasks.iter().map(|t| board.name(t)).collect();
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
    if a.kind != "question" && !a.choices.is_empty() {
        return Err(format!(
            "only a question has `choices`; send this {} without them, then the question as its own `say` with kind question",
            a.kind
        ));
    }
    if a.kind == "question" && a.choices.is_empty() && !a.exact_answer {
        return Err(
            "a question needs `choices`, or `exact_answer: true` when its decision asks for one exact answer (a name, a number, a path)"
                .into(),
        );
    }
    let mut labels = std::collections::HashSet::new();
    if a.sections.iter().any(|s| !labels.insert(&s.task)) {
        return Err("each task (or General) must have only one section".into());
    }
    if a.kind == "question" {
        for s in &a.sections {
            if !s.status.trim().is_empty() || !s.body.trim().is_empty() {
                return Err(format!(
                    "{}: question sections use structured question context, not status/body",
                    s.task
                ));
            }
            let q = s
                .question
                .as_ref()
                .ok_or_else(|| format!("{} needs structured `question` context", s.task))?;
            for (field, value) in [
                ("requested", &q.requested),
                ("verified_result", &q.verified_result),
                ("input_needed", &q.input_needed),
                ("recommendation", &q.recommendation),
                ("risk", &q.risk),
                ("unchanged", &q.unchanged),
                ("decision", &q.decision),
            ] {
                if value.trim().is_empty() {
                    return Err(format!(
                        "{}: question.{field} must not be empty; explain unknown or not applicable truthfully",
                        s.task
                    ));
                }
                words(&format!("{}: question.{field}", s.task), value, FIELD_WORDS)?;
            }
            if q.blockers.len() > MAX_BLOCKERS {
                return Err(format!(
                    "{}: question.blockers: {} blockers, at most {MAX_BLOCKERS}",
                    s.task,
                    q.blockers.len()
                ));
            }
            if q.blockers.is_empty() {
                return Err(format!(
                    "{}: question.blockers needs at least one entry; explain if no technical blocker applies",
                    s.task
                ));
            }
            for (i, b) in q.blockers.iter().enumerate() {
                for (field, value) in [("issue", &b.issue), ("evidence", &b.evidence), ("impact", &b.impact)] {
                    if value.trim().is_empty() {
                        return Err(format!(
                            "{}: question.blockers[{i}].{field} must not be empty; ask the worker for facts or state what is unknown",
                            s.task
                        ));
                    }
                    words(
                        &format!("{}: question.blockers[{i}].{field}", s.task),
                        value,
                        FIELD_WORDS,
                    )?;
                }
            }
        }
        if a.choices
            .iter()
            .any(|c| !matches!(c.action.as_deref(), Some("answer" | "plan" | "execute")))
        {
            return Err(
                "each question choice needs `action`: answer, plan (prepare only), or execute (make changes)".into(),
            );
        }
    } else {
        if a.sections.iter().any(|s| s.question.is_some()) {
            return Err("only a question has structured `question` context; other kinds use status/body".into());
        }
        if a.sections
            .iter()
            .any(|s| s.body.trim().is_empty() && s.status.trim().is_empty())
        {
            return Err("`body` is empty".into());
        }
    }
    Ok(())
}

/// Super's `ask_user`: the regular tool, refusing a question text over [`FIELD_WORDS`]
/// words or an option description over 25. Other sessions keep the plain [`AskUser`].
///
/// [`AskUser`]: novacode_tools::ask::AskUser
pub struct ShortAskUser(pub novacode_tools::ask::AskUser);

const OPTION_WORDS: usize = 25;

impl Tool for ShortAskUser {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        "Ask the user one or more structured questions when a decision is theirs or the request is ambiguous: each with 2–6 short choices (mark one recommended), an optional description and preview per choice, and `multi` for several picks. The user can always type their own answer. Group related questions into one call. Short points: each question text at most 40 words, each option description at most 25 words; longer is refused."
    }
    fn schema(&self) -> Value {
        self.0.schema()
    }
    fn sequential(&self) -> bool {
        self.0.sequential()
    }
    fn large_output(&self) -> bool {
        self.0.large_output()
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        // Bad arguments go on to the inner tool, which reports them.
        let qs: Vec<Question> = serde_json::from_value(args["questions"].clone()).unwrap_or_default();
        for (i, q) in qs.iter().enumerate() {
            let n = i + 1;
            let short = words(&format!("question {n}"), &q.question, FIELD_WORDS).and_then(|_| {
                q.options.iter().try_for_each(|o| {
                    words(
                        &format!("question {n} option {} description", o.label),
                        o.description.as_deref().unwrap_or(""),
                        OPTION_WORDS,
                    )
                })
            });
            if let Err(e) = short {
                return Box::pin(async move { ToolOutput::error(e) });
            }
        }
        self.0.execute(args, ctx)
    }
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
        sections: a
            .sections
            .iter()
            .map(|s| SaySection {
                task: s.task.clone(),
                line: board.status_line(&s.task).unwrap_or_default(),
                status: s.status.trim().to_string(),
                body: s.body.trim().to_string(),
                question: s.question.clone(),
            })
            .collect(),
        tasks: a
            .sections
            .iter()
            .filter(|s| s.task != "General")
            .map(|s| s.task.clone())
            .collect(),
        lines: vec![],
        status: String::new(),
        body: String::new(),
        choices: a
            .choices
            .iter()
            .map(|c| Choice {
                label: match c.action.as_deref() {
                    Some("plan") => format!("Prepare plan only: {}", c.label),
                    Some("execute") => format!("Execute changes: {}", c.label),
                    _ => c.label.clone(),
                },
                description: c.description.clone(),
                preview: None,
                recommended: false,
            })
            .collect(),
        answer: None,
    }
}

/// The question's card: task subtitles, board lines and the same context headings as the transcript.
/// Older saved blocks keep their status/body layout.
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
    for section in &s.sections {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&section.task);
        text.push('\n');
        if !section.line.is_empty() {
            text.push_str(&format!("{}\n", section.line));
        }
        if let Some(q) = &section.question {
            for (heading, content) in q.paragraphs() {
                text.push_str(&format!("\n{heading}:\n- {}\n", content.trim()));
            }
        } else {
            if !section.status.is_empty() {
                text.push_str(&format!("Where things stand: {}\n", section.status));
            }
            text.push('\n');
            text.push_str(&section.body);
        }
    }
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
returns the user's answer. A question is short points: each question field and each blocker's issue, evidence and \
impact at most 40 words, at most 5 blockers; longer is refused."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": KINDS, "description": "update (task news), question (you need the user's answer), warning (something is wrong), done (work finished), note (an aside), reply (answer to the user's message)." },
                "priority": { "type": "string", "enum": ["normal", "high"], "description": "high only when the user should look now. Default normal." },
                "sections": { "type": "array", "minItems": 1, "description": "One subsection per task, with only its facts. Use General for unrelated content. Do not repeat labels.", "items": { "type": "object", "properties": {
                    "task": { "type": "string", "description": "Exact board task label, or General." },
                    "status": { "type": "string", "description": "For non-question kinds: where this task stands and what led here. Required for warning." },
                    "body": { "type": "string", "description": "For non-question kinds: Markdown facts for this task only, then a concrete next action. Questions use question instead." },
                    "question": { "type": "object", "description": "Required only for question, in every section. Ask the worker for missing facts first. State unknowns explicitly, never invent evidence or impact. For General, truthful not-applicable explanations are valid. Each text field is one short point of at most 40 words, shown as a bullet under its heading.", "properties": {
                        "requested": { "type": "string", "minLength": 1, "description": "What the user requested, in plain language." },
                        "verified_result": { "type": "string", "minLength": 1, "description": "What is verified now, and what behaviour remains unproven. Do not treat deployment as proof that behaviour works." },
                        "blockers": { "type": "array", "minItems": 1, "maxItems": 5, "description": "Separate issues, one entry each (at most 5; issue, evidence and impact at most 40 words each). If none applies, explain why in an entry; do not invent a technical blocker.", "items": { "type": "object", "properties": {
                            "issue": { "type": "string", "minLength": 1, "description": "The exact blocker, or why blockers are not applicable." },
                            "evidence": { "type": "string", "minLength": 1, "description": "Exact failed command/error or observed result; explicitly say what is unknown if unavailable." },
                            "impact": { "type": "string", "minLength": 1, "description": "Effect on requested behaviour or users; explicitly unknown if not established." }
                        }, "required": ["issue", "evidence", "impact"] } },
                        "input_needed": { "type": "string", "minLength": 1, "description": "Why this needs the user's decision rather than more worker fact-finding." },
                        "recommendation": { "type": "string", "minLength": 1, "description": "Concrete next action; distinguish preparing a plan from executing changes." },
                        "risk": { "type": "string", "minLength": 1, "description": "Risk of the recommendation, or explain what is unknown/not applicable." },
                        "unchanged": { "type": "string", "minLength": 1, "description": "What stays unchanged if the user accepts, including production and scope boundaries." },
                        "decision": { "type": "string", "minLength": 1, "description": "The precise decision or exact answer needed. Permission to plan is not permission to execute." }
                    }, "required": ["requested", "verified_result", "blockers", "input_needed", "recommendation", "risk", "unchanged", "decision"] }
                }, "required": ["task"] } },
                "choices": { "type": "array", "items": { "type": "object", "properties": {
                    "label": { "type": "string" }, "description": { "type": "string" },
                    "action": { "type": "string", "enum": ["answer", "plan", "execute"], "description": "answer: information/preference/no changes; plan: prepare a plan only; execute: make the specified changes. Code labels plan/execute explicitly. Never label permission to execute as answer." }
                }, "required": ["label", "action"] }, "description": "Only for kind question: its answers (2-6); the user can also type their own. Distinguish preparing a plan from executing changes." },
                "exact_answer": { "type": "boolean", "description": "A question without choices: question.decision asks for one exact answer (a name, number or path) the user types." }
            },
            "required": ["kind", "sections"]
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
                // A card's question is kept with its row: a restarted super opens it again.
                let kept = (a.kind == "question" && !typed).then(|| card(say));
                if let Err(e) = self.sup.edit(|b| {
                    let status = say
                        .sections
                        .iter()
                        .filter_map(|s| {
                            let status = s
                                .question
                                .as_ref()
                                .map_or(s.status.as_str(), |q| q.verified_result.as_str());
                            (!status.trim().is_empty()).then(|| format!("{}: {}", s.task, status.trim()))
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    b.note_say(&id, &say.kind, &say.tasks, &status, typed);
                    if let Some(n) = b.needs.iter_mut().find(|n| n.id == id) {
                        n.card = kept;
                    }
                }) {
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

    fn question_section(task: &str) -> Value {
        json!({"task": task, "question": {
            "requested": "Prove that live input is transformed correctly.",
            "verified_result": "Deployment succeeded; the live transformation is not yet verified.",
            "blockers": [
                {"issue": "Live transformation has no proof", "evidence": "Unknown: worker has not identified a safe live test.", "impact": "The requested live behaviour is still unproven."},
                {"issue": "Workspace gate failed", "evidence": "scripts/check.sh exited 101: test scoring::round_trip FAILED", "impact": "Unknown: worker must establish whether users are affected."}
            ],
            "input_needed": "Investigation outside the scoring files exceeds the approved scope.",
            "recommendation": "Prepare a bounded investigation plan only; do not execute changes.",
            "risk": "Completion remains blocked while the plan is prepared.",
            "unchanged": "Production and the current deployment remain unchanged.",
            "decision": "May I prepare the investigation plan without changing production?"
        }})
    }

    fn question_args(task: &str) -> Value {
        json!({"kind": "question", "sections": [question_section(task)], "choices": [
            {"label": "Investigate", "action": "plan"}, {"label": "Keep blocked", "action": "answer"}
        ]})
    }

    #[test]
    fn question_context_requires_every_fact_or_explicit_unknown() {
        let b = board();
        let valid = question_args("T2-api");
        assert_eq!(check(&args(valid.clone()), &b), Ok(()));
        for field in [
            "requested",
            "verified_result",
            "input_needed",
            "recommendation",
            "risk",
            "unchanged",
            "decision",
        ] {
            for missing in [false, true] {
                let mut v = valid.clone();
                let q = v["sections"][0]["question"].as_object_mut().unwrap();
                if missing {
                    q.remove(field);
                } else {
                    q.insert(field.into(), json!("  \n "));
                }
                let e = check(&args(v), &b).unwrap_err();
                assert!(e.contains(field), "{field}: {e}");
            }
        }
        for field in ["issue", "evidence", "impact"] {
            let mut v = valid.clone();
            v["sections"][0]["question"]["blockers"][1]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(check(&args(v), &b).unwrap_err().contains(field));
        }
        let mut v = valid.clone();
        v["sections"][0]["question"]["blockers"] = json!([]);
        assert!(check(&args(v), &b).unwrap_err().contains("blockers"));
        let mut v = valid.clone();
        v["sections"][0].as_object_mut().unwrap().remove("question");
        assert!(check(&args(v), &b).unwrap_err().contains("question"));
        let mut v = valid.clone();
        v["kind"] = json!("note");
        v["choices"] = json!([]);
        assert!(check(&args(v), &b).unwrap_err().contains("only a question"));
        for field in ["status", "body"] {
            let mut v = valid.clone();
            v["sections"][0][field] = json!("Do not silently discard these facts");
            assert!(check(&args(v), &b).unwrap_err().contains("not status/body"));
        }
        for action in [json!(null), json!("approve")] {
            let mut v = valid.clone();
            v["choices"][0]["action"] = action;
            let e = check(&args(v), &b).unwrap_err();
            assert!(e.contains("action"), "{e}");
        }
    }

    #[test]
    fn question_fields_are_short_points() {
        let b = board();
        let words = |n: usize| vec!["word"; n].join(" ");
        let mut v = question_args("T2-api");
        v["sections"][0]["question"]["recommendation"] = json!(words(40));
        v["sections"][0]["question"]["blockers"][1]["evidence"] = json!(words(40));
        assert_eq!(check(&args(v.clone()), &b), Ok(()));
        for field in [
            "requested",
            "verified_result",
            "input_needed",
            "recommendation",
            "risk",
            "unchanged",
            "decision",
        ] {
            let mut v = v.clone();
            v["sections"][0]["question"][field] = json!(words(63));
            let e = check(&args(v), &b).unwrap_err();
            assert!(e.contains(&format!("{field}: 63 words, at most 40")), "{e}");
        }
        for field in ["issue", "evidence", "impact"] {
            let mut v = v.clone();
            v["sections"][0]["question"]["blockers"][1][field] = json!(words(41));
            let e = check(&args(v), &b).unwrap_err();
            assert!(e.contains(&format!("blockers[1].{field}: 41 words, at most 40")), "{e}");
        }
        let blocker = v["sections"][0]["question"]["blockers"][0].clone();
        v["sections"][0]["question"]["blockers"] = json!(vec![blocker; 6]);
        let e = check(&args(v), &b).unwrap_err();
        assert!(e.contains("6 blockers, at most 5"), "{e}");
    }

    #[tokio::test]
    async fn super_s_ask_user_is_short_but_a_normal_session_s_is_not() {
        let d = tempfile::tempdir().unwrap();
        let ctx = ctx(d.path());
        let words = |n: usize| vec!["word"; n].join(" ");
        let input = |q: usize, desc: usize| {
            json!({"questions": [{"question": words(q), "options": [
                {"label": "A", "description": words(desc)}, {"label": "B"}
            ]}]})
        };
        let asker = || Arc::new(Answer(Mutex::default(), Some("A")));
        let normal = novacode_tools::ask::AskUser(asker());
        let short = ShortAskUser(novacode_tools::ask::AskUser(asker()));
        assert_eq!(short.name(), "ask_user");
        assert!(short.description().contains("at most 40 words"));
        assert!(!short.execute(input(40, 25), &ctx).await.is_error);
        let e = short.execute(input(41, 3), &ctx).await;
        assert!(
            e.is_error && text(&e).contains("question 1: 41 words, at most 40"),
            "{}",
            text(&e)
        );
        let e = short.execute(input(3, 26), &ctx).await;
        assert!(
            e.is_error && text(&e).contains("question 1 option A description: 26 words, at most 25"),
            "{}",
            text(&e)
        );
        assert!(!normal.execute(input(100, 100), &ctx).await.is_error);
    }

    fn text(o: &ToolOutput) -> String {
        match &o.content[0] {
            Block::Text { text, .. } => text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn question_cards_keep_each_tasks_evidence_and_plan_permission_separate() {
        let mut b = board();
        b.tasks.push(Task {
            id: "T1".into(),
            label: "T1-ui".into(),
            ..Default::default()
        });
        let mut v = question_args("T2-api");
        v["sections"]
            .as_array_mut()
            .unwrap()
            .insert(0, question_section("T1-ui"));
        v["sections"][0]["question"]["verified_result"] = json!("UI result verified by screenshot.");
        v["choices"]
            .as_array_mut()
            .unwrap()
            .push(json!({"label": "Approved patch", "action": "execute"}));
        let a = args(v);
        assert_eq!(check(&a, &b), Ok(()));
        let s = frame(&a, &b, 0, "reply to you");
        let q = card(&s);
        let first = q.question.find("T1-ui\n").unwrap();
        let second = q.question.find("T2-api\n").unwrap();
        assert!(q.question[first..second].contains("Verified result:\n- UI result verified by screenshot."));
        assert!(q.question[second..].contains("scripts/check.sh exited 101: test scoring::round_trip FAILED"));
        for heading in [
            "What you requested",
            "Blocker 1",
            "Blocker 2",
            "Evidence",
            "Impact",
            "Why your input is needed",
            "Recommendation",
            "Risk",
            "What stays unchanged",
            "Decision",
        ] {
            assert!(
                q.question.contains(&format!("{heading}:\n")),
                "{heading}: {}",
                q.question
            );
        }
        assert_eq!(q.options[0].label, "Prepare plan only: Investigate");
        assert_eq!(q.options[1].label, "Keep blocked");
        assert_eq!(q.options[2].label, "Execute changes: Approved patch");
        let replay: Say = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(card(&replay), q);
    }

    #[test]
    fn general_questions_need_no_fabricated_task_or_technical_blocker() {
        let mut v = question_args("General");
        v["choices"] = json!([]);
        v["exact_answer"] = json!(true);
        v["sections"][0]["question"] = json!({
            "requested": "Choose a project name.", "verified_result": "No name has been selected.",
            "blockers": [{"issue": "Not applicable: this is a naming preference, not a technical failure.", "evidence": "Your request leaves the name open.", "impact": "Only the name depends on your answer."}],
            "input_needed": "The name is your preference.", "recommendation": "Use Atlas.",
            "risk": "Unknown: trademark availability has not been checked.", "unchanged": "No files or deployments will change.",
            "decision": "What name should I record?"
        });
        let a = args(v);
        assert_eq!(check(&a, &Board::default()), Ok(()));
        let s = frame(&a, &Board::default(), 0, "reply to you");
        assert!(s.tasks.is_empty());
        let q = card(&s);
        assert!(q.question.starts_with("General\n"));
        assert!(q.question.contains("Not applicable: this is a naming preference"));
        assert!(q.question.contains("What name should I record?"));
    }

    #[test]
    fn legacy_question_cards_replay_without_structured_context() {
        for (saved, expected) in [
            (
                json!({"kind": "question", "tasks": ["T2-api"], "lines": ["T2-api · blocked"], "status": "Old status", "body": "Old decision?"}),
                "T2-api · blocked\nWhere things stand: Old status\n\nOld decision?",
            ),
            (
                json!({"kind": "question", "tasks": ["T2-api"], "sections": [{"task": "T2-api", "line": "T2-api · blocked", "status": "Old status", "body": "Old decision?"}]}),
                "T2-api\nT2-api · blocked\nWhere things stand: Old status\n\nOld decision?",
            ),
        ] {
            let s: Say = serde_json::from_value(saved).unwrap();
            assert_eq!(card(&s).question, expected);
        }
    }

    #[test]
    fn sections_keep_each_tasks_facts_and_general_separate() {
        let mut b = board();
        b.tasks.push(Task {
            id: "T1".into(),
            label: "T1-ui".into(),
            ..Default::default()
        });
        let a = args(json!({"kind": "update", "sections": [
            {"task": "T1-ui", "status": "UI ready", "body": "UI facts"},
            {"task": "T2-api", "status": "API blocked", "body": "API facts"},
            {"task": "General", "body": "Unrelated facts"}
        ]}));
        assert_eq!(check(&a, &b), Ok(()));
        let s = frame(&a, &b, 0, "reply to you");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["sections"][0]["task"], "T1-ui");
        assert!(v["sections"][0]["line"].as_str().unwrap().starts_with("T1-ui ·"));
        assert_eq!(v["sections"][1]["body"], "API facts");
        assert!(
            v["sections"][1]["line"]
                .as_str()
                .unwrap()
                .starts_with("T2-api · blocked")
        );
        assert_eq!(v["sections"][2]["line"], "");
        assert_eq!(v["sections"][2]["body"], "Unrelated facts");
        assert_eq!(s.tasks, ["T1-ui", "T2-api"]);
        let q = card(&s);
        assert!(q.question.contains("T1-ui\n") && q.question.contains("General\n\nUnrelated facts"));
    }

    #[test]
    fn each_refusal_goes_back_to_the_model() {
        let b = board();
        let no = |v: Value| check(&args(v), &b).unwrap_err();
        assert!(
            no(json!({"kind":"shout","sections":[{"task":"General","status":"","body":"x"}]}))
                .contains("`kind` must be one of")
        );
        assert!(
            no(json!({"kind":"note","priority":"urgent","sections":[{"task":"General","status":"","body":"x"}]}))
                .contains("normal or high")
        );
        assert!(
            no(json!({"kind":"question","exact_answer":true,"sections":[{"task":"General"}]}))
                .contains("needs structured `question` context")
        );
        assert!(
            no(json!({"kind":"warning","sections":[{"task":"General","status":"","body":"x"}]}))
                .contains("a warning needs `status`")
        );
        let e = no(json!({"kind":"update","sections":[{"task":"T9","status":"","body":"x"}]}));
        assert!(e.contains("unknown task label(s): T9") && e.contains("T2-api"), "{e}");
        assert!(
            no(json!({"kind":"question","sections":[{"task":"General","status":"s","body":"Port?"}]}))
                .contains("`exact_answer: true`")
        );
        assert!(
            no(json!({"kind":"note","sections":[{"task":"General","status":"","body":" "}]}))
                .contains("`body` is empty")
        );
        assert!(no(json!({"kind": "note", "sections": []})).contains("must not be empty"));
        assert!(
            no(json!({"kind": "note", "sections": [
                {"task": "General", "body": "a"}, {"task": "General", "body": "b"}
            ]}))
            .contains("only one section")
        );
        assert!(serde_json::from_value::<Args>(json!({"kind": "note", "body": "old input"})).is_err());
        // Choices belong to a question only: it is its own `say`.
        let e = no(
            json!({"kind":"done","choices":[{"label":"Merge"}],"sections":[{"task":"General","status":"","body":"Done. Merge?"}]}),
        );
        assert!(
            e.contains("only a question has `choices`") && e.contains("its own `say` with kind question"),
            "{e}"
        );
        // Accepted: a question with choices, or one exact answer.
        let mut typed = question_args("General");
        typed["choices"] = json!([]);
        typed["exact_answer"] = json!(true);
        for ok in [
            question_args("General"),
            typed,
            json!({"kind":"update","sections":[{"task":"T2-api","status":"","body":"x"}]}),
        ] {
            assert_eq!(check(&args(ok.clone()), &b), Ok(()), "{ok}");
        }
    }

    #[test]
    fn the_frame_carries_board_status_lines_time_and_trigger() {
        let a = args(
            json!({"kind":"warning","priority":"high","sections":[{"task":"T2-api","status":"blocked","body":"**x**"}]}),
        );
        let s = frame(&a, &board(), 1_700_000_000_000, "after T2-api reported blocked");
        assert_eq!(s.sections.len(), 1);
        assert_eq!(s.sections[0].task, "T2-api");
        assert_eq!(
            s.sections[0].line,
            "T2-api · blocked · needs the db url · no report yet"
        );
        assert!(s.lines.is_empty() && s.body.is_empty() && s.status.is_empty());
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
        assert!(
            q.question.starts_with("T2-api\nT2-api · blocked") && q.question.contains("Where things stand: blocked")
        );
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
        assert_eq!(
            describe("T1 merged (now ready). If the user").as_deref(),
            Some("after T1 merged")
        );
    }

    /// A live run: a decision-check nudge was the last wake before the task report's run.
    /// Nudges are never triggers; the say after the task report names it.
    #[test]
    fn nudges_are_never_triggers_and_a_task_event_wins_a_batch() {
        let t = |es: Vec<Entry>| trigger(&es.iter().collect::<Vec<_>>());
        let user = entry(EntryKind::Message {
            message: Message::user("list the w files"),
        });
        let decision = custom("super", &super::super::context::nudge("list only w files in T1"));
        let report = custom("super", "T1-list-w-files reported done (now review): listed 3 files");
        let want = "after T1-list-w-files reported done";
        // The exact order: user msg, say reply, decision check, todo call, task report, say.
        assert_eq!(
            t(vec![
                user.clone(),
                assistant(true),
                decision.clone(),
                assistant(true),
                report.clone(),
                assistant(true)
            ]),
            want
        );
        // The same with the runs ended by empty replies.
        assert_eq!(
            t(vec![
                user,
                assistant(true),
                assistant(false),
                decision.clone(),
                assistant(true),
                assistant(false),
                report.clone(),
                assistant(true)
            ]),
            want
        );
        // A run started only by a decision check belongs to the event before it.
        assert_eq!(
            t(vec![
                report.clone(),
                assistant(false),
                decision.clone(),
                assistant(true)
            ]),
            want
        );
        // Queued wakes: the task event, not the health check, whatever the order.
        let check = custom("super", &super::super::link::health_check_text());
        assert_eq!(
            t(vec![assistant(false), report, check, decision, assistant(true)]),
            want
        );
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
        let mut input = question_args("T2-api");
        input["choices"] = json!([{"label": "Wait", "action": "answer"}, {"label": "Prepare a fix", "action": "plan"}]);
        let o = t.execute(input, &ctx).await;
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
            json!({"kind":"warning","sections":[{"task":"T2-api","status":"s","body":"b"}]}),
            &ctx,
        )
        .await;
        assert_eq!(t.sup.board.lock().unwrap().needs.len(), 1);
        assert_eq!(t.sup.board.lock().unwrap().needs[0].status, "T2-api: s");
        t.execute(
            json!({"kind":"update","sections":[{"task":"T2-api","status":"","body":"fixed"}]}),
            &ctx,
        )
        .await;
        assert!(Board::load(&t.sup.dir).unwrap().needs.is_empty());
        // A refusal is an error result.
        let o = t
            .execute(
                json!({"kind":"warning","sections":[{"task":"General","status":"","body":"b"}]}),
                &ctx,
            )
            .await;
        assert!(o.is_error);
    }

    #[test]
    fn plain_text_fallback_is_a_general_section() {
        let b = crate::superagent::context::builder(
            std::path::PathBuf::new(),
            Arc::new(|| {}),
            Arc::new(|_, _| String::new()),
            Default::default(),
            Default::default(),
        );
        let a = args((b.plain.unwrap().args)("plain reply"));
        assert_eq!(check(&a, &board()), Ok(()));
        let s = frame(&a, &board(), 0, "reply to you");
        assert_eq!(s.sections[0].task, "General");
        assert_eq!(s.sections[0].body, "plain reply");
        assert!(s.sections[0].line.is_empty());
    }

    #[tokio::test]
    async fn a_structured_typed_question_stays_until_a_reply() {
        let (t, asker, d) = tool(None).await;
        let ctx = ctx(d.path());
        let mut input = question_args("General");
        input["choices"] = json!([]);
        input["exact_answer"] = json!(true);
        let o = t.execute(input, &ctx).await;
        assert!(!o.is_error);
        assert!(asker.0.lock().unwrap().is_empty());
        let saved = Board::load(&t.sup.dir).unwrap();
        assert_eq!(saved.needs.len(), 1);
        assert!(saved.needs[0].typed && saved.needs[0].card.is_none());
        assert!(saved.needs[0].status.starts_with("General: Deployment succeeded"));
        let o = t
            .execute(
                json!({"kind": "reply", "sections": [{"task": "General", "body": "I will prepare the plan only."}]}),
                &ctx,
            )
            .await;
        assert!(!o.is_error);
        assert!(Board::load(&t.sup.dir).unwrap().needs.is_empty());
    }

    #[tokio::test]
    async fn a_dismissed_card_stays_on_needs_you() {
        let (t, _, d) = tool(None).await;
        let ctx = ctx(d.path());
        let o = t.execute(question_args("General"), &ctx).await;
        let text = match &o.content[0] {
            Block::Text { text, .. } => text.clone(),
            _ => String::new(),
        };
        assert!(text.contains("closed the card"), "{text}");
        let saved = Board::load(&t.sup.dir).unwrap();
        assert_eq!(saved.needs.len(), 1);
        assert!(saved.needs[0].status.starts_with("General: Deployment succeeded"));
        let kept = saved.needs[0].card.as_ref().unwrap();
        assert!(
            kept.question
                .contains("What stays unchanged:\n- Production and the current deployment remain unchanged.")
        );
        assert_eq!(kept.options[0].label, "Prepare plan only: Investigate");
    }
}
