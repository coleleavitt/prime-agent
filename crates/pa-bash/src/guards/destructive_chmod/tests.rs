//! Ported assertions of `prime-agent-runtime/test/test_bash_chmod_guard.py`
//! that reached into the guard's private helpers (the detection vectors, the
//! eval and `sh -c` payload scanners, and the cases that patched a helper).

use std::collections::BTreeMap;

use super::location::Locations;
use super::payloads::PayloadKind;
use super::{find_invocations, prepare, Chmod};
use crate::context::GuardContext;
use crate::script::Script;

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

/// `_find_recursive_chmod_chown_invocations(_prepare(command))`.
fn invocation_count(command: &str) -> usize {
    let prepared = prepare(&chars(command)).expect("prepare");
    find_invocations(&prepared.words, None).len()
}

struct Workspace {
    _root: tempfile::TempDir,
    context: GuardContext,
}

fn workspace(prefix_env: &[(&str, &str)]) -> Workspace {
    let root = tempfile::tempdir().expect("temp root");
    let real = root.path().canonicalize().expect("canonical root");
    std::fs::create_dir(real.join("work")).expect("work");
    std::fs::create_dir(real.join("home")).expect("home");
    let mut env = BTreeMap::from([
        ("HOME".to_string(), real.join("home").display().to_string()),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
    ]);
    for (name, value) in prefix_env {
        env.insert((*name).to_string(), (*value).to_string());
    }
    Workspace {
        _root: root,
        context: GuardContext::new(real.join("work"), env),
    }
}

fn payload_flagged(command: &str, kind: PayloadKind) -> bool {
    let workspace = workspace(&[]);
    let chmod = Chmod {
        locations: Locations::new(&workspace.context),
        prefix: None,
    };
    chmod
        .wrapper_payloads_hide_shell_code(&chars(command), kind)
        .expect("scan")
        .is_some()
}

fn verdict(workspace: &Workspace, command: &str, prefix: Option<&str>) -> Result<(), String> {
    let script = Script::compose(command, prefix);
    super::check(
        &Script {
            command,
            script: &script,
            prefix,
        },
        &workspace.context,
    )
}

const MATCHING: &[&str] = &[
    "chmod -R 755 sub",
    "chmod --recursive 755 sub",
    "chmod -vR 755 sub",
    "chmod -R 755 sub --reference=/tmp/mode",
    "chmod sub -R 755",
    "chmod 755 -R sub",
    "chown -R user sub",
    "chown --recursive user:group sub",
    "chown sub -R user",
    "chmod -R 755",
    "chmod -R 755 sub && chown -R user sub",
    "chmod -R 755 sub; echo done",
    "/bin/chmod -R 755 sub",
    "\"chmod\" -R 755 sub",
    "chmod '-R' 755 sub",
    "\"chown\" \"-R\" user sub",
    "\\chmod -R 755 sub",
    "sudo chmod -R 755 sub",
    "FOO=1 chmod -R 755 sub",
    "chmod -R \\\n755 sub",
    "chmod 2>/dev/null -R 755 sub",
    "chmod -R 755 sub 2>/dev/null",
    "chmod -R 755 &>/dev/null sub",
    "chmod -R 755 -- sub",
    "(chmod -R 755 sub)",
    "{ chmod -R 755 sub; }",
    "echo $(chmod -R 755 sub)",
    "chmod -R 755 sub # cleanup",
    "xargs chmod -R 755",
    "$'chmod' -R 755 sub",
    "chmod $'-R' 755 sub",
    "$\"chmod\" -R 755 sub",
    "chmod -R 755 $'sub'",
    "chmod --rec 755 sub",
    "chmod --recur 755 sub",
    "chmod --recursiv 755 sub",
    "chown --recurs user sub",
];

const NON_MATCHING: &[&str] = &[
    "chmod 755 sub",
    "chmod -v 755 sub",
    "chmod --changes 755 sub",
    "chmod -r 755 sub",
    "chmod +x sub",
    "chmod 755 .git",
    "chown user sub",
    "chown -h user sub",
    "chown user:group sub",
    "chmod -- 755 sub",
    "echo 'chmod -R 755 ~'",
    "echo \"chmod -R 755 ~\"",
    "# chmod -R 755 sub",
    "echo one \\\n two",
    "git status",
    "echo hello world",
    "npm run check",
    "echo $'chmod -R 755 sub'",
    "chmod --ref 755 sub",
    "chmod --reference=/tmp/mode 755 sub",
    "chmod --changes 755 sub",
];

/// `RecursiveChmodDetectionTest.test_matches_recursive_chmod_chown` and
/// `test_does_not_match_other_commands`.
#[test]
fn detection_vectors_match_exactly_the_recursive_invocations() {
    let unmatched: Vec<&str> = MATCHING
        .iter()
        .copied()
        .filter(|command| invocation_count(command) == 0)
        .collect();
    assert_eq!(unmatched, Vec::<&str>::new());
    let matched: Vec<&str> = NON_MATCHING
        .iter()
        .copied()
        .filter(|command| invocation_count(command) != 0)
        .collect();
    assert_eq!(matched, Vec::<&str>::new());
}

/// `RecursiveChmodDetectionTest.test_counts_each_invocation_in_compound_commands`.
#[test]
fn each_invocation_of_a_compound_command_counts() {
    assert_eq!(invocation_count("chmod -R 755 sub && chown -R u sub"), 2);
}

