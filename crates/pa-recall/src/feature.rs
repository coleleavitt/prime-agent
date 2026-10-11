//! Workspace Recall as a session feature: stores the invalidation, not the
//! answer. At the end of every agent run of a top-level session it writes
//! the repo's mark of digests (HEAD, index, dirty paths, build claims) on
//! its own background worker. The first `ipython` result of a top-level
//! session recomputes every digest against the live filesystem and gets a
//! bounded `<workspace_recall>` block appended, saying what changed, what
//! is provably unchanged, and what could not be checked.
//!
//! A build claim is a `bash()` build or test command that exited 0 inside
//! an `ipython` cell during which the workspace digest did not move.
//!
//! Every failure degrades to the native behaviour: no mark and no block.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pa_core::features::{
    FeatureFuture,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    ToolResultObservation,
};
use tokio::sync::watch;
use tracing::field::Empty;

use crate::claims::{
    ClaimStatus,
    RECALL_MAX_CLAIMS,
    is_build_claim_command,
    mentions_build_command,
};
use crate::git::{find_recall_repo, resolve_repo_root};
use crate::mark::{
    CaptureFailure,
    RECALL_UNVERIFIABLE,
    capture_workspace,
    is_fully_verifiable,
    workspace_digest,
};
use crate::render::{RECALL_BLOCK_MAX_BYTES, render_recall_block};
use crate::store::{
    MarkSkipReason,
    RecallClaimInput,
    RecallMarkFile,
    WrittenMark,
    read_recall_mark,
    read_recall_skip,
    recall_repo_key,
    write_recall_mark,
    write_recall_skip,
};
use crate::time::{format_iso, now_ms};
use crate::witness::witness_workspace;

/// The kill switch: `0`, `off`, `false` or `no` disables Workspace Recall.
pub const WORKSPACE_RECALL_ENV: &str = "PRIME_AGENT_WORKSPACE_RECALL";

const WITNESS_TOOL_NAME: &str = "ipython";
/// How long a witness waits for its session's first mark to record the
/// mark it is about to replace.
const PRIOR_MARK_WAIT: Duration = Duration::from_millis(300);
/// Recall work on the tool path (witness, cell digests) gives up after this.
const TOOL_PATH_DEADLINE: Duration = Duration::from_secs(1);
/// How long a missed deadline keeps this process, and no other, off the
/// repo's tool path: the stall may have been its own.
const DEADLINE_SKIP_TTL_MS: i64 = 60_000;
const MAX_TRACKED_CELLS: usize = 32;

/// Whether a kill-switch value leaves recall enabled.
#[must_use]
pub fn is_workspace_recall_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    })
}

/// Why a session's mark was not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Mark(MarkSkipReason),
    /// The kill switch is set.
    Disabled,
    /// RLM children share the parent's workspace and never mark.
    ChildSession,
}

/// How one mark attempt settled; reported to [`RecallOptions::on_mark_settled`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkOutcome {
    Written {
        session_id: String,
        repo_root: String,
        repo_key: String,
        mark: Box<RecallMarkFile>,
    },
    Skipped {
        session_id: String,
        reason: SkipReason,
    },
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Writes one repo's mark; replaceable for tests that need a mark that
/// fails or never settles.
pub type MarkWriter = Arc<
    dyn Fn(
            String,
            PathBuf,
            Vec<RecallClaimInput>,
            i64,
        ) -> BoxFuture<Result<WrittenMark, MarkSkipReason>>
        + Send
        + Sync,
>;

