//! The factory's kernel host requests: the thin `rlm.factory` client's
//! surface over this session's executor.
//!
//! - `factory.spec` serves the validator functions (and every
//!   `rlm.harness` factory write): one spec operation per request.
//! - `factory.run` / `.status` / `.stop` / `.resume` / `.graph` / `.watch`
//!   drive the executor; `factory.machine` serves `export_machine`'s run
//!   lookup.
//!
//! Executor replies are `{"result": ...}`, or `{"error": <sentence>}` for a
//! refusal the kernel raises as `ValueError` (unknown runs, invalid specs,
//! a resume of a run that is not paused); a malformed request is a host
//! error (`RuntimeError`). The executor requests ride the same opt-in gate
//! as the kernel client: while `factory.enabled` is off they refuse with
//! the one disabled sentence.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use super::executor::model::LibraryOrigin;
use super::executor::{FactoryExecutor, FactoryRefusal, ResolvedSubagent, RunRequest};
use super::lane::{factory_enabled_in, FACTORY_DISABLED_MESSAGE};
use super::pyvalue::{decode_node_table, PyValue};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Register `factory.spec`, which every session serves (validation needs
/// no executor).
pub fn register_factory_spec_handler(handlers: &mut HostRequestHandlers) {
    handlers.register(
        "factory.spec",
        host_handler(|payload| async move { super::spec_ops::run_spec_op(&payload.data) }),
    );
}

/// Register the executor requests over one session's executor; `agent_dir`
/// holds the `factory.enabled` setting the gate reads.
pub fn register_factory_executor_handlers(
    handlers: &mut HostRequestHandlers,
    executor: &Arc<FactoryExecutor>,
    agent_dir: &Path,
) {
    for request_type in [
        "factory.run",
        "factory.status",
        "factory.stop",
        "factory.resume",
        "factory.graph",
        "factory.watch",
        "factory.machine",
    ] {
        let executor = Arc::clone(executor);
        let agent_dir = agent_dir.to_path_buf();
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let executor = Arc::clone(&executor);
                let agent_dir = agent_dir.clone();
                async move {
                    let reply = handle(&executor, &agent_dir, request_type, &payload.data).await?;
                    Ok(match reply {
                        Ok(result) => json!({ "result": result }),
                        Err(FactoryRefusal(error)) => json!({ "error": error }),
                    })
                }
            }),
        );
    }
}

fn string_field<'a>(data: &'a Value, key: &str, request_type: &str) -> anyhow::Result<&'a str> {
    data.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{request_type} {key} must be a string"))
}

