//! The RAVO controller loop (TS `ravo/controller.ts`): inspect, plan,
//! implement (or repair), evaluate with every evaluator, step the reducer,
//! and on a committed certificate pass the external commit gate and the
//! archive's champion compare-and-set; otherwise diagnose and repair until
//! a round, repair, deadline or token limit, cancellation, or a stale
//! commit stops it. Every phase is checkpointed.
//!
//! Evaluators run one at a time (TS: up to `concurrency` at once); the
//! token admission floor is the same per call.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::archive::{ArchiveError, RavoArchive};
use super::context::BoundedContextView;
use crate::js::{canonical_json, sha256_hex};
use crate::reducer::{
    ravo_step, GateStatus, RavoConfig, RavoCriterionObservation, RavoEvaluation,
    RavoGateCertificate, RavoObservation, RavoProposal, RavoState,
};

/// A boxed child future.
pub type ChildFuture<'a, T> = Pin<Box<dyn Future<Output = ChildResult<T>> + Send + 'a>>;

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RavoPhase {
    Idle,
    Inspect,
    Plan,
    Implement,
    Evaluate,
    Diagnose,
    Repair,
    CommitGate,
    Accepted,
    Stopped,
}

/// Why a run stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RavoStopReason {
    Accepted,
    RoundLimit,
    RepairLimit,
    Deadline,
    Budget,
    Cancelled,
    StaleCas,
}

/// A candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerProposal {
    pub id: String,
    pub parent_id: Option<String>,
    pub repair_of: Option<String>,
    pub artifact: Value,
}

/// What the inspector found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionFindings {
    pub summary: String,
    pub facts: Vec<String>,
}

/// The plan a round executes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoPlan {
    pub id: String,
    pub steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_advice: Option<String>,
}

/// One finding of a rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub source: String,
    pub status: GateStatus,
    pub detail: String,
}

/// Why a candidate was rejected, for its repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticFeedback {
    pub proposal_id: String,
    pub rejection: String,
    pub findings: Vec<Finding>,
}

/// How a child call ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildResult<T> {
    Completed {
        value: T,
        tokens: u64,
    },
    /// `status`: `error`, `aborted`, `budget_exceeded`, ...
    Failed {
        status: String,
        tokens: u64,
        error: Option<String>,
    },
}

/// What a child call may spend.
#[derive(Debug, Clone)]
pub struct ChildCallOptions {
    pub cancel: CancellationToken,
    pub token_budget: u64,
}

/// An evaluator's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluatorKind {
    Fast,
    Deep,
    Opponent,
}

impl EvaluatorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Deep => "deep",
            Self::Opponent => "opponent",
        }
    }
}

/// One evaluator the run consults per candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluatorSpec {
    pub id: String,
    pub kind: EvaluatorKind,
    pub criterion_id: Option<String>,
}

/// An evaluator's observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub status: GateStatus,
    pub score: Option<u64>,
    pub detail: Option<String>,
}

/// What a supervisor is shown.
#[derive(Debug, Clone)]
pub struct SupervisorSignal {
    pub plan: RavoPlan,
    pub trajectory: Vec<RavoGateCertificate>,
}

/// The supervisor's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorAdvice {
    pub intervene: bool,
    pub advice: Option<String>,
}

/// The commit gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    pub accepted: bool,
    pub detail: Option<String>,
    /// The state the certificate was earned against moved outside the run.
    pub stale: bool,
}

/// A progress event.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressEvent {
    Phase {
        phase: RavoPhase,
        round: u64,
    },
    Proposal {
        proposal_id: String,
        parent_id: Option<String>,
        repair_of: Option<String>,
    },
    Evaluation {
        proposal_id: String,
        certificate: Box<RavoGateCertificate>,
    },
    Supervisor {
        intervened: bool,
        detail: Option<String>,
    },
    Stopped {
        reason: RavoStopReason,
    },
}

/// The run's durable progress.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerCheckpoint {
    pub run_id: String,
    pub phase: RavoPhase,
    pub round: u64,
    pub repairs: u64,
    pub state: RavoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection: Option<InspectionFindings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<RavoPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_evaluation: Option<RavoEvaluation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<ControllerProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<DiagnosticFeedback>,
    pub certificates: Vec<RavoGateCertificate>,
    pub spent_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_baseline: Option<Value>,
    pub error_budget: Value,
}

