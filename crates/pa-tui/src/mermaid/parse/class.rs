//! The class- and ER-diagram grammars (both draw compartment boxes). Ported from
//! grok-mermaid 0.2.3 `parse.ts` (Apache-2.0; see `LICENSE-grok-mermaid`).

use super::super::graph::{
    parse_dir, ClassInfo, Dir, Edge, Graph, Head, LineKind, Shape, MAX_EDGES, MAX_MEMBERS,
};
use super::super::js_text;
use super::super::labels::{ascii_lower, clean_label, decode_html_entities, is_id_char};
use super::{
    collect, drop_style_tags, first_word, header_kind, non_empty, split_once, statements_of,
};

/// Relation operators, longest-first so `--|>` wins over `--`.
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

/// Statements a class diagram carries no layout meaning for.
const CLASS_IGNORED: [&str; 9] = [
    "note",
    "callback",
    "click",
    "link",
    "style",
    "cssclass",
    "classdef",
    "namespace",
    "}",
];

/// A class or ER graph with its compartment content, parallel to the nodes.
type Compartmented = (Graph, Vec<ClassInfo>);

/// Declare a class, keeping `infos` aligned with `graph.nodes`.
fn declare(graph: &mut Graph, infos: &mut Vec<ClassInfo>, name: &str) -> Option<usize> {
    let idx = graph.node_index(name, None, Shape::Rect);
    infos.resize_with(graph.nodes.len(), ClassInfo::default);
    idx
}

pub(in crate::mermaid) fn parse_class(src: &str) -> Option<Compartmented> {
    let statements = statements_of(src);
    if !header_kind(&statements)?.starts_with("classdiagram") {
        return None;
    }

    let mut graph = Graph::new(Dir::Down);
    let mut infos: Vec<ClassInfo> = Vec::new();
    let mut cur_class: Option<usize> = None;

    for st in &statements[1..] {
        if let Some(class) = cur_class {
            if st == "}" {
                cur_class = None;
            } else {
                push_member(&mut infos[class], st);
            }
            continue;
        }
        let st = drop_style_tags(st);

        let first = ascii_lower(first_word(&st));
        if first == "direction" {
            graph.dir = parse_dir(js_text::words(&st).get(1).copied().unwrap_or(""));
            continue;
        }
        if CLASS_IGNORED.contains(&first.as_str()) {
            continue;
        }
        if first == "class" {
            let rest = js_text::trim(&st["class".len()..]);
            let (name, open) = match rest.strip_suffix('{') {
                Some(name) => (js_text::trim(name), true),
                None => (rest, false),
            };
            if name.is_empty() || js_text::has_space(name) {
                return None;
            }
            let idx = declare(&mut graph, &mut infos, name)?;
            if open {
                cur_class = Some(idx);
            }
            continue;
        }

        if let Some(after) = st.strip_prefix("<<") {
            let (annotation, name) = split_once(after, ">>")?;
            let name = js_text::trim(name);
            if name.is_empty() || js_text::has_space(name) {
                return None;
            }
            let idx = declare(&mut graph, &mut infos, name)?;
            infos[idx].annotation = Some(js_text::trim(annotation).to_owned());
            continue;
        }

        if let Some(rel) = parse_class_relation(&st) {
            let f = declare(&mut graph, &mut infos, &rel.from)?;
            let t = declare(&mut graph, &mut infos, &rel.to)?;
            if graph.edges.len() >= MAX_EDGES {
                return None;
            }
            graph.edges.push(Edge {
                from: f,
                to: t,
                label: rel.label,
                head_to: rel.head_to,
                head_from: rel.head_from,
                line: rel.line,
            });
            continue;
        }

        let (id, text) = split_once(&st, ":")?;
        let id = js_text::trim(id);
        let text = js_text::trim(text);
        if id.is_empty() || js_text::has_space(id) || text.is_empty() {
            return None;
        }
        let idx = declare(&mut graph, &mut infos, id)?;
        push_member(&mut infos[idx], text);
    }

    if graph.nodes.is_empty() {
        return None;
    }
    infos.resize_with(graph.nodes.len(), ClassInfo::default);
    Some((graph, infos))
}

