//! The machine library: `MACHINE.md` files (import, export, share).
//!
//! A MACHINE.md is the shareable unit of the library, mirroring the
//! SKILL.md conventions: strict frontmatter (name, description, version,
//! author) followed by one fenced `machine-spec` block whose payload is a
//! JSON factory spec in the exact schema the write-time validator accepts
//! ([`machine_file`]). The library resolves from two levels, repo first,
//! user second; which directories those are is the kernel's knowledge (the
//! repo level is the runtime package's own `rlm/machines`, or
//! `PRIME_AGENT_MACHINES_DIR`), so every operation takes them.
//!
//! This is the one implementation behind the kernel's `rlm.factory`
//! library functions (thin clients sending `factory.library` requests,
//! [`handle_request`]) and the `prime-agent factory list | import |
//! export` commands ([`cli_dispatch`]). Every sentence, ordering and
//! failure is the original Python library's.

mod machine_file;
mod pyfs;
mod pyjson;

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::pyvalue::{decode_node_table, encode_node_table, py_str_repr, PyValue};
use super::spec::validate_factory_spec;
use machine_file::{dumps_pretty, py_str, single_line};
use machine_file::{
    machine_description_errors, machine_name_errors, parse_machine_file, render_machine_file,
    MachineFile, RenderFields, MACHINE_FILE_NAME,
};
pub use pyfs::{Fs, FsError};

/// An exception a library operation raises in the kernel.
#[derive(Debug, Clone, PartialEq)]
pub enum Raise {
    Value(String),
    Type(String),
    Attribute(String),
    Recursion(String),
    /// `OSError` or `UnicodeDecodeError` from a file operation.
    Fs(FsError),
    /// `MachineResolutionError(message, broken=...)`.
    Resolution {
        message: String,
        broken: bool,
    },
}

impl Raise {
    /// `str(exception)`.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Value(message)
            | Self::Type(message)
            | Self::Attribute(message)
            | Self::Recursion(message)
            | Self::Resolution { message, .. } => message.clone(),
            Self::Fs(error) => error.to_string(),
        }
    }

    /// Whether the original `except (ValueError, OSError)` arms caught it
    /// (`UnicodeDecodeError` and `MachineResolutionError` are `ValueError`s).
    fn is_value_or_os_error(&self) -> bool {
        matches!(self, Self::Value(_) | Self::Fs(_) | Self::Resolution { .. })
    }

    /// The reply envelope's `error`: the exception class and what the
    /// client needs to raise it.
    fn to_json(&self) -> Value {
        match self {
            Self::Value(message) => json!({"type": "ValueError", "message": message}),
            Self::Type(message) => json!({"type": "TypeError", "message": message}),
            Self::Attribute(message) => json!({"type": "AttributeError", "message": message}),
            Self::Recursion(message) => json!({"type": "RecursionError", "message": message}),
            Self::Resolution { message, broken } => {
                json!({"type": "MachineResolutionError", "message": message, "broken": broken})
            }
            Self::Fs(FsError::Os {
                errno,
                strerror,
                filename,
            }) => json!({
                "type": "OSError",
                "message": self.message(),
                "errno": errno,
                "strerror": strerror,
                "filename": filename,
            }),
            Self::Fs(FsError::Decode {
                start,
                end,
                reason,
                bytes,
            }) => json!({
                "type": "UnicodeDecodeError",
                "message": self.message(),
                "start": start,
                "end": end,
                "reason": reason,
                "bytes": bytes,
            }),
        }
    }
}

impl From<FsError> for Raise {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}

/// The library levels in resolution order: `[("repo", dir), ("user", dir)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryDirs(pub Vec<(String, PathBuf)>);

impl LibraryDirs {
    /// The personal level, where imports land.
    fn user_dir(&self) -> Option<&Path> {
        self.0
            .iter()
            .find(|(source, _)| source == "user")
            .map(|(_, dir)| dir.as_path())
    }
}

