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
    /// Git's per-worktree admin folder (`.git/worktrees/<name>` in a linked one).
    dir: PathBuf,
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
        dir: dir.into(),
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

/// `worktree.copy` default: untracked project files a worktree needs to work like the main checkout.
pub const COPY_DEFAULT: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    ".mcp.json",
    ".codegraph/",
    ".claude/",
    ".novacode/",
];

/// Where the copied paths are recorded: in git's admin folder, so the record never shows as untracked.
/// One line per `worktree.copy` entry copied: `<dir>/` for a folder, `<blob id> <file>` for a file
/// (older records: a bare path per copied file).
const COPIES: &str = "novacode-copies";

/// The untracked paths (relative, `/`-separated) in the worktree `cwd` is in that are
/// copies, not work: deleted on removal.
pub fn copies(cwd: &Path) -> Vec<String> {
    repo(cwd).map(|r| status(&r).copies).unwrap_or_default()
}

/// A recorded copy: a folder (tool state: anything untracked in it is not work), or a file
/// with the blob id it was copied with (edited or tracked since = work).
enum Copied {
    Dir(String),
    File(String, Option<String>),
}

fn read_copies(r: &Repo) -> Vec<Copied> {
    std::fs::read_to_string(r.dir.join(COPIES))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| match (l.strip_suffix('/'), l.split_once(' ')) {
            (Some(d), _) => Copied::Dir(d.to_string()),
            (_, Some((id, f))) if id.len() >= 40 && id.chars().all(|c| c.is_ascii_hexdigit()) => {
                Copied::File(f.to_string(), Some(id.to_string()))
            }
            _ => Copied::File(l.to_string(), None),
        })
        .collect()
}

/// The worktree's `git status`, split into untracked copies (safe to delete) and work.
struct Status {
    copies: Vec<String>,
    work: Vec<String>,
}

fn status(r: &Repo) -> Status {
    let rec = read_copies(r);
    let env = env_copies(r);
    // File copies whose contents changed since they were copied.
    let hashed: Vec<(&str, &str)> = rec
        .iter()
        .filter_map(|c| match c {
            Copied::File(f, Some(id)) if r.top.join(f).is_file() => Some((f.as_str(), id.as_str())),
            _ => None,
        })
        .collect();
    let mut edited: Vec<&str> = Vec::new();
    if !hashed.is_empty() {
        let mut args = vec!["hash-object", "--"];
        args.extend(hashed.iter().map(|(f, _)| *f));
        let now = git(&r.top, &args).unwrap_or_default();
        let now: Vec<&str> = now.lines().collect();
        for (i, (f, id)) in hashed.iter().enumerate() {
            if now.get(i) != Some(id) {
                edited.push(f);
            }
        }
    }
    let is_copy = |p: &str| {
        env.iter().any(|e| e == p)
            || rec.iter().any(|c| match c {
                Copied::Dir(d) => p.starts_with(&format!("{d}/")),
                Copied::File(f, _) => f == p && !edited.contains(&p),
            })
    };
    let (mut copies, mut work) = (Vec::new(), Vec::new());
    // `-uall` lists files one by one, so a folder holding copies and new work isn't one line.
    for l in git(&r.top, &["status", "--porcelain", "-uall", "--ignore-submodules=all"])
        .unwrap_or_default()
        .lines()
    {
        match l.strip_prefix("?? ") {
            Some(p) if is_copy(p) => copies.push(p.to_string()),
            _ => work.push(l.get(3..).unwrap_or(l).to_string()),
        }
    }
    // An edited copy git ignores is still work, unless it is tracked (committed) now.
    if !edited.is_empty() {
        let mut args = vec!["ls-files", "--"];
        args.extend(edited.iter().copied());
        let tracked = git(&r.top, &args).unwrap_or_default();
        for f in edited {
            if !work.iter().any(|w| w == f) && !tracked.lines().any(|t| t == f) {
                work.push(f.to_string());
            }
        }
    }
    Status { copies, work }
}

