//! The harness store: the one implementation behind every reader and writer
//! of `harness_state.json`, including the kernel's `rlm.harness` API.
//!
//! [`document`] owns the file (lenient read, durable atomic write). This
//! module owns the kernel-facing operations (`rlm.harness.create`,
//! `get`, `list`, `set_enabled`, `search`, `overview`, ...) with the exact
//! validation, id minting, versioning and messages the kernel API has
//! always had; the runtime's `rlm/harness.py` is a thin client that sends
//! each call as a `harness.<op>` host request ([`handle_request`]). Outside
//! a kernel (a plain Python process) the same requests go through the
//! `prime-agent --prime-agent-harness-request` one-shot.
//!
//! Which store a call targets (the session-local store, the global one, an
//! explicit file, or the kernel's in-memory fallback) is the client's
//! decision: it resolves the kernel's `RLM_*` environment and sends the
//! file. Package harness overlays are never a target: they are read-only
//! and the kernel API does not see them.

pub mod document;
mod overview;
mod pyfmt;
mod search;
mod validate;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::refinement::{
    HarnessEntry, HarnessRefinementEvent, HarnessScope, HarnessState, RefinementKind,
};
use document::{
    parse_harness_document, read_harness_state_file, write_harness_state_file, WriteDurability,
};
use validate::{Arg, EntryFields, FactoryChecks, FactorySpec};

pub use document::{LoadedHarnessState, LEGACY_ENTRY_SOURCE};
pub use validate::UNSERIALIZABLE_KEY;

/// The kinds in the kernel's order (`_KINDS`): listings and the overview
/// walk them in this order.
const KINDS: [(&str, RefinementKind); 5] = [
    ("prompt", RefinementKind::Prompt),
    ("memory", RefinementKind::Memory),
    ("skill", RefinementKind::Skill),
    ("subagent", RefinementKind::Subagent),
    ("factory", RefinementKind::Factory),
];

/// How a write takes the store's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockPolicy {
    /// How long a write waits for one holder: past the stale window, so a
    /// crashed holder's leftover is always reclaimed first.
    wait: Duration,
    retry: Duration,
    /// A lock this old is a crashed holder's leftover (the TS host's
    /// `HARNESS_STATE_LOCK_STALE_MS`).
    stale: Duration,
}

const STORE_LOCK: LockPolicy = LockPolicy {
    wait: Duration::from_secs(15),
    retry: Duration::from_millis(5),
    stale: Duration::from_secs(10),
};

/// The Python exception class a store call raises in the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    Value,
    Type,
    Runtime,
    Timeout,
    Os,
}

impl StoreErrorKind {
    fn python_name(self) -> &'static str {
        match self {
            StoreErrorKind::Value => "ValueError",
            StoreErrorKind::Type => "TypeError",
            StoreErrorKind::Runtime => "RuntimeError",
            StoreErrorKind::Timeout => "TimeoutError",
            StoreErrorKind::Os => "OSError",
        }
    }
}

/// A refused or failed store call: the kernel raises `kind` with `message`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", kind.python_name())]
pub struct StoreError {
    pub kind: StoreErrorKind,
    pub message: String,
}

impl StoreError {
    fn new(kind: StoreErrorKind, message: String) -> Self {
        Self { kind, message }
    }
}

fn value_error(message: String) -> StoreError {
    StoreError::new(StoreErrorKind::Value, message)
}

fn type_error(message: String) -> StoreError {
    StoreError::new(StoreErrorKind::Type, message)
}

fn os_error(error: &anyhow::Error) -> StoreError {
    StoreError::new(StoreErrorKind::Os, format!("{error:#}"))
}

/// Where one call's store lives.
#[derive(Debug, Clone)]
enum StoreLocation {
    /// A state file.
    File(PathBuf),
    /// The kernel's in-memory store: the client holds the document.
    Memory(Value),
}

