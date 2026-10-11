//! `graph` / `flowchart`: node chains with inline shapes and labelled links, plus
//! `subgraph` grouping. Ported from lovely-mermaid 0.3.3 `diagrams/flowchart.ts`
//! (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! The grammar is lenient: a statement contributes whatever prefix parsed and the rest is
//! dropped, recorded in `graph.warnings`.

use super::super::graph::{
    Edge,
    Graph,
    Group,
    Head,
    LineKind,
    MAX_GROUP_DEPTH,
    MAX_GROUPS,
    Shape,
    parse_dir,
};
use super::super::js_text;
use super::super::labels::{ascii_lower, clean_label, decode_html_entities, is_id_char};
use super::super::layout::{layout_flowchart, layout_grouped};
use super::super::statements::{first_word, header_kind, non_empty, split_top, statements_of};
use super::Drawn;

pub(in crate::render) const HEADERS: &[&str] = &["graph", "flowchart"];

pub(in crate::render) fn render(src: &str) -> Option<Drawn> {
    let mut graph = parse_graph(src)?;
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

fn parse_graph(src: &str) -> Option<Graph> {
    let statements = statements_of(src);
    let kind = header_kind(&statements)?;
    if !HEADERS.contains(&kind.as_str()) {
        return None;
    }

    let header_words = js_text::words(&statements[0]);
    let mut graph = Graph::new(parse_dir(header_words.get(1).copied().unwrap_or("TB")));
    let mut stack: Vec<usize> = Vec::new();

    for st in &statements[1..] {
        match ascii_lower(first_word(st)).as_str() {
            "subgraph" => {
                if graph.groups.len() >= MAX_GROUPS || stack.len() >= MAX_GROUP_DEPTH {
                    graph.truncate(format!(
                        "subgraph cap ({MAX_GROUPS} groups, depth {MAX_GROUP_DEPTH}) reached"
                    ));
                } else {
                    let (id, label) = parse_subgraph_decl(js_text::trim(&st["subgraph".len()..]));
                    graph.groups.push(Group {
                        id,
                        label,
                        parent: stack.last().copied(),
                    });
                    stack.push(graph.groups.len() - 1);
                    graph.cur_group = stack.last().copied();
                    continue;
                }
            }
            "end" => {
                stack.pop();
                graph.cur_group = stack.last().copied();
                continue;
            }
            // Author classes, link targets, and styling carry no cell of their own.
            "classdef" | "class" | "click" | "style" | "linkstyle" | "direction" => continue,
            _ => {}
        }
        if graph.truncated.is_none() {
            parse_statement(st, &mut graph);
        }
        if graph.truncated.is_some() {
            break;
        }
    }

    if let Some(cap) = &graph.truncated {
        let warning = format!("diagram truncated: {cap}");
        graph.warnings.push(warning);
    }
    (!graph.nodes.is_empty()).then_some(graph)
}

/// `subgraph id[Title]`, `subgraph "Title"`, or a bare title.
fn parse_subgraph_decl(rest: &str) -> (String, String) {
    if let Some(quoted) = rest.strip_prefix('"') {
        if let Some(close) = quoted.find('"') {
            let label = &quoted[..close];
            return (label.to_owned(), decode_html_entities(label));
        }
    }
    if let Some(open) = rest.find('[') {
        let id = js_text::trim(&rest[..open]);
        let label = clean_label(js_text::trim(rest[open + 1..].trim_end_matches(']')));
        if !id.is_empty() && !label.is_empty() {
            return (id.to_owned(), label);
        }
    }
    (rest.to_owned(), rest.to_owned())
}

fn collect(chars: &[char]) -> String {
    chars.iter().collect()
}

/// Warn about unread source unless a cap already explains it.
fn warn(graph: &mut Graph, warning: String) {
    if graph.truncated.is_none() {
        graph.warnings.push(warning);
    }
}

/// A chain of `node link node link node ...`, each link fanning out over `&`. Parses as far
/// as it can and keeps the prefix; whatever it could not read lands in `graph.warnings`.
fn parse_statement(st: &str, graph: &mut Graph) {
    let chars: Vec<char> = st.chars().collect();

    let Some((head, next)) = parse_node_group(&chars, 0, graph) else {
        warn(
            graph,
            format!("dropped, does not start with a node: \"{st}\""),
        );
        return;
    };
    let mut prev = head;
    let mut i = next;

    loop {
        i = skip_spaces(&chars, i);
        if i >= chars.len() {
            break;
        }
        let Some(link) = parse_link(&chars, i) else {
            let rest = collect(&chars[i..]);
            warn(graph, format!("dropped, expected a link: \"{rest}\""));
            break;
        };
        i = skip_spaces(&chars, link.next);
        let Some((target, next)) = parse_node_group(&chars, i, graph) else {
            warn(graph, format!("dropped, link has no target: \"{st}\""));
            break;
        };
        i = next;
        for &f in &prev {
            for &t in &target {
                // `A <-- B` reads right-to-left: swap the endpoints so the arrow written on
                // the left becomes a normal forward head.
                let reversed = link.left == Head::Arrow && link.right != Head::Arrow;
                let pushed = graph.push_edge(Edge::new(
                    if reversed { t } else { f },
                    if reversed { f } else { t },
                    link.label.clone(),
                    if reversed { Head::Arrow } else { link.right },
                    if reversed { link.right } else { link.left },
                    link.line,
                ));
                if !pushed {
                    return;
                }
            }
        }
        prev = target;
    }
}

/// One or more nodes joined by `&`, which fan out into a cross product.
fn parse_node_group(
    chars: &[char],
    start: usize,
    graph: &mut Graph,
) -> Option<(Vec<usize>, usize)> {
    let (first, mut i) = parse_node(chars, start, graph)?;
    let mut group = vec![first];
    loop {
        let j = skip_spaces(chars, i);
        if chars.get(j) != Some(&'&') {
            break;
        }
        let (next, after) = parse_node(chars, j + 1, graph)?;
        group.push(next);
        i = after;
    }
    Some((group, i))
}

fn skip_spaces(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }
    i
}

