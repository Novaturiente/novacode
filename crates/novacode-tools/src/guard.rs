//! Output guards (L5.3), applied once when the result is created (L1.7):
//! 1. cut at 2000 lines / 50 KB, keeping head or tail, full text in a spill file;
//! 2. refuse output too big for the context unless `accept_large_output`.

use crate::Keep;
use std::path::Path;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const ACCEPT_LARGE: &str = "accept_large_output";

/// Context numbers the refusal guard needs.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub window_tokens: u64,
    pub used_tokens: u64,
}

pub fn estimate_tokens(text: &str) -> u64 {
    novacode_types::estimate::text(text)
}

/// Cut `text` to the limits. Returns the text to send; when cut, the full text
/// is written to `spill` and a hint says where.
pub fn truncate(text: &str, keep: Keep, spill: &Path) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return text.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    let iter: Box<dyn Iterator<Item = &&str>> = if keep == Keep::Head {
        Box::new(lines.iter())
    } else {
        Box::new(lines.iter().rev())
    };
    for l in iter {
        if kept.len() == MAX_LINES || bytes + l.len() > MAX_BYTES {
            break;
        }
        bytes += l.len();
        kept.push(l);
    }
    if keep == Keep::Tail {
        kept.reverse();
    }
    let spilled = spill
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::write(spill, text));
    let where_ = match spilled {
        Ok(()) => format!("Full output: {}", spill.display()),
        Err(e) => format!("Full output could not be saved: {e}"),
    };
    let body: String = kept.concat();
    let (shown, total) = (kept.len(), lines.len());
    match keep {
        Keep::Head => format!("{body}\n[Showing lines 1-{shown} of {total}. {where_}]"),
        Keep::Tail => format!("[Showing the last {shown} of {total} lines. {where_}]\n{body}"),
    }
}

/// `Err(message)` when the output must be refused instead of sent.
pub fn check_budget(text: &str, budget: Budget, accept_large: bool) -> Result<String, String> {
    let tokens = estimate_tokens(text);
    let w = budget.window_tokens;
    let after = budget.used_tokens + tokens;
    let too_big = tokens * 10 > w * 3 || tokens > 50_000;
    let pct = |n: u64| n * 100 / w.max(1);
    if after * 10 > w * 9 {
        // Past 90% even acceptance can't help: the model must shrink the context first.
        return Err(format!(
            "Output refused: ~{}k tokens would bring the context to {}% of the window. Narrow the query (offset/limit, a more specific pattern) or ask the user to compact.",
            tokens / 1000,
            pct(after)
        ));
    }
    if too_big && !accept_large {
        return Err(format!(
            "Output refused: ~{}k tokens ({}% of the window). Narrow the query, or call again with {ACCEPT_LARGE}: true if you need all of it.",
            tokens / 1000,
            pct(tokens)
        ));
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_head_and_tail_with_spill() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let spill = dir.path().join("s.txt");
        let head = truncate(&text, Keep::Head, &spill);
        assert!(head.starts_with("line 0\n") && head.contains("[Showing lines 1-2000 of 3000."));
        assert_eq!(std::fs::read_to_string(&spill).unwrap(), text);
        let tail = truncate(&text, Keep::Tail, &spill);
        assert!(tail.ends_with("line 2999\n") && !tail.contains("line 999\n"));
        assert_eq!(truncate("short", Keep::Head, &spill), "short");
    }

    #[test]
    fn budget_refusals() {
        let big = "x".repeat(4 * 40_000); // ~40k tokens
        let b = Budget {
            window_tokens: 100_000,
            used_tokens: 0,
        };
        assert!(check_budget(&big, b, false).unwrap_err().contains(ACCEPT_LARGE));
        assert!(check_budget(&big, b, true).is_ok());
        let full = Budget {
            window_tokens: 100_000,
            used_tokens: 89_000,
        };
        assert!(
            check_budget(&"x".repeat(8000), full, true)
                .unwrap_err()
                .contains("compact")
        );
        assert!(check_budget("small", b, false).is_ok());
    }
}
