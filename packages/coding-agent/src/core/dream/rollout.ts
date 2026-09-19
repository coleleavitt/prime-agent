/**
 * Stage 1 of Dream-RSI: the online exploration (rollout) driver.
 *
 * `runOnlineExploration` grows a `DiscoveryTree` over a scored task. Each round
 * the fixed policy interpreter picks a batch of eligible cells (A(T) = {root}
 * union {leaves}) and every chosen cell is resumed once by a `Proposer`,
 * producing a scored child. The batch is legal by construction — distinct cells,
 * at most `workers`, never a node together with its child — and `assertLegalBatch`
 * re-checks it defensively.
 *
 * The grown tree is a function of the seed alone: the only randomness is the
 * forked rng, and every fork label (`root`, `select:<round>`, and the per-attempt
 * `attemptRngLabel`) is built from round, parent seq and child slot, never from
 * an id. The clock reaches only the on-disk identity — `treeId`
 * (`<task>-s<seed>-i<iteration>-<ms>`), the node ids `<treeId>-n<seq>` and the
 * `createdTs`/`ts` fields — so two rollouts of one seed at different wall times
 * produce the same scores, shapes and reveal order under different ids. With
 * the default local proposer a rollout spends zero model tokens and touches no
 * network.
 *
 * The exploration policy is DATA, not code (see `policy.ts`): this driver only
 * ever hands it to `interpreter.ts`. The LLM proposer is an ASYNC path
 * (`proposer.ts`/`llm.ts`, built by the in-session lane); this synchronous
 * driver refuses the `useLlmProposer` flag rather than await it, so the default
 * path stays token-free. The async sibling `runOnlineExplorationWithAgent` lives
 * in `llm.ts` and shares every pure helper here.
 */

import { withSpan } from "@earendil-works/pi-ai";
import { canonicalJson, sha256 } from "../ravo/canonical-json.js";
import { applyStopRule, interpretPolicy, projectProposeParams } from "./interpreter.js";
import type { Cell, ObservationView, RevealedNode } from "./observation.js";
import { assertLegalBatch } from "./observation.js";
import { type ExplorationPolicy, policyId } from "./policy.js";
import { createLocalProposer, type ProposeOutcome, type Proposer } from "./proposer.js";
import type { SeededRng } from "./rng.js";
import { TreeWriter } from "./store.js";
import type { DreamTaskId, ProposeParams, ScoredTask } from "./task.js";
import { createTree, type DiscoveryNode, type DiscoveryTree, nodeRecordOf } from "./tree.js";
import type { DreamClock, TreeHeaderRecord } from "./types.js";

/** A round improves the best score only when it beats it by more than this. Shared by both drivers. */
export const IMPROVE_EPS = 1e-12;

export interface ExploreOptions {
	task: ScoredTask<unknown>;
	/** Task id for the tree header; defaults to `task.id`. */
	taskId?: DreamTaskId;
	/** Task size parameter (e.g. circle count) recorded on the header. */
	n?: number;
	seed: number | string;
	/** The one seeded RNG; forked per round and per attempt, never used ambiently. */
	rng: SeededRng;
	/** The one injected clock; the only clock-derived id is `treeId`. */
	clock: DreamClock;
	/** Max parallelism W: cells per round. */
	workers: number;
	/** Max online rounds. */
	k1: number;
	/** Dream store directory the tree and its blobs are written under. */
	dir: string;
	/** The typed, serializable policy the fixed interpreter drives. */
	policy: ExplorationPolicy;
	/** Iteration index within a loop (0 for a standalone rollout). */
	iteration: number;
	/** Generation attempt; defaults to the local zero-token proposer. */
	proposer?: Proposer<unknown>;
	/** When set, this sync driver refuses (the LLM proposer is async and token-spending). */
	useLlmProposer?: boolean;
	/** Override the tree id (defaults to a deterministic id from task/seed/iteration/clock). */
	treeId?: string;
	/**
	 * Span name for this rollout; defaults to `dream.explore`. The loop passes
	 * `dream.redeploy` for the rollouts that resume the pool with a chosen policy,
	 * so the observability rows match without double-wrapping.
	 */
	spanName?: string;
}

