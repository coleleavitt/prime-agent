//! End-to-end tests of the LSP feature against a fake language server
//! (`fake_server`): the `lsp.*` host requests, diagnostics delivery after
//! writes, crash recovery bounds, and shutdown with the session.
//!
//! This binary is also the fake server: started with `PA_LSP_FAKE_SERVER=1`
//! it serves JSON-RPC on stdio instead of running the tests, which is why
//! the target has its own main (libtest would write to stdout).

mod fake_server;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pa_core::features::{
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolBatchCall,
    ToolBatchObservation,
    ToolResultObservation,
};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_lsp::LspFeature;
use pa_lsp::config::{ConfigSource, ConfiguredServer, LspConfig, ServerConfig};
use serde_json::{Value, json};

const FAKE_SERVER_ENV: &str = "PA_LSP_FAKE_SERVER";

const PROGRAM: &str = "fn helper\n  let x\nfn main\n  helper()\n  helper()\n";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(root.join("agent")).expect("agent dir");
        Self {
            _tmp: tmp,
            root,
            cwd,
        }
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.cwd.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    fn context(&self, session: &str) -> Arc<SessionFeatureContext> {
        Arc::new(SessionFeatureContext {
            agent_dir: self.root.join("agent"),
            cwd: self.cwd.clone(),
            session_id: session.to_string(),
            python_skill_import_names: Vec::new(),
            model: serde_json::from_value(json!({
                "id": "m1", "name": "M1", "api": "test", "provider": "p1",
                "baseUrl": "http://localhost", "reasoning": false,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 1000, "maxTokens": 100
            }))
            .expect("model"),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
        })
    }

    fn starts_file(&self) -> PathBuf {
        self.root.join("starts")
    }

    fn starts(&self) -> Vec<u32> {
        std::fs::read_to_string(self.starts_file())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    /// A fake server for `.fake` files, with `extra` merged into its
    /// configuration and `env` into its environment.
    fn config(&self, extra: &Value, env: &[(&str, &str)]) -> LspConfig {
        let mut environment = json!({
            FAKE_SERVER_ENV: "1",
            "FAKE_LSP_STARTS_FILE": self.starts_file().display().to_string(),
        });
        for (key, value) in env {
            environment[*key] = json!(value);
        }
        let mut server = json!({
            "command": std::env::current_exe().expect("test binary").display().to_string(),
            "extensionToLanguage": {".fake": "fake"},
            "env": environment,
            "startupTimeout": 20_000,
            "requestTimeout": 20_000,
            "shutdownTimeout": 5_000,
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            server[key] = value.clone();
        }
        LspConfig {
            servers: vec![ConfiguredServer {
                name: "fake".to_string(),
                config: serde_json::from_value::<ServerConfig>(server).expect("config"),
                source: ConfigSource::Settings,
            }],
            errors: Vec::new(),
            diagnostics_wait: Duration::from_secs(10),
        }
    }
}

/// A feature serving `config`, with its host handlers for `context`.
fn feature(
    config: LspConfig,
    context: &SessionFeatureContext,
) -> (LspFeature, HostRequestHandlers) {
    let feature = LspFeature::with_loader(Arc::new(move |_: &Path, _: &Path| config.clone()));
    let mut handlers = HostRequestHandlers::new();
    feature.register_host_handlers(context, &mut handlers);
    (feature, handlers)
}

async fn call(handlers: &HostRequestHandlers, request: &str, data: Value) -> Result<Value, String> {
    let handler = handlers
        .get(request)
        .unwrap_or_else(|| panic!("{request} is registered"));
    handler(HostRequestPayload { data })
        .await
        .map_err(|error| error.to_string())
}

async fn text(handlers: &HostRequestHandlers, request: &str, data: Value) -> String {
    let reply = call(handlers, request, data).await.expect("an answer");
    reply["text"].as_str().expect("text").to_string()
}

fn wrote(paths: &[&Path]) -> ToolResultObservation {
    ToolResultObservation {
        tool_call_id: "call-1".to_string(),
        tool_name: "python_repl".to_string(),
        args: json!({"code": "..."}),
        is_error: false,
        content: Vec::new(),
        details: Value::Null,
        host_facts: json!({"fileWrites": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>()}),
        earlier_results_of_tool: 0,
    }
}

fn batch() -> ToolBatchObservation {
    ToolBatchObservation {
        tool_calls: vec![ToolBatchCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "python_repl".to_string(),
        }],
    }
}

