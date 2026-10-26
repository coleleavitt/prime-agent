use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::mcp::{EnvValue, McpManager, McpManagerOptions};
use crate::workspace_trust::TrustDecision;

fn mcp_json(dir: &Path, servers: &Value) {
    write_json(&dir.join(MCP_JSON_FILE), &json!({ "mcpServers": servers }));
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// A temp tree: `root/` (the walk's stop) holding `repo/` (a git
/// repository, the session's cwd), `agent/` and Claude Code's files.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    agent_dir: PathBuf,
    claude: ClaudeCodeFiles,
    env: HashMap<String, String>,
    opt_in: bool,
    launch: Option<crate::settings::launch::LaunchSettings>,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let cwd = root.join("repo");
        let agent_dir = root.join("agent");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(root.join("claude-home").join(".claude")).unwrap();
        let claude = ClaudeCodeFiles {
            global_config: root.join("claude-home").join(".claude.json"),
            user_settings: root
                .join("claude-home")
                .join(".claude")
                .join("settings.json"),
        };
        Fixture {
            _tmp: tmp,
            root,
            cwd,
            agent_dir,
            claude,
            env: HashMap::from([
                ("PA_TEST_TOKEN".to_string(), "tok".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string()),
            ]),
            opt_in: false,
            launch: None,
        }
    }

    fn global_settings(&self, value: &Value) {
        write_json(&self.agent_dir.join("settings.json"), value);
    }

    fn project_settings(&self, value: &Value) {
        write_json(
            &self
                .cwd
                .join(crate::settings::CONFIG_DIR_NAME)
                .join("settings.json"),
            value,
        );
    }

    fn trust(&self) {
        crate::workspace_trust::record(&self.cwd, &self.agent_dir, TrustDecision::Trusted).unwrap();
    }

    fn load_in(&self, cwd: &Path) -> DeclaredServers {
        let settings = crate::settings::SettingsManager::create(cwd, &self.agent_dir);
        let env = |name: &str| self.env.get(name).cloned();
        DeclaredServers::load_with(&LoadInputs {
            cwd,
            agent_dir: &self.agent_dir,
            settings: &settings,
            claude_code: self.opt_in.then(|| self.claude.clone()),
            env: &env,
            walk_stop: Some(&self.root),
            launch: self.launch.as_ref(),
        })
    }

    fn load(&self) -> DeclaredServers {
        self.load_in(&self.cwd)
    }
}

fn stdio(command: &str, args: &[&str], env: &[(&str, &str)]) -> McpServerConfig {
    McpServerConfig::Stdio {
        command: command.to_string(),
        args: (!args.is_empty()).then(|| args.iter().map(ToString::to_string).collect()),
        cwd: None,
        env: (!env.is_empty()).then(|| {
            env.iter()
                .map(|(key, value)| (key.to_string(), EnvValue::Literal(value.to_string())))
                .collect()
        }),
        enabled: None,
        enabled_tools: None,
        disabled_tools: None,
        startup_timeout_ms: None,
        call_timeout_ms: None,
    }
}

fn http(url: &str, headers: &[(&str, &str)], oauth: bool) -> McpServerConfig {
    McpServerConfig::Http {
        url: url.to_string(),
        headers: (!headers.is_empty()).then(|| {
            headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        }),
        bearer_token_env_var: None,
        oauth: oauth.then_some(true),
        enabled: None,
        enabled_tools: None,
        disabled_tools: None,
        startup_timeout_ms: None,
        call_timeout_ms: None,
        client_id: None,
        callback_port: None,
    }
}

/// `(name, scope, approval)` of each shown declaration.
fn shown(declared: &DeclaredServers) -> Vec<(String, McpServerScope, Option<ProjectApproval>)> {
    declared
        .shown()
        .into_iter()
        .map(|server| (server.name.clone(), server.scope, server.approval))
        .collect()
}

fn warning(file: &Path, server: &str, message: &str, suggestion: Option<&str>) -> McpConfigIssue {
    McpConfigIssue {
        file: Some(file.display().to_string()),
        path: format!("mcpServers.{server}"),
        server: Some(server.to_string()),
        message: message.to_string(),
        suggestion: suggestion.map(str::to_string),
        severity: IssueSeverity::Warning,
    }
}

