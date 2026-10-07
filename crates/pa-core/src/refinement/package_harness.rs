//! Read-only continual harness overlays from installed packages (upstream
//! #2298, TS `refinement/package-harness.ts`).
//!
//! A package declares `harness` in its `pi` manifest (or ships the
//! conventional `harness/` directory, or a settings package filter selects
//! it); each `harness/<prompt|memory|skill|subagent>/<id>.json` file mounts
//! as one entry of a package overlay state. The overlay never enters an
//! editable store (`harness_state.json`, `rlm.harness` CRUD, refinement
//! history), so nothing can mutate it: [`overlay_package_harness`] places it
//! BELOW every editable entry of the same `(kind, id)`, the digest labels it
//! `package:<id>` with sanitized provenance, and refine refuses update and
//! delete edits against it (a same-id create stays an editable override).
//!
//! Provenance (`origin`, sanitized `source`, `scope`, package-relative
//! `file`, optional `revision`, `readOnly`) rides the entry's `provenance`
//! key — the same JSON shape TS writes on the entry — and never renders a
//! local filesystem path.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{empty_harness_state, HarnessEntry, HarnessState, RefinementKind};
use crate::packages::resolve::{MetadataSource, ResolvedResource};
use crate::packages::SourceScope;
use crate::skills::diagnostics::{ResourceCollision, ResourceDiagnostic};

/// Package entries are pure content overlays: a fixed timestamp keeps the
/// rendered digest stable across reloads of an unchanged package.
const PACKAGE_HARNESS_TIMESTAMP: &str = "1970-01-01T00:00:00.000Z";
const REVISION_LENGTH: usize = 12;
const RESERVED_HARNESS_IDS: &[&str] = &["prototype"];
/// The JS `Object.prototype` member names TS rejects as ids.
const OBJECT_PROTOTYPE_KEYS: &[&str] = &[
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];
/// The entry key provenance rides under (TS `HarnessEntry.provenance`).
pub const PACKAGE_PROVENANCE_KEY: &str = "provenance";

/// Where a read-only entry came from (TS `PackageHarnessProvenance`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageHarnessProvenance {
    /// Always `"package"`.
    pub origin: String,
    /// The sanitized configured source (never a local path).
    pub source: String,
    /// `user`, `project`, or `temporary`.
    pub scope: String,
    /// The entry's path relative to the package root.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub read_only: bool,
}

/// A mounted overlay plus its load diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageHarnessLoad {
    pub state: HarnessState,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

impl Default for PackageHarnessLoad {
    fn default() -> Self {
        Self {
            state: empty_harness_state(),
            diagnostics: Vec::new(),
        }
    }
}

/// An entry's package provenance, when it is a package overlay entry.
#[must_use]
pub fn package_provenance(entry: &HarnessEntry) -> Option<PackageHarnessProvenance> {
    let provenance: PackageHarnessProvenance =
        serde_json::from_value(entry.extensions.get(PACKAGE_PROVENANCE_KEY)?.clone()).ok()?;
    (provenance.origin == "package").then_some(provenance)
}

/// Whether an entry is a read-only package overlay entry.
#[must_use]
pub fn is_package_entry(entry: &HarnessEntry) -> bool {
    package_provenance(entry).is_some()
}

/// TS `mergeHarnessStates`'s package step: overlay entries rank below every
/// editable entry — an editable local/global entry with the same
/// `(kind, id)` shadows the package entry.
pub fn overlay_package_harness(merged: &mut HarnessState, package: &HarnessState) {
    merged.schema = merged.schema.max(package.schema);
    for (kind, entries) in &package.entries {
        let target = merged.entries.entry(*kind).or_default();
        let editable_ids: std::collections::HashSet<String> =
            target.values().map(|entry| entry.id.clone()).collect();
        for (id, entry) in entries {
            if !editable_ids.contains(&entry.id) {
                target.insert(id.clone(), entry.clone());
            }
        }
    }
}

fn kind_from_dir(name: &str) -> Option<RefinementKind> {
    match name {
        "prompt" => Some(RefinementKind::Prompt),
        "memory" => Some(RefinementKind::Memory),
        "skill" => Some(RefinementKind::Skill),
        "subagent" => Some(RefinementKind::Subagent),
        _ => None,
    }
}

