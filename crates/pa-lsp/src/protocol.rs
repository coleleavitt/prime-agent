//! The slice of the Language Server Protocol this client reads, parsed
//! tolerantly: a malformed entry in a server's answer is dropped (Claude
//! Code filters the same way) instead of failing the whole answer, and
//! fields the client does not use are ignored.
//!
//! Positions on the wire are 0-based UTF-16 code units; everything the
//! model sees is 1-based, as editors show it.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC's "content modified" error: the server asks the client to retry.
pub const CONTENT_MODIFIED: i64 = -32801;
/// JSON-RPC's "method not found" error.
pub const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A location: a document and a range in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// One diagnostic as published (`textDocument/publishDiagnostics`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    /// 1 error, 2 warning, 3 information, 4 hint; absent means error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<u8>,
    /// A number or a string on the wire; kept as its text.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "code_text"
    )]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
}

fn code_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => Some(text),
        Value::Number(number) => Some(number.to_string()),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    })
}

/// A diagnostic's severity, named as Claude Code names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// The wire number's severity; an absent or unknown one is an error
    /// (Claude Code's reading).
    #[must_use]
    pub fn from_wire(value: Option<u8>) -> Self {
        match value {
            Some(2) => Self::Warning,
            Some(3) => Self::Info,
            Some(4) => Self::Hint,
            _ => Self::Error,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Info => "Info",
            Self::Hint => "Hint",
        }
    }

    /// The symbol Claude Code's diagnostics text marks the severity with.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Error => "\u{2718}",
            Self::Warning => "\u{26a0}",
            Self::Info => "\u{2139}",
            Self::Hint => "\u{2605}",
        }
    }
}

/// `textDocument/publishDiagnostics`' parameters.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PublishDiagnostics {
    pub uri: String,
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub diagnostics: Vec<Value>,
}

impl PublishDiagnostics {
    /// The well-formed diagnostics (malformed entries dropped).
    #[must_use]
    pub fn parsed(&self) -> Vec<Diagnostic> {
        self.diagnostics
            .iter()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect()
    }
}

/// A hierarchical document symbol (`DocumentSymbol`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbol {
    pub name: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub kind: u32,
    pub range: Range,
    #[serde(default)]
    pub children: Vec<DocumentSymbol>,
}

/// A flat symbol (`SymbolInformation` / `WorkspaceSymbol` with a range).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInformation {
    pub name: String,
    pub kind: u32,
    pub location: Location,
    #[serde(default)]
    pub container_name: Option<String>,
}

/// A call hierarchy item; `raw` is sent back to the server verbatim for the
/// incoming/outgoing calls request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: Range,
    pub detail: Option<String>,
    pub raw: Value,
}

impl CallHierarchyItem {
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        #[derive(Deserialize)]
        struct Item {
            name: String,
            kind: u32,
            uri: String,
            range: Range,
            #[serde(default)]
            detail: Option<String>,
        }
        let item: Item = serde_json::from_value(value.clone()).ok()?;
        Some(Self {
            name: item.name,
            kind: item.kind,
            uri: item.uri,
            range: item.range,
            detail: item.detail,
            raw: value.clone(),
        })
    }
}

/// One incoming or outgoing call: the other end and the call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub item: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

/// Parse `callHierarchy/incomingCalls` (`field` = `from`) or
/// `callHierarchy/outgoingCalls` (`field` = `to`) results.
#[must_use]
pub fn calls(value: &Value, field: &str) -> Vec<Call> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let item = CallHierarchyItem::parse(entry.get(field)?)?;
            let from_ranges = entry
                .get("fromRanges")
                .and_then(|ranges| serde_json::from_value::<Vec<Range>>(ranges.clone()).ok())
                .unwrap_or_default();
            Some(Call { item, from_ranges })
        })
        .collect()
}

/// Parse a definition/implementation/references answer: a `Location`, a
/// `Location[]`, a `LocationLink[]` (its target selection range, else its
/// target range), or null.
#[must_use]
pub fn locations(value: &Value) -> Vec<Location> {
    let one = |entry: &Value| -> Option<Location> {
        if let Some(target) = entry.get("targetUri").and_then(Value::as_str) {
            let range = entry
                .get("targetSelectionRange")
                .or_else(|| entry.get("targetRange"))
                .and_then(|range| serde_json::from_value::<Range>(range.clone()).ok())?;
            return Some(Location {
                uri: target.to_string(),
                range,
            });
        }
        serde_json::from_value::<Location>(entry.clone())
            .ok()
            .filter(|location| !location.uri.is_empty())
    };
    match value {
        Value::Array(entries) => entries.iter().filter_map(one).collect(),
        Value::Object(_) => one(value).into_iter().collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Vec::new(),
    }
}

