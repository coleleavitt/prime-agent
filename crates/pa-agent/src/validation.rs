//! Tool-argument validation, the TS `validateToolArguments` contract: the
//! JSON Schema subset the product's tool schemas use, with the same
//! primitive coercion behavior. The error message format matches the TS
//! reference exactly so surfaced text is identical.

use serde_json::Value;

/// Validates tool call arguments against the tool's JSON Schema, returning
/// the validated (and potentially coerced) arguments.
///
/// # Errors
///
/// Returns the preformatted validation error message (TS throws
/// `Error(message)`; the caller wraps it into an error tool result).
pub fn validate_tool_arguments(
    tool_name: &str,
    schema: &Value,
    arguments: &Value,
) -> Result<Value, String> {
    let mut args = arguments.clone();
    coerce(schema, &mut args);
    let mut errors = Vec::new();
    check(schema, &args, "", &mut errors);
    if errors.is_empty() {
        return Ok(args);
    }
    let error_lines = errors
        .iter()
        .map(|(path, message)| format!("  - {path}: {message}"))
        .collect::<Vec<_>>()
        .join("\n");
    let error_lines = if error_lines.is_empty() {
        "Unknown validation error".to_string()
    } else {
        error_lines
    };
    let received =
        serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string());
    Err(format!(
        "Validation failed for tool \"{tool_name}\":\n{error_lines}\n\nReceived arguments:\n{received}"
    ))
}

