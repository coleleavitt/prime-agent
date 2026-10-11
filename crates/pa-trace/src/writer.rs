//! The background writer: records are handed off through a bounded queue and
//! written by one worker thread, so no traced code path (paint included)
//! ever waits on the disk. The worker starts with the first record; a
//! process that records nothing starts no thread and opens no file.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

use crate::log_file::RotatingLog;

/// Queued lines before new ones are dropped (diagnostics are best-effort and
/// must never block the caller).
const QUEUE_BOUND: usize = 16_384;

enum Message {
    Line(String),
    Flush(SyncSender<()>),
}

/// The handle traced code writes through.
pub(crate) struct LogWriter {
    log: RotatingLog,
    sender: OnceLock<Option<SyncSender<Message>>>,
}

impl LogWriter {
    pub(crate) fn new(log: RotatingLog) -> Self {
        LogWriter {
            log,
            sender: OnceLock::new(),
        }
    }

    /// Queue one line; never blocks. A full queue or a writer that could not
    /// start drops the line.
    pub(crate) fn write(&self, line: String) {
        if let Some(sender) = self.sender() {
            let _ = sender.try_send(Message::Line(line));
        }
    }

    /// Wait until every line queued before this call is on disk, at most
    /// `timeout`. Answers whether the writer caught up in time.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let Some(Some(sender)) = self.sender.get() else {
            // Nothing was ever written.
            return true;
        };
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        if sender.try_send(Message::Flush(done_tx)).is_err() {
            return false;
        }
        done_rx.recv_timeout(timeout).is_ok()
    }

    fn sender(&self) -> Option<&SyncSender<Message>> {
        self.sender
            .get_or_init(|| {
                let (sender, receiver) = mpsc::sync_channel(QUEUE_BOUND);
                let log = self.log.clone();
                std::thread::Builder::new()
                    .name("pa-trace-writer".to_string())
                    .spawn(move || run_writer(&log, &receiver))
                    .ok()
                    .map(|_| sender)
            })
            .as_ref()
    }
}

/// Write queued lines in batches (one lock per batch) until every sender is
/// gone. Flush acknowledgements go out once everything before them is
/// written.
fn run_writer(log: &RotatingLog, receiver: &Receiver<Message>) {
    let mut batch: Vec<String> = Vec::new();
    let mut acks: Vec<SyncSender<()>> = Vec::new();
    while let Ok(first) = receiver.recv() {
        let mut next = Some(first);
        while let Some(message) = next {
            match message {
                Message::Line(line) => batch.push(line),
                Message::Flush(ack) => acks.push(ack),
            }
            next = receiver.recv_timeout(Duration::ZERO).ok();
        }
        // Best-effort: a read-only or vanished log directory drops the batch.
        let _ = log.append(&batch);
        batch.clear();
        for ack in acks.drain(..) {
            let _ = ack.try_send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_waits_for_every_queued_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.jsonl");
        let writer = LogWriter::new(RotatingLog::with_limits(path.clone(), 1 << 20, 5));
        assert!(writer.flush(Duration::from_secs(5)), "idle writer flushes");
        assert!(!path.exists(), "no record, no file");
        let lines: Vec<String> = (0..500).map(|index| format!("line {index}")).collect();
        for line in &lines {
            writer.write(line.clone());
        }
        assert!(writer.flush(Duration::from_secs(30)));
        let written: Vec<String> = std::fs::read_to_string(&path)
            .expect("log")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(written, lines);
    }
}