/// The end of a `:::name` class tag starting at `at`, if one is there: a name never ends
/// in `-`, so `A:::x-->B` keeps its link.
fn class_tag_end(chars: &[char], at: usize) -> Option<usize> {
    if chars.get(at..at + 3) != Some(&[':', ':', ':'][..]) {
        return None;
    }
    let mut k = at + 3;
    while k < chars.len() && (is_id_char(chars[k]) || chars[k] == '-') {
        k += 1;
    }
    while k > at + 3 && chars[k - 1] == '-' {
        k -= 1;
    }
    (k > at + 3).then_some(k)
}

fn parse_node(chars: &[char], start: usize, graph: &mut Graph) -> Option<(usize, usize)> {
    let mut i = skip_spaces(chars, start);
    let id_start = i;
    // `-` joins the id only when an id char follows, so kebab-case ids parse while `-->`
    // / `-.` / `--` still terminate.
    while i < chars.len()
        && (is_id_char(chars[i])
            || (chars[i] == '-' && chars.get(i + 1).is_some_and(|&c| is_id_char(c))))
    {
        i += 1;
    }
    if i == id_start {
        return None;
    }
    let id = collect(&chars[id_start..i]);

    let shaped = if chars.get(i) == Some(&'@') && chars.get(i + 1) == Some(&'{') {
        read_at_shape(chars, i + 2)
    } else {
        read_shape_at(chars, i)
    };
    if let Some(unclosed) = shaped.unclosed {
        graph.warnings.push(format!(
            "node \"{id}\": label is missing its closing `{unclosed}`"
        ));
    }
    let index = graph.node_index(&id, shaped.label, shaped.shape)?;

    // `id:::name` (after any shape) attaches an author class; swallow it so the statement
    // keeps parsing.
    let next = class_tag_end(chars, shaped.after).unwrap_or(shaped.after);
    Some((index, next))
}

/// What a shape bracket yielded; `unclosed` names the closer that was never found.
struct Shaped {
    shape: Shape,
    label: Option<String>,
    after: usize,
    unclosed: Option<&'static str>,
}

