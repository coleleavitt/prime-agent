import { createHash } from "node:crypto";
import {
	closeSync,
	existsSync,
	fsyncSync,
	linkSync,
	mkdirSync,
	openSync,
	realpathSync,
	rmSync,
	statSync,
	writeSync,
} from "node:fs";
import { readdir, readFile, stat } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { EventLog } from "../../core/event-log.js";
import { canonicalSessionPath } from "../../core/session-lease.js";
import { getSessionArtifactPathForFile, readSessionInfo, type SessionInfo } from "../../core/session-manager.js";
import { readFirstLineSync } from "../../utils/file-lines.js";

/**
 * Daemon-owned RLM spawn ledger.
 *
 * One append-only JSONL file per sessions dir, written by daemon processes at
 * the moments they admit a spawn, perform a rename, or record a deletion.
 * Family topology (parent/child edges, depths, names) is read back from this
 * file instead of being re-derived from writer-owned session headers,
 * registries, and bodies at read time.
 *
 * Multi-writer reality: the supervisor and each session worker hold their own
 * instance over the same file. Appends are single small O_APPEND writes (well
 * under PIPE_BUF-scale sizes), whose atomicity we rely on for interleaving;
 * reads re-read the whole file per operation, so cross-process staleness is
 * bounded to in-flight appends. In-process appends are serialized on an
 * internal queue.
 */

export const RLM_LEDGER_DIR = "rlm-ledger";

/** Bounded read: a ledger beyond these limits fails closed loudly. */
export const RLM_LEDGER_MAX_BYTES = 32 * 1024 * 1024;
export const RLM_LEDGER_MAX_RECORDS = 100_000;

export type RlmLedgerDeleteReason = "user" | "parent-teardown" | "revoked" | "gc";

interface RlmLedgerMetaRecord {
	v: 1;
	op: "meta";
	at: string;
	sessionsDir: string;
}

export interface RlmLedgerSpawnRecord {
	v: 1;
	op: "spawn";
	at: string;
	childId: string;
	parent: string;
	child: string;
	depth: number;
	name: string;
}

export interface RlmLedgerRenameRecord {
	v: 1;
	op: "rename";
	at: string;
	childId: string;
	child: string;
	name: string;
}

export interface RlmLedgerDeleteRecord {
	v: 1;
	op: "delete";
	at: string;
	childId: string;
	child: string;
	reason: RlmLedgerDeleteReason;
}

export type RlmLedgerRecord = RlmLedgerSpawnRecord | RlmLedgerRenameRecord | RlmLedgerDeleteRecord;

/** A live edge after replaying the ledger (last-writer-wins per childId+child). */
export interface RlmLedgerEdge {
	childId: string;
	parent: string;
	child: string;
	depth: number;
	name: string;
	deleted?: RlmLedgerDeleteReason;
}

/** Minimal registry-entry shape the seeder consumes (matches the daemon writer). */
export interface RlmLedgerSeedRegistryEntry {
	childId: string;
	sessionName: string;
	sessionFile: string;
	rlmDepth?: number;
	status: "running" | "completed" | "deleted";
}

export interface LegacyRlmSubagentRegistryEntry extends RlmLedgerSeedRegistryEntry {
	type: "rlm_subagent";
	sessionDir: string;
	parentSessionId: string;
	parentSessionFile?: string;
	rlmMaxDepth?: number;
	rlmParentNodeId?: string;
	prompt?: string;
	spawnCode?: string;
	model?: { provider: string; modelId: string };
	createdAt: number;
	updatedAt: string;
}

export interface RlmLedgerSeedSource {
	readRegistryForSessionFile(sessionFile: string): Promise<RlmLedgerSeedRegistryEntry[]>;
}

export async function readLegacyRlmSubagentRegistry(
	path: string,
	options: { throwOnReadError?: boolean; log?: (message: string) => void; onReadError?: () => void } = {},
): Promise<LegacyRlmSubagentRegistryEntry[]> {
	let contents: string;
	try {
		contents = await readFile(path, "utf8");
	} catch (error) {
		options.onReadError?.();
		if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
			options.log?.(
				`failed to read RLM subagent registry: ${error instanceof Error ? error.message : String(error)}`,
			);
			if (options.throwOnReadError) throw error;
		}
		return [];
	}
	const latest = new Map<string, LegacyRlmSubagentRegistryEntry>();
	for (const line of contents.split(/\r?\n/)) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		try {
			const entry = JSON.parse(trimmed) as Partial<LegacyRlmSubagentRegistryEntry>;
			if (
				entry.type !== "rlm_subagent" ||
				typeof entry.childId !== "string" ||
				typeof entry.sessionName !== "string" ||
				typeof entry.sessionFile !== "string" ||
				(entry.status !== "running" && entry.status !== "completed" && entry.status !== "deleted") ||
				(entry.rlmDepth !== undefined && (!Number.isSafeInteger(entry.rlmDepth) || entry.rlmDepth < 0))
			) {
				continue;
			}
			latest.set(entry.childId, {
				...entry,
				sessionDir: typeof entry.sessionDir === "string" ? entry.sessionDir : dirname(entry.sessionFile),
				// rlmMaxDepth is optional hydration metadata the ledger seeder never
				// reads; a damaged value must not discard the child's topology edge,
				// so it is dropped instead of rejecting the whole entry.
				rlmMaxDepth:
					entry.rlmMaxDepth !== undefined && Number.isSafeInteger(entry.rlmMaxDepth) && entry.rlmMaxDepth >= 0
						? entry.rlmMaxDepth
						: undefined,
			} as LegacyRlmSubagentRegistryEntry);
		} catch (error) {
			options.log?.(
				`ignored malformed RLM subagent registry entry: ${error instanceof Error ? error.message : String(error)}`,
			);
		}
	}
	return [...latest.values()];
}

export function createRlmLedgerRegistrySeedSource(): RlmLedgerSeedSource {
	return {
		readRegistryForSessionFile: async (sessionFile) => {
			let headerId: string | undefined;
			try {
				const firstLine = readFirstLineSync(sessionFile);
				if (firstLine) {
					const header = JSON.parse(firstLine) as { id?: unknown };
					if (typeof header.id === "string") headerId = header.id;
				}
			} catch {
				return [];
			}
			if (!headerId) return [];
			return readLegacyRlmSubagentRegistry(
				join(getSessionArtifactPathForFile(sessionFile, headerId), "rlm-subagents.jsonl"),
			);
		},
	};
}

/** Canonicalize a directory: realpath when it exists, plain resolve otherwise. */
function canonicalizeDirPath(dir: string): string {
	const resolved = resolve(dir);
	try {
		return realpathSync(resolved);
	} catch {
		return resolved;
	}
}

export function rlmLedgerPath(agentDir: string, sessionsDir: string): string {
	const canonical = canonicalizeDirPath(sessionsDir);
	const hash = createHash("sha256").update(canonical).digest("hex").slice(0, 16);
	return join(agentDir, RLM_LEDGER_DIR, `${hash}.jsonl`);
}

