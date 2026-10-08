//! Python's `json` module, as the machine library uses it.
//!
//! A MACHINE.md payload, a double-quoted frontmatter value, and the
//! rendered spec all went through Python's `json.loads` while the library
//! lived in the kernel, and their failures surfaced verbatim in the import
//! gate's sentences. [`loads`] reproduces the C scanner of Python 3.11
//! (the kernel's interpreter): `NaN`/`Infinity`/`-Infinity`, arbitrary
//! precision ints (with the 4300-digit conversion limit), last-value-wins
//! duplicate keys at the first key's position, and every
//! `JSONDecodeError` message with its `line`/`column`/`char` position in
//! code points. [`dumps`] is `json.dumps(value)` with the default
//! separators and `ensure_ascii`.

use std::fmt::Write as _;

use super::super::pyvalue::{py_float_repr, PyValue};

/// How deep containers may nest before the decoder gives up the way
/// Python's recursion limit does (the limit is 1000 frames; the kernel's
/// own frames take a few of them).
const MAX_DEPTH: usize = 990;

/// Python's default `sys.get_int_max_str_digits()`.
const INT_MAX_STR_DIGITS: usize = 4300;

/// Why a document did not decode, as the kernel raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// `json.JSONDecodeError` (a `ValueError`): the full message.
    Decode(String),
    /// The int-conversion limit's `ValueError`.
    Value(String),
    /// `RecursionError`: the nesting passed the interpreter's limit.
    Recursion(String),
}

impl LoadError {
    /// `str(error)`.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Decode(message) | Self::Value(message) | Self::Recursion(message) => message,
        }
    }
}

/// `JSONDecodeError(msg, doc, pos)`'s message.
fn decode_error(doc: &[char], message: &str, pos: usize) -> LoadError {
    let before = &doc[..pos.min(doc.len())];
    let lineno = before.iter().filter(|ch| **ch == '\n').count() + 1;
    let colno = match before.iter().rposition(|ch| *ch == '\n') {
        Some(newline) => pos - newline,
        None => pos + 1,
    };
    LoadError::Decode(format!(
        "{message}: line {lineno} column {colno} (char {pos})"
    ))
}

fn is_ws(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r')
}

/// One open container while scanning.
enum Frame {
    List(Vec<PyValue>),
    Dict {
        pairs: Vec<(PyValue, PyValue)>,
        key: Option<String>,
    },
}

struct Scanner<'a> {
    doc: &'a [char],
}