export interface ExploreResult {
	treeId: string;
	tree: DiscoveryTree;
	rounds: number;
	/** Revealed non-root nodes (`tree.size - 1`): evaluated attempts, the compute axis. */
	revealedCount: number;
	/**
	 * Revealed nodes whose candidate a child agent generated (`origin: "llm"`).
	 * 0 on the local path; on the LLM path `revealedCount - agentGeneratedCount`
	 * is the number of attempts the local mutator stood in for.
	 */
	agentGeneratedCount: number;
	bestScore: number;
	bestNodeId: string;
	rootScore: number;
	tokens: number;
}

/** Thrown when the synchronous rollout is asked for the LLM proposer it cannot run. */
export class LlmProposerUnavailableError extends Error {
	constructor() {
		super(
			"LLM proposer/dreamer require an in-session agent handler and are unavailable from the standalone CLI; the default local proposer runs at zero tokens.",
		);
		this.name = "LlmProposerUnavailableError";
	}
}

export function toCell(node: DiscoveryNode): Cell {
	return {
		nodeId: node.id,
		isRoot: node.parentId === null,
		parentId: node.parentId,
		score: node.score,
		valid: node.valid,
	};
}

export function toRevealed(node: DiscoveryNode): RevealedNode {
	return {
		nodeId: node.id,
		parentId: node.parentId,
		isRoot: node.parentId === null,
		score: node.score,
		valid: node.valid,
		seq: node.seq,
		round: node.round,
	};
}

/** The live decision view over a growing tree; mirrors the replay view exactly. */
export function liveObservation(tree: DiscoveryTree, workers: number, round: number): ObservationView {
	return {
		maxParallelism: workers,
		round,
		observed: () => tree.allNodes().map(toRevealed),
		legalActions: () => tree.eligible().map(toCell),
		legalRoots: () =>
			tree
				.eligible()
				.filter((node) => node.parentId === null)
				.map(toCell),
		bestScore: () => tree.bestScore(),
		revealedNonRootCount: () => tree.size - 1,
	};
}

/**
 * The mutable state one rollout grows, shared by the synchronous driver here and
 * the asynchronous LLM driver in `llm.ts`. It carries the built tree, its writer,
 * the artifact cache, and the frozen per-rollout knobs, so both drivers differ
 * only in HOW they obtain each attempt's `ProposeOutcome` (sync `Proposer` vs.
 * awaited `AsyncProposer`); everything else — ids, span attrs, persistence and
 * the stop rule — flows through the identical helpers below.
 */
export interface RolloutState {
	task: ScoredTask<unknown>;
	taskId: DreamTaskId;
	tree: DiscoveryTree;
	writer: TreeWriter;
	/** Live artifacts keyed by node id, so a child can resume its parent's artifact. */
	artifacts: Map<string, unknown>;
	treeId: string;
	workers: number;
	k1: number;
	iteration: number;
	params: ProposeParams;
	policy: ExplorationPolicy;
	clock: DreamClock;
	rng: SeededRng;
}

/** Scalar span attributes for the outer `dream.explore`/`dream.redeploy` span (tree id is set once known). */
export function exploreSpanAttrs(options: ExploreOptions): Record<string, string | number> {
	return {
		"dream.policy_id": policyId(options.policy),
		"dream.k1": Math.max(1, Math.trunc(options.k1)),
		"dream.workers": Math.max(1, Math.trunc(options.workers)),
		"dream.iteration": Math.max(0, Math.trunc(options.iteration)),
	};
}

/**
 * Write the tree header, root node and root blob, build the in-memory tree and
 * return the shared `RolloutState`. Deterministic: the only clock-derived id is
 * `treeId`, and the only randomness is the labelled `root` fork.
 */