/// Copy each untracked file under the `list` entries from `main` to `wt`; returns the copied paths.
/// Never tracked files, `daemon.*` (codegraph's lock), sockets or symlinks.
fn copy_untracked(main: &Path, wt: &Path, list: &[String]) -> Vec<String> {
    fn walk(main: &Path, wt: &Path, rel: &str, tracked: &[&str], out: &mut Vec<String>) {
        let src = main.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&src) else {
            return;
        };
        if meta.is_dir() {
            // A nested repo would show as one `?? dir/` line, never a copy.
            if src.join(".git").exists() {
                return;
            }
            let mut names: Vec<_> = std::fs::read_dir(&src)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for n in names {
                walk(main, wt, &format!("{rel}/{n}"), tracked, out);
            }
            return;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let dst = wt.join(rel);
        if !meta.is_file() || name.starts_with("daemon.") || tracked.contains(&rel) || dst.exists() {
            return;
        }
        // std::fs::copy clones (reflinks) where the file system supports it.
        let ok = dst.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok()) && std::fs::copy(&src, &dst).is_ok();
        if ok {
            out.push(rel.to_string());
        }
    }
    let mut out = Vec::new();
    for entry in list {
        let rel = entry.trim_end_matches('/');
        // Only plain paths inside the repo.
        if rel.is_empty()
            || !Path::new(rel)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            continue;
        }
        let rel = rel.replace('\\', "/");
        let listed = git(main, &["ls-files", "-z", "--", &rel]).unwrap_or_default();
        let tracked: Vec<&str> = listed.split('\0').filter(|s| !s.is_empty()).collect();
        walk(main, wt, &rel, &tracked, &mut out);
    }
    out
}

pub struct EnterWorktree(pub Vec<String>);

#[derive(Deserialize, JsonSchema)]
struct EnterArgs {
    /// Short kebab-case name, e.g. fix-login-redirect.
    name: String,
    /// Commit or branch to start the new branch from (default: HEAD).
    base: Option<String>,
}

/// Create (or reuse) `<repo>/.worktrees/<name>` on branch `worktree-<name>`.
/// Works from inside another worktree too: the folder is always under the main checkout.
/// `copy` is the `worktree.copy` setting: untracked paths copied into a new worktree.
pub fn create(cwd: &Path, name: &str, copy: &[String]) -> Result<(PathBuf, String), String> {
    enter(
        &repo(cwd)?.main,
        &EnterArgs {
            name: name.into(),
            base: None,
        },
        copy,
    )
}

/// Like `create`, but never reuses (subagents): a taken name (folder, registered
/// worktree or branch, say from an earlier session) gets `-2`, `-3`…
pub fn create_new(cwd: &Path, name: &str, copy: &[String]) -> Result<(PathBuf, String), String> {
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
    enter(&main, &EnterArgs { name: free, base: None }, copy)
}

/// A detached checkout of `commit` at `path` (a review's scratch worktree), with the
/// `copy` list's untracked files from the main checkout. Removed with `git worktree remove --force`.
pub fn add_detached(cwd: &Path, path: &Path, commit: &str, copy: &[String]) -> Result<(), String> {
    let r = repo(cwd)?;
    git(
        &r.main,
        &["worktree", "add", "--detach", &path.to_string_lossy(), commit],
    )
    .map_err(|e| format!("git worktree add failed: {e}"))?;
    novacode_base::git::pin(path, &r.common);
    copy_untracked(&r.main, path, copy);
    Ok(())
}

