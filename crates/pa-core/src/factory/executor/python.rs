//! Python states: one short-lived isolated subprocess per call.
//!
//! A state carrying `python: {import, callable}` runs `fn(inputs)` in a
//! fresh `python -I` process from the session's Python environment
//! ([`FactoryPython`]): the bound inputs arrive as one JSON object on
//! stdin, and the declared outputs leave as one JSON object on stdout
//! (everything the call prints, its own subprocesses included, goes to
//! stderr). stdin, stdout and stderr are temp files, so a large output
//! never blocks the call on a full pipe. Each call leads its own process group:
//! a kill, and the call's own exit, take everything it left running with
//! it.
//!
//! A call holds a `max_parallel` slot but is no child: it never counts
//! toward `max_children`. Its time limit is the state's `budget_ms`, else
//! the run's, else [`PYTHON_STATE_TIMEOUT_MS_DEFAULT`]; a call past it is
//! killed and fails without a retry.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::sync::{Notify, oneshot};

use super::binding::{ANSWER_BINDING_CAP, ANSWER_CAPTURE_CAP, char_prefix, py_json_dumps_compact};
use super::engine::{Admission, Slot};
use super::model::{FactoryRun, RunState, Status, kind};
use super::{Inner, RunCell};
use crate::factory::pyvalue::{PyValue, py_repr, py_str_repr};

/// A python call's time limit when neither the state nor the run sets
/// `budget_ms`.
pub const PYTHON_STATE_TIMEOUT_MS_DEFAULT: u64 = 60_000;

/// How much of a failed python call's stderr (bytes, its tail) the error
/// text carries.
pub const PYTHON_STDERR_TAIL_CAP: u64 = 2000;

/// The `python -I -c` program that runs one call: inputs arrive as JSON on
/// stdin, everything the call prints goes to stderr, and the returned
/// value is the only thing written to stdout.
const PYTHON_RUNNER: &str = r#"
import asyncio, importlib, inspect, json, os, sys
sys.path.insert(0, os.getcwd())
inputs = json.load(sys.stdin.buffer)
result_stream = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr
result = getattr(importlib.import_module(sys.argv[1]), sys.argv[2])(inputs)
if inspect.iscoroutine(result):
    result = asyncio.run(result)
json.dump(result, result_stream)
result_stream.close()
"#;

/// How one call ended: its exit code (a signal reads as its negated
/// number, like Python's `returncode`), its stdout, and its stderr tail.
pub(super) struct PythonExit {
    code: i32,
    stdout: String,
    stderr: String,
}

/// One running call: its kill switch and the task that reaps it.
pub(super) struct PythonCall {
    /// Dropping the sender kills the call too (the executor went away).
    kill: Option<oneshot::Sender<()>>,
    exit: tokio::task::JoinHandle<PythonExit>,
}

impl PythonCall {
    /// Start `module.callable(inputs)` from `command` (the session's
    /// interpreter, environment, working directory, and sandbox). `exited`
    /// is notified once the call has exited.
    async fn start(
        command: std::process::Command,
        module: &str,
        callable: &str,
        inputs: &Map<String, Value>,
        exited: Arc<Notify>,
    ) -> std::io::Result<Self> {
        let mut stdin = ScratchFile::create()?;
        stdin
            .file
            .write_all(Value::Object(inputs.clone()).to_string().as_bytes())?;
        stdin.file.seek(SeekFrom::Start(0))?;
        let stdout = ScratchFile::create()?;
        let stderr = ScratchFile::create()?;
        let mut command = tokio::process::Command::from(command);
        command
            .args(["-I", "-c", PYTHON_RUNNER, module, callable])
            .stdin(std::process::Stdio::from(stdin.file.try_clone()?))
            .stdout(std::process::Stdio::from(stdout.file.try_clone()?))
            .stderr(std::process::Stdio::from(stderr.file.try_clone()?))
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        crate::platform::process::set_no_window(command.as_std_mut());
        let mut child = crate::platform::process::spawn_retrying_text_busy(&mut command).await?;
        let pid = child.id().map_or(0, |pid| pid as i32);
        let (kill, killed) = oneshot::channel::<()>();
        let exit = tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = killed => {
                    let _ = crate::platform::process::kill_process_group(pid);
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // A call lives no longer than its exit: anything it left
            // running in its process group goes with it (the reaped
            // leader's `Child` is still held, so its pid stays anchored).
            let _ = crate::platform::process::kill_process_group(pid);
            drop((child, stdin));
            let exit = PythonExit {
                code: status.map_or(-1, exit_code),
                stdout: read_all(stdout),
                stderr: read_tail(stderr, PYTHON_STDERR_TAIL_CAP),
            };
            exited.notify_one();
            exit
        });
        Ok(Self {
            kill: Some(kill),
            exit,
        })
    }

    fn finished(&self) -> bool {
        self.exit.is_finished()
    }

    /// Wait for the exit (the call already exited, or was killed).
    async fn finish(self) -> PythonExit {
        self.exit.await.unwrap_or_else(|error| PythonExit {
            code: -1,
            stdout: String::new(),
            stderr: format!("the python call's waiter failed: {error}"),
        })
    }

    /// Kill a running call and wait for it to exit.
    async fn stop(mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
        let _ = self.finish().await;
    }
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(-1)
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// A call's stdio file: written by the call through an inherited handle
/// (a full pipe could never block it), removed with this owner.
struct ScratchFile {
    file: std::fs::File,
    path: std::path::PathBuf,
}

