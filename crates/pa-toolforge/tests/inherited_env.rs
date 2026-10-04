//! The exit test runs in the user's environment: variables and the user's
//! own `PYTHONPATH` reach it, behind the package root. Its own binary: the
//! test sets process environment variables.

mod common;

use std::sync::{Arc, Mutex};

use common::{gate_python, options, slugify_request};
use pa_toolforge::{publish, GatePhase, PublishRequest, PublishStatus};

#[tokio::test]
async fn the_exit_test_inherits_variables_and_the_users_python_path() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let user_root = dir.path().join("user-path");
    std::fs::create_dir_all(&user_root).unwrap();
    std::fs::write(
        user_root.join("prime_agent_user_only_helper.py"),
        "EXPECTED = \"a-b\"\n",
    )
    .unwrap();
    std::env::set_var("PRIME_AGENT_TOOLFORGE_GATE_PROBE", "inherited");
    std::env::set_var("PYTHONPATH", &user_root);
    let installs = Arc::new(Mutex::new(Vec::new()));
    let options = options(&dir.path().join("agent"), &python, &installs);

    let result = publish(
        &PublishRequest {
            exit_test: [
                "import os",
                "import prime_agent_user_only_helper",
                "import slugify",
                "",
                "assert os.environ.get(\"PRIME_AGENT_TOOLFORGE_GATE_PROBE\") == \"inherited\", sorted(os.environ)",
                "assert slugify.run(\"A B\") == prime_agent_user_only_helper.EXPECTED",
                "",
            ]
            .join("\n"),
            ..slugify_request()
        },
        &options,
    )
    .await;
    std::env::remove_var("PRIME_AGENT_TOOLFORGE_GATE_PROBE");
    std::env::remove_var("PYTHONPATH");

    assert_eq!(result.reason, None);
    assert_eq!(result.status, PublishStatus::Published);
    assert_eq!(
        result
            .gate
            .iter()
            .map(|run| (run.phase, run.outcome.as_str(), run.ok))
            .collect::<Vec<_>>(),
        vec![
            (GatePhase::Negative, "raised", true),
            (GatePhase::Positive, "clean", true),
        ]
    );
}
