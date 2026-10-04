//! The JavaScript number and string semantics the TS reports print with:
//! `Number.prototype.toFixed` / `toPrecision` / `toExponential` (round half
//! up on the exact binary value), `String.prototype.padStart` / `padEnd`
//! (UTF-16 lengths), and `Math.round`.

/// Significant digits of a finite, positive double, exactly (a double has at
/// most 767 of them): the digit string and the decimal exponent of its first
/// digit.
fn exact_digits(value: f64) -> (Vec<u8>, i64) {
    let formatted = format!("{value:.800e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .unwrap_or((formatted.as_str(), "0"));
    let digits = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|digit| digit - b'0')
        .collect();
    (digits, exponent.parse().unwrap_or(0))
}

/// `digits` (first digit at decimal exponent `exponent`) rounded half up to
/// `keep` significant digits; the result may carry into a new leading digit
/// (`exponent + 1`). `keep` may be zero or negative: the value then rounds
/// to `0` or to one unit at the rounding position.
fn round_half_up(digits: &[u8], exponent: i64, keep: i64) -> (Vec<u8>, i64) {
    let Ok(kept) = usize::try_from(keep) else {
        return (Vec::new(), exponent);
    };
    let round_up = digits.get(kept).is_some_and(|digit| *digit >= 5);
    let mut out: Vec<u8> = (0..kept)
        .map(|index| digits.get(index).copied().unwrap_or(0))
        .collect();
    if !round_up {
        return (out, exponent);
    }
    for digit in out.iter_mut().rev() {
        if *digit == 9 {
            *digit = 0;
        } else {
            *digit += 1;
            return (out, exponent);
        }
    }
    // Every kept digit carried (or none was kept): a new leading 1.
    out.insert(0, 1);
    out.pop_if_longer(kept);
    (out, exponent + 1)
}

trait PopIfLonger {
    fn pop_if_longer(&mut self, len: usize);
}

impl PopIfLonger for Vec<u8> {
    /// Keep the significant-digit count after a carry (`9.99` -> `10.0`).
    fn pop_if_longer(&mut self, len: usize) {
        if len > 0 && self.len() > len {
            self.pop();
        }
    }
}

fn digit_string(digits: &[u8]) -> String {
    digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect()
}

/// `value.toFixed(fraction)` for a finite `value` below `1e21` in
/// magnitude (the reports never print anything larger).
#[must_use]
pub(crate) fn to_fixed(value: f64, fraction: usize) -> String {
    if !value.is_finite() {
        return js_number_text(value);
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    let fraction_len = i64::try_from(fraction).unwrap_or(i64::MAX);
    let (whole, frac) = if magnitude == 0.0 {
        ("0".to_string(), "0".repeat(fraction))
    } else {
        let (digits, exponent) = exact_digits(magnitude);
        let (rounded, exponent) = round_half_up(&digits, exponent, exponent + 1 + fraction_len);
        if rounded.is_empty() {
            return format!(
                "{}{}",
                if negative { "-" } else { "" },
                to_fixed(0.0, fraction)
            );
        }
        // `rounded` holds the digits from 10^exponent down to 10^-fraction.
        let mut all = digit_string(&rounded);
        let integer_len = exponent + 1;
        if integer_len <= 0 {
            let zeros = usize::try_from(-integer_len).unwrap_or(0);
            all = format!("{}{all}", "0".repeat(zeros));
        }
        let total = fraction + usize::try_from(integer_len.max(0)).unwrap_or(0);
        while all.len() < total {
            all.push('0');
        }
        let split = all.len() - fraction;
        let whole = if split == 0 {
            "0".to_string()
        } else {
            all[..split].to_string()
        };
        (whole, all[split..].to_string())
    };
    let sign = if negative { "-" } else { "" };
    if fraction == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac}")
    }
}

