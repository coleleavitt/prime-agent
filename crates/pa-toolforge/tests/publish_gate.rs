//! The double-run gate end to end through real interpreters: stage, gate,
//! promote, ledger (the TS `toolforge double-run gate` suite).

mod common;

use std::sync::{Arc, Mutex};

use common::{gate_python, options, slugify_request, SLUGIFY_SOURCE};
use pa_toolforge::{
    load_ledger, publish, published_packages, GatePhase, PublishRequest, PublishResult,
    PublishStatus, PublishedPackage, RejectionStage,
};

/// The gate's (phase, outcome, ok) triples, durations and details aside.
fn gate_shape(result: &PublishResult) -> Vec<(GatePhase, String, bool)> {
    result
        .gate
        .iter()
        .map(|run| (run.phase, run.outcome.clone(), run.ok))
        .collect()
}

#[tokio::test]
async fn publishes_when_the_exit_test_fails_without_and_passes_with() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &python, &installs);

    let result = publish(&slugify_request(), &options).await;

    let package_root = dir.path().join("skills").join("slugify");
    assert_eq!(
        result,
        PublishResult {
            status: PublishStatus::Published,
            name: "slugify".to_string(),
            import_name: "slugify".to_string(),
            package_path: package_root.display().to_string(),
            src_path: package_root.join("src").display().to_string(),
            version: 1,
            installed: false,
            gate: result.gate.clone(),
            reason: None,
            install_detail: Some("test installer: promoted without installing".to_string()),
            rejection: None,
        }
    );
    assert_eq!(
        gate_shape(&result),
        vec![
            (GatePhase::Negative, "raised".to_string(), true),
            (GatePhase::Positive, "clean".to_string(), true),
        ]
    );
    assert_eq!(
        result.gate[0].detail,
        "NotImplementedError: toolforge negative run: slugify.run is not implemented"
    );
    assert_eq!(*installs.lock().unwrap(), vec![package_root.clone()]);
    assert!(package_root.join("SKILL.md").is_file());
    assert!(package_root.join("pyproject.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(package_root.join("src/slugify/__init__.py")).unwrap(),
        SLUGIFY_SOURCE
    );
    assert!(std::fs::read_to_string(package_root.join("_exit_test.py"))
        .unwrap()
        .contains("slugify.run"));
    // Nothing is left in staging.
    assert_eq!(
        std::fs::read_dir(dir.path().join("toolforge/staging"))
            .unwrap()
            .count(),
        0
    );

    let ledger = load_ledger(&options.ledger_path);
    assert_eq!(ledger.records.len(), 1);
    let record = &ledger.records[0];
    assert_eq!(
        (
            record.name.as_str(),
            record.status,
            record.version,
            record.session_id.as_deref(),
            &record.gate
        ),
        (
            "slugify",
            PublishStatus::Published,
            1,
            Some("toolforge-test"),
            &result.gate
        )
    );
    assert_eq!(
        published_packages(dir.path()),
        vec![PublishedPackage {
            name: "slugify".to_string(),
            import_name: "slugify".to_string(),
            package_path: package_root.clone(),
            src_path: package_root.join("src"),
            version: 1,
        }]
    );
}

#[tokio::test]
async fn rejects_an_exit_test_that_passes_without_the_implementation() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &python, &installs);

    let result = publish(
        &PublishRequest {
            exit_test: "import slugify\n\nassert True\n".to_string(),
            ..slugify_request()
        },
        &options,
    )
    .await;

    assert_eq!(result.status, PublishStatus::Rejected);
    assert_eq!(result.rejection, Some(RejectionStage::Negative));
    assert_eq!(
        result.reason.as_deref(),
        Some("negative run did not fail: the exit test must raise against a stub that implements nothing, but it passed. replay case completed without raising")
    );
    assert_eq!(
        gate_shape(&result),
        vec![(GatePhase::Negative, "clean".to_string(), false)]
    );
    assert!(!dir.path().join("skills/slugify").exists());
    assert!(installs.lock().unwrap().is_empty());
    let ledger = load_ledger(&options.ledger_path);
    assert_eq!(
        ledger
            .records
            .iter()
            .map(|record| record.status)
            .collect::<Vec<_>>(),
        vec![PublishStatus::Rejected]
    );
}

#[tokio::test]
async fn rejects_an_implementation_that_fails_its_own_exit_test() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &python, &installs);

    let result = publish(
        &PublishRequest {
            source: "def run(text):\n    return \"nope\"\n".to_string(),
            ..slugify_request()
        },
        &options,
    )
    .await;

    assert_eq!(result.status, PublishStatus::Rejected);
    assert_eq!(result.rejection, Some(RejectionStage::Positive));
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with(
                "positive run did not pass: the exit test must succeed against the real package, but it raised. AssertionError: nope"
            )),
        "{:?}",
        result.reason
    );
    assert_eq!(
        result
            .gate
            .iter()
            .map(|run| (run.phase, run.ok))
            .collect::<Vec<_>>(),
        vec![(GatePhase::Negative, true), (GatePhase::Positive, false)]
    );
    assert!(!dir.path().join("skills/slugify").exists());
}

