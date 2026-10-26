//! grep, find, ls (L5.9) on ripgrep's library crates; .gitignore respected,
//! match counts capped with the true total reported.
// shortcut: these walk the local filesystem directly, not through ExecutionEnv; a remote env (L8.11) needs its own search.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const MAX_LINE: usize = 500;

fn clip(s: &str) -> &str {
    match s.char_indices().nth(MAX_LINE) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn rel(p: &Path, base: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).display().to_string()
}

/// Run blocking search work off the async threads.
async fn blocking<F: FnOnce() -> ToolOutput + Send + 'static>(f: F) -> ToolOutput {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| ToolOutput::error(format!("search failed: {e}")))
}

// ---------- grep ----------

pub struct Grep;

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regular expression (Rust regex syntax), or a literal string with literal=true.
    pattern: String,
    /// File or directory to search (default: working directory).
    path: Option<String>,
    /// Only search files matching this glob, e.g. "*.rs".
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    /// Lines of context before and after each match.
    context: Option<usize>,
    /// Maximum matches to show (default 100).
    limit: Option<usize>,
}

struct Collect<'a> {
    path: &'a str,
    out: &'a mut Vec<String>,
    matches: &'a mut usize,
    limit: usize,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        *self.matches += 1;
        if *self.matches <= self.limit {
            let line = String::from_utf8_lossy(m.bytes());
            self.out.push(format!(
                "{}:{}: {}",
                self.path,
                m.line_number().unwrap_or(0),
                clip(line.trim_end())
            ));
        }
        Ok(true)
    }
    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if *self.matches < self.limit {
            let line = String::from_utf8_lossy(c.bytes());
            self.out.push(format!(
                "{}-{}- {}",
                self.path,
                c.line_number().unwrap_or(0),
                clip(line.trim_end())
            ));
        }
        Ok(true)
    }
}

impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regex. Respects .gitignore. Output: path:line: text."
    }
    fn schema(&self) -> Value {
        schema_for::<GrepArgs>()
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: GrepArgs = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let root = ctx.path(a.path.as_deref().unwrap_or("."));
            let (cwd, cancel) = (ctx.cwd.clone(), ctx.cancel.clone());
            blocking(move || grep(a, root, cwd, cancel)).await
        })
    }
}

fn grep(a: GrepArgs, root: PathBuf, cwd: PathBuf, cancel: CancellationToken) -> ToolOutput {
    let pat = if a.literal {
        regex_escape(&a.pattern)
    } else {
        a.pattern.clone()
    };
    let matcher = match RegexMatcherBuilder::new().case_insensitive(a.ignore_case).build(&pat) {
        Ok(m) => m,
        Err(e) => return ToolOutput::error(format!("bad pattern: {e}")),
    };
    let glob = match a.glob.as_deref().map(globset::Glob::new).transpose() {
        Ok(g) => g.map(|g| g.compile_matcher()),
        Err(e) => return ToolOutput::error(format!("bad glob: {e}")),
    };
    let ctxn = a.context.unwrap_or(0).min(10);
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(ctxn)
        .after_context(ctxn)
        // Stop at the first NUL byte: binary files report no matches (not on by default).
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    let limit = a.limit.unwrap_or(100).clamp(1, 1000);
    let (mut out, mut matches, mut files) = (Vec::new(), 0usize, 0usize);
    // `require_git(false)`: honour .gitignore outside a git repo too.
    for entry in WalkBuilder::new(&root).require_git(false).build().flatten() {
        if cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.path();
        if let Some(g) = &glob
            && !g.is_match(p.file_name().unwrap_or_default())
            && !g.is_match(p.strip_prefix(&root).unwrap_or(p))
        {
            continue;
        }
        let shown = rel(p, &cwd);
        let before = matches;
        let sink = Collect {
            path: &shown,
            out: &mut out,
            matches: &mut matches,
            limit,
        };
        let _ = searcher.search_path(&matcher, p, sink); // unreadable files error and are skipped; binary ones stop at their first NUL
        if matches > before {
            files += 1;
        }
    }
    if matches == 0 {
        return ToolOutput::text("No matches.");
    }
    let mut text = out.join("\n");
    if matches > limit {
        text.push_str(&format!(
            "\n[{matches} matches in {files} files; showing the first {limit}. Narrow the pattern or path.]"
        ));
    }
    ToolOutput::text(text)
}

fn regex_escape(s: &str) -> String {
    s.chars().fold(String::new(), |mut o, c| {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            o.push('\\');
        }
        o.push(c);
        o
    })
}

// ---------- find ----------

pub struct Find;

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// Glob, e.g. "**/*.rs" or "Cargo.toml" (a pattern without '/' matches file names anywhere).
    pattern: String,
    /// Directory to search (default: working directory).
    path: Option<String>,
    /// Maximum results (default 1000).
    limit: Option<usize>,
}

