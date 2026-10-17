//! The policy end to end: settings files on disk, layering and trust,
//! precedence, compound commands, modes, the feature's verdicts and the
//! persistence of an "always allow" answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_core::features::{
    FeatureVerdict,
    HostRequestObservation,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
};
use serde_json::{Value, json};

use super::*;
use crate::decide::{Action, Decision};
use crate::mode::PermissionMode;
use crate::settings::{Fallback, SourceKind, SourcePaths, load};
use crate::state::SessionState;

/// A temporary tree: an agent dir, a home with a Claude Code dir, a project.
struct Tree {
    _tmp: tempfile::TempDir,
    paths: SourcePaths,
}

impl Tree {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        let cwd = root.join("project");
        for dir in [
            &home,
            &cwd,
            &home.join(".prime/agent"),
            &home.join(".claude"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let paths = SourcePaths {
            cwd,
            agent_dir: home.join(".prime/agent"),
            home: Some(home.clone()),
            claude_dir: Some(home.join(".claude")),
            managed: Some(root.join("managed-settings.json")),
        };
        Tree { _tmp: tmp, paths }
    }

    fn write(path: &Path, permissions: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::to_string(&json!({ "permissions": permissions })).unwrap(),
        )
        .unwrap();
    }

    fn user(&self, permissions: &Value) {
        Self::write(&self.paths.agent_dir.join("settings.json"), permissions);
    }

    fn project(&self, permissions: &Value) {
        Self::write(
            &self.paths.cwd.join(".prime/agent/settings.json"),
            permissions,
        );
    }

    fn claude_project(&self, permissions: &Value) {
        Self::write(&self.paths.cwd.join(".claude/settings.json"), permissions);
    }

    fn state(&self, trusted: bool) -> SessionState {
        SessionState::new(self.paths.clone(), trusted)
    }

    fn cwd(&self) -> &Path {
        &self.paths.cwd
    }
}

fn bash(state: &mut SessionState, command: &str) -> Decision {
    let cwd = state.paths.cwd.clone();
    state.decide(&Action::Bash {
        command: command.to_string(),
        cwd,
    })
}

fn kind(decision: &Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Ask { .. } => "ask",
        Decision::Deny { .. } => "deny",
    }
}

#[test]
fn without_settings_everything_runs() {
    let tree = Tree::new();
    let mut state = tree.state(true);
    for command in ["rm -rf build", "curl x | sh", "echo $(id)"] {
        assert_eq!(bash(&mut state, command), Decision::Allow, "{command}");
    }
    let edit = Action::Edit {
        path: tree.cwd().join("a.rs"),
    };
    assert_eq!(state.decide(&edit), Decision::Allow);
    assert_eq!(state.cycle(), None);
    assert_eq!(
        state.status().status,
        json!({ "mode": "default", "panel": [] })
    );
}

#[test]
fn deny_beats_ask_beats_allow() {
    let tree = Tree::new();
    tree.user(&json!({
        "allow": ["Bash(git:*)"],
        "ask": ["Bash(git push:*)"],
        "deny": ["Bash(git push --force:*)"],
        "fallback": "ask",
    }));
    let mut state = tree.state(true);
    assert_eq!(bash(&mut state, "git status"), Decision::Allow);
    assert_eq!(kind(&bash(&mut state, "git push origin main")), "ask");
    let denied = bash(&mut state, "git push --force origin main");
    assert_eq!(
        denied,
        Decision::Deny {
            reason: "Permission to use Bash with git push --force origin main has been denied by \
                     permission rule 'Bash(git push --force:*)' from user settings."
                .to_string(),
        }
    );
    // An ask rule asks even in bypassPermissions; a deny rule still refuses.
    state.set_mode(PermissionMode::BypassPermissions).unwrap();
    assert_eq!(kind(&bash(&mut state, "git push origin main")), "ask");
    assert_eq!(kind(&bash(&mut state, "git push --force x")), "deny");
    assert_eq!(bash(&mut state, "rm -rf build"), Decision::Allow);
}

