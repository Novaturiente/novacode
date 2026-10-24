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
        &[
            "Change files with `edit`, not with sed, perl or scripts run through bash (strictly forbidden unless no tool can do it). Put all changes to one file in one call's `edits`; changes to several files go in parallel calls in one message.",
        ]
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
            // Changed since the last read (a formatter, the model's own bash, the user):
            // each old_text matching exactly once still proves what it replaces, so
            // apply and say so instead of costing a read-and-retry round trip.
            let stale = ctx.files.check_stale(ctx, &g.path).await;
            let bytes = match ctx.env.read(&path).await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", a.path)),
            };
            let Ok(raw) = String::from_utf8(bytes) else {
                return ToolOutput::error(format!("{} is not UTF-8 text", a.path));
            };
            let pairs: Vec<(String, String)> = a.edits.into_iter().map(|e| (e.old_text, e.new_text)).collect();
            let (new, diff, summary) = match apply_edits(&raw, &pairs) {
                Ok(r) => r,
                Err(e) => {
                    let why = stale.map_or(String::new(), |s| format!(" {s}"));
                    return ToolOutput::error(format!("{}: {e}. No edits were applied.{why}", a.path));
                }
            };
            if let Err(e) = ctx.env.write(&path, new.as_bytes()).await {
                return ToolOutput::error(format!("cannot write {}: {e}", a.path));
            }
            ctx.files.record(ctx, &g.path).await;
            let note = if stale.is_some() {
                " (it had changed on disk since your last read; the edits matched its current text)"
            } else {
                ""
            };
            // The model wrote every line of an exact edit, so echoing them back only
            // fills the context; the full diff goes to the screen (details.diff).
            let text = match summary {
                Some(at) => format!("Edited {}{note}: {at}", a.path),
                None => format!(
                    "Edited {}{note} (matched ignoring whitespace/quotes; check the result)\n{diff}",
                    a.path
                ),
            };
            ToolOutput {
                details: Some(serde_json::json!({ "diff": diff })),
                ..ToolOutput::text(text)
            }
        })
    }
}

/// Apply all edits to `raw`, returning the new file text and a compact diff.
/// The third value is a one-line summary for the model, or `None` when a fuzzy
/// match replaced text other than what the model sent (then it needs the diff).
pub fn apply_edits(raw: &str, edits: &[(String, String)]) -> Result<(String, String, Option<String>), String> {
    let (bom, body) = raw.strip_prefix(BOM).map_or(("", raw), |b| (BOM, b));
    let crlf = body.contains("\r\n");
    let text = if crlf {
        body.replace("\r\n", "\n")
    } else {
        body.to_string()
    };
    let mut ranges: Vec<(Range<usize>, String)> = Vec::new();
    let mut fuzzy = false;
    for (i, (old, new)) in edits.iter().enumerate() {
        let (old, new) = (old.replace("\r\n", "\n"), new.replace("\r\n", "\n"));
        if old.is_empty() {
            return Err(format!("edit {}: old_text is empty", i + 1));
        }
        let r = find_unique(&text, &old).map_err(|e| format!("edit {}: {e}", i + 1))?;
        fuzzy |= text[r.clone()] != old;
        ranges.push((r, new));
    }
    ranges.sort_by_key(|(r, _)| r.start);
    if ranges.windows(2).any(|w| w[0].0.end > w[1].0.start) {
        return Err("two edits overlap; merge them into one".into());
    }
    let mut out = text.clone();
    let mut diff = String::new();
    let mut spots = Vec::new();
    for (r, new) in ranges.iter().rev() {
        let line = text[..r.start].matches('\n').count() + 1;
        spots.insert(
            0,
            format!(
                "line {line} (-{} +{})",
                text[r.clone()].lines().count(),
                new.lines().count()
            ),
        );
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
    Ok((format!("{bom}{out}"), diff, (!fuzzy).then(|| spots.join(", "))))
}

fn find_unique(text: &str, old: &str) -> Result<Range<usize>, String> {
    // Overlapping hits count too ("}\n}\n" in "}\n}\n}\n" is 2), so restart one char after each start.
    let mut exact = Vec::new();
    let mut from = 0;
    while let Some(i) = text.get(from..).and_then(|t| t.find(old)) {
        let at = from + i;
        exact.push(at);
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
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
        let (out, diff, summary) = apply_edits(src, &[e("a", "A"), e("c\n", "C\nD\n")]).unwrap();
        assert_eq!(out, "\u{feff}A\r\nb\r\nC\r\nD\r\n");
        assert_eq!(summary.as_deref(), Some("line 1 (-1 +1), line 3 (-1 +2)"));
        assert!(diff.starts_with("1- a\n1+ A\n3- c\n"));
    }

    #[tokio::test]
    async fn edits_in_a_row_and_after_outside_changes() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let c = ctx(d.path());
        let edit = |o: &str, n: &str| json!({"path": "a.txt", "edits": [{"old_text": o, "new_text": n}]});
        crate::read::Read.execute(json!({"path": "a.txt"}), &c).await;
        // Two edits of one file in a row: the second sees the first's own write as seen.
        let r = Edit.execute(edit("one", "1"), &c).await;
        assert!(!r.is_error && !text(&r).contains("changed on disk"), "{}", text(&r));
        assert!(!Edit.execute(edit("two", "2"), &c).await.is_error);
        // Changed behind its back (e.g. `sed -i` through bash), mtime moves.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "1\n2\nTHREE\n").unwrap();
        let r = Edit.execute(edit("THREE", "3"), &c).await;
        assert!(
            !r.is_error && text(&r).contains("changed on disk since your last read"),
            "{}",
            text(&r)
        );
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "1\n2\n3\n");
        // Changed again, and the old text is gone: refused, with both reasons.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "x\n").unwrap();
        let r = Edit.execute(edit("3", "three"), &c).await;
        assert!(
            r.is_error && text(&r).contains("not found") && text(&r).contains("Read it again"),
            "{}",
            text(&r)
        );
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\n");
    }

    #[test]
    fn fuzzy_and_failures() {
        let src = "fn x() {   \n    say(\u{201C}hi\u{201D});\n}\n";
        let (out, _, summary) =
            apply_edits(src, &[e("fn x() {\n    say(\"hi\");", "fn y() {\n    say(\"yo\");")]).unwrap();
        assert_eq!(summary, None, "fuzzy: the model gets the diff");
        assert_eq!(out, "fn y() {\n    say(\"yo\");\n}\n");
        assert!(apply_edits("a a", &[e("a", "b")]).unwrap_err().contains("2 places"));
        assert!(apply_edits("abc", &[e("zz", "b")]).unwrap_err().contains("not found"));
        assert!(
            apply_edits("}\n}\n}\n", &[e("}\n}\n", "x")])
                .unwrap_err()
                .contains("matches 2 places"),
            "overlapping matches count"
        );
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
        assert_eq!(text(&o), "Edited f: line 2 (-1 +1)");
        assert_eq!(
            o.details.unwrap()["diff"],
            "2- two\n2+ 2\n",
            "the screen still gets the diff"
        );
    }
}
