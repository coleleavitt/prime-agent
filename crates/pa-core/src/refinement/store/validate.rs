//! Call arguments as the kernel sent them, and the write-time validation
//! every kernel harness write passes before anything is stored.
//!
//! The kernel API is Python: an argument may be any Python value, so each
//! one arrives as its JSON form plus its Python type name (the rejection
//! messages name the type, and JSON cannot tell a tuple from a list).
//! Values with no JSON form arrive as an [`UNSERIALIZABLE_KEY`] marker; one
//! that survives validation (nested inside a dict) refuses the save with
//! Python's own `json.dump` error.

use serde_json::{Map, Value};

use super::pyfmt;
use super::{StoreError, StoreErrorKind};

/// The marker key a kernel client puts in place of a value JSON cannot
/// carry: `{"__rlm_harness_unserializable__": "<Python type name>"}`.
pub const UNSERIALIZABLE_KEY: &str = "__rlm_harness_unserializable__";

/// One call argument: its JSON form (`null` for Python `None`) and its
/// Python type name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Arg {
    pub(crate) value: Value,
    pub(crate) type_name: String,
}

impl Arg {
    pub(crate) fn new(value: Value, type_name: Option<&str>) -> Self {
        let type_name = type_name
            .map(str::to_string)
            .or_else(|| unserializable_type(&value).map(str::to_string))
            .unwrap_or_else(|| pyfmt::type_name(&value).to_string());
        Self { value, type_name }
    }

    pub(crate) fn none() -> Self {
        Self::new(Value::Null, None)
    }

    pub(crate) fn is_none(&self) -> bool {
        self.value.is_null()
    }

    /// The value when it is a Python `str` (subclasses included).
    pub(crate) fn as_str(&self) -> Option<&str> {
        self.value.as_str()
    }

    /// The value when it is a Python `dict` (subclasses included).
    pub(crate) fn as_record(&self) -> Option<&Map<String, Value>> {
        self.value
            .as_object()
            .filter(|map| !map.contains_key(UNSERIALIZABLE_KEY))
    }

    /// The value when it is a Python `list` (a tuple is not one).
    pub(crate) fn as_list(&self) -> Option<&Vec<Value>> {
        self.value.as_array().filter(|_| self.type_name != "tuple")
    }

    /// Python truthiness.
    pub(crate) fn is_truthy(&self) -> bool {
        match &self.value {
            Value::Null => false,
            Value::Bool(flag) => *flag,
            Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
            Value::String(text) => !text.is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(map) => map.contains_key(UNSERIALIZABLE_KEY) || !map.is_empty(),
        }
    }

    /// `repr(value)`.
    pub(crate) fn repr(&self) -> String {
        match unserializable_type(&self.value) {
            Some(type_name) => format!("<{type_name} object>"),
            None => pyfmt::repr(&self.value),
        }
    }

    /// Python `_type_name`: "a list", "an empty string", or the type name.
    pub(crate) fn type_display(&self) -> String {
        if self.type_name == "list" {
            return "a list".to_string();
        }
        if self.value.as_str() == Some("") {
            return "an empty string".to_string();
        }
        self.type_name.clone()
    }

    /// Whether the value can be a `dict` key (an id or kind lookup): a
    /// list, dict, or set lookup raises Python's `TypeError`.
    pub(crate) fn require_hashable(&self) -> Result<(), StoreError> {
        if matches!(
            self.type_name.as_str(),
            "list" | "dict" | "set" | "bytearray"
        ) {
            return Err(StoreError::new(
                StoreErrorKind::Type,
                format!("unhashable type: '{}'", self.type_name),
            ));
        }
        Ok(())
    }
}

fn unserializable_type(value: &Value) -> Option<&str> {
    value
        .as_object()
        .and_then(|map| map.get(UNSERIALIZABLE_KEY))
        .and_then(Value::as_str)
}

