//! The user docs ship inside the binary and are written to `~/.novacode/docs/` when the
//! version changes, so every install path (AUR, tarball, cargo, `novacode update`) has
//! docs that match the running binary. `/docs` points the session at them.

use novacode_base::paths::Paths;
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");

macro_rules! pages {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../../../docs/", $name)))),*]
    };
}

/// Every page of `docs/` (a test checks none is missing) plus the README as the index.
const PAGES: &[(&str, &str)] = pages![
    "automation.md",
    "config.md",
    "context.md",
    "extensions.md",
    "install.md",
    "mcp.md",
    "migrating.md",
    "packages.md",
    "providers.md",
    "quickstart.md",
    "security.md",
    "sessions.md",
    "skills.md",
    "subagents.md",
    "tools.md",
    "troubleshooting.md",
    "tui.md",
];
const README: &str = include_str!("../../../README.md");

pub fn dir(paths: &Paths) -> PathBuf {
    paths.home().join("docs")
}

/// Write the docs when the copy on disk is from another version. Cheap when current
/// (one small file read); a failure only means `/docs` points at stale or no files.
pub fn install(paths: &Paths) -> std::io::Result<()> {
    let d = dir(paths);
    if std::fs::read_to_string(d.join("VERSION")).is_ok_and(|v| v.trim() == VERSION) {
        return Ok(());
    }
    // Our own folder: rebuilt whole so pages removed upstream disappear.
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("README.md"), README)?;
    for (name, text) in PAGES {
        std::fs::write(d.join(name), text)?;
    }
    std::fs::write(d.join("VERSION"), VERSION)
}

fn title(text: &str) -> &str {
    text.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or("").trim()
}

/// The note `/docs` adds to the session: where the docs and the config files are.
pub fn brief(paths: &Paths, cwd: &Path) -> String {
    let d = dir(paths);
    let home = paths.home();
    let mut s = format!(
        "The user wants to inspect or customise novacode, the harness running this session (version {VERSION}).\n\
         Its documentation is in {}: read the relevant page before answering or changing setup.\n\
         - README.md: overview and feature list\n",
        d.display()
    );
    for (name, text) in PAGES {
        s.push_str(&format!("- {name}: {}\n", title(text)));
    }
    s.push_str("\nConfig files (✓ exists, · not yet):\n");
    let user = [
        "settings.json",
        "AGENTS.md",
        "SYSTEM.md",
        "APPEND_SYSTEM.md",
        "keybindings.json",
        "mcp.json",
        "packages.lock.json",
        "skills",
        "prompts",
        "agents",
        "extensions",
        "themes",
    ];
    let project = [
        ".novacode/settings.json",
        "AGENTS.md",
        ".mcp.json",
        ".novacode/packages.json",
        ".novacode/skills",
        ".novacode/prompts",
        ".novacode/agents",
        ".novacode/extensions",
    ];
    let mark = |p: PathBuf| format!("{} {}\n", if p.exists() { "✓" } else { "·" }, p.display());
    for f in user {
        s.push_str(&mark(home.join(f)));
    }
    for f in project {
        s.push_str(&mark(cwd.join(f)));
    }
    s.push_str(
        "\nEdit the user's config files, never the docs copy (it is rewritten on every update). \
         After a change, tell the user to run /reload (or restart for providers), and `novacode doctor` checks the setup.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_ships_and_installs_once_per_version() {
        let dir_pages: std::collections::BTreeSet<String> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs"))
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.ends_with(".md"))
                .collect();
        let shipped: std::collections::BTreeSet<String> = PAGES.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(dir_pages, shipped, "add new docs pages to PAGES");

        let tmp = std::env::temp_dir().join(format!("nc-docs-{}", std::process::id()));
        let paths = Paths::new(&tmp);
        install(&paths).unwrap();
        let d = dir(&paths);
        assert!(d.join("tools.md").exists() && d.join("README.md").exists());
        std::fs::write(d.join("tools.md"), "edited").unwrap();
        install(&paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("tools.md")).unwrap(),
            "edited",
            "same version: untouched"
        );
        std::fs::write(d.join("VERSION"), "0.0.0").unwrap();
        std::fs::write(d.join("gone.md"), "x").unwrap();
        install(&paths).unwrap();
        assert!(!d.join("gone.md").exists() && std::fs::read_to_string(d.join("tools.md")).unwrap() != "edited");

        let b = brief(&paths, &tmp);
        assert!(b.contains("- tools.md: Tools") && b.contains(&format!("· {}", tmp.join(".mcp.json").display())));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
