//! The fork's automatic-refine policy (TS `4c99bf8c2`): an approved
//! automatic review promotes its refine to the global harness, through the
//! RAVO gate, unless the reviewer asked for `"scope": "local"`; the review
//! prompt asks for that field.

use pa_core::refinement::executor::{AutoRefinePolicy, AutoRefineReview, AutoRefineRun};

/// The review's system prompt (TS `AUTO_REFINE_REVIEW_SYSTEM_PROMPT`).
pub const GLOBAL_DEFAULT_REVIEW_SYSTEM_PROMPT: &str = "You are Prime Agent's automatic /refine review gate.\n\nDecide whether this checkpoint should run /refine. Auto /refine writes local continual harness state by default, so approve when the trajectory contains evidence useful to this session's future turns.\nReject one-off noise, unsupported hypotheses, and transient tool outputs.\n\nScope defaults to global (cross-session) — the permissive default. Emit \"scope\": \"local\" ONLY when the entry is genuinely session-specific and must not affect future sessions (current-run progress, task state, one-off coordination). Durable lessons, corrections, preferences, and reusable facts stay global; when in doubt, omit scope (global).\n\nReturn JSON only:\n{\n  \"shouldRefine\": true|false,\n  \"rationale\": \"short reason\",\n  \"instructions\": \"optional concise instructions for /refine if shouldRefine is true\",\n  \"scope\": \"local\"|\"global\"\n}";

/// The closing guidance of the review's prompt (TS `reviewAutoRefine`).
pub const GLOBAL_DEFAULT_REVIEW_GUIDANCE: &str = "Return shouldRefine=true when the trajectory contains evidence useful to this session's future turns. Prefer local harness edits for current task progress and current-run coordination; a transient condition (an open blocker, a pending rename) belongs there only with how to re-check it. Scope defaults to local: set scope=global only when the trajectory holds an explicit operator or user correction stating a durable standing rule (\"always\", \"never\", \"when you finish\", \"do not ... unless\") meant to hold in future sessions; routine progress, task state, and one-off facts are never global.";

/// Global unless the reviewer asked for a local refine.
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobalDefaultAutoRefine;

/// Whether the review asked to stay session-scoped: only an explicit
/// `"local"`; an absent or unknown scope is global, the permissive default.
#[must_use]
pub fn review_is_local(review: &AutoRefineReview) -> bool {
    review
        .reply
        .get("scope")
        .and_then(serde_json::Value::as_str)
        == Some("local")
}

/// The instructions an approved review carries into its run (TS
/// `autoRefineInstructions`).
#[must_use]
pub fn global_default_instructions(reason: &str, review: &AutoRefineReview) -> String {
    let detail = review
        .instructions
        .as_deref()
        .map(|instructions| format!("\nReviewer instructions: {instructions}"))
        .unwrap_or_default();
    let rationale = &review.rationale;
    if review_is_local(review) {
        format!(
            "Automatic refine review triggered by {reason}. Keep this session-scoped: only create/update/delete local harness entries if there is clear evidence that should help this session continue. Prefer an empty edits array over speculative or one-off memories. Do not promote anything global. Reviewer rationale: {rationale}{detail}"
        )
    } else {
        format!(
            "Automatic refine review triggered by {reason}. Create, update, or delete global harness entries when there is clear evidence they will help future Prime Agent sessions; a durable preference, correction, or reusable fact belongs here. Prefer an empty edits array over speculative or one-off memories, and do not record transient session progress or task state globally. Reviewer rationale: {rationale}{detail}"
        )
    }
}

impl AutoRefinePolicy for GlobalDefaultAutoRefine {
    fn review_system_prompt(&self) -> &'static str {
        GLOBAL_DEFAULT_REVIEW_SYSTEM_PROMPT
    }

    fn review_guidance(&self) -> &'static str {
        GLOBAL_DEFAULT_REVIEW_GUIDANCE
    }

    fn approved_refine(&self, reason: &str, review: &AutoRefineReview) -> AutoRefineRun {
        AutoRefineRun {
            global: !review_is_local(review),
            instructions: global_default_instructions(reason, review),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn review(reply: serde_json::Value) -> AutoRefineReview {
        let serde_json::Value::Object(reply) = reply else {
            panic!("a JSON object reply");
        };
        AutoRefineReview {
            should_refine: true,
            rationale: "a standing rule".to_string(),
            instructions: reply
                .get("instructions")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            reply,
        }
    }

    /// Absent, unknown or explicit global scope runs globally; only an
    /// explicit `local` stays session-scoped, with the TS instruction texts.
    #[test]
    fn only_an_explicit_local_review_stays_local() {
        let policy = GlobalDefaultAutoRefine;
        for reply in [
            json!({}),
            json!({"scope": "global"}),
            json!({"scope": "nonsense"}),
            json!({"scope": 3}),
        ] {
            assert_eq!(
                policy.approved_refine("compact", &review(reply)),
                AutoRefineRun {
                    global: true,
                    instructions: "Automatic refine review triggered by compact. Create, update, or delete global harness entries when there is clear evidence they will help future Prime Agent sessions; a durable preference, correction, or reusable fact belongs here. Prefer an empty edits array over speculative or one-off memories, and do not record transient session progress or task state globally. Reviewer rationale: a standing rule".to_string(),
                }
            );
        }
        assert_eq!(
            policy.approved_refine(
                "compact",
                &review(json!({"scope": "local", "instructions": "note it"}))
            ),
            AutoRefineRun {
                global: false,
                instructions: "Automatic refine review triggered by compact. Keep this session-scoped: only create/update/delete local harness entries if there is clear evidence that should help this session continue. Prefer an empty edits array over speculative or one-off memories. Do not promote anything global. Reviewer rationale: a standing rule\nReviewer instructions: note it".to_string(),
            }
        );
    }

    /// The review asks for the scope field (TS texts).
    #[test]
    fn the_review_prompt_asks_for_a_scope() {
        let policy = GlobalDefaultAutoRefine;
        assert!(
            policy
                .review_system_prompt()
                .ends_with("  \"scope\": \"local\"|\"global\"\n}")
        );
        assert!(
            policy
                .review_system_prompt()
                .contains("Scope defaults to global (cross-session) — the permissive default.")
        );
        assert!(policy
            .review_guidance()
            .starts_with("Return shouldRefine=true when the trajectory contains evidence useful to this session's future turns."));
    }
}
