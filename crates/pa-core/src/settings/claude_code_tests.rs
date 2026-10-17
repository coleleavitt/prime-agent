//! The Claude Code layer through [`SettingsManager`]: each key Claude
//! Code's files contribute, behind the opt-in, under this agent's own
//! settings, and gated by workspace trust.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::SettingsManager;
use super::claude_code::{ClaudePaths, ClaudeScope};

struct Tree {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    agent_dir: PathBuf,
    paths: ClaudePaths,
}

fn write(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let agent_dir = dir.path().join("agent");
        let home = dir.path().join("home");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let paths = ClaudePaths {
            cwd: cwd.clone(),
            claude_dir: Some(home.join(".claude")),
            home: Some(home),
            managed: Some(dir.path().join("managed-settings.json")),
        };
        Tree {
            _dir: dir,
            cwd,
            agent_dir,
            paths,
        }
    }

    fn own(&self, value: &Value) {
        write(&self.agent_dir.join("settings.json"), value);
    }

    fn claude(&self, scope: ClaudeScope, value: &Value) {
        write(&self.paths.file(scope).unwrap(), value);
    }

    fn manager(&self) -> SettingsManager {
        SettingsManager::create_with_claude_paths(&self.cwd, &self.agent_dir, self.paths.clone())
    }

    fn trust(&self) {
        crate::workspace_trust::record(
            &self.cwd,
            &self.agent_dir,
            crate::workspace_trust::TrustDecision::Trusted,
        )
        .unwrap();
    }
}

#[test]
fn nothing_is_read_without_the_opt_in() {
    let tree = Tree::new();
    tree.own(&json!({}));
    tree.claude(
        ClaudeScope::User,
        &json!({
            "statusLine": { "type": "command", "command": "echo cc" },
            "outputStyle": "Explanatory",
            "model": "claude-sonnet-4-5",
            "env": { "A": "1" },
            "cleanupPeriodDays": 7,
            "disableAllHooks": true,
            "permissions": { "additionalDirectories": ["/shared"] },
        }),
    );
    let manager = tree.manager();
    assert!(manager.claude_code().is_none());
    assert_eq!(manager.get_status_line(), None);
    assert_eq!(manager.get_output_style(), None);
    assert_eq!(manager.get_default_model(), None);
    assert!(manager.get_claude_code_env().is_empty());
    assert_eq!(manager.disable_all_hooks(), None);
    assert_eq!(
        manager.get_session_archive_policy().max_age_days,
        Some(super::DEFAULT_SESSION_ARCHIVE_MAX_AGE_DAYS)
    );
    assert!(manager.get_additional_directories(&tree.cwd).is_empty());
}

#[test]
fn status_line_comes_from_claude_code_when_unset_and_disable_all_hooks_turns_it_off() {
    let tree = Tree::new();
    tree.own(&json!({ "claudeCodeSettings": true }));
    let line = json!({ "type": "command", "command": "echo cc", "padding": 1 });
    tree.claude(ClaudeScope::User, &json!({ "statusLine": line }));
    assert_eq!(tree.manager().get_status_line(), Some(&line));
    // The agent's own status line wins.
    let own = json!({ "type": "command", "command": "echo own" });
    tree.own(&json!({ "claudeCodeSettings": true, "statusLine": own }));
    assert_eq!(tree.manager().get_status_line(), Some(&own));
    tree.claude(
        ClaudeScope::User,
        &json!({ "statusLine": line, "disableAllHooks": true }),
    );
    assert_eq!(tree.manager().get_status_line(), None);
    assert_eq!(
        tree.manager().disable_all_hooks(),
        Some("Claude Code user settings")
    );
}

#[test]
fn a_project_status_line_waits_for_workspace_trust() {
    let tree = Tree::new();
    tree.own(&json!({ "claudeCodeSettings": true }));
    let line = json!({ "type": "command", "command": "./status.sh" });
    tree.claude(ClaudeScope::Project, &json!({ "statusLine": line }));
    let manager = tree.manager();
    assert_eq!(manager.get_status_line(), None);
    assert_eq!(
        manager.claude_code_problems(),
        vec!["Claude Code project settings: statusLine wait for workspace trust".to_string()]
    );
    tree.trust();
    assert_eq!(tree.manager().get_status_line(), Some(&line));
}