/// The first value inside `value` that JSON cannot carry, as Python's
/// `json.dump` reports it.
pub(crate) fn find_unserializable(value: &Value) -> Option<StoreError> {
    match value {
        Value::Object(map) => {
            if let Some(type_name) = unserializable_type(value) {
                return Some(StoreError::new(
                    StoreErrorKind::Type,
                    format!("Object of type {type_name} is not JSON serializable"),
                ));
            }
            map.values().find_map(find_unserializable)
        }
        Value::Array(items) => items.iter().find_map(find_unserializable),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn value_error(message: String) -> StoreError {
    StoreError::new(StoreErrorKind::Value, message)
}

/// The best available entry name for rejection messages.
pub(crate) fn describe_entry(id: &Arg, title: &Arg) -> String {
    id.as_str()
        .filter(|id| !id.is_empty())
        .or_else(|| title.as_str().filter(|title| !title.is_empty()))
        .unwrap_or("<unnamed>")
        .to_string()
}

pub(crate) fn require_text(
    kind: &str,
    entry_name: &str,
    field: &str,
    value: &Arg,
) -> Result<(), StoreError> {
    if value.as_str().is_some_and(|text| !text.is_empty()) {
        return Ok(());
    }
    Err(value_error(format!(
        "{kind} entry {} rejected: {field} must be a non-empty string, got {}",
        pyfmt::repr_str(entry_name),
        value.type_display()
    )))
}

fn require_optional_text(
    kind: &str,
    entry_name: &str,
    field: &str,
    value: &Arg,
) -> Result<(), StoreError> {
    if value.is_none() {
        return Ok(());
    }
    require_text(kind, entry_name, field, value)
}

fn require_optional_record(
    kind: &str,
    entry_name: &str,
    field: &str,
    value: &Arg,
) -> Result<(), StoreError> {
    if value.is_none() || value.as_record().is_some() {
        return Ok(());
    }
    Err(value_error(format!(
        "{kind} entry {} rejected: {field} must be a dict when provided, got {}",
        pyfmt::repr_str(entry_name),
        value.type_display()
    )))
}

/// A skill's Python reference: `{"type": "python"}`, an import, and a
/// callable or call pattern.
pub(crate) fn validate_python_skill_reference(
    reference: &Arg,
    entry_name: &str,
) -> Result<(), StoreError> {
    let prefix = if entry_name.is_empty() {
        String::new()
    } else {
        format!("skill entry {} rejected: ", pyfmt::repr_str(entry_name))
    };
    let reject = |message: &str| Err(value_error(format!("{prefix}{message}")));
    let Some(reference) = reference.as_record() else {
        return reject("skill entries require a Python reference");
    };
    if reference.get("type").and_then(Value::as_str) != Some("python") {
        return reject("skill reference.type must be 'python'");
    }
    let present = |keys: [&str; 2]| {
        keys.iter().any(|key| {
            reference
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
        })
    };
    if !present(["import", "python_import"]) {
        return reject("skill reference requires a Python import");
    }
    if !present(["callable", "call_pattern"]) {
        return reject("skill reference requires a callable or call_pattern");
    }
    Ok(())
}

/// The factory spec checks a write needs beyond the entry's shape.
#[derive(Debug, Clone, Default)]
pub(crate) struct FactoryChecks {
    /// The agent dir whose `settings.json` holds the `factory.enabled`
    /// opt-in (read only when a factory write asks).
    pub(crate) agent_dir: Option<std::path::PathBuf>,
    /// What the kernel's factory validator (`rlm.factory.validate_factory_spec`)
    /// reported for the spec this write stores; `None` when it was not run.
    pub(crate) spec_errors: Option<Vec<String>>,
}

impl FactoryChecks {
    pub(crate) fn require_enabled(&self) -> Result<(), StoreError> {
        if self
            .agent_dir
            .as_deref()
            .is_some_and(crate::refinement::factory_enabled)
        {
            return Ok(());
        }
        Err(value_error(
            crate::refinement::FACTORY_DISABLED_MESSAGE.to_string(),
        ))
    }

    fn spec_errors(&self) -> Result<&[String], StoreError> {
        self.spec_errors.as_deref().ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::Runtime,
                "factory spec validation results are missing from the request".to_string(),
            )
        })
    }

    /// The spec errors as one rejection (`"; "`-joined), if any.
    pub(crate) fn check_spec(&self, prefix: &str) -> Result<(), StoreError> {
        let errors = self.spec_errors()?;
        if errors.is_empty() {
            return Ok(());
        }
        Err(value_error(format!("{prefix}{}", errors.join("; "))))
    }
}

