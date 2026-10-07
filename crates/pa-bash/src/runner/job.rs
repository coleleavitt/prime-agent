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
use super::fence::{self, MarkerScanner};
use super::journal::Journal;
use crate::platform::{self, ControlChannel, Process, Signal};

/// Output read per pump iteration.
const READ_CHUNK: usize = 65_536;
/// How long a finalize waits for the pump to settle when the fence never
/// arrived (process exit or EOF without a marker).
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// Progress events while output flows, at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
/// Silence before the first no-output warning (overridable per kernel), and
/// between repeats.
pub(crate) const DEFAULT_NO_OUTPUT_WARN: Duration = Duration::from_mins(5);
const NO_OUTPUT_REPEAT: Duration = Duration::from_mins(5);
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
        output: String,
        duration: Duration,
        fields: Map<String, Value>,
    },
    /// The whole process group is gone; `bytes` is the stream's final size.
    Reaped { bytes: u64 },
}

impl JobEvent {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            JobEvent::Progress { msg, fields } => {
                json!({"type": "progress", "msg": msg, "fields": fields})
            }
            JobEvent::Finished {
                exit_code,
                output,
                duration,
                fields,
            } => json!({
                "type": "finished",
                "exitCode": exit_code,
                "output": output,
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
    Open(MarkerScanner),
    /// The marker arrived (with the output as of that moment) or the stream
    /// ended without one (`None`): later bytes are plain output.
    Closed(Option<String>),
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
    /// The pump is between reading a chunk and committing it.
    transfer: bool,
    eof: bool,
    status: StatusReport,
    finished: Option<(i32, Duration)>,
    reaped: bool,
    events: Vec<JobEvent>,
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
                fence: Fence::Open(MarkerScanner::new(&launch.token)),
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
        let scanned = match &mut state.fence {
            Fence::Closed(_) => {
                self.record(state, chunk);
                return;
            }
            Fence::Open(scanner) => scanner.feed(chunk),
        };
        self.record(state, &scanned.before);
        if let Some(after) = scanned.fence {
            state.fence = Fence::Closed(Some(state.buffer.text()));
            self.changed.notify_all();
            self.record(state, &after);
        }
    }

    fn abandon_fence(&self, state: &mut State) {
        if let Fence::Open(scanner) = &mut state.fence {
            let pending = scanner.take_pending();
            state.fence = Fence::Closed(None);
            self.record(state, &pending);
            self.changed.notify_all();
        }
    }

    fn record(&self, state: &mut State, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
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
            if fence::find(&seam, CARGO_BUILD_LOCK_TEXT).is_some()
                || fence::find(chunk, CARGO_BUILD_LOCK_TEXT).is_some()
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
        state.events.push(JobEvent::Progress { msg, fields });
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
        let output = match &state.fence {
            Fence::Closed(output) => output.clone(),
            Fence::Open(_) => None,
        };
        if output.is_none() {
            state = self.drain_grace(state);
        }
        self.finalize(&mut state, status, output);
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
            self.finalize(&mut state, exit_code, None);
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
        let mut state = self.lock();
        let bytes = state.buffer.total();
        state.events.push(JobEvent::Reaped { bytes });
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

    fn finalize(&self, state: &mut State, exit_code: i32, output: Option<String>) {
        if state.finished.is_some() {
            return;
        }
        let now = Instant::now();
        let duration = now.duration_since(self.started);
        state.finished = Some((exit_code, duration));
        let output = output.unwrap_or_else(|| state.buffer.text());
        let mut fields = self.progress_fields(state, now);
        if let Some(at) = state.last_output_at {
            fields.insert("bash.last_output_at".into(), iso_utc(at).into());
        }
        state.events.push(JobEvent::Finished {
            exit_code,
            output,
            duration,
            fields,
        });
        self.changed.notify_all();
    }

    fn warn_no_output(&self, threshold: Duration) {
        let mut state = self.lock();
        let mut next = state.last_output + threshold;
        loop {
            let wait = next.saturating_duration_since(Instant::now());
            let (guard, _) = self
                .changed
                .wait_timeout_while(state, wait, |state| !state.reaped && Instant::now() < next)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
            if state.reaped {
                return;
            }
            let now = Instant::now();
            if now < next {
                continue;
            }
            if now.duration_since(state.last_output) < threshold {
                next = state.last_output + threshold;
                continue;
            }
            self.emit_progress(&mut state, "command_no_output", now);
            next = now + NO_OUTPUT_REPEAT;
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
        let events: Vec<JobEvent> = state.events.iter().skip(cursor).cloned().collect();
        let next = cursor + events.len();
        // The reaped event is always the last one.
        let done = matches!(state.events.last(), Some(JobEvent::Reaped { .. }))
            && next == state.events.len();
        (events, next, done)
    }

    /// The rendered output and the stream byte count.
    pub(crate) fn output(&self) -> (String, u64) {
        let state = self.lock();
        (state.buffer.text(), state.buffer.total())
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