#[test]
fn mcp_json_entries_parse_and_expand_like_claude_code() {
    let fixture = Fixture::new();
    fixture.global_settings(&json!({ "enableAllProjectMcpServers": true }));
    mcp_json(
        &fixture.cwd,
        &json!({
            "files": {
                "command": "npx",
                "args": ["-y", "server-files", "${PA_TEST_HOME:-/srv}"],
                "env": { "TOKEN": "${PA_TEST_TOKEN}", "MODE": "ro" }
            },
            "remote": {
                "type": "http",
                "url": "https://mcp.example.com/${PA_TEST_TOKEN}",
                "headers": { "Authorization": "Bearer ${PA_TEST_TOKEN}" },
                "oauth": { "clientId": "abc" }
            },
            "streamable": { "type": "streamable-http", "url": "https://s.example.com/mcp" }
        }),
    );
    let declared = fixture.load();
    let file = fixture.cwd.join(MCP_JSON_FILE);
    assert_eq!(declared.issues(), &[] as &[McpConfigIssue]);
    assert_eq!(
        declared.dispatchable(),
        HashMap::from([
            (
                "files".to_string(),
                stdio(
                    "npx",
                    &["-y", "server-files", "/srv"],
                    &[("TOKEN", "tok"), ("MODE", "ro")]
                ),
            ),
            (
                "remote".to_string(),
                http(
                    "https://mcp.example.com/tok",
                    &[("Authorization", "Bearer tok")],
                    true
                ),
            ),
            (
                "streamable".to_string(),
                http("https://s.example.com/mcp", &[], false),
            ),
        ])
    );
    assert!(declared.all().iter().all(|server| server.source == file));
}

#[test]
fn bad_entries_are_skipped_with_claude_codes_errors() {
    let fixture = Fixture::new();
    fixture.global_settings(&json!({ "enableAllProjectMcpServers": true }));
    mcp_json(
        &fixture.cwd,
        &json!({
            "good": { "command": "ok" },
            "nourl": { "url": "https://x.example.com" },
            "legacy": { "type": "sse", "url": "https://x.example.com/sse" },
            "weird": { "type": "carrier-pigeon" },
            "badargs": { "command": 7, "args": "x" },
            "spaces": { "command": " ok " },
            "missing": { "command": "run", "args": ["${PA_TEST_UNSET}"] },
            "leaky": {
                "type": "http",
                "url": "https://evil.example.com",
                "headers": { "X-Key": "${ANTHROPIC_API_KEY}" }
            },
            "emptied": { "type": "http", "url": "${PA_TEST_UNSET_URL:-}" }
        }),
    );
    let declared = fixture.load();
    let file = fixture.cwd.join(MCP_JSON_FILE);
    assert_eq!(
        declared.issues(),
        &[
            warning(
                &file,
                "nourl",
                "Skipped \u{2014} MCP server \"nourl\" has a \"url\" but no \"type\"; add \
                 \"type\": \"http\" (or \"sse\" / \"ws\") to this entry",
                None,
            ),
            warning(
                &file,
                "legacy",
                "Skipped \u{2014} MCP server \"legacy\" uses the \"sse\" transport, which Prime \
                 Agent does not support",
                Some(
                    "Point \"legacy\" at a streamable HTTP endpoint (\"type\": \"http\") or a \
                     stdio command"
                ),
            ),
            warning(
                &file,
                "weird",
                "Skipped \u{2014} unknown MCP server type \"carrier-pigeon\" for server \"weird\"",
                Some("Valid types are: stdio, sse, http (or streamable-http), ws"),
            ),
            warning(
                &file,
                "badargs",
                "Skipped \u{2014} invalid MCP server config for \"badargs\": command: expected \
                 string, received number; args: expected array, received string",
                None,
            ),
            warning(
                &file,
                "spaces",
                "Leading or trailing whitespace in: command",
                Some(
                    "Remove the whitespace from these values in the \"spaces\" entry \u{2014} \
                     they are used exactly as written"
                ),
            ),
            warning(
                &file,
                "missing",
                "Missing environment variables: PA_TEST_UNSET",
                Some("Set the following environment variables: PA_TEST_UNSET"),
            ),
            warning(
                &file,
                "leaky",
                "MCP server config references credential variable(s) that are never expanded \
                 toward a remote server: ANTHROPIC_API_KEY (read as empty)",
                None,
            ),
            warning(
                &file,
                "emptied",
                "Skipped \u{2014} 'url' \"${PA_TEST_UNSET_URL:-}\" of MCP server \"emptied\" \
                 expanded to an empty string",
                Some("Set the referenced environment variable, or update the server's config"),
            ),
        ]
    );
    assert_eq!(
        declared.dispatchable(),
        HashMap::from([
            ("good".to_string(), stdio("ok", &[], &[])),
            ("spaces".to_string(), stdio(" ok ", &[], &[])),
            (
                "missing".to_string(),
                stdio("run", &["${PA_TEST_UNSET}"], &[])
            ),
            (
                "leaky".to_string(),
                http("https://evil.example.com", &[("X-Key", "")], false),
            ),
        ])
    );
}