/// One library file's validity verdict, shared by scan and resolve: read,
/// decode, the strict parser, the write-time spec validator. `(machine,
/// "")` when valid, `(None, "<path>: <errors>")` when it fails to read or
/// parse, `(machine, "<path>: <spec errors>")` when only the spec fails.
fn read_library_machine(fs: &Fs, path: &Path) -> Result<(Option<MachineFile>, String), Raise> {
    let label = path.display();
    let text = match fs.read_text(path) {
        Ok(text) => text,
        Err(error @ FsError::Os { .. }) => {
            return Ok((None, format!("{label}: unreadable ({error})")))
        }
        Err(error @ FsError::Decode { .. }) => {
            return Ok((None, format!("{label}: not valid UTF-8 ({error})")))
        }
    };
    let (machine, errors) = parse_machine_file(&text, &label.to_string())?;
    let Some(machine) = machine.filter(|_| errors.is_empty()) else {
        return Ok((None, format!("{label}: {}", errors.join("; "))));
    };
    let spec_errors = validate_factory_spec(&machine.spec);
    if !spec_errors.is_empty() {
        return Ok((
            Some(machine),
            format!("{label}: {}", spec_errors.join("; ")),
        ));
    }
    Ok((Some(machine), String::new()))
}

/// `sorted(directory.glob("*/MACHINE.md"))`: every subdirectory (symlinks
/// followed, dotted names included) holding the file, by name; an
/// unreadable directory yields nothing.
fn library_files(fs: &Fs, directory: &Path) -> Vec<PathBuf> {
    let mut names: Vec<String> = fs
        .names(directory)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| fs.is_dir(&directory.join(name)))
        .filter(|name| fs.exists(&directory.join(name).join(MACHINE_FILE_NAME)))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| directory.join(name).join(MACHINE_FILE_NAME))
        .collect()
}

/// One listed machine (`list_machines` row).
fn listing(machine: &MachineFile, source: &str, path: &Path) -> Value {
    json!({
        "name": machine.name,
        "description": machine.description,
        "version": machine.version,
        "author": machine.author,
        "source": source,
        "path": path.display().to_string(),
    })
}

/// One pass over both levels: the listed machines (repo wins a name,
/// sorted by name) and the broken-file warnings (`<path>: <errors>`).
///
/// # Errors
///
/// Raises what parsing a file raised past its error list.
pub(crate) fn scan_machine_library(
    fs: &Fs,
    dirs: &LibraryDirs,
) -> Result<(Vec<Value>, Vec<String>), Raise> {
    let mut machines: Vec<(String, Value)> = Vec::new();
    let mut warnings = Vec::new();
    for (source, directory) in &dirs.0 {
        if !fs.is_dir(directory) {
            continue;
        }
        for path in library_files(fs, directory) {
            let (machine, error) = read_library_machine(fs, &path)?;
            let Some(machine) = machine.filter(|_| error.is_empty()) else {
                warnings.push(error);
                continue;
            };
            if machines.iter().any(|(name, _)| *name == machine.name) {
                continue;
            }
            machines.push((machine.name.clone(), listing(&machine, source, &path)));
        }
    }
    machines.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok((machines.into_iter().map(|(_, row)| row).collect(), warnings))
}

/// Resolve one machine by name: repo first, user second, with the
/// listing's validity verdict (an invalid file never claims its name; a
/// name only invalid files carry is broken, never missing).
///
/// # Errors
///
/// `ValueError` for an invalid name, `MachineResolutionError` for a broken
/// or missing machine.
pub(crate) fn resolve_machine(
    fs: &Fs,
    name: &PyValue,
    dirs: &LibraryDirs,
) -> Result<(MachineFile, PathBuf), Raise> {
    let errors = machine_name_errors(name);
    if !errors.is_empty() {
        return Err(Raise::Value(errors.join("; ")));
    }
    let name = py_str(name);
    let mut broken: Option<String> = None;
    for (_, directory) in &dirs.0 {
        let path = directory.join(&name).join(MACHINE_FILE_NAME);
        if !fs.is_file(&path) {
            continue;
        }
        let (machine, file_error) = read_library_machine(fs, &path)?;
        if !file_error.is_empty() {
            let carries_name = machine.as_ref().is_none_or(|machine| machine.name == name);
            if broken.is_none() && carries_name {
                broken = Some(file_error);
            }
            continue;
        }
        if let Some(machine) = machine.filter(|machine| machine.name == name) {
            return Ok((machine, path));
        }
    }
    let (listed, _) = scan_machine_library(fs, dirs)?;
    for entry in &listed {
        if entry["name"] == name.as_str() {
            let path = PathBuf::from(entry["path"].as_str().unwrap_or_default());
            let text = fs.read_text(&path)?;
            let (machine, parse_errors) = parse_machine_file(&text, &path.display().to_string())?;
            return match machine.filter(|_| parse_errors.is_empty()) {
                Some(machine) => Ok((machine, path)),
                None => Err(Raise::Resolution {
                    message: parse_errors.join("; "),
                    broken: true,
                }),
            };
        }
    }
    if let Some(message) = broken {
        return Err(Raise::Resolution {
            message,
            broken: true,
        });
    }
    let names: Vec<&str> = listed
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    let listing = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    };
    Err(Raise::Resolution {
        message: format!(
            "unknown machine {}: no MACHINE.md for it in the machine library (machines: {listing})",
            py_str_repr(&name)
        ),
        broken: false,
    })
}