fn reminder(body: &str) -> String {
    format!(
        "<system-reminder>\n<new-diagnostics>The following new diagnostic issues were detected:\n\n{body}</new-diagnostics>\n</system-reminder>"
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "one server start serves every operation; the test asserts that too"
)]
async fn navigation_answers_like_claude_code() {
    let fixture = Fixture::new();
    let path = fixture.write("main.fake", PROGRAM);
    let context = fixture.context("nav");
    let (_feature, handlers) = feature(fixture.config(&json!({}), &[]), &context);
    let at =
        |line: u32, character: u32| json!({"path": path, "line": line, "character": character});

    let definition = call(&handlers, "lsp.definition", at(4, 3))
        .await
        .expect("definition");
    assert_eq!(definition["text"], "Defined in main.fake:1:4");
    assert_eq!(
        definition["items"],
        json!([{"path": path, "line": 1, "character": 4, "end_line": 1, "end_character": 10}])
    );
    assert_eq!(definition["server"], "fake");
    assert_eq!(
        text(&handlers, "lsp.implementation", at(4, 3)).await,
        "Defined in main.fake:1:4"
    );
    assert_eq!(
        text(&handlers, "lsp.references", at(4, 3)).await,
        "Found 3 references across 1 files:\n\nmain.fake:\n  Line 1:4\n  Line 4:3\n  Line 5:3"
    );
    assert_eq!(
        text(&handlers, "lsp.hover", at(1, 4)).await,
        "Hover info at 1:4:\n\n`helper`: a fake symbol"
    );
    assert_eq!(
        text(&handlers, "lsp.document_symbols", json!({"path": path})).await,
        "Document symbols:\nhelper (Function) fn() - Line 1\n  x (Variable) - Line 2\nmain (Function) fn() - Line 3"
    );
    // The server is running now, so no path is needed.
    assert_eq!(
        text(&handlers, "lsp.workspace_symbols", json!({"query": "ma"})).await,
        "Found 1 symbol in workspace:\n\nmain.fake:\n  main (Function) - Line 3 in toy"
    );
    assert_eq!(
        text(&handlers, "lsp.call_hierarchy", at(1, 4)).await,
        "Found 1 incoming call:\n\nmain.fake:\n  main (Function) - Line 3 [calls at: 4:3, 5:3]"
    );
    assert_eq!(
        text(
            &handlers,
            "lsp.call_hierarchy",
            json!({"path": path, "line": 3, "character": 4, "direction": "outgoing"})
        )
        .await,
        "Found 2 outgoing calls:\n\nmain.fake:\n  helper (Function) - Line 1 [called from: 4:3]\n  helper (Function) - Line 1 [called from: 5:3]"
    );
    assert_eq!(
        text(
            &handlers,
            "lsp.call_hierarchy",
            json!({"path": path, "line": 1, "character": 4, "direction": "prepare"})
        )
        .await,
        "Call hierarchy item: helper (Function) - main.fake:1 [fn]"
    );
    assert_eq!(
        text(&handlers, "lsp.definition", at(2, 1)).await,
        "No definition found. This may occur if the cursor is not on a symbol, or if the definition is in an external library not indexed by the LSP server."
    );

    let other = fixture.write("notes.txt", "hello\n");
    let unserved = call(
        &handlers,
        "lsp.hover",
        json!({"path": other, "line": 1, "character": 1}),
    )
    .await
    .expect("an answer");
    assert_eq!(unserved["available"], false);
    assert_eq!(
        unserved["text"],
        "No LSP server available for file type: .txt"
    );
    let missing = call(
        &handlers,
        "lsp.hover",
        json!({"path": fixture.cwd.join("gone.fake"), "line": 1, "character": 1}),
    )
    .await;
    assert_eq!(
        missing,
        Err(format!(
            "File does not exist: {}",
            fixture.cwd.join("gone.fake").display()
        ))
    );
    let bad = call(
        &handlers,
        "lsp.hover",
        json!({"path": path, "line": 0, "character": 1}),
    )
    .await;
    assert_eq!(
        bad,
        Err("line must be a positive integer (1-based, as shown in editors)".to_string())
    );
    let servers = call(&handlers, "lsp.servers", json!({}))
        .await
        .expect("servers");
    assert_eq!(servers["servers"][0]["status"], "running");
    assert_eq!(servers["servers"][0]["extensions"], json!([".fake"]));
    assert_eq!(
        fixture.starts().len(),
        1,
        "one server start serves every request"
    );
}

