//! The Python-value model the factory spec validator reads.
//!
//! Factory specs are authored in the kernel as Python objects, and the
//! write-time validator's rules (and its error sentences) are defined over
//! Python semantics: a tuple is not a list, `True` is not an integer, a
//! dict may carry non-string keys, a float may be NaN, a container may
//! contain itself. JSON cannot carry those distinctions, so the kernel
//! client ships a spec as a flat node table (one entry per value, children
//! by index — the table's own nesting is constant, so a deeply nested spec
//! never meets a JSON parser's recursion bound) and this module rebuilds
//! the value tree, with every non-JSON object reduced to an opaque leaf
//! that carries its Python `repr` and truthiness.
//!
//! [`py_repr`] reproduces Python's `repr()` for the rebuilt values so the
//! validator's `{value!r}` sentences stay byte-identical.

use std::fmt::Write as _;

use serde_json::{json, Map, Number, Value};

/// One Python value, as the validator sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum PyValue {
    None,
    Bool(bool),
    /// An int that fits `i128`.
    Int(i128),
    /// An int beyond `i128`, kept as its decimal digits (sign included).
    BigInt(String),
    Float(f64),
    Str(String),
    List(Vec<PyValue>),
    /// A dict in insertion order; keys may be any value.
    Dict(Vec<(PyValue, PyValue)>),
    /// Anything JSON has no spelling for (a tuple, set, bytes, an object),
    /// a back-reference to an enclosing container (a cycle), or a container
    /// nested past the client's encoding bound. `index` names the original
    /// object in the client's registry, so a value handed back (a canonical
    /// machine's passthrough fields) decodes to a copy of the original.
    Opaque {
        index: usize,
        repr: String,
        truthy: bool,
        /// `type(value).__name__` (`object` from a client that did not
        /// send it).
        type_name: String,
        /// `json.dumps(value)` when the encoder can spell it (a tuple of
        /// JSON values): what the machine renderer prints for it.
        json: Option<String>,
    },
}

/// The shared `None` a missing key reads as (Python's `dict.get`).
static NONE: PyValue = PyValue::None;

impl PyValue {
    /// `isinstance(value, dict)`.
    #[must_use]
    pub fn is_dict(&self) -> bool {
        matches!(self, Self::Dict(_))
    }