/// Dispatch on the bracket following an id to pick shape and closing token.
fn read_shape_at(chars: &[char], i: usize) -> Shaped {
    let c = chars.get(i).copied();
    let n = chars.get(i + 1).copied();
    match (c, n) {
        (Some('['), Some('[')) => read_shape(chars, i + 2, "]]", Shape::Rect),
        (Some('['), Some('(')) => read_shape(chars, i + 2, ")]", Shape::Round),
        (Some('[' | '>'), _) => read_shape(chars, i + 1, "]", Shape::Rect),
        (Some('('), Some('(')) => read_shape(chars, i + 2, "))", Shape::Round),
        (Some('('), Some('[')) => read_shape(chars, i + 2, "])", Shape::Round),
        (Some('('), _) => read_shape(chars, i + 1, ")", Shape::Round),
        (Some('{'), Some('{')) => read_shape(chars, i + 2, "}}", Shape::Diamond),
        (Some('{'), _) => read_shape(chars, i + 1, "}", Shape::Diamond),
        _ => Shaped {
            shape: Shape::Rect,
            label: None,
            after: i,
            unclosed: None,
        },
    }
}

/// Read label text up to `closer`. Quoting is decided by the first non-space character:
/// inside a quoted label the closer is ignored until the quote closes.
fn read_shape(chars: &[char], start: usize, closer: &'static str, shape: Shape) -> Shaped {
    let mut j = start;
    while matches!(chars.get(j), Some(' ' | '\t')) {
        j += 1;
    }
    let quoted = chars.get(j) == Some(&'"');
    let closer_chars: Vec<char> = closer.chars().collect();

    let mut i = start;
    let mut text = String::new();
    let mut in_quotes = false;
    while i < chars.len() {
        let c = chars[i];
        if quoted && c == '"' {
            in_quotes = !in_quotes;
            text.push(c);
            i += 1;
            continue;
        }
        if !in_quotes && chars[i..].starts_with(&closer_chars) {
            return Shaped {
                shape,
                label: Some(clean_label(&text)),
                after: i + closer_chars.len(),
                unclosed: None,
            };
        }
        text.push(c);
        i += 1;
    }
    // Ran off the end looking for the closer: everything after the opening bracket became
    // label text, so any link operator in it was swallowed.
    Shaped {
        shape,
        label: Some(clean_label(&text)),
        after: chars.len(),
        unclosed: Some(closer),
    }
}

/// Flowchart v2 shape names that read as something other than a plain box; every other
/// name means "some kind of box".
fn at_shape(name: &str) -> Shape {
    match name {
        "rounded" | "stadium" | "pill" | "terminal" | "cyl" | "cylinder" | "database" | "db"
        | "circle" | "circ" | "sm-circ" | "small-circle" | "dbl-circ" | "double-circle"
        | "fr-circ" | "framed-circle" | "start" | "stop" | "event" | "delay" | "cloud" | "bang" => {
            Shape::Round
        }
        "diam" | "diamond" | "decision" | "question" | "hex" | "hexagon" | "prepare" => {
            Shape::Diamond
        }
        _ => Shape::Rect,
    }
}

/// The v2 node syntax `id@{shape: cyl, label: "..."}`, cursor past the `@{`: `key: value`
/// pairs split on top-level commas; quoted values may hold commas and `}`.
fn read_at_shape(chars: &[char], start: usize) -> Shaped {
    let mut i = start;
    let mut depth = 0usize;
    let mut in_quotes = false;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == '{' {
            depth += 1;
        } else if c == '}' {
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
        i += 1;
    }
    let body = collect(&chars[start..i]);
    let closed = chars.get(i) == Some(&'}');

    let mut shape = Shape::Rect;
    let mut label = None;
    for pair in split_top(&body, |c| c == ',') {
        let Some((key, value)) = pair.split_once(':') else {
            continue;
        };
        match ascii_lower(js_text::trim(key)).as_str() {
            "shape" => shape = at_shape(&ascii_lower(js_text::trim(value))),
            "label" => label = non_empty(clean_label(value)),
            _ => {}
        }
    }
    Shaped {
        shape,
        label,
        after: if closed { i + 1 } else { chars.len() },
        unclosed: (!closed).then_some("}"),
    }
}

