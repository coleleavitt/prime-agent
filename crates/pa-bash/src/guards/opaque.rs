//! The evidence gate on code the model cannot see: a rule may refuse an
//! opaque node only when the node's own visible source names the rule's
//! danger.

use crate::model::{Model, Opaque, OpaqueKind};

/// Every kind of hidden code.
pub(super) const ANY: [OpaqueKind; 6] = [
    OpaqueKind::Script,
    OpaqueKind::Stdin,
    OpaqueKind::Pipe,
    OpaqueKind::Dynamic,
    OpaqueKind::CommandWord,
    OpaqueKind::Unparsed,
];

/// The first opaque node of one of `kinds` whose evidence `found`
/// recognises, with the evidence it names (`` `push` and `-f` ``).
pub(super) fn evidenced<'m>(
    model: &'m Model,
    kinds: &[OpaqueKind],
    found: impl Fn(&str) -> Option<String>,
) -> Option<(&'m Opaque, String)> {
    model
        .opaques
        .iter()
        .filter(|opaque| kinds.contains(&opaque.kind))
        .find_map(|opaque| found(&opaque.evidence).map(|evidence| (opaque, evidence)))
}

/// The message reason for an evidenced opaque node: what hides the code,
/// and the evidence its visible text carries.
pub(super) fn reason(opaque: &Opaque, evidence: &str) -> String {
    format!(
        "{}, and its visible text mentions {evidence}",
        opaque.describe()
    )
}
