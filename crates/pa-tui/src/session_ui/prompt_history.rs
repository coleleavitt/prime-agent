//! Per-project prompt history: the client half of up-arrow recall that
//! survives restarts and resume (daemon capability `prompt_history`).
//!
//! The daemon owns the store (one writer, `pa_core::prompt_history`). This
//! module does two things from the client:
//!
//! - [`SessionUi::record_prompt_history`]: fire-and-forget, called beside every
//!   `editor.add_to_history` so the exact text the user typed — across plain,
//!   steer, follow-up, slash and `!` bash submits — is persisted for future
//!   sessions.
//! - [`SessionUi::seed_prompt_history`]: on attach, loads the project's recall
//!   list and seeds the editor's up-arrow ring so a freshly opened or resumed
//!   session recalls prompts from every earlier session in the project.
//!
//! Both are no-ops when the daemon lacks the capability or the environment
//! turns the feature off (Claude Code's `CLAUDE_CODE_SKIP_PROMPT_HISTORY`).

use std::sync::OnceLock;
use std::time::Duration;

use pa_types::daemon::DaemonCommand;
use regex::Regex;
use serde_json::{Map, Value};

use super::{AgentView, SessionUi, UI_REQUEST_TIMEOUT_MS};
use crate::editor::EditorPasteSnapshot;

/// The daemon server capability behind `append_prompt_history` and
/// `get_prompt_history` (mirrors `pa_daemon::protocol::PROMPT_HISTORY_CAPABILITY`;
/// pa-tui is a wire client and does not link the daemon).
const PROMPT_HISTORY_CAPABILITY: &str = "prompt_history";

/// The environment switch that turns prompt history off (mirrors
/// `pa_core::prompt_history::DISABLE_ENV`).
const DISABLE_ENV: &str = "PRIME_AGENT_DISABLE_PROMPT_HISTORY";

fn disabled_by_env() -> bool {
    std::env::var(DISABLE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// A Ctrl+C-cleared draft, held in a one-slot stash so Up on an empty
/// editor restores it bit-for-bit. Carries both the text and the
/// collapsed-paste snapshot so a draft with a pasted file (`[paste #N
/// ...]`) round-trips through the clear + restore cycle without losing
/// the paste's expansion (CC's `holdCleared` tuple: text, pastedContents,
/// clearedAt). When a new clear overwrites the slot, the OLD slot's text
/// is promoted to the persistent prompt history, so it survives even a
/// user who clears several drafts in a row without pressing Up.
#[derive(Debug, Clone)]
pub(super) struct ClearedDraft {
    pub(super) text: String,
    pub(super) paste_snapshot: EditorPasteSnapshot,
}

/// Heuristic secret-shape check: keep drafts containing an API-key-shaped
/// string, a private-key header, or a long opaque token out of the
/// persistent history. This is a filter, not a guarantee — a secret
/// pasted inline with other text still falls through on purpose (the
/// user asked to send it, and the clear+restore slot is in-memory only).
/// The public history is what survives restart; this gate keeps common
/// leaks (an accidentally pasted `.env` or `-----BEGIN` block) from
/// becoming persistent.
pub(super) fn is_sensitive_prompt(text: &str) -> bool {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        // Each is a shape common in a pasted credential blob. The
        // combined check costs microseconds on a short draft; a long
        // draft is rare (holdCleared only fires on Ctrl+C with a
        // non-empty editor) and still worth the check.
        let sources = [
            // PEM/OpenSSH private keys, PGP private blocks.
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
            r"-----BEGIN OPENSSH PRIVATE KEY-----",
            r"-----BEGIN PGP PRIVATE KEY BLOCK-----",
            // `AKIA...` / `ASIA...` AWS access key ids (exact 20 chars).
            r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
            // Anthropic / OpenAI / GitHub / Slack token prefixes.
            r"\bsk-ant-[A-Za-z0-9_-]{20,}\b",
            r"\bsk-[A-Za-z0-9_-]{20,}\b",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
            r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b",
            // Google OAuth client secret / API keys.
            r"\bAIza[0-9A-Za-z_-]{35}\b",
            // An env-file-style line: KEY=<long opaque>.
            r"(?m)^[A-Z][A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|PWD)[A-Z0-9_]*\s*=\s*\S{16,}",
        ];
        sources
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect::<Vec<_>>()
    });
    patterns.iter().any(|re| re.is_match(text))
}

impl SessionUi {
    /// Whether prompt history is live for this session (daemon supports it and
    /// the environment has not turned it off).
    fn prompt_history_active(&self) -> bool {
        !disabled_by_env()
            && self
                .client
                .supports_server_capability(PROMPT_HISTORY_CAPABILITY)
    }

