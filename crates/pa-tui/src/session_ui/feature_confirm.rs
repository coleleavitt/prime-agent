//! The questions a session's hooks and plugins put to the user before an
//! action (their `ask` answer, published as the session's `confirmation`
//! feature status): the oldest pending one opens the yes/no confirm panel,
//! defaulting to No so a keystroke meant for the editor never allows an
//! action, and the answer goes back as `answer_feature_confirmation`. A
//! question answered elsewhere (another client, the daemon's timeout, an
//! aborted run) closes its panel. A question that offers to remember its
//! answer (`remember`, a permission rule) gets a third choice, "Yes, and
//! don't ask again for <rule>", answered with `remember: true`.

use serde_json::Value;

use super::{AgentView, DaemonCommand, Map, PendingConfirm, SessionUi};

/// The feature status the pending questions publish under
/// (`pa_core::features::CONFIRMATION_FEATURE`).
pub(super) const CONFIRMATION_FEATURE: &str = "confirmation";

/// The widest line the panel shows (it truncates, never wraps).
const PANEL_WIDTH: usize = 72;

/// The most lines of an action the panel shows.
const MAX_ACTION_LINES: usize = 6;

/// One pending question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FeatureQuestion {
    pub(super) id: String,
    pub(super) action: String,
    pub(super) reason: String,
    /// What allowing it for good remembers; `None` offers yes or no only.
    pub(super) remember: Option<String>,
}

/// The panel's plain allow.
const ALLOW_CHOICE: &str = "Yes";

/// The panel's refusal.
const DENY_CHOICE: &str = "No";

/// The panel's allow-and-remember choice for `rule`.
fn remember_choice(rule: &str) -> String {
    format!("Yes, and don't ask again for {rule}")
}

/// How a panel choice answers a question: `(allow, remember)`.
pub(super) fn answer_of_choice(choice: &str) -> (bool, bool) {
    match choice {
        ALLOW_CHOICE => (true, false),
        DENY_CHOICE => (false, false),
        other => {
            let remember = other.starts_with("Yes, and don't ask again");
            (remember, remember)
        }
    }
}

/// The pending questions a `confirmation` status carries, oldest first.
pub(super) fn questions_of(status: &Value) -> Vec<FeatureQuestion> {
    status
        .get("confirmations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let text = |key: &str| {
                question
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            Some(FeatureQuestion {
                id: text("id").filter(|id| !id.is_empty())?,
                action: text("action").unwrap_or_default(),
                reason: text("reason").unwrap_or_default(),
                remember: text("remember").filter(|rule| !rule.trim().is_empty()),
            })
        })
        .collect()
}

/// The pending questions a roster summary carries
/// (`featureStatus.confirmation.status`).
pub(super) fn questions_from_summary(summary: &Value) -> Vec<FeatureQuestion> {
    summary
        .get("featureStatus")
        .and_then(|statuses| statuses.get(CONFIRMATION_FEATURE))
        .and_then(|entry| entry.get("status"))
        .map(questions_of)
        .unwrap_or_default()
}

/// `text` hand-wrapped at [`PANEL_WIDTH`] characters, at most `max_lines`
/// lines, the last marked when cut.
fn wrap(text: &str, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for source in text.lines() {
        let chars: Vec<char> = source.chars().collect();
        if chars.is_empty() {
            continue;
        }
        for chunk in chars.chunks(PANEL_WIDTH) {
            lines.push(chunk.iter().collect::<String>());
        }
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push_str("...");
        }
    }
    lines
}

/// The panel's message for `question`.
fn message(question: &FeatureQuestion) -> String {
    let mut lines = wrap(&question.action, MAX_ACTION_LINES);
    lines.push(String::new());
    lines.extend(wrap(&question.reason, 3));
    lines.join("\n")
}

impl SessionUi {
    /// Adopt the session's pending questions: close the panel of one no
    /// longer pending, and open the oldest when no other confirm is open. A
    /// question this client already answered stays answered while the
    /// daemon still lists it.
    pub(super) fn apply_feature_questions(
        &mut self,
        mut questions: Vec<FeatureQuestion>,
        view: &mut AgentView,
    ) {
        self.answered_questions
            .retain(|id| questions.iter().any(|question| &question.id == id));
        questions.retain(|question| !self.answered_questions.contains(&question.id));
        self.feature_questions = questions;
        if let Some(PendingConfirm::FeatureQuestion { id }) = &self.pending_confirm {
            if !self
                .feature_questions
                .iter()
                .any(|question| &question.id == id)
            {
                self.pending_confirm = None;
                view.confirm = None;
                self.dirty = true;
            }
        }
        self.open_next_feature_question(view);
    }