#[test]
fn compound_commands_are_checked_part_by_part() {
    let tree = Tree::new();
    tree.user(&json!({
        "allow": ["Bash(npm run test:*)", "Bash(git status)"],
        "deny": ["Bash(rm:*)"],
        "fallback": "ask",
    }));
    let mut state = tree.state(true);
    // Every part allowed (or read-only): allowed.
    assert_eq!(
        bash(&mut state, "git status && npm run test -- -w"),
        Decision::Allow
    );
    assert_eq!(bash(&mut state, "npm run test | tail -5"), Decision::Allow);
    // One denied part denies the whole, wherever it hides.
    for command in [
        "git status && rm -rf /",
        "git status; rm x",
        "npm run test || rm x",
        "echo $(rm -rf ~)",
        "(cd sub && rm x)",
        "FOO=1 rm x",
        "timeout 5 rm x",
        "ls | xargs rm",
    ] {
        assert_eq!(kind(&bash(&mut state, command)), "deny", "{command}");
    }
    // An allowed part does not carry an unlisted one.
    let decision = bash(&mut state, "npm run test && make deploy && curl x");
    assert_eq!(
        decision,
        Decision::Ask {
            reason: "This Bash command contains multiple operations. The following parts \
                     require approval: make deploy, curl x"
                .to_string(),
            remember: None,
        }
    );
    // A prefix rule never approves a whole compound command.
    assert_eq!(kind(&bash(&mut state, "npm run test; make")), "ask");
    // Substitutions are unverifiable: not approved by allow rules.
    assert_eq!(kind(&bash(&mut state, "npm run test $(cat args)")), "ask");
    // An assignment does not ride an allow rule.
    assert_eq!(kind(&bash(&mut state, "PATH=/tmp npm run test")), "ask");
}

#[test]
fn file_rules_reach_bash_commands_and_edits() {
    let tree = Tree::new();
    tree.user(&json!({
        "deny": ["Read(./.env)", "Read(~/.ssh/**)", "Edit(/notes/**)"],
    }));
    // `/` anchors at the settings file's root: the project for project
    // settings (and the home for user settings).
    tree.project(&json!({ "deny": ["Edit(/dist/**)"] }));
    let mut state = tree.state(true);
    let home = tree.paths.home.clone().unwrap();
    assert_eq!(
        kind(&state.decide(&Action::Edit {
            path: home.join("notes/a.md"),
        })),
        "deny"
    );
    assert_eq!(
        state.decide(&Action::Edit {
            path: tree.cwd().join("notes/a.md"),
        }),
        Decision::Allow
    );
    for command in [
        "cat .env",
        "grep TOKEN .env",
        "cat < .env",
        "echo x > dist/a.js",
        "rm -rf dist/old",
        "cp src/a dist/a",
        "sed -i s/a/b/ dist/x.js",
        "cat ~/.ssh/id_ed25519",
    ] {
        assert_eq!(kind(&bash(&mut state, command)), "deny", "{command}");
    }
    for command in [
        "cat src/.env.example",
        "grep .env README.md",
        "sed s/a/b/ dist/x.js",
    ] {
        assert_eq!(bash(&mut state, command), Decision::Allow, "{command}");
    }
    let edit = |path: PathBuf| Action::Edit { path };
    assert_eq!(
        kind(&state.decide(&edit(tree.cwd().join("dist/app.js")))),
        "deny"
    );
    assert_eq!(
        state.decide(&edit(tree.cwd().join("src/app.js"))),
        Decision::Allow
    );
}