    /// Persist one submitted prompt to the project's history (fire-and-forget).
    /// Call it beside `editor.add_to_history` at every submit site.
    pub(crate) fn record_prompt_history(&self, text: &str) {
        if text.trim().is_empty() || !self.prompt_history_active() {
            return;
        }
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let display = text.to_string();
        tokio::spawn(async move {
            let _ = client
                .request_ok(DaemonCommand::AppendPromptHistory {
                    id: None,
                    active_session_id,
                    display,
                    rest: Map::default(),
                })
                .await;
        });
    }

    /// Record a promoted draft (from the hold-cleared slot), applying the
    /// sensitive-content gate and dedup'ing against the tail of what Up
    /// would recall. The daemon dedups on load too, so a duplicate record
    /// is harmless; this just keeps the write path quiet when a user
    /// repeatedly clears the same text.
    pub(super) fn record_promoted_history(&self, text: &str, editor_head: Option<&str>) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if is_sensitive_prompt(text) {
            return;
        }
        if editor_head.is_some_and(|head| head.trim() == trimmed) {
            return;
        }
        self.record_prompt_history(text);
    }

    /// Hold the newly cleared draft in the slot, and promote whatever the
    /// slot held BEFORE into persistent history. Mirrors CC's
    /// `holdCleared(prev, next, { intoHistory: true })`: the current
    /// draft stays reachable via Up (the slot restores IMMEDIATELY), but
    /// the previous slot's text survives beyond this session.
    pub(super) fn hold_cleared_draft(&mut self, view: &mut AgentView, new_draft: ClearedDraft) {
        if let Some(prev) = self.cleared_draft.take() {
            let head = view.editor.get_history().first().map(String::as_str);
            self.record_promoted_history(&prev.text, head);
        }
        self.cleared_draft = Some(new_draft);
    }

    /// Flush the hold-cleared slot to persistent history on the exit
    /// path. Called from the two detach-for-exit sites in
    /// `interactive/run.rs`; the daemon's append is fire-and-forget, so
    /// the exit deadline stays bounded.
    pub(crate) fn flush_cleared_draft_on_exit(&mut self) {
        let Some(prev) = self.cleared_draft.take() else {
            return;
        };
        // No view available here; the daemon dedups newest-wins on load,
        // so skipping client-side dedup is fine.
        self.record_promoted_history(&prev.text, None);
    }

    /// Seed the editor's up-arrow ring from the project's persisted history on
    /// attach. Best effort: a miss or timeout leaves recall empty rather than
    /// failing the attach.
    pub(crate) async fn seed_prompt_history(&self, view: &mut AgentView) {
        if !self.prompt_history_active() {
            return;
        }
        let Ok(data) = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetPromptHistory {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    limit: None,
                    rest: Map::default(),
                },
            )
            .await
        else {
            return;
        };
        let entries: Vec<String> = data
            .get("entries")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("display").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !entries.is_empty() {
            view.editor.seed_history(&entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prompt_is_not_sensitive() {
        assert!(!is_sensitive_prompt("fix the login bug please"));
        assert!(!is_sensitive_prompt(""));
        assert!(!is_sensitive_prompt("run `cargo test -p pa-tui`"));
    }

    #[test]
    fn pem_private_key_header_is_sensitive() {
        let text = "here is my key\n-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJB...";
        assert!(is_sensitive_prompt(text));
    }

    #[test]
    fn openssh_private_key_header_is_sensitive() {
        let text = "-----BEGIN OPENSSH PRIVATE KEY-----";
        assert!(is_sensitive_prompt(text));
    }

    #[test]
    fn aws_access_key_id_is_sensitive() {
        assert!(is_sensitive_prompt(
            "my key is AKIAIOSFODNN7EXAMPLE and ..."
        ));
        assert!(is_sensitive_prompt("ASIAIOSFODNN7EXAMPLE rotate soon"));
    }

    #[test]
    fn anthropic_token_is_sensitive() {
        assert!(is_sensitive_prompt(
            "ANTHROPIC_API_KEY=sk-ant-apitoken-abcdefghij1234567890"
        ));
    }

    #[test]
    fn github_token_prefix_is_sensitive() {
        assert!(is_sensitive_prompt(
            "export GH=ghp_abcdefghijklmnopqrstuvwxyz1234"
        ));
    }

    #[test]
    fn env_line_with_secret_shape_is_sensitive() {
        assert!(is_sensitive_prompt(
            "DATABASE_PASSWORD=verylongopaquesecretvalue12345"
        ));
        assert!(is_sensitive_prompt("API_KEY=aaaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn short_env_line_is_not_sensitive() {
        // A short right-hand side (< 16) isn't the "pasted credential"
        // shape we're guarding against; the user may be editing a
        // template or explaining the format in English.
        assert!(!is_sensitive_prompt("API_KEY=short"));
    }
}