/// How the run ended.
#[derive(Debug, Clone)]
pub struct ControllerResult {
    pub reason: RavoStopReason,
    pub checkpoint: ControllerCheckpoint,
    pub gate_certificate_digest: Option<String>,
}

/// The run's children, evaluators, gate and observers.
pub trait ControllerHooks: Send + Sync {
    fn inspect<'a>(
        &'a self,
        context: &'a BoundedContextView,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, InspectionFindings>;

    fn plan<'a>(
        &'a self,
        context: &'a BoundedContextView,
        inspection: &'a InspectionFindings,
        feedback: Option<&'a DiagnosticFeedback>,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, RavoPlan>;

    fn implement<'a>(
        &'a self,
        context: &'a BoundedContextView,
        inspection: &'a InspectionFindings,
        plan: &'a RavoPlan,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, ControllerProposal>;

    fn repair<'a>(
        &'a self,
        context: &'a BoundedContextView,
        candidate: &'a ControllerProposal,
        feedback: &'a DiagnosticFeedback,
        plan: &'a RavoPlan,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, ControllerProposal>;

    /// Exactly one fast and one deep evaluator, then the opponents.
    fn evaluators(&self) -> Vec<EvaluatorSpec>;

    fn evaluate<'a>(
        &'a self,
        evaluator: &'a EvaluatorSpec,
        proposal: &'a ControllerProposal,
        context: &'a BoundedContextView,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, Observation>;

    /// Whether this round consults the supervisor.
    fn should_consult_supervisor(&self, signal: &SupervisorSignal) -> bool;

    fn supervisor<'a>(
        &'a self,
        signal: &'a SupervisorSignal,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, SupervisorAdvice>;

    fn commit_gate<'a>(
        &'a self,
        proposal: &'a ControllerProposal,
        certificate: &'a RavoGateCertificate,
    ) -> Pin<Box<dyn Future<Output = GateOutcome> + Send + 'a>>;

    /// Observers never fail the run.
    fn on_progress(&self, event: &ProgressEvent);

    fn on_checkpoint(&self, checkpoint: &ControllerCheckpoint);
}

/// The run's bounds and state.
pub struct ControllerOptions<'a> {
    pub run_id: String,
    pub context: BoundedContextView,
    pub initial_state: RavoState,
    pub reducer_config: RavoConfig,
    pub archive: &'a RavoArchive,
    /// The error-budget ledger snapshot (no probabilistic evaluator runs
    /// in the product, so it never moves).
    pub error_budget: Value,
    pub max_rounds: u64,
    pub max_repairs: u64,
    pub deadline: std::time::Duration,
    pub token_budget: u64,
    pub reservation_per_call: u64,
    pub cancel: CancellationToken,
}

/// A terminal outcome that is not an error.
enum Stop {
    Reason(RavoStopReason),
    Error(String),
}

/// A child's failure, inside the round.
enum CallError {
    Stop(RavoStopReason),
    Child(String),
}

/// The error-budget snapshot of a ledger with family delta 1/20 that
/// allocated nothing (TS `new ErrorBudgetLedger(Rational.of(1, 20)).toJSON()`).
#[must_use]
pub fn empty_error_budget() -> Value {
    json!({
        "schemaVersion": 1,
        "familyDelta": "1/20",
        "allocatedDelta": "0/1",
        "spentDelta": "0/1",
        "calibrations": [],
        "allocations": [],
        "evaluations": [],
    })
}

fn archive_error(error: &ArchiveError) -> Stop {
    Stop::Error(error.to_string())
}

struct Run<'a, 'h> {
    options: &'a ControllerOptions<'a>,
    hooks: &'h dyn ControllerHooks,
    cp: ControllerCheckpoint,
    started: Instant,
}

impl Run<'_, '_> {
    fn checkpoint_value(&self) -> Value {
        serde_json::to_value(&self.cp).unwrap_or(Value::Null)
    }

    fn persist(&mut self) -> Result<(), Stop> {
        self.cp.error_budget = self.options.error_budget.clone();
        let mut payload = Map::new();
        payload.insert("runId".into(), json!(self.options.run_id));
        payload.insert("checkpoint".into(), self.checkpoint_value());
        self.options
            .archive
            .append("run", payload)
            .map_err(|error| archive_error(&error))?;
        self.hooks.on_checkpoint(&self.cp);
        Ok(())
    }