/// Add a member to the attribute or method compartment, eliding past the cap.
fn push_member(info: &mut ClassInfo, raw: &str) {
    if let Some(after) = raw.strip_prefix("<<") {
        if let Some((annotation, _)) = split_once(after, ">>") {
            info.annotation = Some(js_text::trim(annotation).to_owned());
        }
        return;
    }
    let member = decode_html_entities(&display_generics(js_text::trim(raw)));
    let list = if member.contains('(') {
        &mut info.methods
    } else {
        &mut info.attrs
    };
    push_capped(list, member);
}

/// Push under the per-box cap; the first item past it becomes the `…` marker.
fn push_capped(list: &mut Vec<String>, item: String) {
    if list.len() < MAX_MEMBERS {
        list.push(item);
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
}

fn parse_class_relation(st: &str) -> Option<ClassRelation> {
    let chars: Vec<char> = st.chars().collect();
    let mut found = None;
    'outer: for pos in 0..chars.len() {
        let tail = collect(&chars[pos..(pos + 4).min(chars.len())]);
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
            found = Some((pos, op, head_from, head_to, line));
            break 'outer;
        }
    }
    let (pos, op, head_from, head_to, line) = found?;

    let lhs_raw = collect(&chars[..pos]);
    let rhs_raw = collect(&chars[pos + op.len()..]);
    let (lhs, card_from) = strip_cardinality_suffix(js_text::trim(&lhs_raw));
    let (rhs, card_to) = strip_cardinality_prefix(js_text::trim(&rhs_raw));

    let (to_id, rel_label) = match split_once(rhs, ":") {
        Some((to_id, label)) => (to_id, non_empty(decode_html_entities(js_text::trim(label)))),
        None => (rhs, None),
    };
    let to_id = js_text::trim(to_id);

    if lhs.is_empty() || to_id.is_empty() || js_text::has_space(lhs) || js_text::has_space(to_id) {
        return None;
    }

    let parts: Vec<&str> = [card_from, rel_label.as_deref().unwrap_or(""), card_to]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    Some(ClassRelation {
        from: lhs.to_owned(),
        to: to_id.to_owned(),
        head_from,
        head_to,
        line,
        label: non_empty(parts.join(" ")),
    })
}

/// `Class "1"`: a quoted cardinality trailing the left-hand name.
fn strip_cardinality_suffix(s: &str) -> (&str, &str) {
    let t = js_text::trim_end(s);
    if let Some(rest) = t.strip_suffix('"') {
        if let Some(q) = rest.rfind('"') {
            return (js_text::trim_end(&rest[..q]), &rest[q + 1..]);
        }
    }
    (t, "")
}

/// `"0..*" Class`: a quoted cardinality leading the right-hand name.
fn strip_cardinality_prefix(s: &str) -> (&str, &str) {
    let t = js_text::trim_start(s);
    if let Some(rest) = t.strip_prefix('"') {
        if let Some(q) = rest.find('"') {
            return (js_text::trim_start(&rest[q + 1..]), &rest[..q]);
        }
    }
    (t, "")
}

/// Mermaid writes generics as `List~T~`; show them as `List<T>`.
pub(in crate::mermaid) fn display_generics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut open = false;
    for c in s.chars() {
        if c == '~' {
            out.push(if open { '>' } else { '<' });
            open = !open;
        } else {
            out.push(c);
        }
    }
    out
}

// ------------------------------------------------------------------------ ER

