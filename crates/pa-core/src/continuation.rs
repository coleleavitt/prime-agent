//! Self-continuation: decide whether to push the agent into another turn
//! after it stops, and what to tell it.
//!
//! The host owns the state (task counts, in-flight work, permission mode) and
//! calls [`decide`] when a turn ends. A `Some` result means "inject
//! [`ContinueReason::nudge`] as a user message (jfc wraps it in
//! `<system-reminder>` tags) and start the next turn". The host also owns the
//! counter: increment it on every self-continuation and reset it to 0 when the
//! user types something.

/// Default cap on consecutive self-continuations (jfc's
/// `[continuation] max_self_continuations`).
pub const DEFAULT_MAX_SELF_CONTINUATIONS: u32 = 25;

/// Why the provider ended the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    Other(String),
}

/// Refusals never self-continue: re-prompting a refusal is a retry loop.
#[must_use]
pub fn should_self_continue_after_stop_reason(stop_reason: &StopReason) -> bool {
    !matches!(stop_reason, StopReason::Refusal)
        && !matches!(stop_reason, StopReason::Other(s) if looks_like_refusal_stop_reason(s))
}

#[must_use]
pub fn looks_like_refusal_stop_reason(reason: &str) -> bool {
    reason == "content_filter" || reason.contains("refusal")
}

/// Snapshot of the runtime fields that must all be quiet before the next turn
/// may start.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnIdleness {
    pub is_streaming: bool,
    pub pending_approval: bool,
    pub approval_queue_len: usize,
    pub pending_tool_calls_len: usize,
    pub queued_prompts_len: usize,
    pub pending_classifications: usize,
    pub in_flight_eager_dispatches: usize,
    pub in_flight_tool_batches: usize,
}

/// True iff the runtime is quiescent. The `in_flight_*` checks matter: when a
/// turn ends with `ToolUse`, the host dispatches the tool batch and only then
/// reaches this check. Continuing while those counters are positive starts the
/// next turn over still-running tools, which then come back as abandoned.
#[must_use]
pub fn turn_is_idle_for_self_continue(idle: &TurnIdleness) -> bool {
    !idle.is_streaming
        && !idle.pending_approval
        && idle.approval_queue_len == 0
        && idle.pending_tool_calls_len == 0
        && idle.queued_prompts_len == 0
        && idle.pending_classifications == 0
        && idle.in_flight_eager_dispatches == 0
        && idle.in_flight_tool_batches == 0
}

/// Detect a "permission-asking stall": the assistant finished a chunk of work
/// and ended the turn by *asking whether to do the next obvious step* instead
/// of doing it. Corpus analysis of 133 jfc turns where the user had to type
/// "continue" showed ~41% ended this way ("Want me to …?", a trailing
/// question, "shall I", "let me know").
///
/// `text` is the plain text of the last assistant message. Pass it only when
/// that message made no tool calls; tool-bearing turns continue through the
/// normal agent loop instead.
/// Permission-asking phrases in the tail of a stalled turn.
const STALL_PHRASES: &[&str] = &[
    "want me to",
    "shall i ",
    "should i ",
    "would you like",
    "do you want",
    "let me know",
    "if you want",
    "if you'd like",
    "ready to proceed",
    "ready to implement",
    "want to pick",
    "any changes before",
    "or call this a stable checkpoint",
];

#[must_use]
pub fn assistant_text_stalls(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return false;
    }

    // Inspect the closing window (last ~240 bytes) — stalls live at the end.
    // The window start is a raw byte offset, so snap it forward to the next
    // char boundary; otherwise a multi-byte char (e.g. an em-dash `—`) straddling
    // the cut would panic the slice.
    let mut tail_start = trimmed.len().saturating_sub(240);
    while tail_start < trimmed.len() && !trimmed.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = trimmed[tail_start..].to_lowercase();

    if STALL_PHRASES.iter().any(|p| tail.contains(p)) {
        return true;
    }

    // Action announced but not taken: the turn ended by *narrating the next
    // step* ("Now I'll implement …", "Let me verify …", "I'll start by …")
    // with no tool call. Corpus mining of the user's jfc + prime-agent
    // transcripts (2026-11-12) found this is how the model stalls most often:
    // of 36 non-error text turns the user had to resume by hand, the
    // permission-phrase + trailing-`?` rule caught 3 (8%); adding this one
    // raised it to 25 (69%). Safe here because this guard only sees genuinely
    // ended, tool-free turns — an announced action with no tool run is a stall.
    if last_sentence_announces_action(trimmed) {
        return true;
    }

    // Ended on a dumped table/data block with no closing prose, or by listing
    // what is still undone without claiming the next step — both are "there is
    // obviously more to do" stalls that a user drives through with "continue".
    if ends_on_data_block(trimmed) || REMAINING_WORK_MARKERS.iter().any(|m| tail.contains(m)) {
        return true;
    }

    // A trailing question mark is a weaker but real signal: the turn ended on
    // a question, which in a factory context is a request for direction.
    trimmed.ends_with('?')
}

