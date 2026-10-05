// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration test (upstream #2174): the `kernel.environment` policy. The default
//! (`inherit`) keeps today's behaviour, the kernel sees the whole host environment; under
//! `scrub-credentials` the kernel and its `bash()` children do not inherit the model-provider API
//! keys Prime Agent manages (nor do processes it spawns), while everything else (including the generic GitHub tokens tools like
//! `gh` use) stays. Its own test binary: it mutates the process environment. The kernel Python is
//! ambient product state; skipped when absent.

use std::path::PathBuf;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus, KernelEnvironment};

fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = PathBuf::from(format!("{home}/.prime/agent/kernel-venv/bin/python"));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live env-policy test",
        candidate.display()
    );
    None
}

async fn visible_keys(python: &std::path::Path, environment: KernelEnvironment) -> Option<String> {
    let dir = tempfile::TempDir::new().unwrap();
    let provisioner = IpythonKernelProvisioner::new(
        dir.path(),
        IpythonKernelProvisionerOptions {
            python: Some(python.to_path_buf()),
            environment,
            ..Default::default()
        },
    );
    let manager = provisioner.ensure(None, None).await.unwrap();
    let result = manager
        .execute(
            "import os, subprocess\n\
             _child = subprocess.run(['env'], capture_output=True, text=True).stdout\n\
             _keys = ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'PRIME_API_KEY', 'GITHUB_TOKEN', \
             'PA_TEST_UNRELATED']\n\
             ([k for k in _keys if k in os.environ], [k for k in _keys if k + '=' in _child])",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    provisioner.dispose(None).await;
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    result.result
}

#[tokio::test]
async fn scrub_credentials_drops_provider_keys_from_the_kernel_and_its_children() {
    let Some(python) = kernel_python() else {
        return;
    };
    for (key, value) in [
        ("OPENAI_API_KEY", "sk-test-openai"),
        ("ANTHROPIC_API_KEY", "sk-ant-test"),
        ("PRIME_API_KEY", "prime-test"),
        ("GITHUB_TOKEN", "ghp-test"),
        ("PA_TEST_UNRELATED", "kept"),
    ] {
        std::env::set_var(key, value);
    }
    assert_eq!(
        (
            visible_keys(&python, KernelEnvironment::Inherit).await,
            visible_keys(&python, KernelEnvironment::ScrubCredentials).await,
        ),
        (
            Some(
                "(['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'PRIME_API_KEY', 'GITHUB_TOKEN', \
                 'PA_TEST_UNRELATED'], ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'PRIME_API_KEY', \
                 'GITHUB_TOKEN', 'PA_TEST_UNRELATED'])"
                    .to_string()
            ),
            Some(
                "(['GITHUB_TOKEN', 'PA_TEST_UNRELATED'], ['GITHUB_TOKEN', 'PA_TEST_UNRELATED'])"
                    .to_string()
            ),
        )
    );
}