    fn set_phase(&mut self, phase: RavoPhase) -> Result<(), Stop> {
        self.cp.phase = phase;
        tracing::Span::current().record("ravo.phase", phase_name(phase));
        self.hooks.on_progress(&ProgressEvent::Phase {
            phase,
            round: self.cp.round,
        });
        self.persist()
    }

    /// Admit a child: not cancelled, before the deadline, with at least the
    /// admission floor of the budget unspent.
    fn admit(&self) -> Result<ChildCallOptions, CallError> {
        if self.options.cancel.is_cancelled() {
            return Err(CallError::Stop(RavoStopReason::Cancelled));
        }
        if self.started.elapsed() >= self.options.deadline {
            return Err(CallError::Stop(RavoStopReason::Deadline));
        }
        let remaining = self
            .options
            .token_budget
            .saturating_sub(self.cp.spent_tokens);
        if remaining < self.options.reservation_per_call {
            return Err(CallError::Stop(RavoStopReason::Budget));
        }
        Ok(ChildCallOptions {
            cancel: self.options.cancel.clone(),
            token_budget: remaining,
        })
    }

    /// Settle a child's result: spend its tokens; a failure is a stop when
    /// it was cancellation or budget, else a child failure.
    fn settle<T>(&mut self, result: ChildResult<T>) -> Result<T, CallError> {
        match result {
            ChildResult::Completed { value, tokens } => {
                self.cp.spent_tokens = self.cp.spent_tokens.saturating_add(tokens);
                Ok(value)
            }
            ChildResult::Failed {
                status,
                tokens,
                error,
            } => {
                self.cp.spent_tokens = self.cp.spent_tokens.saturating_add(tokens);
                if self.options.cancel.is_cancelled() {
                    return Err(CallError::Stop(RavoStopReason::Cancelled));
                }
                if status == "budget_exceeded" {
                    return Err(CallError::Stop(RavoStopReason::Budget));
                }
                Err(CallError::Child(error.unwrap_or(status)))
            }
        }
    }

    fn stop(&mut self, reason: RavoStopReason) -> ControllerResult {
        self.cp.phase = if reason == RavoStopReason::Accepted {
            RavoPhase::Accepted
        } else {
            RavoPhase::Stopped
        };
        let span = tracing::Span::current();
        span.record("ravo.reason", stop_name(reason));
        span.record("ravo.rounds", self.cp.round);
        span.record("ravo.repairs", self.cp.repairs);
        span.record("ravo.spent_tokens", self.cp.spent_tokens);
        let mut payload = Map::new();
        payload.insert("runId".into(), json!(self.options.run_id));
        payload.insert("reason".into(), json!(stop_name(reason)));
        payload.insert("round".into(), json!(self.cp.round));
        if let Err(error) = self.options.archive.append("stop", payload) {
            tracing::debug!(%error, "the RAVO archive refused the stop record");
        }
        self.hooks.on_progress(&ProgressEvent::Stopped { reason });
        ControllerResult {
            reason,
            checkpoint: self.cp.clone(),
            gate_certificate_digest: None,
        }
    }

    fn call_error(error: CallError) -> Stop {
        match error {
            CallError::Stop(reason) => Stop::Reason(reason),
            CallError::Child(message) => Stop::Error(message),
        }
    }