/// The library gate: parse a MACHINE.md, validate its spec, persist it
/// byte-for-byte into `target_dir`.
///
/// # Errors
///
/// `ValueError` with every error sentence for a missing, malformed, or
/// spec-invalid file (nothing persists), or the file operation's failure.
pub(crate) fn import_machine(fs: &Fs, source: &Path, target_dir: &Path) -> Result<Value, Raise> {
    if !fs.is_file(source) {
        return Err(Raise::Value(format!(
            "machine file not found: {}",
            source.display()
        )));
    }
    let raw = fs.read_bytes(source)?;
    let text = pyfs::decode_utf8(raw.clone())?;
    let (machine, errors) = parse_machine_file(&text, &source.display().to_string())?;
    let Some(machine) = machine.filter(|_| errors.is_empty()) else {
        return Err(Raise::Value(errors.join("; ")));
    };
    let spec_errors = validate_factory_spec(&machine.spec);
    if !spec_errors.is_empty() {
        return Err(Raise::Value(spec_errors.join("; ")));
    }
    let destination = target_dir.join(&machine.name).join(MACHINE_FILE_NAME);
    if let Some(parent) = destination.parent() {
        fs.mkdir_parents(parent)?;
    }
    let created = !fs.exists(&destination);
    fs.write_bytes(&destination, &raw)?;
    Ok(json!({
        "name": machine.name,
        "path": destination.display().to_string(),
        "created": created,
    }))
}

/// `destination.parent.mkdir(parents=True, exist_ok=True)` after refusing
/// a directory target.
fn prepare_export_target(fs: &Fs, destination: &Path) -> Result<(), Raise> {
    if fs.is_dir(destination) {
        return Err(Raise::Value(format!(
            "export path {} is a directory (pass a file path)",
            destination.display()
        )));
    }
    match destination.parent() {
        Some(parent) => Ok(fs.mkdir_parents(parent)?),
        None => Ok(()),
    }
}

/// Write an export target, never silently clobbering one: the
/// no-overwrite path creates the file exclusively (a file created
/// concurrently, or a symlink planted at the target, refuses).
fn write_export_target(
    fs: &Fs,
    destination: &Path,
    text: &str,
    overwrite: bool,
) -> Result<(), Raise> {
    if overwrite {
        return Ok(fs.write_text(destination, text)?);
    }
    if fs.create_new_text(destination, text)? {
        return Ok(());
    }
    Err(Raise::Value(format!(
        "export path {} already exists (pass overwrite=True to replace it)",
        destination.display()
    )))
}

/// What `export_factory_spec` serializes.
pub(crate) struct SpecExport<'a> {
    pub spec: &'a PyValue,
    /// The fence text, or the exception the JSON encoder raised for it.
    pub spec_json: Result<&'a str, &'a Raise>,
    pub name: &'a PyValue,
    pub description: &'a PyValue,
    pub version: &'a PyValue,
    pub author: &'a PyValue,
    pub out_path: &'a Path,
    pub overwrite: bool,
}

/// Serialize any spec to MACHINE.md: validated first (an exported file
/// always re-imports), byte-pretty, and never over an existing file unless
/// `overwrite` says so.
///
/// # Errors
///
/// `ValueError` with every validation sentence, or the render or write
/// failure.
pub(crate) fn export_factory_spec(fs: &Fs, export: &SpecExport<'_>) -> Result<Value, Raise> {
    let mut errors = validate_factory_spec(export.spec);
    errors.extend(machine_name_errors(export.name));
    errors.extend(machine_description_errors(export.description));
    if !errors.is_empty() {
        return Err(Raise::Value(errors.join("; ")));
    }
    prepare_export_target(fs, export.out_path)?;
    let text = render_machine_file(&RenderFields {
        name: export.name,
        description: export.description,
        version: export.version,
        author: export.author,
        spec: export.spec,
        spec_json: export.spec_json,
    })?;
    write_export_target(fs, export.out_path, &text, export.overwrite)?;
    Ok(json!({
        "name": py_str(export.name),
        "path": export.out_path.display().to_string(),
        "source": "spec",
    }))
}

