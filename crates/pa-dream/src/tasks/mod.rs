//! The built-in scored tasks and their registry (TS `tasks/index.ts`).

pub mod autocorrelation;
pub mod circle_packing;
pub mod python_speedup;
pub mod sum_difference;

use std::fmt;
use std::sync::Arc;

use crate::task::DynTask;

/// A built-in task id (TS `DREAM_TASK_IDS`, in order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DreamTaskId {
    CirclePacking,
    SumDifference,
    PythonSpeedup,
    Autocorrelation,
}

/// Every built-in task id, in the TS order.
pub const DREAM_TASK_IDS: &[DreamTaskId] = &[
    DreamTaskId::CirclePacking,
    DreamTaskId::SumDifference,
    DreamTaskId::PythonSpeedup,
    DreamTaskId::Autocorrelation,
];

/// The circle counts the paper studies; the default is the smaller one.
pub const DEFAULT_CIRCLE_PACKING_N: usize = 26;

impl DreamTaskId {
    /// The wire id.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CirclePacking => "circle-packing",
            Self::SumDifference => "sum-difference",
            Self::PythonSpeedup => "python-speedup",
            Self::Autocorrelation => "autocorrelation",
        }
    }

    /// The task named by `text`.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        DREAM_TASK_IDS
            .iter()
            .copied()
            .find(|id| id.as_str() == text)
    }

    /// Whether `evaluate` reads the wall clock (`timing`) or not (`deterministic`).
    #[must_use]
    pub fn scoring(self) -> &'static str {
        match self {
            Self::PythonSpeedup => "timing",
            Self::CirclePacking | Self::SumDifference | Self::Autocorrelation => "deterministic",
        }
    }
}

impl fmt::Display for DreamTaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A size the task does not accept (TS `RangeError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TaskSizeError(pub String);

/// The size `resolve_task` builds with: `n`, else the task's default; `None`
/// for a task without one.
#[must_use]
pub fn resolve_task_n(task: DreamTaskId, n: Option<usize>) -> Option<usize> {
    match task {
        DreamTaskId::CirclePacking => Some(n.unwrap_or(DEFAULT_CIRCLE_PACKING_N)),
        DreamTaskId::Autocorrelation => {
            Some(n.unwrap_or(autocorrelation::DEFAULT_AUTOCORRELATION_N))
        }
        DreamTaskId::SumDifference | DreamTaskId::PythonSpeedup => None,
    }
}

fn shown(n: Option<usize>) -> String {
    n.map_or_else(|| "undefined".to_string(), |n| n.to_string())
}

/// Build a task.
///
/// # Errors
///
/// [`TaskSizeError`] for a size the task rejects.
pub fn resolve_task(
    task: DreamTaskId,
    n: Option<usize>,
) -> Result<Arc<dyn DynTask>, TaskSizeError> {
    match task {
        DreamTaskId::CirclePacking => {
            let size = n.unwrap_or(DEFAULT_CIRCLE_PACKING_N);
            if size < 2 {
                return Err(TaskSizeError(format!(
                    "circle-packing requires an integer n >= 2 (got {})",
                    shown(n)
                )));
            }
            Ok(Arc::new(circle_packing::CirclePacking::new(size)))
        }
        DreamTaskId::SumDifference => Ok(Arc::new(sum_difference::SumDifference)),
        DreamTaskId::PythonSpeedup => Ok(Arc::new(python_speedup::PythonSpeedup::new())),
        DreamTaskId::Autocorrelation => {
            let size = n.unwrap_or(autocorrelation::DEFAULT_AUTOCORRELATION_N);
            if !autocorrelation::AUTOCORRELATION_BIN_COUNTS.contains(&size) {
                let counts: Vec<String> = autocorrelation::AUTOCORRELATION_BIN_COUNTS
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                return Err(TaskSizeError(format!(
                    "autocorrelation requires n in {{{}}} (got {})",
                    counts.join(", "),
                    shown(n)
                )));
            }
            Ok(Arc::new(autocorrelation::Autocorrelation::new(size)))
        }
    }
}

/// The task-specific context an LLM proposer prompt carries, if any.
#[must_use]
pub fn task_prompt_context(task: DreamTaskId, n: Option<usize>) -> Option<String> {
    match task {
        DreamTaskId::PythonSpeedup => {
            Some(python_speedup::PYTHON_SPEEDUP_PROMPT_CONTEXT.to_string())
        }
        DreamTaskId::Autocorrelation => Some(autocorrelation::autocorrelation_prompt_context(n)),
        DreamTaskId::CirclePacking | DreamTaskId::SumDifference => None,
    }
}
