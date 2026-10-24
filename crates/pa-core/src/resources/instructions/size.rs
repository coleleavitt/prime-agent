//! Instruction-file size warnings (Claude Code's `xLo` diagnostics, shown by
//! its `/doctor` and `/status`): a file over the per-file limit, or a set of
//! files whose combined size is over the total limit, costs prompt space on
//! every request, so the user is told instead of the files loading silently.
//!
//! The limits follow Claude Code's `Aet`/`MZn`: the per-file limit is 5% of
//! the context window in characters (4 per token), at least 40 000; the total
//! limit is at least 120 000 and never below the per-file one. A file cut at
//! [`super::MAX_INSTRUCTION_FILE_BYTES`] is reported too.

use super::{InstructionFile, MAX_INSTRUCTION_FILE_BYTES, TRUNCATION_NOTE_PREFIX};

/// Claude Code's floor for one file (`AZn`).
pub const MIN_FILE_LIMIT_CHARS: usize = 40_000;
/// Claude Code's floor for all files together (`PZn`).
pub const MIN_TOTAL_LIMIT_CHARS: usize = 120_000;
/// The share of the context window one file may take (`xZn`).
const WINDOW_SHARE: f64 = 0.05;
/// Characters per token in the limit estimate (`kh` for current models).
const CHARS_PER_TOKEN: f64 = 4.0;
/// The window assumed when the model's is unknown (`d6e`).
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// The per-file and total limits for a context window (`Aet`, `MZn`).
#[must_use]
pub fn limits(context_window: Option<u64>) -> (usize, usize) {
    let window = context_window
        .filter(|window| *window > 0)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let scaled = (window as f64 * WINDOW_SHARE * CHARS_PER_TOKEN).round() as usize;
    let file_limit = scaled.max(MIN_FILE_LIMIT_CHARS);
    (file_limit, file_limit.max(MIN_TOTAL_LIMIT_CHARS))
}

/// Claude Code's compact count (`Bs`: `Intl` compact notation, one decimal
/// from a thousand up, lowercased): `40.0k`, `1.2m`, `950`.
#[must_use]
pub fn compact_count(count: usize) -> String {
    let value = count as f64;
    if count < 1_000 {
        return count.to_string();
    }
    let (scaled, suffix) = if value >= 1e9 {
        (value / 1e9, "b")
    } else if value >= 1e6 {
        (value / 1e6, "m")
    } else {
        (value / 1e3, "k")
    };
    // Round half away from zero, like Claude Code's `Intl` compact notation
    // (Rust's `{:.1}` would round half to even: 45.25 -> 45.2, not 45.3).
    let rounded = (scaled * 10.0).round() / 10.0;
    format!("{rounded:.1}{suffix}")
}

/// The warnings for `files` under `context_window`, in Claude Code's words:
/// the total first, then each large file, then each file cut at the read
/// cap. Empty when every file fits.
#[must_use]
pub fn size_warnings(files: &[InstructionFile], context_window: Option<u64>) -> Vec<String> {
    let (file_limit, total_limit) = limits(context_window);
    let sizes: Vec<usize> = files
        .iter()
        .map(|file| file.content.chars().count())
        .collect();
    let mut warnings = Vec::new();
    // `NVt`: the files that are each within the limit, summed.
    let total: usize = sizes.iter().filter(|size| **size <= file_limit).sum();
    if total > total_limit {
        let all: usize = sizes.iter().sum();
        warnings.push(format!(
            "Instruction files will impact performance: {} files, {} chars in total > {}",
            files.len(),
            compact_count(all),
            compact_count(total_limit)
        ));
    }
    for (file, size) in files.iter().zip(&sizes) {
        if *size > file_limit {
            warnings.push(format!(
                "Large {} will impact performance ({} chars > {})",
                file.path.display(),
                compact_count(*size),
                compact_count(file_limit)
            ));
        }
    }
    for file in files {
        if file.content.contains(TRUNCATION_NOTE_PREFIX) {
            warnings.push(format!(
                "{} is larger than {} KiB; only its first {} KiB is loaded",
                file.path.display(),
                MAX_INSTRUCTION_FILE_BYTES / 1024,
                MAX_INSTRUCTION_FILE_BYTES / 1024
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::resources::instructions::InstructionKind;

    fn file(name: &str, chars: usize) -> InstructionFile {
        InstructionFile {
            path: PathBuf::from(name),
            kind: InstructionKind::Project,
            content: "x".repeat(chars),
            parent: None,
            globs: Vec::new(),
        }
    }

    #[test]
    fn limits_scale_with_the_window_above_claude_codes_floors() {
        assert_eq!(limits(None), (40_000, 120_000));
        assert_eq!(limits(Some(128_000)), (40_000, 120_000));
        assert_eq!(limits(Some(1_000_000)), (200_000, 200_000));
    }

    #[test]
    fn compact_counts_read_like_claude_codes() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(40_000), "40.0k");
        assert_eq!(compact_count(45_250), "45.3k");
        assert_eq!(compact_count(1_234_567), "1.2m");
    }

    #[test]
    fn a_large_file_is_named_with_its_size_and_the_limit() {
        let files = vec![file("small.md", 100), file("/repo/CLAUDE.md", 45_000)];
        assert_eq!(
            size_warnings(&files, None),
            vec!["Large /repo/CLAUDE.md will impact performance (45.0k chars > 40.0k)".to_string()]
        );
        assert!(size_warnings(&files, Some(1_000_000)).is_empty());
    }

    #[test]
    fn many_files_within_the_limit_still_warn_on_their_total() {
        let files = vec![
            file("a.md", 39_000),
            file("b.md", 39_000),
            file("c.md", 39_000),
            file("d.md", 39_000),
        ];
        assert_eq!(
            size_warnings(&files, None),
            vec![
                "Instruction files will impact performance: 4 files, 156.0k chars in total > 120.0k"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_file_cut_at_the_read_cap_is_reported() {
        let mut cut = file("/repo/AGENTS.md", 10);
        cut.content.push_str("\n\n");
        cut.content.push_str(TRUNCATION_NOTE_PREFIX);
        cut.content.push_str(" larger than 262144 bytes]");
        assert_eq!(
            size_warnings(&[cut], None),
            vec![
                "/repo/AGENTS.md is larger than 256 KiB; only its first 256 KiB is loaded"
                    .to_string()
            ]
        );
    }
}
