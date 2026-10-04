//! Stage 1: the online exploration (rollout) driver (TS `rollout.ts`).
//!
//! Each round the fixed interpreter picks a legal batch of eligible cells and
//! every chosen cell is resumed once by a [`Proposer`]. The grown tree is a
//! function of the seed alone: every rng fork is labelled by round, parent seq
//! and child slot ([`attempt_rng_label`]), never by an id, so the clock reaches
//! only the on-disk identity (tree id, node ids, timestamps).

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::improve::CandidateOrigin;
use crate::interpreter::{
    apply_stop_rule, assert_legal_batch, interpret_policy, project_propose_params, Cell,
    ObservationView, StopState, IMPROVE_EPS,
};
use crate::json::canonical_json;
use crate::policy::{policy_id, sha256_hex, ExplorationPolicy};
use crate::proposer::{LocalProposer, Proposer};
use crate::records::{NodeOrigin, RevealRecord, RevealTag, TreeHeaderRecord, TreeTag};
use crate::rng::{Seed, SeededRng};
use crate::store::{DreamStoreError, TreeWriter};
use crate::task::{Artifact, DynTask};
use crate::tree::{DiscoveryNode, DiscoveryTree, NodeInput};

/// The injected wall clock (milliseconds); never read ambiently in the core.
pub type DreamClock<'a> = &'a dyn Fn() -> u64;

/// One point of a best-so-far curve.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ScoreImprovement {
    pub probe: u32,
    pub score: f64,
}

/// The improvements of a best-so-far curve over `(seq, score, valid)` in order.
#[must_use]
pub fn improvements_of(nodes: impl IntoIterator<Item = (u32, f64, bool)>) -> Vec<ScoreImprovement> {
    let mut out = Vec::new();
    let mut best: Option<f64> = None;
    for (seq, score, valid) in nodes {
        if valid && best.is_none_or(|current| score > current) {
            best = Some(score);
            out.push(ScoreImprovement { probe: seq, score });
        }
    }
    out
}

/// The per-attempt fork label.
#[must_use]
pub fn attempt_rng_label(round: u32, parent_seq: u32, branch: usize) -> String {
    format!("r{round}:p{parent_seq}:b{branch}")
}

/// One rollout's options.
pub struct ExploreOptions<'a> {
    pub task: &'a dyn DynTask,
    /// Task id recorded on the header.
    pub task_id: String,
    /// Task size recorded on the header.
    pub n: Option<u32>,
    pub seed: Seed,
    /// The one seeded rng; forked per round and attempt, never drawn directly.
    pub rng: SeededRng,
    pub clock: DreamClock<'a>,
    /// Max parallelism W.
    pub workers: u32,
    /// Max online rounds.
    pub k1: u32,
    /// The store the tree and blobs are written under.
    pub dir: &'a Path,
    pub policy: ExplorationPolicy,
    pub iteration: u32,
    /// The generation attempt; the local zero-token proposer when `None`.
    pub proposer: Option<&'a mut dyn Proposer>,
    /// Override the tree id (default `<task>-s<seed>-i<iteration>-<clock>`).
    pub tree_id: Option<String>,
    /// Checked before every round: a cancelled run stops growing the tree
    /// (the in-session path; the standalone runner passes `None`).
    pub cancel: Option<&'a tokio_util::sync::CancellationToken>,
}

/// One rollout's outcome.
#[derive(Debug, Clone)]
pub struct ExploreResult {
    pub tree_id: String,
    pub tree: DiscoveryTree,
    pub rounds: u32,
    /// Revealed non-root nodes (`tree.size - 1`): the compute axis.
    pub revealed_count: u32,
    /// Nodes a child agent generated (0 on the local path).
    pub agent_generated_count: u32,
    pub best_score: f64,
    pub best_node_id: String,
    /// `seq` of the best valid node; 0 when the root is best.
    pub probes_to_best: u32,
    pub improvements: Vec<ScoreImprovement>,
    pub root_score: f64,
    pub tokens: u64,
}

