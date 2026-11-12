//! Slop Guard — automated quality checks for LLM-generated code.
//!
//! Runs after every file-modifying tool (Write/Edit/MultiEdit)
//! and surfaces findings to the model via system-reminder so it can
//! self-correct before moving to the next step.
//!
//! Checks:
//! 1. Duplication detection (5+ line sliding-window hash match)
//! 2. Dead code (cargo check --message-format=json `dead_code` warnings)
//! 3. Architectural coherence (pattern divergence heuristics)
//! 4. Churn tracking (git log frequency per file)
//! 5. Complexity budget (LOC + nesting depth per function)
//! 6. Test quality (implementation-coupling heuristics)
//! 7. Rust security profile (unsafe/UB/codegen patterns routed to proof oracles)

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Whether the per-file checks look at the whole file or only the lines the
/// current edit changed. Per-edit callers want `Changed` (measured to cut ~90%
/// of the noise — findings about untouched code); an on-demand full audit wants
/// `Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlopScope {
    /// Scan the whole file (every function, every window).
    Full,
    /// Only report findings on lines this edit added (needs the old content;
    /// falls back to `Full` for a new file with no previous version).
    #[default]
    Changed,
}

/// Report produced by `run_all_checks`.
pub struct SlopReport {
    pub has_findings: bool,
    pub findings: Vec<SlopFinding>,
}

/// A single quality finding.
pub struct SlopFinding {
    pub rule: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<usize>,
}

/// The research lane a finding should enter before an LLM tries to "fix" it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResearchFindingClass {
    MiriProvable,
    SanitizerProvable,
    FuzzerCandidate,
    LlvmDiffCandidate,
    IncrCacheCandidate,
    UnsafeContractCandidate,
    StaticReviewCandidate,
}

impl ResearchFindingClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MiriProvable => "miri_provable",
            Self::SanitizerProvable => "sanitizer_provable",
            Self::FuzzerCandidate => "fuzzer_candidate",
            Self::LlvmDiffCandidate => "llvm_diff_candidate",
            Self::IncrCacheCandidate => "incr_cache_candidate",
            Self::UnsafeContractCandidate => "unsafe_contract_candidate",
            Self::StaticReviewCandidate => "static_review_candidate",
        }
    }
}

impl std::fmt::Display for ResearchFindingClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Concrete proof artifact the agent should produce or ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProofArtifact {
    MiriTest,
    SanitizerRun,
    FuzzReproducer,
    LlvmDiff,
    IncrementalCacheBundle,
    UnsafeContractReview,
    StaticReview,
}

impl ProofArtifact {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MiriTest => "focused Miri test",
            Self::SanitizerRun => "sanitizer command",
            Self::FuzzReproducer => "minimized fuzz/PoC input",
            Self::LlvmDiff => "LLVM/rustc differential repro",
            Self::IncrementalCacheBundle => "incremental cache + before/after diff",
            Self::UnsafeContractReview => "unsafe contract review",
            Self::StaticReview => "static code review",
        }
    }
}

impl std::fmt::Display for ProofArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SlopFinding {
    /// Classify this finding into the research lane that should validate it.
    #[must_use]
    pub fn research_class(&self) -> ResearchFindingClass {
        match self.rule.as_str() {
            "miri_provable" => ResearchFindingClass::MiriProvable,
            "sanitizer_provable" => ResearchFindingClass::SanitizerProvable,
            "fuzzer_candidate" => ResearchFindingClass::FuzzerCandidate,
            "llvm_diff_candidate" => ResearchFindingClass::LlvmDiffCandidate,
            "incr_cache_candidate" => ResearchFindingClass::IncrCacheCandidate,
            "unsafe_contract_candidate" => ResearchFindingClass::UnsafeContractCandidate,
            "security_regression" | "guardrail_removal" => {
                ResearchFindingClass::UnsafeContractCandidate
            }
            _ => ResearchFindingClass::StaticReviewCandidate,
        }
    }

    /// Suggested concrete artifact(s) to request before treating a finding as fixed.
    #[must_use]
    pub fn suggested_proof_artifacts(&self) -> Vec<ProofArtifact> {
        match self.research_class() {
            ResearchFindingClass::MiriProvable => {
                vec![ProofArtifact::MiriTest, ProofArtifact::UnsafeContractReview]
            }
            ResearchFindingClass::SanitizerProvable => vec![ProofArtifact::SanitizerRun],
            ResearchFindingClass::FuzzerCandidate => {
                vec![ProofArtifact::FuzzReproducer, ProofArtifact::MiriTest]
            }
            ResearchFindingClass::LlvmDiffCandidate => vec![ProofArtifact::LlvmDiff],
            ResearchFindingClass::IncrCacheCandidate => {
                vec![ProofArtifact::IncrementalCacheBundle]
            }
            ResearchFindingClass::UnsafeContractCandidate => {
                vec![
                    ProofArtifact::UnsafeContractReview,
                    ProofArtifact::StaticReview,
                ]
            }
            ResearchFindingClass::StaticReviewCandidate => Vec::new(),
        }
    }
}

/// New-file line numbers (1-based) whose trimmed content is not present in the
/// previous version — a cheap "what did this edit add" set, used to scope the
/// whole-file checks to the change. Returns `None` when there is no previous
/// version (a freshly created file: everything is new, so nothing is scoped out).
///
/// Measured motivation (2026-11-12, re-scan of 218 real jfc edits): unscoped,
/// 97% of complexity findings and 62% of duplication findings were about code
/// the edit never touched.
fn changed_lines(old: Option<&str>, new: &str) -> Option<HashSet<usize>> {
    let old = old?;
    let mut old_counts: HashMap<&str, i32> = HashMap::new();
    for l in old.lines() {
        *old_counts.entry(l.trim()).or_default() += 1;
    }
    let mut changed = HashSet::new();
    for (idx, l) in new.lines().enumerate() {
        let t = l.trim();
        let c = old_counts.entry(t).or_default();
        if *c > 0 {
            *c -= 1; // matched an unchanged line
        } else {
            changed.insert(idx + 1); // genuinely new content
        }
    }
    Some(changed)
}

/// Keep a finding only when it falls on a changed line, or when there is no
/// change set (new file / direct call). Findings without a line always pass.
fn on_changed_line(f: &SlopFinding, changed: Option<&HashSet<usize>>) -> bool {
    match (changed, f.line) {
        (Some(set), Some(l)) => set.contains(&l),
        _ => true,
    }
}

// ─── 1. Duplication Detection ───────────────────────────────────────────

const DUP_WINDOW: usize = 5;

/// Hash a normalized 5-line window (trimmed, lowercased).
fn hash_window(lines: &[&str]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for line in lines {
        line.trim().hash(&mut hasher);
    }
    hasher.finish()
}

/// Return true when `file_path` or `file_content` identifies a test scope:
/// test code is allowed to use `unwrap`/`expect`, long descriptive test
/// names, and other patterns that would otherwise be flagged. Separately
/// from the inline `#[cfg(test)] mod tests { ... }` convention this picks
/// up the "whole file is a test module" case where a parent declares
/// `mod tests;` and the sibling file has no `#[cfg(test)]` attribute of
/// its own.
///
/// Signals, in order:
/// - filename is `test.rs`/`tests.rs` or ends in `_test.rs`/`_tests.rs`
/// - path has a `tests/` directory component (cargo integration-test
///   convention)
/// - content starts with `#![cfg(test)]` (inner attribute, whole file)
/// - content has any `#[cfg(test)]` (inline test module, pre-existing signal)
fn is_test_scope_file(file_path: Option<&Path>, file_content: &str) -> bool {
    if let Some(path) = file_path {
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name == "test.rs"
                || name == "tests.rs"
                || name.ends_with("_test.rs")
                || name.ends_with("_tests.rs"))
        {
            return true;
        }
        if path.components().any(|c| c.as_os_str() == "tests") {
            return true;
        }
    }
    let trimmed = file_content.trim_start();
    if trimmed.starts_with("#![cfg(test)]") {
        return true;
    }
    file_content.contains("#[cfg(test)]")
}

/// Return `source` rewritten so that every byte inside a string literal,
/// raw string, char literal, line comment, or block comment is replaced
/// with ' '. Newlines are preserved so line numbers survive and `'{'`
/// / `'}'` / `"{"` /  `"\n}"` no longer trip a structural brace counter.
///
/// Block comments nest as rustc nests them; raw strings are closed by the
/// matching `"` with the right number of trailing `#`s.
#[expect(
    clippy::too_many_lines,
    reason = "one flat state-machine match reads better than scattered mini-helpers"
)]
fn code_only(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum Lex {
        Normal,
        LineCmt,
        BlockCmt(u32),
        Str,
        StrEsc,
        RawStr(u32),
    }
    let bytes = source.as_bytes();
    let mut out: Vec<u8> = vec![b' '; bytes.len()];
    let mut state = Lex::Normal;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match state {
            Lex::Normal => {
                if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    state = Lex::LineCmt;
                    i += 2;
                    continue;
                }
                if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    state = Lex::BlockCmt(1);
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    state = Lex::Str;
                    i += 1;
                    continue;
                }
                if b == b'r'
                    && i + 1 < bytes.len()
                    && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#')
                {
                    let mut hashes = 0u32;
                    let mut k = i + 1;
                    while k < bytes.len() && bytes[k] == b'#' {
                        hashes += 1;
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b'"' {
                        state = Lex::RawStr(hashes);
                        i = k + 1;
                        continue;
                    }
                }
                if b == b'\'' {
                    if let Some(close_off) = char_literal_end(&bytes[i + 1..]) {
                        // Char literal: consume everything up to and including
                        // the closing `'`, dropping its bytes so a `'{'`
                        // doesn't bump the brace counter.
                        i += 1 + close_off + 1;
                        continue;
                    }
                    // Lifetime (`'a`), not a char literal — leave it visible.
                    out[i] = b;
                    i += 1;
                    continue;
                }
                out[i] = b;
                i += 1;
            }
            Lex::LineCmt => {
                if b == b'\n' {
                    out[i] = b'\n';
                    state = Lex::Normal;
                }
                i += 1;
            }
            Lex::BlockCmt(d) => {
                if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    state = Lex::BlockCmt(d + 1);
                    i += 2;
                    continue;
                }
                if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    state = if d == 1 {
                        Lex::Normal
                    } else {
                        Lex::BlockCmt(d - 1)
                    };
                    i += 2;
                    continue;
                }
                if b == b'\n' {
                    out[i] = b'\n';
                }
                i += 1;
            }
            Lex::Str => {
                match b {
                    b'\\' => state = Lex::StrEsc,
                    b'"' => state = Lex::Normal,
                    b'\n' => out[i] = b'\n',
                    _ => {}
                }
                i += 1;
            }
            Lex::StrEsc => {
                if b == b'\n' {
                    out[i] = b'\n';
                }
                state = Lex::Str;
                i += 1;
            }
            Lex::RawStr(h) => {
                if b == b'"' {
                    // Need `h` trailing `#` to close.
                    let mut seen = 0u32;
                    let mut k = i + 1;
                    while k < bytes.len() && bytes[k] == b'#' && seen < h {
                        seen += 1;
                        k += 1;
                    }
                    if seen == h {
                        state = Lex::Normal;
                        i = k;
                        continue;
                    }
                }
                if b == b'\n' {
                    out[i] = b'\n';
                }
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// Return the byte offset (relative to the opening `'`) of the closing
/// `'` of a char literal starting at `rest[0]`, or `None` if the opening
/// `'` is part of a lifetime (`'a`) rather than a char literal. Covers
/// `'x'`, `'\n'`, `'\\'`, `'\''`, `'\0'`, `'\xHH'`, `'\u{...}'`, and
/// multi-byte UTF-8 chars.
fn char_literal_end(rest: &[u8]) -> Option<usize> {
    if rest.is_empty() {
        return None;
    }
    if rest[0] == b'\\' {
        if rest.len() < 3 {
            return None;
        }
        match rest[1] {
            b'x' => {
                // `\xHH` → 4 bytes before closing `'`.
                if rest.len() >= 5 && rest[4] == b'\'' {
                    Some(4)
                } else {
                    None
                }
            }
            b'u' => {
                // `\u{HHHH..}` → variable length.
                if rest[2] != b'{' {
                    return None;
                }
                let mut k = 3;
                while k < rest.len() && rest[k] != b'}' {
                    k += 1;
                }
                if k + 1 < rest.len() && rest[k + 1] == b'\'' {
                    Some(k + 1)
                } else {
                    None
                }
            }
            _ => {
                // Simple escape: `\n`, `\t`, `\\`, `\'`, `\"`, `\0`, etc.
                if rest[2] == b'\'' { Some(2) } else { None }
            }
        }
    } else {
        // Non-escape: a single char (one Unicode scalar).
        let char_len = utf8_char_len(rest[0]);
        if rest.len() > char_len && rest[char_len] == b'\'' {
            Some(char_len)
        } else {
            // Opening `'` isn't followed by a scalar + `'`; treat as a
            // lifetime (e.g. `'a`), not a char literal.
            None
        }
    }
}

fn utf8_char_len(first: u8) -> usize {
    // ASCII and continuation / invalid leading bytes read as one byte; a
    // leading byte with 2/3/4 high bits set names a 2/3/4-byte codepoint.
    if first < 0xC0 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}

/// Is `line` substantive enough to count toward a 5-line "duplicated"
/// block? Bare closing braces, attribute lines, and plain `use`/`mod`
/// declarations are all shapes LLMs legitimately re-emit without
/// duplicating any logic, so a 5-line block that is only such lines is
/// never a real duplicate.
fn is_substantive_dup_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < 4 {
        return false;
    }
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return false;
    }
    if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
        return false;
    }
    let is_import_or_mod = trimmed.starts_with("use ")
        || trimmed.starts_with("pub use ")
        || trimmed.starts_with("pub(crate) use ")
        || trimmed.starts_with("pub(super) use ")
        || trimmed.starts_with("mod ")
        || trimmed.starts_with("pub mod ")
        || trimmed.starts_with("pub(crate) mod ")
        || trimmed.starts_with("extern crate ")
        || trimmed.starts_with("extern ");
    if is_import_or_mod {
        return false;
    }
    let non_punct = trimmed
        .chars()
        .filter(|c| !c.is_ascii_punctuation() && !c.is_whitespace())
        .count();
    if non_punct < 3 {
        return false;
    }
    let ident_chars = trimmed
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .count();
    ident_chars >= 3
}