    /// `isinstance(value, list)`.
    #[must_use]
    pub fn is_list(&self) -> bool {
        matches!(self, Self::List(_))
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[PyValue]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    /// The string-keyed entry `key` of a dict (`None` for a non-dict or a
    /// missing key): Python's `"key" in d` plus `d["key"]`.
    #[must_use]
    pub fn entry(&self, key: &str) -> Option<&PyValue> {
        match self {
            Self::Dict(pairs) => pairs
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Python's `d.get(key)`: a missing key reads as `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> &PyValue {
        self.entry(key).unwrap_or(&NONE)
    }

    /// Python's `key in d` for a string key.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.entry(key).is_some()
    }

    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// `_is_int`: real integers; booleans are not ints.
    #[must_use]
    pub fn is_int(&self) -> bool {
        matches!(self, Self::Int(_) | Self::BigInt(_))
    }

    /// `_is_number`: ints and floats; booleans are not numbers.
    #[must_use]
    pub fn is_number(&self) -> bool {
        matches!(self, Self::Int(_) | Self::BigInt(_) | Self::Float(_))
    }

    /// `_is_scalar`: str, int, float, bool, or None.
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Self::None
                | Self::Bool(_)
                | Self::Int(_)
                | Self::BigInt(_)
                | Self::Float(_)
                | Self::Str(_)
        )
    }

    /// `_is_nonempty_str`.
    #[must_use]
    pub fn is_nonempty_str(&self) -> bool {
        matches!(self, Self::Str(text) if !text.is_empty())
    }

    /// The integer's comparison key: `(sign class, value)`, where a big
    /// positive int orders above every `i128` and a big negative one below.
    fn int_order(&self) -> Option<(i8, i128)> {
        match self {
            Self::Int(value) => Some((0, *value)),
            Self::BigInt(digits) => Some(if digits.starts_with('-') {
                (-1, 0)
            } else {
                (1, 0)
            }),
            _ => None,
        }
    }

    /// `value > 0` for an int (`_is_positive_int` after `_is_int`).
    #[must_use]
    pub fn int_gt(&self, bound: i128) -> bool {
        match self.int_order() {
            Some((0, value)) => value > bound,
            Some((class, _)) => class > 0,
            None => false,
        }
    }

    /// `value >= bound` for an int.
    #[must_use]
    pub fn int_ge(&self, bound: i128) -> bool {
        self.int_gt(bound) || self.int_eq(bound)
    }

    /// `value <= bound` for an int.
    #[must_use]
    pub fn int_le(&self, bound: i128) -> bool {
        self.is_int() && !self.int_gt(bound)
    }

    fn int_eq(&self, bound: i128) -> bool {
        matches!(self, Self::Int(value) if *value == bound)
    }

    /// `_is_positive_int`.
    #[must_use]
    pub fn is_positive_int(&self) -> bool {
        self.is_int() && self.int_gt(0)
    }

    /// `a > b` between two ints (both already `is_int`).
    #[must_use]
    pub fn int_greater_than(&self, other: &PyValue) -> bool {
        match (self.int_order(), other.int_order()) {
            (Some((0, a)), Some((0, b))) => a > b,
            (Some((ca, _)), Some((cb, _))) if ca != cb => ca > cb,
            // Two big ints of one sign: compare magnitudes by digits.
            (Some((class, _)), Some(_)) => {
                let (Self::BigInt(a), Self::BigInt(b)) = (self, other) else {
                    return false;
                };
                let magnitude = |digits: &str| digits.trim_start_matches('-').to_string();
                let (a, b) = (magnitude(a), magnitude(b));
                let greater = (a.len(), a.as_str()) > (b.len(), b.as_str());
                if class > 0 {
                    greater
                } else {
                    (b.len(), b.as_str()) > (a.len(), a.as_str())
                }
            }
            _ => false,
        }
    }

    /// The int as `u64` when it fits (canonical limits after validation).
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Int(value) => u64::try_from(*value).ok(),
            _ => None,
        }
    }

    /// Python truthiness (`bool(value)`).
    #[must_use]
    pub fn truthy(&self) -> bool {
        match self {
            Self::None => false,
            Self::Bool(value) => *value,
            Self::Int(value) => *value != 0,
            Self::BigInt(_) => true,
            Self::Float(value) => *value != 0.0,
            Self::Str(text) => !text.is_empty(),
            Self::List(items) => !items.is_empty(),
            Self::Dict(pairs) => !pairs.is_empty(),
            Self::Opaque { truthy, .. } => *truthy,
        }
    }

    /// `value is True`.
    #[must_use]
    pub fn is_true(&self) -> bool {
        matches!(self, Self::Bool(true))
    }

    /// The value as JSON for the executor's canonical machine: JSON shapes
    /// map one to one; a value JSON cannot carry (an opaque object, a
    /// non-finite float, an int beyond `u64`/`i64`, a non-string dict key)
    /// renders as its Python `repr` string. A validated machine carries such
    /// values only in fields the executor never reads (passthrough keys).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::None => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Int(value) => i64::try_from(*value)
                .map(Value::from)
                .or_else(|_| u64::try_from(*value).map(Value::from))
                .unwrap_or_else(|_| Value::String(value.to_string())),
            Self::BigInt(digits) => Value::String(digits.clone()),
            Self::Float(value) => Number::from_f64(*value)
                .map_or_else(|| Value::String(py_float_repr(*value)), Value::Number),
            Self::Str(text) => Value::String(text.clone()),
            Self::List(items) => Value::Array(items.iter().map(Self::to_json).collect()),
            Self::Dict(pairs) => {
                let mut map = Map::new();
                for (key, value) in pairs {
                    let key = match key {
                        Self::Str(text) => text.clone(),
                        other => py_repr(other),
                    };
                    map.insert(key, value.to_json());
                }
                Value::Object(map)
            }
            Self::Opaque { repr, .. } => Value::String(repr.clone()),
        }
    }

    /// Build a value from plain JSON (a value the host itself produced,
    /// never a client node table).
    #[must_use]
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => Self::None,
            Value::Bool(flag) => Self::Bool(*flag),
            Value::Number(number) => number_to_py(number),
            Value::String(text) => Self::Str(text.clone()),
            Value::Array(items) => Self::List(items.iter().map(Self::from_json).collect()),
            Value::Object(map) => Self::Dict(
                map.iter()
                    .map(|(key, value)| (Self::Str(key.clone()), Self::from_json(value)))
                    .collect(),
            ),
        }
    }
}

fn number_to_py(number: &Number) -> PyValue {
    if let Some(value) = number.as_i64() {
        PyValue::Int(i128::from(value))
    } else if let Some(value) = number.as_u64() {
        PyValue::Int(i128::from(value))
    } else {
        PyValue::Float(number.as_f64().unwrap_or(f64::NAN))
    }
}