fn kind_name(kind: RefinementKind) -> &'static str {
    match kind {
        RefinementKind::Prompt => "prompt",
        RefinementKind::Memory => "memory",
        RefinementKind::Skill => "skill",
        RefinementKind::Subagent => "subagent",
        RefinementKind::Factory => "factory",
    }
}

fn harness_id_error(id: &str) -> Option<String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        return Some("package harness id must match [A-Za-z0-9_.-]+".to_string());
    }
    if OBJECT_PROTOTYPE_KEYS.contains(&id) || RESERVED_HARNESS_IDS.contains(&id) {
        return Some(format!("package harness id {id} is reserved"));
    }
    None
}

fn scope_rank(scope: SourceScope) -> u8 {
    match scope {
        SourceScope::Project => 0,
        SourceScope::User => 1,
        SourceScope::Temporary => 2,
    }
}

fn scope_name(scope: SourceScope) -> &'static str {
    match scope {
        SourceScope::Project => "project",
        SourceScope::User => "user",
        SourceScope::Temporary => "temporary",
    }
}

/// TS `parsePackageHarnessPath`: `harness/<kind>/<id>.json` inside the root.
fn parse_harness_path(resource: &ResolvedResource) -> Result<(RefinementKind, String), String> {
    let base_dir = resource
        .metadata
        .base_dir
        .as_deref()
        .ok_or("package harness resource is missing its package root")?;
    let relative = resource
        .path
        .strip_prefix(base_dir)
        .map_err(|_| "package harness file must be inside its package root".to_string())?;
    let segments: Vec<&str> = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().unwrap_or(""),
            _ => "..",
        })
        .collect();
    if segments.contains(&"..") || segments.is_empty() {
        return Err("package harness file must be inside its package root".to_string());
    }
    if segments.len() != 3 || segments[0] != "harness" {
        return Err("package harness file must use harness/<kind>/<id>.json".to_string());
    }
    let kind_dir = segments[1];
    let Some(kind) = kind_from_dir(kind_dir) else {
        let shown = if kind_dir.is_empty() {
            "<empty>"
        } else {
            kind_dir
        };
        return Err(format!("package harness path has unsupported kind {shown}"));
    };
    let Some(id) = segments[2].strip_suffix(".json") else {
        return Err("package harness file must use a .json extension".to_string());
    };
    if id.is_empty() {
        return Err("package harness file name must contain a nonempty id".to_string());
    }
    if let Some(error) = harness_id_error(id) {
        return Err(error);
    }
    Ok((kind, id.to_string()))
}

fn object(value: Option<&Value>, field: &str) -> Result<serde_json::Map<String, Value>, String> {
    match value {
        None => Ok(serde_json::Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(format!(
            "package harness entry {field} must be an object when provided"
        )),
    }
}

fn nonempty(map: &serde_json::Map<String, Value>, key: &str) -> bool {
    map.get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

/// TS `validatePackageHarnessEntry`.
fn validate_entry(
    value: &Value,
    kind: RefinementKind,
    id: &str,
    source: &str,
) -> Result<HarnessEntry, String> {
    let Some(record) = value.as_object() else {
        return Err("package harness file must contain a JSON object".to_string());
    };
    for field in ["id", "kind", "title", "content"] {
        if !nonempty(record, field) {
            return Err(format!(
                "package harness entry {field} must be a nonempty string"
            ));
        }
    }
    if record.get("kind").and_then(Value::as_str) != Some(kind_name(kind)) {
        return Err(format!(
            "package harness entry kind must match path kind {}",
            kind_name(kind)
        ));
    }
    if record.get("id").and_then(Value::as_str) != Some(id) {
        return Err(format!("package harness entry id must match file id {id}"));
    }
    if let Some(scope) = record.get("scope") {
        if scope != "local" && scope != "global" {
            return Err(
                "package harness entry scope must be local or global when provided".to_string(),
            );
        }
    }
    let path = match record.get("path") {
        None => if kind == RefinementKind::Prompt {
            "policy"
        } else {
            "general"
        }
        .to_string(),
        Some(Value::String(path)) if !path.trim().is_empty() => path.clone(),
        Some(_) => {
            return Err(
                "package harness entry path must be a nonempty string when provided".to_string(),
            )
        }
    };
    let reference = object(record.get("reference"), "reference")?;
    let arguments = object(record.get("arguments"), "arguments")?;
    let metadata = object(record.get("metadata"), "metadata")?;
    let version = match record.get("version") {
        None => 1,
        Some(version) => match version.as_u64().filter(|version| *version >= 1) {
            Some(version) => version,
            None => {
                return Err(
                    "package harness entry version must be a positive integer when provided"
                        .to_string(),
                )
            }
        },
    };
    if kind == RefinementKind::Skill {
        if reference.get("type").and_then(Value::as_str) != Some("python") {
            return Err("package harness skill reference.type must be python".to_string());
        }
        if !(nonempty(&reference, "import") || nonempty(&reference, "python_import")) {
            return Err("package harness skill requires a python import".to_string());
        }
        if !(nonempty(&reference, "callable") || nonempty(&reference, "call_pattern")) {
            return Err("package harness skill requires a callable or call_pattern".to_string());
        }
    }
    Ok(HarnessEntry {
        id: id.to_string(),
        kind,
        title: record["title"].as_str().unwrap_or_default().to_string(),
        content: record["content"].as_str().unwrap_or_default().to_string(),
        path,
        // Package overlays are scope-less: an exported scope is accepted,
        // never carried.
        scope: None,
        reference,
        arguments,
        metadata,
        source: source.to_string(),
        created_at: PACKAGE_HARNESS_TIMESTAMP.to_string(),
        updated_at: PACKAGE_HARNESS_TIMESTAMP.to_string(),
        version,
        extensions: serde_json::Map::new(),
    })
}

fn credential_key(key: &str) -> bool {
    static PATTERN: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            fancy_regex::Regex::new(
                r"(?i)(?:^|[-_])(token|secret|password|passwd|credential|authorization|auth|api[-_]?key|access[-_]?key|signature|sig)(?:$|[-_])",
            )
            .expect("credential key pattern")
        })
        .is_match(key)
        .unwrap_or(false)
}