/// Embedding and test hooks; [`RecallOptions::default`] is the product.
#[derive(Clone, Default)]
pub struct RecallOptions {
    /// Called after each mark attempt settles, including skipped ones.
    pub on_mark_settled: Option<Arc<dyn Fn(MarkOutcome) + Send + Sync>>,
    /// Replaces the mark writer.
    pub write_mark: Option<MarkWriter>,
    /// Replaces the wall clock (epoch milliseconds) for the skip windows.
    pub clock: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    /// Replaces the [`WORKSPACE_RECALL_ENV`] kill-switch read.
    pub enabled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WitnessState {
    Pending,
    Done,
}

struct PendingMark {
    /// Turns true once the first run has recorded the mark it replaces, or
    /// has decided not to write.
    prior_ready: watch::Receiver<bool>,
    rerun: Option<Arc<SessionFeatureContext>>,
}

#[derive(Debug, Clone)]
struct TrackedCell {
    session_id: String,
    repo_root: String,
    digest: String,
}

#[derive(Debug, Clone)]
struct PendingClaim {
    repo_root: String,
    claim: RecallClaimInput,
}

#[derive(Default)]
struct State {
    /// Sessions whose witness is decided.
    witness: HashMap<String, WitnessState>,
    /// The mark a session's own first mark replaced, kept until its first
    /// witness. Without it a session that answers once without ipython
    /// would witness against the mark it just wrote and report nothing.
    prior_marks: HashMap<String, Option<RecallMarkFile>>,
    pending_marks: HashMap<String, PendingMark>,
    /// Workspace digest taken before a build cell ran, by tool call id.
    tracked_cells: VecDeque<(String, TrackedCell)>,
    /// Claims waiting for the session's next mark.
    pending_claims: HashMap<String, Vec<PendingClaim>>,
    /// Repos whose git timed out, by repo key, to an epoch ms; mirrored on
    /// disk for every process sharing the agent dir.
    timed_out_repos: HashMap<String, i64>,
    /// Repos whose tool-path recall missed its deadline, by repo key, to an
    /// epoch ms. In memory only, never shared.
    deadline_misses: HashMap<String, i64>,
}

/// The mark worker: its own runtime thread, so marks survive the host
/// runtime and `flush` can wait for them at process exit.
struct MarkWorker {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    in_flight: Mutex<usize>,
    idle: Condvar,
}

struct Inner {
    options: RecallOptions,
    state: Mutex<State>,
    worker: MarkWorker,
}

/// The Workspace Recall session feature.
#[derive(Clone)]
pub struct WorkspaceRecall {
    inner: Arc<Inner>,
}

impl Default for WorkspaceRecall {
    fn default() -> Self {
        Self::new(RecallOptions::default())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPathSkip {
    GitTimeout,
    Deadline,
}

impl ToolPathSkip {
    fn as_str(self) -> &'static str {
        match self {
            ToolPathSkip::GitTimeout => "git_timeout",
            ToolPathSkip::Deadline => "deadline",
        }
    }
}

struct CellDigest {
    digest: String,
    verifiable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestPhase {
    ToolCall,
    ToolResult,
}

fn mark_skip_label(reason: &MarkSkipReason) -> &'static str {
    match reason {
        MarkSkipReason::GitUnavailable => "git_unavailable",
        MarkSkipReason::GitTimeout => "git_timeout",
        MarkSkipReason::LockBusy => "lock_busy",
        MarkSkipReason::WriteFailed(_) => "write_failed",
        MarkSkipReason::NotRepo => "not_repo",
    }
}

fn capture_label(failure: CaptureFailure) -> &'static str {
    match failure {
        CaptureFailure::GitUnavailable => "git_unavailable",
        CaptureFailure::GitTimeout => "git_timeout",
        CaptureFailure::NotRepo => "not_repo",
    }
}

impl WorkspaceRecall {
    #[must_use]
    pub fn new(options: RecallOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                state: Mutex::new(State::default()),
                worker: MarkWorker {
                    runtime: Mutex::new(None),
                    in_flight: Mutex::new(0),
                    idle: Condvar::new(),
                },
            }),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(runtime) = lock(&self.worker.runtime).take() {
            runtime.shutdown_background();
        }
    }
}

impl Inner {
    fn enabled(&self) -> bool {
        match &self.options.enabled {
            Some(enabled) => enabled(),
            None => {
                is_workspace_recall_enabled(std::env::var(WORKSPACE_RECALL_ENV).ok().as_deref())
            }
        }
    }

    fn now(&self) -> i64 {
        self.options
            .clock
            .as_ref()
            .map_or_else(now_ms, |clock| clock())
    }

    fn settle(&self, outcome: MarkOutcome) {
        if let Some(callback) = &self.options.on_mark_settled {
            callback(outcome);
        }
    }

