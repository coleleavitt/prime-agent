//! `rlm.ask_user`: the model asks the user multiple-choice questions,
//! Claude Code's `AskUserQuestion` (2.1.309 tool `Wee` in `chunk-dfmm12cq`).
//!
//! The kernel sends `user.ask` with Claude Code's input shape (1-4
//! questions, each with a `header` chip, 2-4 `options` of `label`,
//! `description` and an optional `preview`, and `multiSelect`); the host
//! validates it with Claude Code's rules and messages, puts it on the
//! session's confirmation surface as an [`QUESTION_KIND_ASK_USER`] question
//! (the TUI's question panel, which adds the free-text "Other" answer),
//! admits the client's answer with Claude Code's limits, and answers the
//! kernel with the answers and the text Claude Code's tool result carries.
//!
//! Like Claude Code's tool, it is not available without an interactive
//! user (print mode has no surface; CC disables the tool headless) or in a
//! subagent. A dismissed question raises in the kernel with Claude Code's
//! rejection text; the `askUserQuestionTimeout` setting (Claude Code's key,
//! `never` by default) answers with its away-from-keyboard text instead.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::features::{QUESTION_KIND_ASK_USER, QuestionAnswer, ask_question};
use crate::kernel::shared::{HostRequestHandlers, host_handler};

/// The host request `rlm.ask_user` sends.
pub const ASK_USER_REQUEST: &str = "user.ask";

/// Claude Code's bounds: questions per call, options per question.
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
/// Claude Code's answer limits (`chunk-dfmm12cq`): `P` caps one answer
/// string, one multi-select label and the free response; `te` caps one
/// note; `ne` caps the sum of answers and notes (`s`); `se = r0 - oe` caps
/// that sum plus the question keys and previews (`s + r`).
const MAX_FIELD_CHARS: usize = 32_768;
const MAX_NOTES_CHARS: usize = 8_192;
const MAX_ANSWERS_TOTAL_CHARS: usize = 32_768;
const MAX_COMBINED_CHARS: usize = 48_000;
/// The answer a question with notes but no selection records.
const NOTES_ONLY: &str = "(notes only)";

/// Claude Code's steer for a question with fewer than two options.
const TOO_FEW_OPTIONS: &str = "This call included a question with fewer than 2 options, so it was rejected and the person never saw it. A question with a single option has no decision in it. Do not retry this call and do not invent a filler second option. Instead, state the one path you were going to offer as the approach you are taking, then continue with the task. If this call also contained questions with 2 to 4 options (each with distinct labels), you may re-ask those questions alone in a new call. Ask a question only when the person has at least two genuinely distinct choices.";

/// Claude Code's uniqueness rule.
const NOT_UNIQUE: &str =
    "Question texts must be unique, option labels must be unique within each question";

/// What a dismissed question raises in the kernel (Claude Code's rejection).
pub const DECLINED_MESSAGE: &str = "The user doesn't want to proceed with these questions. STOP what you are doing and wait for the user to tell you how to proceed.";

/// Why the questions cannot be asked here: no interactive user.
pub const UNAVAILABLE_MESSAGE: &str = "rlm.ask_user is unavailable: no one can answer questions in this session (print mode, or no interactive client is attached). Proceed using your best judgment and state the assumptions you made in your reply.";

/// Why a subagent cannot ask.
pub const SUBAGENT_MESSAGE: &str = "rlm.ask_user is only available in the main session: a subagent cannot ask the user. Make a reasonable choice, or report what you need in your final answer.";

/// One option of a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

/// One validated question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

impl UserQuestion {
    fn to_json(&self) -> Value {
        let options: Vec<Value> = self
            .options
            .iter()
            .map(|option| {
                let mut value = json!({ "label": option.label, "description": option.description });
                if let Some(preview) = &option.preview {
                    value["preview"] = json!(preview);
                }
                value
            })
            .collect();
        json!({
            "question": self.question,
            "header": self.header,
            "options": options,
            "multiSelect": self.multi_select,
        })
    }
}

