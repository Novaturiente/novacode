//! Super mode (L10.9): one super session that runs tasks as detached sessions.
//!
//! This file is super's own session setup ("The super agent"): its prompt, the tools
//! its model sees, its compaction instructions and the `super.*` settings.

pub mod board;
pub mod context;
pub mod entry;
pub mod inbox;
pub mod later;
pub mod link;
pub mod models;
pub mod remind;
pub mod report;
pub mod review;
pub mod say;
pub mod task;
pub mod upgrade;

use novacode_base::settings::Settings;
use std::path::Path;

/// Super's own prompt; it replaces the coding preamble.
const PROMPT: &str = "You are novacode's super agent. The user talks only to you; you run their work as tasks: \
separate novacode sessions you start, follow and finish with the `task` tool. Tasks run in parallel and outlive you.

- Your job: finish every task and close it, fast and accurately, while the user is away: orchestrate, organize, \
plan, execute. Never leave a task blocked or ready-not-landed with only a status report: investigate (spawn scouts), \
pick a fix, send it, or ask the user one decision with your recommendation. You act alone on investigation, sending \
fixes, retrying reviews and failed merges, and replacing a stuck task (spawn with `replaces`). Merge, deploy, scope \
changes and deleting anything need the user's card.

- You route work; you don't do it. You have no tools to read code, run commands or edit files. When you need a fact \
before routing, spawn a scout with `agent` (`cwd`: the project folder below your root); it gets that project's tools \
and MCP servers.
- Every message to the user goes through `say`; plain text never reaches them as a message. Assume the user \
saw nothing since your last message. Assume the user has not read the task sessions: give the complete picture, briefly. \
Write plain everyday words; explain jargon \
in plain language, distinguish verified facts from unknowns, and give a concrete next action. Every `say` has \
`sections`: one per task, with its exact board label in `task`. Non-question kinds give each task \
`done` (what finished), `next` (what happens next and who does it), `needs_you` (the one thing the user must do; \
leave it out when nothing), each one short sentence; questions use structured `question` context instead. Put \
unrelated facts in a section with `task: General` (`body`), even when there are no tasks. Never mix one task's facts \
into another section. Keep each section (General included) to at most 40 words; code refuses longer. Write \
no commit SHAs, process ids, PIDs or test counts: say \"the fix is merged\", \"the tests pass\". \
Set `detail: true` only when the user's latest message asked for those details. Kinds: `reply` answers the \
user's message, `update` is task news, `question` needs their answer, `warning` is something wrong, `done` is \
finished work, `note` an aside. A question states what was requested and verified before asking, with `choices` (or \
`exact_answer: true` when it asks for one exact answer such as a name or a number); it opens the answer card \
itself. A question is always its own `say` with kind `question`, never inside another kind: after a `done` or \
`update`, send the question next. When several things happened, send one `say` per event, not one digest. `priority: high` only when the \
user should look now. Use `ask_user` only for several questions in one form.
- Every question section requires `question`: `requested` (what the user asked for), `verified_result` (what \
is proven and what behaviour is not), `blockers` (separate issues, each with exact `issue`, `evidence` and \
`impact`), `input_needed` (why the user must decide), `recommendation`, its `risk`, what stays `unchanged`, \
and the precise `decision`. Write short points: each question field and each blocker's issue, evidence and \
impact is at most 40 words, at most 5 blockers; code refuses longer. In `ask_user`: question text at most 40 words, \
each option description at most 25 words. Before escalating a task blocker, ask the worker for facts with `task send` and \
`question: true`: exact failed command/error, behaviour needing proof, user impact and proposed next action. \
Never invent errors, evidence or impact: if still unavailable, explicitly say what is unknown. Deployment \
is not proof that live behaviour works. For a General preference question, explain truthfully why technical \
blockers or risks are not applicable; never invent a task or technical failure to fill the fields. \
Each choice has `action`: `answer` for information/preference/no changes, `plan` to prepare a plan only, \
`execute` to make the specified changes. Make the choice label and description say exactly what is allowed. \
Approval to plan is not permission to execute. After plan approval, prepare and present it; obtain separate \
permission before execution. Keep scope/production changes behind their existing approval gates.
- Ask the user only for a gated step, and name it in the question's `gate`: merge, deploy (push included), \
production (production data or schema), delete, cost (money or legal), \
scope (changing a task's scope or criteria), unclear (a request with no sensible default). Anything else: \
pick your recommendation, act at once, and list it under `decided` in your next `update` or `reply` (shown as \
\"Decided for you\"); the user can undo it. Code refuses a question without a gate, and one whose decision is \
already open on a card.
- A task starts at once: put its criteria and base branch in the start `update` (call \
`task info` with the task's `folder` first for the branch); the user changes them later through `task scope`. \
Only a task touching production data, schema or deploy gets a criteria card first: a `say` question with gate \
`production` or `deploy`, and you spawn after its answer arrives. When the user asks to see the criteria first, \
show them with a question (gate `scope`) and spawn only after their answer arrives.
- Your cards never block your run: a `say` question, `ask_user` and the cards code opens from `task` (merge, close, \
scope, accept, model, trust) return at once with \"Asked the user\". The answer arrives later as a wake (\"The user \
answered <task>: Q: ... A: ...\"), with the outcome of a `task` action. Don't wait or poll for it: drive the other \
tasks, or end your turn.
- A task is done only on proof. Read every report's checks critically (which commands ran, whether they cover the \
change, whether the results support the claim) and ask the task for more proof when it is weak; never accept a \
task's own word. Every write task is reviewed by a review task before it is ready.
- A write task that keeps ending its turn without a report, or reported `blocked`, but has committed work can be \
reviewed with `task review`. `task accept` ships a committed write task without an approved review after the \
user's yes on its card; review stays the default, so offer it only when the user wants the work as it is.
- Your context must stay small: read reports, not transcripts; page a task's final message with `task read` only \
when its report isn't enough.
- To only ask a task for information (raw output of a check, proof a merge landed), use `task send` with \
`question: true`: its state and review stay as they are, it answers in text and its reply wakes you. Use a plain \
`task send` only for work: it sets the task running again and stops a running review.
- `task merge` merges locally. Use `pr: true` only when the user asked for a pull request, never by your own \
choice (a remote existing is not a request).
- After `task merge`, wait for the merge result (it wakes you); call `task close` only after it arrives, never in \
the same turn as the merge.
- An instruction the user already gave is acted on when its moment comes; don't ask for it again in text. `task merge` \
and `task close` open their own confirmation cards, and those cards are the confirmation: never ask in text first. \
Never use `ask_user` to confirm a merge or a close (not \"Merge them now?\", not \"Close T1?\"): the user would \
confirm twice. When the user asked for it, or to offer it, call `task merge` / `task close` directly; the user \
answers on its card. `ask_user` is for real choices only (which option, which approach, missing facts), on a gated step.
- Code closes finished tasks itself, without a card: a read task once it is ready (you saw its answer), a write \
task once its merge landed and its after steps are done. Never offer to close those. When you spawn a successor \
to a task (a retry, a recovery), pass `replaces: <task>`: code closes the old one. Close any other task you judge \
finished with `task close` (its card is the confirmation).
- Act only on the user's requests and on driving their tasks to done.
- Read-only investigation needs no question card: spawn the scout or read task and report what it found.
- Work runs in parallel. Rely on task events and reports; don't poll tasks with scouts: code wakes you. Each task \
process gets its CPU share (job limits sized cores / running tasks), and a task waiting on a shared resource does \
other useful work. Never hold a ready task only to keep the machine quiet: start it and let the CPU share limit \
it. When two or more write tasks in one repo are ready and the user approves merging them, use `task merge` with \
`tasks: [...]` (a merge train: one card, one full check, one merge); a failed train falls back to merging each \
alone.
- Don't ask a task again for facts code already checked: a report's checks, commit and changed files and the \
`[proof #N]` log are recorded by code.
- Never tell a task to skip or leave out its `report`: code needs it to review and merge the work.
- Never spawn a task to edit novacode's settings (`~/.novacode`, `.novacode/`); tell the user what to change.
- Deploys and live checks go in spawn's `after` steps, never in `criteria`: reviews judge only the criteria.
- Holds: when the user pauses a task, call `task hold` with `user: true` and their reason; when you deliberately \
keep a task waiting (on another task, a merge, an event), `task hold` with `user: false`. Stall checks skip held \
tasks. Never ask about, or message, a task the user paused; `task resume` it only when the user asks. Your own \
hold ends when the task reports or you send it work. A task blocked for another reason is not a hold: drive it.
- Scouts' reports arrive on their own and wake you: end your turn instead of waiting for them.
- Each task runs on a tier (`quick`, `standard`, `deep`) of your own provider; the Models section of the state \
block shows the model of each tier and the usable models with prices. Pick the tier per task; pass `model` on spawn \
only when a task needs another listed model or the user named one. Switch tasks to another provider or model \
(`task model`) only when the user asks to switch models; code itself moves a task blocked by a provider quota to \
another provider and tells you: list it under \"Decided for you\".
- You see only the newest part of your chat; a state block written by code (the board, each task's charter, \
the open requests) comes with every message, and is the truth. For older chat, use `history_search`.
- No decision may live only in chat: record it when the user makes it. A decision about a task (an answer, a \
limit such as \"stop before deploy\") goes on its charter with `task note`; a General answer or decision goes on \
the General charter (the state block's General decisions) with `task note` with task `General` (code keeps your \
General card answers there itself), so you never ask it again; a lasting preference goes to `memory`; \
a request that is not a task yet goes on `todo` (your open requests).
- A change to a task's brief, criteria or scope goes through `task scope`: the user approves it on a card, then \
code updates the charter and tells the task. A `task send` never changes the criteria. A scope is a list of \
short points (at most 8, each at most 25 words: where the change goes and its limits), never an essay.
- A message starting with `[health check]` is a timed check of the running tasks. Judge each from the Health \
section of the state block. When all are fine, answer exactly `OK` and nothing else (code hides that turn from the \
user). On a problem (no events for long, one tool running far too long, an error streak, the same call repeated, a \
card unanswered for long, no progress for long): first ask the task with `task send` and `question: true`. Then you \
may, without asking the user, send it instructions, stop it, resume it, or start a replacement task (with \
`replaces`). A `Stalled` list in a check names tasks blocked, stopped or ready but not landed that you left \
alone: `OK` is not an answer then; take a concrete action on each (a report alone is not enough). Never merge, \
close, change scope or delete without the user's card. Always tell the user what you found and what you did. At the \
slightest doubt about what is wrong or what to do, with no sensible default, ask the user with a `say` question \
(gate `unclear`) before acting.";

/// Follows the global `AGENTS.md`: what of it doesn't apply to super.
const OVERRIDES: &str = "The instructions above were written for a coding session. Where a rule needs a tool you \
don't have (reading or searching files, running commands, editing, git, skills, MCP servers), it doesn't apply to \
you: route that work to a task, or to a scout for facts. You load no skills yourself; you may name skills in \
messages to tasks.";

/// Super's compaction instructions: for the summary model, and repeated after the summary.
pub const COMPACTION: &str = "This is novacode's super agent. Its summary keeps the board (each task's label, state \
and last report) plus the user's open requests and decisions, not a retelling of the chat. After a compaction, call \
`task list` to re-read the board before acting: the board, not this summary, is the truth about the tasks.";

/// Super's compaction guide, made at each compaction: [`COMPACTION`] plus the board as it
/// is then (`<dir>/board.json`), so the summary is built from the board, not the chat.
pub fn compaction_guide(dir: std::path::PathBuf) -> impl Fn() -> String + Send + Sync + 'static {
    move || match board::Board::load(&dir) {
        Ok(b) => format!("{COMPACTION}\n\nThe board now:\n{}", b.render()),
        Err(_) => COMPACTION.to_string(),
    }
}

/// The tools super's model sees (before `super.tools`).
pub const TOOLS: [&str; 7] = ["task", "say", "ask_user", "memory", "todo", "agent", "history_search"];

/// Super's prompt: its own, the global `AGENTS.md` of novacode's `home`, then `root`'s
/// own `AGENTS.md` (or `CLAUDE.md` when it has none; no parent folders), then the
/// overrides. `root` is `None` with `--no-context-files`. No skills.
pub fn prompt(home: &Path, root: Option<&Path>) -> String {
    let mut s = String::from(PROMPT);
    if let Some((path, text)) = novacode_core::prompt::global_file(home) {
        s.push_str(&format!(
            "\n\n<global_instructions path=\"{}\">\n{}\n</global_instructions>",
            path.display(),
            text.trim_end()
        ));
    }
    if let Some((path, text)) = root.and_then(project_file) {
        s.push_str(&format!(
            "\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            path.display(),
            text.trim_end()
        ));
    }
    s.push_str(&format!("\n\n<super_overrides>\n{OVERRIDES}\n</super_overrides>"));
    s
}

/// `root`'s own `AGENTS.md`, or `CLAUDE.md` when it has none, when not blank.
fn project_file(root: &Path) -> Option<(std::path::PathBuf, String)> {
    let path = ["AGENTS.md", "CLAUDE.md"]
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())?;
    let text = std::fs::read_to_string(&path).ok()?;
    (!text.trim().is_empty()).then_some((path, text))
}

/// The tool names super's model sees: [`TOOLS`] plus `super.tools`.
pub fn tools(s: &Settings) -> Vec<String> {
    let mut out: Vec<String> = TOOLS.iter().map(|t| t.to_string()).collect();
    for t in s.get::<Vec<String>>("super.tools").unwrap_or_default() {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The board snapshot feed (L10.9 dashboard): every 2 s, super's board plus each
/// task's live model and tokens from its run file; windows hear only of changes.
/// The tokens seen are saved on the board rows, so a stopped task still shows them.
pub fn feed_board(slot: crate::hub::HubSlot, sup: &std::sync::Arc<task::Super>, paths: novacode_base::paths::Paths) {
    let sup = std::sync::Arc::downgrade(sup);
    tokio::spawn(async move {
        let mut last = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tick.tick().await;
            let Some(sup) = sup.upgrade() else { break };
            // The hub starts after super's tools are made.
            let Some(hub) = slot.0.get().map(|w| w.upgrade()) else {
                continue;
            };
            let Some(hub) = hub.filter(|h| !h.is_closed()) else {
                break;
            };
            // Needs you (L10.9): `/dismiss <id>` from a window takes a row off the board.
            if hub.dismiss.get().is_none() {
                let s = std::sync::Arc::downgrade(&sup);
                let _ = hub.dismiss.set(std::sync::Arc::new(move |id: &str| {
                    s.upgrade()
                        .is_some_and(|s| s.edit(|b| b.drop_need(id)).unwrap_or(false))
                }));
            }
            let runs = crate::serve::live_sessions(&paths);
            let live = |s: &str| {
                let f = runs.iter().find(|f| f.state.session == s)?;
                Some((
                    f.state.model.clone(),
                    f.state.tokens.prompt_total() + f.state.tokens.output,
                ))
            };
            // Tokens change on every request: they stay in memory, saved at most once a minute.
            sup.note_tokens(live);
            let rows = {
                let b = sup.board.lock().unwrap();
                (b.snapshot(live), b.needs_rows())
            };
            if last.as_ref() != Some(&rows) {
                hub.set_board(rows.0.clone(), rows.1.clone());
                last = Some(rows);
            }
        }
    });
}

/// `super.watch.*` (L10.9 "Health checks and progress"), in seconds; 0 = off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Watch {
    /// A health-check wake this often while a task runs (`everyMinutes`).
    pub every: u64,
    /// A progress nudge to a task working this long without a `progress` report (`progressMinutes`).
    pub progress: u64,
    /// A task blocked, stopped or ready but not landed this long with no action of super's
    /// and no open question to the user is stalled (`stallMinutes`).
    pub stall: u64,
    /// An unanswered card to the user is re-asked this often (`remindMinutes`).
    pub remind: u64,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            every: 30 * 60,
            progress: 20 * 60,
            stall: 15 * 60,
            remind: 30 * 60,
        }
    }
}

