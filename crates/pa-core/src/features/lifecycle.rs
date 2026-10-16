//! The lifecycle seams' plumbing: the installed features' answers at the
//! agent's stop, after a tool batch, around a compaction, and at the
//! session's end, each composed in installation order.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    AgentStopObservation,
    CompactionObservation,
    SessionFeature,
    SessionFeatureContext,
    ToolBatchObservation,
};

/// Non-empty texts, joined in order; `None` when there are none.
fn joined(texts: Vec<Option<String>>) -> Option<String> {
    let texts: Vec<String> = texts
        .into_iter()
        .flatten()
        .filter(|text| !text.trim().is_empty())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

/// Every feature's [`SessionFeature::before_agent_stop`] answer: the
/// continuation texts joined in installation order, `None` to let the agent
/// stop.
pub(crate) async fn review_agent_stop(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    stop: &AgentStopObservation,
) -> Option<String> {
    joined(
        futures::future::join_all(
            features
                .iter()
                .map(|feature| feature.before_agent_stop(context, stop)),
        )
        .await,
    )
}

/// Every feature's [`SessionFeature::after_tool_batch`] context, joined in
/// installation order.
pub(crate) async fn review_tool_batch(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    batch: &ToolBatchObservation,
) -> Option<String> {
    joined(
        futures::future::join_all(
            features
                .iter()
                .map(|feature| feature.after_tool_batch(context, batch)),
        )
        .await,
    )
}

/// The compaction's summary instructions: the user's own, then every
/// feature's [`SessionFeature::before_compaction`] addition.
pub(crate) async fn compaction_instructions(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    compaction: &CompactionObservation,
) -> Option<String> {
    let added = joined(
        futures::future::join_all(
            features
                .iter()
                .map(|feature| feature.before_compaction(context, compaction)),
        )
        .await,
    );
    joined(vec![compaction.custom_instructions.clone(), added])
}

/// Tell every feature a compaction committed with `summary`.
pub(crate) fn observe_compaction(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    compaction: &CompactionObservation,
    summary: &str,
) {
    for feature in features {
        feature.after_compaction(context, compaction, summary);
    }
}

/// The session engine's end-of-session notice: tells the features once,
/// when the host ends the session or the engine drops, whichever is first.
pub(crate) struct SessionEndNotice {
    features: Vec<Arc<dyn SessionFeature>>,
    context: Arc<SessionFeatureContext>,
    fired: AtomicBool,
}

impl SessionEndNotice {
    pub(crate) fn new(
        features: &[Arc<dyn SessionFeature>],
        context: &Arc<SessionFeatureContext>,
    ) -> Self {
        Self {
            features: features.to_vec(),
            context: Arc::clone(context),
            fired: AtomicBool::new(false),
        }
    }

    /// Tell every feature the session ended with `reason`; later calls do
    /// nothing.
    pub(crate) fn fire(&self, reason: &str) {
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        for feature in &self.features {
            feature.on_session_end(&self.context, reason);
        }
    }
}

impl Drop for SessionEndNotice {
    fn drop(&mut self) {
        self.fire("other");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::features::{CompactionTrigger, FeatureFuture};

    /// Answers every lifecycle seam with its configured text and records
    /// session ends.
    struct Answering {
        text: Option<&'static str>,
        ends: Mutex<Vec<String>>,
    }

    impl SessionFeature for Answering {
        fn name(&self) -> &'static str {
            "answering"
        }

        fn before_agent_stop(
            &self,
            _context: &Arc<SessionFeatureContext>,
            _stop: &AgentStopObservation,
        ) -> FeatureFuture<Option<String>> {
            let text = self.text.map(str::to_string);
            Box::pin(async move { text })
        }

        fn before_compaction(
            &self,
            _context: &Arc<SessionFeatureContext>,
            _compaction: &CompactionObservation,
        ) -> FeatureFuture<Option<String>> {
            let text = self.text.map(str::to_string);
            Box::pin(async move { text })
        }

        fn on_session_end(&self, _context: &Arc<SessionFeatureContext>, reason: &str) {
            self.ends.lock().unwrap().push(reason.to_string());
        }
    }

    fn answering(text: Option<&'static str>) -> Arc<Answering> {
        Arc::new(Answering {
            text,
            ends: Mutex::default(),
        })
    }

    #[tokio::test]
    async fn answers_join_in_installation_order_and_blanks_drop() {
        let context = Arc::new(super::super::tests::context("lifecycle-join"));
        let features: Vec<Arc<dyn SessionFeature>> = vec![
            answering(Some("first")),
            answering(None),
            answering(Some("  ")),
            answering(Some("second")),
        ];
        let stop = AgentStopObservation {
            last_assistant_message: "done".to_string(),
            continued: false,
        };
        assert_eq!(
            review_agent_stop(&features, &context, &stop).await,
            Some("first\n\nsecond".to_string())
        );
        assert_eq!(
            review_agent_stop(
                &[answering(None) as Arc<dyn SessionFeature>],
                &context,
                &stop
            )
            .await,
            None
        );
        let compaction = CompactionObservation {
            trigger: CompactionTrigger::Manual,
            custom_instructions: Some("keep the API notes".to_string()),
        };
        assert_eq!(
            compaction_instructions(&features, &context, &compaction).await,
            Some("keep the API notes\n\nfirst\n\nsecond".to_string())
        );
        assert_eq!(
            compaction_instructions(&[], &context, &compaction).await,
            Some("keep the API notes".to_string())
        );
    }

    #[test]
    fn the_session_end_fires_once() {
        let context = Arc::new(super::super::tests::context("lifecycle-end"));
        let feature = answering(None);
        let notice =
            SessionEndNotice::new(&[Arc::clone(&feature) as Arc<dyn SessionFeature>], &context);
        notice.fire("closed");
        notice.fire("again");
        drop(notice);
        assert_eq!(*feature.ends.lock().unwrap(), vec!["closed".to_string()]);
        let dropped = answering(None);
        drop(SessionEndNotice::new(
            &[Arc::clone(&dropped) as Arc<dyn SessionFeature>],
            &context,
        ));
        assert_eq!(*dropped.ends.lock().unwrap(), vec!["other".to_string()]);
    }
}
