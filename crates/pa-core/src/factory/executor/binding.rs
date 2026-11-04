//! Pure binding helpers: output capture, prompt rendering, guard
//! evaluation, and the Python-compatible JSON text the bound values carry.
//!
//! A bound json value is rendered the way the original Python executor
//! rendered it (`json.dumps` defaults: `", "`/`": "` separators, ASCII
//! escapes, Python float spelling), so child prompts stay byte-identical.

use std::cmp::Ordering;
use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

use crate::factory::pyvalue::{PyValue, py_float_repr, py_repr};

/// Local cap (characters) on a captured answer's PREVIEW: the
/// `answer_captured` ledger event and the status node's `answer_preview`
/// stay compact (the host's own roster previews cap at 160 characters).
pub const ANSWER_CAPTURE_CAP: usize = 200;

/// Local cap (characters) on the settle capture's binding lane, at parity
/// with the host's collect bound (the daemon's `ANSWER_TEXT_MAX_CHARS`:
/// 65,536 characters). Collect carries the child's full final answer
/// (`answer_text`, host-bounded at that size); input binding and output
/// capture work on that text, not the roster preview, so a fenced JSON
/// output longer than the preview binds whole (upstream #3462's M2 class).
/// The old 8192-character cap cut a valid fenced JSON between the two
/// bounds mid-object before the parse. The cap still bounds each captured
/// answer the executor stores, and a bind failure names the stored
/// answer's length when it reaches the cap.
pub const ANSWER_BINDING_CAP: usize = 65_536;

const RATE_LIMIT_MARKERS: [&str; 8] = [
    "rate limit",
    "rate-limit",
    "ratelimit",
    "429",
    "too many requests",
    "throttled",
    "quota",
    "usage limit",
];

/// Heuristic: the host reports admission failures as error strings.
#[must_use]
pub fn is_rate_limit_error(message: &str) -> bool {
    let lowered = message.to_lowercase();
    RATE_LIMIT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// The first `count` characters (a Python slice `text[:count]`).
#[must_use]
pub fn char_prefix(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

/// Python's `repr()` of a JSON value (state ids, `from` fields).
#[must_use]
pub fn json_repr(value: &Value) -> String {
    py_repr(&PyValue::from_json(value))
}

/// Python's `json.dumps(value)` with default arguments.
#[must_use]
pub fn py_json_dumps(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, Separators::DEFAULT, &mut out);
    out
}

/// Python's `json.dumps(value, separators=(",", ":"))`: the compact text a
/// python state's outputs are captured as.
#[must_use]
pub fn py_json_dumps_compact(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, Separators::COMPACT, &mut out);
    out
}

/// `json.dumps`'s `(item, key)` separators.
#[derive(Clone, Copy)]
struct Separators {
    item: &'static str,
    key: &'static str,
}

impl Separators {
    const DEFAULT: Self = Self {
        item: ", ",
        key: ": ",
    };
    const COMPACT: Self = Self {
        item: ",",
        key: ":",
    };
}

fn write_json(value: &Value, separators: Separators, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number_text(number)),
        Value::String(text) => write_json_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(separators.item);
                }
                write_json(item, separators, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (position, (key, item)) in map.iter().enumerate() {
                if position > 0 {
                    out.push_str(separators.item);
                }
                write_json_string(key, out);
                out.push_str(separators.key);
                write_json(item, separators, out);
            }
            out.push('}');
        }
    }
}

fn number_text(number: &Number) -> String {
    if number.is_f64() {
        py_float_repr(number.as_f64().unwrap_or(f64::NAN))
    } else {
        number.to_string()
    }
}

/// `json.dumps` string escaping with `ensure_ascii=True`.
fn write_json_string(text: &str, out: &mut String) {
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
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// The text a bound value renders as: a string verbatim, anything else as
/// its JSON text (the Python executor's `value if isinstance(value, str)
/// else json.dumps(value)`).
#[must_use]
pub fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_json_dumps(other),
    }
}

