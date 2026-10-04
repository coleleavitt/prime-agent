//! `stateDiagram` / `stateDiagram-v2`: states, transitions, descriptions, and composite
//! states. Ported from lovely-mermaid 0.3.3 `diagrams/state.ts` (Apache-2.0; see
//! `LICENSE-lovely-mermaid`).
//!
//! Lenient: an unreadable statement is dropped and recorded. A composite (`state X { … }`)
//! becomes a group, drawn as a titled frame by the machinery flowchart subgraphs use; `--`
//! splits a composite into unlabelled sibling region groups. `[*]` is scoped per group.

use super::super::graph::{
    parse_dir, Edge, Graph, Group, Head, LineKind, Shape, MAX_GROUPS, MAX_GROUP_DEPTH,
};
use super::super::js_text;
use super::super::labels::{ascii_lower, decode_html_entities};
use super::super::layout::{layout_flowchart, layout_grouped};
use super::super::statements::{
    first_word, header_kind, non_empty, split_colon, statements_of, take_tags,
};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["statediagram", "statediagram-v2"];

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let mut graph = parse_state(src)?;
    let canvas = if graph.groups.is_empty() {
        layout_flowchart(&mut graph)
    } else {
        layout_grouped(&graph)
    }?;
    Some(Drawn {
        canvas,
        warnings: graph.warnings,
    })
}

/// An open composite: the group itself and its current `--` region, if any.
struct Open {
    base: usize,
    region: Option<usize>,
}

/// A new group under the current scope, or `None` once a cap is hit.
fn new_group(graph: &mut Graph, depth: usize, id: String, label: String) -> Option<usize> {
    if graph.groups.len() >= MAX_GROUPS || depth >= MAX_GROUP_DEPTH {
        graph.truncate(format!(
            "subgraph cap ({MAX_GROUPS} groups, depth {MAX_GROUP_DEPTH}) reached"
        ));
        return None;
    }
    graph.groups.push(Group {
        id,
        label,
        parent: graph.cur_group,
    });
    Some(graph.groups.len() - 1)
}