/// The store one call targets, as the client resolved it.
#[derive(Debug, Clone)]
struct StoreTarget {
    location: StoreLocation,
    scope: HarnessScope,
    /// Set for a store that refuses writes (the kernel without a session
    /// store): every write raises it as a `RuntimeError`.
    write_error: Option<String>,
    lock: LockPolicy,
}

impl StoreTarget {
    fn file_label(&self) -> String {
        match &self.location {
            StoreLocation::File(path) => path.display().to_string(),
            StoreLocation::Memory(_) => "None".to_string(),
        }
    }
}

/// One call's view of its store.
struct Session {
    state: HarnessState,
    scope: HarnessScope,
    load_error: Option<String>,
    dirty: bool,
}

/// Take the store's lock for one write. The holder keeps it fresh with a
/// heartbeat, so a write whose synced save outlasts the stale window (an
/// fsync under I/O pressure) is never judged a crashed holder's leftover
/// and has its lock taken by a concurrent writer mid-write.
///
/// The wait bounds how long ONE holder keeps the lock, not how long this
/// write queues: each time the lock changes hands the wait restarts, so a
/// convoy of writers that each hold briefly never times a write out.
fn lock_store(
    path: &Path,
    policy: LockPolicy,
) -> Result<crate::platform::HeartbeatLock, StoreError> {
    use crate::platform::LockDir;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| StoreError::new(StoreErrorKind::Os, error.to_string()))?;
    }
    let lock_path = LockDir::path_for(path);
    let mut holder = None;
    let mut deadline = Instant::now() + policy.wait;
    loop {
        match LockDir::acquire(path, policy.stale) {
            Ok(held) => return Ok(held.with_heartbeat(policy.stale / 2)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let current = LockDir::holder_at(&lock_path);
                if current.is_some() && current != holder {
                    holder = current;
                    deadline = Instant::now() + policy.wait;
                } else if Instant::now() >= deadline {
                    return Err(StoreError::new(
                        StoreErrorKind::Timeout,
                        format!(
                            "harness state is locked by another process: {} (held longer than {}s)",
                            lock_path.display(),
                            policy.wait.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(policy.retry);
            }
            Err(error) => return Err(StoreError::new(StoreErrorKind::Os, error.to_string())),
        }
    }
}

/// What a finished call hands back besides its result.
struct Outcome {
    state: HarnessState,
    load_error: Option<String>,
}

/// Run `op` against the target store: a write holds the file's lock across
/// its read, change and save, so a concurrent writer (another kernel, the
/// host's `/refine`) never lands in between.
fn with_store<T>(
    target: &StoreTarget,
    write: bool,
    op: impl FnOnce(&mut Session) -> Result<T, StoreError>,
) -> Result<(T, Outcome), StoreError> {
    if write {
        if let Some(message) = &target.write_error {
            return Err(StoreError::new(StoreErrorKind::Runtime, message.clone()));
        }
    }
    let (_lock, loaded) = match &target.location {
        StoreLocation::File(path) => {
            let lock = if write {
                Some(lock_store(path, target.lock)?)
            } else {
                None
            };
            (lock, read_harness_state_file(path, target.scope))
        }
        StoreLocation::Memory(document) => (
            None,
            LoadedHarnessState {
                state: parse_harness_document(document, target.scope),
                load_error: None,
            },
        ),
    };
    let mut session = Session {
        state: loaded.state,
        scope: target.scope,
        load_error: loaded.load_error,
        dirty: false,
    };
    let result = op(&mut session)?;
    if session.dirty {
        let unserializable = session
            .state
            .entries
            .values()
            .flat_map(|entries| entries.values())
            .find_map(|entry| {
                [&entry.reference, &entry.arguments, &entry.metadata]
                    .into_iter()
                    .find_map(|record| {
                        validate::find_unserializable(&Value::Object(record.clone()))
                    })
            });
        if let Some(error) = unserializable {
            return Err(error);
        }
        if let StoreLocation::File(path) = &target.location {
            write_harness_state_file(path, &session.state, WriteDurability::Sync)
                .map_err(|error| os_error(&error))?;
        }
        session.load_error = None;
    }
    Ok((
        result,
        Outcome {
            state: session.state,
            load_error: session.load_error,
        },
    ))
}

/// `kind` as one of the store's kinds.
fn harness_kind(kind: &Arg) -> Result<(&'static str, RefinementKind), StoreError> {
    kind.require_hashable()?;
    kind.as_str()
        .and_then(|name| KINDS.into_iter().find(|(known, _)| *known == name))
        .ok_or_else(|| {
            value_error(format!(
                "unknown harness kind {}; expected one of ('prompt', 'memory', 'skill', 'subagent', 'factory')",
                kind.repr()
            ))
        })
}

fn record_of(arg: &Arg) -> Option<Map<String, Value>> {
    arg.as_record().cloned()
}

impl Session {
    fn bucket(
        &mut self,
        kind: RefinementKind,
    ) -> &mut std::collections::BTreeMap<String, HarnessEntry> {
        self.state.entries.entry(kind).or_default()
    }

    fn lookup(&self, kind: RefinementKind, id: &Arg) -> Result<Option<&HarnessEntry>, StoreError> {
        id.require_hashable()?;
        Ok(id.as_str().and_then(|id| {
            self.state
                .entries
                .get(&kind)
                .and_then(|bucket| bucket.get(id))
        }))
    }

    /// Entries of `kinds`, ordered by kind, path, title and id.
    fn listed(&self, kinds: &[RefinementKind]) -> Vec<HarnessEntry> {
        let mut entries: Vec<HarnessEntry> = kinds
            .iter()
            .filter_map(|kind| self.state.entries.get(kind))
            .flat_map(|bucket| bucket.values().cloned())
            .collect();
        // Python sorts on the kind's name, so the order is alphabetical.
        let kind_name = |entry: &HarnessEntry| {
            KINDS
                .iter()
                .find(|(_, kind)| *kind == entry.kind)
                .map_or("", |(name, _)| *name)
        };
        entries.sort_by(|left, right| {
            (kind_name(left), &left.path, &left.title, &left.id).cmp(&(
                kind_name(right),
                &right.path,
                &right.title,
                &right.id,
            ))
        });
        entries
    }

    /// `list(kind)`: one kind, or every kind for a falsy `kind`.
    fn list(&self, kind: &Arg) -> Result<Vec<HarnessEntry>, StoreError> {
        if !kind.is_truthy() {
            let all: Vec<RefinementKind> = KINDS.iter().map(|(_, kind)| *kind).collect();
            return Ok(self.listed(&all));
        }
        let (_, kind) = harness_kind(kind)?;
        Ok(self.listed(&[kind]))
    }

    /// The shared create/update/upsert write (`_upsert`).
    fn upsert(
        &mut self,
        kind: &Arg,
        fields: &WriteFields,
        factory: &FactoryChecks,
    ) -> Result<HarnessEntry, StoreError> {
        let (kind_name, kind_key) = harness_kind(kind)?;
        let entry_name = validate::describe_entry(&fields.id, &fields.title);
        validate::require_text(kind_name, &entry_name, "title", &fields.title)?;
        if !fields.id.is_none() {
            validate::require_text(kind_name, &entry_name, "id", &fields.id)?;
        }
        let entry_id = match fields.id.as_str() {
            Some(id) => id.to_string(),
            None => search::slug(fields.title.as_str().unwrap_or_default(), kind_name),
        };
        let id_arg = Arg::new(Value::String(entry_id.clone()), None);
        let exists = self.bucket(kind_key).contains_key(&entry_id);
        validate::validate_entry_shape(
            &EntryFields {
                kind: kind_name,
                id: &id_arg,
                title: &fields.title,
                content: &fields.content,
                path: &fields.path,
                reference: &fields.reference,
                arguments: &fields.arguments,
                metadata: &fields.metadata,
                source: &fields.source,
            },
            exists,
            factory,
        )?;
        let now = pyfmt::isoformat_now();
        let text = |arg: &Arg| arg.as_str().unwrap_or_default().to_string();
        let scope = self.scope;
        let bucket = self.bucket(kind_key);
        let entry = if let Some(existing) = bucket.get_mut(&entry_id) {
            existing.title = text(&fields.title);
            existing.content = text(&fields.content);
            // Omitted (None) fields keep their stored value, so a
            // content-only update keeps a skill's contract and an entry's
            // grouping path; an explicit value (even `{}`) replaces it.
            if let Some(path) = fields.path.as_str() {
                existing.path = path.to_string();
            }
            if let Some(reference) = record_of(&fields.reference) {
                existing.reference = reference;
            }
            if let Some(arguments) = record_of(&fields.arguments) {
                existing.arguments = arguments;
            }
            if let Some(metadata) = record_of(&fields.metadata) {
                existing.metadata = metadata;
            }
            existing.source = text(&fields.source);
            existing.updated_at = now;
            existing.version += 1;
            existing.clone()
        } else {
            let entry = HarnessEntry {
                id: entry_id.clone(),
                kind: kind_key,
                title: text(&fields.title),
                content: text(&fields.content),
                path: fields.path.as_str().unwrap_or("general").to_string(),
                scope: Some(scope),
                reference: record_of(&fields.reference).unwrap_or_default(),
                arguments: record_of(&fields.arguments).unwrap_or_default(),
                metadata: record_of(&fields.metadata).unwrap_or_default(),
                source: text(&fields.source),
                created_at: now.clone(),
                updated_at: now,
                version: 1,
                extensions: Map::new(),
            };
            bucket.insert(entry_id, entry.clone());
            entry
        };
        self.dirty = true;
        Ok(entry)
    }

    /// `create`: an id that already exists is refused.
    fn create(
        &mut self,
        kind: &Arg,
        fields: &WriteFields,
        factory: &FactoryChecks,
    ) -> Result<HarnessEntry, StoreError> {
        let (kind_name, kind_key) = harness_kind(kind)?;
        let entry_name = validate::describe_entry(&fields.id, &fields.title);
        validate::require_text(kind_name, &entry_name, "title", &fields.title)?;
        if !fields.id.is_none() {
            validate::require_text(kind_name, &entry_name, "id", &fields.id)?;
        }
        let entry_id = match fields.id.as_str() {
            Some(id) => id.to_string(),
            None => search::slug(fields.title.as_str().unwrap_or_default(), kind_name),
        };
        if self.bucket(kind_key).contains_key(&entry_id) {
            return Err(value_error(format!(
                "{kind_name} entry {} already exists",
                pyfmt::repr_str(&entry_id)
            )));
        }
        let fields = WriteFields {
            id: Arg::new(Value::String(entry_id), None),
            ..fields.clone()
        };
        self.upsert(kind, &fields, factory)
    }

    /// `update`: the entry must exist.
    fn update(
        &mut self,
        kind: &Arg,
        fields: &WriteFields,
        factory: &FactoryChecks,
    ) -> Result<HarnessEntry, StoreError> {
        let (kind_name, kind_key) = harness_kind(kind)?;
        let entry_name = validate::describe_entry(&fields.id, &fields.title);
        validate::require_text(kind_name, &entry_name, "id", &fields.id)?;
        let id = fields.id.as_str().unwrap_or_default();
        if !self.bucket(kind_key).contains_key(id) {
            return Err(value_error(format!(
                "{kind_name} entry {} does not exist",
                fields.id.repr()
            )));
        }
        self.upsert(kind, fields, factory)
    }
}

/// The fields of one entry write, as sent.
#[derive(Debug, Clone)]
struct WriteFields {
    id: Arg,
    title: Arg,
    content: Arg,
    path: Arg,
    reference: Arg,
    arguments: Arg,
    metadata: Arg,
    source: Arg,
}

/// One decoded request: its arguments with their Python types.
struct Request<'a> {
    data: &'a Value,
}

impl Request<'_> {
    fn arg(&self, name: &str) -> Arg {
        let value = self
            .data
            .get("args")
            .and_then(|args| args.get(name))
            .cloned()
            .unwrap_or(Value::Null);
        let type_name = self
            .data
            .get("types")
            .and_then(|types| types.get(name))
            .and_then(Value::as_str);
        Arg::new(value, type_name)
    }

    fn write_fields(&self) -> WriteFields {
        WriteFields {
            id: self.arg("id"),
            title: self.arg("title"),
            content: self.arg("content"),
            path: self.arg("path"),
            reference: self.arg("reference"),
            arguments: self.arg("arguments"),
            metadata: self.arg("metadata"),
            source: self.arg("source"),
        }
    }

    /// The factory checks one request needs. `factoryArguments` is the
    /// Python value of the arguments a factory write stores (a node table:
    /// the validator's rules are Python semantics), and the spec in them is
    /// validated as `rlm.harness` always did: a generic write's `machine`
    /// (else `dag`) when it is an object, a `create_factory` /
    /// `update_factory` spec when it is created or replaced. A client that
    /// ran the validator itself sends `factorySpecErrors` instead.
    fn factory_checks(&self, request_type: &str) -> Result<FactoryChecks, StoreError> {
        let agent_dir = self
            .data
            .get("agentDir")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let spec = match self.data.get("factoryArguments") {
            Some(table) => {
                let arguments = crate::factory::pyvalue::decode_node_table(table)
                    .map_err(|error| type_error(format!("harness request {error}")))?;
                let machine = arguments.get("machine");
                let spec = if machine.is_none() {
                    arguments.get("dag")
                } else {
                    machine
                };
                let stored = if request_type == "harness.factory" {
                    self.arg("create").value.as_bool().unwrap_or(false) || !spec.is_none()
                } else {
                    spec.is_dict()
                };
                if stored {
                    FactorySpec::Value(spec.clone())
                } else {
                    FactorySpec::Missing
                }
            }
            None => self
                .data
                .get("factorySpecErrors")
                .and_then(Value::as_array)
                .map_or(FactorySpec::Missing, |errors| {
                    FactorySpec::Reported(errors.iter().map(pyfmt::str_of).collect())
                }),
        };
        Ok(FactoryChecks { agent_dir, spec })
    }

    fn target(&self) -> Result<StoreTarget, StoreError> {
        let store = self
            .data
            .get("store")
            .and_then(Value::as_object)
            .ok_or_else(|| type_error("harness request carries no store".to_string()))?;
        let scope = match store.get("scope").and_then(Value::as_str) {
            Some("global") => HarnessScope::Global,
            _ => HarnessScope::Local,
        };
        let location = match store.get("file").and_then(Value::as_str) {
            Some(file) => StoreLocation::File(PathBuf::from(file)),
            None => StoreLocation::Memory(store.get("document").cloned().unwrap_or(Value::Null)),
        };
        Ok(StoreTarget {
            location,
            scope,
            write_error: store
                .get("writeError")
                .and_then(Value::as_str)
                .map(str::to_string),
            lock: STORE_LOCK,
        })
    }
}

fn entry_json(entry: &HarnessEntry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

fn event_json(event: &HarnessRefinementEvent) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}

/// `[:limit]` for the overview: an int (negative counts from the end) or
/// `None`.
fn slice_limit(limit: &Arg) -> Result<Option<i64>, StoreError> {
    match &limit.value {
        Value::Null => Ok(None),
        Value::Bool(flag) => Ok(Some(i64::from(*flag))),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            Ok(Some(number.as_i64().unwrap_or(i64::MAX)))
        }
        Value::Number(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            Err(type_error(
                "slice indices must be integers or None or have an __index__ method".to_string(),
            ))
        }
    }
}

