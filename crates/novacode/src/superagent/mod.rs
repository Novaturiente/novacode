//! Super mode (L10.9): one super session that runs tasks as detached sessions.
//!
//! This file is super's own session setup ("The super agent"): its prompt, the tools
//! its model sees, its compaction instructions and the `super.*` settings.

pub mod board;
pub mod context;
pub mod entry;
pub mod inbox;
pub mod link;
pub mod report;
pub mod review;
pub mod task;

use novacode_base::settings::Settings;
use serde::Deserialize;
use std::path::Path;

/// Super's own prompt; it replaces the coding preamble.
const PROMPT: &str = "You are novacode's super agent. The user talks only to you; you run their work as tasks: \
separate novacode sessions you start, follow and finish with the `task` tool. Tasks run in parallel and outlive you.

- You route work; you don't do it. You have no tools to read code, run commands or edit files. When you need a fact \
before routing, spawn a scout with `agent` (`cwd`: the project folder below your root); it gets that project's tools \
and MCP servers.
- Name the task by its label in every message about it.
- Before spawning a non-trivial task, show the user 1-5 checkable acceptance criteria, with the base branch: call \
`task info` with the task's `folder` first and show what it says about the branch; a small, clear task starts at once. \
When the user asks to see the criteria first, show them with `ask_user` and wait for the answer before you spawn.
- A task is done only on proof. Read every report's checks critically (which commands ran, whether they cover the \
change, whether the results support the claim) and ask the task for more proof when it is weak; never accept a \
task's own word. Every write task is reviewed by a review task before it is ready.
- A write task that keeps ending its turn without a report but has committed work can be reviewed with `task review`.
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
answers on its card. `ask_user` is for real choices only (which option, which approach, missing facts). \
When the user said to close a task after its merge, call `task close` as soon as the merge result arrives.
- Act only on the user's requests.
- You see only the newest part of your chat; a state block written by code (the board, each task's charter, \
the open requests) comes with every message, and is the truth. For older chat, use `history_search`.
- No decision may live only in chat: record it when the user makes it. A decision about a task (an answer, a \
limit such as \"stop before deploy\") goes on its charter with `task note`; a lasting preference goes to `memory`; \
a request that is not a task yet goes on `todo` (your open requests).
- A change to a task's brief, criteria or scope goes through `task scope`: the user approves it on a card, then \
code updates the charter and tells the task. A `task send` never changes the criteria.
- A message starting with `[health check]` is a timed check of the running tasks. Judge each from the Health \
section of the state block. When all are fine, answer exactly `OK` and nothing else (code hides that turn from the \
user). On a problem (no events for long, one tool running far too long, an error streak, the same call repeated, a \
card unanswered for long, no progress for long): first ask the task with `task send` and `question: true`. Then you \
may, without asking the user, send it instructions, stop it, resume it, or start a replacement task. Never merge, \
close, change scope or delete without the user's card. Always tell the user what you found and what you did. At the \
slightest doubt about what is wrong or what to do, ask the user with `ask_user` before acting.";

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
pub const TOOLS: [&str; 6] = ["task", "ask_user", "memory", "todo", "agent", "history_search"];

/// Super's prompt: its own, the global `AGENTS.md` of novacode's `home`, then the
/// overrides. No project `AGENTS.md` and no skills.
pub fn prompt(home: &Path) -> String {
    let mut s = String::from(PROMPT);
    if let Some((path, text)) = novacode_core::prompt::global_file(home) {
        s.push_str(&format!(
            "\n\n<global_instructions path=\"{}\">\n{}\n</global_instructions>",
            path.display(),
            text.trim_end()
        ));
    }
    s.push_str(&format!("\n\n<super_overrides>\n{OVERRIDES}\n</super_overrides>"));
    s
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

/// A model tier for tasks (`super.tiers.<name>`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Tier {
    pub model: String,
    /// When to use it, for super's model.
    #[serde(default)]
    pub note: String,
}

/// The tiers, weakest to strongest, with the shipped full model ids (user answer r3 Q12).
const TIERS: [(&str, &str, &str); 4] = [
    ("quick", "claude/claude-haiku-5-5", "small, clear changes and lookups"),
    ("standard", "claude/claude-sonnet-5-5", "most tasks"),
    (
        "deep",
        "claude/claude-opus-5-5:high",
        "hard bugs, design and wide changes",
    ),
    (
        "review",
        "codex/gpt-6.1-sol:high",
        "reviews of deep tasks and of user-named models",
    ),
];

/// `super.tiers`, weakest first: each shipped tier, replaced by the user's entry of that name.
pub fn tiers(s: &Settings) -> Vec<(String, Tier)> {
    let user: std::collections::BTreeMap<String, Tier> = s.get("super.tiers").unwrap_or_default();
    TIERS
        .iter()
        .map(|(name, model, note)| {
            let t = user.get(*name).cloned().unwrap_or_else(|| Tier {
                model: model.to_string(),
                note: note.to_string(),
            });
            (name.to_string(), t)
        })
        .collect()
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
            let runs = crate::serve::live_sessions(&paths);
            let live = |s: &str| {
                let f = runs.iter().find(|f| f.state.session == s)?;
                Some((
                    f.state.model.clone(),
                    f.state.tokens.prompt_total() + f.state.tokens.output,
                ))
            };
            let rows = {
                let mut b = sup.board.lock().unwrap();
                if b.note_tokens(live) {
                    let _ = b.save(&sup.dir);
                }
                b.snapshot(live)
            };
            if last.as_ref() != Some(&rows) {
                hub.set_board(rows.clone());
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
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            every: 30 * 60,
            progress: 20 * 60,
        }
    }
}

/// `super.watch.everyMinutes` (default 30) and `super.watch.progressMinutes` (default 20).
pub fn watch(s: &Settings) -> Watch {
    let d = Watch::default();
    Watch {
        every: s.get::<u64>("super.watch.everyMinutes").map_or(d.every, |m| m * 60),
        progress: s
            .get::<u64>("super.watch.progressMinutes")
            .map_or(d.progress, |m| m * 60),
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
    fn prompt_has_global_agents_md_and_overrides_but_no_project_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "GLOBAL-RULE").unwrap();
        let p = prompt(d.path());
        let (own, global, over) = (
            p.find("super agent").unwrap(),
            p.find("GLOBAL-RULE").unwrap(),
            p.find("<super_overrides>").unwrap(),
        );
        assert!(own < global && global < over, "{p}");
        // No global file: own prompt and overrides only.
        let empty = tempfile::tempdir().unwrap();
        let p = prompt(empty.path());
        assert!(!p.contains("global_instructions") && p.contains("<super_overrides>"));
    }

    #[test]
    fn merge_and_close_are_confirmed_only_on_their_own_cards() {
        assert!(PROMPT.contains("Never use `ask_user` to confirm a merge or a close"));
        assert!(PROMPT.contains("Use `pr: true` only when the user asked for a pull request"));
        assert!(PROMPT.contains("`ask_user` is for real choices only"));
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

    /// B9: super shows the base branch with the criteria, from `task info`.
    #[test]
    fn criteria_are_shown_with_the_base_branch_from_task_info() {
        assert!(PROMPT.contains("`task info` with the task's `folder`"), "{PROMPT}");
    }

    #[test]
    fn watch_settings_default_to_30_and_20_minutes() {
        let (_d, s) = settings("{}");
        assert_eq!(
            watch(&s),
            Watch {
                every: 1800,
                progress: 1200
            }
        );
        let (_d, s) = settings(r#"{"super": {"watch": {"everyMinutes": 5, "progressMinutes": 0}}}"#);
        assert_eq!(
            watch(&s),
            Watch {
                every: 300,
                progress: 0
            }
        );
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
    fn tools_are_super_s_six_plus_super_tools() {
        let (_d, s) = settings("{}");
        assert_eq!(
            tools(&s),
            ["task", "ask_user", "memory", "todo", "agent", "history_search"]
        );
        let (_d, s) = settings(r#"{"super": {"tools": ["web_search", "todo"]}}"#);
        assert_eq!(
            tools(&s),
            [
                "task",
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
    fn tier_defaults_are_full_ids_that_resolve_and_user_entries_replace_them() {
        let (_d, s) = settings("{}");
        let t = tiers(&s);
        let names: Vec<&str> = t.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["quick", "standard", "deep", "review"]);
        assert_eq!(max_running(&s), 5);
        let p = novacode_ai::connect::Providers::load(Path::new("/nonexistent"), &Settings::default());
        for (name, tier) in &t {
            let r = p.resolve(&tier.model).unwrap_or_else(|e| panic!("{name}: {e}"));
            let profile = p.profile(&r.provider).unwrap();
            assert!(
                p.catalog.get(profile.catalog_id(), &r.model).is_some(),
                "{name}: `{}` is not in the bundled catalog",
                tier.model
            );
        }
        let (_d, s) = settings(r#"{"super": {"maxRunning": 2, "tiers": {"quick": {"model": "faux/script"}}}}"#);
        let t = tiers(&s);
        assert_eq!(t[0].1.model, "faux/script");
        assert_eq!(t[1].1.model, "claude/claude-sonnet-5-5");
        assert_eq!(max_running(&s), 2);
    }
}
