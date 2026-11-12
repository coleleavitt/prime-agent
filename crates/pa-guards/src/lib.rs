//! Quality guards for Prime Agent, ported from the standalone `agent-guards`
//! crate (which was extracted from jfc).
//!
//! Pure logic — no runtime wiring. The `pa-hooks` / continuation paths call:
//! - [`slop_guard::run_all_checks_scoped`] after an edit (advice),
//! - [`work_quality::evaluate_work_quality`] before a task is marked done (gate),
//! - [`continuation::decide`] when a turn ends (self-continue decision).

// Design-choice exceptions for this heuristic text/metric code (the sanctioned
// per-crate #![allow] route). Each is intentional, not a latent bug:
// - the checks count lines/branches and compute ratio percentages, where an
//   exact numeric cast is irrelevant (the thresholds are coarse);
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
// - duplication/scoping take a borrowed `HashSet<usize>` from one known caller;
//   generalizing the hasher buys nothing here;
#![allow(clippy::implicit_hasher)]
// - the `.unwrap()`s are on compile-time-constant regexes and on windows whose
//   length was just checked — they cannot panic at runtime;
#![allow(clippy::missing_panics_doc)]
// - source-file detection matches the lowercase extensions these agents emit;
#![allow(clippy::case_sensitive_file_extension_comparisons)]
// - a couple of report builders push `format!` results into a String, which is
//   clearer here than threading `write!`.
#![allow(clippy::format_push_string)]

pub mod continuation;
mod feature;
mod rust_lex;
pub mod scaffold_detector;
pub mod slop_guard;
pub mod work_quality;

pub use feature::GuardsFeature;
