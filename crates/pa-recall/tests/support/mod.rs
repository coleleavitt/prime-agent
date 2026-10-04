//! Shared fixtures: real git repos under a temp dir, and a driver that
//! plays one process's [`WorkspaceRecall`] through its session hooks the
//! way the engine does.

#![allow(dead_code)] // each test binary uses its own subset

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use pa_core::features::{
    SessionFeature, SessionFeatureContext, ToolCallObservation, ToolResultObservation,
};
use pa_recall::{
    write_recall_mark, MarkOutcome, MarkWriter, RecallClaimInput, RecallOptions, WorkspaceRecall,
    WrittenMark,
};

pub const TSGO_CLAIM: &str = "npx tsgo --noEmit";

/// Run git in `repo` with no user, system, or hook configuration leaking in.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/nonexistent-prime-agent-recall-hooks",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Recall Test",
            "-c",
            "user.email=recall@test.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 git output")
}

pub struct TempDirs {
    dirs: Mutex<Vec<tempfile::TempDir>>,
}

impl TempDirs {
    pub fn new() -> Self {
        Self {
            dirs: Mutex::new(Vec::new()),
        }
    }

    /// A fresh directory, resolved through symlinks (macOS `/var` -> `/private/var`).
    pub fn dir(&self, prefix: &str) -> PathBuf {
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("tempdir");
        let path = std::fs::canonicalize(dir.path()).expect("canonical tempdir");
        self.dirs.lock().unwrap().push(dir);
        path
    }
}

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

pub fn write_three_files(repo: &Path) {
    write(&repo.join("a.txt"), "alpha\n");
    write(&repo.join("b.txt"), "bravo\n");
    write(&repo.join("c.txt"), "charlie\n");
}

/// A git repo with three committed files.
pub fn create_repo(temp: &TempDirs) -> PathBuf {
    let repo = temp.dir("prime-agent-recall-repo-");
    git(&repo, &["init", "-q"]);
    write_three_files(&repo);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

pub fn pkg_paths(pkg: &str) -> Vec<String> {
    (0..5).map(|index| format!("{pkg}/f{index}.ts")).collect()
}

/// root.ts plus five files in each of pkga/ and pkgb/, committed.
pub fn create_packages_repo(temp: &TempDirs) -> PathBuf {
    let repo = temp.dir("prime-agent-recall-packages-");
    git(&repo, &["init", "-q"]);
    write(&repo.join("root.ts"), "root\n");
    for pkg in ["pkga", "pkgb"] {
        for index in 0..5 {
            write(
                &repo.join(pkg).join(format!("f{index}.ts")),
                &format!("{pkg} {index}\n"),
            );
        }
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

/// 250 untracked files, so a mark records the first 200 and leaves 50 unrecorded.
pub fn write_untracked_files(repo: &Path) -> Vec<String> {
    let paths: Vec<String> = (0..250).map(|index| format!("u/f{index:03}.txt")).collect();
    for path in &paths {
        write(&repo.join(path), &format!("{path}\n"));
    }
    paths
}

pub fn root(repo: &Path) -> String {
    repo.to_str().unwrap().to_string()
}

pub fn claim(command: &str) -> RecallClaimInput {
    RecallClaimInput {
        command: command.to_string(),
        exit_code: 0,
        digest_at_claim: None,
        at: None,
    }
}

pub async fn write_mark_ok(
    repo: &Path,
    agent_dir: &Path,
    claims: &[RecallClaimInput],
) -> WrittenMark {
    write_recall_mark(&root(repo), agent_dir, claims, pa_recall_now())
        .await
        .expect("mark written")
}

pub fn pa_recall_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

pub fn context(repo: &Path, agent_dir: &Path, session_id: &str) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: agent_dir.to_path_buf(),
        cwd: repo.to_path_buf(),
        session_id: session_id.to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .expect("stub model"),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    })
}

pub fn child_context(
    repo: &Path,
    agent_dir: &Path,
    session_id: &str,
) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        rlm_depth: 1,
        session_artifact_dir: None,
        ..(*context(repo, agent_dir, session_id)).clone()
    })
}