fn parse_state(src: &str) -> Option<Graph> {
    let statements = statements_of(src);
    let kind = header_kind(&statements)?;
    if !HEADERS.contains(&kind.as_str()) {
        return None;
    }

    let mut graph = Graph::new(parse_dir(""));
    let mut in_note = false;
    let mut stack: Vec<Open> = Vec::new();

    for st in &statements[1..] {
        if in_note {
            if ascii_lower(st) == "end note" {
                in_note = false;
            }
            continue;
        }
        let first = ascii_lower(first_word(st));
        match first.as_str() {
            "direction" => {
                graph.dir = parse_dir(js_text::words(st).get(1).copied().unwrap_or(""));
            }
            "note" => {
                // A single-line `note … : text` needs no terminator.
                if !st.contains(':') {
                    in_note = true;
                }
            }
            "state" => {
                let rest = js_text::trim(&st[first_word(st).len()..]);
                let open = rest.ends_with('{');
                let body = if open {
                    js_text::trim(&rest[..rest.len() - 1])
                } else {
                    rest
                };
                if open {
                    // A composite. An unreadable declaration still opens an anonymous frame:
                    // the `{` was consumed, so the `}` balance must hold.
                    let named = composite_name(body);
                    if named.is_none() {
                        graph.drop_statement(st);
                    }
                    let (id, label) = named
                        .unwrap_or_else(|| (format!("anon {}", graph.groups.len()), String::new()));
                    if let Some(gi) = new_group(&mut graph, stack.len(), id, label) {
                        stack.push(Open {
                            base: gi,
                            region: None,
                        });
                        graph.cur_group = Some(gi);
                    }
                } else if parse_state_decl(body, &mut graph).is_none() {
                    graph.drop_statement(st);
                }
            }
            "}" => {
                stack.pop();
                graph.cur_group = stack.last().map(|top| top.region.unwrap_or(top.base));
            }
            "--" => {
                // Region divider: members so far move into region 1 on the first `--`; each
                // divider opens the next unlabelled sibling region.
                if let Some(top) = stack.last() {
                    let base = top.base;
                    let first_divider = top.region.is_none();
                    let depth = stack.len();
                    if first_divider {
                        let id = format!("region {}", graph.groups.len());
                        if let Some(r1) = new_group(&mut graph, depth, id, String::new()) {
                            for g in &mut graph.node_group {
                                if *g == Some(base) {
                                    *g = Some(r1);
                                }
                            }
                            for (i, g) in graph.groups.iter_mut().enumerate() {
                                if i != r1 && g.parent == Some(base) {
                                    g.parent = Some(r1);
                                }
                            }
                        }
                    }
                    graph.cur_group = Some(base);
                    let id = format!("region {}", graph.groups.len());
                    let next = new_group(&mut graph, depth, id, String::new());
                    if let Some(top) = stack.last_mut() {
                        top.region = next;
                    }
                    graph.cur_group = Some(next.unwrap_or(base));
                }
            }
            // Author classes and styling directives carry no layout meaning.
            "classdef" | "class" | "hide" | "scale" => {}
            _ => {
                let parsed = if st.contains("-->") {
                    parse_transition(st, &mut graph)
                } else {
                    parse_state_desc(st, &mut graph)
                };
                if parsed.is_none() {
                    graph.drop_statement(st);
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

/// The id and label of a composite declaration body.
fn composite_name(body: &str) -> Option<(String, String)> {
    if let Some(quoted) = body.strip_prefix('"') {
        let close = quoted.find('"')?;
        let label = decode_html_entities(&quoted[..close]);
        let after = js_text::trim(&quoted[close + 1..]);
        // A `:::` tag on a composite is dropped: groups paint no classed cells.
        let id = match after.strip_prefix("as") {
            Some(alias) => take_tags(js_text::trim(alias)).to_owned(),
            None => take_tags(&label).to_owned(),
        };
        return (!id.is_empty()).then_some((id, label));
    }
    // A stereotype on a composite carries no drawing of its own; keep the name.
    let id = take_tags(js_text::trim(body.split("<<").next().unwrap_or("")));
    (!id.is_empty() && !js_text::has_space(id)).then(|| (id.to_owned(), id.to_owned()))
}

/// `state "Label" as id` or `state id <<choice>>` — the non-composite forms.
fn parse_state_decl(rest: &str, graph: &mut Graph) -> Option<()> {
    if rest.is_empty() {
        return Some(());
    }

    if let Some(quoted) = rest.strip_prefix('"') {
        let close = quoted.find('"')?;
        let label = &quoted[..close];
        let after = js_text::trim(&quoted[close + 1..]);
        let id = match after.strip_prefix("as") {
            Some(alias) => take_tags(js_text::trim(alias)),
            None => take_tags(label),
        };
        graph.node_label(id, decode_html_entities(label))?;
        return Some(());
    }

    let mut shape = Shape::Round;
    let mut token = rest;
    let mut stereotyped = false;
    if let Some(pos) = rest.find("<<") {
        let stereo = rest[pos + 2..]
            .strip_suffix(">>")
            .unwrap_or(&rest[pos + 2..]);
        if js_text::trim(stereo) == "choice" {
            shape = Shape::Diamond;
        }
        token = js_text::trim(&rest[..pos]);
        stereotyped = true;
    }
    let id = take_tags(token);
    if id.is_empty() || js_text::has_space(id) {
        return None;
    }
    graph.node_index(id, stereotyped.then(|| id.to_owned()), shape)?;
    Some(())
}

/// `A --> B: label`, including chains `A --> B --> C`.
fn parse_transition(st: &str, graph: &mut Graph) -> Option<()> {
    let mut rest: &str = st;
    let mut prev: Option<usize> = None;

    while let Some((lhs, rhs)) = rest.split_once("-->") {
        let from_tok = take_tags(js_text::trim(js_text::trim_end(lhs).trim_end_matches('-')));
        // Mid-chain the source is the previous target, so nothing may precede.
        if prev.is_some() != from_tok.is_empty() {
            return None;
        }
        let from = match prev {
            Some(prev) => prev,
            None => state_endpoint(graph, from_tok, true)?,
        };

        // After the label colon it is all label — mermaid never chains past a label, so an
        // arrow inside one is text, not a link.
        let next_arrow = rhs.find("-->");
        let colon = split_colon(rhs);
        let label_first =
            colon.is_some_and(|(head, _)| next_arrow.is_none_or(|at| head.len() < at));
        let (to_part, label, tail) = match (label_first, colon, next_arrow) {
            (true, Some((head, text)), _) => (
                head,
                non_empty(decode_html_entities(js_text::trim(text))),
                "",
            ),
            (_, _, Some(at)) => (&rhs[..at], None, &rhs[at..]),
            _ => (rhs, None, ""),
        };

        let to_tok = take_tags(js_text::trim(
            js_text::trim_end(js_text::trim_start(to_part).trim_start_matches('>'))
                .trim_end_matches('-'),
        ));
        if to_tok.is_empty() {
            return None;
        }
        let to = state_endpoint(graph, to_tok, false)?;

        let edge = Edge::new(from, to, label, Head::Arrow, Head::None, LineKind::Solid);
        if !graph.push_edge(edge) {
            return Some(());
        }
        prev = Some(to);
        rest = tail;
    }
    Some(())
}

/// `[*]` is start or end depending on which side of the arrow it sits, and is scoped to
/// the enclosing composite — each frame has its own start and end.
fn state_endpoint(graph: &mut Graph, id: &str, is_source: bool) -> Option<usize> {
    if id == "[*]" {
        let scope = graph
            .cur_group
            .map_or_else(String::new, |g| format!(" g{g}"));
        let end = if is_source { "start" } else { "end" };
        return graph.node_index(
            &format!("[*]{end}{scope}"),
            Some("●".to_owned()),
            Shape::Round,
        );
    }
    graph.node_index(id, None, Shape::Round)
}

/// `id: description`, or a bare state name; either may carry `:::` tags.
fn parse_state_desc(st: &str, graph: &mut Graph) -> Option<()> {
    let split = split_colon(st);
    let id = take_tags(js_text::trim(split.map_or(st, |(head, _)| head)));
    if let Some((_, desc)) = split {
        let desc = js_text::trim(desc);
        if id.is_empty() || js_text::has_space(id) || desc.is_empty() {
            return None;
        }
        // Repeated descriptions accumulate (joined and wrapped); a bare node's label is its
        // id — that one is replaced.
        let before = graph.index.get(id).map(|&i| graph.nodes[i].label.clone());
        let text = decode_html_entities(desc);
        let label = match before {
            Some(before) if before != id => format!("{before} {text}"),
            _ => text,
        };
        graph.node_label(id, label)?;
    } else {
        if id.is_empty() || js_text::has_space(id) {
            return None;
        }
        graph.node_index(id, None, Shape::Round)?;
    }
    Some(())
}