    fn git_timeout_until(&self, repo_root: &str, repo_key: &str, agent_dir: &Path) -> Option<i64> {
        let now = self.now();
        if let Some(until) = lock(&self.state).timed_out_repos.get(repo_key).copied() {
            if until > now {
                return Some(until);
            }
        }
        let stored = read_recall_skip(repo_root, agent_dir, now);
        let mut state = lock(&self.state);
        match stored {
            Some(until) => state.timed_out_repos.insert(repo_key.to_string(), until),
            None => state.timed_out_repos.remove(repo_key),
        };
        stored
    }

    /// Why the tool path should leave this repo alone right now, if it should.
    fn tool_path_skip(
        &self,
        repo_root: &str,
        repo_key: &str,
        agent_dir: &Path,
    ) -> Option<ToolPathSkip> {
        let now = self.now();
        {
            let mut state = lock(&self.state);
            if let Some(until) = state.deadline_misses.get(repo_key).copied() {
                if until > now {
                    return Some(ToolPathSkip::Deadline);
                }
                state.deadline_misses.remove(repo_key);
            }
        }
        self.git_timeout_until(repo_root, repo_key, agent_dir)
            .map(|_| ToolPathSkip::GitTimeout)
    }

    fn remember_git_timeout(&self, repo_root: &str, repo_key: &str, agent_dir: &Path) {
        let until = write_recall_skip(repo_root, agent_dir, self.now());
        lock(&self.state)
            .timed_out_repos
            .insert(repo_key.to_string(), until);
    }

    fn remember_deadline_miss(&self, repo_key: &str) {
        let until = self.now() + DEADLINE_SKIP_TTL_MS;
        lock(&self.state)
            .deadline_misses
            .insert(repo_key.to_string(), until);
    }

    fn add_claims(&self, session_id: &str, claims: Vec<PendingClaim>) {
        if claims.is_empty() {
            return;
        }
        let mut state = lock(&self.state);
        let held = state
            .pending_claims
            .entry(session_id.to_string())
            .or_default();
        held.extend(claims);
        let excess = held.len().saturating_sub(RECALL_MAX_CLAIMS);
        held.drain(..excess);
    }

    fn take_claims(&self, session_id: &str, repo_root: &str) -> Vec<PendingClaim> {
        lock(&self.state)
            .pending_claims
            .remove(session_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|claim| claim.repo_root == repo_root)
            .collect()
    }

    fn track_cell(&self, tool_call_id: String, cell: TrackedCell) {
        let mut state = lock(&self.state);
        state.tracked_cells.retain(|(id, _)| *id != tool_call_id);
        state.tracked_cells.push_back((tool_call_id, cell));
        // A blocked or abandoned call never reaches its result; drop the
        // oldest instead of growing.
        while state.tracked_cells.len() > MAX_TRACKED_CELLS {
            state.tracked_cells.pop_front();
        }
    }

    fn take_tracked_cell(&self, tool_call_id: &str) -> Option<TrackedCell> {
        let mut state = lock(&self.state);
        let position = state
            .tracked_cells
            .iter()
            .position(|(id, _)| id == tool_call_id)?;
        state.tracked_cells.remove(position).map(|(_, cell)| cell)
    }

