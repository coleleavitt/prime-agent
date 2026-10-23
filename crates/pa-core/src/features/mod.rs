//! The session-feature seam: optional, separately built capabilities plug
//! into every session through this registry, so native crates never name
//! them (see `docs/fork-feature-crates.md`).
//!
//! The composition root installs the enabled features once, at process
//! start; [`crate::session_engine::engine::create_session`] asks each
//! installed feature to register its kernel host-request handlers for the
//! session it is building and, when any feature is installed, routes the
//! session's tool-call and run-end lifecycle through them. With nothing
//! installed (the native product, and every native test) the seam is a
//! no-op: no hook is installed on the agent loop at all.
//!
//! Submodules: [`policy`] routes a session's tool calls, kernel host
//! requests and prompts through the features' reviews; [`confirm`] asks the
//! user when a review answers `Ask`, and [`question`] puts structured
//! questions on the same surface; [`status`] carries the statuses
//! features publish to the session-event surface; [`lifecycle`] composes
//! the features' answers at the agent's stop, after a tool batch, around a
//! compaction and at the session's end; [`notice`] delivers a feature's
//! notice to the model, waking an idle session.

mod confirm;
mod lifecycle;
mod notice;
mod policy;
mod question;
mod status;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub use confirm::{
    CONFIRMATION_FEATURE,
    CONFIRMATION_TIMEOUT,
    ConfirmationAnswer,
    ConfirmationRequest,
    ConfirmationSurface,
    answer_confirmation,
    answer_confirmation_remembering,
    answer_pending,
    confirm,
    confirm_offering,
    pending_confirmations,
    register_confirmation_surface,
};
pub(crate) use lifecycle::{
    SessionEndNotice,
    compaction_instructions,
    observe_compaction,
    review_agent_stop,
    review_tool_batch,
};
pub use notice::{
    FEATURE_NOTICE_CUSTOM_TYPE,
    FeatureNotice,
    FeatureNoticeSink,
    deliver_feature_notice,
    feature_notice_row,
    register_feature_notice_sink,
};
use pa_agent::types::{AgentMessage, ToolResultContent};
use pa_telemetry::{Properties, TelemetryClient, base_properties, lookup};
pub use policy::FeatureNotes;
pub(crate) use policy::{host_request_gate, review_prompt, tool_call_hooks};
pub use question::{
    QUESTION_KIND_ASK_USER,
    QUESTION_KIND_ENTER_PLAN_MODE,
    QUESTION_KIND_PLAN_APPROVAL,
    QuestionAnswer,
    QuestionRequest,
    ask_question,
    pending_questions,
};
pub use status::{
    FeatureStatus,
    FeatureStatusSink,
    publish_feature_status,
    register_feature_status_sink,
};

use crate::kernel::shared::HostRequestHandlers;
use crate::refinement::gate::RefinementGate;
use crate::refinement::prompt_hook::{HarnessPromptHook, HarnessPromptHooks};
use crate::session_engine::telemetry::TelemetryWiring;

/// A feature hook's boxed future: hooks run inline on the session's tool
/// path, so implementations bound their own work.
pub type FeatureFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What a feature may know about the session it is joining.
#[derive(Clone)]
pub struct SessionFeatureContext {
    /// The agent directory (`~/.prime/agent` or its override).
    pub agent_dir: PathBuf,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session id.
    pub session_id: String,
    /// Import names of the Python skills this session's kernel binds.
    pub python_skill_import_names: Vec<String>,
    /// The model the session was created with: the default a feature's own
    /// model selectors fall back to (like `model.info`, a creation-time
    /// fact).
    pub model: pa_agent::types::Model,
    /// The session's telemetry; `None` for sessions without telemetry
    /// (opted out, tests, one-shot paths).
    pub telemetry: Option<FeatureTelemetry>,
    /// The session's depth in the RLM recursion tree: 0 for a top-level
    /// session, N for a child spawned at depth N.
    pub rlm_depth: u32,
    /// The session's artifact directory (where its local harness state,
    /// kernel snapshot and other per-session files live); `None` for a
    /// session that persists nothing.
    pub session_artifact_dir: Option<PathBuf>,
}

/// A tool call the loop is about to execute (arguments validated).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}