/// Find 5+ line blocks in `new_content` that already exist in other files
/// under `cwd`. Returns (`existing_file`, `existing_line`, `matched_text_preview`).
#[must_use]
pub fn check_duplication(new_content: &str, file_path: &Path, cwd: &Path) -> Vec<SlopFinding> {
    check_duplication_scoped(new_content, file_path, cwd, None)
}

/// Duplication check, optionally restricted to windows that overlap a set of
/// changed line numbers (1-based). With `changed = None` every window counts
/// (the original whole-file behavior). With a set, only blocks the current edit
/// *introduced* are flagged — measured to cut ~62% of pre-existing-duplicate noise.
#[must_use]
pub fn check_duplication_scoped(
    new_content: &str,
    file_path: &Path,
    cwd: &Path,
    changed: Option<&HashSet<usize>>,
) -> Vec<SlopFinding> {
    let new_lines: Vec<&str> = new_content.lines().collect();
    if new_lines.len() < DUP_WINDOW {
        return Vec::new();
    }

    // Build hashes for all windows in the new file.
    let mut new_hashes: HashSet<u64> = HashSet::new();
    for (start, window) in new_lines.windows(DUP_WINDOW).enumerate() {
        // Skip windows whose content is mostly boilerplate. The previous
        // "trim.len() > 3" heuristic let in windows of just closing
        // braces, `#[derive(...)]`, bare `use` lines and `#[cfg(test)]`,
        // which clustered 1,296 false positives across one real repo
        // (every Orion decoder's `use serde::Serialize;\n#[derive(...)]`
        // header matched every other decoder's).
        let substantive = window.iter().filter(|l| is_substantive_dup_line(l)).count();
        if substantive < 3 {
            continue;
        }
        // When scoped, only consider windows that overlap a changed line.
        if let Some(set) = changed {
            let overlaps = (start + 1..=start + DUP_WINDOW).any(|ln| set.contains(&ln));
            if !overlaps {
                continue;
            }
        }
        new_hashes.insert(hash_window(window));
    }

    if new_hashes.is_empty() {
        return Vec::new();
    }

    let mut findings = Vec::new();
    let canonical_new = file_path.canonicalize().ok();

    // Walk .rs files in workspace looking for matches.
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .git_ignore(true)
        .max_depth(Some(12))
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Skip self.
        if let Some(ref cn) = canonical_new
            && path.canonicalize().ok().as_ref() == Some(cn)
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < DUP_WINDOW {
            continue;
        }
        for (i, window) in lines.windows(DUP_WINDOW).enumerate() {
            let substantive = window.iter().filter(|l| is_substantive_dup_line(l)).count();
            if substantive < 3 {
                continue;
            }
            let h = hash_window(window);
            if new_hashes.contains(&h) {
                let preview = window[0..2].join(" / ");
                // Slice on a char boundary — `preview` is arbitrary
                // source code (file paths in module separators, em-dashes
                // in comments, …) and a fixed-byte cap blew up at runtime
                // when a multi-byte glyph straddled byte 80
                // (`thread 'tokio-rt-worker' panicked at slop_guard.rs:114:
                //  end byte index 80 is not a char boundary; it is inside
                //  '─' (bytes 79..82)`). `floor_char_boundary` rounds DOWN
                // to the nearest char boundary at or before the requested
                // index, so we get at most 80 bytes of preview without
                // ever splitting a UTF-8 sequence.
                let preview_slice: &str = if preview.len() > 80 {
                    &preview[..preview.floor_char_boundary(80)]
                } else {
                    &preview
                };
                findings.push(SlopFinding {
                    rule: "duplication".into(),
                    message: format!(
                        "5+ line block already exists at {}:{} — consider reusing: `{}`",
                        path.strip_prefix(cwd).unwrap_or(path).display(),
                        i + 1,
                        preview_slice
                    ),
                    file: Some(path.strip_prefix(cwd).unwrap_or(path).display().to_string()),
                    line: Some(i + 1),
                });
                // One finding per file is enough.
                break;
            }
        }

        // Cap total duplication findings.
        if findings.len() >= 5 {
            break;
        }
    }

    findings
}

// ─── 2. Dead Code Detection ─────────────────────────────────────────────

/// Run `cargo check --message-format=json` and extract `dead_code` warnings.
/// Returns quickly — uses cached incremental compilation.
#[must_use]
pub fn check_dead_code(cwd: &Path) -> Vec<SlopFinding> {
    let output = std::process::Command::new("cargo")
        .args(["check", "--message-format=json", "-q"])
        .current_dir(cwd)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut findings = Vec::new();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg.get("reason").and_then(|v| v.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = msg.get("message") else {
            continue;
        };
        let code = message
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if code != "dead_code" {
            continue;
        }
        let text = message
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let span = message
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first());
        let file = span
            .and_then(|s| s.get("file_name"))
            .and_then(|f| f.as_str())
            .unwrap_or("?");
        let line_num = span
            .and_then(|s| s.get("line_start"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;

        findings.push(SlopFinding {
            rule: "dead_code".into(),
            message: format!("{file}:{line_num}: {text}"),
            file: Some(file.to_string()),
            line: Some(line_num),
        });

        if findings.len() >= 10 {
            break;
        }
    }
    findings
}

// ─── 3. Architectural Coherence ─────────────────────────────────────────

/// Check for common pattern divergences in Rust code.
#[must_use]
pub fn check_coherence(file_content: &str, file_path: &Path) -> Vec<SlopFinding> {
    let mut findings = Vec::new();
    // Treat external test modules (sibling `tests.rs`, `_tests.rs`, files
    // under `tests/`) as a test scope too — the pre-existing
    // `file_content.contains("#[cfg(test)]")` only caught the inline
    // `#[cfg(test)] mod tests { ... }` convention and flagged every
    // `unwrap()` in `tests.rs` files as production code.
    let is_test = is_test_scope_file(Some(file_path), file_content);
    let is_lib = file_path
        .to_str()
        .map(|s| !s.contains("/main.rs") && !s.contains("/bin/"))
        .unwrap_or(true);

    // Check: anyhow::Result in library code (should use typed errors). Attach the
    // first anyhow line so this is scoped to edits that actually touch anyhow,
    // instead of re-reporting the whole-file count on every unrelated edit.
    if is_lib && !is_test {
        let anyhow_count = file_content.matches("anyhow::Result").count()
            + file_content.matches("anyhow::Error").count()
            + file_content.matches("anyhow::bail!").count();
        if anyhow_count > 3 {
            let first_line = file_content
                .lines()
                .position(|l| {
                    l.contains("anyhow::Result")
                        || l.contains("anyhow::Error")
                        || l.contains("anyhow::bail!")
                })
                .map(|i| i + 1);
            findings.push(SlopFinding {
                rule: "coherence".into(),
                message: format!(
                    "Library code uses anyhow {anyhow_count} times — consider typed errors (thiserror/snafu) for public API boundaries"
                ),
                file: None,
                line: first_line,
            });
        }
    }

    // Check: unwrap() in non-test production code.
    if !is_test {
        let unwrap_lines: Vec<usize> = file_content
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim();
                (t.contains(".unwrap()") || t.contains(".expect("))
                    && !t.starts_with("//")
                    && !t.starts_with('*')
            })
            .map(|(i, _)| i + 1)
            .collect();
        if unwrap_lines.len() > 5 {
            findings.push(SlopFinding {
                rule: "coherence".into(),
                message: format!(
                    "{} unwrap()/expect() calls in non-test code (lines: {:?}…) — consider proper error handling",
                    unwrap_lines.len(),
                    &unwrap_lines[..unwrap_lines.len().min(5)]
                ),
                file: None,
                line: Some(unwrap_lines[0]),
            });
        }
    }

    // Check: `todo!()` or `unimplemented!()` that shouldn't be committed.
    for (i, line) in file_content.lines().enumerate() {
        let t = line.trim();
        if (t.contains("todo!()") || t.contains("unimplemented!()")) && !t.starts_with("//") {
            findings.push(SlopFinding {
                rule: "coherence".into(),
                message: format!(
                    "Line {}: contains todo!()/unimplemented!() — should not be committed",
                    i + 1
                ),
                file: None,
                line: Some(i + 1),
            });
        }
    }

    findings
}

// ─── 4. Churn Tracking ──────────────────────────────────────────────────

/// Check git log for files with high edit frequency in the last 7 days.
#[must_use]
pub fn check_churn(cwd: &Path) -> Vec<SlopFinding> {
    let output = std::process::Command::new("git")
        .args([
            "log",
            "--since=7 days ago",
            "--name-only",
            "--pretty=format:",
        ])
        .current_dir(cwd)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut counts: HashMap<&str, u32> = HashMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        *counts.entry(line).or_default() += 1;
    }

    let mut findings = Vec::new();
    let mut high_churn: Vec<(&&str, &u32)> = counts.iter().filter(|&(_, &v)| v > 5).collect();
    high_churn.sort_by(|a, b| b.1.cmp(a.1));

    for (file, count) in high_churn.into_iter().take(5) {
        findings.push(SlopFinding {
            rule: "churn".into(),
            message: format!(
                "{file} edited {count} times in 7 days — consider a consolidation/refactoring pass"
            ),
            file: Some(file.to_string()),
            line: None,
        });
    }
    findings
}

// ─── 5. Complexity Budget ───────────────────────────────────────────────

const MAX_FUNCTION_LOC: usize = 80;
const MAX_NESTING_DEPTH: usize = 5;