/// `ChmodEvalPayloadDetectionTest`.
#[test]
fn eval_payloads_hiding_recursion_are_flagged_and_safe_ones_are_not() {
    let hiding = [
        "eval 'chmod -R 755 ~'",
        "eval \"chown -R user ~\"",
        "eval 'cd sub && chmod -R 755 .'",
        "eval \"chmod -R 755 ~\"",
        "eval 'eval \"chmod -R 755 ~\"'",
        "\"eval\" \"chmod -R 755 ~\"",
        "eval $(echo 'chmod -R 755 ~')",
        "eval 'chmod -R \\\n755 ~'",
        "eval 'bash -c \"chmod -R 755 ~\"'",
        "eval 'bash -c \"chown -R user ~\"'",
        "eval $'chmod -R 755 ~'",
        "e\"val\" 'chmod -R 755 ~'",
        "ev\"al\" 'chmod -R 755 ~'",
        "$'eval' 'chmod -R 755 ~'",
    ];
    let missed: Vec<&str> = hiding
        .into_iter()
        .filter(|command| !payload_flagged(command, PayloadKind::Eval))
        .collect();
    assert_eq!(missed, Vec::<&str>::new());
    let safe = [
        "eval",
        "eval 'echo hi'",
        "eval 'chmod 755 sub'",
        "eval \"echo 'chmod -R 755 ~'\"",
        "eval 'echo \"chmod -R 755 ~\"'",
        "echo 'eval chmod -R 755 ~'",
        "npm run eval:suite",
    ];
    let flagged: Vec<&str> = safe
        .into_iter()
        .filter(|command| payload_flagged(command, PayloadKind::Eval))
        .collect();
    assert_eq!(flagged, Vec::<&str>::new());
}

/// `ShellCPayloadDetectionTest`.
#[test]
fn shell_c_payloads_hiding_shell_code_are_flagged_and_safe_ones_are_not() {
    let hiding = [
        "sh -c 'chmod -R 755 ~'",
        "bash -c \"chown -R user ~\"",
        "bash -lc 'chmod -R 755 ~'",
        "bash -xc 'chmod -R 755 ~'",
        "zsh -c 'chmod -R 755 ~'",
        "sh -e -c 'chmod -R 755 ~'",
        "sh -c $(echo 'chmod -R 755 ~')",
        "FOO=1 sh -c 'chmod -R 755 ~'",
        "sh -c 'bash -c \"chmod -R 755 ~\"'",
        "bash -c 'eval \"chmod -R 755 ~\"'",
        "bash -c $'chmod -R 755 ~'",
        "bash -c $\"chmod -R 755 ~\"",
        "bash -c '$cmd -R 755 ~'",
        "bash -c 'BASH_ENV=/tmp/x echo hi'",
        "bash -c 'bash <(printf \"chmod -R 755 ~\")'",
        "b\"ash\" -c 'chmod -R 755 ~'",
        "$'bash' -c 'chmod -R 755 ~'",
    ];
    let missed: Vec<&str> = hiding
        .into_iter()
        .filter(|command| !payload_flagged(command, PayloadKind::ShellC))
        .collect();
    assert_eq!(missed, Vec::<&str>::new());
    let safe = [
        "sh -c 'echo hi'",
        "sh -c 'echo \"chmod -R 755 ~\"'",
        "bash -c \"echo 'chmod -R 755 ~'\"",
        "bash -lc 'chmod 755 sub'",
        "sh --rcfile x -c 'echo hi'",
        "echo 'bash -c chmod -R 755 ~'",
        "sh -c 'echo $x -R hi'",
        "sh -c 'echo BASH_ENV=x'",
    ];
    let flagged: Vec<&str> = safe
        .into_iter()
        .filter(|command| payload_flagged(command, PayloadKind::ShellC))
        .collect();
    assert_eq!(flagged, Vec::<&str>::new());
}

/// `RecursiveChmodGuardTest.test_guard_scan_is_not_quadratic`: thousands of
/// separators stay linear.
#[test]
fn the_scan_stays_linear_in_command_length() {
    let workspace = workspace(&[]);
    let command = vec!["echo hi"; 4000].join("; ");
    let start = std::time::Instant::now();
    for _ in 0..3 {
        assert_eq!(verdict(&workspace, &command, None), Ok(()));
    }
    assert!(
        start.elapsed() < std::time::Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

/// `RecursiveChmodGuardTest.test_guard_only_resolves_on_pattern_match` (it
/// patched `_resolve_chmod_effective_cwd` to observe that commands without a
/// recursive invocation never reach the resolver): the verdicts it pinned.
#[test]
fn only_a_recursive_invocation_reaches_operand_resolution() {
    let workspace = workspace(&[]);
    std::fs::create_dir(workspace.context.cwd().join("sub")).expect("sub");
    assert_eq!(verdict(&workspace, "echo hi", None), Ok(()));
    assert_eq!(verdict(&workspace, "chmod 755 sub", None), Ok(()));
    let refusal = verdict(&workspace, "chmod -R 755 ~", None).expect_err("refused");
    assert!(refusal.contains("names the home directory"), "{refusal}");
}

/// `RecursiveChmodGuardTest.test_wrapper_script_gate_uses_the_captured_prefix`
/// (it patched `_guard_destructive_chmod` to write the prefix variable
/// mid-call): the check reads only the prefix it was handed, so a relocating
/// prefix refuses the wrapper script whatever the environment says now.
#[test]
fn the_wrapper_script_gate_reads_the_captured_prefix() {
    let workspace = workspace(&[("PRIME_AGENT_BASH_COMMAND_PREFIX", "")]);
    std::fs::write(workspace.context.cwd().join("safe-name.sh"), ":\n").expect("script");
    let refusal = verdict(&workspace, "bash safe-name.sh", Some("cd /tmp")).expect_err("refused");
    assert!(refusal.contains("changes directory"), "{refusal}");
    assert_eq!(verdict(&workspace, "bash safe-name.sh", None), Ok(()));
}
