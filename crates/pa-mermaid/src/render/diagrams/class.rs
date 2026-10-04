//! `classDiagram`: classes with member compartments and UML relations. Ported from
//! lovely-mermaid 0.3.3 `diagrams/class.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! Lenient: an unreadable statement is dropped and recorded. Compartments live on
//! `Node::sections`: `[title, attrs, methods]`, the title being the optional
//! `«annotation»` line over the class name.

use super::super::graph::{parse_dir, Edge, Graph, Head, LineKind, Node, Shape, MAX_MEMBERS};
use super::super::js_text;
use super::super::labels::{
    ascii_lower, clean_label, decode_html_entities, display_generics, is_id_char,
};
use super::super::layout::layout_class;
use super::super::statements::{
    first_word, header_kind, is_class_assign, non_empty, quote_mask, split_colon, statements_of,
    take_tags,
};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["classdiagram", "classdiagram-v2"];

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let mut graph = parse_class(src)?;
    let canvas = layout_class(&mut graph)?;
    Some(Drawn {
        canvas,
        warnings: graph.warnings,
    })
}

/// Relation operators, longest first so `--|>` wins over `--`:
/// `(op, head_from, head_to, line)`.
const CLASS_OPS: [(&str, Head, Head, LineKind); 14] = [
    ("<|--", Head::Triangle, Head::None, LineKind::Solid),
    ("--|>", Head::None, Head::Triangle, LineKind::Solid),
    ("<|..", Head::Triangle, Head::None, LineKind::Dotted),
    ("..|>", Head::None, Head::Triangle, LineKind::Dotted),
    ("*--", Head::DiamondFill, Head::None, LineKind::Solid),
    ("--*", Head::None, Head::DiamondFill, LineKind::Solid),
    ("o--", Head::DiamondOpen, Head::None, LineKind::Solid),
    ("--o", Head::None, Head::DiamondOpen, LineKind::Solid),
    ("<--", Head::Arrow, Head::None, LineKind::Solid),
    ("-->", Head::None, Head::Arrow, LineKind::Solid),
    ("<..", Head::Arrow, Head::None, LineKind::Dotted),
    ("..>", Head::None, Head::Arrow, LineKind::Dotted),
    ("--", Head::None, Head::None, LineKind::Solid),
    ("..", Head::None, Head::None, LineKind::Dotted),
];

const MAX_CLASS_OP: usize = 4;

/// The open `{` body.
#[derive(Clone, Copy)]
enum Body {
    Class(usize),
    /// A dropped declaration's body, swallowed whole.
    Skip,
}

/// Declare a class from an id token, dropping any `:::` tags it carries.
fn declare(graph: &mut Graph, token: &str) -> Option<usize> {
    let id = take_tags(token);
    let idx = graph.node_index(id, None, Shape::Rect)?;
    let node = &mut graph.nodes[idx];
    if node.sections.is_none() {
        node.sections = Some(vec![vec![display_generics(id)], Vec::new(), Vec::new()]);
    }
    Some(idx)
}