async fn writes_deliver_new_diagnostics_once() {
    let fixture = Fixture::new();
    let context = fixture.context("diag");
    let (feature, _handlers) = feature(fixture.config(&json!({}), &[]), &context);
    let path = fixture.write("a.fake", "fn a\n  ERROR here\n");
    assert_eq!(
        feature.after_tool_call(&context, &wrote(&[&path])).await,
        None
    );
    assert_eq!(
        feature.after_tool_batch(&context, &batch()).await,
        Some(reminder(
            "a.fake:\n  \u{2718} [Line 2:3] found ERROR [E1] (fake)"
        ))
    );
    // Nothing new: nothing delivered.
    assert_eq!(feature.after_tool_batch(&context, &batch()).await, None);
    // Another write: the file's diagnostics are new again, errors first.
    std::fs::write(&path, "fn a\n  WARN first\n  ERROR here\n").expect("rewrite");
    let _ = feature.after_tool_call(&context, &wrote(&[&path])).await;
    assert_eq!(
        feature.after_tool_batch(&context, &batch()).await,
        Some(reminder(
            "a.fake:\n  \u{2718} [Line 3:3] found ERROR [E1] (fake)\n  \u{26a0} [Line 2:3] found WARN [E2] (fake)"
        ))
    );
    // A write the boundary does not see reaches the next prompt instead.
    std::fs::write(&path, "fn a\n  ERROR again\n").expect("rewrite");
    let _ = feature.after_tool_call(&context, &wrote(&[&path])).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let delivered = loop {
        match feature.prompt_context(&context, "next").await {
            PromptDecision::Continue {
                context: Some(text),
            } => break text,
            PromptDecision::Continue { context: None }
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => panic!("no prompt delivery: {other:?}"),
        }
    };
    assert_eq!(
        delivered,
        reminder("a.fake:\n  \u{2718} [Line 2:3] found ERROR [E1] (fake)")
    );
    assert_eq!(
        feature.prompt_context(&context, "again").await,
        PromptDecision::Continue { context: None }
    );
    // A write to a file no server handles starts nothing.
    let other = fixture.write("b.txt", "ERROR\n");
    let _ = feature.after_tool_call(&context, &wrote(&[&other])).await;
    assert_eq!(feature.after_tool_batch(&context, &batch()).await, None);
    assert_eq!(fixture.starts().len(), 1);
}

async fn explicit_diagnostics_are_not_delivered_again() {
    let fixture = Fixture::new();
    let context = fixture.context("read");
    let (feature, handlers) = feature(fixture.config(&json!({}), &[]), &context);
    let path = fixture.write("b.fake", "fn b\nWARN x\n");
    let reply = call(&handlers, "lsp.diagnostics", json!({"path": path}))
        .await
        .expect("diagnostics");
    assert_eq!(
        reply["text"],
        "b.fake:\n  \u{26a0} [Line 2:1] found WARN [E2] (fake)"
    );
    assert_eq!(reply["items"][0]["severity"], "Warning");
    assert_eq!(reply["result_count"], 1);
    assert_eq!(
        feature.prompt_context(&context, "next").await,
        PromptDecision::Continue { context: None }
    );
    let clean = fixture.write("c.fake", "fn c\n");
    assert_eq!(
        text(&handlers, "lsp.diagnostics", json!({"path": clean})).await,
        "No diagnostics in c.fake."
    );
}

async fn crashes_restart_a_bounded_number_of_times() {
    let fixture = Fixture::new();
    let path = fixture.write("main.fake", PROGRAM);
    let context = fixture.context("crash");
    let config = fixture.config(
        &json!({"maxRestarts": 1}),
        &[("FAKE_LSP_CRASH_ON", "textDocument/hover")],
    );
    let (_feature, handlers) = feature(config, &context);
    let hover = json!({"path": path, "line": 1, "character": 4});
    let first = call(&handlers, "lsp.hover", hover.clone()).await;
    assert_eq!(
        first,
        Err(
            "LSP request 'textDocument/hover' failed for server 'fake': the server exited"
                .to_string()
        )
    );
    // The restart (one allowed) crashes again.
    assert!(call(&handlers, "lsp.hover", hover.clone()).await.is_err());
    assert_eq!(fixture.starts().len(), 2);
    let given_up = call(&handlers, "lsp.hover", hover.clone())
        .await
        .expect_err("given up");
    assert!(
        given_up.starts_with("LSP server 'fake' exceeded max crash recovery attempts (1); last error: LSP server 'fake' exited: fake server crashing on textDocument/hover"),
        "{given_up}"
    );
    assert_eq!(fixture.starts().len(), 2, "no start past the bound");
    let servers = call(&handlers, "lsp.servers", json!({}))
        .await
        .expect("servers");
    assert_eq!(servers["servers"][0]["status"], "failed");
    // Other requests fail the same way: the server is gone for the session.
    assert!(call(&handlers, "lsp.definition", hover).await.is_err());
}