    // One round end to end: its phases share the checkpoint they advance.
    #[allow(clippy::too_many_lines)]
    async fn round(&mut self) -> Result<Option<ControllerResult>, Stop> {
        let hooks = self.hooks;
        let context = &self.options.context;
        self.set_phase(RavoPhase::Inspect)?;
        let inspection = if let Some(inspection) = self.cp.inspection.clone() {
            inspection
        } else {
            let options = self.admit().map_err(Self::call_error)?;
            let result = hooks.inspect(context, options).await;
            let inspection = self.settle(result).map_err(Self::call_error)?;
            self.cp.inspection = Some(inspection.clone());
            inspection
        };
        self.set_phase(RavoPhase::Plan)?;
        let options = self.admit().map_err(Self::call_error)?;
        let feedback = self.cp.feedback.clone();
        let result = hooks
            .plan(context, &inspection, feedback.as_ref(), options)
            .await;
        let mut plan = self.settle(result).map_err(Self::call_error)?;
        let signal = SupervisorSignal {
            plan: plan.clone(),
            trajectory: self.cp.certificates.clone(),
        };
        if hooks.should_consult_supervisor(&signal) {
            let advice = match self.admit() {
                Ok(options) => {
                    let result = hooks.supervisor(&signal, options).await;
                    self.settle(result)
                }
                Err(error) => Err(error),
            };
            match advice {
                Ok(advice) => {
                    if let (true, Some(text)) = (advice.intervene, advice.advice.clone()) {
                        plan.supervisor_advice = Some(text);
                    }
                    hooks.on_progress(&ProgressEvent::Supervisor {
                        intervened: advice.intervene,
                        detail: advice.advice,
                    });
                }
                Err(CallError::Stop(reason)) => return Err(Stop::Reason(reason)),
                Err(CallError::Child(_)) => hooks.on_progress(&ProgressEvent::Supervisor {
                    intervened: false,
                    detail: Some("supervisor unavailable".to_string()),
                }),
            }
        } else {
            hooks.on_progress(&ProgressEvent::Supervisor {
                intervened: false,
                detail: None,
            });
        }
        self.cp.plan = Some(plan.clone());
        let repairing = self.cp.feedback.is_some() && self.cp.candidate.is_some();
        self.set_phase(if self.cp.feedback.is_some() {
            RavoPhase::Repair
        } else {
            RavoPhase::Implement
        })?;
        let candidate = self.propose(&inspection, &plan, repairing).await?;
        self.cp.candidate = Some(candidate.clone());
        self.cp.feedback = None;
        hooks.on_progress(&ProgressEvent::Proposal {
            proposal_id: candidate.id.clone(),
            parent_id: candidate.parent_id.clone(),
            repair_of: candidate.repair_of.clone(),
        });
        let mut payload = Map::new();
        payload.insert("runId".into(), json!(self.options.run_id));
        payload.insert("proposalId".into(), json!(candidate.id));
        payload.insert("parentId".into(), json!(candidate.parent_id));
        payload.insert("repairOf".into(), json!(candidate.repair_of));
        self.options
            .archive
            .append("proposal", payload)
            .map_err(|error| archive_error(&error))?;
        let baseline = self
            .options
            .archive
            .recover()
            .map_err(|error| archive_error(&error))?
            .cas();
        self.cp.archive_baseline = Some(json!({
            "revision": baseline.revision,
            "championDigest": baseline.champion_digest,
        }));
        self.set_phase(RavoPhase::Evaluate)?;
        let mut observations: Vec<(EvaluatorSpec, Observation)> = Vec::new();
        for evaluator in hooks.evaluators() {
            let observation = self.evaluate_one(&evaluator, &candidate).await?;
            observations.push((evaluator, observation));
        }
        let evaluation = assemble_evaluation(&candidate.id, &observations);
        self.cp.last_evaluation = Some(evaluation.clone());
        let stepped = ravo_step(
            &self.cp.state,
            &RavoProposal {
                id: candidate.id.clone(),
                artifact: candidate.artifact.clone(),
            },
            &evaluation,
            &self.options.reducer_config,
        );
        self.cp.certificates.push(stepped.certificate.clone());
        hooks.on_progress(&ProgressEvent::Evaluation {
            proposal_id: candidate.id.clone(),
            certificate: Box::new(stepped.certificate.clone()),
        });
        let mut payload = Map::new();
        payload.insert("runId".into(), json!(self.options.run_id));
        payload.insert("proposalId".into(), json!(candidate.id));
        payload.insert(
            "certificate".into(),
            serde_json::to_value(&stepped.certificate).unwrap_or(Value::Null),
        );
        self.options
            .archive
            .append("evaluation", payload)
            .map_err(|error| archive_error(&error))?;
        let span = tracing::Span::current();
        if stepped.certificate.committed {
            self.set_phase(RavoPhase::CommitGate)?;
            let gate = hooks.commit_gate(&candidate, &stepped.certificate).await;
            if !gate.accepted && gate.stale {
                span.record("ravo.outcome", "stopped");
                return Ok(Some(self.stop(RavoStopReason::StaleCas)));
            }
            if gate.accepted {
                let digest = sha256_hex(&canonical_json(&json!({
                    "proposal": candidate,
                    "certificate": stepped.certificate,
                    "errorBudget": self.options.error_budget,
                })));
                let mut payload = Map::new();
                payload.insert("runId".into(), json!(self.options.run_id));
                payload.insert("proposalId".into(), json!(candidate.id));
                payload.insert("certificateDigest".into(), json!(digest));
                match self.options.archive.accept(payload, &baseline, &digest) {
                    Ok(_) => {}
                    Err(ArchiveError::StaleCommit) => {
                        span.record("ravo.outcome", "stopped");
                        return Ok(Some(self.stop(RavoStopReason::StaleCas)));
                    }
                    Err(error) => return Err(archive_error(&error)),
                }
                self.cp.state = stepped.state;
                self.cp.error_budget = self.options.error_budget.clone();
                self.cp.phase = RavoPhase::Accepted;
                span.record("ravo.outcome", "accepted");
                hooks.on_progress(&ProgressEvent::Stopped {
                    reason: RavoStopReason::Accepted,
                });
                return Ok(Some(ControllerResult {
                    reason: RavoStopReason::Accepted,
                    checkpoint: self.cp.clone(),
                    gate_certificate_digest: Some(digest),
                }));
            }
            self.cp.feedback = Some(diagnostic(
                &stepped.certificate,
                Some(
                    gate.detail
                        .unwrap_or_else(|| "external commit gate rejected".to_string()),
                ),
            ));
        } else {
            self.cp.feedback = Some(diagnostic(&stepped.certificate, None));
        }
        span.record("ravo.outcome", "rejected");
        self.cp.state.evaluated_proposal_ids = stepped.state.evaluated_proposal_ids;
        let mut payload = Map::new();
        payload.insert("runId".into(), json!(self.options.run_id));
        payload.insert("proposalId".into(), json!(candidate.id));
        payload.insert(
            "feedback".into(),
            serde_json::to_value(&self.cp.feedback).unwrap_or(Value::Null),
        );
        self.options
            .archive
            .append("reject", payload)
            .map_err(|error| archive_error(&error))?;
        self.set_phase(RavoPhase::Diagnose)?;
        self.cp.repairs += 1;
        if self.cp.repairs > self.options.max_repairs {
            return Ok(Some(self.stop(RavoStopReason::RepairLimit)));
        }
        Ok(None)
    }

