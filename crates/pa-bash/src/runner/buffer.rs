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
    tail: VecDeque<Vec<u8>>,
    tail_size: usize,
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
        self.tail.push_back(chunk.to_vec());
        self.tail_size += chunk.len();
        // Trim the oldest chunk instead of dropping it whole, so exactly
        // TAIL_CAP bytes stay.
        while self.tail_size > TAIL_CAP {
            let excess = self.tail_size - TAIL_CAP;
            let Some(oldest) = self.tail.front_mut() else {
                break;
            };
            if oldest.len() <= excess {
                let removed = oldest.len();
                self.tail.pop_front();
                self.tail_size -= removed;
                self.dropped += removed as u64;
            } else {
                oldest.drain(..excess);
                self.tail_size -= excess;
                self.dropped += excess as u64;
            }
        }
    }

    /// The resident bytes.
    pub(crate) fn size(&self) -> usize {
        self.head.len() + self.tail_size
    }

    /// Every byte ever written, the dropped middle included: watchers report
    /// ranges over this stream offset, which keeps growing past the caps.
    pub(crate) fn total(&self) -> u64 {
        self.size() as u64 + self.dropped
    }

    /// The output as text (invalid UTF-8 replaced), with a marker where the
    /// middle was dropped.
    pub(crate) fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().flatten().copied().collect();
        if self.dropped == 0 {
            let mut all = self.head.clone();
            all.extend_from_slice(&tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        format!(
            "{}\n... [{} bytes dropped] ...\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            String::from_utf8_lossy(&tail)
        )
    }
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
        assert_eq!(buffer.tail_size, TAIL_CAP);
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