/// A tool call's executed result, before the loop finalizes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub is_error: bool,
    /// The result's content blocks as the tool returned them, before any
    /// feature appended text.
    pub content: Vec<ToolResultContent>,
    /// The result's structured details (`AgentToolResult::details`); `Null`
    /// when it reported none.
    pub details: serde_json::Value,
    /// Host-side facts the tool reported (`AgentToolResult::host_facts`);
    /// `Null` when it reported none.
    pub host_facts: serde_json::Value,
    /// Results of the same tool already in the session's context, this call
    /// excluded (a resumed session counts its restored history).
    pub earlier_results_of_tool: usize,
}

/// A kernel host request about to run (`bash.run`, `computer_use.click`,
/// `mcp.session.call_tool`, ...): the kernel's actions reach the host this
/// way, so a `bash()` call in a cell is reviewed as the command it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct HostRequestObservation {
    pub request_type: String,
    /// The request's payload as the kernel sent it (`type` included).
    pub data: serde_json::Value,
}

/// How a kernel host request ended: its handler's answer (for the
/// kernel's `bash.*`, the job reply, whose own `status` may be `error`), or
/// the error the kernel raises.
pub type HostRequestOutcome<'a> = Result<&'a serde_json::Value, &'a str>;

/// A feature's policy answer for a tool call or host request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureVerdict {
    Allow,
    /// Ask the user, with the reason shown to them: the action runs only
    /// if they allow it (see [`confirm()`]). Without a client that can ask
    /// (print mode), or without an answer in [`CONFIRMATION_TIMEOUT`], it
    /// is refused.
    Ask(String),
    /// Refuse it: the model sees the reason as the call's error (a tool
    /// call's error result; the kernel's raised error for a host request).
    Block(String),
}

/// The most feature continuations one run takes
/// ([`SessionFeature::before_agent_stop`]); a feature that keeps asking past
/// it is ignored for the rest of the run, so a misbehaving stop policy
/// cannot loop the agent forever.
pub const MAX_FEATURE_CONTINUATIONS_PER_RUN: usize = 8;

/// The agent is about to stop ([`SessionFeature::before_agent_stop`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStopObservation {
    /// The text of the turn's final assistant message.
    pub last_assistant_message: String,
    /// Whether a feature continuation already ran in this run.
    pub continued: bool,
}

/// One call of a finished tool batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBatchCall {
    pub tool_call_id: String,
    pub tool_name: String,
}

/// A finished tool batch ([`SessionFeature::after_tool_batch`]), in call
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBatchObservation {
    pub tool_calls: Vec<ToolBatchCall>,
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// The user asked (`/compact`, the client's compact command).
    Manual,
    /// The session decided (a context threshold, an overflow, or a
    /// model-requested compaction).
    Auto,
}

impl CompactionTrigger {
    /// Claude Code's spelling (`manual` | `auto`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionTrigger::Manual => "manual",
            CompactionTrigger::Auto => "auto",
        }
    }
}

/// A compaction ([`SessionFeature::before_compaction`],
/// [`SessionFeature::after_compaction`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionObservation {
    pub trigger: CompactionTrigger,
    /// The user's own summary instructions (`/compact <instructions>`).
    pub custom_instructions: Option<String>,
}

/// A feature's answer for a submitted prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDecision {
    /// Admit it, with optional hidden context the turn carries ahead of the
    /// prompt.
    Continue { context: Option<String> },
    /// Refuse the prompt; the reason is the admission error.
    Block(String),
}

/// A feature's handle onto its session's telemetry client: catalogued
/// events only, stamped with the session's base properties, gated live on
/// the session's opt-out switch. Delivery is the client's background
/// worker's (fire-and-forget).
#[derive(Clone)]
pub struct FeatureTelemetry {
    track: Arc<TrackFn>,
}

/// A tracking function: event name and the feature's own properties.
type TrackFn = dyn Fn(&str, &Properties) + Send + Sync;

impl std::fmt::Debug for FeatureTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FeatureTelemetry")
    }
}

impl FeatureTelemetry {
    /// Wrap a raw tracking function (a feature test's recorder). Unlike
    /// [`Self::from_wiring`] it applies no catalog check or switch.
    pub fn new(track: impl Fn(&str, Properties) + Send + Sync + 'static) -> Self {
        Self {
            track: Arc::new(move |name, properties: &Properties| track(name, properties.clone())),
        }
    }

