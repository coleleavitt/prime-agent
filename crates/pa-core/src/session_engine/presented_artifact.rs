//! `artifact.present` (upstream #1062): the kernel's `present_artifact()` shows
//! an on-disk artifact to the USER without attaching it to the model's context.
//! The host validates and captures the file under the session's artifact tree
//! (so the source may be removed afterwards) and records one display-only
//! `prime-agent.presented-artifact` custom row: a label line plus the bounded
//! inline image preview the kernel prepared (raster images), or the captured
//! path (other files). `convert_to_llm` drops the row, so the model never sees
//! the preview; the receipt the kernel gets back is metadata only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use pa_types::ai::{ImageContent, TextContent, UserContent, UserContentBlock};
use pa_types::session::CustomMessage;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::kernel::shared::{HostRequestHandlers, host_handler};
use crate::session::manager::SessionManager;

pub const PRESENTED_ARTIFACT_CUSTOM_TYPE: &str = "prime-agent.presented-artifact";
/// The largest artifact the host captures.
pub const MAX_PRESENTED_ARTIFACT_BYTES: u64 = 20 * 1024 * 1024;
/// The largest inline preview payload (base64 characters).
pub const MAX_PREVIEW_BASE64_CHARS: usize = 350_000;
/// The largest preview edge the kernel may send.
pub const MAX_PREVIEW_DIMENSION: u64 = 1600;
const MAX_LABEL_CHARS: usize = 500;
const CAPTURE_DIR_NAME: &str = "presented-artifacts";

/// Where a host shows (and persists) a presented-artifact row; without one
/// the row appends to the engine's own session.
pub type PresentedArtifactSink = Arc<dyn Fn(CustomMessage) -> anyhow::Result<()> + Send + Sync>;

/// The session's presentation seam: hosts whose durable session lives outside
/// the engine (the daemon worker) install a sink after the session is built.
#[derive(Default)]
pub struct PresentedArtifacts {
    sink: std::sync::RwLock<Option<PresentedArtifactSink>>,
}

impl PresentedArtifacts {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Route every presented row through `sink` (persist + broadcast).
    pub fn set_sink(&self, sink: PresentedArtifactSink) {
        *self
            .sink
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    fn sink(&self) -> Option<PresentedArtifactSink> {
        self.sink
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The kernel-prepared inline preview of a raster artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactPreview {
    pub data: String,
    pub mime_type: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub original_width: Option<u64>,
    pub original_height: Option<u64>,
}

/// One validated `artifact.present` request.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentRequest {
    pub path: String,
    pub label: Option<String>,
    pub preview: Option<ArtifactPreview>,
}

/// Validate an `artifact.present` payload: a non-empty `path`, an optional
/// `label` (trimmed, at most 500 characters), and an optional `preview`
/// (`{data, mime_type, width?, height?, original_width?, original_height?}`:
/// a supported raster type whose bytes match it, within the preview bounds).
///
/// # Errors
///
/// The request's validation message.
pub fn parse_present_request(data: &Value) -> anyhow::Result<PresentRequest> {
    let path = data
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("artifact.present requires a non-empty path"))?
        .to_string();
    let label = match data.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(label)) => {
            let label = label.trim();
            if label.chars().count() > MAX_LABEL_CHARS {
                anyhow::bail!(
                    "artifact.present label must be at most {MAX_LABEL_CHARS} characters"
                );
            }
            (!label.is_empty()).then(|| label.to_string())
        }
        Some(_) => anyhow::bail!("artifact.present label must be a string"),
    };
    let preview = match data.get("preview") {
        None | Some(Value::Null) => None,
        Some(preview) => Some(parse_preview(preview)?),
    };
    Ok(PresentRequest {
        path,
        label,
        preview,
    })
}

