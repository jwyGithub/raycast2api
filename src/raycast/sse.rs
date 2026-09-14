//! Incremental Server-Sent-Events decoder.
//!
//! The Raycast stream arrives as arbitrary byte chunks that don't align to line
//! or event boundaries. This decoder buffers bytes and only emits the payload
//! of complete `data:` lines, so multi-byte UTF-8 characters are never split
//! mid-decode (each full line is itself valid UTF-8).

/// Buffers stream bytes and yields completed `data:` payloads.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw bytes; returns the `data:` payloads (with the
    /// `data:` prefix stripped and trimmed) completed by this chunk. Non-`data:`
    /// lines (`id:`, `event:`, blank keep-alives) are skipped.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if let Some(data) = line.strip_prefix("data:") {
                out.push(data.trim().to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_data_lines_and_skips_others() {
        let mut d = SseDecoder::new();
        let out = d.push_bytes(b"id: 0\ndata: {\"text\":\"hi\"}\n\nevent: complete\n");
        assert_eq!(out, vec!["{\"text\":\"hi\"}".to_string()]);
    }

    #[test]
    fn buffers_partial_lines_across_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push_bytes(b"data: {\"te").is_empty());
        let out = d.push_bytes(b"xt\":\"ok\"}\n");
        assert_eq!(out, vec!["{\"text\":\"ok\"}".to_string()]);
    }

    #[test]
    fn does_not_split_multibyte_utf8_across_chunks() {
        // "你" is E4 BD A0 in UTF-8; split it across two chunks.
        let mut d = SseDecoder::new();
        let full = "data: {\"text\":\"你\"}\n".as_bytes();
        let split = 9; // somewhere inside the multi-byte sequence
        assert!(d.push_bytes(&full[..split]).is_empty());
        let out = d.push_bytes(&full[split..]);
        assert_eq!(out, vec!["{\"text\":\"你\"}".to_string()]);
    }
}
