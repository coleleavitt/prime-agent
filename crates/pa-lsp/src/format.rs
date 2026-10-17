//! Claude Code's LSP tool result texts, and the structured items the kernel
//! gets beside them. Positions in both are 1-based.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use crate::protocol::{
    Call,
    CallHierarchyItem,
    DocumentSymbol,
    DocumentSymbols,
    Location,
    SymbolInformation,
    display_uri,
    symbol_kind,
    uri_to_path,
};

/// One operation's answer for the kernel.
#[derive(Debug, Clone, PartialEq)]
pub struct Formatted {
    pub text: String,
    pub items: Vec<Value>,
    pub result_count: usize,
    pub file_count: usize,
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn path_of(uri: &str) -> String {
    uri_to_path(uri).map_or_else(|| uri.to_string(), |path| path.display().to_string())
}

fn file_count<'a>(uris: impl Iterator<Item = &'a str>) -> usize {
    uris.collect::<std::collections::HashSet<_>>().len()
}

/// Group by displayed file, keeping first-seen order.
fn grouped<'a, T>(
    entries: &'a [T],
    uri: impl Fn(&T) -> &str,
    cwd: &Path,
) -> Vec<(String, Vec<&'a T>)> {
    let mut groups: Vec<(String, Vec<&T>)> = Vec::new();
    for entry in entries {
        let file = display_uri(uri(entry), cwd);
        match groups.iter_mut().find(|(known, _)| *known == file) {
            Some((_, members)) => members.push(entry),
            None => groups.push((file, vec![entry])),
        }
    }
    groups
}

fn location_item(location: &Location) -> Value {
    json!({
        "path": path_of(&location.uri),
        "line": location.range.start.line + 1,
        "character": location.range.start.character + 1,
        "end_line": location.range.end.line + 1,
        "end_character": location.range.end.character + 1,
    })
}

fn location_text(location: &Location, cwd: &Path) -> String {
    format!(
        "{}:{}:{}",
        display_uri(&location.uri, cwd),
        location.range.start.line + 1,
        location.range.start.character + 1
    )
}

const NO_DEFINITION: &str = "No definition found. This may occur if the cursor is not on a symbol, or if the definition is in an external library not indexed by the LSP server.";