    /// Workspace digest for build claims, bounded like the witness; `None`
    /// when it could not be taken in time.
    async fn cell_digest(
        &self,
        phase: DigestPhase,
        repo_root: &str,
        agent_dir: &Path,
        compare_to: Option<&str>,
    ) -> Option<CellDigest> {
        let started = Instant::now();
        let repo_key = recall_repo_key(repo_root, agent_dir);
        let span = tracing::info_span!(
            "recall.digest",
            recall.phase = match phase {
                DigestPhase::ToolCall => "tool_call",
                DigestPhase::ToolResult => "tool_result",
            },
            recall.repo_key = %repo_key,
            recall.skipped = Empty,
            recall.skip_reason = Empty,
            recall.negative_cache = Empty,
            recall.verifiable = Empty,
            recall.digest_matched = Empty,
            recall.ms = Empty,
        );
        let result = async {
            if let Some(skip) = self.tool_path_skip(repo_root, &repo_key, agent_dir) {
                span.record("recall.skipped", true);
                span.record("recall.skip_reason", skip.as_str());
                span.record("recall.negative_cache", true);
                return None;
            }
            let captured = tokio::time::timeout(
                TOOL_PATH_DEADLINE,
                capture_workspace(repo_root, Some(agent_dir)),
            )
            .await;
            let snapshot = match captured {
                Err(_) => {
                    span.record("recall.skipped", true);
                    span.record("recall.skip_reason", "deadline");
                    self.remember_deadline_miss(&repo_key);
                    return None;
                }
                Ok(Err(failure)) => {
                    span.record("recall.skipped", true);
                    span.record("recall.skip_reason", capture_label(failure));
                    if failure == CaptureFailure::GitTimeout {
                        self.remember_git_timeout(repo_root, &repo_key, agent_dir);
                    }
                    return None;
                }
                Ok(Ok(snapshot)) => snapshot,
            };
            let digest = CellDigest {
                digest: workspace_digest(&snapshot.state),
                verifiable: is_fully_verifiable(&snapshot.state),
            };
            span.record("recall.verifiable", digest.verifiable);
            if let Some(compare_to) = compare_to {
                span.record("recall.digest_matched", digest.digest == compare_to);
            }
            Some(digest)
        }
        .await;
        span.record(
            "recall.ms",
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        result
    }

    async fn record_cell_claims(
        &self,
        host_facts: &serde_json::Value,
        cell: TrackedCell,
        agent_dir: &Path,
    ) {
        let commands: Vec<String> = host_facts
            .get("bashCommands")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|command| {
                command.get("exitCode").and_then(serde_json::Value::as_i64) == Some(0)
                    && command
                        .get("commandTruncated")
                        .and_then(serde_json::Value::as_bool)
                        != Some(true)
            })
            .filter_map(|command| command.get("command")?.as_str().map(str::to_string))
            .filter(|command| is_build_claim_command(command))
            .collect();
        if commands.is_empty() {
            return;
        }
        let after = self
            .cell_digest(
                DigestPhase::ToolResult,
                &cell.repo_root,
                agent_dir,
                Some(&cell.digest),
            )
            .await;
        if !after.is_some_and(|after| after.verifiable && after.digest == cell.digest) {
            return;
        }
        let at = format_iso(self.now());
        let claims = commands
            .into_iter()
            .map(|command| PendingClaim {
                repo_root: cell.repo_root.clone(),
                claim: RecallClaimInput {
                    command,
                    exit_code: 0,
                    digest_at_claim: Some(cell.digest.clone()),
                    at: Some(at.clone()),
                },
            })
            .collect();
        self.add_claims(&cell.session_id, claims);
    }

    async fn witness_against_mark(
        &self,
        span: &tracing::Span,
        session_id: &str,
        repo_root: &str,
        agent_dir: &Path,
    ) -> Option<String> {
        let prior_ready = lock(&self.state)
            .pending_marks
            .get(session_id)
            .map(|pending| pending.prior_ready.clone());
        if let Some(mut prior_ready) = prior_ready {
            let _ =
                tokio::time::timeout(PRIOR_MARK_WAIT, prior_ready.wait_for(|ready| *ready)).await;
        }
        let has_prior = lock(&self.state).prior_marks.contains_key(session_id);
        let read = if has_prior {
            None
        } else {
            read_recall_mark(repo_root, agent_dir)
        };
        // The session's first mark may have recorded its prior copy while
        // the file was being read; that copy wins.
        let mark = match lock(&self.state).prior_marks.remove(session_id) {
            Some(prior) => prior,
            None => read,
        };
        let Some(mark) = mark else {
            span.record("recall.has_mark", false);
            span.record("recall.block_bytes", 0);
            return None;
        };
        span.record("recall.has_mark", true);
        let report = match witness_workspace(repo_root, &mark, agent_dir).await {
            Ok(report) => report,
            Err(failure) => {
                span.record("recall.skipped", true);
                span.record("recall.skip_reason", capture_label(failure));
                span.record("recall.block_bytes", 0);
                if failure == CaptureFailure::GitTimeout {
                    self.remember_git_timeout(
                        repo_root,
                        &recall_repo_key(repo_root, agent_dir),
                        agent_dir,
                    );
                }
                return None;
            }
        };
        let block = render_recall_block(&report, RECALL_BLOCK_MAX_BYTES);
        let current = report
            .claims
            .iter()
            .filter(|verdict| verdict.status == ClaimStatus::Current)
            .count();
        span.record("recall.changed", report.changed.len());
        span.record(
            "recall.changed_unknown",
            report.changed_unknown_reason.is_some(),
        );
        if let Some(unchanged) = report.unchanged_count {
            span.record("recall.unchanged", unchanged);
        }
        span.record("recall.unverifiable", report.unverifiable.len());
        span.record("recall.uncompared", report.uncompared_count);
        span.record("recall.claims_current", current);
        span.record("recall.claims_expired", report.claims.len() - current);
        span.record("recall.head_moved", report.head_moved);
        span.record("recall.block_bytes", block.len());
        Some(block)
    }

