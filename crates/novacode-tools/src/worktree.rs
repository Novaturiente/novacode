//! `enter_worktree` / `exit_worktree` (L5.22): move the session into
//! `<repo>/.worktrees/<name>` on branch `worktree-<name>`, and back. The cwd
//! change travels as `details.set_cwd`, which the agent applies after the call.

use crate::ask::Asker;
use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use novacode_types::{Choice, Question};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = novacode_base::git::command_in(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

struct Repo {
    top: PathBuf,
    common: PathBuf,
    /// Inside a linked worktree, not the main checkout.
    linked: bool,
    main: PathBuf,
}

fn repo(cwd: &Path) -> Result<Repo, String> {
    let r = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .map_err(|_| format!("{} is not in a git repository", cwd.display()))?;
    let mut it = r.lines();
    let (top, dir, common) = (
        it.next().unwrap_or(""),
        it.next().unwrap_or(""),
        it.next().unwrap_or(""),
    );
    let main = git(cwd, &["worktree", "list", "--porcelain"])?
        .lines()
        .find_map(|l| l.strip_prefix("worktree ").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(top));
    Ok(Repo {
        top: top.into(),
        common: common.into(),
        linked: dir != common,
        main,
    })
}

fn set_cwd(text: String, cwd: &Path) -> ToolOutput {
    ToolOutput {
        details: Some(json!({ "set_cwd": cwd })),
        ..ToolOutput::text(text)
    }
}

/// Untracked root `.env*` files in the worktree that still match the main checkout's.
fn env_copies(r: &Repo) -> Vec<String> {
    std::fs::read_dir(&r.top)
        .into_iter()
        .flatten()
        .flatten()
        .map(|f| f.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".env"))
        .filter(|n| {
            let a = std::fs::read(r.top.join(n)).ok();
            a.is_some() && a == std::fs::read(r.main.join(n)).ok()
        })
        .collect()
}

pub struct EnterWorktree;

#[derive(Deserialize, JsonSchema)]
struct EnterArgs {
    /// Short kebab-case name, e.g. fix-login-redirect.
    name: String,
    /// Commit or branch to start the new branch from (default: HEAD).
    base: Option<String>,
}

/// Create (or reuse) `<repo>/.worktrees/<name>` on branch `worktree-<name>`.
/// Works from inside another worktree too: the folder is always under the main checkout.
pub fn create(cwd: &Path, name: &str) -> Result<(PathBuf, String), String> {
    enter(
        &repo(cwd)?.main,
        &EnterArgs {
            name: name.into(),
            base: None,
        },
    )
}

/// Like `create`, but never reuses (subagents): a taken name (folder, registered
/// worktree or branch, say from an earlier session) gets `-2`, `-3`…
pub fn create_new(cwd: &Path, name: &str) -> Result<(PathBuf, String), String> {
    let main = repo(cwd)?.main;
    let listed = git(&main, &["worktree", "list", "--porcelain"])?;
    let taken = |n: &str| {
        let path = main.join(".worktrees").join(n);
        path.exists()
            || listed
                .lines()
                .any(|l| l.strip_prefix("worktree ").is_some_and(|p| Path::new(p) == path))
            || git(
                &main,
                &["show-ref", "--verify", "-q", &format!("refs/heads/worktree-{n}")],
            )
            .is_ok()
    };
    let mut free = name.to_string();
    let mut i = 1;
    while taken(&free) {
        i += 1;
        free = format!("{name}-{i}");
    }
    enter(&main, &EnterArgs { name: free, base: None })
}

