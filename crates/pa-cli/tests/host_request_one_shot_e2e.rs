//! `prime-agent --prime-agent-harness-request` end to end: the one-shot a
//! Python process outside a kernel sends `rlm.harness` and `rlm.factory`
//! requests to answers every session-free host request exactly as the
//! kernel handlers do — the harness store, the factory spec validator, and
//! the machine library.

use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Run one request through the one-shot: (exit code, stdout reply, stderr).
fn one_shot(request: &Value) -> (Option<i32>, Option<Value>, String) {
    let state = tempfile::tempdir().expect("state dir");
    let mut child = pa_types::platform::test_isolation::TestState::new(state.path())
        .apply(&mut Command::new(env!("CARGO_BIN_EXE_prime-agent")))
        .arg("--prime-agent-harness-request")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn prime-agent");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write request");
    let output = child.wait_with_output().expect("one-shot exits");
    let stdout = String::from_utf8_lossy(&output.stdout);
    (
        output.status.code(),
        serde_json::from_str(stdout.trim()).ok(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A node table holding one plain-JSON value (the kernel client's
/// encoding of a JSON-shaped Python value).
fn table(value: &Value) -> Value {
    let mut nodes = Vec::new();
    encode(value, &mut nodes);
    json!({"nodes": nodes, "root": 0})
}

fn encode(value: &Value, nodes: &mut Vec<Value>) {
    let slot = nodes.len();
    nodes.push(Value::Null);
    let node = match value {
        Value::Null => json!(["n"]),
        Value::Bool(flag) => json!(["b", flag]),
        Value::Number(number) if number.is_f64() => json!(["f", number]),
        Value::Number(number) => json!(["i", number.to_string()]),
        Value::String(text) => json!(["s", text]),
        Value::Array(items) => {
            let mut children = Vec::new();
            for item in items {
                children.push(nodes.len());
                encode(item, nodes);
            }
            json!(["l", children])
        }
        Value::Object(map) => {
            let mut pairs = Vec::new();
            for (key, item) in map {
                let key_slot = nodes.len();
                encode(&Value::String(key.clone()), nodes);
                let value_slot = nodes.len();
                encode(item, nodes);
                pairs.push(json!([key_slot, value_slot]));
            }
            json!(["d", pairs])
        }
    };
    nodes[slot] = node;
}

#[test]
fn the_one_shot_serves_the_factory_spec_validator() {
    let (code, reply, stderr) = one_shot(&json!({
        "type": "factory.spec",
        "op": "validate_spec",
        "value": table(&json!({"nodes": []})),
    }));
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        reply,
        Some(json!({"errors": ["factory dag must declare between 1 and 1024 nodes, got 0"]}))
    );
}

#[test]
fn the_one_shot_serves_the_machine_library() {
    let dir = tempfile::tempdir().expect("temp dir");
    let machine = dir.path().join("repo").join("sweep");
    std::fs::create_dir_all(&machine).expect("machine dir");
    std::fs::write(
        machine.join("MACHINE.md"),
        "---\nname: sweep\ndescription: Sweeps.\nversion: 1\nauthor: Tester\n---\n\n\
         ```machine-spec\n{\"states\": [{\"id\": \"a\", \"entry\": true, \"subagent\": {\"prompt\": \"P.\"}}]}\n```\n",
    )
    .expect("machine file");
    let repo = dir.path().join("repo").display().to_string();
    let user = dir.path().join("user").display().to_string();
    let (code, reply, stderr) = one_shot(&json!({
        "type": "factory.library",
        "op": "scan",
        "dirs": [["repo", repo], ["user", user]],
    }));
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        reply,
        Some(json!({"ok": true, "result": {
            "machines": [{
                "name": "sweep",
                "description": "Sweeps.",
                "version": "1",
                "author": "Tester",
                "source": "repo",
                "path": machine.join("MACHINE.md").display().to_string(),
            }],
            "warnings": [],
        }}))
    );
}

#[test]
fn a_malformed_factory_request_fails_with_its_reason() {
    let (code, reply, stderr) = one_shot(&json!({"type": "factory.library", "op": "nope"}));
    assert_eq!(code, Some(1));
    assert_eq!(reply, None);
    assert!(
        stderr.contains("unknown factory.library op \"nope\""),
        "{stderr}"
    );
}

#[test]
fn harness_requests_still_reach_the_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("harness_state.json").display().to_string();
    let (code, reply, stderr) = one_shot(&json!({
        "type": "harness.list",
        "store": {"file": file, "scope": "local", "document": null, "writeError": null},
        "args": {"kind": null},
        "types": {"kind": "NoneType"},
    }));
    assert_eq!(code, Some(0), "{stderr}");
    let reply = reply.expect("reply");
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["result"], json!([]));
}
