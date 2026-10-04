//! The JavaScript number semantics the pie chart reads and prints its values with:
//! `Number(string)`, `Math.round`, and `String(number)`.

/// JS `Number(s)` for an already-trimmed `s`; `None` where JS yields `NaN`.
pub(super) fn parse(s: &str) -> Option<f64> {
    if s.is_empty() {
        return Some(0.0);
    }
    match s {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = s.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            // Exact while the value fits 128 bits; beyond that, accumulate in floating point.
            return Some(u128::from_str_radix(digits, radix).map_or_else(
                |_| {
                    digits.chars().fold(0.0, |acc, c| {
                        acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
                    })
                },
                |v| v as f64,
            ));
        }
    }
    is_decimal_literal(s)
        .then(|| s.parse::<f64>().ok())
        .flatten()
}

/// The `StrDecimalLiteral` grammar: `[+-]? (d+ (. d*)? | . d+) ([eE] [+-]? d+)?`.
fn is_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

/// JS `Math.round`: halves round toward positive infinity.
pub(super) fn round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// JS `String(number)` (`Number.prototype.toString(10)`).
pub(super) fn to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v == 0.0 {
        return "0".to_owned();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let sign = if v < 0.0 { "-" } else { "" };
    // Shortest round-trip digits and the decimal exponent, from Rust's `{:e}`.
    let sci = format!("{:e}", v.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len() as i64;
    let n = exp.parse::<i64>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e_sign = if e < 0 { '-' } else { '+' };
        let mantissa = if k == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{mantissa}e{e_sign}{}", e.abs())
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::{parse, round, to_string};

    #[test]
    fn parses_like_js_number() {
        let cases: [(&str, Option<f64>); 14] = [
            ("", Some(0.0)),
            ("42.5", Some(42.5)),
            ("+5", Some(5.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("1e3", Some(1000.0)),
            ("0x1A", Some(26.0)),
            ("0b101", Some(5.0)),
            ("0o17", Some(15.0)),
            ("-0x1", None),
            ("1_000", None),
            ("abc", None),
            ("Infinity", Some(f64::INFINITY)),
            ("inf", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse(input), expected, "{input:?}");
        }
    }

    #[test]
    fn prints_like_js_string() {
        let cases: [(f64, &str); 10] = [
            (42.0, "42"),
            (42.96, "42.96"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (1e-7, "1e-7"),
            (0.000_001, "0.000001"),
            (-0.0, "0"),
            (-2.5, "-2.5"),
            (1.5e-10, "1.5e-10"),
        ];
        for (value, expected) in cases {
            assert_eq!(to_string(value), expected, "{value}");
        }
    }

    #[test]
    fn rounds_halves_up_like_js() {
        assert_eq!(
            [round(2.5), round(-2.5), round(0.49), round(1.5)].map(to_string),
            ["3", "-2", "0", "2"]
        );
    }
}
