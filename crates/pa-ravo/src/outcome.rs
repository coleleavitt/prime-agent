//! The structured line each refinement's final decision logs (TS
//! `logRefinementOutcome`), with the TS record's field names and shapes:
//! `refinement.committed` (`proposalId`, `addressed` as an array,
//! `deepScore`, `missed`, `reason`, `scope`), `refinement.applied_unmeasured`
//! (`proposalId`, `deepScore`, `reason`, `scope`), and `refinement.rejected`
//! (`proposalId`, `decision`, `deepScore`, `missed`, `claimed`, `reason`,
//! `scope`, and `cause` for a `reject_*` decision). The array rides a
//! `<key>.json` field (`pa-trace`'s JSON-valued field convention).

/// Where the outcome lines go (TS `REFINEMENT_LOG_COMPONENT`).
pub const REFINEMENT_LOG_TARGET: &str = "pa_ravo::refinement";

/// One refinement's final decision.
pub(crate) struct RefinementOutcome<'a> {
    pub proposal_id: &'a str,
    /// `commit`, `commit_unmeasured`, `partial`, `reject_*`.
    pub decision: &'a str,
    /// The fingerprints the commit is credited with addressing.
    pub addressed: &'a [String],
    pub deep_score: u64,
    pub missed: usize,
    pub claimed: usize,
    pub reason: &'a str,
    pub scope: &'a str,
    pub cause: Option<&'a str>,
}

/// Log `outcome` as TS did.
pub(crate) fn log_refinement_outcome(outcome: &RefinementOutcome<'_>) {
    let RefinementOutcome {
        proposal_id,
        decision,
        addressed,
        deep_score,
        missed,
        claimed,
        reason,
        scope,
        cause,
    } = *outcome;
    if decision == "commit" && !addressed.is_empty() {
        let addressed = serde_json::to_string(addressed).unwrap_or_else(|_| "[]".to_string());
        tracing::info!(
            target: REFINEMENT_LOG_TARGET,
            proposalId = proposal_id,
            addressed.json = addressed.as_str(),
            deepScore = deep_score,
            missed = missed,
            reason = reason,
            scope = scope,
            "refinement.committed"
        );
        return;
    }
    if matches!(decision, "commit" | "commit_unmeasured" | "rollback") {
        tracing::info!(
            target: REFINEMENT_LOG_TARGET,
            proposalId = proposal_id,
            deepScore = deep_score,
            reason = reason,
            scope = scope,
            "refinement.applied_unmeasured"
        );
        return;
    }
    let cause = cause.filter(|_| decision.starts_with("reject_"));
    tracing::info!(
        target: REFINEMENT_LOG_TARGET,
        proposalId = proposal_id,
        decision = decision,
        deepScore = deep_score,
        missed = missed,
        claimed = claimed,
        reason = reason,
        scope = scope,
        cause = cause,
        "refinement.rejected"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    /// One event's fields, in order.
    type Fields = Vec<(String, String)>;

    /// Every event's message and fields, in order.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Fields>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visit(Vec<(String, String)>);
            impl tracing::field::Visit for Visit {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .push((field.name().to_string(), format!("{value:?}")));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push((field.name().to_string(), value.to_string()));
                }
            }
            let mut visit = Visit(Vec::new());
            event.record(&mut visit);
            self.0.lock().unwrap().push(visit.0);
        }
    }

    fn fields(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    /// The TS field names and shapes: camelCase keys, the addressed list as
    /// a JSON array, `cause` only on a `reject_*` decision.
    #[test]
    fn outcomes_carry_the_ts_record_fields() {
        let capture = Capture::default();
        let addressed = vec!["fa".to_string(), "fb".to_string()];
        let outcome = |decision, addressed: &'static [String], cause| RefinementOutcome {
            proposal_id: "p1",
            decision,
            addressed,
            deep_score: 80,
            missed: 1,
            claimed: 2,
            reason: "manual",
            scope: "local",
            cause,
        };
        let addressed: &'static [String] = Box::leak(addressed.into_boxed_slice());
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(capture.clone()),
            || {
                log_refinement_outcome(&outcome("commit", addressed, None));
                log_refinement_outcome(&outcome("commit", &[], None));
                log_refinement_outcome(&outcome("reject_deep", addressed, Some("gate")));
                log_refinement_outcome(&outcome("partial", addressed, Some("gate")));
            },
        );
        assert_eq!(
            *capture.0.lock().unwrap(),
            [
                fields(&[
                    ("message", "refinement.committed"),
                    ("proposalId", "p1"),
                    ("addressed.json", r#"["fa","fb"]"#),
                    ("deepScore", "80"),
                    ("missed", "1"),
                    ("reason", "manual"),
                    ("scope", "local"),
                ]),
                fields(&[
                    ("message", "refinement.applied_unmeasured"),
                    ("proposalId", "p1"),
                    ("deepScore", "80"),
                    ("reason", "manual"),
                    ("scope", "local"),
                ]),
                fields(&[
                    ("message", "refinement.rejected"),
                    ("proposalId", "p1"),
                    ("decision", "reject_deep"),
                    ("deepScore", "80"),
                    ("missed", "1"),
                    ("claimed", "2"),
                    ("reason", "manual"),
                    ("scope", "local"),
                    ("cause", "gate"),
                ]),
                fields(&[
                    ("message", "refinement.rejected"),
                    ("proposalId", "p1"),
                    ("decision", "partial"),
                    ("deepScore", "80"),
                    ("missed", "1"),
                    ("claimed", "2"),
                    ("reason", "manual"),
                    ("scope", "local"),
                ]),
            ]
        );
    }
}