/// Python `str.isspace()` per character (the `\s` class of a str regex).
fn is_py_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// The payloads of every fenced json block (an opening fence spelled
/// with the `json` tag), in order: the original regex scan's `findall`
/// with lazy, whitespace-trimmed bodies.
fn fenced_json_blocks(answer: &str) -> Vec<&str> {
    const OPEN: &str = "```json";
    const CLOSE: &str = "```";
    let mut blocks = Vec::new();
    let mut rest = answer;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        let body = after.trim_start_matches(is_py_space);
        let Some(close) = body.find(CLOSE) else {
            break;
        };
        blocks.push(body[..close].trim_end_matches(is_py_space));
        rest = &body[close + CLOSE.len()..];
    }
    blocks
}

/// The bare JSON object that terminates the answer, if one does.
///
/// A string-aware balanced-brace scan walks the answer's top-level object
/// spans (braces inside string literals never count); the terminal object
/// is the last balanced span that only benign residue follows (whitespace
/// or punctuation such as fence fragments, dashes, or dots; never letters,
/// digits, quotes, or more brackets). That is the settle shape the strict
/// fence contract rejected: a JSON tail emitted as plain text after prose,
/// an unclosed fence, or a dirty tail. Prose is never JSON syntax: quotes
/// outside an object do not open strings, and a brace in prose only opens
/// a candidate where it starts a string key. An unterminated candidate is
/// a truncated emit and owns the tail, so nothing later binds.
fn terminal_json_object(answer: &str) -> Option<&str> {
    let chars: Vec<(usize, char)> = answer.char_indices().collect();
    let length = chars.len();
    let mut last: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < length {
        if chars[index].1 != '{' {
            index += 1;
            continue;
        }
        // A candidate opens only where its brace starts a string key.
        let mut cursor = index + 1;
        while cursor < length && is_py_space(chars[cursor].1) {
            cursor += 1;
        }
        if cursor == length || chars[cursor].1 != '"' {
            index += 1;
            continue;
        }
        let mut depth = 1;
        let mut in_string = false;
        let mut escaped = false;
        while cursor < length {
            let ch = chars[cursor].1;
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
            } else if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    last = Some((index, cursor + 1));
                    cursor += 1;
                    break;
                }
            }
            cursor += 1;
        }
        if depth > 0 {
            return None;
        }
        index = cursor;
    }
    let (start, end) = last?;
    let byte = |position: usize| {
        chars
            .get(position)
            .map_or(answer.len(), |(offset, _)| *offset)
    };
    let (start, end) = (byte(start), byte(end));
    let dirty = answer[end..]
        .chars()
        .any(|ch| ch.is_alphanumeric() || "\"'{}[]\\".contains(ch));
    (!dirty).then(|| &answer[start..end])
}

/// Extract one named JSON output from an upstream answer: the fenced json
/// block whose object carries the port (trailing blocks first), then the
/// answer's terminal bare JSON object, else the whole answer parsed as one
/// object. Only an object that contains the port binds, whatever its shape.
///
/// # Errors
///
/// Returns the binding-failure sentence when no candidate carries the port.
pub fn parse_json_output(answer: &str, output_name: &str) -> Result<Value, String> {
    let mut candidates: Vec<&str> = fenced_json_blocks(answer).into_iter().rev().collect();
    candidates.extend(terminal_json_object(answer));
    candidates.push(answer.trim_matches(is_py_space));
    for candidate in candidates {
        let Ok(Value::Object(mut parsed)) = serde_json::from_str::<Value>(candidate) else {
            continue;
        };
        if let Some(value) = parsed.remove(output_name) {
            return Ok(value);
        }
    }
    Err(format!(
        "no JSON object containing output {} in the upstream answer",
        crate::factory::pyvalue::py_str_repr(output_name)
    ))
}

