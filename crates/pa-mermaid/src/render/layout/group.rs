//! Flowcharts with `subgraph`: each subgraph becomes a framed box holding its own
//! independently laid-out canvas; state composites and their `--` regions draw through
//! the same machinery. Ported from lovely-mermaid 0.3.3 `layout.ts` (Apache-2.0; see
//! `LICENSE-lovely-mermaid`).

use std::collections::HashMap;

use super::super::canvas::Canvas;
use super::super::graph::{Edge, Graph, Node, Shape};
use super::{NodeExtra, layout_canvas, orient};

/// One thing laid out in a scope: a node, or a nested subgraph's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Item {
    Node(usize),
    Group(usize),
}

/// The scope that draws an edge (`None` is the top level) and its two endpoints there.
type ScopeEdges = HashMap<Option<usize>, Vec<(Item, Item, usize)>>;

/// Lay out a flowchart that uses `subgraph`. An edge is drawn in the innermost scope
/// containing both endpoints; one crossing a subgraph boundary attaches to the frame.
pub(in crate::render) fn layout_grouped(graph: &Graph) -> Option<Canvas> {
    // A node whose id matches a subgraph id stands in for that subgraph.
    let mut proxy: HashMap<usize, usize> = HashMap::new();
    for (gi, g) in graph.groups.iter().enumerate() {
        if let Some(&ni) = graph.index.get(&g.id) {
            proxy.insert(ni, gi);
        }
    }

    let group_chain = |g: Option<usize>| {
        let mut chain = Vec::new();
        let mut cur = g;
        while let Some(c) = cur {
            chain.push(c);
            cur = graph.groups[c].parent;
        }
        chain.reverse();
        chain
    };
    let endpoint = |n: usize| match proxy.get(&n) {
        None => (Item::Node(n), group_chain(graph.node_group[n])),
        Some(&gi) => (Item::Group(gi), group_chain(graph.groups[gi].parent)),
    };

    let mut scope_edges: ScopeEdges = HashMap::new();
    let mut referenced = vec![false; graph.groups.len()];
    for (ei, e) in graph.edges.iter().enumerate() {
        let (f_key, f_chain) = endpoint(e.from);
        let (t_key, t_chain) = endpoint(e.to);
        let mut k = 0;
        while k < f_chain.len() && k < t_chain.len() && f_chain[k] == t_chain[k] {
            k += 1;
        }
        let scope = (k > 0).then(|| f_chain[k - 1]);
        let f_key = f_chain.get(k).map_or(f_key, |&g| Item::Group(g));
        let t_key = t_chain.get(k).map_or(t_key, |&g| Item::Group(g));
        for key in [f_key, t_key] {
            if let Item::Group(g) = key {
                referenced[g] = true;
            }
        }
        scope_edges
            .entry(scope)
            .or_default()
            .push((f_key, t_key, ei));
    }

    let mut direct_nodes: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (ni, &g) in graph.node_group.iter().enumerate() {
        if !proxy.contains_key(&ni) {
            direct_nodes.entry(g).or_default().push(ni);
        }
    }

    // Drop empty subgraphs, but keep any that an edge attaches to. Walked by the actual
    // child relation: state `--` regions reparent earlier groups under later ones, so index
    // order says nothing about depth.
    let mut child_groups: Vec<Vec<usize>> = vec![Vec::new(); graph.groups.len()];
    for (gi, g) in graph.groups.iter().enumerate() {
        if let Some(parent) = g.parent {
            child_groups[parent].push(gi);
        }
    }
    let mut keep = vec![false; graph.groups.len()];
    for (gi, g) in graph.groups.iter().enumerate() {
        if g.parent.is_none() {
            visit_keep(gi, &child_groups, &referenced, &direct_nodes, &mut keep);
        }
    }

    let scopes = Scopes {
        graph,
        scope_edges: &scope_edges,
        direct_nodes: &direct_nodes,
        keep: &keep,
    };
    scopes.build(None).map(|canvas| orient(canvas, graph.dir))
}

/// Whether group `gi` stays (it holds nodes, a kept child, or an edge end), recording
/// the answer for it and every descendant.
fn visit_keep(
    gi: usize,
    child_groups: &[Vec<usize>],
    referenced: &[bool],
    direct_nodes: &HashMap<Option<usize>, Vec<usize>>,
    keep: &mut [bool],
) -> bool {
    let mut kept = referenced[gi]
        || direct_nodes
            .get(&Some(gi))
            .is_some_and(|nodes| !nodes.is_empty());
    for &c in &child_groups[gi] {
        if visit_keep(c, child_groups, referenced, direct_nodes, keep) {
            kept = true;
        }
    }
    keep[gi] = kept;
    kept
}

/// The bucketed graph every scope lays out from.
struct Scopes<'a> {
    graph: &'a Graph,
    scope_edges: &'a ScopeEdges,
    direct_nodes: &'a HashMap<Option<usize>, Vec<usize>>,
    keep: &'a [bool],
}

impl Scopes<'_> {
    fn build(&self, scope: Option<usize>) -> Option<Canvas> {
        let graph = self.graph;
        let mut items: Vec<Item> = self
            .direct_nodes
            .get(&scope)
            .map(|nodes| nodes.iter().map(|&n| Item::Node(n)).collect())
            .unwrap_or_default();
        items.extend(
            (0..graph.groups.len())
                .filter(|&gi| graph.groups[gi].parent == scope && self.keep[gi])
                .map(Item::Group),
        );

        if items.is_empty() {
            return Some(Canvas::new(1, 1));
        }

        let mut index_of: HashMap<Item, usize> = HashMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        let mut extras: Vec<NodeExtra> = Vec::new();
        for &item in &items {
            index_of.insert(item, nodes.len());
            match item {
                Item::Node(i) => {
                    nodes.push(Node {
                        sections: None,
                        ..graph.nodes[i].clone()
                    });
                    extras.push(NodeExtra::Plain);
                }
                Item::Group(i) => {
                    let sub = self.build(Some(i))?;
                    nodes.push(Node {
                        label: graph.groups[i].label.clone(),
                        shape: Shape::Rect,
                        sections: None,
                    });
                    extras.push(NodeExtra::Frame(sub));
                }
            }
        }

        let mut edges: Vec<Edge> = Vec::new();
        for &(f, t, ei) in self.scope_edges.get(&scope).into_iter().flatten() {
            let (Some(&fi), Some(&ti)) = (index_of.get(&f), index_of.get(&t)) else {
                continue;
            };
            // A scope edge carries the label, heads, and line; no cardinalities.
            let e = &graph.edges[ei];
            edges.push(Edge::new(
                fi,
                ti,
                e.label.clone(),
                e.head_to,
                e.head_from,
                e.line,
            ));
        }

        // Layout only reads nodes, edges, and the direction.
        let mut synth = Graph::new(graph.dir);
        synth.nodes = nodes;
        synth.edges = edges;
        layout_canvas(&mut synth, &extras)
    }
}
