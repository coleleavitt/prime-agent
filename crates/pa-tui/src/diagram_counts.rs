//! Diagram render counts: how many settled `mermaid` fences the TUI drew, kept as source,
//! or drew after the renderer adapted their layout to fit — the composition root reads
//! them once per session run ([`take_render_counts`]).
//!
//! Counting is off until the composition root turns it on ([`set_render_counting`]). Each
//! diagram counts once, at its first settled (never streaming) layout: repaints, row counts,
//! and resizes of the same source are recognized by a 64-bit digest of the source in a
//! fixed, lock-free table, so the paint path only ever loads, compares-and-swaps, and
//! increments atomics — no lock and no I/O. Equal sources count once per run; a run that
//! settles more distinct diagrams than the table holds stops counting new ones (an
//! undercount, never a per-frame recount).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Distinct diagrams one run can count.
const SEEN_SLOTS: usize = 512;

static COUNTING: AtomicBool = AtomicBool::new(false);
static DRAWN: AtomicU64 = AtomicU64::new(0);
static KEPT_SOURCE: AtomicU64 = AtomicU64::new(0);
static ADAPTED: AtomicU64 = AtomicU64::new(0);
/// Digests of the diagrams counted this run; `0` marks a free slot.
static SEEN: [AtomicU64; SEEN_SLOTS] = [const { AtomicU64::new(0) }; SEEN_SLOTS];

/// The settled diagrams of one session run, by outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderCounts {
    /// Drawn as diagram rows (adapted or not).
    pub drawn: u64,
    /// Kept as their fenced source: unreadable, unsupported, too wide, or (built-in
    /// renderer) incomplete.
    pub kept_source: u64,
    /// Drawn after the installed renderer adapted the layout to fit (a subset of `drawn`).
    pub adapted: u64,
}

/// How one settled diagram was shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Drawn { adapted: bool },
    KeptSource,
}

impl Outcome {
    /// The outcome an installed renderer's layout shows.
    pub(crate) fn of(layout: &super::DiagramLayout) -> Self {
        match layout {
            super::DiagramLayout::Rows { adapted, .. } => Self::Drawn { adapted: *adapted },
            super::DiagramLayout::Source { .. } => Self::KeptSource,
        }
    }
}

/// Turn counting on or off. Turning it off also drops what was counted, so nothing counted
/// before an off period is ever read after it.
pub fn set_render_counting(enabled: bool) {
    if !COUNTING.swap(enabled, Ordering::Relaxed) || enabled {
        return;
    }
    let _ = take_render_counts();
}

/// The counts since the last take, resetting them (and which diagrams were seen) for the
/// next run.
pub fn take_render_counts() -> RenderCounts {
    for slot in &SEEN {
        slot.store(0, Ordering::Relaxed);
    }
    RenderCounts {
        drawn: DRAWN.swap(0, Ordering::Relaxed),
        kept_source: KEPT_SOURCE.swap(0, Ordering::Relaxed),
        adapted: ADAPTED.swap(0, Ordering::Relaxed),
    }
}

/// Count one settled diagram's outcome, once per source per run.
pub(crate) fn settled(source: &str, outcome: Outcome) {
    if !COUNTING.load(Ordering::Relaxed) || !first_sight(digest(source)) {
        return;
    }
    match outcome {
        Outcome::Drawn { adapted } => {
            DRAWN.fetch_add(1, Ordering::Relaxed);
            if adapted {
                ADAPTED.fetch_add(1, Ordering::Relaxed);
            }
        }
        Outcome::KeptSource => {
            KEPT_SOURCE.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A non-zero digest of `source` (zero marks a free slot).
fn digest(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish().max(1)
}

/// Claim `digest`'s slot: `true` the first time it is seen this run, `false` after (and
/// when the table is full).
fn first_sight(digest: u64) -> bool {
    // The modulus is below `SEEN_SLOTS`, so the narrowing is lossless.
    #[allow(clippy::cast_possible_truncation)]
    let start = (digest % SEEN_SLOTS as u64) as usize;
    for probe in 0..SEEN_SLOTS {
        let slot = &SEEN[(start + probe) % SEEN_SLOTS];
        match slot.compare_exchange(0, digest, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(held) if held == digest => return false,
            Err(_) => {}
        }
    }
    false
}
