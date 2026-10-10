//! The refinement-gate seam: an installed feature may judge a planned
//! refinement before it applies, refuse it, and keep its own bookkeeping in
//! the harness state file.
//!
//! The native product installs no gate, and the refine funnel then runs
//! exactly as before. A feature supplies one through
//! [`crate::features::SessionFeature::refinement_gate`]; the funnel
//! ([`crate::session_engine::refine::execute_refinement_gated`]) asks it to
//! evaluate every planned, non-empty, non-rollback proposal, asks the
//! resulting verdict to admit or refuse the edit set against the store as
//! it stands when the edits would apply, and lets the verdict record what
//! happened in the state it saves.

use std::sync::Arc;

use pa_types::session::AgentMessage;

use super::executor::RefinerFn;
use super::planner::RefinementProposal;
use super::{HarnessScope, HarnessState, RefinementResult};
use crate::features::FeatureFuture;
use crate::session_engine::refine::RefinementSource;
use crate::session_engine::turn_boundary::{RefineRequester, RefineTrigger};

/// A value held for the whole run of one refine (plan, gate, apply, save)
/// and dropped when it ends, however it ends.
pub type RefineGuard = Box<dyn Send>;

/// Everything a gate may read about one planned refinement.
pub struct RefinementGateRequest {
    /// The id the refinement will be recorded under.
    pub proposal_id: String,
    /// The planned edit set, display prefixes already stripped: exactly the
    /// edits that would apply.
    pub proposal: RefinementProposal,
    /// The store the plan targets.
    pub scope: HarnessScope,
    /// That store as it was read before planning.
    pub baseline_state: HarnessState,
    /// What the planner saw (the target store, merged with the global one
    /// for a local refine).
    pub planning_state: HarnessState,
    /// The conversation the plan was made from.
    pub messages: Vec<AgentMessage>,
    /// The model the refinement runs on.
    pub model: pa_types::ai::Model,
    /// Who asked for the refinement.
    pub source: RefinementSource,
    /// Why a feature asked for it, when one did.
    pub trigger: Option<RefineTrigger>,
    /// One call to `model`, for a gate that consults it.
    pub model_call: RefinerFn,
}

/// A verdict's decision on the edit set at apply time.
pub enum GateAdmission {
    /// Apply the edits.
    Apply,
    /// Apply nothing; record this result instead (no edit applied).
    Reject(Box<RefinementResult>),
}

/// The outcome of one gate evaluation, consulted while the refinement
/// applies. Implementations must be cheap and must not block: the funnel
/// calls them between re-reading the target store and saving it.
pub trait RefinementGateVerdict: Send + Sync {
    /// Decide on `proposal` against `current`, the target store re-read
    /// immediately before the edits would apply.
    fn admit(&self, proposal: &RefinementProposal, current: &HarnessState) -> GateAdmission;

    /// After [`Self::admit`] refused: update `state` (the re-read store);
    /// return `true` to have the funnel save it.
    fn record_rejection(&self, state: &mut HarnessState) -> bool;

    /// After [`Self::admit`] admitted, before the edits apply: update
    /// `state` (the re-read store) first, such as settling bookkeeping the
    /// apply must see. The default changes nothing.
    fn prepare_application(&self, state: &mut HarnessState) {
        let _ = state;
    }

    /// After [`Self::admit`] admitted and the edits were applied to `state`
    /// (`result` says which applied): update `state` and `result` before
    /// the funnel saves and records them.
    fn record_application(&self, state: &mut HarnessState, result: &mut RefinementResult);
}

/// A feature's judge of planned refinements. One instance serves one
/// session (see [`crate::features::SessionFeature::refinement_gate`]).
pub trait RefinementGate: Send + Sync {
    /// A refine of this session started; the returned guard lives until
    /// its harness write landed or it failed. The default holds nothing.
    fn begin_refine(&self) -> Option<RefineGuard> {
        None
    }

    /// The session accepts refinements this gate's feature requests: it may
    /// auto-refine, and auto-refine is on. Called once, on the
    /// session-creation path; the default ignores it.
    fn attach_refine_requester(&self, requester: RefineRequester) {
        let _ = requester;
    }

    /// Evaluate one planned refinement. `Ok(None)` lets it apply ungated;
    /// an `Err` fails the refinement before anything applies.
    fn evaluate(
        &self,
        request: RefinementGateRequest,
    ) -> FeatureFuture<anyhow::Result<Option<Box<dyn RefinementGateVerdict>>>>;
}

/// A session's gate, with the model call its evaluation may make.
pub struct RefinementGating {
    pub gate: Arc<dyn RefinementGate>,
    pub model_call: RefinerFn,
}