fn parse_class(src: &str) -> Option<Graph> {
    let statements = statements_of(src);
    let kind = header_kind(&statements)?;
    if !HEADERS.contains(&kind.as_str()) {
        return None;
    }

    let mut graph = Graph::new(parse_dir(""));
    let mut body: Option<Body> = None;

    for st in &statements[1..] {
        if let Some(open) = body {
            if st == "}" {
                body = None;
            } else if let Body::Class(idx) = open {
                push_member(&mut graph.nodes[idx], st);
            }
            continue;
        }

        let first = ascii_lower(first_word(st));
        match first.as_str() {
            "direction" => {
                graph.dir = parse_dir(js_text::words(st).get(1).copied().unwrap_or(""));
                continue;
            }
            // Styles, notes, namespaces, and link targets draw nothing of their own.
            "classdef" | "note" | "callback" | "style" | "namespace" | "}" | "link" | "click" => {
                continue
            }
            "cssclass" => {
                let rest = st[first_word(st).len()..].replace('"', "");
                if !is_class_assign(&rest) {
                    graph.drop_statement(st);
                }
                continue;
            }
            _ => {}
        }

        if first == "class" {
            parse_class_decl(st, &mut graph, &mut body);
        } else if let Some(after) = st.strip_prefix("<<") {
            match after.split_once(">>") {
                Some((annotation, name))
                    if !js_text::trim(name).is_empty()
                        && !js_text::has_space(js_text::trim(name)) =>
                {
                    if let Some(idx) = declare(&mut graph, js_text::trim(name)) {
                        set_annotation(&mut graph.nodes[idx], js_text::trim(annotation));
                    }
                }
                _ => graph.drop_statement(st),
            }
        } else if let Some(rel) = parse_class_relation(st) {
            let from = declare(&mut graph, &rel.from);
            let to = from.and_then(|_| declare(&mut graph, &rel.to));
            if let (Some(from), Some(to)) = (from, to) {
                graph.push_edge(Edge {
                    from,
                    to,
                    label: rel.label,
                    card_from: rel.card_from,
                    card_to: rel.card_to,
                    head_to: rel.head_to,
                    head_from: rel.head_from,
                    line: rel.line,
                });
            }
        } else {
            match split_colon(st) {
                Some((id, text))
                    if !js_text::trim(id).is_empty()
                        && !js_text::has_space(js_text::trim(id))
                        && !js_text::trim(text).is_empty() =>
                {
                    if let Some(idx) = declare(&mut graph, js_text::trim(id)) {
                        push_member(&mut graph.nodes[idx], js_text::trim(text));
                    }
                }
                _ => graph.drop_statement(st),
            }
        }
        if let Some(cap) = &graph.truncated {
            let warning = format!("diagram truncated: {cap}");
            graph.warnings.push(warning);
            break;
        }
    }

    (!graph.nodes.is_empty()).then_some(graph)
}

/// The JS `/^(\S+?)\[(.+)\](:::\S+)?$/` over a class name: `(id, label, tags)` for the
/// `class A["Label"]` form.
fn labeled_class(name: &str) -> Option<(&str, &str, &str)> {
    let is_terminator = |c: char| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    for (p, _) in name.match_indices('[') {
        let id = &name[..p];
        if js_text::has_space(id) {
            return None;
        }
        if id.is_empty() {
            continue;
        }
        let rest = &name[p + 1..];
        if rest.contains(is_terminator) {
            continue;
        }
        // Greedy `(.+)`: the last `]` whose tail is empty or a `:::` tag run.
        let close = rest.match_indices(']').rev().find(|&(q, _)| {
            let tail = &rest[q + 1..];
            q > 0
                && (tail.is_empty()
                    || tail
                        .strip_prefix(":::")
                        .is_some_and(|tags| !tags.is_empty() && !js_text::has_space(tags)))
        });
        if let Some((q, _)) = close {
            return Some((id, &rest[..q], &rest[q + 1..]));
        }
    }
    None
}

/// `class Name`, `class Name {`, `class Name["Label"]`, or the assignment form
/// `class A,B name`.
fn parse_class_decl(st: &str, graph: &mut Graph, body: &mut Option<Body>) {
    let rest = js_text::trim(&st["class".len()..]);
    let open = rest.ends_with('{');
    let mut name = if open {
        js_text::trim(&rest[..rest.len() - 1]).to_owned()
    } else {
        rest.to_owned()
    };
    // `class A["Label"]`: the label titles the box, the id keys relations. Peeled before
    // the space test — labels carry spaces.
    let mut label = None;
    if let Some((id, text, tags)) = labeled_class(&name) {
        label = non_empty(clean_label(text));
        name = format!("{id}{tags}");
    }
    if !open && js_text::has_space(&name) {
        // Class names carry no spaces, so `class Agent focus` is the assignment form.
        if !is_class_assign(&name) {
            graph.drop_statement(st);
        }
    } else if name.is_empty() || js_text::has_space(&name) {
        // A bad declaration that opened a body swallows it whole.
        graph.drop_statement(st);
        if open {
            *body = Some(Body::Skip);
        }
    } else {
        let idx = declare(graph, &name);
        if let (Some(idx), Some(label)) = (idx, label) {
            let node = &mut graph.nodes[idx];
            // The name is the last title line (an annotation may precede it).
            if let Some(last) = node
                .sections
                .as_mut()
                .and_then(|s| s.first_mut())
                .and_then(|title| title.last_mut())
            {
                last.clone_from(&label);
            }
            node.label = label;
        }
        if open {
            *body = Some(idx.map_or(Body::Skip, Body::Class));
        }
    }
}

