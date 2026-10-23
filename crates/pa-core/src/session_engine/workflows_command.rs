//! `/workflows`: the saved workflows `rlm.workflow.run(name=...)` resolves and this session's
//! recent workflow runs.
//!
//! Claude Code's Workflow tool takes a `name` resolved from `.claude/workflows/` (project dirs up
//! to the repository root, then `~/.claude/workflows`; the nearer project directory wins) and its
//! `/workflows` command watches the runs. Here a saved workflow is a Python script (`main(wf)` and
//! an optional `meta` dict), named by its file stem, searched in the same order as the kernel's
//! `rlm.workflow` resolver ([`saved_workflow_dirs`]): `.prime/workflows/` then
//! `.claude/workflows/` in the working directory and each ancestor, then the user's
//! `<agent dir>/workflows/` and `~/.claude/workflows/`. Claude Code's saved workflows are
//! JavaScript; `.js` files are not runnable here and are not listed.
//!
//! The runs are the session's `workflows/<run id>/run.json` records (the journal directories
//! `rlm.workflow` writes under the session artifact directory).

use std::path::{Path, PathBuf};

/// The project-relative saved-workflow directories, in precedence order.
pub const PROJECT_WORKFLOW_DIRS: [&str; 2] = [".prime/workflows", ".claude/workflows"];
/// The runs listed at most.
const MAX_RUNS: usize = 10;

/// Where `/workflows` looks.
#[derive(Debug, Clone, Default)]
pub struct WorkflowsContext {
    /// The agent directory (`~/.prime/agent`): its `workflows/` holds the user's saved
    /// workflows.
    pub agent_dir: PathBuf,
    /// The session artifact directory whose `workflows/` holds the run journals.
    pub session_artifact_dir: Option<PathBuf>,
}

/// One saved workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedWorkflow {
    /// The file stem: `rlm.workflow.run(name=...)` takes it.
    pub name: String,
    /// `meta["description"]`, when the script's `meta` literal carries one.
    pub description: Option<String>,
    pub path: PathBuf,
    /// `project` or `user`.
    pub scope: &'static str,
}

/// The saved-workflow directories in precedence order: each project directory (the working
/// directory and its ancestors, nearest first, up to the repository root; the walk stops before
/// the home directory) contributes `.prime/workflows` then `.claude/workflows`; then the user's
/// `<agent dir>/workflows` and `<claude dir>/workflows`.
#[must_use]
pub fn saved_workflow_dirs(
    cwd: &Path,
    agent_dir: &Path,
    home: Option<&Path>,
    claude_dir: Option<&Path>,
) -> Vec<(PathBuf, &'static str)> {
    let mut dirs = Vec::new();
    for dir in crate::skills::claude_skills::project_ancestors(cwd, home) {
        for relative in PROJECT_WORKFLOW_DIRS {
            dirs.push((dir.join(relative), "project"));
        }
    }
    dirs.push((agent_dir.join("workflows"), "user"));
    if let Some(claude_dir) = claude_dir {
        dirs.push((claude_dir.join("workflows"), "user"));
    }
    dirs
}

/// Every saved workflow the directories hold; the first of a name wins.
#[must_use]
pub fn discover_saved_workflows(dirs: &[(PathBuf, &'static str)]) -> Vec<SavedWorkflow> {
    let mut found: Vec<SavedWorkflow> = Vec::new();
    for (dir, scope) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "py") && path.is_file())
            .collect();
        files.sort();
        for path in files {
            let Some(name) = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
            else {
                continue;
            };
            if name.starts_with('_') || found.iter().any(|known| known.name == name) {
                continue;
            }
            let description = std::fs::read_to_string(&path)
                .ok()
                .and_then(|source| meta_string(&source, "description"));
            found.push(SavedWorkflow {
                name,
                description,
                path,
                scope,
            });
        }
    }
    found.sort_by(|left, right| left.name.cmp(&right.name));
    found
}