/// The turn ended by dumping a markdown table or data block and stopping: the
/// last non-empty line is a table row with no sentence summarizing it.
fn ends_on_data_block(text: &str) -> bool {
    let last = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    last.starts_with('|') && last.ends_with('|') && last.matches('|').count() >= 3
}

/// Phrases that enumerate remaining work without claiming the next step
/// ("X still needs...", "Y is still unverified", "after that, ..."). Distinct
/// from a legitimate wait ("still before the nightly"), which stays undetected.
const REMAINING_WORK_MARKERS: &[&str] = &[
    "still needs",
    "still unverified",
    "still open",
    "still to be",
    "still has to",
    "what's left",
    "what remains",
    "after that,",
];

/// Strong continue signal: the turn ended by announcing or listing a next
/// action it did not take ("Now I'll …", "Let me …", "Next steps: 1. …"), or by
/// teeing up output with a trailing colon and then stopping ("Now rebuild and
/// check:"). True in both modes. The trailing-colon case ran 31% continue in the
/// 2026-11-12 corpus vs an 8% baseline — the model was about to produce content
/// and the turn ended before it did.
#[must_use]
pub fn assistant_announces_action(text: &str) -> bool {
    let trimmed = text.trim_end();
    last_sentence_announces_action(trimmed)
        || announces_plan_list(trimmed)
        || ends_on_teed_up_colon(trimmed)
}

/// True when the turn ends on a `:` that tees up content the model never wrote.
/// Guards against a bare label by requiring a few words before the colon.
fn ends_on_teed_up_colon(text: &str) -> bool {
    let last = text.lines().next_back().unwrap_or("").trim();
    last.ends_with(':') && last.split_whitespace().count() >= 3
}

/// "Next steps:" / numbered-plan ending with no action taken.
fn announces_plan_list(text: &str) -> bool {
    let tl = text.trim_end();
    let tl = tl[tl.len().saturating_sub(200)..].to_lowercase();
    tl.contains("next steps:") || tl.contains("remaining steps:") || tl.contains("plan:")
}

/// Near-future action leads: the model says it is *about to* do something.
const ACTION_LEADS: &[&str] = &[
    "let me ",
    "let's ",
    "i'll ",
    "i will ",
    "i am going to ",
    "i'm going to ",
    "now i'll ",
    "now i will ",
    "now let me ",
    "next i'll ",
    "next, i'll ",
    "next let me ",
];

/// Connectives after which an action lead still announces the next step
/// ("...so first I'll reread", "...then let me check"). Each carries its own
/// surrounding spaces so a substring match cannot fire inside a word
/// ("underst`and i'll`").
const ACTION_CONNECTIVES: &[&str] = &[" so ", " then ", " first ", ", so ", ", then "];

/// True when the last sentence of `text` starts with — or, after a connective
/// like "—", "so", "then", "first", contains — a near-future action lead.
/// Checking only the last sentence keeps a mid-report "let me explain" from
/// tripping it.
fn last_sentence_announces_action(text: &str) -> bool {
    let last = text
        .rsplit(['.', '!', '?', '\n'])
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_lowercase();
    ACTION_LEADS.iter().any(|lead| {
        last.starts_with(lead)
            || last.contains(&format!("— {lead}"))
            || last.contains(&format!(". {lead}"))
            || ACTION_CONNECTIVES
                .iter()
                .any(|conn| last.contains(&format!("{conn}{lead}")))
    })
}