async fn restart_on_crash_false_gives_up_at_once() {
    let fixture = Fixture::new();
    let path = fixture.write("main.fake", PROGRAM);
    let context = fixture.context("norestart");
    let config = fixture.config(
        &json!({"restartOnCrash": false}),
        &[("FAKE_LSP_CRASH_ON", "textDocument/hover")],
    );
    let (_feature, handlers) = feature(config, &context);
    let hover = json!({"path": path, "line": 1, "character": 4});
    assert!(call(&handlers, "lsp.hover", hover.clone()).await.is_err());
    let second = call(&handlers, "lsp.hover", hover)
        .await
        .expect_err("gave up");
    assert!(
        second.starts_with("LSP server 'fake' crashed (restartOnCrash is false)"),
        "{second}"
    );
    assert_eq!(fixture.starts().len(), 1);
}

async fn content_modified_is_retried() {
    let fixture = Fixture::new();
    let path = fixture.write("main.fake", PROGRAM);
    let context = fixture.context("modified");
    let config = fixture.config(
        &json!({}),
        &[("FAKE_LSP_CONTENT_MODIFIED", "textDocument/references")],
    );
    let (_feature, handlers) = feature(config, &context);
    let reply = call(
        &handlers,
        "lsp.references",
        json!({"path": path, "line": 1, "character": 4}),
    )
    .await
    .expect("retried");
    assert_eq!(reply["result_count"], 3);
}

async fn server_settings_answer_workspace_configuration() {
    let fixture = Fixture::new();
    let path = fixture.write("main.fake", PROGRAM);
    let context = fixture.context("settings");
    let config = fixture.config(
        &json!({"settings": {"fake": {"strict": true}}}),
        &[("FAKE_LSP_CONFIG_SECTION", "fake")],
    );
    let (_feature, handlers) = feature(config, &context);
    assert_eq!(
        text(
            &handlers,
            "lsp.hover",
            json!({"path": path, "line": 3, "character": 4})
        )
        .await,
        "Hover info at 3:4:\n\n`main`: a fake symbol\nconfig={\"strict\":true}"
    );
}

async fn the_session_end_stops_its_servers() {
    let fixture = Fixture::new();
    let path = fixture.write("main.fake", PROGRAM);
    let context = fixture.context("end");
    let (feature, handlers) = feature(fixture.config(&json!({}), &[]), &context);
    let _ = text(
        &handlers,
        "lsp.hover",
        json!({"path": path, "line": 1, "character": 4}),
    )
    .await;
    let pid = fixture.starts()[0];
    assert!(process_alive(pid));
    feature.on_session_end(&context, "other");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while process_alive(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "server {pid} still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A request after the end is refused (the handlers outlived it).
    assert!(
        call(
            &handlers,
            "lsp.hover",
            json!({"path": path, "line": 1, "character": 4})
        )
        .await
        .is_err()
    );
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

type Test = fn() -> Pin<Box<dyn Future<Output = ()>>>;

fn tests() -> Vec<(&'static str, Test)> {
    vec![
        ("navigation_answers_like_claude_code", || {
            Box::pin(navigation_answers_like_claude_code())
        }),
        ("writes_deliver_new_diagnostics_once", || {
            Box::pin(writes_deliver_new_diagnostics_once())
        }),
        ("explicit_diagnostics_are_not_delivered_again", || {
            Box::pin(explicit_diagnostics_are_not_delivered_again())
        }),
        ("crashes_restart_a_bounded_number_of_times", || {
            Box::pin(crashes_restart_a_bounded_number_of_times())
        }),
        ("restart_on_crash_false_gives_up_at_once", || {
            Box::pin(restart_on_crash_false_gives_up_at_once())
        }),
        ("content_modified_is_retried", || {
            Box::pin(content_modified_is_retried())
        }),
        ("server_settings_answer_workspace_configuration", || {
            Box::pin(server_settings_answer_workspace_configuration())
        }),
        ("the_session_end_stops_its_servers", || {
            Box::pin(the_session_end_stops_its_servers())
        }),
    ]
}

fn main() {
    if std::env::var_os(FAKE_SERVER_ENV).is_some() {
        fake_server::run();
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in tests() {
            println!("{name}: test");
        }
        return;
    }
    let filters: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    let selected: Vec<_> = tests()
        .into_iter()
        .filter(|(name, _)| {
            filters.is_empty() || filters.iter().any(|filter| name.contains(filter.as_str()))
        })
        .collect();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in selected {
        let outcome = std::panic::catch_unwind(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(test());
        });
        if outcome.is_ok() {
            println!("test {name} ... ok");
        } else {
            println!("test {name} ... FAILED");
            failed.push(name);
        }
    }
    if failed.is_empty() {
        println!("\ntest result: ok");
    } else {
        println!("\nfailures: {failed:?}\ntest result: FAILED");
        std::process::exit(1);
    }
}
