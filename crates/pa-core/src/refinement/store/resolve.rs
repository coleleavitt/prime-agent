//! `harness.resolve_factory`: everything a factory run needs from the
//! harness, in one request.
//!
//! `rlm.factory.run('<spec_id>')` names a stored factory entry first and a
//! library machine second, and every state's string `subagent` names a
//! harness subagent entry (by id, else the first by title). The kernel
//! client used to make those lookups one `harness.get`/`harness.list` call
//! at a time; this request makes them against the store in one read, with
//! the client's own routing (a `local:`/`global:` id prefix, the global
//! store the client resolved) and `run_factory`'s exact refusals. The reply
//! is what the client ships to `factory.run`: the spec id, the
//! `{"spec", "subagents"}` node table, and a library run's origin.

use serde_json::{json, Value};

use super::validate::Arg;
use super::{
    store_target, type_error, value_error, with_store, Outcome, Request, Session, StoreError,
    StoreErrorKind, StoreTarget,
};
use crate::factory::library::{resolve_machine, Fs, FsError, LibraryDirs, Raise};
use crate::factory::pyvalue::{decode_node_table, encode_node_table, PyValue};
use crate::refinement::{HarnessEntry, RefinementKind};

/// `_strip_scope_prefix`: an id shown as `local:<id>`/`global:<id>` routes
/// to that store (`true` for the global one).
fn strip_scope_prefix(id: &Arg) -> (Arg, bool) {
    if let Some((scope, rest)) = id.as_str().and_then(|id| id.split_once(':')) {
        if !rest.is_empty() && (scope == "local" || scope == "global") {
            return (Arg::new(json!(rest), Some("str")), scope == "global");
        }
    }
    (id.clone(), false)
}

/// The stores a lookup can route to.
struct Stores<'a> {
    global: Option<StoreTarget>,
    session: &'a Session,
}

impl Stores<'_> {
    /// `harness.get(kind, id)`: the global store for a `global:` id (when
    /// the client named one apart from this store), else this store.
    fn get(&self, kind: RefinementKind, id: &Arg) -> Result<Option<HarnessEntry>, StoreError> {
        let (id, global) = strip_scope_prefix(id);
        match self.global.as_ref().filter(|_| global) {
            Some(target) => {
                with_store(target, false, |store| Ok(store.lookup(kind, &id)?.cloned()))
                    .map(|(entry, _)| entry)
            }
            None => Ok(self.session.lookup(kind, &id)?.cloned()),
        }
    }

    /// One reference's spawn settings (`None` for an unknown reference):
    /// the subagent entry by id, else the first listed by title.
    fn subagent(&self, reference: &str) -> Result<Option<Value>, StoreError> {
        let by_id = self.get(
            RefinementKind::Subagent,
            &Arg::new(json!(reference), Some("str")),
        )?;
        let entry = by_id.or_else(|| {
            self.session
                .listed(&[RefinementKind::Subagent])
                .into_iter()
                .find(|entry| entry.title == reference)
        });
        Ok(entry.map(|entry| {
            let setting = |key: &str| entry.metadata.get(key).cloned().unwrap_or(Value::Null);
            json!({"content": entry.content, "model": setting("model"), "thinking": setting("thinking")})
        }))
    }
}

/// Every string `subagent` reference of a spec's states (or dag nodes), in
/// first-seen order.
fn references(spec: &PyValue) -> Vec<String> {
    let rows = if spec.has("states") || spec.has("transitions") {
        spec.get("states")
    } else {
        spec.get("nodes")
    };
    let mut seen: Vec<String> = Vec::new();
    for row in rows.as_list().unwrap_or_default() {
        if let Some(reference) = row.get("subagent").as_str() {
            if !seen.iter().any(|known| known == reference) {
                seen.push(reference.to_string());
            }
        }
    }
    seen
}

/// A stored factory entry's spec: `machine`, else `dag`.
fn entry_spec(entry: &HarnessEntry) -> PyValue {
    let field = |key: &str| {
        entry
            .arguments
            .get(key)
            .filter(|value| !value.is_null())
            .map(PyValue::from_json)
    };
    field("machine")
        .or_else(|| field("dag"))
        .unwrap_or(PyValue::None)
}

