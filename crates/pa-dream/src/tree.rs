//! The pure in-memory discovery tree (TS `tree.ts`).
//!
//! The eligible set A(T) is {root} ∪ {leaves}: a node leaves A(T) the moment
//! it gains a child, so every non-root node ends with at most one child while
//! the root accumulates many — exactly the shape replay relies on.

use std::collections::HashMap;

use crate::records::{NodeOrigin, NodeRecord, NodeTag, TreeHeaderRecord};

/// One node of a growing tree.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub branch: u32,
    pub seq: u32,
    pub round: u32,
    pub score: f64,
    pub valid: bool,
    pub fail_class: Option<String>,
    pub origin: NodeOrigin,
    pub artifact_ref: String,
    pub tokens: u64,
    pub ts: u64,
}

impl DiscoveryNode {
    /// The persisted line: scalar fields only, `origin` always written.
    #[must_use]
    pub fn to_record(&self) -> NodeRecord {
        NodeRecord {
            record_type: NodeTag::Tag,
            id: self.id.clone(),
            parent_id: self.parent_id.clone(),
            branch: self.branch,
            seq: self.seq,
            round: self.round,
            score: self.score,
            valid: self.valid,
            fail_class: self.fail_class.clone(),
            origin: Some(self.origin.as_str().to_string()),
            artifact_ref: self.artifact_ref.clone(),
            tokens: self.tokens,
            ts: self.ts,
        }
    }
}

/// What the caller supplies when appending a node; `seq`/`id`/`branch` are assigned.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeInput {
    pub parent_id: String,
    pub round: u32,
    pub score: f64,
    pub valid: bool,
    pub fail_class: Option<String>,
    pub origin: NodeOrigin,
    pub artifact_ref: String,
    pub tokens: u64,
    pub ts: u64,
}

/// Node counts by origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OriginCounts {
    pub root: usize,
    pub local: usize,
    pub llm: usize,
}

/// Why records do not form a tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("a discovery tree needs exactly one root, found {0}")]
    RootCount(usize),
    #[error("unknown parent {0}")]
    UnknownParent(String),
}

fn finite_score(score: f64) -> f64 {
    if score.is_finite() {
        score
    } else {
        0.0
    }
}

/// A growing discovery tree. Nodes are held in seq order.
#[derive(Debug, Clone)]
pub struct DiscoveryTree {
    pub header: TreeHeaderRecord,
    pub root_id: String,
    nodes: Vec<DiscoveryNode>,
    by_id: HashMap<String, usize>,
    children: HashMap<String, Vec<usize>>,
    next_seq: u32,
}

impl DiscoveryTree {
    /// Build a tree from nodes (any order); children come back in seq order.
    ///
    /// # Errors
    ///
    /// [`TreeError::RootCount`] unless exactly one node has no parent.
    pub fn new(header: TreeHeaderRecord, mut nodes: Vec<DiscoveryNode>) -> Result<Self, TreeError> {
        let roots = nodes.iter().filter(|node| node.parent_id.is_none()).count();
        if roots != 1 {
            return Err(TreeError::RootCount(roots));
        }
        nodes.sort_by_key(|node| node.seq);
        let root_id = nodes
            .iter()
            .find(|node| node.parent_id.is_none())
            .map(|node| node.id.clone())
            .unwrap_or_default();
        let mut tree = Self {
            header,
            root_id,
            nodes: Vec::with_capacity(nodes.len()),
            by_id: HashMap::new(),
            children: HashMap::new(),
            next_seq: 0,
        };
        for node in nodes {
            tree.insert(node);
        }
        Ok(tree)
    }

    /// A fresh tree with a single root node (seq 0).
    #[must_use]
    pub fn with_root(
        header: TreeHeaderRecord,
        root_ref: String,
        root_score: f64,
        root_valid: bool,
    ) -> Self {
        let root = DiscoveryNode {
            id: format!("{}-n0", header.tree_id),
            parent_id: None,
            branch: 0,
            seq: 0,
            round: 0,
            score: finite_score(root_score),
            valid: root_valid,
            fail_class: None,
            origin: NodeOrigin::Root,
            artifact_ref: root_ref,
            tokens: 0,
            ts: header.created_ts,
        };
        let root_id = root.id.clone();
        let mut tree = Self {
            header,
            root_id,
            nodes: Vec::new(),
            by_id: HashMap::new(),
            children: HashMap::new(),
            next_seq: 0,
        };
        tree.insert(root);
        tree
    }