    /// The handle over one session's telemetry wiring (the engine builds
    /// it per session; a feature's tests build it over a mock-sink client).
    #[must_use]
    pub fn from_wiring(wiring: &TelemetryWiring) -> Self {
        let client: TelemetryClient = wiring.client.clone();
        let execution_mode = wiring.execution_mode.clone();
        let enabled = wiring
            .telemetry_enabled
            .as_ref()
            .map(|switch| Arc::clone(&switch.enabled));
        Self {
            track: Arc::new(move |name, properties| {
                if lookup(name).is_none() || enabled.as_ref().is_some_and(|enabled| !enabled()) {
                    return;
                }
                let mut tracked = base_properties(
                    execution_mode
                        .as_deref()
                        .unwrap_or(crate::session_engine::telemetry::EXECUTION_MODE_UNKNOWN),
                );
                tracked.merge(properties);
                client.track(name, tracked);
            }),
        }
    }

    /// Queue one event. Tracks nothing when the name is not in the
    /// `pa-telemetry` catalog or telemetry is switched off right now; the
    /// catalog's typed rules normalize the properties on delivery.
    pub fn track(&self, name: &str, properties: &Properties) {
        (self.track)(name, properties);
    }
}

/// What a feature slash command produced.
pub struct FeatureCommandOutcome {
    /// The command's durable result row.
    pub text: String,
    /// Background work whose settlement is reported as a later result row
    /// (`Ok` text as a success row, `Err` as `Command failed: <message>`).
    pub completion: Option<FeatureFuture<Result<String, String>>>,
}

impl std::fmt::Debug for FeatureCommandOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureCommandOutcome")
            .field("text", &self.text)
            .field("completion", &self.completion.is_some())
            .finish()
    }
}

/// One optional capability. Implementations live in their own crates and
/// are installed by the composition root; every method has a no-op default
/// so a feature implements only the seams it uses. Hooks receive the
/// session's context on every call: one feature instance serves every
/// session in the process, so per-session state is keyed by
/// [`SessionFeatureContext::session_id`].
pub trait SessionFeature: Send + Sync {
    /// Stable feature name (`trace`, `recall`, ...), used for diagnostics.
    fn name(&self) -> &'static str;