/// A string value of the script's top-level `meta = {...}` literal (best effort: the kernel reads
/// the literal with `ast.literal_eval`; the listing only needs a plain quoted string).
fn meta_string(source: &str, key: &str) -> Option<String> {
    let start = source.lines().position(|line| {
        let line = line.trim_start_matches('\u{feff}');
        line.starts_with("meta") && {
            let rest = line["meta".len()..].trim_start();
            rest.starts_with('=') || rest.starts_with(':')
        }
    })?;
    let block: String = source
        .lines()
        .skip(start)
        .take_while(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let pattern = fancy_regex::Regex::new(&format!(
        r#"["']{}["']\s*:\s*(?:"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)')"#,
        fancy_regex::escape(key)
    ))
    .ok()?;
    let captures = pattern.captures(&block).ok()??;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str().replace("\\\"", "\"").replace("\\'", "'"))
}

/// One recorded run (`run.json`).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRun {
    pub run_id: String,
    pub name: String,
    pub status: String,
    pub started_at: f64,
    pub agents: Option<u64>,
}

/// The session's recorded runs, newest first.
#[must_use]
pub fn recent_runs(session_artifact_dir: &Path) -> Vec<WorkflowRun> {
    let root = session_artifact_dir.join("workflows");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut runs: Vec<WorkflowRun> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("wf_"))
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("run.json")).ok())
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter_map(|record| {
            let text = |key: &str| record.get(key).and_then(serde_json::Value::as_str);
            Some(WorkflowRun {
                run_id: text("runId")?.to_string(),
                name: text("name").unwrap_or("workflow").to_string(),
                status: text("status").unwrap_or("unknown").to_string(),
                started_at: record
                    .get("startedAt")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                agents: record.get("agents").and_then(serde_json::Value::as_u64),
            })
        })
        .collect();
    runs.sort_by(|left, right| right.started_at.total_cmp(&left.started_at));
    runs
}

/// The `/workflows` report.
#[must_use]
pub fn workflows_report(context: &WorkflowsContext, cwd: &Path) -> String {
    let home = pa_types::platform::home_dir();
    let claude_dir = crate::skills::claude_commands::user_claude_dir();
    let dirs = saved_workflow_dirs(
        cwd,
        &context.agent_dir,
        home.as_deref(),
        claude_dir.as_deref(),
    );
    let saved = discover_saved_workflows(&dirs);
    let runs = context
        .session_artifact_dir
        .as_deref()
        .map(recent_runs)
        .unwrap_or_default();
    render_report(&saved, &runs, cwd)
}