/// Check functions for excessive length or nesting depth.
/// Detect borrow/clone slop — wasteful `.clone()`/`.to_owned()` that allocates
/// where a borrow or a `Copy` would do. The patterns and false-positive
/// estimates come from a survey of rust-lang Zulip clippy discussions; only the
/// Tier-1 (<5% FP) patterns that map to real clippy lints are flagged:
///   • `.clone()` on a primitive literal/Copy expression (`clippy::clone_on_copy`)
///   • `&&x.clone()` double-ref clone (`clippy::clone_double_ref`)
///   • `.to_string()` on a value already `String`, `.to_vec()` on a `Vec`,
///     `.to_path_buf()` on a `PathBuf` (`clippy::implicit_clone`)
/// Line-based + conservative: when in doubt it stays silent. Production code
/// only (skips the `#[cfg(test)]` region, where intentional clones are common).
#[must_use]
pub fn check_borrow_clone(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    // Production region only — tests legitimately clone fixtures a lot.
    let region = match file_content.find("#[cfg(test)]") {
        Some(idx) => &file_content[..idx],
        None => file_content,
    };

    // Numeric/Copy primitive cloned directly, e.g. `count.clone()` where count
    // is an integer, or `x.clone()` immediately after a Copy binding. We can't
    // do type inference line-based, so we only flag the unambiguous shapes:
    // a clone on a numeric/bool/char literal, or on a `*deref`-of-Copy.
    for (i, line) in region.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with('*') || t.starts_with("/*") {
            continue;
        }
        // Skip lines that explicitly opt out.
        if t.contains("// allow-clone") || t.contains("clippy::clone_on_copy") {
            continue;
        }

        // 1) Double-ref clone: `&&expr.clone()` or `(&&expr).clone()` — clones a
        //    reference, almost never intended (clippy::clone_double_ref).
        if t.contains("&&")
            && t.contains(".clone()")
            && let Some(pos) = t.find("&&")
        {
            let after = &t[pos + 2..];
            if after.contains(".clone()") && !after.contains("&&") {
                findings.push(SlopFinding {
                        rule: "borrow_clone".into(),
                        message: format!(
                            "Line {}: `&&…clone()` clones a double reference (clippy::clone_double_ref) — clone the value, not the reference",
                            i + 1
                        ),
                        file: None,
                        line: Some(i + 1),
                    });
            }
        }

        // 2) clone_on_copy: a numeric/bool/char LITERAL `.clone()`, e.g.
        //    `0.clone()`, `true.clone()`, `'x'.clone()`. Unambiguous and rare,
        //    but it shows up in generated code.
        if let Some(c) = clone_on_literal(t) {
            findings.push(SlopFinding {
                rule: "borrow_clone".into(),
                message: format!(
                    "Line {}: `.clone()` on a Copy {c} (clippy::clone_on_copy) — drop the clone, Copy types are copied",
                    i + 1
                ),
                file: None,
                line: Some(i + 1),
            });
        }

        // 3) implicit_clone: `.to_string()` on a String / `.to_vec()` on a Vec /
        //    `.to_path_buf()` on a PathBuf — only when the receiver is a clear
        //    String/Vec/Path *literal-ish* token to keep FP low. We flag the
        //    high-confidence form `<ident>.clone().to_string()` and
        //    `String::from(x).to_string()` which are always redundant.
        if t.contains(".clone().to_string()")
            || t.contains(".to_string().to_string()")
            || t.contains(".to_vec().to_vec()")
        {
            findings.push(SlopFinding {
                rule: "borrow_clone".into(),
                message: format!(
                    "Line {}: redundant double conversion (e.g. `.clone().to_string()`) — one allocation suffices",
                    i + 1
                ),
                file: None,
                line: Some(i + 1),
            });
        }
    }

    findings.truncate(8);
    findings
}

/// If `line` contains a `.clone()` applied directly to a Copy *literal*
/// (numeric like `42`/`1.0`, `true`/`false`, or a `'c'` char literal), return a
/// human label for the kind. Conservative: only matches a literal immediately
/// before `.clone()`, never an identifier (which could be a non-Copy type).
fn clone_on_literal(line: &str) -> Option<&'static str> {
    let idx = line.find(".clone()")?;
    let before = line[..idx].trim_end();
    let last = before
        .rsplit(|c: char| c.is_whitespace() || "([{,=".contains(c))
        .next()?;
    if last.is_empty() {
        return None;
    }
    if last == "true" || last == "false" {
        return Some("bool");
    }
    // char literal: 'x' (length 3) or escaped '\n' (length 4)
    if last.starts_with('\'') && last.ends_with('\'') && last.len() >= 3 {
        return Some("char");
    }
    // numeric literal: all chars digits / . / _ / type suffix like i32/u8/f64
    let numeric_core: String = last
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '_')
        .collect();
    if !numeric_core.is_empty() && numeric_core.chars().any(|c| c.is_ascii_digit()) {
        let suffix = &last[numeric_core.len()..];
        if suffix.is_empty()
            || matches!(
                suffix,
                "i8" | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "isize"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "usize"
                    | "f32"
                    | "f64"
            )
        {
            return Some("number");
        }
    }
    None
}

#[must_use]
pub fn check_complexity(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();
    let lines: Vec<&str> = file_content.lines().collect();
    // Count braces on a copy of the source with string literals, char
    // literals, line comments, and block comments blanked out. The naive
    // `for ch in l.chars()` loop treated `'{'` and `"{"` as structural
    // opens, which inflated `parse_from_reader` in ers-rs `proformex.rs`
    // from its real 28 lines to a reported 886.
    let cleaned = code_only(file_content);
    let cleaned_lines: Vec<&str> = cleaned.lines().collect();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        // Detect function start: `fn name(` or `pub fn name(` or `async fn ...`
        let is_fn = (line.contains("fn ") && line.contains('('))
            && !line.starts_with("//")
            && !line.starts_with('*');

        if is_fn {
            let fn_start = i;
            // Extract function name.
            let name = line
                .split("fn ")
                .nth(1)
                .and_then(|s| s.split('(').next())
                .unwrap_or("?")
                .trim();

            // Count LOC and max nesting depth until matching closing
            // brace, using the lexed (literal/comment-stripped) view.
            let mut depth: i32 = 0;
            let mut max_depth: i32 = 0;
            let mut fn_loc = 0;
            let mut started = false;
            let mut j = i;
            while j < cleaned_lines.len() {
                let l = cleaned_lines[j];
                for ch in l.chars() {
                    if ch == '{' {
                        depth += 1;
                        started = true;
                        if depth > max_depth {
                            max_depth = depth;
                        }
                    } else if ch == '}' {
                        depth -= 1;
                    }
                }
                if started {
                    fn_loc += 1;
                }
                if started && depth == 0 {
                    break;
                }
                j += 1;
            }

            if fn_loc > MAX_FUNCTION_LOC {
                findings.push(SlopFinding {
                    rule: "complexity".into(),
                    message: format!(
                        "`{name}` at line {} is {fn_loc} lines (max {MAX_FUNCTION_LOC}) — consider splitting",
                        fn_start + 1
                    ),
                    file: None,
                    line: Some(fn_start + 1),
                });
            }
            if max_depth as usize > MAX_NESTING_DEPTH {
                findings.push(SlopFinding {
                    rule: "complexity".into(),
                    message: format!(
                        "`{name}` at line {} has nesting depth {max_depth} (max {MAX_NESTING_DEPTH}) — consider early returns or extraction",
                        fn_start + 1
                    ),
                    file: None,
                    line: Some(fn_start + 1),
                });
            }

            i = j + 1;
        } else {
            i += 1;
        }
    }
    findings
}

// ─── 6. Test Quality ────────────────────────────────────────────────────

/// Check for implementation-coupled test patterns.
/// Test names whose shape means "passes by compiling / not panicking", which
/// legitimately carry no assertion.
fn is_compile_smoke_test(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "builds",
        "compiles",
        "is_copyable",
        "is_clone",
        "is_send",
        "is_sync",
        "constructs",
        "default",
        "smoke",
        "roundtrips_without_panic",
        "without_panic",
    ];
    let n = name.to_ascii_lowercase();
    MARKERS.iter().any(|m| n.contains(m))
}

#[must_use]
pub fn check_test_quality(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    // Only check test code.
    let Some(idx) = file_content.find("#[cfg(test)]") else {
        return findings;
    };
    let test_section = &file_content[idx..];
    // Line numbers below are file-relative: the test section starts after this
    // many lines, so add it to each in-section index.
    let base = file_content[..idx].lines().count();

    // Mask string/char/comment content for the brace-depth walk below: a `}`
    // inside a string (`r#"...}..."#`) or char literal (`'}'`) in a test body
    // would otherwise decrement depth to 0 early, ending the function scan
    // before its `assert!` line and producing a spurious "no assertions"
    // finding. `assert`/`fn`/`#[test]` are code, so they survive masking and
    // the substring checks still match. Lines map 1:1, so line numbers align.
    let masked_section = crate::rust_lex::mask_source(test_section);

    // Check: assertions on very long string literals (fragile snapshot tests).
    for (i, line) in test_section.lines().enumerate() {
        let t = line.trim();
        if (t.contains("assert_eq!") || t.contains("assert_ne!")) && t.len() > 200 {
            findings.push(SlopFinding {
                rule: "test_quality".into(),
                message: format!(
                    "Test assertion at line ~{} is >200 chars — fragile string snapshot; consider semantic checks",
                    base + i + 1
                ),
                file: None,
                line: Some(base + i + 1),
            });
        }
    }

    // Check: test functions with no assertions. Walk the *masked* section so
    // braces/`assert` tokens inside string or char literals don't perturb the
    // depth counter (masking maps every char 1:1, so line numbers still align).
    let fn_re = regex::Regex::new(r"fn\s+(\w+)\s*\(").unwrap();
    let mut in_test_fn = false;
    let mut fn_name = String::new();
    let mut fn_start = 0;
    let mut has_assert = false;
    let mut depth: i32 = 0;

    for (i, line) in masked_section.lines().enumerate() {
        if line.contains("#[test]") || line.contains("#[tokio::test]") {
            in_test_fn = true;
            has_assert = false;
            depth = 0;
            continue;
        }
        if in_test_fn
            && depth == 0
            && let Some(m) = fn_re.captures(line)
        {
            fn_name = m.get(1).map(|m| m.as_str().to_owned()).unwrap_or_default();
            fn_start = base + i + 1;
        }
        if in_test_fn {
            for ch in line.chars() {
                if ch == '{' {
                    depth += 1;
                }
                if ch == '}' {
                    depth -= 1;
                }
            }
            if line.contains("assert") || line.contains("panic!") || line.contains("should_panic") {
                has_assert = true;
            }
            if depth == 0 && !fn_name.is_empty() {
                // A compile/smoke test legitimately has no assertion: it passes
                // by building or not panicking (`*_builds`, `*_is_copyable`,
                // `*_compiles`, `*_constructs`, `*_default`). Flagging those is a
                // false positive, so skip them.
                if !has_assert && fn_start > 0 && !is_compile_smoke_test(&fn_name) {
                    findings.push(SlopFinding {
                        rule: "test_quality".into(),
                        message: format!(
                            "Test `{fn_name}` at line ~{fn_start} has no assertions — tests should verify behavior"
                        ),
                        file: None,
                        line: Some(fn_start),
                    });
                }
                in_test_fn = false;
                fn_name.clear();
            }
        }
    }

    findings
}

// ─── 7. Silent Failure Detection ────────────────────────────────────────

