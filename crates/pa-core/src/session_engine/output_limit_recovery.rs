//! Output-limit recovery: the loop policy that continues a reply cut off at
//! the output-token limit.
//!
//! Two shapes share the loop's [`LengthContinuation`] hook. An explicit
//! `lengthContinuations: n` keeps upstream #969's visible `[auto-continue]`
//! rows. Unset, the session follows Claude Code's recovery (`daj64yn4:Ad`,
//! transition `max_output_tokens_recovery`, nudge `Wl` -> `Mct`): up to three
//! hidden "Output token limit hit. Resume directly ..." rows, a reply the
//! limit cut while it was still reasoning included. A tool call the limit
//! cut is already answered by its error result (`prepare_tool_call`).

use std::sync::Arc;

use pa_agent::agent_loop::LengthContinuation;

use crate::settings::experimental::{
    CLAUDE_CODE_OUTPUT_LIMIT_RECOVERIES,
    OUTPUT_LIMIT_RECOVERY_TEXT,
    OutputLimitRecovery,
};

/// The custom type of Claude Code's hidden recovery row.
pub const OUTPUT_LIMIT_RECOVERY_CUSTOM_TYPE: &str = "output_limit_recovery";

/// Called once per delivered continuation (the session's adoption counter).
pub type RecoveryNote = Arc<dyn Fn() + Send + Sync>;

/// The loop policy for `recovery`; `None` when it is off.
#[must_use]
pub fn length_continuation(
    recovery: OutputLimitRecovery,
    note: RecoveryNote,
) -> Option<LengthContinuation> {
    match recovery {
        OutputLimitRecovery::Off => None,
        OutputLimitRecovery::Continuations(max_continuations) => Some(LengthContinuation {
            max_continuations,
            message: Arc::new(move |attempt, max| {
                note();
                crate::autonomous::autonomous_continuation_loop_row(
                    &format!(
                        "[auto-continue {attempt}/{max}: the previous reply was cut off at the output-token limit]\n\nContinue exactly where the previous reply stopped. Do not repeat what was already written."
                    ),
                    pa_agent::now_ms().max(0) as u64,
                )
            }),
            reasoning_only: false,
        }),
        OutputLimitRecovery::ClaudeCode => Some(LengthContinuation {
            max_continuations: CLAUDE_CODE_OUTPUT_LIMIT_RECOVERIES,
            message: Arc::new(move |_attempt, _max| {
                note();
                recovery_row()
            }),
            reasoning_only: true,
        }),
    }
}

/// Claude Code's hidden nudge as a loop row: the model reads it, the
/// transcript does not show it.
fn recovery_row() -> pa_agent::types::AgentMessage {
    super::session_message_to_loop(&super::SessionAgentMessage::Custom(
        super::plan_mode::hidden_row(
            OUTPUT_LIMIT_RECOVERY_CUSTOM_TYPE,
            OUTPUT_LIMIT_RECOVERY_TEXT,
        ),
    ))
    .unwrap_or_else(|| {
        crate::autonomous::autonomous_continuation_loop_row(
            OUTPUT_LIMIT_RECOVERY_TEXT,
            pa_agent::now_ms().max(0) as u64,
        )
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn counting() -> (Arc<AtomicU32>, RecoveryNote) {
        let count = Arc::new(AtomicU32::new(0));
        let note = {
            let count = Arc::clone(&count);
            Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }) as RecoveryNote
        };
        (count, note)
    }

    #[test]
    fn off_installs_nothing() {
        let (_, note) = counting();
        assert!(length_continuation(OutputLimitRecovery::Off, note).is_none());
    }

    #[test]
    fn claude_codes_recovery_is_three_hidden_nudges_covering_reasoning() {
        let (count, note) = counting();
        let policy = length_continuation(OutputLimitRecovery::ClaudeCode, note).unwrap();
        assert_eq!(policy.max_continuations, 3);
        assert!(policy.reasoning_only);
        let row = (policy.message)(1, 3);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let pa_agent::types::AgentMessage::Custom(custom) = row else {
            panic!("expected a hidden custom row, got {row:?}");
        };
        assert_eq!(
            custom.payload,
            serde_json::json!({
                "customType": OUTPUT_LIMIT_RECOVERY_CUSTOM_TYPE,
                "content": OUTPUT_LIMIT_RECOVERY_TEXT,
                "display": false,
                "timestamp": custom.payload["timestamp"],
            })
        );
    }

    #[test]
    fn an_explicit_count_keeps_the_visible_upstream_row() {
        let (_, note) = counting();
        let policy = length_continuation(OutputLimitRecovery::Continuations(2), note).unwrap();
        assert_eq!(policy.max_continuations, 2);
        assert!(!policy.reasoning_only);
        let pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::User(user)) =
            (policy.message)(1, 2)
        else {
            panic!("expected a user row");
        };
        let pa_agent::types::UserContent::Parts(parts) = user.content else {
            panic!("expected parts");
        };
        let [pa_agent::types::UserPart::Text(text)] = parts.as_slice() else {
            panic!("expected one text part");
        };
        assert!(text.text.starts_with("[auto-continue 1/2:"));
    }
}
