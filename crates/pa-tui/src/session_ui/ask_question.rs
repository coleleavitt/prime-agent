//! The multiple-choice question panel (`rlm.ask_user`, Claude Code's
//! AskUserQuestion): the model's 1-4 questions published on the session's
//! confirmation surface under the status's `questions` key. The panel steps
//! through the questions, each with its header chip, 2-4 options (label,
//! description, and a focused option's multiline preview), a `multiSelect`
//! toggle, and an automatic free-text "Other" row. The built answer object
//! (`{answers: {question: label | [labels]}, response?}`) is sent back as
//! `answer_feature_confirmation`'s `answer`, and escape declines the whole
//! request.

use crossterm::event::{KeyCode, KeyEvent};
use serde_json::{Map, Value, json};

use super::{AgentView, DaemonCommand, SessionUi};
use crate::Line;
use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{hint_row, key_hint, menu_row};
use crate::theme::{Theme, ThemeColor};
use crate::width::truncate_line;

/// The free-text "Other" row's label.
const OTHER_LABEL: &str = "Other (type your own answer)";

/// The published question's `kind` for `rlm.ask_user` (mirrors
/// `pa_core::features::QUESTION_KIND_ASK_USER`; pa-tui does not link the
/// engine, so the wire value is kept here too).
const ASK_USER_KIND: &str = "ask_user";

/// What a key press did to the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AskAction {
    /// Still collecting answers.
    None,
    /// Escape or ctrl+c: the whole request is declined.
    Cancel,
    /// Every question answered: the answer object for the kernel.
    Submit(Value),
}

/// One option the model offered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelOption {
    label: String,
    description: String,
    preview: Option<String>,
}

/// One question the panel walks the user through.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelQuestion {
    question: String,
    header: String,
    options: Vec<PanelOption>,
    multi_select: bool,
    /// Cursor over the rows: `0..options.len()` options, then the Other row.
    cursor: usize,
    /// Which options are picked (parallel to `options`).
    picked: Vec<bool>,
    /// The free-text "Other" answer, used when non-empty.
    other: String,
    /// Typing into [`Self::other`] (the cursor is on the Other row and the
    /// user began typing or picked it).
    editing_other: bool,
}

impl PanelQuestion {
    /// The Other row's index (one past the last option).
    fn other_row(&self) -> usize {
        self.options.len()
    }

    /// Whether the cursor is on the Other row.
    fn on_other(&self) -> bool {
        self.cursor == self.other_row()
    }

    /// This question's answer, if the user chose anything: a single label
    /// (or the Other text), or a multi-select list of labels (plus the
    /// Other text when given).
    fn answer(&self) -> Option<Value> {
        let mut labels: Vec<String> = self
            .options
            .iter()
            .zip(&self.picked)
            .filter_map(|(option, picked)| picked.then(|| option.label.clone()))
            .collect();
        let other = self.other.trim();
        if self.multi_select {
            if !other.is_empty() {
                labels.push(other.to_string());
            }
            if labels.is_empty() {
                None
            } else {
                Some(Value::Array(
                    labels.into_iter().map(Value::String).collect(),
                ))
            }
        } else if let Some(label) = labels.into_iter().next() {
            Some(Value::String(label))
        } else if other.is_empty() {
            None
        } else {
            Some(Value::String(other.to_string()))
        }
    }
}

/// The pending question panel. Owns the editor dock while open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AskQuestionPanel {
    /// The pending item's id (the surface answers it by this id).
    pub(crate) id: String,
    questions: Vec<PanelQuestion>,
    current: usize,
}