fn render_report(saved: &[SavedWorkflow], runs: &[WorkflowRun], cwd: &Path) -> String {
    let mut lines = Vec::new();
    if saved.is_empty() {
        lines.push(
            "No saved workflows. Save a workflow script (`async def main(wf)`, optional `meta` dict) as .prime/workflows/<name>.py or .claude/workflows/<name>.py; run it with `await rlm.workflow.run(name=\"<name>\")`."
                .to_string(),
        );
    } else {
        lines.push("Saved workflows (run with `await rlm.workflow.run(name=...)`):".to_string());
        for workflow in saved {
            let shown = workflow
                .path
                .strip_prefix(cwd)
                .unwrap_or(&workflow.path)
                .display()
                .to_string();
            let description = workflow
                .description
                .as_deref()
                .map(|text| format!(" - {text}"))
                .unwrap_or_default();
            lines.push(format!(
                "  {} ({}, {shown}){description}",
                workflow.name, workflow.scope
            ));
        }
    }
    lines.push(String::new());
    if runs.is_empty() {
        lines.push("No workflow runs in this session.".to_string());
    } else {
        lines.push("Recent runs (`await rlm.workflow.status(run_id)`):".to_string());
        for run in runs.iter().take(MAX_RUNS) {
            let agents = run
                .agents
                .map(|count| format!(", {count} agent{}", if count == 1 { "" } else { "s" }))
                .unwrap_or_default();
            lines.push(format!(
                "  {} {} - {}{agents}",
                run.run_id, run.name, run.status
            ));
        }
        if runs.len() > MAX_RUNS {
            lines.push(format!("  ... {} older", runs.len() - MAX_RUNS));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_nearest_directory_wins_and_prime_before_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let cwd = repo.join("app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let agent_dir = tmp.path().join("agent");
        let claude_dir = tmp.path().join("claude");
        write(
            &cwd.join(".claude/workflows/review.py"),
            "meta = {\n    \"name\": \"review\",\n    \"description\": \"Review the diff\",\n}\n\nasync def main(wf):\n    pass\n",
        );
        write(
            &cwd.join(".prime/workflows/review.py"),
            "async def main(wf): pass\n",
        );
        write(
            &repo.join(".claude/workflows/triage.py"),
            "meta = {'description': 'Triage issues'}\n",
        );
        write(
            &agent_dir.join("workflows/triage.py"),
            "async def main(wf): pass\n",
        );
        write(
            &claude_dir.join("workflows/mine.py"),
            "async def main(wf): pass\n",
        );
        write(
            &claude_dir.join("workflows/legacy.js"),
            "export const meta = {}\n",
        );
        write(&claude_dir.join("workflows/_helpers.py"), "X = 1\n");
        let dirs = saved_workflow_dirs(&cwd, &agent_dir, Some(tmp.path()), Some(&claude_dir));
        let saved = discover_saved_workflows(&dirs);
        assert_eq!(
            saved,
            vec![
                SavedWorkflow {
                    name: "mine".to_string(),
                    description: None,
                    path: claude_dir.join("workflows/mine.py"),
                    scope: "user",
                },
                SavedWorkflow {
                    name: "review".to_string(),
                    description: None,
                    path: cwd.join(".prime/workflows/review.py"),
                    scope: "project",
                },
                SavedWorkflow {
                    name: "triage".to_string(),
                    description: Some("Triage issues".to_string()),
                    path: repo.join(".claude/workflows/triage.py"),
                    scope: "project",
                },
            ]
        );
        assert_eq!(
            meta_string(
                &std::fs::read_to_string(cwd.join(".claude/workflows/review.py")).unwrap(),
                "description"
            ),
            Some("Review the diff".to_string())
        );
    }

    #[test]
    fn the_report_lists_saved_workflows_and_recent_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write(
            &cwd.join(".prime/workflows/audit.py"),
            "meta = {\"description\": \"Audit deps\"}\nasync def main(wf): pass\n",
        );
        let artifacts = tmp.path().join("artifacts");
        for (id, name, status, started, agents) in [
            ("wf_aaaaaaaaaaaa", "audit", "completed", 10.0, 3),
            ("wf_bbbbbbbbbbbb", "review", "running", 20.0, 1),
        ] {
            write(
                &artifacts.join("workflows").join(id).join("run.json"),
                &serde_json::json!({
                    "runId": id, "name": name, "status": status,
                    "startedAt": started, "agents": agents,
                })
                .to_string(),
            );
        }
        let context = WorkflowsContext {
            agent_dir: tmp.path().join("agent"),
            session_artifact_dir: Some(artifacts),
        };
        let saved =
            discover_saved_workflows(&saved_workflow_dirs(&cwd, &context.agent_dir, None, None));
        let runs = recent_runs(context.session_artifact_dir.as_deref().unwrap());
        assert_eq!(
            render_report(&saved, &runs, &cwd),
            "Saved workflows (run with `await rlm.workflow.run(name=...)`):\n  audit (project, .prime/workflows/audit.py) - Audit deps\n\nRecent runs (`await rlm.workflow.status(run_id)`):\n  wf_bbbbbbbbbbbb review - running, 1 agent\n  wf_aaaaaaaaaaaa audit - completed, 3 agents"
        );
        assert!(render_report(&[], &[], &cwd).ends_with("\n\nNo workflow runs in this session."));
    }
}