/// Detect patterns that swallow errors: `let _ = expr`, empty match arms,
/// catch-all `_ => {}` with no logging/handling.
#[must_use]
pub fn check_silent_failure(file_content: &str, file_path: &Path) -> Vec<SlopFinding> {
    let mut findings = Vec::new();
    let is_test = file_content.contains("#[cfg(test)]");

    // Only flag in non-test production code
    let check_region = if is_test {
        if let Some(idx) = file_content.find("#[cfg(test)]") {
            &file_content[..idx]
        } else {
            file_content
        }
    } else {
        file_content
    };

    for (i, line) in check_region.lines().enumerate() {
        let t = line.trim();
        // Skip comments
        if t.starts_with("//") || t.starts_with('*') || t.starts_with("/*") {
            continue;
        }

        // `let _ = <expr>` where expr likely returns Result/Option
        if t.starts_with("let _ =") && !t.contains("// intentional") {
            // Heuristic: if the RHS likely returns Result (contains ?, .await, function call)
            let rhs = &t["let _ =".len()..];
            if rhs.contains('(') || rhs.contains(".await") {
                findings.push(SlopFinding {
                    rule: "silent_failure".into(),
                    message: format!(
                        "Line {}: `let _ = ...` discards a potentially meaningful Result/error — handle or log it",
                        i + 1
                    ),
                    file: Some(file_path.display().to_string()),
                    line: Some(i + 1),
                });
            }
        }

        // Empty catch-all match arm: `_ => {}`  or `_ => ()`
        if (t == "_ => {}," || t == "_ => {}") && !t.contains("// intentional") {
            findings.push(SlopFinding {
                rule: "silent_failure".into(),
                message: format!(
                    "Line {}: empty catch-all `_ => {{}}` silently ignores cases — add handling or logging",
                    i + 1
                ),
                file: Some(file_path.display().to_string()),
                line: Some(i + 1),
            });
        }

        // `Err(_) => {}` or `Err(_e) => {}`
        if ((t.starts_with("Err(_") && t.ends_with("=> {},"))
            || (t.starts_with("Err(_") && t.ends_with("=> {}")))
            && !t.contains("// intentional")
        {
            findings.push(SlopFinding {
                rule: "silent_failure".into(),
                message: format!(
                    "Line {}: swallowed Err variant — at minimum log the error",
                    i + 1
                ),
                file: Some(file_path.display().to_string()),
                line: Some(i + 1),
            });
        }
    }

    // Cap findings
    findings.truncate(8);
    findings
}

// ─── 8. Comment Slop Detection ──────────────────────────────────────────

/// Detect AI-generated narrator-style comments that add no information.
#[must_use]
pub fn check_comment_slop(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    // Patterns that indicate LLM narrator comments
    let slop_patterns: &[&str] = &[
        "// Step ",
        "// Now we ",
        "// Now, we ",
        "// First, ",
        "// First we ",
        "// Next, ",
        "// Next we ",
        "// Then, ",
        "// Then we ",
        "// Finally, ",
        "// Finally we ",
        "// Here we ",
        "// Here, we ",
        "// This function ",
        "// This method ",
        "// This is ",
        "// This will ",
        "// We need to ",
        "// We can ",
        "// Let's ",
        "// The following ",
        "// Below we ",
        "// As you can see",
        "// Note that we",
        "// Basically, ",
        "// Simply ",
        "// Just ",
        "// Obviously ",
    ];

    let mut slop_count = 0;
    let mut first_line = None;

    for (i, line) in file_content.lines().enumerate() {
        let t = line.trim();
        for pattern in slop_patterns {
            if t.starts_with(pattern) {
                slop_count += 1;
                if first_line.is_none() {
                    first_line = Some(i + 1);
                }
                break;
            }
        }
    }

    // Only flag if there's a pattern of narrator comments (3+)
    if slop_count >= 3 {
        findings.push(SlopFinding {
            rule: "comment_slop".into(),
            message: format!(
                "{slop_count} narrator-style comments detected (first at line {}) — remove comments that merely restate the code",
                first_line.unwrap_or(0)
            ),
            file: None,
            line: first_line,
        });
    }

    findings
}

// ─── 9. Metric Mimicry Detection ────────────────────────────────────────

/// Detect trivial/meaningless assertions that technically pass but verify nothing.
#[must_use]
pub fn check_metric_mimicry(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    // Only check test code
    let Some(idx) = file_content.find("#[cfg(test)]") else {
        return findings;
    };
    let test_section = &file_content[idx..];
    // Line numbers below are file-relative: the test section starts after this
    // many lines, so add it to each in-section index.
    let base = file_content[..idx].lines().count();

    for (i, line) in test_section.lines().enumerate() {
        let t = line.trim();

        // assert!(true)
        if t.contains("assert!(true)") {
            findings.push(SlopFinding {
                rule: "metric_mimicry".into(),
                message: format!(
                    "Line ~{}: `assert!(true)` always passes — test verifies nothing",
                    base + i + 1
                ),
                file: None,
                line: Some(base + i + 1),
            });
        }

        // assert_eq!(x, x) — same expression on both sides
        if t.contains("assert_eq!") {
            // Extract the two args (simple heuristic: split on first comma)
            if let Some(start) = t.find("assert_eq!(") {
                let inner = &t[start + "assert_eq!(".len()..];
                if let Some(end) = inner.rfind(')') {
                    let args = &inner[..end];
                    // Find the top-level comma (not inside nested parens)
                    let mut depth = 0;
                    let mut comma_pos = None;
                    for (ci, ch) in args.char_indices() {
                        match ch {
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' | '}' => depth -= 1,
                            ',' if depth == 0 => {
                                comma_pos = Some(ci);
                                break;
                            }
                            _ => {}
                        }
                    }
                    if let Some(cp) = comma_pos {
                        let left = args[..cp].trim();
                        let right = args[cp + 1..].trim();
                        if left == right && !left.is_empty() {
                            findings.push(SlopFinding {
                                rule: "metric_mimicry".into(),
                                message: format!(
                                    "Line ~{}: `assert_eq!({left}, {right})` compares identical expressions — always passes",
                                    base + i + 1
                                ),
                                file: None,
                                line: Some(base + i + 1),
                            });
                        }
                    }
                }
            }
        }

        // assert!(!false)
        if t.contains("assert!(!false)")
            || t.contains("assert!(1 == 1)")
            || t.contains("assert!(0 == 0)")
        {
            findings.push(SlopFinding {
                rule: "metric_mimicry".into(),
                message: format!(
                    "Line ~{}: tautological assertion — always passes, verifies nothing",
                    base + i + 1
                ),
                file: None,
                line: Some(base + i + 1),
            });
        }
    }

    findings.truncate(5);
    findings
}

// ─── 10. Conditional Test Detection ─────────────────────────────────────

/// Flag control flow (if, match, while) inside test functions.
#[must_use]
pub fn check_conditional_tests(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    let Some(idx) = file_content.find("#[cfg(test)]") else {
        return findings;
    };
    let test_section = &file_content[idx..];
    // Line numbers below are file-relative: the test section starts after this
    // many lines, so add it to each in-section index.
    let base = file_content[..idx].lines().count();

    let control_flow_keywords = ["if ", "match ", "while ", "for "];
    let mut in_test_fn = false;
    let mut fn_name = String::new();
    let mut fn_start = 0;
    let mut depth: i32 = 0;
    let mut has_conditional = false;
    let mut conditional_count = 0;

    let fn_re = regex::Regex::new(r"fn\s+(\w+)\s*\(").unwrap();

    for (i, line) in test_section.lines().enumerate() {
        if line.contains("#[test]") || line.contains("#[tokio::test]") {
            in_test_fn = true;
            has_conditional = false;
            conditional_count = 0;
            depth = 0;
            continue;
        }
        if in_test_fn
            && depth == 0
            && let Some(m) = fn_re.captures(line)
        {
            fn_name = m.get(1).map(|m| m.as_str().to_owned()).unwrap_or_default();
            fn_start = base + i + 1;
        }
        if in_test_fn {
            for ch in line.chars() {
                if ch == '{' {
                    depth += 1;
                }
                if ch == '}' {
                    depth -= 1;
                }
            }

            let t = line.trim();
            // Skip comments
            if !t.starts_with("//") && !t.starts_with('*') {
                for kw in &control_flow_keywords {
                    if t.starts_with(kw) || t.contains(&format!(" {kw}")) {
                        // Exclude `if let` which is idiomatic pattern matching
                        if !(*kw == "if " && t.contains("if let ")) {
                            has_conditional = true;
                            conditional_count += 1;
                        }
                        break;
                    }
                }
            }

            if depth == 0 && !fn_name.is_empty() {
                // Only flag if there are multiple conditionals (1 is often fine)
                if has_conditional && conditional_count >= 4 {
                    findings.push(SlopFinding {
                        rule: "conditional_test".into(),
                        message: format!(
                            "Test `{fn_name}` at line ~{fn_start} has {conditional_count} control-flow branches — consider splitting into separate test cases"
                        ),
                        file: None,
                        line: Some(fn_start),
                    });
                }
                in_test_fn = false;
                fn_name.clear();
            }
        }
    }

    findings.truncate(5);
    findings
}

// ─── 11. Naming Quality ─────────────────────────────────────────────────

/// Flag poor identifier naming patterns in newly written code.
#[must_use]
pub fn check_naming_quality(file_content: &str) -> Vec<SlopFinding> {
    naming_quality_impl(file_content, None)
}

/// `check_naming_quality` with the file path available, so test-only
/// files (`tests.rs`, `*_tests.rs`, files under `tests/`) skip the
/// long-identifier check without having to carry an inline
/// `#[cfg(test)]` attribute of their own.
#[must_use]
pub fn check_naming_quality_with_path(file_content: &str, file_path: &Path) -> Vec<SlopFinding> {
    naming_quality_impl(file_content, Some(file_path))
}

fn naming_quality_impl(file_content: &str, file_path: Option<&Path>) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    let vague_names: &[&str] = &[
        "handle_data",
        "process_item",
        "do_stuff",
        "do_thing",
        "do_work",
        "handle_event",
        "process_data",
        "run_task",
        "execute_action",
        "manager",
        "handler",
        "processor",
        "helper",
        "utils",
    ];

    let binding_re =
        regex::Regex::new(r"(?:let|const|static|fn)\s+(mut\s+)?([a-z_][a-z0-9_]*)").unwrap();

    // Whole-file test scope: a sibling `tests.rs`/`_tests.rs`/`tests/`
    // file loaded via `mod tests;` from a parent has no inline
    // `#[cfg(test)]` of its own, so the per-position region check below
    // would never fire on it. Short-circuit the whole file in that case.
    let whole_file_is_test = is_test_scope_file(file_path, file_content)
        && !file_content.contains("#[cfg(test)]\nmod ")
        && !file_content.contains("#[cfg(test)]\npub mod ");

    let is_test_region = |pos: usize| -> bool {
        if whole_file_is_test {
            return true;
        }
        file_content[..pos].rfind("#[cfg(test)]").is_some()
            && file_content[..pos]
                .rfind("\nmod ")
                .is_some_and(|m| m > file_content[..pos].rfind("#[cfg(test)]").unwrap_or(0))
    };

    let mut vague_count = 0;
    let mut first_vague_line = None;

    for (i, line) in file_content.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }

        // Check for vague function/variable names
        if let Some(caps) = binding_re.captures(t)
            && let Some(name) = caps.get(2)
        {
            let n = name.as_str();

            // Skip test code (test names can be descriptive-verbose)
            let line_start = file_content
                .lines()
                .take(i)
                .map(|l| l.len() + 1)
                .sum::<usize>();
            if is_test_region(line_start) {
                continue;
            }

            // Vague names
            if vague_names.contains(&n) {
                vague_count += 1;
                if first_vague_line.is_none() {
                    first_vague_line = Some(i + 1);
                }
            }

            // Overly long identifiers (>50 chars)
            if n.len() > 50 {
                findings.push(SlopFinding {
                        rule: "naming_quality".into(),
                        message: format!(
                            "Line {}: identifier `{}...` is {} chars — consider a shorter, clearer name",
                            i + 1,
                            &n[..30],
                            n.len()
                        ),
                        file: None,
                        line: Some(i + 1),
                    });
            }
        }
    }

    if vague_count >= 3 {
        findings.push(SlopFinding {
            rule: "naming_quality".into(),
            message: format!(
                "{vague_count} vague identifier names detected (first at line {}) — use domain-specific names",
                first_vague_line.unwrap_or(0)
            ),
            file: None,
            line: first_vague_line,
        });
    }

    findings.truncate(5);
    findings
}

// ─── 12. Guardrail Removal Detection ────────────────────────────────────

