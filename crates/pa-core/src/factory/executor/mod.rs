//! The factory executor: runs canonicalized state-machine factories
//! through the session's child host, host-side.
//!
//! Ownership split: the child host owns the children (admission,
//! settlement, cancellation — the same path `rlm.spawn` takes); this
//! executor owns each run's state, in the session's host process, with a
//! durable record per run. A kernel restart or crash therefore never
//! touches a running workflow: the control loop keeps collecting,
//! admitting and transitioning, and the next kernel reads the same runs
//! through `rlm.factory`. A host restart reloads the records and pauses
//! the runs that were in flight as interrupted (see [`store`]).
//!
//! Machine semantics: admission enters every entry state; each settle is
//! queued and its outgoing transitions evaluated once — every passing
//! guard fires (fan-out is legal), a fire enters the target unless it is
//! out of `max_entries` (`transition_blocked`), and a self-loop or
//! back-edge re-enters its target with freshly re-bound inputs. A run
//! completes at quiescence: no entry in flight and no unevaluated settle.

pub mod binding;
pub(crate) mod engine;
pub mod model;
pub mod ports;
pub mod snapshot;
pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Map, Value};

pub use self::engine::{BACKOFF_MAX_ATTEMPTS, POLL_TIMEOUT_MS};
use self::model::{FactoryRun, LibraryOrigin, RunState, StateRun};
use self::ports::{FactoryChildren, FactoryClock, FactoryNotices};
pub use self::snapshot::{EVENT_WINDOW, GRAPH_EVENTS_TAIL, GRAPH_RUNS_WINDOW, LAST_FIRED_WINDOW};
use self::store::RunStore;
use crate::factory::pyvalue::{PyValue, py_repr, py_str_repr, py_strip};
use crate::factory::spec::{STATE_MAX_ENTRIES_DEFAULT, canonicalize_factory_spec};

/// Upper bound on one `watch` timeout (seconds).
pub const WATCH_TIMEOUT_CAP_SECONDS: f64 = 60.0;

/// A refusal the kernel raises as `ValueError` (unknown runs and specs,
/// invalid specs, unresolvable subagents, a resume of a run that is not
/// paused): the sentence is the Python executor's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FactoryRefusal(pub String);

/// One run's shared cell: the run behind a lock that is never held across
/// an await, the watch revision feed, the control-loop task, and the
/// persistence bookkeeping.
pub struct RunCell {
    run: Mutex<FactoryRun>,
    revision: tokio::sync::watch::Sender<u64>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    store: Option<RunStore>,
    /// Set while a record write is queued.
    pub(crate) dirty: AtomicBool,
}

