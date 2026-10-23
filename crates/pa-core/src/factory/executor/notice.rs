//! One run milestone as the parent session's notice: the executor builds a
//! [`FactoryNotice`] per deduped milestone and hands it to the
//! [`super::ports::FactoryNotices`] lane, which renders it as a
//! `<task-notification>` row (see
//! [`crate::session_engine::messages::task_notification`]).

use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::session_engine::messages::{
    FACTORY_RUN_NOTICE_CUSTOM_TYPE,
    FACTORY_RUN_TASK_TYPE,
    TaskNotification,
    task_notification,
};

/// The milestone kind a host restart reports for a run it found in flight.
pub const INTERRUPTED_MILESTONE: &str = "interrupted";
/// The milestone kind a client-side (user) stop reports.
pub const CANCELLED_MILESTONE: &str = "cancelled";

/// The longest single final-state answer the completion notice quotes, in
/// chars (the whole `<result>` is bounded again by the builder).
const ANSWER_EXCERPT_CHARS: usize = 600;

/// One parent-visible milestone of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryNotice {
    pub run_id: String,
    /// The milestone kind: `finished`, `failed`, `paused`,
    /// `budget_exceeded`, `max_transitions_exceeded`,
    /// `max_children_exceeded`, `interrupted`, or `cancelled`.
    pub kind: String,
    pub detail: String,
    /// The state the milestone is about, when it is about one.
    pub node: Option<String>,
    /// The run's `name=` label, else its spec id.
    pub label: String,
    /// The run state after the milestone (`done`, `failed`, `paused`,
    /// `stopped`): the notice lane withdraws a queued notice once the model
    /// has read this state itself.
    pub run_state: String,
    /// `(state id, captured answer)` of the run's final states (no outgoing
    /// transition), in declared order: a completed run's result.
    pub answers: Vec<(String, String)>,
}

impl FactoryNotice {
    /// The milestone payload the TS-era `factory.progress` request carried
    /// (`run_id`, `kind`, `detail`, and `node` when set): the executor's
    /// tests pin this contract.
    #[must_use]
    pub fn payload(&self) -> Value {
        let mut payload = Map::new();
        payload.insert("run_id".into(), Value::from(self.run_id.clone()));
        payload.insert("kind".into(), Value::from(self.kind.clone()));
        payload.insert("detail".into(), Value::from(self.detail.clone()));
        if let Some(node) = &self.node {
            payload.insert("node".into(), Value::from(node.clone()));
        }
        Value::Object(payload)
    }

    /// The notification `<status>`: `completed | failed | interrupted |
    /// cancelled`, or `paused` for the pause milestones (escalate, budget,
    /// `max_transitions`, `max_children`), which wait on the model's
    /// `resume`.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match self.kind.as_str() {
            "finished" => "completed",
            "failed" => "failed",
            INTERRUPTED_MILESTONE => "interrupted",
            CANCELLED_MILESTONE => "cancelled",
            _ => "paused",
        }
    }

    /// The one-line `<summary>`.
    #[must_use]
    pub fn summary(&self) -> String {
        let reason = match self.kind.as_str() {
            "budget_exceeded" => " (run budget exceeded)",
            "max_transitions_exceeded" => " (max_transitions exceeded)",
            "max_children_exceeded" => " (max_children exceeded)",
            "cancelled" => " by the user",
            _ => "",
        };
        format!(
            "Factory run \"{}\" {}{reason}",
            self.label.trim(),
            self.status()
        )
    }

    /// The `<result>` body: the milestone detail (the resume call for a
    /// pause), a completed run's final answers, and where the full record
    /// is read.
    #[must_use]
    pub fn result(&self) -> String {
        let mut result = self.detail.trim().to_string();
        for (state, answer) in &self.answers {
            let answer = answer.trim();
            if answer.is_empty() {
                continue;
            }
            let excerpt: String = answer.chars().take(ANSWER_EXCERPT_CHARS).collect();
            let cut = if excerpt.len() < answer.len() {
                " …"
            } else {
                ""
            };
            let _ = write!(result, "\n\n{state}: {excerpt}{cut}");
        }
        let _ = write!(
            result,
            "\n\nFull status: await rlm.factory.status('{}')",
            self.run_id
        );
        result
    }

    /// The model-facing notification text.
    #[must_use]
    pub fn text(&self) -> String {
        let summary = self.summary();
        let result = self.result();
        task_notification(&TaskNotification {
            task_id: &self.run_id,
            task_type: FACTORY_RUN_TASK_TYPE,
            status: self.status(),
            summary: &summary,
            tool_use_id: None,
            output_file: None,
            note: None,
            result: Some(&result),
            trailer: None,
        })
    }

    /// The durable custom row the parent session admits; its details carry
    /// what the lane's withdrawal matches on.
    #[must_use]
    pub fn message(&self, timestamp: u64) -> pa_types::session::CustomMessage {
        pa_types::session::CustomMessage {
            custom_type: FACTORY_RUN_NOTICE_CUSTOM_TYPE.to_string(),
            content: pa_types::ai::UserContent::Text(self.text()),
            display: true,
            details: Some(json!({
                "taskId": self.run_id,
                "taskType": FACTORY_RUN_TASK_TYPE,
                "status": self.status(),
                "summary": self.summary(),
                "milestone": self.kind,
                "runState": self.run_state,
            })),
            timestamp,
            rest: Map::default(),
        }
    }
}