    /// Register this feature's kernel host-request handlers for one session.
    /// Handlers registered here override native ones of the same type, so
    /// a feature must only claim request types it owns.
    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let _ = (context, handlers);
    }

    /// Observe a tool call before it executes. The loop awaits the future
    /// before running the tool, so it must stay short and bounded; it
    /// cannot block or alter the call.
    fn before_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<()> {
        let _ = (context, call);
        Box::pin(async {})
    }

    /// Observe a tool call's result before the loop finalizes it; text
    /// returned here is appended to the result the model sees, as its own
    /// text block. Awaited on the tool path, so it must stay bounded.
    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        let _ = (context, result);
        Box::pin(async { None })
    }

    /// Decide whether a tool call may run, after plan mode allowed it. Every
    /// installed feature answers; the first `Block`, in installation order,
    /// refuses the call, and otherwise any `Ask` asks the user. Awaited on
    /// the tool path, so it must stay bounded.
    fn review_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let _ = (context, call);
        Box::pin(async { FeatureVerdict::Allow })
    }

    /// Decide whether a kernel host request may run (the kernel's `bash()`,
    /// computer use, MCP calls, ...). Every installed feature answers; the
    /// first `Block` refuses the request before its handler runs, and
    /// otherwise any `Ask` asks the user. The kernel's bookkeeping requests
    /// are not reviewed (see [`is_reviewed_host_request`]). Awaited on the
    /// kernel's request path, so it must stay bounded.
    fn review_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let _ = (context, request);
        Box::pin(async { FeatureVerdict::Allow })
    }

    /// Observe a kernel host request's outcome before the kernel sees it
    /// (a `bash.run` or `bash.follow` reply carries the command's finished
    /// event). Text returned here reaches the model: appended to the tool
    /// call whose cell sent the request, or, when no tool call is running,
    /// carried by the next admitted prompt. Called for every request except
    /// the kernel's bookkeeping ones (see [`is_observed_host_request`]);
    /// awaited before the reply, so it must stay bounded and return a ready
    /// future when it has nothing to do.
    fn after_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
        outcome: HostRequestOutcome<'_>,
    ) -> FeatureFuture<Option<String>> {
        let _ = (context, request, outcome);
        Box::pin(async { None })
    }

    /// Review a prompt the session is admitting as a new turn: add hidden
    /// context the turn carries ahead of it, or refuse it. Every installed
    /// feature answers; a `Block` refuses, and the contexts join in
    /// installation order. Awaited on the admission path, so it must stay
    /// bounded.
    fn prompt_context(
        &self,
        context: &Arc<SessionFeatureContext>,
        prompt: &str,
    ) -> FeatureFuture<PromptDecision> {
        let _ = (context, prompt);
        Box::pin(async { PromptDecision::Continue { context: None } })
    }

    /// The session was created: `history` is the conversation it resumed
    /// with (empty for a new session). Called once, before the session's
    /// first run, on the session-creation path, so it must not block.
    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, history: &[AgentMessage]) {
        let _ = (context, history);
    }

    /// A message was finalized into the session's context (a prompt, an
    /// assistant reply, a tool result, a steering message), in order.
    /// Called from the run's event stream: it must not block, so durable
    /// work is handed to the feature's own background worker.
    fn on_message_end(&self, context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        let _ = (context, message);
    }

    /// The agent finished responding: its run ended and it is not continuing
    /// on a queued steer (a steer ends the run at the next boundary and a
    /// new run starts on it at once). Claude Code's `Stop`. Called after
    /// [`Self::on_agent_end`], from the run's event stream: it must not
    /// block.
    fn on_agent_stop(&self, context: &Arc<SessionFeatureContext>) {
        let _ = context;
    }

    /// The model finished a turn without tool calls and the run is about to
    /// end (no queued steer or follow-up continues it): Claude Code's `Stop`
    /// decision point. Text returned here keeps the agent going: it reaches
    /// the model as a hidden row and the same run makes another model call.
    /// `stop.continued` is true once a feature continuation already ran in
    /// this run, so a feature can avoid looping; the session caps
    /// continuations per run at [`MAX_FEATURE_CONTINUATIONS_PER_RUN`].
    /// Awaited on the turn boundary, so it must stay bounded.
    fn before_agent_stop(
        &self,
        context: &Arc<SessionFeatureContext>,
        stop: &AgentStopObservation,
    ) -> FeatureFuture<Option<String>> {
        let _ = (context, stop);
        Box::pin(async { None })
    }

    /// A tool batch finished and the run is about to make its next model
    /// request. Text returned here rides that request as a hidden row (the
    /// contexts of every feature join in installation order). Awaited on the
    /// turn boundary, so it must stay bounded and return a ready future when
    /// it has nothing to do.
    fn after_tool_batch(
        &self,
        context: &Arc<SessionFeatureContext>,
        batch: &ToolBatchObservation,
    ) -> FeatureFuture<Option<String>> {
        let _ = (context, batch);
        Box::pin(async { None })
    }

    /// The session is about to compact. Text returned here is appended to
    /// the compaction's summary instructions (after the user's own `/compact`
    /// instructions). Awaited before the summarizer runs, so it must stay
    /// bounded.
    fn before_compaction(
        &self,
        context: &Arc<SessionFeatureContext>,
        compaction: &CompactionObservation,
    ) -> FeatureFuture<Option<String>> {
        let _ = (context, compaction);
        Box::pin(async { None })
    }

    /// A compaction committed with `summary`. Called on the compaction path:
    /// it must not block.
    fn after_compaction(
        &self,
        context: &Arc<SessionFeatureContext>,
        compaction: &CompactionObservation,
        summary: &str,
    ) {
        let _ = (context, compaction, summary);
    }

    /// The session ended: its engine was torn down (closed, released, or
    /// dropped), with `reason` (`other` unless the host knows better).
    /// Called at most once per session engine, possibly outside any async
    /// runtime: it must not block, and work that must finish before the
    /// process exits is awaited by [`Self::flush`].
    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, reason: &str) {
        let _ = (context, reason);
    }

    /// The session's agent run ended (every run, including one a queued
    /// steer ended; see [`Self::on_agent_stop`]). Called from the run's
    /// event stream: it must not block, so durable work is handed to the
    /// feature's own background worker.
    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        let _ = context;
    }

    /// The process is about to exit: finish or abandon background work by
    /// `deadline`. Called once, outside any async runtime.
    fn flush(&self, deadline: Instant) {
        let _ = deadline;
    }

    /// The gate this feature judges the session's refinements with (see
    /// [`crate::refinement::gate`]); `None`, the default, leaves them
    /// ungated. Called once, on the session-creation path; the first
    /// installed feature that returns a gate is the session's gate.
    fn refinement_gate(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn RefinementGate>> {
        let _ = context;
        None
    }

    /// How the session's automatic refine reviews ask and what an approval
    /// runs (see [`crate::refinement::executor::AutoRefinePolicy`]);
    /// `None`, the default, keeps the native policy. Called once, on the
    /// session-creation path; the first installed feature that offers one
    /// is the session's.
    fn auto_refine_policy(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn crate::refinement::executor::AutoRefinePolicy>> {
        let _ = context;
        None
    }

    /// Session slash commands this feature contributes (`/name args`). The
    /// composition root's [`install`] registers them in the shared
    /// `pa_types::slash_commands` registry, so every surface (TUI
    /// autocomplete and dispatch, daemon and print-mode admission) treats
    /// them as session commands; a name a builtin owns is dropped. Read once,
    /// at install.
    fn slash_commands(&self) -> Vec<pa_types::slash_commands::BuiltinSlashCommand> {
        Vec::new()
    }

    /// Execute one of this feature's slash commands in a session; `None`
    /// when `name` is not this feature's. The outcome's text is the
    /// command's durable result row; its `completion`, when given, is
    /// awaited in the background and its text (or error) appended as a
    /// second durable result row when it settles (a command that starts
    /// background work reports how the work ended). An `Err` is the
    /// command's failure (`Command failed: <message>`).
    fn execute_slash_command(
        &self,
        context: &Arc<SessionFeatureContext>,
        name: &str,
        args: &str,
    ) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
        let _ = (context, name, args);
        None
    }

    /// Built-in skills this feature contributes: directory names under the
    /// bundled skills directory's hidden feature directory
    /// (`skills/.features/<name>/`, see
    /// [`crate::packages::FEATURE_SKILLS_DIR`]). They load as built-in skills
    /// (Python ones are installed into the kernel) only while the feature is
    /// installed; native scans never see them. Empty by default.
    fn bundled_skills(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Whether the loaded skill `skill_name` is model-visible in this
    /// session: listed in its system prompt and harness digest, and bound
    /// in its kernel. A skill is visible only while every installed
    /// feature agrees, so a feature answers for the skills it owns and
    /// returns `true` for any other. The default, `true`, hides nothing.
    /// Called on the session-creation path, before the kernel's skills are
    /// decided: the context's `python_skill_import_names` is empty here.
    /// `/skill:<name>` expansion still sees every loaded skill.
    fn session_skill_visible(&self, context: &SessionFeatureContext, skill_name: &str) -> bool {
        let _ = (context, skill_name);
        true
    }

    /// The hook this feature adjusts the session's harness digest with (see
    /// [`crate::refinement::prompt_hook`]); `None`, the default, leaves the
    /// digest native. Called once, on the session-creation path; every
    /// installed feature's hook applies, in installation order.
    fn harness_prompt_hook(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn HarnessPromptHook>> {
        let _ = context;
        None
    }
}

static INSTALLED: OnceLock<Vec<Arc<dyn SessionFeature>>> = OnceLock::new();

/// Install the process's enabled features. The composition root calls this
/// once, before any session starts; later calls are ignored and return
/// `false`.
pub fn install(features: Vec<Arc<dyn SessionFeature>>) -> bool {
    let commands: Vec<_> = features
        .iter()
        .flat_map(|feature| feature.slash_commands())
        .collect();
    let installed = INSTALLED.set(features).is_ok();
    if installed && !commands.is_empty() {
        pa_types::slash_commands::register_feature_slash_commands(commands);
    }
    installed
}

/// The installed features, empty when none were installed.
#[must_use]
pub fn installed() -> &'static [Arc<dyn SessionFeature>] {
    INSTALLED.get().map_or(&[], Vec::as_slice)
}

/// The built-in skill directory names every installed feature contributes.
#[must_use]
pub fn installed_bundled_skills() -> Vec<String> {
    installed()
        .iter()
        .flat_map(|feature| feature.bundled_skills())
        .map(str::to_string)
        .collect()
}

/// The skills of `skills` every feature of `features` lets the session see
/// ([`SessionFeature::session_skill_visible`]), in order; all of them when
/// there are no features.
pub(crate) fn session_visible_skills(
    features: &[Arc<dyn SessionFeature>],
    context: &SessionFeatureContext,
    skills: &[crate::skills::Skill],
) -> Vec<crate::skills::Skill> {
    skills
        .iter()
        .filter(|skill| {
            features
                .iter()
                .all(|feature| feature.session_skill_visible(context, &skill.name))
        })
        .cloned()
        .collect()
}

/// Run the installed feature that owns slash command `name`; `None` when
/// none does.
pub(crate) fn execute_feature_slash_command(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    name: &str,
    args: &str,
) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
    features
        .iter()
        .find_map(|feature| feature.execute_slash_command(context, name, args))
}

