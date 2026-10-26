//! Facts about a live session that features read but do not own: its
//! conversation log (Claude Code's `transcript_path`) and its permission
//! mode (Claude Code's `permission_mode`). The session engine records the
//! log when it builds a session; whoever owns the mode (the permissions
//! feature, the native plan mode) records it when it changes. The registry
//! is bounded: the oldest sessions' facts are dropped first.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

/// The most sessions whose facts are kept.
const MAX_SESSIONS: usize = 256;

#[derive(Debug, Clone, Default)]
struct Facts {
    transcript_path: Option<PathBuf>,
    permission_mode: Option<String>,
    /// The `hooks` blocks the session's model-visible Claude Code skills
    /// declared, as `(skill name, hooks value)`; empty when none did.
    skill_hooks: Vec<(String, serde_json::Value)>,
}

#[derive(Default)]
struct Registry {
    facts: HashMap<String, Facts>,
    /// Session ids, oldest first.
    order: VecDeque<String>,
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn update(session_id: &str, change: impl FnOnce(&mut Facts)) {
    let mut registry = registry();
    if !registry.facts.contains_key(session_id) {
        if registry.order.len() >= MAX_SESSIONS {
            if let Some(oldest) = registry.order.pop_front() {
                registry.facts.remove(&oldest);
            }
        }
        registry.order.push_back(session_id.to_string());
    }
    change(registry.facts.entry(session_id.to_string()).or_default());
}

/// Record session `session_id`'s conversation log.
pub fn set_session_transcript_path(session_id: &str, path: Option<PathBuf>) {
    update(session_id, |facts| facts.transcript_path = path);
}

/// Session `session_id`'s conversation log, when it persists one.
#[must_use]
pub fn session_transcript_path(session_id: &str) -> Option<PathBuf> {
    registry()
        .facts
        .get(session_id)
        .and_then(|facts| facts.transcript_path.clone())
}

/// Record session `session_id`'s permission mode in Claude Code's spelling
/// (`default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk`).
pub fn set_session_permission_mode(session_id: &str, mode: &str) {
    update(session_id, |facts| {
        facts.permission_mode = Some(mode.to_string());
    });
}

/// Session `session_id`'s permission mode; `default` when nothing recorded
/// one.
#[must_use]
pub fn session_permission_mode(session_id: &str) -> String {
    registry()
        .facts
        .get(session_id)
        .and_then(|facts| facts.permission_mode.clone())
        .unwrap_or_else(|| "default".to_string())
}

/// Record the `hooks` blocks declared by session `session_id`'s model-visible
/// Claude Code skills, as `(skill name, hooks value)` pairs. The session engine
/// records these once it has resolved the visible skills (trust and shadowing
/// already applied); the hooks feature reads them when it builds the session's
/// hooks and bounds-checks each block.
pub fn set_session_skill_hooks(session_id: &str, hooks: Vec<(String, serde_json::Value)>) {
    update(session_id, |facts| facts.skill_hooks = hooks);
}

/// The `hooks` blocks session `session_id`'s model-visible skills declared;
/// empty when none did or the session recorded none.
#[must_use]
pub fn session_skill_hooks(session_id: &str) -> Vec<(String, serde_json::Value)> {
    registry()
        .facts
        .get(session_id)
        .map(|facts| facts.skill_hooks.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_recorded_per_session_and_bounded() {
        assert_eq!(session_permission_mode("facts-a"), "default");
        assert_eq!(session_transcript_path("facts-a"), None);
        set_session_transcript_path("facts-a", Some(PathBuf::from("/s/a.jsonl")));
        set_session_permission_mode("facts-a", "plan");
        assert_eq!(
            session_transcript_path("facts-a"),
            Some(PathBuf::from("/s/a.jsonl"))
        );
        assert_eq!(session_permission_mode("facts-a"), "plan");
        assert_eq!(session_permission_mode("facts-b"), "default");
        for n in 0..=MAX_SESSIONS {
            set_session_permission_mode(&format!("facts-fill-{n}"), "dontAsk");
        }
        assert_eq!(session_permission_mode("facts-a"), "default");
        assert!(registry().facts.len() <= MAX_SESSIONS);
    }

    #[test]
    fn skill_hooks_are_recorded_per_session() {
        assert!(session_skill_hooks("sh-a").is_empty());
        let hooks = vec![(
            "deploy".to_string(),
            serde_json::json!({ "PreToolUse": [] }),
        )];
        set_session_skill_hooks("sh-a", hooks.clone());
        assert_eq!(session_skill_hooks("sh-a"), hooks);
        assert!(session_skill_hooks("sh-b").is_empty());
    }
}
