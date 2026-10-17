//! Project trust (L6.3): whether a folder's executable config (project
//! settings, extensions, MCP, hooks, project skills) may load. Decisions live
//! in `<home>/trust.json`; the nearest ancestor entry wins. AGENTS.md and
//! CLAUDE.md are plain text and always load (L4.9 revision).

use novacode_base::storage::{self, Durability};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub struct TrustStore {
    path: PathBuf,
}

/// Folders a project can configure novacode from; trust is only asked when one exists.
const PROJECT_CONFIG: &[&str] = &[
    ".novacode/settings.json",
    ".novacode/extensions",
    ".novacode/skills",
    ".mcp.json",
];

impl TrustStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> BTreeMap<PathBuf, bool> {
        match storage::read_json(&self.path) {
            Ok(Some(m)) => m,
            Ok(None) => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "unreadable trust file; treating every project as untrusted");
                BTreeMap::new()
            }
        }
    }

    /// The decision for `dir`: its own entry or its nearest ancestor's. `None` = never asked.
    pub fn decision(&self, dir: &Path) -> Option<bool> {
        let map = self.read();
        dir.ancestors().find_map(|a| map.get(a).copied())
    }

    /// Record a decision for `dir` (and everything below it).
    pub fn set(&self, dir: &Path, trusted: bool) -> io::Result<()> {
        let _lock = storage::lock(&self.path)?;
        let mut map = self.read();
        map.insert(dir.to_path_buf(), trusted);
        let bytes = serde_json::to_vec_pretty(&map).map_err(io::Error::other)?;
        storage::write_atomic(&self.path, &bytes, Durability::Durable)
    }

    pub fn remove(&self, dir: &Path) -> io::Result<bool> {
        let _lock = storage::lock(&self.path)?;
        let mut map = self.read();
        let had = map.remove(dir).is_some();
        let bytes = serde_json::to_vec_pretty(&map).map_err(io::Error::other)?;
        storage::write_atomic(&self.path, &bytes, Durability::Durable)?;
        Ok(had)
    }
}

/// True when `dir` has project config that trust would gate.
pub fn has_project_config(dir: &Path) -> bool {
    PROJECT_CONFIG.iter().any(|p| dir.join(p).exists())
}

/// Resolve trust without a UI: `--trust` wins, then a stored decision; otherwise untrusted.
/// The interactive 4-choice prompt lives in the binary (it needs the TUI).
pub fn resolve(store: &TrustStore, dir: &Path, cli_trust: bool) -> bool {
    cli_trust || store.decision(dir).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_ancestor_wins() {
        let d = tempfile::tempdir().unwrap();
        let s = TrustStore::new(d.path().join("trust.json"));
        assert_eq!(s.decision(Path::new("/w/a/b")), None);
        s.set(Path::new("/w"), true).unwrap();
        s.set(Path::new("/w/a"), false).unwrap();
        assert_eq!(s.decision(Path::new("/w/x")), Some(true));
        assert_eq!(s.decision(Path::new("/w/a/b")), Some(false));
        assert!(!resolve(&s, Path::new("/w/a/b"), false));
        assert!(resolve(&s, Path::new("/w/a/b"), true));
        assert!(s.remove(Path::new("/w/a")).unwrap());
        assert_eq!(s.decision(Path::new("/w/a/b")), Some(true));
    }
}
