//! The element-indexed text the model reads, and its diff.
//!
//! [`serialize`] renders a tree into one stable line per element; [`diff`]
//! pairs two renders by their index-stripped content and marks changed lines
//! `~`, added lines `+` and removed lines `-`, omitting unchanged ones. An
//! unchanged element whose index shifted still surfaces (as `~`), so a reused
//! old index is visible instead of silently targeting another element.

mod seqmatch;

use std::fmt::Write as _;

use crate::element::Element;
use crate::pyfmt::{repr_float, repr_str, round1};

use seqmatch::{opcodes, Tag};

/// Render one tree depth-first into indexed lines:
/// `{indent}[{index}] role (subrole) 'title' = 'value' description='…'
/// placeholder='…' [secure] (actions: a, b) @ (x, y) WxH`, every empty
/// attribute omitted. A secure field never renders its value.
#[must_use]
pub fn serialize(tree: &[Element]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut stack: Vec<(&Element, usize)> = tree.iter().rev().map(|element| (element, 0)).collect();
    while let Some((element, depth)) = stack.pop() {
        lines.push(line(lines.len(), depth, element));
        stack.extend(
            element
                .children
                .iter()
                .rev()
                .map(|child| (child, depth + 1)),
        );
    }
    lines
}

fn non_empty(text: Option<&String>) -> Option<&str> {
    text.map(String::as_str).filter(|text| !text.is_empty())
}

fn line(index: usize, depth: usize, element: &Element) -> String {
    let role = non_empty(element.role.as_ref()).unwrap_or("AXUnknown");
    let mut parts = vec![format!("{}[{index}] {role}", "  ".repeat(depth))];
    if let Some(subrole) = non_empty(element.subrole.as_ref()) {
        parts.push(format!("({subrole})"));
    }
    if let Some(title) = non_empty(element.title.as_ref()) {
        parts.push(repr_str(title));
    }
    if element.is_secure() {
        parts.push("[secure]".to_string());
    } else if let Some(value) = non_empty(element.value.as_ref()) {
        parts.push(format!("= {}", repr_str(value)));
    }
    if let Some(description) = non_empty(element.description.as_ref()) {
        parts.push(format!("description={}", repr_str(description)));
    }
    if let Some(placeholder) = non_empty(element.placeholder.as_ref()) {
        parts.push(format!("placeholder={}", repr_str(placeholder)));
    }
    if !element.actions.is_empty() {
        parts.push(format!("(actions: {})", element.actions.join(", ")));
    }
    if let Some((x, y)) = element.position {
        let mut geometry = format!("@ ({}, {})", number(x), number(y));
        if let Some((width, height)) = element.size {
            let _ = write!(geometry, " {}x{}", number(width), number(height));
        }
        parts.push(geometry);
    }
    parts.join(" ")
}

/// One coordinate, compactly: integral values without a fraction, others
/// rounded to one decimal (Python's `round(x, 1)` then float `repr`).
fn number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        repr_float(round1(value))
    }
}

/// Mark the changes between two renders, omitting unchanged lines.
#[must_use]
pub fn diff(previous: &[String], current: &[String]) -> String {
    let previous_content: Vec<(&str, &str)> =
        previous.iter().map(|line| strip_index(line)).collect();
    let current_content: Vec<(&str, &str)> = current.iter().map(|line| strip_index(line)).collect();
    let mut output: Vec<String> = Vec::new();
    let marked = |marker: char, lines: &[String]| -> Vec<String> {
        lines.iter().map(|line| format!("{marker}{line}")).collect()
    };
    for op in opcodes(&previous_content, &current_content) {
        let (old, new) = (&previous[op.i1..op.i2], &current[op.j1..op.j2]);
        match op.tag {
            Tag::Equal => {
                for (old_line, new_line) in old.iter().zip(new) {
                    if index_of(old_line) != index_of(new_line) {
                        output.push(format!("~{new_line}"));
                    }
                }
            }
            Tag::Replace if old.len() == new.len() => output.extend(marked('~', new)),
            Tag::Replace => {
                output.extend(marked('-', old));
                output.extend(marked('+', new));
            }
            Tag::Delete => output.extend(marked('-', old)),
            Tag::Insert => output.extend(marked('+', new)),
        }
    }
    output.join("\n")
}

/// The `[N] ` index prefix of one line (after its indent), when present.
fn index_span(line: &str) -> Option<(usize, usize)> {
    let indent = line.len() - line.trim_start().len();
    let rest = line[indent..].strip_prefix('[')?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || !rest[digits..].starts_with("] ") {
        return None;
    }
    // [indent, indent + "[" + digits + "] ")
    Some((indent, indent + 1 + digits + 2))
}

fn index_of(line: &str) -> Option<&str> {
    index_span(line).map(|(start, end)| &line[start + 1..end - 2])
}

/// The line with its index dropped, as (indent, rest): shifted elements
/// with equal content at the same depth pair up. Only ever compared.
fn strip_index(line: &str) -> (&str, &str) {
    match index_span(line) {
        Some((start, end)) => (&line[..start], &line[end..]),
        None => ("", line),
    }
}

#[cfg(test)]
mod tests;
