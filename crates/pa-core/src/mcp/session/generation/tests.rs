//! Inventory honesty (the in-kernel client's pagination contract) and the
//! startup stderr policy of one generation.

use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;
use serde_json::{Value, json};

use super::*;

fn page(names: &[&str], next: Option<&str>) -> ListToolsResult {
    let tools: Vec<Value> = names
        .iter()
        .map(|name| json!({ "name": name, "description": "", "inputSchema": { "type": "object" } }))
        .collect();
    let mut result = json!({ "tools": tools });
    if let Some(next) = next {
        result["nextCursor"] = json!(next);
    }
    serde_json::from_value(result).expect("a tools/list page")
}

/// Run `discover` over `pages` (served in order, the last repeating);
/// returns its outcome and the cursors it asked with.
async fn run(
    pages: Vec<ListToolsResult>,
    max_pages: usize,
) -> (Result<Vec<String>, McpSessionError>, Vec<Option<String>>) {
    let asked: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let served = Arc::new(Mutex::new(0usize));
    let outcome = discover("svc", max_pages, |cursor| {
        asked.lock_or_recover().push(cursor);
        let mut index = served.lock_or_recover();
        let result = pages[(*index).min(pages.len() - 1)].clone();
        *index += 1;
        async move { Ok(result) }
    })
    .await
    .map(|tools| tools.into_iter().map(|(name, _)| name).collect());
    let asked = asked.lock_or_recover().clone();
    (outcome, asked)
}

#[tokio::test]
async fn pages_are_followed_in_order_and_names_stay_unique() {
    assert_eq!(
        run(
            vec![
                page(&["one"], Some("cursor-2")),
                page(&["two", "one"], None)
            ],
            MAX_TOOL_PAGES
        )
        .await,
        (
            Ok(vec!["one".to_string(), "two".to_string()]),
            vec![None, Some("cursor-2".to_string())]
        )
    );
}

#[tokio::test]
async fn a_repeated_cursor_refuses_instead_of_looping() {
    assert_eq!(
        run(vec![page(&["one"], Some("same"))], MAX_TOOL_PAGES).await,
        (
            Err(McpSessionError::new(
                McpErrorKind::Discovery,
                "MCP server 'svc' repeated a tools/list pagination cursor; its tool inventory cannot be completed"
            )),
            vec![None, Some("same".to_string())]
        )
    );
}

#[tokio::test]
async fn the_page_cap_refuses_a_partial_inventory() {
    let pages = (0..10)
        .map(|index| page(&["one"], Some(&format!("cursor-{index}"))))
        .collect();
    assert_eq!(
        run(pages, 3).await,
        (
            Err(McpSessionError::new(
                McpErrorKind::Discovery,
                "MCP server 'svc' paginated tools/list beyond 3 pages; refusing to publish a partial tool inventory"
            )),
            vec![
                None,
                Some("cursor-0".to_string()),
                Some("cursor-1".to_string())
            ]
        )
    );
}

#[tokio::test]
async fn an_empty_cursor_is_malformed() {
    assert_eq!(
        run(
            vec![page(&["one"], Some("")), page(&["two"], None)],
            MAX_TOOL_PAGES
        )
        .await,
        (
            Err(McpSessionError::new(
                McpErrorKind::Discovery,
                "MCP server 'svc' returned a malformed tools/list pagination cursor"
            )),
            vec![None]
        )
    );
}

#[test]
fn a_listed_tool_keeps_its_exact_name_and_schema() {
    let schema = json!({ "type": "object", "properties": { "x": { "const": 1 } } });
    let listed = page(&["raw.tool/name"], None);
    let mut tool = listed.tools[0].clone();
    tool.input_schema = Arc::new(schema.as_object().unwrap().clone());
    assert_eq!(
        tool_entry(&tool),
        json!({ "name": "raw.tool/name", "description": "", "inputSchema": schema })
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_successful_startup_discards_the_stderr_it_captured() {
    let Some(python) = super::super::tests::python() else {
        return;
    };
    let launch = StdioLaunch {
        command: python,
        args: vec![
            super::super::tests::fixture("stdio_server.py")
                .to_string_lossy()
                .to_string(),
        ],
        cwd: std::env::temp_dir(),
        env: vec![(
            "FIXTURE_STDERR_NOTE".to_string(),
            "startup note".to_string(),
        )],
        secrets: Vec::new(),
        disclosable: true,
        private_values: Vec::new(),
    };
    let generation = Generation::open(
        "svc",
        json!({ "type": "stdio" }),
        Target::Stdio(launch, None),
        Discovery::Full,
    )
    .await
    .expect("the fixture starts");
    let stderr = generation
        .child
        .as_ref()
        .expect("a stdio child")
        .stderr
        .clone();
    assert_eq!(stderr.tail(&[], &[]), "");
    generation.close().await;
}

/// A host-spawned stdio server runs under the session's OS sandbox, like
/// the kernel whose `rlm.mcp` calls it serves: under `workspace-write` it
/// starts and writes inside its cwd, and a write outside it fails, so the
/// server never starts. Skips without Landlock or a writable directory
/// outside `/tmp` (which the sandbox keeps writable).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_stdio_server_spawns_under_the_session_sandbox() {
    let Some(python) = super::super::tests::python() else {
        return;
    };
    let Ok(base) = tempfile::tempdir_in("/var/tmp") else {
        eprintln!("/var/tmp is not usable; skipping the sandboxed stdio test");
        return;
    };
    let root = base.path().canonicalize().unwrap();
    let (work, outside) = (root.join("work"), root.join("outside"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let settings = crate::settings::SettingsManager::in_memory(&crate::settings::Settings {
        sandbox: Some(crate::settings::SandboxSettings {
            mode: Some("workspace-write".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let sandbox = crate::os_sandbox::SessionSandbox::resolve(&settings, None, &work)
        .expect("an enabled sandbox");
    if sandbox.status_label().ends_with("(unavailable)") {
        eprintln!(
            "skipping the sandboxed stdio test: {}",
            sandbox.prompt_line()
        );
        return;
    }
    let launch = |pid_file: &std::path::Path| StdioLaunch {
        command: python.clone(),
        args: vec![
            super::super::tests::fixture("stdio_server.py")
                .to_string_lossy()
                .to_string(),
        ],
        cwd: work.clone(),
        env: vec![(
            "FIXTURE_PID_FILE".to_string(),
            pid_file.to_string_lossy().to_string(),
        )],
        secrets: Vec::new(),
        disclosable: true,
        private_values: Vec::new(),
    };
    let mut started = Vec::new();
    for pid_file in [work.join("server.pid"), outside.join("server.pid")] {
        let opened = Generation::open(
            "svc",
            json!({ "type": "stdio" }),
            Target::Stdio(launch(&pid_file), Some(Box::new(sandbox.clone()))),
            Discovery::Full,
        )
        .await;
        started.push((opened.is_ok(), pid_file.exists()));
        if let Ok(generation) = opened {
            generation.close().await;
        }
    }
    assert_eq!(started, vec![(true, true), (false, false)]);
}
