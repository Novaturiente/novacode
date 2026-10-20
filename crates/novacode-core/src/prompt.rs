//! System prompt assembly (L4.7) and context files (L4.9).
//!
//! The prompt is a list of named sections rendered in a fixed order. The model
//! gets them all once; later only the sections that changed are appended, as a
//! system delta, so the cached prefix never changes (L1.7). Values that drift
//! (date, branch, HEAD) are written at session start and reported by an appended
//! reminder when they change.

use std::path::{Path, PathBuf};
use std::process::Command;

/// (name, rendered text). `preamble` is plain; every other section is wrapped in
/// a tag of its own name, so a later update can name what it replaces.
pub type Sections = Vec<(String, String)>;

pub const DEFAULT_PREAMBLE: &str = "You are novacode, a coding agent working in the user's terminal. You help by reading and searching code, editing files and running commands with your tools.

- Read a file before you edit it. Prefer small, exact edits over rewriting whole files.
- Check your work with the project's own build and test commands when it has them.
- Every model call costs time and money: put independent tool calls in one message (reads, searches, edits to different files), and chain dependent steps in one codemode script when there are three or more.
- Calls in one message run at the same time. Never put a call in the same message as a call whose result or side effect it needs (a file it creates, an edit it checks, a build it tests): send it in the next message or chain both in codemode.
- Ask the user when a request has more than one reasonable reading or an action can't be undone.
- Keep replies short and concrete, and show file paths clearly.";

/// Where the prompt's parts come from.
#[derive(Debug, Clone)]
pub struct Sources {
    pub cwd: PathBuf,
    /// novacode's home: global `AGENTS.md`, `SYSTEM.md`, `APPEND_SYSTEM.md`.
    pub home: PathBuf,
    /// `--no-context-files` turns this off.
    pub context_files: bool,
    /// Skill folders, earlier ones winning a name clash (L4.10). Read on every rebuild,
    /// so a new skill reaches the model as a section delta.
    pub skill_dirs: Vec<PathBuf>,
    /// Guidelines of the listed tools; the agent fills this from its tools.
    pub tool_guidelines: Vec<String>,
    /// A subagent's role prompt (L10.1): (text, replaces the preamble).
    pub agent: Option<(String, bool)>,
    /// The saved-memory index (L10.4), read once at session start so it never drifts.
    pub memory_index: Option<String>,
    /// The active mode preset's section (L10.7); switching appends a delta.
    pub mode: Option<String>,
}

/// What drifts during a session: compared before each run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvState {
    pub date: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Read the current day (local time) and git position.
pub fn env_state(cwd: &Path) -> EnvState {
    let out = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let git = out("git", &["rev-parse", "--abbrev-ref", "HEAD", "--short", "HEAD"]);
    let mut lines = git.as_deref().unwrap_or_default().lines().map(str::to_string);
    EnvState {
        date: out("date", &["+%Y-%m-%d"]).unwrap_or_else(utc_today),
        branch: lines.next(),
        head: lines.next(),
    }
}

/// The `environment` section: written once at session start.
pub fn environment(cwd: &Path, env: &EnvState) -> String {
    let mut s = format!("Working directory: {}\nDate: {}", cwd.display(), env.date);
    if let (Some(b), Some(h)) = (&env.branch, &env.head) {
        let dirty = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0);
        s.push_str(&format!("\nGit: branch {b}, HEAD {h}"));
        if dirty > 0 {
            s.push_str(&format!(
                ", {dirty} changed file{} at session start",
                if dirty == 1 { "" } else { "s" }
            ));
        }
    }
    s
}

/// The reminder for a drift, or `None` when nothing the model knows changed.
pub fn reminder(old: &EnvState, new: &EnvState) -> Option<String> {
    let mut parts = Vec::new();
    if old.date != new.date {
        parts.push(format!("The date is now {}.", new.date));
    }
    match (&new.branch, &new.head) {
        (Some(b), Some(h)) if old.branch.as_ref() != Some(b) => {
            parts.push(format!("Git branch is now {b} (HEAD {h})."))
        }
        (_, Some(h)) if old.head.as_ref() != Some(h) => parts.push(format!("Git HEAD moved to {h}.")),
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Build every section. `environment` is passed in: it never changes after start.
pub fn build(src: &Sources, environment: &str) -> Sections {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok().filter(|s| !s.trim().is_empty());
    let mut out: Sections = Vec::new();
    let mut add = |name: &str, body: String| {
        let body = body.trim_end().to_string();
        let text = if name == "preamble" {
            body
        } else {
            format!("<{name}>\n{body}\n</{name}>")
        };
        out.push((name.into(), text));
    };
    match &src.agent {
        Some((role, true)) => add("preamble", role.clone()),
        _ => add(
            "preamble",
            read(src.home.join("SYSTEM.md")).unwrap_or_else(|| DEFAULT_PREAMBLE.into()),
        ),
    }
    if let Some((role, false)) = &src.agent {
        add("agent", role.clone());
    }
    if !src.tool_guidelines.is_empty() {
        let lines: Vec<String> = src.tool_guidelines.iter().map(|g| format!("- {g}")).collect();
        add("tool_guidelines", lines.join("\n"));
    }
    if let Some(a) = read(src.home.join("APPEND_SYSTEM.md")) {
        add("addendum", a);
    }
    if src.context_files {
        let files = context_files(&src.cwd, &src.home);
        if !files.is_empty() {
            let mut body = String::from("Project-specific instructions and guidelines:");
            for (p, text) in files {
                body.push_str(&format!(
                    "\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    p.display(),
                    text.trim_end()
                ));
            }
            add("project_context", body);
        }
    }
    let skills = crate::resources::render_skills(&crate::resources::skills(&src.skill_dirs).0);
    if !skills.is_empty() {
        add("skills", skills);
    }
    if let Some(m) = src.memory_index.as_ref().filter(|m| !m.is_empty()) {
        add("memory", m.clone());
    }
    add("environment", environment.into());
    if let Some(m) = &src.mode {
        add("mode", m.clone());
    }
    out
}

/// Context files: global first, then every folder from `/` down to `cwd`; one per
/// folder (`AGENTS.override.md` > `AGENTS.md` > `CLAUDE.md`), deduplicated by real
/// path so worktrees don't load the same file twice (L4.9).
pub fn context_files(cwd: &Path, home: &Path) -> Vec<(PathBuf, String)> {
    const NAMES: [&str; 5] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];
    let mut dirs = vec![home.to_path_buf()];
    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    dirs.extend(chain.into_iter().map(Path::to_path_buf));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in dirs {
        let Some(p) = NAMES.iter().map(|n| d.join(n)).find(|p| p.is_file()) else {
            continue;
        };
        let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if !seen.insert(real) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&p)
            && !text.trim().is_empty()
        {
            out.push((p, text));
        }
    }
    out
}