    async fn propose(
        &mut self,
        inspection: &InspectionFindings,
        plan: &RavoPlan,
        repairing: bool,
    ) -> Result<ControllerProposal, Stop> {
        let hooks = self.hooks;
        let context = &self.options.context;
        let span = tracing::info_span!(
            "ravo.proposal",
            ravo.round = self.cp.round,
            ravo.kind = if repairing { "repair" } else { "implement" },
            ravo.proposal_id = tracing::field::Empty,
            ravo.candidate_tokens = tracing::field::Empty,
        );
        let spent_before = self.cp.spent_tokens;
        let options = self.admit().map_err(Self::call_error)?;
        let prior = self.cp.candidate.clone();
        let result = match (repairing, prior.as_ref(), self.cp.feedback.clone()) {
            (true, Some(candidate), Some(feedback)) => {
                hooks
                    .repair(context, candidate, &feedback, plan, options)
                    .instrument(span.clone())
                    .await
            }
            _ => {
                hooks
                    .implement(context, inspection, plan, options)
                    .instrument(span.clone())
                    .await
            }
        };
        let proposal = self.settle(result).map_err(Self::call_error)?;
        span.record("ravo.proposal_id", proposal.id.as_str());
        span.record("ravo.candidate_tokens", self.cp.spent_tokens - spent_before);
        validate_proposal(&proposal, prior.as_ref(), repairing).map_err(Stop::Error)?;
        Ok(proposal)
    }

    async fn evaluate_one(
        &mut self,
        evaluator: &EvaluatorSpec,
        candidate: &ControllerProposal,
    ) -> Result<Observation, Stop> {
        let span = tracing::info_span!(
            "ravo.evaluation",
            ravo.proposal_id = candidate.id.as_str(),
            ravo.evaluator = evaluator.id.as_str(),
            ravo.evaluator_kind = evaluator.kind.as_str(),
            ravo.verdict = tracing::field::Empty,
        );
        let options = match self.admit() {
            Ok(options) => options,
            Err(CallError::Stop(reason)) => return Err(Stop::Reason(reason)),
            Err(CallError::Child(message)) => return Err(Stop::Error(message)),
        };
        let result = self
            .hooks
            .evaluate(evaluator, candidate, &self.options.context, options)
            .instrument(span.clone())
            .await;
        let observation = match self.settle(result) {
            Ok(observation) => observation,
            Err(CallError::Stop(reason)) => return Err(Stop::Reason(reason)),
            Err(CallError::Child(message)) => Observation {
                status: GateStatus::Error,
                score: None,
                detail: Some(message),
            },
        };
        span.record("ravo.verdict", gate_name(observation.status));
        Ok(observation)
    }
}

