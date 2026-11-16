//! The `<session-history>` message: applied compartments rendered at the
//! tier the decay curve picks (the frozen part), then the compartments
//! published since the last refold at full detail.

use crate::decay::{DecayInput, compute_budget_pressure, rendered_tier};
use crate::state::Compartment;

/// The history message's opening line.
pub(crate) const HISTORY_OPEN: &str = "<session-history>";
const HISTORY_CLOSE: &str = "</session-history>";
const HISTORY_LEAD: &str = "Earlier in this session, folded into compartments (oldest first; detail fades with age). \
ctx.expand(start=N, end=M) returns a compartment's raw transcript.";

fn attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One compartment at `tier` (1..4; 5 renders nothing). An empty tier falls
/// back to the nearest more detailed one.
fn compartment(compartment: &Compartment, tier: u8) -> Option<String> {
    if tier >= 5 {
        return None;
    }
    let wanted = usize::from(tier.max(1)) - 1;
    let body = (0..=wanted)
        .rev()
        .chain(wanted + 1..4)
        .map(|index| compartment.tiers[index].trim())
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    Some(format!(
        "<compartment tags=\"§{}§-§{}§\" title=\"{}\">\n{body}\n</compartment>",
        compartment.start_tag,
        compartment.end_tag,
        attribute(&compartment.title)
    ))
}

/// `compartments` (oldest first) at their decayed tiers within
/// `history_budget` tokens.
pub(crate) fn decayed(compartments: &[Compartment], history_budget: f64) -> String {
    let count = compartments.len();
    let inputs: Vec<DecayInput> = compartments
        .iter()
        .enumerate()
        .map(|(position, compartment)| DecayInput {
            index: u32::try_from(count - position).unwrap_or(u32::MAX),
            importance: compartment.importance,
        })
        .collect();
    let pressure = compute_budget_pressure(&inputs, history_budget);
    compartments
        .iter()
        .zip(&inputs)
        .filter_map(|(compartment_, input)| {
            compartment(
                compartment_,
                rendered_tier(input.index, input.importance, pressure, 0.0),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `compartments` at full detail.
pub(crate) fn fresh(compartments: &[Compartment]) -> String {
    compartments
        .iter()
        .filter_map(|compartment_| compartment(compartment_, 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The history message text.
pub(crate) fn history(frozen: &str, fresh: &str) -> String {
    let mut out = format!("{HISTORY_OPEN}\n{HISTORY_LEAD}\n");
    for part in [frozen, fresh] {
        if !part.is_empty() {
            out.push_str(part);
            out.push('\n');
        }
    }
    out.push_str(HISTORY_CLOSE);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn compartment_n(n: u32, importance: i32) -> Compartment {
        Compartment {
            start_key: None,
            end_key: None,
            start_tag: n * 10,
            end_tag: n * 10 + 9,
            title: format!("step {n}"),
            importance,
            created_ms: 0,
            tiers: [
                format!("p1 of {n} with every anchor"),
                format!("p2 of {n}"),
                format!("p3 of {n}"),
                String::new(),
            ],
        }
    }

    #[test]
    fn old_compartments_fade_and_the_newest_stays_verbose() {
        let compartments: Vec<Compartment> = (1..=80).map(|n| compartment_n(n, 50)).collect();
        let text = decayed(&compartments, 4_000.0);
        assert!(
            text.contains("p1 of 80 with every anchor"),
            "the newest renders at P1"
        );
        assert!(
            !text.contains("p1 of 1 "),
            "the oldest is no longer verbose"
        );
        assert!(text.len() < fresh(&compartments).len());
    }

    #[test]
    fn an_empty_tier_falls_back_and_titles_are_escaped() {
        let mut one = compartment_n(1, 50);
        one.title = "a \"quoted\" <title>".to_string();
        let text = compartment(&one, 4).unwrap();
        assert!(text.contains("p3 of 1"), "P4 empty falls back to P3");
        assert!(text.contains("title=\"a &quot;quoted&quot; &lt;title&gt;\""));
        assert_eq!(compartment(&one, 5), None);
        assert_eq!(
            history("", ""),
            format!("{HISTORY_OPEN}\n{HISTORY_LEAD}\n{HISTORY_CLOSE}")
        );
    }
}