fn string_field(object: &Map<String, Value>, key: &str, at: &str) -> anyhow::Result<String> {
    match object.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => anyhow::bail!("{at}.{key} must be a string"),
        None => anyhow::bail!("{at}.{key} is required"),
    }
}

/// Validate `rlm.ask_user`'s questions with Claude Code's rules.
///
/// # Errors
///
/// The message the model reads: the field at fault, Claude Code's
/// fewer-than-two-options steer, or its uniqueness rule.
pub fn parse_questions(payload: &Value) -> anyhow::Result<Vec<UserQuestion>> {
    let Some(raw) = payload.get("questions").and_then(Value::as_array) else {
        anyhow::bail!("questions must be a list of 1-4 questions");
    };
    if raw.is_empty() || raw.len() > MAX_QUESTIONS {
        anyhow::bail!(
            "questions must hold 1-4 questions (got {}); split more into separate calls",
            raw.len()
        );
    }
    // The steer wins over field errors, like Claude Code's
    // `validationErrorSteer` over the schema's issues.
    if raw.iter().any(|question| {
        question
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|options| options.len() < MIN_OPTIONS)
    }) {
        anyhow::bail!("{TOO_FEW_OPTIONS}");
    }
    let mut questions = Vec::with_capacity(raw.len());
    for (index, question) in raw.iter().enumerate() {
        let at = format!("questions[{index}]");
        let Some(object) = question.as_object() else {
            anyhow::bail!("{at} must be an object");
        };
        let text = string_field(object, "question", &at)?;
        if text.trim().is_empty() {
            anyhow::bail!("{at}.question must not be empty");
        }
        let header = string_field(object, "header", &at)?;
        let Some(raw_options) = object.get("options").and_then(Value::as_array) else {
            anyhow::bail!("{at}.options must be a list of 2-4 options");
        };
        if raw_options.len() > MAX_OPTIONS {
            anyhow::bail!(
                "{at}.options must hold 2-4 options (got {}); there is no 'Other' option to add, it is provided automatically",
                raw_options.len()
            );
        }
        let mut options = Vec::with_capacity(raw_options.len());
        for (option_index, option) in raw_options.iter().enumerate() {
            let option_at = format!("{at}.options[{option_index}]");
            let Some(option) = option.as_object() else {
                anyhow::bail!("{option_at} must be an object");
            };
            let label = string_field(option, "label", &option_at)?;
            if label.trim().is_empty() {
                anyhow::bail!("{option_at}.label must not be empty");
            }
            let description = match option.get("description") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(text)) => text.clone(),
                Some(_) => anyhow::bail!("{option_at}.description must be a string"),
            };
            let preview = match option.get("preview") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => anyhow::bail!("{option_at}.preview must be a string"),
            };
            options.push(QuestionOption {
                label,
                description,
                preview,
            });
        }
        let multi_select = match object.get("multiSelect") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(multi)) => *multi,
            Some(_) => anyhow::bail!("{at}.multiSelect must be a boolean"),
        };
        questions.push(UserQuestion {
            question: text,
            header,
            options,
            multi_select,
        });
    }
    let mut texts: Vec<&str> = questions.iter().map(|q| q.question.as_str()).collect();
    texts.sort_unstable();
    texts.dedup();
    let labels_unique = questions.iter().all(|question| {
        let mut labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        labels.len() == question.options.len()
    });
    if texts.len() != questions.len() || !labels_unique {
        anyhow::bail!("{NOT_UNIQUE}");
    }
    Ok(questions)
}

/// One question's annotation from the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annotation {
    pub preview: Option<String>,
    pub notes: Option<String>,
}

/// The user's admitted answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserAnswer {
    /// Question text to answer (a multi-select's labels comma-joined).
    pub answers: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, Annotation>,
    /// Free text the user typed instead of answering.
    pub response: Option<String>,
}