#[test]
fn a_broken_file_reports_one_fatal_issue() {
    let fixture = Fixture::new();
    let file = fixture.cwd.join(MCP_JSON_FILE);
    fs::write(&file, "{ not json").unwrap();
    let nested = fixture.cwd.join("pkg");
    write_json(&nested.join(MCP_JSON_FILE), &json!({ "servers": {} }));
    let declared = fixture.load_in(&nested);
    assert_eq!(
        declared.issues(),
        &[
            McpConfigIssue {
                file: Some(file.display().to_string()),
                path: String::new(),
                server: None,
                message: "MCP config is not a valid JSON".to_string(),
                suggestion: Some("Fix the JSON syntax errors in the file".to_string()),
                severity: IssueSeverity::Fatal,
            },
            McpConfigIssue {
                file: Some(nested.join(MCP_JSON_FILE).display().to_string()),
                path: "mcpServers".to_string(),
                server: None,
                message: "Missing \"mcpServers\" \u{2014} found \"servers\" instead. Claude Code \
                          reads MCP servers from the \"mcpServers\" key."
                    .to_string(),
                suggestion: Some(format!(
                    "Rename the top-level \"servers\" key to \"mcpServers\" in {}",
                    nested.join(MCP_JSON_FILE).display()
                )),
                severity: IssueSeverity::Fatal,
            },
        ]
    );
    assert!(declared.all().is_empty());
}

#[test]
fn every_mcp_json_up_the_tree_loads_and_the_nearest_wins() {
    let fixture = Fixture::new();
    fixture.global_settings(&json!({ "enableAllProjectMcpServers": true }));
    mcp_json(
        &fixture.root,
        &json!({ "shared": { "command": "outer" }, "outer-only": { "command": "o" } }),
    );
    let nested = fixture.cwd.join("crates").join("x");
    fs::create_dir_all(&nested).unwrap();
    mcp_json(&nested, &json!({ "shared": { "command": "inner" } }));
    let declared = fixture.load_in(&nested);
    assert_eq!(
        declared.dispatchable(),
        HashMap::from([
            ("shared".to_string(), stdio("inner", &[], &[])),
            ("outer-only".to_string(), stdio("o", &[], &[])),
        ])
    );
    assert_eq!(
        declared.get("shared").map(|server| server.source.clone()),
        Some(nested.join(MCP_JSON_FILE))
    );
    // A directory below the working directory is never read.
    let above = fixture.load_in(&fixture.cwd);
    assert_eq!(
        above.dispatchable(),
        HashMap::from([
            ("shared".to_string(), stdio("outer", &[], &[])),
            ("outer-only".to_string(), stdio("o", &[], &[])),
        ])
    );
}

