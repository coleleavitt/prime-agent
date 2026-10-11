//! The MACHINE.md file format: strict frontmatter plus one fenced
//! `machine-spec` block whose payload is a JSON factory spec.
//!
//! This owns the FILE format only (frontmatter, fence, JSON payload) and
//! the deterministic contract prose a rendered file carries; the spec stays
//! in the validated schema ([`crate::factory::spec`]). Every rule and
//! sentence is the kernel's original `rlm.factory` parser's, byte for byte.

use std::fmt::Write as _;

use super::super::pyvalue::{PyValue, py_repr, py_str_repr, py_strip};
use super::Raise;
use super::pyjson::{self, LoadError, type_name};

pub const MACHINE_FILE_NAME: &str = "MACHINE.md";
pub const MACHINE_SPEC_FENCE: &str = "machine-spec";
pub const MACHINE_NAME_MAX_LENGTH: usize = 64;
pub const MACHINE_DESCRIPTION_MAX_LENGTH: usize = 1024;
pub const MACHINE_FRONTMATTER_FIELDS: [&str; 4] = ["name", "description", "version", "author"];

/// A parsed MACHINE.md.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineFile {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub spec: PyValue,
}

/// `str(value)` (an f-string field).
#[must_use]
pub fn py_str(value: &PyValue) -> String {
    match value {
        PyValue::Str(text) => text.clone(),
        other => py_repr(other),
    }
}

/// `str.isspace()` for one character.
fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

fn rstrip(text: &str) -> &str {
    text.trim_end_matches(is_space)
}

fn lstrip(text: &str) -> &str {
    text.trim_start_matches(is_space)
}