/// Claude Code's multi-select join (`rMt`): labels holding `", "` or a
/// quote are JSON-quoted.
fn join_labels(labels: &[String]) -> String {
    labels
        .iter()
        .map(|label| {
            if label.contains(", ") || label.contains('"') {
                serde_json::to_string(label).unwrap_or_else(|_| label.clone())
            } else {
                label.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Claude Code's split of a joined multi-select answer (`D`); `None` when
/// it is not a well-formed join.
fn split_labels(joined: &str) -> Option<Vec<String>> {
    let mut labels = Vec::new();
    let mut rest = joined;
    loop {
        if rest.starts_with('"') {
            let bytes = rest.as_bytes();
            let mut escaped = false;
            let mut end = None;
            for (index, byte) in bytes.iter().enumerate().skip(1) {
                if escaped {
                    escaped = false;
                    continue;
                }
                match byte {
                    b'\\' => escaped = true,
                    b'"' => {
                        end = Some(index);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end?;
            labels.push(serde_json::from_str::<String>(&rest[..=end]).ok()?);
            rest = &rest[end + 1..];
        } else {
            let (label, remainder) = match rest.find(", ") {
                Some(index) => (&rest[..index], &rest[index..]),
                None => (rest, ""),
            };
            if label.contains('"') {
                return None;
            }
            labels.push(label.to_string());
            rest = remainder;
        }
        if rest.is_empty() {
            return Some(labels);
        }
        rest = rest.strip_prefix(", ")?;
        if rest.is_empty() {
            return None;
        }
    }
}

/// Admit the client's answer object with Claude Code's limits
/// (`admitCardAnswer`/`ue`): answers and annotations name asked questions, an
/// array answers only a multi-select, and the sizes are bounded (`s` is the
/// answers and notes, `r` the question keys and previews).
///
/// # Errors
///
/// The rule the answer broke.
pub fn admit_answer(questions: &[UserQuestion], answer: &Value) -> anyhow::Result<UserAnswer> {
    let Some(answer) = answer.as_object() else {
        anyhow::bail!("the answer must be an object");
    };
    let find = |text: &str| questions.iter().find(|question| question.question == text);
    let mut admitted = UserAnswer::default();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let mut answers_chars = 0usize;
    let mut keys_chars = 0usize;
    if let Some(answers) = answer.get("answers").filter(|value| !value.is_null()) {
        let Some(answers) = answers.as_object() else {
            anyhow::bail!("answers must be an object");
        };
        for (text, value) in answers {
            let Some(question) = find(text) else {
                anyhow::bail!("answers names a question that was not asked");
            };
            touched.insert(text.clone());
            let joined = match value {
                Value::String(text) => {
                    if text.len() > MAX_FIELD_CHARS {
                        anyhow::bail!("an answer is too long");
                    }
                    text.clone()
                }
                Value::Array(labels) if question.multi_select => {
                    if labels.len() > question.options.len() + 1 {
                        anyhow::bail!("too many answers for one question");
                    }
                    let labels = labels
                        .iter()
                        .map(|label| label.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| anyhow::anyhow!("a multi-select answer must be strings"))?;
                    if labels.iter().any(|label| label.len() > MAX_FIELD_CHARS) {
                        anyhow::bail!("an answer is too long");
                    }
                    join_labels(&labels)
                }
                _ => anyhow::bail!("an answer must be a string (a list for a multi-select)"),
            };
            answers_chars += joined.len();
            admitted.answers.insert(text.clone(), joined);
        }
    }
    if let Some(annotations) = answer.get("annotations").filter(|value| !value.is_null()) {
        let Some(annotations) = annotations.as_object() else {
            anyhow::bail!("annotations must be an object");
        };
        for (text, value) in annotations {
            let Some(question) = find(text) else {
                anyhow::bail!("annotations names a question that was not asked");
            };
            touched.insert(text.clone());
            let Some(value) = value.as_object() else {
                anyhow::bail!("an annotation must be an object");
            };
            let mut annotation = Annotation::default();
            for (key, field) in value {
                let Some(field) = field.as_str() else {
                    anyhow::bail!("annotation fields must be strings");
                };
                match key.as_str() {
                    "preview" => {
                        if !question
                            .options
                            .iter()
                            .any(|option| option.preview.as_deref() == Some(field))
                        {
                            anyhow::bail!(
                                "an annotation's preview must be one the question showed"
                            );
                        }
                        keys_chars += field.len();
                        annotation.preview = Some(field.to_string());
                    }
                    "notes" => {
                        if field.len() > MAX_NOTES_CHARS {
                            anyhow::bail!("a note is too long");
                        }
                        answers_chars += field.len();
                        annotation.notes = Some(field.to_string());
                    }
                    _ => anyhow::bail!("unknown annotation field {key}"),
                }
            }
            admitted.annotations.insert(text.clone(), annotation);
        }
    }
    match answer.get("response") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => {
            if text.len() > MAX_FIELD_CHARS {
                anyhow::bail!("the response is too long");
            }
            answers_chars += text.len();
            admitted.response = Some(text.clone());
        }
        Some(_) => anyhow::bail!("response must be a string"),
    }
    keys_chars += touched.iter().map(String::len).sum::<usize>();
    if answers_chars > MAX_ANSWERS_TOTAL_CHARS || answers_chars + keys_chars > MAX_COMBINED_CHARS {
        anyhow::bail!("the answer is too large");
    }
    Ok(admitted)
}

/// Claude Code's away-from-keyboard text (`W`).
#[must_use]
pub fn timed_out_message(timeout: Duration) -> String {
    format!(
        "No response after {}s \u{2014} the user may be away from keyboard. Proceed using your best judgment based on the context so far; you can re-ask this question later if it's still relevant.",
        timeout.as_secs_f64().round()
    )
}

/// Whether every answer is exactly an offered option (Claude Code's
/// "answered" versus "read the answers carefully" split).
fn answers_match_options(questions: &[UserQuestion], answer: &UserAnswer) -> bool {
    questions.iter().all(|question| {
        if answer
            .annotations
            .get(&question.question)
            .is_some_and(|annotation| annotation.notes.is_some())
        {
            return false;
        }
        let Some(given) = answer.answers.get(&question.question) else {
            return true;
        };
        if given.is_empty() || given == NOTES_ONLY {
            return true;
        }
        let offered = |label: &str| question.options.iter().any(|option| option.label == label);
        if offered(given) {
            return true;
        }
        if !question.multi_select {
            return false;
        }
        split_labels(given).is_some_and(|labels| {
            !labels.is_empty()
                && join_labels(&labels) == *given
                && labels.iter().all(|label| offered(label))
        })
    })
}

/// The text Claude Code's tool result carries for an answer
/// (`mapToolResultToToolResultBlockParam`).
#[must_use]
pub fn answer_message(questions: &[UserQuestion], answer: &UserAnswer) -> String {
    let answered: Vec<String> = questions
        .iter()
        .filter_map(|question| {
            let given = answer
                .answers
                .get(&question.question)
                .filter(|given| !given.is_empty() && given.as_str() != NOTES_ONLY);
            let annotation = answer.annotations.get(&question.question);
            let notes = annotation.and_then(|annotation| annotation.notes.as_deref());
            if given.is_none() && notes.is_none() {
                return None;
            }
            let mut parts = vec![match given {
                Some(given) => format!("\"{}\"=\"{given}\"", question.question),
                None => format!("\"{}\"=(no option selected)", question.question),
            }];
            if let Some(preview) = annotation.and_then(|annotation| annotation.preview.as_deref()) {
                parts.push(format!("selected preview:\n{preview}"));
            }
            if let Some(notes) = notes {
                parts.push(format!("notes: {notes}"));
            }
            Some(parts.join(" "))
        })
        .collect();
    let answered = answered.join(", ");
    if let Some(response) = answer
        .response
        .as_deref()
        .filter(|response| !response.trim().is_empty())
    {
        return format!("The user responded: {response}");
    }
    if answered.is_empty() {
        return "The user did not answer the questions.".to_string();
    }
    if answers_match_options(questions, answer) {
        format!(
            "Your questions have been answered: {answered}. You can now continue with these answers in mind."
        )
    } else {
        format!(
            "The user answered: {answered}. Read the answers carefully \u{2014} they may request clarification, changes, or that you not proceed \u{2014} and follow what they actually say."
        )
    }
}

/// Parse the `askUserQuestionTimeout` setting (Claude Code's values
/// `60s`, `5m`, `10m`, `never`, `default`; any `<n>s` or `<n>m`): `None`
/// waits for the user.
#[must_use]
pub fn parse_timeout(setting: Option<&str>) -> Option<Duration> {
    let setting = setting?.trim();
    let (number, unit) = if let Some(number) = setting.strip_suffix('s') {
        (number, 1)
    } else if let Some(number) = setting.strip_suffix('m') {
        (number, 60)
    } else {
        return None;
    };
    number
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .map(|value| Duration::from_secs(value * unit))
}

/// The status line while the questions wait.
fn waiting_line(questions: &[UserQuestion]) -> String {
    let headers: Vec<&str> = questions
        .iter()
        .map(|question| question.header.trim())
        .filter(|header| !header.is_empty())
        .collect();
    if headers.is_empty() {
        "waiting for your answer".to_string()
    } else {
        format!("waiting for your answer: {}", headers.join(", "))
    }
}

/// Ask `questions` on `session_id`'s surface and build the kernel's reply.
///
/// # Errors
///
/// No surface ([`UNAVAILABLE_MESSAGE`]), a dismissal
/// ([`DECLINED_MESSAGE`]), or an answer the admission refused.
pub async fn ask(
    session_id: &str,
    questions: &[UserQuestion],
    timeout: Option<Duration>,
) -> anyhow::Result<Value> {
    let payload = json!({
        "questions": questions.iter().map(UserQuestion::to_json).collect::<Vec<_>>(),
    });
    let answer = ask_question(
        session_id,
        QUESTION_KIND_ASK_USER,
        &waiting_line(questions),
        payload,
        timeout,
    )
    .await;
    match answer {
        QuestionAnswer::Answered(answer) => {
            let admitted = admit_answer(questions, &answer)
                .map_err(|error| anyhow::anyhow!("the client's answer was refused: {error}"))?;
            let mut reply = json!({
                "status": "answered",
                "answers": admitted.answers,
                "message": answer_message(questions, &admitted),
            });
            if !admitted.annotations.is_empty() {
                let annotations: Map<String, Value> = admitted
                    .annotations
                    .iter()
                    .map(|(question, annotation)| {
                        let mut value = Map::new();
                        if let Some(preview) = &annotation.preview {
                            value.insert("preview".to_string(), json!(preview));
                        }
                        if let Some(notes) = &annotation.notes {
                            value.insert("notes".to_string(), json!(notes));
                        }
                        (question.clone(), Value::Object(value))
                    })
                    .collect();
                reply["annotations"] = Value::Object(annotations);
            }
            if let Some(response) = admitted
                .response
                .filter(|response| !response.trim().is_empty())
            {
                reply["response"] = json!(response);
            }
            Ok(reply)
        }
        QuestionAnswer::TimedOut(timeout) => Ok(json!({
            "status": "timed_out",
            "answers": {},
            "message": timed_out_message(timeout),
        })),
        QuestionAnswer::Declined => anyhow::bail!("{DECLINED_MESSAGE}"),
        QuestionAnswer::Unavailable => anyhow::bail!("{UNAVAILABLE_MESSAGE}"),
    }
}

/// Register `user.ask` for a session at `rlm_depth`, waiting up to
/// `timeout` for the user.
pub(crate) fn register_host_handlers(
    handlers: &mut HostRequestHandlers,
    session_id: String,
    rlm_depth: u32,
    timeout: Option<Duration>,
) {
    handlers.register(
        ASK_USER_REQUEST,
        host_handler(move |payload| {
            let session_id = session_id.clone();
            Box::pin(async move {
                if rlm_depth > 0 {
                    anyhow::bail!("{SUBAGENT_MESSAGE}");
                }
                let questions = parse_questions(&payload.data)?;
                ask(&session_id, &questions, timeout).await
            })
        }),
    );
}

#[cfg(test)]
#[path = "ask_user_tests.rs"]
mod tests;