impl ScratchFile {
    fn create() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "pa-factory-python-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self { file, path })
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_all(mut scratch: ScratchFile) -> String {
    let mut bytes = Vec::new();
    let _ = scratch
        .file
        .seek(SeekFrom::Start(0))
        .and_then(|_| scratch.file.read_to_end(&mut bytes));
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The stripped text of the file's last `cap` bytes.
fn read_tail(mut scratch: ScratchFile, cap: u64) -> String {
    let file = &mut scratch.file;
    let mut bytes = Vec::new();
    let _ = file
        .seek(SeekFrom::End(0))
        .and_then(|end| file.seek(SeekFrom::Start(end.saturating_sub(cap))))
        .and_then(|_| file.read_to_end(&mut bytes));
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// One run's running python calls (by instance slot), and the signal a
/// call's exit raises for the control loop. Taken out of the table under
/// the run lock, so stop and the loop never both claim one call.
#[derive(Default)]
pub(crate) struct PythonCalls {
    calls: Mutex<HashMap<Slot, PythonCall>>,
    exited: Arc<Notify>,
}

impl PythonCalls {
    fn calls(&self) -> std::sync::MutexGuard<'_, HashMap<Slot, PythonCall>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take(&self, slot: Slot) -> Option<PythonCall> {
        self.calls().remove(&slot)
    }

    fn any_finished(&self, slots: &[(Slot, f64)]) -> bool {
        let calls = self.calls();
        slots
            .iter()
            .any(|(slot, _)| calls.get(slot).is_some_and(PythonCall::finished))
    }

    /// Whether the slot's call exited (an unknown slot reads as not).
    pub(super) fn finished(&self, slot: Slot) -> bool {
        self.calls().get(&slot).is_some_and(PythonCall::finished)
    }
}

/// Python's `type(value).__name__` for a decoded JSON value.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `repr(sorted(names))`.
fn sorted_repr<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    py_repr(&PyValue::List(
        names
            .into_iter()
            .map(|name| PyValue::Str(name.to_string()))
            .collect(),
    ))
}

/// A python call's outputs from its stdout, or the error sentence when
/// stdout is not one object whose keys are exactly the declared outputs (a
/// `text` output must be a string).
fn python_outputs(spec: &Value, stdout: &str) -> Result<Map<String, Value>, String> {
    let Ok(parsed) = serde_json::from_str::<Value>(stdout) else {
        return Err("python state did not write one JSON object to stdout".to_string());
    };
    let Value::Object(outputs) = parsed else {
        return Err(format!(
            "python state returned {}, not a dict",
            type_name(&parsed)
        ));
    };
    let declared: Vec<(&str, &str)> = spec
        .get("outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|out| {
            Some((
                out.get("name").and_then(Value::as_str)?,
                out.get("type").and_then(Value::as_str)?,
            ))
        })
        .collect();
    let same_keys = outputs.len() == declared.len()
        && declared.iter().all(|(name, _)| outputs.contains_key(*name));
    if !same_keys {
        return Err(format!(
            "python state returned keys {} but declares outputs {}",
            sorted_repr(outputs.keys().map(String::as_str)),
            sorted_repr(declared.iter().map(|(name, _)| *name))
        ));
    }
    for (name, port_type) in &declared {
        if *port_type == "text" && !outputs[*name].is_string() {
            return Err(format!(
                "python state output {} is text but the call returned {}",
                py_str_repr(name),
                type_name(&outputs[*name])
            ));
        }
    }
    Ok(outputs)
}