export function beginRollout(options: ExploreOptions): RolloutState {
	const { task, rng, clock, policy, dir } = options;
	const taskId = options.taskId ?? task.id;
	const workers = Math.max(1, Math.trunc(options.workers));
	const k1 = Math.max(1, Math.trunc(options.k1));
	const iteration = Math.max(0, Math.trunc(options.iteration));
	const params = projectProposeParams(policy);
	const pid = policyId(policy);
	const createdTs = clock();
	const treeId = options.treeId ?? `${taskId}-s${options.seed}-i${iteration}-${createdTs}`;
	const header: TreeHeaderRecord = {
		type: "tree",
		version: 1,
		treeId,
		taskId,
		...(options.n !== undefined ? { n: options.n } : {}),
		w: workers,
		seed: options.seed,
		policyId: pid,
		iteration,
		createdTs,
	};
	const writer = new TreeWriter(treeId, dir);
	writer.writeHeader(header);

	const artifacts = new Map<string, unknown>();
	const rootArtifact = task.root(rng.fork("root"));
	const rootEval = task.evaluate(rootArtifact);
	const rootSerialized = task.serialize(rootArtifact);
	const rootRef = sha256(canonicalJson(rootSerialized));
	const tree = createTree(header, rootRef, rootEval.score, rootEval.valid);
	const rootNode = tree.nodeById(tree.rootId)!;
	artifacts.set(rootNode.id, rootArtifact);
	writer.appendNode(nodeRecordOf(rootNode));
	writer.writeBlob(rootNode.seq, rootSerialized);

	return { task, taskId, tree, writer, artifacts, treeId, workers, k1, iteration, params, policy, clock, rng };
}

/**
 * The per-attempt rng fork label. (round, parent seq, child slot) is unique per
 * attempt within a tree — a cell is selected at most once per round — and it
 * carries no id, so the stream depends on the seed and iteration alone. A label
 * built from the node id would embed the tree id's clock milliseconds and make
 * every proposal, and so the whole tree, differ from run to run.
 */
export function attemptRngLabel(round: number, parentSeq: number, branch: number): string {
	return `r${round}:p${parentSeq}:b${branch}`;
}

/** The rng one attempt resumes `cell` with; shared by both drivers so their streams are identical. */
export function attemptRng(state: RolloutState, cell: Cell, round: number): SeededRng {
	const parent = state.tree.nodeById(cell.nodeId);
	if (!parent) throw new Error(`attempt on unknown cell ${cell.nodeId}`);
	const branch = state.tree.children(cell.nodeId).length;
	return state.rng.fork(attemptRngLabel(round, parent.seq, branch));
}

/**
 * Interpret the policy over the live tree and return this round's legal batch.
 * An empty batch signals the round loop to stop. Uses the `select:${round}` fork
 * label, so the batch is identical for both drivers.
 */
export function selectRoundCells(state: RolloutState, round: number): Cell[] {
	const view = liveObservation(state.tree, state.workers, round);
	const cells = interpretPolicy(state.policy, view, state.rng.fork(`select:${round}`));
	if (cells.length === 0) return [];
	assertLegalBatch(view, cells);
	return cells;
}

/**
 * Commit one already-produced attempt outcome to the tree inside a `dream.attempt`
 * span: evaluate, serialize, append the node and its blob. The node-id attribute
 * is read from `tree.size` BEFORE the node is added, and the local proposer opens
 * no span of its own, so the emitted span tree is identical to a fully sync
 * rollout. The outcome's `origin` (default `local`) is persisted on the node and
 * set on the span as `dream.origin`. Returns the created node and the tokens the
 * outcome reported.
 */