/// Whether one queued custom row is a factory run notice for `run_id` whose
/// run state was `run_state` (the lane's withdrawal predicate).
#[must_use]
pub fn is_notice_for(row: &Value, run_id: &str, run_state: &str) -> bool {
    if row.get("customType").and_then(Value::as_str) != Some(FACTORY_RUN_NOTICE_CUSTOM_TYPE) {
        return false;
    }
    let details = row.get("details").unwrap_or(&Value::Null);
    details.get("taskId").and_then(Value::as_str) == Some(run_id)
        && details.get("runState").and_then(Value::as_str) == Some(run_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(kind: &str, run_state: &str) -> FactoryNotice {
        FactoryNotice {
            run_id: "run1".into(),
            kind: kind.into(),
            detail: "run complete: 2 state(s), 1 transition(s) fired".into(),
            node: None,
            label: "builder".into(),
            run_state: run_state.into(),
            answers: vec![("review".into(), "approved".into())],
        }
    }

    #[test]
    fn a_completed_run_renders_the_task_notification_shape() {
        let notice = notice("finished", "done");
        assert_eq!(
            notice.text(),
            "<task-notification>\n\
             <task-id>run1</task-id>\n\
             <task-type>factory_run</task-type>\n\
             <status>completed</status>\n\
             <summary>Factory run \"builder\" completed</summary>\n\
             <result>run complete: 2 state(s), 1 transition(s) fired\n\n\
             review: approved\n\n\
             Full status: await rlm.factory.status('run1')</result>\n\
             </task-notification>"
        );
        let message = notice.message(7);
        assert_eq!(message.custom_type, "factory_run_notice");
        assert!(message.display);
        assert_eq!(message.timestamp, 7);
        assert_eq!(
            message.details,
            Some(json!({
                "taskId": "run1",
                "taskType": "factory_run",
                "status": "completed",
                "summary": "Factory run \"builder\" completed",
                "milestone": "finished",
                "runState": "done",
            }))
        );
        // The row is model context, not a display-only record.
        let converted = crate::session_engine::messages::convert_to_llm(&[
            pa_types::session::AgentMessage::Custom(message.clone()),
        ]);
        assert!(
            matches!(&converted[..], [pa_types::session::AgentMessage::User(user)]
                if user.content.text() == notice.text()),
            "{converted:?}"
        );
        let row = serde_json::to_value(&message).unwrap();
        assert!(is_notice_for(&row, "run1", "done"));
        assert!(!is_notice_for(&row, "run1", "paused"));
        assert!(!is_notice_for(&row, "run2", "done"));
    }

    #[test]
    fn milestone_kinds_map_to_notification_statuses() {
        let cases = [
            ("finished", "completed", "Factory run \"builder\" completed"),
            ("failed", "failed", "Factory run \"builder\" failed"),
            (
                "interrupted",
                "interrupted",
                "Factory run \"builder\" interrupted",
            ),
            (
                "cancelled",
                "cancelled",
                "Factory run \"builder\" cancelled by the user",
            ),
            ("paused", "paused", "Factory run \"builder\" paused"),
            (
                "budget_exceeded",
                "paused",
                "Factory run \"builder\" paused (run budget exceeded)",
            ),
        ];
        for (kind, status, summary) in cases {
            let notice = notice(kind, "paused");
            assert_eq!(notice.status(), status, "{kind}");
            assert_eq!(notice.summary(), summary, "{kind}");
        }
    }

    #[test]
    fn long_answers_are_excerpted() {
        let mut notice = notice("finished", "done");
        notice.answers = vec![("review".into(), "x".repeat(5_000))];
        let result = notice.result();
        assert!(result.contains(&format!("review: {} …", "x".repeat(ANSWER_EXCERPT_CHARS))));
        assert!(notice.text().len() < 3_000);
    }
}
