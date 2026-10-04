//! JavaScript string semantics the parsers depend on: `\s` is JS whitespace (Unicode
//! `White_Space` plus U+FEFF, minus U+0085), which `trim`/`split(/\s+/)` use — Rust's own
//! `trim` differs on exactly those two code points.

use crate::width::is_whitespace_char as is_space;

/// JS `String.prototype.trim`.
pub(super) fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// JS `String.prototype.trimStart`.
pub(super) fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// JS `String.prototype.trimEnd`.
pub(super) fn trim_end(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// JS `s.split(/\s+/).filter((w) => w !== '')`.
pub(super) fn words(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|w| !w.is_empty()).collect()
}

/// JS `/\s/.test(s)`.
pub(super) fn has_space(s: &str) -> bool {
    s.chars().any(is_space)
}