fn credential_value(value: &str) -> bool {
    static PATTERN: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            fancy_regex::Regex::new(
                r"(?i)^(?:bearer\s+|basic\s+|gh[pousr]_|github_pat_|glpat-|sk[-_]|xox[baprs]-)",
            )
            .expect("credential value pattern")
        })
        .is_match(value)
        .unwrap_or(false)
}

/// TS `redactCredentialParameters`: `key=value` pairs after `?`/`&`/`#`
/// whose key or value looks like a credential become `key=[redacted]`; a
/// credential-looking fragment becomes `#[redacted]`.
fn redact_credential_parameters(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find(['?', '&', '#']) {
        out.push_str(&rest[..=at]);
        rest = &rest[at + 1..];
        let end = rest.find(['&', '#']).unwrap_or(rest.len());
        let pair = &rest[..end];
        match pair.split_once('=') {
            Some((key, value))
                if !key.is_empty() && (credential_key(key) || credential_value(value)) =>
            {
                out.push_str(key);
                out.push_str("=[redacted]");
            }
            _ => out.push_str(pair),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    if let Some(fragment_at) = out.find('#') {
        let fragment = &out[fragment_at + 1..];
        let decoded = percent_decode(fragment);
        if credential_value(&decoded) {
            return format!("{}#[redacted]", &out[..fragment_at]);
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map_or_else(|| text.to_string(), |(_, value)| value.into_owned())
}

/// TS `redactScpLikeCredentials`: `user:pass@host:path` loses its userinfo.
/// Divergence: an `npm:@scope/name` spec is no scp form (TS reads its
/// `npm:` as `user:` userinfo and renders `scope/name`), so it stays.
fn redact_scp_like_credentials(source: &str) -> String {
    if source.starts_with("npm:") {
        return source.to_string();
    }
    let query_at = source.find(['?', '#']).unwrap_or(source.len());
    let (identity, suffix) = source.split_at(query_at);
    let prefix = if identity.starts_with("git:") {
        "git:"
    } else {
        ""
    };
    let scp = &identity[prefix.len()..];
    let Some(at) = scp.find('@').filter(|at| *at > 0) else {
        return source.to_string();
    };
    let (user_info, host_path) = (&scp[..at], &scp[at + 1..]);
    let looks_scp = host_path.contains(':') || host_path.contains('/');
    let has_credentials =
        user_info.contains(':') || credential_key(user_info) || credential_value(user_info);
    if !looks_scp || user_info == "git" || !has_credentials {
        return source.to_string();
    }
    format!("{prefix}{host_path}{suffix}")
}

/// TS `sanitizePackageSource`: URL userinfo and credential query parameters
/// stripped, scp-like `user:pass@` segments dropped.
///
/// # Panics
///
/// Never in practice: the URL-start pattern is a constant that compiles.
#[must_use]
pub fn sanitize_package_source(source: &str) -> String {
    static URL_START: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
    let url_start = URL_START
        .get_or_init(|| fancy_regex::Regex::new(r"[A-Za-z][A-Za-z0-9+.-]*://").expect("url"))
        .find(source)
        .ok()
        .flatten()
        .map(|found| found.start());
    let Some(url_at) = url_start else {
        return redact_credential_parameters(&redact_scp_like_credentials(source));
    };
    let (prefix, rest) = source.split_at(url_at);
    if let Ok(mut url) = url::Url::parse(rest) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, value)| !(credential_key(key) || credential_value(value)))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        if url.query().is_some() {
            if kept.is_empty() {
                url.set_query(None);
            } else {
                url.query_pairs_mut().clear().extend_pairs(kept);
            }
        }
        redact_credential_parameters(&format!("{prefix}{url}"))
    } else {
        let stripped = match rest.find("//").map(|at| at + 2) {
            Some(start) => match rest[start..].find('@') {
                Some(at)
                    if !rest[start..start + at]
                        .chars()
                        .any(|ch| ch == '/' || ch.is_whitespace()) =>
                {
                    format!("{}{}", &rest[..start], &rest[start + at + 1..])
                }
                _ => rest.to_string(),
            },
            None => rest.to_string(),
        };
        redact_credential_parameters(&format!("{prefix}{stripped}"))
    }
}

/// TS `describePackageSource`: a local package renders as
/// `local:<dirname>`, never its filesystem path.
fn describe_package_source(source: &str, base_dir: &Path) -> String {
    let sanitized = sanitize_package_source(source);
    if !crate::packages::is_local_path(&sanitized) {
        return sanitized;
    }
    format!(
        "local:{}",
        base_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    )
}

fn hex_sha(text: &str) -> bool {
    (7..=40).contains(&text.len())
        && text
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
}

fn short(sha: &str) -> String {
    sha.chars().take(REVISION_LENGTH).collect()
}

/// TS `readGitRevision`: HEAD (through a `.git` file indirection), its
/// loose ref, or the packed refs — no subprocess.
fn read_git_revision(base_dir: &Path) -> Option<String> {
    let mut git_dir = base_dir.join(".git");
    if !git_dir.exists() {
        return None;
    }
    if !git_dir.is_dir() {
        let indirection = std::fs::read_to_string(&git_dir).ok()?;
        git_dir = Path::new(indirection.trim().strip_prefix("gitdir:")?.trim()).to_path_buf();
    }
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return hex_sha(head).then(|| short(head));
    };
    if let Ok(sha) = std::fs::read_to_string(git_dir.join(reference)) {
        let sha = sha.trim();
        return (!sha.is_empty()).then(|| short(sha));
    }
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    let line = packed
        .lines()
        .find(|line| line.ends_with(&format!(" {reference}")))?;
    let sha = line.split(' ').next()?;
    hex_sha(sha).then(|| short(sha))
}