/// Provider-degradation lines that render as ordinary assistant text. A
/// self-continue here just re-runs the failing request — the user should retry
/// or the host should back off. jfc's refusal gate does not cover these because
/// they arrive with a normal stop reason, not `Refusal`.
const PROVIDER_ERROR_MARKERS: &[&str] = &[
    "overloaded",
    "accounts exhausted",
    "rate limit",
    "content filtering",
    "content filter",
    "press ctrl+r",
    "try again",
];

/// True when the assistant text is really a surfaced provider error.
#[must_use]
pub fn looks_like_provider_error(text: &str) -> bool {
    let tl = text.trim_end();
    let tl = tl[tl.len().saturating_sub(300)..].to_lowercase();
    PROVIDER_ERROR_MARKERS.iter().any(|m| tl.contains(m))
}

/// The model kicked off background work and is correctly waiting for it
/// (64 such turn-ends in the 2026-11-12 corpus, none of which a human resumed by
/// hand). Continuing here interrupts a legitimate wait, so it suppresses a
/// self-continue regardless of mode.
const WAIT_MARKERS: &[&str] = &[
    "in the background",
    "running in the background",
    "i'll report",
    "will report back",
    "once it finishes",
    "once it completes",
    "once the build",
    "waiting for",
    "let it run",
    "kicked off",
    "i'll check on",
    "check back",
];

/// True when the last sentence says the model is waiting on something it already
/// started.
#[must_use]
pub fn assistant_is_waiting(text: &str) -> bool {
    let tl = text.trim_end();
    let tl = tl[tl.len().saturating_sub(240)..].to_lowercase();
    WAIT_MARKERS.iter().any(|m| tl.contains(m))
}

/// How aggressively to self-continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContinueMode {
    /// A session the user is driving by hand. A bare question ("want me to?",
    /// trailing "?") is treated as a real question for the user to answer, not a
    /// stall to drive through — in the corpus humans answered those 99.6% of the
    /// time. Only a strong signal continues: an announced-but-untaken action,
    /// queued tasks, or an output-truncated reply.
    #[default]
    Interactive,
    /// An autonomous / factory run with no user at the keyboard. Any stall —
    /// including a bare permission question — drives the next step. This is
    /// jfc's original behavior.
    Autonomous,
}

impl ContinueMode {
    /// Whether `text` is a stall this mode drives through — the one place that
    /// maps a mode to its signal leaf. `Interactive` takes only the strong
    /// announced-but-untaken-action signal; `Autonomous` takes any stall,
    /// including a bare permission question.
    #[must_use]
    pub fn text_stalls(self, text: &str) -> bool {
        match self {
            Self::Interactive => assistant_announces_action(text),
            Self::Autonomous => assistant_text_stalls(text),
        }
    }
}

/// Why the agent is being pushed into another turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueReason {
    /// The reply hit the output-token limit mid-answer.
    OutputTruncated,
    /// The task list still has open work.
    TasksRemain { pending: usize, in_progress: usize },
    /// The reply ended by asking permission for the next step.
    Stalled,
}

impl ContinueReason {
    /// The message jfc injects as the next user turn.
    #[must_use]
    pub fn nudge(&self) -> String {
        match self {
            Self::OutputTruncated => {
                "Your previous response was cut off because it reached the maximum output \
                 length. Continue it from exactly where it stopped — do not repeat what you \
                 already wrote, and do not restart. Pick up mid-sentence if needed."
                    .to_string()
            }
            Self::TasksRemain {
                pending,
                in_progress,
            } => format!(
                "Continue the remaining work. There are {pending} pending and {in_progress} in-progress task(s) — \
                 work through them. Do NOT stop to ask permission for the next in-scope step; \
                 only pause for genuine forks (incompatible interpretations, irreversible actions, \
                 or missing external input). When the whole scope is done, verify (build/test/commit) \
                 and report."
            ),
            Self::Stalled => {
                "Continue — do the next step you just proposed instead of asking whether to. \
                 Only pause for genuine forks (incompatible interpretations, irreversible actions, \
                 or missing external input). When the full scope is done, verify and report."
                    .to_string()
            }
        }
    }
}