impl Scanner<'_> {
    fn at(&self, idx: usize) -> Option<char> {
        self.doc.get(idx).copied()
    }

    fn skip_ws(&self, mut idx: usize) -> usize {
        while self.at(idx).is_some_and(is_ws) {
            idx += 1;
        }
        idx
    }

    fn matches(&self, idx: usize, word: &str) -> bool {
        let count = word.chars().count();
        idx + count <= self.doc.len() && self.doc[idx..idx + count].iter().copied().eq(word.chars())
    }

    /// `scanstring_unicode`: `begin` is the opening quote.
    fn string(&self, begin: usize) -> Result<(String, usize), LoadError> {
        let doc = self.doc;
        let len = doc.len();
        let mut out = String::new();
        let mut end = begin + 1;
        loop {
            let mut next = end;
            let mut terminator = None;
            while next < len {
                let ch = doc[next];
                if ch == '"' || ch == '\\' {
                    terminator = Some(ch);
                    break;
                }
                if (ch as u32) <= 0x1f {
                    return Err(decode_error(doc, "Invalid control character at", next));
                }
                next += 1;
            }
            let Some(terminator) = terminator else {
                return Err(decode_error(doc, "Unterminated string starting at", begin));
            };
            out.extend(&doc[end..next]);
            next += 1;
            if terminator == '"' {
                return Ok((out, next));
            }
            if next == len {
                return Err(decode_error(doc, "Unterminated string starting at", begin));
            }
            let escape = doc[next];
            if escape != 'u' {
                end = next + 1;
                let ch = match escape {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => return Err(decode_error(doc, "Invalid \\escape", end - 2)),
                };
                out.push(ch);
                continue;
            }
            next += 1;
            end = next + 4;
            if end >= len {
                return Err(decode_error(doc, "Invalid \\uXXXX escape", next - 1));
            }
            let mut code = self.hex4(next, end)?;
            next = end;
            if (0xD800..=0xDBFF).contains(&code)
                && end + 6 < len
                && doc[next] == '\\'
                && doc[next + 1] == 'u'
            {
                let low = self.hex4(next + 2, end + 6)?;
                if (0xDC00..=0xDFFF).contains(&low) {
                    code = 0x1_0000 + (((code - 0xD800) << 10) | (low - 0xDC00));
                    end += 6;
                }
            }
            // A lone surrogate is a valid Python str but not a Rust one: it
            // decodes to U+FFFD.
            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
        }
    }

    /// Four hex digits at `from..to`; a bad digit reports the escape's
    /// `u` (Python's `end - 5`).
    fn hex4(&self, from: usize, to: usize) -> Result<u32, LoadError> {
        let mut code = 0_u32;
        for idx in from..to {
            let digit = self.doc[idx]
                .to_digit(16)
                .ok_or_else(|| decode_error(self.doc, "Invalid \\uXXXX escape", to - 5))?;
            code = (code << 4) | digit;
        }
        Ok(code)
    }

    /// `_match_number_unicode`; `None` is Python's `StopIteration(start)`.
    fn number(&self, start: usize) -> Result<Option<(PyValue, usize)>, LoadError> {
        let doc = self.doc;
        let digit = |idx: usize| doc.get(idx).is_some_and(char::is_ascii_digit);
        let end_idx = doc.len() - 1;
        let mut idx = start;
        if doc[idx] == '-' {
            idx += 1;
            if idx > end_idx {
                return Ok(None);
            }
        }
        if ('1'..='9').contains(&doc[idx]) {
            idx += 1;
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
        } else if doc[idx] == '0' {
            idx += 1;
        } else {
            return Ok(None);
        }
        let mut is_float = false;
        if idx < end_idx && doc[idx] == '.' && digit(idx + 1) {
            is_float = true;
            idx += 2;
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
        }
        if idx < end_idx && matches!(doc[idx], 'e' | 'E') {
            let e_start = idx;
            idx += 1;
            if idx < end_idx && matches!(doc[idx], '-' | '+') {
                idx += 1;
            }
            while idx <= end_idx && digit(idx) {
                idx += 1;
            }
            if digit(idx - 1) {
                is_float = true;
            } else {
                idx = e_start;
            }
        }
        let text: String = doc[start..idx].iter().collect();
        if is_float {
            let value: f64 = text.parse().unwrap_or(f64::NAN);
            return Ok(Some((PyValue::Float(value), idx)));
        }
        let digits = text.trim_start_matches('-').len();
        if digits > INT_MAX_STR_DIGITS {
            return Err(LoadError::Value(format!(
                "Exceeds the limit ({INT_MAX_STR_DIGITS} digits) for integer string conversion: \
                 value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
            )));
        }
        let value = text
            .parse::<i128>()
            .map_or_else(|_| PyValue::BigInt(text.clone()), PyValue::Int);
        Ok(Some((value, idx)))
    }
}

/// Where one scan step landed.
enum Step {
    /// A finished value and the index after it.
    Value(PyValue, usize),
    /// A container opened; scanning continues inside it at the index.
    Open(Frame, usize),
}

