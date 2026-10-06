//! The `vision.read` kernel host request (upstream #2664, path B of the
//! image-model contract whose path A is [`super::image_delegation`]): the
//! bundled `attach_image` skill on a text-only session model hands its
//! images here instead of failing, and one child on the resolved
//! `settings.imageModel` reads them. Only the child's text reading comes
//! back — the session model never receives an image block.
//!
//! The read reuses the image-turn delegation's child lifecycle
//! ([`crate::rlm_children::SupervisorChildSessions::delegate_image_turn`]),
//! its resolver and refusals ([`AgentSessionEngine::resolve_image_turn_route`])
//! and its allowlist gate, so a text-only session reads an image through
//! `attach_image` exactly as it would through a pasted image. It is
//! registered only on a supervisor-backed worker (the only one that can
//! host the child); elsewhere the request stays unavailable and the skill
//! keeps its vision-capability error.
//!
//! Bounds (upstream's): at most [`MAX_IMAGES`] images per read, each at
//! most [`MAX_IMAGE_BYTES`] decoded and [`MAX_TOTAL_BYTES`] together, PNG /
//! JPEG / GIF / WebP only; the question is capped at
//! [`MAX_QUESTION_CHARS`], the reading at [`MAX_READING_CHARS`], and the
//! child gets [`READ_TIMEOUT`] before the read is abandoned (the child is
//! killed and settled, like an aborted turn delegation).

use std::time::{Duration, Instant};

use pa_core::kernel::shared::{host_handler, HostRequestHandlers};
use pa_types::sync::MutexExt;
use serde_json::{json, Value};

use super::AgentSessionEngine;

/// Images one read carries.
const MAX_IMAGES: usize = 8;
/// Decoded bytes per image.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Decoded bytes per read.
const MAX_TOTAL_BYTES: usize = 24 * 1024 * 1024;
/// Characters of the caller's question the child sees.
const MAX_QUESTION_CHARS: usize = 2000;
/// Characters of the child's reading returned to the kernel.
const MAX_READING_CHARS: usize = 4000;
/// How long the child may take to answer.
const READ_TIMEOUT: Duration = Duration::from_secs(180);
/// The image types a read accepts (the skill's own format check).
const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
/// The question a read without one asks.
const DEFAULT_QUESTION: &str =
    "Describe what the attached images show and transcribe any text they contain.";

/// One validated `vision.read` request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VisionReadRequest {
    pub(crate) images: Vec<pa_agent::types::ImageContent>,
    pub(crate) question: String,
}

/// Why a read produced no reading.
#[derive(Debug, Clone, PartialEq)]
enum VisionReadError {
    /// No child spawned: no usable image model, blocked images, a
    /// vision-capable session model, or the allowlist.
    Refused(String),
    /// The child ran and failed (or timed out, or the caller cancelled).
    Failed(String),
}

/// Validate a `vision.read` payload: `images` is a non-empty array of
/// `{data, mime_type}` (base64 data of a supported type) within the
/// bounds; `question` is an optional string.
///
/// # Errors
///
/// The refusal naming the first entry or bound the payload breaks: a read
/// never silently drops an image.
pub(crate) fn parse_vision_read(data: &Value) -> anyhow::Result<VisionReadRequest> {
    let Some(entries) = data.get("images").and_then(Value::as_array) else {
        anyhow::bail!("vision.read images must be an array of {{data, mime_type}} objects");
    };
    if entries.is_empty() {
        anyhow::bail!("vision.read needs at least one image");
    }
    if entries.len() > MAX_IMAGES {
        anyhow::bail!(
            "vision.read carries at most {MAX_IMAGES} images per read, got {}",
            entries.len()
        );
    }
    let mut images = Vec::with_capacity(entries.len());
    let mut total_bytes = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        let mime_type = entry
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !IMAGE_MIME_TYPES.contains(&mime_type) {
            anyhow::bail!(
                "vision.read image {index} has unsupported type \"{mime_type}\" (PNG, JPEG, GIF, WebP)"
            );
        }
        let data = entry
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(bytes) = decoded_len(data) else {
            anyhow::bail!("vision.read image {index} data is not base64");
        };
        if bytes > MAX_IMAGE_BYTES {
            anyhow::bail!(
                "vision.read image {index} is {bytes} bytes; images must be at most {MAX_IMAGE_BYTES} bytes"
            );
        }
        total_bytes += bytes;
        if total_bytes > MAX_TOTAL_BYTES {
            anyhow::bail!("vision.read images exceed {MAX_TOTAL_BYTES} bytes together");
        }
        images.push(pa_agent::types::ImageContent {
            data: data.to_string(),
            mime_type: mime_type.to_string(),
        });
    }
    let question = match data.get("question") {
        None | Some(Value::Null) => DEFAULT_QUESTION.to_string(),
        Some(Value::String(question)) if question.trim().is_empty() => DEFAULT_QUESTION.to_string(),
        Some(Value::String(question)) => question.chars().take(MAX_QUESTION_CHARS).collect(),
        Some(_) => anyhow::bail!("vision.read question must be a string when provided"),
    };
    Ok(VisionReadRequest { images, question })
}

/// The decoded size of non-empty, padded standard base64, or `None` when
/// `data` is not that.
fn decoded_len(data: &str) -> Option<usize> {
    let bytes = data.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    let body = &bytes[..bytes.len() - padding];
    let alphabet = |byte: &u8| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/';
    (padding <= 2 && body.iter().all(alphabet)).then(|| bytes.len() / 4 * 3 - padding)
}

