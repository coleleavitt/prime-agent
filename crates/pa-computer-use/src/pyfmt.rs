//! Python text forms the model-facing output keeps byte-identical.
//!
//! The rendered accessibility tree quotes titles and values with Python's
//! `repr()`, formats coordinates through `round(x, 1)` and float `repr`, and
//! app matching compares `str.casefold()` forms. These are the exact `CPython`
//! algorithms, with the Unicode data taken from the kernel's interpreter
//! ([`tables`]).

mod tables;

/// `repr(text)` for a Python `str`.
#[must_use]
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        let code = u32::from(character);
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if character == quote => {
                out.push('\\');
                out.push(character);
            }
            _ if code < 0x20 || code == 0x7F => push_escape(&mut out, 'x', code, 2),
            _ if code < 0x7F => out.push(character),
            _ if is_printable(code) => out.push(character),
            _ if code <= 0xFF => push_escape(&mut out, 'x', code, 2),
            _ if code <= 0xFFFF => push_escape(&mut out, 'u', code, 4),
            _ => push_escape(&mut out, 'U', code, 8),
        }
    }
    out.push(quote);
    out
}

/// `\\{kind}` plus `code` in `width` lowercase hex digits.
fn push_escape(out: &mut String, kind: char, code: u32, width: usize) {
    use std::fmt::Write;
    let _ = write!(out, "\\{kind}{code:0width$x}");
}

fn is_printable(code: u32) -> bool {
    tables::NON_PRINTABLE
        .binary_search_by(|&(start, end)| {
            if end < code {
                std::cmp::Ordering::Less
            } else if start > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_err()
}

/// `repr(value)` for a Python `float` (the shortest round-tripping digits,
/// positional between `1e-4` and `1e16`, scientific outside).
#[must_use]
pub fn repr_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    // `{:e}` is Rust's shortest round-trip form: `-1.25e3`.
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return scientific;
    };
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            let zeros = "0".repeat(usize::try_from(-point).unwrap_or_default());
            format!("0.{zeros}{digits}")
        } else {
            let point = usize::try_from(point).unwrap_or_default();
            if digits.len() <= point {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!(
            "{sign}{first}{fraction}e{exponent_sign}{:02}",
            exponent.unsigned_abs()
        )
    }
}

/// Python's `round(value, 1)`: the correctly rounded one-decimal value
/// (ties to even on the exact binary value), back as a float.
#[must_use]
pub fn round1(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.1}").parse().unwrap_or(value)
}

/// Python's `f"{value:.0f}"`.
#[must_use]
pub fn fixed0(value: f64) -> String {
    format!("{value:.0}")
}

/// Python's `str.casefold()`.
#[must_use]
pub fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match tables::CASEFOLD_EXCEPTIONS.binary_search_by_key(&character, |&(key, _)| key) {
            Ok(index) => out.push_str(tables::CASEFOLD_EXCEPTIONS[index].1),
            Err(_) => out.extend(character.to_lowercase()),
        }
    }
    out
}

/// Python's `a.casefold() == b.casefold()`.
#[must_use]
pub fn casefold_eq(left: &str, right: &str) -> bool {
    casefold(left) == casefold(right)
}

/// Python's `str(value)` for an `int` or `float` JSON number from the kernel.
#[must_use]
pub fn str_number(value: &serde_json::Number) -> String {
    if let Some(integer) = value.as_i64() {
        integer.to_string()
    } else if let Some(integer) = value.as_u64() {
        integer.to_string()
    } else {
        repr_float(value.as_f64().unwrap_or(f64::NAN))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_str_matches_cpython() {
        // Expected values printed by CPython 3.11's repr().
        let cases = [
            ("Save", "'Save'"),
            ("it's", "\"it's\""),
            ("both ' and \"", "'both \\' and \"'"),
            ("tab\there\nnew\rret\\", "'tab\\there\\nnew\\rret\\\\'"),
            ("\u{1b}esc\u{7f}", "'\\x1besc\\x7f'"),
            ("nbsp\u{a0}soft\u{ad}", "'nbsp\\xa0soft\\xad'"),
            ("zw\u{200b}sp", "'zw\\u200bsp'"),
            ("emoji \u{1F600}", "'emoji \u{1F600}'"),
            ("é — ü", "'é — ü'"),
            ("\u{e0001}tag", "'\\U000e0001tag'"),
        ];
        for (input, expected) in cases {
            assert_eq!(repr_str(input), expected, "{input:?}");
        }
    }

    #[test]
    fn repr_float_matches_cpython() {
        let cases = [
            (150.0, "150.0"),
            (400.5, "400.5"),
            (0.1, "0.1"),
            (12.3, "12.3"),
            (-2.5, "-2.5"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123_456_789_012_345.6, "123456789012345.6"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.25e-7, "1.25e-07"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (f64::INFINITY, "inf"),
        ];
        for (input, expected) in cases {
            assert_eq!(repr_float(input), expected, "{input}");
        }
    }

    #[test]
    fn round1_rounds_the_exact_binary_value_half_to_even() {
        // CPython: round(0.25, 1) == 0.2, round(0.35, 1) == 0.3 (0.35 is
        // below .35 in binary), round(2.675, 1) == 2.7, round(-1.25, 1) == -1.2.
        let cases = [
            (0.25, 0.2),
            (0.35, 0.3),
            (2.675, 2.7),
            (-1.25, -1.2),
            (10.65, 10.7),
        ];
        for (input, expected) in cases {
            assert_eq!(round1(input).to_bits(), f64::to_bits(expected), "{input}");
        }
    }

    #[test]
    fn fixed0_rounds_ties_to_even_like_python_format() {
        assert_eq!(fixed0(400.0), "400");
        assert_eq!(fixed0(2.5), "2");
        assert_eq!(fixed0(3.5), "4");
        assert_eq!(fixed0(1599.6), "1600");
    }

    #[test]
    fn casefold_matches_cpython() {
        assert_eq!(casefold("Weiß"), "weiss");
        assert_eq!(casefold("WEISS"), "weiss");
        assert_eq!(casefold("ΣΑΣ"), "σασ");
        assert_eq!(casefold("ﬁle"), "file");
        assert_eq!(casefold("Org.GNOME.TextEditor"), "org.gnome.texteditor");
        assert!(casefold_eq("Weiß", "WEISS"));
        assert!(!casefold_eq("Weiß", "nope"));
    }
}