/// Compare new content against what was there before (if available).
/// Flags removal of validation/bounds/security patterns.
#[must_use]
pub fn check_guardrail_removal(old_content: Option<&str>, new_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    let Some(old) = old_content else {
        return findings;
    };

    // Patterns whose count should not decrease significantly
    let guardrail_patterns: &[(&str, &str)] = &[
        (".is_empty()", "emptiness check"),
        (".is_none()", "None check"),
        (".is_err()", "error check"),
        ("bounds", "bounds check"),
        ("validate", "validation"),
        ("sanitize", "sanitization"),
        ("authorize", "authorization"),
        ("authenticate", "authentication"),
        ("if len", "length check"),
        (".len()", "length check"),
        ("< len", "bounds check"),
        (">= len", "bounds check"),
        ("overflow", "overflow protection"),
    ];

    for (pattern, name) in guardrail_patterns {
        let old_count = old.matches(pattern).count();
        let new_count = new_content.matches(pattern).count();

        // Only flag if there was meaningful usage that got removed
        if old_count >= 2 && new_count == 0 {
            findings.push(SlopFinding {
                rule: "guardrail_removal".into(),
                message: format!(
                    "All {old_count} `{pattern}` ({name}) patterns removed — verify this is intentional"
                ),
                file: None,
                line: None,
            });
        } else if old_count >= 3 && new_count < old_count / 2 {
            findings.push(SlopFinding {
                rule: "guardrail_removal".into(),
                message: format!(
                    "{name} usage dropped from {old_count} to {new_count} — verify removals are safe"
                ),
                file: None,
                line: None,
            });
        }
    }

    findings.truncate(5);
    findings
}

// ─── 13. Security Regression Detection ──────────────────────────────────

/// Detect removal of security-critical patterns (parameterized queries,
/// crypto usage, auth checks, input sanitization).
#[must_use]
pub fn check_security_regression(old_content: Option<&str>, new_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    let Some(old) = old_content else {
        return findings;
    };

    // Security-critical patterns: if they existed before and vanished, that's bad
    let security_patterns: &[(&str, &str, &str)] = &[
        (
            "prepare(",
            "parameterized query",
            "SQL injection risk: parameterized queries replaced with string building",
        ),
        (
            "bind(",
            "parameter binding",
            "SQL injection risk: parameter binding removed",
        ),
        ("hmac", "HMAC verification", "integrity check removed"),
        (
            "verify_signature",
            "signature verification",
            "signature verification removed",
        ),
        (
            "hash_password",
            "password hashing",
            "password hashing removed",
        ),
        (
            "bcrypt",
            "bcrypt",
            "bcrypt usage removed — verify replacement is equally secure",
        ),
        (
            "argon2",
            "argon2",
            "argon2 usage removed — verify replacement is equally secure",
        ),
        ("csrf", "CSRF protection", "CSRF protection removed"),
        ("rate_limit", "rate limiting", "rate limiting removed"),
        ("escape_html", "HTML escaping", "XSS protection removed"),
        (
            "sanitize_input",
            "input sanitization",
            "input sanitization removed",
        ),
        (
            "Content-Security-Policy",
            "CSP header",
            "CSP header removed",
        ),
    ];

    for (pattern, _name, warning) in security_patterns {
        let old_has = old.contains(pattern);
        let new_has = new_content.contains(pattern);

        if old_has && !new_has {
            findings.push(SlopFinding {
                rule: "security_regression".into(),
                message: warning.to_string(),
                file: None,
                line: None,
            });
        }
    }

    // Check for string concatenation replacing parameterized queries
    let old_format_in_query = old.matches("format!(\"SELECT").count()
        + old.matches("format!(\"INSERT").count()
        + old.matches("format!(\"UPDATE").count()
        + old.matches("format!(\"DELETE").count();
    let new_format_in_query = new_content.matches("format!(\"SELECT").count()
        + new_content.matches("format!(\"INSERT").count()
        + new_content.matches("format!(\"UPDATE").count()
        + new_content.matches("format!(\"DELETE").count();

    if new_format_in_query > old_format_in_query && new_format_in_query >= 2 {
        findings.push(SlopFinding {
            rule: "security_regression".into(),
            message: format!(
                "SQL string concatenation increased ({old_format_in_query} → {new_format_in_query}) — potential injection vulnerability"
            ),
            file: None,
            line: None,
        });
    }

    findings.truncate(5);
    findings
}

// ─── 14. Rust Security Profile ──────────────────────────────────────────

fn is_comment_or_blank(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*')
}

fn code_before_line_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or(line).trim()
}

fn push_security_profile_finding(
    findings: &mut Vec<SlopFinding>,
    seen: &mut HashSet<&'static str>,
    key: &'static str,
    class: ResearchFindingClass,
    line: usize,
    message: impl Into<String>,
) {
    if seen.insert(key) {
        findings.push(SlopFinding {
            rule: class.as_str().into(),
            message: message.into(),
            file: None,
            line: Some(line),
        });
    }
}

/// Detect Rust patterns that should be routed to a concrete verifier rather than
/// reviewed only by prose. When `old_content` is present, unchanged matching
/// lines are ignored so edits do not repeatedly report pre-existing unsafe code.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "a flat list of pattern checks reads better than a chain of one-call helpers"
)]
pub fn check_rust_security_profile(
    old_content: Option<&str>,
    new_content: &str,
    _file_path: &Path,
) -> Vec<SlopFinding> {
    let mut findings = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    let old = old_content.unwrap_or("");

    for (idx, raw_line) in new_content.lines().enumerate() {
        if is_comment_or_blank(raw_line) {
            continue;
        }
        let line_no = idx + 1;
        let code = code_before_line_comment(raw_line);
        if code.is_empty() {
            continue;
        }
        if old_content.is_some() && old.contains(code) {
            continue;
        }

        if code.contains("unsafe {") || code.contains("unsafe fn") {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "unsafe-boundary",
                ResearchFindingClass::UnsafeContractCandidate,
                line_no,
                "Unsafe boundary changed or added - document caller/callee preconditions and validate them before self-fixing",
            );
        }

        if code.contains("unsafe impl")
            && (code.contains(" Send")
                || code.contains(" Sync")
                || code.contains("Send for")
                || code.contains("Sync for"))
        {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "unsafe-send-sync",
                ResearchFindingClass::SanitizerProvable,
                line_no,
                "Unsafe Send/Sync impl changed - run a concurrency-oriented proof such as loom/shuttle or sanitizer-backed tests where possible",
            );
        }

        if code.contains("transmute")
            || code.contains("MaybeUninit")
            || code.contains("ManuallyDrop")
            || code.contains(".set_len(")
            || code.contains("set_len(")
            || code.contains("get_unchecked")
            || code.contains("from_raw_parts")
        {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "miri-ub-surface",
                ResearchFindingClass::MiriProvable,
                line_no,
                "Potential UB surface changed (transmute/MaybeUninit/ManuallyDrop/set_len/get_unchecked/from_raw_parts) - require a focused Miri repro or safety proof",
            );
        }

        let pointer_to_int_cast = (code.contains(" as usize") || code.contains(" as isize"))
            && (code.contains("*const")
                || code.contains("*mut")
                || code.contains(".as_ptr()")
                || code.contains(".as_mut_ptr()")
                || code.contains("ptr::"));
        if pointer_to_int_cast || code.contains("expose_addr") || code.contains("from_exposed_addr")
        {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "strict-provenance",
                ResearchFindingClass::MiriProvable,
                line_no,
                "Pointer/integer provenance boundary changed - test under Miri strict provenance assumptions or preserve pointer provenance explicitly",
            );
        }

        if code.contains("extern \"C\"")
            || code.contains("extern \"system\"")
            || code.contains("#[no_mangle]")
            || code.contains("#[export_name")
        {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "ffi-boundary",
                ResearchFindingClass::UnsafeContractCandidate,
                line_no,
                "FFI/export boundary changed - review ABI, ownership, unwind, and validity contracts",
            );
        }

        if code.contains("#[target_feature")
            || code.contains("asm!(")
            || code.contains("global_asm!")
        {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "codegen-sensitive",
                ResearchFindingClass::LlvmDiffCandidate,
                line_no,
                "Codegen-sensitive feature changed (target_feature/asm) - use differential compiler or target-specific repro before trusting the change",
            );
        }

        if code.contains("read_volatile")
            || code.contains("write_volatile")
            || code.contains("static mut")
            || code.contains("UnsafeCell")
            || code.contains("AtomicPtr")
        {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "aliasing-concurrency",
                ResearchFindingClass::UnsafeContractCandidate,
                line_no,
                "Aliasing/concurrency primitive changed - review Rust memory-model contracts and add an executable oracle when feasible",
            );
        }

        if code.contains("catch_unwind") || code.contains("AssertUnwindSafe") {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "panic-unwind",
                ResearchFindingClass::FuzzerCandidate,
                line_no,
                "Unwind boundary changed - add a panic-injection or unusual trait-impl PoC to prove invariants survive unwinding",
            );
        }

        if code.contains("HashStable") || code.contains("DepNode") || code.contains("eval_always") {
            push_security_profile_finding(
                &mut findings,
                &mut seen,
                "incremental-correctness",
                ResearchFindingClass::IncrCacheCandidate,
                line_no,
                "Incremental/query-stability surface changed - preserve before/after source, diff, compiler version, and cache evidence",
            );
        }

        if findings.len() >= 8 {
            break;
        }
    }

    findings
}

// ─── 15. Premature Abstraction Detection ────────────────────────────────

/// Detect trait definitions that have exactly one implementor in the same file.
#[must_use]
pub fn check_premature_abstraction(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    let trait_re = regex::Regex::new(r"(?:pub\s+)?trait\s+(\w+)").unwrap();
    let impl_re = regex::Regex::new(r"impl\s+(\w+)\s+for\s+").unwrap();

    // Collect trait definitions
    let mut traits: Vec<(String, usize)> = Vec::new();
    for (i, line) in file_content.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if let Some(caps) = trait_re.captures(t)
            && let Some(name) = caps.get(1)
        {
            // Skip well-known patterns
            let n = name.as_str();
            if ![
                "Debug", "Clone", "Default", "Display", "Error", "From", "Into", "Iterator",
            ]
            .contains(&n)
            {
                traits.push((n.to_string(), i + 1));
            }
        }
    }

    // Count implementations per trait
    for (trait_name, def_line) in &traits {
        let impl_count = impl_re
            .captures_iter(file_content)
            .filter(|caps| caps.get(1).map(|m| m.as_str()) == Some(trait_name.as_str()))
            .count();

        if impl_count == 1 {
            findings.push(SlopFinding {
                rule: "premature_abstraction".into(),
                message: format!(
                    "Trait `{trait_name}` at line {def_line} has exactly 1 implementor in this file — consider using concrete types until a second impl is needed"
                ),
                file: None,
                line: Some(*def_line),
            });
        }
    }

    findings.truncate(3);
    findings
}

// ─── 15. Unused Imports Detection ───────────────────────────────────────

/// Run `cargo check --message-format=json` and extract `unused_imports` warnings.
/// Similar to `check_dead_code` but specifically for imports.
#[must_use]
pub fn check_unused_imports(cwd: &Path) -> Vec<SlopFinding> {
    let output = std::process::Command::new("cargo")
        .args(["check", "--message-format=json", "-q"])
        .current_dir(cwd)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut findings = Vec::new();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg.get("reason").and_then(|v| v.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = msg.get("message") else {
            continue;
        };
        let code = message
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if code != "unused_imports" {
            continue;
        }
        let text = message
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let span = message
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first());
        let file = span
            .and_then(|s| s.get("file_name"))
            .and_then(|f| f.as_str())
            .unwrap_or("?");
        let line_num = span
            .and_then(|s| s.get("line_start"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;

        findings.push(SlopFinding {
            rule: "unused_imports".into(),
            message: format!("{file}:{line_num}: {text}"),
            file: Some(file.to_string()),
            line: Some(line_num),
        });

        if findings.len() >= 10 {
            break;
        }
    }
    findings
}

// ─── 16. Resource Leak Detection ────────────────────────────────────────

/// Detect potential resource leaks: <File::open>, `TcpStream::connect`, etc.
/// without proper RAII scope management.
#[must_use]
pub fn check_resource_leaks(file_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();
    let is_test = file_content.contains("#[cfg(test)]");

    let check_region = if is_test {
        if let Some(idx) = file_content.find("#[cfg(test)]") {
            &file_content[..idx]
        } else {
            file_content
        }
    } else {
        file_content
    };

    // Look for resource acquisition without ? or proper error handling
    let resource_patterns: &[(&str, &str)] = &[
        ("File::open(", "File handle"),
        ("File::create(", "File handle"),
        ("TcpStream::connect(", "TCP stream"),
        ("TcpListener::bind(", "TCP listener"),
        ("UdpSocket::bind(", "UDP socket"),
        ("OpenOptions::new()", "File handle"),
    ];

    for (i, line) in check_region.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }

        for (pattern, resource_type) in resource_patterns {
            if t.contains(pattern) {
                // Check if it's properly handled: has ?, is in a let binding, or uses .unwrap()
                // Flag if it's stored in a variable without ? and the function is long
                if t.contains(".unwrap()") && !t.contains("drop(") {
                    // Only flag unwrap on resources in functions >20 lines
                    // (short functions likely handle it)
                    findings.push(SlopFinding {
                        rule: "resource_leak".into(),
                        message: format!(
                            "Line {}: {resource_type} acquired with .unwrap() — if the function returns early later, the resource may leak. Consider using `?` or an explicit `drop()`",
                            i + 1
                        ),
                        file: None,
                        line: Some(i + 1),
                    });
                }
            }
        }
    }

    findings.truncate(5);
    findings
}

