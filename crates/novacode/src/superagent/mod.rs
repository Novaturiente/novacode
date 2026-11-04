//! Super mode (L10.9): one super session that runs tasks as detached sessions.
//!
//! This file is super's own session setup ("The super agent"): its prompt, the tools
//! its model sees, its compaction instructions and the `super.*` settings.

pub mod board;
pub mod entry;
pub mod inbox;
pub mod link;
pub mod report;
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
- Before spawning a non-trivial task, show the user 1-5 checkable acceptance criteria; a small, clear task starts at once.
- A task is done only on proof. Read every report's checks critically (which commands ran, whether they cover the \
change, whether the results support the claim) and ask the task for more proof when it is weak; never accept a \
task's own word. Every write task is reviewed by a review task before it is ready.
- Your context must stay small: read reports, not transcripts; page a task's final message with `task read` only \
when its report isn't enough.
- Act only on the user's requests.";

/// Follows the global `AGENTS.md`: what of it doesn't apply to super.
const OVERRIDES: &str = "The instructions above were written for a coding session. Where a rule needs a tool you \
don't have (reading or searching files, running commands, editing, git, skills, MCP servers), it doesn't apply to \
you: route that work to a task, or to a scout for facts. You load no skills yourself; you may name skills in \
messages to tasks.";

/// Super's compaction instructions: for the summary model, and repeated after the summary.
pub const COMPACTION: &str = "This is novacode's super agent. Its summary keeps the board (each task's label, state \
and last report) plus the user's open requests and decisions, not a retelling of the chat. After a compaction, call \
`task list` to re-read the board before acting: the board, not this summary, is the truth about the tasks.";

/// The tools super's model sees (before `super.tools`).
pub const TOOLS: [&str; 5] = ["task", "ask_user", "memory", "todo", "agent"];

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
pub fn feed_board(hub: std::sync::Arc<crate::hub::Hub>, dir: std::path::PathBuf, paths: novacode_base::paths::Paths) {
    tokio::spawn(async move {
        let mut last = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        while !hub.is_closed() {
            tick.tick().await;
            let Ok(b) = board::Board::load(&dir) else { continue };
            let runs = crate::serve::live_sessions(&paths);
            let rows = b.snapshot(|s| {
                let f = runs.iter().find(|f| f.state.session == s)?;
                Some((
                    f.state.model.clone(),
                    f.state.tokens.prompt_total() + f.state.tokens.output,
                ))
            });
            if last.as_ref() != Some(&rows) {
                hub.set_board(rows.clone());
                last = Some(rows);
            }
        }
    });
}

/// `super.maxRunning`: tasks in `running` at once; more wait as `queued`.
pub fn max_running(s: &Settings) -> usize {
    s.get("super.maxRunning").unwrap_or(5)
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
    fn tools_are_super_s_five_plus_super_tools() {
        let (_d, s) = settings("{}");
        assert_eq!(tools(&s), ["task", "ask_user", "memory", "todo", "agent"]);
        let (_d, s) = settings(r#"{"super": {"tools": ["web_search", "todo"]}}"#);
        assert_eq!(tools(&s), ["task", "ask_user", "memory", "todo", "agent", "web_search"]);
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