impl FactoryRun {
    /// Whether the state at `position` runs a python callable.
    pub(super) fn runs_python(&self, position: usize) -> bool {
        self.states[position].spec.get("python").is_some()
    }

    /// A python call's time limit: the state's `budget_ms`, else the
    /// run's, else the default.
    fn python_limit_ms(&self, position: usize) -> u64 {
        self.states[position]
            .spec
            .get("budget_ms")
            .and_then(Value::as_u64)
            .or(self.run_budget_ms)
            .unwrap_or(PYTHON_STATE_TIMEOUT_MS_DEFAULT)
    }

    /// Every running python call: its slot and deadline.
    pub(super) fn python_slots(&self) -> Vec<(Slot, f64)> {
        let mut slots = Vec::new();
        for (state_position, state) in self.states.iter().enumerate() {
            for (entry_position, entry) in state.entries.iter().enumerate() {
                for (instance_position, instance) in entry.instances.iter().enumerate() {
                    if let (Status::Running, Some(deadline)) = (instance.status, instance.deadline)
                        && instance.child_id.is_none()
                    {
                        slots.push((
                            Slot {
                                state: state_position,
                                entry: entry_position,
                                instance: instance_position,
                            },
                            deadline,
                        ));
                    }
                }
            }
        }
        slots
    }
}

impl Inner {
    /// Start one python state process (`admit`'s counterpart).
    pub(super) async fn start_python(&self, cell: &Arc<RunCell>, slot: Slot) -> Admission {
        let (module, callable, inputs) = cell.with(|run| {
            let target = &run.states[slot.state].spec["python"];
            let field = |key: &str| target[key].as_str().unwrap_or_default().to_string();
            let (module, callable) = (field("import"), field("callable"));
            let instance = run.slot_instance(slot);
            instance.attempt += 1;
            (
                module,
                callable,
                instance.inputs.clone().unwrap_or_default(),
            )
        });
        let started = match self.python.command().await {
            Ok(command) => PythonCall::start(
                command,
                &module,
                &callable,
                &inputs,
                Arc::clone(&cell.python.exited),
            )
            .await
            .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        let process = match started {
            Ok(process) => process,
            Err(error) => {
                let reason = format!("python state could not start: {error}");
                self.apply_instance_failure(cell, slot, reason, /*retry*/ false)
                    .await;
                return Admission::Failed;
            }
        };
        let now = self.clock.now();
        let refused = cell.with(|run| {
            if run.state != RunState::Running {
                return Some(process);
            }
            let limit_ms = run.python_limit_ms(slot.state);
            let instance = run.slot_instance(slot);
            instance.spawned_at = Some(now);
            instance.deadline = Some(now + limit_ms as f64 / 1000.0);
            instance.status = Status::Running;
            let attempt = instance.attempt;
            cell.python.calls().insert(slot, process);
            run.record_at(
                kind::STARTED,
                slot,
                None,
                vec![("attempt", Value::from(attempt))],
            );
            None
        });
        if let Some(process) = refused {
            // stop() ran while the process was starting; its pass could not
            // see it.
            process.stop().await;
            cell.with(|run| {
                run.slot_instance(slot).status = Status::Cancelled;
                run.record_at(kind::CANCELLED, slot, None, Vec::new());
            });
            return Admission::Stopped;
        }
        Admission::Admitted
    }

    /// Wait for the first python process to exit, bounded by the nearest time
    /// limit and the collect poll timeout.
    pub(super) async fn wait_python(&self, cell: &Arc<RunCell>, slots: &[(Slot, f64)]) {
        let nearest = slots
            .iter()
            .map(|(_, deadline)| *deadline)
            .fold(f64::INFINITY, f64::min);
        let timeout = (nearest - self.clock.now())
            .max(0.0)
            .min(super::POLL_TIMEOUT_MS as f64 / 1000.0);
        if timeout <= 0.0 || cell.python.any_finished(slots) {
            return;
        }
        tokio::select! {
            () = cell.python.exited.notified() => {}
            () = self.clock.sleep(timeout) => {}
        }
    }

    /// Settle one python process that exited or ran past its time limit.
    pub(super) async fn apply_python_exit(&self, cell: &Arc<RunCell>, slot: Slot) {
        let now = self.clock.now();
        // Claimed under the run lock, like `cancel_running` claims a child.
        let claimed = cell.with(|run| {
            if run.slot_instance(slot).status != Status::Running {
                return None;
            }
            let process = cell.python.take(slot)?;
            let limit_ms = run.python_limit_ms(slot.state);
            let instance = run.slot_instance(slot);
            instance.duration_ms = instance
                .spawned_at
                .map(|spawned_at| ((now - spawned_at) * 1000.0).max(0.0) as u64);
            Some((process, limit_ms))
        });
        let Some((process, limit_ms)) = claimed else {
            return;
        };
        if !process.finished() {
            // Past its time limit: the limit is spent, so no retry.
            process.stop().await;
            let reason = format!("python state time limit {limit_ms}ms exceeded");
            self.apply_instance_failure(cell, slot, reason, /*retry*/ false)
                .await;
            return;
        }
        let exit = process.finish().await;
        let failure = cell.with(|run| {
            run.settle_count += 1;
            if exit.code != 0 {
                let detail = if exit.stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", exit.stderr)
                };
                return Some(format!(
                    "python state exited with code {}{detail}",
                    exit.code
                ));
            }
            let outputs = match python_outputs(&run.states[slot.state].spec, &exit.stdout) {
                Ok(outputs) => outputs,
                Err(error) => return Some(error),
            };
            let answer = char_prefix(
                &py_json_dumps_compact(&Value::Object(outputs.clone())),
                ANSWER_BINDING_CAP,
            )
            .to_string();
            let instance = run.slot_instance(slot);
            instance.status = Status::Done;
            instance.answer = Some(answer.clone());
            let duration = instance.duration_ms;
            run.record_at(
                kind::SETTLED,
                slot,
                None,
                vec![
                    ("status", Value::from("done")),
                    ("duration_ms", duration.map_or(Value::Null, Value::from)),
                ],
            );
            let at = run.slot_at(slot);
            let (node, entry, instance) = (at.node.map(str::to_string), at.entry, at.instance);
            run.event(
                kind::ANSWER_CAPTURED,
                super::model::EventAt {
                    node: node.as_deref(),
                    entry,
                    instance,
                    detail: None,
                },
                "arrived",
                vec![(
                    "answer",
                    Value::from(char_prefix(&answer, ANSWER_CAPTURE_CAP)),
                )],
            );
            let entry = &mut run.states[slot.state].entries[slot.entry];
            entry.answer = Some(answer);
            entry.outputs = Some(outputs);
            entry.output_errors = Some(Map::new());
            entry.status = Status::Done;
            run.queue_settle(slot.state, slot.entry);
            None
        });
        if let Some(reason) = failure {
            self.apply_instance_failure(cell, slot, reason, /*retry*/ true)
                .await;
        }
    }