fn is_link_char(c: char) -> bool {
    matches!(c, '-' | '.' | '=' | '<' | '>')
}

struct Link {
    left: Head,
    right: Head,
    line: LineKind,
    label: Option<String>,
    next: usize,
}

/// Read a link operator and its label: `-->|text|`, or the inline `-- text -->` when the
/// first operator carried no head.
fn parse_link(chars: &[char], start: usize) -> Option<Link> {
    let mut i = skip_spaces(chars, start);
    let mut left = Head::None;
    // A leading `o`/`x` decorates the tail, but only directly before an operator.
    if let (Some(&c @ ('o' | 'x')), Some('-' | '.' | '=')) = (chars.get(i), chars.get(i + 1)) {
        left = if c == 'o' { Head::Circle } else { Head::Cross };
        i += 1;
    }

    let op_start = i;
    while i < chars.len() && is_link_char(chars[i]) {
        i += 1;
    }
    if i == op_start {
        return None;
    }
    let op1 = &chars[op_start..i];
    if left == Head::None && op1.first() == Some(&'<') {
        left = Head::Arrow;
    }

    let mut line = line_kind(op1);
    let mut right = if op1.contains(&'>') {
        Head::Arrow
    } else {
        Head::None
    };
    if right == Head::None {
        if let Some((head, next)) = trailing_head(chars, i) {
            right = head;
            i = next;
        }
    }

    if chars.get(i) == Some(&'|') {
        i += 1;
        let l_start = i;
        // A quoted stretch keeps its `|`s as text: `-->|"a|b"|`.
        let mut in_quotes = false;
        while i < chars.len() && (in_quotes || chars[i] != '|') {
            if chars[i] == '"' {
                in_quotes = !in_quotes;
            }
            i += 1;
        }
        let label = clean_label(&collect(&chars[l_start..i]));
        if chars.get(i) == Some(&'|') {
            i += 1;
        }
        return Some(Link {
            left,
            right,
            line,
            label: non_empty(label),
            next: i,
        });
    }

    if right == Head::None {
        let text_start = skip_spaces(chars, i);
        let mut j = text_start;
        // The label runs to the closing operator, which always starts with two link chars
        // (`--`, `-.`, `==`): a lone `=`/`.`/`-` is label text, and a quoted stretch is
        // label text throughout.
        while j < chars.len() {
            if chars[j] == '"' {
                j += 1;
                while j < chars.len() && chars[j] != '"' {
                    j += 1;
                }
                if j < chars.len() {
                    j += 1;
                }
            } else if is_link_char(chars[j]) && chars.get(j + 1).is_some_and(|&c| is_link_char(c)) {
                break;
            } else {
                j += 1;
            }
        }
        if j < chars.len() && j > text_start && chars[j] != '<' {
            let text = collect(&chars[text_start..j]);
            let second_op_at = j;
            while j < chars.len() && is_link_char(chars[j]) {
                j += 1;
            }
            let op2 = &chars[second_op_at..j];
            if op2.contains(&'>') {
                right = Head::Arrow;
            } else if let Some((head, next)) = trailing_head(chars, j) {
                right = head;
                j = next;
            }
            if line == LineKind::Solid {
                line = line_kind(op2);
            }
            return Some(Link {
                left,
                right,
                line,
                label: non_empty(clean_label(&text)),
                next: j,
            });
        }
    }

    Some(Link {
        left,
        right,
        line,
        label: None,
        next: i,
    })
}

fn line_kind(op: &[char]) -> LineKind {
    if op.contains(&'=') {
        LineKind::Thick
    } else if op.contains(&'.') {
        LineKind::Dotted
    } else {
        LineKind::Solid
    }
}

/// A trailing `o`/`x` head, only when followed by a statement boundary.
fn trailing_head(chars: &[char], i: usize) -> Option<(Head, usize)> {
    let head = match chars.get(i)? {
        'o' => Head::Circle,
        'x' => Head::Cross,
        _ => return None,
    };
    let boundary = matches!(chars.get(i + 1), None | Some(' ' | '\t' | '|' | '&' | ';'));
    boundary.then_some((head, i + 1))
}
