//! Ports of the Python git-guard tests that reach into the guard's internals
//! (`is_destructive_git_discard_command`, `_probe_uncommitted_changes` and its
//! timeout/cap, the probe commands a patched probe recorded).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::sites::is_destructive_git_discard_command;
use super::*;

const MATCHING_COMMANDS: [&str; 122] = [
    "git checkout -- .",
    "git checkout .",
    "git checkout HEAD -- .",
    "git restore .",
    "git restore --source=HEAD~1 .",
    "git clean -f",
    "git clean -fd",
    "git clean -fdx",
    "git clean -d",
    "git clean",
    "git -c clean.requireForce=false clean",
    "git clean --force",
    "git checkout --pathspec-from-file=ps.txt",
    "git checkout HEAD --pathspec-from-file=ps.txt",
    "git reset --hard",
    "git reset --hard HEAD~1",
    "git restore --pathspec-from-file=ps.txt",
    "git restore -s HEAD --pathspec-from-file=ps.txt",
    "git restore --staged --worktree --pathspec-from-file=-",
    "G='echo hi'; G='git reset --hard' H=\"$G\"; $H",
    "git config clean.requireForce false && git clean",
    "git checkout -b tmp 2>/dev/null; git checkout -- .",
    "git checkout main && git reset --hard",
    "echo start\ngit clean -fd",
    "npm test & git clean -fd &",
    "git checkout :/",
    "git checkout -- :/",
    "git checkout HEAD -- :/",
    "git restore :/",
    "git restore -s@ .",
    "git restore -s@ :/",
    "git restore --source=HEAD :/",
    "git restore -s HEAD~1 :/",
    "git restore -s STASH .",
    "git restore -sSTASH .",
    "git restore --source HEAD .",
    "git restore -qs HEAD .",
    "git restore -Ws HEAD .",
    "git restore --no-overlay .",
    "git restore --overlay .",
    "git restore --ignore-unmerged .",
    "git restore --recurse-submodules .",
    "git restore -- .",
    "git checkout -- ./",
    "git checkout ./",
    "git restore ./",
    "git -C sub reset --hard",
    "git --git-dir=sub/.git reset --hard",
    "git reset -q --hard",
    "git reset --no-refresh --hard",
    "git -C repo -C nested reset --hard",
    "GIT_DIR=sub/.git git reset --hard",
    "GIT_DIR=sub/.git GIT_WORK_TREE=sub git reset --hard",
    "git checkout -f -- .",
    "git checkout --theirs -- .",
    "git checkout -m .",
    "git checkout --conflict=diff3 .",
    "git checkout HEAD .",
    "git checkout HEAD~1 -- .",
    "git checkout origin/main .",
    "git checkout -f main",
    "git checkout --force main",
    "git clean -f -- -n",
    "git reset 2>/dev/null --hard",
    "git reset 2> /dev/null --hard",
    "git reset 2>&1 --hard",
    "git 2>/dev/null reset --hard",
    "git restore 2>/dev/null .",
    "git clean -f 2>/dev/null",
    "git checkout 2>/dev/null -- .",
    "git restore --staged --worktree .",
    "git restore -SW .",
    "source setup.sh && git reset --hard",
    "git restore --quiet .",
    "git restore -q .",
    "git restore --quiet --source=HEAD .",
    "g\\it reset --ha\\rd",
    "git res\\et --hard",
    "\"git\" reset --hard",
    "g'it' reset --hard",
    "G=git; $G reset --hard",
    "G=git; ${G} reset --hard",
    "G=git; echo G=other; $G reset --hard",
    "G=git; printf '%s' G=other; $G reset --hard",
    "G=git; # G=other\n$G reset --hard",
    "FOO=1 cd sub && git reset --hard",
    "/usr/bin/git reset --hard",
    "./git reset --hard",
    "G='git reset --hard'; $G",
    "G=\"git restore .\"; $G",
    "G=\"it's # \"; $G git reset --hard",
    "echo 'git' 'reset' '--hard'",
    "git reset &>/dev/null --hard",
    "git reset &> /dev/null --hard",
    "git reset &>>/dev/null --hard",
    "git reset >&/dev/null --hard",
    "{ cd sub && git reset --hard; }",
    "export GIT_DIR=sub/.git GIT_WORK_TREE=sub; git reset --hard",
    "for i in 1; do export GIT_DIR=sub/.git GIT_WORK_TREE=sub; git reset --hard; done",
    "GIT_DIR=sub/.git; git reset --hard",
    "git -Csub reset --hard",
    "git -cfoo.bar=1 reset --hard",
    "git reset \\\n--hard",
    "git checkout -- \\\n.",
    "git clean -f \\\n-d",
    "cat <<EOF ; git reset --hard\nEOF",
    "cat <<EOF && git reset --hard\nEOF",
    "declare -x G=git; $G reset --hard",
    "readonly G=git; $G reset --hard",
    "export -n G=git; $G reset --hard",
    "G=other; command export G=git; $G reset --hard",
    "X=git Y='-C sub reset --hard'; $X $Y",
    "G=git; H='-C sub'; $G $H reset --hard",
    "G=git; G=other git status; $G reset --hard",
    "G=git; G=other git; $G reset --hard",
    "echo \"$(echo \")\")\"; git reset --hard",
    "V=\"$(echo \")\")\"; git reset --hard",
    "shopt -s expand_aliases\nalias g=git\ng reset --hard",
    "alias g=git\ng reset --hard",
    "alias echo=git\necho reset --hard",
    "alias git=echo\ngit reset --hard",
    "shopt -s expand_aliases\nalias g='git reset --hard'\nunalias -n g\ng",
];

