//! `artifact.present`: validation, capture, the display-only row, and the
//! model-context exclusion.
use super::*;
use crate::kernel::shared::HostRequestPayload;

/// The smallest valid PNG prefix the type sniffing accepts.
fn png_bytes(tag: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend([
        0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 4, 0, 0, 0, 2, tag,
    ]);
    bytes
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn an_image_presents_its_preview_in_a_display_only_row() {
    let dir = tempfile::TempDir::new().unwrap();
    let source = dir.path().join("render one.png");
    std::fs::write(&source, png_bytes(1)).unwrap();
    let request = parse_present_request(&json!({
        "path": "render one.png",
        "label": "  Direction A  ",
        "preview": {
            "data": b64(&png_bytes(2)), "mime_type": "image/png",
            "width": 4, "height": 2, "original_width": 4, "original_height": 2
        }
    }))
    .unwrap();
    let artifacts = dir.path().join("session");
    let captured =
        capture_presented_artifact(&request, dir.path(), &artifacts, "sess-1", "pres-1", 7)
            .unwrap();
    let digest = Sha256::digest(png_bytes(1));
    let artifact_id = hex_prefix(&digest, 8);
    let captured_path = artifacts
        .join("presented-artifacts")
        .join(format!("{artifact_id}-render-one.png"));
    let path_text = captured_path.to_string_lossy().to_string();
    assert_eq!(std::fs::read(&captured_path).unwrap(), png_bytes(1));
    assert_eq!(
        serde_json::to_value(&captured.message).unwrap(),
        json!({
            "customType": "prime-agent.presented-artifact",
            "content": [
                { "type": "text", "text": "Direction A" },
                { "type": "image", "data": b64(&png_bytes(2)), "mimeType": "image/png" }
            ],
            "display": true,
            "details": {
                "artifactId": artifact_id, "presentationId": "pres-1", "sessionId": "sess-1",
                "name": "render one.png", "label": "Direction A", "kind": "image",
                "mimeType": "image/png", "byteSize": png_bytes(1).len(), "path": path_text,
                "width": 4, "height": 2, "originalWidth": 4, "originalHeight": 2
            },
            "timestamp": 7
        })
    );
    assert_eq!(
        captured.receipt,
        json!({
            "artifactId": artifact_id, "presentationId": "pres-1", "kind": "image",
            "name": "render one.png", "mimeType": "image/png", "byteSize": png_bytes(1).len(),
            "path": path_text, "width": 4, "height": 2, "originalWidth": 4, "originalHeight": 2
        })
    );
    // The preview is the user's: the model's request never carries the row.
    assert!(crate::session_engine::messages::convert_to_llm(&[
        pa_types::session::AgentMessage::Custom(captured.message)
    ])
    .is_empty());
    // The source can go away: the capture is the durable copy.
    std::fs::remove_file(&source).unwrap();
    assert!(captured_path.is_file());
}

#[test]
fn a_generic_file_presents_its_captured_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let source = dir.path().join("report.csv");
    std::fs::write(&source, "a,b\n1,2\n").unwrap();
    let request = parse_present_request(&json!({ "path": source.to_string_lossy() })).unwrap();
    let captured =
        capture_presented_artifact(&request, dir.path(), dir.path(), "s", "p", 1).unwrap();
    let path = captured.receipt["path"].as_str().unwrap().to_string();
    assert_eq!(
        serde_json::to_value(&captured.message.content).unwrap(),
        json!([
            { "type": "text", "text": "Artifact: report.csv" },
            { "type": "text", "text": path }
        ])
    );
    assert_eq!(captured.receipt["kind"], "file");
    assert_eq!(captured.receipt["mimeType"], "text/plain");
}