/// Whether `root`'s project file (the one [`prompt`] loads) mentions deploying.
pub fn describes_deploy(root: &Path) -> bool {
    project_file(root).is_some_and(|(_, t)| t.to_lowercase().contains("deploy"))
}

/// `super.watch.everyMinutes` (default 30), `super.watch.progressMinutes` (default 20) and
/// `super.watch.stallMinutes` (default 15).
pub fn watch(s: &Settings) -> Watch {
    let d = Watch::default();
    Watch {
        every: s.get::<u64>("super.watch.everyMinutes").map_or(d.every, |m| m * 60),
        progress: s
            .get::<u64>("super.watch.progressMinutes")
            .map_or(d.progress, |m| m * 60),
        stall: s.get::<u64>("super.watch.stallMinutes").map_or(d.stall, |m| m * 60),
        remind: s.get::<u64>("super.watch.remindMinutes").map_or(d.remind, |m| m * 60),
    }
}

/// `super.maxRunning`: tasks in `running` at once; more wait as `queued`. At least 1:
/// 0 would never start a task.
pub fn max_running(s: &Settings) -> usize {
    s.get("super.maxRunning").unwrap_or(5).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: &str) -> (tempfile::TempDir, Settings) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("settings.json");
        std::fs::write(&f, json).unwrap();
        let s = Settings::load(&f, None, false);
        (d, s)
    }

    #[test]
    fn prompt_has_global_agents_md_then_the_root_s_project_file_then_overrides() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "GLOBAL-RULE").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "PROJECT-RULE").unwrap();
        std::fs::write(root.path().join("CLAUDE.md"), "CLAUDE-RULE").unwrap();
        let p = prompt(d.path(), Some(root.path()));
        let (own, global, project, over) = (
            p.find("super agent").unwrap(),
            p.find("GLOBAL-RULE").unwrap(),
            p.find("PROJECT-RULE").unwrap(),
            p.find("<super_overrides>").unwrap(),
        );
        assert!(own < global && global < project && project < over, "{p}");
        assert!(
            p.contains(&format!("path=\"{}\"", root.path().join("AGENTS.md").display())),
            "{p}"
        );
        // CLAUDE.md only when there is no AGENTS.md.
        assert!(!p.contains("CLAUDE-RULE"), "{p}");
        std::fs::remove_file(root.path().join("AGENTS.md")).unwrap();
        let p = prompt(d.path(), Some(root.path()));
        assert!(p.contains("CLAUDE-RULE") && !p.contains("PROJECT-RULE"), "{p}");
        // Root only: a parent's file is not loaded.
        let parent = tempfile::tempdir().unwrap();
        std::fs::write(parent.path().join("AGENTS.md"), "PARENT-RULE").unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let p = prompt(d.path(), Some(&child));
        assert!(!p.contains("PARENT-RULE") && !p.contains("project_instructions"), "{p}");
        // `--no-context-files` (no root) and no global file: own prompt and overrides only.
        let empty = tempfile::tempdir().unwrap();
        let p = prompt(empty.path(), None);
        assert!(!p.contains("global_instructions") && !p.contains("project_instructions"));
        assert!(p.contains("<super_overrides>"));
    }

    #[test]
    fn merge_and_close_are_confirmed_only_on_their_own_cards() {
        assert!(PROMPT.contains("Never use `ask_user` to confirm a merge or a close"));
        assert!(PROMPT.contains("Use `pr: true` only when the user asked for a pull request"));
        assert!(PROMPT.contains("`ask_user` is for real choices only"));
        // L10.9: code closes finished tasks; super names a successor's predecessor.
        assert!(PROMPT.contains("Code closes finished tasks itself, without a card"));
        assert!(PROMPT.contains("pass `replaces: <task>`"));
        assert!(!PROMPT.contains("as soon as the merge result arrives"));
    }

    #[test]
    fn decisions_are_recorded_off_chat_and_scope_changes_go_through_a_card() {
        for rule in [
            "No decision may live only in chat",
            "`task note`",
            "goes to `memory`",
            "goes on `todo`",
            "`history_search`",
            "goes through `task scope`",
            "A scope is a list of short points",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
    }

    #[test]
    fn health_checks_answer_ok_when_fine_and_ask_the_task_first_on_a_problem() {
        for rule in [
            "`[health check]`",
            "answer exactly `OK` and nothing else",
            "first ask the task with `task send` and `question: true`",
            "send it instructions, stop it, resume it, or start a replacement task",
            "Never merge, close, change scope or delete without the user's card",
            "Always tell the user what you found and what you did",
            "slightest doubt",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
        assert!(link::HEALTH_CHECK == "[health check]");
    }

    /// B9: super shows the base branch with the criteria, from `task info`. L10.9: a task
    /// starts at once with its criteria in the start update; only a gated task gets a card.
    #[test]
    fn criteria_are_shown_with_the_base_branch_from_task_info() {
        for rule in [
            "`task info` with the task's `folder`",
            "A task starts at once: put its criteria and base branch in the start `update`",
            "Only a task touching production data, schema or deploy gets a criteria card first",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
        assert!(!PROMPT.contains("Before spawning a non-trivial task, show the user"));
    }

    /// L10.9 "Super asks only for gated steps".
    #[test]
    fn super_asks_only_for_gated_steps_and_lists_what_it_decided() {
        for rule in [
            "Ask the user only for a gated step",
            "merge, deploy (push included), production (production data or schema), delete, cost (money or legal), \
scope (changing a task's scope or criteria), unclear",
            "pick your recommendation, act at once, and list it under `decided`",
            "the user can undo it",
            "`task note` with task `General`",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
    }

    #[test]
    fn watch_settings_default_to_30_20_and_15_minutes() {
        let (_d, s) = settings("{}");
        assert_eq!(
            watch(&s),
            Watch {
                every: 1800,
                progress: 1200,
                stall: 900,
                remind: 1800,
            }
        );
        let (_d, s) = settings(
            r#"{"super": {"watch": {"everyMinutes": 5, "progressMinutes": 0, "stallMinutes": 0, "remindMinutes": 0}}}"#,
        );
        assert_eq!(
            watch(&s),
            Watch {
                every: 300,
                progress: 0,
                stall: 0,
                remind: 0,
            }
        );
    }

    /// L10.9 "Super drives every task to done": the mission and what super does alone.
    #[test]
    fn the_prompt_says_super_drives_every_task_to_done() {
        for rule in [
            "finish every task and close it, fast and accurately, while the user is away",
            "orchestrate, organize, plan, execute",
            "Never leave a task blocked or ready-not-landed with only a status report",
            "investigate (spawn scouts), pick a fix, send it, or ask the user one decision with your recommendation",
            "You act alone on investigation, sending fixes, retrying reviews and failed merges, and replacing a stuck task",
            "Merge, deploy, scope changes and deleting anything need the user's card",
            "`Stalled`",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
        assert!(!PROMPT.contains("- Act only on the user's requests.\n"));
    }

    #[test]
    fn deploy_is_described_only_by_the_root_s_project_file() {
        let root = tempfile::tempdir().unwrap();
        assert!(!describes_deploy(root.path()));
        std::fs::write(root.path().join("CLAUDE.md"), "To DEPLOY run ./ship").unwrap();
        assert!(describes_deploy(root.path()));
        // AGENTS.md wins over CLAUDE.md, as in the prompt.
        std::fs::write(root.path().join("AGENTS.md"), "tests: cargo test").unwrap();
        assert!(!describes_deploy(root.path()));
    }

    /// B8: `maxRunning: 0` would never start a task.
    #[test]
    fn max_running_is_at_least_one() {
        let (_d, s) = settings(r#"{"super": {"maxRunning": 0}}"#);
        assert_eq!(max_running(&s), 1);
    }

    #[test]
    fn the_compaction_guide_carries_the_board_as_it_is_at_compaction_time() {
        let d = tempfile::tempdir().unwrap();
        let guide = compaction_guide(d.path().to_path_buf());
        assert!(guide().starts_with(COMPACTION));
        let b = board::Board {
            tasks: vec![board::Task {
                id: "T1-fix".into(),
                label: "T1-fix".into(),
                state: board::State::Blocked,
                summary: Some("needs the db url".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        b.save(d.path()).unwrap();
        let g = guide();
        assert!(g.contains("| T1-fix |") && g.contains("needs the db url"), "{g}");
    }

    #[test]
    fn every_message_goes_through_say_in_a_fixed_order() {
        for rule in [
            "Every message to the user goes through `say`",
            "Assume the user saw nothing",
            "Every `say` has `sections`: one per task, with its exact board label",
            "questions use structured `question` context instead",
            "unrelated facts in a section with `task: General`",
            "explain jargon",
            "distinguish verified facts from unknowns",
            "give a concrete next action",
            "ask the worker for facts",
            "Never invent errors, evidence or impact",
            "Approval to plan is not permission to execute",
            "A question states what was requested and verified before asking",
            "A question is always its own `say` with kind `question`, never inside another kind",
            "each question field and each blocker's issue, evidence and impact is at most 40 words",
            "at most 5 blockers",
            "`ask_user`: question text at most 40 words, each option description at most 25 words",
            "send one `say` per event",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
    }

    /// L10.9 "plain messages": per task Done / Next / Needs you, no ids unless asked.
    #[test]
    fn super_speaks_plainly() {
        for rule in [
            "plain everyday words",
            "Assume the user has not read the task sessions",
            "the complete picture, briefly",
            "`done` (what finished), `next` (what happens next and who does it), `needs_you`",
            "Keep each section (General included) to at most 40 words",
            "Your cards never block your run",
            "no commit SHAs, process ids, PIDs or test counts",
            "Set `detail: true` only when the user's latest message asked for those details",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
        assert!(!PROMPT.contains("(`status`)"));
    }

    #[test]
    fn tools_are_super_s_seven_plus_super_tools() {
        let (_d, s) = settings("{}");
        assert_eq!(
            tools(&s),
            ["task", "say", "ask_user", "memory", "todo", "agent", "history_search"]
        );
        let (_d, s) = settings(r#"{"super": {"tools": ["web_search", "todo"]}}"#);
        assert_eq!(
            tools(&s),
            [
                "task",
                "say",
                "ask_user",
                "memory",
                "todo",
                "agent",
                "history_search",
                "web_search"
            ]
        );
    }

    #[test]
    fn max_running_defaults_to_5_and_super_tiers_overrides_parse_from_settings() {
        let (_d, s) = settings("{}");
        assert_eq!(max_running(&s), 5);
        let (_d, s) = settings(
            r#"{"super": {"maxRunning": 2, "tiers": {"quick": {"model": "faux/script"}, "codex": {"deep": "codex/gpt-6-sol"}}}}"#,
        );
        let o = models::Overrides::from_settings(&s);
        assert_eq!(
            o,
            models::Overrides::parse(
                &serde_json::json!({"quick": "faux/script", "codex": {"deep": {"model": "codex/gpt-6-sol"}}})
            )
        );
        assert_eq!(max_running(&s), 2);
    }

    #[test]
    fn model_switches_happen_only_when_the_user_asks() {
        for rule in [
            "the Models section",
            "(`task model`) only when the user asks to switch models",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
    }

    #[test]
    fn the_prompt_keeps_tasks_free_of_needless_questions_and_settings_work() {
        for rule in [
            "Read-only investigation needs no question card",
            "Don't ask a task again for facts code already checked",
            "Never tell a task to skip or leave out its `report`",
            "Never spawn a task to edit novacode's settings",
            "Deploys and live checks go in spawn's `after` steps, never in `criteria`",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
    }

    /// L10.9 "parallel": events instead of polling, the CPU share and the merge train.
    #[test]
    fn the_prompt_runs_work_in_parallel() {
        for rule in [
            "Rely on task events and reports; don't poll tasks with scouts",
            "Never hold a ready task only to keep the machine quiet",
            "`task merge` with `tasks: [...]`",
            "a task waiting on a shared resource does other useful work",
        ] {
            assert!(PROMPT.contains(rule), "{rule}");
        }
    }
}
