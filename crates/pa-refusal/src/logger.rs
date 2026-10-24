//! Refusal logger: writes JSONL event files to `~/.prime/agent/`.
//!
//! Log files (directory overridable with `REFUSAL_LOG_DIR`):
//! - `refusal-events.jsonl` — every refusal event
//! - `content-filter-outcomes.jsonl` — every finished request with filter summary
//! - `refused-bodies/*.json.gz` — gzipped request bodies of every refusal
//!
//! The refused-bodies directory is swept by size (256MB cap), oldest removed first.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use crate::filter::FilterSummary;

/// Log directory. `REFUSAL_LOG_DIR` overrides it for test isolation.
pub fn log_dir() -> PathBuf {
    std::env::var("REFUSAL_LOG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/home/cole".into());
            PathBuf::from(home).join(".prime/agent")
        })
}

/// Path to the refusal-events JSONL file.
pub fn refusal_events_path() -> PathBuf {
    log_dir().join("refusal-events.jsonl")
}

/// Path to the content-filter-outcomes JSONL file.
pub fn filter_outcomes_path() -> PathBuf {
    log_dir().join("content-filter-outcomes.jsonl")
}

/// Directory for gzipped refused request bodies.
pub fn refused_bodies_dir() -> PathBuf {
    log_dir().join("refused-bodies")
}

const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;
const REFUSED_BODY_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A refusal event record (matches TS `RefusalEvent`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RefusalEvent {
    pub timestamp: String,
    pub session_id: Option<String>,
    pub model: String,
    pub category: Option<String>,
    pub explanation: Option<String>,
    pub request_id: Option<String>,
    pub body_size: Option<u64>,
    pub message_count: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub was_rerouted: bool,
    pub fallback_model: Option<String>,
    pub trigger_terms: Option<Vec<String>>,
    pub body_file: Option<String>,
}

/// A content filter outcome event (matches TS `ContentFilterOutcomeEvent`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilterOutcomeEvent {
    pub timestamp: Option<String>,
    pub host: String,
    pub session_id: Option<String>,
    pub model: String,
    pub request_id: Option<String>,
    pub stop_reason: String,
    pub refusal_category: Option<String>,
    pub filter: Option<FilterSummary>,
}

/// Write a refusal event to `refusal-events.jsonl`.
pub fn log_refusal_event(event: &RefusalEvent) -> Result<(), Box<dyn std::error::Error>> {
    let path = refusal_events_path();
    let dir: std::path::PathBuf = path.parent().map(|p| p.to_path_buf()).unwrap_or(log_dir());
    fs::create_dir_all(dir).ok();

    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(serde_json::to_string(event)?.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let meta = fs::metadata(&path)?;
    if meta.len() > MAX_LOG_SIZE {
        rotate_log(&path)?;
    }

    Ok(())
}

/// Write a filter outcome event to `content-filter-outcomes.jsonl`.
pub fn log_filter_outcome(event: &FilterOutcomeEvent) -> Result<(), Box<dyn std::error::Error>> {
    let path = filter_outcomes_path();
    let dir: std::path::PathBuf = path.parent().map(|p| p.to_path_buf()).unwrap_or(log_dir());
    fs::create_dir_all(dir).ok();

    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(serde_json::to_string(event)?.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let meta = fs::metadata(&path)?;
    if meta.len() > MAX_LOG_SIZE {
        rotate_log(&path)?;
    }

    Ok(())
}

/// Feedback event: actual Anthropic response used to validate/adjust surrogate weights.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeedbackEvent {
    pub timestamp: String,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub predicted_refusal: bool,
    pub actual_refusal: bool,
    pub stop_reason: Option<String>,
    pub feedback_type: String,
    pub original_score: f64,
    pub adjusted_score: f64,
}

impl FeedbackEvent {
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the feedback record's fields one-to-one"
    )]
    pub fn new(
        timestamp: String,
        session_id: Option<String>,
        request_id: Option<String>,
        predicted_refusal: bool,
        actual_refusal: bool,
        stop_reason: Option<String>,
        original_score: f64,
        adjusted_score: f64,
    ) -> Self {
        let feedback_type = if predicted_refusal && actual_refusal {
            "true_positive"
        } else if predicted_refusal && !actual_refusal {
            "false_positive"
        } else if !predicted_refusal && actual_refusal {
            "false_negative"
        } else {
            "true_negative"
        };

        Self {
            timestamp,
            session_id,
            request_id,
            predicted_refusal,
            actual_refusal,
            stop_reason,
            feedback_type: feedback_type.to_string(),
            original_score,
            adjusted_score,
        }
    }
}

/// Write feedback events to `feedback-events.jsonl`.
pub fn log_feedback_event(event: &FeedbackEvent) -> Result<(), Box<dyn std::error::Error>> {
    let path = log_dir().join("feedback-events.jsonl");
    let file = OpenOptions::new().create(true).append(true).open(path)?;

    let json = serde_json::to_string(event)?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "{}", json)?;
    Ok(())
}

/// Save a gzipped refused request body.
pub fn save_refused_body(
    body: &serde_json::Value,
    session_id: &str,
    _model: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let dir = refused_bodies_dir();
    fs::create_dir_all(&dir).ok();

    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S");
    let filename = format!("{}-{}.json.gz", session_id, timestamp);
    let path = dir.join(&filename);

    let body_json = serde_json::to_string(body)?;
    let compressed = gzip_compress(&body_json)?;
    fs::write(&path, &compressed)?;

    sweep_bodies(&dir)?;

    Ok(format!("refused-bodies/{}", filename))
}

fn rotate_log(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let bak = path.with_extension("jsonl.bak");
    fs::rename(path, &bak)?;
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .open(path)?;
    Ok(())
}

fn sweep_bodies(dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut files: Vec<(PathBuf, u64)> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| (e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)))
        .collect();

    files.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name()));

    let mut total = 0u64;
    let mut to_remove = Vec::new();

    for (path, size) in &files {
        total += size;
        if total > REFUSED_BODY_MAX_BYTES {
            to_remove.push(path.clone());
        }
    }

    for path in to_remove {
        fs::remove_file(&path).ok();
    }

    Ok(())
}

fn gzip_compress(data: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data.as_bytes())?;
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn test_gzip_roundtrip() {
        let original = r#"{"messages": [{"role": "user", "content": "test"}]}"#;
        let compressed = gzip_compress(original).unwrap();
        let mut decompressed = String::new();
        flate2::read::GzDecoder::new(&compressed[..])
            .read_to_string(&mut decompressed)
            .unwrap();
        assert_eq!(original, decompressed);
    }
}
