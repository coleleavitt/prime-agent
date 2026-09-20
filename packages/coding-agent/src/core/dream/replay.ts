/**
 * Stage 2: the frozen replay simulator.
 *
 * A recorded tree is a deterministic, zero-execution simulator. An alternative
 * policy re-walks it: each round it builds a `ReplayObservation` whose legal
 * actions are {root} ∪ {revealed non-root leaves of the revealed subtree}, runs
 * the SAME interpreter that drove the online rollout, and reveals — never
 * generates — the recorded child of each selected cell. The root reveals its
 * earliest-seq unrevealed child; a non-root leaf reveals its single unrevealed
 * child. A legally-selected cell with no unrevealed recorded child is OUT OF
 * SUPPORT: it reveals nothing but is CHARGED as a probe (`selectedCells`,
 * `bestSoFar`), which is the off-policy penalty for a policy that would explore
 * branches the online run never recorded. Improvement detection uses the same
 * `IMPROVE_EPS` as the online drivers, so the patience rule fires on the same
 * round online and in replay.
 *
 * A round whose every selected cell is out of support reveals nothing and leaves
 * the observation unchanged, so the deterministic interpreter selects the same
 * dead cells next round: from there the replay only burns cells and rounds until
 * the stop rule fires or `k2` is reached, and with `k2 > k1` that can carry
 * `rounds` past the online cap. Those rounds are counted on purpose: they are the
 * off-policy penalty a CANDIDATE pays for a walk the recording cannot support
 * (the chain-versus-full-batch case in `test/dream-improve.test.ts` depends on
 * it). They are never a penalty on the incumbent, because the selection
 * (`improve.ts` `measurePool`) scores only trees the incumbent replays with zero
 * out-of-support cells, and a policy replaying the tree it grew reveals it
 * exactly and stops at `allRevealed`.
 *
 * Replay uses NO rng and no clock, so thousands of policies are scored at zero
 * cost and every result is byte-reproducible. `simulatePolicy` opens no span so
 * it can run inside a tight dreaming loop; `simulatePolicyWithSpan` is the
 * standalone-subcommand entry that reports one `dream.replay` root span.
 */

import { withSpan } from "@earendil-works/pi-ai";
import { applyStopRule, IMPROVE_EPS, interpretPolicy, type StopState } from "./interpreter.js";
import { computeObjective, DEFAULT_OBJECTIVE, poolScoreScale, type ReplayObjectiveConfig } from "./objective.js";
import { assertLegalBatch, type Cell, type ObservationView, type RevealedNode } from "./observation.js";
import { type ExplorationPolicy, policyId } from "./policy.js";
import type { SeededRng } from "./rng.js";
import type { RecordedTree } from "./store.js";
import type { NodeRecord } from "./types.js";

export interface ReplayResult {
	policyId: string;
	treeId: string;
	/** Ids revealed, in reveal order (root first). */
	revealedIds: string[];
	/** Revealed non-root nodes (probes spent). */
	N: number;
	/** Decision rounds the policy took. */
	rounds: number;
	/** Best valid score over the revealed prefix (0 when nothing valid). */
	bestScore: number;
	/** Legal selections (cells) that revealed nothing because the recorded branch was exhausted. */
	outOfSupportCells: number;
	/** Charged selections: `N + outOfSupportCells`. */
	selectedCells: number;
	/** `N / selectedCells`; 1 when nothing was selected. */
	inSupport: number;
	/**
	 * Running best valid score after each CHARGED selection (length `selectedCells`);
	 * an out-of-support selection repeats the running best. 0 while nothing valid.
	 */
	bestSoFar: number[];
	/** 1-based index of the first charged selection at which `bestScore` was reached; 0 when the root is best. */
	probesToBest: number;
	/** 1-based decision round in which `bestScore` was first reached; 0 when the root is best. */
	roundsToBest: number;
}

export interface ReplayConfig {
	k2: number;
}

/** `ReplayConfig` plus the online round cap that fixes the objective's probe budget (`w * k1`). */
export interface ReplayScoreConfig extends ReplayConfig {
	k1: number;
}

function cellOf(node: NodeRecord): Cell {
	return {
		nodeId: node.id,
		isRoot: node.parentId === null,
		parentId: node.parentId,
		score: node.score,
		valid: node.valid,
	};
}