/// One simulated process: a recall instance and its settled-mark stream.
pub struct Process {
    pub recall: WorkspaceRecall,
    settled: Mutex<mpsc::Receiver<MarkOutcome>>,
    pub enabled: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct ProcessOptions {
    pub write_mark: Option<MarkWriter>,
    pub clock: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

impl Process {
    pub fn new() -> Self {
        Self::with(ProcessOptions::default())
    }

    pub fn with(options: ProcessOptions) -> Self {
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        let enabled = Arc::new(AtomicBool::new(true));
        let switch = Arc::clone(&enabled);
        let recall = WorkspaceRecall::new(RecallOptions {
            on_mark_settled: Some(Arc::new(move |outcome| {
                let _ = sender.lock().unwrap().send(outcome);
            })),
            write_mark: options.write_mark,
            clock: options.clock,
            enabled: Some(Arc::new(move || switch.load(Ordering::SeqCst))),
        });
        Self {
            recall,
            settled: Mutex::new(receiver),
            enabled,
        }
    }

    /// The next settled mark, waiting for the worker to report it.
    pub fn next_settled(&self) -> MarkOutcome {
        self.settled
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30))
            .expect("a mark settles")
    }

    /// Run end: schedule the mark and wait for it to settle.
    pub fn agent_end(&self, context: &Arc<SessionFeatureContext>) -> MarkOutcome {
        self.recall.on_agent_end(context);
        self.next_settled()
    }

    pub async fn tool_call(&self, context: &Arc<SessionFeatureContext>, id: &str, code: &str) {
        self.recall
            .before_tool_call(
                context,
                &ToolCallObservation {
                    tool_call_id: id.to_string(),
                    tool_name: "ipython".to_string(),
                    args: serde_json::json!({ "code": code }),
                },
            )
            .await;
    }

    pub async fn tool_result_with(
        &self,
        context: &Arc<SessionFeatureContext>,
        id: &str,
        tool: &str,
        host_facts: serde_json::Value,
        earlier_results_of_tool: usize,
    ) -> Option<String> {
        self.recall
            .after_tool_call(
                context,
                &ToolResultObservation {
                    tool_call_id: id.to_string(),
                    tool_name: tool.to_string(),
                    args: serde_json::json!({ "code": "print('orient')" }),
                    is_error: false,
                    content: Vec::new(),
                    details: serde_json::Value::Null,
                    host_facts,
                    earlier_results_of_tool,
                },
            )
            .await
    }

    /// The first-cell block, if any, of an `ipython` result.
    pub async fn tool_result(
        &self,
        context: &Arc<SessionFeatureContext>,
        id: &str,
    ) -> Option<String> {
        self.tool_result_with(context, id, "ipython", serde_json::Value::Null, 0)
            .await
    }
}

/// Lines listed under a section header such as "Changed (1):".
pub fn section(block: &str, label: &str) -> Vec<String> {
    let lines: Vec<&str> = block.split('\n').collect();
    let Some(start) = lines.iter().position(|line| {
        line.starts_with(&format!("{label} (")) || *line == format!("{label}: none.")
    }) else {
        return Vec::new();
    };
    if lines[start].ends_with("none.") {
        return Vec::new();
    }
    lines[start + 1..]
        .iter()
        .take_while(|line| line.starts_with("  "))
        .map(|line| line.trim().to_string())
        .collect()
}

/// Session A: its run end writes the mark, and a build claim is recorded
/// against that state.
pub async fn mark_session_a(repo: &Path, agent_dir: &Path) {
    let process = Process::new();
    let outcome = process.agent_end(&context(repo, agent_dir, "session-a"));
    assert!(
        matches!(outcome, MarkOutcome::Written { .. }),
        "{outcome:?}"
    );
    let claimed = write_mark_ok(repo, agent_dir, &[claim(TSGO_CLAIM)]).await;
    let commands: Vec<&str> = claimed
        .mark
        .claims
        .iter()
        .map(|claim| claim.command.as_str())
        .collect();
    assert_eq!(commands, [TSGO_CLAIM]);
}
