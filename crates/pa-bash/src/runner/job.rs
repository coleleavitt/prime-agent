//! One running command: its output pump, its status reporter, its exit
//! watcher, and the event stream a kernel follows.
//!
//! The result is final at foreground completion, read from the status channel
//! (so `cmd &` does not hang the await); a shell that dies without writing a
//! status (an early `exit`, `exec`, a fatal signal) finalizes from its exit
//! code instead. Either way the job is *reaped* only once its whole process
//! group is gone: the leader's exit kills any members it left behind, and only
//! a delivered kill retires the orphan-journal record.

use std::io::{PipeReader, PipeWriter, Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Map, Value};

use super::buffer::OutputBuffer;
use super::clock::iso_utc;
use memchr::memmem;

use super::fence::MarkerScanner;
use super::journal::Journal;
use crate::platform::{self, ControlChannel, Process, Signal};

/// Output read per pump iteration.
const READ_CHUNK: usize = 65_536;
/// How long a finalize waits for the pump to settle when the fence never
/// arrived (process exit or EOF without a marker).
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// Progress events while output flows, at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
/// Silence before the first no-output warning (overridable per kernel); each
/// repeat in the same silence waits until the silence has doubled.
pub(crate) const DEFAULT_NO_OUTPUT_WARN: Duration = Duration::from_mins(5);
/// Output that means a cargo build waits on another build's lock.
const CARGO_BUILD_LOCK_TEXT: &[u8] = b"Blocking waiting for file lock on build directory";

/// Why a command is waiting, when its output says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitReason {
    CargoBuildLock,
}

impl WaitReason {
    fn as_str(self) -> &'static str {
        match self {
            WaitReason::CargoBuildLock => "cargo_build_lock",
        }
    }
}

/// What a kernel following the job learns, in order.
#[derive(Debug, Clone)]
pub(crate) enum JobEvent {
    /// A structured progress event (`command_progress`, `cargo_lock_wait`,
    /// `command_no_output`) with the job's progress fields at that moment.
    Progress {
        msg: &'static str,
        fields: Map<String, Value>,
    },
    /// The command's result.
    Finished {
        exit_code: i32,
        output: Arc<str>,
        duration: Duration,
        fields: Map<String, Value>,
    },
    /// The whole process group is gone; `bytes` is the stream's final size.
    Reaped { bytes: u64 },
}

/// A [`JobEvent`] as the job keeps it: the result's text is rendered from
/// the buffer when a follower reads it, so a finished job holds its output
/// once (in the buffer), not once per copy.
#[derive(Debug, Clone)]
enum Recorded {
    Progress {
        msg: &'static str,
        fields: Map<String, Value>,
    },
    Finished {
        exit_code: i32,
        duration: Duration,
        fields: Map<String, Value>,
    },
    Reaped {
        bytes: u64,
    },
}

/// Which bytes of the buffer are the command's result.
#[derive(Debug, Clone)]
enum ResultText {
    /// Not decided yet.
    Unset,
    /// The buffer as it is now: no byte arrived since the result was pinned.
    Live,
    /// The buffer as it was when the result was pinned, rendered just before
    /// a later byte (output past the fence) landed.
    Frozen(Arc<str>),
}

impl JobEvent {
    pub(crate) fn to_json(&self) -> Value {
        let mut json = self.to_json_without_output();
        if let JobEvent::Finished { output, .. } = self {
            json["output"] = (**output).into();
        }
        json
    }

    /// [`Self::to_json`], a finished event's `output` left out.
    pub(crate) fn to_json_without_output(&self) -> Value {
        match self {
            JobEvent::Progress { msg, fields } => {
                json!({"type": "progress", "msg": msg, "fields": fields})
            }
            JobEvent::Finished {
                exit_code,
                duration,
                fields,
                ..
            } => json!({
                "type": "finished",
                "exitCode": exit_code,
                "duration": duration.as_secs_f64(),
                "fields": fields,
            }),
            JobEvent::Reaped { bytes } => json!({"type": "reaped", "bytes": bytes}),
        }
    }
}