/// `" ".join(text.split())` for a str, `""` for anything else.
#[must_use]
pub fn single_line(value: &PyValue) -> String {
    value.as_str().map_or_else(String::new, |text| {
        text.split(is_space)
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// `re.fullmatch(r"[a-z0-9][a-z0-9-]*", name)`.
fn is_machine_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

/// `re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 ._/@+~-]*", value)`.
fn is_plain_frontmatter_value(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || " ._/@+~-".contains(ch))
}

/// Name rules mirrored from the skill library (`validate_name`).
#[must_use]
pub fn machine_name_errors(name: &PyValue) -> Vec<String> {
    let Some(name) = name.as_str().filter(|name| !name.is_empty()) else {
        return vec!["machine name must be a non-empty string".to_string()];
    };
    let mut errors = Vec::new();
    let length = name.chars().count();
    if length > MACHINE_NAME_MAX_LENGTH {
        errors.push(format!(
            "machine name exceeds {MACHINE_NAME_MAX_LENGTH} characters ({length})"
        ));
    }
    if !is_machine_name(name) {
        errors.push(
            "machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.ends_with('-') {
        errors.push("machine name must not end with a hyphen".to_string());
    }
    errors
}

/// Description rules mirrored from the skill library
/// (`validate_description`), plus the library's own single-line rule.
#[must_use]
pub fn machine_description_errors(description: &PyValue) -> Vec<String> {
    let Some(description) = description
        .as_str()
        .filter(|text| !py_strip(text).is_empty())
    else {
        return vec!["frontmatter description is required".to_string()];
    };
    let length = description.chars().count();
    if length > MACHINE_DESCRIPTION_MAX_LENGTH {
        return vec![format!(
            "frontmatter description exceeds {MACHINE_DESCRIPTION_MAX_LENGTH} characters ({length})"
        )];
    }
    if description.contains(['\n', '\r']) {
        return vec!["frontmatter description must be a single line".to_string()];
    }
    Vec::new()
}

/// Unquote one frontmatter value: plain, single-quoted, or double-quoted.
fn unquote_frontmatter_value(raw: &str, field: &str) -> Result<String, String> {
    let value = py_strip(raw);
    let quoted = |quote: char| {
        value.chars().count() >= 2 && value.starts_with(quote) && value.ends_with(quote)
    };
    if quoted('"') {
        return match pyjson::loads(value) {
            Ok(PyValue::Str(text)) => Ok(text),
            Ok(_) => Err(format!("frontmatter {field} must be a string scalar")),
            Err(error) => Err(format!(
                "frontmatter {field} has an invalid double-quoted value ({})",
                error.message()
            )),
        };
    }
    if quoted('\'') {
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    if value.contains(':') {
        return Err(format!(
            "frontmatter {field} is not a plain scalar (quote the value to include ':' characters)"
        ));
    }
    Ok(value.to_string())
}

/// The parsed frontmatter fields in file order, and the body after them.
type Frontmatter = (Vec<(String, String)>, String);

/// The strict frontmatter subset: `Ok((fields, body))` or the errors.
fn parse_frontmatter(text: &str, source: &str) -> Result<Frontmatter, Vec<String>> {
    let normalized = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if rstrip(lines[0]) != "---" {
        return Err(vec![format!(
            "{source}: MACHINE.md must start with a `---` frontmatter block"
        )]);
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut errors = Vec::new();
    let mut close = None;
    for (index, raw_line) in lines.iter().enumerate().skip(1) {
        let line = rstrip(raw_line);
        if line == "---" {
            close = Some(index);
            break;
        }
        let number = index + 1;
        if py_strip(line).is_empty() {
            errors.push(format!(
                "{source}: frontmatter line {number} is empty (one `key: value` line per field)"
            ));
            continue;
        }
        let Some((key, raw_value)) = line.split_once(':') else {
            errors.push(format!(
                "{source}: frontmatter line {number} must be `key: value`"
            ));
            continue;
        };
        let key = py_strip(key);
        if !MACHINE_FRONTMATTER_FIELDS.contains(&key) {
            errors.push(format!(
                "{source}: unknown frontmatter key {} (allowed: {})",
                py_str_repr(key),
                MACHINE_FRONTMATTER_FIELDS.join(", ")
            ));
            continue;
        }
        if fields.iter().any(|(known, _)| known == key) {
            errors.push(format!(
                "{source}: frontmatter field {} is declared more than once",
                py_str_repr(key)
            ));
            continue;
        }
        if py_strip(raw_value).is_empty() {
            errors.push(format!(
                "{source}: frontmatter field {} requires a value",
                py_str_repr(key)
            ));
            continue;
        }
        match unquote_frontmatter_value(raw_value, key) {
            Ok(value) => fields.push((key.to_string(), value)),
            Err(error) => errors.push(format!("{source}: {error}")),
        }
    }
    let Some(close) = close else {
        return Err(vec![format!(
            "{source}: frontmatter is not closed (end it with a `---` line)"
        )]);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok((fields, lines[close + 1..].join("\n")))
}

/// The single fenced `machine-spec` payload of the body. Other fenced
/// blocks are skipped as opaque units.
fn extract_machine_spec_block(body: &str, source: &str) -> Result<String, Vec<String>> {
    let lines: Vec<&str> = body.split('\n').collect();
    let mut contents = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = rstrip(lines[index]);
        if !lstrip(line).starts_with("```") {
            index += 1;
            continue;
        }
        let open_index = index;
        let info = py_strip(&py_strip(line)[3..]).to_string();
        index += 1;
        let mut content_lines = Vec::new();
        let mut closed = false;
        while index < lines.len() {
            if rstrip(lines[index]) == "```" {
                closed = true;
                index += 1;
                break;
            }
            content_lines.push(lines[index]);
            index += 1;
        }
        if info != MACHINE_SPEC_FENCE {
            if !closed {
                return Err(vec![format!(
                    "{source}: the ```{info} fence opened at line {} is never closed",
                    open_index + 1
                )]);
            }
            continue;
        }
        if !closed {
            return Err(vec![format!(
                "{source}: the ```{MACHINE_SPEC_FENCE} fence is never closed"
            )]);
        }
        contents.push(content_lines.join("\n"));
    }
    match contents.len() {
        0 => Err(vec![format!(
            "{source}: MACHINE.md requires exactly one fenced ```{MACHINE_SPEC_FENCE} block; found none"
        )]),
        1 => Ok(contents.remove(0)),
        count => Err(vec![format!(
            "{source}: MACHINE.md requires exactly one fenced ```{MACHINE_SPEC_FENCE} block; found {count}"
        )]),
    }
}

/// `parse_machine_file`: `Ok((machine, []))` or `Ok((None, errors))`.
///
/// # Errors
///
/// Raises what the original raised past its error list: a payload nested
/// past the interpreter's recursion limit (`RecursionError`).
pub fn parse_machine_file(
    text: &str,
    source: &str,
) -> Result<(Option<MachineFile>, Vec<String>), Raise> {
    let (fields, body) = match parse_frontmatter(text, source) {
        Ok(parsed) => parsed,
        Err(errors) => return Ok((None, errors)),
    };
    let payload = match extract_machine_spec_block(&body, source) {
        Ok(payload) => payload,
        Err(errors) => return Ok((None, errors)),
    };
    let spec = match pyjson::loads(&payload) {
        Ok(spec) => spec,
        Err(LoadError::Recursion(message)) => return Err(Raise::Recursion(message)),
        Err(error) => {
            return Ok((
                None,
                vec![format!(
                    "{source}: the ```{MACHINE_SPEC_FENCE} block must contain a JSON object ({})",
                    error.message()
                )],
            ));
        }
    };
    if !spec.is_dict() {
        return Ok((
            None,
            vec![format!(
                "{source}: the ```{MACHINE_SPEC_FENCE} block must contain a JSON object, got a {}",
                type_name(&spec)
            )],
        ));
    }
    let field = |key: &str| {
        fields
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, value)| value.clone())
    };
    let name = field("name").unwrap_or_default();
    let description = field("description");
    let mut errors = machine_name_errors(&PyValue::Str(name.clone()));
    errors.extend(machine_description_errors(
        &description.clone().map_or(PyValue::None, PyValue::Str),
    ));
    if !errors.is_empty() {
        return Ok((None, errors));
    }
    Ok((
        Some(MachineFile {
            name,
            description: description.unwrap_or_default(),
            version: field("version").unwrap_or_default(),
            author: field("author").unwrap_or_default(),
            spec,
        }),
        Vec::new(),
    ))
}

/// One frontmatter value as the renderer writes it: plain when YAML-safe,
/// else double-quoted (`json.dumps(value, ensure_ascii=False)`).
fn render_frontmatter_value(value: &PyValue) -> Result<String, Raise> {
    let PyValue::Str(text) = value else {
        return Err(Raise::Type(format!(
            "expected string or bytes-like object, got {}",
            py_str_repr(type_name(value))
        )));
    };
    if is_plain_frontmatter_value(text) {
        return Ok(text.clone());
    }
    Ok(unicode_json_string(text))
}

/// `value[:60]` and its f-string spelling, for the inline-subagent prompt.
fn slice_prefix(value: &PyValue) -> Result<String, Raise> {
    match value {
        PyValue::Str(text) => Ok(text.chars().take(60).collect()),
        PyValue::List(items) => Ok(py_repr(&PyValue::List(
            items.iter().take(60).cloned().collect(),
        ))),
        PyValue::Dict(_) => Err(Raise::Type("unhashable type: 'slice'".to_string())),
        other => Err(Raise::Type(format!(
            "{} object is not subscriptable",
            py_str_repr(type_name(other))
        ))),
    }
}

/// `for item in value or []`: the items a contract loop visits (a dict
/// iterates its keys, a str its characters; neither is a dict, so the
/// loop skips them all).
fn iter_items(value: &PyValue) -> Result<&[PyValue], Raise> {
    match value {
        PyValue::List(items) => Ok(items),
        other if !other.truthy() => Ok(&[]),
        PyValue::Dict(_) | PyValue::Str(_) => Ok(&[]),
        other => Err(Raise::Type(format!(
            "{} object is not iterable",
            py_str_repr(type_name(other))
        ))),
    }
}

/// Deterministic contract prose generated from the spec (both forms).
fn machine_contract_lines(spec: &PyValue) -> Result<Vec<String>, Raise> {
    if !spec.is_dict() {
        return Err(Raise::Attribute(format!(
            "{} object has no attribute 'get'",
            py_str_repr(type_name(spec))
        )));
    }
    let mut lines = Vec::new();
    let run = spec.get("run");
    if run.is_dict() {
        let mut parts = vec![
            format!("failure_policy={}", py_str(run.get("failure_policy"))),
            format!("max_parallel={}", py_str(run.get("max_parallel"))),
        ];
        for key in ["budget_ms", "max_transitions"] {
            if let Some(value) = run.entry(key) {
                parts.push(format!("{key}={}", py_str(value)));
            }
        }
        lines.push(format!("Run: {}", parts.join(", ")));
    }
    let states = if spec.get("states").is_list() {
        spec.get("states")
    } else {
        spec.get("nodes")
    };
    let Some(states) = states.as_list() else {
        return Ok(lines);
    };
    lines.push(String::new());
    lines.push("States:".to_string());
    for state in states.iter().filter(|state| state.is_dict()) {
        let mut flags = Vec::new();
        if state.get("entry").truthy() {
            flags.push("entry".to_string());
        }
        for key in [
            "lifecycle",
            "max_entries",
            "retries",
            "failure_policy",
            "budget_ms",
        ] {
            if let Some(value) = state.entry(key) {
                flags.push(format!("{key}={}", py_str(value)));
            }
        }
        let mut label = format!("- {}", py_str(state.get("id")));
        if !flags.is_empty() {
            let _ = write!(label, " ({})", flags.join(", "));
        }
        lines.push(label);
        let subagent = state.get("subagent");
        match subagent {
            PyValue::Dict(_) => {
                let name = subagent.get("name");
                let settings = if name.truthy() {
                    py_str(name)
                } else {
                    match subagent.entry("prompt") {
                        Some(prompt) => slice_prefix(prompt)?,
                        None => String::new(),
                    }
                };
                lines.push(format!("  subagent: inline ({settings})"));
            }
            PyValue::Str(reference) => lines.push(format!("  subagent: {reference}")),
            _ => {}
        }
        for input in iter_items(state.get("inputs"))?
            .iter()
            .filter(|item| item.is_dict())
        {
            let optional = if input.get("optional").truthy() {
                " [optional]"
            } else {
                ""
            };
            lines.push(format!(
                "  input: {} ({}) <- {}{optional}",
                py_str(input.get("name")),
                py_str(input.get("type")),
                py_str(input.get("from"))
            ));
        }
        for output in iter_items(state.get("outputs"))?
            .iter()
            .filter(|item| item.is_dict())
        {
            lines.push(format!(
                "  output: {} ({})",
                py_str(output.get("name")),
                py_str(output.get("type"))
            ));
        }
        let foreach = state.get("foreach");
        if foreach.is_dict() {
            lines.push(format!(
                "  foreach: over {}, max {}",
                py_str(foreach.get("over")),
                py_str(foreach.get("max"))
            ));
        }
    }
    if let Some(transitions) = spec.get("transitions").as_list() {
        lines.push(String::new());
        lines.push("Transitions:".to_string());
        for transition in transitions.iter().filter(|item| item.is_dict()) {
            let raw_from = transition.get("from");
            let source_text = match raw_from {
                PyValue::List(items) => format!(
                    "[{}]",
                    items.iter().map(py_str).collect::<Vec<_>>().join(", ")
                ),
                other => py_str(other),
            };
            let guard = transition.get("when");
            let mut guard_text = String::new();
            if guard.is_dict() {
                let port = guard.get("output");
                let path = guard.get("path");
                let target = if path.truthy() {
                    format!("{}.{}", py_str(port), py_str(path))
                } else {
                    py_str(port)
                };
                let value = pyjson::dumps(guard.get("value")).map_err(Raise::Type)?;
                guard_text = format!(" when {target} {} {value}", py_str(guard.get("op")));
            }
            lines.push(format!(
                "- {source_text} -> {}{guard_text}",
                py_str(transition.get("to"))
            ));
        }
    }
    Ok(lines)
}

/// The fields `render_machine_file` reads from a `MachineFile`.
pub struct RenderFields<'a> {
    pub name: &'a PyValue,
    pub description: &'a PyValue,
    pub version: &'a PyValue,
    pub author: &'a PyValue,
    pub spec: &'a PyValue,
    /// `json.dumps(spec, indent=2, ensure_ascii=False)`, or the exception
    /// that dump raised (raised after the prose, where the original did).
    pub spec_json: Result<&'a str, &'a Raise>,
}

/// `render_machine_file`: canonical MACHINE.md text, byte-stable.
///
/// # Errors
///
/// Raises what the original raised for a machine it cannot render (a
/// non-str frontmatter field, a spec the prose or the JSON encoder rejects).
pub fn render_machine_file(machine: &RenderFields<'_>) -> Result<String, Raise> {
    let frontmatter = [
        "---".to_string(),
        format!("name: {}", render_frontmatter_value(machine.name)?),
        format!(
            "description: {}",
            render_frontmatter_value(machine.description)?
        ),
        format!("version: {}", render_frontmatter_value(machine.version)?),
        format!("author: {}", render_frontmatter_value(machine.author)?),
        "---".to_string(),
    ];
    let mut sections = vec![
        frontmatter.join("\n"),
        String::new(),
        format!("# {}", py_str(machine.name)),
        String::new(),
        "## Machine contract".to_string(),
        String::new(),
    ];
    sections.extend(machine_contract_lines(machine.spec)?);
    let spec_json = machine.spec_json.map_err(Clone::clone)?;
    sections.push(String::new());
    sections.push(format!("```{MACHINE_SPEC_FENCE}"));
    sections.push(spec_json.to_string());
    sections.push("```".to_string());
    Ok(sections.join("\n") + "\n")
}

/// `json.dumps(value, indent=2, ensure_ascii=False)` for a value the host
/// itself holds (a stored entry's or a run's spec: JSON data).
///
/// # Errors
///
/// Returns the `TypeError` the encoder raises for a value JSON cannot
/// spell.
pub fn dumps_pretty(value: &PyValue) -> Result<String, Raise> {
    let mut out = String::new();
    write_pretty(value, 0, &mut out)?;
    Ok(out)
}

fn write_pretty(value: &PyValue, level: usize, out: &mut String) -> Result<(), Raise> {
    let newline = |out: &mut String, level: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(level));
    };
    match value {
        PyValue::List(items) if !items.is_empty() => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_pretty(item, level + 1, out)?;
            }
            newline(out, level);
            out.push(']');
        }
        PyValue::Dict(pairs) if !pairs.is_empty() => {
            out.push('{');
            for (position, (key, item)) in pairs.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                let key = match key {
                    PyValue::Str(text) => text.clone(),
                    PyValue::List(_) | PyValue::Dict(_) | PyValue::Opaque { .. } => {
                        return Err(Raise::Type(format!(
                            "keys must be str, int, float, bool or None, not {}",
                            type_name(key)
                        )));
                    }
                    scalar => pyjson::dumps(scalar).map_err(Raise::Type)?,
                };
                out.push_str(&unicode_json_string(&key));
                out.push_str(": ");
                write_pretty(item, level + 1, out)?;
            }
            newline(out, level);
            out.push('}');
        }
        PyValue::Str(text) => out.push_str(&unicode_json_string(text)),
        scalar => out.push_str(&pyjson::dumps(scalar).map_err(Raise::Type)?),
    }
    Ok(())
}

/// `json.dumps(text, ensure_ascii=False)` for one str.
fn unicode_json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
