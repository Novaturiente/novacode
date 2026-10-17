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
    pub fn looks_like_text_tool_call(text: &str) -> bool {
        ["<invoke name=", "<function_calls>", "<tool_call>", "to=functions."]
            .iter()
            .any(|m| text.contains(m))
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
        assert!(Recovery::looks_like_text_tool_call("ok <invoke name=\"read\">"));
    }
}
