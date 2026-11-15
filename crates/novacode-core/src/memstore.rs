//! Cross-session memory (L10.4): SQLite + FTS5, one global database and one
//! per project under `<home>/memory/`. Written by the model (`memory remember`)
//! and by the observer's lasting reflections; read through `memory search`, a short
//! index written once at session start, and [`Memories::recall`] after each prompt.

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
pub fn project_root(cwd: &Path) -> PathBuf {
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

/// The file stem of a project's stores under `<home>/memory/projects/`.
pub fn project_key(root: &Path) -> String {
    let h = root
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

fn now_ms() -> i64 {
    crate::session::now_ms() as i64
}

/// Unpinned items a database keeps; past it, each new item evicts the least used one.
const CAP: i64 = 500;

/// Word overlap at which a new reflection counts as one already saved.
const NEAR: f64 = 0.6;

/// Words too common to say what a prompt is about.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "what", "have", "are", "was", "were", "can", "you", "your",
    "our", "how", "why", "now", "then", "all", "any", "but", "not", "use", "into", "need", "want", "should", "would",
    "could", "make", "there", "they", "them", "will", "just", "also", "about", "when", "where", "which", "while",
    "each", "more", "some", "only", "does", "did", "done", "let", "lets", "see", "get", "got", "please", "proceed",
    "yes", "okay", "look", "check", "show", "its", "it's", "has", "had",
];

/// The words of `text` worth matching on: lowercased, 3+ characters, no stopwords.
fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let w = w.to_lowercase();
        if w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// Same words, mostly (Jaccard of the word sets, 0..1).
fn overlap(a: &str, b: &str) -> f64 {
    let (a, b) = (terms(a), terms(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let both = a.iter().filter(|w| b.contains(w)).count() as f64;
    both / ((a.len() + b.len()) as f64 - both)
}

impl Memories {
    pub fn open(home: &Path, cwd: &Path) -> Self {
        let root = project_root(cwd);
        let key = project_key(&root);
        Memories {
            global: home.join("memory").join("global.db"),
            project: home.join("memory").join("projects").join(format!("{key}.db")),
            root,
        }
    }

    fn path(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Project => &self.project,
            Scope::Global => &self.global,
        }
    }

    /// For reading: `None` when nothing was ever saved there (no empty file made).
    fn read_conn(&self, scope: Scope) -> Option<Connection> {
        self.path(scope).is_file().then(|| self.conn(scope).ok()).flatten()
    }

    fn conn(&self, scope: Scope) -> rusqlite::Result<Connection> {
        let path = self.path(scope);
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

    /// Save `text`; the same text again (case and spacing aside) refreshes it instead,
    /// and so does a `reflection` with mostly the same words as a saved item. Past
    /// [`CAP`] unpinned items, the least used one makes room.
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
            let near = kind == "reflection";
            rows.flatten()
                .find(|(_, t)| norm(t) == norm(text) || (near && overlap(t, text) >= NEAR))
                .map(|(id, _)| id)
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
                let id = c.last_insert_rowid();
                // One in, one out: a store over the cap stays its size (a cleanup shrinks it).
                c.execute(
                    "DELETE FROM items WHERE id = (SELECT id FROM items WHERE pinned = 0 AND id != ?1 \
                     AND (SELECT count(*) FROM items WHERE pinned = 0) > ?2 \
                     ORDER BY hits > 0, max(used_ms, created_ms), id LIMIT 1)",
                    params![id, CAP],
                )?;
                id
            }
        };
        Ok(format!("{}{id}", scope.prefix()))
    }

    pub fn forget(&self, id: &str) -> rusqlite::Result<bool> {
        let Some((scope, n)) = parse_id(id) else {
            return Ok(false);
        };
        let Some(c) = self.read_conn(scope) else {
            return Ok(false);
        };
        Ok(c.execute("DELETE FROM items WHERE id = ?1", [n])? > 0)
    }

    /// Best matches of both databases (bm25), project first on a tie; counts a hit on each.
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<Item>> {
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() > 1)
            .map(str::to_string)
            .collect();
        let found = self.matches(&words, limit, false)?;
        self.hit(&found)?;
        Ok(found)
    }

    /// Saved items that match `prompt` on at least two of its words (one for a
    /// one-word prompt), best first, leaving out `seen` ids; counts a hit on each.
    /// A word matches as a prefix (`deploy` finds `deploys`), plural `s` dropped.
    pub fn recall(&self, prompt: &str, limit: usize, seen: &[String]) -> rusqlite::Result<Vec<Item>> {
        let words: Vec<String> = terms(prompt)
            .into_iter()
            .map(|w| match w.strip_suffix('s') {
                Some(stem) if stem.chars().count() >= 4 => stem.to_string(),
                _ => w,
            })
            .collect();
        let need = words.len().min(2);
        let found: Vec<Item> = self
            .matches(&words, limit * 4, true)?
            .into_iter()
            .filter(|i| !seen.contains(&i.id))
            .filter(|i| {
                let have = terms(&i.text);
                need > 0 && words.iter().filter(|w| have.iter().any(|h| h.starts_with(*w))).count() >= need
            })
            .take(limit)
            .collect();
        self.hit(&found)?;
        Ok(found)
    }

    fn hit(&self, items: &[Item]) -> rusqlite::Result<()> {
        for it in items {
            if let Some((scope, n)) = parse_id(&it.id)
                && let Some(c) = self.read_conn(scope)
            {
                c.execute(
                    "UPDATE items SET hits = hits + 1, used_ms = ?2 WHERE id = ?1",
                    params![n, now_ms()],
                )?;
            }
        }
        Ok(())
    }

    /// Items matching any of `words` (bm25 over both databases), best first.
    fn matches(&self, words: &[String], limit: usize, prefix: bool) -> rusqlite::Result<Vec<Item>> {
        // Quoted, so FTS syntax in the query can't break it.
        let star = if prefix { "*" } else { "" };
        let q = words
            .iter()
            .map(|w| format!("\"{}\"{star}", w.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits: Vec<(f64, Item)> = Vec::new();
        for scope in [Scope::Project, Scope::Global] {
            let Some(c) = self.read_conn(scope) else { continue };
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
            hits.extend(rows.flatten());
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
            let Some(c) = self.read_conn(scope) else { continue };
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
        self.read_conn(scope)
            .and_then(|c| {
                c.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                    .optional()
                    .ok()
                    .flatten()
            })
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

    #[test]
    fn reading_makes_no_database() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Memories::open(tmp.path(), tmp.path());
        assert_eq!(m.index(10, 2000), "");
        assert!(m.search("anything at all", 5).unwrap().is_empty());
        assert!(m.recall("deploy the site", 5, &[]).unwrap().is_empty());
        assert!(!m.forget("p1").unwrap());
        assert_eq!(m.count(Scope::Project), 0);
        assert!(!tmp.path().join("memory").exists(), "no empty files");
    }

    #[test]
    fn recall_matches_the_prompt_and_counts_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Memories::open(tmp.path(), tmp.path());
        let deploy = m
            .remember(
                Scope::Project,
                "Pushing to main deploys production; wait for the user's go-ahead.",
                "reflection",
                false,
                "s",
            )
            .unwrap();
        m.remember(
            Scope::Project,
            "The main menu lives in src/nav.ts.",
            "reflection",
            false,
            "s",
        )
        .unwrap();
        let got = m.recall("Please deploy this to production", 5, &[]).unwrap();
        assert_eq!(got.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), [deploy.as_str()]);
        assert!(
            m.recall("deploy production", 5, std::slice::from_ref(&deploy))
                .unwrap()
                .is_empty(),
            "seen ids skipped"
        );
        assert!(m.recall("what about main", 5, &[]).unwrap().len() <= 2);
        assert!(m.recall("the and for", 5, &[]).unwrap().is_empty(), "only stopwords");
        // Hits rank the index: the recalled item comes first.
        assert!(
            m.index(1, 2000).contains(&format!("[{deploy}]")),
            "{}",
            m.index(1, 2000)
        );
    }

    #[test]
    fn near_duplicate_reflections_and_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let m = Memories::open(tmp.path(), tmp.path());
        let a = m
            .remember(
                Scope::Project,
                "Tests run with cargo nextest in the workspace root, never cargo test.",
                "reflection",
                false,
                "s",
            )
            .unwrap();
        let b = m
            .remember(
                Scope::Project,
                "Tests run with cargo nextest in the workspace root, not cargo test.",
                "reflection",
                false,
                "s",
            )
            .unwrap();
        assert_eq!(a, b, "a reworded reflection refreshes the saved one");
        let c = m
            .remember(
                Scope::Project,
                "Tests run with cargo nextest in the workspace root, not cargo test.",
                "fact",
                false,
                "s",
            )
            .unwrap();
        assert_ne!(a, c, "the model's own facts merge only on the same text");
        let pinned = m.remember(Scope::Project, "pinned rule", "fact", true, "s").unwrap();
        for i in 0..CAP + 5 {
            m.remember(Scope::Project, &format!("filler number {i} zz{i}"), "fact", false, "s")
                .unwrap();
        }
        let c = m.read_conn(Scope::Project).unwrap();
        let unpinned: i64 = c
            .query_row("SELECT count(*) FROM items WHERE pinned = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unpinned, CAP, "held at the cap");
        assert!(
            m.search("pinned rule", 5).unwrap().iter().any(|i| i.id == pinned),
            "pinned never evicted"
        );
        assert!(m.search("zz0", 5).unwrap().is_empty(), "the oldest unused went first");
    }
}
