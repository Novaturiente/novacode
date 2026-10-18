//! edit (L5.6): several exact replacements in one call, all-or-nothing.
//! Exact match first, then a line-wise fuzzy match (trailing whitespace,
//! Unicode quotes/dashes/nbsp). CRLF and BOM are preserved. Returns a compact diff.

use crate::{Tool, ToolCtx, ToolOutput, parse, schema_for};
use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::ops::Range;

pub struct Edit;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// File path, relative to the working directory or absolute.
    path: String,
    /// Replacements, matched against the file as it is before this call.
    edits: Vec<OneEdit>,
}

#[derive(Deserialize, JsonSchema)]
struct OneEdit {
    /// Exact text to replace; must occur exactly once.
    old_text: String,
    /// Replacement text.
    new_text: String,
}

const BOM: &str = "\u{feff}";

impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }
    fn guidelines(&self) -> &[&str] {
        &["Change files with `edit`, not with sed, perl or scripts run through bash."]
    }
    fn description(&self) -> &str {
        "Replace text in a file. Each old_text must match exactly one place in the current file (whitespace-tolerant fallback). All edits apply together or none do."
    }
    fn schema(&self) -> Value {
        schema_for::<Args>()
    }
    fn large_output(&self) -> bool {
        false
    }
    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let a: Args = match parse(args) {
                Ok(a) => a,
                Err(e) => return e,
            };
            if a.edits.is_empty() {
                return ToolOutput::error("edits is empty");
            }
            let path = ctx.path(&a.path);
            let g = ctx.files.lock(ctx, &path).await;
            if let Some(stale) = ctx.files.check_stale(ctx, &g.path).await {
                return ToolOutput::error(stale);
            }
            let bytes = match ctx.env.read(&path).await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", a.path)),
            };
            let Ok(raw) = String::from_utf8(bytes) else {
                return ToolOutput::error(format!("{} is not UTF-8 text", a.path));
            };
            let pairs: Vec<(String, String)> = a.edits.into_iter().map(|e| (e.old_text, e.new_text)).collect();
            let (new, diff) = match apply_edits(&raw, &pairs) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("{}: {e}. No edits were applied.", a.path)),
            };
            if let Err(e) = ctx.env.write(&path, new.as_bytes()).await {
                return ToolOutput::error(format!("cannot write {}: {e}", a.path));
            }
            ctx.files.record(ctx, &g.path).await;
            ToolOutput::text(format!("Edited {}\n{diff}", a.path))
        })
    }
}

/// Apply all edits to `raw`, returning the new file text and a compact diff.
pub fn apply_edits(raw: &str, edits: &[(String, String)]) -> Result<(String, String), String> {
    let (bom, body) = raw.strip_prefix(BOM).map_or(("", raw), |b| (BOM, b));
    let crlf = body.contains("\r\n");
    let text = if crlf {
        body.replace("\r\n", "\n")
    } else {
        body.to_string()
    };
    let mut ranges: Vec<(Range<usize>, String)> = Vec::new();
    for (i, (old, new)) in edits.iter().enumerate() {
        let (old, new) = (old.replace("\r\n", "\n"), new.replace("\r\n", "\n"));
        if old.is_empty() {
            return Err(format!("edit {}: old_text is empty", i + 1));
        }
        let r = find_unique(&text, &old).map_err(|e| format!("edit {}: {e}", i + 1))?;
        ranges.push((r, new));
    }
    ranges.sort_by_key(|(r, _)| r.start);
    if ranges.windows(2).any(|w| w[0].0.end > w[1].0.start) {
        return Err("two edits overlap; merge them into one".into());
    }
    let mut out = text.clone();
    let mut diff = String::new();
    for (r, new) in ranges.iter().rev() {
        let line = text[..r.start].matches('\n').count() + 1;
        let mut hunk = String::new();
        for l in text[r.clone()].lines() {
            hunk.push_str(&format!("{line}- {l}\n"));
        }
        for l in new.lines() {
            hunk.push_str(&format!("{line}+ {l}\n"));
        }
        diff.insert_str(0, &hunk);
        out.replace_range(r.clone(), new);
    }
    let out = if crlf { out.replace('\n', "\r\n") } else { out };
    Ok((format!("{bom}{out}"), diff))
}