impl AskQuestionPanel {
    /// Build the panel from a published `questions` entry's payload (the
    /// `ask_user` kind): `None` when it has no questions. `id` is the
    /// pending item's id.
    pub(crate) fn from_payload(id: &str, payload: &Value) -> Option<Self> {
        let raw = payload.get("questions").and_then(Value::as_array)?;
        let questions: Vec<PanelQuestion> = raw
            .iter()
            .filter_map(|question| {
                let text = question.get("question").and_then(Value::as_str)?;
                let header = question
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let options: Vec<PanelOption> = question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        let label = option.get("label").and_then(Value::as_str)?;
                        Some(PanelOption {
                            label: label.to_string(),
                            description: option
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            preview: option
                                .get("preview")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    })
                    .collect();
                let multi_select = question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let picked = vec![false; options.len()];
                Some(PanelQuestion {
                    question: text.to_string(),
                    header: header.to_string(),
                    options,
                    multi_select,
                    cursor: 0,
                    picked,
                    other: String::new(),
                    editing_other: false,
                })
            })
            .collect();
        if questions.is_empty() {
            return None;
        }
        Some(AskQuestionPanel {
            id: id.to_string(),
            questions,
            current: 0,
        })
    }

    fn question(&self) -> &PanelQuestion {
        &self.questions[self.current]
    }

    fn question_mut(&mut self) -> &mut PanelQuestion {
        &mut self.questions[self.current]
    }

    /// Build the answer object and advance; submit after the last question.
    fn advance(&mut self) -> AskAction {
        if self.current + 1 < self.questions.len() {
            self.current += 1;
            return AskAction::None;
        }
        let mut answers = Map::new();
        for question in &self.questions {
            if let Some(answer) = question.answer() {
                answers.insert(question.question.clone(), answer);
            }
        }
        let mut object = Map::new();
        if !answers.is_empty() {
            object.insert("answers".to_string(), Value::Object(answers));
        }
        AskAction::Submit(Value::Object(object))
    }

    /// One key press while the panel owns the frame.
    pub(crate) fn handle_key(&mut self, kb: &KeybindingsManager, key: &KeyEvent) -> AskAction {
        // Text editing on the Other row takes printable keys and backspace
        // first, so typing an option's letter never moves the cursor.
        if self.question().editing_other {
            match key.code {
                KeyCode::Char(ch)
                    if !key.modifiers.intersects(
                        crossterm::event::KeyModifiers::CONTROL
                            | crossterm::event::KeyModifiers::ALT
                            | crossterm::event::KeyModifiers::SUPER,
                    ) =>
                {
                    self.question_mut().other.push(ch);
                    return AskAction::None;
                }
                KeyCode::Backspace => {
                    self.question_mut().other.pop();
                    return AskAction::None;
                }
                _ => {}
            }
        }
        let id = crate::keys::key_event_to_id(key);
        let Some(id) = id else {
            return AskAction::None;
        };
        if id == "ctrl+c" || kb.matches(&id, "tui.select.cancel") {
            return AskAction::Cancel;
        }
        if kb.matches(&id, "tui.select.up") {
            let question = self.question_mut();
            question.editing_other = false;
            question.cursor = question.cursor.saturating_sub(1);
            return AskAction::None;
        }
        if kb.matches(&id, "tui.select.down") {
            let question = self.question_mut();
            question.editing_other = false;
            let last = question.other_row();
            question.cursor = (question.cursor + 1).min(last);
            return AskAction::None;
        }
        // Space toggles a multi-select option (or begins the Other text).
        if id == "space" && self.question().multi_select {
            let on_other = self.question().on_other();
            let cursor = self.question().cursor;
            let question = self.question_mut();
            if on_other {
                question.editing_other = true;
            } else {
                question.picked[cursor] = !question.picked[cursor];
            }
            return AskAction::None;
        }
        if kb.matches(&id, "tui.select.confirm") {
            let on_other = self.question().on_other();
            let multi = self.question().multi_select;
            let cursor = self.question().cursor;
            if on_other && !self.question().editing_other {
                // First Enter on the Other row opens its text field; the
                // next Enter accepts what was typed.
                self.question_mut().editing_other = true;
                return AskAction::None;
            }
            if !multi && !on_other {
                // A single-select pick is exclusive.
                let question = self.question_mut();
                for picked in &mut question.picked {
                    *picked = false;
                }
                question.picked[cursor] = true;
            }
            self.question_mut().editing_other = false;
            return self.advance();
        }
        AskAction::None
    }

