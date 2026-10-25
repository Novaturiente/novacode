//! File checkpoints for rewind (L4.15).
//!
//! In a git repo, each prompt snapshots the whole worktree (tracked changes,
//! deletions and untracked files up to [`MAX_UNTRACKED`]) into the repo's own
//! object store through a temporary index. The user's index, HEAD and refs are
//! never touched. The trees are unreferenced, so `git gc` may prune them after
//! its grace period (about two weeks); rewind then says so.
//!
//! Outside git there is no folder snapshot: a shadow repo under
//! `<home>/checkpoints/` keeps the content of each file before novacode's
//! edit/write tools first change it, so only those changes rewind.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// A snapshot's tree id and the untracked files it skipped.
pub type Snapshot = Result<(String, Vec<String>), String>;

/// Untracked files above this are skipped (and listed on rewind).
pub const MAX_UNTRACKED: u64 = 1 << 20;

#[derive(Debug, Clone)]
pub struct Repo {
    /// Worktree root.
    pub root: PathBuf,
    /// `Some` = shadow store outside git (pre-images only).
    pub shadow: Option<PathBuf>,
}

fn git(repo: &Repo, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    // The repo's own commands run pinned (L6.5): a planted `commondir` is ignored.
    let mut c = match &repo.shadow {
        Some(dir) => {
            let mut c = novacode_base::git::bookkeeping();
            c.arg("--git-dir").arg(dir).arg("--work-tree").arg(&repo.root);
            c
        }
        None => novacode_base::git::bookkeeping_in(&repo.root),
    };
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let out = c
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The repo of `cwd`, or a shadow store for it under `home`.
pub fn resolve(cwd: &Path, home: &Path) -> Option<Repo> {
    let top = novacode_base::git::bookkeeping_in(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    if let Some(root) = top {
        return Some(Repo { root, shadow: None });
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in cwd.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = home.join("checkpoints").join(format!("{h:016x}"));
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir).ok()?;
        novacode_base::git::bookkeeping()
            .args(["init", "--bare", "-q"])
            .arg(&dir)
            .status()
            .ok()
            .filter(|s| s.success())?;
    }
    Some(Repo {
        root: cwd.to_path_buf(),
        shadow: Some(dir),
    })
}

/// A temporary index file, removed on drop.
struct TempIndex(PathBuf);

impl TempIndex {
    fn fresh() -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("novacode-index-{}-{n}", std::process::id())))
    }

    fn new(repo: &Repo) -> Self {
        let t = Self::fresh();
        let p = t.0.clone();
        // Seeding from the real index lets git reuse its stat cache instead of rehashing every file.
        if repo.shadow.is_none()
            && let Ok(rel) = git(repo, &["rev-parse", "--git-path", "index"], None)
        {
            let _ = std::fs::copy(repo.root.join(rel.trim()), &p);
        }
        t
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Snapshot: the tree id, and untracked files skipped for size. In a shadow
/// store only `paths` (files novacode edited) are snapshot.
pub fn snapshot(repo: &Repo, paths: &[String]) -> Snapshot {
    let idx = TempIndex::new(repo);
    let i = Some(idx.0.as_path());
    let mut skipped = Vec::new();
    if repo.shadow.is_some() {
        let have: Vec<&str> = paths
            .iter()
            .filter(|p| repo.root.join(p).exists())
            .map(String::as_str)
            .collect();
        for batch in have.chunks(200) {
            let mut args = vec!["add", "-A", "--"];
            args.extend(batch);
            git(repo, &args, i)?;
        }
    } else {
        git(repo, &["add", "-u"], i)?;
        let out = git(repo, &["ls-files", "-z", "-o", "--exclude-standard"], i)?;
        let mut small = Vec::new();
        for p in out.split('\0').filter(|p| !p.is_empty()) {
            match std::fs::metadata(repo.root.join(p)) {
                Ok(m) if m.len() > MAX_UNTRACKED => skipped.push(p.to_string()),
                Ok(_) => small.push(p),
                Err(_) => {}
            }
        }
        for batch in small.chunks(200) {
            let mut args = vec!["add", "--"];
            args.extend(batch);
            git(repo, &args, i)?;
        }
    }
    Ok((git(repo, &["write-tree"], i)?.trim().to_string(), skipped))
}

/// Store a file's current content (shadow stores, before novacode first edits it).
/// `None` = the file doesn't exist yet.
pub fn pre_image(repo: &Repo, path: &str) -> Result<Option<String>, String> {
    let abs = repo.root.join(path);
    if !abs.exists() {
        return Ok(None);
    }
    Ok(Some(
        git(repo, &["hash-object", "-w", "--", &abs.to_string_lossy()], None)?
            .trim()
            .to_string(),
    ))
}

/// `tree` with some paths set to a blob or removed (`None`).
pub fn with_patches(repo: &Repo, tree: &str, patches: &[(String, Option<String>)]) -> Result<String, String> {
    let idx = TempIndex::fresh();
    let i = Some(idx.0.as_path());
    git(repo, &["read-tree", tree], i)?;
    for (p, blob) in patches {
        match blob {
            Some(b) => git(
                repo,
                &["update-index", "--add", "--cacheinfo", &format!("100644,{b},{p}")],
                i,
            )?,
            None => git(repo, &["update-index", "--force-remove", "--", p], i)?,
        };
    }
    Ok(git(repo, &["write-tree"], i)?.trim().to_string())
}

/// Whether a snapshot still exists (`git gc` prunes unreferenced trees).
pub fn exists(repo: &Repo, tree: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{tree}^{{tree}}")], None).is_ok()
}

/// Make the files match `target`: files that are only in the current state are
/// deleted, the rest written. Returns the tree of the state before, for undo,
/// and the number of files changed.
pub fn apply(repo: &Repo, target: &str, tracked: &[String]) -> Result<(String, usize), String> {
    let (cur, _) = snapshot(repo, tracked)?;
    let diff = git(
        repo,
        &["diff-tree", "-r", "-z", "--name-status", "--no-renames", &cur, target],
        None,
    )?;
    let tok: Vec<&str> = diff.split('\0').filter(|t| !t.is_empty()).collect();
    let mut write = Vec::new();
    let mut n = 0;
    for pair in tok.chunks(2) {
        let [status, path] = pair else { continue };
        n += 1;
        if *status == "D" {
            let _ = std::fs::remove_file(repo.root.join(path));
        } else {
            write.push(*path);
        }
    }
    if !write.is_empty() {
        let idx = TempIndex::fresh();
        let i = Some(idx.0.as_path());
        git(repo, &["read-tree", target], i)?;
        for batch in write.chunks(200) {
            let mut args = vec!["checkout-index", "-f", "--"];
            args.extend(batch);
            git(repo, &args, i)?;
        }
    }
    Ok((cur, n))
}

/// What a file rewind did.
#[derive(Debug, Clone)]
pub struct Rewound {
    /// The state before the rewind, for undo.
    pub undo: String,
    pub changed: usize,
    /// Untracked files the checkpoint skipped for size: left as they are.
    pub skipped: Vec<String>,
}

/// Files back to how they were before `prompt` (an entry id on the branch).
pub fn rewind(store: &crate::session::Store, repo: &Repo, prompt: &str) -> Result<Rewound, String> {
    use crate::session::EntryKind;
    let path = store.branch();
    let i = path
        .iter()
        .position(|e| e.id == prompt)
        .ok_or("that prompt isn't on this branch")?;
    let (ci, tree, skipped) = path[i..]
        .iter()
        .enumerate()
        .find_map(|(k, e)| match &e.kind {
            EntryKind::Checkpoint { tree, skipped } => Some((i + k, tree.clone(), skipped.clone())),
            _ => None,
        })
        .ok_or("no file checkpoint for that prompt")?;
    if !exists(repo, &tree) {
        return Err(
            "that checkpoint's snapshot is gone: git gc prunes unreferenced snapshots after about two weeks".into(),
        );
    }
    let mut tracked: Vec<String> = Vec::new();
    let mut patches: Vec<(String, Option<String>)> = Vec::new();
    for (k, e) in path.iter().enumerate() {
        if let EntryKind::FileBefore { path: p, blob } = &e.kind {
            if !tracked.contains(p) {
                tracked.push(p.clone());
            }
            if k > ci && !patches.iter().any(|(x, _)| x == p) {
                patches.push((p.clone(), blob.clone()));
            }
        }
    }
    let target = if patches.is_empty() {
        tree
    } else {
        with_patches(repo, &tree, &patches)?
    };
    let (undo, changed) = apply(repo, &target, &tracked)?;
    Ok(Rewound { undo, changed, skipped })
}

/// `rel` (relative to the repo root) as it was when the session in `file` began:
/// the first checkpoint's copy, or outside git the copy kept before the first edit.
/// `Ok(None)` = it didn't exist then (or nothing was kept for it).
pub fn session_base(file: &Path, repo: &Repo, rel: &str) -> Result<Option<String>, String> {
    use crate::session::{Entry, EntryKind};
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    for e in text
        .lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
    {
        match e.kind {
            EntryKind::Checkpoint { tree, .. } if repo.shadow.is_none() => {
                return Ok(git(repo, &["show", &format!("{tree}:{rel}")], None).ok());
            }
            EntryKind::FileBefore { path, blob } if path == rel => {
                return match blob {
                    Some(b) => git(repo, &["cat-file", "blob", &b], None).map(Some),
                    None => Ok(None),
                };
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Undo a rewind: files back to `undo`.
pub fn undo(store: &crate::session::Store, repo: &Repo, undo: &str) -> Result<usize, String> {
    let mut tracked: Vec<String> = Vec::new();
    for e in store.entries() {
        if let crate::session::EntryKind::FileBefore { path, .. } = &e.kind
            && !tracked.contains(path)
        {
            tracked.push(path.clone());
        }
    }
    Ok(apply(repo, undo, &tracked)?.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn snapshot_and_rewind_in_a_repo() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let sh = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
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
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        sh(&["add", "a.txt"]);
        std::fs::write(root.join("notes.txt"), "untracked\n").unwrap();
        std::fs::write(root.join("big.bin"), vec![0u8; (MAX_UNTRACKED + 1) as usize]).unwrap();
        let repo = resolve(root, root).unwrap();
        assert!(repo.shadow.is_none());
        let (tree, skipped) = snapshot(&repo, &[]).unwrap();
        assert_eq!(skipped, ["big.bin"]);
        // The session's first checkpoint is the base the Files tab diffs against.
        let entry = crate::session::Entry {
            id: "e1".into(),
            parent: None,
            ts_ms: 0,
            kind: crate::session::EntryKind::Checkpoint {
                tree: tree.clone(),
                skipped: vec![],
            },
        };
        let sd = tempfile::tempdir().unwrap();
        let file = sd.path().join("s.jsonl");
        std::fs::write(&file, format!("{{}}\n{}\n", serde_json::to_string(&entry).unwrap())).unwrap();
        assert_eq!(session_base(&file, &repo, "a.txt").unwrap().as_deref(), Some("one\n"));
        assert_eq!(session_base(&file, &repo, "new.txt").unwrap(), None);
        // The turn edits, deletes and creates files, as bash would.
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::remove_file(root.join("notes.txt")).unwrap();
        std::fs::write(root.join("new.txt"), "created later\n").unwrap();
        let (undo, n) = apply(&repo, &tree, &[]).unwrap();
        assert_eq!(n, 3);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\n");
        assert!(root.join("notes.txt").exists() && !root.join("new.txt").exists());
        assert!(root.join("big.bin").exists(), "skipped files are left alone");
        // The user's index still has the staged "one", untouched by the snapshot.
        let staged = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "a.txt");
        // Undo brings the turn's state back.
        apply(&repo, &undo, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "two\n");
        assert!(root.join("new.txt").exists() && !root.join("notes.txt").exists());
        assert!(exists(&repo, &tree) && !exists(&repo, "0000000000000000000000000000000000000000"));
    }

    #[test]
    fn shadow_store_rewinds_edited_files() {
        let d = tempfile::tempdir().unwrap();
        let (home, work) = (d.path().join("h"), d.path().join("w"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("f.txt"), "before\n").unwrap();
        let repo = resolve(&work, &home).unwrap();
        assert!(repo.shadow.is_some());
        let (tree, _) = snapshot(&repo, &[]).unwrap();
        // novacode's edit tool: pre-image first, then the change; also a new file.
        let pre = pre_image(&repo, "f.txt").unwrap();
        std::fs::write(work.join("f.txt"), "after\n").unwrap();
        let none = pre_image(&repo, "g.txt").unwrap();
        std::fs::write(work.join("g.txt"), "new\n").unwrap();
        let tracked = vec!["f.txt".to_string(), "g.txt".to_string()];
        let target = with_patches(&repo, &tree, &[("f.txt".into(), pre), ("g.txt".into(), none)]).unwrap();
        apply(&repo, &target, &tracked).unwrap();
        assert_eq!(std::fs::read_to_string(work.join("f.txt")).unwrap(), "before\n");
        assert!(!work.join("g.txt").exists());
    }
}