    /// Rebuild a tree from persisted node records.
    ///
    /// # Errors
    ///
    /// [`TreeError::RootCount`] unless exactly one record has no parent.
    pub fn from_records(
        header: TreeHeaderRecord,
        records: &[NodeRecord],
    ) -> Result<Self, TreeError> {
        let nodes = records
            .iter()
            .map(|record| DiscoveryNode {
                id: record.id.clone(),
                parent_id: record.parent_id.clone(),
                branch: record.branch,
                seq: record.seq,
                round: record.round,
                score: finite_score(record.score),
                valid: record.valid,
                fail_class: record.fail_class.clone(),
                origin: record.origin(),
                artifact_ref: record.artifact_ref.clone(),
                tokens: record.tokens,
                ts: record.ts,
            })
            .collect();
        Self::new(header, nodes)
    }

    fn insert(&mut self, node: DiscoveryNode) {
        let index = self.nodes.len();
        if let Some(parent) = &node.parent_id {
            self.children.entry(parent.clone()).or_default().push(index);
        }
        self.next_seq = self.next_seq.max(node.seq + 1);
        self.by_id.insert(node.id.clone(), index);
        self.nodes.push(node);
    }

    /// The node with `id`.
    #[must_use]
    pub fn node_by_id(&self, id: &str) -> Option<&DiscoveryNode> {
        self.by_id.get(id).map(|index| &self.nodes[*index])
    }

    /// Children of a node, in seq order.
    #[must_use]
    pub fn children(&self, id: &str) -> Vec<&DiscoveryNode> {
        self.children
            .get(id)
            .map(|indices| indices.iter().map(|index| &self.nodes[*index]).collect())
            .unwrap_or_default()
    }

    fn child_count(&self, id: &str) -> usize {
        self.children.get(id).map_or(0, Vec::len)
    }

    /// Nodes with no children, in seq order.
    #[must_use]
    pub fn leaves(&self) -> Vec<&DiscoveryNode> {
        self.nodes
            .iter()
            .filter(|node| self.child_count(&node.id) == 0)
            .collect()
    }

    /// The eligible starting points A(T) = {root} ∪ {leaves}, in seq order.
    #[must_use]
    pub fn eligible(&self) -> Vec<&DiscoveryNode> {
        self.nodes
            .iter()
            .filter(|node| node.parent_id.is_none() || self.child_count(&node.id) == 0)
            .collect()
    }

    /// Every node, in seq order.
    #[must_use]
    pub fn all_nodes(&self) -> &[DiscoveryNode] {
        &self.nodes
    }

    /// Node count.
    #[must_use]
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// Node counts by origin.
    #[must_use]
    pub fn origin_counts(&self) -> OriginCounts {
        let mut counts = OriginCounts::default();
        for node in &self.nodes {
            match node.origin {
                NodeOrigin::Root => counts.root += 1,
                NodeOrigin::Local => counts.local += 1,
                NodeOrigin::Llm => counts.llm += 1,
            }
        }
        counts
    }

    /// Max score over valid nodes; 0 when none is valid.
    #[must_use]
    pub fn best_score(&self) -> f64 {
        self.best_node().map_or(0.0, |node| node.score)
    }

    /// The first highest-scoring valid node, in seq order.
    #[must_use]
    pub fn best_node(&self) -> Option<&DiscoveryNode> {
        let mut best: Option<&DiscoveryNode> = None;
        for node in &self.nodes {
            if node.valid && best.is_none_or(|current| node.score > current.score) {
                best = Some(node);
            }
        }
        best
    }

    /// Append a child of `input.parent_id`, assigning seq, id and child slot.
    ///
    /// # Errors
    ///
    /// [`TreeError::UnknownParent`] when the parent is not in the tree.
    pub fn add_node(&mut self, input: NodeInput) -> Result<&DiscoveryNode, TreeError> {
        if !self.by_id.contains_key(&input.parent_id) {
            return Err(TreeError::UnknownParent(input.parent_id));
        }
        let seq = self.next_seq;
        let branch = u32::try_from(self.child_count(&input.parent_id)).unwrap_or(u32::MAX);
        self.insert(DiscoveryNode {
            id: format!("{}-n{seq}", self.header.tree_id),
            parent_id: Some(input.parent_id),
            branch,
            seq,
            round: input.round,
            score: finite_score(input.score),
            valid: input.valid,
            fail_class: input.fail_class,
            origin: input.origin,
            artifact_ref: input.artifact_ref,
            tokens: input.tokens,
            ts: input.ts,
        });
        Ok(&self.nodes[self.nodes.len() - 1])
    }