fn parse_preview(preview: &Value) -> anyhow::Result<ArtifactPreview> {
    let data = preview
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("artifact.present preview data must be a base64 string"))?;
    let mime_type = preview
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("artifact.present preview mime_type must be a string"))?;
    if data.len() > MAX_PREVIEW_BASE64_CHARS {
        anyhow::bail!(
            "artifact.present preview is {} base64 characters; previews must be at most {MAX_PREVIEW_BASE64_CHARS}",
            data.len()
        );
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| anyhow::anyhow!("artifact.present preview data is not base64"))?;
    if detect_image_mime(&bytes) != Some(mime_type) {
        anyhow::bail!(
            "artifact.present preview is not a {mime_type} image (PNG, JPEG, GIF, or WebP)"
        );
    }
    let dimension = |key: &str, bound: Option<u64>| -> anyhow::Result<Option<u64>> {
        match preview.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|value| *value > 0 && bound.is_none_or(|bound| *value <= bound))
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("artifact.present preview {key} is out of range")),
        }
    };
    Ok(ArtifactPreview {
        data: data.to_string(),
        mime_type: mime_type.to_string(),
        width: dimension("width", Some(MAX_PREVIEW_DIMENSION))?,
        height: dimension("height", Some(MAX_PREVIEW_DIMENSION))?,
        original_width: dimension("original_width", None)?,
        original_height: dimension("original_height", None)?,
    })
}

/// The raster type a byte prefix declares (PNG, JPEG, GIF, WebP).
#[must_use]
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn fallback_mime_type(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "md" | "csv") => "text/plain",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn safe_artifact_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    let mut dash = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            cleaned.push(character);
            dash = false;
        } else if !dash {
            cleaned.push('-');
            dash = true;
        }
    }
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "artifact".to_string()
    } else {
        cleaned.to_string()
    }
}

/// The lowercase hex of the first `bytes` bytes of `digest`.
fn hex_prefix(digest: &[u8], bytes: usize) -> String {
    use std::fmt::Write as _;
    digest
        .iter()
        .take(bytes)
        .fold(String::with_capacity(bytes * 2), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A captured presentation: the display-only row and the kernel's receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedArtifact {
    pub message: CustomMessage,
    pub receipt: Value,
}

/// Capture `request`'s file under `<artifact_dir>/presented-artifacts/` and
/// build its display-only row. `presentation_id` and `timestamp` are the
/// caller's (one per presentation).
///
/// # Errors
///
/// A missing, non-regular, or oversized artifact; a preview for a file that
/// is not a raster image; a failed capture write.
pub fn capture_presented_artifact(
    request: &PresentRequest,
    cwd: &Path,
    artifact_dir: &Path,
    session_id: &str,
    presentation_id: &str,
    timestamp: u64,
) -> anyhow::Result<CapturedArtifact> {
    let raw = Path::new(&request.path);
    let source = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let metadata = std::fs::metadata(&source).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow::anyhow!("Artifact does not exist: {}", request.path)
        }
        _ => anyhow::anyhow!("Artifact {} could not be read: {error}", request.path),
    })?;
    if !metadata.is_file() {
        anyhow::bail!("Artifact path must be a regular file: {}", request.path);
    }
    if metadata.len() > MAX_PRESENTED_ARTIFACT_BYTES {
        anyhow::bail!(
            "Artifact exceeds the 20 MiB presentation limit: {}",
            request.path
        );
    }
    let bytes = std::fs::read(&source)?;
    if bytes.len() as u64 > MAX_PRESENTED_ARTIFACT_BYTES {
        anyhow::bail!(
            "Artifact exceeds the 20 MiB presentation limit: {}",
            request.path
        );
    }
    let digest = Sha256::digest(&bytes);
    let artifact_id = hex_prefix(&digest, 8);
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact")
        .to_string();
    let source_image = detect_image_mime(&bytes);
    let preview = match (&request.preview, source_image) {
        (Some(_), None) => anyhow::bail!(
            "artifact.present preview is only accepted for a raster image artifact: {}",
            request.path
        ),
        (preview, Some(_)) => preview.clone(),
        (None, None) => None,
    };
    let destination = artifact_dir
        .join(CAPTURE_DIR_NAME)
        .join(format!("{artifact_id}-{}", safe_artifact_name(&name)));
    capture_file(&bytes, &destination)?;
    let destination_text = destination.to_string_lossy().to_string();
    let label_text = request
        .label
        .clone()
        .unwrap_or_else(|| format!("Artifact: {name}"));
    let mut blocks = vec![UserContentBlock::Text(TextContent {
        text: label_text,
        text_signature: None,
        rest: serde_json::Map::default(),
    })];
    let mut details = json!({
        "artifactId": artifact_id,
        "presentationId": presentation_id,
        "sessionId": session_id,
        "name": name,
    });
    let (kind, mime_type) = if let Some(preview) = &preview {
        blocks.push(UserContentBlock::Image(ImageContent {
            data: preview.data.clone(),
            mime_type: preview.mime_type.clone(),
            rest: serde_json::Map::default(),
        }));
        ("image", preview.mime_type.clone())
    } else {
        blocks.push(UserContentBlock::Text(TextContent {
            text: destination_text.clone(),
            text_signature: None,
            rest: serde_json::Map::default(),
        }));
        (
            "file",
            source_image
                .unwrap_or_else(|| fallback_mime_type(&name))
                .to_string(),
        )
    };
    let mut receipt = json!({
        "artifactId": artifact_id,
        "presentationId": presentation_id,
        "kind": kind,
        "name": name,
        "mimeType": mime_type,
        "byteSize": bytes.len(),
        "path": destination_text,
    });
    if let Some(label) = &request.label {
        details["label"] = json!(label);
    }
    details["kind"] = json!(kind);
    details["mimeType"] = json!(mime_type);
    details["byteSize"] = json!(bytes.len());
    details["path"] = json!(destination_text);
    if let Some(preview) = &preview {
        for (key, value) in [
            ("width", preview.width),
            ("height", preview.height),
            ("originalWidth", preview.original_width),
            ("originalHeight", preview.original_height),
        ] {
            if let Some(value) = value {
                details[key] = json!(value);
                receipt[key] = json!(value);
            }
        }
    }
    Ok(CapturedArtifact {
        message: CustomMessage {
            custom_type: PRESENTED_ARTIFACT_CUSTOM_TYPE.to_string(),
            content: UserContent::Blocks(blocks),
            display: true,
            details: Some(details),
            timestamp,
            rest: serde_json::Map::default(),
        },
        receipt,
    })
}