#[test]
fn modes_change_what_runs_without_asking() {
    let tree = Tree::new();
    tree.user(&json!({ "fallback": "ask", "additionalDirectories": ["~/extra"] }));
    let mut state = tree.state(true);
    let inside = Action::Edit {
        path: tree.cwd().join("src/a.rs"),
    };
    let extra = Action::Edit {
        path: tree.paths.home.as_ref().unwrap().join("extra/b.rs"),
    };
    let outside = Action::Edit {
        path: PathBuf::from("/etc/hosts"),
    };
    // Default with the ask fallback: edits ask, offering a directory rule.
    assert_eq!(
        state.decide(&inside),
        Decision::Ask {
            reason: "Prime Agent requested permissions to use Edit, but you haven't granted it \
                     yet."
                .to_string(),
            remember: Some("Edit(./src/**)".to_string()),
        }
    );
    // Read-only commands run; others ask with a prefix rule to remember.
    assert_eq!(
        bash(&mut state, "ls -la && git log --oneline | head"),
        Decision::Allow
    );
    assert_eq!(
        bash(&mut state, "npm test --watch"),
        Decision::Ask {
            reason: "Prime Agent requested permissions to use Bash, but you haven't granted it \
                     yet."
                .to_string(),
            remember: Some("Bash(npm test *)".to_string()),
        }
    );
    // acceptEdits: edits and file commands in the working directories.
    state.set_mode(PermissionMode::AcceptEdits).unwrap();
    assert_eq!(state.decide(&inside), Decision::Allow);
    assert_eq!(state.decide(&extra), Decision::Allow);
    assert_eq!(kind(&state.decide(&outside)), "ask");
    assert_eq!(
        bash(&mut state, "mkdir -p out && touch out/a"),
        Decision::Allow
    );
    assert_eq!(kind(&bash(&mut state, "rm -rf /tmp/x")), "ask");
    assert_eq!(kind(&bash(&mut state, "npm test")), "ask");
    // plan: edits refused, non-read-only commands ask.
    state.set_mode(PermissionMode::Plan).unwrap();
    assert_eq!(kind(&state.decide(&inside)), "deny");
    assert_eq!(bash(&mut state, "git diff"), Decision::Allow);
    assert_eq!(kind(&bash(&mut state, "touch a")), "ask");
    // dontAsk: what would ask is refused.
    state.set_mode(PermissionMode::DontAsk).unwrap();
    assert_eq!(kind(&state.decide(&inside)), "deny");
    assert_eq!(kind(&bash(&mut state, "npm test")), "deny");
    assert_eq!(bash(&mut state, "ls"), Decision::Allow);
    // bypassPermissions: everything runs.
    state.set_mode(PermissionMode::BypassPermissions).unwrap();
    assert_eq!(state.decide(&outside), Decision::Allow);
}

#[test]
fn web_and_mcp_rules_match_by_domain_and_server() {
    let tree = Tree::new();
    tree.user(&json!({
        "allow": ["WebFetch(domain:docs.rs)", "mcp__github"],
        "deny": ["WebFetch(domain:*.evil.test)", "mcp__github__delete_repo", "WebSearch"],
        "fallback": "ask",
    }));
    let mut state = tree.state(true);
    let fetch = |url: &str| Action::WebFetch {
        url: url.to_string(),
    };
    let mcp = |tool: &str| Action::Mcp {
        server: "github".to_string(),
        tool: tool.to_string(),
    };
    assert_eq!(
        state.decide(&fetch("https://docs.rs/regex")),
        Decision::Allow
    );
    assert_eq!(kind(&state.decide(&fetch("https://a.evil.test/x"))), "deny");
    assert_eq!(
        state.decide(&fetch("https://example.com/")),
        Decision::Ask {
            reason: "Prime Agent requested permissions to use WebFetch, but you haven't granted \
                     it yet."
                .to_string(),
            remember: Some("WebFetch(domain:example.com)".to_string()),
        }
    );
    assert_eq!(state.decide(&mcp("get_issue")), Decision::Allow);
    assert_eq!(kind(&state.decide(&mcp("delete_repo"))), "deny");
    assert_eq!(
        kind(&state.decide(&Action::WebSearch {
            query: "rust".to_string()
        })),
        "deny"
    );
    // Tool-wide rules match the model's tool calls by name.
    tree.user(&json!({ "deny": ["python_repl"] }));
    state.reload();
    assert_eq!(
        kind(&state.decide(&Action::Tool {
            name: "python_repl".to_string()
        })),
        "deny"
    );
}

