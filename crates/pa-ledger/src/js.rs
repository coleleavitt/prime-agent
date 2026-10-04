//! The JavaScript string semantics the TS product's fingerprints depend on.
//!
//! A fingerprint id is a hash of normalized text, so every length cap, trim
//! and whitespace class must agree with V8's, or a failure observed by one
//! binary would never match the same failure observed by the other.
//! Lengths are UTF-16 code units (`String.prototype.length`), whitespace is
//! ECMAScript `WhiteSpace` plus `LineTerminator` (`\s`, `trim`), and a word
//! character is ASCII (`\w`, `\b`).

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// The characters ECMAScript `\s` and `String.prototype.trim` match.
#[must_use]
pub(crate) fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// The ECMAScript `\s` class as a regex character-class body.
pub(crate) const JS_WHITESPACE_CLASS: &str = r"\t\n\x0B\x0C\r \x{00A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// `String.prototype.trim`.
#[must_use]
pub(crate) fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// `String.prototype.length`: UTF-16 code units.
#[must_use]
pub(crate) fn js_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text.slice(0, units)` by UTF-16 code units. A cut through a surrogate
/// pair keeps neither half (JS would keep a lone high surrogate, which a
/// Rust string cannot hold).
#[must_use]
pub(crate) fn js_prefix(text: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        used += ch.len_utf16();
        if used > units {
            return &text[..index];
        }
    }
    text
}

/// `text.replace(/\s+/g, " ")`.
#[must_use]
pub(crate) fn collapse_js_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if is_js_whitespace(ch) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out
}

/// `JSON.stringify(text)`: `serde_json` escapes exactly the characters V8
/// does for a well-formed string (`"`, `\`, the C0 controls with the same
/// short forms, lowercase `\u00xx` otherwise).
#[must_use]
pub(crate) fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| String::from("\"\""))
}

/// `new Date(ms).toISOString()` for a non-negative epoch-millisecond time.
#[must_use]
pub fn iso_from_millis(millis: u64) -> String {
    let days = millis / 86_400_000;
    let rem = millis % 86_400_000;
    let (hour, minute, second, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    // Howard Hinnant's civil-from-days, for the proleptic Gregorian calendar.
    let z = i64::try_from(days).unwrap_or(i64::MAX / 2) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let mut out = String::with_capacity(24);
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z"
    );
    out
}

/// Milliseconds since the Unix epoch (`Date.now()`).
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `new Date().toISOString()`.
#[must_use]
pub fn now_iso() -> String {
    iso_from_millis(now_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_and_prefixes_count_utf16_units() {
        assert_eq!(js_len("a\u{1F600}b"), 4);
        assert_eq!(js_prefix("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(js_prefix("a\u{1F600}b", 2), "a");
        assert_eq!(js_prefix("abc", 10), "abc");
    }

    #[test]
    fn trim_and_collapse_use_the_ecmascript_whitespace_set() {
        assert_eq!(js_trim("\u{FEFF} x \u{2028}"), "x");
        // NEL is White_Space to Unicode but not to ECMAScript.
        assert_eq!(js_trim("\u{0085}x"), "\u{0085}x");
        assert_eq!(collapse_js_whitespace("a \t\n b\u{3000}c"), "a b c");
    }

    #[test]
    fn iso_matches_to_iso_string() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso_from_millis(1_767_225_600_000),
            "2026-01-01T00:00:00.000Z"
        );
        assert_eq!(iso_from_millis(951_782_400_123), "2000-02-29T00:00:00.123Z");
    }
}
