//! `JSON.stringify` with V8's output: keys in insertion order (the
//! workspace's `serde_json` keeps it), numbers as ECMAScript
//! `Number::toString`, and the two-space pretty layout of
//! `JSON.stringify(value, null, 2)`.

use serde_json::Value;

/// A JSON number for `value`: integral values below 2^53 as integers (so
/// they print without a fraction), anything else as a float that
/// [`stringify_pretty`] prints the JavaScript way. Non-finite values are
/// `null`, as `JSON.stringify` writes them.
#[must_use]
pub(crate) fn number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    serde_json::to_value(pa_types::JsNumber(value)).unwrap_or(Value::Null)
}

/// `JSON.stringify(value, null, 2)`.
#[must_use]
pub(crate) fn stringify_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                out.push_str(&number.to_string());
            } else {
                out.push_str(&js_number(number.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(text) => out.push_str(&json_string(text)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                indent(out, depth + 1);
                write_value(out, item, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                indent(out, depth + 1);
                out.push_str(&json_string(key));
                out.push_str(": ");
                write_value(out, item, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push('}');
        }
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// `JSON.stringify(text)`: `serde_json` escapes exactly the characters V8
/// does for a well-formed string.
fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| String::from("\"\""))
}

/// ECMAScript `Number::toString` for a finite double over the shortest
/// round-trip digits.
#[must_use]
pub(crate) fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` is the shortest round-trip form: `d[.ddd]e<exp>`.
    let formatted = format!("{:e}", value.abs());
    let (mantissa, exponent) = formatted
        .split_once('e')
        .unwrap_or((formatted.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digit_count = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    // The decimal point sits `point` digits into `digits`.
    let point = exponent.parse::<i64>().unwrap_or(0) + 1;
    let count = |value: i64| usize::try_from(value).unwrap_or(0);
    let body = if digit_count <= point && point <= 21 {
        format!("{digits}{}", "0".repeat(count(point - digit_count)))
    } else if 0 < point && point <= 21 {
        let (whole, fraction) = digits.split_at(count(point));
        format!("{whole}.{fraction}")
    } else if -6 < point && point <= 0 {
        format!("0.{}{digits}", "0".repeat(count(-point)))
    } else {
        let exp = point - 1;
        let exp_sign = if exp < 0 { "-" } else { "+" };
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() { "" } else { "." };
        format!("{first}{dot}{rest}e{exp_sign}{}", exp.abs())
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pretty_output_is_json_stringify_with_two_spaces() {
        let value = json!({
            "a": [],
            "b": {},
            "c": [1, number(0.5), number(0.000_001_5), number(40.0), null, "x\"y"],
            "d": { "e": true }
        });
        assert_eq!(
            stringify_pretty(&value),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    0.5,\n    0.0000015,\n    40,\n    null,\n    \"x\\\"y\"\n  ],\n  \"d\": {\n    \"e\": true\n  }\n}"
        );
        assert_eq!(js_number(1.5e-7), "1.5e-7");
        assert_eq!(js_number(-0.25), "-0.25");
        assert_eq!(number(f64::NAN), Value::Null);
    }
}