#[test]
fn an_untrusted_project_can_only_tighten() {
    let tree = Tree::new();
    tree.project(&json!({
        "allow": ["Bash(curl:*)"],
        "deny": ["Bash(make deploy)"],
        "defaultMode": "acceptEdits",
        "additionalDirectories": ["/"],
    }));
    tree.user(&json!({ "fallback": "ask" }));
    let untrusted = load(&tree.paths, false);
    assert_eq!(untrusted.default_mode, PermissionMode::Default);
    assert!(untrusted.additional_directories.is_empty());
    assert_eq!(
        untrusted
            .rules
            .iter()
            .map(|rule| (rule.behavior, rule.value.to_string(), rule.source))
            .collect::<Vec<_>>(),
        vec![(
            RuleBehavior::Deny,
            "Bash(make deploy)".to_string(),
            SourceKind::Project
        )]
    );
    assert_eq!(untrusted.problems.len(), 3, "{:?}", untrusted.problems);
    let trusted = load(&tree.paths, true);
    assert_eq!(trusted.default_mode, PermissionMode::AcceptEdits);
    assert_eq!(trusted.rules.len(), 2);
    // A project never defaults to bypass; only the user chooses the fallback.
    tree.project(&json!({ "defaultMode": "bypassPermissions", "fallback": "allow" }));
    let settings = load(&tree.paths, true);
    assert_eq!(settings.default_mode, PermissionMode::Default);
    assert_eq!(settings.fallback, Fallback::Ask);
    assert!(!settings.bypass_available);
}

#[test]
fn claude_code_settings_are_read_only_when_the_user_opts_in() {
    let tree = Tree::new();
    tree.claude_project(&json!({ "deny": ["Bash(rm:*)"] }));
    Tree::write(
        &tree
            .paths
            .claude_dir
            .as_ref()
            .unwrap()
            .join("settings.json"),
        &json!({ "ask": ["WebFetch"] }),
    );
    let mut state = tree.state(true);
    assert_eq!(bash(&mut state, "rm x"), Decision::Allow);
    tree.user(&json!({ "claudeCodeSettings": true }));
    state.refresh();
    assert_eq!(kind(&bash(&mut state, "rm x")), "deny");
    let sources: Vec<SourceKind> = state
        .settings
        .rules
        .iter()
        .map(|rule| rule.source)
        .collect();
    assert_eq!(
        sources,
        vec![SourceKind::ClaudeProject, SourceKind::ClaudeUser]
    );
    // Managed settings may restrict the rules to their own.
    Tree::write(
        tree.paths.managed.as_ref().unwrap(),
        &json!({ "allowManagedPermissionRulesOnly": true, "deny": ["WebSearch"] }),
    );
    state.refresh();
    let rules: Vec<String> = state
        .settings
        .rules
        .iter()
        .map(|rule| rule.value.to_string())
        .collect();
    assert_eq!(rules, vec!["WebSearch"]);
}

#[test]
fn invalid_rules_are_skipped_with_a_problem() {
    let tree = Tree::new();
    tree.user(&json!({ "deny": ["Bash(git", "bash(ls)", "Bash(rm:*)"] }));
    let settings = load(&tree.paths, true);
    assert_eq!(settings.rules.len(), 1);
    assert_eq!(settings.problems.len(), 2, "{:?}", settings.problems);
    assert!(settings.problems[1].contains("Tool names must start with uppercase"));
}