/// Everything [`decide`] reads at the end of a turn.
#[derive(Debug, Clone, Copy)]
pub struct TurnEnd<'a> {
    pub stop_reason: &'a StopReason,
    pub idle: TurnIdleness,
    /// Plan mode is read-only by contract: never auto-act.
    pub plan_mode: bool,
    /// Host switch (jfc: `JFC_AUTO_CONTINUE`, then `[continuation] auto_continue`).
    pub auto_continue: bool,
    pub pending_tasks: usize,
    pub in_progress_tasks: usize,
    /// Text of the last assistant message, or `None` when it made tool calls.
    pub last_assistant_text: Option<&'a str>,
    /// Consecutive self-continuations since the user last typed.
    pub self_continuation_count: u32,
    pub max_self_continuations: u32,
    /// How aggressively to continue (see [`ContinueMode`]).
    pub mode: ContinueMode,
}

/// Decide whether to self-continue. Same order of checks as jfc's
/// `maybe_self_continue`: stop reason, idleness, plan mode, the switch, a
/// reason to continue, then the cap.
#[must_use]
pub fn decide(turn: &TurnEnd<'_>) -> Option<ContinueReason> {
    if !should_self_continue_after_stop_reason(turn.stop_reason)
        || !turn_is_idle_for_self_continue(&turn.idle)
        || turn.plan_mode
        || !turn.auto_continue
    {
        return None;
    }
    // A surfaced provider error (Overloaded, accounts exhausted, content
    // filtering) is not a stall to drive through — resuming re-runs the failure.
    // The model correctly waiting on background work it already started is not a
    // stall either — continuing would interrupt the wait.
    if turn
        .last_assistant_text
        .is_some_and(|text| looks_like_provider_error(text) || assistant_is_waiting(text))
    {
        return None;
    }
    let output_truncated = matches!(turn.stop_reason, StopReason::MaxTokens);
    let tasks_remain = turn.pending_tasks > 0 || turn.in_progress_tasks > 0;
    // The stall signal depends on the mode. Interactive only drives through a
    // strong signal (an announced-but-untaken action); a bare permission
    // question is left for the user. Autonomous drives through any stall,
    // including a bare question (jfc's original behavior).
    let stalled = turn
        .last_assistant_text
        .is_some_and(|t| turn.mode.text_stalls(t));
    if !tasks_remain && !stalled && !output_truncated {
        return None;
    }
    if turn.self_continuation_count >= turn.max_self_continuations {
        return None;
    }
    Some(if output_truncated {
        ContinueReason::OutputTruncated
    } else if tasks_remain {
        ContinueReason::TasksRemain {
            pending: turn.pending_tasks,
            in_progress: turn.in_progress_tasks,
        }
    } else {
        ContinueReason::Stalled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim stalls from the jfc session corpus — all must be detected.
    #[test]
    fn detects_corpus_stall_phrases() {
        let stalls = [
            "Want me to start implementing any of these?",
            "Want me to take a closer look and propose an exact patch?",
            "Want me to fire another round of 30 agents covering the remaining 165 tasks?",
            "Does this plan look right? Any changes before I start coding?",
            "Want me to dive deeper into a specific section of either file?",
            "Shall I proceed with the refactor?",
            "Should I wire that in next?",
            "Let me know which direction you'd prefer.",
            "I can do that next if you want.",
        ];
        for s in stalls {
            assert!(assistant_text_stalls(s), "should detect stall: {s:?}");
        }
    }

    #[test]
    fn ignores_genuine_completions() {
        let done = [
            "Done. Pushed `abc1234`.",
            "All 6 gaps shipped and the release binary is rebuilt.",
            "Fixed the bug and committed. Tests pass.",
            "The cache now serves 5.5x faster on warm reads.",
        ];
        for s in done {
            assert!(
                !assistant_text_stalls(s),
                "should NOT flag completion: {s:?}"
            );
        }
    }

    #[test]
    fn detects_trailing_question() {
        assert!(assistant_text_stalls(
            "I found three candidates. Which one should we tackle first?"
        ));
    }

    // 2026-11-13 corpus: an announced action reached through a connective
    // ("...so first I'll reread") is a strong signal — both modes drive it.
    #[test]
    fn detects_conjunction_action_lead() {
        let s = "I'll start a headless camoufox browser. The helper I used earlier                  didn't survive the kernel restart, so first I'll reread how the                  camoufox controller takes commands";
        assert!(assistant_announces_action(s));
        assert!(assistant_text_stalls(s));
    }

    // 2026-11-13 corpus: the turn dumped a results table and stopped (autonomous
    // leaf). A closing sentence after the table is a completion, not a stall.
    #[test]
    fn detects_data_block_end() {
        let table = "Quick reference:
                     | Tool | Query | Results |
                     |------|-------|---------|
                     | Shodan | port:11434 | 206316 |
                     | rustscan | -a ips.txt | Port scan |";
        assert!(assistant_text_stalls(table));
        let summarized = format!(
            "{table}

That is the full scan surface; nothing else is open."
        );
        assert!(!assistant_text_stalls(&summarized));
    }

    // 2026-11-13 corpus: remaining work enumerated without claiming it.
    #[test]
    fn detects_remaining_work_enumeration() {
        let s = "I ran the script and reported what it prints. After that, the                  TrollStore app still needs its persistence helper, and whether                  Dopamine works on this A8X is still unverified.";
        assert!(assistant_text_stalls(s));
    }

    // A legitimate ETA wait ("still before the nightly") is not remaining-work
    // enumeration and must not be driven through.
    #[test]
    fn eta_wait_is_not_a_remaining_work_stall() {
        let s = "The fetch finishes around 04:45Z (11:45 PM your time). That is                  still before the 07:00Z nightly, so the backfill has room.";
        assert!(!assistant_text_stalls(s));
    }

    #[test]
    fn empty_text_does_not_stall() {
        assert!(!assistant_text_stalls(""));
        assert!(!assistant_text_stalls("   \n"));
    }

    // A "want me to" far from the end of a long report must not trip.
    #[test]
    fn only_checks_the_tail() {
        let long = format!(
            "Earlier I asked want me to do X — you said yes, so I did it.\n\n{}",
            "Done. All tasks complete, committed, and pushed. ".repeat(8)
        );
        assert!(!assistant_text_stalls(&long));
    }

    // Regression: an em-dash straddling the 240-byte window must not panic.
    #[test]
    fn multibyte_char_on_tail_boundary_does_not_panic() {
        for pad in 230..250usize {
            let s = format!("{}— and that wraps it up.", "x".repeat(pad));
            let _ = assistant_text_stalls(&s);
        }
    }

    #[test]
    fn refusal_stop_reason_blocks_continue() {
        assert!(!should_self_continue_after_stop_reason(
            &StopReason::Refusal
        ));
        assert!(!should_self_continue_after_stop_reason(&StopReason::Other(
            "content_filter".to_string()
        )));
        assert!(should_self_continue_after_stop_reason(&StopReason::EndTurn));
        assert!(should_self_continue_after_stop_reason(
            &StopReason::MaxTokens
        ));
    }

    // Regression from jfc ses_20260528_130646.log: continuing while a tool
    // batch was in flight serialized the running tool as abandoned.
    #[test]
    fn in_flight_work_blocks_continue() {
        assert!(turn_is_idle_for_self_continue(&TurnIdleness::default()));
        let batch = TurnIdleness {
            in_flight_tool_batches: 1,
            ..TurnIdleness::default()
        };
        let eager = TurnIdleness {
            in_flight_eager_dispatches: 1,
            ..TurnIdleness::default()
        };
        assert!(!turn_is_idle_for_self_continue(&batch));
        assert!(!turn_is_idle_for_self_continue(&eager));
    }

    fn turn_end<'a>(stop_reason: &'a StopReason, text: Option<&'a str>) -> TurnEnd<'a> {
        TurnEnd {
            stop_reason,
            idle: TurnIdleness::default(),
            plan_mode: false,
            auto_continue: true,
            pending_tasks: 0,
            in_progress_tasks: 0,
            last_assistant_text: text,
            self_continuation_count: 0,
            max_self_continuations: DEFAULT_MAX_SELF_CONTINUATIONS,
            mode: ContinueMode::Autonomous,
        }
    }

    #[test]
    fn decide_picks_the_reason() {
        let end = StopReason::EndTurn;
        assert_eq!(decide(&turn_end(&end, Some("Done."))), None);
        assert_eq!(
            decide(&turn_end(&end, Some("Want me to wire it in?"))),
            Some(ContinueReason::Stalled)
        );
        let tasks = TurnEnd {
            pending_tasks: 2,
            ..turn_end(&end, Some("Done."))
        };
        assert_eq!(
            decide(&tasks),
            Some(ContinueReason::TasksRemain {
                pending: 2,
                in_progress: 0
            })
        );
        let max = StopReason::MaxTokens;
        assert_eq!(
            decide(&turn_end(&max, Some("halfway through a sent"))),
            Some(ContinueReason::OutputTruncated)
        );
    }

    #[test]
    fn decide_respects_gates_and_cap() {
        let end = StopReason::EndTurn;
        let stalled = turn_end(&end, Some("Shall I continue?"));
        assert!(
            decide(&TurnEnd {
                plan_mode: true,
                ..stalled
            })
            .is_none()
        );
        assert!(
            decide(&TurnEnd {
                auto_continue: false,
                ..stalled
            })
            .is_none()
        );
        assert!(
            decide(&TurnEnd {
                self_continuation_count: 25,
                ..stalled
            })
            .is_none()
        );
        // A tool-bearing last message is never a stall.
        assert!(decide(&turn_end(&end, None)).is_none());
    }

    #[test]
    fn detects_action_announced_not_taken() {
        // Verbatim from the corpus — the model narrated its next step and stopped.
        let stalls = [
            "Good — I have the pattern. Now I'll implement all 7 features. Let me write the pkg files.",
            "Task t11 is functionally complete — let me verify its acceptance criteria mechanically.",
            "Let me check the manifest of each to find which one is for the Orion advisor app:",
            "I'll start by examining the current state of the project before planning anything.",
        ];
        for s in stalls {
            assert!(
                assistant_text_stalls(s),
                "should detect action stall: {s:?}"
            );
        }
    }

    #[test]
    fn action_lead_far_from_end_does_not_stall() {
        // "let me" buried mid-report, genuine completion at the end.
        let s =
            "Earlier I said let me check X, and I did. Done. All tests pass and it is committed.";
        assert!(!assistant_text_stalls(s));
    }

    #[test]
    fn provider_errors_are_not_stalls_to_continue() {
        let err = "**Error:** all Anthropic OAuth accounts exhausted. _Press Ctrl+R to retry._";
        assert!(looks_like_provider_error(err));
        let end = StopReason::EndTurn;
        let t = turn_end(&end, Some(err));
        assert_eq!(
            decide(&t),
            None,
            "a surfaced provider error must not self-continue"
        );
    }

    #[test]
    fn interactive_leaves_a_bare_question_for_the_user() {
        let end = StopReason::EndTurn;
        let q = TurnEnd {
            mode: ContinueMode::Interactive,
            ..turn_end(
                &end,
                Some("Which backend do you want — sqlite or postgres?"),
            )
        };
        assert_eq!(
            decide(&q),
            None,
            "interactive must not drive through a genuine question"
        );
        // ...but still continues on an announced-but-untaken action.
        let a = TurnEnd {
            mode: ContinueMode::Interactive,
            ..turn_end(
                &end,
                Some("Now I'll wire the parser in. Let me edit the module."),
            )
        };
        assert_eq!(decide(&a), Some(ContinueReason::Stalled));
    }

    #[test]
    fn autonomous_drives_through_a_bare_question() {
        let end = StopReason::EndTurn;
        let q = TurnEnd {
            mode: ContinueMode::Autonomous,
            ..turn_end(&end, Some("Want me to wire it in?"))
        };
        assert_eq!(decide(&q), Some(ContinueReason::Stalled));
    }

    #[test]
    fn waiting_on_background_work_never_continues() {
        assert!(assistant_is_waiting(
            "The build is running in the background; I'll report back."
        ));
        let end = StopReason::EndTurn;
        let w = TurnEnd {
            pending_tasks: 1,
            ..turn_end(
                &end,
                Some("Kicked off the full test suite in the background. Waiting for it."),
            )
        };
        assert_eq!(decide(&w), None, "must not interrupt a legitimate wait");
    }

    #[test]
    fn trailing_colon_tees_up_a_stall() {
        assert!(assistant_announces_action(
            "Now rebuild the crate and check the output:"
        ));
        assert!(assistant_text_stalls(
            "Here is the plan. Let me add the blocking keys next:"
        ));
        // A bare one-word label is not a tee-up.
        assert!(!assistant_announces_action("Notes:"));
        // A normal completion still does not stall.
        assert!(!assistant_announces_action("Done. All tests pass."));
    }
}
