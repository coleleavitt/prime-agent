//! The session feature: observes failures at every turn boundary, keeps the
//! session's local ledger and the machine's global one, stamps
//! `failure.fingerprint` on the `tool.execute` span of a failed call, and
//! appends the resolution hint to an `ipython` result whose failure was
//! fixed before.
//!
//! Hooks never block: a message is fingerprinted in memory (pure work) and
//! every disk read and write runs on the feature's own worker thread, one
//! job at a time, in the order the session produced them. The resolution
//! index runs on the blocking pool on the tool path, as the TS tool did.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pa_agent::types::{AgentMessage, Message};
use pa_core::features::{
    FeatureFuture, SessionFeature, SessionFeatureContext, ToolResultObservation,
};
use pa_telemetry::Properties;
use pa_types::trace_context::SPAN_ATTRIBUTES_TARGET;
use serde_json::Value;

use crate::extract::{observe_message, tool_result_text, IPYTHON_TOOL_NAME};
use crate::fingerprint::fingerprint_tool_result_text;
use crate::harness::{
    global_failure_ledger_enabled_from_env, global_harness_state_dir, local_harness_state_dir,
    with_harness_state_lock, HarnessDocument,
};
use crate::ledger::{
    apply_replay_verifications, merge_failure_observations, observation_ordinal,
    update_failure_ledger, FailureLedger, FailureObservation, FailureRecord, ReplayVerification,
    DEFAULT_RECURRENCE_THRESHOLD,
};
use crate::resolution::{ResolutionCell, ResolutionIndex, ResolutionIndexOptions, ResolutionStore};
use crate::resolution_store::open_resolution_store;

/// The adoption event of the resolution index: a hint was appended.
pub const RESOLUTION_HINT_EVENT: &str = "failure_resolution_hint";

/// How the feature behaves.
#[derive(Debug, Clone)]
pub struct LedgerOptions {
    /// Keep the global ledger; `None` reads `PRIME_AGENT_GLOBAL_LEDGER` at
    /// each use (on unless it says `0`, `off`, `false` or `no`).
    pub global_ledger: Option<bool>,
    /// Occurrences at which an actionable fingerprint recurs.
    pub recurrence_threshold: u64,
    /// Append resolution hints to `ipython` results.
    pub resolution_index: bool,
}

impl Default for LedgerOptions {
    fn default() -> Self {
        Self {
            global_ledger: None,
            recurrence_threshold: DEFAULT_RECURRENCE_THRESHOLD,
            resolution_index: true,
        }
    }
}

/// Which harness state a flush writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerScope {
    /// `<sessionArtifactDir>/harness/harness_state.json`, owned by one session.
    Local,
    /// `<agentDir>/harness/harness_state.json`, shared by every process.
    Global,
}

/// What one turn boundary observed and what it did to the ledgers.
#[derive(Debug)]
pub struct LedgerBoundary<'a> {
    /// Assistant turns on the session's branch at this boundary.
    pub turn: u64,
    /// The failures observed since the previous boundary.
    pub observations: &'a [FailureObservation],
    /// The session's local ledger after the update.
    pub local: &'a FailureLedger,
    /// The ledger recurrence is judged on: the global one (this session's
    /// unflushed observations merged in) when it is on and something was
    /// observed, the local one otherwise.
    pub effective: &'a FailureLedger,
    /// Records this boundary moved into the recurring set of `effective`.
    pub newly_recurring: &'a [FailureRecord],
    /// Fingerprints that recurred actionably: the occurrence is actionable
    /// and so is its record in `effective`. Distinct, in observation order.
    pub recurred_ids: &'a [String],
    /// `observation_ordinal(local)`.
    pub local_ordinal: u64,
    /// `observation_ordinal(effective)` when the global ledger was read.
    pub global_ordinal: Option<u64>,
    /// The global harness state as read for this boundary, when it was.
    pub global_state: Option<&'a HarnessDocument>,
}

/// A harness state write about to land; observers may set other keys of
/// the same document (under the lock, for the global scope).
pub struct LedgerFlush<'a> {
    pub scope: LedgerScope,
    pub session_id: &'a str,
    pub document: &'a mut HarnessDocument,
}

