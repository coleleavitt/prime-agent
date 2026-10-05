//! Shared diff computation utilities for the edit tool: a faithful port of jsdiff
//! v9 `diffLines` so generated diffs match the TypeScript product byte for byte.

use std::path::Path;

use unicode_normalization::UnicodeNormalization;

use crate::tools::jsdiff::diff_lines;
use crate::tools::path_utils::resolve_to_cwd;

// Line endings / normalization

/// Detect whether the content uses CRLF or LF line endings.
pub fn detect_line_ending(content: &str) -> LineEnding {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    match (crlf_idx, lf_idx) {
        (_, None) | (None, _) => LineEnding::Lf,
        (Some(c), Some(l)) => {
            if c < l {
                LineEnding::CrLf
            } else {
                LineEnding::Lf
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    CrLf,
    Lf,
}

/// Convert CRLF and lone CR line endings to LF.
pub fn normalize_to_lf(text: &str) -> String {
    // Replace \r\n first, then remaining \r (matches the TS chain).
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Convert LF endings back to the original file's ending.
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}

/// JS `String.prototype.trimEnd` whitespace set (differs from Rust's).
fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{09}'
            ..='\u{0D}'
                | ' '
                | '\u{A0}'
                | '\u{1680}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    ) || ('\u{2000}'..='\u{200A}').contains(&ch)
}

fn js_trim_end(line: &str) -> &str {
    line.trim_end_matches(is_js_whitespace)
}

/// Normalize text for fuzzy matching, progressively:
/// - NFKC normalization
/// - strip trailing whitespace per line
/// - smart quotes, Unicode dashes, and special spaces to ASCII/space
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.chars().nfkc().collect();
    let trimmed: String = nfkc
        .split('\n')
        .map(js_trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    trimmed.chars().map(fuzzy_fold_char).collect()
}

/// The per-character fold applied after NFKC: smart quotes, Unicode dashes,
/// and special spaces to their ASCII forms. It maps whitespace to whitespace
/// and nothing else to whitespace, so it commutes with the trailing trim.
fn fuzzy_fold_char(ch: char) -> char {
    match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{00A0}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c if ('\u{2002}'..='\u{200A}').contains(&c) => ' ',
        c => c,
    }
}

/// Strip UTF-8 BOM if present, returning both the BOM and the text without it.
pub fn strip_bom(content: &str) -> (&str, &str) {
    if let Some(rest) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}", rest)
    } else {
        ("", content)
    }
}

// Fuzzy matching

/// Fuzzy-normalized content plus the offsets at which it lines up with the
/// original: each `(normalized, original)` pair means
/// `normalize_for_fuzzy_match(&original[..o])` ends exactly where
/// `normalized[..n]` does. A fuzzy match is located in normalized space and
/// mapped back through these boundaries, so the replacement is spliced into
/// the original content and nothing outside the match is normalized.
struct FuzzyIndex {
    normalized: String,
    boundaries: Vec<(usize, usize)>,
}

impl FuzzyIndex {
    fn new(content: &str) -> Self {
        let mut normalized = String::with_capacity(content.len());
        let mut boundaries = Vec::new();
        let mut line_start = 0;
        for (line_index, line) in content.split('\n').enumerate() {
            if line_index > 0 {
                // The newline itself maps 1:1.
                boundaries.push((normalized.len(), line_start - 1));
                normalized.push('\n');
                boundaries.push((normalized.len(), line_start));
            }
            Self::push_line(line, line_start, &mut normalized, &mut boundaries);
            line_start += line.len() + 1;
        }
        Self {
            normalized,
            boundaries,
        }
    }

    /// Append one line's normalized form. Boundaries fall between clusters
    /// (a starter plus its combining marks), which NFKC normalizes
    /// independently; a line where that does not hold keeps only its ends.
    fn push_line(
        line: &str,
        line_start: usize,
        normalized: &mut String,
        boundaries: &mut Vec<(usize, usize)>,
    ) {
        let normalized_start = normalized.len();
        let whole: String = line.chars().nfkc().map(fuzzy_fold_char).collect();
        let visible_len = js_trim_end(&whole).len();

        let mut cluster_ends: Vec<(usize, usize)> = Vec::new(); // (normalized len, original end)
        let mut folded = String::with_capacity(whole.len());
        let mut cluster_start = 0;
        for (offset, ch) in line.char_indices().skip(1) {
            if unicode_normalization::char::canonical_combining_class(ch) == 0 {
                folded.extend(
                    line[cluster_start..offset]
                        .chars()
                        .nfkc()
                        .map(fuzzy_fold_char),
                );
                cluster_ends.push((folded.len(), offset));
                cluster_start = offset;
            }
        }
        folded.extend(line[cluster_start..].chars().nfkc().map(fuzzy_fold_char));
        if folded != whole {
            cluster_ends.clear();
        }

        boundaries.push((normalized_start, line_start));
        boundaries.extend(
            cluster_ends
                .into_iter()
                .filter(|&(len, _)| len <= visible_len)
                .map(|(len, end)| (normalized_start + len, line_start + end)),
        );
        // Without a cluster boundary at the visible end, the end of the
        // visible text maps to the end of the line.
        if boundaries.last().map(|&(n, _)| n) != Some(normalized_start + visible_len) {
            boundaries.push((normalized_start + visible_len, line_start + line.len()));
        }
        normalized.push_str(&whole[..visible_len]);
    }