    async fn witness(
        &self,
        context: &SessionFeatureContext,
        earlier_results: usize,
    ) -> Option<String> {
        if context.rlm_depth > 0 || earlier_results > 0 {
            return None;
        }
        let repo_root = resolve_repo_root(&find_recall_repo(&context.cwd)?)?;
        let agent_dir = &context.agent_dir;
        let repo_key = recall_repo_key(&repo_root, agent_dir);
        let span = tracing::info_span!(
            "recall.witness",
            recall.repo_key = %repo_key,
            recall.skipped = Empty,
            recall.skip_reason = Empty,
            recall.negative_cache = Empty,
            recall.has_mark = Empty,
            recall.changed = Empty,
            recall.changed_unknown = Empty,
            recall.unchanged = Empty,
            recall.unverifiable = Empty,
            recall.uncompared = Empty,
            recall.claims_current = Empty,
            recall.claims_expired = Empty,
            recall.head_moved = Empty,
            recall.block_bytes = Empty,
        );
        if let Some(skip) = self.tool_path_skip(&repo_root, &repo_key, agent_dir) {
            span.record("recall.skipped", true);
            span.record("recall.skip_reason", skip.as_str());
            span.record("recall.negative_cache", true);
            span.record("recall.block_bytes", 0);
            return None;
        }
        let outcome = tokio::time::timeout(
            TOOL_PATH_DEADLINE,
            self.witness_against_mark(&span, &context.session_id, &repo_root, agent_dir),
        )
        .await;
        if let Ok(block) = outcome {
            return block;
        }
        span.record("recall.skipped", true);
        span.record("recall.skip_reason", "deadline");
        span.record("recall.block_bytes", 0);
        self.remember_deadline_miss(&repo_key);
        None
    }

    async fn mark_repo(
        &self,
        span: &tracing::Span,
        session_id: &str,
        repo_root: &str,
        agent_dir: &Path,
        prior_ready: &watch::Sender<bool>,
    ) -> MarkOutcome {
        let repo_key = recall_repo_key(repo_root, agent_dir);
        let skipped = |reason: MarkSkipReason| MarkOutcome::Skipped {
            session_id: session_id.to_string(),
            reason: SkipReason::Mark(reason),
        };
        // Only a git timeout stops marks: they run off the tool path, so a
        // missed deadline there is no reason to skip.
        if self
            .git_timeout_until(repo_root, &repo_key, agent_dir)
            .is_some()
        {
            span.record("recall.skipped", true);
            span.record("recall.skip_reason", "git_timeout");
            span.record("recall.negative_cache", true);
            return skipped(MarkSkipReason::GitTimeout);
        }
        let record_prior = {
            let state = lock(&self.state);
            state.witness.get(session_id) != Some(&WitnessState::Done)
                && !state.prior_marks.contains_key(session_id)
        };
        if record_prior {
            // Recorded before this write starts, so a witness that reads the
            // file afterwards can tell which mark was prior.
            let prior = read_recall_mark(repo_root, agent_dir);
            let mut state = lock(&self.state);
            if state.witness.get(session_id) != Some(&WitnessState::Done) {
                state.prior_marks.insert(session_id.to_string(), prior);
            }
        }
        let _ = prior_ready.send(true);
        let claims = self.take_claims(session_id, repo_root);
        let inputs: Vec<RecallClaimInput> =
            claims.iter().map(|claim| claim.claim.clone()).collect();
        let now = self.now();
        let result = match &self.options.write_mark {
            Some(writer) => {
                writer(repo_root.to_string(), agent_dir.to_path_buf(), inputs, now).await
            }
            None => write_recall_mark(repo_root, agent_dir, &inputs, now).await,
        };
        match result {
            Ok(written) => {
                let unverifiable = written
                    .mark
                    .state
                    .dirty
                    .iter()
                    .filter(|(_, digest)| digest == RECALL_UNVERIFIABLE)
                    .count();
                let overflow = written.mark.state.dirty_overflow;
                span.record(
                    "recall.dirty_count",
                    written.mark.state.dirty.len() + overflow,
                );
                span.record("recall.claims", written.mark.claims.len());
                span.record("recall.unverifiable", unverifiable + overflow);
                MarkOutcome::Written {
                    session_id: session_id.to_string(),
                    repo_root: repo_root.to_string(),
                    repo_key: written.repo_key,
                    mark: Box::new(written.mark),
                }
            }
            Err(reason) => {
                self.add_claims(session_id, claims);
                span.record("recall.skipped", true);
                span.record("recall.skip_reason", mark_skip_label(&reason));
                if reason == MarkSkipReason::GitTimeout {
                    self.remember_git_timeout(repo_root, &repo_key, agent_dir);
                }
                if let MarkSkipReason::WriteFailed(error) = &reason {
                    tracing::warn!(parent: span, error, "recall mark write failed");
                }
                skipped(reason)
            }
        }
    }