/// Export one library machine: its file copies verbatim (read as text, so
/// line endings normalize like the original's `read_text`/`write_text`).
///
/// # Errors
///
/// The resolution failure, or the target refusal or write failure.
pub(crate) fn export_library_machine(
    fs: &Fs,
    name: &PyValue,
    out_path: &Path,
    dirs: &LibraryDirs,
    overwrite: bool,
) -> Result<Value, Raise> {
    let (machine, path) = resolve_machine(fs, name, dirs)?;
    prepare_export_target(fs, out_path)?;
    write_export_target(fs, out_path, &fs.read_text(&path)?, overwrite)?;
    Ok(json!({
        "name": machine.name,
        "path": out_path.display().to_string(),
        "source": "library",
    }))
}

/// A host-held spec as the fence renders it.
fn host_spec_export(
    fs: &Fs,
    spec: &PyValue,
    name: &PyValue,
    description: &str,
    out_path: &Path,
    overwrite: bool,
) -> Result<Value, Raise> {
    let spec_json = dumps_pretty(spec);
    export_factory_spec(
        fs,
        &SpecExport {
            spec,
            spec_json: spec_json.as_deref(),
            name,
            description: &PyValue::Str(description.to_string()),
            version: &PyValue::Str("1".to_string()),
            author: &PyValue::Str(String::new()),
            out_path,
            overwrite,
        },
    )
}

/// What the kernel found for an `export_machine` target before the
/// library: a stored factory entry, else a live run's export view.
pub(crate) enum ExportSource {
    /// The entry's `arguments`, `content`, and `title`.
    Entry {
        arguments: PyValue,
        content: PyValue,
        title: PyValue,
    },
    /// The run's export view (`factory.machine`).
    Run(PyValue),
    Library,
}

/// `export_machine`: a stored factory entry first, a live run's canonical
/// machine second, the library last.
///
/// # Errors
///
/// `ValueError` for an entry or run that cannot become a machine, or the
/// export's own failure.
pub(crate) fn export_machine(
    fs: &Fs,
    target: &PyValue,
    source: &ExportSource,
    out_path: &Path,
    dirs: &LibraryDirs,
    overwrite: bool,
) -> Result<Value, Raise> {
    match source {
        ExportSource::Entry {
            arguments,
            content,
            title,
        } => {
            let machine = arguments.get("machine");
            let spec = if machine.is_none() {
                arguments.get("dag")
            } else {
                machine
            };
            if spec.is_none() {
                return Err(Raise::Value(format!(
                    "factory entry {} carries no machine or dag spec",
                    super::pyvalue::py_repr(target)
                )));
            }
            let mut errors = machine_name_errors(target);
            if !errors.is_empty() {
                errors.push(format!(
                    "the stored entry id {} cannot become a machine name",
                    super::pyvalue::py_repr(target)
                ));
                return Err(Raise::Value(errors.join("; ")));
            }
            let mut description = single_line(content);
            if description.is_empty() {
                description = single_line(title);
            }
            host_spec_export(fs, spec, target, &description, out_path, overwrite)
        }
        ExportSource::Run(run) if run.get("machine").truthy() => {
            let spec_id = run.get("spec_id");
            let mut errors = machine_name_errors(spec_id);
            if !errors.is_empty() {
                errors.push(format!(
                    "the run's spec id {} cannot become a machine name",
                    super::pyvalue::py_repr(spec_id)
                ));
                return Err(Raise::Value(errors.join("; ")));
            }
            let mut description = single_line(run.get("name"));
            if description.is_empty() {
                description = format!("factory run {}", py_str(run.get("run_id")));
            }
            host_spec_export(
                fs,
                run.get("machine"),
                spec_id,
                &description,
                out_path,
                overwrite,
            )
        }
        ExportSource::Run(_) | ExportSource::Library => {
            export_library_machine(fs, target, out_path, dirs, overwrite)
        }
    }
}

/// `Path(text).expanduser()` for a path the CLI payload carries.
fn expand_user(text: &str) -> PathBuf {
    let Some(rest) = text.strip_prefix('~') else {
        return PathBuf::from(text);
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return PathBuf::from(text);
    }
    match std::env::var_os("HOME") {
        Some(home) => {
            let home = home.to_string_lossy();
            let joined = format!("{}{rest}", home.trim_end_matches('/'));
            PathBuf::from(if joined.is_empty() {
                "/".to_string()
            } else {
                joined
            })
        }
        None => PathBuf::from(text),
    }
}