pub(in crate::mermaid) fn parse_er(src: &str) -> Option<Compartmented> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "erdiagram" {
        return None;
    }

    let mut graph = Graph::new(Dir::Down);
    let mut infos: Vec<ClassInfo> = Vec::new();
    let mut cur_entity: Option<usize> = None;

    for st in &statements[1..] {
        if let Some(entity) = cur_entity {
            if st == "}" {
                cur_entity = None;
            } else {
                push_er_attribute(&mut infos[entity], st);
            }
            continue;
        }

        if let Some((rel, label)) = split_er_relationship(st) {
            let tokens = js_text::words(rel);
            if tokens.len() != 3 {
                return None;
            }
            let (card_l, card_r, line) = parse_er_op(tokens[1])?;
            let f = er_entity(&mut graph, &mut infos, tokens[0])?;
            let t = er_entity(&mut graph, &mut infos, tokens[2])?;
            if graph.edges.len() >= MAX_EDGES {
                return None;
            }
            let rel_label = label.map(clean_label).unwrap_or_default();
            let parts: Vec<&str> = [card_l, rel_label.as_str(), card_r]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            graph.edges.push(Edge {
                from: f,
                to: t,
                label: non_empty(parts.join(" ")),
                head_to: Head::None,
                head_from: Head::None,
                line,
            });
            continue;
        }

        let (decl, open) = match st.strip_suffix('{') {
            Some(decl) => (js_text::trim(decl), true),
            None => (st.as_str(), false),
        };
        if decl.is_empty() || js_text::words(decl).len() != 1 {
            return None;
        }
        let idx = er_entity(&mut graph, &mut infos, decl)?;
        if open {
            cur_entity = Some(idx);
        }
    }

    if graph.nodes.is_empty() {
        return None;
    }
    infos.resize_with(graph.nodes.len(), ClassInfo::default);
    Some((graph, infos))
}

fn er_entity(graph: &mut Graph, infos: &mut Vec<ClassInfo>, token: &str) -> Option<usize> {
    let idx = match token.find('[') {
        Some(open) => {
            let id = &token[..open];
            let label = clean_label(token[open + 1..].trim_end_matches(']'));
            if id.is_empty() || label.is_empty() {
                return None;
            }
            graph.node_label(id, label)
        }
        None => graph.node_index(token, None, Shape::Rect),
    }?;
    infos.resize_with(graph.nodes.len(), ClassInfo::default);
    Some(idx)
}

/// The relationship half and the trimmed label of a statement naming a crow's-foot
/// operator.
fn split_er_relationship(st: &str) -> Option<(&str, Option<&str>)> {
    let (rel, label) = match split_once(st, ":") {
        Some((rel, label)) => (rel, Some(js_text::trim(label))),
        None => (st, None),
    };
    js_text::words(rel)
        .iter()
        .any(|t| parse_er_op(t).is_some())
        .then_some((rel, label))
}

/// A crow's-foot operator: two cardinality glyphs around `--` or `..`.
fn parse_er_op(tok: &str) -> Option<(&'static str, &'static str, LineKind)> {
    if tok.len() != 6 || !tok.is_ascii() {
        return None;
    }
    let line = match &tok[2..4] {
        "--" => LineKind::Solid,
        ".." => LineKind::Dotted,
        _ => return None,
    };
    Some((er_card(&tok[..2])?, er_card(&tok[4..])?, line))
}

fn er_card(tok: &str) -> Option<&'static str> {
    match tok {
        "|o" | "o|" => Some("0..1"),
        "||" => Some("1"),
        "}o" | "o{" => Some("0..*"),
        "}|" | "|{" => Some("1..*"),
        _ => None,
    }
}

/// ER attributes are `type name`; a trailing quoted comment is dropped.
fn push_er_attribute(info: &mut ClassInfo, raw: &str) {
    let parts: Vec<&str> = js_text::words(raw)
        .into_iter()
        .take_while(|tok| !tok.starts_with('"'))
        .collect();
    if parts.is_empty() {
        return;
    }
    push_capped(&mut info.attrs, decode_html_entities(&parts.join(" ")));
}