/// `goToDefinition` / `goToImplementation`.
#[must_use]
pub fn definitions(locations: &[Location], cwd: &Path) -> Formatted {
    let text = match locations {
        [] => NO_DEFINITION.to_string(),
        [one] => format!("Defined in {}", location_text(one, cwd)),
        many => format!(
            "Found {} definitions:\n{}",
            many.len(),
            many.iter()
                .map(|location| format!("  {}", location_text(location, cwd)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    Formatted {
        text,
        items: locations.iter().map(location_item).collect(),
        result_count: locations.len(),
        file_count: file_count(locations.iter().map(|location| location.uri.as_str())),
    }
}

const NO_REFERENCES: &str = "No references found. This may occur if the symbol has no usages, or if the LSP server has not fully indexed the workspace.";

/// `findReferences`.
#[must_use]
pub fn references(locations: &[Location], cwd: &Path) -> Formatted {
    let files = file_count(locations.iter().map(|location| location.uri.as_str()));
    let text = match locations {
        [] => NO_REFERENCES.to_string(),
        [one] => format!("Found 1 reference:\n  {}", location_text(one, cwd)),
        many => {
            let groups = grouped(many, |location| location.uri.as_str(), cwd);
            let mut lines = vec![format!(
                "Found {} references across {} files:",
                many.len(),
                groups.len()
            )];
            for (file, members) in groups {
                lines.push(format!("\n{file}:"));
                for location in members {
                    lines.push(format!(
                        "  Line {}:{}",
                        location.range.start.line + 1,
                        location.range.start.character + 1
                    ));
                }
            }
            lines.join("\n")
        }
    };
    Formatted {
        text,
        items: locations.iter().map(location_item).collect(),
        result_count: locations.len(),
        file_count: files,
    }
}

/// `hover`: the contents, after the range's position when there is one.
#[must_use]
pub fn hover(contents: Option<(String, Option<(u32, u32)>)>) -> Formatted {
    let Some((text, range)) = contents else {
        return Formatted {
            text: "No hover information available. This may occur if the cursor is not on a symbol, or if the LSP server has not fully indexed the file.".to_string(),
            items: Vec::new(),
            result_count: 0,
            file_count: 0,
        };
    };
    let shown = match range {
        Some((line, character)) => {
            format!("Hover info at {}:{}:\n\n{text}", line + 1, character + 1)
        }
        None => text.clone(),
    };
    Formatted {
        text: shown,
        items: vec![json!({ "text": text })],
        result_count: 1,
        file_count: 1,
    }
}

fn tree_lines(
    symbol: &DocumentSymbol,
    depth: usize,
    lines: &mut Vec<String>,
    items: &mut Vec<Value>,
    path: &str,
) {
    let mut line = format!(
        "{}{} ({})",
        "  ".repeat(depth),
        symbol.name,
        symbol_kind(symbol.kind)
    );
    if let Some(detail) = symbol.detail.as_deref().filter(|detail| !detail.is_empty()) {
        let _ = write!(line, " {detail}");
    }
    let _ = write!(line, " - Line {}", symbol.range.start.line + 1);
    lines.push(line);
    items.push(json!({
        "name": symbol.name,
        "kind": symbol_kind(symbol.kind),
        "detail": symbol.detail,
        "path": path,
        "line": symbol.range.start.line + 1,
        "character": symbol.range.start.character + 1,
        "depth": depth,
    }));
    for child in &symbol.children {
        tree_lines(child, depth + 1, lines, items, path);
    }
}

/// `documentSymbol` of the file at `path`.
#[must_use]
pub fn document_symbols(symbols: &DocumentSymbols, path: &Path, cwd: &Path) -> Formatted {
    match symbols {
        DocumentSymbols::Flat(flat) => workspace_symbols(flat, cwd),
        DocumentSymbols::Tree(tree) if tree.is_empty() => Formatted {
            text: "No symbols found in document. This may occur if the file is empty, not supported by the LSP server, or if the server has not fully indexed the file.".to_string(),
            items: Vec::new(),
            result_count: 0,
            file_count: 0,
        },
        DocumentSymbols::Tree(tree) => {
            let mut lines = vec!["Document symbols:".to_string()];
            let mut items = Vec::new();
            let shown = path.display().to_string();
            for symbol in tree {
                tree_lines(symbol, 0, &mut lines, &mut items, &shown);
            }
            Formatted {
                text: lines.join("\n"),
                result_count: items.len(),
                items,
                file_count: 1,
            }
        }
    }
}

const NO_WORKSPACE_SYMBOLS: &str = "No symbols found in workspace. This may occur if the workspace is empty, or if the LSP server has not finished indexing the project.";

/// `workspaceSymbol`.
#[must_use]
pub fn workspace_symbols(symbols: &[SymbolInformation], cwd: &Path) -> Formatted {
    if symbols.is_empty() {
        return Formatted {
            text: NO_WORKSPACE_SYMBOLS.to_string(),
            items: Vec::new(),
            result_count: 0,
            file_count: 0,
        };
    }
    let groups = grouped(symbols, |symbol| symbol.location.uri.as_str(), cwd);
    let mut lines = vec![format!(
        "Found {} {} in workspace:",
        symbols.len(),
        plural(symbols.len(), "symbol")
    )];
    for (file, members) in &groups {
        lines.push(format!("\n{file}:"));
        for symbol in members {
            let mut line = format!(
                "  {} ({}) - Line {}",
                symbol.name,
                symbol_kind(symbol.kind),
                symbol.location.range.start.line + 1
            );
            if let Some(container) = symbol
                .container_name
                .as_deref()
                .filter(|container| !container.is_empty())
            {
                let _ = write!(line, " in {container}");
            }
            lines.push(line);
        }
    }
    Formatted {
        text: lines.join("\n"),
        items: symbols
            .iter()
            .map(|symbol| {
                json!({
                    "name": symbol.name,
                    "kind": symbol_kind(symbol.kind),
                    "path": path_of(&symbol.location.uri),
                    "line": symbol.location.range.start.line + 1,
                    "character": symbol.location.range.start.character + 1,
                    "container": symbol.container_name,
                })
            })
            .collect(),
        result_count: symbols.len(),
        file_count: groups.len(),
    }
}

fn call_item(item: &CallHierarchyItem) -> Value {
    json!({
        "name": item.name,
        "kind": symbol_kind(item.kind),
        "detail": item.detail,
        "path": path_of(&item.uri),
        "line": item.range.start.line + 1,
        "character": item.range.start.character + 1,
    })
}

/// `prepareCallHierarchy`.
#[must_use]
pub fn call_hierarchy_items(items: &[CallHierarchyItem], cwd: &Path) -> Formatted {
    let line = |item: &CallHierarchyItem| {
        let mut text = format!(
            "{} ({}) - {}:{}",
            item.name,
            symbol_kind(item.kind),
            display_uri(&item.uri, cwd),
            item.range.start.line + 1
        );
        if let Some(detail) = item.detail.as_deref().filter(|detail| !detail.is_empty()) {
            let _ = write!(text, " [{detail}]");
        }
        text
    };
    let text = match items {
        [] => "No call hierarchy item found at this position".to_string(),
        [one] => format!("Call hierarchy item: {}", line(one)),
        many => {
            let mut lines = vec![format!("Found {} call hierarchy items:", many.len())];
            lines.extend(many.iter().map(|item| format!("  {}", line(item))));
            lines.join("\n")
        }
    };
    Formatted {
        text,
        items: items.iter().map(call_item).collect(),
        result_count: items.len(),
        file_count: file_count(items.iter().map(|item| item.uri.as_str())),
    }
}

/// Which way a call hierarchy is walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

/// `incomingCalls` / `outgoingCalls`.
#[must_use]
pub fn calls(calls: &[Call], direction: Direction, cwd: &Path) -> Formatted {
    if calls.is_empty() {
        let text = match direction {
            Direction::Incoming => "No incoming calls found (nothing calls this function)",
            Direction::Outgoing => "No outgoing calls found (this function calls nothing)",
        };
        return Formatted {
            text: text.to_string(),
            items: Vec::new(),
            result_count: 0,
            file_count: 0,
        };
    }
    let (word, sites) = match direction {
        Direction::Incoming => ("incoming", "calls at"),
        Direction::Outgoing => ("outgoing", "called from"),
    };
    let groups = grouped(calls, |call| call.item.uri.as_str(), cwd);
    let mut lines = vec![format!(
        "Found {} {word} {}:",
        calls.len(),
        plural(calls.len(), "call")
    )];
    for (file, members) in &groups {
        lines.push(format!("\n{file}:"));
        for call in members {
            let mut line = format!(
                "  {} ({}) - Line {}",
                call.item.name,
                symbol_kind(call.item.kind),
                call.item.range.start.line + 1
            );
            if !call.from_ranges.is_empty() {
                let ranges = call
                    .from_ranges
                    .iter()
                    .map(|range| format!("{}:{}", range.start.line + 1, range.start.character + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = write!(line, " [{sites}: {ranges}]");
            }
            lines.push(line);
        }
    }
    Formatted {
        text: lines.join("\n"),
        items: calls
            .iter()
            .map(|call| {
                let mut item = call_item(&call.item);
                item["ranges"] = call
                    .from_ranges
                    .iter()
                    .map(|range| json!({"line": range.start.line + 1, "character": range.start.character + 1}))
                    .collect();
                item
            })
            .collect(),
        result_count: calls.len(),
        file_count: groups.len(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::protocol::{Position, Range};

    fn location(uri: &str, line: u32, character: u32) -> Location {
        Location {
            uri: uri.to_string(),
            range: Range {
                start: Position { line, character },
                end: Position { line, character },
            },
        }
    }

    #[test]
    fn definitions_and_references_read_like_claude_code() {
        let cwd = Path::new("/repo");
        assert_eq!(definitions(&[], cwd).text, NO_DEFINITION);
        assert_eq!(
            definitions(&[location("file:///repo/src/a.rs", 9, 3)], cwd).text,
            "Defined in src/a.rs:10:4"
        );
        let refs = references(
            &[
                location("file:///repo/a.rs", 0, 0),
                location("file:///repo/b.rs", 4, 2),
                location("file:///repo/a.rs", 7, 1),
            ],
            cwd,
        );
        assert_eq!(
            refs.text,
            "Found 3 references across 2 files:\n\na.rs:\n  Line 1:1\n  Line 8:2\n\nb.rs:\n  Line 5:3"
        );
        assert_eq!((refs.result_count, refs.file_count), (3, 2));
        assert_eq!(
            refs.items[1],
            json!({"path": "/repo/b.rs", "line": 5, "character": 3, "end_line": 5, "end_character": 3})
        );
    }

    #[test]
    fn hover_names_its_position() {
        assert_eq!(
            hover(Some(("fn a()".to_string(), Some((2, 0))))).text,
            "Hover info at 3:1:\n\nfn a()"
        );
        assert_eq!(hover(None).result_count, 0);
    }
}
