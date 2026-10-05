use super::*;
use crate::agent_engine::image_delegation::tests::{
    delegating_engine_with_socket, image_content, register_text_only_battery_model,
    ScriptedSupervisor,
};
use crate::agent_engine::tests::FAUX_TEST_LOCK;
use pa_core::kernel::shared::HostRequestPayload;

fn png(data: &str) -> Value {
    json!({ "data": data, "mime_type": "image/png" })
}

/// Call one registered handler on a fresh runtime (the kernel bridge's
/// shape: the handler future runs on an async worker).
fn call(handlers: &HostRequestHandlers, request_type: &str, data: Value) -> anyhow::Result<Value> {
    let handler = handlers
        .get(request_type)
        .unwrap_or_else(|| panic!("{request_type} is not registered"))
        .clone();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(handler(HostRequestPayload {
            data,
            cell_source_code: None,
        }))
}

/// The engine of a daemon-backed text-only session with its self-arc
/// registered and its `vision.read` handler installed.
fn vision_read_handlers(
    dir: &std::path::Path,
    socket: &std::path::Path,
) -> (std::sync::Arc<AgentSessionEngine>, HostRequestHandlers) {
    let engine = std::sync::Arc::new(delegating_engine_with_socket(dir, socket));
    engine.register_arc();
    let mut handlers = HostRequestHandlers::default();
    engine.register_vision_read_host_handler(&mut handlers);
    (engine, handlers)
}

#[test]
fn a_payload_parses_into_its_images_and_question() {
    assert_eq!(
        parse_vision_read(&json!({
            "images": [png("QUJD"), { "data": "QQ==", "mime_type": "image/webp" }],
            "question": "what does the chart say?",
        }))
        .unwrap(),
        VisionReadRequest {
            images: vec![
                image_content("QUJD"),
                pa_agent::types::ImageContent {
                    data: "QQ==".to_string(),
                    mime_type: "image/webp".to_string(),
                },
            ],
            question: "what does the chart say?".to_string(),
        }
    );
    // No (or a blank) question asks the default one; a long one is capped.
    let defaulted = parse_vision_read(&json!({ "images": [png("QUJD")], "question": " " }));
    assert_eq!(defaulted.unwrap().question, DEFAULT_QUESTION);
    let long = parse_vision_read(&json!({ "images": [png("QUJD")], "question": "x".repeat(5000) }));
    assert_eq!(long.unwrap().question, "x".repeat(MAX_QUESTION_CHARS));
}

#[test]
fn a_payload_outside_the_bounds_is_refused_whole() {
    let refusal = |data: Value| format!("{:#}", parse_vision_read(&data).unwrap_err());
    let oversized = "A".repeat((MAX_IMAGE_BYTES / 3 + 1) * 4);
    let near_cap = "A".repeat(MAX_IMAGE_BYTES / 3 * 4);
    assert_eq!(
        [
            refusal(json!({})),
            refusal(json!({ "images": [] })),
            refusal(json!({ "images": vec![png("QUJD"); MAX_IMAGES + 1] })),
            refusal(json!({ "images": [{ "data": "QUJD", "mime_type": "application/pdf" }] })),
            refusal(json!({ "images": [png("QUJD"), png("not base64!")] })),
            refusal(json!({ "images": [png(&oversized)] })),
            refusal(json!({ "images": vec![png(&near_cap); 4] })),
            refusal(json!({ "images": [png("QUJD")], "question": 7 })),
        ],
        [
            "vision.read images must be an array of {data, mime_type} objects".to_string(),
            "vision.read needs at least one image".to_string(),
            format!("vision.read carries at most {MAX_IMAGES} images per read, got 9"),
            "vision.read image 0 has unsupported type \"application/pdf\" (PNG, JPEG, GIF, WebP)"
                .to_string(),
            "vision.read image 1 data is not base64".to_string(),
            format!(
                "vision.read image 0 is {} bytes; images must be at most {MAX_IMAGE_BYTES} bytes",
                MAX_IMAGE_BYTES / 3 * 3 + 3
            ),
            format!("vision.read images exceed {MAX_TOTAL_BYTES} bytes together"),
            "vision.read question must be a string when provided".to_string(),
        ]
    );
}

