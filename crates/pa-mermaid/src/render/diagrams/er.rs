//! `erDiagram`: entities with attribute compartments and crow's-foot relationships.
//! Ported from lovely-mermaid 0.3.3 `diagrams/er.ts` (Apache-2.0; see
//! `LICENSE-lovely-mermaid`).
//!
//! Lenient: an unreadable statement is dropped and recorded. Entities use
//! `Node::sections` as `[title, attrs]`.

use super::super::graph::{parse_dir, Edge, Graph, Head, LineKind, Node, Shape, MAX_MEMBERS};
use super::super::js_text;
use super::super::labels::{clean_label, decode_html_entities, display_generics};
use super::super::layout::layout_class;
use super::super::statements::{header_kind, non_empty, statements_of};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["erdiagram"];

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let mut graph = parse_er(src)?;
    let canvas = layout_class(&mut graph)?;
    Some(Drawn {
        canvas,
        warnings: graph.warnings,
    })
}

/// The open `{` body.
#[derive(Clone, Copy)]
enum Body {
    Entity(usize),
    /// A dropped declaration's body, swallowed whole.
    Skip,
}

fn parse_er(src: &str) -> Option<Graph> {
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
            } else if let Body::Entity(idx) = open {
                push_er_attribute(&mut graph.nodes[idx], st);
            }
            continue;
        }

        if let Some((rel, label)) = split_er_relationship(st) {
            let tokens = er_tokens(rel);
            let op = if tokens.len() == 3 {
                parse_er_op(&tokens[1])
            } else {
                None
            };
            let from = op.and_then(|_| er_entity(&mut graph, &tokens[0]));
            let to = from.and_then(|_| er_entity(&mut graph, &tokens[2]));
            match (op, from, to) {
                (Some((card_l, card_r, line)), Some(from), Some(to)) => {
                    let rel_label = label.map_or_else(String::new, clean_label);
                    graph.push_edge(Edge {
                        from,
                        to,
                        label: non_empty(rel_label),
                        card_from: Some(card_l.to_owned()),
                        card_to: Some(card_r.to_owned()),
                        head_to: Head::None,
                        head_from: Head::None,
                        line,
                    });
                }
                _ => graph.drop_statement(st),
            }
        } else {
            let open = st.ends_with('{');
            let decl = if open {
                js_text::trim(&st[..st.len() - 1])
            } else {
                st.as_str()
            };
            if decl.is_empty() || er_tokens(decl).len() != 1 {
                // A bad declaration that opened a body swallows it whole.
                graph.drop_statement(st);
                if open {
                    body = Some(Body::Skip);
                }
            } else {
                let idx = er_entity(&mut graph, decl);
                if open {
                    body = Some(idx.map_or(Body::Skip, Body::Entity));
                }
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

/// Resolve an entity token (`NAME`, `"Quoted Name"`, or `id[Label]`), keeping its title
/// row fresh. A quoted name is its own identity; the quotes are not part of the title.
fn er_entity(graph: &mut Graph, token: &str) -> Option<usize> {
    let open = if token.starts_with('"') {
        None
    } else {
        token.find('[')
    };
    let idx = if let Some(open) = open {
        let id = &token[..open];
        if !token.ends_with(']') {
            graph
                .warnings
                .push(format!("entity \"{id}\": alias is missing its closing `]`"));
        }
        let label = clean_label(token[open + 1..].trim_end_matches(']'));
        if id.is_empty() || label.is_empty() {
            return None;
        }
        graph.node_label(id, label)?
    } else if token.starts_with('"') {
        let label = clean_label(token);
        if label.is_empty() {
            return None;
        }
        graph.node_index(token, Some(label), Shape::Rect)?
    } else {
        graph.node_index(token, None, Shape::Rect)?
    };
    let node = &mut graph.nodes[idx];
    let title = vec![display_generics(&node.label)];
    match node.sections.as_mut() {
        Some(sections) => sections[0] = title,
        None => node.sections = Some(vec![title, Vec::new()]),
    }
    Some(idx)
}

/// Whitespace tokens, quoted spans kept whole so aliases may contain spaces.
fn er_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            cur.push(c);
        } else if !in_quotes && js_text::is_space(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `(relationship, label)` when some token of the statement is a crow's-foot operator.
fn split_er_relationship(st: &str) -> Option<(&str, Option<&str>)> {
    let (rel, label) = match st.split_once(':') {
        Some((rel, label)) => (rel, Some(js_text::trim(label))),
        None => (st, None),
    };
    er_tokens(rel)
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
        "}o" | "o{" => Some("*"),
        "}|" | "|{" => Some("1..*"),
        _ => None,
    }
}

/// ER attributes are `type name`; a trailing quoted comment is dropped.
fn push_er_attribute(node: &mut Node, raw: &str) {
    let parts: Vec<&str> = js_text::words(raw)
        .into_iter()
        .take_while(|tok| !tok.starts_with('"'))
        .collect();
    if parts.is_empty() {
        return;
    }
    let line = decode_html_entities(&parts.join(" "));
    let Some(attrs) = node.sections.as_mut().map(|s| &mut s[1]) else {
        return;
    };
    if attrs.len() < MAX_MEMBERS {
        attrs.push(line);
    } else if attrs.len() == MAX_MEMBERS {
        attrs.push("…".to_owned());
    }
}
