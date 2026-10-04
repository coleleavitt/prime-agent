//! The JavaScript semantics RAVO's persisted state and digests depend on:
//! `String.prototype.localeCompare` (the order the TS reducer sorts ids
//! in), `canonicalJson` + SHA-256 (the digests a certificate binds), and
//! UTF-16 slicing.

use std::cmp::Ordering;
use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Printable ASCII in the order ICU's root collation (V8's
/// `localeCompare` with no locale) gives it at the primary level, with
/// each lowercase letter before its uppercase form.
const ICU_ASCII_ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789aAbBcCdDeEfFgGhHiIjJkKlLmMnNoOpPqQrRsStTuUvVwWxXyYzZ";

/// The primary weight of `ch`: case-folded rank in [`ICU_ASCII_ORDER`];
/// anything else ranks after it by code point (identifiers RAVO sorts are
/// ASCII; the order of other text is not specified here).
fn primary(ch: char) -> u32 {
    let folded = ch.to_ascii_lowercase();
    ICU_ASCII_ORDER
        .find(folded)
        .map_or(0x100 + u32::from(ch), |rank| {
            u32::try_from(rank).unwrap_or(u32::MAX)
        })
}

/// `left.localeCompare(right)` for the ASCII identifiers RAVO sorts:
/// punctuation before digits before letters, letters case-insensitively,
/// then lowercase before uppercase.
#[must_use]
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    left.chars()
        .map(primary)
        .cmp(right.chars().map(primary))
        .then_with(|| {
            left.chars()
                .map(|ch| ch.is_ascii_uppercase())
                .cmp(right.chars().map(|ch| ch.is_ascii_uppercase()))
        })
}

/// `[...new Set(values)].sort((a, b) => a.localeCompare(b))`.
#[must_use]
pub fn sorted_unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !out.contains(&value) {
            out.push(value);
        }
    }
    out.sort_by(|left, right| locale_compare(left, right));
    out
}

/// `JSON.stringify(number)` for a finite double: ECMAScript
/// `Number::toString` over the shortest round-trip digits.
#[must_use]
pub fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` is the shortest round-trip form: `d[.ddd]e<exp>`.
    let formatted = format!("{:e}", value.abs());
    let (mantissa, exponent) = formatted
        .split_once('e')
        .unwrap_or((formatted.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len();
    // The decimal point sits `n` digits into `digits`.
    let n = exponent.parse::<i64>().unwrap_or(0) + 1;
    let count = |value: i64| usize::try_from(value).unwrap_or(0);
    let k_signed = i64::try_from(k).unwrap_or(i64::MAX);
    let body = if k_signed <= n && n <= 21 {
        format!("{digits}{}", "0".repeat(count(n - k_signed)))
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at(count(n));
        format!("{whole}.{fraction}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(count(-n)))
    } else {
        let exp = n - 1;
        let exp_sign = if exp < 0 { "-" } else { "+" };
        let (first, rest) = digits.split_at(1);
        let point = if rest.is_empty() { "" } else { "." };
        format!("{first}{point}{rest}e{exp_sign}{}", exp.abs())
    };
    format!("{sign}{body}")
}

fn write_number(out: &mut String, number: &serde_json::Number) {
    if number.is_i64() || number.is_u64() {
        let _ = write!(out, "{number}");
    } else {
        out.push_str(&js_number(number.as_f64().unwrap_or(0.0)));
    }
}

/// The TS `canonicalJson`: `JSON.stringify` with object keys sorted by
/// UTF-16 code unit at every level.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => out.push_str(&json_string(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(key));
                out.push(':');
                write_canonical(out, &map[key]);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(text)`: `serde_json` escapes exactly what V8 does for a
/// well-formed string.
#[must_use]
pub fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| String::from("\"\""))
}

/// Lowercase hex SHA-256 of `text`'s UTF-8 bytes (the TS `sha256`).
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `text.slice(-units)` by UTF-16 code units. A cut through a surrogate
/// pair keeps neither half.
#[must_use]
pub fn js_tail(text: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices().rev() {
        used += ch.len_utf16();
        if used > units {
            return &text[index + ch.len_utf8()..];
        }
    }
    text
}

/// `Math.round(value)`: halves round toward positive infinity.
#[must_use]
pub fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_order_puts_punctuation_digits_letters_and_case_like_icu() {
        let mut ids: Vec<String> = [
            "a", "A", "b", "B", "aa", "Aa", "a-b", "ab", "a_b", "a:b", "a1",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        ids.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(
            ids,
            ["a", "A", "a_b", "a-b", "a:b", "a1", "aa", "Aa", "ab", "b", "B"]
        );
    }

    #[test]
    fn numbers_print_like_number_to_string() {
        let cases = [
            (1e21, "1e+21"),
            (1.5e-7, "1.5e-7"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (0.000_001, "0.000001"),
            (2.5, "2.5"),
            (-0.1, "-0.1"),
            (100.0, "100"),
        ];
        for (value, expected) in cases {
            assert_eq!(js_number(value), expected, "{value}");
        }
    }

    #[test]
    fn tails_count_utf16_units() {
        assert_eq!(js_tail("abc\u{1F600}d", 3), "\u{1F600}d");
        assert_eq!(js_tail("abc\u{1F600}d", 2), "d");
        assert_eq!(js_tail("ab", 5), "ab");
    }
}
