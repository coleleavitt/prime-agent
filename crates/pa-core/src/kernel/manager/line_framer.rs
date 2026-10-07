//! Newline framing for the kernel's stdout protocol: bounded, and linear in the bytes read.

use std::ops::ControlFlow;

/// What one [`ProtocolLineFramer::push`] settled into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushOutcome {
    /// Every complete line was handed out; any newline-free tail stays buffered.
    Continue,
    /// The buffered line passed the ceiling: the buffer was dropped, the stream is poisoned.
    Oversized,
    /// The line handler asked to stop reading.
    Stop,
}

/// Splits the protocol byte stream into lines. Only the newline-free tail of earlier reads is kept,
/// so each pushed byte is scanned once however long a line grows.
#[derive(Debug)]
pub(super) struct ProtocolLineFramer {
    buffered: Vec<u8>,
    max_line_bytes: usize,
    /// Bytes examined while looking for newlines (the linear-scan invariant's test witness).
    #[cfg(test)]
    scanned: usize,
}

impl ProtocolLineFramer {
    pub(super) fn new(max_line_bytes: usize) -> Self {
        Self {
            buffered: Vec::new(),
            max_line_bytes,
            #[cfg(test)]
            scanned: 0,
        }
    }

    /// Append one read and hand every complete line (without its `\n`) to `on_line`.
    pub(super) fn push(
        &mut self,
        bytes: &[u8],
        mut on_line: impl FnMut(&[u8]) -> ControlFlow<()>,
    ) -> PushOutcome {
        // `buffered` holds no newline, so only the new bytes can.
        let mut scan_from = self.buffered.len();
        self.buffered.extend_from_slice(bytes);
        if self.buffered.len() > self.max_line_bytes {
            self.buffered.clear();
            return PushOutcome::Oversized;
        }
        // Consume by offset and drain the prefix once per read: per-line drains would shift the
        // tail each iteration (quadratic copying).
        let mut consumed = 0;
        while let Some(relative) = self.buffered[scan_from..]
            .iter()
            .position(|&byte| byte == b'\n')
        {
            let end = scan_from + relative;
            self.note_scanned(relative + 1);
            let line = &self.buffered[consumed..end];
            consumed = end + 1;
            scan_from = consumed;
            if on_line(line).is_break() {
                return PushOutcome::Stop;
            }
        }
        self.note_scanned(self.buffered.len() - scan_from);
        self.buffered.drain(..consumed);
        PushOutcome::Continue
    }

    #[cfg(test)]
    fn note_scanned(&mut self, bytes: usize) {
        self.scanned += bytes;
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    fn note_scanned(&mut self, _bytes: usize) {}

    #[cfg(test)]
    fn scanned(&self) -> usize {
        self.scanned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `stream` in pipe-sized reads; collect the lines.
    fn frame(framer: &mut ProtocolLineFramer, stream: &[u8]) -> (Vec<Vec<u8>>, PushOutcome) {
        let mut lines = Vec::new();
        let mut outcome = PushOutcome::Continue;
        for read in stream.chunks(64 * 1024) {
            outcome = framer.push(read, |line| {
                lines.push(line.to_vec());
                ControlFlow::Continue(())
            });
            if outcome != PushOutcome::Continue {
                break;
            }
        }
        (lines, outcome)
    }

    /// A 31 MiB blank line arrives in 64 KiB reads; rescanning the buffered prefix per read would
    /// examine about 8 GB. Each byte is examined exactly once.
    #[test]
    fn a_multi_mib_line_is_scanned_once() {
        let frame_line = br#"{"event":"stdout","text":"big"}"#;
        let mut stream = vec![b' '; 31 * 1024 * 1024];
        stream.push(b'\n');
        stream.extend_from_slice(frame_line);
        stream.push(b'\n');
        let mut framer = ProtocolLineFramer::new(32 * 1024 * 1024);
        let (lines, outcome) = frame(&mut framer, &stream);
        let blank_line = lines
            .first()
            .is_some_and(|line| line.iter().all(|&b| b == b' '));
        let lengths: Vec<usize> = lines.iter().map(Vec::len).collect();
        assert_eq!(
            (lengths, blank_line, lines.last(), outcome, framer.scanned()),
            (
                vec![31 * 1024 * 1024, frame_line.len()],
                true,
                Some(&frame_line.to_vec()),
                PushOutcome::Continue,
                stream.len()
            )
        );
    }

    #[test]
    fn lines_split_across_reads_and_a_partial_tail_stay_buffered() {
        let mut framer = ProtocolLineFramer::new(1024);
        let mut lines = Vec::new();
        for read in [&b"a\nb"[..], b"c\n\nd", b"e"] {
            let outcome = framer.push(read, |line| {
                lines.push(String::from_utf8(line.to_vec()).unwrap());
                ControlFlow::Continue(())
            });
            assert_eq!(outcome, PushOutcome::Continue);
        }
        assert_eq!(lines, ["a", "bc", ""]);
        assert_eq!(framer.buffered, b"de");
    }

    #[test]
    fn a_line_past_the_ceiling_poisons_and_drops_the_buffer() {
        let mut framer = ProtocolLineFramer::new(8);
        let (lines, outcome) = frame(&mut framer, b"ok\n0123456789");
        assert_eq!(
            (lines, outcome),
            (Vec::<Vec<u8>>::new(), PushOutcome::Oversized)
        );
        assert!(framer.buffered.is_empty());
    }

    #[test]
    fn a_stopping_handler_ends_the_push() {
        let mut framer = ProtocolLineFramer::new(1024);
        let mut seen = Vec::new();
        let outcome = framer.push(b"one\ntwo\nthree\n", |line| {
            seen.push(line.to_vec());
            ControlFlow::Break(())
        });
        assert_eq!((seen, outcome), (vec![b"one".to_vec()], PushOutcome::Stop));
    }
}