/// The run request a `factory.run` payload carries: `spec_id`, `name`, and
/// a node table whose root is `{"spec": ..., "subagents": {ref: null |
/// {"content", "model", "thinking"}}}`, plus the library origin of a
/// template run (`machine`, `machine_path`).
fn run_request(data: &Value) -> anyhow::Result<RunRequest> {
    let spec_id = string_field(data, "spec_id", "factory.run")?.to_string();
    let name = data.get("name").and_then(Value::as_str).map(str::to_string);
    let root = decode_node_table(
        data.get("value")
            .ok_or_else(|| anyhow::anyhow!("factory.run value is required"))?,
    )?;
    let mut subagents = HashMap::new();
    if let PyValue::Dict(pairs) = root.get("subagents") {
        for (reference, entry) in pairs {
            let Some(reference) = reference.as_str() else {
                continue;
            };
            let resolved = entry.is_dict().then(|| ResolvedSubagent {
                content: entry.get("content").clone(),
                model: entry.get("model").clone(),
                thinking: entry.get("thinking").clone(),
            });
            subagents.insert(reference.to_string(), resolved);
        }
    }
    let library = data
        .get("machine")
        .and_then(Value::as_str)
        .map(|machine| LibraryOrigin {
            name: machine.to_string(),
            path: data
                .get("machine_path")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    Ok(RunRequest {
        spec_id,
        name,
        spec: root.get("spec").clone(),
        subagents,
        library,
    })
}

async fn handle(
    executor: &FactoryExecutor,
    agent_dir: &std::path::Path,
    request_type: &str,
    data: &Value,
) -> anyhow::Result<Result<Value, FactoryRefusal>> {
    if request_type == "factory.machine" {
        let run_id = string_field(data, "run_id", request_type)?;
        return Ok(Ok(executor.run_machine(run_id).unwrap_or(Value::Null)));
    }
    if !factory_enabled_in(agent_dir) {
        return Ok(Err(FactoryRefusal(FACTORY_DISABLED_MESSAGE.to_string())));
    }
    Ok(match request_type {
        "factory.run" => executor.run(run_request(data)?).await,
        "factory.status" => executor.status(string_field(data, "run_id", request_type)?),
        "factory.stop" => {
            executor
                .stop(string_field(data, "run_id", request_type)?)
                .await
        }
        "factory.resume" => {
            executor
                .resume(string_field(data, "run_id", request_type)?)
                .await
        }
        "factory.watch" => {
            let run_id = string_field(data, "run_id", request_type)?;
            let timeout = data.get("timeout").and_then(Value::as_f64);
            executor.watch(run_id, timeout, compact_view(data)).await
        }
        _ => graph(executor, data),
    })
}

/// The `compact` flag a graph or watch request asks for (the kernel API's
/// default is the full view).
fn compact_view(data: &Value) -> bool {
    data.get("compact")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `factory.graph`: no `ref` lists every reportable run; a `ref` naming a
/// live run returns its snapshot; otherwise the stored spec the client
/// resolved (`spec_id` + a `spec` node table) answers with its static
/// structure.
fn graph(executor: &FactoryExecutor, data: &Value) -> Result<Value, FactoryRefusal> {
    let reference = data.get("ref").and_then(Value::as_str);
    if let Some(snapshot) = executor.graph(reference, compact_view(data)) {
        return Ok(snapshot);
    }
    let reference = reference.unwrap_or_default();
    let stored = data
        .get("spec")
        .and_then(|table| decode_node_table(table).ok())
        .zip(data.get("spec_id").and_then(Value::as_str));
    let Some((spec, spec_id)) = stored else {
        return Err(FactoryRefusal(format!(
            "unknown factory run or spec {}",
            super::pyvalue::py_str_repr(reference)
        )));
    };
    executor.spec_graph(reference, spec_id, &spec)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::factory::executor::tests::fake::Case;
    use crate::factory::pyvalue::encode_node_table;
    use crate::kernel::shared::HostRequestPayload;

    struct Lane {
        case: Case,
        handlers: HostRequestHandlers,
        agent_dir: tempfile::TempDir,
    }

    impl Lane {
        fn new() -> Self {
            let case = Case::new();
            let agent_dir = tempfile::TempDir::new().expect("agent dir");
            let mut handlers = HostRequestHandlers::new();
            register_factory_spec_handler(&mut handlers);
            register_factory_executor_handlers(&mut handlers, &case.executor, agent_dir.path());
            let lane = Self {
                case,
                handlers,
                agent_dir,
            };
            lane.enable(true);
            lane
        }

        fn enable(&self, enabled: bool) {
            std::fs::write(
                self.agent_dir.path().join("settings.json"),
                json!({ "factory": { "enabled": enabled } }).to_string(),
            )
            .expect("settings");
        }

        async fn call(&self, request_type: &str, data: Value) -> Value {
            let handler = self.handlers.get(request_type).expect("registered");
            handler(HostRequestPayload {
                data,
                cell_source_code: None,
            })
            .await
            .expect("handled")
        }
    }

    fn run_payload(spec: &Value, subagents: &Value) -> Value {
        let root = PyValue::Dict(vec![
            (PyValue::Str("spec".into()), PyValue::from_json(spec)),
            (
                PyValue::Str("subagents".into()),
                PyValue::from_json(subagents),
            ),
        ]);
        json!({ "spec_id": "sw", "name": "the run", "value": encode_node_table(&root) })
    }

    #[tokio::test]
    async fn the_kernel_lane_runs_reports_and_refuses_through_one_envelope() {
        let lane = Lane::new();
        let spec = json!({ "nodes": [{ "id": "a", "subagent": "worker" }] });
        let subagents =
            json!({ "worker": { "content": "Do it.", "model": null, "thinking": null } });
        let started = lane
            .call("factory.run", run_payload(&spec, &subagents))
            .await;
        let run_id = started["result"]["run_id"]
            .as_str()
            .expect("run id")
            .to_string();
        assert_eq!(started["result"]["spec_id"], "sw");
        assert_eq!(started["result"]["name"], "the run");
        assert_eq!(lane.case.host.spawn_prompts("a"), ["Do it.".to_string()]);
        let status = lane
            .call("factory.status", json!({ "run_id": run_id }))
            .await;
        assert_eq!(status["result"]["run_id"], run_id.as_str());
        // A refusal rides the envelope (the kernel raises it as ValueError).
        assert_eq!(
            lane.call("factory.status", json!({ "run_id": "nope" }))
                .await,
            json!({ "error": "unknown factory run 'nope'" })
        );
        let missing = run_payload(&spec, &json!({ "worker": null }));
        assert_eq!(
            lane.call("factory.run", missing).await,
            json!({ "error": "state 'a' references unknown subagent 'worker'" })
        );
        // The export view of a live run, and of none.
        let view = lane
            .call("factory.machine", json!({ "run_id": run_id }))
            .await;
        assert_eq!(view["result"]["spec_id"], "sw");
        assert_eq!(view["result"]["machine"]["states"][0]["id"], "a");
        assert_eq!(
            lane.call("factory.machine", json!({ "run_id": "nope" }))
                .await,
            json!({ "result": null })
        );
    }

    #[tokio::test]
    async fn the_kernel_lane_rides_the_opt_in_gate() {
        let lane = Lane::new();
        lane.enable(false);
        for (request_type, data) in [
            ("factory.status", json!({ "run_id": "r" })),
            ("factory.graph", json!({})),
            ("factory.watch", json!({ "run_id": "r", "timeout": 0.0 })),
        ] {
            assert_eq!(
                lane.call(request_type, data).await,
                json!({ "error": FACTORY_DISABLED_MESSAGE }),
                "{request_type}"
            );
        }
        // Validation and the export view stay ungated (harness writes gate
        // themselves first; deletes and exports are not gated).
        let reply = lane
            .call(
                "factory.spec",
                json!({ "op": "validate_spec", "value": encode_node_table(&PyValue::from_json(&json!({ "nodes": [] }))) }),
            )
            .await;
        assert_eq!(
            reply,
            json!({ "errors": ["factory dag must declare between 1 and 1024 nodes, got 0"] })
        );
        assert_eq!(
            lane.call("factory.machine", json!({ "run_id": "r" })).await,
            json!({ "result": null })
        );
    }

    #[tokio::test]
    async fn a_spec_graph_needs_the_resolved_entry() {
        let lane = Lane::new();
        let spec = encode_node_table(&PyValue::from_json(
            &json!({ "nodes": [{ "id": "a", "subagent": "w" }] }),
        ));
        let graph = lane
            .call(
                "factory.graph",
                json!({ "ref": "sw", "spec_id": "sw", "spec": spec }),
            )
            .await;
        assert_eq!(graph["result"]["spec_id"], "sw");
        assert_eq!(graph["result"]["machine"]["order"], json!(["a"]));
        assert_eq!(
            lane.call("factory.graph", json!({ "ref": "missing" }))
                .await,
            json!({ "error": "unknown factory run or spec 'missing'" })
        );
        assert_eq!(
            lane.call("factory.graph", json!({})).await,
            json!({ "result": { "runs": [] } })
        );
    }
}