function bestOverRevealed(recorded: RecordedTree, revealed: ReadonlySet<string>): number {
	let best = 0;
	let seen = false;
	for (const id of revealed) {
		const node = recorded.nodeById(id);
		if (node?.valid && (!seen || node.score > best)) {
			best = node.score;
			seen = true;
		}
	}
	return seen ? best : 0;
}

/** The replay-side view over a recorded tree and the currently revealed ids. */
class ReplayObservation implements ObservationView {
	readonly maxParallelism: number;
	readonly round: number;
	private readonly recorded: RecordedTree;
	private readonly revealed: ReadonlySet<string>;

	constructor(recorded: RecordedTree, revealed: ReadonlySet<string>, round: number) {
		this.recorded = recorded;
		this.revealed = revealed;
		this.round = round;
		this.maxParallelism = Math.max(1, Math.trunc(recorded.header.w));
	}

	observed(): RevealedNode[] {
		const out: RevealedNode[] = [];
		for (const node of this.recorded.nodes) {
			if (!this.revealed.has(node.id)) continue;
			out.push({
				nodeId: node.id,
				parentId: node.parentId,
				isRoot: node.parentId === null,
				score: node.score,
				valid: node.valid,
				seq: node.seq,
				round: node.round,
			});
		}
		return out;
	}

	legalActions(): Cell[] {
		const out: Cell[] = [];
		for (const node of this.recorded.nodes) {
			if (!this.revealed.has(node.id)) continue;
			if (node.parentId === null) {
				out.push(cellOf(node));
				continue;
			}
			const hasRevealedChild = this.recorded.childrenOf(node.id).some((child) => this.revealed.has(child.id));
			if (!hasRevealedChild) out.push(cellOf(node));
		}
		return out;
	}

	legalRoots(): Cell[] {
		return this.legalActions().filter((cell) => cell.isRoot);
	}

	bestScore(): number {
		return bestOverRevealed(this.recorded, this.revealed);
	}

	revealedNonRootCount(): number {
		let count = 0;
		for (const id of this.revealed) {
			const node = this.recorded.nodeById(id);
			if (node && node.parentId !== null) count++;
		}
		return count;
	}
}

export interface ReplaySimulator {
	readonly recorded: RecordedTree;
	readonly round: number;
	revealedIds(): string[];
	view(): ObservationView;
	allRevealed(): boolean;
	/** Reveal the deterministic recorded child of a legal cell; undefined when out of support. */
	revealFor(cell: Cell): string | undefined;
	advanceRound(): void;
	reset(): void;
}

class ReplaySimulatorImpl implements ReplaySimulator {
	readonly recorded: RecordedTree;
	private revealed: Set<string>;
	private order: string[];
	private roundNum = 0;

	constructor(recorded: RecordedTree) {
		this.recorded = recorded;
		this.revealed = new Set([recorded.rootId]);
		this.order = [recorded.rootId];
	}

	get round(): number {
		return this.roundNum;
	}

	revealedIds(): string[] {
		return [...this.order];
	}

	revealedSet(): ReadonlySet<string> {
		return this.revealed;
	}

	view(): ObservationView {
		return new ReplayObservation(this.recorded, this.revealed, this.roundNum);
	}

	allRevealed(): boolean {
		return this.revealed.size >= this.recorded.nodes.length;
	}

	revealFor(cell: Cell): string | undefined {
		for (const child of this.recorded.childrenOf(cell.nodeId)) {
			if (!this.revealed.has(child.id)) {
				this.revealed.add(child.id);
				this.order.push(child.id);
				return child.id;
			}
		}
		return undefined;
	}

	advanceRound(): void {
		this.roundNum++;
	}

	reset(): void {
		this.revealed = new Set([this.recorded.rootId]);
		this.order = [this.recorded.rootId];
		this.roundNum = 0;
	}
}

export function createReplaySimulator(recorded: RecordedTree): ReplaySimulator {
	return new ReplaySimulatorImpl(recorded);
}

/**
 * Re-walk `recorded` with `policy` and return the replay result. Deterministic
 * and zero-cost: no rng, no clock, no generation. `rng` is accepted only to
 * match the shared interpreter signature and is not consulted.
 */