    async fn run_mark(&self, context: &SessionFeatureContext, prior_ready: &watch::Sender<bool>) {
        let session_id = context.session_id.clone();
        let outcome = if !self.enabled() {
            MarkOutcome::Skipped {
                session_id,
                reason: SkipReason::Disabled,
            }
        } else if let Some(repo_root) =
            find_recall_repo(&context.cwd).and_then(|dir| resolve_repo_root(&dir))
        {
            // A detached root: the run that triggered it has ended.
            let span = tracing::info_span!(
                parent: None,
                "recall.mark",
                recall.repo_key = %recall_repo_key(&repo_root, &context.agent_dir),
                recall.skipped = Empty,
                recall.skip_reason = Empty,
                recall.negative_cache = Empty,
                recall.dirty_count = Empty,
                recall.claims = Empty,
                recall.unverifiable = Empty,
                recall.ms = Empty,
            );
            let started = Instant::now();
            let outcome = self
                .mark_repo(
                    &span,
                    &session_id,
                    &repo_root,
                    &context.agent_dir,
                    prior_ready,
                )
                .await;
            span.record(
                "recall.ms",
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            outcome
        } else {
            MarkOutcome::Skipped {
                session_id,
                reason: SkipReason::Mark(MarkSkipReason::NotRepo),
            }
        };
        let _ = prior_ready.send(true);
        self.settle(outcome);
    }

    fn worker_handle(&self) -> Option<tokio::runtime::Handle> {
        let mut runtime = lock(&self.worker.runtime);
        if runtime.is_none() {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("recall-mark")
                .enable_all()
                .build()
            {
                Ok(built) => *runtime = Some(built),
                Err(error) => {
                    tracing::debug!(%error, "recall mark worker could not start; marks skipped");
                    return None;
                }
            }
        }
        runtime.as_ref().map(|runtime| runtime.handle().clone())
    }

    fn schedule_mark(self: &Arc<Self>, context: &Arc<SessionFeatureContext>) {
        let (prior_ready, receiver) = watch::channel(false);
        {
            let mut state = lock(&self.state);
            if let Some(pending) = state.pending_marks.get_mut(&context.session_id) {
                pending.rerun = Some(Arc::clone(context));
                return;
            }
            state.pending_marks.insert(
                context.session_id.clone(),
                PendingMark {
                    prior_ready: receiver,
                    rerun: None,
                },
            );
        }
        let Some(handle) = self.worker_handle() else {
            lock(&self.state).pending_marks.remove(&context.session_id);
            return;
        };
        *lock(&self.worker.in_flight) += 1;
        let inner = Arc::clone(self);
        let context = Arc::clone(context);
        handle.spawn(async move {
            let mut next = Some(context);
            while let Some(run) = next {
                inner.run_mark(&run, &prior_ready).await;
                // Read and release under one lock, so a rerun requested now
                // is never dropped.
                let mut state = lock(&inner.state);
                next = state
                    .pending_marks
                    .get_mut(&run.session_id)
                    .and_then(|pending| pending.rerun.take());
                if next.is_none() {
                    state.pending_marks.remove(&run.session_id);
                }
            }
            let mut in_flight = lock(&inner.worker.in_flight);
            *in_flight -= 1;
            inner.worker.idle.notify_all();
        });
    }
}

impl SessionFeature for WorkspaceRecall {
    fn name(&self) -> &'static str {
        "recall"
    }