    /// The original offset where a match starting at `normalized_offset`
    /// begins: the latest aligned position, so the replaced span is minimal.
    fn start_offset(&self, normalized_offset: usize) -> Option<usize> {
        let end = self
            .boundaries
            .partition_point(|&(n, _)| n <= normalized_offset);
        self.boundaries[..end]
            .iter()
            .rev()
            .take_while(|&&(n, _)| n == normalized_offset)
            .map(|&(_, o)| o)
            .max()
    }

    /// The original offset where a match ending at `normalized_offset` ends:
    /// the earliest aligned position, so trailing whitespace is kept.
    fn end_offset(&self, normalized_offset: usize) -> Option<usize> {
        let start = self
            .boundaries
            .partition_point(|&(n, _)| n < normalized_offset);
        self.boundaries[start..]
            .iter()
            .take_while(|&&(n, _)| n == normalized_offset)
            .map(|&(_, o)| o)
            .min()
    }
}

/// Outcome of locating one edit's `oldText` in the original content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextMatch {
    /// The original-content span `[start, end)` to replace.
    Unique {
        start: usize,
        end: usize,
    },
    NotFound,
    Duplicate(usize),
}

/// Find `old_text` in `content`. An exact match takes precedence, and its
/// uniqueness is judged among exact matches only; only when there is none is
/// the text matched (and counted) in fuzzy-normalized space, with the match
/// mapped back to an original-content span.
fn find_text(content: &str, old_text: &str, fuzzy: &mut Option<FuzzyIndex>) -> TextMatch {
    match content.matches(old_text).count() {
        0 => {}
        1 => {
            let start = content.find(old_text).unwrap_or_default();
            return TextMatch::Unique {
                start,
                end: start + old_text.len(),
            };
        }
        occurrences => return TextMatch::Duplicate(occurrences),
    }

    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if fuzzy_old_text.is_empty() {
        return TextMatch::NotFound;
    }
    let index = fuzzy.get_or_insert_with(|| FuzzyIndex::new(content));
    let mut found = index.normalized.match_indices(&fuzzy_old_text);
    let Some((normalized_start, _)) = found.next() else {
        return TextMatch::NotFound;
    };
    let others = found.count();
    if others > 0 {
        return TextMatch::Duplicate(others + 1);
    }
    let span = index
        .start_offset(normalized_start)
        .zip(index.end_offset(normalized_start + fuzzy_old_text.len()));
    match span {
        Some((start, end))
            if start <= end
                && normalize_for_fuzzy_match(&content[start..end]) == fuzzy_old_text =>
        {
            TextMatch::Unique { start, end }
        }
        // A boundary inside an NFKC expansion (or a cluster that does not
        // normalize on its own) has no original offset to splice at.
        _ => TextMatch::NotFound,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
}

fn get_not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        return format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        );
    }
    format!(
        "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
    )
}

fn get_duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        return format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        );
    }
    format!(
        "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
    )
}

fn get_empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        return format!("oldText must not be empty in {path}.");
    }
    format!("edits[{edit_index}].oldText must not be empty in {path}.")
}

fn get_no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        return format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        );
    }
    format!("No changes made to {path}. The replacements produced identical content.")
}

