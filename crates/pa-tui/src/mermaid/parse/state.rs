//! The state-diagram grammar. Ported from grok-mermaid 0.2.3 `parse.ts` (Apache-2.0; see
//! `LICENSE-grok-mermaid`).

use super::super::graph::{Edge, Graph, Head, LineKind, Shape, parse_dir};
use super::super::js_text;
use super::super::labels::{ascii_lower, decode_html_entities};
use super::{drop_style_tags, first_word, header_kind, non_empty, split_once, statements_of};

pub(in crate::mermaid) fn parse_state(src: &str) -> Option<Graph> {
    let statements = statements_of(src);
    if !header_kind(&statements)?.starts_with("statediagram") {
        return None;
    }

    let mut graph = Graph::new(super::super::graph::Dir::Down);
    let mut in_note = false;

    for st in &statements[1..] {
        if in_note {
            if ascii_lower(st) == "end note" {
                in_note = false;
            }
            continue;
        }
        let st = drop_style_tags(st);
        let first = ascii_lower(first_word(&st));
        match first.as_str() {
            "direction" => {
                graph.dir = parse_dir(js_text::words(&st).get(1).copied().unwrap_or(""));
            }
            // A single-line `note ... : text` needs no terminator.
            "note" => in_note = !st.contains(':'),
            "state" => parse_state_decl(&st, &mut graph)?,
            // Styling and composite-state punctuation carry no layout meaning.
            "classdef" | "class" | "hide" | "scale" | "}" | "--" => {}
            _ if st.contains("-->") => parse_transition(&st, &mut graph)?,
            _ => parse_state_desc(&st, &mut graph)?,
        }
        if graph.over_cap {
            return None;
        }
    }

    (!graph.nodes.is_empty()).then_some(graph)
}

/// `state "Label" as id`, `state id <<choice>>`, or `state id {`.
fn parse_state_decl(st: &str, graph: &mut Graph) -> Option<()> {
    let rest = js_text::trim(&st["state".len()..]);
    let rest = js_text::trim(rest.strip_suffix('{').unwrap_or(rest));
    if rest.is_empty() {
        return Some(());
    }

    if let Some(quoted) = rest.strip_prefix('"') {
        let close = quoted.find('"')?;
        let label = &quoted[..close];
        let after = js_text::trim(&quoted[close + 1..]);
        let id = match after.strip_prefix("as") {
            Some(id) => js_text::trim(id),
            None => label,
        };
        return graph.node_label(id, decode_html_entities(label)).map(drop);
    }

    let mut shape = Shape::Round;
    let mut id = rest;
    let mut stereotyped = false;
    if let Some(pos) = rest.find("<<") {
        let stereo = &rest[pos + 2..];
        let stereo = js_text::trim(stereo.strip_suffix(">>").unwrap_or(stereo));
        if stereo == "choice" {
            shape = Shape::Diamond;
        }
        id = js_text::trim(&rest[..pos]);
        stereotyped = true;
    }
    if id.is_empty() || js_text::has_space(id) {
        return None;
    }
    graph
        .node_index(id, stereotyped.then(|| id.to_owned()), shape)
        .map(drop)
}

/// `A --> B: label`, including chains `A --> B --> C`.
fn parse_transition(st: &str, graph: &mut Graph) -> Option<()> {
    let mut rest = st;
    let mut prev: Option<usize> = None;

    while let Some((lhs, rhs)) = split_once(rest, "-->") {
        let from_id = js_text::trim(js_text::trim_end(lhs).trim_end_matches('-'));
        // Mid-chain the source is the previous target, so nothing may precede the arrow.
        let from = if let Some(prev) = prev {
            if !from_id.is_empty() {
                return None;
            }
            prev
        } else {
            if from_id.is_empty() {
                return None;
            }
            state_endpoint(graph, from_id, Endpoint::Source)?
        };

        let (to_part_raw, tail) = match rhs.find("-->") {
            Some(next_arrow) => (&rhs[..next_arrow], &rhs[next_arrow..]),
            None => (rhs, ""),
        };
        let (to_part, label) = match split_once(to_part_raw, ":") {
            Some((to_part, label)) => (
                to_part,
                non_empty(decode_html_entities(js_text::trim(label))),
            ),
            None => (to_part_raw, None),
        };

        let to_id = js_text::trim_start(to_part).trim_start_matches('>');
        let to_id = js_text::trim(js_text::trim_end(to_id).trim_end_matches('-'));
        if to_id.is_empty() {
            return None;
        }
        let to = state_endpoint(graph, to_id, Endpoint::Target)?;

        if !graph.push_edge(Edge {
            from,
            to,
            label,
            head_to: Head::Arrow,
            head_from: Head::None,
            line: LineKind::Solid,
        }) {
            return Some(());
        }
        prev = Some(to);
        rest = tail;
    }
    Some(())
}

/// Which side of the arrow a `[*]` sits on: it is the start or the end state.
#[derive(Clone, Copy)]
enum Endpoint {
    Source,
    Target,
}

fn state_endpoint(graph: &mut Graph, id: &str, side: Endpoint) -> Option<usize> {
    if id == "[*]" {
        let key = match side {
            Endpoint::Source => "[*]start",
            Endpoint::Target => "[*]end",
        };
        return graph.node_index(key, Some("●".to_owned()), Shape::Round);
    }
    graph.node_index(id, None, Shape::Round)
}

/// `id: description`, or a bare state name.
fn parse_state_desc(st: &str, graph: &mut Graph) -> Option<()> {
    if let Some((id, desc)) = split_once(st, ":") {
        let id = js_text::trim(id);
        let desc = js_text::trim(desc);
        if id.is_empty() || js_text::has_space(id) || desc.is_empty() {
            return None;
        }
        return graph.node_label(id, decode_html_entities(desc)).map(drop);
    }
    if js_text::has_space(st) {
        return None;
    }
    graph.node_index(st, None, Shape::Round).map(drop)
}
