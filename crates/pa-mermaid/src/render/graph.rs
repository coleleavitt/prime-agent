//! The shared diagram model: flowchart, state, class, and ER sources all parse into a
//! [`Graph`]; sequence diagrams have their own model. Ported from lovely-mermaid 0.3.3
//! `graph.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! The package also carries author classes (`:::name`, `class A,B name`), link targets
//! (`click`/`link`), and parsed `classDef`s through the model onto its spans. They never
//! change a cell's glyph or role — only how the package splits a row into spans, and its
//! own ANSI styling — and the product styles by role alone, so this port parses those
//! statements without carrying them.

use std::collections::HashMap;

/// Caps that keep layout bounded; reaching one truncates the diagram.
pub(super) const MAX_NODES: usize = 128;
pub(super) const MAX_EDGES: usize = 512;
pub(super) const MAX_GROUPS: usize = 24;
pub(super) const MAX_GROUP_DEPTH: usize = 6;
/// Class members / ER attributes listed per box before eliding with `…`.
pub(super) const MAX_MEMBERS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shape {
    Rect,
    Round,
    Diamond,
}

/// Decoration at one end of an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Head {
    None,
    Arrow,
    Circle,
    Cross,
    Triangle,
    DiamondFill,
    DiamondOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LineKind {
    Solid,
    Dotted,
    Thick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dir {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Debug, Clone)]
pub(super) struct Node {
    pub(super) label: String,
    pub(super) shape: Shape,
    /// Compartment content for class and ER boxes, pre-formatted by the parser: one string
    /// per row, one list per compartment (title, attributes, methods).
    pub(super) sections: Option<Vec<Vec<String>>>,
}

#[derive(Debug, Clone)]
pub(super) struct Edge {
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) label: Option<String>,
    /// Cardinalities (ER crow's-foot, class multiplicities), painted at their own end.
    pub(super) card_from: Option<String>,
    pub(super) card_to: Option<String>,
    pub(super) head_to: Head,
    pub(super) head_from: Head,
    pub(super) line: LineKind,
}

impl Edge {
    /// A plain edge with no cardinalities.
    pub(super) fn new(
        from: usize,
        to: usize,
        label: Option<String>,
        head_to: Head,
        head_from: Head,
        line: LineKind,
    ) -> Self {
        Self {
            from,
            to,
            label,
            card_from: None,
            card_to: None,
            head_to,
            head_from,
            line,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Group {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) parent: Option<usize>,
}

/// `LR`/`RL`/`BT` as written in a header or `direction` statement; else down.
pub(super) fn parse_dir(token: &str) -> Dir {
    match token.to_ascii_uppercase().as_str() {
        "LR" => Dir::Right,
        "RL" => Dir::Left,
        "BT" => Dir::Up,
        _ => Dir::Down,
    }
}

#[derive(Debug, Clone)]
pub(super) struct Graph {
    pub(super) nodes: Vec<Node>,
    pub(super) edges: Vec<Edge>,
    pub(super) index: HashMap<String, usize>,
    pub(super) groups: Vec<Group>,
    /// Innermost subgraph each node was declared in, parallel to `nodes`.
    pub(super) node_group: Vec<Option<usize>>,
    pub(super) cur_group: Option<usize>,
    /// The cap that was hit, if any: the parser stops and renders the prefix.
    pub(super) truncated: Option<String>,
    /// Source the grammar could not read and dropped.
    pub(super) warnings: Vec<String>,
    pub(super) dir: Dir,
}

impl Graph {
    pub(super) fn new(dir: Dir) -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
            groups: Vec::new(),
            node_group: Vec::new(),
            cur_group: None,
            truncated: None,
            warnings: Vec::new(),
            dir,
        }
    }

    /// Record the first cap hit.
    pub(super) fn truncate(&mut self, cap: String) {
        if self.truncated.is_none() {
            self.truncated = Some(cap);
        }
    }

    /// Index of `id`, creating the node if new. A later declaration carrying a label
    /// overwrites the placeholder an edge created. `None` once `MAX_NODES` is reached.
    pub(super) fn node_index(
        &mut self,
        id: &str,
        label: Option<String>,
        shape: Shape,
    ) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            if let Some(label) = label {
                self.nodes[existing].label = label;
                self.nodes[existing].shape = shape;
            }
            return Some(existing);
        }
        if self.nodes.len() >= MAX_NODES {
            self.truncate(format!("node cap ({MAX_NODES}) reached"));
            return None;
        }
        self.index.insert(id.to_owned(), self.nodes.len());
        self.nodes.push(Node {
            label: label.unwrap_or_else(|| id.to_owned()),
            shape,
            sections: None,
        });
        self.node_group.push(self.cur_group);
        Some(self.nodes.len() - 1)
    }

    /// Set a node's label without disturbing its shape, creating it if new.
    pub(super) fn node_label(&mut self, id: &str, label: String) -> Option<usize> {
        if let Some(&existing) = self.index.get(id) {
            self.nodes[existing].label = label;
            return Some(existing);
        }
        self.node_index(id, Some(label), Shape::Round)
    }

    /// Record an unreadable statement; skipped once truncated (a cap-caused failure is
    /// not the statement's fault).
    pub(super) fn drop_statement(&mut self, st: &str) {
        if self.truncated.is_none() {
            self.warnings
                .push(format!("dropped, unreadable statement: \"{st}\""));
        }
    }

    /// Append an edge, or flag `truncated` when `MAX_EDGES` is reached.
    pub(super) fn push_edge(&mut self, edge: Edge) -> bool {
        if self.edges.len() >= MAX_EDGES {
            self.truncate(format!("edge cap ({MAX_EDGES}) reached"));
            return false;
        }
        self.edges.push(edge);
        true
    }
}
