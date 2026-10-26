//! Incremental `text/event-stream` decoder (RFC-0009 4.6; 0009-044).
//!
//! Only the `data` field is used. Comment lines (for example `: done`) and other
//! fields are ignored. An event ends at a blank line; a final event without a
//! trailing blank line is discarded, as the EventSource algorithm specifies.

/// Decodes SSE bytes into complete event payloads.
#[derive(Debug, Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: String,
    has_data: bool,
}

impl SseDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk and returns the payloads of events completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line, &mut out);
            } else {
                self.line.push(byte);
            }
        }
        out
    }

    fn line_done(&mut self, raw: &[u8], out: &mut Vec<String>) {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            if self.has_data {
                out.push(std::mem::take(&mut self.data));
                self.has_data = false;
            }
            return;
        }
        if line.first() == Some(&b':') {
            return;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (line, &[][..]),
        };
        let value = if value.first() == Some(&b' ') {
            &value[1..]
        } else {
            value
        };
        if field == b"data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(&String::from_utf8_lossy(value));
            self.has_data = true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Frames split across chunks decode to the same payloads.
    #[test]
    fn decodes_frames_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in [
            "data: {\"a\":1}\n",
            "\ndata: {\"b\"",
            ":2}\r\n\r\n: done\n\n",
        ] {
            events.extend(decoder.push(chunk.as_bytes()));
        }
        assert_eq!(events, vec!["{\"a\":1}".to_owned(), "{\"b\":2}".to_owned()]);
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"data: one\ndata: two\n\n");
        assert_eq!(events, vec!["one\ntwo".to_owned()]);
    }

    #[test]
    fn unterminated_final_event_is_discarded() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: partial\n").is_empty());
    }
}