#[test]
fn rules_persist_to_the_local_settings_file() {
    let tree = Tree::new();
    tree.user(&json!({ "fallback": "ask" }));
    let mut state = tree.state(true);
    assert_eq!(kind(&bash(&mut state, "npm test")), "ask");
    assert_eq!(
        state.add_rule(RuleBehavior::Allow, "Bash(npm test *)"),
        crate::state::Saved::LocalFile
    );
    assert_eq!(bash(&mut state, "npm test"), Decision::Allow);
    let local: Value =
        serde_json::from_str(&std::fs::read_to_string(tree.paths.local_file()).unwrap()).unwrap();
    assert_eq!(
        local,
        json!({ "permissions": { "allow": ["Bash(npm test *)"] } })
    );
    // Adding it again changes nothing; removing it takes it out.
    state.add_rule(RuleBehavior::Allow, "Bash(npm test *)");
    assert!(state.remove_rule("Bash(npm test *)").unwrap());
    assert_eq!(kind(&bash(&mut state, "npm test")), "ask");
    // An untrusted workspace keeps the rule for the session only.
    let mut untrusted = tree.state(false);
    assert!(matches!(
        untrusted.add_rule(RuleBehavior::Allow, "Bash(make *)"),
        crate::state::Saved::Session(_)
    ));
    assert_eq!(bash(&mut untrusted, "make all"), Decision::Allow);
}

#[test]
fn the_status_carries_the_indicator_and_the_cycle_commands() {
    let tree = Tree::new();
    tree.user(&json!({ "fallback": "ask" }));
    let mut state = tree.state(true);
    assert_eq!(
        state.status().status,
        json!({ "mode": "default", "panel": [], "cycle": ["/permissions mode acceptEdits"] })
    );
    state.set_mode(PermissionMode::AcceptEdits).unwrap();
    assert_eq!(
        state.status().status,
        json!({
            "mode": "acceptEdits",
            "panel": ["\u{25b8}\u{25b8} accept edits on (shift+tab to cycle)"],
            "cycle": ["/permissions mode plan", "/plan on"],
        })
    );
    state.set_mode(PermissionMode::Plan).unwrap();
    assert_eq!(
        state.status().status["cycle"],
        json!(["/plan off", "/permissions mode default"])
    );
    // With nothing asking, the key stays the plan toggle until a mode is set.
    let quiet = Tree::new();
    let mut state = quiet.state(true);
    assert_eq!(state.cycle(), None);
    state.set_mode(PermissionMode::Plan).unwrap();
    assert_eq!(
        state.status().status["cycle"],
        json!(["/plan off", "/permissions mode default"])
    );
}

#[test]
fn the_command_lists_rules_with_sources_and_changes_the_mode() {
    let tree = Tree::new();
    tree.user(&json!({ "deny": ["Bash(rm:*)"], "allow": ["WebSearch"] }));
    let mut state = tree.state(true);
    let (listing, changed) = command::run(&mut state, "").unwrap();
    assert!(!changed);
    assert!(
        listing.starts_with("Permission mode: default (Default); actions no rule covers: allow\n")
    );
    assert!(listing.contains("Deny:\n  Bash(rm:*)  (user settings)\nAsk: none\nAllow:\n  WebSearch  (user settings)\n"), "{listing}");
    assert!(listing.contains("Python code the kernel runs directly"));
    let (text, changed) = command::run(&mut state, "mode accept-edits").unwrap();
    assert!(changed);
    assert_eq!(text, "Permission mode set to acceptEdits (Accept edits).");
    assert!(command::run(&mut state, "mode warp").is_err());
    let (text, _) = command::run(&mut state, "check Bash(rm -rf x)").unwrap();
    assert!(text.starts_with("Bash(rm -rf x): denied: "), "{text}");
    let (text, _) = command::run(&mut state, "deny Read(./.env)").unwrap();
    assert_eq!(
        text,
        format!(
            "Added deny rule Read(./.env) to {}.",
            crate::settings::LOCAL_SETTINGS_FILE
        )
    );
    assert!(command::run(&mut state, "allow bash(ls)").is_err());
    let error = command::run(&mut state, "remove Bash(rm:*)").unwrap_err();
    assert!(error.contains("comes from user settings"), "{error}");
}