#[test]
fn invalid_requests_refuse_with_the_reason() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    std::fs::write(dir.path().join("pic.png"), png_bytes(1)).unwrap();
    std::fs::create_dir(dir.path().join("folder")).unwrap();
    let refusal = |payload: Value| -> String {
        match parse_present_request(&payload).and_then(|request| {
            capture_presented_artifact(&request, dir.path(), dir.path(), "s", "p", 1)
        }) {
            Ok(captured) => panic!("accepted: {captured:?}"),
            Err(error) => format!("{error:#}"),
        }
    };
    assert_eq!(
        refusal(json!({})),
        "artifact.present requires a non-empty path"
    );
    assert_eq!(
        refusal(json!({ "path": "notes.txt", "label": 5 })),
        "artifact.present label must be a string"
    );
    assert_eq!(
        refusal(json!({ "path": "notes.txt", "label": "x".repeat(501) })),
        "artifact.present label must be at most 500 characters"
    );
    assert_eq!(
        refusal(json!({ "path": "missing.png" })),
        "Artifact does not exist: missing.png"
    );
    assert_eq!(
        refusal(json!({ "path": "folder" })),
        "Artifact path must be a regular file: folder"
    );
    assert_eq!(
        refusal(
            json!({ "path": "notes.txt", "preview": { "data": b64(&png_bytes(2)), "mime_type": "image/png" } })
        ),
        "artifact.present preview is only accepted for a raster image artifact: notes.txt"
    );
    assert_eq!(
        refusal(
            json!({ "path": "pic.png", "preview": { "data": b64(b"not an image"), "mime_type": "image/png" } })
        ),
        "artifact.present preview is not a image/png image (PNG, JPEG, GIF, or WebP)"
    );
    assert_eq!(
        refusal(json!({ "path": "pic.png", "preview": { "data": "x".repeat(MAX_PREVIEW_BASE64_CHARS + 1), "mime_type": "image/png" } })),
        format!(
            "artifact.present preview is {} base64 characters; previews must be at most {MAX_PREVIEW_BASE64_CHARS}",
            MAX_PREVIEW_BASE64_CHARS + 1
        )
    );
    assert_eq!(
        refusal(
            json!({ "path": "pic.png", "preview": { "data": b64(&png_bytes(2)), "mime_type": "image/png", "width": 4000 } })
        ),
        "artifact.present preview width is out of range"
    );
}

async fn presented_rows(session: &Mutex<SessionManager>) -> Vec<String> {
    session
        .lock()
        .await
        .get_all_entries()
        .iter()
        .filter_map(|entry| match entry {
            pa_types::session::FileEntry::CustomMessage { payload, .. } => {
                Some(payload.custom_type.clone())
            }
            _ => None,
        })
        .collect()
}

/// Through the host request: the row lands in the engine's session (or the
/// installed sink), and the kernel gets the receipt back.
#[tokio::test]
async fn the_host_request_records_the_row_and_answers_the_receipt() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("pic.png"), png_bytes(1)).unwrap();
    let session = Arc::new(Mutex::new(SessionManager::in_memory(dir.path())));
    let presented = Arc::new(PresentedArtifacts::new());
    let counters = Arc::new(crate::session_engine::telemetry::SessionCounters::default());
    let mut handlers = HostRequestHandlers::default();
    register_artifact_present_handler(
        &mut handlers,
        &presented,
        PresentContext {
            cwd: dir.path().to_path_buf(),
            artifact_dir: Some(dir.path().join("artifacts")),
            session_id: "sess".to_string(),
            session: Arc::clone(&session),
            counters: Some(Arc::clone(&counters)),
        },
    );
    let handler = handlers
        .get("artifact.present")
        .expect("registered")
        .clone();
    let payload = |data: Value| HostRequestPayload {
        data,
        cell_source_code: None,
    };
    let receipt = handler(payload(json!({ "path": "pic.png" })))
        .await
        .unwrap();
    assert_eq!(
        receipt["kind"], "file",
        "no preview: the image presents as a file"
    );
    let rows = presented_rows(&session).await;
    assert_eq!(rows, vec![PRESENTED_ARTIFACT_CUSTOM_TYPE.to_string()]);
    // Adoption: each shown artifact counts once (never what it showed).
    assert_eq!(
        counters
            .adoption_count(crate::session_engine::telemetry::SessionAdoption::ArtifactPresented),
        1
    );

    // An installed sink owns the row instead.
    let seen: Arc<std::sync::Mutex<Vec<CustomMessage>>> = Arc::default();
    let sink_seen = Arc::clone(&seen);
    presented.set_sink(Arc::new(move |message| {
        sink_seen.lock().unwrap().push(message);
        Ok(())
    }));
    let receipt = handler(payload(json!({
        "path": "pic.png",
        "preview": { "data": b64(&png_bytes(2)), "mime_type": "image/png" }
    })))
    .await
    .unwrap();
    assert_eq!(receipt["kind"], "image");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        presented_rows(&session).await,
        vec![PRESENTED_ARTIFACT_CUSTOM_TYPE.to_string()],
        "the sink's row never doubles into the engine session"
    );
}