/// The shared-path dry run for a factory's `arguments`.
fn validate_factory_arguments(
    entry_name: &str,
    arguments: &Map<String, Value>,
    factory: &FactoryChecks,
) -> Result<(), StoreError> {
    let prefix = format!("factory entry {} rejected: ", pyfmt::repr_str(entry_name));
    let present = |key: &str| arguments.get(key).filter(|value| !value.is_null());
    let (dag, machine) = (present("dag"), present("machine"));
    if dag.is_some() && machine.is_some() {
        return Err(value_error(format!(
            "{prefix}pass either dag or machine, not both"
        )));
    }
    let spec = machine.or(dag);
    if spec.is_none_or(|spec| Arg::new(spec.clone(), None).as_record().is_none()) {
        return Err(value_error(format!(
            "{prefix}factory entries require a dag or machine object in arguments"
        )));
    }
    factory.check_spec(&prefix)
}

/// The fields one create/update/upsert writes.
pub(crate) struct EntryFields<'a> {
    pub(crate) kind: &'a str,
    pub(crate) id: &'a Arg,
    pub(crate) title: &'a Arg,
    pub(crate) content: &'a Arg,
    pub(crate) path: &'a Arg,
    pub(crate) reference: &'a Arg,
    pub(crate) arguments: &'a Arg,
    pub(crate) metadata: &'a Arg,
    pub(crate) source: &'a Arg,
}

/// Reject an invalid harness entry before anything is persisted, naming
/// the entry and the field (a malformed entry would otherwise crash the
/// digest that renders every session's system prompt).
pub(crate) fn validate_entry_shape(
    fields: &EntryFields<'_>,
    exists: bool,
    factory: &FactoryChecks,
) -> Result<(), StoreError> {
    let kind = fields.kind;
    let entry_name = describe_entry(fields.id, fields.title);
    require_text(kind, &entry_name, "id", fields.id)?;
    require_text(kind, &entry_name, "title", fields.title)?;
    require_text(kind, &entry_name, "content", fields.content)?;
    require_optional_text(kind, &entry_name, "path", fields.path)?;
    require_optional_record(kind, &entry_name, "reference", fields.reference)?;
    require_optional_record(kind, &entry_name, "arguments", fields.arguments)?;
    require_optional_record(kind, &entry_name, "metadata", fields.metadata)?;
    require_text(kind, &entry_name, "source", fields.source)?;
    if kind == "skill" {
        if fields.reference.is_none() {
            // An update that omits the reference keeps the stored one.
            if !exists {
                return Err(value_error(format!(
                    "skill entry {} rejected: skill entries require a Python reference",
                    pyfmt::repr_str(&entry_name)
                )));
            }
        } else {
            validate_python_skill_reference(fields.reference, &entry_name)?;
        }
    }
    if kind == "factory" {
        // The opt-in gate precedes any spec work; an update that omits the
        // arguments keeps the stored (validated) spec.
        factory.require_enabled()?;
        match fields.arguments.as_record() {
            None if !exists => {
                return Err(value_error(format!(
                    "factory entry {} rejected: factory entries require a dag or machine object in arguments",
                    pyfmt::repr_str(&entry_name)
                )));
            }
            None => {}
            Some(arguments) => validate_factory_arguments(&entry_name, arguments, factory)?,
        }
    }
    Ok(())
}

/// Reject a refinement event whose persisted shape would break the digest.
pub(crate) fn validate_refinement_event(
    trigger: &Arg,
    changes: &Arg,
    evidence: &Arg,
    outcome: &Arg,
) -> Result<(), StoreError> {
    if trigger.as_str().is_none_or(str::is_empty) {
        return Err(value_error(format!(
            "refinement event rejected: trigger must be a non-empty string, got {}",
            trigger.type_display()
        )));
    }
    if let Some(change) = changes.as_str() {
        if change.is_empty() {
            return Err(value_error(
                "refinement event rejected: changes must be a non-empty string or a list of strings"
                    .to_string(),
            ));
        }
    } else if let Some(items) = changes.as_list() {
        if !items
            .iter()
            .all(|item| item.as_str().is_some_and(|text| !text.is_empty()))
        {
            return Err(value_error(
                "refinement event rejected: changes must be a list of non-empty strings"
                    .to_string(),
            ));
        }
    } else {
        return Err(value_error(format!(
            "refinement event rejected: changes must be a string or a list of strings, got {}",
            changes.type_display()
        )));
    }
    for (field, value) in [("evidence", evidence), ("outcome", outcome)] {
        if value.as_str().is_none() {
            return Err(value_error(format!(
                "refinement event rejected: {field} must be a string when provided, got {}",
                value.type_display()
            )));
        }
    }
    Ok(())
}
