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
    /// Bytes of `buf` already searched for a line end, so a long line isn't rescanned per chunk.
    scanned: usize,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        // A line ends at `\r\n`, `\n` or a lone `\r` (SSE spec).
        while let Some(i) = self.buf[self.scanned..].iter().position(|&b| b == b'\n' || b == b'\r') {
            let i = self.scanned + i;
            if self.buf[i] == b'\r' && i + 1 == self.buf.len() {
                self.scanned = i; // its `\n` may come in the next chunk
                return out;
            }
            let end = if self.buf[i] == b'\r' && self.buf[i + 1] == b'\n' {
                i + 2
            } else {
                i + 1
            };
            let line: Vec<u8> = self.buf.drain(..end).take(i).collect();
            self.scanned = 0;
            self.line(&String::from_utf8_lossy(&line), &mut out);
        }
        self.scanned = self.buf.len();
        out
    }

    /// Flush a final event that wasn't followed by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut out = Vec::new();
        self.scanned = 0;
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

    #[test]
    fn lone_cr_ends_lines_and_split_crlf_counts_once() {
        let mut p = SseParser::default();
        let mut got = p.push(b"event: a\rdata: 1\r\rdata: 2\r");
        got.extend(p.push(b"\n\r\n"));
        let got: Vec<_> = got.iter().map(|e| (e.event.as_str(), e.data.as_str())).collect();
        assert_eq!(got, [("a", "1"), ("", "2")]);
    }
}
