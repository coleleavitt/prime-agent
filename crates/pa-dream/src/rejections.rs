//! The per-run rejection log of the LLM proposer and dreamer (TS
//! `rejections.ts`): one JSONL line per child result that was refused, so a
//! rejected output's cause is recoverable after the fact.
//!
//! Layout: `<dreamDir>/rejections/<runKey>.jsonl`, beside `trees/` (never
//! inside it, so tree listing never sees it). Every field is a scalar; the
//! child's output is never stored whole, only a bounded head/tail excerpt.
//! Reached only from the LLM path: the local path never writes a rejection,
//! so it stays byte-identical.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::child::RunAgentStatus;
use crate::json;
use crate::proposer::ProposalRejectReason;
use crate::store::{DreamStoreError, append_private, create_dir_private};

/// Total characters (UTF-16 code units, as JS counts them) an excerpt keeps.
pub const REJECTION_EXCERPT_CHARS: usize = 240;
const EXCERPT_JOINER: &str = " ... ";

/// Which child role produced the rejected result; absent reads as `proposer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RejectionRole {
    Proposer,
    Dreamer,
}

/// One rejected child result, as a caller records it (`type` and `ts` are the log's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectionInput {
    /// `Some(Dreamer)` for the dreamer; the proposer omits the field.
    pub role: Option<RejectionRole>,
    /// Loop iteration whose rollout the attempt belongs to.
    pub iteration: u32,
    /// Online round of the rollout (0 for the dreamer).
    pub round: u32,
    /// 1-based child result index within the call: 2 is the retry.
    pub attempt: u32,
    pub reason: ProposalRejectReason,
    /// The child's terminal status.
    pub status: RunAgentStatus,
    /// True when this was the call's last child result and the local mutator stood in.
    pub fell_back: bool,
    pub tokens: u64,
    pub output_tokens: u64,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    pub excerpt: String,
}

/// One parsed line of a rejection log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRejection {
    pub ts: u64,
    pub input: RejectionInput,
}

/// `<dir>/rejections`.
#[must_use]
pub fn rejections_dir(dir: &Path) -> PathBuf {
    dir.join("rejections")
}

/// `<dir>/rejections/<runKey>.jsonl`.
#[must_use]
pub fn rejections_path(dir: &Path, run_key: &str) -> PathBuf {
    rejections_dir(dir).join(format!("{run_key}.jsonl"))
}

/// Head and tail of `output`, at most `max_chars` UTF-16 code units in total
/// (TS `excerptOf`). A surrogate pair cut at the seam becomes U+FFFD, where
/// JS would keep the lone surrogate.
#[must_use]
pub fn excerpt_of(output: &str, max_chars: usize) -> String {
    let units: Vec<u16> = output.encode_utf16().collect();
    let cap = max_chars.max(EXCERPT_JOINER.len() + 2);
    if units.len() <= cap {
        return output.to_string();
    }
    let keep = cap - EXCERPT_JOINER.len();
    let head = keep.div_ceil(2);
    let tail = keep - head;
    format!(
        "{}{EXCERPT_JOINER}{}",
        String::from_utf16_lossy(&units[..head]),
        String::from_utf16_lossy(&units[units.len() - tail..])
    )
}

/// The JSON line of one record, in the TS key order.
fn line_of(ts: u64, input: &RejectionInput) -> Value {
    let mut line = serde_json::Map::new();
    line.insert("type".into(), Value::from("rejection"));
    line.insert("ts".into(), Value::from(ts));
    if let Some(role) = input.role {
        line.insert(
            "role".into(),
            Value::from(match role {
                RejectionRole::Proposer => "proposer",
                RejectionRole::Dreamer => "dreamer",
            }),
        );
    }
    line.insert("iteration".into(), Value::from(input.iteration));
    line.insert("round".into(), Value::from(input.round));
    line.insert("attempt".into(), Value::from(input.attempt));
    line.insert("reason".into(), Value::from(input.reason.as_str()));
    line.insert("status".into(), Value::from(input.status.as_str()));
    line.insert("fellBack".into(), Value::from(input.fell_back));
    line.insert("tokens".into(), Value::from(input.tokens));
    line.insert("outputTokens".into(), Value::from(input.output_tokens));
    if let Some(stop_reason) = &input.stop_reason {
        line.insert("stopReason".into(), Value::from(stop_reason.as_str()));
    }
    if let Some(error) = &input.error {
        line.insert("error".into(), Value::from(error.as_str()));
    }
    line.insert("excerpt".into(), Value::from(input.excerpt.as_str()));
    Value::Object(line)
}

/// Append-only writer for one run's rejection log; the file is created on the
/// first rejection.
pub struct RejectionLog<'a> {
    pub path: PathBuf,
    clock: &'a dyn Fn() -> u64,
}

impl<'a> RejectionLog<'a> {
    #[must_use]
    pub fn new(path: PathBuf, clock: &'a dyn Fn() -> u64) -> Self {
        Self { path, clock }
    }

