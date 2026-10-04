//! JavaScript value semantics the TS product's parsing relies on.

use std::fmt::Write as _;

/// JS `Number(text)` for a string (ECMAScript `StringToNumber`): `text`
/// without its JS white space and line terminators parses as `0` when
/// empty, `±Infinity` (exact case, optional sign), an unsigned `0x`/`0o`/`0b`
/// integer of any length, or a signed decimal literal with an optional
/// exponent, each rounded to the nearest `f64`; anything else is `NaN`.
/// Unlike `f64::from_str`, `inf`/`nan`/`infinity` are `NaN`, and unlike
/// `str::trim`, U+FEFF trims and U+0085 does not.
#[must_use]
pub fn js_number(text: &str) -> f64 {
    let text = js_trim(text);
    if text.is_empty() {
        return 0.0;
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let bytes = text.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let radix_bits = match bytes[1] {
            b'x' | b'X' => Some(4),
            b'o' | b'O' => Some(3),
            b'b' | b'B' => Some(1),
            _ => None,
        };
        if let Some(bits) = radix_bits {
            return non_decimal_integer(&text[2..], bits).unwrap_or(f64::NAN);
        }
    }
    if is_decimal_literal(bytes) {
        // The grammar is checked above, so the std parser (correctly
        // rounded, overflow to infinity, `-0` kept) sees only JS literals.
        text.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// JS `String.prototype.trim`: `text` without its leading and trailing JS
/// white space and line terminators (the set `Number(text)` ignores).
#[must_use]
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_white_space)
}

/// JS `String(x)` (ECMAScript `Number::toString(x)`, the text
/// `JSON.stringify` writes for a finite double): shortest round-trip digits,
/// plain notation from `1e-7` up to `1e21`, exponent form (`1e+21`, `1e-7`)
/// outside it, and `0` for `-0`.
#[must_use]
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value < 0.0;
    // `{:e}` is the shortest round-trip digit string, as ECMAScript requires.
    let exp_form = format!("{:e}", value.abs());
    let (mantissa, exponent) = exp_form.split_once('e').unwrap_or((exp_form.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let k = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let n = exponent + 1;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        let split = usize::try_from(n).unwrap_or(0);
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if e >= 0 { '+' } else { '-' };
        let _ = write!(out, "e{sign}{}", e.abs());
    }
    out
}

/// ECMAScript `WhiteSpace` and `LineTerminator` code points.
fn is_js_white_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `StrDecimalLiteral` without `Infinity`: `[+-]` then `digits [. digits?]`
/// or `. digits`, then an optional `e`/`E` `[+-] digits`.
fn is_decimal_literal(bytes: &[u8]) -> bool {
    fn digits(bytes: &[u8], at: usize) -> usize {
        bytes[at..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
    }
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let whole = digits(bytes, at);
    at += whole;
    let mut fraction = 0;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        fraction = digits(bytes, at);
        at += fraction;
    }
    if whole == 0 && fraction == 0 {
        return false;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exponent = digits(bytes, at);
        if exponent == 0 {
            return false;
        }
        at += exponent;
    }
    at == bytes.len()
}

/// An unsigned integer in radix `2^bits_per_digit`, rounded to nearest-even
/// like JS: the top 64 significant bits, with any lower set bit folded into
/// the last one as a sticky bit (64 > 53 + 2, so the rounding is exact),
/// then scaled by the dropped bit count. `None` for an empty or bad digit.
fn non_decimal_integer(digits: &str, bits_per_digit: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    let radix = 1_u32 << bits_per_digit;
    let mut mantissa: u64 = 0;
    let mut dropped: i32 = 0;
    let mut sticky = false;
    for c in digits.chars() {
        let digit = c.to_digit(radix)?;
        for bit in (0..bits_per_digit)
            .rev()
            .map(|at| u64::from((digit >> at) & 1))
        {
            if dropped == 0 && mantissa.leading_zeros() > 0 {
                mantissa = (mantissa << 1) | bit;
            } else {
                dropped = dropped.saturating_add(1);
                sticky |= bit == 1;
            }
        }
    }
    mantissa |= u64::from(sticky);
    // u64 -> f64 rounds to nearest-even; scaling by a power of two is exact
    // until it overflows to infinity, which is the JS result too.
    #[allow(clippy::cast_precision_loss)]
    let value = mantissa as f64;
    Some(value * 2_f64.powi(dropped))
}

#[cfg(test)]
mod tests;
