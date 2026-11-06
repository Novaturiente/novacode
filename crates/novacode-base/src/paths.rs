//! Home and project directories (L0.4).

use std::path::{Path, PathBuf};

/// novacode's login and trust files in its home folder (plus `auth-*.json`): the sandbox
/// hides them, the risk rule refuses replacing them (L5).
pub const LOGIN_FILES: &[&str] = &["auth.json", "mcp-auth.json", "trust.json"];
/// The rest of novacode's home folder that only the user changes: what runs as them.
pub const CONFIG_ENTRIES: &[&str] = &["settings.json", "extensions", "packages"];

/// `name`, a top-level entry of novacode's home folder, is one of its secrets (logins,
/// trust, settings, extensions, packages); `same` compares two names (case-folding on Windows).
pub fn is_secret_entry(name: &str, same: impl Fn(&str, &str) -> bool) -> bool {
    LOGIN_FILES.iter().chain(CONFIG_ENTRIES).any(|s| same(name, s))
        || (name.len() >= 10
            && name.get(..5).is_some_and(|h| same(h, "auth-"))
            && name.get(name.len() - 5..).is_some_and(|t| same(t, ".json")))
}

/// Where novacode keeps its files. Tests build one over a temp dir instead of
/// touching env vars, so they never see the real home (L0.9).
#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("cannot find home dir: set NOVACODE_HOME or HOME")]
pub struct NoHome;

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `$NOVACODE_HOME`, else `$HOME/.novacode`.
    pub fn from_env() -> Result<Self, NoHome> {
        let non_empty = |k| std::env::var_os(k).filter(|v| !v.is_empty());
        if let Some(h) = non_empty("NOVACODE_HOME") {
            return Ok(Self::new(h));
        }
        crate::platform::home()
            .filter(|h| !h.as_os_str().is_empty())
            .map(|h| Self::new(h.join(".novacode")))
            .ok_or(NoHome)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn settings(&self) -> PathBuf {
        self.home.join("settings.json")
    }
    pub fn logs(&self) -> PathBuf {
        self.home.join("logs")
    }
    /// Project config dir for a working directory: `<cwd>/.novacode`.
    pub fn project_dir(cwd: &Path) -> PathBuf {
        cwd.join(".novacode")
    }
    pub fn project_settings(cwd: &Path) -> PathBuf {
        Self::project_dir(cwd).join("settings.json")
    }
}