/// A consumer of the ledger (RAVO): it sees every boundary and may fold its
/// own state into the ledger's flushes. Every method runs on the ledger's
/// worker thread, with no ledger lock held, so it may call
/// [`LedgerHandle`] methods.
pub trait LedgerObserver: Send + Sync {
    /// A turn boundary updated the ledgers.
    fn on_boundary(&self, context: &Arc<SessionFeatureContext>, boundary: &LedgerBoundary<'_>) {
        let _ = (context, boundary);
    }

    /// Whether this session's flushes must wait (a refine plan binds the
    /// state); the ledger stays dirty and the next boundary retries.
    fn hold_flush(&self, session_id: &str) -> bool {
        let _ = session_id;
        false
    }

    /// Whether the session has global state of its own to write even when
    /// no observation is pending (regressions, trust evidence).
    fn wants_global_flush(&self, session_id: &str) -> bool {
        let _ = session_id;
        false
    }

    /// Whether the session has local state of its own to write even when
    /// its ledger is clean (trust evidence).
    fn wants_local_flush(&self, session_id: &str) -> bool {
        let _ = session_id;
        false
    }

    /// A flush is about to write `flush.document`.
    fn on_flush(&self, flush: &mut LedgerFlush<'_>) {
        let _ = flush;
    }

    /// Whether the flush announced by [`Self::on_flush`] landed.
    fn on_flush_result(&self, scope: LedgerScope, session_id: &str, landed: bool) {
        let _ = (scope, session_id, landed);
    }
}

/// Per-session bookkeeping.
struct SessionLedger {
    context: Arc<SessionFeatureContext>,
    /// Branch index of the next message (custom rows are not branch messages).
    next_entry: u64,
    /// Assistant messages on the branch.
    turns: u64,
    /// Failures observed since the last boundary, not yet stamped.
    unscanned: Vec<FailureObservation>,
    /// The local ledger while it holds unflushed changes.
    ledger: Option<FailureLedger>,
    dirty: bool,
    /// Observations not yet folded into the global ledger on disk.
    global_pending: Vec<FailureObservation>,
    local_verifications: Vec<ReplayVerification>,
    global_verifications: Vec<ReplayVerification>,
}

enum Job {
    Boundary {
        session: Arc<Mutex<SessionLedger>>,
        observations: Vec<FailureObservation>,
        turn: u64,
        through: u64,
    },
    Flush(Arc<Mutex<SessionLedger>>, FlushScope),
    Barrier(Sender<()>),
}

/// Which harness states a flush writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushScope {
    All,
    /// The global state only: the local one is written only where no cell
    /// runs, since the kernel writes it without a lock.
    Global,
}

type Clock = Arc<dyn Fn() -> String + Send + Sync>;

