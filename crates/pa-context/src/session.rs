//! One session's context manager: its state (persisted under the session
//! artifacts), the last request's raw messages and report (for `ctx.*` and
//! the nudge), and the background historian.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use pa_agent::types::AgentMessage;
use pa_core::features::SessionFeatureContext;
use pa_core::session_engine::side_model::{SideBill, SideModel, SideRequest};

use crate::historian;
use crate::state::{self, ContextState};
use crate::transform::{self, Inputs, Job, Report};

/// The provider's prompt cache is assumed cold after this long idle.
const CACHE_IDLE_MS: u64 = 5 * 60 * 1_000;
/// The historian's backoff after one failure...
const RETRY_MS: u64 = 10 * 60 * 1_000;
/// ...and after three in a row.
const LONG_RETRY_MS: u64 = 60 * 60 * 1_000;
/// `0`/`off`/`false` runs no historian.
pub(crate) const HISTORIAN_ENV: &str = "PRIME_AGENT_CONTEXT_HISTORIAN";

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(crate) fn switched_off(var: &str) -> bool {
    std::env::var(var).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    })
}

/// The last request: what was sent from, and what it saw.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) raw: Arc<Vec<AgentMessage>>,
    pub(crate) report: Report,
}

/// One session.
pub(crate) struct ContextSession {
    pub(crate) session_id: String,
    state_path: Option<PathBuf>,
    pub(crate) window: usize,
    pub(crate) state: Mutex<ContextState>,
    pub(crate) last: Mutex<Option<Snapshot>>,
    last_request_ms: AtomicU64,
    historian_running: AtomicBool,
    /// The historian's model and its selector; `None` runs no historian.
    model: Option<(SideModel, String)>,
}

impl ContextSession {
    /// The session of `context`, its state loaded from the session
    /// artifacts (in memory only without them).
    pub(crate) fn new(context: &SessionFeatureContext) -> Self {
        let state_path = context.session_artifact_dir.as_deref().map(state::path);
        let model = (!switched_off(HISTORIAN_ENV) && !context.daemonless).then(|| {
            let selector =
                pa_core::settings::SettingsManager::create(&context.cwd, &context.agent_dir)
                    .get_auxiliary_model()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}/{}", context.model.provider, context.model.id));
            (
                SideModel::Selector {
                    selector: selector.clone(),
                    agent_dir: context.agent_dir.clone(),
                    cwd: context.cwd.clone(),
                },
                selector,
            )
        });
        Self::with(
            context.session_id.clone(),
            state_path,
            usize::try_from(context.model.context_window).unwrap_or(usize::MAX),
            model,
        )
    }

    /// A session with everything injected (tests, embedders).
    pub(crate) fn with(
        session_id: String,
        state_path: Option<PathBuf>,
        window: usize,
        model: Option<(SideModel, String)>,
    ) -> Self {
        let state = state_path.as_deref().map(state::load).unwrap_or_default();
        Self {
            session_id,
            state_path,
            window,
            state: Mutex::new(state),
            last: Mutex::new(None),
            last_request_ms: AtomicU64::new(0),
            historian_running: AtomicBool::new(false),
            model,
        }
    }

    /// Persist `state` (best effort: a failed write keeps the in-memory
    /// state, which the next change retries).
    pub(crate) fn save(&self, state: &ContextState) {
        if let Some(path) = &self.state_path
            && let Err(error) = state::save(path, state)
        {
            tracing::warn!(target: "pa_context", %error, "context state not saved");
        }
    }

    /// Change the state under its lock and persist it.
    pub(crate) fn update<R>(&self, change: impl FnOnce(&mut ContextState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let result = change(&mut state);
        self.save(&state);
        result
    }

    /// One request's messages.
    pub(crate) fn transform(self: &Arc<Self>, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        let now = now_ms();
        let previous = self.last_request_ms.swap(now, Ordering::SeqCst);
        let cold = previous == 0 || now.saturating_sub(previous) >= CACHE_IDLE_MS;
        let raw = Arc::new(messages);
        let (out, report) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let before = state.clone();
            let result = transform::run(
                &raw,
                &mut state,
                &Inputs {
                    window: self.window,
                    cold,
                },
            );
            if *state != before {
                self.save(&state);
            }
            result
        };
        if let Some(job) = report.job.clone() {
            self.start_historian(job, now);
        }
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(Snapshot { raw, report });
        out
    }

    /// The last request's snapshot.
    pub(crate) fn snapshot(&self) -> Option<Snapshot> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a historian run is in flight.
    pub(crate) fn historian_running(&self) -> bool {
        self.historian_running.load(Ordering::SeqCst)
    }

    fn start_historian(self: &Arc<Self>, job: Job, now: u64) {
        let Some((model, selector)) = self.model.clone() else {
            return;
        };
        if now
            < self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .historian
                .retry_after_ms
        {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self
            .historian_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let session = Arc::clone(self);
        runtime.spawn(async move {
            let outcome = session.fold(&model, &job).await;
            session.publish(&job, outcome, &selector);
            session.historian_running.store(false, Ordering::SeqCst);
        });
    }

    async fn fold(&self, model: &SideModel, job: &Job) -> anyhow::Result<Vec<state::Compartment>> {
        let reply = model
            .complete(
                SideRequest::single(historian::HISTORIAN_SYSTEM_PROMPT, historian::prompt(job)),
                SideBill {
                    session_id: &self.session_id,
                    source: "context",
                },
            )
            .await?;
        historian::parse(&reply, job)
    }

    /// Record a run's outcome: its compartments when the history it
    /// continues is still the published one, else nothing (a compaction or
    /// another run moved it); a failure backs off.
    pub(crate) fn publish(
        &self,
        job: &Job,
        outcome: anyhow::Result<Vec<state::Compartment>>,
        selector: &str,
    ) {
        self.update(|state| match outcome {
            Ok(compartments) => {
                let current = state.compartments.iter().rev().find_map(|c| c.end_key.clone());
                if current != job.after {
                    tracing::debug!(target: "pa_context", "historian result is stale; dropped");
                    return;
                }
                tracing::info!(
                    target: "pa_context",
                    compartments = compartments.len(),
                    tokens = job.tokens,
                    model = selector,
                    "historian folded a range"
                );
                let published = now_ms();
                state
                    .compartments
                    .extend(compartments.into_iter().map(|mut compartment| {
                        compartment.created_ms = published;
                        compartment
                    }));
                state.historian.failures = 0;
                state.historian.retry_after_ms = 0;
                state.historian.runs += 1;
                state.historian.last_error = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                tracing::warn!(target: "pa_context", error = %message, model = selector, "historian failed");
                state.historian.failures += 1;
                state.historian.retry_after_ms = now_ms()
                    + if state.historian.failures >= 3 {
                        LONG_RETRY_MS
                    } else {
                        RETRY_MS
                    };
                state.historian.last_error = Some(message.chars().take(300).collect());
            }
        });
    }
}