fn cli_failure(message: &str) -> Value {
    json!({"ok": false, "errors": [message]})
}

/// `{"ok": true, **result}` in the original key order.
fn cli_success(result: Value) -> Value {
    let mut reply = Map::new();
    reply.insert("ok".to_string(), Value::Bool(true));
    if let Value::Object(fields) = result {
        reply.extend(fields);
    }
    Value::Object(reply)
}

/// One required non-empty str field of a CLI payload.
fn cli_text<'a>(payload: &'a PyValue, key: &str) -> Option<&'a str> {
    payload.get(key).as_str().filter(|text| !text.is_empty())
}

/// The JSON facade of the `prime-agent factory` subcommands: one payload
/// in (an op and what the user typed), `{"ok": true, ...}` or `{"ok":
/// false, "errors": [...]}` out, every error as data.
///
/// # Errors
///
/// Raises only what the original let escape (a `RecursionError` from a
/// pathologically nested payload).
pub fn cli_dispatch(fs: &Fs, payload: &PyValue, dirs: &LibraryDirs) -> Result<Value, Raise> {
    if !payload.is_dict() {
        return Ok(cli_failure("factory cli payload must be a JSON object"));
    }
    let op = payload.get("op");
    let caught = |result: Result<Value, Raise>| match result {
        Ok(result) => Ok(cli_success(result)),
        Err(error) if error.is_value_or_os_error() => Ok(cli_failure(&error.message())),
        Err(error) => Err(error),
    };
    match op.as_str() {
        Some("list") => {
            let (machines, warnings) = scan_machine_library(fs, dirs)?;
            Ok(json!({"ok": true, "machines": machines, "warnings": warnings}))
        }
        Some("import") => {
            let Some(path) = cli_text(payload, "path") else {
                return Ok(cli_failure("factory import requires a `path` string"));
            };
            let target = dirs.user_dir().map(Path::to_path_buf).unwrap_or_default();
            caught(import_machine(fs, &expand_user(path), &target))
        }
        Some("export") => {
            let Some(name) = cli_text(payload, "name") else {
                return Ok(cli_failure("factory export requires a `name` string"));
            };
            let Some(out) = cli_text(payload, "out") else {
                return Ok(cli_failure("factory export requires an `out` string"));
            };
            caught(export_library_machine(
                fs,
                &PyValue::Str(name.to_string()),
                &expand_user(out),
                dirs,
                false,
            ))
        }
        _ => Ok(cli_failure(&format!(
            "unknown factory cli op {} (expected 'list', 'import' or 'export')",
            super::pyvalue::py_repr(op)
        ))),
    }
}

// ---------------------------------------------------------------------------
// The `factory.library` request: the kernel client's transport.
// ---------------------------------------------------------------------------

/// A request field the client always sends.
fn field<'a>(data: &'a Value, key: &str) -> anyhow::Result<&'a Value> {
    data.get(key)
        .ok_or_else(|| anyhow::anyhow!("factory.library {key} is required"))
}

/// A Python value the client shipped as a node table.
fn value_field(data: &Value, key: &str) -> anyhow::Result<PyValue> {
    Ok(decode_node_table(field(data, key)?)?)
}

fn text_field<'a>(data: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    field(data, key)?
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("factory.library {key} must be a string"))
}

fn path_field(data: &Value, key: &str) -> anyhow::Result<PathBuf> {
    text_field(data, key).map(PathBuf::from)
}

fn overwrite_field(data: &Value) -> bool {
    data.get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `dirs`: `[[source, dir], ...]` in resolution order.
fn dirs_field(data: &Value) -> anyhow::Result<LibraryDirs> {
    let levels = field(data, "dirs")?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("factory.library dirs must be a list"))?;
    let mut dirs = Vec::with_capacity(levels.len());
    for level in levels {
        let pair = level.as_array().map(Vec::as_slice);
        let Some([Value::String(source), Value::String(dir)]) = pair else {
            anyhow::bail!("factory.library dirs entries must be [source, dir] pairs");
        };
        dirs.push((source.clone(), PathBuf::from(dir)));
    }
    Ok(LibraryDirs(dirs))
}

/// A parsed machine as the client rebuilds its `MachineFile`.
fn machine_json(machine: &MachineFile) -> Value {
    json!({
        "name": machine.name,
        "description": machine.description,
        "version": machine.version,
        "author": machine.author,
        "spec": encode_node_table(&machine.spec),
    })
}