struct Inner {
    options: LedgerOptions,
    observers: Vec<Arc<dyn LedgerObserver>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionLedger>>>>,
    resolutions: Mutex<HashMap<String, Arc<Mutex<ResolutionIndex>>>>,
    worker: Mutex<Option<Sender<Job>>>,
    now: Clock,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The failure-ledger session feature (`pa-cli` installs it behind
/// `feature = "ledger"`).
pub struct FailureLedgerFeature {
    inner: Arc<Inner>,
}

/// A handle onto the feature's state for a consumer crate (RAVO).
#[derive(Clone)]
pub struct LedgerHandle {
    inner: Arc<Inner>,
}

impl Default for FailureLedgerFeature {
    fn default() -> Self {
        Self::new(LedgerOptions::default())
    }
}

impl FailureLedgerFeature {
    /// The feature with no observers.
    #[must_use]
    pub fn new(options: LedgerOptions) -> Self {
        Self::with_observers(options, Vec::new())
    }

    /// The feature reporting to `observers`.
    #[must_use]
    pub fn with_observers(options: LedgerOptions, observers: Vec<Arc<dyn LedgerObserver>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                observers,
                sessions: Mutex::new(HashMap::new()),
                resolutions: Mutex::new(HashMap::new()),
                worker: Mutex::new(None),
                now: Arc::new(crate::js::now_iso),
            }),
        }
    }

    /// Stamp observations with `now` instead of the wall clock (tests).
    ///
    /// # Panics
    ///
    /// When called after the feature was shared (`handle()` or a clone).
    #[must_use]
    pub fn with_clock(mut self, now: impl Fn() -> String + Send + Sync + 'static) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_clock before the feature is shared")
            .now = Arc::new(now);
        self
    }

    /// A handle for a consumer crate.
    #[must_use]
    pub fn handle(&self) -> LedgerHandle {
        LedgerHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Inner {
    fn global_enabled(&self) -> bool {
        self.options
            .global_ledger
            .unwrap_or_else(global_failure_ledger_enabled_from_env)
    }

    fn session(&self, context: &Arc<SessionFeatureContext>) -> Arc<Mutex<SessionLedger>> {
        let mut sessions = lock(&self.sessions);
        Arc::clone(
            sessions
                .entry(context.session_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(SessionLedger::new(context, 0, 0)))),
        )
    }

    fn existing(&self, session_id: &str) -> Option<Arc<Mutex<SessionLedger>>> {
        lock(&self.sessions).get(session_id).cloned()
    }

    fn submit(self: &Arc<Self>, job: Job) {
        let mut worker = lock(&self.worker);
        let sender = worker.get_or_insert_with(|| {
            let (sender, receiver) = mpsc::channel();
            let inner = Arc::downgrade(self);
            let spawned = std::thread::Builder::new()
                .name("failure-ledger".to_string())
                .spawn(move || run_worker(&inner, &receiver));
            if let Err(error) = spawned {
                tracing::warn!(%error, "failure ledger worker could not start");
            }
            sender
        });
        if sender.send(job).is_err() {
            *worker = None;
        }
    }

    fn wait_idle(self: &Arc<Self>, deadline: Instant) -> bool {
        if lock(&self.worker).is_none() {
            return true;
        }
        let (done, idle) = mpsc::channel();
        self.submit(Job::Barrier(done));
        let remaining = deadline.saturating_duration_since(Instant::now());
        idle.recv_timeout(remaining).is_ok()
    }

    fn boundary(
        &self,
        session: &Arc<Mutex<SessionLedger>>,
        mut observations: Vec<FailureObservation>,
        turn: u64,
        through: u64,
    ) {
        let mut state = lock(session);
        let Some(artifact_dir) = state.context.session_artifact_dir.clone() else {
            return;
        };
        let local_dir = local_harness_state_dir(&artifact_dir);
        let at = (self.now)();
        for observation in &mut observations {
            observation.turn = turn;
            observation.at.clone_from(&at);
        }
        let mut ledger = state
            .ledger
            .take()
            .unwrap_or_else(|| HarnessDocument::load(&local_dir).failures());
        // The branch shrank (rewind, branch switch): resume from the new tail.
        ledger.last_scanned_entry_index = ledger.last_scanned_entry_index.min(through);
        let threshold = Some(self.options.recurrence_threshold);
        let local = update_failure_ledger(&ledger, &observations, threshold, Some(through));
        state.ledger = Some(local.ledger.clone());
        state.dirty = true;
        let local_ordinal = observation_ordinal(Some(&local.ledger));
        let mut effective = local.ledger.clone();
        let mut newly_recurring = local.newly_recurring;
        let mut global_ordinal = None;
        let mut global_state = None;
        if self.global_enabled() && !observations.is_empty() {
            let document =
                HarnessDocument::load(&global_harness_state_dir(&state.context.agent_dir));
            let fresh = fresh_global(&document.failures(), &state.global_pending);
            let merged = merge_failure_observations(&fresh, &observations, threshold);
            state.global_pending.extend(observations.iter().cloned());
            global_ordinal = Some(observation_ordinal(Some(&merged.ledger)));
            effective = merged.ledger;
            newly_recurring = merged.newly_recurring;
            global_state = Some(document);
        }
        let mut recurred_ids: Vec<String> = Vec::new();
        for observation in &observations {
            let id = &observation.fingerprint.id;
            let record_actionable = effective
                .failures
                .get(id)
                .map_or_else(|| observation.is_actionable(), FailureRecord::is_actionable);
            if observation.is_actionable() && record_actionable && !recurred_ids.contains(id) {
                recurred_ids.push(id.clone());
            }
        }
        let context = Arc::clone(&state.context);
        drop(state);
        if !self.observers.is_empty() {
            let boundary = LedgerBoundary {
                turn,
                observations: &observations,
                local: &local.ledger,
                effective: &effective,
                newly_recurring: &newly_recurring,
                recurred_ids: &recurred_ids,
                local_ordinal,
                global_ordinal,
                global_state: global_state.as_ref(),
            };
            for observer in &self.observers {
                observer.on_boundary(&context, &boundary);
            }
        }
        self.flush(session, FlushScope::All);
    }

    /// Persist the session's pending local and global ledger changes.
    fn flush(&self, session: &Arc<Mutex<SessionLedger>>, scope: FlushScope) {
        let session_id = lock(session).context.session_id.clone();
        if self
            .observers
            .iter()
            .any(|observer| observer.hold_flush(&session_id))
        {
            return;
        }
        self.flush_global(session, &session_id);
        if scope == FlushScope::All {
            self.flush_local(session, &session_id);
        }
    }

    fn flush_local(&self, session: &Arc<Mutex<SessionLedger>>, session_id: &str) {
        let (ledger, verifications, local_dir) = {
            let state = lock(session);
            let Some(artifact_dir) = state.context.session_artifact_dir.as_deref() else {
                return;
            };
            if !state.dirty
                && state.local_verifications.is_empty()
                && !self
                    .observers
                    .iter()
                    .any(|observer| observer.wants_local_flush(session_id))
            {
                return;
            }
            (
                state.ledger.clone(),
                state.local_verifications.clone(),
                local_harness_state_dir(artifact_dir),
            )
        };
        let mut document = HarnessDocument::load(&local_dir);
        let ledger = ledger.unwrap_or_else(|| document.failures());
        document.set_failures(&apply_replay_verifications(&ledger, &verifications));
        self.announce_flush(LedgerScope::Local, session_id, &mut document);
        let landed = match document.save(&local_dir) {
            Ok(_) => true,
            Err(error) => {
                tracing::debug!(%error, "local failure ledger flush failed; the next boundary retries");
                false
            }
        };
        if landed {
            // Boundaries and flushes run one at a time on the worker, so
            // nothing replaced the ledger meanwhile: disk is authoritative.
            let mut state = lock(session);
            state.local_verifications.drain(..verifications.len());
            state.dirty = false;
            state.ledger = None;
        }
        self.report_flush(LedgerScope::Local, session_id, landed);
    }

    fn flush_global(&self, session: &Arc<Mutex<SessionLedger>>, session_id: &str) {
        if !self.global_enabled() {
            return;
        }
        let (pending, verifications, agent_dir) = {
            let state = lock(session);
            (
                state.global_pending.clone(),
                state.global_verifications.clone(),
                state.context.agent_dir.clone(),
            )
        };
        let observer_due = self
            .observers
            .iter()
            .any(|observer| observer.wants_global_flush(session_id));
        if pending.is_empty() && verifications.is_empty() && !observer_due {
            return;
        }
        let dir = global_harness_state_dir(&agent_dir);
        let span = tracing::info_span!(
            "harness.ledger.flush",
            session.id = session_id,
            ledger.scope = "global",
            ledger.observations = pending.len(),
            ledger.verifications = verifications.len(),
            ledger.fingerprints = tracing::field::Empty,
            error = tracing::field::Empty,
        );
        let _entered = span.enter();
        let written = with_harness_state_lock(&dir, || {
            let mut document = HarnessDocument::load(&dir);
            let merged = merge_failure_observations(&document.failures(), &pending, None);
            let ledger = apply_replay_verifications(&merged.ledger, &verifications);
            document.set_failures(&ledger);
            self.announce_flush(LedgerScope::Global, session_id, &mut document);
            document.save(&dir).map(|_| ledger.failures.len())
        });
        let landed = match written {
            Ok(Ok(fingerprints)) => {
                span.record("ledger.fingerprints", fingerprints);
                true
            }
            Ok(Err(error)) | Err(error) => {
                span.record("error", error.to_string());
                false
            }
        };
        if landed {
            let mut state = lock(session);
            state.global_pending.drain(..pending.len());
            state.global_verifications.drain(..verifications.len());
        }
        self.report_flush(LedgerScope::Global, session_id, landed);
    }

    fn announce_flush(&self, scope: LedgerScope, session_id: &str, document: &mut HarnessDocument) {
        for observer in &self.observers {
            observer.on_flush(&mut LedgerFlush {
                scope,
                session_id,
                document,
            });
        }
    }

    fn report_flush(&self, scope: LedgerScope, session_id: &str, landed: bool) {
        for observer in &self.observers {
            observer.on_flush_result(scope, session_id, landed);
        }
    }

    fn resolution_index(&self, context: &SessionFeatureContext) -> Arc<Mutex<ResolutionIndex>> {
        let mut resolutions = lock(&self.resolutions);
        Arc::clone(
            resolutions
                .entry(context.session_id.clone())
                .or_insert_with(|| {
                    let store = open_resolution_store(&context.cwd, &context.agent_dir)
                        .map(|store| Box::new(store) as Box<dyn ResolutionStore>);
                    Arc::new(Mutex::new(ResolutionIndex::new(ResolutionIndexOptions {
                        store,
                        ..ResolutionIndexOptions::default()
                    })))
                }),
        )
    }
}