/// What the status reporter learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusReport {
    /// Still reading the channel.
    Pending,
    /// The foreground status the shell wrote.
    Delivered(i32),
    /// The shell died without writing one.
    Missing,
}

/// The fence's view of the output stream.
#[derive(Debug)]
enum Fence {
    /// Still looking for the marker.
    Open(Box<MarkerScanner>),
    /// The marker arrived (`marked`: the result is the output as of that
    /// moment) or the stream ended without one: later bytes are plain output.
    Closed { marked: bool },
}

#[derive(Debug)]
struct State {
    buffer: OutputBuffer,
    last_output: Instant,
    last_output_at: Option<SystemTime>,
    last_progress: Option<Instant>,
    wait_reason: Option<WaitReason>,
    cargo_tail: Vec<u8>,
    fence: Fence,
    result: ResultText,
    /// The pump is between reading a chunk and committing it.
    transfer: bool,
    eof: bool,
    status: StatusReport,
    finished: Option<(i32, Duration)>,
    reaped: bool,
    events: Vec<Recorded>,
}

/// One spawned command.
#[derive(Debug)]
pub(crate) struct Job {
    pub(crate) id: String,
    pub(crate) command: String,
    pub(crate) pid: u32,
    pub(crate) started: Instant,
    pub(crate) started_at: SystemTime,
    journal: Journal,
    state: Mutex<State>,
    changed: Condvar,
    /// Serializes signals with the reap, so no signal can reach a group id
    /// that was released after the group died.
    kill_lock: Mutex<()>,
    /// A second handle on the output pipe, for asking how many bytes wait
    /// unread while the pump owns the reading end.
    output_probe: Option<PipeReader>,
    control: platform::Control,
}

/// Everything a new job's threads need.
pub(crate) struct Launch {
    pub id: String,
    pub command: String,
    pub token: String,
    pub journal: Journal,
    pub no_output_warn: Option<Duration>,
}

impl Job {
    /// Wire up a freshly spawned command (already journaled, gate still
    /// closed) and start its threads; the caller opens the gate.
    pub(crate) fn start(launch: Launch, spawned: platform::Spawned) -> Arc<Job> {
        let spawned_control = spawned.control();
        let platform::Spawned {
            process,
            channel,
            output,
        } = spawned;
        let now = Instant::now();
        let control = spawned_control;
        let job = Arc::new(Job {
            id: launch.id,
            command: launch.command,
            pid: process.id(),
            started: now,
            started_at: SystemTime::now(),
            journal: launch.journal,
            state: Mutex::new(State {
                buffer: OutputBuffer::default(),
                last_output: now,
                last_output_at: None,
                last_progress: None,
                wait_reason: None,
                cargo_tail: Vec::new(),
                fence: Fence::Open(Box::new(MarkerScanner::new(&launch.token))),
                result: ResultText::Unset,
                transfer: false,
                eof: false,
                status: StatusReport::Pending,
                finished: None,
                reaped: false,
                events: Vec::new(),
            }),
            changed: Condvar::new(),
            kill_lock: Mutex::new(()),
            output_probe: output.try_clone().ok(),
            control,
        });
        job.clone()
            .run_threads(process, channel, output, launch.no_output_warn);
        job
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn run_threads(
        self: Arc<Self>,
        process: Process,
        mut channel: ControlChannel,
        output: PipeReader,
        no_output_warn: Option<Duration>,
    ) {
        let waker = channel.take_waker();
        let pump = Arc::clone(&self);
        std::thread::spawn(move || pump.pump(output));
        let reporter = Arc::clone(&self);
        std::thread::spawn(move || reporter.report(channel));
        if let Some(threshold) = no_output_warn {
            let warner = Arc::clone(&self);
            std::thread::spawn(move || warner.warn_no_output(threshold));
        }
        std::thread::spawn(move || self.watch(process, waker));
    }

    /// Move output from the pipe into the buffer until EOF.
    fn pump(&self, mut output: PipeReader) {
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            if !platform::wait_readable(&output) {
                break;
            }
            self.lock().transfer = true;
            let read = output.read(&mut chunk);
            let mut state = self.lock();
            state.transfer = false;
            match read {
                Ok(0) | Err(_) => break,
                Ok(read) => self.consume(&mut state, &chunk[..read]),
            }
        }
        let mut state = self.lock();
        self.abandon_fence(&mut state);
        state.eof = true;
        drop(state);
        self.changed.notify_all();
    }

