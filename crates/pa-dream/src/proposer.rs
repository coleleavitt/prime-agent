//! The generation-attempt seam a rollout runs against (TS `proposer.ts`).
//!
//! [`LocalProposer`] wraps `task.propose`: pure, synchronous and zero-token.
//! An LLM proposer (the in-session phase) implements [`Proposer`] too, marks
//! an accepted child candidate `origin: llm` and a local stand-in for a
//! rejected child output `origin: local`, and keeps a [`ProposalTally`].

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::improve::CandidateOrigin;
use crate::rng::SeededRng;
use crate::store::DreamStoreError;
use crate::task::{Artifact, DynTask, ProposeParams};

/// One attempt's outcome.
pub struct ProposeOutcome {
    pub artifact: Artifact,
    pub tokens: u64,
    /// Who generated `artifact`; `None` reads as the local proposer.
    pub origin: Option<CandidateOrigin>,
}

/// A generation attempt. Implementations must draw randomness only from `rng`
/// so a rollout stays a function of its seed.
pub trait Proposer {
    /// Produce a child of `parent` (a fresh artifact when `None`).
    ///
    /// # Errors
    ///
    /// [`DreamStoreError::Aborted`] when the run was cancelled mid-attempt,
    /// or a store error from the proposer's own logging; the rollout stops.
    fn propose(
        &mut self,
        parent: Option<&Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> Result<ProposeOutcome, DreamStoreError>;
}

/// The zero-token local proposer.
pub struct LocalProposer<'a> {
    task: &'a dyn DynTask,
}

impl<'a> LocalProposer<'a> {
    #[must_use]
    pub fn new(task: &'a dyn DynTask) -> Self {
        Self { task }
    }
}

impl Proposer for LocalProposer<'_> {
    fn propose(
        &mut self,
        parent: Option<&Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> Result<ProposeOutcome, DreamStoreError> {
        Ok(ProposeOutcome {
            artifact: self.task.propose(parent, params, rng, round),
            tokens: 0,
            origin: None,
        })
    }
}

/// Why a child proposer result was rejected (TS `PROPOSAL_REJECT_REASONS`, in order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProposalRejectReason {
    Parse,
    Shape,
    InvalidCandidate,
    Error,
    Length,
    Aborted,
    TurnLimit,
    Budget,
}

/// Every reject reason, in the TS order.
pub const PROPOSAL_REJECT_REASONS: [ProposalRejectReason; 8] = [
    ProposalRejectReason::Parse,
    ProposalRejectReason::Shape,
    ProposalRejectReason::InvalidCandidate,
    ProposalRejectReason::Error,
    ProposalRejectReason::Length,
    ProposalRejectReason::Aborted,
    ProposalRejectReason::TurnLimit,
    ProposalRejectReason::Budget,
];

impl ProposalRejectReason {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Shape => "shape",
            Self::InvalidCandidate => "invalid-candidate",
            Self::Error => "error",
            Self::Length => "length",
            Self::Aborted => "aborted",
            Self::TurnLimit => "turn-limit",
            Self::Budget => "budget",
        }
    }

    /// Parse a wire literal.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        PROPOSAL_REJECT_REASONS
            .iter()
            .copied()
            .find(|reason| reason.as_str() == name)
    }

    fn index(self) -> usize {
        PROPOSAL_REJECT_REASONS
            .iter()
            .position(|reason| *reason == self)
            .unwrap_or(0)
    }
}

/// Rejected child results by reason; serializes with every reason present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RejectCounts([u64; 8]);

impl RejectCounts {
    /// The count for one reason.
    #[must_use]
    pub fn get(&self, reason: ProposalRejectReason) -> u64 {
        self.0[reason.index()]
    }

    /// Add one rejection.
    pub fn add(&mut self, reason: ProposalRejectReason) {
        self.0[reason.index()] += 1;
    }

    /// Every count summed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }

    /// `(reason, count)` in the TS order.
    pub fn iter(&self) -> impl Iterator<Item = (ProposalRejectReason, u64)> + '_ {
        PROPOSAL_REJECT_REASONS
            .iter()
            .map(|reason| (*reason, self.get(*reason)))
    }
}

impl Serialize for RejectCounts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PROPOSAL_REJECT_REASONS.len()))?;
        for (reason, count) in self.iter() {
            map.serialize_entry(reason.as_str(), &count)?;
        }
        map.end()
    }
}

/// Per-rollout proposer provenance; all zero on the local path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalTally {
    /// Child results examined (accepted + rejected, retries included).
    pub llm_proposals: u64,
    /// Child results that entered the tree as `origin: llm` nodes.
    pub llm_accepted: u64,
    pub llm_rejected: RejectCounts,
    /// Attempts whose candidate came from the local mutator after a rejection.
    pub local_fallbacks: u64,
}

impl ProposalTally {
    /// One child result entered the tree (TS `tallyAccepted`).
    pub fn accept(&mut self) {
        self.llm_proposals += 1;
        self.llm_accepted += 1;
    }

    /// One child result was refused; `fell_back` when the local mutator stood
    /// in for it (TS `tallyRejected`).
    pub fn reject(&mut self, reason: ProposalRejectReason, fell_back: bool) {
        self.llm_proposals += 1;
        self.llm_rejected.add(reason);
        if fell_back {
            self.local_fallbacks += 1;
        }
    }

    /// `self + other`.
    #[must_use]
    pub fn plus(&self, other: &Self) -> Self {
        let mut sum = *self;
        sum.llm_proposals += other.llm_proposals;
        sum.llm_accepted += other.llm_accepted;
        sum.local_fallbacks += other.local_fallbacks;
        for (index, count) in other.llm_rejected.0.iter().enumerate() {
            sum.llm_rejected.0[index] += count;
        }
        sum
    }
}