/// Why a client node table could not be rebuilt.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("malformed factory value table: {0}")]
pub struct NodeTableError(String);

fn table_error(message: impl Into<String>) -> NodeTableError {
    NodeTableError(message.into())
}

/// Rebuild one value from the kernel client's node table.
///
/// The table is `{"nodes": [...], "root": <index>}` with one node per
/// value, each a tagged array: `["n"]` None, `["b", bool]`, `["i",
/// "<digits>"]` an int, `["f", <number> | "nan" | "inf" | "-inf"]`,
/// `["s", str]`, `["l", [child...]]`, `["d", [[key, value]...]]`, and
/// `["o", <registry index>, <repr>, <truthy>, <type name>, <json>]` for an
/// opaque leaf (the type name and the `json.dumps` spelling, or null, are
/// optional). Every
/// child index is larger than its parent's (the client emits pre-order),
/// and every node is used at most once, so the rebuild is one reverse pass
/// with no recursion.
///
/// # Errors
///
/// Returns an error when the table is not well formed.
pub fn decode_node_table(table: &Value) -> Result<PyValue, NodeTableError> {
    let nodes = table
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| table_error("nodes must be a list"))?;
    let root = table
        .get("root")
        .and_then(Value::as_u64)
        .and_then(|root| usize::try_from(root).ok())
        .ok_or_else(|| table_error("root must be an index"))?;
    check_table_depth(nodes)?;
    let mut built: Vec<Option<PyValue>> = vec![None; nodes.len()];
    for index in (0..nodes.len()).rev() {
        let node = nodes[index]
            .as_array()
            .ok_or_else(|| table_error(format!("node {index} must be a list")))?;
        let tag = node
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| table_error(format!("node {index} needs a tag")))?;
        let arg = |position: usize| node.get(position).unwrap_or(&Value::Null);
        let mut take = |child: &Value| -> Result<PyValue, NodeTableError> {
            let child = child
                .as_u64()
                .and_then(|child| usize::try_from(child).ok())
                .filter(|child| *child > index && *child < nodes.len())
                .ok_or_else(|| table_error(format!("node {index} has a bad child index")))?;
            built[child]
                .take()
                .ok_or_else(|| table_error(format!("node {child} is referenced twice")))
        };
        let value = match tag {
            "n" => PyValue::None,
            "b" => PyValue::Bool(
                arg(1)
                    .as_bool()
                    .ok_or_else(|| table_error(format!("node {index} bool")))?,
            ),
            "i" => {
                let digits = arg(1)
                    .as_str()
                    .ok_or_else(|| table_error(format!("node {index} int digits")))?;
                let valid = {
                    let body = digits.strip_prefix('-').unwrap_or(digits);
                    !body.is_empty() && body.bytes().all(|byte| byte.is_ascii_digit())
                };
                if !valid {
                    return Err(table_error(format!("node {index} int digits")));
                }
                digits
                    .parse::<i128>()
                    .map_or_else(|_| PyValue::BigInt(digits.to_string()), PyValue::Int)
            }
            "f" => match arg(1) {
                Value::Number(number) => PyValue::Float(number.as_f64().unwrap_or(f64::NAN)),
                Value::String(text) if text == "nan" => PyValue::Float(f64::NAN),
                Value::String(text) if text == "inf" => PyValue::Float(f64::INFINITY),
                Value::String(text) if text == "-inf" => PyValue::Float(f64::NEG_INFINITY),
                _ => return Err(table_error(format!("node {index} float"))),
            },
            "s" => PyValue::Str(
                arg(1)
                    .as_str()
                    .ok_or_else(|| table_error(format!("node {index} str")))?
                    .to_string(),
            ),
            "l" => {
                let children = arg(1)
                    .as_array()
                    .ok_or_else(|| table_error(format!("node {index} list")))?;
                let mut items = Vec::with_capacity(children.len());
                for child in children {
                    items.push(take(child)?);
                }
                PyValue::List(items)
            }
            "d" => {
                let pairs = arg(1)
                    .as_array()
                    .ok_or_else(|| table_error(format!("node {index} dict")))?;
                let mut entries = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    let pair = pair
                        .as_array()
                        .filter(|pair| pair.len() == 2)
                        .ok_or_else(|| table_error(format!("node {index} dict pair")))?;
                    let key = take(&pair[0])?;
                    let value = take(&pair[1])?;
                    entries.push((key, value));
                }
                PyValue::Dict(entries)
            }
            "o" => PyValue::Opaque {
                index: arg(1)
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| table_error(format!("node {index} opaque index")))?,
                repr: arg(2)
                    .as_str()
                    .ok_or_else(|| table_error(format!("node {index} opaque repr")))?
                    .to_string(),
                truthy: arg(3).as_bool().unwrap_or(true),
                type_name: arg(4).as_str().unwrap_or("object").to_string(),
                json: arg(5).as_str().map(str::to_string),
            },
            other => {
                return Err(table_error(format!(
                    "node {index} has unknown tag {other:?}"
                )))
            }
        };
        built[index] = Some(value);
    }
    built
        .get_mut(root)
        .and_then(Option::take)
        .ok_or_else(|| table_error("root index out of range"))
}

