//! JSON exactly as the TS product wrote it.
//!
//! Every file this crate owns was first written by `JSON.stringify`, and a
//! policy id is the sha256 of a canonical JSON, so byte compatibility needs
//! three things `serde_json` does not do by default:
//!
//! - numbers print like ECMAScript `Number::toString` (`1` not `1.0`, `1e-7`,
//!   `1e+21`), through [`JsFormatter`];
//! - a canonical form with sorted keys ([`canonical_json`], TS `canonicalJson`);
//! - parsing that rounds every decimal exactly ([`parse`]): `serde_json`'s default
//!   float parser is best-effort, and a node score that came back one ulp off
//!   would move a replay's best, the probation floor and every `V` downstream.

use std::io;

use serde::Serialize;
use serde_json::ser::{CompactFormatter, Formatter, PrettyFormatter};
use serde_json::{Map, Number, Value};

/// ECMAScript `Number::toString(x)` for a finite or non-finite double.
#[must_use]
pub fn js_number(value: f64) -> String {
    pa_types::js::js_number_to_string(value)
}

/// ECMAScript `x.toFixed(digits)`: `String(x)` when `x` is not finite or
/// `|x| >= 1e21`, else the exact binary value rounded at `digits` with a
/// decimal tie going to the larger magnitude (`0.125.toFixed(2)` is `0.13`;
/// Rust's `{:.2}` ties to even and prints `0.12`). `-0` prints as `0`.
#[must_use]
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return js_number(value);
    }
    // A double has at most 1074 fractional digits, so this expansion is exact
    // and the rounding below sees the true value.
    let exact = format!("{:.1074}", value.abs());
    let (whole, fraction) = exact.split_once('.').unwrap_or((exact.as_str(), ""));
    let mut kept: Vec<u8> = whole.bytes().chain(fraction.bytes().take(digits)).collect();
    if fraction
        .as_bytes()
        .get(digits)
        .is_some_and(|digit| *digit >= b'5')
    {
        let mut carry = true;
        for digit in kept.iter_mut().rev() {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            kept.insert(0, b'1');
        }
    }
    let split = kept.len() - digits;
    let mut out = String::with_capacity(kept.len() + 2);
    if value < 0.0 {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&kept[..split]).unwrap_or("0"));
    if digits > 0 {
        out.push('.');
        out.push_str(std::str::from_utf8(&kept[split..]).unwrap_or(""));
    }
    out
}

/// A `serde_json` formatter that writes doubles as `JSON.stringify` does.
/// Wraps the compact or the 2-space pretty layout (`JSON.stringify(x,
/// undefined, 2)`, which `serde_json`'s pretty layout already matches).
pub struct JsFormatter<F> {
    inner: F,
}

impl JsFormatter<CompactFormatter> {
    fn compact() -> Self {
        Self {
            inner: CompactFormatter,
        }
    }
}

impl JsFormatter<PrettyFormatter<'static>> {
    fn pretty() -> Self {
        Self {
            inner: PrettyFormatter::with_indent(b"  "),
        }
    }
}

impl<F: Formatter> Formatter for JsFormatter<F> {
    fn write_f64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        writer.write_all(js_number(value).as_bytes())
    }

    fn write_f32<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        writer.write_all(js_number(f64::from(value)).as_bytes())
    }

    fn begin_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.begin_array(writer)
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.inner.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.begin_object(writer)
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.inner.begin_object_key(writer, first)
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.inner.end_object_value(writer)
    }
}

fn write_with<F: Formatter, T: Serialize + ?Sized>(value: &T, formatter: F) -> String {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    // Serializing plain data structures into memory cannot fail.
    if value.serialize(&mut serializer).is_err() {
        return "null".to_string();
    }
    String::from_utf8(out).unwrap_or_default()
}

/// `JSON.stringify(value)`.
#[must_use]
pub fn stringify<T: Serialize + ?Sized>(value: &T) -> String {
    write_with(value, JsFormatter::compact())
}

/// `JSON.stringify(value, undefined, 2)`.
#[must_use]
pub fn stringify_pretty<T: Serialize + ?Sized>(value: &T) -> String {
    write_with(value, JsFormatter::pretty())
}

