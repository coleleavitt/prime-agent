//! The `get_app` spec as the kernel client encodes it.
//!
//! The model passes a name, a bundle id, or a `{"bundle_id" | "name" |
//! "path": ...}` dict, possibly malformed. The client sends the shape plus
//! the Python `str()` and `repr()` of the original value, which the error
//! messages quote verbatim.

use serde_json::Value;

/// One `get_app` spec.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppSpec {
    pub shape: SpecShape,
    /// Python's `str(spec)`.
    pub display: String,
    /// Python's `repr(spec)`.
    pub repr: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpecShape {
    Text(String),
    /// The dict's string-keyed entries (a non-string value is `None`) and
    /// its keys as `sorted(spec, key=str)` (the error details quote them).
    Dict {
        entries: Vec<(String, Option<String>)>,
        keys: Vec<Value>,
    },
    /// Neither a string nor a dict: the Python type name.
    Other(String),
}

/// The three dict spec keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecKey {
    BundleId,
    Name,
    Path,
}

impl SpecKey {
    /// In the order the Python skill tries them.
    pub(crate) const ALL: [SpecKey; 3] = [SpecKey::BundleId, SpecKey::Name, SpecKey::Path];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SpecKey::BundleId => "bundle_id",
            SpecKey::Name => "name",
            SpecKey::Path => "path",
        }
    }
}

impl AppSpec {
    /// Decode the wire form.
    pub(crate) fn from_wire(value: &Value) -> Option<Self> {
        let display = value.get("str")?.as_str()?.to_string();
        let repr = value.get("repr")?.as_str()?.to_string();
        let shape = match value.get("kind")?.as_str()? {
            "str" => SpecShape::Text(value.get("value")?.as_str()?.to_string()),
            "dict" => SpecShape::Dict {
                entries: value
                    .get("entries")?
                    .as_array()?
                    .iter()
                    .map(|entry| {
                        let pair = entry.as_array()?;
                        Some((
                            pair.first()?.as_str()?.to_string(),
                            pair.get(1)?.as_str().map(ToString::to_string),
                        ))
                    })
                    .collect::<Option<_>>()?,
                keys: value.get("keys")?.as_array()?.clone(),
            },
            "other" => SpecShape::Other(value.get("type")?.as_str()?.to_string()),
            _ => return None,
        };
        Some(Self {
            shape,
            display,
            repr,
        })
    }

    /// A plain string spec (the guard re-resolves a bound app id with one).
    pub(crate) fn text(value: &str) -> Self {
        Self {
            shape: SpecShape::Text(value.to_string()),
            display: value.to_string(),
            repr: crate::pyfmt::repr_str(value),
        }
    }

    /// A one-key dict spec with a string value.
    #[cfg(test)]
    pub(crate) fn dict(key: &str, value: &str) -> Self {
        let key_repr = crate::pyfmt::repr_str(key);
        let value_repr = crate::pyfmt::repr_str(value);
        Self {
            shape: SpecShape::Dict {
                entries: vec![(key.to_string(), Some(value.to_string()))],
                keys: vec![Value::from(key)],
            },
            display: format!("{{{key_repr}: {value_repr}}}"),
            repr: format!("{{{key_repr}: {value_repr}}}"),
        }
    }

    /// The value of one dict key, when the spec is a dict carrying it as a
    /// string.
    pub(crate) fn entry(&self, key: SpecKey) -> Option<&str> {
        let key = key.as_str();
        match &self.shape {
            SpecShape::Dict { entries, .. } => entries
                .iter()
                .find(|(name, _)| name == key)
                .and_then(|(_, value)| value.as_deref()),
            SpecShape::Text(_) | SpecShape::Other(_) => None,
        }
    }

    /// Whether the dict carries `key` at all (with any value).
    pub(crate) fn has_key(&self, key: SpecKey) -> bool {
        let key = key.as_str();
        match &self.shape {
            SpecShape::Dict { entries, .. } => entries.iter().any(|(name, _)| name == key),
            SpecShape::Text(_) | SpecShape::Other(_) => false,
        }
    }
}

/// Python's `str.strip()` emptiness test.
pub(crate) fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_forms_decode() {
        assert_eq!(
            AppSpec::from_wire(
                &json!({"kind": "str", "value": "Slack", "str": "Slack", "repr": "'Slack'"})
            ),
            Some(AppSpec::text("Slack"))
        );
        let dict = AppSpec::from_wire(&json!({
            "kind": "dict",
            "entries": [["bundle_id", 5], ["name", "x"]],
            "keys": ["bundle_id", "name"],
            "str": "{'bundle_id': 5, 'name': 'x'}",
            "repr": "{'bundle_id': 5, 'name': 'x'}",
        }))
        .unwrap();
        assert_eq!(dict.entry(SpecKey::BundleId), None);
        assert!(dict.has_key(SpecKey::BundleId));
        assert_eq!(dict.entry(SpecKey::Name), Some("x"));
        assert_eq!(
            AppSpec::from_wire(&json!({"kind": "other", "type": "int", "str": "5", "repr": "5"}))
                .map(|spec| spec.shape),
            Some(SpecShape::Other("int".to_string()))
        );
        assert_eq!(AppSpec::from_wire(&json!({"kind": "str"})), None);
    }
}