fn search(session: &Session, request: &Request<'_>) -> Result<Value, StoreError> {
    let query = request.arg("query");
    let Some(query) = query.as_str() else {
        return Err(type_error(format!(
            "query must be str, got {}",
            query.type_name
        )));
    };
    let limit = request.arg("limit");
    let limit = match (&limit.value, limit.type_name.as_str()) {
        (Value::Number(number), "int") => number.as_u64().filter(|limit| *limit >= 1),
        _ => None,
    }
    .ok_or_else(|| type_error("limit must be a positive int".to_string()))?;
    let terms = search::query_terms(query);
    if terms.is_empty() {
        return Ok(json!([]));
    }
    let entries = session.list(&request.arg("kind"))?;
    let entries: Vec<&HarnessEntry> = entries.iter().collect();
    let ranked = search::rank(
        &entries,
        &terms,
        usize::try_from(limit).unwrap_or(usize::MAX),
    );
    Ok(Value::Array(ranked.into_iter().map(entry_json).collect()))
}

fn snapshot(session: &Session, target: &StoreTarget) -> Value {
    let entries: Map<String, Value> = KINDS
        .iter()
        .map(|(name, kind)| {
            let records: Map<String, Value> = session
                .state
                .entries
                .get(kind)
                .into_iter()
                .flatten()
                .map(|(id, entry)| (id.clone(), entry_json(entry)))
                .collect();
            ((*name).to_string(), Value::Object(records))
        })
        .collect();
    json!({
        "file_path": target.file_label(),
        "scope": overview::scope_label(target.scope),
        "entries": entries,
        "refinements": session.state.refinements.iter().map(event_json).collect::<Vec<_>>(),
    })
}