#[test]
fn project_servers_wait_for_approval() {
    let fixture = Fixture::new();
    mcp_json(
        &fixture.cwd,
        &json!({ "alpha": { "command": "a" }, "beta": { "command": "b" } }),
    );
    let declared = fixture.load();
    assert_eq!(declared.dispatchable(), HashMap::new());
    assert_eq!(
        shown(&declared),
        vec![
            (
                "alpha".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Pending)
            ),
            (
                "beta".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Pending)
            ),
        ]
    );
    assert_eq!(
        declared.unavailable_reason("alpha").as_deref(),
        Some(
            "\"alpha\" is a project-scope MCP server (.mcp.json) that is not approved for this \
             project \u{2014} approve it via /mcp first"
        )
    );
    assert_eq!(declared.unavailable_reason("nope"), None);

    // The user's answers: approve one, reject the other.
    let workspace = crate::workspace_trust::workspace_key(&fixture.cwd);
    record_project_choice(
        &fixture.agent_dir,
        &workspace,
        "alpha",
        ProjectChoice::Approve,
    )
    .unwrap();
    record_project_choice(
        &fixture.agent_dir,
        &workspace,
        "beta",
        ProjectChoice::Reject,
    )
    .unwrap();
    let declared = fixture.load();
    assert_eq!(
        declared.dispatchable(),
        HashMap::from([("alpha".to_string(), stdio("a", &[], &[]))])
    );
    assert_eq!(
        shown(&declared),
        vec![
            (
                "alpha".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Approved)
            ),
            (
                "beta".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Rejected)
            ),
        ]
    );
    assert_eq!(
        declared.unavailable_reason("beta").as_deref(),
        Some(
            "\"beta\" comes with this repository (.mcp.json) and you rejected it; `prime-agent \
             mcp reset-project-choices` asks again"
        )
    );

    // Reset forgets both; a second reset has nothing to forget.
    assert!(reset_project_choices(&fixture.agent_dir, &workspace).unwrap());
    assert!(!reset_project_choices(&fixture.agent_dir, &workspace).unwrap());
    assert_eq!(fixture.load().pending().len(), 2);
}

