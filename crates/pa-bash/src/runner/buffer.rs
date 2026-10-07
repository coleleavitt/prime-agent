//! A command's retained output: the first [`HEAD_CAP`] bytes plus a rolling
//! [`TAIL_CAP`]-byte tail; the middle is dropped and counted.

use std::collections::VecDeque;

/// Bytes kept from the start of the stream.
pub(crate) const HEAD_CAP: usize = 512 * 1024;
/// Bytes kept from the end of the stream.
pub(crate) const TAIL_CAP: usize = 3 * 512 * 1024;

#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    head: Vec<u8>,
    /// A ring of the latest bytes past the head: trimming its front moves
    /// nothing, so a long stream costs one copy per byte.
    tail: VecDeque<u8>,
    dropped: u64,
}

impl OutputBuffer {
    pub(crate) fn write(&mut self, mut chunk: &[u8]) {
        if self.head.len() < HEAD_CAP {
            let take = (HEAD_CAP - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if chunk.is_empty() {
            return;
        }
        // Bytes that would fall out of the tail at once never enter it.
        let skip = chunk.len().saturating_sub(TAIL_CAP);
        self.dropped += skip as u64;
        chunk = &chunk[skip..];
        let excess = (self.tail.len() + chunk.len()).saturating_sub(TAIL_CAP);
        self.tail.drain(..excess);
        self.dropped += excess as u64;
        self.tail.extend(chunk);
    }

    /// Keep only the last `keep` resident bytes (the buffer of a stream that
    /// has ended): `total` is unchanged, and `text` renders them after the
    /// drop marker.
    pub(crate) fn compact(&mut self, keep: usize) {
        if self.size() <= keep {
            return;
        }
        let total = self.total();
        let (front, back) = self.tail.as_slices();
        let all = [self.head.as_slice(), front, back].concat();
        self.tail = VecDeque::from(all[all.len() - keep..].to_vec());
        self.head = Vec::new();
        self.dropped = total - keep as u64;
    }

    /// The resident bytes.
    pub(crate) fn size(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// Every byte ever written, the dropped middle included: watchers report
    /// ranges over this stream offset, which keeps growing past the caps.
    pub(crate) fn total(&self) -> u64 {
        self.size() as u64 + self.dropped
    }

    /// The output as text (invalid UTF-8 replaced), with a marker where the
    /// middle was dropped.
    pub(crate) fn text(&self) -> String {
        let (front, back) = self.tail.as_slices();
        if self.dropped == 0 {
            return lossy([self.head.as_slice(), front, back].concat());
        }
        format!(
            "{}\n... [{} bytes dropped] ...\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            lossy([front, back].concat())
        )
    }
}

/// `bytes` as text, invalid UTF-8 replaced (validated in one pass first: the
/// common, valid case keeps the allocation).
fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python `test_buffer_tail_retention_is_exact`.
    #[test]
    fn tail_retention_is_exact() {
        let mut buffer = OutputBuffer::default();
        buffer.write(&vec![b'x'; HEAD_CAP]);
        buffer.write(&vec![b'a'; TAIL_CAP]);
        buffer.write(&[b'b'; 1000]);
        assert_eq!(buffer.tail.len(), TAIL_CAP);
        let text = buffer.text();
        assert!(text.ends_with(&"b".repeat(1000)));
        assert!(text.contains(&format!("{}{}", "a".repeat(1000), "b".repeat(1000))));
        assert!(text.contains("\n... [1000 bytes dropped] ...\n"));
    }

    /// Python `test_stream_bytes_grow_past_the_buffer_caps`.
    #[test]
    fn stream_bytes_grow_past_the_caps() {
        let mut buffer = OutputBuffer::default();
        let total = HEAD_CAP + TAIL_CAP + 100_000;
        buffer.write(&vec![b'x'; total]);
        assert_eq!(buffer.total(), total as u64);
        assert!(buffer.size() < total);
        buffer.write(&[b'y'; 5_000]);
        assert_eq!(buffer.total(), (total + 5_000) as u64);
    }

    #[test]
    fn small_output_is_whole_and_lossy() {
        let mut buffer = OutputBuffer::default();
        buffer.write(b"ok \xff\n");
        assert_eq!(buffer.text(), "ok \u{fffd}\n");
    }
}
