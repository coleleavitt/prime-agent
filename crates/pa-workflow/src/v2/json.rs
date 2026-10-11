//! The strict JSON codec bounds (`WORKFLOW-V2.md` §3): duplicate keys,
//! invalid UTF-8, trailing content, non-finite numbers, more than 32 JSON
//! levels, more than 10,000 JSON nodes, and messages above 1 MiB all fail.
//!
//! [`parse`] reads wire text; [`check_bounds`] applies the same level, node,
//! and size bounds to a value that arrived already parsed (a kernel host
//! request), as the runtime's client does before it sends (`_validate_message_bounds`).

use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use super::wire::WireError;

/// The encoded-message ceiling.
pub const MAX_MESSAGE_BYTES: usize = 1_048_576;
/// The nesting ceiling (the top-level value is level 1).
pub const MAX_DEPTH: usize = 32;
/// The JSON value-count ceiling.
pub const MAX_NODES: usize = 10_000;

/// Strictly parse one wire message.
///
/// # Errors
///
/// A size, UTF-8, syntax, duplicate-key, depth, or node-count violation.
pub fn parse(input: &[u8]) -> Result<Value, WireError> {
    if input.len() > MAX_MESSAGE_BYTES {
        return Err(WireError::new("$", "exceeds 1 MiB"));
    }
    let text =
        std::str::from_utf8(input).map_err(|_| WireError::new("$", "contains invalid UTF-8"))?;
    let nodes = Cell::new(0);
    let violation = Cell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    // The bounds are enforced by the seed; serde_json's own recursion
    // limit (128) sits above them.
    let parsed = Strict {
        depth: 1,
        nodes: &nodes,
        violation: &violation,
    }
    .deserialize(&mut deserializer);
    let value = match parsed {
        Ok(value) => value,
        Err(error) => {
            return Err(violation.take().unwrap_or_else(|| {
                if error.is_eof() {
                    WireError::new("$", "is truncated JSON")
                } else {
                    WireError::new("$", "contains invalid JSON")
                }
            }));
        }
    };
    deserializer
        .end()
        .map_err(|_| WireError::new("$", "has trailing content"))?;
    Ok(value)
}

/// Apply the level, node, and encoded-size bounds to a parsed value.
///
/// # Errors
///
/// The first bound the value exceeds.
pub fn check_bounds(value: &Value) -> Result<(), WireError> {
    fn walk(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), WireError> {
        if depth > MAX_DEPTH {
            return Err(WireError::new("$", "exceeds maximum depth"));
        }
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(WireError::new("$", "exceeds maximum JSON nodes"));
        }
        match value {
            Value::Array(items) => items
                .iter()
                .try_for_each(|item| walk(item, depth + 1, nodes)),
            Value::Object(object) => object
                .values()
                .try_for_each(|item| walk(item, depth + 1, nodes)),
            _ => Ok(()),
        }
    }
    walk(value, 1, &mut 0)?;
    let encoded = serde_json::to_vec(value).map_err(|_| WireError::new("$", "is not JSON"))?;
    if encoded.len() > MAX_MESSAGE_BYTES {
        return Err(WireError::new("$", "exceeds 1 MiB"));
    }
    Ok(())
}

/// One value at `depth`, sharing the message's node counter. A bound
/// violation is parked in `violation` so the caller reports it rather than
/// serde's generic message.
struct Strict<'a> {
    depth: usize,
    nodes: &'a Cell<usize>,
    violation: &'a Cell<Option<WireError>>,
}

impl Strict<'_> {
    fn child(&self) -> Self {
        Strict {
            depth: self.depth + 1,
            nodes: self.nodes,
            violation: self.violation,
        }
    }

    fn refuse<E: de::Error>(&self, why: &'static str) -> E {
        self.violation.set(Some(WireError::new("$", why)));
        E::custom(why)
    }

    fn admit<E: de::Error>(&self) -> Result<(), E> {
        if self.depth > MAX_DEPTH {
            return Err(self.refuse("exceeds maximum depth"));
        }
        self.nodes.set(self.nodes.get() + 1);
        if self.nodes.get() > MAX_NODES {
            return Err(self.refuse("exceeds maximum JSON nodes"));
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for Strict<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a strict JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        self.admit()?;
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        self.admit()?;
        Ok(Value::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        self.admit()?;
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        self.admit()?;
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| self.refuse("contains a non-finite number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        self.admit()?;
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        self.admit()?;
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        self.admit()?;
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        self.admit()?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self.child())? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        self.admit()?;
        let mut object = Map::new();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(self.refuse("contains a duplicate object key"));
            }
            let value = map.next_value_seed(self.child())?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reason(input: &[u8]) -> String {
        parse(input).unwrap_err().to_string()
    }

    #[test]
    fn a_strict_message_parses_to_the_same_value() {
        assert_eq!(
            parse(r#" {"a":[1,-2,true,null,"x\u00e9é"],"b":{}} "#.as_bytes()).unwrap(),
            json!({ "a": [1, -2, true, null, "xéé"], "b": {} })
        );
    }

    #[test]
    fn duplicate_keys_trailing_bytes_and_invalid_utf8_fail() {
        assert_eq!(
            reason(br#"{"requestId":"a","requestId":"b"}"#),
            "$ contains a duplicate object key"
        );
        assert_eq!(
            reason(br#"{"a":{"k":1,"k":2}}"#),
            "$ contains a duplicate object key"
        );
        assert_eq!(reason(b"{} x"), "$ has trailing content");
        assert_eq!(reason(&[0x7b, 0xff, 0x7d]), "$ contains invalid UTF-8");
        assert_eq!(reason(br#""\ud800""#), "$ contains invalid JSON");
        assert_eq!(reason(b"NaN"), "$ contains invalid JSON");
        assert_eq!(reason(b"[1,"), "$ is truncated JSON");
    }

    #[test]
    fn depth_nodes_and_size_are_bounded() {
        let nested = |levels: usize| format!("{}{}", "[".repeat(levels), "]".repeat(levels));
        assert!(parse(nested(MAX_DEPTH).as_bytes()).is_ok());
        assert_eq!(
            reason(nested(MAX_DEPTH + 1).as_bytes()),
            "$ exceeds maximum depth"
        );
        // The array itself is one node.
        let items = |count: usize| format!("[{}]", vec!["0"; count].join(","));
        assert!(parse(items(MAX_NODES - 1).as_bytes()).is_ok());
        assert_eq!(
            reason(items(MAX_NODES).as_bytes()),
            "$ exceeds maximum JSON nodes"
        );
        let big = serde_json::to_string(&"x".repeat(MAX_MESSAGE_BYTES)).unwrap();
        assert_eq!(reason(big.as_bytes()), "$ exceeds 1 MiB");
    }

    #[test]
    fn parsed_values_get_the_same_bounds() {
        let mut deep = json!(0);
        for _ in 0..MAX_DEPTH {
            deep = json!([deep]);
        }
        assert_eq!(
            check_bounds(&deep).unwrap_err().to_string(),
            "$ exceeds maximum depth"
        );
        assert!(check_bounds(&json!(vec![0; MAX_NODES - 1])).is_ok());
        assert_eq!(
            check_bounds(&json!(vec![0; MAX_NODES]))
                .unwrap_err()
                .to_string(),
            "$ exceeds maximum JSON nodes"
        );
        assert_eq!(
            check_bounds(&json!("x".repeat(MAX_MESSAGE_BYTES)))
                .unwrap_err()
                .to_string(),
            "$ exceeds 1 MiB"
        );
    }
}