/// `value.toExponential(fraction)` for a finite `value`.
#[must_use]
pub(crate) fn to_exponential(value: f64, fraction: usize) -> String {
    if !value.is_finite() {
        return js_number_text(value);
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let magnitude = value.abs();
    let keep = fraction + 1;
    let (digits, exponent) = if magnitude == 0.0 {
        (vec![0; keep], 0)
    } else {
        let (digits, exponent) = exact_digits(magnitude);
        round_half_up(&digits, exponent, i64::try_from(keep).unwrap_or(i64::MAX))
    };
    let text = digit_string(&digits);
    let (first, rest) = text.split_at(1);
    let point = if rest.is_empty() { "" } else { "." };
    let exponent_sign = if exponent < 0 { "-" } else { "+" };
    format!(
        "{sign}{first}{point}{rest}e{exponent_sign}{}",
        exponent.abs()
    )
}

/// `value.toPrecision(precision)` for a finite `value`.
#[must_use]
pub(crate) fn to_precision(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return js_number_text(value);
    }
    if value == 0.0 {
        return to_fixed(0.0, precision.saturating_sub(1));
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let (digits, exponent) = exact_digits(value.abs());
    let precision_len = i64::try_from(precision).unwrap_or(i64::MAX);
    let (rounded, exponent) = round_half_up(&digits, exponent, precision_len);
    if exponent < -6 || exponent >= precision_len {
        let text = digit_string(&rounded);
        let (first, rest) = text.split_at(1);
        let point = if rest.is_empty() { "" } else { "." };
        let exponent_sign = if exponent < 0 { "-" } else { "+" };
        return format!(
            "{sign}{first}{point}{rest}e{exponent_sign}{}",
            exponent.abs()
        );
    }
    let text = digit_string(&rounded);
    let body = if exponent >= 0 {
        let split = usize::try_from(exponent + 1).unwrap_or(0);
        let (whole, frac) = text.split_at(split.min(text.len()));
        if frac.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{frac}")
        }
    } else {
        let zeros = usize::try_from(-exponent - 1).unwrap_or(0);
        format!("0.{}{text}", "0".repeat(zeros))
    };
    format!("{sign}{body}")
}

/// `String(value)` for the integral counts and finite rates the reports
/// print.
#[must_use]
pub(crate) fn js_number_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    crate::json::js_number(value)
}

/// `String.prototype.length`: UTF-16 code units.
#[must_use]
pub(crate) fn js_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text.padEnd(width)`.
#[must_use]
pub(crate) fn pad_end(text: &str, width: usize) -> String {
    let len = js_len(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(len)))
}

/// `text.padStart(width)`.
#[must_use]
pub(crate) fn pad_start(text: &str, width: usize) -> String {
    let len = js_len(text);
    format!("{}{text}", " ".repeat(width.saturating_sub(len)))
}

/// `Math.round(value)`: the nearest integer, halves toward positive
/// infinity.
#[must_use]
pub(crate) fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rounds_half_up_on_the_exact_value() {
        let cases = [
            (0.125, 2, "0.13"),
            (-0.125, 2, "-0.13"),
            (1.005, 2, "1.00"),
            (8.0, 2, "8.00"),
            (0.857_142_857_142_857_1, 2, "0.86"),
            (-7.142_857_142_857_143, 2, "-7.14"),
            (99.995, 2, "100.00"),
            (0.004, 2, "0.00"),
            (0.005, 2, "0.01"),
            (0.0, 2, "0.00"),
            (-0.0, 2, "0.00"),
            (-0.001, 2, "-0.00"),
            (123.5, 0, "124"),
            (0.4, 0, "0"),
            (0.5, 0, "1"),
            (12.0, 1, "12.0"),
            (1234.5678, 0, "1235"),
            (0.000_01, 2, "0.00"),
            (-0.000_01, 2, "-0.00"),
        ];
        for (value, digits, expected) in cases {
            assert_eq!(to_fixed(value, digits), expected, "{value}");
        }
    }

    #[test]
    fn precision_and_exponential_match_number_formatting() {
        let precision = [
            (0.012_345_6, 3, "0.0123"),
            (0.99951, 3, "1.00"),
            (0.5, 3, "0.500"),
            (0.001_234_5, 3, "0.00123"),
            (0.000_000_123, 3, "1.23e-7"),
            (1234.0, 3, "1.23e+3"),
        ];
        for (value, digits, expected) in precision {
            assert_eq!(to_precision(value, digits), expected, "{value}");
        }
        let exponential = [
            (0.000_123_45, 2, "1.23e-4"),
            (0.000_999_9, 2, "1.00e-3"),
            (0.000_001, 0, "1e-6"),
            (5.0, 2, "5.00e+0"),
        ];
        for (value, digits, expected) in exponential {
            assert_eq!(to_exponential(value, digits), expected, "{value}");
        }
    }

    #[test]
    fn padding_and_rounding_follow_javascript() {
        assert_eq!(pad_start("ab", 4), "  ab");
        assert_eq!(pad_end("ab", 4), "ab  ");
        assert_eq!(pad_end("abcdef", 4), "abcdef");
        assert_eq!(
            [2.5, -2.5, 0.499_999_999_999_999_94, 1.4].map(|value| js_round(value).to_string()),
            ["3", "-2", "0", "1"]
        );
        assert_eq!(js_number_text(4.0), "4");
        assert_eq!(js_number_text(0.5), "0.5");
    }
}
