//! Home and project directories (L0.4).

use std::path::{Path, PathBuf};

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
