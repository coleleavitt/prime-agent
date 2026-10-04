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

/// Primitive coercion mirroring TS's plain-JSON-schema path
/// (`coercePrimitiveByType`; Rust schemas carry no `TypeBox` metadata, so
/// `Value.Convert`'s looser rules never apply): strings parse into
/// number/boolean when the schema requests it (numbers as JS `Number(text)`),
/// and numbers/booleans stringify (as JS `String(x)`) when it requests a string.
fn coerce(schema: &Value, value: &mut Value) {
    let types = schema_type(schema);
    // TS `matchesUnionMember`: under a multi-type `type`, a value that
    // already is one of the members is kept as it is (unknown type names
    // match nothing here, unlike in `check`).
    let matches_union_member = types.len() > 1
        && types.iter().any(|ty| {
            matches!(
                *ty,
                "number" | "integer" | "boolean" | "string" | "null" | "array" | "object"
            ) && type_matches(ty, value)
        });
    if types.is_empty() || matches_union_member {
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
            // A whole double under `integer` (`9.3e18`, `-2^63`) becomes the
            // JSON integer `JSON.stringify` writes for it when one holds it.
            ("integer", Value::Number(n)) if !n.is_i64() && !n.is_u64() => {
                if let Some(whole) = n.as_f64().filter(|n| n.fract() == 0.0) {
                    *value = number_value(whole);
                    return coerce_children(schema, value);
                }
            }
            // TS's plain-JSON-schema `coercePrimitiveByType`: exactly
            // "true"/"false", the numbers 1/0 and `null`; no trim, no case
            // folding ("TRUE" and "1" convert only under `TypeBox`
            // `Value.Convert`, which Rust schemas, carrying no `TypeBox`
            // metadata, never take).
            ("boolean", Value::String(s)) if s == "true" || s == "false" => {
                *value = Value::Bool(s == "true");
                return coerce_children(schema, value);
            }
            ("boolean", Value::Number(n)) => {
                if let Some(flag) = n.as_f64().and_then(js_bool_of_number) {
                    *value = Value::Bool(flag);
                    return coerce_children(schema, value);
                }
            }
            ("boolean", Value::Null) => {
                *value = Value::Bool(false);
                return coerce_children(schema, value);
            }
            // JS `String(n)` of the double TS parsed.
            ("string", Value::Number(n)) => {
                let text = n
                    .as_f64()
                    .map_or_else(|| n.to_string(), pa_types::js::js_number_to_string);
                *value = Value::String(text);
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

/// A coerced double as a JSON number: a whole value that `i64` or `u64`
/// holds exactly becomes a JSON integer (the number `JSON.stringify` writes);
/// any other finite double (a fraction, or a whole value past `u64`) stays a
/// JSON double, which `integer` still accepts when it has no fraction.
fn number_value(n: f64) -> Value {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    const TWO_64: f64 = 18_446_744_073_709_551_616.0;
    if n.fract() == 0.0 && (-TWO_63..TWO_63).contains(&n) {
        // Whole and inside [-2^63, 2^63): the conversion is exact.
        #[allow(clippy::cast_possible_truncation)]
        let whole = n as i64;
        Value::from(whole)
    } else if n.fract() == 0.0 && (TWO_63..TWO_64).contains(&n) {
        // Whole and inside [2^63, 2^64): the conversion is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = n as u64;
        Value::from(whole)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

/// TS's `value === 1` / `value === 0` (so `1.0` and `-0` count).
#[expect(clippy::float_cmp, reason = "JS strict equality against exact 1 and 0")]
fn js_bool_of_number(n: f64) -> Option<bool> {
    if n == 1.0 {
        Some(true)
    } else if n == 0.0 {
        Some(false)
    } else {
        None
    }
}

/// JS `Number.isInteger`: a JSON integer, or a finite double with no
/// fraction (`1e20` is an integer to TS though no Rust integer type holds it).
fn is_js_integer(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            n.is_i64()
                || n.is_u64()
                || n.as_f64()
                    .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        }
        _ => false,
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
        Value::Number(_) => {
            if is_js_integer(value) {
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
        "integer" => is_js_integer(value),
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

    /// Validate one argument `v` given as JSON text, as a provider delivers it.
    fn validate_json(ty: &str, raw: &str) -> Result<Value, String> {
        let schema = json!({ "type": "object", "properties": { "v": { "type": ty } } });
        let arguments: Value =
            serde_json::from_str(&format!("{{\"v\":{raw}}}")).expect("argument JSON");
        validate_tool_arguments("t", &schema, &arguments)
    }

    fn mismatch(ty: &str, raw: &str, received: &str) -> Result<Value, String> {
        let arguments: Value =
            serde_json::from_str(&format!("{{\"v\":{raw}}}")).expect("argument JSON");
        Err(format!(
            "Validation failed for tool \"t\":\n  - v: Expected {ty}, received {received}\n\n\
             Received arguments:\n{}",
            serde_json::to_string_pretty(&arguments).expect("pretty")
        ))
    }

    /// TS's plain-JSON-schema coercion (the path Rust's schemas take: they
    /// carry no `TypeBox` metadata) turns only the exact strings `"true"` /
    /// `"false"`, the numbers `1` / `0` and `null` into a boolean. `TypeBox`
    /// `Value.Convert` would also take `"TRUE"`, `"True"`, `"1"`, `"0"`;
    /// neither path trims, so `" true "` stays a string. Oracle: node runs of
    /// TS `validateToolArguments`.
    #[test]
    fn booleans_coerce_like_the_plain_json_schema_path() {
        let cases = [
            "\"true\"",
            "\"false\"",
            "\"TRUE\"",
            "\"True\"",
            "\" true \"",
            "\"true \"",
            "\"1\"",
            "\"0\"",
            "\"yes\"",
            "\"\"",
            "1",
            "0",
            "1.0",
            "-0",
            "2",
            "null",
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|raw| validate_json("boolean", raw))
            .collect();
        assert_eq!(
            actual,
            vec![
                Ok(json!({ "v": true })),
                Ok(json!({ "v": false })),
                mismatch("boolean", "\"TRUE\"", "string"),
                mismatch("boolean", "\"True\"", "string"),
                mismatch("boolean", "\" true \"", "string"),
                mismatch("boolean", "\"true \"", "string"),
                mismatch("boolean", "\"1\"", "string"),
                mismatch("boolean", "\"0\"", "string"),
                mismatch("boolean", "\"yes\"", "string"),
                mismatch("boolean", "\"\"", "string"),
                Ok(json!({ "v": true })),
                Ok(json!({ "v": false })),
                Ok(json!({ "v": true })),
                Ok(json!({ "v": false })),
                mismatch("boolean", "2", "integer"),
                Ok(json!({ "v": false })),
            ]
        );
    }

    /// TS checks `integer` with `Number.isInteger`, so any finite double with
    /// no fraction passes, however large; Rust held integers to `i64`/`u64`.
    /// A whole value that fits `i64`/`u64` comes back as a JSON integer (the
    /// same number `JSON.stringify` writes); beyond that it stays a JSON
    /// number. Oracle: node runs of TS `validateToolArguments`.
    #[test]
    fn integers_accept_every_finite_whole_double() {
        let cases = [
            "1e20",
            "\"1e20\"",
            "9223372036854775808",
            "9.223372036854775808e18",
            "\"9223372036854775808\"",
            "9.3e18",
            "-9.223372036854775808e18",
            "-9223372036854780000",
            "-9.3e18",
            "1e21",
            "\"1e21\"",
            "1.8446744073709552e19",
            "\"18446744073709551616\"",
            "1.5e300",
            "-1e20",
            "1.5",
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|raw| validate_json("integer", raw))
            .collect();
        assert_eq!(
            actual,
            vec![
                Ok(json!({ "v": 1e20 })),
                Ok(json!({ "v": 1e20 })),
                Ok(json!({ "v": 9_223_372_036_854_775_808_u64 })),
                Ok(json!({ "v": 9_223_372_036_854_775_808_u64 })),
                Ok(json!({ "v": 9_223_372_036_854_775_808_u64 })),
                Ok(json!({ "v": 9_300_000_000_000_000_000_u64 })),
                Ok(json!({ "v": i64::MIN })),
                Ok(json!({ "v": -9.223_372_036_854_78e18 })),
                Ok(json!({ "v": -9.3e18 })),
                Ok(json!({ "v": 1e21 })),
                Ok(json!({ "v": 1e21 })),
                Ok(json!({ "v": 1.844_674_407_370_955_2e19 })),
                Ok(json!({ "v": 1.844_674_407_370_955_2e19 })),
                Ok(json!({ "v": 1.5e300 })),
                Ok(json!({ "v": -1e20 })),
                mismatch("integer", "1.5", "number"),
            ]
        );
    }

    /// A number coerced to a `string` prints as JS `String(n)` (the double
    /// TS parsed, shortest round-trip, `1e+21` past 21 digits).
    #[test]
    fn numbers_coerce_to_strings_as_js_prints_them() {
        let cases = [
            "5",
            "1.5",
            "1e20",
            "1e21",
            "1e-7",
            "0.30000000000000004",
            "9223372036854775807",
            "1e15",
            "2.5e-5",
            "123456789012345680000",
            "true",
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|raw| validate_json("string", raw))
            .collect();
        let expected: Vec<Result<Value, String>> = [
            "5",
            "1.5",
            "100000000000000000000",
            "1e+21",
            "1e-7",
            "0.30000000000000004",
            "9223372036854776000",
            "1000000000000000",
            "0.000025",
            "123456789012345680000",
            "true",
        ]
        .iter()
        .map(|text| Ok(json!({ "v": text })))
        .collect();
        assert_eq!(actual, expected);
    }

    /// A value that already matches one member of a multi-type `type` is
    /// left alone (TS `matchesUnionMember`), so the boolean and integer
    /// coercions above never rewrite a valid `null`, `1` or `"5"`. Oracle:
    /// node runs of TS `validateToolArguments`.
    #[test]
    fn a_value_matching_a_union_member_is_not_coerced() {
        let cases = [
            (json!(["boolean", "null"]), json!(null)),
            (json!(["string", "number"]), json!("5")),
            (json!(["number", "string"]), json!("5")),
            (json!(["integer", "boolean"]), json!("true")),
            (json!(["boolean", "integer"]), json!(1)),
            (json!(["integer", "null"]), json!(1e20)),
            (json!(["string", "boolean"]), json!(1)),
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|(ty, v)| {
                let schema = json!({ "type": "object", "properties": { "v": { "type": ty } } });
                validate_tool_arguments("t", &schema, &json!({ "v": v }))
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                Ok(json!({ "v": null })),
                Ok(json!({ "v": "5" })),
                Ok(json!({ "v": "5" })),
                Ok(json!({ "v": true })),
                Ok(json!({ "v": 1 })),
                Ok(json!({ "v": 1e20 })),
                Ok(json!({ "v": "1" })),
            ]
        );
    }
}