/// The deepest nesting a node table may describe. The kernel client stops
/// descending far earlier (an opaque leaf stands in for anything deeper),
/// so this only bounds a malformed table: a rebuilt tree is dropped and
/// compared recursively, and an unbounded one would exhaust the stack.
const MAX_TABLE_DEPTH: usize = 4096;

/// Forward pass over the parent-before-child table: each child's depth is
/// its parent's plus one.
fn check_table_depth(nodes: &[Value]) -> Result<(), NodeTableError> {
    let mut depth = vec![0_usize; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        let children: Vec<&Value> = match (
            node.get(0).and_then(Value::as_str),
            node.get(1).and_then(Value::as_array),
        ) {
            (Some("l"), Some(items)) => items.iter().collect(),
            (Some("d"), Some(pairs)) => pairs
                .iter()
                .filter_map(Value::as_array)
                .flat_map(|pair| pair.iter())
                .collect(),
            _ => continue,
        };
        for child in children {
            if let Some(child) = child
                .as_u64()
                .and_then(|child| usize::try_from(child).ok())
                .filter(|child| *child > index && *child < nodes.len())
            {
                depth[child] = depth[index] + 1;
                if depth[child] > MAX_TABLE_DEPTH {
                    return Err(table_error(format!(
                        "values nest deeper than {MAX_TABLE_DEPTH} levels"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Encode a value back into a node table for the client (the canonical
/// machine's passthrough fields keep their Python identity: an opaque leaf
/// decodes to a copy of the client's original object).
#[must_use]
pub fn encode_node_table(value: &PyValue) -> Value {
    // Pre-order with an explicit stack: a node's slot is reserved before
    // its children are emitted, so child indices always exceed the parent's.
    let mut nodes: Vec<Value> = vec![Value::Null];
    let mut stack: Vec<(&PyValue, usize)> = vec![(value, 0)];
    while let Some((current, slot)) = stack.pop() {
        let node = match current {
            PyValue::None => json!(["n"]),
            PyValue::Bool(flag) => json!(["b", flag]),
            PyValue::Int(number) => json!(["i", number.to_string()]),
            PyValue::BigInt(digits) => json!(["i", digits]),
            PyValue::Float(number) => match Number::from_f64(*number) {
                Some(number) => json!(["f", number]),
                None => json!(["f", py_float_repr(*number)]),
            },
            PyValue::Str(text) => json!(["s", text]),
            PyValue::List(items) => {
                let mut children = Vec::with_capacity(items.len());
                for item in items {
                    let child = nodes.len();
                    nodes.push(Value::Null);
                    children.push(child);
                    stack.push((item, child));
                }
                json!(["l", children])
            }
            PyValue::Dict(pairs) => {
                let mut children = Vec::with_capacity(pairs.len());
                for (key, item) in pairs {
                    let key_slot = nodes.len();
                    nodes.push(Value::Null);
                    let value_slot = nodes.len();
                    nodes.push(Value::Null);
                    children.push(json!([key_slot, value_slot]));
                    stack.push((key, key_slot));
                    stack.push((item, value_slot));
                }
                json!(["d", children])
            }
            PyValue::Opaque {
                index,
                repr,
                truthy,
                type_name,
                json,
            } => json!(["o", index, repr, truthy, type_name, json]),
        };
        nodes[slot] = node;
    }
    json!({ "nodes": nodes, "root": 0 })
}

/// Python's `repr()` for one value.
#[must_use]
pub fn py_repr(value: &PyValue) -> String {
    let mut out = String::new();
    write_repr(value, &mut out);
    out
}

fn write_repr(value: &PyValue, out: &mut String) {
    match value {
        PyValue::None => out.push_str("None"),
        PyValue::Bool(true) => out.push_str("True"),
        PyValue::Bool(false) => out.push_str("False"),
        PyValue::Int(number) => out.push_str(&number.to_string()),
        PyValue::BigInt(digits) => out.push_str(digits),
        PyValue::Float(number) => out.push_str(&py_float_repr(*number)),
        PyValue::Str(text) => out.push_str(&py_str_repr(text)),
        PyValue::List(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_repr(item, out);
            }
            out.push(']');
        }
        PyValue::Dict(pairs) => {
            out.push('{');
            for (position, (key, item)) in pairs.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_repr(key, out);
                out.push_str(": ");
                write_repr(item, out);
            }
            out.push('}');
        }
        PyValue::Opaque { repr, .. } => out.push_str(repr),
    }
}

/// Python's `repr(float)`: the shortest round-trip digits, positional
/// between `1e-4` and `1e16`, otherwise scientific with a signed two-digit
/// (minimum) exponent.
#[must_use]
pub fn py_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    // `{:e}` prints the shortest round-trip mantissa: "1.5e16", "-3e-5".
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        // Positional: the decimal point sits after `exponent + 1` digits.
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else {
            let point = point as usize;
            if digits.len() <= point {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        };
        return format!("{sign}{body}");
    }
    let head = &digits[..1];
    let tail = &digits[1..];
    let mantissa = if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head}.{tail}")
    };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    format!(
        "{sign}{mantissa}e{exponent_sign}{:02}",
        exponent.unsigned_abs()
    )
}

/// Python's `repr(str)`: single quotes unless the text carries a single
/// quote and no double quote; backslash, the chosen quote, `\t\n\r`, and
/// non-printable characters escape.
#[must_use]
pub fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !is_py_printable(c) => {
                let code = c as u32;
                if code < 0x100 {
                    let _ = write!(out, "\\x{code:02x}");
                } else if code < 0x1_0000 {
                    let _ = write!(out, "\\u{code:04x}");
                } else {
                    let _ = write!(out, "\\U{code:08x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `str.isprintable()` per character: everything except the categories
/// Cc, Cf, Co, Zl, Zp, and Zs other than the ASCII space (unassigned code
/// points are treated as printable; the table carries no Cn data).
fn is_py_printable(ch: char) -> bool {
    let code = ch as u32;
    if ch == ' ' {
        return true;
    }
    if ch.is_control() {
        return false;
    }
    let non_printable: &[(u32, u32)] = &[
        // Zs (except U+0020)
        (0x00A0, 0x00A0),
        (0x1680, 0x1680),
        (0x2000, 0x200A),
        (0x202F, 0x202F),
        (0x205F, 0x205F),
        (0x3000, 0x3000),
        // Zl, Zp
        (0x2028, 0x2029),
        // Cf
        (0x00AD, 0x00AD),
        (0x0600, 0x0605),
        (0x061C, 0x061C),
        (0x06DD, 0x06DD),
        (0x070F, 0x070F),
        (0x0890, 0x0891),
        (0x08E2, 0x08E2),
        (0x180E, 0x180E),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x2064),
        (0x2066, 0x206F),
        (0xFEFF, 0xFEFF),
        (0xFFF9, 0xFFFB),
        (0x110BD, 0x110BD),
        (0x110CD, 0x110CD),
        (0x13430, 0x1343F),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0001, 0xE0001),
        (0xE0020, 0xE007F),
        // Co
        (0xE000, 0xF8FF),
        (0xF0000, 0xFFFFD),
        (0x10_0000, 0x10_FFFD),
    ];
    !non_printable
        .iter()
        .any(|(low, high)| (*low..=*high).contains(&code))
}

/// Python's `str.strip()` with no argument: Unicode whitespace, which
/// (unlike Rust's `White_Space`) includes the separators U+001C..U+001F.
#[must_use]
pub fn py_strip(text: &str) -> &str {
    let is_space = |ch: char| ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch);
    text.trim_matches(is_space)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1.0, "1.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123_456_789_012_345_680.0, "1.2345678901234568e+17"),
            (-2.5e-7, "-2.5e-07"),
            (1e100, "1e+100"),
            (-0.0, "-0.0"),
            (f64::NAN, "nan"),
            (f64::NEG_INFINITY, "-inf"),
            (12345.678, "12345.678"),
        ];
        let rendered: Vec<(f64, String)> = cases
            .iter()
            .map(|(value, _)| (*value, py_float_repr(*value)))
            .collect();
        let expected: Vec<(f64, String)> = cases
            .iter()
            .map(|(value, text)| (*value, (*text).to_string()))
            .collect();
        // NaN != NaN, so compare the rendered text column only.
        let texts =
            |rows: &[(f64, String)]| rows.iter().map(|row| row.1.clone()).collect::<Vec<_>>();
        assert_eq!(texts(&rendered), texts(&expected));
    }

    #[test]
    fn str_repr_matches_python() {
        let cases = [
            ("abc", "'abc'"),
            ("it's", "\"it's\""),
            ("say \"hi\" it's", "'say \"hi\" it\\'s'"),
            ("a\nb\tc\\", "'a\\nb\\tc\\\\'"),
            ("\u{7}", "'\\x07'"),
            ("\u{a0}x", "'\\xa0x'"),
            ("\u{200b}", "'\\u200b'"),
            ("caf\u{e9}", "'caf\u{e9}'"),
            ("\u{1F600}", "'\u{1F600}'"),
        ];
        let rendered: Vec<String> = cases.iter().map(|(text, _)| py_str_repr(text)).collect();
        let expected: Vec<String> = cases.iter().map(|(_, repr)| (*repr).to_string()).collect();
        assert_eq!(rendered, expected);
    }

    #[test]
    fn container_repr_matches_python() {
        let value = PyValue::Dict(vec![
            (
                PyValue::Str("a".into()),
                PyValue::List(vec![PyValue::Int(1), PyValue::Bool(true), PyValue::None]),
            ),
            (PyValue::Int(2), PyValue::Float(2.0)),
            (
                PyValue::Str("t".into()),
                PyValue::Opaque {
                    index: 0,
                    repr: "('x',)".into(),
                    truthy: true,
                    type_name: "tuple".into(),
                    json: Some("[\"x\"]".into()),
                },
            ),
        ]);
        assert_eq!(
            py_repr(&value),
            "{'a': [1, True, None], 2: 2.0, 't': ('x',)}"
        );
    }

    #[test]
    fn node_tables_round_trip() {
        let value = PyValue::Dict(vec![
            (
                PyValue::Str("states".into()),
                PyValue::List(vec![
                    PyValue::Str("a".into()),
                    PyValue::Float(f64::INFINITY),
                ]),
            ),
            (
                PyValue::Int(7),
                PyValue::BigInt("123456789012345678901234567890123456789012".into()),
            ),
            (
                PyValue::Str("o".into()),
                PyValue::Opaque {
                    index: 3,
                    repr: "{1, 2}".into(),
                    truthy: false,
                    type_name: "set".into(),
                    json: None,
                },
            ),
        ]);
        let table = encode_node_table(&value);
        assert_eq!(decode_node_table(&table), Ok(value));
    }

    #[test]
    fn malformed_tables_are_errors_not_panics() {
        let reused = json!({ "nodes": [["l", [1, 1]], ["n"]], "root": 0 });
        assert_eq!(
            decode_node_table(&reused),
            Err(NodeTableError("node 1 is referenced twice".into()))
        );
        let backwards = json!({ "nodes": [["n"], ["l", [0]]], "root": 1 });
        assert_eq!(
            decode_node_table(&backwards),
            Err(NodeTableError("node 1 has a bad child index".into()))
        );
    }

    #[test]
    fn strip_matches_python_whitespace() {
        assert_eq!(py_strip("\u{1c}\t name \u{2003}\n"), "name");
    }

    #[test]
    fn deep_tables_rebuild_without_recursion() {
        let depth = MAX_TABLE_DEPTH;
        let mut nodes = Vec::with_capacity(depth + 1);
        for index in 0..depth {
            nodes.push(json!(["l", [index + 1]]));
        }
        nodes.push(json!(["n"]));
        let table = json!({ "nodes": nodes, "root": 0 });
        let mut value = decode_node_table(&table).expect("deep table");
        let mut levels = 0;
        while let PyValue::List(mut items) = value {
            levels += 1;
            value = items.pop().expect("one child");
        }
        assert_eq!(levels, depth);
        let mut too_deep: Vec<Value> = (0..=depth).map(|index| json!(["l", [index + 1]])).collect();
        too_deep.push(json!(["n"]));
        assert_eq!(
            decode_node_table(&json!({ "nodes": too_deep, "root": 0 })),
            Err(NodeTableError(format!(
                "values nest deeper than {MAX_TABLE_DEPTH} levels"
            )))
        );
    }
}
