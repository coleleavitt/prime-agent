/**
 * Dream store: append-only JSONL trees plus scalar-keyed artifact blobs.
 *
 * The layout mirrors `RavoArchive` (an events log next to a `blobs/`
 * directory): each tree is `<dreamDir>/trees/<treeId>.jsonl` with a header line,
 * node lines, and reveal lines, while the full artifacts live under
 * `<dreamDir>/trees/<treeId>/blobs/<seq>.json` so the node lines stay
 * scalar-only. All filesystem access is synchronous, matching
 * `learning-index.ts`. `writeHeader` truncates, so a same-seed re-run into the
 * same store is idempotent.
 */

import {
	appendFileSync,
	chmodSync,
	copyFileSync,
	existsSync,
	mkdirSync,
	readdirSync,
	readFileSync,
	writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { expandTildePath, getAgentDir } from "../../config.js";
import { canonicalJson, sha256 } from "../ravo/canonical-json.js";
import {
	isNodeRecord,
	isRevealRecord,
	isTreeHeaderRecord,
	type NodeRecord,
	nodeOrigin,
	type RevealRecord,
	type TreeHeaderRecord,
	type TreeRecord,
} from "./types.js";

const DIR_MODE = 0o700;
const FILE_MODE = 0o600;

export class DreamStoreError extends Error {}

/** The dream store directory: `$PRIME_AGENT_DREAM_DIR` or `<agent-dir>/dream`. */
export function getDreamDir(): string {
	const override = process.env.PRIME_AGENT_DREAM_DIR;
	if (override && override.length > 0) return expandTildePath(override);
	return join(getAgentDir(), "dream");
}

export function getDreamTreesDir(dir?: string): string {
	return join(dir ?? getDreamDir(), "trees");
}

export function treePath(treeId: string, dir?: string): string {
	return join(getDreamTreesDir(dir), `${treeId}.jsonl`);
}

function blobDir(treeId: string, dir?: string): string {
	return join(getDreamTreesDir(dir), treeId, "blobs");
}

function blobPath(treeId: string, seq: number, dir?: string): string {
	return join(blobDir(treeId, dir), `${seq}.json`);
}

/**
 * Experiments live beside the pool, never in it: `<dir>/experiments/<id>/<arm>`
 * is a complete dream store of its own (its trees under `.../trees`), so
 * `listTrees`/`freezePool` on `<dir>` never see an experiment's trees and the
 * arms never see each other's.
 */
export function experimentsDir(dir: string): string {
	return join(dir, "experiments");
}

export function experimentDir(dir: string, experimentId: string): string {
	return join(experimentsDir(dir), experimentId);
}

export function experimentArmDir(dir: string, experimentId: string, arm: string): string {
	return join(experimentDir(dir, experimentId), arm);
}

export function experimentResultPath(dir: string, experimentId: string): string {
	return join(experimentDir(dir, experimentId), "result.json");
}

/** Experiment ids (directory names under `<dir>/experiments`), sorted; `[]` when none. */
export function listExperimentIds(dir: string): string[] {
	try {
		return readdirSync(experimentsDir(dir), { withFileTypes: true })
			.filter((entry) => entry.isDirectory())
			.map((entry) => entry.name)
			.sort((a, b) => a.localeCompare(b));
	} catch {
		return [];
	}
}

/**
 * Copy one recorded tree (its JSONL file and every blob) from one store to
 * another, byte for byte, with the store's 0700/0600 modes. The experiment
 * runner uses it to share a single round-1 rollout across every arm.
 */
export function copyTree(treeId: string, fromDir: string, toDir: string): void {
	const source = treePath(treeId, fromDir);
	if (!existsSync(source)) throw new DreamStoreError(`no tree ${treeId} in ${fromDir}`);
	const target = treePath(treeId, toDir);
	mkdirSync(dirname(target), { recursive: true, mode: DIR_MODE });
	copyFileSync(source, target);
	chmodSync(target, FILE_MODE);
	const sourceBlobs = blobDir(treeId, fromDir);
	let names: string[];
	try {
		names = readdirSync(sourceBlobs);
	} catch {
		return;
	}
	const targetBlobs = blobDir(treeId, toDir);
	mkdirSync(targetBlobs, { recursive: true, mode: DIR_MODE });
	for (const name of names) {
		const targetBlob = join(targetBlobs, name);
		copyFileSync(join(sourceBlobs, name), targetBlob);
		chmodSync(targetBlob, FILE_MODE);
	}
}

/** Append-only writer for one tree file plus its blobs. */
export class TreeWriter {
	constructor(
		private readonly treeId: string,
		private readonly dir?: string,
	) {}

	/** Create or truncate the tree file with its header line (idempotent for a re-run). */
	writeHeader(header: TreeHeaderRecord): void {
		const path = treePath(this.treeId, this.dir);
		mkdirSync(dirname(path), { recursive: true, mode: DIR_MODE });
		writeFileSync(path, `${JSON.stringify(header)}\n`, { mode: FILE_MODE });
	}

	appendNode(node: NodeRecord): void {
		appendFileSync(treePath(this.treeId, this.dir), `${JSON.stringify(node)}\n`, { mode: FILE_MODE });
	}

	appendReveal(reveal: RevealRecord): void {
		appendFileSync(treePath(this.treeId, this.dir), `${JSON.stringify(reveal)}\n`, { mode: FILE_MODE });
	}

	/** Persist an artifact as a canonical-JSON blob keyed by seq; returns its digest. */
	writeBlob(seq: number, content: unknown): string {
		const path = blobPath(this.treeId, seq, this.dir);
		mkdirSync(dirname(path), { recursive: true, mode: DIR_MODE });
		const json = canonicalJson(content);
		writeFileSync(path, json, { mode: FILE_MODE });
		return sha256(json);
	}

	readBlob(seq: number): unknown {
		return JSON.parse(readFileSync(blobPath(this.treeId, seq, this.dir), "utf8"));
	}
}

/** A frozen, read-only tree: the replay simulator's only input. */
export interface RecordedTree {
	readonly header: TreeHeaderRecord;
	readonly rootId: string;
	/** All node records in seq order, including the root at seq 0; every one carries a resolved `origin`. */
	readonly nodes: readonly NodeRecord[];
	/** Online reveal lines (informational; replay derives reveals itself). */
	readonly reveals: readonly RevealRecord[];
	nodeById(id: string): NodeRecord | undefined;
	/** Recorded children of a node, in seq order. */
	childrenOf(id: string): NodeRecord[];
	allNodeIds(): string[];
	/** Load the full artifact for a node; throws if no blob loader was provided. */
	loadBlob(node: NodeRecord): unknown;
}

class RecordedTreeImpl implements RecordedTree {
	readonly header: TreeHeaderRecord;
	readonly rootId: string;
	readonly nodes: readonly NodeRecord[];
	readonly reveals: readonly RevealRecord[];
	private readonly byId = new Map<string, NodeRecord>();
	private readonly childIds = new Map<string, string[]>();
	private readonly blobLoader: (node: NodeRecord) => unknown;

	constructor(
		header: TreeHeaderRecord,
		nodes: readonly NodeRecord[],
		reveals: readonly RevealRecord[],
		blobLoader: (node: NodeRecord) => unknown,
	) {
		this.header = header;
		this.reveals = reveals;
		this.blobLoader = blobLoader;
		const sorted = [...nodes].sort((a, b) => a.seq - b.seq);
		this.nodes = sorted;
		const roots = sorted.filter((node) => node.parentId === null);
		if (roots.length !== 1) {
			throw new DreamStoreError(`a recorded tree needs exactly one root, found ${roots.length}`);
		}
		this.rootId = roots[0]!.id;
		for (const node of sorted) {
			this.byId.set(node.id, node);
			if (node.parentId !== null) {
				const siblings = this.childIds.get(node.parentId) ?? [];
				siblings.push(node.id);
				this.childIds.set(node.parentId, siblings);
			}
		}
	}

	nodeById(id: string): NodeRecord | undefined {
		return this.byId.get(id);
	}

	childrenOf(id: string): NodeRecord[] {
		return (this.childIds.get(id) ?? []).map((childId) => this.byId.get(childId)!);
	}

	allNodeIds(): string[] {
		return this.nodes.map((node) => node.id);
	}

	loadBlob(node: NodeRecord): unknown {
		return this.blobLoader(node);
	}
}

function noBlobLoader(node: NodeRecord): unknown {
	throw new DreamStoreError(`no blob loader configured for node ${node.id}`);
}

/** A node record with its origin resolved (the legacy default for a line written before provenance). */
function withOrigin(record: NodeRecord): NodeRecord {
	return { ...record, origin: nodeOrigin(record) };
}

/**
 * Build a frozen recorded tree from records in memory (no filesystem access).
 * Node records come back with `origin` resolved, so a reader never has to know
 * the legacy default.
 */
export function buildRecordedTree(
	records: readonly TreeRecord[],
	blobLoader: (node: NodeRecord) => unknown = noBlobLoader,
): RecordedTree {
	const header = records.find(isTreeHeaderRecord);
	if (!header) throw new DreamStoreError("records have no tree header");
	const nodes = records.filter(isNodeRecord).map(withOrigin);
	const reveals = records.filter(isRevealRecord);
	return new RecordedTreeImpl(header, nodes, reveals, blobLoader);
}

function parseRecords(content: string): TreeRecord[] {
	const records: TreeRecord[] = [];
	for (const line of content.split("\n")) {
		if (line.length === 0) continue;
		let parsed: unknown;
		try {
			parsed = JSON.parse(line);
		} catch {
			throw new DreamStoreError("tree file has an unparsable line");
		}
		if (typeof parsed !== "object" || parsed === null) continue;
		const type = (parsed as { type?: unknown }).type;
		if (type === "tree" || type === "node" || type === "reveal") {
			records.push(parsed as TreeRecord);
		}
	}
	return records;
}

/** Read a persisted tree, with a blob loader that reads `<treeId>/blobs/<seq>.json`. */
export function readTree(treeId: string, dir?: string): RecordedTree {
	const path = treePath(treeId, dir);
	if (!existsSync(path)) throw new DreamStoreError(`no tree ${treeId} in the dream store`);
	const records = parseRecords(readFileSync(path, "utf8"));
	return buildRecordedTree(records, (node) => JSON.parse(readFileSync(blobPath(treeId, node.seq, dir), "utf8")));
}

export interface TreeSummary {
	treeId: string;
	taskId: string;
	w: number;
	seed: number | string;
	policyId: string;
	iteration: number;
	createdTs: number;
	nodeCount: number;
	/** Nodes whose candidate a child agent generated (`origin: "llm"`); 0 for a local tree. */
	agentGeneratedCount: number;
	bestScore: number;
}

function agentGeneratedCountOf(records: readonly TreeRecord[]): number {
	let count = 0;
	for (const record of records) {
		if (isNodeRecord(record) && nodeOrigin(record) === "llm") count += 1;
	}
	return count;
}

function bestScoreOf(records: readonly TreeRecord[]): number {
	let best = 0;
	let seen = false;
	for (const record of records) {
		if (isNodeRecord(record) && record.valid && Number.isFinite(record.score)) {
			if (!seen || record.score > best) {
				best = record.score;
				seen = true;
			}
		}
	}
	return seen ? best : 0;
}

/** Summaries of every tree in the store, sorted by treeId. */
export function listTrees(dir?: string): TreeSummary[] {
	const treesDir = getDreamTreesDir(dir);
	let names: string[];
	try {
		names = readdirSync(treesDir);
	} catch {
		return [];
	}
	const summaries: TreeSummary[] = [];
	for (const name of names) {
		if (!name.endsWith(".jsonl")) continue;
		let records: TreeRecord[];
		try {
			records = parseRecords(readFileSync(join(treesDir, name), "utf8"));
		} catch {
			continue;
		}
		const header = records.find(isTreeHeaderRecord);
		if (!header) continue;
		summaries.push({
			treeId: header.treeId,
			taskId: header.taskId,
			w: header.w,
			seed: header.seed,
			policyId: header.policyId,
			iteration: header.iteration,
			createdTs: header.createdTs,
			nodeCount: records.filter(isNodeRecord).length,
			agentGeneratedCount: agentGeneratedCountOf(records),
			bestScore: bestScoreOf(records),
		});
	}
	return summaries.sort((a, b) => a.treeId.localeCompare(b.treeId));
}