/// TS `canonicalJson`: keys sorted at every level, numbers as `JSON.stringify`.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // JS sorts by UTF-16 code units; for the BMP that is code-point order.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&stringify(key));
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&stringify(scalar)),
    }
}

/// A JSON value from a double: `Null` for a non-finite one, as `JSON.stringify`
/// writes it.
#[must_use]
pub fn number(value: f64) -> Value {
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// Why a document is not JSON.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON at byte {offset}: {reason}")]
pub struct ParseError {
    pub offset: usize,
    pub reason: &'static str,
}

/// `JSON.parse` with exactly rounded doubles: integers stay integers, every
/// other number is parsed by the correctly rounding `str::parse::<f64>`.
///
/// # Errors
///
/// [`ParseError`] when `text` is not one JSON value.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(value)
}

const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, reason: &'static str) -> ParseError {
        ParseError {
            offset: self.pos,
            reason,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, literal: &str, value: Value) -> Result<Value, ParseError> {
        if self.text[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("unexpected token"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        match self.bytes.get(self.pos) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.expect_literal("true", Value::Bool(true)),
            Some(b'f') => self.expect_literal("false", Value::Bool(false)),
            Some(b'n') => self.expect_literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error("expected a value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut map = Map::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(self.error("expected a key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            // JSON.parse keeps the last duplicate at the first key's position.
            map.insert(key, value);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let end = self.pos + 4;
        let digits = self
            .text
            .get(self.pos..end)
            .ok_or_else(|| self.error("short unicode escape"))?;
        let code = u32::from_str_radix(digits, 16).map_err(|_| self.error("bad unicode escape"))?;
        self.pos = end;
        Ok(code)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&byte) = self.bytes.get(self.pos) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.text[start..self.pos]);
            match self.bytes.get(self.pos) {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = *self
                        .bytes
                        .get(self.pos)
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high)
                                && self.text[self.pos..].starts_with("\\u")
                            {
                                let save = self.pos;
                                self.pos += 2;
                                let low = self.hex4()?;
                                if (0xDC00..0xE000).contains(&low) {
                                    0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                                } else {
                                    self.pos = save;
                                    high
                                }
                            } else {
                                high
                            };
                            // A lone surrogate has no Rust `char`; U+FFFD stands in.
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.error("bad escape")),
                    }
                }
                _ => return Err(self.error("unterminated string")),
            }
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            return Err(self.error("expected digits"));
        }
        if self.bytes[digits_start] == b'0' && self.pos - digits_start > 1 {
            return Err(self.error("leading zero"));
        }
        let mut integral = true;
        if self.bytes.get(self.pos) == Some(&b'.') {
            integral = false;
            self.pos += 1;
            let fraction_start = self.pos;
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == fraction_start {
                return Err(self.error("expected fraction digits"));
            }
        }
        if let Some(b'e' | b'E') = self.bytes.get(self.pos) {
            integral = false;
            self.pos += 1;
            if let Some(b'+' | b'-') = self.bytes.get(self.pos) {
                self.pos += 1;
            }
            let exponent_start = self.pos;
            while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == exponent_start {
                return Err(self.error("expected exponent digits"));
            }
        }
        let text = &self.text[start..self.pos];
        if integral {
            if let Ok(value) = text.parse::<u64>() {
                return Ok(Value::Number(value.into()));
            }
            if let Ok(value) = text.parse::<i64>() {
                return Ok(Value::Number(value.into()));
            }
        }
        let value: f64 = text.parse().map_err(|_| self.error("bad number"))?;
        Ok(number(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_ecmascript() {
        let cases: [(f64, &str); 16] = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-2.0, "-2"),
            (0.5, "0.5"),
            (1_000_000.0, "1000000"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (0.524_727_268_236_976_9, "0.5247272682369769"),
            (2f64.powi(60), "1152921504606847000"),
            (f64::NAN, "NaN"),
        ];
        let printed: Vec<(f64, String)> = cases.iter().map(|(v, _)| (*v, js_number(*v))).collect();
        let expected: Vec<(f64, String)> =
            cases.iter().map(|(v, s)| (*v, (*s).to_string())).collect();
        assert_eq!(
            printed.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>(),
            expected.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>()
        );
    }

    /// Expected strings are node's `x.toFixed(digits)`: a decimal tie rounds
    /// to the larger magnitude, and `|x| >= 1e21` prints as `String(x)`.
    #[test]
    fn to_fixed_matches_node() {
        let cases: [(f64, usize, &str); 29] = [
            (0.007_812_5, 6, "0.007813"),
            (-0.007_812_5, 6, "-0.007813"),
            (0.125, 2, "0.13"),
            (-0.125, 2, "-0.13"),
            (0.5, 0, "1"),
            (2.5, 0, "3"),
            (-2.5, 0, "-3"),
            (1.005, 2, "1.00"),
            (2.675, 2, "2.67"),
            (0.0, 6, "0.000000"),
            (-0.0, 6, "0.000000"),
            (-1e-7, 6, "-0.000000"),
            (1e-7, 6, "0.000000"),
            (0.999_999_5, 6, "1.000000"),
            (999_999.999_999_5, 6, "999999.999999"),
            (1e20, 2, "100000000000000000000.00"),
            (1e21, 2, "1e+21"),
            (-1e21, 6, "-1e+21"),
            (3.402_823_669_209_385e38, 6, "3.402823669209385e+38"),
            (-3.402_823_669_209_385e38, 6, "-3.402823669209385e+38"),
            (123.456, 0, "123"),
            (5e-324, 6, "0.000000"),
            (f64::MAX, 2, "1.7976931348623157e+308"),
            (0.000_001, 6, "0.000001"),
            (5e-7, 6, "0.000000"),
            (0.804_978_3, 6, "0.804978"),
            (f64::NAN, 6, "NaN"),
            (f64::INFINITY, 6, "Infinity"),
            (f64::NEG_INFINITY, 2, "-Infinity"),
        ];
        let printed: Vec<(f64, usize, String)> = cases
            .iter()
            .map(|&(value, digits, _)| (value, digits, to_fixed(value, digits)))
            .collect();
        let expected: Vec<(f64, usize, String)> = cases
            .iter()
            .map(|&(value, digits, text)| (value, digits, text.to_string()))
            .collect();
        assert_eq!(format!("{printed:?}"), format!("{expected:?}"));
    }

    #[test]
    fn stringify_writes_integral_doubles_without_a_fraction() {
        let value = serde_json::json!({"a": 1.0, "b": [0.5, 2.0], "c": null, "d": "x\u{1}"});
        assert_eq!(
            stringify(&value),
            r#"{"a":1,"b":[0.5,2],"c":null,"d":"x\u0001"}"#
        );
        assert_eq!(
            stringify_pretty(&value),
            "{\n  \"a\": 1,\n  \"b\": [\n    0.5,\n    2\n  ],\n  \"c\": null,\n  \"d\": \"x\\u0001\"\n}"
        );
        assert_eq!(
            stringify_pretty(&serde_json::json!({"e": [], "f": {}})),
            "{\n  \"e\": [],\n  \"f\": {}\n}"
        );
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let value = serde_json::json!({"b": 1, "a": {"d": [1.5, {"z": 1, "y": 2}], "c": true}});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"c":true,"d":[1.5,{"y":2,"z":1}]},"b":1}"#
        );
    }

    #[test]
    fn parse_rounds_every_decimal_exactly_and_round_trips_the_text() {
        for text in [
            "0.12345678901234567",
            "0.5247272682369769",
            "1.2580980849318234",
            "9007199254740993.5",
            "4.9406564584124654e-324",
            "1e-7",
        ] {
            let parsed = parse(text).expect("parses");
            let exact: f64 = text.parse().expect("std parses");
            assert_eq!(
                parsed.as_f64().map(f64::to_bits),
                Some(exact.to_bits()),
                "{text}"
            );
        }
        let doc = parse(r#" {"a":[1,-2,3.5e2,"é😀"],"b":{"c":null,"d":false}} "#).expect("parses");
        assert_eq!(
            doc,
            serde_json::json!({"a": [1, -2, 350.0, "é😀"], "b": {"c": null, "d": false}})
        );
        assert!(parse("{\"a\":1,}").is_err());
        assert!(parse("[01]").is_err());
        assert!(parse("1 2").is_err());
    }
}
