// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration tests for the callable Python skill wrapper the kernel bootstrap installs
//! around every skill that exposes `run()`: a skill laid out as `<skill>/<skill>.py` is callable as
//! `<skill>.<skill>(...)` (#2221), and the wrapper pickles by reference so a variable holding it
//! survives a namespace snapshot (#1278). The kernel Python is ambient; skipped when absent.

use std::path::{Path, PathBuf};

use pa_core::kernel::bootstrap::KernelPythonSkill;
use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

/// A skill package in the bundled `attach_image`/`websearch` layout: the package re-exports
/// `run` from a same-named submodule, so `demo_skill.demo_skill` is that submodule.
fn write_demo_skill(root: &Path) {
    let package = root.join("demo_skill");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("__init__.py"), "from .demo_skill import run\n").unwrap();
    std::fs::write(
        package.join("demo_skill.py"),
        "async def run(value):\n    \"\"\"Add one.\"\"\"\n    return value + 1\n",
    )
    .unwrap();
}

async fn run_cell(
    manager: &pa_core::kernel::manager::ReplKernelManager,
    code: &str,
) -> (ExecuteStatus, String) {
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .expect("execute");
    let text = result.result.unwrap_or_default();
    let detail = result
        .error
        .map(|error| format!("{}: {}", error.ename, error.evalue))
        .unwrap_or_default();
    (result.status, format!("{text}{detail}"))
}

/// Boot a kernel with the demo skill pre-imported (the bootstrap wraps it).
async fn boot(
    python: PathBuf,
    dir: &Path,
) -> (
    IpythonKernelProvisioner,
    pa_core::kernel::manager::ReplKernelManager,
) {
    write_demo_skill(dir);
    let provisioner = IpythonKernelProvisioner::new(
        dir,
        IpythonKernelProvisionerOptions {
            python: Some(python),
            python_skills: vec![KernelPythonSkill {
                name: "demo-skill".to_string(),
                import_name: "demo_skill".to_string(),
                package_path: dir.join("demo_skill"),
                pyproject_path: dir.join("pyproject.toml"),
            }],
            ..Default::default()
        },
    );
    let manager = provisioner.ensure(None, None).await.unwrap();
    (provisioner, manager)
}

#[tokio::test]
async fn a_skill_is_callable_by_its_submodule_name() {
    let Some(python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let (provisioner, manager) = boot(python, dir.path()).await;
    // #2221: the same-named submodule no longer shadows the callable.
    assert_eq!(
        run_cell(
            &manager,
            "(await demo_skill(1), await demo_skill.demo_skill(2), await demo_skill.run(3))"
        )
        .await,
        (ExecuteStatus::Ok, "(2, 3, 4)".to_string())
    );
    provisioner.dispose(None).await;
}

#[tokio::test]
async fn a_variable_holding_a_skill_survives_a_snapshot_round_trip() {
    let Some(python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let (provisioner, manager) = boot(python, dir.path()).await;
    // #1278: the wrapper pickles by reference, so the variable is saved and restored.
    // The snapshot files go under the test's own dir (a bare `mkdtemp()` outlived the test).
    let fixture_root = format!("_fixture_root = {:?}\n", dir.path().to_string_lossy());
    let snapshot_round_trip = "import os, tempfile\n\
from rlm import repl as _repl\n\
_dir = tempfile.mkdtemp(dir=_fixture_root)\n\
_saved = _repl._snapshot_state({'tools': {'d': demo_skill}}, os.path.join(_dir, 's.dill'), os.path.join(_dir, 's.json'), _repl.DEFAULT_SNAPSHOT_MAX_BYTES, _repl.DEFAULT_SNAPSHOT_MAX_VARIABLE_BYTES, False)\n\
_ns = {}\n\
_back = _repl._restore_state(_ns, os.path.join(_dir, 's.dill'))\n\
(_saved['saved'], _saved['skipped'], _back['restored'], await _ns['tools']['d'].run(20))";
    assert_eq!(
        run_cell(&manager, &(fixture_root + snapshot_round_trip)).await,
        (
            ExecuteStatus::Ok,
            "(['tools'], [], ['tools'], 21)".to_string()
        )
    );
    provisioner.dispose(None).await;
}