export function simulatePolicy(
	recorded: RecordedTree,
	policy: ExplorationPolicy,
	cfg: ReplayConfig,
	rng?: SeededRng,
): ReplayResult {
	const sim = new ReplaySimulatorImpl(recorded);
	const k2 = Math.max(1, Math.trunc(cfg.k2));
	const root = recorded.nodeById(recorded.rootId);
	let runningBest = 0;
	let seenValid = false;
	if (root?.valid) {
		runningBest = root.score;
		seenValid = true;
	}
	// `bestScore` lags the true running max by up to IMPROVE_EPS, exactly as the
	// online driver's does; the stop rule reads it, the result reports the true max.
	let bestScore = seenValid ? runningBest : 0;
	let roundsSinceImprovement = 0;
	let rounds = 0;
	let outOfSupport = 0;
	const bestSoFar: number[] = [];
	/** The 1-based round of each charged selection (parallel to `bestSoFar`). */
	const selectionRound: number[] = [];
	while (rounds < k2) {
		if (sim.allRevealed()) break;
		const view = sim.view();
		const batch = interpretPolicy(policy, view, rng);
		if (batch.length === 0) break;
		assertLegalBatch(view, batch);
		for (const cell of batch) {
			const revealedId = sim.revealFor(cell);
			if (revealedId === undefined) {
				outOfSupport++;
			} else {
				const node = recorded.nodeById(revealedId);
				if (node?.valid && (!seenValid || node.score > runningBest)) {
					runningBest = node.score;
					seenValid = true;
				}
			}
			bestSoFar.push(seenValid ? runningBest : 0);
			selectionRound.push(rounds + 1);
		}
		rounds++;
		sim.advanceRound();
		const newBest = bestOverRevealed(recorded, sim.revealedSet());
		if (newBest > bestScore + IMPROVE_EPS) {
			bestScore = newBest;
			roundsSinceImprovement = 0;
		} else {
			roundsSinceImprovement++;
		}
		let revealedNonRoot = 0;
		for (const id of sim.revealedSet()) {
			const node = recorded.nodeById(id);
			if (node && node.parentId !== null) revealedNonRoot++;
		}
		const state: StopState = {
			round: rounds,
			bestScore,
			roundsSinceImprovement,
			revealedNonRootCount: revealedNonRoot,
		};
		if (applyStopRule(policy, state)) break;
	}
	const revealedIds = sim.revealedIds();
	const finalBest = bestOverRevealed(recorded, sim.revealedSet());
	const N = revealedIds.length - 1;
	const selectedCells = N + outOfSupport;
	let probesToBest = 0;
	if (seenValid && !(root?.valid && root.score >= finalBest)) {
		const first = bestSoFar.findIndex((score) => score >= finalBest);
		probesToBest = first < 0 ? 0 : first + 1;
	}
	const roundsToBest = probesToBest === 0 ? 0 : selectionRound[probesToBest - 1]!;
	return {
		policyId: policyId(policy),
		treeId: recorded.header.treeId,
		revealedIds,
		N,
		rounds,
		bestScore: finalBest,
		outOfSupportCells: outOfSupport,
		selectedCells,
		inSupport: selectedCells === 0 ? 1 : N / selectedCells,
		bestSoFar,
		probesToBest,
		roundsToBest,
	};
}

/**
 * The standalone `dream replay` entry: one simulation wrapped in a `dream.replay`
 * root span carrying the result and its objective value. With a single tree the
 * pool is that tree, so `dream.v` is scored against its own score range and the
 * budget `header.w * k1`.
 */
export function simulatePolicyWithSpan(
	recorded: RecordedTree,
	policy: ExplorationPolicy,
	cfg: ReplayScoreConfig,
	objective: ReplayObjectiveConfig = DEFAULT_OBJECTIVE,
): ReplayResult {
	return withSpan(
		"dream.replay",
		{ "dream.policy_id": policyId(policy), "dream.tree_id": recorded.header.treeId },
		(span) => {
			const result = simulatePolicy(recorded, policy, cfg);
			span.setAttributes({
				"dream.revealed_n": result.N,
				"dream.rounds": result.rounds,
				"dream.v": computeObjective(result, objective, poolScoreScale([recorded]), {
					workers: recorded.header.w,
					k1: cfg.k1,
				}),
				"dream.out_of_support": result.outOfSupportCells,
				"dream.in_support": result.inSupport,
				"dream.probes_to_best": result.probesToBest,
				"dream.simulations": 1,
			});
			return result;
		},
	);
}