fn set_enabled(session: &mut Session, request: &Request<'_>) -> Result<Value, StoreError> {
    let (kind_name, kind_key) = harness_kind(&request.arg("kind"))?;
    let id = request.arg("id");
    if session.lookup(kind_key, &id)?.is_none() {
        return Err(value_error(format!(
            "{kind_name} entry {} does not exist",
            id.repr()
        )));
    }
    let enabled = request.arg("enabled").value.as_bool().unwrap_or(true);
    let key = id.as_str().unwrap_or_default().to_string();
    let entry = session
        .bucket(kind_key)
        .get_mut(&key)
        .expect("the lookup above found the entry");
    entry.set_enabled(enabled);
    entry.updated_at = pyfmt::isoformat_now();
    let entry = entry.clone();
    session.dirty = true;
    Ok(entry_json(&entry))
}

fn record_refinement(session: &mut Session, request: &Request<'_>) -> Result<Value, StoreError> {
    let changes = request.arg("changes");
    validate::validate_refinement_event(
        &request.arg("trigger"),
        &changes,
        &request.arg("evidence"),
        &request.arg("outcome"),
    )?;
    let id = request.arg("id");
    if !id.is_none() && id.as_str().is_none_or(str::is_empty) {
        return Err(value_error(format!(
            "refinement event rejected: id must be a non-empty string when provided, got {}",
            id.type_display()
        )));
    }
    let changes = match changes.as_str() {
        Some(change) => vec![change.to_string()],
        None => changes
            .as_list()
            .map(|items| items.iter().map(pyfmt::str_of).collect())
            .unwrap_or_default(),
    };
    let text = |name: &str| request.arg(name).as_str().unwrap_or_default().to_string();
    let event = HarnessRefinementEvent {
        id: id.as_str().map_or_else(
            || format!("refine_{:04}", session.state.refinements.len() + 1),
            str::to_string,
        ),
        trigger: text("trigger"),
        changes,
        evidence: text("evidence"),
        outcome: text("outcome"),
        created_at: pyfmt::isoformat_now(),
        reason: None,
    };
    session.state.refinements.push(event.clone());
    session.dirty = true;
    Ok(event_json(&event))
}