/// Render bound input values into a prompt template: each `{name}`
/// placeholder is replaced in a single left-to-right pass (a value that
/// looks like a placeholder is never re-substituted; at one position the
/// first declared placeholder wins), and inputs without a placeholder are
/// appended in a trailing `## Inputs` section so no bound value is dropped.
#[must_use]
pub fn render_prompt(template: &str, values: &[(String, String)]) -> String {
    if values.is_empty() {
        return template.to_string();
    }
    let placeholders: Vec<String> = values
        .iter()
        .map(|(name, _)| format!("{{{name}}}"))
        .collect();
    let mut used = vec![false; values.len()];
    let mut rendered = String::with_capacity(template.len());
    let mut position = 0;
    while position < template.len() {
        let rest = &template[position..];
        let matched = placeholders
            .iter()
            .position(|placeholder| rest.starts_with(placeholder.as_str()));
        if let Some(index) = matched {
            rendered.push_str(&values[index].1);
            used[index] = true;
            position += placeholders[index].len();
            continue;
        }
        let ch = rest.chars().next().unwrap_or_default();
        rendered.push(ch);
        position += ch.len_utf8();
    }
    let unplaced: Vec<&(String, String)> = values
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|(value, _)| value)
        .collect();
    if !unplaced.is_empty() {
        rendered.push_str("\n\n## Inputs\n");
        for (name, value) in unplaced {
            let _ = writeln!(rendered, "- {name}: {value}");
        }
    }
    rendered
}