    /// Kill every running python process of the run (stop, `fail_fast`, or a
    /// failed control loop: the calls have no other owner once the loop
    /// exits).
    pub(super) async fn cancel_python(&self, cell: &Arc<RunCell>) {
        loop {
            let claimed = cell.with(|run| {
                let (slot, _) = run
                    .python_slots()
                    .into_iter()
                    .find(|(slot, _)| cell.python.calls().contains_key(slot))?;
                let process = cell.python.take(slot)?;
                run.slot_instance(slot).status = Status::Cancelled;
                let entry = &mut run.states[slot.state].entries[slot.entry];
                if entry.status == Status::Running {
                    entry.status = Status::Cancelled;
                }
                run.touch();
                Some((slot, process))
            });
            let Some((slot, process)) = claimed else {
                return;
            };
            process.stop().await;
            cell.with(|run| run.record_at(kind::CANCELLED, slot, None, Vec::new()));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn outputs_must_be_one_object_with_exactly_the_declared_keys() {
        let spec = json!({ "outputs": [
            { "name": "out", "type": "json" },
            { "name": "note", "type": "text" }
        ] });
        let verdicts = [
            python_outputs(&spec, r#"{"out": [1], "note": "n"}"#),
            python_outputs(&spec, "not json"),
            python_outputs(&spec, "[1]"),
            python_outputs(&spec, r#"{"other": 1}"#),
            python_outputs(&spec, r#"{"out": 1, "note": 2}"#),
        ];
        assert_eq!(
            verdicts,
            [
                Ok(json!({ "out": [1], "note": "n" })
                    .as_object()
                    .cloned()
                    .unwrap()),
                Err("python state did not write one JSON object to stdout".to_string()),
                Err("python state returned list, not a dict".to_string()),
                Err(
                    "python state returned keys ['other'] but declares outputs ['note', 'out']"
                        .to_string()
                ),
                Err("python state output 'note' is text but the call returned int".to_string()),
            ]
        );
    }
}