// ─── 16b. Blocking-in-async Detection ───────────────────────────────────

/// Flag blocking calls inside an `async fn`. In a Tokio worker thread a
/// blocking syscall (`std::thread::sleep`, sync `std::fs`, `reqwest::blocking`)
/// stalls the executor for every task sharing that thread — the single most
/// common async-Rust performance footgun and a recurring topic in the Rust
/// community (clippy's nascent `blocking_in_async`, the Tokio docs' "Spawning
/// blocking work" guidance). This is a heuristic: it scans the body of each
/// `async fn` for a fixed set of blocking call shapes and skips lines that
/// already hop to a blocking pool (`spawn_blocking`, `block_in_place`).
#[must_use]
pub fn check_blocking_in_async(file_content: &str) -> Vec<SlopFinding> {
    // (fragment, label, whole_segment_match). `whole_segment_match` means the
    // call ident must end at the fragment boundary — used to stop a prefix like
    // `std::fs::read` from matching `std::fs::read_dir`/`read_to_string`, and
    // `.read_to_string(` from matching the *async* AsyncReadExt method (only the
    // sync `std::io::Read::read_to_string` shape, called on a non-`.await` line,
    // is blocking; async usage is followed by `.await`).
    const BLOCKING: &[(&str, &str)] = &[
        (
            "std::thread::sleep(",
            "std::thread::sleep blocks the executor; use tokio::time::sleep",
        ),
        (
            "thread::sleep(",
            "thread::sleep blocks the executor; use tokio::time::sleep",
        ),
        (
            "std::fs::read(",
            "synchronous std::fs::read blocks the executor; use tokio::fs or spawn_blocking",
        ),
        (
            "std::fs::read_to_string(",
            "synchronous std::fs::read_to_string blocks the executor; use tokio::fs or spawn_blocking",
        ),
        (
            "std::fs::write(",
            "synchronous std::fs::write blocks the executor; use tokio::fs or spawn_blocking",
        ),
        (
            "reqwest::blocking",
            "reqwest::blocking inside async blocks the executor; use the async client",
        ),
    ];

    // Mask strings/char-literals/comments so a `}` or a blocking-looking token
    // inside one can't terminate the body scan early or trigger a false hit.
    let masked = crate::rust_lex::mask_source(file_content);
    let lines: Vec<&str> = masked.lines().collect();
    let mut findings = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        let is_async_fn = t.starts_with("async fn ")
            || t.starts_with("pub async fn ")
            || t.contains(" async fn ");
        if !is_async_fn {
            i += 1;
            continue;
        }
        // Walk the function body by brace depth (over masked source, so braces
        // inside strings/comments are already gone).
        let mut depth: i32 = 0;
        let mut started = false;
        // Tracks the brace depth of the innermost `spawn_blocking` /
        // `block_in_place` closure so a blocking call on a *later* line inside
        // that closure is correctly suppressed (multi-line offload pattern).
        let mut hop_depth: Option<i32> = None;
        let mut j = i;
        while j < lines.len() {
            let body_line = lines[j];

            // A spawn-off marker opens an offload region from here until its
            // closure's braces close.
            if body_line.contains("spawn_blocking") || body_line.contains("block_in_place") {
                hop_depth = Some(depth);
            }

            if started && j > i && hop_depth.is_none() {
                for (frag, msg) in BLOCKING {
                    if body_line.contains(frag) {
                        findings.push(SlopFinding {
                            rule: "blocking_in_async".into(),
                            message: format!("Line {}: {msg}.", j + 1),
                            file: None,
                            line: Some(j + 1),
                        });
                        break;
                    }
                }
            }

            for c in body_line.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        started = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }

            // Reconcile the offload region. If the spawn line opened a closure
            // brace, `depth` is now above the spawn depth and suppression
            // persists into the closure body; once the closure closes (depth
            // back to/below the spawn depth) — or for a single-line offload
            // that never opened a brace — clear it so later sibling lines are
            // checked normally.
            if let Some(hd) = hop_depth
                && depth <= hd
            {
                hop_depth = None;
            }

            if started && depth <= 0 {
                break;
            }
            j += 1;
        }
        i = j + 1;
    }

    findings.truncate(5);
    findings
}

// ─── 17. Complexity Delta Tracking ──────────────────────────────────────

/// Compare complexity between old and new content. Flag significant increases.
#[must_use]
pub fn check_complexity_delta(old_content: Option<&str>, new_content: &str) -> Vec<SlopFinding> {
    let mut findings = Vec::new();

    let Some(old) = old_content else {
        return findings;
    };

    // Simple complexity proxy: count control flow keywords
    let complexity_of = |content: &str| -> usize {
        let keywords = [
            "if ", "else ", "match ", "while ", "for ", "loop ", "&&", "||", "?",
        ];
        keywords.iter().map(|kw| content.matches(kw).count()).sum()
    };

    let old_complexity = complexity_of(old);
    let new_complexity = complexity_of(new_content);

    if old_complexity > 0 {
        let increase_pct = ((new_complexity as f64 - old_complexity as f64) / old_complexity as f64
            * 100.0) as i64;
        if increase_pct > 30 && new_complexity > old_complexity + 10 {
            findings.push(SlopFinding {
                rule: "complexity_delta".into(),
                message: format!(
                    "Control-flow complexity increased by {increase_pct}% ({old_complexity} → {new_complexity}) — research shows >20% correlates with vulnerability introduction"
                ),
                file: None,
                line: None,
            });
        }
    }

    findings
}

// ─── Unified Runner ─────────────────────────────────────────────────────

/// Run all applicable checks. Fast path: only runs checks relevant to the
/// file type (.rs → all checks; other → duplication + complexity only).
///
/// `old_content` is the file's previous content (from `file_checkpoint`) if available,
/// enabling diff-based checks like `guardrail_removal` and `security_regression`.
#[must_use]
pub fn run_all_checks(file_path: &Path, file_content: &str, cwd: &Path) -> SlopReport {
    run_all_checks_with_old(file_path, file_content, None, cwd)
}

/// Full version with optional old content for diff-based checks.
#[must_use]
pub fn run_all_checks_with_old(
    file_path: &Path,
    file_content: &str,
    old_content: Option<&str>,
    cwd: &Path,
) -> SlopReport {
    run_all_checks_scoped(
        file_path,
        file_content,
        old_content,
        cwd,
        SlopScope::Changed,
    )
}

/// Full control over scoping. `SlopScope::Full` reports the whole file even when
/// the old content is known; `SlopScope::Changed` restricts the per-file checks
/// to the edit's added lines.
#[must_use]
pub fn run_all_checks_scoped(
    file_path: &Path,
    file_content: &str,
    old_content: Option<&str>,
    cwd: &Path,
    scope: SlopScope,
) -> SlopReport {
    let is_rust = file_path.extension().and_then(|e| e.to_str()) == Some("rs");

    // `Changed` scopes the whole-file checks to the lines this edit added
    // (2026-11-12 re-scan of 218 real jfc edits: unscoped, 97% of complexity and
    // 62% of duplication findings were about untouched code). `Full`, or a new
    // file with no previous version, leaves every check whole-file.
    let changed = match scope {
        SlopScope::Full => None,
        SlopScope::Changed => changed_lines(old_content, file_content),
    };

    // Line-scoped checks: keep only findings on a changed line.
    let mut scoped: Vec<SlopFinding> = Vec::new();
    scoped.extend(check_complexity(file_content));
    if is_rust {
        scoped.extend(check_coherence(file_content, file_path));
        scoped.extend(check_silent_failure(file_content, file_path));
        scoped.extend(check_borrow_clone(file_content));
        scoped.extend(check_naming_quality_with_path(file_content, file_path));
        scoped.extend(check_resource_leaks(file_content));
        scoped.extend(check_blocking_in_async(file_content));
    }
    scoped.retain(|f| on_changed_line(f, changed.as_ref()));

    let mut findings = scoped;

    // Comment slop is a file-level ratio signal, not line-scoped.
    findings.extend(check_comment_slop(file_content));

    if is_rust {
        // Rust security profile already diffs against old_content internally.
        findings.extend(check_rust_security_profile(
            old_content,
            file_content,
            file_path,
        ));

        // Test quality (only if file has tests).
        if file_content.contains("#[cfg(test)]") {
            findings.extend(check_test_quality(file_content));
            findings.extend(check_metric_mimicry(file_content));
            findings.extend(check_conditional_tests(file_content));
        }

        // Duplication (skip for very large files — too slow), scoped to the
        // windows this edit touched when we have a previous version.
        if file_content.len() < 200_000 {
            findings.extend(check_duplication_scoped(
                file_content,
                file_path,
                cwd,
                changed.as_ref(),
            ));
        }

        // Diff-based checks (require old content).
        if old_content.is_some() {
            findings.extend(check_guardrail_removal(old_content, file_content));
            findings.extend(check_security_regression(old_content, file_content));
            findings.extend(check_complexity_delta(old_content, file_content));
        }
    }

    // Churn (whole-repo, file-independent) and the cargo-check-based checks
    // (dead_code, unused_imports) are deliberately NOT run per edit — churn
    // alone produced ~⅓ of all findings and never concerned the edited file.
    // Call `check_churn`/`check_dead_code`/`check_unused_imports` explicitly
    // from an on-demand command instead.

    SlopReport {
        has_findings: !findings.is_empty(),
        findings,
    }
}

