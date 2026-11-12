//! The task-completion gate: scan the current change for stubs and decide
//! whether the agent may mark its task done.

use std::path::{Path, PathBuf};

/// Result of evaluating a task's work output.
#[derive(Debug, Clone)]
pub struct EvaluationResult {
    pub passed: bool,
    pub issues: Vec<EvaluationIssue>,
}

#[derive(Debug, Clone)]
pub struct EvaluationIssue {
    pub file: PathBuf,
    pub line: usize,
    pub pattern: String,
    pub context: String,
}

impl std::fmt::Display for EvaluationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.passed {
            write!(f, "Evaluation passed: no stub patterns detected")
        } else {
            writeln!(
                f,
                "Evaluation FAILED: {} stub patterns detected:",
                self.issues.len()
            )?;
            for issue in &self.issues {
                writeln!(
                    f,
                    "  {}:{} — found `{}` in: {}",
                    issue.file.display(),
                    issue.line,
                    issue.pattern,
                    issue.context.trim()
                )?;
            }
            Ok(())
        }
    }
}

/// True for source files we want the scaffold detector to scan.
fn is_source_file(name: &str) -> bool {
    name.ends_with(".rs")
        || name.ends_with(".ts")
        || name.ends_with(".tsx")
        || name.ends_with(".py")
        || name.ends_with(".go")
        || name.ends_with(".js")
        || name.ends_with(".jsx")
}

/// Run a `git` invocation in `git_root` and return its stdout lines mapped to
/// absolute paths, filtered to source files. Returns `None` on failure.
fn git_source_paths(git_root: &Path, args: &[&str]) -> Option<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(git_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| is_source_file(l))
            .map(|l| git_root.join(l))
            .collect(),
    )
}

/// Collect untracked, not-ignored new source files. These have no diff base,
/// so they're scanned whole (every line is "added").
fn untracked_source_files(git_root: &Path) -> Vec<PathBuf> {
    git_source_paths(git_root, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default()
}

/// Per-file unified diff against HEAD (staged + unstaged), source files only.
/// Returns `(relative_path, diff_body)` pairs. Splitting on the `diff --git`
/// header keeps each file's hunk text isolated so the scanner attributes
/// findings to the right file.
fn changed_file_diffs(git_root: &Path) -> Vec<(PathBuf, String)> {
    let output = std::process::Command::new("git")
        .args(["diff", "HEAD", "--unified=0"])
        .current_dir(git_root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_body = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Flush the previous file.
            if let Some(p) = current_path.take()
                && is_source_file(&p.to_string_lossy())
            {
                out.push((p, std::mem::take(&mut current_body)));
            }
            current_body.clear();
            // `a/path b/path` → take the b/ side.
            let b_path = rest
                .split_whitespace()
                .next_back()
                .and_then(|s| s.strip_prefix("b/"))
                .unwrap_or("");
            current_path = (!b_path.is_empty()).then(|| git_root.join(b_path));
        } else if current_path.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if let Some(p) = current_path
        && is_source_file(&p.to_string_lossy())
    {
        out.push((p, current_body));
    }
    out
}

/// Evaluate the **current change** for scaffold/stub patterns using the
/// weighted [`crate::scaffold_detector`].
///
/// Diff-aware: only lines the change *added* are evaluated (via
/// [`scaffold_detector::scan_added_lines`] over `git diff HEAD`), plus
/// untracked new files scanned whole. This is the key correctness property —
/// pre-existing patterns in a file the change merely touched do NOT block
/// completion; only stubs the change introduced do.
///
/// A finding set blocks completion when it contains any `Critical` indicator
/// (e.g. `unimplemented!()`) or its cumulative weight crosses
/// [`scaffold_detector::DEFAULT_BLOCK_THRESHOLD`]. Test/fixture files have
/// their findings downgraded one tier.
#[must_use]
pub fn evaluate_work_quality(git_root: &Path) -> EvaluationResult {
    use crate::scaffold_detector;

    let mut issues = Vec::new();
    let mut all_findings = Vec::new();

    let mut record = |file: &Path, findings: Vec<scaffold_detector::ScaffoldFinding>| {
        for f in findings {
            all_findings.push(f.clone());
            issues.push(EvaluationIssue {
                file: file.to_path_buf(),
                line: f.line,
                pattern: format!(
                    "{} [{}/{}]",
                    f.matched,
                    f.severity.label(),
                    f.category.label()
                ),
                context: f.context,
            });
        }
    };

    // Added lines of tracked changes (diff-aware — ignores pre-existing code).
    for (file, diff) in changed_file_diffs(git_root) {
        if scaffold_detector::is_self_referential(&file) {
            continue;
        }
        let findings =
            scaffold_detector::scan_added_lines(&diff, scaffold_detector::is_test_path(&file));
        record(&file, findings);
    }

    // Untracked new files have no diff base — scan whole (all lines are new).
    for file in untracked_source_files(git_root) {
        record(&file, scaffold_detector::scan_file(&file));
    }

    let blocked =
        scaffold_detector::should_block(&all_findings, scaffold_detector::DEFAULT_BLOCK_THRESHOLD);

    EvaluationResult {
        // `passed` is the inverse of "should block". Low-weight findings are
        // reported in `issues` for visibility but do not fail the gate.
        passed: !blocked,
        issues,
    }
}
