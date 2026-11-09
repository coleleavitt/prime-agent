//! The CLI's direct Python dispatch must use the same project startup boundary
//! as kernel probes and the persistent REPL.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn factory_dispatch_and_probes_drop_project_python_paths() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let trusted = root.path().join("trusted");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&trusted).unwrap();
    let python = root.path().join("fake-python");
    std::fs::write(
        &python,
        r#"#!/bin/sh
[ "$1" = "-P" ] || exit 41
[ "$2" = "-s" ] || exit 45
[ "$PYTHONNOUSERSITE" = "1" ] || exit 46
[ -z "${PYTHONPATH+x}" ] || exit 42
[ -z "${PYTHONHOME+x}" ] || exit 43
[ -z "${PYTHONUSERBASE+x}" ] || exit 44
case "$*" in
  *rlm.factory*)
    touch "$DISPATCH_MARKER"
    printf '%s\n' '[]'
    ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    let marker = root.path().join("factory-dispatched");
    let path = std::env::join_paths([Path::new("."), &project, &trusted]).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    pa_types::platform::test_isolation::TestState::for_agent_dir(root.path().join("agent"))
        .apply(&mut command);
    let output = command
        .args(["factory", "list", "--json"])
        .current_dir(&project)
        .env("PRIME_AGENT_KERNEL_PYTHON", &python)
        .env("PYTHONPATH", path)
        .env("PYTHONHOME", &project)
        .env("PYTHONUSERBASE", &project)
        .env("DISPATCH_MARKER", &marker)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        marker.exists(),
        "the factory library lookup never reached Python"
    );
}

#[test]
fn missing_home_reports_agent_directory_requirement_without_a_panic() {
    // Isolated first (no ambient state redirect reaches the binary), then
    // without the two variables the agent dir resolves from.
    let state = tempfile::tempdir().unwrap();
    let homeless = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
        pa_types::platform::test_isolation::TestState::new(state.path()).apply(&mut command);
        command
            .args(args)
            .env_remove("HOME")
            .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
            .output()
            .unwrap()
    };
    let output = homeless(&["config"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HOME or PRIME_AGENT_CODING_AGENT_DIR is required"),
        "unexpected stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "unexpected panic: {stderr}");

    let help = homeless(&["--help"]);
    assert!(help.status.success(), "help should not require a home");

    for args in [["help", "package"], ["package", "--help"]] {
        let help = homeless(&args);
        assert!(
            help.status.success(),
            "{args:?} should show help without a home: {}",
            String::from_utf8_lossy(&help.stderr)
        );
    }

    let package = homeless(&["package", "list"]);
    assert_eq!(package.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&package.stderr)
            .contains("HOME or PRIME_AGENT_CODING_AGENT_DIR is required")
    );
}
