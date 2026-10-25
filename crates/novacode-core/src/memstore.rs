//! Cross-session memory (L10.4): SQLite + FTS5, one global database and one
//! per project under `<home>/memory/`. Written by the model (`memory remember`)
//! and by the observer's reflections; read through `memory search` and a short
//! index written once at session start (never injected per turn).

use futures_util::future::BoxFuture;
use novacode_tools::{Tool, ToolCtx, ToolOutput};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items(
  id INTEGER PRIMARY KEY,
  text TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'fact',
  pinned INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL,
  used_ms INTEGER NOT NULL DEFAULT 0,
  hits INTEGER NOT NULL DEFAULT 0,
  source TEXT
);
CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(text, content='items', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS items_ad AFTER DELETE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS items_au AFTER UPDATE OF text ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, text) VALUES ('delete', old.id, old.text);
  INSERT INTO items_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    fn prefix(self) -> char {
        match self {
            Scope::Project => 'p',
            Scope::Global => 'g',
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// `p12` (project) or `g3` (global).
    pub id: String,
    pub text: String,
    pub kind: String,
    pub pinned: bool,
}

/// Both databases of one working folder.
#[derive(Debug, Clone)]
pub struct Memories {
    global: PathBuf,
    project: PathBuf,
    root: PathBuf,
}

/// The project a folder belongs to: the main repo's top (worktrees map back), else the folder.
fn project_root(cwd: &Path) -> PathBuf {
    novacode_base::git::command_in(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .and_then(|git| git.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn now_ms() -> i64 {
    crate::session::now_ms() as i64
}

impl Memories {
    pub fn open(home: &Path, cwd: &Path) -> Self {
        let root = project_root(cwd);
        let key = root
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        Memories {
            global: home.join("memory").join("global.db"),
            project: home.join("memory").join("projects").join(format!("{key:016x}.db")),
            root,
        }
    }

    fn conn(&self, scope: Scope) -> rusqlite::Result<Connection> {
        let path = match scope {
            Scope::Project => &self.project,
            Scope::Global => &self.global,
        };
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let c = Connection::open(path)?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch(SCHEMA)?;
        if scope == Scope::Project {
            c.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES ('path', ?1)",
                [self.root.to_string_lossy()],
            )?;
        }
        Ok(c)
    }

    /// Save `text`; the same text again (case and spacing aside) refreshes it instead.
    pub fn remember(
        &self,
        scope: Scope,
        text: &str,
        kind: &str,
        pinned: bool,
        source: &str,
    ) -> rusqlite::Result<String> {
        let text = text.trim();
        let c = self.conn(scope)?;
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let same: Option<i64> = {
            let mut st = c.prepare("SELECT id, text FROM items")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            rows.flatten().find(|(_, t)| norm(t) == norm(text)).map(|(id, _)| id)
        };
        let id = match same {
            Some(id) => {
                c.execute(
                    "UPDATE items SET pinned = max(pinned, ?2), used_ms = ?3 WHERE id = ?1",
                    params![id, pinned as i64, now_ms()],
                )?;
                id
            }
            None => {
                c.execute(
                    "INSERT INTO items(text, kind, pinned, created_ms, source) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![text, kind, pinned as i64, now_ms(), source],
                )?;
                c.last_insert_rowid()
            }
        };
        Ok(format!("{}{id}", scope.prefix()))
    }

    pub fn forget(&self, id: &str) -> rusqlite::Result<bool> {
        let Some((scope, n)) = parse_id(id) else {
            return Ok(false);
        };
        Ok(self.conn(scope)?.execute("DELETE FROM items WHERE id = ?1", [n])? > 0)
    }

    /// Best matches of both databases (bm25), project first on a tie; counts a hit on each.
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<Item>> {
        // Any word may match: quoted, so FTS syntax in the query can't break it.
        let q = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() > 1)
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits: Vec<(f64, Item)> = Vec::new();
        for scope in [Scope::Project, Scope::Global] {
            let c = self.conn(scope)?;
            let mut st = c.prepare(
                "SELECT items.id, items.text, items.kind, items.pinned, bm25(items_fts) \
                 FROM items_fts JOIN items ON items.id = items_fts.rowid \
                 WHERE items_fts MATCH ?1 ORDER BY bm25(items_fts) LIMIT ?2",
            )?;
            let rows = st.query_map(params![q, limit as i64], |r| {
                Ok((
                    r.get::<_, f64>(4)?,
                    Item {
                        id: format!("{}{}", scope.prefix(), r.get::<_, i64>(0)?),
                        text: r.get(1)?,
                        kind: r.get(2)?,
                        pinned: r.get::<_, i64>(3)? != 0,
                    },
                ))
            })?;
            let found: Vec<(f64, Item)> = rows.flatten().collect();
            for (_, it) in &found {
                if let Some((_, n)) = parse_id(&it.id) {
                    c.execute(
                        "UPDATE items SET hits = hits + 1, used_ms = ?2 WHERE id = ?1",
                        params![n, now_ms()],
                    )?;
                }
            }
            hits.extend(found);
        }
        // bm25: lower is better. Stable sort keeps project ahead on a tie.
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(hits.into_iter().take(limit).map(|(_, i)| i).collect())
    }

    /// Pinned items, then the most used and newest, within `max_chars`: the
    /// session-start index (L10.4 ranking). Empty when there's nothing saved.
    pub fn index(&self, max_items: usize, max_chars: usize) -> String {
        let mut items = Vec::new();
        for scope in [Scope::Project, Scope::Global] {
            let Ok(c) = self.conn(scope) else { continue };
            let Ok(mut st) =
                c.prepare("SELECT id, text FROM items ORDER BY pinned DESC, hits DESC, created_ms DESC LIMIT ?1")
            else {
                continue;
            };
            let rows = st.query_map([max_items as i64], |r| {
                Ok(format!(
                    "- [{}{}] {}",
                    scope.prefix(),
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?
                ))
            });
            items.extend(rows.into_iter().flatten().flatten());
        }
        let mut out = String::new();
        for line in items.into_iter().take(max_items) {
            let line: String = line.chars().take(300).collect();
            if out.len() + line.len() > max_chars {
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        if out.is_empty() {
            return out;
        }
        format!(
            "Saved memories from earlier sessions (p = this project, g = everywhere). \
             Search for more with the `memory` tool before redoing past work.\n{out}"
        )
    }

    pub fn count(&self, scope: Scope) -> usize {
        self.conn(scope)
            .and_then(|c| {
                c.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                    .optional()
            })
            .ok()
            .flatten()
            .unwrap_or(0) as usize
    }
}

fn parse_id(id: &str) -> Option<(Scope, i64)> {
    let scope = match id.chars().next()? {
        'p' => Scope::Project,
        'g' => Scope::Global,
        _ => return None,
    };
    Some((scope, id[1..].parse().ok()?))
}

/// The `memory` tool: remember, search, forget.
pub struct MemoryTool {
    pub mem: Memories,
    pub session: String,
}

impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "Long-term memory across sessions. remember {text, scope?: project|global, kind?, pin?} saves a lasting fact \
         (a user preference, a decision and its reason, how something in this project works); search {query, limit?} \
         finds saved ones; forget {id} deletes one that is wrong or stale. Save only what a future session needs."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["remember", "search", "forget"]},
                "text": {"type": "string"},
                "scope": {"type": "string", "enum": ["project", "global"], "description": "global = true in every project (user preferences)."},
                "kind": {"type": "string", "description": "fact, preference, decision…"},
                "pin": {"type": "boolean", "description": "Always list it at session start."},
                "query": {"type": "string"},
                "limit": {"type": "number"},
                "id": {"type": "string"}
            },
            "required": ["action"]
        })
    }
    fn sequential(&self) -> bool {
        false
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let mem = self.mem.clone();
            let session = self.session.clone();
            // SQLite is blocking: off the async threads.
            let r = tokio::task::spawn_blocking(move || -> Result<String, String> {
                let s = |k: &str| args[k].as_str().unwrap_or_default().to_string();
                match s("action").as_str() {
                    "remember" => {
                        let text = s("text");
                        if text.trim().is_empty() {
                            return Err("remember needs text".into());
                        }
                        let scope = if s("scope") == "global" {
                            Scope::Global
                        } else {
                            Scope::Project
                        };
                        let kind = Some(s("kind")).filter(|k| !k.is_empty()).unwrap_or("fact".into());
                        let id = mem
                            .remember(scope, &text, &kind, args["pin"].as_bool().unwrap_or(false), &session)
                            .map_err(|e| e.to_string())?;
                        Ok(format!("saved as {id}"))
                    }
                    "search" => {
                        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
                        let found = mem.search(&s("query"), limit).map_err(|e| e.to_string())?;
                        Ok(if found.is_empty() {
                            "nothing saved matches".into()
                        } else {
                            found
                                .iter()
                                .map(|i| format!("[{}] ({}) {}", i.id, i.kind, i.text))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                    }
                    "forget" => match mem.forget(&s("id")).map_err(|e| e.to_string())? {
                        true => Ok(format!("forgot {}", s("id"))),
                        false => Err(format!("no memory `{}`", s("id"))),
                    },
                    other => Err(format!("unknown action `{other}`")),
                }
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            match r {
                Ok(t) => ToolOutput::text(t),
                Err(e) => ToolOutput::error(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_search_index_forget() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Memories::open(tmp.path(), tmp.path());
        let a = m
            .remember(
                Scope::Project,
                "Tests run with `cargo nextest`, not cargo test.",
                "fact",
                false,
                "s1",
            )
            .unwrap();
        let b = m
            .remember(
                Scope::Global,
                "The user wants tabs, never spaces.",
                "preference",
                true,
                "s1",
            )
            .unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("p1", "g1"));
        // Same text again refreshes instead of duplicating.
        assert_eq!(
            m.remember(
                Scope::Project,
                "tests run with `cargo  nextest`, not cargo test.",
                "fact",
                false,
                "s2"
            )
            .unwrap(),
            "p1"
        );
        assert_eq!(m.count(Scope::Project), 1);
        let found = m.search("how do I run the tests? nextest", 5).unwrap();
        assert_eq!(found[0].id, "p1");
        assert!(m.search("tabs or spaces", 5).unwrap().iter().any(|i| i.id == "g1"));
        assert!(
            m.search("\"unbalanced OR (", 5).is_ok(),
            "FTS syntax in a query can't break it"
        );
        let idx = m.index(10, 2000);
        assert!(
            idx.contains("[g1] The user wants tabs") && idx.contains("[p1]"),
            "{idx}"
        );
        assert!(m.forget("p1").unwrap());
        assert!(!m.forget("p1").unwrap());
        assert!(m.search("nextest", 5).unwrap().is_empty());
        assert_eq!(
            Memories::open(tmp.path(), tmp.path()).count(Scope::Global),
            1,
            "persists"
        );
    }
}
