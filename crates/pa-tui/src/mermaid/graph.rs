//! The shared diagram model: flowchart, state, class, and ER sources all parse into a
//! [`Graph`]; sequence diagrams have their own model. Ported from grok-mermaid 0.2.3
//! `graph.ts` (Apache-2.0; see `LICENSE-grok-mermaid`).

use std::collections::HashMap;

/// Caps that keep layout bounded; exceeding one abandons the diagram.
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
}

#[derive(Debug, Clone)]
pub(super) struct Edge {
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) label: Option<String>,
    pub(super) head_to: Head,
    pub(super) head_from: Head,
    pub(super) line: LineKind,
}

#[derive(Debug, Clone)]
pub(super) struct Group {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) parent: Option<usize>,
}

/// Extra compartment content for class and ER boxes.
#[derive(Debug, Clone, Default)]
pub(super) struct ClassInfo {
    pub(super) annotation: Option<String>,
    pub(super) attrs: Vec<String>,
    pub(super) methods: Vec<String>,
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
    /// Set when a cap was hit; the caller abandons the parse.
    pub(super) over_cap: bool,
    /// Text the lenient flowchart grammar could not read and dropped.
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
            over_cap: false,
            warnings: Vec::new(),
            dir,
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
            self.over_cap = true;
            return None;
        }
        self.index.insert(id.to_owned(), self.nodes.len());
        self.nodes.push(Node {
            label: label.unwrap_or_else(|| id.to_owned()),
            shape,
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

    /// Append an edge, or flag `over_cap` when `MAX_EDGES` is reached.
    pub(super) fn push_edge(&mut self, edge: Edge) -> bool {
        if self.edges.len() >= MAX_EDGES {
            self.over_cap = true;
            return false;
        }
        self.edges.push(edge);
        true
    }
}