fn enter(cwd: &Path, a: &EnterArgs) -> Result<(PathBuf, String), String> {
    if a.name.is_empty() || !a.name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(format!("bad worktree name `{}`: letters, digits, . _ - only", a.name));
    }
    let r = repo(cwd)?;
    if r.linked {
        return Err(format!(
            "already in worktree {}; call exit_worktree first",
            r.top.display()
        ));
    }
    let path = r.top.join(".worktrees").join(&a.name);
    let branch = format!("worktree-{}", a.name);
    let registered = git(&r.top, &["worktree", "list", "--porcelain"])?
        .lines()
        .any(|l| l.strip_prefix("worktree ").is_some_and(|p| Path::new(p) == path));
    if registered {
        // L6.5: novacode's git commands in the worktree use the main checkout's common dir.
        novacode_base::git::pin(&path, &r.common);
        return Ok((
            path,
            format!(
                "entered existing worktree {} (branch {branch})",
                r.top.join(".worktrees").join(&a.name).display()
            ),
        ));
    }
    if path.exists() {
        return Err(format!("{} exists but is not a git worktree", path.display()));
    }
    // Keep the folder out of `git status` without touching the user's .gitignore.
    if git(&r.top, &["check-ignore", "-q", ".worktrees/x"]).is_err() {
        let ex = r.common.join("info").join("exclude");
        let _ = std::fs::create_dir_all(ex.parent().unwrap_or(&r.common));
        let mut cur = std::fs::read_to_string(&ex).unwrap_or_default();
        if !cur.ends_with('\n') && !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(".worktrees/\n");
        std::fs::write(&ex, cur).map_err(|e| e.to_string())?;
    }
    let p = path.to_string_lossy().to_string();
    let exists = git(&r.top, &["show-ref", "--verify", "-q", &format!("refs/heads/{branch}")]).is_ok();
    if exists {
        git(&r.top, &["worktree", "add", &p, &branch])
    } else {
        git(
            &r.top,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &p,
                a.base.as_deref().unwrap_or("HEAD"),
            ],
        )
    }
    .map_err(|e| format!("git worktree add failed: {e}"))?;
    novacode_base::git::pin(&path, &r.common);
    // Root .env* files are untracked by design; the worktree needs them to run.
    let mut copied = Vec::new();
    for f in std::fs::read_dir(&r.top).into_iter().flatten().flatten() {
        let n = f.file_name().to_string_lossy().to_string();
        if n.starts_with(".env")
            && f.path().is_file()
            && !path.join(&n).exists()
            && std::fs::copy(f.path(), path.join(&n)).is_ok()
        {
            copied.push(n);
        }
    }
    let mut msg = format!("created worktree {} on branch {branch}", path.display());
    if !copied.is_empty() {
        msg += &format!("; copied {}", copied.join(", "));
    }
    Ok((path, msg))
}

/// The linked worktree `cwd` is in: what leaving it would lose.
pub struct Here {
    r: Repo,
    pub branch: String,
    pub dirty: usize,
    pub ahead: String,
}

impl Here {
    pub fn top(&self) -> &Path {
        &self.r.top
    }
    pub fn main(&self) -> &Path {
        &self.r.main
    }
    /// `<name>` when this is novacode's own `<main>/.worktrees/<name>`, so `-w <name>` reopens it.
    pub fn name(&self) -> Option<String> {
        (self.r.top.parent()? == self.r.main.join(".worktrees"))
            .then(|| self.r.top.file_name().map(|n| n.to_string_lossy().into_owned()))?
    }
    pub fn summary(&self) -> String {
        format!(
            "{} ({}): {} uncommitted file(s), {} commit(s) not in the main checkout",
            self.r.top.display(),
            if self.branch.is_empty() {
                "detached"
            } else {
                &self.branch
            },
            self.dirty,
            self.ahead
        )
    }
}