    /// Open the oldest pending question, unless a confirm is already open.
    pub(super) fn open_next_feature_question(&mut self, view: &mut AgentView) {
        if self.pending_confirm.is_some() || view.confirm.is_some() || view.ask_question.is_some() {
            return;
        }
        let Some(question) = self.feature_questions.first() else {
            return;
        };
        let mut choices = vec![ALLOW_CHOICE.to_string()];
        if let Some(rule) = &question.remember {
            choices.push(remember_choice(rule));
        }
        choices.push(DENY_CHOICE.to_string());
        view.confirm = Some(crate::confirm::ConfirmPanel::choices_defaulting_to_last(
            "Allow this action?",
            &message(question),
            choices,
        ));
        self.pending_confirm = Some(PendingConfirm::FeatureQuestion {
            id: question.id.clone(),
        });
        self.dirty = true;
    }

    /// Send the user's answer to question `id`. The daemon's next status
    /// confirms it left the pending list; a failed send (the connection
    /// dropped, which the chrome reports) leaves it to the daemon's
    /// timeout.
    pub(super) fn answer_feature_question(&mut self, id: String, allow: bool, remember: bool) {
        self.feature_questions.retain(|question| question.id != id);
        self.answered_questions.insert(id.clone());
        let client = self.client.clone();
        // `remember` is additive on the wire: a daemon that predates it
        // reads a plain allow.
        let mut rest = Map::default();
        if allow && remember {
            rest.insert("remember".to_string(), Value::Bool(true));
        }
        let command = DaemonCommand::AnswerFeatureConfirmation {
            id: None,
            active_session_id: self.active_session_id.clone(),
            confirmation_id: id,
            allow,
            rest,
        };
        tokio::spawn(async move {
            let _ = client.request_ok(command).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn questions_parse_from_a_status_and_a_summary() {
        let status = json!({ "confirmations": [
            { "id": "confirm-1", "action": "Run `rm -rf build`", "reason": "hook asks" },
            { "id": "", "action": "dropped: no id" },
            { "id": "confirm-2" },
            { "id": "confirm-3", "action": "Run `npm test`", "reason": "rule asks", "remember": "Bash(npm test *)" },
        ]});
        let expected = vec![
            FeatureQuestion {
                id: "confirm-1".to_string(),
                action: "Run `rm -rf build`".to_string(),
                reason: "hook asks".to_string(),
                remember: None,
            },
            FeatureQuestion {
                id: "confirm-2".to_string(),
                action: String::new(),
                reason: String::new(),
                remember: None,
            },
            FeatureQuestion {
                id: "confirm-3".to_string(),
                action: "Run `npm test`".to_string(),
                reason: "rule asks".to_string(),
                remember: Some("Bash(npm test *)".to_string()),
            },
        ];
        assert_eq!(questions_of(&status), expected);
        assert_eq!(
            questions_from_summary(&json!({
                "featureStatus": { "confirmation": { "line": "waiting", "status": status } }
            })),
            expected
        );
        assert!(questions_from_summary(&json!({})).is_empty());
    }

    #[test]
    fn a_long_action_wraps_and_is_cut() {
        let question = FeatureQuestion {
            id: "c".to_string(),
            action: "x".repeat(PANEL_WIDTH * 8),
            reason: "policy".to_string(),
            remember: None,
        };
        let lines: Vec<String> = message(&question).lines().map(str::to_string).collect();
        assert_eq!(lines.len(), MAX_ACTION_LINES + 2);
        assert!(
            lines[..MAX_ACTION_LINES - 1]
                .iter()
                .all(|line| line.chars().count() == PANEL_WIDTH)
        );
        assert!(lines[MAX_ACTION_LINES - 1].ends_with("..."));
        assert_eq!(lines[MAX_ACTION_LINES], "");
        assert_eq!(lines[MAX_ACTION_LINES + 1], "policy");
    }

    #[test]
    fn panel_choices_answer_allow_remember_or_deny() {
        assert_eq!(answer_of_choice(ALLOW_CHOICE), (true, false));
        assert_eq!(answer_of_choice(DENY_CHOICE), (false, false));
        assert_eq!(
            answer_of_choice(&remember_choice("Bash(npm test *)")),
            (true, true)
        );
        assert_eq!(answer_of_choice("something else"), (false, false));
    }
}