function nowIso(): string {
	return new Date().toISOString();
}

function isDeleteReason(value: unknown): value is RlmLedgerDeleteReason {
	return value === "user" || value === "parent-teardown" || value === "revoked" || value === "gc";
}

/**
 * Parse one ledger line. Returns undefined for a well-formed v:1 record with
 * an unknown op (forward-compat: newer writers may add ops; readers skip
 * them). Any other violation throws. Version policy: v !== 1 fails loudly —
 * a future v2 must move to a new file/hash (or accept breaking old readers),
 * because silently skipping records a reader cannot understand would corrupt
 * topology.
 */
function parseLedgerLine(
	line: string,
	index: number,
): RlmLedgerRecord | RlmLedgerMetaRecord | RlmLedgerAdmitRecord | undefined {
	let parsed: unknown;
	try {
		parsed = JSON.parse(line);
	} catch (error) {
		throw new Error(
			`Malformed RLM ledger line ${index + 1}: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
	const record = parsed as {
		v?: unknown;
		op?: unknown;
		at?: unknown;
		sessionsDir?: unknown;
		childId?: unknown;
		parent?: unknown;
		child?: unknown;
		depth?: unknown;
		name?: unknown;
		reason?: unknown;
	};
	if (typeof record.at !== "string" || (record.v !== 1 && record.v !== RLM_LEDGER_ADMIT_VERSION)) {
		throw new Error(`Malformed RLM ledger line ${index + 1}: missing v/at`);
	}
	// V2 composite admission records (v:2, op:"admit") share this per-sessions-dir
	// file but are owned by RlmCompositeAdmissionLedger. The V1 topology reader
	// recognizes and skips them: they are not v1 spawn/rename/delete edges.
	// Returning the record (not undefined) keeps replaySync from misreporting an
	// unknown op; the topology loop below ignores op "admit".
	if (record.v === RLM_LEDGER_ADMIT_VERSION) {
		if (record.op !== RLM_LEDGER_ADMIT_OP) {
			return undefined;
		}
		return record as unknown as RlmLedgerAdmitRecord;
	}
	switch (record.op) {
		case "meta":
			if (typeof record.sessionsDir !== "string") {
				throw new Error(`Malformed RLM ledger line ${index + 1}: meta without sessionsDir`);
			}
			return record as unknown as RlmLedgerMetaRecord;
		case "spawn":
			if (
				typeof record.childId !== "string" ||
				typeof record.parent !== "string" ||
				typeof record.child !== "string" ||
				typeof record.name !== "string" ||
				typeof record.depth !== "number" ||
				!Number.isSafeInteger(record.depth) ||
				record.depth < 1
			) {
				throw new Error(`Malformed RLM ledger line ${index + 1}: invalid spawn record`);
			}
			return record as unknown as RlmLedgerSpawnRecord;
		case "rename":
			if (
				typeof record.childId !== "string" ||
				typeof record.child !== "string" ||
				typeof record.name !== "string"
			) {
				throw new Error(`Malformed RLM ledger line ${index + 1}: invalid rename record`);
			}
			return record as unknown as RlmLedgerRenameRecord;
		case "delete":
			if (typeof record.childId !== "string" || typeof record.child !== "string" || !isDeleteReason(record.reason)) {
				throw new Error(`Malformed RLM ledger line ${index + 1}: invalid delete record`);
			}
			return record as unknown as RlmLedgerDeleteRecord;
		default:
			return undefined;
	}
}

function edgeKey(childId: string, child: string): string {
	return `${childId}\u0000${canonicalSessionPath(child)}`;
}

/**
 * Per-sessions-dir spawn ledger. All operations are serialized on an internal
 * queue; the first operation lazily seeds a missing ledger from the existing
 * per-parent registries (memoized; a seeding failure degrades to an empty
 * ledger and is never fail-closed).
 */
export class RlmSpawnLedger {
	private readonly path: string;
	private readonly eventLog: EventLog;
	private readonly canonicalSessionsDir: string;
	private queue: Promise<unknown> = Promise.resolve();
	private seedAttempted = false;
	/** Last replay guarded by a file stat snapshot; see replaySyncCached(). */
	private edgeCache?: {
		stat: { size: number; mtimeMs: number; ino: number };
		edges: Map<string, RlmLedgerEdge>;
	};

	constructor(
		agentDir: string,
		sessionsDir: string,
		private readonly seedSource?: RlmLedgerSeedSource,
		private readonly log: (message: string) => void = () => {},
	) {
		this.canonicalSessionsDir = canonicalizeDirPath(sessionsDir);
		this.path = rlmLedgerPath(agentDir, sessionsDir);
		this.eventLog = new EventLog(this.path, {
			maxBytes: RLM_LEDGER_MAX_BYTES,
			maxRecords: RLM_LEDGER_MAX_RECORDS,
			log: (message) => this.log(`RLM ledger: ${message}`),
		});
	}

	get ledgerPath(): string {
		return this.path;
	}

	appendSpawn(input: { childId: string; parent: string; child: string; depth: number; name: string }): Promise<void> {
		return this.enqueue(() => this.appendSpawnUnlocked(input));
	}

	appendRename(input: { childId: string; child: string; name: string }): Promise<void> {
		return this.enqueue(() => {
			this.appendRecord({
				v: 1,
				op: "rename",
				at: nowIso(),
				childId: input.childId,
				child: canonicalSessionPath(input.child),
				name: input.name,
			});
		});
	}

	/** Rename by child session path alone (offline saved-session rename knows no childId). */
	appendRenameByChildPath(child: string, name: string): Promise<void> {
		return this.enqueue(() => {
			const target = canonicalSessionPath(child);
			for (const edge of this.replaySyncCached().values()) {
				if (!edge.deleted && canonicalSessionPath(edge.child) === target) {
					this.appendRecord({ v: 1, op: "rename", at: nowIso(), childId: edge.childId, child: target, name });
				}
			}
		});
	}

	appendDelete(input: { childId: string; child: string; reason: RlmLedgerDeleteReason }): Promise<void> {
		return this.enqueue(() => {
			this.appendRecord({
				v: 1,
				op: "delete",
				at: nowIso(),
				childId: input.childId,
				child: canonicalSessionPath(input.child),
				reason: input.reason,
			});
		});
	}

	/** Resolves once every operation enqueued so far has completed (durably, for appends). */
	flush(): Promise<void> {
		return this.queue.then(() => undefined);
	}

	/**
	 * Replay edges without liveness reconciliation. Deleted edges are filtered
	 * by default; `includeDeleted` keeps the tombstones (marked with their
	 * delete reason) for consumers that need a deleted child's identity, such
	 * as cleanup retries.
	 */
	edges(includeDeleted = false): Promise<RlmLedgerEdge[]> {
		return this.enqueue(() =>
			[...this.replaySyncCached().values()].filter((edge) => includeDeleted || !edge.deleted),
		);
	}

	/**
	 * Family of every session rooted in this ledger's sessions dir: bounded
	 * readdir of *.jsonl roots as depth-0 rows plus live ledger edges, both
	 * reconciled by stat (a dead parent or child drops the edge). Depths are
	 * verified parent+1 between ledger-known depths; a contradictory edge is
	 * dropped and logged, never fails the whole family.
	 */
	family(): Promise<SessionInfo[]> {
		return this.enqueue(() => this.familyUnlocked());
	}

	/** Same-parent rows for a child session path, including the child itself. */
	siblings(sessionPath: string): Promise<SessionInfo[]> {
		return this.enqueue(async () => {
			const target = canonicalSessionPath(sessionPath);
			const family = await this.familyUnlocked();
			const edges = [...this.replaySyncCached().values()].filter((edge) => !edge.deleted);
			const parentByChild = new Map(
				edges.map((edge) => [canonicalSessionPath(edge.child), canonicalSessionPath(edge.parent)]),
			);
			const parent = parentByChild.get(target);
			if (parent !== undefined) {
				const rows = family.filter((row) => parentByChild.get(canonicalSessionPath(row.path)) === parent);
				// The target's edge can be reconciliation-dropped (parent file
				// gone) while its own file still exists: fall back to presenting
				// the survivor alone rather than an empty set the callers would
				// read as "session not found".
				if (!rows.some((row) => canonicalSessionPath(row.path) === target)) {
					try {
						if ((await stat(target)).isFile()) {
							return [await this.sessionRow(target, 0, undefined, undefined)];
						}
					} catch {
						// fall through to the (possibly empty) sibling rows
					}
				}
				return rows;
			}
			// Roots are siblings of the other roots. A session outside both the
			// ledger and the sessions dir is presented alone (matching the
			// registry-walking reader's behavior for parentless sessions).
			const roots = family.filter((row) => row.rlmDepth === 0);
			if (roots.some((row) => canonicalSessionPath(row.path) === target)) {
				return roots;
			}
			try {
				if (!(await stat(target)).isFile()) return [];
			} catch {
				return [];
			}
			return [await this.sessionRow(target, 0, undefined, undefined)];
		});
	}

	private enqueue<T>(fn: () => Promise<T> | T): Promise<T> {
		const next = this.queue.then(async () => {
			if (!this.seedAttempted) {
				this.seedAttempted = true;
				try {
					await this.seed();
				} catch (error) {
					this.log(`RLM ledger seeding failed: ${error instanceof Error ? error.message : String(error)}`);
				}
			}
			return fn();
		});
		this.queue = next.catch(() => undefined);
		return next;
	}

	private appendSpawnUnlocked(input: {
		childId: string;
		parent: string;
		child: string;
		depth: number;
		name: string;
	}): void {
		// Enforce the same invariants parseLedgerLine checks: never write a
		// record this reader would refuse to read back.
		if (!input.childId || !input.parent || !input.child || !Number.isSafeInteger(input.depth) || input.depth < 1) {
			throw new Error(
				`RLM ledger: invalid spawn for ${input.childId || "<missing childId>"} (depth ${input.depth})`,
			);
		}
		const childPath = canonicalSessionPath(input.child);
		// Advisory, per-process: catches double-admission mistakes inside this
		// daemon. It is NOT a global uniqueness guarantee — other processes
		// append to the same file between our read and write.
		for (const edge of this.replaySyncCached().values()) {
			if (!edge.deleted && canonicalSessionPath(edge.child) === childPath && edge.childId !== input.childId) {
				throw new Error(`RLM ledger: duplicate child session path ${childPath} (already ${edge.childId})`);
			}
		}
		this.appendRecord({
			v: 1,
			op: "spawn",
			at: nowIso(),
			childId: input.childId,
			parent: canonicalSessionPath(input.parent),
			child: childPath,
			depth: input.depth,
			name: input.name,
		});
	}

	/** Live edges reconciled by stat, exactly like family(): a dead parent or child drops the edge. */
	liveEdges(): Promise<RlmLedgerEdge[]> {
		return this.enqueue(() => this.liveEdgesUnlocked());
	}

	private async liveEdgesUnlocked(
		edges = [...this.replaySyncCached().values()].filter((edge) => !edge.deleted),
	): Promise<RlmLedgerEdge[]> {
		const statCache = new Map<string, boolean>();
		const exists = async (path: string): Promise<boolean> => {
			const cached = statCache.get(path);
			if (cached !== undefined) return cached;
			let ok = false;
			try {
				ok = (await stat(path)).isFile();
			} catch {
				ok = false;
			}
			statCache.set(path, ok);
			return ok;
		};
		const alive: RlmLedgerEdge[] = [];
		for (const edge of edges) {
			if ((await exists(canonicalSessionPath(edge.child))) && (await exists(canonicalSessionPath(edge.parent)))) {
				alive.push(edge);
			}
		}
		return alive;
	}

	private async familyUnlocked(): Promise<SessionInfo[]> {
		// One replay, one stat snapshot: byChild comes from the same alive set that emits child rows,
		// so a child whose dead edge was reconciled away degrades to a root row instead of vanishing.
		let alive: RlmLedgerEdge[] = await this.liveEdgesUnlocked(
			[...this.replaySyncCached().values()].filter((candidate) => !candidate.deleted),
		);
		const byChild = new Map<string, RlmLedgerEdge>();
		for (const edge of alive) {
			byChild.set(canonicalSessionPath(edge.child), edge);
		}
		const rootPaths: string[] = [];
		let rootEntries: string[] = [];
		try {
			rootEntries = await readdir(this.canonicalSessionsDir);
		} catch {
			rootEntries = [];
		}
		for (const entry of rootEntries.filter((name) => name.endsWith(".jsonl")).sort()) {
			const path = canonicalSessionPath(join(this.canonicalSessionsDir, entry));
			// Ledger children that live directly in the sessions dir are not roots.
			if (byChild.has(path)) continue;
			rootPaths.push(path);
		}
		// Verify depth monotonicity between ledger-known depths only: a root's
		// presented depth of 0 is a display convention, not an assertion (a
		// nested daemon's roots legitimately carry env-derived depths > 0). A
		// contradictory edge is dropped and logged; one bad edge must not fail
		// the whole family.
		const depthByPath = new Map<string, number>();
		for (const edge of alive) {
			depthByPath.set(canonicalSessionPath(edge.child), edge.depth);
		}
		alive = alive.filter((edge) => {
			const parentDepth = depthByPath.get(canonicalSessionPath(edge.parent));
			if (parentDepth !== undefined && edge.depth !== parentDepth + 1) {
				this.log(
					`RLM ledger: dropped edge ${edge.childId} with contradictory depth (parent ${parentDepth}, child ${edge.depth})`,
				);
				return false;
			}
			return true;
		});
		const rows: SessionInfo[] = [];
		for (const rootPath of rootPaths) {
			rows.push(await this.sessionRow(rootPath, 0, undefined, undefined));
		}
		for (const edge of alive) {
			rows.push(
				await this.sessionRow(
					canonicalSessionPath(edge.child),
					edge.depth,
					canonicalSessionPath(edge.parent),
					edge.name,
				),
			);
		}
		return rows;
	}

	private async sessionRow(
		path: string,
		depth: number,
		parentPath: string | undefined,
		name: string | undefined,
	): Promise<SessionInfo> {
		// Display-grade fields are best-effort from the ordinary session-info
		// read; topology (path, depth, parent) comes EXCLUSIVELY from the
		// ledger: header-claimed parentSessionPath/rlmDepth (e.g. fork headers)
		// are stripped, never passed through. For roots the ledger carries no
		// name, so the name comes from this read — writer-owned display data,
		// not authority.
		const info = await readSessionInfo(path).catch(() => null);
		if (info) {
			const { parentSessionPath: _headerParent, rlmDepth: _headerDepth, ...display } = info;
			return {
				...display,
				rlmDepth: depth,
				...(parentPath ? { parentSessionPath: parentPath } : {}),
				...(name ? { name } : {}),
			};
		}
		return {
			path,
			id: basename(path, ".jsonl"),
			cwd: "",
			...(name ? { name } : {}),
			...(parentPath ? { parentSessionPath: parentPath } : {}),
			rlmDepth: depth,
			created: new Date(0),
			modified: new Date(0),
			messageCount: 0,
			firstMessage: "",
			allMessagesText: "",
		};
	}

	private async seed(): Promise<void> {
		if (!this.seedSource || existsSync(this.path)) return;
		let rootEntries: string[] = [];
		try {
			rootEntries = await readdir(this.canonicalSessionsDir);
		} catch {
			return;
		}
		// Collect the complete seed first, then publish it atomically via a
		// temp file + rename: the ledger file only exists once seeding is
		// complete, so an interrupted seed leaves nothing and the next
		// construction re-seeds from scratch. A concurrent process appending
		// before the rename creates the real file on demand and thereby
		// suppresses this seed — the same behavior as any pre-existing ledger.
		const records: RlmLedgerSpawnRecord[] = [];
		const queue: Array<{ sessionFile: string; depth: number }> = rootEntries
			.filter((name) => name.endsWith(".jsonl"))
			.sort()
			.map((name) => ({ sessionFile: join(this.canonicalSessionsDir, name), depth: 0 }));
		const visited = new Set<string>(queue.map((item) => canonicalSessionPath(item.sessionFile)));
		while (queue.length > 0) {
			const { sessionFile, depth } = queue.shift()!;
			for (const entry of await this.seedSource.readRegistryForSessionFile(sessionFile)) {
				if (entry.status === "deleted") continue;
				const childPath = canonicalSessionPath(entry.sessionFile);
				if (visited.has(childPath)) continue;
				visited.add(childPath);
				// A registry depth < 1 (legacy 0-depth entries exist in real data)
				// would be unwritable under the spawn invariants; treat it as
				// absent and derive parent depth + 1 instead of skipping the edge.
				const registryDepth = entry.rlmDepth !== undefined && entry.rlmDepth >= 1 ? entry.rlmDepth : undefined;
				const childDepth = registryDepth ?? depth + 1;
				if (!entry.childId) {
					this.log("RLM ledger: skipped seeding a registry entry without a childId");
					continue;
				}
				records.push({
					v: 1,
					op: "spawn",
					at: nowIso(),
					childId: entry.childId,
					parent: canonicalSessionPath(sessionFile),
					child: childPath,
					depth: childDepth,
					name: entry.sessionName,
				});
				queue.push({ sessionFile: entry.sessionFile, depth: childDepth });
			}
		}
		if (records.length === 0) return;
		const meta: RlmLedgerMetaRecord = { v: 1, op: "meta", at: nowIso(), sessionsDir: this.canonicalSessionsDir };
		const payload = [meta, ...records].map((record) => `${JSON.stringify(record)}\n`).join("");
		// A seed beyond the read bounds would publish a ledger every replaySync
		// refuses to read — manufacturing the exact poisoned state the bounds
		// exist to prevent. Skip seeding entirely (flat families, the documented
		// degradation mode) rather than publishing partial topology: profiles
		// this large are pathological, and a truncated tree would be more
		// confusing than a flat one. Not thrown: a hard error here would stick
		// via seedAttempted and the next append would create an empty ledger.
		if (records.length + 1 > RLM_LEDGER_MAX_RECORDS || Buffer.byteLength(payload) > RLM_LEDGER_MAX_BYTES) {
			this.log(
				`RLM ledger: seed exceeds read bounds (${records.length} records, ${Buffer.byteLength(payload)} bytes); skipping seeding`,
			);
			return;
		}
		const dir = dirname(this.path);
		mkdirSync(dir, { recursive: true, mode: 0o700 });
		const tempPath = `${this.path}.seed-${process.pid}-${Date.now()}`;
		const handle = openSync(tempPath, "wx", 0o600);
		try {
			writeSync(handle, payload);
			fsyncSync(handle);
		} finally {
			closeSync(handle);
		}
		try {
			this.publishSeedFile(tempPath);
		} finally {
			rmSync(tempPath, { force: true });
		}
	}

	private publishSeedFile(tempPath: string): void {
		// Atomic no-clobber publish: link() fails with EEXIST if a live append
		// created the real file meanwhile — that append wins (its data is
		// fresher than the registries) and the seed is discarded. No-clobber
		// publication is a hard requirement for seeding: post-consolidation,
		// deletes live only in the ledger, so any clobber window can lose live
		// appends and resurrect deleted edges. Filesystems that cannot provide
		// link() therefore get flat pre-ledger history (the documented
		// degradation mode) rather than a check-then-rename race.
		try {
			linkSync(tempPath, this.path);
		} catch (error) {
			const code = (error as NodeJS.ErrnoException).code;
			if (code === "EEXIST") {
				return;
			}
			this.log(`RLM ledger: link publish unavailable (${code ?? "unknown"}); skipping seeding`);
		}
	}

	private appendRecord(record: RlmLedgerRecord): void {
		this.eventLog.appendSync([record], {
			durable: true,
			onCreate: () => [
				{ v: 1, op: "meta", at: nowIso(), sessionsDir: this.canonicalSessionsDir } satisfies RlmLedgerMetaRecord,
			],
		});
		// Our own writes must not be served stale from the stat-guarded cache;
		// other processes' appends are caught by the stat guard itself.
		this.edgeCache = undefined;
	}

	/**
	 * Replay the ledger behind a stat-guarded edge cache: a file whose size,
	 * mtime, and inode are unchanged reuses the cached edges instead of
	 * re-parsing. Any append forces a fresh replay - appendRecord drops the
	 * cache for our own writes, and another process's append changes the
	 * stat - so staleness stays bounded to in-flight appends. A missing file
	 * bypasses the cache and replays to an empty edge set.
	 */
	private replaySyncCached(): Map<string, RlmLedgerEdge> {
		let snapshot: { size: number; mtimeMs: number; ino: number } | undefined;
		try {
			const current = statSync(this.path);
			snapshot = { size: current.size, mtimeMs: current.mtimeMs, ino: current.ino };
		} catch {
			snapshot = undefined;
		}
		const cache = this.edgeCache;
		if (
			snapshot !== undefined &&
			cache !== undefined &&
			cache.stat.size === snapshot.size &&
			cache.stat.mtimeMs === snapshot.mtimeMs &&
			cache.stat.ino === snapshot.ino
		) {
			return cache.edges;
		}
		const edges = this.replaySync();
		if (snapshot !== undefined) {
			this.edgeCache = { stat: snapshot, edges };
		}
		return edges;
	}

	private replaySync(): Map<string, RlmLedgerEdge> {
		const edges = new Map<string, RlmLedgerEdge>();
		const records = this.eventLog.replaySync((line, index) => {
			const record = parseLedgerLine(line, index);
			if (record === undefined) {
				this.log(`RLM ledger: skipped record with unknown op on line ${index + 1}`);
			}
			return record;
		});
		for (const record of records) {
			if (record.op === "meta") continue;
			// V2 composite admission records are not v1 topology edges; skip them
			// before edgeKey (they carry no childId/child).
			if (record.op === RLM_LEDGER_ADMIT_OP) continue;
			const key = edgeKey(record.childId, record.child);
			switch (record.op) {
				case "spawn":
					edges.set(key, {
						childId: record.childId,
						parent: record.parent,
						child: record.child,
						depth: record.depth,
						name: record.name,
					});
					break;
				case "rename": {
					const existing = edges.get(key);
					if (existing) existing.name = record.name;
					break;
				}
				case "delete": {
					const existing = edges.get(key);
					if (existing) existing.deleted = record.reason;
					break;
				}
			}
		}
		return edges;
	}
}

// The catalog scan never visits session-artifacts, where RLM children persist:
// without this merge a passivated descendant's row (and its spend) survives only
// as long as some resident roster remembers it.
export async function withPassiveRlmDescendantInfos(
	savedSessions: SessionInfo[],
	ledger: RlmSpawnLedger,
	options: { cwd?: string; onSession?: (info: SessionInfo) => void; log?: (message: string) => void } = {},
): Promise<SessionInfo[]> {
	const sessions = [...savedSessions];
	const seen = new Set(savedSessions.map((info) => canonicalSessionPath(info.path)));
	let edges: RlmLedgerEdge[];
	try {
		edges = await ledger.liveEdges();
	} catch (error) {
		// A broken ledger must not take the whole catalog down with it.
		options.log?.(`Could not merge passive RLM descendants: ${String(error)}`);
		return sessions;
	}
	for (const edge of edges) {
		const childPath = canonicalSessionPath(edge.child);
		if (seen.has(childPath)) continue;
		seen.add(childPath);
		const info = await readSessionInfo(childPath);
		if (!info) continue;
		if (options.cwd !== undefined && (!info.cwd || resolve(info.cwd) !== resolve(options.cwd))) continue;
		// The ledger edge is the authoritative topology (family() semantics); a fork
		// can leave the transcript header pointing at a dead ancestor path.
		const merged: SessionInfo = {
			...info,
			parentSessionPath: edge.parent,
			rlmDepth: edge.depth,
		};
		sessions.push(merged);
		options.onSession?.(merged);
	}
	return sessions;
}

// Shared user-delete policy: only a readable no-parent transcript is positively top-level; children and
// unknown targets tombstone via the ledger BEFORE the file delete (a tombstoned-but-undeleted file is
// the accepted orphan of a failed delete).
export async function tombstoneSavedSessionDelete(
	ledger: RlmSpawnLedger,
	sessionPath: string,
	knownSummary: { runtimeKind?: "top-level" | "subagent" } | undefined,
): Promise<{ deletedInfo: SessionInfo | undefined; ledgerEdge: RlmLedgerEdge | undefined }> {
	const deletedPath = canonicalSessionPath(sessionPath);
	const deletedInfo = (await readSessionInfo(sessionPath).catch(() => null)) ?? undefined;
	const knownChild =
		knownSummary?.runtimeKind === "subagent" ||
		deletedInfo?.parentSessionPath !== undefined ||
		(deletedInfo?.rlmDepth ?? 0) > 0;
	const positivelyTopLevel = !knownChild && (knownSummary !== undefined || deletedInfo !== undefined);
	if (positivelyTopLevel) return { deletedInfo, ledgerEdge: undefined };
	const edges = await ledger.edges();
	// Tombstone every matching edge: a duplicate edge for the path (corrupt or raced appends) left
	// live would resurrect a later recreation at that path as a subagent.
	const matching = edges.filter((edge) => canonicalSessionPath(edge.child) === deletedPath);
	for (const edge of matching) {
		await ledger.appendDelete({ childId: edge.childId, child: sessionPath, reason: "user" });
	}
	return { deletedInfo, ledgerEdge: matching[0] };
}

// ============================================================================
// Workflow V2 Slice 3 — composite topology admission (capability UNAVAILABLE)
// ============================================================================
//
// This section adds the closed V2 composite admission record and its replay
// indexes to the same per-sessions-dir ledger file. It is dormant: no route,
// daemon path, or capability negotiation reaches it (see
// workflow-v2-capability.ts, which returns CAPABILITY_UNAVAILABLE). It exists
// only behind unavailable wiring for Slice 3 conformance work.
//
// Authority split (docs/WORKFLOW-V2-SLICE3.md §3): the supervisor is the sole
// OS-fenced writer of native RLM topology. One durable composite `admit`
// record atomically reserves the request receipt, direct-parent edge, child
// identity, and initial-turn identity BEFORE any child filesystem, runtime, or
// provider effect (§4). Workers open a read-only reader and never admit.
//
// Durability/atomicity model: one composite record is a single JSONL line
// appended through EventLog (single O_APPEND write + fsync), then VERIFIED by
// re-reading. A crash before the terminating newline leaves an uncommitted
// torn tail that EventLog skips on read and truncates on the next append — the
// record was never durable and never launched, so it is "absent, retry once"
// (§8.1). An interior corrupt/contradictory record fails closed as
// ADMISSION_UNKNOWN. Composite lines can far exceed PIPE_BUF, so single-write
// atomicity holds ONLY because the supervisor is the sole writer; global
// sole-writer routing of every topology mutation is an enablement precondition
// (§4.2) and is NOT yet globally enforced while V1 writers remain.
//
// No settlement, result, usage, capture, or cursor data lives here (§3): those
// belong to the per-worker retained journal. This module owns admission only.

/** V2 composite-record op/version. Distinct from the V1 v:1 topology records. */
export const RLM_LEDGER_ADMIT_VERSION = 2 as const;
export const RLM_LEDGER_ADMIT_OP = "admit" as const;

const ADMISSION_RECEIPT_PROTOCOL = "prime.workflow.retained-admission-receipt/v2-slice3" as const;
const ADMISSION_RECEIPT_PAYLOAD_PROTOCOL = "prime.workflow.retained-admission-receipt-payload/v2-slice3" as const;
const MATERIALIZE_COMMAND_PROTOCOL = "prime.workflow.retained-materialize/v2-slice3" as const;
const TOOLS_NONE_PROFILE = "workflow-v2-tools-none-v1" as const;

export type Slice3ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";

/** Complete supervisor/worker/route generation fence (schema $defs/fence). */
export interface Slice3Fence {
	supervisorGeneration: number;
	supervisorIncarnationId: string;
	workerId: string;
	workerGeneration: number;
	workerIncarnationId: string;
	routeRevision: number;
}

/** Non-self-referential admission-receipt payload (schema $defs/admissionReceiptPayload). */
export interface AdmissionReceiptPayload {
	authorityId: string;
	rootSessionId: string;
	parentSessionId: string;
	workflowRunId: string;
	nodeId: string;
	attemptId: string;
	workflowChildId: string;
	protocol: typeof ADMISSION_RECEIPT_PAYLOAD_PROTOCOL;
	requestId: string;
	requestDigest: string;
	rlmChildId: string;
	turnId: string;
	effectiveModel: string;
	profile: typeof TOOLS_NONE_PROFILE;
	tools: "none";
	maxTurns: 1;
	effectiveThinkingLevel: Slice3ThinkingLevel;
	admissionSequence: number;
	operation: "child.admit";
	fence: Slice3Fence;
}

/** Admission-receipt envelope (schema $defs/admissionReceipt). digest = SHA-256(RFC 8785(payload)). */
export interface AdmissionReceipt {
	protocol: typeof ADMISSION_RECEIPT_PROTOCOL;
	payload: AdmissionReceiptPayload;
	digest: string;
}

/** One composite admit record (schema $defs/admitRecord). Single durable line. */
export interface RlmLedgerAdmitRecord {
	v: typeof RLM_LEDGER_ADMIT_VERSION;
	op: typeof RLM_LEDGER_ADMIT_OP;
	at: string;
	fence: Slice3Fence;
	authorityId: string;
	rootSessionId: string;
	parentSessionId: string;
	parentSessionPath: string;
	requestId: string;
	requestDigest: string;
	workflowRunId: string;
	nodeId: string;
	attemptId: string;
	workflowChildId: string;
	rlmChildId: string;
	childSessionPath: string;
	childArtifactDir: string;
	depth: number;
	name: string;
	turnId: string;
	promptUtf8Bytes: number;
	promptDigest: string;
	canonicalRequest: string;
	effectiveModel: string;
	profile: typeof TOOLS_NONE_PROFILE;
	tools: "none";
	maxTurns: 1;
	receipt: string;
	receiptDigest: string;
	effectiveThinkingLevel: Slice3ThinkingLevel;
	admissionSequence: number;
	decodedReceipt: AdmissionReceipt;
}

/** Generation-bound materialization command (schema $defs/materializeCommand). */
export interface Slice3MaterializeCommand {
	protocol: typeof MATERIALIZE_COMMAND_PROTOCOL;
	commandId: string;
	fence: Slice3Fence;
	requestId: string;
	requestDigest: string;
	admissionReceiptDigest: string;
	rlmChildId: string;
	turnId: string;
	canonicalRequest: string;
	effectiveModel: string;
	profile: typeof TOOLS_NONE_PROFILE;
	tools: "none";
	maxTurns: 1;
	effectiveThinkingLevel: Slice3ThinkingLevel;
	authorityId: string;
	rootSessionId: string;
	parentSessionId: string;
	workflowRunId: string;
	nodeId: string;
	attemptId: string;
	workflowChildId: string;
	admissionSequence: number;
}

/**
 * Codec seam owned by core/workflow-v2-slice3-codec.ts
 * (WorkflowV2Slice3SemanticValidator/v1). Injected so the ledger never carries
 * a second canonicalizer/decoder (§9: the codec is the sole authority). The
 * ledger owns only durable append, replay, and idempotency resolution.
 */
export interface Slice3LedgerCodec {
	/** RFC 8785 canonical UTF-8 bytes of a JSON value. */
	canonicalize(value: unknown): Buffer;
	/** "sha256:" + lowercase hex of SHA-256 over bytes. */
	digest(bytes: Buffer): string;
	/** Canonical RFC 4648 padded base64 of bytes. */
	encodeCanonicalBytes(bytes: Buffer): string;
	/**
	 * Strict decode + full §12 semantic validation of one composite admit
	 * record: recomputes every digest and byte count, proves the decoded
	 * receipt envelope/payload equal their stored canonical bytes and bind the
	 * enclosing record, and rejects duplicate keys, unknown fields, invalid
	 * UTF-8, noncanonical numbers/strings, and bound violations. Throws
	 * Slice3CodecError on any violation; never returns a partially valid record.
	 */
	validateAdmitRecord(record: unknown): RlmLedgerAdmitRecord;
}

/** Typed error the codec throws for any structural/semantic violation. */
export class Slice3CodecError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "Slice3CodecError";
	}
}

export type AdmitConflictCode = "REQUEST_ID_CONFLICT" | "ADMISSION_UNKNOWN" | "STORE_CORRUPT";

export type AdmitOutcome =
	| { disposition: "admitted" | "replayed"; record: RlmLedgerAdmitRecord }
	| { disposition: "conflict" | "rejected"; code: AdmitConflictCode; requestId: string };

/**
 * Everything the authenticated supervisor derives for one child.admit before
 * touching the ledger (§4.2 steps 1-7). The caller supplies authority, parent,
 * route, fence, preallocated child/turn identity, deterministic paths, the
 * resolved model/profile, and the already-canonicalized request bytes/digest.
 * The ledger allocates only the admission sequence, receipt, and `at`, then
 * writes the one composite record.
 */
export interface AdmitInput {
	fence: Slice3Fence;
	authorityId: string;
	rootSessionId: string;
	parentSessionId: string;
	parentSessionPath: string;
	requestId: string;
	requestDigest: string;
	workflowRunId: string;
	nodeId: string;
	attemptId: string;
	workflowChildId: string;
	rlmChildId: string;
	childSessionPath: string;
	childArtifactDir: string;
	depth: number;
	name: string;
	turnId: string;
	promptUtf8Bytes: number;
	promptDigest: string;
	/** Canonical request bytes (the supervisor built these from the strict child.admit). */
	canonicalRequest: Buffer;
	effectiveModel: string;
	effectiveThinkingLevel: Slice3ThinkingLevel;
}

const RLM_COMPOSITE_ADMISSION_QUEUE_INIT = Promise.resolve();

/** Scoped idempotency key (§4.2 step 4): (authorityId, parentSessionId, "child.admit", requestId). */
function admissionScopeKey(authorityId: string, parentSessionId: string, requestId: string): string {
	return `${authorityId}\u0000${parentSessionId}\u0000child.admit\u0000${requestId}`;
}

/**
 * V2 composite-admission ledger over the same per-sessions-dir file as the V1
 * RlmSpawnLedger. Two modes: "writer" (the OS-fenced supervisor) may admit;
 * "reader" (workers) may only read and validate. Every admit re-checks the
 * injected writer fence at the append boundary, so a preflight check can never
 * authorize a later durable write.
 */
export class RlmCompositeAdmissionLedger {
	private readonly path: string;
	private readonly eventLog: EventLog;
	private queue: Promise<unknown> = RLM_COMPOSITE_ADMISSION_QUEUE_INIT;

	constructor(
		agentDir: string,
		sessionsDir: string,
		private readonly options: {
			mode: "writer" | "reader";
			codec: Slice3LedgerCodec;
			/**
			 * Proves the caller still holds OS endpoint ownership + the current
			 * supervisor generation. Throws to fence a stale writer. Called at
			 * construction-independent append time, never cached across awaits.
			 */
			assertWriterFence?: () => void;
			log?: (message: string) => void;
		},
	) {
		this.path = rlmLedgerPath(agentDir, sessionsDir);
		this.eventLog = new EventLog(this.path, {
			maxBytes: RLM_LEDGER_MAX_BYTES,
			maxRecords: RLM_LEDGER_MAX_RECORDS,
			log: (message) => this.options.log?.(`RLM composite admission: ${message}`),
		});
	}

	get ledgerPath(): string {
		return this.path;
	}

	/**
	 * Admit one child.admit. Returns the durable composite record on success
	 * (disposition "admitted") or the byte-identical stored record on replay
	 * (disposition "replayed"); a conflicting/uncertain scope returns a closed
	 * typed conflict WITHOUT allocating or writing.
	 */
	admit(input: AdmitInput): Promise<AdmitOutcome> {
		return this.enqueue(() => this.admitUnlocked(input));
	}

	/** Read-only resolution of a scoped key (workers validate their bound receipt through this). */
	lookup(authorityId: string, parentSessionId: string, requestId: string): Promise<RlmLedgerAdmitRecord | undefined> {
		return this.enqueue(() => {
			const index = this.replayAdmissionsSync();
			const found = index.get(admissionScopeKey(authorityId, parentSessionId, requestId));
			return found?.record;
		});
	}

	/** All admitted records in this ledger (validated), for reconciliation/inspection. */
	admissions(): Promise<RlmLedgerAdmitRecord[]> {
		return this.enqueue(() => [...this.replayAdmissionsSync().values()].map((entry) => entry.record));
	}

	private enqueue<T>(fn: () => T): Promise<T> {
		const next = this.queue.then(() => fn());
		this.queue = next.catch(() => undefined);
		return next;
	}

	private admitUnlocked(input: AdmitInput): AdmitOutcome {
		if (this.options.mode !== "writer") {
			// A reader can never admit — only the OS-fenced supervisor writes topology.
			throw new Slice3CodecError("RLM composite admission: admit called on a read-only ledger");
		}
		// Fence recheck at the mutation boundary (§5: a preflight cannot authorize a later write).
		this.options.assertWriterFence?.();

		const scopeKey = admissionScopeKey(input.authorityId, input.parentSessionId, input.requestId);

		// Replay the bounded canonical ledger. Torn tail is skipped by EventLog
		// (uncommitted, retried); interior corruption/contradiction fences.
		let index: Map<string, AdmissionIndexEntry>;
		try {
			index = this.replayAdmissionsSync();
		} catch {
			return { disposition: "rejected", code: "ADMISSION_UNKNOWN", requestId: input.requestId };
		}

		const existing = index.get(scopeKey);
		if (existing) {
			if (existing.corrupt) {
				return { disposition: "rejected", code: "STORE_CORRUPT", requestId: input.requestId };
			}
			if (existing.record.requestDigest === input.requestDigest) {
				// Same scope + same request bytes: return the byte-identical stored receipt.
				return { disposition: "replayed", record: existing.record };
			}
			// Same request ID, different canonical bytes: conflict BEFORE any allocation/effect.
			return { disposition: "conflict", code: "REQUEST_ID_CONFLICT", requestId: input.requestId };
		}

		// Allocate the supervisor admission sequence (monotone, sole-writer + serialized queue).
		const admissionSequence = this.nextAdmissionSequenceSync(index);

		let record: RlmLedgerAdmitRecord;
		try {
			record = this.buildAdmitRecord(input, admissionSequence);
			// Gate on the sole codec/semantic validator BEFORE any durable byte.
			this.options.codec.validateAdmitRecord(record);
		} catch {
			// A record this validator would refuse must never be written.
			return { disposition: "rejected", code: "ADMISSION_UNKNOWN", requestId: input.requestId };
		}

		// Fence recheck immediately before the durable write.
		this.options.assertWriterFence?.();
		this.eventLog.appendSync([record], { durable: true });

		// Verify: re-read and confirm the exact record is durable (detects a torn
		// or short write). Success is reported ONLY after the record replays with
		// a matching receipt digest; otherwise the outcome is unknown and no
		// launch may proceed.
		let verified: Map<string, AdmissionIndexEntry>;
		try {
			verified = this.replayAdmissionsSync();
		} catch {
			return { disposition: "rejected", code: "ADMISSION_UNKNOWN", requestId: input.requestId };
		}
		const stored = verified.get(scopeKey);
		if (!stored || stored.corrupt || stored.record.receiptDigest !== record.receiptDigest) {
			return { disposition: "rejected", code: "ADMISSION_UNKNOWN", requestId: input.requestId };
		}
		return { disposition: "admitted", record: stored.record };
	}

	private nextAdmissionSequenceSync(index: Map<string, AdmissionIndexEntry>): number {
		let max = -1;
		for (const entry of index.values()) {
			if (!entry.corrupt && entry.record.admissionSequence > max) {
				max = entry.record.admissionSequence;
			}
		}
		return max + 1;
	}

	private buildAdmitRecord(input: AdmitInput, admissionSequence: number): RlmLedgerAdmitRecord {
		const codec = this.options.codec;
		const payload: AdmissionReceiptPayload = {
			authorityId: input.authorityId,
			rootSessionId: input.rootSessionId,
			parentSessionId: input.parentSessionId,
			workflowRunId: input.workflowRunId,
			nodeId: input.nodeId,
			attemptId: input.attemptId,
			workflowChildId: input.workflowChildId,
			protocol: ADMISSION_RECEIPT_PAYLOAD_PROTOCOL,
			requestId: input.requestId,
			requestDigest: input.requestDigest,
			rlmChildId: input.rlmChildId,
			turnId: input.turnId,
			effectiveModel: input.effectiveModel,
			profile: TOOLS_NONE_PROFILE,
			tools: "none",
			maxTurns: 1,
			effectiveThinkingLevel: input.effectiveThinkingLevel,
			admissionSequence,
			operation: "child.admit",
			fence: input.fence,
		};
		// Non-self-referential digest: SHA-256(RFC 8785(payload)).
		const receiptDigest = codec.digest(codec.canonicalize(payload));
		const decodedReceipt: AdmissionReceipt = {
			protocol: ADMISSION_RECEIPT_PROTOCOL,
			payload,
			digest: receiptDigest,
		};
		const receiptBytes = codec.canonicalize(decodedReceipt);
		return {
			v: RLM_LEDGER_ADMIT_VERSION,
			op: RLM_LEDGER_ADMIT_OP,
			at: nowIso(),
			fence: input.fence,
			authorityId: input.authorityId,
			rootSessionId: input.rootSessionId,
			parentSessionId: input.parentSessionId,
			parentSessionPath: input.parentSessionPath,
			requestId: input.requestId,
			requestDigest: input.requestDigest,
			workflowRunId: input.workflowRunId,
			nodeId: input.nodeId,
			attemptId: input.attemptId,
			workflowChildId: input.workflowChildId,
			rlmChildId: input.rlmChildId,
			childSessionPath: input.childSessionPath,
			childArtifactDir: input.childArtifactDir,
			depth: input.depth,
			name: input.name,
			turnId: input.turnId,
			promptUtf8Bytes: input.promptUtf8Bytes,
			promptDigest: input.promptDigest,
			canonicalRequest: codec.encodeCanonicalBytes(input.canonicalRequest),
			effectiveModel: input.effectiveModel,
			profile: TOOLS_NONE_PROFILE,
			tools: "none",
			maxTurns: 1,
			receipt: codec.encodeCanonicalBytes(receiptBytes),
			receiptDigest,
			effectiveThinkingLevel: input.effectiveThinkingLevel,
			admissionSequence,
			decodedReceipt,
		};
	}

	/**
	 * Build the generation-bound materialization command for an admitted record
	 * (§5). The command carries the exact fence, receipt digest, and every
	 * bound identity; the worker recomputes and validates it and allocates
	 * nothing. `commandId` binds one delivery; the supervisor may redeliver only
	 * the same generation-bound command for the same admitted record.
	 */
	buildMaterializeCommand(record: RlmLedgerAdmitRecord, commandId: string): Slice3MaterializeCommand {
		return {
			protocol: MATERIALIZE_COMMAND_PROTOCOL,
			commandId,
			fence: record.fence,
			requestId: record.requestId,
			requestDigest: record.requestDigest,
			admissionReceiptDigest: record.receiptDigest,
			rlmChildId: record.rlmChildId,
			turnId: record.turnId,
			canonicalRequest: record.canonicalRequest,
			effectiveModel: record.effectiveModel,
			profile: TOOLS_NONE_PROFILE,
			tools: "none",
			maxTurns: 1,
			effectiveThinkingLevel: record.effectiveThinkingLevel,
			authorityId: record.authorityId,
			rootSessionId: record.rootSessionId,
			parentSessionId: record.parentSessionId,
			workflowRunId: record.workflowRunId,
			nodeId: record.nodeId,
			attemptId: record.attemptId,
			workflowChildId: record.workflowChildId,
			admissionSequence: record.admissionSequence,
		};
	}

	/**
	 * Replay v2 admit records into a scoped index. V1 records and non-admit v2
	 * ops are ignored (legacy read compatibility). Each admit record is
	 * revalidated through the codec; a semantically invalid or scope-colliding
	 * record marks its scope corrupt so admission fences instead of allocating.
	 */
	private replayAdmissionsSync(): Map<string, AdmissionIndexEntry> {
		const index = new Map<string, AdmissionIndexEntry>();
		const records = this.eventLog.replaySync((line, lineIndex) => parseAdmitLine(line, lineIndex));
		for (const raw of records) {
			if (raw === undefined) continue;
			let record: RlmLedgerAdmitRecord;
			try {
				record = this.options.codec.validateAdmitRecord(raw);
			} catch {
				// A stored record the validator rejects is corruption: mark its
				// scope corrupt so the scope fences rather than silently dropping.
				const key = admissionScopeKey(
					stringOr(raw.authorityId),
					stringOr(raw.parentSessionId),
					stringOr(raw.requestId),
				);
				index.set(key, { record: undefined as never, corrupt: true });
				continue;
			}
			const key = admissionScopeKey(record.authorityId, record.parentSessionId, record.requestId);
			const existing = index.get(key);
			if (existing) {
				if (existing.corrupt) continue;
				// Contradictory duplicate for the same scope: fence (§4.2 step 3).
				if (existing.record.receiptDigest !== record.receiptDigest) {
					index.set(key, { record: undefined as never, corrupt: true });
				}
				// Byte-identical duplicate is idempotent: keep the first.
				continue;
			}
			index.set(key, { record, corrupt: false });
		}
		return index;
	}
}

interface AdmissionIndexEntry {
	record: RlmLedgerAdmitRecord;
	corrupt: boolean;
}

function stringOr(value: unknown): string {
	return typeof value === "string" ? value : "";
}

/**
 * Parse one ledger line for the V2 admission reader. Returns a raw v2 admit
 * object (validated deeply by the codec later), or undefined for v1/meta and
 * non-admit v2 ops. An interior line that claims to be a v2 admit but lacks the
 * scoped-key identity strings fails closed (throws) — consistent with the
 * EventLog interior-malformed rule.
 */
function parseAdmitLine(line: string, index: number): RlmLedgerAdmitRawLine | undefined {
	let parsed: unknown;
	try {
		parsed = JSON.parse(line);
	} catch (error) {
		throw new Error(
			`Malformed RLM ledger line ${index + 1}: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
	if (!parsed || typeof parsed !== "object") {
		throw new Error(`Malformed RLM ledger line ${index + 1}: not an object`);
	}
	const record = parsed as { v?: unknown; op?: unknown };
	if (record.v !== RLM_LEDGER_ADMIT_VERSION) return undefined;
	if (record.op !== RLM_LEDGER_ADMIT_OP) return undefined;
	const raw = parsed as RlmLedgerAdmitRawLine;
	if (
		typeof raw.authorityId !== "string" ||
		typeof raw.parentSessionId !== "string" ||
		typeof raw.requestId !== "string" ||
		typeof raw.receiptDigest !== "string"
	) {
		throw new Error(`Malformed RLM ledger line ${index + 1}: v2 admit missing scoped-key identity`);
	}
	return raw;
}

interface RlmLedgerAdmitRawLine {
	v: number;
	op: string;
	authorityId: unknown;
	parentSessionId: unknown;
	requestId: unknown;
	receiptDigest: unknown;
	[key: string]: unknown;
}