/// Apply one or more exact-text replacements to LF-normalized content: all edits
/// are matched against the same original content and applied in reverse order so
/// offsets remain stable. A fuzzy edit is located in fuzzy-normalized space but
/// replaces only its own span of the original, so `base_content` is always the
/// original content and the diff shows exactly what is written.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(get_empty_old_text_error(path, i, normalized_edits.len()));
        }
    }

    let mut fuzzy = None;
    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        match find_text(normalized_content, &edit.old_text, &mut fuzzy) {
            TextMatch::Unique { start, end } => matched_edits.push(MatchedEdit {
                edit_index: i,
                match_index: start,
                match_length: end - start,
                new_text: edit.new_text.clone(),
            }),
            TextMatch::NotFound => {
                return Err(get_not_found_error(path, i, normalized_edits.len()));
            }
            TextMatch::Duplicate(occurrences) => {
                return Err(get_duplicate_error(
                    path,
                    i,
                    normalized_edits.len(),
                    occurrences,
                ));
            }
        }
    }

    matched_edits.sort_by_key(|edit| edit.match_index);
    for i in 1..matched_edits.len() {
        let previous = &matched_edits[i - 1];
        let current = &matched_edits[i];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let mut new_content = base_content.clone();
    for edit in matched_edits.iter().rev() {
        new_content = format!(
            "{}{}{}",
            &new_content[..edit.match_index],
            edit.new_text,
            &new_content[edit.match_index + edit.match_length..]
        );
    }

    if base_content == new_content {
        return Err(get_no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStringResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

/// Generate a unified diff string with line numbers and context.
/// Returns both the diff string and the first changed line number (in the new file).
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
    start_line: usize,
) -> DiffStringResult {
    let parts = diff_lines(old_content, new_content);
    let mut output: Vec<String> = Vec::new();

    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = start_line.saturating_sub(1) + old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    let mut old_line_num = start_line;
    let mut new_line_num = start_line;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (i, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        if part.added || part.removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }

            for line in &raw {
                if part.added {
                    let line_num = format!("{new_line_num:>line_num_width$}");
                    output.push(format!("+{line_num} {line}"));
                    new_line_num += 1;
                } else {
                    let line_num = format!("{old_line_num:>line_num_width$}");
                    output.push(format!("-{line_num} {line}"));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_part_is_change =
                i + 1 < parts.len() && (parts[i + 1].added || parts[i + 1].removed);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in &raw {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading_lines = &raw[..context_lines];
                    let trailing_lines = &raw[raw.len() - context_lines..];
                    let skipped_lines = raw.len() - leading_lines.len() - trailing_lines.len();

                    for line in leading_lines {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }

                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;

                    for line in trailing_lines {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown_lines = &raw[..context_lines.min(raw.len())];
                let skipped_lines = raw.len() - shown_lines.len();

                for line in shown_lines {
                    let line_num = format!("{old_line_num:>line_num_width$}");
                    output.push(format!(" {line_num} {line}"));
                    old_line_num += 1;
                    new_line_num += 1;
                }

                if skipped_lines > 0 {
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }
            } else if has_trailing_change {
                let skipped_lines = raw.len().saturating_sub(context_lines);
                if skipped_lines > 0 {
                    output.push(format!(" {:>line_num_width$} ...", ""));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }

                for line in raw.iter().skip(skipped_lines) {
                    let line_num = format!("{old_line_num:>line_num_width$}");
                    output.push(format!(" {line_num} {line}"));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    DiffStringResult {
        diff: output.join("\n"),
        first_changed_line,
    }
}

impl Default for DiffStringContext {
    fn default() -> Self {
        Self {
            context_lines: 4,
            start_line: 1,
        }
    }
}

/// Optional parameters for [`generate_diff_string`].
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DiffStringContext {
    pub context_lines: usize,
    pub start_line: usize,
}

/// [`generate_diff_string`] with TS defaults (4 context lines, start line 1).
pub fn generate_diff_string_default(old_content: &str, new_content: &str) -> DiffStringResult {
    generate_diff_string(old_content, new_content, 4, 1)
}

// Preview diff computation (reads the file from disk)

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct EditDiffResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum EditDiffOutcome {
    Ok(EditDiffResult),
    Err(String),
}

/// Compute the diff for one or more edit operations without applying them.
/// Used for preview rendering in the TUI before the tool executes.
#[allow(dead_code)]
pub fn compute_edits_diff(path: &str, edits: &[Edit], cwd: &str) -> EditDiffOutcome {
    let absolute_path = resolve_to_cwd(path, cwd);

    // access(R_OK) probe with Node-style error codes.
    if let Err(code) = access_readable(&absolute_path) {
        return EditDiffOutcome::Err(format!("Could not edit file: {path}. Error code: {code}."));
    }

    let raw_content = match std::fs::read(&absolute_path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => return EditDiffOutcome::Err(format!("Could not edit file: {path}. {err}.")),
    };
    apply_edits_with_diff(&raw_content, edits, path)
        .map_or_else(EditDiffOutcome::Err, EditDiffOutcome::Ok)
}

/// Strip the BOM, normalize endings, apply the edits, and generate the diff.
#[allow(dead_code)]
pub fn apply_edits_with_diff(
    raw_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<EditDiffResult, String> {
    let (_, content) = strip_bom(raw_content);
    let normalized_content = normalize_to_lf(content);
    let applied = apply_edits_to_normalized_content(&normalized_content, edits, path)?;
    let diff = generate_diff_string_default(&applied.base_content, &applied.new_content);
    Ok(EditDiffResult {
        diff: diff.diff,
        first_changed_line: diff.first_changed_line,
    })
}

/// Map an io error to the errno name Node exposes as `error.code`.
pub fn errno_name(err: &std::io::Error) -> String {
    if let Some(raw) = err.raw_os_error() {
        return match raw {
            1 => "EPERM".to_string(),
            2 => "ENOENT".to_string(),
            13 => "EACCES".to_string(),
            20 => "ENOTDIR".to_string(),
            21 => "EISDIR".to_string(),
            30 => "EROFS".to_string(),
            36 => "ENAMETOOLONG".to_string(),
            40 => "ELOOP".to_string(),
            other => format!("E{other}"),
        };
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => "ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "EACCES".to_string(),
        _ => "EUNKNOWN".to_string(),
    }
}

#[allow(dead_code)]
fn access_readable(path: &str) -> Result<(), String> {
    crate::platform::perms::is_readable(Path::new(path)).map_err(|err| errno_name(&err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old_text: &str, new_text: &str) -> Edit {
        Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    // Upstream #1648: a unique exact match is not rejected because another
    // span only matches after fuzzy normalization (`it's` vs `it’s`).
    #[test]
    fn unique_exact_match_wins_over_normalized_duplicates() {
        let content = "it\u{2019}s here\nit's here\n";
        let applied =
            apply_edits_to_normalized_content(content, &[edit("it's here", "gone")], "f.txt")
                .unwrap();
        assert_eq!(
            applied,
            AppliedEditsResult {
                base_content: content.to_string(),
                new_content: "it\u{2019}s here\ngone\n".to_string(),
            }
        );
    }

    #[test]
    fn duplicates_are_still_rejected_in_their_own_space() {
        let exact = apply_edits_to_normalized_content("ab\nab\n", &[edit("ab", "x")], "f.txt");
        assert_eq!(
            exact,
            Err(get_duplicate_error("f.txt", 0, 1, 2)),
            "genuine exact duplicates"
        );
        let fuzzy = apply_edits_to_normalized_content(
            "it\u{2019}s\nit\u{2018}s\n",
            &[edit("it's", "x")],
            "f.txt",
        );
        assert_eq!(
            fuzzy,
            Err(get_duplicate_error("f.txt", 0, 1, 2)),
            "fuzzy duplicates"
        );
    }

    // Upstream #654/#657: a fuzzy match splices the replacement into the
    // original content; nothing outside the matched span is normalized.
    #[test]
    fn fuzzy_edit_leaves_bytes_outside_the_match_untouched() {
        let content =
            "keep  \n\u{201C}quoted\u{201D} it\u{2019}s \u{2014} ok\n\u{BD} \u{FB01}le  \n";
        let applied = apply_edits_to_normalized_content(
            content,
            &[edit("\"quoted\" it's - ok", "plain")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(
            applied,
            AppliedEditsResult {
                base_content: content.to_string(),
                new_content: "keep  \nplain\n\u{BD} \u{FB01}le  \n".to_string(),
            }
        );
    }

    #[test]
    fn fuzzy_and_exact_edits_mix_against_the_original_content() {
        let content = "a\u{2014}b  \ncafe\u{301} \u{2018}x\u{2019}\ntail\u{A0} \n";
        let applied = apply_edits_to_normalized_content(
            content,
            &[edit("tail", "TAIL"), edit("caf\u{E9} 'x'", "menu")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(
            applied,
            AppliedEditsResult {
                base_content: content.to_string(),
                new_content: "a\u{2014}b  \nmenu\nTAIL\u{A0} \n".to_string(),
            }
        );
    }

    // A fuzzy match ending on the last visible character keeps that
    // line's trailing whitespace; including the newline consumes it.
    #[test]
    fn fuzzy_match_trailing_whitespace_semantics() {
        let content = "alpha \u{2014}   \nbeta\n";
        let without_newline =
            apply_edits_to_normalized_content(content, &[edit("alpha -", "A")], "f.txt").unwrap();
        assert_eq!(without_newline.new_content, "A   \nbeta\n");
        let with_newline =
            apply_edits_to_normalized_content(content, &[edit("alpha -\n", "A\n")], "f.txt")
                .unwrap();
        assert_eq!(with_newline.new_content, "A\nbeta\n");
    }

    // A fuzzy match that starts or ends inside an NFKC expansion has no
    // original-content boundary, so it is reported as not found.
    #[test]
    fn fuzzy_match_inside_an_nfkc_expansion_is_not_found() {
        let result = apply_edits_to_normalized_content(
            "x\u{BD}y \u{2019}\n",
            &[edit("\u{2044}2y '", "z")],
            "f.txt",
        );
        assert_eq!(result, Err(get_not_found_error("f.txt", 0, 1)));
    }
}
