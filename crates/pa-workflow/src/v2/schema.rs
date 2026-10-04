//! The closed Workflow V2 schema and its strict interpreter.
//!
//! The schema is the runtime's packaged copy
//! (`prime-agent-runtime/schemas/workflow-v2.schema.json`), embedded byte for
//! byte so the host and the runtime's client validate against one authority
//! (TS `workflow-v2-wire.ts` embeds a generated projection of the same file).
//! The interpreter is the TS codec's: JSON Schema 2020-12 restricted to the
//! keywords the schema uses, plus the mandatory `x-utf8MaxBytes` assertion.
//! A keyword value it does not understand (an unknown `pattern` or `format`,
//! a non-local `$ref`) fails closed.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::wire::{canonical_json, WireError};

/// The schema bytes, exactly as the runtime packages them.
pub const SCHEMA_JSON: &str =
    include_str!("../../../../prime-agent-runtime/schemas/workflow-v2.schema.json");

/// `Number.MAX_SAFE_INTEGER`: the wire's integer ceiling.
const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

static DEFS: OnceLock<Map<String, Value>> = OnceLock::new();

/// The schema's `$defs`, parsed once.
fn defs() -> &'static Map<String, Value> {
    DEFS.get_or_init(|| {
        let mut root: Value =
            serde_json::from_str(SCHEMA_JSON).expect("the embedded Workflow V2 schema is JSON");
        match root.get_mut("$defs").map(Value::take) {
            Some(Value::Object(defs)) => defs,
            _ => panic!("the embedded Workflow V2 schema has no $defs object"),
        }
    })
}

/// Validate `value` against the named `$defs` entry.
///
/// # Errors
///
/// The first violation, with its JSON path.
pub fn validate_def(value: &Value, name: &str) -> Result<(), WireError> {
    validate_def_at(value, name, "$")
}

/// [`validate_def`] for a value nested at `path` of its message.
///
/// # Errors
///
/// The first violation, with its JSON path.
pub fn validate_def_at(value: &Value, name: &str, path: &str) -> Result<(), WireError> {
    let schema = defs()
        .get(name)
        .ok_or_else(|| WireError::new(path, format!("has no schema definition {name}")))?;
    validate(value, schema, path)
}

fn resolve(schema: &Value) -> Result<&Value, WireError> {
    let Some(reference) = schema.get("$ref") else {
        return Ok(schema);
    };
    let name = reference
        .as_str()
        .and_then(|reference| reference.strip_prefix("#/$defs/"))
        .ok_or_else(|| WireError::new("$", "uses an unsupported schema reference"))?;
    defs()
        .get(name)
        .ok_or_else(|| WireError::new("$", "uses an unknown schema reference"))
}

fn matches(value: &Value, schema: &Value) -> bool {
    validate(value, schema, "$").is_ok()
}

fn fail<T>(path: &str, why: &str) -> Result<T, WireError> {
    Err(WireError::new(path, why))
}

#[allow(clippy::too_many_lines)] // One keyword table, read top to bottom.
fn validate(value: &Value, raw: &Value, path: &str) -> Result<(), WireError> {
    let schema = resolve(raw)?;
    if let Some(Value::Array(branches)) = schema.get("oneOf") {
        let mut hits = branches.iter().filter(|branch| matches(value, branch));
        let (Some(hit), None) = (hits.next(), hits.next()) else {
            return fail(path, "does not match exactly one closed variant");
        };
        return validate(value, hit, path);
    }
    if let Some(Value::Array(branches)) = schema.get("anyOf") {
        let Some(hit) = branches.iter().find(|branch| matches(value, branch)) else {
            return fail(path, "does not match any allowed variant");
        };
        validate(value, hit, path)?;
    }
    if let Some(constant) = schema.get("const") {
        if value != constant {
            return fail(path, "has the wrong constant");
        }
    }
    if let Some(Value::Array(members)) = schema.get("enum") {
        if !members.contains(value) {
            return fail(path, "is outside the closed enum");
        }
    }
    let kind = schema.get("type").and_then(Value::as_str);
    if kind == Some("object")
        || schema.get("properties").is_some()
        || schema.get("required").is_some()
    {
        validate_object(value, schema, path)?;
    } else {
        match kind {
            Some("array") => validate_array(value, schema, path)?,
            Some("string") => validate_string(value, schema, path)?,
            Some("integer") => {
                let Some(number) = safe_integer(value) else {
                    return fail(path, "must be a safe integer");
                };
                if bound(schema, "minimum").is_some_and(|minimum| number < minimum) {
                    return fail(path, "is below its minimum");
                }
                if bound(schema, "maximum").is_some_and(|maximum| number > maximum) {
                    return fail(path, "is above its maximum");
                }
            }
            Some("boolean") if !value.is_boolean() => return fail(path, "must be boolean"),
            Some("null") if !value.is_null() => return fail(path, "must be null"),
            Some("boolean" | "null") | None => {}
            Some(_) => return fail(path, "uses an unsupported schema type"),
        }
    }
    if let Some(Value::Array(all)) = schema.get("allOf") {
        for branch in all {
            validate(value, branch, path)?;
        }
    }
    if let Some(condition) = schema.get("if") {
        let branch = if matches(value, condition) {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            validate(value, branch, path)?;
        }
    }
    if schema
        .get("not")
        .is_some_and(|forbidden| matches(value, forbidden))
    {
        return fail(path, "matches a forbidden shape");
    }
    if let Some(required) = schema.get("contains") {
        let present = value
            .as_array()
            .is_some_and(|items| items.iter().any(|item| matches(item, required)));
        if !present {
            return fail(path, "lacks a required item");
        }
    }
    Ok(())
}