/// The global ledger on disk with `pending` (this session's unflushed
/// observations) folded in.
fn fresh_global(on_disk: &FailureLedger, pending: &[FailureObservation]) -> FailureLedger {
    if pending.is_empty() {
        on_disk.clone()
    } else {
        merge_failure_observations(on_disk, pending, None).ledger
    }
}

fn run_worker(inner: &std::sync::Weak<Inner>, receiver: &Receiver<Job>) {
    while let Ok(job) = receiver.recv() {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        match job {
            Job::Boundary {
                session,
                observations,
                turn,
                through,
            } => inner.boundary(&session, observations, turn, through),
            Job::Flush(session, scope) => inner.flush(&session, scope),
            Job::Barrier(done) => {
                let _ = done.send(());
            }
        }
    }
}

impl SessionLedger {
    fn new(context: &Arc<SessionFeatureContext>, next_entry: u64, turns: u64) -> Self {
        Self {
            context: Arc::clone(context),
            next_entry,
            turns,
            unscanned: Vec::new(),
            ledger: None,
            dirty: false,
            global_pending: Vec::new(),
            local_verifications: Vec::new(),
            global_verifications: Vec::new(),
        }
    }
}

/// How an `ipython` cell that ran ended, from its details: `Some(failed)`
/// for status `ok` / `error` / `aborted`, `None` for anything else (a
/// crashed kernel, a thrown error) — the TS tool fed its index only from the
/// cells that ran, with its own `isError` (`error` or `aborted`). The loop's
/// flag cannot stand in for it: a cell that raised returns normally.
fn executed_cell(details: &Value) -> Option<bool> {
    let details = details.as_object()?;
    if details.contains_key("kernelCrashed") {
        return None;
    }
    match details.get("status").and_then(Value::as_str)? {
        "ok" => Some(false),
        "error" | "aborted" => Some(true),
        _ => None,
    }
}