/// Rewrite the title compartment as `«annotation»` over the class name.
fn set_annotation(node: &mut Node, annotation: &str) {
    let title = vec![format!("«{annotation}»"), display_generics(&node.label)];
    if let Some(sections) = node.sections.as_mut() {
        sections[0] = title;
    }
}

/// Add a member to the attribute or method compartment, eliding past the cap.
fn push_member(node: &mut Node, raw: &str) {
    if let Some(after) = raw.strip_prefix("<<") {
        if let Some((annotation, _)) = after.split_once(">>") {
            set_annotation(node, js_text::trim(annotation));
        }
        return;
    }
    let member = decode_html_entities(&display_generics(js_text::trim(raw)));
    let Some(sections) = node.sections.as_mut() else {
        return;
    };
    let list = if member.contains('(') {
        &mut sections[2]
    } else {
        &mut sections[1]
    };
    if list.len() < MAX_MEMBERS {
        list.push(member);
    } else if list.len() == MAX_MEMBERS {
        list.push("…".to_owned());
    }
}

struct ClassRelation {
    from: String,
    to: String,
    head_from: Head,
    head_to: Head,
    line: LineKind,
    label: Option<String>,
    card_from: Option<String>,
    card_to: Option<String>,
}

fn parse_class_relation(st: &str) -> Option<ClassRelation> {
    let chars: Vec<char> = st.chars().collect();
    // Skip quoted spans, or the `..` inside a cardinality like `"0..*"` would match the
    // dotted-link operator.
    let quoted = quote_mask(&chars);
    let mut found = None;
    'outer: for pos in 0..chars.len() {
        if quoted[pos] {
            continue;
        }
        let tail: String = chars[pos..(pos + MAX_CLASS_OP).min(chars.len())]
            .iter()
            .collect();
        for &(op, head_from, head_to, line) in &CLASS_OPS {
            if !tail.starts_with(op) {
                continue;
            }
            // `o` is also an identifier character: skip a match glued to a name.
            if op.starts_with('o') && pos > 0 && is_id_char(chars[pos - 1]) {
                continue;
            }
            let after = chars.get(pos + op.len());
            if op.ends_with('o') && after.is_some_and(|&c| is_id_char(c)) {
                continue;
            }
            found = Some((pos, op.len(), head_from, head_to, line));
            break 'outer;
        }
    }
    let (pos, op_len, head_from, head_to, line) = found?;

    let lhs_raw: String = chars[..pos].iter().collect();
    let rhs_raw: String = chars[pos + op_len..].iter().collect();
    let (lhs, card_from) = strip_cardinality_suffix(js_text::trim(&lhs_raw));
    let (rhs, card_to) = strip_cardinality_prefix(js_text::trim(&rhs_raw));

    let split = split_colon(rhs);
    let to_id = js_text::trim(split.map_or(rhs, |(head, _)| head));
    let label = split.and_then(|(_, text)| non_empty(decode_html_entities(js_text::trim(text))));

    if lhs.is_empty() || to_id.is_empty() || js_text::has_space(lhs) || js_text::has_space(to_id) {
        return None;
    }
    Some(ClassRelation {
        from: lhs.to_owned(),
        to: to_id.to_owned(),
        head_from,
        head_to,
        line,
        label,
        card_from: (!card_from.is_empty()).then(|| card_from.to_owned()),
        card_to: (!card_to.is_empty()).then(|| card_to.to_owned()),
    })
}

/// `Class "1"` — a quoted cardinality trailing the left-hand name.
fn strip_cardinality_suffix(s: &str) -> (&str, &str) {
    let t = js_text::trim_end(s);
    if let Some(rest) = t.strip_suffix('"') {
        if let Some(q) = rest.rfind('"') {
            return (js_text::trim_end(&rest[..q]), &rest[q + 1..]);
        }
    }
    (t, "")
}

/// `"0..*" Class` — a quoted cardinality leading the right-hand name.
fn strip_cardinality_prefix(s: &str) -> (&str, &str) {
    let t = js_text::trim_start(s);
    if let Some(rest) = t.strip_prefix('"') {
        if let Some(q) = rest.find('"') {
            return (js_text::trim_start(&rest[q + 1..]), &rest[..q]);
        }
    }
    (t, "")
}