/// The live decision view over a growing tree; mirrors replay's exactly.
struct LiveObservation<'a> {
    tree: &'a DiscoveryTree,
    workers: usize,
}

fn to_cell(node: &DiscoveryNode) -> Cell {
    Cell {
        node_id: node.id.clone(),
        is_root: node.parent_id.is_none(),
        parent_id: node.parent_id.clone(),
        score: node.score,
        valid: node.valid,
    }
}

impl ObservationView for LiveObservation<'_> {
    fn max_parallelism(&self) -> usize {
        self.workers
    }

    fn legal_actions(&self) -> Vec<Cell> {
        self.tree.eligible().into_iter().map(to_cell).collect()
    }

    fn best_score(&self) -> f64 {
        self.tree.best_score()
    }
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Run one online exploration and persist the tree.
///
/// # Errors
///
/// [`DreamStoreError`] when the tree cannot be written.
///
/// # Panics
///
/// Never in practice: the interpreter emits only legal batches over the tree's own cells.
#[allow(clippy::too_many_lines)] // one round loop, kept whole as in the TS
pub fn run_online_exploration(
    options: ExploreOptions<'_>,
) -> Result<ExploreResult, DreamStoreError> {
    let workers = options.workers.max(1);
    let k1 = options.k1.max(1);
    let iteration = options.iteration;
    let span = tracing::info_span!(
        "dream.explore",
        dream.policy_id = %policy_id(&options.policy),
        dream.k1 = k1,
        dream.workers = workers,
        dream.iteration = iteration,
        dream.tree_id = tracing::field::Empty,
    );
    let _entered = span.enter();
    let task = options.task;
    let mut local = LocalProposer::new(task);
    let proposer: &mut dyn Proposer = match options.proposer {
        Some(proposer) => proposer,
        None => &mut local,
    };
    let params = project_propose_params(&options.policy);
    let created_ts = (options.clock)();
    let tree_id = options.tree_id.unwrap_or_else(|| {
        format!(
            "{}-s{}-i{iteration}-{created_ts}",
            options.task_id, options.seed
        )
    });
    span.record("dream.tree_id", tree_id.as_str());
    let header = TreeHeaderRecord {
        record_type: TreeTag::Tag,
        version: 1,
        tree_id: tree_id.clone(),
        task_id: options.task_id.clone(),
        n: options.n,
        w: workers,
        seed: options.seed.clone(),
        policy_id: policy_id(&options.policy),
        iteration,
        created_ts,
    };
    let writer = TreeWriter::new(&tree_id, options.dir);
    writer.write_header(&header)?;

    let mut artifacts: HashMap<String, Artifact> = HashMap::new();
    let root_artifact = task.root(&mut options.rng.fork("root"));
    let root_eval = task.evaluate(&root_artifact);
    let root_serialized = task.serialize(&root_artifact);
    let root_ref = sha256_hex(canonical_json(&root_serialized).as_bytes());
    let mut tree = DiscoveryTree::with_root(header, root_ref, root_eval.score, root_eval.valid);
    let root = tree.all_nodes()[0].clone();
    artifacts.insert(root.id.clone(), root_artifact);
    writer.append_node(&root.to_record())?;
    writer.write_blob(root.seq, &root_serialized)?;

    let mut rounds = 0;
    let mut best_score = tree.best_score();
    let mut last_improve_round = 0;
    let mut tokens = 0;
    for round in 1..=k1 {
        if options
            .cancel
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            break;
        }
        let cells = {
            let view = LiveObservation {
                tree: &tree,
                workers: usize::try_from(workers).unwrap_or(usize::MAX),
            };
            let cells = interpret_policy(&options.policy, &view);
            if !cells.is_empty() {
                if let Err(error) = assert_legal_batch(&view, &cells) {
                    panic!("illegal rollout batch: {error}");
                }
            }
            cells
        };
        if cells.is_empty() {
            break;
        }
        rounds = round;
        let round_span = tracing::info_span!(
            "dream.round",
            dream.round = round,
            dream.batch_size = cells.len(),
            dream.revealed_count = tree.size() - 1,
            dream.best_score = best_score,
        );
        let round_entered = round_span.enter();
        let mut revealed_this_round = Vec::with_capacity(cells.len());
        for cell in &cells {
            let (parent_seq, branch) = tree.node_by_id(&cell.node_id).map_or_else(
                || panic!("attempt on unknown cell {}", cell.node_id),
                |parent| (parent.seq, tree.children(&parent.id).len()),
            );
            let mut rng = options
                .rng
                .fork(&attempt_rng_label(round, parent_seq, branch));
            let outcome =
                proposer.propose(artifacts.get(&cell.node_id), &params, &mut rng, round)?;
            let attempt_span = tracing::info_span!(
                "dream.attempt",
                dream.node_id = %format!("{tree_id}-n{}", tree.size()),
                dream.parent_id = %cell.node_id,
                dream.task = %options.task_id,
                dream.valid = tracing::field::Empty,
                dream.score = tracing::field::Empty,
                dream.tokens = tracing::field::Empty,
                dream.origin = tracing::field::Empty,
                dream.fail_class = tracing::field::Empty,
            );
            let _attempt = attempt_span.enter();
            let evaluation = task.evaluate(&outcome.artifact);
            let serialized = task.serialize(&outcome.artifact);
            let artifact_ref = sha256_hex(canonical_json(&serialized).as_bytes());
            let origin = match outcome.origin {
                Some(CandidateOrigin::Llm) => NodeOrigin::Llm,
                Some(CandidateOrigin::Local) | None => NodeOrigin::Local,
            };
            let node = tree
                .add_node(NodeInput {
                    parent_id: cell.node_id.clone(),
                    round,
                    score: evaluation.score,
                    valid: evaluation.valid,
                    fail_class: evaluation
                        .fail_class
                        .map(|class| class.as_str().to_string()),
                    origin,
                    artifact_ref,
                    tokens: outcome.tokens,
                    ts: (options.clock)(),
                })
                .unwrap_or_else(|error| panic!("rollout tree: {error}"))
                .clone();
            artifacts.insert(node.id.clone(), outcome.artifact);
            writer.append_node(&node.to_record())?;
            writer.write_blob(node.seq, &serialized)?;
            attempt_span.record("dream.valid", node.valid);
            attempt_span.record("dream.score", node.score);
            attempt_span.record("dream.tokens", node.tokens);
            attempt_span.record("dream.origin", node.origin.as_str());
            if let Some(fail_class) = &node.fail_class {
                attempt_span.record("dream.fail_class", fail_class.as_str());
            }
            revealed_this_round.push(node.id.clone());
            tokens += outcome.tokens;
        }
        drop(round_entered);
        writer.append_reveal(&RevealRecord {
            record_type: RevealTag::Tag,
            round,
            ids: revealed_this_round,
        })?;
        let round_best = tree.best_score();
        if round_best > best_score + IMPROVE_EPS {
            best_score = round_best;
            last_improve_round = round;
        }
        let stop = StopState {
            round,
            best_score,
            rounds_since_improvement: round - last_improve_round,
        };
        if apply_stop_rule(&options.policy, &stop) {
            break;
        }
    }

    let (best_score, best_node_id) = tree
        .best_node()
        .map_or((root.score, root.id.clone()), |node| {
            (node.score, node.id.clone())
        });
    let improvements = improvements_of(
        tree.all_nodes()
            .iter()
            .map(|node| (node.seq, node.score, node.valid)),
    );
    Ok(ExploreResult {
        tree_id,
        rounds,
        revealed_count: to_u32(tree.size() - 1),
        agent_generated_count: to_u32(tree.origin_counts().llm),
        best_score,
        best_node_id,
        probes_to_best: improvements.last().map_or(0, |point| point.probe),
        improvements,
        root_score: root.score,
        tokens,
        tree,
    })
}