impl SessionFeature for FailureLedgerFeature {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, history: &[AgentMessage]) {
        let branch = history
            .iter()
            .filter(|message| matches!(message, AgentMessage::Standard(_)))
            .count() as u64;
        let turns = history
            .iter()
            .filter(|message| matches!(message, AgentMessage::Standard(Message::Assistant(_))))
            .count() as u64;
        lock(&self.inner.sessions).insert(
            context.session_id.clone(),
            Arc::new(Mutex::new(SessionLedger::new(context, branch, turns))),
        );
    }

    fn on_message_end(&self, context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        let AgentMessage::Standard(standard) = message else {
            return;
        };
        let session = self.inner.session(context);
        let job = {
            let mut state = lock(&session);
            let entry = state.next_entry;
            state.next_entry += 1;
            if let Some(observation) = observe_message(message, entry, 0, &String::new) {
                state.unscanned.push(observation);
            }
            if !matches!(standard, Message::Assistant(_)) {
                return;
            }
            state.turns += 1;
            let observations = std::mem::take(&mut state.unscanned);
            if state.context.session_artifact_dir.is_none() {
                return;
            }
            Job::Boundary {
                session: Arc::clone(&session),
                observations,
                turn: state.turns,
                through: entry + 1,
            }
        };
        self.inner.submit(job);
    }

    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        let text = tool_result_text(&result.content);
        let tool_name = Some(result.tool_name.as_str());
        // Runs inside the call's `tool.execute` span: the span key and the
        // ledger key are the same string by construction.
        if let Some(fingerprint) = fingerprint_tool_result_text(tool_name, &text, result.is_error) {
            tracing::event!(
                target: SPAN_ATTRIBUTES_TARGET,
                tracing::Level::INFO,
                failure.fingerprint = fingerprint.id.as_str()
            );
        }
        let code = result.args.get("code").and_then(Value::as_str);
        let (true, IPYTHON_TOOL_NAME, Some(code), Some(is_error)) = (
            self.inner.options.resolution_index,
            result.tool_name.as_str(),
            code,
            executed_cell(&result.details),
        ) else {
            return Box::pin(async { None });
        };
        let code = code.to_string();
        let inner = Arc::clone(&self.inner);
        let context = Arc::clone(context);
        Box::pin(async move {
            let observed = tokio::task::spawn_blocking(move || {
                let index = inner.resolution_index(&context);
                let hint = lock(&index).observe(ResolutionCell {
                    code: &code,
                    output: &text,
                    is_error,
                });
                (hint, context)
            })
            .await;
            let (hint, context) = observed.ok()?;
            let hint = hint?;
            if let Some(telemetry) = &context.telemetry {
                let mut properties = Properties::new();
                properties.set("origin", Value::from(hint.origin.as_str()));
                telemetry.track(RESOLUTION_HINT_EVENT, &properties);
            }
            Some(hint.text)
        })
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        if let Some(session) = self.inner.existing(&context.session_id) {
            self.inner.submit(Job::Flush(session, FlushScope::All));
        }
    }

    fn flush(&self, deadline: Instant) {
        let sessions: Vec<_> = lock(&self.inner.sessions).values().cloned().collect();
        if lock(&self.inner.worker).is_none() {
            return;
        }
        for session in sessions {
            self.inner.submit(Job::Flush(session, FlushScope::All));
        }
        if !self.inner.wait_idle(deadline) {
            tracing::debug!("failure ledger flush abandoned at the exit deadline");
        }
    }
}