const NON_MATCHING_COMMANDS: [&str; 51] = [
    "git status",
    "git log --oneline",
    "git checkout -b new-branch",
    "git checkout main",
    "git checkout -m main",
    "git checkout -b newbranch .",
    "git checkout -- single-file.txt",
    "echo 'git reset --hard'",
    "git commit -m \"git reset --hard\"",
    "echo \"git clean -fd\"",
    "echo preparing # git reset --hard",
    "git checkout ./nested",
    "git restore --staged .",
    "git restore --staged :/",
    "git restore single-file.txt",
    "git clean -n",
    "git clean -n -f .",
    "git clean --dry-run",
    "git restore --staged --pathspec-from-file=ps.txt",
    "git reset",
    "git reset --soft HEAD~1",
    "git stash",
    "git add .",
    "G='git reset --hard'; G='echo hi' H=\"$G\"; $H",
    "echo hello world",
    "npm run check",
    "git status > status.txt",
    "git log --oneline > log.txt 2>/dev/null",
    "echo 2>/dev/null hi",
    "{ echo hi; }",
    "export FOO=1",
    "git restore --staged --quiet .",
    "echo \\# git reset --hard",
    "cat <<EOF\\ngit reset --hard\\nEOF",
    "echo one  two",
    "git -Csub status",
    "# git reset --hard",
    "$G reset --hard",
    "G=git; echo x; G=other; $G reset --hard",
    "cat <<'EOF'\n$(git reset --hard)\nEOF",
    "cat <<\"EOF\"\n$(git reset --hard)\nEOF",
    "echo eval 'git reset --hard'",
    "G='git clean -n'; $G",
    "G='echo hi'; $G",
    "G=git; command export G=other; $G reset --hard",
    "shopt -s expand_aliases; alias g=git; g status",
    "alias g='echo hi'\ng reset --hard",
    "alias g=$X\ng reset --hard",
    "if true; then export GIT_DIR=sub/.git GIT_WORK_TREE=sub; fi",
    "for i in 1; do echo hi; done",
    "# 'git' reset --hard",
];