    /// The pane's rendered rows: the step counter, the current question's
    /// header chip and text, its options (the focused one's preview below),
    /// the Other row, and the key hint.
    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let question = self.question();
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Vec::new());
        let counter = if self.questions.len() > 1 {
            format!("  Question {}/{}", self.current + 1, self.questions.len())
        } else {
            "  Question".to_string()
        };
        let mut title_spans = vec![crate::Span::raw(counter)];
        if !question.header.trim().is_empty() {
            title_spans.push(crate::Span::raw("  "));
            title_spans.push(theme.fg_span(ThemeColor::Accent, format!("[{}]", question.header)));
        }
        lines.push(title_spans);
        lines.push(truncate_line(
            &vec![crate::Span::raw(format!("  {}", question.question))],
            width,
            "",
        ));
        lines.push(Vec::new());
        for (index, option) in question.options.iter().enumerate() {
            let marker = if question.multi_select {
                if question.picked[index] {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let mut row = vec![crate::Span::raw(format!("{marker}{}", option.label))];
            if !option.description.is_empty() {
                row.push(theme.fg_span(ThemeColor::Muted, format!("  — {}", option.description)));
            }
            lines.push(menu_row(theme, width, row, &[], index == question.cursor));
        }
        let other_row = {
            let shown = if question.other.is_empty() {
                OTHER_LABEL.to_string()
            } else {
                format!("Other: {}", question.other)
            };
            let caret = if question.editing_other {
                "\u{2502}"
            } else {
                ""
            };
            vec![crate::Span::raw(format!("{shown}{caret}"))]
        };
        lines.push(menu_row(theme, width, other_row, &[], question.on_other()));
        if let Some(preview) = question
            .options
            .get(question.cursor)
            .and_then(|option| option.preview.as_deref())
        {
            lines.push(Vec::new());
            for line in preview.lines() {
                lines.push(truncate_line(
                    &vec![theme.fg_span(ThemeColor::Muted, format!("  {line}"))],
                    width,
                    "",
                ));
            }
        }
        lines.push(Vec::new());
        lines.push(hint_row(theme, width, &hint(kb, question.multi_select)));
        lines
    }
}

/// The `ask_user` questions a `confirmation` status carries (its
/// `questions` key), oldest first, as `(id, payload)`.
pub(super) fn ask_questions_of(status: &Value) -> Vec<(String, Value)> {
    status
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|question| question.get("kind").and_then(Value::as_str) == Some(ASK_USER_KIND))
        .filter_map(|question| {
            let id = question
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())?;
            let payload = question
                .get("payload")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Some((id.to_string(), payload))
        })
        .collect()
}

/// The `ask_user` questions a roster summary carries.
pub(super) fn ask_questions_from_summary(summary: &Value) -> Vec<(String, Value)> {
    summary
        .get("featureStatus")
        .and_then(|statuses| statuses.get(super::feature_confirm::CONFIRMATION_FEATURE))
        .and_then(|entry| entry.get("status"))
        .map(ask_questions_of)
        .unwrap_or_default()
}

impl SessionUi {
    /// Adopt the session's pending ask-user questions: forget answers no
    /// longer listed, drop the ones this client answered, close an open
    /// panel no longer pending, and open the oldest otherwise.
    pub(super) fn apply_ask_questions(
        &mut self,
        mut questions: Vec<(String, Value)>,
        view: &mut AgentView,
    ) {
        self.answered_ask_questions
            .retain(|id| questions.iter().any(|(question_id, _)| question_id == id));
        questions.retain(|(id, _)| !self.answered_ask_questions.contains(id));
        self.ask_questions = questions;
        if let Some(panel) = view.ask_question.as_ref() {
            if !self.ask_questions.iter().any(|(id, _)| id == &panel.id) {
                view.ask_question = None;
                self.dirty = true;
            }
        }
        self.open_next_ask_question(view);
    }