    fn before_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<()> {
        // Every tool call passes here: the checks before the source scan
        // stay allocation- and I/O-free.
        if call.tool_name != WITNESS_TOOL_NAME || context.rlm_depth > 0 {
            return Box::pin(async {});
        }
        let Some(code) = call.args.get("code").and_then(serde_json::Value::as_str) else {
            return Box::pin(async {});
        };
        if !mentions_build_command(code) || !self.inner.enabled() {
            return Box::pin(async {});
        }
        let inner = Arc::clone(&self.inner);
        let context = Arc::clone(context);
        let tool_call_id = call.tool_call_id.clone();
        Box::pin(async move {
            let Some(repo_root) =
                find_recall_repo(&context.cwd).and_then(|dir| resolve_repo_root(&dir))
            else {
                return;
            };
            let before = inner
                .cell_digest(DigestPhase::ToolCall, &repo_root, &context.agent_dir, None)
                .await;
            if let Some(before) = before.filter(|before| before.verifiable) {
                inner.track_cell(
                    tool_call_id,
                    TrackedCell {
                        session_id: context.session_id.clone(),
                        repo_root,
                        digest: before.digest,
                    },
                );
            }
        })
    }

    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        if result.tool_name != WITNESS_TOOL_NAME || !self.inner.enabled() {
            return Box::pin(async { None });
        }
        let inner = Arc::clone(&self.inner);
        let context = Arc::clone(context);
        let cell = inner.take_tracked_cell(&result.tool_call_id);
        let host_facts = result.host_facts.clone();
        let earlier_results = result.earlier_results_of_tool;
        Box::pin(async move {
            let claims_recorded = async {
                if let Some(cell) = cell {
                    inner
                        .record_cell_claims(&host_facts, cell, &context.agent_dir)
                        .await;
                }
            };
            let witness_due = {
                let mut state = lock(&inner.state);
                if state.witness.contains_key(&context.session_id) {
                    false
                } else {
                    state
                        .witness
                        .insert(context.session_id.clone(), WitnessState::Pending);
                    true
                }
            };
            if !witness_due {
                claims_recorded.await;
                return None;
            }
            let (block, ()) =
                tokio::join!(inner.witness(&context, earlier_results), claims_recorded);
            let mut state = lock(&inner.state);
            state
                .witness
                .insert(context.session_id.clone(), WitnessState::Done);
            state.prior_marks.remove(&context.session_id);
            block
        })
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        if !self.inner.enabled() {
            self.inner.settle(MarkOutcome::Skipped {
                session_id: context.session_id.clone(),
                reason: SkipReason::Disabled,
            });
            return;
        }
        if context.rlm_depth > 0 {
            self.inner.settle(MarkOutcome::Skipped {
                session_id: context.session_id.clone(),
                reason: SkipReason::ChildSession,
            });
            return;
        }
        self.inner.schedule_mark(context);
    }

    fn flush(&self, deadline: Instant) {
        let mut in_flight = lock(&self.inner.worker.in_flight);
        while *in_flight > 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            in_flight = self
                .inner
                .worker
                .idle
                .wait_timeout(in_flight, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kill_switch_accepts_the_ts_spellings() {
        assert!(is_workspace_recall_enabled(None));
        assert!(is_workspace_recall_enabled(Some("1")));
        for value in ["0", "off", "FALSE", " no "] {
            assert!(!is_workspace_recall_enabled(Some(value)), "{value}");
        }
    }
}