/// TS `readPackageRevision`: the git revision, else `v<package.json version>`.
fn read_package_revision(base_dir: &Path) -> Option<String> {
    read_git_revision(base_dir).or_else(|| {
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(base_dir.join("package.json")).ok()?)
                .ok()?;
        let version = manifest.get("version")?.as_str()?.trim();
        (!version.is_empty()).then(|| format!("v{version}"))
    })
}

fn provenance(resource: &ResolvedResource, base_dir: &Path) -> PackageHarnessProvenance {
    let source = match &resource.metadata.source {
        MetadataSource::Package(source) => source.clone(),
        other => other.source_label(),
    };
    PackageHarnessProvenance {
        origin: "package".to_string(),
        source: describe_package_source(&source, base_dir),
        scope: scope_name(resource.metadata.scope).to_string(),
        file: resource
            .path
            .strip_prefix(base_dir)
            .map(|relative| {
                relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default(),
        revision: read_package_revision(base_dir),
        read_only: true,
    }
}

fn warning(message: String, path: &Path) -> ResourceDiagnostic {
    ResourceDiagnostic::Warning {
        message,
        path: Some(path.display().to_string()),
    }
}

/// TS `loadPackageHarness`: mount the enabled package harness resources.
/// `(kind, id)` stays unique across packages — project packages beat user
/// packages (then temporary), earlier beats later; a later duplicate is a
/// `collision` diagnostic. Invalid files are warnings and never break
/// loading.
#[must_use]
pub fn load_package_harness(resources: &[ResolvedResource]) -> PackageHarnessLoad {
    let mut load = PackageHarnessLoad::default();
    let mut ordered: Vec<(usize, &ResolvedResource)> = resources
        .iter()
        .enumerate()
        .filter(|(_, resource)| resource.enabled)
        .collect();
    ordered.sort_by_key(|(index, resource)| (scope_rank(resource.metadata.scope), *index));
    for (_, resource) in ordered {
        let (kind, id) = match parse_harness_path(resource) {
            Ok(parsed) => parsed,
            Err(message) => {
                load.diagnostics.push(warning(message, &resource.path));
                continue;
            }
        };
        let parsed: Value = match std::fs::read_to_string(&resource.path)
            .map_err(|error| error.to_string())
            .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
        {
            Ok(parsed) => parsed,
            Err(error) => {
                load.diagnostics.push(warning(
                    format!("failed to read package harness entry: {error}"),
                    &resource.path,
                ));
                continue;
            }
        };
        let base_dir = resource.metadata.base_dir.clone().unwrap_or_default();
        let provenance = provenance(resource, &base_dir);
        let mut entry = match validate_entry(&parsed, kind, &id, &provenance.source) {
            Ok(entry) => entry,
            Err(message) => {
                load.diagnostics.push(warning(message, &resource.path));
                continue;
            }
        };
        let entries = load.state.entries.entry(kind).or_default();
        if let Some(existing) = entries.get(&id) {
            let winner = package_provenance(existing);
            let winner_source = winner
                .as_ref()
                .map_or_else(|| existing.source.clone(), |winner| winner.source.clone());
            let winner_file = winner
                .as_ref()
                .map_or_else(|| existing.id.clone(), |winner| winner.file.clone());
            load.diagnostics.push(ResourceDiagnostic::Collision {
                message: format!(
                    "package harness {}:{id} collision; keeping {winner_source}",
                    kind_name(kind)
                ),
                path: resource.path.display().to_string(),
                collision: ResourceCollision {
                    resource_type: "harness",
                    name: format!("{}:{id}", kind_name(kind)),
                    winner_path: format!("{winner_source}#{winner_file}"),
                    loser_path: format!("{}#{}", provenance.source, provenance.file),
                },
            });
            continue;
        }
        entry.extensions.insert(
            PACKAGE_PROVENANCE_KEY.to_string(),
            json!(serde_json::to_value(&provenance).unwrap_or(Value::Null)),
        );
        entries.insert(id, entry);
    }
    load
}

/// TS `harnessEntryLabel`: `package:<id>` for an overlay entry, else
/// `<scope>:<id>`.
#[must_use]
pub fn harness_entry_label(entry: &HarnessEntry) -> String {
    if is_package_entry(entry) {
        return format!("package:{}", entry.id);
    }
    let scope = match entry.scope {
        Some(super::HarnessScope::Local) => "local",
        _ => "global",
    };
    format!("{scope}:{}", entry.id)
}

/// TS `harnessVersionText`: package-controlled versions stay bounded.
#[must_use]
pub fn harness_version_text(version: u64) -> String {
    version.to_string().chars().take(12).collect()
}

/// TS `packageProvenanceText`: the prompt-visible provenance of an overlay
/// entry (never a local path); empty for editable entries.
#[must_use]
pub fn package_provenance_text(entry: &HarnessEntry, max_length: usize) -> String {
    let Some(provenance) = package_provenance(entry) else {
        return String::new();
    };
    let compact = |text: &str| super::compact_text(text, max_length);
    let revision = provenance
        .revision
        .as_deref()
        .map(|revision| format!(" rev={}", compact(revision)))
        .unwrap_or_default();
    format!(
        " [read-only package; scope={}; source={}{revision}; file={}]",
        provenance.scope,
        compact(&provenance.source),
        compact(&provenance.file)
    )
}

#[cfg(test)]
mod tests;