/// Format a `SlopReport` into a concise bulleted list.
#[must_use]
pub fn format_report(report: &SlopReport) -> String {
    if !report.has_findings || report.findings.is_empty() {
        return String::new();
    }
    let mut out = String::from("Slop Guard findings:\n");
    for finding in &report.findings {
        let loc = match (&finding.file, finding.line) {
            (Some(f), Some(l)) => format!(" ({f}:{l})"),
            (None, Some(l)) => format!(" (line {l})"),
            (Some(f), None) => format!(" ({f})"),
            (None, None) => String::new(),
        };
        out.push_str(&format!(
            "  • [{}]{loc}: {}\n",
            finding.rule, finding.message
        ));
        let artifacts = finding.suggested_proof_artifacts();
        if !artifacts.is_empty() {
            let proof = artifacts
                .iter()
                .map(|artifact| artifact.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "    route: {}; proof: {proof}\n",
                finding.research_class()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complexity_flags_long_function_normal() {
        let code = format!("pub fn big_one() {{\n{}\n}}", "    let x = 1;\n".repeat(90));
        let findings = check_complexity(&code);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "complexity" && f.message.contains("lines")),
            "expected complexity finding, got: {findings:?}",
        );
    }

    #[test]
    fn complexity_flags_deep_nesting_normal() {
        let code = "fn deep() {\n  if true {\n    if true {\n      if true {\n        if true {\n          if true {\n            if true {\n              x();\n            }\n          }\n        }\n      }\n    }\n  }\n}";
        let findings = check_complexity(code);
        assert!(
            findings.iter().any(|f| f.message.contains("nesting depth")),
            "expected nesting finding, got: {findings:?}",
        );
    }

    #[test]
    fn complexity_ok_for_short_function_normal() {
        let code = "fn small() {\n    println!(\"hi\");\n}";
        let findings = check_complexity(code);
        assert!(findings.is_empty());
    }

    #[test]
    fn coherence_flags_todo_normal() {
        let code = "fn wip() {\n    todo!()\n}";
        let findings = check_coherence(code, Path::new("src/lib.rs"));
        assert!(findings.iter().any(|f| f.message.contains("todo!()")));
    }

    #[test]
    fn coherence_flags_many_unwraps_normal() {
        let code = (0..8)
            .map(|i| format!("let x{i} = foo.unwrap();"))
            .collect::<Vec<_>>()
            .join("\n");
        let findings = check_coherence(&code, Path::new("src/lib.rs"));
        assert!(findings.iter().any(|f| f.message.contains("unwrap()")));
    }

    #[test]
    fn coherence_ok_in_test_code_normal() {
        let code = "#[cfg(test)]\nmod tests {\n    fn t() { foo.unwrap(); }\n}";
        let findings = check_coherence(code, Path::new("src/lib.rs"));
        // unwrap in test code is fine
        assert!(findings.iter().all(|f| !f.message.contains("unwrap()")));
    }

    #[test]
    fn test_quality_flags_no_assertions_normal() {
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn does_nothing() {\n        let x = 1;\n    }\n}";
        let findings = check_test_quality(code);
        assert!(
            findings.iter().any(|f| f.message.contains("no assertions")),
            "got: {findings:?}"
        );
    }

    #[test]
    fn test_quality_ok_with_assert_normal() {
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn works() {\n        assert_eq!(1, 1);\n    }\n}";
        let findings = check_test_quality(code);
        assert!(
            findings
                .iter()
                .all(|f| !f.message.contains("no assertions"))
        );
    }

    // Regression: a test whose body has a `}` inside a string/char literal must
    // NOT be falsely flagged as having no assertions. Before masking, the
    // literal brace closed the function scan early, hiding the real assert.
    #[test]
    fn test_quality_not_fooled_by_brace_in_literal_robust() {
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn has_assert_after_brace_literal() {\n        let s = \"}\";\n        assert_eq!(s, \"}\");\n    }\n}";
        let findings = check_test_quality(code);
        assert!(
            findings
                .iter()
                .all(|f| !f.message.contains("no assertions")),
            "brace-in-string literal caused a false no-assertions finding: {findings:?}"
        );
    }

    #[test]
    fn borrow_clone_flags_copy_literal_clone_normal() {
        let code = "fn f() {\n    let x = 42.clone();\n    let y = true.clone();\n}\n";
        let findings = check_borrow_clone(code);
        assert_eq!(findings.len(), 2, "got: {findings:?}");
        assert!(findings.iter().all(|f| f.rule == "borrow_clone"));
        assert!(findings.iter().any(|f| f.message.contains("Copy number")));
        assert!(findings.iter().any(|f| f.message.contains("Copy bool")));
    }

    #[test]
    fn borrow_clone_flags_double_ref_clone_normal() {
        let code = "fn f(x: &String) {\n    let y = &&x.clone();\n}\n";
        let findings = check_borrow_clone(code);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("double reference")),
            "got: {findings:?}"
        );
    }

    #[test]
    fn borrow_clone_flags_redundant_double_conversion_normal() {
        let code = "fn f(s: String) {\n    let y = s.clone().to_string();\n}\n";
        let findings = check_borrow_clone(code);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("redundant double conversion")),
            "got: {findings:?}"
        );
    }

    #[test]
    fn borrow_clone_skips_test_region_robust() {
        let code = "#[cfg(test)]\nmod tests {\n    fn t() { let x = 42.clone(); }\n}\n";
        let findings = check_borrow_clone(code);
        assert!(
            findings.is_empty(),
            "test-region clone slop is ignored: {findings:?}"
        );
    }

    #[test]
    fn borrow_clone_respects_allow_comment_robust() {
        let code = "fn f() {\n    let x = 42.clone(); // allow-clone\n}\n";
        let findings = check_borrow_clone(code);
        assert!(findings.is_empty());
    }

    #[test]
    fn churn_returns_empty_on_no_git_robust() {
        // Run in /tmp which has no git repo.
        let findings = check_churn(Path::new("/tmp"));
        // Should not panic, may return empty or non-empty depending on system.
        let _ = findings;
    }

    #[test]
    fn format_report_empty_returns_empty_normal() {
        let report = SlopReport {
            has_findings: false,
            findings: Vec::new(),
        };
        assert_eq!(format_report(&report), "");
    }

    #[test]
    fn format_report_with_findings_normal() {
        let report = SlopReport {
            has_findings: true,
            findings: vec![SlopFinding {
                rule: "complexity".into(),
                message: "too long".into(),
                file: Some("src/main.rs".into()),
                line: Some(42),
            }],
        };
        let formatted = format_report(&report);
        assert!(formatted.contains("complexity"));
        assert!(formatted.contains("too long"));
        assert!(formatted.contains("src/main.rs:42"));
    }

    #[test]
    fn silent_failure_flags_let_underscore_normal() {
        let code = "fn foo() {\n    let _ = do_something();\n}";
        let findings = check_silent_failure(code, Path::new("src/lib.rs"));
        assert!(
            findings.iter().any(|f| f.rule == "silent_failure"),
            "expected silent_failure finding, got: {findings:?}"
        );
    }

    #[test]
    fn silent_failure_ignores_test_code_normal() {
        let code = "#[cfg(test)]\nmod tests {\n    fn t() {\n        let _ = foo();\n    }\n}";
        let findings = check_silent_failure(code, Path::new("src/lib.rs"));
        assert!(
            findings.iter().all(|f| f.rule != "silent_failure"),
            "should not flag in test code"
        );
    }

    #[test]
    fn comment_slop_flags_narrator_comments_normal() {
        let code = "// Step 1: do the thing\nfn a() {}\n// Now we need to check\nfn b() {}\n// This function handles\nfn c() {}";
        let findings = check_comment_slop(code);
        assert!(
            findings.iter().any(|f| f.rule == "comment_slop"),
            "expected comment_slop finding, got: {findings:?}"
        );
    }

    #[test]
    fn comment_slop_ok_with_few_comments_normal() {
        let code = "// Step 1: only one\nfn a() {}\nfn b() {}";
        let findings = check_comment_slop(code);
        assert!(findings.is_empty());
    }

    #[test]
    fn metric_mimicry_flags_assert_true_normal() {
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert!(true);\n    }\n}";
        let findings = check_metric_mimicry(code);
        assert!(
            findings.iter().any(|f| f.rule == "metric_mimicry"),
            "expected metric_mimicry finding, got: {findings:?}"
        );
    }

    #[test]
    fn metric_mimicry_flags_self_equality_normal() {
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        let x = 5;\n        assert_eq!(x, x);\n    }\n}";
        let findings = check_metric_mimicry(code);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "metric_mimicry" && f.message.contains("identical")),
            "expected self-equality finding, got: {findings:?}"
        );
    }

    #[test]
    fn metric_mimicry_handles_non_ascii_assert_args() {
        // Regression: `chars().enumerate()` returned a char index and then
        // sliced the string with it as a byte offset, which panicked when
        // the args contained a multi-byte character ahead of the comma.
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert_eq!(name(\"李明\"), \"李明\");\n    }\n}";
        let findings = check_metric_mimicry(code);
        assert!(
            findings.iter().all(|f| !f.message.contains("identical")),
            "unexpected self-equality finding on non-ASCII args: {findings:?}",
        );
    }

    #[test]
    fn conditional_test_flags_multiple_branches_normal() {
        // Threshold is 4 branches (2–3 is normal test shape). This has 4 `if`s.
        let branch = "        if x > 0 { assert!(x > 0); }\n";
        let code = format!(
            "#[cfg(test)]\nmod tests {{\n    #[test]\n    fn branchy() {{\n{}    }}\n}}",
            branch.repeat(4)
        );
        let findings = check_conditional_tests(&code);
        assert!(
            findings.iter().any(|f| f.rule == "conditional_test"),
            "4 branches should flag, got: {findings:?}"
        );
    }

    #[test]
    fn conditional_test_tolerates_a_couple_branches_robust() {
        let code = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn ok() {\n        if a { assert!(a); }\n        if b { assert!(b); }\n    }\n}";
        let findings = check_conditional_tests(code);
        assert!(
            findings.is_empty(),
            "2 branches is normal and must not flag, got: {findings:?}"
        );
    }

    #[test]
    fn naming_quality_flags_vague_names_normal() {
        let code = "fn handle_data() {}\nfn process_item() {}\nfn do_stuff() {}";
        let findings = check_naming_quality(code);
        assert!(
            findings.iter().any(|f| f.rule == "naming_quality"),
            "expected naming_quality finding, got: {findings:?}"
        );
    }

    #[test]
    fn guardrail_removal_flags_removed_checks_normal() {
        let old = "fn f() {\n    if x.is_empty() { return; }\n    if y.is_empty() { return; }\n    if z.is_empty() { return; }\n}";
        let new = "fn f() {\n    process(x, y, z);\n}";
        let findings = check_guardrail_removal(Some(old), new);
        assert!(
            findings.iter().any(|f| f.rule == "guardrail_removal"),
            "expected guardrail_removal finding, got: {findings:?}"
        );
    }

    #[test]
    fn security_regression_flags_removed_crypto_normal() {
        let old =
            "fn auth() {\n    let h = bcrypt::hash(password)?;\n    verify_signature(&token)?;\n}";
        let new = "fn auth() {\n    // simplified\n    Ok(())\n}";
        let findings = check_security_regression(Some(old), new);
        assert!(
            findings.iter().any(|f| f.rule == "security_regression"),
            "expected security_regression finding, got: {findings:?}"
        );
    }

    #[test]
    fn rust_security_profile_routes_ub_surface_to_miri_normal() {
        let code = "fn widen(xs: &mut Vec<u8>) {\n    unsafe { xs.set_len(32); }\n}\n";
        let findings = check_rust_security_profile(None, code, Path::new("src/lib.rs"));
        let finding = findings
            .iter()
            .find(|f| f.rule == "miri_provable")
            .expect("expected miri-provable security finding");
        assert_eq!(finding.research_class(), ResearchFindingClass::MiriProvable);
        assert!(
            finding
                .suggested_proof_artifacts()
                .contains(&ProofArtifact::MiriTest)
        );
    }

    #[test]
    fn rust_security_profile_ignores_unchanged_line_when_old_content_known_robust() {
        let old = "fn widen(xs: &mut Vec<u8>) {\n    unsafe { xs.set_len(32); }\n}\n";
        let new = "fn widen(xs: &mut Vec<u8>) {\n    unsafe { xs.set_len(32); }\n    let _ = xs.len();\n}\n";
        let findings = check_rust_security_profile(Some(old), new, Path::new("src/lib.rs"));
        assert!(
            findings.iter().all(|f| f.rule != "miri_provable"),
            "unchanged unsafe line should not be reflagged: {findings:?}"
        );
    }

    #[test]
    fn format_report_includes_oracle_route_for_security_findings_normal() {
        let report = SlopReport {
            has_findings: true,
            findings: vec![SlopFinding {
                rule: "miri_provable".into(),
                message: "potential UB".into(),
                file: Some("src/lib.rs".into()),
                line: Some(7),
            }],
        };
        let formatted = format_report(&report);
        assert!(formatted.contains("route: miri_provable"));
        assert!(formatted.contains("focused Miri test"));
    }

    #[test]
    fn premature_abstraction_flags_single_impl_normal() {
        let code = "trait Processor {\n    fn process(&self);\n}\n\nstruct MyProcessor;\n\nimpl Processor for MyProcessor {\n    fn process(&self) {}\n}";
        let findings = check_premature_abstraction(code);
        assert!(
            findings.iter().any(|f| f.rule == "premature_abstraction"),
            "expected premature_abstraction finding, got: {findings:?}"
        );
    }

    #[test]
    fn complexity_delta_flags_large_increase_normal() {
        let old = "fn f() {\n    if x { y() }\n}";
        let new = "fn f() {\n    if x { if y { if z { if w { if v { for i in 0..10 { while true { match foo { _ => {} } } } } } } } }\n    if a && b || c && d || e && f || g && h {}\n}";
        let findings = check_complexity_delta(Some(old), new);
        assert!(
            findings.iter().any(|f| f.rule == "complexity_delta"),
            "expected complexity_delta finding, got: {findings:?}"
        );
    }

    #[test]
    fn blocking_in_async_flags_thread_sleep_normal() {
        let code = "\
async fn poll() {
    do_work();
    std::thread::sleep(std::time::Duration::from_secs(1));
}";
        let findings = check_blocking_in_async(code);
        assert!(
            findings.iter().any(|f| f.rule == "blocking_in_async"),
            "expected blocking_in_async finding, got: {findings:?}"
        );
    }

    #[test]
    fn blocking_in_async_allows_spawn_blocking_robust() {
        // The same blocking call wrapped in spawn_blocking is fine.
        let code = "\
async fn poll() {
    tokio::task::spawn_blocking(|| std::fs::read(\"x\")).await.unwrap();
}";
        assert!(
            check_blocking_in_async(code).is_empty(),
            "spawn_blocking-wrapped call must not be flagged"
        );
    }

    #[test]
    fn blocking_in_async_ignores_sync_fn_robust() {
        // A blocking call in a *sync* fn is not the executor footgun.
        let code = "\
fn helper() {
    std::thread::sleep(std::time::Duration::from_secs(1));
}";
        assert!(
            check_blocking_in_async(code).is_empty(),
            "sync fn must not be flagged"
        );
    }

    // Regression (auto-review #1): a `}` inside a string earlier in the body
    // must not terminate the body scan and hide a later blocking call.
    #[test]
    fn blocking_in_async_sees_past_brace_in_string_robust() {
        let code = "\
async fn handler() {
    let s = \"}\";
    std::thread::sleep(std::time::Duration::from_secs(1));
}";
        assert!(
            check_blocking_in_async(code)
                .iter()
                .any(|f| f.rule == "blocking_in_async"),
            "string brace hid the later blocking call"
        );
    }

    // Regression (auto-review #2): a blocking call on a different line than the
    // `spawn_blocking` keyword (the idiomatic multi-line closure) must NOT be
    // flagged.
    #[test]
    fn blocking_in_async_allows_multiline_spawn_blocking_robust() {
        let code = "\
async fn poll() {
    tokio::task::spawn_blocking(move || {
        std::fs::write(p, d).unwrap();
    }).await.unwrap();
}";
        assert!(
            check_blocking_in_async(code).is_empty(),
            "multi-line spawn_blocking offload must not be flagged: {:?}",
            check_blocking_in_async(code)
        );
    }

    // Regression (auto-review #3): substring over-match. Async
    // AsyncReadExt::read_to_string().await and std::fs::read_dir must NOT be
    // flagged as blocking.
    #[test]
    fn blocking_in_async_no_substring_overmatch_robust() {
        let code = "\
async fn poll() {
    let n = r.read_to_string(&mut buf).await.unwrap();
    let entries = std::fs::read_dir(p).unwrap();
}";
        assert!(
            check_blocking_in_async(code).is_empty(),
            "async read_to_string / read_dir wrongly flagged: {:?}",
            check_blocking_in_async(code)
        );
    }

    // A genuinely-blocking call after a single-line offload on a sibling line
    // is still caught (the offload suppression must not over-extend).
    #[test]
    fn blocking_in_async_single_line_offload_does_not_oversuppress_robust() {
        let code = "\
async fn poll() {
    let a = tokio::task::spawn_blocking(|| heavy()).await;
    std::thread::sleep(std::time::Duration::from_secs(1));
}";
        assert!(
            check_blocking_in_async(code)
                .iter()
                .any(|f| f.rule == "blocking_in_async"),
            "single-line offload over-suppressed a later sibling blocking call"
        );
    }

    impl std::fmt::Debug for SlopFinding {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "[{}] {}", self.rule, self.message)
        }
    }

    #[test]
    fn changed_lines_marks_only_new_content() {
        let old = "fn a() {}\nfn b() {}\n";
        let new = "fn a() {}\nfn new_one() {}\nfn b() {}\n";
        let set = changed_lines(Some(old), new).unwrap();
        assert!(set.contains(&2), "the inserted line is changed");
        assert!(
            !set.contains(&1) && !set.contains(&3),
            "unchanged lines are not flagged"
        );
    }

    #[test]
    fn complexity_in_untouched_fn_is_scoped_out() {
        // A long pre-existing function the edit did not touch must not fire.
        let big = format!("pub fn big() {{\n{}\n}}\n", "    let x = 1;\n".repeat(90));
        let edit = format!("// a new comment line\n{big}");
        let report = run_all_checks_with_old(Path::new("f.rs"), &edit, Some(&big), Path::new("."));
        assert!(
            !report.findings.iter().any(|f| f.rule == "complexity"),
            "a long function untouched by the edit must be scoped out, got {:?}",
            report.findings
        );
    }

    #[test]
    fn new_file_still_reports_everything() {
        let big = format!("pub fn big() {{\n{}\n}}\n", "    let x = 1;\n".repeat(90));
        let report = run_all_checks_with_old(Path::new("f.rs"), &big, None, Path::new("."));
        assert!(
            report.findings.iter().any(|f| f.rule == "complexity"),
            "a freshly created long function must still be flagged"
        );
    }

    #[test]
    fn test_section_findings_report_file_relative_lines() {
        // 10 lines of real code, then a test with a tautological assert. The
        // metric_mimicry finding must carry the FILE line, not a test-section
        // offset (regression: the old code reported the in-section index).
        let pre = "fn real() {}\n".repeat(10);
        let code = format!(
            "{pre}#[cfg(test)]\nmod t {{\n    #[test]\n    fn x() {{ assert!(true); }}\n}}"
        );
        let findings = check_metric_mimicry(&code);
        let f = findings
            .iter()
            .find(|f| f.rule == "metric_mimicry")
            .expect("one finding");
        assert!(
            f.line.unwrap() > 10,
            "line must be file-relative (>10), got {:?}",
            f.line
        );
    }

    #[test]
    fn test_quality_skips_compile_smoke_tests() {
        let code = "#[cfg(test)]\nmod t {\n    #[test]\n    fn client_builds_normal() { let _c = Client::new(); }\n    #[test]\n    fn theme_is_copyable_normal() { fn f<T: Copy>() {} f::<Theme>(); }\n}";
        let findings = check_test_quality(code);
        assert!(
            !findings.iter().any(|f| f.rule == "test_quality"),
            "compile/smoke tests must not be flagged for missing asserts, got {findings:?}"
        );
    }

    #[test]
    fn test_quality_still_flags_a_real_empty_test() {
        let code =
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn does_the_thing() { let x = compute(); }\n}";
        let findings = check_test_quality(code);
        assert!(
            findings.iter().any(|f| f.rule == "test_quality"),
            "a genuinely assertion-free test must still flag, got {findings:?}"
        );
    }

    #[test]
    fn duplication_ignores_bare_closing_brace_and_boilerplate_use_lines() {
        // `is_substantive_dup_line` filters the per-window counter in
        // `check_duplication_scoped`: a window is only cross-checked
        // when at least 3 of its 5 lines are substantive. The old
        // `trim.len() > 3` heuristic counted trim lengths 4-13 as
        // content (closing braces, derives, use lines, `#[cfg(test)]`,
        // and `}` + next line), which cross-matched unrelated Orion
        // decoders in ers-rs (`bundle_reference.rs` /
        // `billing_reference.rs` / …) and produced 1,296 findings that
        // pointed at files sharing only their header.
        let boilerplate = [
            "}",
            "};",
            "});",
            ")?;",
            "    }",
            "#[cfg(test)]",
            "#[derive(Serialize)]",
            "#[derive(Debug, Clone)]",
            "use serde::Serialize;",
            "use std::sync::Arc;",
            "pub use foo::bar;",
            "mod tests;",
            "pub mod inner;",
            "extern crate alloc;",
            "// comment",
            "/// doc comment",
            "    /// doc comment",
            "{",
            "",
            "    ",
        ];
        for line in boilerplate {
            assert!(
                !is_substantive_dup_line(line),
                "line {line:?} must count as boilerplate"
            );
        }
        let real = [
            "let open = detail.get(\"PolicyUuid\").and_then(str_field);",
            "fn extract_persons(value: &Value) -> Vec<Person> {",
            "    book.persons.extend(extract_persons(&value));",
            "    return Err(Error::UnknownRecord { kind });",
            "    Err(e) => { tracing::warn!(?e, \"skipped\"); }",
        ];
        for line in real {
            assert!(
                is_substantive_dup_line(line),
                "line {line:?} must count as substantive"
            );
        }
    }

    #[test]
    fn complexity_counts_structural_braces_only_not_char_literals_or_strings() {
        // Regression: a 36-line `parse_from_reader` in ers-rs's
        // `proformex.rs` had a `line.starts_with('{')` check whose `'{'`
        // bumped the brace counter, inflating the reported size to 886.
        // The function below contains `'{'` twice and `"{nested}"` once;
        // its real body is 4 lines inside a single `{...}` pair.
        let code = "fn small() {\n    let open = '{';\n    let close = '}';\n    let s = \"{nested}\";\n    println!(\"{s}\");\n}\n";
        let findings = check_complexity(code);
        assert!(
            findings.iter().all(|f| f.rule != "complexity"),
            "a 6-line function with brace literals must not be flagged, got {findings:?}",
        );
    }

    #[test]
    fn coherence_treats_external_test_modules_and_test_file_naming_as_test_regions() {
        // `otlp/tests.rs` is loaded by its parent through `mod tests;`
        // and has no `#[cfg(test)]` of its own. The pre-existing content
        // check would mark it as production, and its many `.unwrap()`
        // calls would flag under `coherence`.
        let code = "\n\
#[test]\n\
fn a() {\n\
    let x = Some(1).unwrap();\n\
    let y = Some(2).unwrap();\n\
    let z = Some(3).unwrap();\n\
    let w = Some(4).unwrap();\n\
    let v = Some(5).unwrap();\n\
    let u = Some(6).unwrap();\n\
    let t = Some(7).unwrap();\n\
}\n";
        let path = Path::new("/tmp/pkg/src/otlp/tests.rs");
        let findings = check_coherence(code, path);
        assert!(
            findings
                .iter()
                .all(|f| !f.message.contains("unwrap()/expect()")),
            "external `tests.rs` must count as a test scope, got {findings:?}",
        );
    }

    #[test]
    fn naming_quality_raises_the_length_threshold_for_test_items() {
        // A 54-char test fn identifier in an external test file was
        // flagged under `naming_quality` (342 findings in ers-rs across
        // `tests.rs`, `*_tests.rs`, and the `tests/` directory). With
        // the test-scope detection the whole file counts as test and
        // the long-name check skips.
        let code = "#[test]\nfn the_crate_classifier_delegates_through_the_router_and_lands() { assert!(true); }\n";
        let path = Path::new("/tmp/pkg/src/otlp/tests.rs");
        let findings = check_naming_quality_with_path(code, path);
        assert!(
            findings.iter().all(|f| f.rule != "naming_quality"),
            "long test-fn names in test files must not flag naming_quality, got {findings:?}",
        );
        // Regression: the inline `#[cfg(test)]` region path still skips.
        let inline = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn the_crate_classifier_delegates_through_the_router_and_lands() { assert!(true); }\n}\n";
        let findings = check_naming_quality_with_path(inline, Path::new("/tmp/pkg/src/lib.rs"));
        assert!(
            findings.iter().all(|f| f.rule != "naming_quality"),
            "inline test region should still skip, got {findings:?}",
        );
        // Guard: a long identifier in PRODUCTION code (non-test file,
        // not inside a `#[cfg(test)]` module) must still flag.
        let prod = "fn this_is_a_really_really_really_really_long_production_function_name() {}\n";
        let findings = check_naming_quality_with_path(prod, Path::new("/tmp/pkg/src/lib.rs"));
        assert!(
            findings.iter().any(|f| f.rule == "naming_quality"),
            "long production identifiers must still flag, got {findings:?}",
        );
    }
}