fn find_unique(text: &str, old: &str) -> Result<Range<usize>, String> {
    let exact: Vec<usize> = text.match_indices(old).map(|(i, _)| i).collect();
    match exact.len() {
        1 => return Ok(exact[0]..exact[0] + old.len()),
        n if n > 1 => {
            return Err(format!(
                "old_text matches {n} places; add surrounding lines to make it unique"
            ));
        }
        _ => {}
    }
    let fuzzy = fuzzy_lines(text, old);
    match fuzzy.len() {
        1 => Ok(fuzzy[0].clone()),
        0 => Err("old_text not found (read the file again to see its current content)".into()),
        n => Err(format!(
            "old_text matches {n} places (whitespace-insensitive); add surrounding lines"
        )),
    }
}

fn norm(line: &str) -> String {
    line.trim_end()
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2007}' | '\u{202F}' => ' ',
            c => c,
        })
        .collect()
}

/// Byte ranges of whole-line windows whose normalised lines equal `old`'s.
fn fuzzy_lines(text: &str, old: &str) -> Vec<Range<usize>> {
    let want: Vec<String> = old.trim_end_matches('\n').split('\n').map(norm).collect();
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let lines: Vec<&str> = text.split('\n').collect();
    let mut found = Vec::new();
    for i in 0..lines.len().saturating_sub(want.len() - 1) {
        if (0..want.len()).all(|k| norm(lines[i + k]) == want[k]) {
            let last = i + want.len() - 1;
            let mut end = starts[last] + lines[last].len();
            if old.ends_with('\n') && end < text.len() {
                end += 1;
            }
            found.push(starts[i]..end);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ctx, text};
    use serde_json::json;

    fn e(o: &str, n: &str) -> (String, String) {
        (o.into(), n.into())
    }

    #[test]
    fn exact_multi_crlf_bom() {
        let src = "\u{feff}a\r\nb\r\nc\r\n";
        let (out, diff) = apply_edits(src, &[e("a", "A"), e("c\n", "C\nD\n")]).unwrap();
        assert_eq!(out, "\u{feff}A\r\nb\r\nC\r\nD\r\n");
        assert!(diff.starts_with("1- a\n1+ A\n3- c\n"));
    }

    #[test]
    fn fuzzy_and_failures() {
        let src = "fn x() {   \n    say(\u{201C}hi\u{201D});\n}\n";
        let (out, _) = apply_edits(src, &[e("fn x() {\n    say(\"hi\");", "fn y() {\n    say(\"yo\");")]).unwrap();
        assert_eq!(out, "fn y() {\n    say(\"yo\");\n}\n");
        assert!(apply_edits("a a", &[e("a", "b")]).unwrap_err().contains("2 places"));
        assert!(apply_edits("abc", &[e("zz", "b")]).unwrap_err().contains("not found"));
        assert!(
            apply_edits("abcd", &[e("abc", "x"), e("cd", "y")])
                .unwrap_err()
                .contains("overlap")
        );
    }

    #[tokio::test]
    async fn all_or_nothing_on_disk() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f"), "one\ntwo\n").unwrap();
        let c = ctx(d.path());
        let bad = json!({"path": "f", "edits": [{"old_text": "one", "new_text": "1"}, {"old_text": "nope", "new_text": "x"}]});
        assert!(Edit.execute(bad, &c).await.is_error);
        assert_eq!(std::fs::read_to_string(d.path().join("f")).unwrap(), "one\ntwo\n");
        let ok = json!({"path": "f", "edits": [{"old_text": "two", "new_text": "2"}]});
        let o = Edit.execute(ok, &c).await;
        assert_eq!(text(&o), "Edited f\n2- two\n2+ 2\n");
    }
}
