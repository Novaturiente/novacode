//! Recovery policy (L3.4, L3.8): per-run caps on each kind of nudge, kept out of the loop.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    /// Tool call written as text instead of a real call.
    TextToolCall,
    /// Empty reply right after tool results.
    EmptyAfterTools,
    /// Reply cut off by the output limit.
    CutOff,
    /// A batch where every call was rejected before running.
    Malformed,
}

impl Nudge {
    fn cap(self) -> u8 {
        match self {
            Nudge::TextToolCall => 2,
            Nudge::EmptyAfterTools => 1,
            Nudge::CutOff => 2,
            Nudge::Malformed => 3,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Nudge::TextToolCall => {
                "You wrote a tool call as text, so it did not run. Use the tool-calling interface to call tools."
            }
            Nudge::EmptyAfterTools => "Your last reply was empty. Continue the task, or tell the user the result.",
            Nudge::CutOff => "Your reply was cut off by the output limit. Continue exactly where you stopped.",
            Nudge::Malformed => "",
        }
    }

    /// The cut-off nudge, quoting where `reply` stopped: without the spot, models
    /// often start the whole reply (a long JSON document) over.
    pub fn cut_off(reply: &str) -> String {
        let tail: String = {
            let rev: Vec<char> = reply.chars().rev().take(80).collect();
            rev.into_iter().rev().collect()
        };
        if tail.trim().is_empty() {
            return Nudge::CutOff.text().into();
        }
        format!(
            "Your reply was cut off by the output limit. It ended with:\n<tail>{tail}</tail>\n\
             Continue from the very next character. Do not repeat anything already written, \
             do not start over, and add no preamble: your text is joined directly to the end above."
        )
    }
}

#[derive(Debug, Default)]
pub struct Recovery {
    used: [u8; 4],
}

impl Recovery {
    /// Count one use; false once the cap is reached.
    pub fn allow(&mut self, n: Nudge) -> bool {
        let slot = &mut self.used[n as usize];
        if *slot >= n.cap() {
            return false;
        }
        *slot += 1;
        true
    }

    // shortcut: detect only; parsing text-written calls into real ones waits until a model family needs it (M5).
    // A marker counts only at the start of a line outside fenced code, so replies that quote markup aren't nudged.
    pub fn looks_like_text_tool_call(text: &str) -> bool {
        const MARKERS: [&str; 4] = ["<invoke name=", "<function_calls>", "<tool_call>", "to=functions."];
        let mut fence: Option<&str> = None;
        for line in text.lines() {
            let t = line.trim_start();
            if let Some(f) = fence {
                if t.starts_with(f) {
                    fence = None;
                }
                continue;
            }
            if let Some(f) = ["```", "~~~"].into_iter().find(|f| t.starts_with(f)) {
                fence = Some(f);
                continue;
            }
            if MARKERS.iter().any(|m| t.starts_with(m)) {
                return true;
            }
            // Harmony leaks mid-line (`…commentary to=functions.read <|message|>{…}`): a call
            // name followed by its envelope counts anywhere outside inline code.
            static HARMONY: std::sync::LazyLock<regex::Regex> =
                std::sync::LazyLock::new(|| regex::Regex::new(r"to=functions\.[A-Za-z_][\w.-]*\s*(<\||\{)").unwrap());
            let prose: String = t.split('`').step_by(2).collect();
            if HARMONY.is_match(&prose) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps() {
        let mut r = Recovery::default();
        assert!(r.allow(Nudge::EmptyAfterTools) && !r.allow(Nudge::EmptyAfterTools));
        assert!(r.allow(Nudge::CutOff) && r.allow(Nudge::CutOff) && !r.allow(Nudge::CutOff));
        assert!((0..3).all(|_| r.allow(Nudge::Malformed)) && !r.allow(Nudge::Malformed));
        assert!(Recovery::looks_like_text_tool_call("ok\n<invoke name=\"read\">"));
    }

    #[test]
    fn cut_off_quotes_the_last_80_chars() {
        let reply = format!("{{\"a\": \"{}énd", "x".repeat(200));
        let n = Nudge::cut_off(&reply);
        let tail = n.split("<tail>").nth(1).unwrap().split("</tail>").next().unwrap();
        assert_eq!(tail.chars().count(), 80);
        assert!(reply.ends_with(tail) && n.contains("do not start over"));
        assert_eq!(Nudge::cut_off("  "), Nudge::CutOff.text());
    }

    #[test]
    fn text_tool_call_only_at_line_start_outside_code() {
        assert!(Recovery::looks_like_text_tool_call(
            "<function_calls>\n<invoke name=\"read\">"
        ));
        assert!(Recovery::looks_like_text_tool_call("  <tool_call>{}"));
        assert!(Recovery::looks_like_text_tool_call("to=functions.read {}"));
        // Prose mid-line.
        assert!(!Recovery::looks_like_text_tool_call(
            "the check matches <invoke name= in text"
        ));
        // Inline code.
        assert!(!Recovery::looks_like_text_tool_call("`<tool_call>` is a marker"));
        // Fenced code, both fence styles.
        assert!(!Recovery::looks_like_text_tool_call(
            "see:\n```xml\n<invoke name=\"read\">\n```\ndone"
        ));
        assert!(!Recovery::looks_like_text_tool_call("~~~\n<function_calls>\n~~~"));
        // After a closed fence, a line-start marker counts again.
        assert!(Recovery::looks_like_text_tool_call("```\nx\n```\n<tool_call>"));
        // Harmony leaks mid-line with its envelope; a mention of the prefix doesn't count.
        assert!(Recovery::looks_like_text_tool_call(
            "<|channel|>commentary to=functions.read <|constrain|>json<|message|>{}"
        ));
        assert!(Recovery::looks_like_text_tool_call(
            "ok to=functions.bash {\"command\": \"ls\"}"
        ));
        assert!(!Recovery::looks_like_text_tool_call(
            "the to=functions. prefix is matched"
        ));
        assert!(!Recovery::looks_like_text_tool_call("`to=functions.read {}` is a leak"));
    }
}