export function commitAttempt(
	state: RolloutState,
	cell: Cell,
	round: number,
	outcome: ProposeOutcome<unknown>,
): { node: DiscoveryNode; tokens: number } {
	return withSpan(
		"dream.attempt",
		{
			"dream.node_id": `${state.treeId}-n${state.tree.size}`,
			"dream.parent_id": cell.nodeId,
			"dream.task": state.taskId,
		},
		(span) => {
			const evaluation = state.task.evaluate(outcome.artifact);
			const serialized = state.task.serialize(outcome.artifact);
			const artifactRef = sha256(canonicalJson(serialized));
			const node = state.tree.addNode({
				parentId: cell.nodeId,
				round,
				score: evaluation.score,
				valid: evaluation.valid,
				...(evaluation.failClass !== undefined ? { failClass: evaluation.failClass } : {}),
				origin: outcome.origin ?? "local",
				artifactRef,
				tokens: outcome.tokens,
				ts: state.clock(),
			});
			state.artifacts.set(node.id, outcome.artifact);
			state.writer.appendNode(nodeRecordOf(node));
			state.writer.writeBlob(node.seq, serialized);
			span.setAttributes({
				"dream.valid": node.valid,
				"dream.score": node.score,
				"dream.tokens": node.tokens,
				"dream.origin": node.origin,
				...(node.failClass !== undefined ? { "dream.fail_class": node.failClass } : {}),
			});
			return { node, tokens: outcome.tokens };
		},
	);
}

/** Whether the rollout should stop after this round, per the policy's stop rule. */
export function applyRoundStop(
	state: RolloutState,
	round: number,
	bestScore: number,
	lastImproveRound: number,
): boolean {
	return applyStopRule(state.policy, {
		round,
		bestScore,
		roundsSinceImprovement: round - lastImproveRound,
		revealedNonRootCount: state.tree.size - 1,
	});
}

/** Summarize the grown tree into an `ExploreResult`. */
export function finishRollout(state: RolloutState, rounds: number, tokens: number): ExploreResult {
	const best = state.tree.bestNode() ?? state.tree.nodeById(state.tree.rootId)!;
	return {
		treeId: state.treeId,
		tree: state.tree,
		rounds,
		revealedCount: state.tree.size - 1,
		agentGeneratedCount: state.tree.originCounts().llm,
		bestScore: best.score,
		bestNodeId: best.id,
		rootScore: state.tree.nodeById(state.tree.rootId)!.score,
		tokens,
	};
}

/**
 * Run one online exploration and return the grown tree. Synchronous and
 * zero-token with the default local proposer; the `useLlmProposer` flag is
 * refused here because the LLM proposer is asynchronous. The async sibling
 * `runOnlineExplorationWithAgent` (in `llm.ts`) drives the identical helpers.
 */
export function runOnlineExploration(options: ExploreOptions): ExploreResult {
	if (options.useLlmProposer) throw new LlmProposerUnavailableError();
	const proposer = options.proposer ?? createLocalProposer(options.task);
	return withSpan(options.spanName ?? "dream.explore", exploreSpanAttrs(options), (span) => {
		const state = beginRollout(options);
		span.setAttributes({ "dream.tree_id": state.treeId });

		let rounds = 0;
		let bestScore = state.tree.bestScore();
		let lastImproveRound = 0;
		let tokens = 0;

		for (let round = 1; round <= state.k1; round++) {
			const cells = selectRoundCells(state, round);
			if (cells.length === 0) break;
			rounds = round;
			const revealedThisRound: string[] = [];
			const revealedBefore = state.tree.size - 1;
			withSpan(
				"dream.round",
				{
					"dream.round": round,
					"dream.batch_size": cells.length,
					"dream.revealed_count": revealedBefore,
					"dream.best_score": bestScore,
				},
				() => {
					for (const cell of cells) {
						const parentArtifact = state.artifacts.get(cell.nodeId) ?? null;
						const outcome = proposer.propose(parentArtifact, state.params, attemptRng(state, cell, round), round);
						const committed = commitAttempt(state, cell, round, outcome);
						revealedThisRound.push(committed.node.id);
						tokens += committed.tokens;
					}
				},
			);
			state.writer.appendReveal({ type: "reveal", round, ids: revealedThisRound });
			const roundBest = state.tree.bestScore();
			if (roundBest > bestScore + IMPROVE_EPS) {
				bestScore = roundBest;
				lastImproveRound = round;
			}
			if (applyRoundStop(state, round, bestScore, lastImproveRound)) break;
		}

		return finishRollout(state, rounds, tokens);
	});
}