fn validate_object(value: &Value, schema: &Value, path: &str) -> Result<(), WireError> {
    let Value::Object(object) = value else {
        return fail(path, "must be an object");
    };
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for key in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !object.contains_key(key) {
            return fail(&format!("{path}.{key}"), "is required");
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        if let Some(unknown) = object.keys().find(|key| !properties.contains_key(*key)) {
            return fail(&format!("{path}.{unknown}"), "is unknown");
        }
    }
    for (key, child) in properties {
        if let Some(field) = object.get(key) {
            validate(field, child, &format!("{path}.{key}"))?;
        }
    }
    Ok(())
}

fn validate_array(value: &Value, schema: &Value, path: &str) -> Result<(), WireError> {
    let Value::Array(items) = value else {
        return fail(path, "must be an array");
    };
    let count = i128::try_from(items.len()).unwrap_or(i128::MAX);
    if bound(schema, "minItems").is_some_and(|minimum| count < minimum) {
        return fail(path, "has too few items");
    }
    if bound(schema, "maxItems").is_some_and(|maximum| count > maximum) {
        return fail(path, "has too many items");
    }
    if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
        let mut seen = HashSet::new();
        for item in items {
            if !seen.insert(canonical_json(item)?) {
                return fail(path, "has duplicate items");
            }
        }
    }
    if let Some(each) = schema.get("items") {
        for (index, item) in items.iter().enumerate() {
            validate(item, each, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn validate_string(value: &Value, schema: &Value, path: &str) -> Result<(), WireError> {
    let Value::String(text) = value else {
        return fail(path, "must be a string");
    };
    // Code-point bounds (the TS codec counts `[...value]`, Python `len`).
    let points = i128::try_from(text.chars().count()).unwrap_or(i128::MAX);
    if bound(schema, "minLength").is_some_and(|minimum| points < minimum) {
        return fail(path, "is too short");
    }
    if bound(schema, "maxLength").is_some_and(|maximum| points > maximum) {
        return fail(path, "is too long");
    }
    let bytes = i128::try_from(text.len()).unwrap_or(i128::MAX);
    if bound(schema, "x-utf8MaxBytes").is_some_and(|maximum| bytes > maximum) {
        return fail(path, "exceeds its UTF-8 byte bound");
    }
    if let Some(pattern) = schema.get("pattern") {
        let matched = match pattern.as_str() {
            Some(ID_PATTERN) => is_id(text),
            Some(DIGEST_PATTERN) => is_digest(text),
            Some("Z$") => text.ends_with('Z'),
            _ => return fail(path, "uses an unsupported schema pattern"),
        };
        if !matched {
            return fail(path, "has invalid syntax");
        }
    }
    match schema.get("format").map(Value::as_str) {
        None => {}
        Some(Some("date-time")) => {
            if !is_utc_date_time(text) {
                return fail(path, "must be a real RFC 3339 UTC time");
            }
        }
        Some(_) => return fail(path, "uses an unsupported schema format"),
    }
    Ok(())
}

/// The schema's `id` pattern.
const ID_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$";
/// The schema's `digest` pattern.
const DIGEST_PATTERN: &str = "^sha256:[0-9a-f]{64}$";

/// `^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`.
#[must_use]
pub fn is_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    matches!(bytes.first(), Some(first) if first.is_ascii_alphanumeric())
        && bytes.len() <= 128
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// `^sha256:[0-9a-f]{64}$`.
#[must_use]
pub fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// `YYYY-MM-DDTHH:MM:SS(.fraction)?Z` naming a real UTC instant (the TS
/// codec re-reads the parsed `Date`'s fields, so day 31 of a 30-day month
/// and second 60 are refused).
fn is_utc_date_time(text: &str) -> bool {
    let Some(body) = text.strip_suffix('Z') else {
        return false;
    };
    let (clock, fraction) = match body.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (body, None),
    };
    if fraction
        .is_some_and(|digits| digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let bytes = clock.as_bytes();
    if bytes.len() != 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return false;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return false;
    }
    let field = |range: std::ops::Range<usize>| -> Option<u32> {
        let digits = clock.get(range)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        field(0..4),
        field(5..7),
        field(8..10),
        field(11..13),
        field(14..16),
        field(17..19),
    ) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}

/// An integer in `-(2^53-1)..=2^53-1`. A float is never an integer here,
/// even when integral (the runtime's client refuses `1.0` too).
fn safe_integer(value: &Value) -> Option<i128> {
    let Value::Number(number) = value else {
        return None;
    };
    let wide = number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))?;
    (wide.abs() <= MAX_SAFE_INTEGER).then_some(wide)
}