impl RunCell {
    fn new(run: FactoryRun, store: Option<RunStore>) -> Arc<Self> {
        let revision = run.revision;
        Arc::new(Self {
            run: Mutex::new(run),
            revision: tokio::sync::watch::Sender::new(revision),
            task: Mutex::new(None),
            store,
            dirty: AtomicBool::new(false),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, FactoryRun> {
        self.run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run one synchronous segment against the run. A segment that changed
    /// the run (its revision moved) wakes every watcher and queues a record
    /// write.
    pub(crate) fn with<R>(self: &Arc<Self>, segment: impl FnOnce(&mut FactoryRun) -> R) -> R {
        let (result, revision, changed) = {
            let mut run = self.lock();
            let before = run.revision;
            let result = segment(&mut run);
            (result, run.revision, run.revision != before)
        };
        if changed {
            self.revision.send_replace(revision);
            if let Some(store) = &self.store {
                store.mark_dirty(self);
            }
        }
        result
    }

    fn set_task(&self, handle: tokio::task::JoinHandle<()>) {
        let previous = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(handle);
        // A superseded loop (resume bumped the generation) exits at its
        // next check on its own; it is not aborted mid-await.
        drop(previous);
    }

    fn take_task(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn run_id(&self) -> String {
        self.lock().run_id.clone()
    }
}

/// The shared executor core the control-loop tasks hold.
pub(crate) struct Inner {
    children: Arc<dyn FactoryChildren>,
    notices: Arc<dyn FactoryNotices>,
    clock: Arc<dyn FactoryClock>,
    store: Option<RunStore>,
    /// Every run this executor hosts, in start order.
    runs: Mutex<Vec<Arc<RunCell>>>,
}

impl Inner {
    fn runs(&self) -> Vec<Arc<RunCell>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn find(&self, run_id: &str) -> Option<Arc<RunCell>> {
        self.runs()
            .into_iter()
            .find(|cell| cell.lock().run_id == run_id)
    }
}

/// The executor's seams and its record directory.
pub struct FactoryExecutorConfig {
    pub children: Arc<dyn FactoryChildren>,
    pub notices: Arc<dyn FactoryNotices>,
    pub clock: Arc<dyn FactoryClock>,
    /// Where run records live; `None` keeps runs in memory only (sessions
    /// without an artifact directory).
    pub store_dir: Option<PathBuf>,
}

/// One harness subagent the kernel resolved for a run (`content` is the
/// prompt template, the metadata's `model`/`thinking` the spawn settings).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSubagent {
    pub content: PyValue,
    pub model: PyValue,
    pub thinking: PyValue,
}

/// One `run()`: the spec to validate, the subagent references the caller
/// resolved (`None` for a reference no harness entry carries), and the
/// library origin of a template run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRequest {
    pub spec_id: String,
    pub name: Option<String>,
    pub spec: PyValue,
    pub subagents: HashMap<String, Option<ResolvedSubagent>>,
    pub library: Option<LibraryOrigin>,
}

/// The executor behind `rlm.factory` and the `/factory` lane.
pub struct FactoryExecutor {
    inner: Arc<Inner>,
}

impl Drop for FactoryExecutor {
    fn drop(&mut self) {
        // The session is going away: its control loops stop with it (the
        // records keep the runs; a restarted session recovers them).
        for cell in self.inner.runs() {
            if let Some(task) = cell.take_task() {
                task.abort();
            }
        }
    }
}

/// The spawn settings `_validate_spawn_settings` checks: absent, or a
/// non-empty string.
fn spawn_setting(value: &PyValue, key: &str) -> Result<Option<String>, String> {
    match value {
        PyValue::None => Ok(None),
        PyValue::Str(text) if !py_strip(text).is_empty() => Ok(Some(text.clone())),
        _ => Err(format!(
            "subagent {key} must be a non-empty string when provided"
        )),
    }
}

/// One state's resolved spawn inputs.
struct Resolved {
    prompt: String,
    name: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
}

/// Resolve every state's subagent reference; collect ALL failures.
fn resolve_subagents(
    canonical: &PyValue,
    table: &HashMap<String, Option<ResolvedSubagent>>,
) -> Result<HashMap<String, Resolved>, String> {
    let mut resolved = HashMap::new();
    let mut errors = Vec::new();
    for state in canonical.get("states").as_list().unwrap_or(&[]) {
        let state_id = state.get("id").as_str().unwrap_or_default().to_string();
        let reference = state.get("subagent");
        let (prompt, name, model, thinking) = if reference.is_dict() {
            let name = reference
                .get("name")
                .as_str()
                .map(|name| py_strip(name).to_string())
                .filter(|name| !name.is_empty());
            (
                reference.get("prompt").clone(),
                name,
                reference.get("model").clone(),
                reference.get("thinking").clone(),
            )
        } else {
            let key = reference.as_str().unwrap_or_default();
            let Some(Some(entry)) = table.get(key) else {
                errors.push(format!(
                    "state {} references unknown subagent {}",
                    py_str_repr(&state_id),
                    py_repr(reference)
                ));
                continue;
            };
            (
                entry.content.clone(),
                None,
                entry.model.clone(),
                entry.thinking.clone(),
            )
        };
        let Some(prompt) = prompt
            .as_str()
            .filter(|prompt| !py_strip(prompt).is_empty())
        else {
            errors.push(format!(
                "state {} has an empty subagent prompt",
                py_str_repr(&state_id)
            ));
            continue;
        };
        let settings = spawn_setting(&model, "model").and_then(|model| {
            spawn_setting(&thinking, "thinking").map(|thinking| (model, thinking))
        });
        let (model, thinking) = match settings {
            Ok(settings) => settings,
            Err(error) => {
                errors.push(format!("state {} {error}", py_str_repr(&state_id)));
                continue;
            }
        };
        resolved.insert(
            state_id,
            Resolved {
                prompt: prompt.to_string(),
                name,
                model,
                thinking,
            },
        );
    }
    if errors.is_empty() {
        Ok(resolved)
    } else {
        Err(errors.join("; "))
    }
}

impl FactoryExecutor {
    /// Build the executor and recover every run recorded in `store_dir`.
    #[must_use]
    pub fn new(config: FactoryExecutorConfig) -> Self {
        let store = config.store_dir.map(RunStore::open);
        let inner = Arc::new(Inner {
            children: config.children,
            notices: config.notices,
            clock: config.clock,
            store: store.clone(),
            runs: Mutex::new(Vec::new()),
        });
        if let Some(store) = &store {
            let mut recovered = Vec::new();
            for mut run in store.load() {
                run.index();
                let before = run.revision;
                let state_before = run.state;
                store::recover_after_restart(&mut run);
                let changed = run.revision != before;
                if changed {
                    tracing::info!(
                        target: "pa_core::factory",
                        run_id = %run.run_id,
                        spec_id = %run.spec_id,
                        from = state_before.as_str(),
                        to = run.state.as_str(),
                        "factory run reconciled after a host restart"
                    );
                }
                let cell = RunCell::new(run, Some(store.clone()));
                if changed {
                    store.mark_dirty(&cell);
                }
                recovered.push(cell);
            }
            *inner
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = recovered;
        }
        Self { inner }
    }

    fn require_run(&self, run_id: &str) -> Result<Arc<RunCell>, FactoryRefusal> {
        self.inner
            .find(run_id)
            .ok_or_else(|| FactoryRefusal(format!("unknown factory run {}", py_str_repr(run_id))))
    }

    /// Validate a spec, resolve every state's subagent, and start a
    /// nonblocking run: admission enters the entry states up to
    /// `max_parallel` and returns; the control loop continues the run.
    ///
    /// # Errors
    ///
    /// The joined validation or resolution errors; nothing starts on any.
    pub async fn run(&self, request: RunRequest) -> Result<Value, FactoryRefusal> {
        let canonical = canonicalize_factory_spec(&request.spec).map_err(FactoryRefusal)?;
        let resolved = resolve_subagents(&canonical, &request.subagents).map_err(FactoryRefusal)?;
        let machine = canonical.to_json();
        let run_block = &machine["run"];
        let limit = |key: &str| {
            run_block
                .get(key)
                .and_then(Value::as_u64)
                .unwrap_or_default()
        };
        let mut states = Vec::new();
        for state_spec in machine["states"].as_array().cloned().unwrap_or_default() {
            let state_id = state_spec["id"].as_str().unwrap_or_default().to_string();
            let spawn = resolved.get(&state_id);
            states.push(StateRun {
                max_entries: state_spec
                    .get("max_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or(STATE_MAX_ENTRIES_DEFAULT as u64),
                state_id,
                spec: state_spec,
                prompt_template: spawn.map(|spawn| spawn.prompt.clone()),
                name: spawn.and_then(|spawn| spawn.name.clone()),
                model: spawn.and_then(|spawn| spawn.model.clone()),
                thinking: spawn.and_then(|spawn| spawn.thinking.clone()),
                entries_used: 0,
                entries: Vec::new(),
                instance_counter: 0,
                error: None,
                cancelled: false,
            });
        }
        let mut run = FactoryRun {
            run_id: uuid::Uuid::new_v4().simple().to_string(),
            spec_id: request.spec_id,
            name: request.name,
            max_parallel: limit("max_parallel"),
            max_transitions: limit("max_transitions"),
            max_children: limit("max_children"),
            run_budget_ms: run_block.get("budget_ms").and_then(Value::as_u64),
            machine,
            state: RunState::Running,
            started_at: self.inner.clock.now(),
            max_transitions_reported: false,
            max_children_reported: false,
            budget_reported: false,
            pause_reason: None,
            last_error: None,
            states,
            pending_evaluations: std::collections::VecDeque::new(),
            events: Vec::new(),
            milestones: std::collections::BTreeSet::new(),
            spawn_count: 0,
            settle_count: 0,
            transitions_fired: 0,
            tool_use_total: 0,
            loop_generation: 0,
            admission_backoff_until: None,
            join_fired: Vec::new(),
            revision: 0,
            library: request.library.clone(),
            transitions_from: HashMap::new(),
            position_of: HashMap::new(),
        };
        run.index();
        let cell = RunCell::new(run, self.inner.store.clone());
        self.inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&cell));
        let mut result = self.inner.launch_run(&cell).await;
        if let (Some(library), Some(object)) = (request.library, result.as_object_mut()) {
            object.insert("machine".into(), Value::from(library.name));
            if let Some(path) = library.path {
                object.insert("machine_path".into(), Value::from(path));
            }
        }
        Ok(result)
    }

    /// State reports, the trailing event window, elapsed time, and usage;
    /// marks the parent-unseen events `delivered`.
    ///
    /// # Errors
    ///
    /// An unknown run id.
    pub fn status(&self, run_id: &str) -> Result<Value, FactoryRefusal> {
        let cell = self.require_run(run_id)?;
        let now = self.inner.clock.now();
        Ok(cell.with(|run| snapshot::status_report(run, now)))
    }

    /// Cancel every running child and mark the run stopped (idempotent).
    ///
    /// # Errors
    ///
    /// An unknown run id.
    pub async fn stop(&self, run_id: &str) -> Result<Value, FactoryRefusal> {
        let cell = self.require_run(run_id)?;
        Ok(self.inner.stop(&cell).await)
    }

    /// Resume a paused run.
    ///
    /// # Errors
    ///
    /// An unknown run id, or a run that is not paused.
    pub async fn resume(&self, run_id: &str) -> Result<Value, FactoryRefusal> {
        let cell = self.require_run(run_id)?;
        self.inner.resume(&cell).await.map_err(FactoryRefusal)
    }

    /// The fused snapshot of one live run, or of every reportable run
    /// (`run_id = None`: every live run plus the newest terminal history,
    /// oldest first, as `{"runs": [...]}`). `Ok(None)` when `run_id` names
    /// no run (the caller falls back to a stored spec).
    #[must_use]
    pub fn graph(&self, run_id: Option<&str>, compact: bool) -> Option<Value> {
        let now = self.inner.clock.now();
        let Some(run_id) = run_id else {
            let runs = self.inner.runs();
            let live = |run: &FactoryRun| run.state.live() || run.running_instance_count() > 0;
            let terminal: Vec<String> = runs
                .iter()
                .filter_map(|cell| {
                    let run = cell.lock();
                    (!live(&run)).then(|| run.run_id.clone())
                })
                .collect();
            let newest: Vec<String> =
                terminal[terminal.len().saturating_sub(GRAPH_RUNS_WINDOW)..].to_vec();
            let rows: Vec<Value> = runs
                .iter()
                .filter_map(|cell| {
                    let run = cell.lock();
                    (live(&run) || newest.contains(&run.run_id))
                        .then(|| snapshot::graph_snapshot(&run, now, compact))
                })
                .collect();
            let mut result = Map::new();
            result.insert("runs".into(), Value::Array(rows));
            return Some(Value::Object(result));
        };
        let cell = self.inner.find(run_id)?;
        let run = cell.lock();
        Some(snapshot::graph_snapshot(&run, now, compact))
    }

    /// A stored spec's static graph (no live overlay).
    ///
    /// # Errors
    ///
    /// The spec's validation errors, as `factory spec <id> does not
    /// validate: <errors>`.
    pub fn spec_graph(
        &self,
        reference: &str,
        spec_id: &str,
        spec: &PyValue,
    ) -> Result<Value, FactoryRefusal> {
        let canonical = canonicalize_factory_spec(spec).map_err(|error| {
            FactoryRefusal(format!(
                "factory spec {} does not validate: {error}",
                py_str_repr(reference)
            ))
        })?;
        Ok(snapshot::spec_snapshot(spec_id, &canonical.to_json()))
    }

    /// Block until the run's state/instance shape changes or the bounded
    /// timeout (seconds, capped at [`WATCH_TIMEOUT_CAP_SECONDS`]) elapses,
    /// then return the graph snapshot plus `changed`. `timeout = None`
    /// stands for a value that is not a number at all.
    ///
    /// # Errors
    ///
    /// An unknown run id, or a missing, NaN, or negative timeout.
    pub async fn watch(
        &self,
        run_id: &str,
        timeout: Option<f64>,
        compact: bool,
    ) -> Result<Value, FactoryRefusal> {
        let cell = self.require_run(run_id)?;
        let timeout = timeout
            .filter(|timeout| !timeout.is_nan() && *timeout >= 0.0)
            .ok_or_else(|| {
                FactoryRefusal("timeout must be a non-negative number of seconds".into())
            })?
            .min(WATCH_TIMEOUT_CAP_SECONDS);
        let clock = Arc::clone(&self.inner.clock);
        let deadline = clock.now() + timeout;
        let mut revisions = cell.revision.subscribe();
        let baseline = snapshot::signature(&cell.lock());
        let mut changed = false;
        loop {
            revisions.borrow_and_update();
            if snapshot::signature(&cell.lock()) != baseline {
                changed = true;
                break;
            }
            let remaining = deadline - clock.now();
            if remaining <= 0.0 {
                break;
            }
            tokio::select! {
                _ = revisions.changed() => {}
                () = clock.sleep(remaining) => {}
            }
        }
        let now = clock.now();
        let mut snapshot = snapshot::graph_snapshot(&cell.lock(), now, compact);
        if let Some(object) = snapshot.as_object_mut() {
            let mut with_changed = Map::new();
            with_changed.insert("changed".into(), Value::Bool(changed));
            with_changed.extend(std::mem::take(object));
            *object = with_changed;
        }
        Ok(snapshot)
    }

    /// One run's export view (`export_machine`): its canonical machine,
    /// spec id, name, and id.
    #[must_use]
    pub fn run_machine(&self, run_id: &str) -> Option<Value> {
        let cell = self.inner.find(run_id)?;
        let run = cell.lock();
        Some(serde_json::json!({
            "run_id": run.run_id,
            "spec_id": run.spec_id,
            "name": run.name,
            "machine": run.machine,
        }))
    }

    /// How many runs are live (the `/factory off` guard's count).
    #[must_use]
    pub fn live_run_count(&self) -> usize {
        self.inner
            .runs()
            .iter()
            .filter(|cell| {
                let run = cell.lock();
                run.state.live() || run.running_instance_count() > 0
            })
            .count()
    }

    /// Wait for one run's current control loop to exit (it exits when the
    /// run leaves `running`, or its generation is superseded).
    pub async fn join(&self, run_id: &str) {
        let Some(cell) = self.inner.find(run_id) else {
            return;
        };
        loop {
            let Some(task) = cell.take_task() else {
                return;
            };
            let _ = task.await;
        }
    }

    /// Take one run's current control-loop handle (a test seam: the
    /// caller awaits a superseded loop's exit itself).
    #[cfg(test)]
    pub(crate) fn take_loop_task(&self, run_id: &str) -> Option<tokio::task::JoinHandle<()>> {
        self.inner.find(run_id)?.take_task()
    }

    /// Resolve once every record write queued so far is on disk.
    pub async fn flush(&self) {
        if let Some(store) = &self.inner.store {
            store.flush().await;
        }
    }

    /// Every run id this executor hosts, in start order.
    #[must_use]
    pub fn run_ids(&self) -> Vec<String> {
        self.inner.runs().iter().map(|cell| cell.run_id()).collect()
    }

    /// A copy of one run's model (tests and diagnostics).
    #[must_use]
    pub fn snapshot_run(&self, run_id: &str) -> Option<FactoryRun> {
        self.inner.find(run_id).map(|cell| cell.lock().clone())
    }
}

#[cfg(test)]
pub(crate) mod tests;