pub fn here(cwd: &Path) -> Result<Here, String> {
    let r = repo(cwd)?;
    if !r.linked {
        return Err("not inside a linked worktree".into());
    }
    let branch = git(&r.top, &["branch", "--show-current"]).unwrap_or_default();
    // The .env copies made on entry aren't the user's changes.
    let copies = env_copies(&r);
    let dirty = git(&r.top, &["status", "--porcelain", "--ignore-submodules=all"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !copies.iter().any(|c| l.trim_start_matches("?? ") == c))
        .count();
    let main_head = git(&r.main, &["rev-parse", "HEAD"])?;
    let ahead = git(&r.top, &["rev-list", "--count", &format!("{main_head}..HEAD")]).unwrap_or_default();
    Ok(Here {
        r,
        branch,
        dirty,
        ahead,
    })
}

/// Remove the worktree (git refuses uncommitted changes; an unmerged branch is kept).
/// Returns what happened, starting with a space.
pub fn remove(h: &Here) -> String {
    let (top, main) = (&h.r.top, &h.r.main);
    for c in env_copies(&h.r) {
        let _ = std::fs::remove_file(top.join(c));
    }
    match git(main, &["worktree", "remove", &top.to_string_lossy()]) {
        Err(e) => format!(" Worktree NOT removed: {e}"),
        Ok(_) if h.branch.is_empty() => " Worktree removed.".into(),
        Ok(_) if git(main, &["branch", "-d", &h.branch]).is_ok() => {
            format!(" Worktree and branch {} removed.", h.branch)
        }
        Ok(_) => format!(" Worktree removed; branch {} kept (not merged).", h.branch),
    }
}

/// `/worktree`: every worktree of the repo with its branch and state; `*` = `cwd`'s.
pub fn list(cwd: &Path) -> Result<String, String> {
    let r = repo(cwd)?;
    let mut out = String::new();
    for block in git(&r.main, &["worktree", "list", "--porcelain"])?.split("\n\n") {
        let Some(path) = block.lines().find_map(|l| l.strip_prefix("worktree ")) else {
            continue;
        };
        let branch = block
            .lines()
            .find_map(|l| l.strip_prefix("branch refs/heads/"))
            .unwrap_or("detached");
        let p = Path::new(path);
        let state = match here(p) {
            Ok(h) => format!("{} uncommitted, {} ahead", h.dirty, h.ahead),
            Err(_) if p == r.main => "main checkout".into(),
            Err(e) => e,
        };
        let mark = if p == r.top { "*" } else { " " };
        out += &format!("{mark} {path}  [{branch}]  {state}\n");
    }
    out += "\n/worktree <name> enters or creates one · /worktree exit leaves · /worktree remove leaves and removes";
    Ok(out)
}

impl Tool for EnterWorktree {
    fn name(&self) -> &str {
        "enter_worktree"
    }
    /// Rarely needed: found through tool_search, so it costs no tokens per call.
    fn exposure(&self) -> crate::Exposure {
        crate::Exposure::Deferred
    }
    fn description(&self) -> &str {
        "Create (or reuse) a git worktree at <repo>/.worktrees/<name> on branch worktree-<name> and move this session into it, so large changes don't touch the main checkout. Root .env files are copied in."
    }
    fn schema(&self) -> Value {
        schema_for::<EnterArgs>()
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: EnterArgs = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let cwd = ctx.cwd.clone();
            match tokio::task::spawn_blocking(move || enter(&cwd, &a)).await {
                Ok(Ok((path, msg))) => set_cwd(
                    format!("{msg}. The working directory is now {}.", path.display()),
                    &path,
                ),
                Ok(Err(e)) => ToolOutput::error(e),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        })
    }
}

/// Asks keep or remove when someone can answer; keeps it otherwise.
pub struct ExitWorktree(pub Option<Arc<dyn Asker>>);

#[derive(Deserialize, JsonSchema)]
struct ExitArgs {}