fn schema_type(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Primitive coercion mirroring `TypeBox` `Value.Convert`: strings parse
/// into number/boolean when the schema requests it (numbers as JS
/// `Number(text)`), and numbers/booleans
/// stringify when the schema requests a string.
fn coerce(schema: &Value, value: &mut Value) {
    let types = schema_type(schema);
    if types.is_empty() {
        coerce_children(schema, value);
        return;
    }
    for ty in types {
        match (ty, &*value) {
            // TS converts a non-blank string through JS `Number(text)` and
            // keeps it only when finite (an integer, for `integer`).
            ("number" | "integer", Value::String(s)) if !pa_types::js::js_trim(s).is_empty() => {
                let n = pa_types::js::js_number(s);
                if n.is_finite() && (ty == "number" || n.fract() == 0.0) {
                    *value = number_value(n);
                    return coerce_children(schema, value);
                }
            }
            ("boolean", Value::String(s)) => {
                let lower = s.trim().to_ascii_lowercase();
                if lower == "true" {
                    *value = Value::Bool(true);
                    return coerce_children(schema, value);
                }
                if lower == "false" {
                    *value = Value::Bool(false);
                    return coerce_children(schema, value);
                }
            }
            ("string", Value::Number(n)) => {
                *value = Value::String(n.to_string());
                return coerce_children(schema, value);
            }
            ("string", Value::Bool(b)) => {
                *value = Value::String(b.to_string());
                return coerce_children(schema, value);
            }
            _ => {}
        }
    }
    coerce_children(schema, value);
}

fn coerce_children(schema: &Value, value: &mut Value) {
    let properties = match schema.get("properties") {
        Some(Value::Object(p)) => p.clone(),
        _ => return,
    };
    match value {
        Value::Object(map) => {
            for (key, sub_schema) in &properties {
                if let Some(v) = map.get_mut(key) {
                    coerce(sub_schema, v);
                }
            }
            // Coerce entries under `additionalProperties: { ... }` schemas too.
            if let Some(additional_schema) = schema.get("additionalProperties") {
                if additional_schema.is_object() {
                    for (key, v) in map.iter_mut() {
                        if !properties.contains_key(key) {
                            coerce(additional_schema, v);
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(Value::Array(item_schemas)) = schema.get("items") {
                // Positional tuple validation; coerce each pair.
                for (i, item) in items.iter_mut().enumerate() {
                    if let Some(s) = item_schemas.get(i) {
                        coerce(s, item);
                    }
                }
            } else if let Some(item_schema) = schema.get("items") {
                for item in items.iter_mut() {
                    coerce(item_schema, item);
                }
            }
        }
        _ => {}
    }
}

fn number_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.223_372_036_854_776e18 {
        // The guard proves the conversion exact: whole value, |n| < 2^63
        // (JSON integers, as JS prints them, so `integer` schemas accept them).
        #[allow(clippy::cast_possible_truncation)]
        let whole = n as i64;
        Value::from(whole)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

/// Instance path formatting mirroring TS `formatValidationPath`: the
/// empty path reads as `root`.
fn format_path(path: &str) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.to_string()
    }
}

/// Check `value` against `schema`, appending `(path, message)` errors.
// One arm per JSON Schema keyword, mirroring the TS reference's shape.
#[allow(clippy::too_many_lines)]
fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<(String, String)>) {
    let types = schema_type(schema);
    if !types.is_empty() && !types.iter().any(|ty| type_matches(ty, value)) {
        let expected = types.join("/");
        let found = type_name(value);
        let base = format_path(path);
        errors.push((base, format!("Expected {expected}, received {found}")));
        // Type mismatch: deeper checks would only add noise.
        return;
    }

    if let Some(Value::Array(enum_values)) = schema.get("enum") {
        if !enum_values.iter().any(|allowed| allowed == value) {
            let base = format_path(path);
            errors.push((
                base,
                "Value did not match any of the expected enum values".to_string(),
            ));
        }
    }
    if let Some(expected_const) = schema.get("const") {
        if expected_const != value {
            let base = format_path(path);
            errors.push((
                base,
                "Value did not match the expected const value".to_string(),
            ));
        }
    }

    match value {
        Value::Object(map) if types.contains(&"object") || schema.get("properties").is_some() => {
            if let Some(Value::Object(properties)) = schema.get("properties") {
                for (key, sub_schema) in properties {
                    let sub_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    if let Some(v) = map.get(key) {
                        check(sub_schema, v, &sub_path, errors);
                    } else {
                        // Optional properties are skipped, mirroring
                        // standard JSON Schema.
                    }
                }
            }
            if let Some(Value::Array(required)) = schema.get("required") {
                for req in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(req) {
                        let base = format_path(path);
                        errors.push((base, format!("Required property '{req}' is missing")));
                    }
                }
            }
            if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
                if let Some(Value::Object(properties)) = schema.get("properties") {
                    for key in map.keys() {
                        if !properties.contains_key(key) {
                            let sub_path = if path.is_empty() {
                                key.clone()
                            } else {
                                format!("{path}.{key}")
                            };
                            errors.push((
                                sub_path,
                                "Property is not allowed by additionalProperties".to_string(),
                            ));
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(Value::Array(item_schemas)) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    let sub_path = if path.is_empty() {
                        index.to_string()
                    } else {
                        format!("{path}.{index}")
                    };
                    if let Some(sub_schema) = item_schemas.get(index) {
                        check(sub_schema, item, &sub_path, errors);
                    }
                }
            } else if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    let sub_path = if path.is_empty() {
                        index.to_string()
                    } else {
                        format!("{path}.{index}")
                    };
                    check(item_schema, item, &sub_path, errors);
                }
            }
        }
        Value::Number(n) => {
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                if n.as_f64().unwrap_or(f64::MIN) < min {
                    let base = format_path(path);
                    errors.push((
                        base,
                        format!("Expected value to be greater than or equal to {min}"),
                    ));
                }
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                if n.as_f64().unwrap_or(f64::MAX) > max {
                    let base = format_path(path);
                    errors.push((
                        base,
                        format!("Expected value to be less than or equal to {max}"),
                    ));
                }
            }
        }
        _ => {}
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::validate_tool_arguments;

    fn validate(ty: &str, raw: &str) -> Result<Value, String> {
        let schema = json!({ "type": "object", "properties": { "v": { "type": ty } } });
        validate_tool_arguments("t", &schema, &json!({ "v": raw }))
    }

    fn rejected(ty: &str, raw: &str) -> Result<Value, String> {
        Err(format!(
            "Validation failed for tool \"t\":\n  - v: Expected {ty}, received string\n\n\
             Received arguments:\n{{\n  \"v\": {}\n}}",
            Value::from(raw)
        ))
    }

    /// Numeric strings convert as JS `Number(text)` where TS's `TypeBox`
    /// `Value.Convert` and its plain-JSON-schema coercion agree (node runs
    /// of TS `validateToolArguments`): hex/binary/octal, exponents and JS
    /// white space convert; `inf`/`nan`/`Infinity` stay strings.
    #[test]
    fn numeric_strings_convert_as_js_number() {
        let cases = [
            ("number", "0x10"),
            ("number", "0b11"),
            ("number", "\u{feff}5\u{2028}"),
            ("number", "1e3"),
            ("integer", "1e3"),
            ("integer", "0o17"),
            ("integer", "9007199254740993"),
            ("number", "inf"),
            ("number", "nan"),
            ("number", "Infinity"),
            ("integer", "-Infinity"),
            ("integer", "1.5"),
        ];
        let actual: Vec<_> = cases.iter().map(|(ty, raw)| validate(ty, raw)).collect();
        assert_eq!(
            actual,
            vec![
                Ok(json!({ "v": 16 })),
                Ok(json!({ "v": 3 })),
                Ok(json!({ "v": 5 })),
                Ok(json!({ "v": 1000 })),
                Ok(json!({ "v": 1000 })),
                Ok(json!({ "v": 15 })),
                Ok(json!({ "v": 9_007_199_254_740_992_i64 })),
                rejected("number", "inf"),
                rejected("number", "nan"),
                rejected("number", "Infinity"),
                rejected("integer", "-Infinity"),
                rejected("integer", "1.5"),
            ]
        );
    }
}
