//! JavaScript value semantics the TS product's parsing relies on.

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