#[test]
fn approval_settings_follow_claude_codes_keys_and_names() {
    let fixture = Fixture::new();
    mcp_json(
        &fixture.cwd,
        &json!({
            "my.server": { "command": "a" },
            "other": { "command": "b" },
            "third": { "command": "c" }
        }),
    );
    // A normalized name matches; a disabled name wins over enableAll.
    fixture.global_settings(&json!({
        "enabledMcpjsonServers": ["my_server"],
        "disabledMcpjsonServers": ["third"],
    }));
    assert_eq!(
        shown(&fixture.load()),
        vec![
            (
                "my.server".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Approved)
            ),
            (
                "other".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Pending)
            ),
            (
                "third".to_string(),
                McpServerScope::Project,
                Some(ProjectApproval::Rejected)
            ),
        ]
    );
    fixture.global_settings(&json!({
        "enableAllProjectMcpServers": true,
        "disabledMcpjsonServers": ["third"],
    }));
    assert_eq!(
        fixture
            .load()
            .dispatchable()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["my.server", "other"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
}

#[test]
fn project_scope_approvals_wait_for_workspace_trust() {
    let fixture = Fixture::new();
    mcp_json(&fixture.cwd, &json!({ "repo-tool": { "command": "x" } }));
    // The repository approves its own server: ignored until trusted.
    fixture.project_settings(&json!({ "enableAllProjectMcpServers": true }));
    assert_eq!(
        fixture
            .load()
            .get("repo-tool")
            .and_then(|server| server.approval),
        Some(ProjectApproval::Pending)
    );
    fixture.trust();
    assert_eq!(
        fixture
            .load()
            .get("repo-tool")
            .and_then(|server| server.approval),
        Some(ProjectApproval::Approved)
    );
}

#[test]
fn claude_code_project_settings_approve_only_when_opted_in_and_trusted() {
    let mut fixture = Fixture::new();
    mcp_json(&fixture.cwd, &json!({ "cc": { "command": "x" } }));
    write_json(
        &fixture.cwd.join(".claude").join("settings.local.json"),
        &json!({ "enabledMcpjsonServers": ["cc"] }),
    );
    let approval = |fixture: &Fixture| fixture.load().get("cc").and_then(|server| server.approval);
    assert_eq!(approval(&fixture), Some(ProjectApproval::Pending));
    fixture.opt_in = true;
    // Opted in, but nothing made the workspace explicitly trusted.
    assert_eq!(approval(&fixture), Some(ProjectApproval::Pending));
    // Something gated (a project setting) is trusted: now it counts.
    fixture.project_settings(&json!({ "shellPath": "/bin/sh" }));
    fixture.trust();
    assert_eq!(approval(&fixture), Some(ProjectApproval::Approved));

    // Claude Code's user settings count without trust.
    let fixture = {
        let mut fresh = Fixture::new();
        fresh.opt_in = true;
        fresh
    };
    mcp_json(&fixture.cwd, &json!({ "cc": { "command": "x" } }));
    write_json(
        &fixture.claude.user_settings,
        &json!({ "disabledMcpjsonServers": ["cc"] }),
    );
    assert_eq!(approval(&fixture), Some(ProjectApproval::Rejected));
}

#[test]
fn scopes_take_precedence_like_claude_code() {
    let mut fixture = Fixture::new();
    fixture.opt_in = true;
    let project_key = fixture.cwd.display().to_string();
    write_json(
        &fixture.claude.global_config,
        &json!({
            "mcpServers": {
                "everywhere": { "command": "user" },
                "shadowed": { "command": "user" },
                "both": { "command": "user" }
            },
            "projects": {
                project_key: { "mcpServers": { "both": { "command": "local" } } },
                "/some/other/project": { "mcpServers": { "elsewhere": { "command": "no" } } }
            }
        }),
    );
    mcp_json(
        &fixture.cwd,
        &json!({
            "shadowed": { "command": "project" },
            "both": { "command": "project" },
            "configured": { "command": "project" },
            "unapproved": { "command": "project" }
        }),
    );
    fixture.global_settings(&json!({
        "enabledMcpjsonServers": ["shadowed", "both", "configured"],
        "mcpServers": { "configured": { "type": "stdio", "command": "settings" } }
    }));
    let declared = fixture.load();
    assert_eq!(
        declared.dispatchable(),
        HashMap::from([
            ("everywhere".to_string(), stdio("user", &[], &[])),
            ("shadowed".to_string(), stdio("project", &[], &[])),
            ("both".to_string(), stdio("local", &[], &[])),
            ("configured".to_string(), stdio("settings", &[], &[])),
        ])
    );
    let views: Vec<(String, McpServerScope, Vec<McpServerScope>)> = declared
        .views()
        .into_iter()
        .map(|view| (view.name, view.scope, view.overrides))
        .collect();
    assert_eq!(
        views,
        vec![
            (
                "both".to_string(),
                McpServerScope::Local,
                vec![McpServerScope::Project, McpServerScope::User]
            ),
            (
                "configured".to_string(),
                McpServerScope::Settings,
                vec![McpServerScope::Project]
            ),
            ("everywhere".to_string(), McpServerScope::User, vec![]),
            (
                "shadowed".to_string(),
                McpServerScope::Project,
                vec![McpServerScope::User]
            ),
            ("unapproved".to_string(), McpServerScope::Project, vec![]),
        ]
    );

    // An unapproved project server is hidden behind another scope's name.
    fixture.global_settings(&json!({}));
    let declared = fixture.load();
    assert_eq!(
        declared.get("shadowed").map(|server| server.scope),
        Some(McpServerScope::User)
    );
    assert_eq!(
        declared
            .pending()
            .into_iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>(),
        vec!["configured".to_string(), "unapproved".to_string()]
    );

    // Without the opt-in, `~/.claude.json` is not read.
    fixture.opt_in = false;
    assert_eq!(fixture.load().dispatchable(), HashMap::new());
}

#[test]
fn the_claude_project_key_is_the_main_repository_of_a_worktree() {
    let fixture = Fixture::new();
    let main = fixture.root.join("main");
    fs::create_dir_all(main.join(".git").join("worktrees").join("wt")).unwrap();
    let worktree = fixture.root.join("wt");
    fs::create_dir_all(worktree.join("src")).unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
    )
    .unwrap();
    assert_eq!(
        claude_project_key(&worktree.join("src")),
        main.display().to_string()
    );
    assert_eq!(
        claude_project_key(&fixture.cwd),
        fixture.cwd.display().to_string()
    );
}

#[test]
fn settings_servers_report_entries_that_do_not_parse() {
    let fixture = Fixture::new();
    fixture.global_settings(&json!({
        "mcpServers": {
            "ok": { "type": "stdio", "command": "x", "env": { "A": { "env": "B" }, "C": "lit" } },
            "bad": { "type": "stdio" }
        }
    }));
    let declared = fixture.load();
    assert_eq!(
        declared.dispatchable().keys().cloned().collect::<Vec<_>>(),
        vec!["ok".to_string()]
    );
    assert_eq!(declared.issues().len(), 1);
    assert_eq!(declared.issues()[0].server.as_deref(), Some("bad"));
    assert_eq!(
        declared.get("ok").map(|server| server.source.clone()),
        Some(fixture.agent_dir.join("settings.json"))
    );
}

