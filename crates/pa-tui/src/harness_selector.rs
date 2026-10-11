//! The `/harness` selector (#1118): the session's continual harness
//! entries as a checkbox list, Enter or Space enabling or disabling the
//! selected one in place. The daemon owns the stores: the selector opens
//! on the `/harness list` result and every toggle runs
//! `/harness enable|disable <key>`, whose result (a model-invisible,
//! off-transcript row) carries the refreshed list back.

use serde_json::Value;

use crate::Line;
use crate::config_selector::{ConfigSelector, SelectorAction, SelectorKind, SelectorRow};
use crate::keybindings::KeybindingsManager;
use crate::theme::Theme;

/// One key press while the selector is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessSelectorAction {
    /// Run `/harness enable|disable <key>` (the row is already flipped).
    Toggle { key: String, enabled: bool },
    /// Esc or Ctrl+C: close.
    Close,
    /// Navigation or filter editing only.
    None,
}

/// One `/harness` result row decoded for the client: the user-facing text
/// and, when the row carries it, the refreshed entry list.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessResult {
    pub text: String,
    pub success: bool,
    pub entries: Option<Vec<HarnessRowEntry>>,
}

/// One entry of the list (`details.harness.entries[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessRowEntry {
    pub key: String,
    pub scope: String,
    pub kind: String,
    pub id: String,
    pub title: String,
    pub enabled: bool,
}

/// Decode a `session_slash_command_result` custom message of the
/// `/harness` command; `None` for every other row.
#[must_use]
pub fn harness_result(message: &Value) -> Option<HarnessResult> {
    if message.get("customType").and_then(Value::as_str)
        != Some(pa_types::slash_commands::SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE)
    {
        return None;
    }
    let details = message.get("details")?;
    if details
        .get("command")
        .and_then(|command| command.get("name"))
        .and_then(Value::as_str)
        != Some("harness")
    {
        return None;
    }
    let text = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        _ => crate::custom_message::custom_content_text(message),
    };
    let entries = details
        .get("harness")
        .and_then(|harness| harness.get("entries"))
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(parse_entry).collect());
    Some(HarnessResult {
        text,
        success: details.get("success").and_then(Value::as_bool) != Some(false),
        entries,
    })
}

fn parse_entry(entry: &Value) -> Option<HarnessRowEntry> {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
    let (scope, kind, id) = (text("scope")?, text("kind")?, text("id")?);
    Some(HarnessRowEntry {
        key: format!("{scope}:{kind}:{id}"),
        title: text("title").unwrap_or_else(|| id.clone()),
        enabled: entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        scope,
        kind,
        id,
    })
}

/// The selector over one entry list; the config selector owns filtering,
/// navigation, and rendering.
#[derive(Debug)]
pub struct HarnessSelector {
    selector: ConfigSelector,
    entries: Vec<HarnessRowEntry>,
}

impl HarnessSelector {
    /// The selector over `entries`, grouped by scope (local first).
    #[must_use]
    pub fn new(entries: Vec<HarnessRowEntry>) -> Self {
        let mut rows = Vec::new();
        for (scope, heading) in [("local", "Local (this session)"), ("global", "Global")] {
            let scoped: Vec<&HarnessRowEntry> = entries
                .iter()
                .filter(|entry| entry.scope == scope)
                .collect();
            if scoped.is_empty() {
                continue;
            }
            rows.push(SelectorRow::Group(heading.to_string()));
            rows.extend(scoped.into_iter().map(|entry| SelectorRow::Item {
                key: entry.key.clone(),
                label: entry.title.clone(),
                checked: entry.enabled,
                type_label: entry.kind.clone(),
                path: entry.id.clone(),
            }));
        }
        Self {
            selector: ConfigSelector::with_kind(rows, SelectorKind::Harness),
            entries,
        }
    }

    /// Fold a refreshed list in place (the selection and filter stay); a
    /// changed entry set rebuilds the list.
    pub fn refresh(&mut self, entries: Vec<HarnessRowEntry>) {
        let same_set = entries.len() == self.entries.len()
            && entries
                .iter()
                .zip(&self.entries)
                .all(|(new, old)| new.key == old.key);
        if same_set {
            for entry in &entries {
                self.selector.set_checked(&entry.key, entry.enabled);
            }
            self.entries = entries;
        } else {
            *self = Self::new(entries);
        }
    }

    /// Undo an optimistic flip the daemon refused.
    pub fn revert(&mut self, key: &str, enabled: bool) {
        self.selector.set_checked(key, !enabled);
    }

