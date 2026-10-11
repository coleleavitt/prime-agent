//! Server-side fallback as the pi plugin requests and replays it
//! (anthropic-auth `packages/pi` `stream.ts`):
//!
//! - an OAuth request to Opus 5 (any point release) or Fable 5 carries
//!   `fallbacks: "default"` and both server-side-fallback betas (base, then
//!   category), so the server may serve a refused turn from a fallback
//!   model inline;
//! - the `fallback` content block a served fallback streams is kept as a
//!   thinking block (a word joiner signed `cortexkit-server-fallback-v1:
//!   <from>|<to>`), the host's session format having no such block;
//! - a later request replays that marker as the `fallback` block when it
//!   goes to a fallback model again, and drops it otherwise.

use serde_json::{Value, json};

use super::convert::family;

/// The marker's text: a word joiner.
pub(crate) const MARKER_TEXT: &str = "\u{2060}";
/// The marker's signature prefix.
pub(crate) const SIGNATURE_PREFIX: &str = "cortexkit-server-fallback-v1:";

/// A served fallback: the model that refused, the one that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    pub(crate) from: String,
    pub(crate) to: String,
}

/// `isServerFallbackModel`: Opus 5 (with its point releases) and Fable 5.
pub(crate) fn is_fallback_model(model: &str) -> bool {
    family::opus_5(model) || model == "claude-fable-5" || model.starts_with("claude-fable-5-")
}

/// `safeFallbackModel`: `claude-` and `[a-z0-9-]`, case-insensitive, at most
/// 128 long.
fn safe_model(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("claude-"))
        && value.len() > 7
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl Marker {
    /// `encodeFallbackMarker`.
    pub(crate) fn encode(&self) -> String {
        format!("{SIGNATURE_PREFIX}{}|{}", self.from, self.to)
    }

    /// `decodeFallbackMarker`.
    pub(crate) fn decode(value: &str) -> Option<Self> {
        let encoded = value.strip_prefix(SIGNATURE_PREFIX)?;
        let separator = encoded.find('|')?;
        if separator == 0 || Some(separator) != encoded.rfind('|') {
            return None;
        }
        let (from, to) = (&encoded[..separator], &encoded[separator + 1..]);
        (safe_model(from) && safe_model(to)).then(|| Self {
            from: from.to_string(),
            to: to.to_string(),
        })
    }

    /// `markerFromFallbackBlock`.
    pub(crate) fn from_block(block: &Value) -> Option<Self> {
        if block.get("type").and_then(Value::as_str) != Some("fallback") {
            return None;
        }
        let model = |side: &str| {
            block
                .get(side)
                .filter(|value| value.is_object())
                .and_then(|value| value.get("model"))
                .and_then(Value::as_str)
                .filter(|model| safe_model(model))
                .map(str::to_string)
        };
        Some(Self {
            from: model("from")?,
            to: model("to")?,
        })
    }
}

/// `rewriteStoredFallbackMarkers`: every marker in an assistant turn becomes
/// the `fallback` block (`enabled`, and the marker decodes) or is dropped.
/// Whether any was found.
pub(crate) fn rewrite_stored_markers(body: &mut Value, enabled: bool) -> bool {
    let mut changed = false;
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return changed;
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut rewritten = Vec::with_capacity(content.len());
        for block in content.drain(..) {
            let signature = block.get("signature").and_then(Value::as_str);
            let marker = block.get("type").and_then(Value::as_str) == Some("thinking")
                && block.get("thinking").and_then(Value::as_str) == Some(MARKER_TEXT)
                && signature.is_some_and(|signature| signature.starts_with(SIGNATURE_PREFIX));
            if !marker {
                rewritten.push(block);
                continue;
            }
            changed = true;
            if let Some(marker) = signature.and_then(Marker::decode).filter(|_| enabled) {
                rewritten.push(json!({
                    "type": "fallback",
                    "from": { "model": marker.from },
                    "to": { "model": marker.to },
                }));
            }
        }
        *content = rewritten;
    }
    changed
}

/// The events pa-ai reads for a streamed `fallback` block: the marker as a
/// thinking block (start, its text, its signature). `None` for any other
/// event.
pub(crate) fn marker_events(event: &Value) -> Option<Vec<Value>> {
    if event.get("type").and_then(Value::as_str) != Some("content_block_start") {
        return None;
    }
    let marker = Marker::from_block(event.get("content_block")?)?;
    let index = event.get("index").cloned().unwrap_or(Value::Null);
    Some(vec![
        json!({ "type": "content_block_start", "index": index, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
        json!({ "type": "content_block_delta", "index": index, "delta": { "type": "thinking_delta", "thinking": MARKER_TEXT } }),
        json!({ "type": "content_block_delta", "index": index, "delta": { "type": "signature_delta", "signature": marker.encode() } }),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip_and_reject_unsafe_models() {
        let marker = Marker {
            from: "claude-opus-5-5".to_string(),
            to: "claude-opus-4-8".to_string(),
        };
        assert_eq!(Marker::decode(&marker.encode()), Some(marker));
        for bad in [
            "cortexkit-server-fallback-v1:|claude-opus-4-8",
            "cortexkit-server-fallback-v1:claude-a|claude-b|claude-c",
            "cortexkit-server-fallback-v1:gpt-5|claude-opus-4-8",
            "cortexkit-server-fallback-v1:claude-|claude-opus-4-8",
            "cortexkit-server-fallback-v1:claude-opus_5|claude-opus-4-8",
            "claude-opus-5|claude-opus-4-8",
        ] {
            assert_eq!(Marker::decode(bad), None, "{bad}");
        }
        assert!(Marker::decode("cortexkit-server-fallback-v1:Claude-OPUS-5|claude-x").is_some());
    }

    #[test]
    fn fallback_models_are_opus_5_and_fable_5() {
        for model in [
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-opus-5-20260101",
            "claude-fable-5",
            "claude-fable-5-1",
        ] {
            assert!(is_fallback_model(model), "{model}");
        }
        for model in [
            "claude-opus-4-8",
            "claude-mythos-5",
            "claude-sonnet-5",
            "claude-fable-50",
        ] {
            assert!(!is_fallback_model(model), "{model}");
        }
    }
}