/// A `textDocument/documentSymbol` answer: hierarchical symbols, or flat
/// symbol information (some servers answer that way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentSymbols {
    Tree(Vec<DocumentSymbol>),
    Flat(Vec<SymbolInformation>),
}

#[must_use]
pub fn document_symbols(value: &Value) -> DocumentSymbols {
    let entries = value.as_array().map(Vec::as_slice).unwrap_or_default();
    if entries
        .first()
        .is_some_and(|first| first.get("location").is_some())
    {
        return DocumentSymbols::Flat(symbol_informations(value));
    }
    DocumentSymbols::Tree(
        entries
            .iter()
            .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
            .collect(),
    )
}

/// A `workspace/symbol` answer (entries without a location range dropped).
#[must_use]
pub fn symbol_informations(value: &Value) -> Vec<SymbolInformation> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
        .collect()
}

/// The text of a hover's contents: a string, a `MarkupContent`, a
/// `MarkedString`, or an array of them (joined by blank lines).
#[must_use]
pub fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(hover_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(object) => object
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Value::Null | Value::Bool(_) | Value::Number(_) => String::new(),
    }
}

/// Claude Code's symbol kind names (`SymbolKind`, 1-based).
#[must_use]
pub fn symbol_kind(kind: u32) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Unknown",
    }
}

/// The `file://` URI of an absolute path; `None` for a relative path.
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path)
        .ok()
        .map(|url| url.to_string())
}

/// The path of a `file://` URI; `None` for another scheme or a malformed one.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

/// How Claude Code shows a location's file: relative to the working
/// directory when that is shorter and does not climb more than one level,
/// else absolute; forward slashes either way.
#[must_use]
pub fn display_uri(uri: &str, cwd: &Path) -> String {
    let Some(path) = uri_to_path(uri) else {
        return uri.to_string();
    };
    let absolute = path.display().to_string().replace('\\', "/");
    if let Some(relative) = relative_path(cwd, &path) {
        let relative = relative.replace('\\', "/");
        if relative.len() < absolute.len() && !relative.starts_with("../../") {
            return relative;
        }
    }
    absolute
}

/// `path` relative to `base` (both absolute), with `..` for each level
/// `path` sits outside `base`; `None` when they share no root.
fn relative_path(base: &Path, path: &Path) -> Option<String> {
    let base: Vec<Component<'_>> = base.components().collect();
    let target: Vec<Component<'_>> = path.components().collect();
    let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return None;
    }
    let mut parts: Vec<String> =
        std::iter::repeat_n("..".to_string(), base.len() - common).collect();
    parts.extend(
        target[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().to_string()),
    );
    Some(if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn locations_accept_every_answer_shape_and_drop_malformed_entries() {
        let range =
            json!({"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}});
        assert_eq!(locations(&Value::Null), Vec::new());
        let one = locations(&json!({"uri": "file:///a.rs", "range": range}));
        assert_eq!(one.len(), 1);
        let links = locations(&json!([
            {"targetUri": "file:///b.rs", "targetRange": range, "targetSelectionRange": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 1}}},
            {"uri": "", "range": range},
            {"nonsense": true},
        ]));
        assert_eq!(
            links,
            vec![Location {
                uri: "file:///b.rs".to_string(),
                range: Range {
                    start: Position {
                        line: 3,
                        character: 0
                    },
                    end: Position {
                        line: 3,
                        character: 1
                    },
                },
            }]
        );
    }

    #[test]
    fn diagnostic_codes_read_as_text() {
        let parsed = PublishDiagnostics {
            uri: "file:///a.rs".to_string(),
            version: None,
            diagnostics: vec![
                json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "message": "m", "code": 2304}),
                json!({"message": "no range"}),
            ],
        }
        .parsed();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].code.as_deref(), Some("2304"));
    }

    #[test]
    fn hover_contents_flatten_to_text() {
        assert_eq!(
            hover_text(&json!([{"language": "rust", "value": "fn a()"}, "docs"])),
            "fn a()\n\ndocs"
        );
        assert_eq!(
            hover_text(&json!({"kind": "markdown", "value": "**x**"})),
            "**x**"
        );
    }

    #[cfg(unix)]
    #[test]
    fn display_uri_is_relative_within_one_level_up() {
        let cwd = Path::new("/work/repo");
        assert_eq!(display_uri("file:///work/repo/src/a.rs", cwd), "src/a.rs");
        assert_eq!(display_uri("file:///work/other/b.rs", cwd), "../other/b.rs");
        assert_eq!(display_uri("file:///usr/lib/c.rs", cwd), "/usr/lib/c.rs");
        assert_eq!(
            display_uri("file:///work/repo/with%20space.rs", cwd),
            "with space.rs"
        );
    }
}