/// Exact numeric comparison with Python semantics (int/int and int/float
/// compare mathematically; any NaN compares unordered).
fn compare_numbers(a: &Number, b: &Number) -> Option<Ordering> {
    let int = |number: &Number| {
        number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
    };
    match (int(a), int(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        (Some(x), None) => compare_int_float(x, b.as_f64()?),
        (None, Some(y)) => compare_int_float(y, a.as_f64()?).map(Ordering::reverse),
        (None, None) => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

fn compare_int_float(int: i128, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float.is_infinite() {
        return Some(if float > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let whole = float.trunc();
    if whole.abs() >= 1.0e38 {
        return Some(if whole > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let whole_int = whole as i128;
    match int.cmp(&whole_int) {
        Ordering::Equal => 0.0_f64.partial_cmp(&(float - whole)),
        other => Some(other),
    }
}

/// JSON-strict equality for eq/ne guards: a boolean never equals a number,
/// numbers compare numerically (exactly), and everything else compares
/// within its own type (containers never equal here).
#[must_use]
pub fn json_equal(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        (Value::Number(a), Value::Number(b)) => compare_numbers(a, b) == Some(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a == b,
        // A bool against a number, and every cross-type pair.
        _ => false,
    }
}

/// Python's `==` between JSON values (`True == 1`, `1 == 1.0`): the
/// membership test `contains` guards use.
fn py_eq(a: &Value, b: &Value) -> bool {
    let numeric = |value: &Value| match value {
        Value::Bool(flag) => Some(Number::from(i64::from(*flag))),
        Value::Number(number) => Some(number.clone()),
        _ => None,
    };
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, p)| y.get(key).is_some_and(|q| py_eq(p, q)))
        }
        _ => match (numeric(a), numeric(b)) {
            (Some(x), Some(y)) => compare_numbers(&x, &y) == Some(Ordering::Equal),
            _ => false,
        },
    }
}

/// Evaluate one transition guard over a settle's captured outputs. A
/// missing or unparseable port fails every op except `exists` (explicitly
/// false then); `ne` needs a found value; an empty `contains` needle is
/// defensively false.
#[must_use]
pub fn guard_passes(when: &Value, outputs: &Map<String, Value>) -> bool {
    let port = when
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let path = when
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let mut found = false;
    let mut value: &Value = &Value::Null;
    if let Some(path) = path {
        if let Some(current @ Value::Object(_)) = outputs.get(port) {
            found = true;
            let mut current = current;
            for part in path.split('.') {
                match current.get(part) {
                    Some(next) if current.is_object() => current = next,
                    _ => {
                        found = false;
                        break;
                    }
                }
            }
            value = current;
        }
    } else if let Some(port_value) = outputs.get(port) {
        found = true;
        value = port_value;
    }
    let op = when.get("op").and_then(Value::as_str).unwrap_or_default();
    if op == "exists" {
        return found;
    }
    if !found {
        return false;
    }
    let expected = when.get("value").unwrap_or(&Value::Null);
    match op {
        "eq" => json_equal(value, expected),
        "ne" => !json_equal(value, expected),
        "gt" | "gte" | "lt" | "lte" => {
            let (Value::Number(actual), Value::Number(bound)) = (value, expected) else {
                return false;
            };
            let Some(ordering) = compare_numbers(actual, bound) else {
                return false;
            };
            match op {
                "gt" => ordering == Ordering::Greater,
                "gte" => ordering != Ordering::Less,
                "lt" => ordering == Ordering::Less,
                _ => ordering != Ordering::Greater,
            }
        }
        "contains" => {
            let Some(needle) = expected.as_array().filter(|needle| !needle.is_empty()) else {
                return false;
            };
            match value {
                Value::Array(haystack) => needle
                    .iter()
                    .all(|item| haystack.iter().any(|candidate| py_eq(candidate, item))),
                Value::String(haystack) => needle
                    .iter()
                    .all(|item| item.as_str().is_some_and(|item| haystack.contains(item))),
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn outputs(value: &Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    // Port of `test_json_output_binds_from_the_fence_that_carries_the_port`
    // (`_parse_json_output` moved here from the kernel).
    #[test]
    fn json_output_binds_from_the_fence_that_carries_the_port() {
        let two_fences = "```json\n{\"o\": 1}\n```\n\n```json\n{\"summary\": \"s\"}\n```";
        assert_eq!(parse_json_output(two_fences, "o"), Ok(json!(1)));
        assert_eq!(parse_json_output(two_fences, "summary"), Ok(json!("s")));
        let both = "```json\n{\"o\": \"first\"}\n```\n\n```json\n{\"o\": \"last\"}\n```";
        assert_eq!(parse_json_output(both, "o"), Ok(json!("last")));
        assert_eq!(parse_json_output("{\"o\": 2}", "o"), Ok(json!(2)));
        assert_eq!(
            parse_json_output(two_fences, "missing"),
            Err("no JSON object containing output 'missing' in the upstream answer".to_string())
        );
    }

    fn missing(output: &str) -> Result<Value, String> {
        Err(format!(
            "no JSON object containing output '{output}' in the upstream answer"
        ))
    }

    // Port of `test_json_output_binds_from_terminal_bare_and_dirty_tails`:
    // the settle shapes the strict fence rejected bind the same declared
    // port with the same output-name requirement.
    #[test]
    fn json_output_binds_from_terminal_bare_and_dirty_tails() {
        let cases = [
            // A JSON tail emitted as plain text after prose.
            (
                "Gate ran clean on the worktree.\n\n{\"verdict\": \"pass\", \"head\": \"abc\"}",
                Ok(json!("pass")),
            ),
            // A fence that lost its closing markers.
            ("```json\n{\"verdict\": \"pass\"}", Ok(json!("pass"))),
            // Benign punctuation residue around the emit.
            ("---\n{\"verdict\": \"pass\"}\n``` --", Ok(json!("pass"))),
            // Braces inside string literals never break the balanced scan.
            (
                "note: {\"verdict\": \"pass\", \"detail\": \"quoted } and { inside\"}",
                Ok(json!("pass")),
            ),
            // A fence still wins over the bare tail, and the terminal emit
            // is the LAST object when several are bare.
            (
                "```json\n{\"verdict\": \"fenced\"}\n```\n\n{\"verdict\": \"bare\"}",
                Ok(json!("fenced")),
            ),
            (
                "{\"verdict\": \"stale\"}\n.\n{\"verdict\": \"last\"}",
                Ok(json!("last")),
            ),
            // An object trailing prose follows is not terminal; a terminal
            // object without the port, and prose alone, fail the same way.
            (
                "{\"verdict\": \"pass\"}\n\nAll checks green, done.",
                missing("verdict"),
            ),
            ("gate done\n\n{\"status\": \"done\"}", missing("verdict")),
            (
                "The gate ran clean; there is nothing to bind.",
                missing("verdict"),
            ),
        ];
        for (answer, expected) in cases {
            assert_eq!(parse_json_output(answer, "verdict"), expected, "{answer:?}");
        }
    }

    // Port of `test_terminal_object_survives_prose_delimiters`: syntax
    // tracks within candidate objects only, so prose that leaves a quote or
    // a brace open never hides the tail object, and a truncated emit owns
    // the tail.
    #[test]
    fn terminal_object_survives_prose_delimiters() {
        let cases = [
            (
                "The gate said \"all green, and\n\n{\"verdict\": \"pass\"}",
                Ok(json!("pass")),
            ),
            (
                "here is the { stray brace in prose\n\n{\"verdict\": \"pass\"}",
                Ok(json!("pass")),
            ),
            (
                "{\"verdict\": \"stale\"} and \"so on\n\n{\"verdict\": \"last\"}",
                Ok(json!("last")),
            ),
            ("{\"outer\": {\"verdict\": \"x\"}", missing("verdict")),
            (
                "{\"verdict\": \"pass\"}\nAll checks green, \"done.\"",
                missing("verdict"),
            ),
        ];
        for (answer, expected) in cases {
            assert_eq!(parse_json_output(answer, "verdict"), expected, "{answer:?}");
        }
    }

    // Port of `test_guard_primitives_json_strict_and_defensive`
    // (`_guard_passes` moved here from the kernel).
    #[test]
    fn guard_primitives_json_strict_and_defensive() {
        let found =
            outputs(&json!({ "verdict": { "approved": true, "count": 1.0, "tags": ["a"] } }));
        let when = |extra: Value| {
            let mut guard = json!({ "output": "verdict" });
            if let (Some(guard), Some(extra)) = (guard.as_object_mut(), extra.as_object()) {
                guard.extend(extra.clone());
            }
            guard
        };
        let big = outputs(&json!({ "verdict": { "n": 9_007_199_254_740_993_u64 } }));
        let verdicts = [
            guard_passes(
                &when(json!({ "path": "approved", "op": "eq", "value": 1 })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "approved", "op": "eq", "value": 0 })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "approved", "op": "ne", "value": 1 })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "count", "op": "eq", "value": 1 })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "count", "op": "lte", "value": 1.5 })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "tags", "op": "contains", "value": ["a"] })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "tags", "op": "contains", "value": [] })),
                &found,
            ),
            guard_passes(
                &when(json!({ "path": "gone", "op": "ne", "value": 1 })),
                &found,
            ),
            guard_passes(&when(json!({ "path": "gone", "op": "exists" })), &found),
            guard_passes(&when(json!({ "path": "approved", "op": "exists" })), &found),
            guard_passes(
                &when(json!({ "path": "n", "op": "eq", "value": 9_007_199_254_740_993_u64 })),
                &big,
            ),
            guard_passes(
                &when(json!({ "path": "n", "op": "eq", "value": 9_007_199_254_740_992_u64 })),
                &big,
            ),
            guard_passes(
                &when(json!({ "path": "n", "op": "ne", "value": 9_007_199_254_740_992_u64 })),
                &big,
            ),
            guard_passes(
                &when(json!({ "path": "count", "op": "eq", "value": 1.0 })),
                &found,
            ),
        ];
        assert_eq!(
            verdicts,
            [
                false, false, true, true, true, true, false, false, false, true, true, false, true,
                true
            ]
        );
    }

    #[test]
    fn dumps_matches_python_json_defaults() {
        let value = json!({ "a": [1, 2.5, 1e16, null, true], "s": "caf\u{e9} \"q\"\n\u{1F600}" });
        assert_eq!(
            py_json_dumps(&value),
            "{\"a\": [1, 2.5, 1e+16, null, true], \"s\": \"caf\\u00e9 \\\"q\\\"\\n\\ud83d\\ude00\"}"
        );
    }

    #[test]
    fn render_substitutes_once_and_appends_unplaced_inputs() {
        let values = vec![
            ("a".to_string(), "{b}".to_string()),
            ("b".to_string(), "B".to_string()),
            ("c".to_string(), "C".to_string()),
        ];
        assert_eq!(
            render_prompt("x {a} {b}", &values),
            "x {b} B\n\n## Inputs\n- c: C\n"
        );
        assert_eq!(render_prompt("plain", &[]), "plain");
    }

    #[test]
    fn rate_limits_are_detected_by_marker() {
        let verdicts = [
            is_rate_limit_error("429 rate limit exceeded"),
            is_rate_limit_error("Quota exhausted"),
            is_rate_limit_error("unsupported thinking level"),
        ];
        assert_eq!(verdicts, [true, true, false]);
    }
}