    /// Append one record stamped with the clock.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn append(&self, input: &RejectionInput) -> Result<(), DreamStoreError> {
        if let Some(parent) = self.path.parent() {
            create_dir_private(parent)?;
        }
        let line = line_of((self.clock)(), input);
        append_private(&self.path, &format!("{}\n", json::stringify(&line)))
    }
}

fn malformed(path: &Path) -> DreamStoreError {
    DreamStoreError::Message(format!("malformed rejection line in {}", path.display()))
}

fn parse_line(value: &Value) -> Option<ProposalRejection> {
    let record = value.as_object()?;
    if record.get("type")?.as_str()? != "rejection" {
        return None;
    }
    // This log writes only non-negative integers; anything else is malformed.
    let count = |key: &str| record.get(key)?.as_u64();
    let role = match record.get("role") {
        None => None,
        Some(Value::String(role)) if role == "proposer" => Some(RejectionRole::Proposer),
        Some(Value::String(role)) if role == "dreamer" => Some(RejectionRole::Dreamer),
        Some(_) => return None,
    };
    let text = |key: &str| -> Option<Option<String>> {
        match record.get(key) {
            None => Some(None),
            Some(Value::String(text)) => Some(Some(text.clone())),
            Some(_) => None,
        }
    };
    let narrow = |value: u64| u32::try_from(value).ok();
    Some(ProposalRejection {
        ts: count("ts")?,
        input: RejectionInput {
            role,
            iteration: narrow(count("iteration")?)?,
            round: narrow(count("round")?)?,
            attempt: narrow(count("attempt")?)?,
            reason: ProposalRejectReason::from_name(record.get("reason")?.as_str()?)?,
            status: RunAgentStatus::from_name(record.get("status")?.as_str()?)?,
            fell_back: record.get("fellBack")?.as_bool()?,
            tokens: count("tokens")?,
            output_tokens: count("outputTokens")?,
            stop_reason: text("stopReason")?,
            error: text("error")?,
            excerpt: record.get("excerpt")?.as_str()?.to_string(),
        },
    })
}

/// Parse a rejection log; a missing file is an empty log.
///
/// # Errors
///
/// [`DreamStoreError`] on a malformed line or an unreadable file.
pub fn read_rejections(path: &Path) -> Result<Vec<ProposalRejection>, DreamStoreError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(DreamStoreError::io(path, error)),
    };
    let mut records = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let value = json::parse(line).map_err(|_| malformed(path))?;
        records.push(parse_line(&value).ok_or_else(|| malformed(path))?);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(reason: ProposalRejectReason) -> RejectionInput {
        RejectionInput {
            role: None,
            iteration: 3,
            round: 2,
            attempt: 1,
            reason,
            status: RunAgentStatus::Completed,
            fell_back: false,
            tokens: 40,
            output_tokens: 15,
            stop_reason: None,
            error: Some("autocorrelation artifact must have a weights array of length 4".into()),
            excerpt: "{\"n\": 4}".into(),
        }
    }

    #[test]
    fn the_excerpt_keeps_head_and_tail_within_the_cap() {
        assert_eq!(excerpt_of("short", 240), "short");
        let long = format!("Let me reason{}[1, 2,", "x".repeat(400));
        let excerpt = excerpt_of(&long, 240);
        assert_eq!(excerpt.encode_utf16().count(), 240);
        assert!(excerpt.starts_with("Let me reason"));
        assert!(excerpt.ends_with("[1, 2,"));
        assert!(excerpt.contains(" ... "));
        // A cap below the joiner is lifted to the joiner plus one unit each side.
        assert_eq!(excerpt_of("abcdefghij", 0), "a ... j");
    }

    #[test]
    fn a_log_round_trips_in_the_ts_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = rejections_path(dir.path(), "unit");
        let clock = || 1_700_000_000_000_u64;
        let log = RejectionLog::new(path.clone(), &clock);
        let mut dreamer = input(ProposalRejectReason::Length);
        dreamer.role = Some(RejectionRole::Dreamer);
        dreamer.stop_reason = Some("length".into());
        dreamer.fell_back = true;
        log.append(&input(ProposalRejectReason::Shape)).unwrap();
        log.append(&dreamer).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            r#"{"type":"rejection","ts":1700000000000,"iteration":3,"round":2,"attempt":1,"reason":"shape","status":"completed","fellBack":false,"tokens":40,"outputTokens":15,"error":"autocorrelation artifact must have a weights array of length 4","excerpt":"{\"n\": 4}"}"#
        );
        assert_eq!(
            read_rejections(&path).unwrap(),
            vec![
                ProposalRejection {
                    ts: 1_700_000_000_000,
                    input: input(ProposalRejectReason::Shape)
                },
                ProposalRejection {
                    ts: 1_700_000_000_000,
                    input: dreamer
                },
            ]
        );
    }

    #[test]
    fn a_missing_log_is_empty_and_a_malformed_line_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = rejections_path(dir.path(), "none");
        assert_eq!(read_rejections(&path).unwrap(), Vec::new());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"type\":\"rejection\"}\n").unwrap();
        assert!(read_rejections(&path).is_err());
    }
}