fn model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "m1", "name": "M1", "api": "test", "provider": "p1",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

fn feature(tree: &Tree, session_id: &str) -> (PermissionsFeature, Arc<SessionFeatureContext>) {
    let paths = tree.paths.clone();
    let feature = PermissionsFeature::with_resolver(Arc::new(move |_| (paths.clone(), true)));
    let context = Arc::new(SessionFeatureContext {
        agent_dir: tree.paths.agent_dir.clone(),
        cwd: tree.paths.cwd.clone(),
        session_id: session_id.to_string(),
        python_skill_import_names: Vec::new(),
        model: model(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    });
    (feature, context)
}

fn host(request_type: &str, data: Value) -> HostRequestObservation {
    HostRequestObservation {
        request_type: request_type.to_string(),
        data,
    }
}

#[tokio::test]
async fn the_feature_reviews_kernel_requests_and_tool_calls() {
    let tree = Tree::new();
    tree.user(&json!({ "deny": ["Bash(rm:*)", "Edit(./locked/**)", "mcp__github__delete_repo"] }));
    let (feature, context) = feature(&tree, "permissions-review");
    let review = |request: HostRequestObservation| feature.review_host_request(&context, &request);
    assert!(matches!(
        review(host("bash.run", json!({ "command": "ls && rm -rf x" }))).await,
        FeatureVerdict::Block(_)
    ));
    assert_eq!(
        review(host("bash.run", json!({ "command": "ls" }))).await,
        FeatureVerdict::Allow
    );
    assert!(matches!(
        review(host("file.edit", json!({ "path": "locked/a.txt" }))).await,
        FeatureVerdict::Block(_)
    ));
    assert!(matches!(
        review(host(
            "mcp.session.call_tool",
            json!({ "server": "github", "tool": "delete_repo" })
        ))
        .await,
        FeatureVerdict::Block(_)
    ));
    assert_eq!(
        review(host("rlm.run", json!({}))).await,
        FeatureVerdict::Allow
    );
    let call = ToolCallObservation {
        tool_call_id: "t1".to_string(),
        tool_name: "bash".to_string(),
        args: json!({ "command": "rm x" }),
    };
    assert!(matches!(
        feature.review_tool_call(&context, &call).await,
        FeatureVerdict::Block(_)
    ));
}

#[tokio::test]
async fn an_ask_without_a_surface_is_refused_and_an_always_allow_persists() {
    let tree = Tree::new();
    tree.user(&json!({ "fallback": "ask", "ask": ["Bash(git push:*)"] }));
    let (feature, context) = feature(&tree, "permissions-confirm");
    // An ask rule's question joins the other features' asks.
    assert!(matches!(
        feature
            .review_host_request(
                &context,
                &host("bash.run", json!({ "command": "git push" }))
            )
            .await,
        FeatureVerdict::Ask(_)
    ));
    // A fallback question is asked here; print mode (no surface) refuses.
    let FeatureVerdict::Block(reason) = feature
        .review_host_request(
            &context,
            &host("bash.run", json!({ "command": "npm test" })),
        )
        .await
    else {
        panic!("expected a refusal");
    };
    assert!(reason.ends_with("(it needs the user's confirmation, and no client can ask for it)"));
    // With a surface, "always allow" adds the offered rule to the local file.
    let _surface = pa_core::features::register_confirmation_surface(&context.session_id);
    let asking = {
        let context = Arc::clone(&context);
        let pending = feature.review_host_request(
            &context,
            &host("bash.run", json!({ "command": "npm test -- --ci" })),
        );
        tokio::spawn(pending)
    };
    let question = loop {
        if let Some(question) = pa_core::features::pending_confirmations(&context.session_id).pop()
        {
            break question;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(question.action, "Run `npm test -- --ci`");
    assert_eq!(question.remember.as_deref(), Some("Bash(npm test *)"));
    assert!(pa_core::features::answer_confirmation_remembering(
        &context.session_id,
        &question.id,
        true,
        true
    ));
    assert_eq!(asking.await.unwrap(), FeatureVerdict::Allow);
    assert_eq!(
        feature
            .review_host_request(
                &context,
                &host("bash.run", json!({ "command": "npm test" }))
            )
            .await,
        FeatureVerdict::Allow
    );
    let local = std::fs::read_to_string(tree.paths.local_file()).unwrap();
    assert!(local.contains("Bash(npm test *)"), "{local}");
}

#[tokio::test]
async fn the_command_runs_through_the_feature() {
    let tree = Tree::new();
    let (feature, context) = feature(&tree, "permissions-command");
    assert_eq!(
        feature
            .slash_commands()
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>(),
        vec!["permissions"]
    );
    assert!(
        feature
            .execute_slash_command(&context, "plan", "")
            .is_none()
    );
    let outcome = feature
        .execute_slash_command(&context, "permissions", "mode plan")
        .unwrap()
        .await
        .unwrap();
    assert!(outcome.text.starts_with("Permission mode set to plan"));
    // In plan mode the `edit` skill's write is refused.
    assert!(matches!(
        feature
            .review_host_request(&context, &host("file.edit", json!({ "path": "a.txt" })))
            .await,
        FeatureVerdict::Block(_)
    ));
}

/// A governed session reports its mode, rule counts and decision counts
/// when it ends (`permissions_session`); an ungoverned one sends nothing.
#[tokio::test]
async fn a_governed_session_reports_its_counts_when_it_ends() {
    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let telemetry = {
        let recorded = Arc::clone(&recorded);
        pa_core::features::FeatureTelemetry::new(move |name, properties| {
            recorded
                .lock()
                .unwrap()
                .push((name.to_string(), properties));
        })
    };
    let with_telemetry = |context: Arc<SessionFeatureContext>| {
        let mut context = (*context).clone();
        context.telemetry = Some(telemetry.clone());
        Arc::new(context)
    };

    let plain = Tree::new();
    let (plain_feature, context) = feature(&plain, "permissions-plain");
    let context = with_telemetry(context);
    let request = host("bash.run", json!({ "command": "ls" }));
    assert_eq!(
        plain_feature.review_host_request(&context, &request).await,
        FeatureVerdict::Allow
    );
    plain_feature.on_session_end(&context, "quit");
    assert!(recorded.lock().unwrap().is_empty());

    let tree = Tree::new();
    tree.user(&json!({ "allow": ["Bash(ls:*)"], "deny": ["Bash(rm:*)"] }));
    let (feature, context) = feature(&tree, "permissions-governed");
    let context = with_telemetry(context);
    for command in ["ls", "rm x"] {
        let request = host("bash.run", json!({ "command": command }));
        let _ = feature.review_host_request(&context, &request).await;
    }
    feature.on_session_end(&context, "quit");
    let recorded = recorded.lock().unwrap();
    let mut expected = pa_telemetry::Properties::new();
    for (key, value) in [
        ("mode", json!("default")),
        ("allow_rules", json!(1)),
        ("ask_rules", json!(0)),
        ("deny_rules", json!(1)),
        ("allowed_count", json!(1)),
        ("asked_count", json!(0)),
        ("denied_count", json!(1)),
        ("remembered_count", json!(0)),
    ] {
        expected.set(key, value);
    }
    assert_eq!(
        *recorded,
        vec![(PERMISSIONS_SESSION_EVENT.to_string(), expected)]
    );
}
