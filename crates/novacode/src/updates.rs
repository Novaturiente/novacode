//! Update check (L11.13): at an interactive start, tell the user when novacode or
//! an installed package has a newer release. Asked at most once per 24 h (cached
//! in `~/.novacode/cache/updates.json`), shown at every start from the cache.
//! Nothing is installed: the note names the command to run.

use novacode_base::settings::Settings;
use novacode_base::storage::{self, Durability};
use novacode_core::packages::{Available, Store};
use novacode_tui::UiEvent;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

const DAY: u64 = 24 * 3600;

/// What the last check found.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    /// When it ran, in Unix seconds.
    pub checked: u64,
    /// novacode's latest release tag, when newer than the version that checked.
    pub novacode: Option<String>,
    pub packages: Vec<Available>,
}

pub fn path(home: &Path) -> PathBuf {
    home.join("cache").join("updates.json")
}

/// Setting `updates.check` (default on).
pub fn enabled(settings: &Settings) -> bool {
    settings.get("updates.check").unwrap_or(true)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The cache, when it is younger than 24 h.
fn fresh(path: &Path, now: u64) -> Option<Cache> {
    let c: Cache = storage::read_json(path).ok()??;
    (c.checked <= now && now - c.checked < DAY).then_some(c)
}

/// Drop entries already done: a novacode tag not newer than `running`, a
/// package whose version now (`current`) is no longer the one it was checked at.
fn pending(mut c: Cache, running: &str, current: impl Fn(&str) -> Option<String>) -> Cache {
    c.novacode = c.novacode.filter(|t| crate::update::newer(t, running));
    c.packages
        .retain(|p| current(&p.name).as_deref() == Some(p.from.as_str()));
    c
}

/// One line per kind of update.
fn notes(c: &Cache, running: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(t) = &c.novacode {
        out.push(format!(
            "novacode {t} is available (you have {running}): run `novacode update`"
        ));
    }
    if !c.packages.is_empty() {
        let list: Vec<String> = c
            .packages
            .iter()
            .map(|p| format!("{} {} → {}", p.name, p.from, p.to))
            .collect();
        out.push(format!(
            "package updates: {}: run `novacode packages update`",
            list.join(", ")
        ));
    }
    out
}

/// Ask GitHub and the package remotes, write the cache. `None` when every
/// request failed: nothing is written, the next start tries again.
async fn refresh(home: &Path) -> Option<Cache> {
    let store = Store::global(home);
    let (own, (packages, answered)) = tokio::join!(crate::update::check(), store.check());
    if let Err(e) = &own {
        tracing::warn!("update check: {e}");
    }
    if own.is_err() && answered == 0 {
        return None;
    }
    let c = Cache {
        checked: now(),
        novacode: own.ok().flatten(),
        packages,
    };
    if let Err(e) = storage::write_json(&path(home), &c, Durability::Fast) {
        tracing::warn!("update check cache: {e}");
    }
    Some(c)
}

static STARTED: AtomicBool = AtomicBool::new(false);

/// At an interactive start, once per process (not again on `/reload`): notes
/// from a fresh cache to show now; else an empty list and a background check
/// that sends its notes to the window as notices.
pub fn at_start(home: &Path, settings: &Settings, tx: UnboundedSender<UiEvent>) -> Vec<String> {
    if !enabled(settings) || STARTED.swap(true, Ordering::SeqCst) {
        return vec![];
    }
    let running = env!("CARGO_PKG_VERSION");
    let current = |name: &str| Store::global(home).current(name);
    if let Some(c) = fresh(&path(home), now()) {
        return notes(&pending(c, running, current), running);
    }
    let home = home.to_path_buf();
    tokio::spawn(async move {
        let Some(c) = refresh(&home).await else { return };
        for text in notes(&c, running) {
            let _ = tx.send(UiEvent::Notice { text, error: false });
        }
    });
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder of this test's own, removed when dropped.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("nc-updates-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pkg(name: &str, from: &str, to: &str) -> Available {
        Available {
            name: name.into(),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn cache_is_fresh_for_a_day() {
        let d = Tmp::new("cache");
        let p = path(&d.0);
        assert_eq!(fresh(&p, 1000), None, "missing");
        let c = Cache {
            checked: 1000,
            novacode: Some("v0.2.6".into()),
            packages: vec![pkg("caveman", "v4.0.1", "v4.1.0")],
        };
        storage::write_json(&p, &c, Durability::Fast).unwrap();
        assert_eq!(fresh(&p, 1000 + DAY - 1), Some(c.clone()));
        assert_eq!(fresh(&p, 1000 + DAY), None, "stale");
        assert_eq!(fresh(&p, 999), None, "from the future");
    }

    #[test]
    fn done_entries_are_dropped() {
        let c = Cache {
            checked: 0,
            novacode: Some("v0.2.6".into()),
            packages: vec![
                pkg("caveman", "v4.0.1", "v4.1.0"),
                pkg("ponytail", "v5.3.2", "v5.4.0"),
                pkg("gone", "v1.0.0", "v1.1.0"),
            ],
        };
        let current = |n: &str| match n {
            "caveman" => Some("v4.1.0".to_string()),
            "ponytail" => Some("v5.3.2".to_string()),
            _ => None,
        };
        let left = pending(c.clone(), "0.2.3", current);
        assert_eq!(left.novacode.as_deref(), Some("v0.2.6"));
        assert_eq!(left.packages, [pkg("ponytail", "v5.3.2", "v5.4.0")]);
        assert_eq!(pending(c.clone(), "0.2.6", current).novacode, None, "already updated");
        assert_eq!(pending(c, "0.3.0", current).novacode, None);
    }

    #[test]
    fn note_text() {
        let c = Cache {
            checked: 0,
            novacode: Some("v0.2.6".into()),
            packages: vec![pkg("caveman", "v4.0.1", "v4.1.0"), pkg("ponytail", "v5.3.2", "v5.4.0")],
        };
        assert_eq!(
            notes(&c, "0.2.3"),
            [
                "novacode v0.2.6 is available (you have 0.2.3): run `novacode update`",
                "package updates: caveman v4.0.1 → v4.1.0, ponytail v5.3.2 → v5.4.0: run `novacode packages update`",
            ]
        );
        assert!(notes(&Cache::default(), "0.2.3").is_empty());
    }

    #[test]
    fn on_by_default() {
        assert!(enabled(&Settings::default()));
        let d = Tmp::new("setting");
        let f = d.0.join("settings.json");
        std::fs::write(&f, r#"{"updates": {"check": false}}"#).unwrap();
        assert!(!enabled(&Settings::load(&f, None, false)));
    }
}