/// `scan_once_unicode` at `idx`: `Err(None)` is `StopIteration(idx)`.
fn scan_value(scanner: &Scanner<'_>, idx: usize, depth: usize) -> Result<Step, Option<LoadError>> {
    let Some(ch) = scanner.at(idx) else {
        return Err(None);
    };
    let constant = |word: &str, value: PyValue| {
        scanner
            .matches(idx, word)
            .then(|| Step::Value(value, idx + word.chars().count()))
    };
    let found = match ch {
        '"' => {
            let (text, next) = scanner.string(idx).map_err(Some)?;
            return Ok(Step::Value(PyValue::Str(text), next));
        }
        '{' | '[' => {
            if depth >= MAX_DEPTH {
                let what = if ch == '{' { "object" } else { "array" };
                return Err(Some(LoadError::Recursion(format!(
                    "maximum recursion depth exceeded while decoding a JSON {what} from a unicode string"
                ))));
            }
            let frame = if ch == '{' {
                Frame::Dict {
                    pairs: Vec::new(),
                    key: None,
                }
            } else {
                Frame::List(Vec::new())
            };
            return Ok(Step::Open(frame, idx + 1));
        }
        'n' => constant("null", PyValue::None),
        't' => constant("true", PyValue::Bool(true)),
        'f' => constant("false", PyValue::Bool(false)),
        'N' => constant("NaN", PyValue::Float(f64::NAN)),
        'I' => constant("Infinity", PyValue::Float(f64::INFINITY)),
        '-' => constant("-Infinity", PyValue::Float(f64::NEG_INFINITY)),
        _ => None,
    };
    if let Some(step) = found {
        return Ok(step);
    }
    match scanner.number(idx).map_err(Some)? {
        Some((value, next)) => Ok(Step::Value(value, next)),
        None => Err(None),
    }
}

/// Insert one decoded pair: `dict(pairs)` keeps a repeated key at its
/// first position with the last value.
fn insert_pair(pairs: &mut Vec<(PyValue, PyValue)>, key: String, value: PyValue) {
    if let Some(slot) = pairs
        .iter_mut()
        .find(|(existing, _)| existing.as_str() == Some(key.as_str()))
    {
        slot.1 = value;
    } else {
        pairs.push((PyValue::Str(key), value));
    }
}

/// `json.loads(text)` (iterative: nesting is bounded by [`MAX_DEPTH`],
/// never by this thread's stack).
///
/// # Errors
///
/// Returns the error Python raises for the same text.
pub fn loads(text: &str) -> Result<PyValue, LoadError> {
    let doc: Vec<char> = text.chars().collect();
    if doc.first() == Some(&'\u{feff}') {
        return Err(decode_error(
            &doc,
            "Unexpected UTF-8 BOM (decode using utf-8-sig)",
            0,
        ));
    }
    let scanner = Scanner { doc: &doc };
    let expecting = |pos: usize| decode_error(&doc, "Expecting value", pos);
    let mut stack: Vec<Frame> = Vec::new();
    let mut idx = scanner.skip_ws(0);
    // `pending`: a value the innermost frame must take next; `None` while
    // the scan must read a value first.
    let mut pending: Option<(PyValue, usize)> = None;
    loop {
        let (value, next) = if let Some(done) = pending.take() {
            done
        } else {
            // Where the next value starts inside the innermost container.
            match stack.last_mut() {
                Some(Frame::List(items)) if items.is_empty() => {
                    idx = scanner.skip_ws(idx);
                    if scanner.at(idx) == Some(']') {
                        stack.pop();
                        pending = Some((PyValue::List(Vec::new()), idx + 1));
                        continue;
                    }
                }
                Some(Frame::Dict { pairs, key }) if key.is_none() => {
                    if pairs.is_empty() {
                        idx = scanner.skip_ws(idx);
                        if scanner.at(idx) == Some('}') {
                            stack.pop();
                            pending = Some((PyValue::Dict(Vec::new()), idx + 1));
                            continue;
                        }
                    }
                    if scanner.at(idx) != Some('"') {
                        return Err(decode_error(
                            &doc,
                            "Expecting property name enclosed in double quotes",
                            idx,
                        ));
                    }
                    let (name, after) = scanner.string(idx)?;
                    idx = scanner.skip_ws(after);
                    if scanner.at(idx) != Some(':') {
                        return Err(decode_error(&doc, "Expecting ':' delimiter", idx));
                    }
                    idx = scanner.skip_ws(idx + 1);
                    *key = Some(name);
                }
                Some(Frame::List(_) | Frame::Dict { .. }) | None => {}
            }
            match scan_value(&scanner, idx, stack.len()) {
                Ok(Step::Value(value, next)) => (value, next),
                Ok(Step::Open(frame, next)) => {
                    stack.push(frame);
                    idx = next;
                    continue;
                }
                Err(Some(error)) => return Err(error),
                Err(None) => return Err(expecting(idx)),
            }
        };
        // Hand the value to the innermost container, then read its
        // delimiter.
        let Some(frame) = stack.last_mut() else {
            let end = scanner.skip_ws(next);
            if end != doc.len() {
                return Err(decode_error(&doc, "Extra data", end));
            }
            return Ok(value);
        };
        idx = scanner.skip_ws(next);
        match frame {
            Frame::List(items) => {
                items.push(value);
                match scanner.at(idx) {
                    Some(']') => {
                        let items = std::mem::take(items);
                        stack.pop();
                        pending = Some((PyValue::List(items), idx + 1));
                    }
                    Some(',') => idx = scanner.skip_ws(idx + 1),
                    _ => return Err(decode_error(&doc, "Expecting ',' delimiter", idx)),
                }
            }
            Frame::Dict { pairs, key } => {
                let name = key.take().unwrap_or_default();
                insert_pair(pairs, name, value);
                match scanner.at(idx) {
                    Some('}') => {
                        let pairs = std::mem::take(pairs);
                        stack.pop();
                        pending = Some((PyValue::Dict(pairs), idx + 1));
                    }
                    Some(',') => idx = scanner.skip_ws(idx + 1),
                    _ => return Err(decode_error(&doc, "Expecting ',' delimiter", idx)),
                }
            }
        }
    }
}