impl LedgerHandle {
    /// Whether the global ledger is on.
    #[must_use]
    pub fn global_ledger_enabled(&self) -> bool {
        self.inner.global_enabled()
    }

    /// Assistant turns on a session's branch (the ledger's turn clock).
    #[must_use]
    pub fn turn(&self, session_id: &str) -> Option<u64> {
        self.inner
            .existing(session_id)
            .map(|session| lock(&session).turns)
    }

    /// The session's local ledger: the unflushed one, else the one on disk.
    #[must_use]
    pub fn session_ledger(&self, session_id: &str) -> Option<FailureLedger> {
        let session = self.inner.existing(session_id)?;
        let state = lock(&session);
        if let Some(ledger) = &state.ledger {
            return Some(ledger.clone());
        }
        let dir = local_harness_state_dir(state.context.session_artifact_dir.as_deref()?);
        drop(state);
        Some(HarnessDocument::load(&dir).failures())
    }

    /// The global ledger as it stands now: on disk, with `session_id`'s
    /// unflushed observations folded in.
    #[must_use]
    pub fn fresh_global_ledger(&self, agent_dir: &Path, session_id: Option<&str>) -> FailureLedger {
        let on_disk = HarnessDocument::load(&global_harness_state_dir(agent_dir)).failures();
        let pending = session_id
            .and_then(|id| self.inner.existing(id))
            .map(|session| lock(&session).global_pending.clone())
            .unwrap_or_default();
        fresh_global(&on_disk, &pending)
    }

    /// Queue replay verifications for the session's next flush (both
    /// ledgers when the global one is on).
    pub fn record_replay_verifications(
        &self,
        session_id: &str,
        verifications: &[ReplayVerification],
    ) {
        let Some(session) = self.inner.existing(session_id) else {
            return;
        };
        let global = self.inner.global_enabled();
        let mut state = lock(&session);
        state
            .local_verifications
            .extend(verifications.iter().cloned());
        if global {
            state
                .global_verifications
                .extend(verifications.iter().cloned());
        }
    }

    /// Ask the worker to flush the session.
    pub fn request_flush(&self, session_id: &str) {
        if let Some(session) = self.inner.existing(session_id) {
            self.inner.submit(Job::Flush(session, FlushScope::All));
        }
    }

    /// Ask the worker to flush the session's global state only: for a
    /// caller that may run while the session's kernel runs a cell (the
    /// kernel writes the local state without a lock).
    pub fn request_global_flush(&self, session_id: &str) {
        if let Some(session) = self.inner.existing(session_id) {
            self.inner.submit(Job::Flush(session, FlushScope::Global));
        }
    }

    /// Wait until every job queued so far ran, up to `timeout`; `false`
    /// when the deadline passed first.
    #[must_use]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        self.inner.wait_idle(Instant::now() + timeout)
    }

    /// The session's local harness state directory.
    #[must_use]
    pub fn local_harness_state_dir(&self, session_id: &str) -> Option<PathBuf> {
        let session = self.inner.existing(session_id)?;
        let state = lock(&session);
        state
            .context
            .session_artifact_dir
            .as_deref()
            .map(local_harness_state_dir)
    }
}
