//! Server-Sent Events parser shared by every adapter (L2.2).

/// One SSE event. `event` is empty when the server sent no `event:` line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Incremental parser: feed raw bytes as they arrive, get whole events out.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    cur: SseEvent,
    has_data: bool,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut out);
        }
        out
    }

    /// Flush a final event that wasn't followed by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            self.line(rest.trim_end_matches('\r'), &mut out);
        }
        self.line("", &mut out);
        out.pop()
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.has_data {
                out.push(std::mem::take(&mut self.cur));
            }
            self.cur = SseEvent::default();
            self.has_data = false;
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.cur.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.cur.data.push('\n');
                }
                self.cur.data.push_str(value);
                self.has_data = true;
            }
            _ => {} // id, retry: unused
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_chunks_crlf_comments_multiline() {
        let mut p = SseParser::default();
        let mut got = p.push(b": ping\r\nevent: a\r\nda");
        assert!(got.is_empty());
        got.extend(p.push(b"ta: {\"x\":1}\r\n\r\ndata: l1\ndata: l2\n\nevent: b\ndata:"));
        got.extend(p.finish());
        let want = [("a", "{\"x\":1}"), ("", "l1\nl2"), ("b", "")];
        let got: Vec<_> = got.iter().map(|e| (e.event.as_str(), e.data.as_str())).collect();
        assert_eq!(got, want);
    }
}