#[tokio::test]
async fn rejects_a_shadowing_name_before_spawning_anything() {
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    // A path that cannot run: the name check must answer before any spawn.
    let options = options(dir.path(), &dir.path().join("no-python"), &installs);

    let result = publish(
        &PublishRequest {
            name: "json".to_string(),
            ..slugify_request()
        },
        &options,
    )
    .await;

    assert_eq!(
        result,
        PublishResult {
            status: PublishStatus::Rejected,
            name: "json".to_string(),
            import_name: String::new(),
            package_path: String::new(),
            src_path: String::new(),
            version: 0,
            installed: false,
            gate: Vec::new(),
            reason: Some("toolforge name \"json\" collides with a Python builtin, keyword, stdlib module or kernel-bound name (json); pick a name nothing else answers to".to_string()),
            install_detail: None,
            rejection: Some(RejectionStage::Name),
        }
    );
    assert!(!dir.path().join("skills").exists());
    assert!(
        !options.ledger_path.exists(),
        "a name refusal is not recorded"
    );
}

#[tokio::test]
async fn rejects_a_name_a_loaded_skill_already_holds() {
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let mut options = options(dir.path(), &dir.path().join("no-python"), &installs);
    options.loaded_import_names = vec!["slugify".to_string()];

    let result = publish(&slugify_request(), &options).await;

    assert_eq!(
        result.reason.as_deref(),
        Some("toolforge name \"slugify\" collides with the loaded skill slugify")
    );
    assert_eq!(result.rejection, Some(RejectionStage::Name));
}

#[tokio::test]
async fn a_shape_refusal_is_recorded_without_running_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &dir.path().join("no-python"), &installs);

    let result = publish(
        &PublishRequest {
            exit_test: "x".repeat(16_001),
            ..slugify_request()
        },
        &options,
    )
    .await;

    assert_eq!(
        result.reason.as_deref(),
        Some("toolforge exit_test exceeds 16000 characters (16001)")
    );
    assert_eq!(result.rejection, Some(RejectionStage::Shape));
    assert!(result.gate.is_empty());
    let ledger = load_ledger(&options.ledger_path);
    assert_eq!(
        ledger
            .records
            .iter()
            .map(|record| (record.status, record.reason.clone()))
            .collect::<Vec<_>>(),
        vec![(
            PublishStatus::Rejected,
            Some("toolforge exit_test exceeds 16000 characters (16001)".to_string())
        )]
    );
}

#[tokio::test]
async fn without_an_interpreter_nothing_is_published() {
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &dir.path().join("no-python"), &installs);

    let result = publish(&slugify_request(), &options).await;

    assert_eq!(result.status, PublishStatus::Rejected);
    assert_eq!(result.rejection, Some(RejectionStage::Negative));
    assert_eq!(
        gate_shape(&result),
        vec![(GatePhase::Negative, "unrunnable".to_string(), false)]
    );
    assert!(installs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn replaces_a_published_package_and_bumps_its_version() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &python, &installs);

    assert_eq!(
        publish(&slugify_request(), &options).await.status,
        PublishStatus::Published
    );
    let second = publish(
        &PublishRequest {
            source: format!("{SLUGIFY_SOURCE}\n\nMARKER = \"second\"\n"),
            ..slugify_request()
        },
        &options,
    )
    .await;

    assert_eq!(
        (second.status, second.version),
        (PublishStatus::Published, 2)
    );
    assert!(
        std::fs::read_to_string(dir.path().join("skills/slugify/src/slugify/__init__.py"))
            .unwrap()
            .contains("MARKER = \"second\"")
    );
    let entries: Vec<String> = std::fs::read_dir(dir.path().join("skills"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["slugify".to_string()]);
}

#[tokio::test]
async fn a_hanging_exit_test_times_out_as_unrunnable() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let mut options = options(dir.path(), &python, &installs);
    options.timeout = std::time::Duration::from_millis(500);

    let result = publish(
        &PublishRequest {
            exit_test: "import threading\nthreading.Event().wait()\n".to_string(),
            ..slugify_request()
        },
        &options,
    )
    .await;

    assert_eq!(result.rejection, Some(RejectionStage::Negative));
    assert_eq!(
        result
            .gate
            .iter()
            .map(|run| (run.outcome.as_str(), run.detail.as_str()))
            .collect::<Vec<_>>(),
        vec![("unrunnable", "replay case timed out after 500ms")]
    );
}

/// A process the exit test starts must not outlive the gate: the run leads
/// its own process group, and the group is killed when the run ends.
#[cfg(unix)]
#[tokio::test]
async fn a_process_the_exit_test_leaves_behind_is_killed() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(dir.path(), &python, &installs);
    let pid_file = dir.path().join("grandchild.pid");

    let result = publish(
        &PublishRequest {
            exit_test: format!(
                "import subprocess\nchild = subprocess.Popen(['sleep', '60'])\nopen({:?}, 'w').write(str(child.pid))\nimport slugify\nassert slugify.run('A B') == 'a-b'\n",
                pid_file.display().to_string()
            ),
            ..slugify_request()
        },
        &options,
    )
    .await;
    assert_eq!(result.status, PublishStatus::Published);

    let pids: Vec<u32> = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .into_iter()
        .collect();
    assert_eq!(pids.len(), 1, "the exit test recorded its grandchild");
    // The kill is delivered before publish returns; the reaper (init or a
    // subreaper) collects the zombie asynchronously, so wait for it with a
    // deadline rather than a fixed sleep.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pa_core::platform::process::pid_exists(pids[0])
        && !is_zombie(pids[0])
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !pa_core::platform::process::pid_exists(pids[0]) || is_zombie(pids[0]),
        "the grandchild {} outlived the gate",
        pids[0]
    );
}

#[cfg(unix)]
fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}
