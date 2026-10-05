//! The repetition guard (upstream #1798): a degenerate generation that
//! settles into emitting the same unit back to back ("the the the ...", one
//! line over and over) never stops on its own and streams to the output
//! cap. The guard watches each streamed text and thinking block and reports
//! a loop once the block's tail is one short unit repeated far past what
//! real output does.
//!
//! Detection is the periodic-tail test: the smallest period of every
//! suffix of the block's tail window, from the KMP failure function over
//! the reversed window (O(window) per check). A loop is a suffix whose
//! smallest period `p` is at most [`RepetitionGuardConfig::max_period_chars`]
//! and that covers at least `max(p * min_repeats, min_span_chars)` chars.
//! The span floor keeps the guard conservative: a 4-char unit must repeat
//! 500 times, a 100-char line 20 times, before the guard fires, so code
//! with legitimately repeated lines (closing braces, a few identical
//! asserts, table rows) never trips it.

use crate::types::{AssistantContent, AssistantMessage};

/// The raw stop reason (`stopReasonRaw`) of a response the guard ended;
/// its `stopReason` is `error`, with [`RepetitionLoop::describe`] as the
/// error message.
pub const REPETITION_STOP_REASON: &str = "repetition_loop";

/// The guard's thresholds. [`Default`] is the conservative product setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepetitionGuardConfig {
    /// The longest repeating unit the guard looks for, in chars.
    pub max_period_chars: usize,
    /// The fewest back-to-back repeats of a unit that count as a loop.
    pub min_repeats: usize,
    /// The shortest looping span (all repeats together), in chars.
    pub min_span_chars: usize,
    /// How much of the block's tail one check reads, in chars.
    pub window_chars: usize,
    /// A block is re-checked once it grew by this many bytes.
    pub check_every_bytes: usize,
    /// Guard reply text too, not only thinking. Off by default: a user can
    /// legitimately ask for repetitive output ("write hello 1000 times"),
    /// while no one asks for looping reasoning.
    pub guard_text: bool,
}

impl Default for RepetitionGuardConfig {
    fn default() -> Self {
        RepetitionGuardConfig {
            max_period_chars: 512,
            min_repeats: 8,
            min_span_chars: 2_000,
            window_chars: 8_192,
            check_every_bytes: 256,
            guard_text: false,
        }
    }
}

/// A detected loop in one content block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepetitionLoop {
    /// The streamed content block that loops.
    pub content_index: usize,
    /// The repeating unit's length, in chars.
    pub period_chars: usize,
    /// How many whole repeats the looping span holds.
    pub repeats: usize,
    /// The block's char offset where the looping span starts.
    pub start_char: usize,
}

impl RepetitionLoop {
    /// The error text a guarded stop carries.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "Generation stopped by the repetition guard: the output repeated one {}-character unit {} times in a row",
            self.period_chars, self.repeats
        )
    }
}

/// The incremental guard over one assistant response.
#[derive(Debug, Clone)]
pub struct RepetitionGuard {
    config: RepetitionGuardConfig,
    /// The block length (bytes) each block was last checked at.
    checked_len: Vec<usize>,
}

impl RepetitionGuard {
    #[must_use]
    pub fn new(config: RepetitionGuardConfig) -> Self {
        RepetitionGuard {
            config,
            checked_len: Vec::new(),
        }
    }

    /// Check the block at `content_index` of the streamed partial; returns
    /// the loop once its tail is degenerate. Tool-call blocks are never
    /// checked (their arguments are data the model was asked to write), and
    /// text blocks only with [`RepetitionGuardConfig::guard_text`].
    pub fn observe(
        &mut self,
        partial: &AssistantMessage,
        content_index: usize,
    ) -> Option<RepetitionLoop> {
        let text = match partial.content.get(content_index)? {
            AssistantContent::Text(text) if self.config.guard_text => text.text.as_str(),
            AssistantContent::Thinking(thinking) => thinking.thinking.as_str(),
            AssistantContent::Text(_) | AssistantContent::ToolCall(_) => return None,
        };
        if self.checked_len.len() <= content_index {
            self.checked_len.resize(content_index + 1, 0);
        }
        if text.len() < self.checked_len[content_index] + self.config.check_every_bytes {
            return None;
        }
        self.checked_len[content_index] = text.len();
        let found = periodic_tail(text, &self.config)?;
        Some(RepetitionLoop {
            content_index,
            ..found
        })
    }
}

/// The degenerate periodic suffix of `text`, if any (`content_index` 0).
#[must_use]
pub fn periodic_tail(text: &str, config: &RepetitionGuardConfig) -> Option<RepetitionLoop> {
    let total_chars = text.chars().count();
    // The tail window, reversed: a prefix of `reversed` is a suffix of the
    // window, so the failure function gives every suffix's smallest period.
    let reversed: Vec<char> = text.chars().rev().take(config.window_chars).collect();
    let mut failure = vec![0usize; reversed.len()];
    let mut best: Option<(usize, usize)> = None;
    for index in 1..reversed.len() {
        let mut matched = failure[index - 1];
        while matched > 0 && reversed[index] != reversed[matched] {
            matched = failure[matched - 1];
        }
        if reversed[index] == reversed[matched] {
            matched += 1;
        }
        failure[index] = matched;
        let span = index + 1;
        let period = span - matched;
        if period <= config.max_period_chars
            && span >= (period * config.min_repeats).max(config.min_span_chars)
        {
            best = Some((span, period));
        }
    }
    let (span, period) = best?;
    Some(RepetitionLoop {
        content_index: 0,
        period_chars: period,
        repeats: span / period,
        start_char: total_chars - span,
    })
}

