//! Reduce nudges (magic-context's channel 1, adapted): when the live tail
//! is large and much of it is tool output the model has not dropped, a
//! hidden row after the tool batch names the oldest outputs. Quiet below
//! the floors; refires only on a stronger band or real growth.

use crate::state::NudgeState;
use crate::transform::Report;

/// The live tail must hold at least this many tokens...
const MIN_TAIL: usize = 60_000;
/// ...and undropped tool output at least this many.
const MIN_UNDROPPED: usize = 25_000;
/// The undropped share of the tail that starts each band.
const BANDS: [f64; 3] = [0.2, 0.4, 0.6];
/// Growth that refires the same band: the larger of this and 8% of the tail.
const REFIRE_GROWTH: usize = 25_000;
/// The oldest outputs a nudge names.
const NAMED: usize = 6;

/// The nudge for `report`, updating `state`; `None` when quiet.
#[expect(
    clippy::cast_precision_loss,
    reason = "token counts fit an f64 exactly"
)]
pub(crate) fn nudge(report: &Report, state: &mut NudgeState) -> Option<String> {
    let undropped: usize = report.outputs.iter().map(|output| output.tokens).sum();
    if report.tail_tokens < MIN_TAIL || undropped < MIN_UNDROPPED {
        if report.tail_tokens < state.fired_at_tail {
            *state = NudgeState::default();
        }
        return None;
    }
    let ratio = undropped as f64 / report.tail_tokens as f64;
    let band = u8::try_from(BANDS.iter().filter(|start| ratio >= **start).count()).unwrap_or(3);
    if band == 0 {
        return None;
    }
    let growth = REFIRE_GROWTH.max(report.tail_tokens / 12);
    if band <= state.band && report.tail_tokens < state.fired_at_tail + growth {
        return None;
    }
    *state = NudgeState {
        fired_at_tail: report.tail_tokens,
        band,
    };
    let count = report.outputs.len();
    let thousands = undropped.div_ceil(1_000);
    let lead = match band {
        1 => format!(
            "Housekeeping: {count} tool outputs (~{thousands}k tokens) are still in context. Drop each with ctx.reduce once you have taken what you need from it; don't wait for the task to finish."
        ),
        2 => format!(
            "Context: {count} tool outputs (~{thousands}k tokens) are still in context. Make a ctx.reduce pass now over the outputs you have already used, then continue."
        ),
        _ => format!(
            "Context is filling: {count} tool outputs (~{thousands}k tokens) are still undropped. Call ctx.reduce now, before your next cell, on every output you have already used."
        ),
    };
    let oldest: Vec<String> = report
        .outputs
        .iter()
        .take(NAMED)
        .map(|output| format!("§{}§ (~{}k)", output.tag, output.tokens.div_ceil(1_000)))
        .collect();
    Some(format!(
        "{lead} Oldest first: {}. Usage {}% of {}k; ctx.expand(tag) brings any of them back.",
        oldest.join(", "),
        (report.share() * 100.0).round(),
        report.window / 1_000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::Output;

    fn report(tail: usize, outputs: &[(u32, usize)]) -> Report {
        Report {
            usage: tail,
            window: 200_000,
            tail_tokens: tail,
            outputs: outputs
                .iter()
                .map(|&(tag, tokens)| Output {
                    tag,
                    tokens,
                    preview: String::new(),
                })
                .collect(),
            ..Report::default()
        }
    }

    #[test]
    fn quiet_below_the_floors_then_bands_escalate() {
        let mut state = NudgeState::default();
        assert_eq!(nudge(&report(50_000, &[(1, 40_000)]), &mut state), None);
        assert_eq!(nudge(&report(100_000, &[(1, 10_000)]), &mut state), None);
        let gentle = nudge(&report(100_000, &[(4, 15_000), (9, 15_000)]), &mut state).unwrap();
        assert!(gentle.starts_with("Housekeeping") && gentle.contains("§4§ (~15k), §9§ (~15k)"));
        assert_eq!(state.band, 1);
        assert_eq!(
            nudge(&report(101_000, &[(4, 15_000), (9, 15_000)]), &mut state),
            None,
            "no refire without growth"
        );
        let urgent = nudge(&report(110_000, &[(4, 40_000), (9, 30_000)]), &mut state).unwrap();
        assert!(urgent.starts_with("Context is filling"));
        assert_eq!(state.band, 3);
    }
}