#[test]
fn output_style_and_model_fill_only_what_the_agent_leaves_unset() {
    let tree = Tree::new();
    tree.own(&json!({ "claudeCodeSettings": true }));
    tree.claude(
        ClaudeScope::User,
        &json!({ "outputStyle": "Explanatory", "model": "claude-opus-4-1[1m]" }),
    );
    let manager = tree.manager();
    assert_eq!(
        (
            manager.get_output_style(),
            manager.get_default_provider(),
            manager.get_default_model(),
            manager.default_model_from_claude_code(),
        ),
        (
            Some("Explanatory"),
            Some("anthropic"),
            Some("claude-opus-4-1"),
            true
        )
    );
    tree.own(&json!({
        "claudeCodeSettings": true,
        "outputStyle": "Learning",
        "defaultProvider": "openai",
        "defaultModel": "gpt-5",
    }));
    let manager = tree.manager();
    assert_eq!(
        (
            manager.get_output_style(),
            manager.get_default_provider(),
            manager.get_default_model(),
            manager.default_model_from_claude_code(),
        ),
        (Some("Learning"), Some("openai"), Some("gpt-5"), false)
    );
    // A provider alone keeps Claude Code's model out (the two go together).
    tree.own(&json!({ "claudeCodeSettings": true, "defaultProvider": "openai" }));
    assert_eq!(tree.manager().get_default_model(), None);
}

#[test]
fn a_saved_model_replaces_the_claude_code_one() {
    let tree = Tree::new();
    tree.own(&json!({ "claudeCodeSettings": true }));
    tree.claude(ClaudeScope::User, &json!({ "model": "claude-sonnet-4-5" }));
    let mut manager = tree.manager();
    manager
        .set_default_model_and_provider("openai", "gpt-5")
        .unwrap();
    assert_eq!(
        (
            manager.get_default_provider(),
            manager.get_default_model(),
            manager.default_model_from_claude_code(),
        ),
        (Some("openai"), Some("gpt-5"), false)
    );
}

#[test]
fn cleanup_period_days_sets_the_archive_age_when_the_agent_does_not() {
    let tree = Tree::new();
    tree.own(&json!({ "claudeCodeSettings": true }));
    tree.claude(ClaudeScope::User, &json!({ "cleanupPeriodDays": 9 }));
    assert_eq!(
        tree.manager().get_session_archive_policy().max_age_days,
        Some(9)
    );
    tree.own(&json!({ "claudeCodeSettings": true, "sessionArchiveMaxAgeDays": 4 }));
    assert_eq!(
        tree.manager().get_session_archive_policy().max_age_days,
        Some(4)
    );
}

#[test]
fn additional_directories_join_the_agents_own() {
    let tree = Tree::new();
    tree.own(&json!({
        "claudeCodeSettings": true,
        "permissions": { "additionalDirectories": ["/own"] },
    }));
    tree.claude(
        ClaudeScope::User,
        &json!({ "permissions": { "additionalDirectories": ["/cc", "/own"] } }),
    );
    tree.claude(
        ClaudeScope::Local,
        &json!({ "permissions": { "additionalDirectories": ["../sibling"] } }),
    );
    assert_eq!(
        tree.manager().get_additional_directories(&tree.cwd),
        vec![PathBuf::from("/own"), PathBuf::from("/cc")]
    );
    tree.trust();
    assert_eq!(
        tree.manager().get_additional_directories(&tree.cwd),
        vec![
            PathBuf::from("/own"),
            tree.cwd.join("../sibling"),
            PathBuf::from("/cc"),
        ]
    );
}

#[test]
fn env_is_the_layered_claude_code_environment() {
    let tree = Tree::new();
    tree.own(&json!({ "permissions": { "claudeCodeSettings": true } }));
    tree.claude(
        ClaudeScope::User,
        &json!({ "env": { "FEATURE": "user", "PATH": "/opt/bin:/usr/bin" } }),
    );
    tree.claude(
        ClaudeScope::Managed,
        &json!({ "env": { "FEATURE": "managed" } }),
    );
    let manager = tree.manager();
    assert_eq!(
        manager.get_claude_code_env(),
        vec![
            ("PATH".to_string(), "/opt/bin:/usr/bin".to_string()),
            ("FEATURE".to_string(), "managed".to_string()),
        ]
    );
    // A PATH override is reported, never silent.
    assert_eq!(
        manager.claude_code_problems(),
        vec![
            "Claude Code user settings: env sets PATH for the kernel and its commands".to_string()
        ]
    );
}
