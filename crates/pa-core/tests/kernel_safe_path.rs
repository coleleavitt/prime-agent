// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration test (upstream #2169): the kernel runs with the project directory as its
//! cwd, which the interpreter would put at `sys.path[0]` for `python -m rlm.repl`. A checkout carrying an
//! `rlm/` package or a `dill.py` would then shadow the runtime's own imports and run repository
//! code inside the kernel (the snapshot `dill` is the pickle engine). The kernel launches with
//! `-P`; project modules stay importable from cells (TS v0.9.8 behaviour) at the lowest
//! priority, after the standard library and site-packages. The kernel Python is ambient product
//! state; skipped when absent.

use std::path::PathBuf;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

#[tokio::test]
async fn project_modules_cannot_shadow_the_kernel_runtime() {
    let Some(python) = kernel_python() else {
        return;
    };
    let project = tempfile::TempDir::new().unwrap();
    let marker = project.path().join("shadow-ran");
    let shadow = format!(
        "open({:?}, 'a').write(__name__ + '\\n')\n",
        marker.display().to_string()
    );
    std::fs::create_dir(project.path().join("rlm")).unwrap();
    std::fs::write(project.path().join("rlm/__init__.py"), &shadow).unwrap();
    std::fs::write(project.path().join("rlm/repl.py"), &shadow).unwrap();
    std::fs::write(project.path().join("dill.py"), &shadow).unwrap();
    std::fs::write(project.path().join("repo_local_module.py"), "VALUE = 42\n").unwrap();

    let provisioner = IpythonKernelProvisioner::new(
        project.path(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            ..Default::default()
        },
    );
    let manager = provisioner.ensure(None, None).await.unwrap();
    let result = manager
        .execute(
            "import os, sys, dill, rlm, repo_local_module\n\
             _project = os.path.realpath(os.getcwd())\n\
             (sys.flags.safe_path, \
             os.path.realpath(dill.__file__).startswith(_project), \
             os.path.realpath(rlm.__file__).startswith(_project), \
             repo_local_module.VALUE)",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    provisioner.dispose(None).await;
    assert_eq!(
        (
            result.status,
            result.result.as_deref(),
            std::fs::read_to_string(&marker).ok()
        ),
        (ExecuteStatus::Ok, Some("(True, False, False, 42)"), None)
    );
}
