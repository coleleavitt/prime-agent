//! `mindmap`: an indentation tree drawn with `├──`/`└──` guides. Ported from
//! lovely-mermaid 0.3.3 `diagrams/mindmap.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! Parses raw lines rather than statements — the indentation is the grammar.

use super::super::canvas::{Canvas, draw_text};
use super::super::graph::MAX_NODES;
use super::super::labels::{WRAP_WIDTH, clean_label, fit_label, src_lines};
use super::super::layout::width_of;
use super::super::statements::frontmatter_end;
use super::super::{Role, js_text};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["mindmap"];

struct MindNode {
    text: String,
    children: Vec<MindNode>,
}

/// `(prefix, text)` per row; prefixes carry the `│ ├ └` guides.
fn walk(node: &MindNode, prefix: String, child_prefix: &str, rows: &mut Vec<(String, String)>) {
    rows.push((prefix, node.text.clone()));
    for (i, child) in node.children.iter().enumerate() {
        let last = i + 1 == node.children.len();
        let branch = if last { "└── " } else { "├── " };
        let guide = if last { "    " } else { "│   " };
        walk(
            child,
            format!("{child_prefix}{branch}"),
            &format!("{child_prefix}{guide}"),
            rows,
        );
    }
}

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let (roots, warnings) = parse_mindmap(src)?;
    let mut rows: Vec<(String, String)> = Vec::new();
    for root in &roots {
        walk(root, String::new(), "", &mut rows);
    }
    let width = rows
        .iter()
        .map(|(p, t)| width_of(p) + width_of(t))
        .max()
        .unwrap_or(0);
    let mut canvas = Canvas::new(width, rows.len() as i64);
    for (y, (prefix, text)) in rows.iter().enumerate() {
        let y = y as i64;
        draw_text(&mut canvas, prefix, 0, y, Role::Edge);
        draw_text(&mut canvas, text, width_of(prefix), y, Role::Text);
    }
    Some(Drawn { canvas, warnings })
}

/// The child list a path of child indices from the roots names.
fn children_at<'a>(roots: &'a mut Vec<MindNode>, path: &[usize]) -> &'a mut Vec<MindNode> {
    let mut list = roots;
    for &i in path {
        list = &mut list[i].children;
    }
    list
}

fn parse_mindmap(src: &str) -> Option<(Vec<MindNode>, Vec<String>)> {
    let all = src_lines(src);
    let lines = &all[frontmatter_end(&all).min(all.len())..];
    let header_at = lines.iter().position(|l| !js_text::trim(l).is_empty())?;
    if js_text::trim(lines[header_at]).to_lowercase() != "mindmap" {
        return None;
    }

    let mut roots: Vec<MindNode> = Vec::new();
    let mut warnings = Vec::new();
    // Ancestors of the next node: (indent, index within its parent's child list).
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut count = 0;
    let mut truncated = false;

    for raw in &lines[header_at + 1..] {
        let no_comment = raw.split("%%").next().unwrap_or("");
        if js_text::trim(no_comment).is_empty() {
            continue;
        }
        let indent = no_comment.encode_utf16().count()
            - js_text::trim_start(no_comment).encode_utf16().count();
        let body = js_text::trim(no_comment);
        // Decoration lines attach to the previous node and draw nothing.
        if body.starts_with("::icon") || body.starts_with(":::") {
            continue;
        }
        let text = node_text(body);
        if text.is_empty() {
            warnings.push(format!("dropped, unreadable statement: \"{body}\""));
            continue;
        }
        if count >= MAX_NODES {
            truncated = true;
            break;
        }
        count += 1;
        let node = MindNode {
            text: fit_label(&text, WRAP_WIDTH),
            children: Vec::new(),
        };
        while stack.last().is_some_and(|&(level, _)| level >= indent) {
            stack.pop();
        }
        let path: Vec<usize> = stack.iter().map(|&(_, i)| i).collect();
        let siblings = children_at(&mut roots, &path);
        siblings.push(node);
        stack.push((indent, siblings.len() - 1));
    }
    if truncated {
        warnings.push(format!("diagram truncated: node cap ({MAX_NODES}) reached"));
    }

    (!roots.is_empty()).then_some((roots, warnings))
}

/// Shape brackets around a mindmap node all mean "text" in a terminal.
const SHAPES: [(&str, &str); 6] = [
    ("((", "))"),
    ("))", "(("),
    ("(-", "-)"),
    ("{{", "}}"),
    ("[", "]"),
    ("(", ")"),
];

fn node_text(body: &str) -> String {
    // `id((text))` — an optional id may precede the bracket.
    for (open, close) in SHAPES {
        if let Some(at) = body.find(open) {
            if body.ends_with(close) && body.len() >= at + open.len() + close.len() {
                return clean_label(&body[at + open.len()..body.len() - close.len()]);
            }
        }
    }
    clean_label(body)
}
