//! Newline-delimited JSON-RPC framing for ACP stdio (0009-015).

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// Reads frames until EOF, calling `on_frame` for each parsed JSON value.
///
/// Blank lines and lines that are not valid JSON are skipped. Frames split
/// across reads or several frames joined in one read are handled by the
/// line reader.
pub(crate) fn read_frames<R: BufRead>(
    mut reader: R,
    mut on_frame: impl FnMut(Value),
) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            on_frame(value);
        }
    }
}

/// Writes one frame: compact JSON followed by `\n`, then flushes.
pub(crate) fn write_frame<W: Write>(writer: &mut W, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Read};

    use super::*;

    /// Yields one byte per read, so every frame arrives split across reads.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    #[test]
    fn trickled_frames_parse_0009_015() {
        let wire = b"{\"id\":1}\n{\"id\":2}\n\n{\"method\":\"x\",\"params\":{}}\n";
        let mut seen = Vec::new();
        read_frames(BufReader::new(Trickle(wire)), |v| seen.push(v)).unwrap_or_default();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0]["id"], 1);
        assert_eq!(seen[1]["id"], 2);
        assert_eq!(seen[2]["method"], "x");
    }

    #[test]
    fn invalid_lines_are_skipped_0009_015() {
        let wire = b"not json\n{\"id\":7}\n";
        let mut seen = Vec::new();
        read_frames(BufReader::new(&wire[..]), |v| seen.push(v)).unwrap_or_default();
        assert_eq!(seen, vec![serde_json::json!({"id": 7})]);
    }

    #[test]
    fn write_frame_is_one_line_0009_015() {
        let mut out = Vec::new();
        write_frame(&mut out, &serde_json::json!({"jsonrpc": "2.0", "id": 1})).unwrap_or_default();
        assert_eq!(out.last(), Some(&b'\n'));
        assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 1);
    }
}