/// Let every installed feature register its handlers for one session.
pub fn register_session_host_handlers(
    context: &SessionFeatureContext,
    handlers: &mut HostRequestHandlers,
) {
    for feature in installed() {
        feature.register_host_handlers(context, handlers);
    }
}

/// Give every installed feature until `timeout` from now to finish its
/// background work. The composition root calls this once before the
/// process exits; with nothing installed it returns at once.
pub fn flush_installed(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    for feature in installed() {
        feature.flush(deadline);
    }
}

/// Kernel host requests no feature reviews: the kernel's reads and
/// cleanup of its own `bash()` jobs (a refused kill would strand a job) and
/// its bookkeeping. `bash.consumed` must also stay synchronous (see
/// `kernel::manager::host_requests`).
const UNREVIEWED_HOST_REQUESTS: &[&str] = &[
    "bash.check",
    "bash.kill",
    "bash.killAll",
    "bash.isDestructiveGitDiscard",
    "bash.shell",
    "bash.childEnv",
    "bash.follow",
    "bash.output",
    "bash.confirmExit",
    "bash.groupAlive",
    "bash.inventory",
    "bash.activity",
    "bash.consumed",
    "model.info",
    "telemetry.emit",
    // The edit skill's file-checkpoint reports: the write they bracket stays
    // in the kernel, so refusing a report would refuse nothing.
    "file_history.before_write",
    "file_history.after_write",
];

