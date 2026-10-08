//! The scan's deterministic work budget.
//!
//! The scan walks command-substitution interiors, payloads and alias bodies
//! recursively, so a command that nests substitutions costs work exponential
//! in the nesting depth rather than proportional to its length. The budget
//! counts nested re-scans (entering an interior, a payload or an alias body
//! costs one unit; the linear passes cost nothing), never characters or
//! wall-clock time: length alone is never a reason to refuse. Measured
//! entries: a flat 34 KB command 0; realistic commands 1-400; the nesting
//! shapes 81 (depth 4 fanout 3) up to 2187 (backtick depth 7 fanout 3).

use std::cell::Cell;

/// Units one guard call may spend on nested re-scans.
const SCAN_WORK_BUDGET: i64 = 4_000;
/// Nesting deeper than this is refused on its own: three levels of nested
/// substitution is already more than any real command needs.
pub(super) const MAX_SUBSTITUTION_DEPTH: u32 = 3;

/// Why a scan stopped before it finished: the guard refuses what it could
/// not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanStop {
    /// The budget ran out: too many nested re-scans.
    LimitExceeded,
    /// Command substitutions nest deeper than the guard follows.
    NestingTooDeep,
}

/// A scan step that may stop the whole scan.
pub(super) type Scan<T> = Result<T, ScanStop>;

/// The work budget of one guard call, shared by every nested scan in it.
#[derive(Debug)]
pub(super) struct Budget {
    remaining: Cell<i64>,
    depth: Cell<u32>,
    max_depth: u32,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            remaining: Cell::new(SCAN_WORK_BUDGET),
            depth: Cell::new(0),
            max_depth: MAX_SUBSTITUTION_DEPTH,
        }
    }

    /// No limit at all: how the scan helpers behave when called outside a
    /// guard check (the Python suites call them that way).
    #[cfg(test)]
    pub(super) fn unlimited() -> Self {
        Self {
            remaining: Cell::new(i64::MAX),
            depth: Cell::new(0),
            max_depth: u32::MAX,
        }
    }

    /// The units spent so far (past the budget once it ran out).
    #[cfg(test)]
    pub(super) fn spent(&self) -> i64 {
        SCAN_WORK_BUDGET - self.remaining.get()
    }

    /// The units one guard call may spend.
    #[cfg(test)]
    pub(super) const LIMIT: i64 = SCAN_WORK_BUDGET;

    /// Spend one unit of nested re-scan work.
    pub(super) fn charge(&self) -> Scan<()> {
        let remaining = self.remaining.get() - 1;
        self.remaining.set(remaining);
        if remaining < 0 {
            return Err(ScanStop::LimitExceeded);
        }
        Ok(())
    }

    /// Enter one substitution interior: charge it, then go one level deeper.
    pub(super) fn enter(&self) -> Scan<()> {
        self.charge()?;
        let depth = self.depth.get() + 1;
        self.depth.set(depth);
        if depth > self.max_depth {
            return Err(ScanStop::NestingTooDeep);
        }
        Ok(())
    }

    /// Leave the interior [`Self::enter`] entered.
    pub(super) fn leave(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }
}