fn enter(cwd: &Path, a: &EnterArgs, copy: &[String]) -> Result<(PathBuf, String), String> {
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
    let files = copy_untracked(&r.top, &path, copy);
    if !files.is_empty() {
        let mut record = String::new();
        for e in copy {
            let e = e.trim_end_matches('/').replace('\\', "/");
            if files.contains(&e) {
                let id = git(&path, &["hash-object", "--", &e])?;
                record += &format!("{id} {e}\n");
                copied.push(e);
            } else if files.iter().any(|f| f.starts_with(&format!("{e}/"))) {
                record += &format!("{e}/\n");
                copied.push(format!("{e}/"));
            }
        }
        let admin = git(&path, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
        std::fs::write(Path::new(&admin).join(COPIES), record).map_err(|e| e.to_string())?;
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
    // The copies made on entry aren't the user's changes.
    let dirty = status(&r).work.len();
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
    remove_with(h, &mut || {})
}

/// `remove`, running `before_git` just before git removes the worktree (tests: a late write).
fn remove_with(h: &Here, before_git: &mut dyn FnMut()) -> String {
    let (top, main) = (&h.r.top, &h.r.main);
    // Delete copies only when git will then remove the worktree.
    let st = status(&h.r);
    if !st.work.is_empty() {
        let shown: Vec<&str> = st.work.iter().take(5).map(String::as_str).collect();
        return format!(
            " Worktree NOT removed: {} uncommitted file(s) ({}).",
            st.work.len(),
            shown.join(", ")
        );
    }
    let delete = |copies: Vec<String>| {
        for c in copies {
            let f = top.join(&c);
            let _ = std::fs::remove_file(&f);
            // Folders the copies left empty; a folder holding anything else stays.
            for dir in f.ancestors().skip(1).take_while(|d| d.starts_with(top) && *d != top) {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
            }
        }
    };
    delete(st.copies);
    before_git();
    let gone = || git(main, &["worktree", "remove", &top.to_string_lossy()]);
    let mut res = gone();
    // A daemon wrote into a copied folder meanwhile: if that is all git saw, delete it and retry once.
    if res.is_err() {
        let st = status(&h.r);
        if st.work.is_empty() && !st.copies.is_empty() {
            delete(st.copies);
            res = gone();
        }
    }
    match res {
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
        "Create (or reuse) a git worktree at <repo>/.worktrees/<name> on branch worktree-<name> and move this session into it, so large changes don't touch the main checkout. Root .env files and the untracked project files in the worktree.copy setting are copied in."
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
            let (cwd, copy) = (ctx.cwd.clone(), self.0.clone());
            match tokio::task::spawn_blocking(move || enter(&cwd, &a, &copy)).await {
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
        let root = novacode_base::platform::canonicalize(d.path()).unwrap();
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
        let o = EnterWorktree(vec![]).execute(json!({"name": "fix-x"}), &c).await;
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
        assert!(
            EnterWorktree(vec![])
                .execute(json!({"name": "bad name"}), &c)
                .await
                .is_error
        );
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
        assert!(text(&EnterWorktree(vec![]).execute(json!({"name": "fix-x"}), &c).await).contains("existing"));
        // `-w` / `/worktree` from inside a worktree resolve against the main checkout.
        assert_eq!(create(&wt, "fix-x", &[]).unwrap().0, wt);
        // Subagents never reuse one: the taken name gets a suffix.
        assert_eq!(
            create_new(&wt, "fix-x", &[]).unwrap().0,
            root.join(".worktrees/fix-x-2")
        );
        assert_eq!(here(&wt).unwrap().name().as_deref(), Some("fix-x"));
        assert!(here(&root).is_err());
        let l = list(&wt).unwrap();
        assert!(
            // git prints `C:/…` on Windows.
            l.replace('\\', "/").contains(&format!(
                "* {}  [worktree-fix-x]  0 uncommitted",
                novacode_base::platform::slash_path(&wt)
            )),
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

    #[test]
    fn copies_untracked_project_files_and_ignores_them() {
        let d = tempfile::tempdir().unwrap();
        let root = novacode_base::platform::canonicalize(d.path()).unwrap();
        let w = |p: &str, s: &str| {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, s).unwrap();
        };
        git(&root, &["init", "-q"]).unwrap();
        // `.claude/` is half tracked; `.codegraph/` is ignored, AGENTS.md is untracked but not ignored.
        w("CLAUDE.md", "tracked\n");
        w(".claude/settings.json", "{}\n");
        w(".gitignore", ".codegraph/\n");
        git(&root, &["add", "."]).unwrap();
        git(
            &root,
            &["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "-m", "x"],
        )
        .unwrap();
        w("AGENTS.md", "rules\n");
        w(".claude/local.md", "mine\n");
        w(".claude/settings.json", "{\"changed\": 1}\n");
        w(".codegraph/codegraph.db", "index");
        w(".codegraph/sub/x.bin", "x");
        w(".codegraph/daemon.pid", "123");
        #[cfg(unix)]
        let _sock = std::os::unix::net::UnixListener::bind(root.join(".codegraph/codegraph.sock")).unwrap();
        let list: Vec<String> = COPY_DEFAULT.iter().map(|s| s.to_string()).collect();
        let (wt, msg) = create(&root, "t", &list).unwrap();
        assert!(msg.contains("AGENTS.md") && msg.contains(".codegraph/"), "{msg}");
        let read = |p: &str| std::fs::read_to_string(wt.join(p)).ok();
        assert_eq!(read("AGENTS.md").as_deref(), Some("rules\n"));
        assert_eq!(read(".codegraph/codegraph.db").as_deref(), Some("index"));
        assert_eq!(read(".codegraph/sub/x.bin").as_deref(), Some("x"));
        assert_eq!(read(".claude/local.md").as_deref(), Some("mine\n"));
        // Tracked files come from the branch, not the main checkout's edits.
        assert_eq!(read(".claude/settings.json").as_deref(), Some("{}\n"));
        assert!(!wt.join(".codegraph/daemon.pid").exists());
        assert!(!wt.join(".codegraph/codegraph.sock").exists());
        // The record sits in git's admin folder, not in the worktree.
        let rec = std::fs::read_to_string(root.join(".git/worktrees/t/novacode-copies")).unwrap();
        assert!(rec.lines().any(|l| l.ends_with(" AGENTS.md")), "{rec}");
        assert!(rec.lines().any(|l| l == ".codegraph/"), "{rec}");
        assert!(!rec.contains("settings.json"), "{rec}");
        assert!(!wt.join("novacode-copies").exists());
        // The untracked copies git shows (`.codegraph/` is ignored).
        let mut got = copies(&wt);
        got.sort();
        assert_eq!(got, [".claude/local.md", "AGENTS.md"]);
        // Only copies: clean, even after a file in a copied folder changed (tool state).
        assert_eq!(here(&wt).unwrap().dirty, 0);
        std::fs::write(wt.join(".claude/local.md"), "edited\n").unwrap();
        std::fs::write(wt.join(".claude/new.md"), "new\n").unwrap();
        assert_eq!(here(&wt).unwrap().dirty, 0);
        // Real work still counts.
        std::fs::write(wt.join("new.md"), "new\n").unwrap();
        assert_eq!(here(&wt).unwrap().dirty, 1);
        std::fs::remove_file(wt.join("new.md")).unwrap();
        let out = remove(&here(&wt).unwrap());
        assert!(out.contains("Worktree and branch worktree-t removed"), "{out}");
        assert!(!wt.exists());
        // The main checkout's files are untouched.
        assert!(root.join("AGENTS.md").exists() && root.join(".codegraph/daemon.pid").exists());
    }

    /// A repo with one commit, AGENTS.md and `.codegraph/` untracked (not ignored), and a
    /// worktree `t` made with the default copy list.
    fn copied_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = novacode_base::platform::canonicalize(d.path()).unwrap();
        git(&root, &["init", "-q"]).unwrap();
        std::fs::write(root.join("README"), "x\n").unwrap();
        git(&root, &["add", "."]).unwrap();
        git(
            &root,
            &["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "-m", "x"],
        )
        .unwrap();
        std::fs::write(root.join("AGENTS.md"), "rules\n").unwrap();
        std::fs::create_dir_all(root.join(".codegraph")).unwrap();
        std::fs::write(root.join(".codegraph/codegraph.db"), "index").unwrap();
        let list: Vec<String> = COPY_DEFAULT.iter().map(|s| s.to_string()).collect();
        let (wt, _) = create(&root, "t", &list).unwrap();
        (d, root, wt)
    }

    #[test]
    fn an_unchanged_copy_is_removed_with_the_worktree() {
        let (_d, _root, wt) = copied_worktree();
        assert_eq!(here(&wt).unwrap().dirty, 0);
        let out = remove(&here(&wt).unwrap());
        assert!(out.contains("Worktree and branch worktree-t removed"), "{out}");
        assert!(!wt.exists());
    }

    #[test]
    fn an_edited_copy_counts_as_work_and_is_kept() {
        let (_d, _root, wt) = copied_worktree();
        std::fs::write(wt.join("AGENTS.md"), "edited rules\n").unwrap();
        assert_eq!(here(&wt).unwrap().dirty, 1);
        let out = remove(&here(&wt).unwrap());
        assert!(out.contains("NOT removed"), "{out}");
        assert_eq!(std::fs::read_to_string(wt.join("AGENTS.md")).unwrap(), "edited rules\n");
    }

    #[test]
    fn a_now_tracked_copy_is_kept() {
        let (_d, root, wt) = copied_worktree();
        // Staged: work, never deleted.
        git(&wt, &["add", "AGENTS.md"]).unwrap();
        assert_eq!(here(&wt).unwrap().dirty, 1);
        let out = remove(&here(&wt).unwrap());
        assert!(out.contains("NOT removed"), "{out}");
        assert_eq!(std::fs::read_to_string(wt.join("AGENTS.md")).unwrap(), "rules\n");
        // Committed: removal leaves the file alone, so git removes the worktree and the branch keeps it.
        git(
            &wt,
            &["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "-m", "y"],
        )
        .unwrap();
        assert_eq!(here(&wt).unwrap().dirty, 0);
        let out = remove(&here(&wt).unwrap());
        assert!(out.contains("branch worktree-t kept"), "{out}");
        assert_eq!(git(&root, &["show", "worktree-t:AGENTS.md"]).unwrap(), "rules");
    }

    /// C9: a copied folder is tool state (codegraph writes into `.codegraph/`).
    #[test]
    fn a_copied_folder_is_tool_state_whatever_is_written_in_it() {
        let (_d, _root, wt) = copied_worktree();
        std::fs::write(wt.join(".codegraph/codegraph.db"), "changed index").unwrap();
        std::fs::write(wt.join(".codegraph/daemon.log"), "log").unwrap();
        assert_eq!(here(&wt).unwrap().dirty, 0);
        let out = remove(&here(&wt).unwrap());
        assert!(out.contains("Worktree and branch worktree-t removed"), "{out}");
        assert!(!wt.exists());
    }

    /// R22: a daemon writes into a copied folder after its copies were deleted, before git removes.
    #[test]
    fn a_file_reappearing_in_a_copied_folder_does_not_block_removal() {
        let (_d, _root, wt) = copied_worktree();
        let mut once = true;
        let out = remove_with(&here(&wt).unwrap(), &mut || {
            if std::mem::take(&mut once) {
                std::fs::create_dir_all(wt.join(".codegraph")).unwrap();
                std::fs::write(wt.join(".codegraph/daemon.log"), "late").unwrap();
            }
        });
        assert!(out.contains("Worktree and branch worktree-t removed"), "{out}");
        assert!(!wt.exists());
    }

    /// C2: a nested git repo inside a copied folder is not copied (it would show as `?? dir/`).
    #[test]
    fn a_nested_repo_is_not_copied() {
        let d = tempfile::tempdir().unwrap();
        let root = novacode_base::platform::canonicalize(d.path()).unwrap();
        git(&root, &["init", "-q"]).unwrap();
        git(
            &root,
            &[
                "-c",
                "user.email=a@b",
                "-c",
                "user.name=a",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".novacode/plug")).unwrap();
        git(&root.join(".novacode/plug"), &["init", "-q"]).unwrap();
        std::fs::write(root.join(".novacode/plug/x"), "x").unwrap();
        std::fs::write(root.join(".novacode/settings.json"), "{}").unwrap();
        let (wt, _) = create(&root, "t", &[".novacode/".into()]).unwrap();
        assert!(wt.join(".novacode/settings.json").exists());
        assert!(!wt.join(".novacode/plug").exists());
        assert_eq!(here(&wt).unwrap().dirty, 0);
    }
}
