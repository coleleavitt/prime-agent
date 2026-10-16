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

/// The output after a stream offset (a line follower's next read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputSince {
    /// The resident bytes from `from`, ending on a character boundary.
    pub(crate) bytes: Vec<u8>,
    /// The stream offset the bytes start at (past any dropped middle).
    pub(crate) from: u64,
    /// The offset after the last returned byte: the next read's cursor.
    pub(crate) to: u64,
    /// Bytes between the requested offset and `from` the buffer no longer
    /// holds (the dropped middle, or a tail trimmed past the reader).
    pub(crate) skipped: u64,
}

impl OutputBuffer {
    /// At most `max` resident bytes after stream offset `offset`, in order:
    /// a reader that fell behind the retained bytes skips to the oldest it
    /// still has (counted in `skipped`). The end is cut back to a complete
    /// UTF-8 character, so the next read resumes on the rest of it.
    pub(crate) fn since(&self, offset: u64, max: usize) -> OutputSince {
        let head_end = self.head.len() as u64;
        let tail_start = head_end + self.dropped;
        let (front, back) = self.tail.as_slices();
        let tail: Vec<u8> = [front, back].concat();
        let segments: [(u64, &[u8]); 2] = [(0, &self.head), (tail_start, &tail)];
        let mut bytes = Vec::new();
        let mut cursor = offset;
        let mut from = None;
        let mut skipped = 0;
        for (start, segment) in segments {
            let end = start + segment.len() as u64;
            if cursor >= end || bytes.len() >= max {
                continue;
            }
            if cursor < start {
                if from.is_none() {
                    skipped += start - cursor;
                } else {
                    // A gap inside the read: stop before it; the next read
                    // reports the skip.
                    break;
                }
                cursor = start;
            }
            let at = usize::try_from(cursor - start).unwrap_or(usize::MAX);
            let take = (segment.len() - at).min(max - bytes.len());
            from.get_or_insert(cursor);
            bytes.extend_from_slice(&segment[at..at + take]);
            cursor += take as u64;
        }
        let complete = match std::str::from_utf8(&bytes) {
            // An incomplete character at the end waits for its rest; an
            // invalid byte inside stays (the reader replaces it).
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Ok(_) | Err(_) => bytes.len(),
        };
        bytes.truncate(complete);
        let from = from.unwrap_or(cursor.min(self.total()));
        OutputSince {
            to: from + bytes.len() as u64,
            bytes,
            from,
            skipped,
        }
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

    /// A line follower reads the stream in order from its cursor, resumes
    /// a split character on the next read, and skips (counting) what the
    /// buffer dropped.
    #[test]
    fn since_follows_the_stream_from_a_cursor() {
        let mut buffer = OutputBuffer::default();
        buffer.write(b"one\ntwo\n");
        let read = buffer.since(0, 1024);
        assert_eq!(
            (read.bytes.as_slice(), read.from, read.to, read.skipped),
            (&b"one\ntwo\n"[..], 0, 8, 0)
        );
        assert_eq!(buffer.since(8, 1024).bytes, b"");
        // A split character waits for its last byte.
        buffer.write(b"\xc3");
        let read = buffer.since(8, 1024);
        assert_eq!((read.bytes.len(), read.to), (0, 8));
        buffer.write(b"\xa9!");
        assert_eq!(buffer.since(8, 1024).bytes, "\u{e9}!".as_bytes());
        // The cap bounds one read; the next continues.
        let read = buffer.since(0, 4);
        assert_eq!((read.bytes.as_slice(), read.to), (&b"one\n"[..], 4));

        // Past the caps, a reader behind the tail skips the dropped middle.
        let mut big = OutputBuffer::default();
        big.write(&vec![b'h'; HEAD_CAP]);
        big.write(&vec![b'm'; TAIL_CAP + 10]);
        big.write(b"end\n");
        let behind = (HEAD_CAP + 2) as u64;
        let read = big.since(behind, 1024);
        assert_eq!(read.from, big.total() - TAIL_CAP as u64);
        assert_eq!(read.skipped, read.from - behind);
        let last = big.since(big.total() - 4, 1024);
        assert_eq!(last.bytes, b"end\n");
    }

    #[test]
    fn small_output_is_whole_and_lossy() {
        let mut buffer = OutputBuffer::default();
        buffer.write(b"ok \xff\n");
        assert_eq!(buffer.text(), "ok \u{fffd}\n");
    }
}