/// `create_skill` / `update_skill`: the reference is checked first, naming
/// the entry as the caller wrote its id.
fn skill_write(
    session: &mut Session,
    request: &Request<'_>,
    factory: &FactoryChecks,
    create: bool,
) -> Result<HarnessEntry, StoreError> {
    let kind = Arg::new(json!("skill"), None);
    let fields = request.write_fields();
    if create {
        session.create(&kind, &fields, factory)
    } else {
        session.update(&kind, &fields, factory)
    }
}

/// `create_factory` / `update_factory` prechecks: the opt-in gate, one spec
/// form, and the spec's own validation, before the shared write path.
fn factory_precheck(request: &Request<'_>, factory: &FactoryChecks) -> Result<(), StoreError> {
    factory.require_enabled()?;
    let dag = request.arg("dag");
    let machine = request.arg("machine");
    let has_spec = !dag.is_none() || !machine.is_none();
    let create = request.arg("create").value.as_bool().unwrap_or(false);
    if !create && !has_spec {
        return Ok(());
    }
    if !dag.is_none() && !machine.is_none() {
        return Err(value_error(
            "pass either dag or machine, not both".to_string(),
        ));
    }
    factory.check_spec("")
}

fn factory_write(
    session: &mut Session,
    request: &Request<'_>,
    factory: &FactoryChecks,
) -> Result<HarnessEntry, StoreError> {
    let kind = Arg::new(json!("factory"), None);
    let dag = request.arg("dag");
    let machine = request.arg("machine");
    let arguments = if !machine.is_none() {
        Arg::new(json!({ "machine": machine.value }), None)
    } else if !dag.is_none() || request.arg("create").value.as_bool().unwrap_or(false) {
        Arg::new(json!({ "dag": dag.value }), None)
    } else {
        Arg::none()
    };
    let fields = WriteFields {
        arguments,
        ..request.write_fields()
    };
    if request.arg("create").value.as_bool().unwrap_or(false) {
        session.create(&kind, &fields, factory)
    } else {
        session.update(&kind, &fields, factory)
    }
}

