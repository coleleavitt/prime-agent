//! Durable run records: one JSON file per run under the session's artifact
//! directory (`<session-artifacts>/<session id>/factory-runs/<run id>.json`).
//!
//! Every committed mutation of a run marks it dirty; one writer thread per
//! executor serializes the latest state and replaces the record atomically
//! (temp file + rename), coalescing bursts, so the record trails the live
//! run by at most one write. A restarted host reloads every record and
//! [`recover_after_restart`] reconciles the runs that were in flight: the
//! children the old host admitted are gone with its session, so their
//! instances re-queue and the run pauses as `interrupted`, which
//! `factory.resume` continues.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{kind, EventAt, FactoryRun, RunState, Status};
use super::RunCell;

/// The run-record directory under a session's artifact directory.
pub const FACTORY_RUNS_DIR: &str = "factory-runs";
/// The record format version (bumped on an incompatible model change).
const RECORD_VERSION: u32 = 1;
/// The pause reason a host restart leaves on an interrupted run.
pub const INTERRUPTED_PAUSE_REASON: &str =
    "interrupted: the host restarted while the run was in flight";

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    run: FactoryRun,
}

enum StoreMessage {
    Dirty(Arc<RunCell>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// A cheap handle to one executor's record writer.
#[derive(Clone)]
pub struct RunStore {
    dir: PathBuf,
    sender: mpsc::Sender<StoreMessage>,
}

impl RunStore {
    /// Start the writer for `dir` (created on first write).
    #[must_use]
    pub fn open(dir: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel::<StoreMessage>();
        let writer_dir = dir.clone();
        let spawned = std::thread::Builder::new()
            .name("factory-run-store".into())
            .spawn(move || write_loop(&writer_dir, &receiver));
        if let Err(error) = spawned {
            tracing::warn!(target: "pa_core::factory", %error, "factory run store writer did not start");
        }
        Self { dir, sender }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Queue `cell` for a write (one queued write per dirty run).
    pub fn mark_dirty(&self, cell: &Arc<RunCell>) {
        if !cell.dirty.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(StoreMessage::Dirty(Arc::clone(cell)));
        }
    }

    /// Resolve once every write queued before this call is on disk.
    pub async fn flush(&self) {
        let (done, wait) = tokio::sync::oneshot::channel();
        if self.sender.send(StoreMessage::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// Every record in the directory, oldest run first (records sort by
    /// their run's start time, the registry's insertion order).
    #[must_use]
    pub fn load(&self) -> Vec<FactoryRun> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut runs: Vec<FactoryRun> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let parsed = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    serde_json::from_slice::<Record>(&bytes).map_err(anyhow::Error::from)
                });
            match parsed {
                Ok(record) if record.version == RECORD_VERSION => runs.push(record.run),
                Ok(record) => tracing::warn!(
                    target: "pa_core::factory",
                    path = %path.display(),
                    version = record.version,
                    "skipping a factory run record of an unknown version"
                ),
                Err(error) => tracing::warn!(
                    target: "pa_core::factory",
                    path = %path.display(),
                    error = %format!("{error:#}"),
                    "skipping an unreadable factory run record"
                ),
            }
        }
        runs.sort_by(|a, b| {
            a.started_at
                .total_cmp(&b.started_at)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        runs
    }
}

fn write_loop(dir: &Path, receiver: &mpsc::Receiver<StoreMessage>) {
    while let Ok(message) = receiver.recv() {
        match message {
            StoreMessage::Dirty(cell) => write_record(dir, &cell),
            StoreMessage::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn write_record(dir: &Path, cell: &Arc<RunCell>) {
    // Clear the flag BEFORE serializing: a mutation that lands while this
    // write runs queues the next one.
    cell.dirty.store(false, Ordering::Release);
    let (run_id, bytes) = {
        let run = cell.lock();
        let record = Record {
            version: RECORD_VERSION,
            run: run.clone(),
        };
        (run.run_id.clone(), serde_json::to_vec(&record))
    };
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(target: "pa_core::factory", %error, run_id, "factory run record did not serialize");
            return;
        }
    };
    if let Err(error) = write_atomic(dir, &run_id, &bytes) {
        tracing::warn!(target: "pa_core::factory", error = %format!("{error:#}"), run_id, "factory run record write failed");
    }
}

fn write_atomic(dir: &Path, run_id: &str, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(format!("{run_id}.json"));
    let temp = dir.join(format!(".{run_id}.json.tmp"));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, &target)?;
    Ok(())
}

/// Reconcile one reloaded run with a restarted host. The old host's
/// children did not survive with its session, so every instance it had in
/// flight re-queues (its lost child is remembered for a best-effort delete
/// on resume), and a run that was running pauses as interrupted — the
/// state `factory.resume` continues from. A run caught mid-stop finishes
/// stopping. Terminal runs only lose their in-flight resident children.
pub fn recover_after_restart(run: &mut FactoryRun) {
    let live = run.state.live();
    let mut lost = 0_usize;
    for position in 0..run.states.len() {
        for entry_position in 0..run.states[position].entries.len() {
            for instance_position in 0..run.states[position].entries[entry_position].instances.len()
            {
                let state_id = run.states[position].state_id.clone();
                let entry = &mut run.states[position].entries[entry_position];
                let entry_index = entry.index;
                let entry_running = entry.status == Status::Running;
                let instance = &mut entry.instances[instance_position];
                if instance.status != Status::Running {
                    continue;
                }
                let child = instance.child_id.take();
                instance.spawned_at = None;
                let requeue = live && run.state != RunState::Stopping && entry_running;
                instance.status = if requeue {
                    Status::Pending
                } else {
                    Status::Cancelled
                };
                if requeue {
                    instance.interrupted_child.clone_from(&child);
                }
                let index = instance.index;
                lost += 1;
                let detail = if requeue {
                    "the host restarted while this child ran; re-queued for admission"
                } else {
                    "the host restarted while this child ran"
                };
                let mut extra = Vec::new();
                if let Some(child) = child {
                    extra.push(("child", Value::from(child)));
                }
                run.record(
                    kind::INTERRUPTED,
                    EventAt {
                        node: Some(&state_id),
                        entry: Some(entry_index),
                        instance: Some(index),
                        detail: Some(detail),
                    },
                    extra,
                );
            }
        }
    }
    match run.state {
        RunState::Running => {
            run.state = RunState::Paused;
            run.pause_reason = Some(INTERRUPTED_PAUSE_REASON.to_string());
            let detail = format!(
                "the host restarted mid-run ({lost} in-flight child(ren) lost); resume with await rlm.factory.resume('{}')",
                run.run_id
            );
            run.record(
                kind::RUN_INTERRUPTED,
                EventAt {
                    detail: Some(&detail),
                    ..EventAt::none()
                },
                Vec::new(),
            );
        }
        RunState::Stopping => {
            for state in &mut run.states {
                if state.entries.iter().any(|entry| entry.status.in_flight())
                    || state.entries.is_empty()
                {
                    state.cancelled = true;
                }
                for entry in &mut state.entries {
                    if entry.status.in_flight() {
                        entry.status = Status::Cancelled;
                    }
                    for instance in &mut entry.instances {
                        if instance.status == Status::Pending {
                            instance.status = Status::Cancelled;
                        }
                    }
                }
            }
            run.state = RunState::Stopped;
            run.record(
                kind::RUN_STOPPED,
                EventAt {
                    detail: Some("stopped; the host restarted mid-stop"),
                    ..EventAt::none()
                },
                Vec::new(),
            );
        }
        RunState::Paused | RunState::Done | RunState::Failed | RunState::Stopped => {}
    }
    // The old loop's backoff deadline died with it.
    run.admission_backoff_until = None;
}
