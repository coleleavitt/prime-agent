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

/// Local cap (characters) on the settle capture's binding lane. Collect
/// carries the child's full final answer (`answer_text`, host-bounded);
/// input binding and output capture work on that text, not the roster
/// preview, so a fenced JSON output longer than the preview binds whole
/// (upstream #3462's M2 class). A bind failure names this cap when the
/// capture was cut at it.
pub const ANSWER_BINDING_CAP: usize = 8192;

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
    write_json(value, &mut out);
    out
}

fn write_json(value: &Value, out: &mut String) {
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
                    out.push_str(", ");
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (position, (key, item)) in map.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_json_string(key, out);
                out.push_str(": ");
                write_json(item, out);
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

/// Extract one named JSON output from an upstream answer: the fenced json
/// block whose object carries the port (trailing blocks first), else the
/// whole answer parsed as one object.
///
/// # Errors
///
/// Returns the binding-failure sentence when no candidate carries the port.
pub fn parse_json_output(answer: &str, output_name: &str) -> Result<Value, String> {
    let mut candidates: Vec<&str> = fenced_json_blocks(answer).into_iter().rev().collect();
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