/// `json.dumps(value)`: `", "`/`": "` separators, `ensure_ascii`, and
/// `NaN`/`Infinity` for non-finite floats. A value JSON has no spelling for
/// (an opaque object, a non-scalar key) is `TypeError`'s message.
///
/// # Errors
///
/// Returns the `TypeError` message `json.dumps` raises for the value.
pub fn dumps(value: &PyValue) -> Result<String, String> {
    let mut out = String::new();
    write_json(value, &mut out)?;
    Ok(out)
}

fn write_json(value: &PyValue, out: &mut String) -> Result<(), String> {
    match value {
        PyValue::None => out.push_str("null"),
        PyValue::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        PyValue::Int(number) => {
            let _ = write!(out, "{number}");
        }
        PyValue::BigInt(digits) => out.push_str(digits),
        PyValue::Float(number) => out.push_str(&json_float(*number)),
        PyValue::Str(text) => push_ascii_string(out, text),
        PyValue::List(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_json(item, out)?;
            }
            out.push(']');
        }
        PyValue::Dict(pairs) => {
            out.push('{');
            for (position, (key, item)) in pairs.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                let key = match key {
                    PyValue::Str(text) => text.clone(),
                    PyValue::Bool(flag) => if *flag { "true" } else { "false" }.to_string(),
                    PyValue::None => "null".to_string(),
                    PyValue::Int(number) => number.to_string(),
                    PyValue::BigInt(digits) => digits.clone(),
                    PyValue::Float(number) => json_float(*number),
                    PyValue::List(_) | PyValue::Dict(_) | PyValue::Opaque { .. } => {
                        return Err(format!(
                            "keys must be str, int, float, bool or None, not {}",
                            type_name(key)
                        ))
                    }
                };
                push_ascii_string(out, &key);
                out.push_str(": ");
                write_json(item, out)?;
            }
            out.push('}');
        }
        PyValue::Opaque {
            json: Some(text), ..
        } => out.push_str(text),
        PyValue::Opaque { json: None, .. } => {
            return Err(format!(
                "Object of type {} is not JSON serializable",
                type_name(value)
            ))
        }
    }
    Ok(())
}

/// The Python type name of a value (`type(value).__name__`).
#[must_use]
pub fn type_name(value: &PyValue) -> &str {
    match value {
        PyValue::None => "NoneType",
        PyValue::Bool(_) => "bool",
        PyValue::Int(_) | PyValue::BigInt(_) => "int",
        PyValue::Float(_) => "float",
        PyValue::Str(_) => "str",
        PyValue::List(_) => "list",
        PyValue::Dict(_) => "dict",
        PyValue::Opaque { type_name, .. } => type_name,
    }
}

/// `float.__repr__` with JSON's spellings of the non-finite values.
fn json_float(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_string()
    } else if number.is_infinite() {
        if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .to_string()
    } else {
        py_float_repr(number)
    }
}

/// `py_encode_basestring_ascii`: every non-ASCII character as `\uXXXX`
/// (astral ones as a surrogate pair).
fn push_ascii_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0_u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}
