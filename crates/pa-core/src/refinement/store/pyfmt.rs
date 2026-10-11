//! Python's text forms for the values the kernel API echoes back.
//!
//! The kernel-facing harness store (`rlm.harness`) is Python API: its
//! rejection messages quote ids with `repr`, its overview prints skill
//! arguments with `json.dumps(sort_keys=True)`, and its timestamps are
//! `datetime.isoformat()`. These helpers reproduce those forms exactly, so
//! the messages and listings the model reads are byte-identical to the
//! retired Python implementation.

use std::fmt::Write as _;

use serde_json::Value;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Python `str.isalnum()` for one character: a letter (`L*`) or a number
/// (`N*`). Rust's `char::is_alphanumeric` also admits `Other_Alphabetic`
/// marks (Devanagari vowel signs), which Python does not.
pub(crate) fn is_alnum(ch: char) -> bool {
    matches!(
        ch.general_category(),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber
    )
}

/// `unicodedata.category(ch).startswith("M")`.
pub(crate) fn is_mark(ch: char) -> bool {
    matches!(
        ch.general_category(),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// Python `str.isspace()` for one character: Rust's `White_Space` set plus
/// the four information separators U+001C..U+001F.
fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python `str.strip()` (no argument).
pub(crate) fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Python `str.isprintable()` for one character.
fn is_printable(ch: char) -> bool {
    if ch == ' ' {
        return true;
    }
    !matches!(
        ch.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::SpaceSeparator
    )
}

/// `repr(text)` for a Python `str`.
pub(crate) fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            _ if (ch as u32) < 0x20 || ch as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", ch as u32);
            }
            _ if (ch as u32) < 0x7f || is_printable(ch) => out.push(ch),
            _ if (ch as u32) <= 0xff => {
                let _ = write!(out, "\\x{:02x}", ch as u32);
            }
            _ if (ch as u32) <= 0xffff => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            _ => {
                let _ = write!(out, "\\U{:08x}", ch as u32);
            }
        }
    }
    out.push(quote);
    out
}

/// `repr(value)` for a float.
pub(crate) fn repr_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    // `{:e}` is the shortest round-trip digit string, like Python's `repr`.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = mantissa
        .strip_prefix('-')
        .map_or(("", mantissa), |rest| ("-", rest));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    // Python switches to exponent form when the decimal point would sit
    // more than 16 places right or 4 places left of the first digit.
    let point = exponent + 1;
    if point > 16 || point <= -4 {
        let (head, tail) = digits.split_at(1);
        let fraction = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return format!(
            "{sign}{head}{fraction}e{exponent_sign}{:02}",
            exponent.abs()
        );
    }
    if point <= 0 {
        let zeros = "0".repeat(point.unsigned_abs() as usize);
        return format!("{sign}0.{zeros}{digits}");
    }
    let point = point as usize;
    if digits.len() <= point {
        let zeros = "0".repeat(point - digits.len());
        return format!("{sign}{digits}{zeros}.0");
    }
    let (whole, fraction) = digits.split_at(point);
    format!("{sign}{whole}.{fraction}")
}

fn number_text(number: &serde_json::Number) -> String {
    if number.is_f64() {
        repr_float(number.as_f64().unwrap_or_default())
    } else {
        number.to_string()
    }
}

/// `repr(value)` for a JSON-shaped Python value (`dict`, `list`, `str`,
/// `int`, `float`, `bool`, `None`).
pub(crate) fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number_text(number),
        Value::String(text) => repr_str(text),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(repr).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{}: {}", repr_str(key), repr(value)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// `str(value)`: the text itself for a `str`, `repr` otherwise.
pub(crate) fn str_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            repr(value)
        }
    }
}

/// The Python type name of a JSON-shaped value, when the caller did not
/// report one.
pub(crate) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            _ if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
}

fn push_json_sorted(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number_text(number)),
        Value::String(text) => push_json_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                push_json_sorted(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                push_json_string(out, key);
                out.push_str(": ");
                push_json_sorted(out, &map[key]);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(value, ensure_ascii=False, sort_keys=True)`.
pub(crate) fn json_dumps_sorted(value: &Value) -> String {
    let mut out = String::new();
    push_json_sorted(&mut out, value);
    out
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The UTC wall clock now, as (date-time parts, microseconds).
fn utc_now() -> ((i64, u32, u32, u32, u32, u32), u32) {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since_epoch.as_secs() as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400) as u32;
    (
        (
            year,
            month,
            day,
            of_day / 3_600,
            of_day % 3_600 / 60,
            of_day % 60,
        ),
        since_epoch.subsec_micros(),
    )
}

/// `datetime.now(timezone.utc).isoformat()`: microseconds, omitted when
/// zero, and a `+00:00` offset.
pub(crate) fn isoformat_now() -> String {
    let ((year, month, day, hour, minute, second), micros) = utc_now();
    let fraction = if micros == 0 {
        String::new()
    } else {
        format!(".{micros:06}")
    };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}+00:00")
}

/// `datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")`.
pub(crate) fn compact_stamp_now() -> String {
    let ((year, month, day, hour, minute, second), micros) = utc_now();
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}{micros:06}Z")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn repr_matches_python() {
        let cases = [
            (json!("plain"), "'plain'"),
            (json!("it's"), "\"it's\""),
            (json!("both ' and \""), "'both \\' and \"'"),
            (json!("tab\tnew\nline\\"), "'tab\\tnew\\nline\\\\'"),
            (
                json!("\u{7}\u{7f}\u{a0}\u{200b}é𠀀"),
                "'\\x07\\x7f\\xa0\\u200bé𠀀'",
            ),
            (json!(7), "7"),
            (json!(-3), "-3"),
            (json!(1.0), "1.0"),
            (json!(true), "True"),
            (json!(null), "None"),
            (json!(["x", 1]), "['x', 1]"),
            (json!({"a": [1, {"b": null}]}), "{'a': [1, {'b': None}]}"),
        ];
        for (value, expected) in cases {
            assert_eq!(repr(&value), expected, "{value}");
        }
    }

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.5, "1.5"),
            (100.0, "100.0"),
            (0.1, "0.1"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.234_567_890_123_456_7e20, "1.2345678901234567e+20"),
            (123_456.789, "123456.789"),
        ];
        for (value, expected) in cases {
            assert_eq!(repr_float(value), expected, "{value}");
        }
    }

    #[test]
    fn sorted_dumps_matches_python() {
        let value = json!({"b": [1, 2.5, "ü\n"], "a": {"z": null, "y": true}});
        assert_eq!(
            json_dumps_sorted(&value),
            r#"{"a": {"y": true, "z": null}, "b": [1, 2.5, "ü\n"]}"#
        );
    }

    #[test]
    fn python_character_classes() {
        // Devanagari vowel sign: a mark, not alphanumeric in Python.
        assert!(!is_alnum('\u{93f}'));
        assert!(is_mark('\u{93f}'));
        assert!(is_alnum('क'));
        assert!(is_alnum('²'));
        assert!(!is_alnum('_'));
        assert_eq!(strip("\u{1c} x \u{3000}"), "x");
    }

    #[test]
    fn isoformat_shape() {
        let stamp = isoformat_now();
        assert!(stamp.ends_with("+00:00"), "{stamp}");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_732), (2026, 10, 6));
        assert_eq!(compact_stamp_now().len(), "20261006T123456123456Z".len());
    }
}