pub fn render(sections: &Sections) -> String {
    sections
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What changed from `prev` to `cur`, in `cur`'s order, removals last.
pub fn diff(prev: &Sections, cur: &Sections) -> Vec<(String, Option<String>)> {
    let mut patch: Vec<(String, Option<String>)> = cur
        .iter()
        .filter(|(n, t)| !prev.iter().any(|(pn, pt)| pn == n && pt == t))
        .map(|(n, t)| (n.clone(), Some(t.clone())))
        .collect();
    patch.extend(
        prev.iter()
            .filter(|(n, _)| !cur.iter().any(|(cn, _)| cn == n))
            .map(|(n, _)| (n.clone(), None)),
    );
    patch
}

/// The text of a delta system message (pi's wording).
pub fn render_update(patch: &[(String, Option<String>)]) -> String {
    patch
        .iter()
        .map(|(n, t)| match t {
            Some(t) => format!("Updated system prompt section \"{n}\":\n\n{t}"),
            None => format!("Removed system prompt section \"{n}\"."),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// UTC date as YYYY-MM-DD (civil-from-days, Howard Hinnant's algorithm); fallback when `date` is missing.
fn utc_today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_date_matches_system_date() {
        let out = Command::new("date").args(["-u", "+%Y-%m-%d"]).output().unwrap();
        assert_eq!(utc_today(), String::from_utf8_lossy(&out.stdout).trim());
    }

    #[test]
    fn context_files_order_precedence_and_dedupe() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let proj = d.path().join("proj");
        let sub = proj.join("sub");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(home.join("AGENTS.md"), "global").unwrap();
        std::fs::write(proj.join("AGENTS.md"), "proj").unwrap();
        std::fs::write(proj.join("CLAUDE.md"), "ignored: AGENTS.md wins").unwrap();
        std::fs::write(sub.join("AGENTS.override.md"), "override").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "ignored: override wins").unwrap();
        let got: Vec<String> = context_files(&sub, &home).into_iter().map(|(_, t)| t).collect();
        assert_eq!(got, ["global", "proj", "override"]);
        // A cwd that is the home folder doesn't load the same file twice.
        assert_eq!(context_files(&home, &home).len(), 1);
    }

    #[test]
    fn sections_diff_and_overrides() {
        let d = tempfile::tempdir().unwrap();
        let src = Sources {
            cwd: d.path().into(),
            home: d.path().join("h"),
            context_files: false,
            skill_dirs: vec![],
            tool_guidelines: vec![],
            agent: None,
            memory_index: None,
            mode: None,
        };
        let a = build(&src, "Date: x");
        assert_eq!(a[0], ("preamble".into(), DEFAULT_PREAMBLE.into()));
        assert_eq!(a.last().unwrap().1, "<environment>\nDate: x\n</environment>");
        std::fs::create_dir_all(&src.home).unwrap();
        std::fs::write(src.home.join("SYSTEM.md"), "Be a pirate.").unwrap();
        std::fs::write(src.home.join("APPEND_SYSTEM.md"), "Also rhyme.").unwrap();
        let b = build(&src, "Date: x");
        let patch = diff(&a, &b);
        assert_eq!(
            patch,
            [
                ("preamble".into(), Some("Be a pirate.".into())),
                ("addendum".into(), Some("<addendum>\nAlso rhyme.\n</addendum>".into()))
            ]
        );
        assert!(render_update(&patch).starts_with("Updated system prompt section \"preamble\":\n\nBe a pirate."));
        std::fs::remove_file(src.home.join("APPEND_SYSTEM.md")).unwrap();
        assert_eq!(diff(&b, &build(&src, "Date: x")), [("addendum".into(), None)]);
        assert!(diff(&b, &b).is_empty());
    }

    #[test]
    fn reminders_name_only_what_changed() {
        let s = |d: &str, b: &str, h: &str| EnvState {
            date: d.into(),
            branch: Some(b.into()),
            head: Some(h.into()),
        };
        assert_eq!(reminder(&s("1", "m", "a"), &s("1", "m", "a")), None);
        assert_eq!(
            reminder(&s("1", "m", "a"), &s("2", "m", "a")).unwrap(),
            "The date is now 2."
        );
        assert_eq!(
            reminder(&s("1", "m", "a"), &s("1", "m", "b")).unwrap(),
            "Git HEAD moved to b."
        );
        assert_eq!(
            reminder(&s("1", "m", "a"), &s("1", "f", "b")).unwrap(),
            "Git branch is now f (HEAD b)."
        );
    }
}