fn bound(schema: &Value, keyword: &str) -> Option<i128> {
    schema.get(keyword).and_then(safe_integer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_schema_is_the_pinned_authority() {
        // TS `scripts/fixtures/workflow-v2-schema-authority.json`.
        assert_eq!(
            super::super::wire::sha256_digest(SCHEMA_JSON.as_bytes()),
            "sha256:1f9088eca248f86bdfce97e23eb15f393ffc329a8fce9b33257729e3369b4a4a"
        );
    }

    #[test]
    fn every_pattern_and_format_in_the_schema_is_understood() {
        fn walk(value: &Value, out: &mut Vec<(String, String)>) {
            match value {
                Value::Object(object) => {
                    for (key, child) in object {
                        if matches!(key.as_str(), "pattern" | "format") {
                            if let Some(text) = child.as_str() {
                                out.push((key.clone(), text.to_string()));
                            }
                        }
                        walk(child, out);
                    }
                }
                Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        walk(&serde_json::from_str(SCHEMA_JSON).unwrap(), &mut found);
        found.sort();
        found.dedup();
        assert_eq!(
            found,
            vec![
                ("format".to_string(), "date-time".to_string()),
                ("pattern".to_string(), "Z$".to_string()),
                ("pattern".to_string(), ID_PATTERN.to_string()),
                ("pattern".to_string(), DIGEST_PATTERN.to_string()),
            ]
        );
    }

    #[test]
    fn ids_and_digests_follow_the_schema_patterns() {
        let long = format!("a{}", "b".repeat(127));
        for good in ["a", "A9._:-z", long.as_str()] {
            assert!(is_id(good), "{good}");
        }
        let too_long = format!("{long}c");
        for bad in ["", " bad", ".a", "a/b", "é", too_long.as_str()] {
            assert!(!is_id(bad), "{bad}");
        }
        assert!(is_digest(&format!("sha256:{}", "0a".repeat(32))));
        assert!(!is_digest(&format!("sha256:{}", "A".repeat(64))));
        assert!(!is_digest(&format!("sha256:{}", "a".repeat(63))));
    }

    #[test]
    fn date_times_are_real_utc_instants() {
        for good in [
            "2026-09-14T00:00:00Z",
            "2024-02-29T23:59:59.123Z",
            "2000-02-29T12:00:00Z",
        ] {
            assert!(is_utc_date_time(good), "{good}");
        }
        for bad in [
            "2026-09-14T00:00:00",
            "2026-09-14T00:00:00+00:00",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-09-14T24:00:00Z",
            "2026-09-14T00:00:60Z",
            "2026-09-14T00:00:00.Z",
            "2026-9-14T00:00:00Z",
        ] {
            assert!(!is_utc_date_time(bad), "{bad}");
        }
    }
}