/// `DestructiveGitDetectionTest.test_matches_destructive_discards` and
/// `test_does_not_match_other_commands`.
#[test]
fn detection_matches_the_discard_taxonomy() {
    for command in MATCHING_COMMANDS {
        assert!(is_destructive_git_discard_command(command), "{command:?}");
    }
    for command in NON_MATCHING_COMMANDS {
        assert!(!is_destructive_git_discard_command(command), "{command:?}");
    }
    for pathological in [
        format!("git {} status", vec!["-x"; 60].join(" ")),
        format!("git {} status", vec!["--x"; 60].join(" ")),
        format!("git restore {}file.txt", "-s ".repeat(60)),
    ] {
        let started = Instant::now();
        assert!(!is_destructive_git_discard_command(&pathological));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

fn context_in(dir: &Path) -> GuardContext {
    let env = BTreeMap::from([
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        ),
        ("HOME".to_string(), dir.display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
    ]);
    GuardContext::new(dir, env)
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs")
        .status;
    assert!(status.success(), "git {args:?}");
}

/// A repository with one commit plus a modified and an untracked file.
fn dirty_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("tracked.txt"), "committed\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-q", "-m", "init"]);
    std::fs::write(root.join("tracked.txt"), "modified\n").expect("write");
    std::fs::write(root.join("untracked.txt"), "uncommitted\n").expect("write");
    dir
}

fn probe(command: &str) -> Probe {
    Probe {
        command: command.to_string(),
        includes_ignored_files: false,
    }
}

/// `test_probe_runs_only_for_discard_commands` and
/// `test_command_prefix_is_replayed_in_the_probe` (both recorded the probe a
/// patched `_probe_uncommitted_changes` was called with).
#[test]
fn only_discards_are_probed_and_the_prefix_is_replayed() {
    let plan = |command: &str, prefix: Option<&str>| {
        let script = Script::compose(command, prefix);
        planned_probes(
            &Script {
                command,
                script: &script,
                prefix,
            },
            false,
        )
    };
    assert_eq!(plan("echo hi", None), Ok(Vec::new()));
    assert_eq!(plan("git status", None), Ok(Vec::new()));
    assert_eq!(
        plan("git checkout -- .", None),
        Ok(vec![probe("git status --porcelain --untracked-files=all")])
    );
    assert_eq!(
        plan("git checkout -- .", Some("export GUARD_TEST_VAR=1")),
        Ok(vec![probe(
            "export GUARD_TEST_VAR=1\ngit status --porcelain --untracked-files=all"
        )])
    );
    // `test_discard_inside_command_prefix_is_refused`: no probe runs.
    let refused = plan("git status", Some("git checkout -- .")).expect_err("refused");
    assert!(
        refused.contains("changes directory (or repository) first"),
        "{refused}"
    );
}

/// `test_probe_reads_are_bounded_and_time_out`: the timeout kills the
/// probe's group (a hang, and a child that exited while a descendant holds
/// stdout) and the bytes read stay capped. The Python probe answered `[]`
/// for the descendant case (it read the empty listing after the kill); the
/// Rust probe reports the timeout, which the guard reads the same way: no
/// dirty paths, fail open.
#[test]
fn probe_reads_are_bounded_and_time_out() {
    let repo = dirty_repo();
    let context = context_in(repo.path());
    let limits = ProbeLimits {
        timeout: Duration::from_secs(1),
        ..PROBE_LIMITS
    };
    for command in ["sleep 60", "(sleep 60) & exit 0"] {
        let started = Instant::now();
        let result = probe_uncommitted_changes(&context, command, limits);
        assert!(started.elapsed() < Duration::from_secs(5), "{command}");
        assert!(result.unwrap_or_default().is_empty(), "{command}");
    }
    // `test_fails_open_when_the_probe_fails`: a failing probe lists nothing.
    assert_eq!(
        probe_uncommitted_changes(&context, "exit 1", PROBE_LIMITS),
        None
    );
    let started = Instant::now();
    let listed = probe_uncommitted_changes(&context, "yes dirty", PROBE_LIMITS).expect("listing");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!listed.is_empty());
    assert!(listed.join("\n").len() <= 64 * 1024);
}

/// `test_refusal_lists_dirty_paths_and_the_kwarg_bypass` and
/// `test_fails_open_outside_a_git_repository`, at the guard level.
#[test]
fn a_dirty_tree_refuses_and_a_non_repository_fails_open() {
    let repo = dirty_repo();
    let context = context_in(repo.path());
    assert_eq!(
        check(&Script::bare("git checkout -- ."), &context),
        Err(messages::dirty_tree(
            &[" M tracked.txt".to_string(), "?? untracked.txt".to_string()],
            false,
            false
        ))
    );
    let plain = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        check(&Script::bare("git reset --hard"), &context_in(plain.path())),
        Ok(())
    );
}