#[test]
fn a_long_reading_is_capped_and_marked() {
    let reading = "é".repeat(MAX_READING_CHARS + 5);
    assert_eq!(
        cap_reading(&reading),
        format!(
            "{}\n[reading truncated at {MAX_READING_CHARS} characters]",
            "é".repeat(MAX_READING_CHARS)
        )
    );
    assert_eq!(cap_reading("short"), "short");
}

/// The whole read over a scripted supervisor: one child on the resolved
/// image model gets the question and the ACTUAL image bytes, and only its
/// text reading comes back (with the model that read it).
#[test]
fn a_text_only_session_reads_images_through_one_image_model_child() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registration = register_text_only_battery_model();
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("scripted-supervisor.sock");
    let supervisor =
        ScriptedSupervisor::spawn(socket.clone(), "a red square on a white background");
    let (engine, handlers) = vision_read_handlers(dir.path(), &socket);
    let reading = call(
        &handlers,
        "vision.read",
        json!({ "images": [png("QUJD")], "question": "what color is the square?" }),
    )
    .unwrap();
    assert_eq!(
        reading,
        json!({
            "text": "a red square on a white background",
            "model": "battery/mock-vision",
        })
    );
    let captured = supervisor.captured.lock().unwrap().clone();
    let commands: Vec<&str> = captured
        .iter()
        .filter_map(|command| command["type"].as_str())
        .filter(|kind| matches!(*kind, "create" | "prompt"))
        .collect();
    assert_eq!(
        commands,
        ["create", "prompt"],
        "exactly one child, one task"
    );
    let create = captured.iter().find(|command| command["type"] == "create");
    let create = create.expect("create reached the supervisor");
    assert_eq!(
        [&create["config"]["provider"], &create["config"]["model"]],
        [&json!("battery"), &json!("mock-vision")]
    );
    let prompt = captured.iter().find(|command| command["type"] == "prompt");
    let prompt = prompt.expect("prompt reached the supervisor");
    assert_eq!(
        prompt["message"],
        json!(vision_read_child_prompt("what color is the square?"))
    );
    assert_eq!(
        prompt["images"],
        json!([{ "type": "image", "data": "QUJD", "mimeType": "image/png" }])
    );
    assert!(!engine.children.as_ref().unwrap().has_running_children());
    registration.unregister();
}

/// No usable image model: the read is refused before any child spawns,
/// with the setting named.
#[test]
fn a_read_without_an_image_model_is_refused_before_any_child() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registration = register_text_only_battery_model();
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("scripted-supervisor.sock");
    let supervisor = ScriptedSupervisor::spawn(socket.clone(), "never asked");
    let (_engine, handlers) = vision_read_handlers(dir.path(), &socket);
    std::fs::write(dir.path().join("agent").join("settings.json"), "{}").unwrap();
    let refusal = format!(
        "{:#}",
        call(&handlers, "vision.read", json!({ "images": [png("QUJD")] })).unwrap_err()
    );
    assert!(
        refusal.contains("does not accept image input")
            && refusal.contains("Set imageModel in settings.json"),
        "{refusal}"
    );
    assert_eq!(*supervisor.captured.lock().unwrap(), Vec::<Value>::new());
    registration.unregister();
}

/// A cancelled read kills and settles its child and reports the cancel.
#[test]
fn a_cancelled_read_settles_its_child() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registration = register_text_only_battery_model();
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("scripted-supervisor.sock");
    let _supervisor = ScriptedSupervisor::spawn(socket.clone(), "an answer nobody reads");
    let engine = delegating_engine_with_socket(dir.path(), &socket);
    let request = parse_vision_read(&json!({ "images": [png("QUJD")] })).unwrap();
    assert_eq!(
        engine.read_images_with_vision_child(request, &|| true),
        Err("the image read was cancelled".to_string())
    );
    assert!(!engine.children.as_ref().unwrap().has_running_children());
    registration.unregister();
}

/// Without supervisor-backed children nothing can host the reader: the
/// request stays unregistered (the skill keeps its vision error).
#[test]
fn a_session_without_children_does_not_register_the_read() {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let engine = std::sync::Arc::new(
        AgentSessionEngine::new(crate::agent_engine::AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap(),
    );
    engine.register_arc();
    let mut handlers = HostRequestHandlers::default();
    engine.register_vision_read_host_handler(&mut handlers);
    assert!(handlers.get("vision.read").is_none());
}
