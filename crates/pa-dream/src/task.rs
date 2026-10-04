//! The pluggable scored objective a discovery tree grows over (TS `task.ts`).
//!
//! A [`ScoredTask`] is pure and deterministic given the injected rng: it seeds a
//! root artifact, perturbs a parent into a child, and scores an artifact by
//! INDEPENDENTLY recomputing validity. A rollout over such a task spends zero
//! model tokens and touches no network. The rollout drivers hold tasks as
//! [`DynTask`] trait objects with type-erased [`Artifact`]s, so a test or a
//! later in-session caller can bring its own task.

use std::any::Any;

use serde_json::Value;

use crate::rng::SeededRng;

/// Knobs the exploration policy projects onto one generation attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProposeParams {
    /// Size of the seeded perturbation.
    pub step_scale: f64,
    /// How many candidates one `propose` may draw before returning its best.
    pub refine_depth: u32,
    /// The raw strategy width the scale derives from.
    pub branch_width: u32,
}

/// Why an artifact scored invalid (TS `DreamFailClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailClass {
    InvalidShape,
    OutOfBounds,
    Overlap,
    NegativeRadius,
    NegativeWeight,
    NonFinite,
    TooSmall,
    Degenerate,
    Incorrect,
    Timeout,
    RuntimeError,
}

impl FailClass {
    /// The wire name a node line records.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidShape => "invalid-shape",
            Self::OutOfBounds => "out-of-bounds",
            Self::Overlap => "overlap",
            Self::NegativeRadius => "negative-radius",
            Self::NegativeWeight => "negative-weight",
            Self::NonFinite => "non-finite",
            Self::TooSmall => "too-small",
            Self::Degenerate => "degenerate",
            Self::Incorrect => "incorrect",
            Self::Timeout => "timeout",
            Self::RuntimeError => "runtime-error",
        }
    }
}

/// The outcome of scoring one artifact; `score` is always finite (0 when invalid).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Evaluation {
    pub valid: bool,
    pub score: f64,
    pub fail_class: Option<FailClass>,
}

impl Evaluation {
    /// A valid evaluation.
    #[must_use]
    pub fn valid(score: f64) -> Self {
        Self {
            valid: true,
            score,
            fail_class: None,
        }
    }

    /// An invalid evaluation: score 0 and the failure class.
    #[must_use]
    pub fn invalid(fail_class: FailClass) -> Self {
        Self {
            valid: false,
            score: 0.0,
            fail_class: Some(fail_class),
        }
    }
}

/// A JSON value that is not the task's artifact shape (TS `TypeError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ArtifactShapeError(pub String);

/// A typed scored task. Implementations must be deterministic given the rng.
pub trait ScoredTask: Send + Sync {
    /// The task's artifact type.
    type Artifact: Send + Sync + 'static;

    /// The task id recorded on every tree header.
    fn id(&self) -> &str;
    /// The root artifact of a fresh tree.
    fn root(&self, rng: &mut SeededRng) -> Self::Artifact;
    /// A child of `parent` (or a fresh artifact when `None`); deterministic given `rng`.
    fn propose(
        &self,
        parent: Option<&Self::Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> Self::Artifact;
    /// Recompute validity and score, independently of how the artifact was made.
    fn evaluate(&self, candidate: &Self::Artifact) -> Evaluation;
    /// The JSON projection persisted as the node's blob and hashed into its `artifactRef`.
    fn serialize(&self, candidate: &Self::Artifact) -> Value;
    /// Parse and clamp a JSON value back into an artifact.
    ///
    /// # Errors
    ///
    /// [`ArtifactShapeError`] when the value is not the artifact's shape.
    fn deserialize(&self, value: &Value) -> Result<Self::Artifact, ArtifactShapeError>;
}

/// A type-erased artifact.
pub type Artifact = Box<dyn Any + Send + Sync>;

/// A [`ScoredTask`] behind a trait object. Every `ScoredTask` is one; an
/// artifact of another task's type is treated as missing (`propose`) or as an
/// `invalid-shape` evaluation.
pub trait DynTask: Send + Sync {
    /// The task id recorded on every tree header.
    fn id(&self) -> &str;
    /// The root artifact of a fresh tree.
    fn root(&self, rng: &mut SeededRng) -> Artifact;
    /// A child of `parent`, deterministic given `rng`.
    fn propose(
        &self,
        parent: Option<&Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> Artifact;
    /// Score an artifact.
    fn evaluate(&self, candidate: &Artifact) -> Evaluation;
    /// The persisted JSON projection.
    fn serialize(&self, candidate: &Artifact) -> Value;
    /// Parse a JSON value into an artifact.
    ///
    /// # Errors
    ///
    /// [`ArtifactShapeError`] when the value is not the artifact's shape.
    fn deserialize(&self, value: &Value) -> Result<Artifact, ArtifactShapeError>;
}

impl<T: ScoredTask> DynTask for T {
    fn id(&self) -> &str {
        ScoredTask::id(self)
    }

    fn root(&self, rng: &mut SeededRng) -> Artifact {
        Box::new(ScoredTask::root(self, rng))
    }

    fn propose(
        &self,
        parent: Option<&Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> Artifact {
        let parent = parent.and_then(|artifact| artifact.downcast_ref::<T::Artifact>());
        Box::new(ScoredTask::propose(self, parent, params, rng, round))
    }

    fn evaluate(&self, candidate: &Artifact) -> Evaluation {
        candidate
            .downcast_ref::<T::Artifact>()
            .map_or(Evaluation::invalid(FailClass::InvalidShape), |artifact| {
                ScoredTask::evaluate(self, artifact)
            })
    }

    fn serialize(&self, candidate: &Artifact) -> Value {
        candidate
            .downcast_ref::<T::Artifact>()
            .map_or(Value::Null, |artifact| {
                ScoredTask::serialize(self, artifact)
            })
    }

    fn deserialize(&self, value: &Value) -> Result<Artifact, ArtifactShapeError> {
        ScoredTask::deserialize(self, value).map(|artifact| Box::new(artifact) as Artifact)
    }
}