fn dispatch(request_type: &str, request: &Request<'_>) -> Result<(Value, Outcome), StoreError> {
    let target = request.target()?;
    let factory = request.factory_checks(request_type)?;
    match request_type {
        "harness.load" => with_store(&target, false, |_| Ok(Value::Null)),
        "harness.save" => {
            let document = request.arg("document").value;
            with_store(&target, true, |session| {
                session.state = parse_harness_document(&document, target.scope);
                session.dirty = true;
                Ok(Value::Null)
            })
        }
        "harness.get" => with_store(&target, false, |session| {
            let (_, kind) = harness_kind(&request.arg("kind"))?;
            Ok(session
                .lookup(kind, &request.arg("id"))?
                .map_or(Value::Null, entry_json))
        }),
        "harness.list" => with_store(&target, false, |session| {
            let entries = session.list(&request.arg("kind"))?;
            Ok(Value::Array(entries.iter().map(entry_json).collect()))
        }),
        "harness.search" => with_store(&target, false, |session| search(session, request)),
        "harness.overview" => {
            let limit = slice_limit(&request.arg("max_entries_per_kind"))?;
            with_store(&target, false, |session| {
                Ok(Value::String(overview::render(
                    &session.state,
                    target.scope,
                    &target.file_label(),
                    limit,
                    |kind| session.listed(&[kind]),
                )))
            })
        }
        "harness.snapshot" => with_store(&target, false, |session| Ok(snapshot(session, &target))),
        "harness.upsert" => with_store(&target, true, |session| {
            session
                .upsert(&request.arg("kind"), &request.write_fields(), &factory)
                .map(|entry| entry_json(&entry))
        }),
        "harness.create" => with_store(&target, true, |session| {
            session
                .create(&request.arg("kind"), &request.write_fields(), &factory)
                .map(|entry| entry_json(&entry))
        }),
        "harness.update" => with_store(&target, true, |session| {
            session
                .update(&request.arg("kind"), &request.write_fields(), &factory)
                .map(|entry| entry_json(&entry))
        }),
        "harness.delete" => with_store(&target, true, |session| {
            let (_, kind) = harness_kind(&request.arg("kind"))?;
            let id = request.arg("id");
            if session.lookup(kind, &id)?.is_none() {
                return Ok(Value::Bool(false));
            }
            session.bucket(kind).remove(id.as_str().unwrap_or_default());
            session.dirty = true;
            Ok(Value::Bool(true))
        }),
        "harness.set_enabled" => {
            let enabled = request.arg("enabled");
            if !enabled.value.is_boolean() {
                return Err(type_error(format!(
                    "enabled must be bool, got {}",
                    enabled.type_name
                )));
            }
            with_store(&target, true, |session| set_enabled(session, request))
        }
        "harness.record_refinement" => {
            with_store(&target, true, |session| record_refinement(session, request))
        }
        "harness.create_skill" | "harness.update_skill" => {
            let create = request_type == "harness.create_skill";
            let reference = request.arg("reference");
            if create || !reference.is_none() {
                let name =
                    validate::describe_entry(&request.arg("describeId"), &request.arg("title"));
                validate::validate_python_skill_reference(&reference, &name)?;
            }
            with_store(&target, true, |session| {
                skill_write(session, request, &factory, create).map(|entry| entry_json(&entry))
            })
        }
        "harness.factory" => {
            factory_precheck(request, &factory)?;
            with_store(&target, true, |session| {
                factory_write(session, request, &factory).map(|entry| entry_json(&entry))
            })
        }
        other => Err(type_error(format!("unknown harness request {other}"))),
    }
}