    fn consume(&self, state: &mut State, chunk: &[u8]) {
        // The scanner leaves the state while it hands bytes to `record`
        // (the lock is held throughout, so nothing sees the gap).
        let mut scanner = match std::mem::replace(&mut state.fence, Fence::Closed { marked: false })
        {
            Fence::Open(scanner) => scanner,
            closed @ Fence::Closed { .. } => {
                state.fence = closed;
                self.record(state, chunk);
                return;
            }
        };
        match scanner.feed_into(chunk, |bytes| self.record(state, bytes)) {
            None => state.fence = Fence::Open(scanner),
            Some(after) => {
                state.fence = Fence::Closed { marked: true };
                state.result = ResultText::Live;
                self.changed.notify_all();
                self.record(state, &after);
            }
        }
    }

    fn abandon_fence(&self, state: &mut State) {
        if let Fence::Open(scanner) = &mut state.fence {
            let pending = scanner.take_pending();
            state.fence = Fence::Closed { marked: false };
            self.record(state, &pending);
            self.changed.notify_all();
        }
    }

    fn record(&self, state: &mut State, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if matches!(state.result, ResultText::Live) {
            // The result is pinned to the buffer as it is now: render it
            // before this byte changes the buffer.
            state.result = ResultText::Frozen(state.buffer.text().into());
        }
        state.buffer.write(chunk);
        let now = Instant::now();
        state.last_output = now;
        state.last_output_at = Some(SystemTime::now());
        // Only the text that could straddle the previous read is joined; the
        // rest of the chunk is searched in place.
        let keep = CARGO_BUILD_LOCK_TEXT.len() - 1;
        if state.wait_reason.is_none() {
            let mut seam = std::mem::take(&mut state.cargo_tail);
            seam.extend_from_slice(&chunk[..chunk.len().min(keep)]);
            if memmem::find(&seam, CARGO_BUILD_LOCK_TEXT).is_some()
                || memmem::find(chunk, CARGO_BUILD_LOCK_TEXT).is_some()
            {
                state.wait_reason = Some(WaitReason::CargoBuildLock);
                self.emit_progress(state, "cargo_lock_wait", now);
            }
        }
        let mut tail = std::mem::take(&mut state.cargo_tail);
        if chunk.len() >= keep {
            tail.clear();
            tail.extend_from_slice(&chunk[chunk.len() - keep..]);
        } else {
            tail.extend_from_slice(chunk);
            tail.drain(..tail.len().saturating_sub(keep));
        }
        state.cargo_tail = tail;
        if state
            .last_progress
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL)
        {
            state.last_progress = Some(now);
            self.emit_progress(state, "command_progress", now);
        }
    }

    fn emit_progress(&self, state: &mut State, msg: &'static str, now: Instant) {
        let fields = self.progress_fields(state, now);
        state.events.push(Recorded::Progress { msg, fields });
        self.changed.notify_all();
    }

    /// The span attributes that describe the job's progress right now.
    fn progress_fields(&self, state: &State, now: Instant) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("bash.pid".into(), self.pid.into());
        fields.insert("bash.pgid".into(), self.pid.into());
        fields.insert(
            "bash.elapsed_ms".into(),
            millis(now.duration_since(self.started)).into(),
        );
        fields.insert(
            "bash.silence_ms".into(),
            millis(now.duration_since(state.last_output)).into(),
        );
        fields.insert("bash.output_bytes".into(), state.buffer.total().into());
        if let Some(reason) = state.wait_reason {
            fields.insert("bash.wait_reason".into(), reason.as_str().into());
        }
        fields
    }

    /// Read the delivered foreground status, then finalize with it once the
    /// fence (or EOF) says the output is complete.
    fn report(&self, channel: ControlChannel) {
        let status = channel.read_status();
        let mut state = self.lock();
        // Reserve the delivered status before draining, so a shell death in
        // the drain window cannot override it with the exit code.
        state.status = status.map_or(StatusReport::Missing, StatusReport::Delivered);
        self.changed.notify_all();
        let Some(status) = status else {
            return;
        };
        let mut state = self
            .changed
            .wait_while(state, |state| matches!(state.fence, Fence::Open(_)))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let marked = matches!(state.fence, Fence::Closed { marked: true });
        if !marked {
            state = self.drain_grace(state);
        }
        self.finalize(&mut state, status, marked);
    }

    /// Observe the shell's death independently of the status channel, then
    /// reap the group and retire the journal record.
    fn watch(self: Arc<Self>, mut process: Process, waker: Option<PipeWriter>) {
        let exit_code = process.wait();
        if let Some(mut waker) = waker {
            // Unblock the status read: background children can hold the
            // status socket open past the shell's lifetime.
            let _ = waker.write_all(b"x");
        }
        let state = self.lock();
        let mut state = self
            .changed
            .wait_while(state, |state| state.status == StatusReport::Pending)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.status == StatusReport::Missing && state.finished.is_none() {
            self.abandon_fence(&mut state);
            state = self.drain_grace(state);
            self.finalize(&mut state, exit_code, false);
        }
        // A delivered status finalizes on the reporter (once the fence or EOF
        // completes the output): the result always precedes the reap.
        let state = self
            .changed
            .wait_while(state, |state| state.finished.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state);
        let delivered = {
            let _kill = self
                .kill_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let delivered = self.control.reap();
            let mut state = self.lock();
            state.reaped = true;
            delivered
        };
        if delivered {
            self.journal.record(self.pid, false);
        }
        // The group is gone, so the pipe's writers are too: let the reader
        // land what they wrote last (an EXIT trap's output after the fence)
        // before the reap reports the stream's byte count. Bounded, for a
        // writer that escaped the group and still holds the pipe.
        let state = self.lock();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, DRAIN_GRACE, |state| !state.eof)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = state.buffer.total();
        state.events.push(Recorded::Reaped { bytes });
        drop(state);
        self.changed.notify_all();
    }

    /// Best-effort settle when the result arrives without a fence: wait (at
    /// most [`DRAIN_GRACE`]) for EOF or for the buffer to stop growing, never
    /// giving up while bytes sit in the pipe or a chunk is mid-commit.
    fn drain_grace<'a>(&'a self, mut state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        let deadline = Instant::now() + DRAIN_GRACE;
        let mut size = state.buffer.size();
        while Instant::now() < deadline {
            let (next, _) = self
                .changed
                .wait_timeout_while(state, Duration::from_millis(50), |state| !state.eof)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if state.eof {
                return state;
            }
            // A chunk between the pipe read and the buffer commit is
            // invisible to both the pipe's byte count and the buffer size.
            if state.transfer
                || self
                    .output_probe
                    .as_ref()
                    .is_some_and(platform::pending_bytes)
            {
                size = state.buffer.size();
                continue;
            }
            let current = state.buffer.size();
            if current == size {
                return state;
            }
            size = current;
        }
        state
    }

    /// Record the result. `marked`: the fence pinned the result text when its
    /// marker arrived; otherwise the result is the buffer as it is now.
    fn finalize(&self, state: &mut State, exit_code: i32, marked: bool) {
        if state.finished.is_some() {
            return;
        }
        let now = Instant::now();
        let duration = now.duration_since(self.started);
        state.finished = Some((exit_code, duration));
        if !marked {
            state.result = ResultText::Live;
        }
        let mut fields = self.progress_fields(state, now);
        if let Some(at) = state.last_output_at {
            fields.insert("bash.last_output_at".into(), iso_utc(at).into());
        }
        state.events.push(Recorded::Finished {
            exit_code,
            duration,
            fields,
        });
        self.changed.notify_all();
    }

    fn warn_no_output(&self, threshold: Duration) {
        let mut state = self.lock();
        // The silence episode the warnings describe ends when output arrives.
        let mut episode = state.last_output;
        let mut warned = None;
        loop {
            let Some(due) = episode.checked_add(no_output_warning_due(threshold, warned)) else {
                return;
            };
            let wait = due.saturating_duration_since(Instant::now());
            let (guard, _) = self
                .changed
                .wait_timeout_while(state, wait, |state| {
                    !state.reaped && state.last_output == episode && Instant::now() < due
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
            if state.reaped {
                return;
            }
            if state.last_output != episode {
                episode = state.last_output;
                warned = None;
                continue;
            }
            let now = Instant::now();
            if now < due {
                continue;
            }
            self.emit_progress(&mut state, "command_no_output", now);
            warned = Some(now.duration_since(episode));
        }
    }

    /// The events after `cursor`, waiting up to `timeout` for one; `done`
    /// once the reaped event has been delivered.
    pub(crate) fn follow(&self, cursor: usize, timeout: Duration) -> (Vec<JobEvent>, usize, bool) {
        let state = self.lock();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.events.len() <= cursor)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let events: Vec<JobEvent> = state
            .events
            .iter()
            .skip(cursor)
            .map(|event| match event {
                Recorded::Progress { msg, fields } => JobEvent::Progress {
                    msg,
                    fields: fields.clone(),
                },
                Recorded::Finished {
                    exit_code,
                    duration,
                    fields,
                } => JobEvent::Finished {
                    exit_code: *exit_code,
                    output: result_text(&state),
                    duration: *duration,
                    fields: fields.clone(),
                },
                Recorded::Reaped { bytes } => JobEvent::Reaped { bytes: *bytes },
            })
            .collect();
        let next = cursor + events.len();
        // The reaped event is always the last one.
        let done = matches!(state.events.last(), Some(Recorded::Reaped { .. }))
            && next == state.events.len();
        (events, next, done)
    }

    /// The rendered output and the stream byte count.
    pub(crate) fn output(&self) -> (String, u64) {
        let state = self.lock();
        (state.buffer.text(), state.buffer.total())
    }

    /// Keep only the last `keep` output bytes (an old reaped job's buffer,
    /// once its stream has ended; the result was delivered long before).
    pub(crate) fn compact(&self, keep: usize) {
        let mut state = self.lock();
        if state.eof && state.reaped {
            state.buffer.compact(keep);
        }
    }

    /// The stream byte count only.
    pub(crate) fn output_bytes(&self) -> u64 {
        self.lock().buffer.total()
    }

    pub(crate) fn is_reaped(&self) -> bool {
        self.lock().reaped
    }

    /// `(exit code, duration)` once finished.
    pub(crate) fn finished(&self) -> Option<(i32, Duration)> {
        self.lock().finished
    }

    /// Progress fields plus `bash.last_output_at` (the inventory record).
    pub(crate) fn snapshot(&self) -> Map<String, Value> {
        let state = self.lock();
        let mut fields = self.progress_fields(&state, Instant::now());
        if let Some(at) = state.last_output_at {
            fields.insert("bash.last_output_at".into(), iso_utc(at).into());
        }
        fields
    }

    /// Signal the group; with SIGTERM, escalate to SIGKILL after `grace`
    /// unless the group is reaped first. False when already reaped.
    pub(crate) fn kill(self: &Arc<Self>, signal: Signal, grace: Duration) -> bool {
        {
            let _kill = self
                .kill_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.is_reaped() {
                return false;
            }
            self.signal_group(signal);
        }
        if signal == Signal::TERM {
            let job = Arc::clone(self);
            std::thread::spawn(move || {
                let state = job.lock();
                let (state, _) = job
                    .changed
                    .wait_timeout_while(state, grace, |state| !state.reaped)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(state);
                let _kill = job
                    .kill_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !job.is_reaped() {
                    job.signal_group(Signal::KILL);
                }
            });
        }
        true
    }

    /// SIGKILL the group now (kernel shutdown). True when delivered.
    pub(crate) fn kill_now(&self) -> bool {
        let _kill = self
            .kill_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_reaped() {
            return true;
        }
        self.signal_group(Signal::KILL)
    }

    /// The cancelled one-shot's teardown: wait `term_grace` for the group to
    /// die after the TERM already sent, SIGKILL it if it did not, then wait up
    /// to `kill_wait` for confirmed death. True when the group is gone.
    pub(crate) fn confirm_group_exit(&self, term_grace: Duration, kill_wait: Duration) -> bool {
        if self.await_group_death(term_grace) {
            return true;
        }
        self.kill_now();
        self.await_group_death(kill_wait)
    }

    fn await_group_death(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.group_alive() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether any member of the command's process group is alive.
    /// Whether any member of the command's process group (Windows: job) is alive.
    pub(crate) fn group_alive(&self) -> bool {
        self.control.group_alive()
    }

    fn signal_group(&self, signal: Signal) -> bool {
        self.control.signal(signal)
    }
}

/// How much silence a silence episode has when its next no-output warning
/// is due: `threshold` for the first, then twice the silence at the last
/// warning (the TS runtime repeated every five minutes, so a job silent for
/// a day logged hundreds of identical warnings; the backoff keeps the
/// repeats, logarithmically).
fn no_output_warning_due(threshold: Duration, warned: Option<Duration>) -> Duration {
    match warned {
        None => threshold,
        Some(warned) => warned.saturating_mul(2),
    }
}

/// The command's result text (rendered from the buffer unless a later byte
/// froze it first).
fn result_text(state: &State) -> Arc<str> {
    match &state.result {
        ResultText::Frozen(text) => Arc::clone(text),
        ResultText::Live | ResultText::Unset => state.buffer.text().into(),
    }
}

/// Python `round(seconds * 1000)`.
fn millis(duration: Duration) -> u64 {
    let rounded = (duration.as_secs_f64() * 1000.0).round_ties_even();
    if rounded.is_finite() && rounded >= 0.0 {
        // In range: a job's elapsed time is far below 2^53 ms.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "non-negative, finite, far below u64::MAX"
        )]
        let millis = rounded as u64;
        millis
    } else {
        0
    }
}

#[cfg(test)]
mod no_output_tests {
    use std::time::Duration;

    use super::no_output_warning_due;

    /// The silence (in minutes) at each warning an unbroken silence of
    /// `silence` earns.
    fn warnings(threshold: Duration, silence: Duration) -> Vec<u64> {
        let mut at = Vec::new();
        let mut warned = None;
        loop {
            let due = no_output_warning_due(threshold, warned);
            if due > silence {
                return at;
            }
            at.push(due.as_secs() / 60);
            warned = Some(due);
        }
    }

    /// A job silent for 25 hours logged 299 warnings in a user's logs (one
    /// every five minutes): the repeats back off instead, so the same
    /// silence warns nine times.
    #[test]
    fn a_long_silence_warns_with_exponential_backoff() {
        assert_eq!(
            warnings(Duration::from_mins(5), Duration::from_hours(25)),
            vec![5, 10, 20, 40, 80, 160, 320, 640, 1280]
        );
    }
}