/// Write `bytes` at `destination` through a temp file; an existing capture
/// with identical bytes (the same artifact presented again) is kept.
fn capture_file(bytes: &[u8], destination: &Path) -> anyhow::Result<()> {
    if std::fs::read(destination).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("artifact capture path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&temporary, bytes)?;
    if let Err(error) = std::fs::rename(&temporary, destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// What the `artifact.present` handler captures into and appends to.
pub struct PresentContext {
    pub cwd: PathBuf,
    /// The session's artifact directory; `None` refuses (nowhere durable to
    /// capture into).
    pub artifact_dir: Option<PathBuf>,
    pub session_id: String,
    /// The engine's session, the default destination of the row.
    pub session: Arc<Mutex<SessionManager>>,
    /// The session counters a shown artifact counts into (adoption).
    pub counters: Option<Arc<super::telemetry::SessionCounters>>,
}

/// Register `artifact.present`: validate, capture, then hand the row to the
/// installed sink (or append it to the engine's session) and answer the
/// receipt.
pub fn register_artifact_present_handler(
    handlers: &mut HostRequestHandlers,
    presented: &Arc<PresentedArtifacts>,
    context: PresentContext,
) {
    let presented = Arc::clone(presented);
    let context = Arc::new(context);
    handlers.register(
        "artifact.present",
        host_handler(move |payload| {
            let presented = Arc::clone(&presented);
            let context = Arc::clone(&context);
            Box::pin(async move {
                let request = parse_present_request(&payload.data)?;
                let Some(artifact_dir) = context.artifact_dir.clone() else {
                    anyhow::bail!("artifact.present requires a session directory to capture into");
                };
                let cwd = context.cwd.clone();
                let session_id = context.session_id.clone();
                let captured = tokio::task::spawn_blocking(move || {
                    capture_presented_artifact(
                        &request,
                        &cwd,
                        &artifact_dir,
                        &session_id,
                        &uuid::Uuid::new_v4().to_string(),
                        crate::autonomous::now_millis(),
                    )
                })
                .await??;
                // Persist before answering: a failed write never shows an
                // artifact that disappears on replay.
                if let Some(sink) = presented.sink() {
                    sink(captured.message)?;
                } else {
                    let message = captured.message;
                    let mut session = context.session.lock().await;
                    session.append_custom_message(
                        &message.custom_type,
                        message.content,
                        message.display,
                        message.details,
                    )?;
                }
                if let Some(counters) = &context.counters {
                    counters.note_adoption(super::telemetry::SessionAdoption::ArtifactPresented);
                }
                Ok(captured.receipt)
            })
        }),
    );
}

#[cfg(test)]
mod tests;