/// The read child's whole task: the caller's question, description only,
/// and its final message IS the reading (no agent messages, no subagents).
pub(crate) fn vision_read_child_prompt(question: &str) -> String {
    format!(
        "You are the image reader for a parent agent session whose model cannot see images. \
The parent asks about the attached images:\n\n<question>\n{question}\n</question>\n\n\
Describe every attached image in complete, concrete detail (contents, any text in it, layout, \
colors, numbers) so the text-only parent model can act on it. Your final message is the reading \
itself: do not send agent messages, do not spawn subagents, and do not solve the parent's task."
    )
}

/// The reading capped at [`MAX_READING_CHARS`], marked when cut.
fn cap_reading(reading: &str) -> String {
    match reading.char_indices().nth(MAX_READING_CHARS) {
        Some((cut, _)) => format!(
            "{}\n[reading truncated at {MAX_READING_CHARS} characters]",
            &reading[..cut]
        ),
        None => reading.to_string(),
    }
}

impl AgentSessionEngine {
    /// Register `vision.read` on a supervisor-backed worker whose engine
    /// arc is registered (the handler holds the engine weakly).
    pub(crate) fn register_vision_read_host_handler(&self, handlers: &mut HostRequestHandlers) {
        if self.children.is_none() {
            return;
        }
        let Some(weak) = self.self_weak.lock_or_recover().clone() else {
            return;
        };
        handlers.register(
            "vision.read",
            host_handler(move |payload| {
                let weak = weak.clone();
                Box::pin(async move {
                    let request = parse_vision_read(&payload.data)?;
                    let Some(engine) = weak.upgrade() else {
                        anyhow::bail!("the session ended before the image could be read");
                    };
                    // Read in the handler's own future: the token is task-local.
                    let cancel = pa_core::kernel::shared::host_request_cancellation();
                    // The engine's model resolution and child wait are the
                    // worker's synchronous seams (the turn path drives them
                    // from its prompt thread): they run off the async workers.
                    tokio::task::spawn_blocking(move || {
                        engine.read_images_with_vision_child(request, &|| {
                            cancel
                                .as_ref()
                                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
                        })
                    })
                    .await
                    .map_err(|error| anyhow::anyhow!("vision.read did not finish: {error}"))?
                    .map_err(anyhow::Error::msg)
                })
            }),
        );
    }

    /// Read the request's images with one child on the resolved image
    /// model: `{text, model}` with the capped reading, or the actionable
    /// refusal / failure. Reports the outcome (`vision read`).
    pub(crate) fn read_images_with_vision_child(
        &self,
        request: VisionReadRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        let image_count = request.images.len();
        let result = self.vision_read(request, cancelled);
        if let Some((client, session_id)) = self.delegation_telemetry() {
            let outcome = match &result {
                Ok(_) => "answered",
                Err(VisionReadError::Refused(_)) => "refused",
                Err(VisionReadError::Failed(_)) => "failed",
            };
            pa_core::session_engine::telemetry::track_vision_read(
                &client,
                &session_id,
                outcome,
                image_count,
            );
        }
        result.map_err(|error| match error {
            VisionReadError::Refused(message) | VisionReadError::Failed(message) => message,
        })
    }

    fn vision_read(
        &self,
        request: VisionReadRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Value, VisionReadError> {
        let Some(children) = self.children.as_ref() else {
            return Err(VisionReadError::Refused(
                "vision.read needs a daemon-backed session to host the image-model child"
                    .to_string(),
            ));
        };
        let resolved = match self.resolve_image_turn_route(true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                let settings =
                    pa_core::settings::SettingsManager::create(self.cwd(), &self.config.agent_dir);
                return Err(VisionReadError::Refused(if settings.get_block_images() {
                    "images.blockImages is on in settings.json: no image is sent to any model, so the image cannot be read.".to_string()
                } else {
                    "The session model can see images: attach_image loads them into context directly.".to_string()
                }));
            }
            Err(refusal) => return Err(VisionReadError::Refused(format!("{refusal:#}"))),
        };
        self.assert_image_model_allowed(&resolved)
            .map_err(VisionReadError::Refused)?;
        // Funded from the delegation budget like the image-turn child; an
        // exhausted pool refuses the read before any child exists.
        let funding = self
            .reserve_image_child_grant()
            .map_err(|error| VisionReadError::Refused(format!("{error:#}")))?;
        let model = format!("{}/{}", resolved.model.provider, resolved.model.id);
        let deadline = Instant::now() + READ_TIMEOUT;
        let outcome = self.runtime.block_on(children.delegate_image_turn(
            crate::rlm_children::ImageDelegationRequest {
                prompt: vision_read_child_prompt(&request.question),
                model: model.clone(),
                thinking: Some(resolved.thinking_level.wire_name().to_string()),
                images: request.images,
                token_budget: funding.as_ref().map(|(_, grant)| *grant),
            },
            &|| cancelled() || Instant::now() >= deadline,
        ));
        match outcome {
            crate::rlm_children::ImageDelegationOutcome::Answered {
                child_id,
                session_name,
                answer,
            } => {
                if let Some((bridge, grant)) = &funding {
                    bridge.attribute_child_grant(*grant, &child_id, &session_name);
                }
                Ok(json!({ "text": cap_reading(&answer), "model": model }))
            }
            crate::rlm_children::ImageDelegationOutcome::Failed { error } => {
                Err(VisionReadError::Failed(if cancelled() {
                    "the image read was cancelled".to_string()
                } else if Instant::now() >= deadline {
                    format!(
                        "the image model {model} did not read the image within {}s; check the imageModel setting (settings.json)",
                        READ_TIMEOUT.as_secs()
                    )
                } else {
                    format!("the image model {model} could not read the image: {error}")
                }))
            }
        }
    }
}

#[cfg(test)]
#[path = "vision_read_tests.rs"]
mod tests;