impl Tool for Find {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern. Respects .gitignore. Paths are relative to the search directory."
    }
    fn schema(&self) -> Value {
        schema_for::<FindArgs>()
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: FindArgs = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let root = ctx.path(a.path.as_deref().unwrap_or("."));
            let cancel = ctx.cancel.clone();
            blocking(move || {
                let g = match globset::Glob::new(&a.pattern) {
                    Ok(g) => g.compile_matcher(),
                    Err(e) => return ToolOutput::error(format!("bad glob: {e}")),
                };
                let by_name = !a.pattern.contains('/');
                let limit = a.limit.unwrap_or(1000).clamp(1, 5000);
                let (mut out, mut total) = (Vec::new(), 0usize);
                for e in WalkBuilder::new(&root).require_git(false).build().flatten() {
                    if cancel.is_cancelled() {
                        return ToolOutput::error("cancelled");
                    }
                    let r = e.path().strip_prefix(&root).unwrap_or(e.path());
                    if r.as_os_str().is_empty() {
                        continue;
                    }
                    let hit = g.is_match(r) || (by_name && g.is_match(r.file_name().unwrap_or_default()));
                    if hit {
                        total += 1;
                        if total <= limit {
                            let dir = if e.file_type().is_some_and(|t| t.is_dir()) {
                                "/"
                            } else {
                                ""
                            };
                            out.push(format!("{}{dir}", r.display()));
                        }
                    }
                }
                out.sort();
                if total == 0 {
                    return ToolOutput::text("No files found.");
                }
                let mut text = out.join("\n");
                if total > limit {
                    text.push_str(&format!("\n[{total} results; showing {limit}]"));
                }
                ToolOutput::text(text)
            })
            .await
        })
    }
}

// ---------- ls ----------

pub struct Ls;

#[derive(Deserialize, JsonSchema)]
struct LsArgs {
    /// Directory to list (default: working directory).
    path: Option<String>,
    /// Maximum entries (default 500).
    limit: Option<usize>,
}

impl Tool for Ls {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List a directory, sorted, directories marked with '/', dotfiles included."
    }
    fn schema(&self) -> Value {
        schema_for::<LsArgs>()
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: LsArgs = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let dir = ctx.path(a.path.as_deref().unwrap_or("."));
            blocking(move || {
                let rd = match std::fs::read_dir(&dir) {
                    Ok(r) => r,
                    Err(e) => return ToolOutput::error(format!("cannot list {}: {e}", dir.display())),
                };
                let mut names: Vec<String> = rd
                    .flatten()
                    .map(|e| {
                        let slash = if e.file_type().is_ok_and(|t| t.is_dir()) {
                            "/"
                        } else {
                            ""
                        };
                        format!("{}{slash}", e.file_name().to_string_lossy())
                    })
                    .collect();
                names.sort();
                let limit = a.limit.unwrap_or(500).clamp(1, 5000);
                let total = names.len();
                names.truncate(limit);
                let mut text = if total == 0 {
                    "(empty directory)".to_string()
                } else {
                    names.join("\n")
                };
                if total > limit {
                    text.push_str(&format!("\n[{total} entries; showing {limit}]"));
                }
                ToolOutput::text(text)
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};
    use serde_json::json;

    #[tokio::test]
    async fn grep_find_ls() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(p.join("b.txt"), "alpha.x\n").unwrap();
        std::fs::write(p.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(p.join("ignored")).unwrap();
        std::fs::write(p.join("ignored/c.rs"), "alpha\n").unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(p)
            .status()
            .unwrap();
        let c = ctx(p);

        let o = Grep.execute(json!({"pattern": "alpha", "glob": "*.rs"}), &c).await;
        assert_eq!(text(&o).replace('\\', "/"), "src/a.rs:1: fn alpha() {}");
        let o = Grep.execute(json!({"pattern": "alpha.x", "literal": true}), &c).await;
        assert_eq!(text(&o), "b.txt:1: alpha.x");
        let o = Grep.execute(json!({"pattern": "fn", "limit": 1}), &c).await;
        assert!(text(&o).contains("[2 matches in 1 files; showing the first 1."));
        let o = Grep.execute(json!({"pattern": "beta", "context": 1}), &c).await;
        assert_eq!(text(&o), "src/a.rs-1- fn alpha() {}\nsrc/a.rs:2: fn beta() {}");

        let o = Find.execute(json!({"pattern": "*.rs"}), &c).await;
        assert_eq!(text(&o), "src/a.rs");
        let o = Ls.execute(json!({}), &c).await;
        assert_eq!(text(&o), ".git/\n.gitignore\nb.txt\nignored/\nsrc/");
    }

    #[tokio::test]
    async fn grep_skips_gitignored_and_binary_files_outside_git() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        assert!(!p.join(".git").exists());
        std::fs::write(p.join(".gitignore"), "junk/\n").unwrap();
        std::fs::create_dir_all(p.join("junk")).unwrap();
        std::fs::write(p.join("junk/a.txt"), "needle\n").unwrap();
        std::fs::write(p.join("bin.dat"), b"needle\n\x00\x01\x02needle\n").unwrap();
        let c = ctx(p);
        let o = Grep.execute(json!({"pattern": "needle"}), &c).await;
        assert_eq!(text(&o), "No matches.");
        let o = Find.execute(json!({"pattern": "*.txt"}), &c).await;
        assert_eq!(text(&o), "No files found.");
    }
}