    /// The node records, in seq order.
    #[must_use]
    pub fn to_node_records(&self) -> Vec<NodeRecord> {
        self.nodes.iter().map(DiscoveryNode::to_record).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::TreeTag;
    use crate::rng::Seed;

    fn header() -> TreeHeaderRecord {
        TreeHeaderRecord {
            record_type: TreeTag::Tag,
            version: 1,
            tree_id: "t1".to_string(),
            task_id: "circle-packing".to_string(),
            n: None,
            w: 2,
            seed: Seed::Number(1),
            policy_id: "p".to_string(),
            iteration: 0,
            created_ts: 0,
        }
    }

    fn legacy(id: &str, parent: Option<&str>, seq: u32, score: f64) -> NodeRecord {
        NodeRecord {
            record_type: NodeTag::Tag,
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            branch: 0,
            seq,
            round: 0,
            score,
            valid: true,
            fail_class: None,
            origin: None,
            artifact_ref: "ref".to_string(),
            tokens: 0,
            ts: 0,
        }
    }

    fn input(parent: &str, score: f64, origin: NodeOrigin) -> NodeInput {
        NodeInput {
            parent_id: parent.to_string(),
            round: 1,
            score,
            valid: true,
            fail_class: None,
            origin,
            artifact_ref: "ref".to_string(),
            tokens: 0,
            ts: 1,
        }
    }

    fn ids(nodes: &[&DiscoveryNode]) -> Vec<String> {
        nodes.iter().map(|node| node.id.clone()).collect()
    }

    #[test]
    fn rebuilds_structure_eligibility_and_best_and_drops_a_parent_from_a_t() {
        let records = [
            legacy("t1-n0", None, 0, 0.3),
            legacy("t1-n1", Some("t1-n0"), 1, 0.5),
            legacy("t1-n2", Some("t1-n1"), 2, 0.7),
        ];
        let mut tree = DiscoveryTree::from_records(header(), &records).expect("tree");
        assert_eq!(ids(&tree.children("t1-n0")), ["t1-n1"]);
        assert_eq!(ids(&tree.eligible()), ["t1-n0", "t1-n2"]);
        assert_eq!(tree.best_node().map(|node| node.id.as_str()), Some("t1-n2"));
        assert_eq!(
            tree.all_nodes()
                .iter()
                .map(|node| node.origin)
                .collect::<Vec<_>>(),
            [NodeOrigin::Root, NodeOrigin::Local, NodeOrigin::Local]
        );
        let added = tree
            .add_node(input("t1-n2", 0.9, NodeOrigin::Local))
            .expect("added")
            .clone();
        assert_eq!(
            (added.id.as_str(), added.seq, added.branch),
            ("t1-n3", 3, 0)
        );
        assert_eq!(ids(&tree.leaves()), ["t1-n3"]);
        assert_eq!(ids(&tree.eligible()), ["t1-n0", "t1-n3"]);
        let two_roots = [legacy("t1-n0", None, 0, 0.0), legacy("t1-nX", None, 1, 0.0)];
        assert_eq!(
            DiscoveryTree::from_records(header(), &two_roots).err(),
            Some(TreeError::RootCount(2))
        );
    }

    #[test]
    fn assigns_seq_branch_and_origin_and_writes_origin_on_every_record() {
        let mut tree = DiscoveryTree::with_root(header(), "root".to_string(), 0.2, true);
        tree.add_node(input("t1-n0", f64::INFINITY, NodeOrigin::Local))
            .expect("local");
        tree.add_node(input("t1-n0", 0.5, NodeOrigin::Llm))
            .expect("llm");
        assert_eq!(
            tree.origin_counts(),
            OriginCounts {
                root: 1,
                local: 1,
                llm: 1
            }
        );
        let records = tree.to_node_records();
        assert_eq!(records[1].score.to_bits(), 0.0f64.to_bits());
        assert_eq!((records[2].branch, records[2].seq), (1, 2));
        assert_eq!(
            crate::json::stringify(&records[2]),
            r#"{"type":"node","id":"t1-n2","parentId":"t1-n0","branch":1,"seq":2,"round":1,"score":0.5,"valid":true,"origin":"llm","artifactRef":"ref","tokens":0,"ts":1}"#
        );
    }
}