/// The fence text a client-held spec renders: the client's own
/// `json.dumps`, or the exception it raised.
fn spec_json_field(data: &Value) -> Result<String, Raise> {
    if let Some(text) = data.get("spec_json").and_then(Value::as_str) {
        return Ok(text.to_string());
    }
    let error = data.get("spec_json_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Err(
        match error
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
        {
            Some("ValueError") => Raise::Value(message),
            Some("RecursionError") => Raise::Recursion(message),
            _ => Raise::Type(message),
        },
    )
}

fn texts(errors: Vec<String>) -> Value {
    Value::from(errors)
}

/// One library operation (`op`) on the decoded request.
fn run_op(data: &Value) -> anyhow::Result<Result<Value, Raise>> {
    let op = text_field(data, "op")?;
    // The kernel's working directory: relative paths are the kernel's.
    let fs = data
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(Fs::here, |cwd| Fs::new(PathBuf::from(cwd)));
    Ok(match op {
        "name_errors" => Ok(texts(machine_name_errors(&value_field(data, "value")?))),
        "description_errors" => Ok(texts(machine_description_errors(&value_field(
            data, "value",
        )?))),
        "parse" => {
            let text = value_field(data, "text")?;
            let source = py_str(&value_field(data, "source")?);
            match text.as_str() {
                None => Err(Raise::Attribute(format!(
                    "{} object has no attribute 'lstrip'",
                    py_str_repr(pyjson::type_name(&text))
                ))),
                Some(text) => parse_machine_file(text, &source).map(|(machine, errors)| {
                    json!({
                        "machine": machine.as_ref().map(machine_json),
                        "errors": errors,
                    })
                }),
            }
        }
        "render" => {
            let spec_json = spec_json_field(data);
            render_machine_file(&RenderFields {
                name: &value_field(data, "name")?,
                description: &value_field(data, "description")?,
                version: &value_field(data, "version")?,
                author: &value_field(data, "author")?,
                spec: &value_field(data, "spec")?,
                spec_json: spec_json.as_deref(),
            })
            .map(Value::String)
        }
        "scan" => scan_machine_library(&fs, &dirs_field(data)?)
            .map(|(machines, warnings)| json!({"machines": machines, "warnings": warnings})),
        "resolve" => resolve_machine(&fs, &value_field(data, "name")?, &dirs_field(data)?).map(
            |(machine, path)| {
                json!({"machine": machine_json(&machine), "path": path.display().to_string()})
            },
        ),
        "import" => import_machine(
            &fs,
            &path_field(data, "path")?,
            &path_field(data, "target_dir")?,
        ),
        "export_spec" => {
            let spec_json = spec_json_field(data);
            export_factory_spec(
                &fs,
                &SpecExport {
                spec: &value_field(data, "spec")?,
                spec_json: spec_json.as_deref(),
                name: &value_field(data, "name")?,
                description: &value_field(data, "description")?,
                version: &value_field(data, "version")?,
                author: &value_field(data, "author")?,
                out_path: &path_field(data, "out_path")?,
                overwrite: overwrite_field(data),
            },
            )
        }
        "export_library" => export_library_machine(
            &fs,
            &value_field(data, "name")?,
            &path_field(data, "out_path")?,
            &dirs_field(data)?,
            overwrite_field(data),
        ),
        "export_machine" => {
            let source = match (data.get("entry"), data.get("run")) {
                (Some(entry), _) if !entry.is_null() => ExportSource::Entry {
                    arguments: value_field(entry, "arguments")?,
                    content: value_field(entry, "content")?,
                    title: value_field(entry, "title")?,
                },
                (_, Some(run)) if !run.is_null() => ExportSource::Run(PyValue::from_json(run)),
                _ => ExportSource::Library,
            };
            export_machine(
                &fs,
                &value_field(data, "target")?,
                &source,
                &path_field(data, "out_path")?,
                &dirs_field(data)?,
                overwrite_field(data),
            )
        }
        "cli" => cli_dispatch(&fs, &value_field(data, "payload")?, &dirs_field(data)?),
        other => anyhow::bail!("unknown factory.library op {other:?}"),
    })
}

/// Serve one `factory.library` request. The reply is `{"ok": true,
/// "result": ...}` or `{"ok": false, "error": {"type", "message", ...}}`
/// with the exception the client raises.
///
/// # Errors
///
/// Returns an error for a malformed request (unknown op, a missing field,
/// a bad node table).
pub fn handle_request(data: &Value) -> anyhow::Result<Value> {
    Ok(match run_op(data)? {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error.to_json()}),
    })
}

#[cfg(test)]
mod tests;