/// Kernel host requests no feature observes after they ran: bookkeeping
/// whose replies carry nothing a policy reads (and `bash.consumed`, which
/// must stay synchronous).
const UNOBSERVED_HOST_REQUESTS: &[&str] = &[
    "bash.consumed",
    "model.info",
    "telemetry.emit",
    "file_history.before_write",
    "file_history.after_write",
];

/// Whether installed features review `request_type` before it runs.
#[must_use]
pub fn is_reviewed_host_request(request_type: &str) -> bool {
    !UNREVIEWED_HOST_REQUESTS.contains(&request_type)
}

/// Whether installed features observe `request_type`'s outcome.
#[must_use]
pub fn is_observed_host_request(request_type: &str) -> bool {
    !UNOBSERVED_HOST_REQUESTS.contains(&request_type)
}

/// The first refinement gate `features` offer the session; `None` when no
/// feature judges refinements.
pub(crate) fn session_refinement_gate(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> Option<Arc<dyn RefinementGate>> {
    features
        .iter()
        .find_map(|feature| feature.refinement_gate(context))
}

/// The first automatic-refine policy `features` offer the session; `None`
/// keeps the native one.
pub(crate) fn session_auto_refine_policy(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> Option<Arc<dyn crate::refinement::executor::AutoRefinePolicy>> {
    features
        .iter()
        .find_map(|feature| feature.auto_refine_policy(context))
}

/// The harness digest hooks `features` offer the session, in order; empty
/// when no feature adjusts the digest.
pub(crate) fn session_harness_prompt_hooks(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> HarnessPromptHooks {
    HarnessPromptHooks(
        features
            .iter()
            .filter_map(|feature| feature.harness_prompt_hook(context))
            .collect(),
    )
}

/// Tell `features` a session was created with `history`; nothing happens
/// when there are no features.
pub(crate) fn observe_session_start(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    history: &[AgentMessage],
) {
    for feature in features {
        feature.on_session_start(context, history);
    }
}

/// Report every finalized message and the end of every agent run of
/// `agent` to `features`, and the agent's stop when no queued steer
/// (`steering_probe`) continues it; nothing is subscribed when there are no
/// features.
pub(crate) async fn observe_agent_events(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    agent: &pa_agent::agent::Agent,
    steering_probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) {
    if features.is_empty() {
        return;
    }
    let features = features.to_vec();
    let context = Arc::clone(context);
    // The subscription lives as long as the agent: dropping the guard
    // keeps the listener.
    let _subscription = agent
        .subscribe(move |event, _signal| {
            match &event {
                pa_agent::types::AgentEvent::MessageEnd { message } => {
                    for feature in &features {
                        feature.on_message_end(&context, message);
                    }
                }
                pa_agent::types::AgentEvent::AgentEnd { .. } => {
                    for feature in &features {
                        feature.on_agent_end(&context);
                    }
                    if !steering_probe.as_ref().is_some_and(|probe| probe()) {
                        for feature in &features {
                            feature.on_agent_stop(&context);
                        }
                    }
                }
                _ => {}
            }
            Box::pin(async { Ok(()) })
        })
        .await;
}

#[cfg(test)]
mod tests;
