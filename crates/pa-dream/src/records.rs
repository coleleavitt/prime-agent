//! The JSONL wire records of a discovery tree (TS `types.ts`).
//!
//! A tree file is one header line, one node line per generation+evaluation
//! attempt, and one reveal line per online round. Node lines hold scalars only;
//! the artifact lives in a sibling blob keyed by `seq`. Field order is the TS
//! object-literal order, so a Rust-written line is byte-identical to the TS one.

use serde::{Deserialize, Serialize};

use crate::rng::Seed;

/// Who generated a node's artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeOrigin {
    /// The task's seeded root.
    Root,
    /// The zero-token local proposer (on the LLM path: a fallback).
    Local,
    /// A child agent's accepted candidate.
    Llm,
}

impl NodeOrigin {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Local => "local",
            Self::Llm => "llm",
        }
    }

    /// The origin named by `text`, if it is a known one.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "root" => Some(Self::Root),
            "local" => Some(Self::Local),
            "llm" => Some(Self::Llm),
            _ => None,
        }
    }
}

macro_rules! tag {
    ($name:ident, $text:literal) => {
        /// The record's `type` discriminator.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
        pub enum $name {
            #[default]
            #[serde(rename = $text)]
            Tag,
        }
    };
}

tag!(TreeTag, "tree");
tag!(NodeTag, "node");
tag!(RevealTag, "reveal");

/// Line 0 of a tree file: the run metadata every node shares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeHeaderRecord {
    #[serde(rename = "type")]
    pub record_type: TreeTag,
    pub version: u32,
    pub tree_id: String,
    pub task_id: String,
    /// Task size parameter (circle count, bin count), when the task takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Max parallelism W the rollout used.
    pub w: u32,
    pub seed: Seed,
    pub policy_id: String,
    pub iteration: u32,
    pub created_ts: u64,
}

/// One node of a discovery tree. `id` is `<treeId>-n<seq>`; the root is seq 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRecord {
    #[serde(rename = "type")]
    pub record_type: NodeTag,
    pub id: String,
    pub parent_id: Option<String>,
    /// Child-slot index within the parent, in creation order.
    pub branch: u32,
    pub seq: u32,
    pub round: u32,
    /// Finite; 0 when invalid.
    pub score: f64,
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_class: Option<String>,
    /// Provenance as recorded. A line without it predates provenance; an
    /// unknown value is kept as written. Read it through [`NodeRecord::origin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub artifact_ref: String,
    pub tokens: u64,
    pub ts: u64,
}

impl NodeRecord {
    /// The node's origin with the legacy default: `root` for the root, `local` otherwise.
    #[must_use]
    pub fn origin(&self) -> NodeOrigin {
        self.origin
            .as_deref()
            .and_then(NodeOrigin::from_name)
            .unwrap_or(if self.parent_id.is_none() {
                NodeOrigin::Root
            } else {
                NodeOrigin::Local
            })
    }
}

/// One online round's reveal set (informational; replay never reads it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevealRecord {
    #[serde(rename = "type")]
    pub record_type: RevealTag,
    pub round: u32,
    pub ids: Vec<String>,
}

/// Any line of a tree file.
#[derive(Debug, Clone, PartialEq)]
pub enum TreeRecord {
    Header(TreeHeaderRecord),
    Node(NodeRecord),
    Reveal(RevealRecord),
}