fn gate_name(status: GateStatus) -> &'static str {
    match status {
        GateStatus::Pass => "pass",
        GateStatus::Fail => "fail",
        GateStatus::Abstain => "abstain",
        GateStatus::Error => "error",
    }
}

fn phase_name(phase: RavoPhase) -> &'static str {
    match phase {
        RavoPhase::Idle => "idle",
        RavoPhase::Inspect => "inspect",
        RavoPhase::Plan => "plan",
        RavoPhase::Implement => "implement",
        RavoPhase::Evaluate => "evaluate",
        RavoPhase::Diagnose => "diagnose",
        RavoPhase::Repair => "repair",
        RavoPhase::CommitGate => "commit_gate",
        RavoPhase::Accepted => "accepted",
        RavoPhase::Stopped => "stopped",
    }
}

/// The stop reason's wire name.
#[must_use]
pub fn stop_name(reason: RavoStopReason) -> &'static str {
    match reason {
        RavoStopReason::Accepted => "accepted",
        RavoStopReason::RoundLimit => "round_limit",
        RavoStopReason::RepairLimit => "repair_limit",
        RavoStopReason::Deadline => "deadline",
        RavoStopReason::Budget => "budget",
        RavoStopReason::Cancelled => "cancelled",
        RavoStopReason::StaleCas => "stale_cas",
    }
}

fn validate_proposal(
    next: &ControllerProposal,
    prior: Option<&ControllerProposal>,
    repair: bool,
) -> Result<(), String> {
    if next.id.is_empty() {
        return Err("proposal id is required".to_string());
    }
    if !repair && (next.parent_id.is_some() || next.repair_of.is_some()) {
        return Err("initial proposal links must be null".to_string());
    }
    if repair {
        let linked = prior.is_some_and(|prior| {
            next.id != prior.id
                && next.parent_id.as_deref() == Some(prior.id.as_str())
                && next.repair_of.as_deref() == Some(prior.id.as_str())
        });
        if !linked {
            return Err("repair must have a new id and link to its parent".to_string());
        }
    }
    Ok(())
}

fn assemble_evaluation(
    proposal_id: &str,
    observations: &[(EvaluatorSpec, Observation)],
) -> RavoEvaluation {
    let observed = |kind: EvaluatorKind| {
        observations
            .iter()
            .find(|(spec, _)| spec.kind == kind)
            .map_or(
                RavoObservation {
                    status: GateStatus::Error,
                    score: None,
                    detail: None,
                },
                |(_, observation)| RavoObservation {
                    status: observation.status,
                    score: observation.score,
                    detail: observation.detail.clone(),
                },
            )
    };
    RavoEvaluation {
        proposal_id: proposal_id.to_string(),
        screen: observed(EvaluatorKind::Fast),
        deep: observed(EvaluatorKind::Deep),
        criteria: observations
            .iter()
            .filter(|(spec, _)| spec.kind == EvaluatorKind::Opponent)
            .map(|(spec, observation)| RavoCriterionObservation {
                criterion_id: spec.criterion_id.clone().unwrap_or_else(|| spec.id.clone()),
                status: observation.status,
                detail: observation
                    .detail
                    .clone()
                    .filter(|detail| !detail.is_empty()),
            })
            .collect(),
    }
}

/// The findings a rejected certificate hands its repair.
#[must_use]
pub fn diagnostic(certificate: &RavoGateCertificate, extra: Option<String>) -> DiagnosticFeedback {
    let mut findings = Vec::new();
    if certificate.screen.status != GateStatus::Pass {
        findings.push(Finding {
            source: "fast".to_string(),
            status: certificate.screen.status,
            detail: certificate
                .screen
                .detail
                .clone()
                .unwrap_or_else(|| "fast screen rejected".to_string()),
        });
    }
    if certificate.deep.status != GateStatus::Pass {
        findings.push(Finding {
            source: "deep".to_string(),
            status: certificate.deep.status,
            detail: certificate
                .deep
                .detail
                .clone()
                .unwrap_or_else(|| "deep evaluator rejected".to_string()),
        });
    }
    for criterion in certificate
        .criteria
        .iter()
        .filter(|item| item.counted_as_missed)
    {
        findings.push(Finding {
            source: format!("opponent:{}", criterion.criterion_id),
            status: criterion.status,
            detail: criterion
                .detail
                .clone()
                .unwrap_or_else(|| "criterion missed".to_string()),
        });
    }
    if let Some(extra) = extra {
        findings.push(Finding {
            source: "commit_gate".to_string(),
            status: GateStatus::Fail,
            detail: extra,
        });
    }
    DiagnosticFeedback {
        proposal_id: certificate.proposal_id.clone(),
        rejection: certificate.rejection.map_or_else(
            || "commit_gate".to_string(),
            |rejection| {
                serde_json::to_value(rejection)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default()
            },
        ),
        findings,
    }
}