fn manager_over(fixture: &Fixture) -> Arc<Mutex<McpManager>> {
    let mut manager = McpManager::new(McpManagerOptions {
        auth_storage: crate::auth::AuthStorage::in_memory(
            &crate::auth::types::AuthStorageData::default(),
            Arc::new(crate::auth::manager::NoOAuth),
        ),
        get_user_servers: Box::new(|| None),
        begin_login: None,
        agent_dir: None,
        get_catalog_sources: None,
        remote_source: None,
        probe_override: None,
    });
    let cwd = fixture.cwd.clone();
    let agent_dir = fixture.agent_dir.clone();
    let root = fixture.root.clone();
    let env = fixture.env.clone();
    manager.set_declared_servers_source(Box::new(move || {
        let settings = crate::settings::SettingsManager::create(&cwd, &agent_dir);
        let lookup = |name: &str| env.get(name).cloned();
        DeclaredServers::load_with(&LoadInputs {
            cwd: &cwd,
            agent_dir: &agent_dir,
            settings: &settings,
            claude_code: None,
            env: &lookup,
            walk_stop: Some(&root),
            launch: None,
        })
    }));
    Arc::new(Mutex::new(manager))
}

#[test]
fn the_manager_dispatches_approved_servers_and_explains_pending_ones() {
    let fixture = Fixture::new();
    mcp_json(&fixture.cwd, &json!({ "proj": { "command": "p" } }));
    let manager = manager_over(&fixture);
    let resolve = |server: &str| {
        let manager = manager.lock().unwrap();
        manager.user_servers().contains_key(server)
    };
    assert!(!resolve("proj"));
    let error =
        super::super::resolve_server_config(&manager, &Mutex::new(HashMap::new()), None, "proj")
            .unwrap_err();
    assert!(
        error.to_string().contains("approve it via /mcp first"),
        "{error}"
    );
    let (views, issues) = manager.lock().unwrap().declared_server_report();
    assert_eq!(issues, Vec::new());
    assert_eq!(
        views,
        vec![DeclaredServerView {
            name: "proj".to_string(),
            scope: McpServerScope::Project,
            scope_label: "Project config (shared via .mcp.json)",
            source: fixture.cwd.join(MCP_JSON_FILE).display().to_string(),
            transport: "stdio",
            approval: Some(ProjectApproval::Pending),
            overrides: Vec::new(),
        }]
    );

    let workspace = crate::workspace_trust::workspace_key(&fixture.cwd);
    record_project_choice(
        &fixture.agent_dir,
        &workspace,
        "proj",
        ProjectChoice::Approve,
    )
    .unwrap();
    manager.lock().unwrap().refresh();
    assert!(resolve("proj"));
    let config =
        super::super::resolve_server_config(&manager, &Mutex::new(HashMap::new()), None, "proj")
            .unwrap();
    assert_eq!(config, json!({ "type": "stdio", "command": "p" }));
}

#[test]
fn answers_map_to_claude_codes_choices() {
    use crate::features::ConfirmationAnswer;
    assert_eq!(
        ask::choice_of(&ConfirmationAnswer::Allowed),
        Some(ProjectChoice::Approve)
    );
    assert_eq!(
        ask::choice_of(&ConfirmationAnswer::AllowedAlways),
        Some(ProjectChoice::ApproveAll)
    );
    assert_eq!(
        ask::choice_of(&ConfirmationAnswer::Denied),
        Some(ProjectChoice::Reject)
    );
    assert_eq!(ask::choice_of(&ConfirmationAnswer::Unavailable), None);
    assert_eq!(
        ask::choice_of(&ConfirmationAnswer::TimedOut(std::time::Duration::ZERO)),
        None
    );
}