impl Tool for ExitWorktree {
    fn name(&self) -> &str {
        "exit_worktree"
    }
    /// Rarely needed: found through tool_search, so it costs no tokens per call.
    fn exposure(&self) -> crate::Exposure {
        crate::Exposure::Deferred
    }
    fn description(&self) -> &str {
        "Leave the current git worktree and move this session back to the main checkout. The user is asked whether to keep or remove it (removal refuses uncommitted changes and keeps unmerged branches). Commit your work first."
    }
    fn schema(&self) -> Value {
        schema_for::<ExitArgs>()
    }
    fn sequential(&self) -> bool {
        true
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, _args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let cwd = ctx.cwd.clone();
            let info = tokio::task::spawn_blocking(move || here(&cwd)).await;
            let h = match info {
                Ok(Ok(x)) => x,
                Ok(Err(e)) => return ToolOutput::error(e),
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let remove = match &self.0 {
                None => false,
                Some(asker) => {
                    let dirty = h.dirty;
                    let q = Question {
                        question: format!("Leave {}. Keep or remove the worktree?", h.summary()),
                        header: Some("Worktree".into()),
                        options: vec![
                            Choice {
                                label: "Keep worktree".into(),
                                description: Some("Leave it on disk to come back to.".into()),
                                preview: None,
                                recommended: dirty > 0,
                            },
                            Choice {
                                label: "Remove worktree".into(),
                                description: Some("Delete the folder; an unmerged branch is kept.".into()),
                                preview: None,
                                recommended: dirty == 0,
                            },
                        ],
                        multi: false,
                    };
                    match asker.ask(vec![q]).await {
                        None => return ToolOutput::text("The user cancelled: still in the worktree."),
                        Some(r) => r
                            .first()
                            .is_some_and(|r| r.chosen.iter().any(|c| c == "Remove worktree")),
                    }
                }
            };
            let main = h.r.main.clone();
            let note = if remove {
                tokio::task::spawn_blocking(move || self::remove(&h))
                    .await
                    .unwrap_or_default()
            } else {
                format!(" Worktree kept at {}.", h.r.top.display())
            };
            set_cwd(format!("Back in the main checkout {}.{note}", main.display()), &main)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};

    #[tokio::test]
    async fn enter_and_exit() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let sh = |a: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(a)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        sh(&["init", "-q"]);
        sh(&[
            "-c",
            "user.email=a@b",
            "-c",
            "user.name=a",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        std::fs::write(root.join(".env"), "K=1\n").unwrap();
        let c = ctx(&root);
        let o = EnterWorktree.execute(json!({"name": "fix-x"}), &c).await;
        let wt = root.join(".worktrees/fix-x");
        assert!(!o.is_error, "{}", text(&o));
        let set = o.details.as_ref().unwrap()["set_cwd"].as_str().unwrap().to_string();
        assert_eq!(
            novacode_base::platform::canonicalize(set).unwrap(),
            novacode_base::platform::canonicalize(&wt).unwrap()
        );
        assert_eq!(std::fs::read_to_string(wt.join(".env")).unwrap(), "K=1\n");
        // The folder is excluded, so the main checkout stays clean.
        assert_eq!(git(&root, &["status", "--porcelain"]).unwrap(), "?? .env");
        assert!(EnterWorktree.execute(json!({"name": "bad name"}), &c).await.is_error);
        // From inside: exit with nobody to ask keeps it.
        let o = ExitWorktree(None).execute(json!({}), &ctx(&wt)).await;
        assert!(text(&o).contains("Worktree kept"), "{}", text(&o));
        let back = o.details.as_ref().unwrap()["set_cwd"].as_str().unwrap().to_string();
        assert_eq!(
            std::fs::canonicalize(back).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        assert!(wt.exists());
        // Entering again reuses it.
        assert!(text(&EnterWorktree.execute(json!({"name": "fix-x"}), &c).await).contains("existing"));
        // `-w` / `/worktree` from inside a worktree resolve against the main checkout.
        assert_eq!(create(&wt, "fix-x").unwrap().0, wt);
        // Subagents never reuse one: the taken name gets a suffix.
        assert_eq!(create_new(&wt, "fix-x").unwrap().0, root.join(".worktrees/fix-x-2"));
        assert_eq!(here(&wt).unwrap().name().as_deref(), Some("fix-x"));
        assert!(here(&root).is_err());
        let l = list(&wt).unwrap();
        assert!(
            l.contains(&format!("* {}  [worktree-fix-x]  0 uncommitted", wt.display())),
            "{l}"
        );
        // Removal: the unchanged .env copy doesn't count as work and doesn't block it.
        struct Remove;
        impl Asker for Remove {
            fn ask(&self, q: Vec<Question>) -> BoxFuture<'_, Option<Vec<novacode_types::Reply>>> {
                assert!(q[0].question.contains("0 uncommitted"), "{}", q[0].question);
                Box::pin(async move {
                    Some(vec![novacode_types::Reply {
                        question: String::new(),
                        chosen: vec!["Remove worktree".into()],
                        text: None,
                        preview: None,
                    }])
                })
            }
        }
        let o = ExitWorktree(Some(Arc::new(Remove))).execute(json!({}), &ctx(&wt)).await;
        assert!(
            text(&o).contains("Worktree and branch worktree-fix-x removed"),
            "{}",
            text(&o)
        );
        assert!(!wt.exists());
    }
}