    /// Open the oldest pending ask-user question, unless a confirm or
    /// question panel is already open.
    pub(super) fn open_next_ask_question(&mut self, view: &mut AgentView) {
        if view.ask_question.is_some() || view.confirm.is_some() || self.pending_confirm.is_some() {
            return;
        }
        let Some((id, payload)) = self.ask_questions.first() else {
            return;
        };
        if let Some(panel) = AskQuestionPanel::from_payload(id, payload) {
            view.ask_question = Some(panel);
            self.dirty = true;
        }
    }

    /// Send the user's answer object to question `id` (`None` declines it).
    /// The daemon's next status confirms it left the pending list.
    pub(super) fn answer_ask_question(&mut self, id: String, answer: Option<Value>) {
        self.ask_questions
            .retain(|(question_id, _)| question_id != &id);
        self.answered_ask_questions.insert(id.clone());
        let client = self.client.clone();
        let allow = answer.is_some();
        let mut rest = Map::default();
        if let Some(answer) = answer {
            rest.insert("answer".to_string(), answer);
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

    /// One key press while the ask-user panel owns the frame.
    pub(super) fn handle_ask_question_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> anyhow::Result<()> {
        let action = {
            let Some(panel) = view.ask_question.as_mut() else {
                return Ok(());
            };
            let kb = view.editor.keybindings();
            panel.handle_key(kb, &key)
        };
        match action {
            AskAction::None => {}
            AskAction::Cancel => {
                if let Some(panel) = view.ask_question.take() {
                    self.answer_ask_question(panel.id, None);
                }
            }
            AskAction::Submit(answer) => {
                if let Some(panel) = view.ask_question.take() {
                    self.answer_ask_question(panel.id, Some(answer));
                }
            }
        }
        self.open_next_ask_question(view);
        self.dirty = true;
        Ok(())
    }
}

/// The pane's key hint.
fn hint(kb: &KeybindingsManager, multi_select: bool) -> String {
    let mut hints = vec![key_hint(
        kb,
        &["tui.select.up", "tui.select.down"],
        "navigate",
    )];
    if multi_select {
        hints.push(Some("Space toggle".to_string()));
    }
    hints.push(key_hint(kb, &["tui.select.confirm"], "select"));
    hints.push(key_hint(kb, &["tui.select.cancel"], "decline"));
    hints
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join(" \u{b7} ")
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use super::*;

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn two_questions() -> Value {
        json!({ "questions": [
            {
                "question": "Which library?",
                "header": "Library",
                "options": [
                    { "label": "date-fns", "description": "Modular" },
                    { "label": "Luxon", "description": "Time zones", "preview": "DateTime.now()" },
                ],
                "multiSelect": false,
            },
            {
                "question": "Which features?",
                "header": "Features",
                "options": [
                    { "label": "Search", "description": "" },
                    { "label": "Export", "description": "" },
                ],
                "multiSelect": true,
            },
        ]})
    }

    #[test]
    fn empty_payload_builds_no_panel() {
        assert!(AskQuestionPanel::from_payload("q-1", &json!({ "questions": [] })).is_none());
        assert!(AskQuestionPanel::from_payload("q-1", &json!({})).is_none());
    }

    #[test]
    fn single_then_multi_select_builds_the_answer_object() {
        let mut panel = AskQuestionPanel::from_payload("q-1", &two_questions()).expect("panel");
        // First question (single-select): move to Luxon and pick it.
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Down)),
            AskAction::None
        );
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Enter)),
            AskAction::None
        );
        // Second question (multi-select): toggle Search, move to Export,
        // toggle it, then confirm.
        assert_eq!(panel.current, 1);
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Char(' '))),
            AskAction::None
        );
        panel.handle_key(&kb(), &press(KeyCode::Down));
        panel.handle_key(&kb(), &press(KeyCode::Char(' ')));
        let action = panel.handle_key(&kb(), &press(KeyCode::Enter));
        assert_eq!(
            action,
            AskAction::Submit(json!({ "answers": {
                "Which library?": "Luxon",
                "Which features?": ["Search", "Export"],
            } }))
        );
    }

    #[test]
    fn the_other_row_captures_free_text() {
        let mut panel = AskQuestionPanel::from_payload("q-1", &two_questions()).expect("panel");
        // Move to the Other row (past both options), open it, type.
        panel.handle_key(&kb(), &press(KeyCode::Down));
        panel.handle_key(&kb(), &press(KeyCode::Down));
        assert!(panel.question().on_other());
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Enter)),
            AskAction::None
        );
        assert!(panel.question().editing_other);
        for ch in "Temporal".chars() {
            panel.handle_key(&kb(), &press(KeyCode::Char(ch)));
        }
        panel.handle_key(&kb(), &press(KeyCode::Backspace));
        // Enter accepts the typed answer and advances to the second question.
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Enter)),
            AskAction::None
        );
        assert_eq!(panel.current, 1);
        // Skip the second question unanswered and submit.
        let action = panel.handle_key(&kb(), &press(KeyCode::Enter));
        assert_eq!(
            action,
            AskAction::Submit(json!({ "answers": { "Which library?": "Tempora" } }))
        );
    }

    #[test]
    fn escape_declines_the_request() {
        let mut panel = AskQuestionPanel::from_payload("q-1", &two_questions()).expect("panel");
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Esc)),
            AskAction::Cancel
        );
    }

    #[test]
    fn an_unanswered_run_submits_an_empty_object() {
        let mut panel = AskQuestionPanel::from_payload("q-1", &two_questions()).expect("panel");
        // Advance both questions without choosing anything.
        assert_eq!(
            panel.handle_key(&kb(), &press(KeyCode::Tab)),
            AskAction::None
        );
        // Tab is not bound to confirm; use the Other-less advance via Enter
        // on a fresh single-select with nothing picked is not allowed, so
        // move the cursor off any option is unnecessary: Enter on an option
        // picks it. Instead walk to Other and Enter twice with no text.
        panel.handle_key(&kb(), &press(KeyCode::Down));
        panel.handle_key(&kb(), &press(KeyCode::Down));
        panel.handle_key(&kb(), &press(KeyCode::Enter));
        panel.handle_key(&kb(), &press(KeyCode::Enter));
        assert_eq!(panel.current, 1);
        panel.handle_key(&kb(), &press(KeyCode::Down));
        panel.handle_key(&kb(), &press(KeyCode::Down));
        panel.handle_key(&kb(), &press(KeyCode::Enter));
        let action = panel.handle_key(&kb(), &press(KeyCode::Enter));
        assert_eq!(action, AskAction::Submit(json!({})));
    }

    #[test]
    fn it_renders_the_header_options_and_preview() {
        let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
        let mut panel = AskQuestionPanel::from_payload("q-1", &two_questions()).expect("panel");
        // Focus Luxon so its preview shows.
        panel.handle_key(&kb(), &press(KeyCode::Down));
        let text: Vec<String> = panel
            .render(&theme, 80, &kb())
            .iter()
            .map(|line| line.iter().map(|span| span.content.clone()).collect())
            .collect();
        assert!(text.iter().any(|row| row.contains("[Library]")));
        assert!(text.iter().any(|row| row.contains("Which library?")));
        assert!(text.iter().any(|row| row.contains("date-fns")));
        assert!(text.iter().any(|row| row.contains("DateTime.now()")));
        assert!(text.iter().any(|row| row.contains(OTHER_LABEL)));
    }
}