/// Serve one kernel `harness.<op>` request. The reply is
/// `{"ok": true, "result", "state", "loadError"}` (the store's document
/// after the call, which the client's view mirrors) or
/// `{"ok": false, "error": {"type", "message"}}` with the Python exception
/// to raise.
#[must_use]
pub fn handle_request(data: &Value) -> Value {
    let request_type = data.get("type").and_then(Value::as_str).unwrap_or_default();
    match dispatch(request_type, &Request { data }) {
        Ok((result, outcome)) => json!({
            "ok": true,
            "result": result,
            "state": serde_json::to_value(&outcome.state).unwrap_or(Value::Null),
            "loadError": outcome.load_error,
        }),
        Err(error) => json!({
            "ok": false,
            "error": {"type": error.kind.python_name(), "message": error.message},
        }),
    }
}

/// Register the `harness.*` handlers. The store work is blocking file I/O
/// (and may wait for the store's lock), so it runs on the blocking pool.
pub fn register_host_handlers(handlers: &mut crate::kernel::shared::HostRequestHandlers) {
    // The request types the kernel's `rlm.harness` client sends.
    for request_type in [
        "harness.load",
        "harness.save",
        "harness.get",
        "harness.list",
        "harness.search",
        "harness.overview",
        "harness.snapshot",
        "harness.upsert",
        "harness.create",
        "harness.update",
        "harness.delete",
        "harness.set_enabled",
        "harness.record_refinement",
        "harness.create_skill",
        "harness.update_skill",
        "harness.factory",
    ] {
        handlers.register(
            request_type,
            crate::kernel::shared::host_handler(|payload| async move {
                tokio::task::spawn_blocking(move || handle_request(&payload.data))
                    .await
                    .map_err(anyhow::Error::from)
            }),
        );
    }
}

#[cfg(test)]
mod tests;