/// `run_factory`'s library fallback: the template's spec and origin, or the
/// refusal that frames why the id names nothing.
fn library_machine(id: &Arg, dirs: &LibraryDirs) -> Result<(PyValue, Value, Value), StoreError> {
    let name = PyValue::from_json(&id.value);
    let spec_id = id.repr();
    match resolve_machine(&Fs::here(), &name, dirs) {
        Ok((machine, path)) => Ok((
            machine.spec,
            json!(machine.name),
            json!(path.display().to_string()),
        )),
        Err(Raise::Resolution {
            message,
            broken: true,
        }) => Err(value_error(format!(
            "the library machine {spec_id} exists but is broken ({message})"
        ))),
        Err(Raise::Resolution {
            message,
            broken: false,
        }) => Err(value_error(format!(
            "unknown factory spec {spec_id}: no stored factory entry and no library machine \
             with that name ({message})"
        ))),
        // Only the name rule (and a file that stops decoding mid-resolve:
        // a UnicodeDecodeError is a ValueError) reaches this arm.
        Err(error @ (Raise::Value(_) | Raise::Fs(FsError::Decode { .. }))) => {
            Err(value_error(format!(
                "unknown factory spec {spec_id}: no stored factory entry, and the id is not a \
                 valid machine name either ({})",
                error.message()
            )))
        }
        Err(error @ Raise::Fs(FsError::Os { .. })) => {
            Err(StoreError::new(StoreErrorKind::Os, error.message()))
        }
        Err(Raise::Recursion(message)) => Err(StoreError::new(StoreErrorKind::Recursion, message)),
        Err(Raise::Type(message) | Raise::Attribute(message)) => Err(type_error(message)),
    }
}

/// `{"type": "harness.resolve_factory", "args": {"id": ...}}`, plus
/// `factorySpec` (a machine the caller holds: only its references
/// resolve), `globalStore` (the store a `global:` reference routes to), and
/// `library` (`[[source, dir], ...]`: fall back to the machine library).
pub(super) fn resolve_factory(
    target: &StoreTarget,
    request: &Request<'_>,
) -> Result<(Value, Outcome), StoreError> {
    let id = request.arg("id");
    let global = request
        .data
        .get("globalStore")
        .and_then(Value::as_object)
        .map(store_target);
    let held = request
        .data
        .get("factorySpec")
        .map(decode_node_table)
        .transpose()
        .map_err(|error| type_error(format!("harness request {error}")))?;
    let library = request
        .data
        .get("library")
        .and_then(Value::as_array)
        .map(|levels| {
            LibraryDirs(
                levels
                    .iter()
                    .filter_map(|level| {
                        let pair = level.as_array()?;
                        Some((
                            pair.first()?.as_str()?.to_string(),
                            pair.get(1)?.as_str()?.into(),
                        ))
                    })
                    .collect(),
            )
        });
    with_store(target, false, |session| {
        let stores = Stores { global, session };
        let mut reply = json!({"spec_id": id.value});
        let spec = match held {
            Some(spec) => spec,
            None => {
                if let Some(entry) = stores.get(RefinementKind::Factory, &id)? {
                    reply["spec_id"] = json!(entry.id);
                    entry_spec(&entry)
                } else {
                    let Some(dirs) = library.as_ref() else {
                        return Err(value_error(format!("unknown factory spec {}", id.repr())));
                    };
                    let (spec, name, path) = library_machine(&id, dirs)?;
                    reply["spec_id"] = name.clone();
                    reply["machine"] = name;
                    reply["machine_path"] = path;
                    spec
                }
            }
        };
        let mut subagents = Vec::new();
        if spec.is_dict() {
            for reference in references(&spec) {
                let resolved = stores.subagent(&reference)?;
                subagents.push((
                    PyValue::Str(reference),
                    resolved.as_ref().map_or(PyValue::None, PyValue::from_json),
                ));
            }
        }
        let value = PyValue::Dict(vec![
            (PyValue::Str("spec".into()), spec),
            (PyValue::Str("subagents".into()), PyValue::Dict(subagents)),
        ]);
        reply["value"] = encode_node_table(&value);
        Ok(reply)
    })
}