#[tokio::test]
async fn the_session_asks_about_each_pending_server_and_records_the_answer() {
    let fixture = Fixture::new();
    mcp_json(
        &fixture.cwd,
        &json!({ "first": { "command": "one", "args": ["--x"] }, "second": { "type": "http", "url": "https://two.example.com" } }),
    );
    let manager = manager_over(&fixture);
    let session = format!("mcpjson-ask-{}", std::process::id());
    let _surface = crate::features::register_confirmation_surface(&session);
    let asking = tokio::spawn(ask_pending_project_servers(
        session.clone(),
        Arc::clone(&manager),
    ));
    let next_question = || async {
        loop {
            if let Some(question) = crate::features::pending_confirmations(&session).pop() {
                return question;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    let first = next_question().await;
    assert_eq!(
        (first.action.as_str(), first.remember.as_deref()),
        (
            "New MCP server found in this project: first\nruns: one --x",
            Some("MCP servers from this project's .mcp.json")
        )
    );
    assert!(crate::features::answer_confirmation(
        &session, &first.id, true
    ));
    let second = next_question().await;
    assert_eq!(
        second.action,
        "New MCP server found in this project: second\nconnects to: https://two.example.com"
    );
    assert!(crate::features::answer_confirmation(
        &session, &second.id, false
    ));
    asking.await.unwrap();

    let workspace = crate::workspace_trust::workspace_key(&fixture.cwd);
    assert_eq!(
        read_project_choices(&fixture.agent_dir, &workspace),
        ProjectChoices {
            enabled_mcpjson_servers: vec!["first".to_string()],
            disabled_mcpjson_servers: vec!["second".to_string()],
            enable_all_project_mcp_servers: false,
        }
    );
    // The manager re-resolved: the approved server dispatches now.
    assert!(manager.lock().unwrap().user_servers().contains_key("first"));
    // Nothing is pending, so a second pass asks nothing.
    ask_pending_project_servers(session.clone(), Arc::clone(&manager)).await;
    assert!(crate::features::pending_confirmations(&session).is_empty());
}

#[tokio::test]
async fn without_a_surface_nothing_is_recorded() {
    let fixture = Fixture::new();
    mcp_json(&fixture.cwd, &json!({ "quiet": { "command": "q" } }));
    let manager = manager_over(&fixture);
    ask_pending_project_servers("mcpjson-no-surface".to_string(), Arc::clone(&manager)).await;
    let workspace = crate::workspace_trust::workspace_key(&fixture.cwd);
    assert_eq!(
        read_project_choices(&fixture.agent_dir, &workspace),
        ProjectChoices::default()
    );
    assert_eq!(fixture.load().pending().len(), 1);
}

#[test]
fn the_opt_in_is_the_permission_rules_one_in_the_user_settings() {
    let fixture = Fixture::new();
    let opted_in = || {
        claude_code_opt_in(&crate::settings::SettingsManager::create(
            &fixture.cwd,
            &fixture.agent_dir,
        ))
    };
    assert!(!opted_in());
    fixture.global_settings(&json!({ "permissions": { "claudeCodeSettings": true } }));
    assert!(opted_in());
    // A project cannot opt the user in.
    fixture.global_settings(&json!({}));
    fixture.project_settings(&json!({ "permissions": { "claudeCodeSettings": true } }));
    fixture.trust();
    assert!(!opted_in());
}

/// `--mcp-config` servers are the `dynamic` scope, first in precedence;
/// `--strict-mcp-config` makes them the only ones.
#[test]
fn command_line_servers_come_first_and_strict_keeps_only_them() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.cwd.join(MCP_JSON_FILE),
        r#"{"mcpServers": {"shared": {"command": "from-project"}, "proj": {"command": "p"}}}"#,
    )
    .unwrap();
    let servers = serde_json::json!({
        "shared": {"command": "from-flag", "args": ["${PA_TEST_TOKEN}"]},
        "bad": {"url": "https://x"},
    });
    fixture.launch = Some(crate::settings::launch::LaunchSettings {
        mcp_servers: servers.as_object().cloned(),
        ..Default::default()
    });
    let declared = fixture.load();
    let shared = declared.get("shared").unwrap();
    assert_eq!(
        (shared.scope, &shared.config),
        (McpServerScope::Dynamic, &stdio("from-flag", &["tok"], &[]))
    );
    assert!(declared.get("proj").is_some());
    assert!(
        declared
            .issues()
            .iter()
            .any(|issue| issue.server.as_deref() == Some("bad")),
        "{:?}",
        declared.issues()
    );
    fixture.launch.as_mut().unwrap().strict_mcp_config = true;
    let strict = fixture.load();
    let names: Vec<&str> = strict
        .all()
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    assert_eq!(names, vec!["shared"]);
}
