//! The example plugin (`examples/plugins/command-guard`, a Rust component
//! built for `wasm32-wasip2`) in a real session: it loads against this
//! host, declares its tool and command, refuses `rm -rf /`, asks about
//! `git push`, answers its `check` tool and its `/wasm guard stats`
//! command. Building it needs the `wasm32-wasip2` target, so the test runs
//! only when `PA_PLUGINS_EXAMPLE_WASM` names the built file
//! (`examples/plugins/command-guard/check.sh` builds it and runs this).
//! Its own test binary: installation is process-global.

// Pedantic-gate dispositions as pa-core's tests (large_futures).
#![allow(clippy::large_futures)]

use std::sync::Arc;

use pa_core::features::{HostRequestObservation, SessionFeature, SessionFeatureContext};
use serde_json::json;

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 100_000,
        max_tokens: 100,
        max_tokens_explicit: false,
    }
}

#[tokio::test]
async fn the_example_guard_plugin_reviews_and_answers() {
    let Some(example) = std::env::var_os("PA_PLUGINS_EXAMPLE_WASM") else {
        eprintln!("PA_PLUGINS_EXAMPLE_WASM is unset: skipping the example plugin");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("plugins")).unwrap();
    std::fs::copy(&example, agent_dir.join("plugins").join("guard.wasm")).unwrap();
    let feature = pa_plugins::PluginsFeature::new();
    let context = Arc::new(SessionFeatureContext {
        agent_dir,
        cwd: tmp.path().to_path_buf(),
        session_id: "example-session".to_string(),
        python_skill_import_names: Vec::new(),
        model: model(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    });
    let run = |command: &str| HostRequestObservation {
        request_type: "bash.run".to_string(),
        data: json!({ "type": "bash.run", "command": command }),
    };
    assert_eq!(
        feature
            .review_host_request(&context, &run("rm -rf /"))
            .await,
        pa_core::features::FeatureVerdict::Block(
            "plugin guard: removing / or the home directory is never allowed".to_string()
        )
    );
    assert_eq!(
        feature
            .review_host_request(&context, &run("git push origin main"))
            .await,
        pa_core::features::FeatureVerdict::Ask(
            "plugin guard: pushing publishes commits".to_string()
        )
    );
    assert_eq!(
        feature.review_host_request(&context, &run("ls")).await,
        pa_core::features::FeatureVerdict::Allow
    );
    // A finished command is counted.
    let finished = json!({
        "status": "ok", "job": { "id": "j1" }, "done": true,
        "events": [{ "type": "finished", "exitCode": 1, "output": "" }]
    });
    assert_eq!(
        feature
            .after_host_request(&context, &run("false"), Ok(&finished))
            .await,
        None
    );
    let mut handlers = pa_core::kernel::shared::HostRequestHandlers::default();
    feature.register_host_handlers(&context, &mut handlers);
    let call = |data: serde_json::Value| {
        handlers.get(pa_plugins::CALL_REQUEST).unwrap()(
            pa_core::kernel::shared::HostRequestPayload { data },
        )
    };
    assert_eq!(
        call(json!({ "plugin": "guard", "method": "check", "data": { "command": "sudo ls" } }))
            .await
            .unwrap(),
        json!({ "verdict": "ask", "reason": "sudo runs as root" })
    );
    let listed = handlers.get(pa_plugins::LIST_REQUEST).unwrap()(
        pa_core::kernel::shared::HostRequestPayload { data: json!({}) },
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["tools"][0]["signature"], "check(command: string)");
    assert_eq!(listed[0]["commands"][0]["name"], "stats");
    let stats = feature
        .execute_slash_command(&context, pa_plugins::COMMAND, "guard stats")
        .expect("the plugins feature owns /wasm")
        .await
        .unwrap();
    assert_eq!(
        stats.text,
        "1 commands finished (1 failed); 1 asked about, 1 blocked"
    );
}