/// Run the controller to a stop.
///
/// # Errors
///
/// An archive failure, a child that failed for a reason other than
/// cancellation or budget, or an invalid proposal: the run ends with that
/// error instead of a stop reason.
pub async fn run_ravo_controller(
    options: &ControllerOptions<'_>,
    hooks: &dyn ControllerHooks,
) -> Result<ControllerResult, String> {
    let span = tracing::info_span!(
        "ravo.run",
        ravo.run_id = options.run_id.as_str(),
        ravo.resumed = false,
        ravo.reason = tracing::field::Empty,
        ravo.rounds = tracing::field::Empty,
        ravo.repairs = tracing::field::Empty,
        ravo.spent_tokens = tracing::field::Empty,
        ravo.certificate_digest = tracing::field::Empty,
    );
    run_controller(options, hooks).instrument(span).await
}

async fn run_controller(
    options: &ControllerOptions<'_>,
    hooks: &dyn ControllerHooks,
) -> Result<ControllerResult, String> {
    let evaluators = hooks.evaluators();
    let count = |kind: EvaluatorKind| evaluators.iter().filter(|spec| spec.kind == kind).count();
    if count(EvaluatorKind::Fast) != 1 || count(EvaluatorKind::Deep) != 1 {
        return Err("exactly one fast and one deep evaluator are required".to_string());
    }
    if options.max_rounds == 0 || options.token_budget == 0 || options.reservation_per_call == 0 {
        return Err("RAVO run limits are invalid".to_string());
    }
    let mut run = Run {
        options,
        hooks,
        cp: ControllerCheckpoint {
            run_id: options.run_id.clone(),
            phase: RavoPhase::Inspect,
            round: 0,
            repairs: 0,
            state: options.initial_state.clone(),
            inspection: None,
            plan: None,
            last_evaluation: None,
            candidate: None,
            feedback: None,
            certificates: Vec::new(),
            spent_tokens: 0,
            archive_baseline: None,
            error_budget: options.error_budget.clone(),
        },
        started: Instant::now(),
    };
    options
        .archive
        .initialize()
        .map_err(|error| error.to_string())?;
    let mut payload = Map::new();
    payload.insert("runId".into(), json!(options.run_id));
    payload.insert("contextDigest".into(), json!(options.context.sha256));
    options
        .archive
        .append("run", payload)
        .map_err(|error| error.to_string())?;
    while run.cp.round < options.max_rounds {
        if options.cancel.is_cancelled() {
            return Ok(run.stop(RavoStopReason::Cancelled));
        }
        if run.started.elapsed() >= options.deadline {
            return Ok(run.stop(RavoStopReason::Deadline));
        }
        run.cp.round += 1;
        let span = tracing::info_span!(
            "ravo.round",
            ravo.round = run.cp.round,
            ravo.phase = tracing::field::Empty,
            ravo.outcome = tracing::field::Empty,
            ravo.reason = tracing::field::Empty,
        );
        let outcome = run.round().instrument(span.clone()).await;
        match outcome {
            Ok(Some(result)) => {
                if let Some(digest) = &result.gate_certificate_digest {
                    tracing::Span::current().record("ravo.certificate_digest", digest.as_str());
                }
                return Ok(result);
            }
            Ok(None) => {}
            Err(Stop::Reason(reason)) => {
                span.record("ravo.outcome", "stopped");
                span.record("ravo.reason", stop_name(reason));
                return Ok(run.stop(reason));
            }
            Err(Stop::Error(message)) => return Err(message),
        }
    }
    Ok(run.stop(RavoStopReason::RoundLimit))
}