/// Cut a looping block down to its first two repeats, so the transcript
/// (and the next request's context) keeps the evidence without the
/// thousands of copies.
pub fn trim_loop(message: &mut AssistantMessage, found: &RepetitionLoop) {
    let keep = found.start_char + found.period_chars * 2;
    let trim = |text: &mut String| {
        if let Some((byte, _)) = text.char_indices().nth(keep) {
            text.truncate(byte);
        }
    };
    match message.content.get_mut(found.content_index) {
        Some(AssistantContent::Text(text)) => trim(&mut text.text),
        Some(AssistantContent::Thinking(thinking)) => trim(&mut thinking.thinking),
        Some(AssistantContent::ToolCall(_)) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn config() -> RepetitionGuardConfig {
        RepetitionGuardConfig::default()
    }

    /// A verbatim word loop and a line loop are degenerate tails.
    #[test]
    fn verbatim_word_and_line_loops_are_detected() {
        let words = format!("Let me think about this. {}", "the ".repeat(600));
        assert_eq!(
            periodic_tail(&words, &config()),
            Some(RepetitionLoop {
                content_index: 0,
                period_chars: 4,
                repeats: 600,
                // The space before the first "the" already belongs to the
                // rotated unit " the".
                start_char: "Let me think about this.".len(),
            })
        );
        let line = "I need to check the configuration file again.\n";
        let lines = format!("Plan:\n{}", line.repeat(60));
        let found = periodic_tail(&lines, &config()).expect("a line loop");
        assert_eq!(
            (found.period_chars, found.repeats),
            (line.chars().count(), 60)
        );
        // A loop with a partial unit at the end still reads as the loop.
        let ragged = format!("{lines}I need to ch");
        assert_eq!(
            periodic_tail(&ragged, &config()).map(|found| found.period_chars),
            Some(line.chars().count())
        );
    }

    /// Real output with repeated structure never trips the guard: code with
    /// repeated lines, a markdown table, a numbered list, JSON, prose.
    #[test]
    fn code_tables_lists_and_prose_are_not_loops() {
        let braces = format!(
            "fn main() {{\n{}{}",
            "    if x {\n        if y {\n            go();\n".repeat(5),
            "        }\n    }\n".repeat(5)
        );
        let asserts = "    assert_eq!(value, expected);\n".repeat(12);
        let code = format!("{braces}\n#[test]\nfn t() {{\n{asserts}}}\n");
        assert_eq!(periodic_tail(&code, &config()), None);

        let table = (0..80).fold(String::new(), |mut table, row| {
            let _ = writeln!(table, "| {row} | value | ok |");
            table
        });
        assert_eq!(
            periodic_tail(&format!("| n | v | s |\n|---|---|---|\n{table}"), &config()),
            None
        );

        let list = (1..200).fold(String::new(), |mut list, item| {
            let _ = writeln!(list, "{item}. Check item {item}.");
            list
        });
        assert_eq!(periodic_tail(&list, &config()), None);

        let json = (0..150).fold(String::new(), |mut json, id| {
            let _ = writeln!(
                json,
                "{{\"id\": {id}, \"name\": \"user{id}\", \"active\": true}},"
            );
            json
        });
        assert_eq!(periodic_tail(&format!("[\n{json}]"), &config()), None);

        let prose = "The scheduler claims a due job, runs it, and records the outcome. "
            .to_string()
            + "A skipped beat keeps its phase. Failures back off exponentially. "
            + "Every mutation wakes the timer so new jobs fire on time.";
        assert_eq!(periodic_tail(&prose.repeat(3), &config()), None);
    }

    /// Legitimately repeated lines below the span floor stay clear: twenty
    /// identical short lines are 600 chars, far under the 2000-char floor.
    #[test]
    fn a_short_run_of_identical_lines_is_not_a_loop() {
        let run = "        }\n".repeat(20);
        assert_eq!(periodic_tail(&run, &config()), None);
        let separators = "-".repeat(120);
        assert_eq!(periodic_tail(&separators, &config()), None);
    }

    /// The guard checks only after the block grew by the check stride, and
    /// reports the block it found looping.
    #[test]
    fn the_guard_reports_the_looping_block() {
        let mut guard = RepetitionGuard::new(RepetitionGuardConfig {
            guard_text: true,
            ..config()
        });
        let text = |text: String| {
            AssistantContent::Text(crate::types::TextContent {
                text,
                text_signature: None,
            })
        };
        let mut message = AssistantMessage {
            content: vec![text("ok ".to_string()), text("na ".repeat(1_000))],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: crate::types::Usage::zero(),
            stop_reason: crate::types::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
        };
        assert_eq!(guard.observe(&message, 0), None);
        let found = guard.observe(&message, 1).expect("the second block loops");
        assert_eq!((found.content_index, found.period_chars), (1, 3));
        trim_loop(&mut message, &found);
        match &message.content[1] {
            AssistantContent::Text(text) => assert_eq!(text.text, "na na "),
            other => panic!("unexpected block {other:?}"),
        }
        // The default guards thinking only: the same text loop passes.
        let mut thinking_only = RepetitionGuard::new(config());
        message.content[1] = AssistantContent::Text(crate::types::TextContent {
            text: "na ".repeat(1_000),
            text_signature: None,
        });
        assert_eq!(thinking_only.observe(&message, 1), None);
        message.content[1] = AssistantContent::Thinking(crate::types::ThinkingContent {
            thinking: "na ".repeat(1_000),
            thinking_signature: None,
            redacted: None,
        });
        assert_eq!(
            thinking_only
                .observe(&message, 1)
                .map(|found| found.period_chars),
            Some(3)
        );
    }
}
