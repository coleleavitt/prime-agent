//! Reading the retained log generations: the compressed `<path>.old.<n>.gz`
//! files (highest generation, i.e. oldest, first), then `<path>.old`, then
//! the live `<path>`.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Decompression bound for one gzip generation (the TS `maxOutputLength`).
const MAX_DECOMPRESSED_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// The existing files of a log, oldest first.
#[must_use]
pub fn retained_log_files(log_path: &Path) -> Vec<PathBuf> {
    let directory = log_path.parent().unwrap_or_else(|| Path::new("."));
    let base = log_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{base}.old.");
    let mut compressed: Vec<(u64, PathBuf)> = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let generation = name
                        .strip_prefix(&prefix)?
                        .strip_suffix(".gz")
                        .filter(|digits| {
                            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                        })?
                        .parse::<u64>()
                        .ok()?;
                    Some((generation, directory.join(name)))
                })
                .collect()
        })
        .unwrap_or_default();
    compressed.sort_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    let mut old = log_path.as_os_str().to_os_string();
    old.push(".old");
    compressed
        .into_iter()
        .map(|(_, path)| path)
        .chain([PathBuf::from(old), log_path.to_path_buf()])
        .filter(|path| path.exists())
        .collect()
}

/// One file's text; `.gz` files are decompressed up to the bound.
///
/// # Errors
///
/// A read or decompression failure (including a generation past the bound).
pub fn read_log_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    if path.extension().is_some_and(|extension| extension == "gz") {
        let mut text = String::new();
        let read = flate2::read::GzDecoder::new(bytes.as_slice())
            .take(MAX_DECOMPRESSED_LOG_BYTES + 1)
            .read_to_string(&mut text)?;
        if read as u64 > MAX_DECOMPRESSED_LOG_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cannot create a string longer than the decompression limit",
            ));
        }
        return Ok(text);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Paths joined with `", "` for messages and headings.
pub(crate) fn join_paths(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A value as `String(value)` / `JSON.stringify(value)` would render it in
/// the TS readers: strings verbatim, numbers in JavaScript form, everything
/// else as compact JSON.
pub(crate) fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number
            .as_f64()
            .filter(|float| !number.is_i64() && !number.is_u64() && float.is_finite())
            .map_or_else(|| number.to_string(), |float| format!("{float}")),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

/// Replace line breaks and tabs with spaces and every other control
/// character (ESC included) with `?`, so a log value cannot drive the
/// terminal.
pub(crate) fn terminal_safe(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\r' | '\n' | '\t' => ' ',
            '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}' => '?',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_generations_oldest_first_and_only_existing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.jsonl");
        assert_eq!(retained_log_files(&path), Vec::<PathBuf>::new());
        for name in [
            "agent.jsonl",
            "agent.jsonl.old",
            "agent.jsonl.old.1.gz",
            "agent.jsonl.old.10.gz",
            "agent.jsonl.old.2.gz",
            "agent.jsonl.old.x.gz",
            "other.jsonl.old.3.gz",
        ] {
            std::fs::write(dir.path().join(name), "").expect("write");
        }
        let names: Vec<String> = retained_log_files(&path)
            .iter()
            .map(|file| {
                file.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "agent.jsonl.old.10.gz",
                "agent.jsonl.old.2.gz",
                "agent.jsonl.old.1.gz",
                "agent.jsonl.old",
                "agent.jsonl",
            ]
        );
    }

    #[test]
    fn renders_values_like_javascript_and_neutralizes_controls() {
        let values: Vec<String> = [
            Value::from("text"),
            Value::from(1050),
            serde_json::from_str("812.5").expect("number"),
            serde_json::from_str("1.0").expect("number"),
            Value::Bool(true),
            Value::Null,
        ]
        .iter()
        .map(js_text)
        .collect();
        assert_eq!(values, ["text", "1050", "812.5", "1", "true", "null"]);
        assert_eq!(terminal_safe("a\nb\tc\u{1b}[31md\u{7f}"), "a b c?[31md?");
    }
}