    /// The enabled flag the selector currently shows for `key`.
    #[must_use]
    pub fn checked(&self, key: &str) -> Option<bool> {
        self.selector.checked(key)
    }

    /// One bracketed paste into the filter.
    pub fn paste(&mut self, text: &str) {
        self.selector.paste(text);
    }

    /// One key id: Enter/Space flips the selected entry and reports it.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> HarnessSelectorAction {
        if key == "ctrl+c" {
            return HarnessSelectorAction::Close;
        }
        match self.selector.handle_key(key, kb) {
            Some(SelectorAction::Close | SelectorAction::Exit) => HarnessSelectorAction::Close,
            Some(SelectorAction::Toggle { key, enabled }) => {
                HarnessSelectorAction::Toggle { key, enabled }
            }
            None => HarnessSelectorAction::None,
        }
    }

    /// The selector's rendered frame.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        self.selector.render(theme, width, kb)
    }
}

/// The session command a toggle sends.
#[must_use]
pub fn toggle_command(key: &str, enabled: bool) -> String {
    format!(
        "/harness {} {key}",
        if enabled { "enable" } else { "disable" }
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::theme::ColorMode;

    fn result_row(content: &str, entries: &Value) -> Value {
        json!({
            "role": "custom",
            "customType": "session_slash_command_result",
            "content": content,
            "display": false,
            "details": {
                "command": { "name": "harness", "args": "", "text": "/harness" },
                "success": true,
                "severity": "info",
                "harness": { "entries": entries, "changed": null }
            }
        })
    }

    fn entries(reviewer_enabled: bool) -> Value {
        json!([
            { "scope": "local", "kind": "memory", "id": "notes", "title": "Notes", "enabled": true, "path": "general", "version": 1 },
            { "scope": "global", "kind": "subagent", "id": "reviewer", "title": "API reviewer", "enabled": reviewer_enabled, "path": "general", "version": 3 }
        ])
    }

    #[test]
    fn a_harness_result_row_decodes_its_entries() {
        let decoded = harness_result(&result_row(
            "Continual harness entries: ...",
            &entries(false),
        ))
        .expect("a harness result");
        assert!(decoded.success);
        assert_eq!(
            decoded.entries.unwrap(),
            vec![
                HarnessRowEntry {
                    key: "local:memory:notes".to_string(),
                    scope: "local".to_string(),
                    kind: "memory".to_string(),
                    id: "notes".to_string(),
                    title: "Notes".to_string(),
                    enabled: true,
                },
                HarnessRowEntry {
                    key: "global:subagent:reviewer".to_string(),
                    scope: "global".to_string(),
                    kind: "subagent".to_string(),
                    id: "reviewer".to_string(),
                    title: "API reviewer".to_string(),
                    enabled: false,
                },
            ]
        );
        // Another command's result row is not ours.
        let mut other = result_row("Plan mode is on.", &json!([]));
        other["details"]["command"]["name"] = json!("plan");
        assert_eq!(harness_result(&other), None);
    }

    #[test]
    fn enter_toggles_the_selected_entry_and_the_result_settles_it() {
        let kb = KeybindingsManager::new();
        let decoded = harness_result(&result_row("", &entries(true))).unwrap();
        let mut selector = HarnessSelector::new(decoded.entries.unwrap());
        assert_eq!(
            selector.handle_key("down", &kb),
            HarnessSelectorAction::None
        );
        assert_eq!(
            selector.handle_key("enter", &kb),
            HarnessSelectorAction::Toggle {
                key: "global:subagent:reviewer".to_string(),
                enabled: false
            }
        );
        assert_eq!(
            toggle_command("global:subagent:reviewer", false),
            "/harness disable global:subagent:reviewer"
        );
        assert_eq!(selector.checked("global:subagent:reviewer"), Some(false));
        // A refused toggle reverts; the refreshed list is authoritative.
        selector.revert("global:subagent:reviewer", false);
        assert_eq!(selector.checked("global:subagent:reviewer"), Some(true));
        let refreshed = harness_result(&result_row("", &entries(false))).unwrap();
        selector.refresh(refreshed.entries.unwrap());
        assert_eq!(selector.checked("global:subagent:reviewer"), Some(false));
        assert_eq!(
            selector.handle_key("escape", &kb),
            HarnessSelectorAction::Close
        );
        let frame = selector.render(&Theme::builtin("prime", ColorMode::TrueColor), 80, &kb);
        let text: Vec<String> = frame
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect();
        assert!(
            text.iter().any(|row| row.contains("Continual Harness")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("API reviewer")),
            "{text:?}"
        );
    }
}
