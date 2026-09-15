/**
 * Workflow V2 Slice 4 — durable per-root SQLite store (owner-only), the sole
 * logical writer of the controller's run aggregate.
 *
 * Normative authority: docs/WORKFLOW-V2.md §7 (SQLite/outbox/inbox/cursors +
 * capacity + retention), §6 (commands + idempotency + event vocabulary), §5/§11
 * (state model + the one projection validator). Closed shapes:
 * docs/api/workflow-v2.schema.json. Slice scope: docs/reviews/v2-slices-4-9-scope.md
 * (Slice 4). Discipline mirrors src/modes/daemon/supervisor-control-db.ts.
 *
 * This module owns durability MECHANISM only; it holds NO controller policy
 * (scheduling, acceptance choice, budget math — slices 5-7). A caller supplies a
 * reducer-validated {@link CommandEffect} (the controller events + outbox
 * operations a command emits); the store atomically checks capacity, idempotency,
 * and fences, folds the events through the pure reducer (validating every
 * projection), and persists normalized rows + gap-free controller facts + an
 * immutable command receipt + stable outbox operation IDs in one
 * `BEGIN IMMEDIATE` transaction with no host/provider/blob/FS latency inside it.
 *
 * DORMANT (§7/scope): no production/controller path imports this module; the
 * Workflow V2 capability stays CAPABILITY_UNAVAILABLE. Open failures (driver
 * absent, WAL/FULL unavailable) are reported as a capability verdict, never
 * thrown into a live path; every hardening/integrity/torn/fence violation on the
 * V2 path fails closed via {@link WorkflowV2StoreError}.
 */

import { createHash } from "node:crypto";
import type { Stats } from "node:fs";
import { chmodSync, closeSync, fsyncSync, lstatSync, mkdirSync, openSync, realpathSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import type { DatabaseSync } from "node:sqlite";
import {
	type AttemptRecord,
	assertRunTerminalizedOutcomeEquality,
	buildDefinitionGraph,
	type DefinitionGraph,
	type RunAggregate,
	reduceFact,
	revalidateAggregate,
	validateProjectionSemantics,
} from "./workflow-v2-reducer.js";
import {
	canonicalJson,
	decodeWorkflowV2Definition,
	decodeWorkflowV2PublicRequest,
	type WorkflowV2Value,
} from "./workflow-v2-wire.js";

// ---------------------------------------------------------------------------
// Identity, modes, limits (§7).
// ---------------------------------------------------------------------------

/** SQLite `application_id` tag: 0x57325354 = "W2ST" (Workflow V2 STore). */
export const STORE_APPLICATION_ID = 0x57325354;
/** `PRAGMA user_version` migration counter; a newer value than this fails closed. */
export const STORE_USER_VERSION = 1;
export const STORE_FILE_NAME = "v2.sqlite";
const STORE_DIR_MODE = 0o700;
const STORE_FILE_MODE = 0o600;
const BUSY_TIMEOUT_MS = 5_000;
const MAX_SAFE = 9007199254740991;

const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const DIGEST_PATTERN = /^sha256:[0-9a-f]{64}$/;

/** Per-root capacity quotas (§7 minimum profile). */
export const STORE_CAPACITY = Object.freeze({
	maxRuns: 1_024,
	maxEvents: 131_072,
	maxCommands: 16_384,
	maxOperations: 16_384,
	maxHostInbox: 262_144,
	maxSettlements: 16_384,
	maxTombstones: 16_384,
	maxTextBytes: 268_435_456, // 256 MiB retained prompt/result UTF-8
	maxDbBytes: 536_870_912, // 512 MiB DB + WAL
	highWaterFraction: 0.9,
});

export type StoreCapability = "available" | "unavailable";

export interface StoreCapabilityProbe {
	capability: StoreCapability;
	reason: string | null;
}

/** Fail-closed error for every hardening/integrity/fence/transaction violation. */
export class WorkflowV2StoreError extends Error {
	constructor(
		readonly code: string,
		message: string,
	) {
		super(message);
		this.name = "WorkflowV2StoreError";
	}
}

function failClosed(code: string, message: string): never {
	// Prefix the code so message-level matchers and logs surface the normative code.
	throw new WorkflowV2StoreError(code, `${code}: ${message}`);
}

interface SqliteModule {
	DatabaseSync: new (path: string, options?: Record<string, unknown>) => DatabaseSync;
}

let cachedSqlite: SqliteModule | null | undefined;

function loadSqlite(): SqliteModule | null {
	if (cachedSqlite !== undefined) return cachedSqlite;
	try {
		const requireFromHere = createRequire(import.meta.url);
		const mod = requireFromHere("node:sqlite") as Partial<SqliteModule>;
		cachedSqlite = typeof mod.DatabaseSync === "function" ? (mod as SqliteModule) : null;
	} catch {
		cachedSqlite = null;
	}
	return cachedSqlite;
}

/** For tests only: reset the memoized driver probe. */
export function resetWorkflowV2StoreDriverCacheForTest(): void {
	cachedSqlite = undefined;
}

function readScalar(db: DatabaseSync, sql: string, column: string): unknown {
	const row = db.prepare(sql).get();
	return row ? (row as Record<string, unknown>)[column] : undefined;
}

/**
 * Capability probe (§7): load node:sqlite, open a throwaway WAL+FULL file DB,
 * and run one BEGIN IMMEDIATE write. ANY failure ⇒ unavailable; never throws,
 * never falls back to a JSON/mutex store. Windows is unavailable unconditionally
 * (owner-only single-writer file semantics unproven for this profile).
 */
export function probeWorkflowV2StoreCapability(): StoreCapabilityProbe {
	if (process.platform === "win32") return { capability: "unavailable", reason: "windows_unsupported" };
	const sqlite = loadSqlite();
	if (!sqlite) return { capability: "unavailable", reason: "driver_absent" };
	let db: DatabaseSync | undefined;
	let probeDir: string | undefined;
	try {
		probeDir = mkdirSyncOwnerOnly(join(tmpProbeRoot(), `wf-v2-store-probe-${process.pid}-${Date.now()}`));
		const probePath = join(probeDir, STORE_FILE_NAME);
		db = new sqlite.DatabaseSync(probePath, { timeout: 0 });
		const journal = readScalar(db, "PRAGMA journal_mode = WAL", "journal_mode");
		if (String(journal).toLowerCase() !== "wal") return { capability: "unavailable", reason: "wal_unavailable" };
		db.exec("PRAGMA synchronous = FULL");
		if (Number(readScalar(db, "PRAGMA synchronous", "synchronous")) !== 2) {
			return { capability: "unavailable", reason: "synchronous_full_unavailable" };
		}
		db.exec("CREATE TABLE probe (a INTEGER PRIMARY KEY, b INTEGER NOT NULL) STRICT");
		db.exec("BEGIN IMMEDIATE");
		const changes = db.prepare("INSERT INTO probe (a, b) VALUES (1, 1)").run().changes;
		if (Number(changes) !== 1) {
			db.exec("ROLLBACK");
			return { capability: "unavailable", reason: "immediate_write_unavailable" };
		}
		db.exec("COMMIT");
		db.close();
		db = undefined;
		return { capability: "available", reason: null };
	} catch {
		return { capability: "unavailable", reason: "probe_failed" };
	} finally {
		if (db) {
			try {
				db.close();
			} catch {
				// verdict already stands
			}
		}
		if (probeDir) removeTreeQuietly(probeDir);
	}
}

// ---------------------------------------------------------------------------
// Filesystem hardening (self-contained; mirrors supervisor-control-db.ts).
// ---------------------------------------------------------------------------

function tmpProbeRoot(): string {
	const suffix = typeof process.getuid === "function" ? String(process.getuid()) : "user";
	return join(tmpdir(), `prime-agent-${suffix}`);
}
function mkdirSyncOwnerOnly(path: string): string {
	mkdirSync(path, { recursive: true, mode: STORE_DIR_MODE });
	return path;
}
function removeTreeQuietly(path: string): void {
	try {
		rmSync(path, { recursive: true, force: true });
	} catch {
		// best effort
	}
}
function isErrnoCode(error: unknown, code: string): boolean {
	return (error as NodeJS.ErrnoException | undefined)?.code === code;
}
function canonicalizeExistingDir(path: string): string {
	return realpathSync.native(resolve(path));
}
function assertNoSymlinkUnderRoot(canonicalRoot: string, target: string): void {
	const abs = resolve(target);
	if (!abs.startsWith(canonicalRoot + sep) && abs !== canonicalRoot) {
		failClosed("store_path_escape", `store path escapes root: ${target}`);
	}
	const relative = abs.slice(canonicalRoot.length);
	const components = relative.split(sep).filter((p) => p.length > 0);
	let current = canonicalRoot;
	for (const component of components) {
		current = join(current, component);
		let stat: Stats;
		try {
			stat = lstatSync(current);
		} catch (error) {
			if (isErrnoCode(error, "ENOENT")) return;
			throw error;
		}
		if (stat.isSymbolicLink()) failClosed("store_symlink_component", `symlink component under root: ${current}`);
	}
}
function assertOwnedPrivate(path: string, stat: Stats, allowedMode: number): void {
	if (typeof process.getuid === "function" && stat.uid !== process.getuid()) {
		failClosed("store_wrong_owner", `store path not owned by current user: ${path}`);
	}
	if ((stat.mode & 0o077) !== 0) failClosed("store_mode_too_open", `store path is group/other-accessible: ${path}`);
	if ((stat.mode & 0o777 & ~allowedMode) !== 0) {
		failClosed("store_mode_too_open", `store path mode exceeds ${allowedMode.toString(8)}: ${path}`);
	}
}
function assertHardenedDir(root: string): string {
	let stat: Stats;
	try {
		stat = lstatSync(root);
	} catch (error) {
		if (isErrnoCode(error, "ENOENT")) failClosed("store_root_missing", `store root missing: ${root}`);
		throw error;
	}
	if (stat.isSymbolicLink()) failClosed("store_root_symlink", `store root is a symlink: ${root}`);
	if (!stat.isDirectory()) failClosed("store_root_not_dir", `store root is not a directory: ${root}`);
	assertOwnedPrivate(root, stat, STORE_DIR_MODE);
	return canonicalizeExistingDir(root);
}
function assertHardenedFile(canonicalRoot: string, filePath: string): void {
	assertNoSymlinkUnderRoot(canonicalRoot, filePath);
	let stat: Stats;
	try {
		stat = lstatSync(filePath);
	} catch (error) {
		if (isErrnoCode(error, "ENOENT")) return;
		throw error;
	}
	if (stat.isSymbolicLink()) failClosed("store_file_symlink", `store file is a symlink: ${filePath}`);
	if (!stat.isFile()) failClosed("store_not_regular_file", `store file is not a regular file: ${filePath}`);
	assertOwnedPrivate(filePath, stat, STORE_FILE_MODE);
	const canonicalFile = realpathSync.native(filePath);
	if (canonicalFile !== join(canonicalRoot, STORE_FILE_NAME)) {
		failClosed("store_path_escape", `store realpath escapes root: ${canonicalFile}`);
	}
}
function fsyncDir(dir: string): void {
	let descriptor: number | undefined;
	try {
		descriptor = openSync(dir, "r");
		fsyncSync(descriptor);
	} finally {
		if (descriptor !== undefined) closeSync(descriptor);
	}
}
function nowIso(clock?: () => string): string {
	return clock ? clock() : new Date().toISOString();
}
function assertId(value: string, field: string): string {
	if (!ID_PATTERN.test(value)) failClosed("store_invalid_id", `invalid ${field}: ${value}`);
	return value;
}
function assertDigest(value: string, field: string): string {
	if (!DIGEST_PATTERN.test(value)) failClosed("store_invalid_digest", `invalid ${field}: ${value}`);
	return value;
}
function sha256Hex(input: string): string {
	return `sha256:${createHash("sha256").update(input, "utf8").digest("hex")}`;
}

// ---------------------------------------------------------------------------
// Schema: the 15 minimum tables (§7) + an internal migration ledger. All STRICT.
// ---------------------------------------------------------------------------

const BASE_DDL = [
	// store_meta: singleton controller-scope + counters.
	"CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;",
	// definitions: canonical immutable DAG bytes bound by digest.
	"CREATE TABLE definitions (definition_digest TEXT PRIMARY KEY, canonical_bytes TEXT NOT NULL, utf8_bytes INTEGER NOT NULL, created_at TEXT NOT NULL) STRICT;",
	// runs: normalized run projection + fences + host cursor.
	"CREATE TABLE runs (" +
		"run_id TEXT PRIMARY KEY, definition_digest TEXT NOT NULL REFERENCES definitions(definition_digest), " +
		"revision INTEGER NOT NULL, controller_epoch INTEGER NOT NULL, cancel_epoch INTEGER NOT NULL, " +
		"phase TEXT NOT NULL, intent TEXT NOT NULL, outcome TEXT, conditions TEXT NOT NULL, host_cursor TEXT, " +
		"terminal_outcome TEXT, terminal_digest TEXT, erased INTEGER NOT NULL DEFAULT 0, " +
		"created_at TEXT NOT NULL, updated_at TEXT NOT NULL) STRICT;",
	// nodes: normalized node projections.
	"CREATE TABLE nodes (run_id TEXT NOT NULL REFERENCES runs(run_id), node_id TEXT NOT NULL, phase TEXT NOT NULL, " +
		"intent TEXT NOT NULL, outcome TEXT, conditions TEXT NOT NULL, PRIMARY KEY (run_id, node_id)) STRICT;",
	// attempts: normalized attempt + embedded turn projection.
	"CREATE TABLE attempts (run_id TEXT NOT NULL REFERENCES runs(run_id), attempt_id TEXT NOT NULL, node_id TEXT NOT NULL, " +
		"operation_id TEXT, rlm_child_id TEXT, turn_id TEXT, settlement_digest TEXT, phase TEXT NOT NULL, intent TEXT NOT NULL, " +
		"outcome TEXT, conditions TEXT NOT NULL, turn_json TEXT, PRIMARY KEY (run_id, attempt_id)) STRICT;",
	// child_turn_bindings: immutable (child,turn) foreign-key binding (never native topology).
	"CREATE TABLE child_turn_bindings (run_id TEXT NOT NULL, attempt_id TEXT NOT NULL, workflow_child_id TEXT NOT NULL, " +
		"rlm_child_id TEXT NOT NULL, turn_id TEXT NOT NULL, request_id TEXT NOT NULL, canonical_digest TEXT NOT NULL, " +
		"bound_at TEXT NOT NULL, PRIMARY KEY (run_id, attempt_id), " +
		"FOREIGN KEY (run_id, attempt_id) REFERENCES attempts(run_id, attempt_id)) STRICT;",
	// commands: idempotency key = request_digest; immutable receipt.
	"CREATE TABLE commands (request_digest TEXT PRIMARY KEY, request_id TEXT NOT NULL, run_id TEXT, command_id TEXT, " +
		"action TEXT NOT NULL, canonical_bytes TEXT NOT NULL, receipt_json TEXT NOT NULL, created_at TEXT NOT NULL) STRICT;",
	// operations: outbox rows (claimed, epoch-fenced).
	"CREATE TABLE operations (operation_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id), attempt_id TEXT, " +
		"kind TEXT NOT NULL, request_id TEXT NOT NULL, canonical_request TEXT NOT NULL, host_request_id TEXT, " +
		"phase TEXT NOT NULL, intent TEXT NOT NULL, outcome TEXT, conditions TEXT NOT NULL, " +
		"claim_epoch INTEGER, claim_owner TEXT, claim_expires_at TEXT, receipt_json TEXT, " +
		"created_at TEXT NOT NULL, updated_at TEXT NOT NULL) STRICT;",
	// host_inbox: dedup by host_event_id.
	"CREATE TABLE host_inbox (host_event_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id), " +
		"host_cursor TEXT NOT NULL, type TEXT NOT NULL, fact_json TEXT NOT NULL, created_at TEXT NOT NULL) STRICT;",
	// host_streams: per-run host cursor authority.
	"CREATE TABLE host_streams (run_id TEXT PRIMARY KEY REFERENCES runs(run_id), host_cursor TEXT, updated_at TEXT NOT NULL) STRICT;",
	// settlements: immutable host settlement evidence.
	"CREATE TABLE settlements (settlement_digest TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id), " +
		"attempt_id TEXT NOT NULL, outcome TEXT NOT NULL, result_kind TEXT NOT NULL, result_utf8_bytes INTEGER, " +
		"result_sha256 TEXT, usage_json TEXT NOT NULL, settlement_json TEXT NOT NULL, created_at TEXT NOT NULL) STRICT;",
	// acceptance: at most one accepted attempt per node.
	"CREATE TABLE acceptance (run_id TEXT NOT NULL REFERENCES runs(run_id), node_id TEXT NOT NULL, attempt_id TEXT, " +
		"decision TEXT NOT NULL, evidence_digest TEXT, decided_at TEXT NOT NULL, PRIMARY KEY (run_id, node_id)) STRICT;",
	// budgets: soft admission accounting.
	"CREATE TABLE budgets (run_id TEXT PRIMARY KEY REFERENCES runs(run_id), max_concurrent_attempts INTEGER NOT NULL, " +
		"max_total_tokens INTEGER NOT NULL, reserved_tokens INTEGER NOT NULL DEFAULT 0, settled_tokens INTEGER NOT NULL DEFAULT 0, " +
		"updated_at TEXT NOT NULL) STRICT;",
	// tombstones: native-topology tombstone bindings (evidence only).
	"CREATE TABLE tombstones (run_id TEXT NOT NULL REFERENCES runs(run_id), rlm_child_id TEXT NOT NULL, request_id TEXT NOT NULL, " +
		"tombstone_digest TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (run_id, rlm_child_id)) STRICT;",
	// events: gap-free per-run controller facts (sequence 1..N).
	"CREATE TABLE events (run_id TEXT NOT NULL REFERENCES runs(run_id), sequence INTEGER NOT NULL, event_id TEXT NOT NULL, " +
		"revision INTEGER NOT NULL, type TEXT NOT NULL, controller_epoch INTEGER NOT NULL, cancel_epoch INTEGER NOT NULL, " +
		"recorded_at TEXT NOT NULL, data_json TEXT NOT NULL, digest TEXT NOT NULL, compacted INTEGER NOT NULL DEFAULT 0, " +
		"PRIMARY KEY (run_id, sequence), UNIQUE (event_id)) STRICT;",
	"CREATE UNIQUE INDEX commands_request_id ON commands (request_id);",
	"CREATE UNIQUE INDEX commands_run_command ON commands (run_id, command_id) WHERE command_id IS NOT NULL;",
	"CREATE INDEX operations_by_run_phase ON operations (run_id, phase);",
	"CREATE INDEX host_inbox_by_run ON host_inbox (run_id);",
	"CREATE INDEX settlements_by_run ON settlements (run_id);",
].join("\n");

interface Migration {
	version: number;
	name: string;
	sql: string;
}

/** Append-only migration registry. New migrations append; never edit history. */
const MIGRATIONS: readonly Migration[] = [{ version: 1, name: "base", sql: BASE_DDL }];

function migrationChecksum(m: Migration): string {
	return sha256Hex(`${m.version}\n${m.name}\n${m.sql}`);
}

const REQUIRED_TABLES = [
	"store_meta",
	"definitions",
	"runs",
	"nodes",
	"attempts",
	"child_turn_bindings",
	"commands",
	"operations",
	"host_inbox",
	"host_streams",
	"settlements",
	"acceptance",
	"budgets",
	"tombstones",
	"events",
] as const;

// ---------------------------------------------------------------------------
// Public option / effect / receipt shapes.
// ---------------------------------------------------------------------------

export type DurabilityEvent = { op: "commit" | "fsync_dir"; path: string };

export interface OpenWorkflowV2StoreOptions {
	/** Owner-only store root: `<session-artifacts>/<root-session-id>/workflows/` (host-derived). */
	root: string;
	/** sha256 of the closed root-session authority scope; bound into store_meta. */
	rootScopeDigest: string;
	/** Deterministic clock for controller/receipt timestamps (tests inject a fixed clock). */
	clock?: () => string;
	onDurabilityEvent?: (event: DurabilityEvent) => void;
}

export interface OutboxEnqueue {
	operationId: string;
	attemptId?: string | null;
	kind: "deliver" | "cancel" | "delete";
	requestId: string;
	/** The exact canonical host request object redelivered verbatim on claim expiry. */
	canonicalRequest: WorkflowV2Value;
}

export interface CommandEffect {
	/** Fully-formed, schema-valid controller events this command appends (gap-free). */
	events: WorkflowV2Value[];
	/** Stable outbox operations enqueued atomically with the command. */
	operations?: OutboxEnqueue[];
	/** Present for `create`: the definition to persist (bound by digest). */
	definition?: WorkflowV2Value;
	/** Present for `create`: the closed budget for the run. */
	budget?: { maxConcurrentAttempts: number; maxTotalTokens: number };
	/** Immutable (child,turn) foreign-key bindings (evidence; never native topology). */
	bindings?: Array<{
		attemptId: string;
		workflowChildId: string;
		rlmChildId: string;
		turnId: string;
		requestId: string;
		canonicalDigest: string;
	}>;
	/** Node acceptance decisions (at most one accepted attempt per node). */
	acceptance?: Array<{
		nodeId: string;
		attemptId?: string | null;
		decision: "not_evaluated" | "accepted" | "rejected";
		evidenceDigest?: string | null;
	}>;
	/** Native-topology tombstone bindings (evidence only). */
	tombstones?: Array<{ rlmChildId: string; requestId: string; tombstoneDigest: string }>;
}

export interface CommandReceipt {
	action: string;
	requestId: string;
	runId: string | null;
	commandId: string | null;
	requestDigest: string;
	revision: number;
	controllerEpoch: number;
	cancelEpoch: number;
	appliedSequences: number[];
	recordedAt: string;
}

export interface ClaimedOperation {
	operationId: string;
	runId: string;
	attemptId: string | null;
	kind: string;
	requestId: string;
	canonicalRequest: string;
	hostRequestId: string | null;
	claimEpoch: number;
	claimExpiresAt: string;
}

// ---------------------------------------------------------------------------
// WorkflowV2Store.
// ---------------------------------------------------------------------------

export class WorkflowV2Store {
	private closed = false;
	private acquiredEpoch: number | undefined;

	private constructor(
		private readonly db: DatabaseSync,
		private readonly root: string,
		private readonly dbPath: string,
		private readonly rootScopeDigest: string,
		private readonly clock: (() => string) | undefined,
		private readonly onDurabilityEvent: ((event: DurabilityEvent) => void) | undefined,
	) {}

	static open(options: OpenWorkflowV2StoreOptions): WorkflowV2Store {
		const rootScopeDigest = assertDigest(options.rootScopeDigest, "rootScopeDigest");
		const sqlite = loadSqlite();
		if (!sqlite) failClosed("store_driver_unavailable", "node:sqlite is unavailable (probe should have gated this)");

		mkdirSyncOwnerOnly(options.root);
		const canonicalRoot = assertHardenedDir(options.root);
		const dbPath = join(canonicalRoot, STORE_FILE_NAME);
		assertHardenedFile(canonicalRoot, dbPath);

		const db = new sqlite.DatabaseSync(dbPath, { timeout: BUSY_TIMEOUT_MS });
		try {
			WorkflowV2Store.applyPragmas(db);
			for (const suffix of ["", "-wal", "-shm"]) {
				try {
					chmodSync(`${dbPath}${suffix}`, STORE_FILE_MODE);
				} catch (error) {
					if (!isErrnoCode(error, "ENOENT")) throw error;
				}
			}
			WorkflowV2Store.assertIntegrity(db);
			WorkflowV2Store.initializeOrValidateSchema(db);
			WorkflowV2Store.bindOrCheckScope(db, rootScopeDigest);
		} catch (error) {
			try {
				db.close();
			} catch {
				// fail-closed already
			}
			throw error;
		}

		const instance = new WorkflowV2Store(
			db,
			canonicalRoot,
			dbPath,
			rootScopeDigest,
			options.clock,
			options.onDurabilityEvent,
		);
		instance.emitDurability({ op: "fsync_dir", path: canonicalRoot });
		fsyncDir(canonicalRoot);
		return instance;
	}

	private static applyPragmas(db: DatabaseSync): void {
		const journal = readScalar(db, "PRAGMA journal_mode = WAL", "journal_mode");
		if (String(journal).toLowerCase() !== "wal")
			failClosed("store_wal_unavailable", `journal_mode is ${String(journal)}`);
		db.exec("PRAGMA synchronous = FULL");
		if (Number(readScalar(db, "PRAGMA synchronous", "synchronous")) !== 2) {
			failClosed("store_synchronous_unavailable", "synchronous is not FULL");
		}
		db.exec("PRAGMA foreign_keys = ON");
		if (Number(readScalar(db, "PRAGMA foreign_keys", "foreign_keys")) !== 1) {
			failClosed("store_foreign_keys_unavailable", "foreign_keys is not ON");
		}
		db.exec(`PRAGMA busy_timeout = ${BUSY_TIMEOUT_MS}`);
		db.exec("PRAGMA cell_size_check = ON");
		db.exec("PRAGMA wal_autocheckpoint = 512");
	}

	private static assertIntegrity(db: DatabaseSync): void {
		const quick = readScalar(db, "PRAGMA quick_check", "quick_check");
		if (String(quick) !== "ok") failClosed("store_integrity_failed", `quick_check returned ${String(quick)}`);
		const full = readScalar(db, "PRAGMA integrity_check", "integrity_check");
		if (String(full) !== "ok") failClosed("store_integrity_failed", `integrity_check returned ${String(full)}`);
	}

	private static tableNames(db: DatabaseSync): Set<string> {
		const rows = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all();
		return new Set(rows.map((r) => String((r as Record<string, unknown>).name)));
	}

	private static initializeOrValidateSchema(db: DatabaseSync): void {
		const applicationId = Number(readScalar(db, "PRAGMA application_id", "application_id"));
		const tables = WorkflowV2Store.tableNames(db);
		if (applicationId === 0) {
			const userTables = [...tables].filter((name) => !name.startsWith("sqlite_"));
			if (userTables.length > 0) failClosed("store_foreign", "untagged store already has tables; refusing to adopt");
			WorkflowV2Store.runMigrations(db, 0);
			db.exec(`PRAGMA application_id = ${STORE_APPLICATION_ID}`);
			return;
		}
		if (applicationId !== STORE_APPLICATION_ID)
			failClosed("store_foreign", `foreign application_id ${applicationId}`);
		const userVersion = Number(readScalar(db, "PRAGMA user_version", "user_version"));
		if (userVersion > STORE_USER_VERSION)
			failClosed("store_user_version_newer", `user_version ${userVersion} > ${STORE_USER_VERSION}`);
		WorkflowV2Store.verifyAppliedMigrations(db, userVersion);
		if (userVersion < MIGRATIONS.length) WorkflowV2Store.runMigrations(db, userVersion);
		for (const required of REQUIRED_TABLES) {
			if (!tables.has(required)) failClosed("store_torn_schema", `missing required table ${required}`);
		}
	}

	/** Verify each already-applied migration's checksum against the registry (§7). */
	private static verifyAppliedMigrations(db: DatabaseSync, appliedThrough: number): void {
		const rows = db.prepare("SELECT version, name, checksum FROM store_migrations ORDER BY version").all();
		if (rows.length < appliedThrough) failClosed("store_migration_missing", "applied migration ledger is short");
		for (const raw of rows) {
			const row = raw as Record<string, unknown>;
			const version = Number(row.version);
			const registry = MIGRATIONS.find((m) => m.version === version);
			if (!registry) failClosed("store_migration_unknown", `applied migration ${version} not in registry`);
			if (String(row.checksum) !== migrationChecksum(registry)) {
				failClosed("store_migration_checksum", `migration ${version} checksum mismatch`);
			}
			if (String(row.name) !== registry.name)
				failClosed("store_migration_name", `migration ${version} name mismatch`);
		}
	}

	/** Apply pending migrations transactionally, recording checksums (append-only). */
	private static runMigrations(db: DatabaseSync, appliedThrough: number): void {
		db.exec("BEGIN IMMEDIATE");
		try {
			if (appliedThrough === 0) {
				db.exec(
					"CREATE TABLE IF NOT EXISTS store_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL) STRICT",
				);
			}
			for (const migration of MIGRATIONS) {
				if (migration.version <= appliedThrough) continue;
				db.exec(migration.sql);
				db.prepare("INSERT INTO store_migrations (version, name, checksum, applied_at) VALUES (?, ?, ?, ?)").run(
					migration.version,
					migration.name,
					migrationChecksum(migration),
					new Date().toISOString(),
				);
			}
			db.exec(`PRAGMA user_version = ${MIGRATIONS.length}`);
			db.exec("COMMIT");
		} catch (error) {
			try {
				db.exec("ROLLBACK");
			} catch {
				// outer open() closes + rethrows
			}
			throw error;
		}
	}

	private static bindOrCheckScope(db: DatabaseSync, rootScopeDigest: string): void {
		const row = db.prepare("SELECT value FROM store_meta WHERE key = 'root_scope_digest'").get() as
			| Record<string, unknown>
			| undefined;
		if (!row) {
			db.prepare("INSERT INTO store_meta (key, value) VALUES ('root_scope_digest', ?)").run(rootScopeDigest);
			db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('controller_epoch', '0')").run();
			db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('text_bytes', '0')").run();
			return;
		}
		if (String(row.value) !== rootScopeDigest)
			failClosed("store_scope_mismatch", "root scope digest does not match the store");
	}

	private emitDurability(event: DurabilityEvent): void {
		try {
			this.onDurabilityEvent?.(event);
		} catch {
			// diagnostic only
		}
	}

	private assertOpen(): void {
		if (this.closed) failClosed("store_closed", "store handle is closed");
	}

	/**
	 * Acquire the sole-writer fence at a strictly higher controllerEpoch (§7
	 * single-writer OS lock, epoch-fenced like the supervisor generation). First
	 * acquire from 0 sets epoch; a later open must present a higher epoch.
	 */
	acquireWriter(controllerEpoch: number): number {
		this.assertOpen();
		if (!Number.isInteger(controllerEpoch) || controllerEpoch < 1 || controllerEpoch > MAX_SAFE) {
			failClosed("store_epoch_range", `controllerEpoch out of range: ${controllerEpoch}`);
		}
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const current = Number(
				(
					this.db.prepare("SELECT value FROM store_meta WHERE key = 'controller_epoch'").get() as Record<
						string,
						unknown
					>
				).value,
			);
			if (controllerEpoch <= current) {
				this.db.exec("ROLLBACK");
				failClosed("store_epoch_stale", `controllerEpoch ${controllerEpoch} <= current ${current}`);
			}
			this.db.prepare("UPDATE store_meta SET value = ? WHERE key = 'controller_epoch'").run(String(controllerEpoch));
			this.commitAndSync();
			this.acquiredEpoch = controllerEpoch;
			return controllerEpoch;
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	private requireWriter(): number {
		if (this.acquiredEpoch === undefined) failClosed("store_not_acquired", "writer epoch not acquired");
		return this.acquiredEpoch;
	}

	private commitAndSync(): void {
		this.db.exec("COMMIT");
		this.emitDurability({ op: "commit", path: this.dbPath });
		this.emitDurability({ op: "fsync_dir", path: this.root });
		fsyncDir(this.root);
	}

	private rollbackQuietly(): void {
		try {
			this.db.exec("ROLLBACK");
		} catch {
			// no-op on an already-aborted transaction
		}
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		try {
			this.db.close();
		} catch {
			// a close failure is not authority
		}
	}

	// ---- meta + capacity ------------------------------------------------------

	private readMeta(key: string): string {
		const row = this.db.prepare("SELECT value FROM store_meta WHERE key = ?").get(key) as
			| Record<string, unknown>
			| undefined;
		if (!row) failClosed("store_meta_missing", `store_meta key ${key} missing`);
		return String(row.value);
	}

	private writeMeta(key: string, value: string): void {
		this.db
			.prepare("INSERT INTO store_meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = ?")
			.run(key, value, value);
	}

	private count(table: string): number {
		// table is a fixed internal identifier, never caller input.
		return Number((this.db.prepare(`SELECT COUNT(*) AS c FROM ${table}`).get() as Record<string, unknown>).c);
	}

	private dbByteSize(): number {
		let total = 0;
		for (const suffix of ["", "-wal"]) {
			try {
				total += Number(lstatSync(`${this.dbPath}${suffix}`).size);
			} catch (error) {
				if (!isErrnoCode(error, "ENOENT")) throw error;
			}
		}
		return total;
	}

	private freeBytes(): number {
		try {
			// statfsSync avoids a native statfs addon (BLOCK-5 alternative on this profile).
			const fs = createRequire(import.meta.url)("node:fs") as typeof import("node:fs");
			const s = fs.statfsSync(this.dbPath);
			return Number(s.bavail) * Number(s.bsize);
		} catch {
			return Number.POSITIVE_INFINITY;
		}
	}

	/**
	 * Reject `create`/`start`/`retry` BEFORE a durable host effect when any row/byte
	 * quota is at the 90% high-water mark, the DB+WAL exceeds its cap, or free
	 * space falls below max(64 MiB, 10% of FS). Settlement/cancel/inbox/reconcile
	 * transactions are exempt (they preserve integrity + evidence).
	 */
	private capacityGuard(action: string): void {
		if (action !== "create" && action !== "start" && action !== "retry") return;
		const hw = STORE_CAPACITY.highWaterFraction;
		const checks: Array<[string, number, number]> = [
			["runs", this.count("runs"), STORE_CAPACITY.maxRuns],
			["events", this.count("events"), STORE_CAPACITY.maxEvents],
			["commands", this.count("commands"), STORE_CAPACITY.maxCommands],
			["operations", this.count("operations"), STORE_CAPACITY.maxOperations],
			["host_inbox", this.count("host_inbox"), STORE_CAPACITY.maxHostInbox],
			["settlements", this.count("settlements"), STORE_CAPACITY.maxSettlements],
			["tombstones", this.count("tombstones"), STORE_CAPACITY.maxTombstones],
			["text_bytes", Number(this.readMeta("text_bytes")), STORE_CAPACITY.maxTextBytes],
		];
		for (const [name, value, quota] of checks) {
			if (value >= Math.floor(quota * hw))
				failClosed("CAPACITY_EXCEEDED", `high-water for ${name}: ${value}/${quota}`);
		}
		if (this.dbByteSize() >= Math.floor(STORE_CAPACITY.maxDbBytes * hw)) {
			failClosed("CAPACITY_EXCEEDED", "db+wal high-water");
		}
		const free = this.freeBytes();
		try {
			const fs = createRequire(import.meta.url)("node:fs") as typeof import("node:fs");
			const s = fs.statfsSync(this.dbPath);
			const total = Number(s.blocks) * Number(s.bsize);
			const floor = Math.max(67_108_864, Math.floor(total * 0.1));
			if (free < floor) failClosed("CAPACITY_EXCEEDED", `free space ${free} < floor ${floor}`);
		} catch {
			// free-space check unavailable: do not fabricate headroom, but the row/byte
			// quotas above already gate; a missing statfs is not itself a rejection.
		}
	}

	private addTextBytes(delta: number): void {
		const current = Number(this.readMeta("text_bytes"));
		this.writeMeta("text_bytes", String(Math.max(0, current + delta)));
	}

	// ---- hydration ------------------------------------------------------------

	private parseConditions(json: string): string[] {
		const parsed = JSON.parse(json);
		if (!Array.isArray(parsed)) failClosed("store_conditions_corrupt", "conditions column is not an array");
		return parsed as string[];
	}

	private loadRunContext(runId: string): { aggregate: RunAggregate; graph: DefinitionGraph } | null {
		const runRow = this.db.prepare("SELECT * FROM runs WHERE run_id = ?").get(runId) as
			| Record<string, unknown>
			| undefined;
		if (!runRow) return null;
		const defRow = this.db
			.prepare("SELECT canonical_bytes FROM definitions WHERE definition_digest = ?")
			.get(String(runRow.definition_digest)) as Record<string, unknown> | undefined;
		if (!defRow) failClosed("store_definition_missing", `run ${runId} references a missing definition`);
		const definition = decodeWorkflowV2Definition(JSON.parse(String(defRow.canonical_bytes)));
		const graph = buildDefinitionGraph(definition);

		const run = validateProjectionSemantics("run", {
			phase: String(runRow.phase),
			intent: String(runRow.intent),
			outcome: runRow.outcome === null ? null : String(runRow.outcome),
			conditions: this.parseConditions(String(runRow.conditions)),
		}) as RunAggregate["run"];
		// §11/§5: re-enforce RunTerminalized outcome-equality on DB load/recovery. A
		// terminal run whose persisted RunTerminalized fact (or terminal_outcome column)
		// disagrees with the normalized outcome is corrupt/tampered — fail closed so no
		// success (or any terminal) projection is ever served. reconcileTerminalMismatch()
		// converts this into a durable quarantine under the writer fence.
		if (run.phase === "terminal") {
			const persisted = this.persistedTerminalOutcome(runId);
			try {
				assertRunTerminalizedOutcomeEquality(persisted, run);
			} catch (error) {
				failClosed(
					"store_terminal_outcome_mismatch",
					`run ${runId} terminal outcome disagreement: ${(error as Error).message}`,
				);
			}
			if (runRow.terminal_outcome !== null && String(runRow.terminal_outcome) !== run.outcome) {
				failClosed(
					"store_terminal_outcome_mismatch",
					`run ${runId} terminal_outcome column disagrees with normalized outcome`,
				);
			}
		}

		const nodes: Record<string, ReturnType<typeof validateProjectionSemantics>> = {};
		for (const nodeId of graph.nodeIds) {
			const nr = this.db.prepare("SELECT * FROM nodes WHERE run_id = ? AND node_id = ?").get(runId, nodeId) as
				| Record<string, unknown>
				| undefined;
			if (!nr) failClosed("store_node_missing", `run ${runId} node ${nodeId} missing`);
			nodes[nodeId] = validateProjectionSemantics("node", {
				phase: String(nr.phase),
				intent: String(nr.intent),
				outcome: nr.outcome === null ? null : String(nr.outcome),
				conditions: this.parseConditions(String(nr.conditions)),
			});
		}

		const attempts: Record<string, AttemptRecord> = {};
		const attemptOrder: string[] = [];
		const attemptRows = this.db.prepare("SELECT * FROM attempts WHERE run_id = ? ORDER BY rowid").all(runId);
		for (const raw of attemptRows) {
			const ar = raw as Record<string, unknown>;
			const attemptId = String(ar.attempt_id);
			attemptOrder.push(attemptId);
			attempts[attemptId] = {
				attemptId,
				nodeId: String(ar.node_id),
				operationId: ar.operation_id === null ? null : String(ar.operation_id),
				rlmChildId: ar.rlm_child_id === null ? null : String(ar.rlm_child_id),
				turnId: ar.turn_id === null ? null : String(ar.turn_id),
				settlementDigest: ar.settlement_digest === null ? null : String(ar.settlement_digest),
				projection: validateProjectionSemantics("attempt", {
					phase: String(ar.phase),
					intent: String(ar.intent),
					outcome: ar.outcome === null ? null : String(ar.outcome),
					conditions: this.parseConditions(String(ar.conditions)),
				}) as AttemptRecord["projection"],
				turn:
					ar.turn_json === null
						? null
						: (validateProjectionSemantics("turn", JSON.parse(String(ar.turn_json))) as AttemptRecord["turn"]),
			};
		}

		const maxSeq = Number(
			(
				this.db.prepare("SELECT COALESCE(MAX(sequence), 0) AS m FROM events WHERE run_id = ?").get(runId) as Record<
					string,
					unknown
				>
			).m,
		);

		const aggregate: RunAggregate = {
			runId,
			revision: Number(runRow.revision),
			controllerEpoch: Number(runRow.controller_epoch),
			cancelEpoch: Number(runRow.cancel_epoch),
			run,
			nodes: nodes as RunAggregate["nodes"],
			nodeOrder: [...graph.nodeIds],
			attempts,
			attemptOrder,
			hostCursor: runRow.host_cursor === null ? null : String(runRow.host_cursor),
			lastControllerSequence: maxSeq,
			appliedHostEventIds: [],
		};
		revalidateAggregate(aggregate);
		return { aggregate, graph };
	}

	private upsertRunRows(agg: RunAggregate, now: string): void {
		const run = agg.run;
		this.db
			.prepare(
				"UPDATE runs SET revision = ?, controller_epoch = ?, cancel_epoch = ?, phase = ?, intent = ?, outcome = ?, " +
					"conditions = ?, host_cursor = ?, terminal_outcome = ?, terminal_digest = ?, updated_at = ? WHERE run_id = ?",
			)
			.run(
				agg.revision,
				agg.controllerEpoch,
				agg.cancelEpoch,
				run.phase,
				run.intent,
				run.outcome,
				JSON.stringify(run.conditions),
				agg.hostCursor,
				run.phase === "terminal" ? run.outcome : null,
				null,
				now,
				agg.runId,
			);
		for (const nodeId of agg.nodeOrder) {
			const node = agg.nodes[nodeId];
			this.db
				.prepare(
					"UPDATE nodes SET phase = ?, intent = ?, outcome = ?, conditions = ? WHERE run_id = ? AND node_id = ?",
				)
				.run(node.phase, node.intent, node.outcome, JSON.stringify(node.conditions), agg.runId, nodeId);
		}
		for (const attemptId of agg.attemptOrder) {
			const rec = agg.attempts[attemptId];
			this.db
				.prepare(
					"INSERT INTO attempts (run_id, attempt_id, node_id, operation_id, rlm_child_id, turn_id, settlement_digest, phase, intent, outcome, conditions, turn_json) " +
						"VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) " +
						"ON CONFLICT(run_id, attempt_id) DO UPDATE SET operation_id = excluded.operation_id, rlm_child_id = excluded.rlm_child_id, " +
						"turn_id = excluded.turn_id, settlement_digest = excluded.settlement_digest, phase = excluded.phase, intent = excluded.intent, " +
						"outcome = excluded.outcome, conditions = excluded.conditions, turn_json = excluded.turn_json",
				)
				.run(
					agg.runId,
					attemptId,
					rec.nodeId,
					rec.operationId,
					rec.rlmChildId,
					rec.turnId,
					rec.settlementDigest,
					rec.projection.phase,
					rec.projection.intent,
					rec.projection.outcome,
					JSON.stringify(rec.projection.conditions),
					rec.turn ? JSON.stringify(rec.turn) : null,
				);
		}
	}

	private appendEvents(runId: string, events: WorkflowV2Value[], startSequence: number): number[] {
		const applied: number[] = [];
		let expected = startSequence;
		for (const event of events) {
			expected += 1;
			if (Number(event.sequence) !== expected) {
				failClosed("store_event_gap", `event sequence ${String(event.sequence)} != expected ${expected}`);
			}
			this.db
				.prepare(
					"INSERT INTO events (run_id, sequence, event_id, revision, type, controller_epoch, cancel_epoch, recorded_at, data_json, digest) " +
						"VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
				)
				.run(
					runId,
					expected,
					String(event.eventId),
					Number(event.revision),
					String(event.type),
					Number(event.controllerEpoch),
					Number(event.cancelEpoch),
					String(event.recordedAt),
					JSON.stringify(event.data),
					String(event.digest),
				);
			applied.push(expected);
		}
		return applied;
	}

	// ---- validate (pure; writes nothing) --------------------------------------

	/**
	 * §6 `validate`: strict structural + semantic validation only. It writes no
	 * idempotency row, command, event, definition, or host effect.
	 */
	validate(request: unknown): { action: "validate"; ok: true; requestId: string } {
		this.assertOpen();
		const decoded = decodeWorkflowV2PublicRequest(request);
		if (decoded.action !== "validate") failClosed("store_not_validate", "validate() requires a validate action");
		return { action: "validate", ok: true, requestId: String(decoded.requestId) };
	}

	// ---- command transaction --------------------------------------------------

	/**
	 * §6/§7 command transaction. One `BEGIN IMMEDIATE`: capacity is checked before
	 * the transaction admits a durable effect; idempotency returns the stored
	 * receipt on identical canonical bytes and IDEMPOTENCY_CONFLICT on changed
	 * bytes; post-create fences (revision/controllerEpoch/cancelEpoch) are enforced
	 * with no mutation on staleness; the effect's controller events are folded
	 * through the pure reducer (validating every projection); normalized rows,
	 * gap-free controller facts, an immutable command receipt, and stable outbox
	 * operations are written; then commit + fsync. No host/provider/blob/FS latency
	 * occurs inside the transaction.
	 */
	applyCommand(request: unknown, effect: CommandEffect): CommandReceipt {
		this.assertOpen();
		this.requireWriter();
		const decoded = decodeWorkflowV2PublicRequest(request);
		const action = String(decoded.action);
		if (action === "validate") failClosed("store_validate_not_mutation", "use validate() for the validate action");
		const canonicalBytes = canonicalJson(decoded);
		const requestDigest = sha256Hex(canonicalBytes);
		const now = nowIso(this.clock);

		this.capacityGuard(action);

		this.db.exec("BEGIN IMMEDIATE");
		try {
			// idempotency: identical canonical bytes ⇒ same digest ⇒ stored receipt.
			const existing = this.db
				.prepare("SELECT receipt_json FROM commands WHERE request_digest = ?")
				.get(requestDigest) as Record<string, unknown> | undefined;
			if (existing) {
				this.db.exec("COMMIT");
				return JSON.parse(String(existing.receipt_json)) as CommandReceipt;
			}
			// requestId is the sole idempotency key (§6): same id + different canonical
			// bytes ⇒ IDEMPOTENCY_CONFLICT with no mutation.
			const priorByRequestId = this.db
				.prepare("SELECT request_digest FROM commands WHERE request_id = ?")
				.get(String(decoded.requestId)) as Record<string, unknown> | undefined;
			if (priorByRequestId && String(priorByRequestId.request_digest) !== requestDigest) {
				this.rollbackQuietly();
				failClosed("IDEMPOTENCY_CONFLICT", `requestId ${String(decoded.requestId)} reused with different bytes`);
			}

			const isCreate = action === "create";
			let prev: RunAggregate | null = null;
			let graph: DefinitionGraph;
			let runId: string;
			let commandId: string | null = null;

			if (isCreate) {
				if (!effect.definition) failClosed("store_create_no_definition", "create requires effect.definition");
				const definition = decodeWorkflowV2Definition(effect.definition);
				graph = buildDefinitionGraph(definition);
				if (effect.events.length === 0) failClosed("store_create_no_events", "create requires a RunAdmitted event");
				runId = String(effect.events[0].runId);
			} else {
				runId = String(decoded.runId);
				commandId = String(decoded.commandId);
				// (run_id, command_id) reuse with different bytes ⇒ IDEMPOTENCY_CONFLICT.
				const priorCommand = this.db
					.prepare("SELECT request_digest FROM commands WHERE run_id = ? AND command_id = ?")
					.get(runId, commandId) as Record<string, unknown> | undefined;
				if (priorCommand && String(priorCommand.request_digest) !== requestDigest) {
					this.rollbackQuietly();
					failClosed("IDEMPOTENCY_CONFLICT", `command ${commandId} reused with different bytes`);
				}
				const ctx = this.loadRunContext(runId);
				if (!ctx) {
					this.rollbackQuietly();
					failClosed("NOT_FOUND", `run ${runId} not found`);
				}
				prev = ctx.aggregate;
				graph = ctx.graph;
				if (
					Number(decoded.expectedRevision) !== prev.revision ||
					Number(decoded.expectedControllerEpoch) !== prev.controllerEpoch ||
					Number(decoded.expectedCancelEpoch) !== prev.cancelEpoch
				) {
					this.rollbackQuietly();
					failClosed("COMMAND_FENCE_STALE", `stale fence for run ${runId}`);
				}
			}

			// fold events through the pure reducer (validates transitions + projections).
			let state: RunAggregate | null = prev;
			for (const event of effect.events) {
				if (String(event.runId) !== runId) failClosed("store_event_run_mismatch", "event runId mismatch");
				state = reduceFact(state, event, graph);
			}
			if (state === null) failClosed("store_no_state", "command produced no run state");

			const prevMaxSeq = prev ? prev.lastControllerSequence : 0;

			if (isCreate) {
				const canonicalDefinition = canonicalJson(effect.definition);
				const defDigest = sha256Hex(canonicalDefinition);
				this.db
					.prepare(
						"INSERT OR IGNORE INTO definitions (definition_digest, canonical_bytes, utf8_bytes, created_at) VALUES (?, ?, ?, ?)",
					)
					.run(defDigest, JSON.stringify(effect.definition), Buffer.byteLength(canonicalDefinition, "utf8"), now);
				this.addTextBytes(Buffer.byteLength(canonicalDefinition, "utf8"));
				this.db
					.prepare(
						"INSERT INTO runs (run_id, definition_digest, revision, controller_epoch, cancel_epoch, phase, intent, outcome, conditions, host_cursor, terminal_outcome, terminal_digest, erased, created_at, updated_at) " +
							"VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
					)
					.run(
						runId,
						defDigest,
						state.revision,
						state.controllerEpoch,
						state.cancelEpoch,
						state.run.phase,
						state.run.intent,
						state.run.outcome,
						JSON.stringify(state.run.conditions),
						state.hostCursor,
						null,
						null,
						now,
						now,
					);
				for (const nodeId of state.nodeOrder) {
					const node = state.nodes[nodeId];
					this.db
						.prepare(
							"INSERT INTO nodes (run_id, node_id, phase, intent, outcome, conditions) VALUES (?, ?, ?, ?, ?, ?)",
						)
						.run(runId, nodeId, node.phase, node.intent, node.outcome, JSON.stringify(node.conditions));
				}
				this.db
					.prepare("INSERT INTO host_streams (run_id, host_cursor, updated_at) VALUES (?, ?, ?)")
					.run(runId, state.hostCursor, now);
				const definitionBudget = (effect.definition as WorkflowV2Value).budget as WorkflowV2Value;
				const budget = effect.budget ?? {
					maxConcurrentAttempts: Number(definitionBudget.maxConcurrentAttempts),
					maxTotalTokens: Number(definitionBudget.maxTotalTokens),
				};
				this.db
					.prepare(
						"INSERT INTO budgets (run_id, max_concurrent_attempts, max_total_tokens, reserved_tokens, settled_tokens, updated_at) VALUES (?, ?, ?, 0, 0, ?)",
					)
					.run(runId, budget.maxConcurrentAttempts, budget.maxTotalTokens, now);
			}

			this.upsertRunRows(state, now);
			const appliedSequences = this.appendEvents(runId, effect.events, prevMaxSeq);

			// immutable (child,turn) bindings (evidence; never native topology rows).
			for (const b of effect.bindings ?? []) {
				this.db
					.prepare(
						"INSERT INTO child_turn_bindings (run_id, attempt_id, workflow_child_id, rlm_child_id, turn_id, request_id, canonical_digest, bound_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
					)
					.run(
						runId,
						assertId(b.attemptId, "attemptId"),
						assertId(b.workflowChildId, "workflowChildId"),
						assertId(b.rlmChildId, "rlmChildId"),
						assertId(b.turnId, "turnId"),
						assertId(b.requestId, "requestId"),
						assertDigest(b.canonicalDigest, "canonicalDigest"),
						now,
					);
			}
			for (const a of effect.acceptance ?? []) {
				this.db
					.prepare(
						"INSERT INTO acceptance (run_id, node_id, attempt_id, decision, evidence_digest, decided_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(run_id, node_id) DO UPDATE SET attempt_id = excluded.attempt_id, decision = excluded.decision, evidence_digest = excluded.evidence_digest, decided_at = excluded.decided_at",
					)
					.run(
						runId,
						assertId(a.nodeId, "nodeId"),
						a.attemptId ? assertId(a.attemptId, "attemptId") : null,
						a.decision,
						a.evidenceDigest ? assertDigest(a.evidenceDigest, "evidenceDigest") : null,
						now,
					);
			}
			for (const t of effect.tombstones ?? []) {
				this.db
					.prepare(
						"INSERT INTO tombstones (run_id, rlm_child_id, request_id, tombstone_digest, created_at) VALUES (?, ?, ?, ?, ?)",
					)
					.run(
						runId,
						assertId(t.rlmChildId, "rlmChildId"),
						assertId(t.requestId, "requestId"),
						assertDigest(t.tombstoneDigest, "tombstoneDigest"),
						now,
					);
			}

			// stable, epoch-fenced outbox operations (pending; claim_unheld).
			for (const op of effect.operations ?? []) {
				const opConditions = ["claim_unheld", "receipt_absent", "effect_unclassified"];
				validateProjectionSemantics("operation", {
					phase: "pending",
					intent: op.kind === "cancel" ? "cancel" : "deliver",
					outcome: null,
					conditions: opConditions,
				});
				this.db
					.prepare(
						"INSERT INTO operations (operation_id, run_id, attempt_id, kind, request_id, canonical_request, host_request_id, phase, intent, outcome, conditions, claim_epoch, claim_owner, claim_expires_at, receipt_json, created_at, updated_at) " +
							"VALUES (?, ?, ?, ?, ?, ?, NULL, 'pending', ?, NULL, ?, NULL, NULL, NULL, NULL, ?, ?)",
					)
					.run(
						assertId(op.operationId, "operationId"),
						runId,
						op.attemptId ?? null,
						op.kind,
						assertId(op.requestId, "requestId"),
						canonicalJson(op.canonicalRequest),
						op.kind === "cancel" ? "cancel" : "deliver",
						JSON.stringify(opConditions),
						now,
						now,
					);
			}

			const receipt: CommandReceipt = {
				action,
				requestId: String(decoded.requestId),
				runId,
				commandId,
				requestDigest,
				revision: state.revision,
				controllerEpoch: state.controllerEpoch,
				cancelEpoch: state.cancelEpoch,
				appliedSequences,
				recordedAt: now,
			};
			this.db
				.prepare(
					"INSERT INTO commands (request_digest, request_id, run_id, command_id, action, canonical_bytes, receipt_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
				)
				.run(
					requestDigest,
					receipt.requestId,
					isCreate ? null : runId,
					commandId,
					action,
					canonicalBytes,
					JSON.stringify(receipt),
					now,
				);

			this.commitAndSync();
			return receipt;
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	// ---- host inbox -----------------------------------------------------------

	/**
	 * §7 host page ingestion. One `BEGIN IMMEDIATE`: dedup by `hostEventId`,
	 * reduce the fact into controller observations (the reducer runs
	 * `validateProjectionSemantics` on every produced projection), persist the
	 * inbox row + settlement evidence, and advance `hostCursor` atomically in the
	 * SAME transaction. Re-ingesting a `hostEventId` is idempotent and never
	 * regresses the cursor. The controller supplies the owning `runId` (host facts
	 * are addressed by request/child/turn, not by run).
	 */
	ingestHostEvent(runId: string, rawEvent: unknown): { applied: boolean; hostCursor: string | null } {
		this.assertOpen();
		this.requireWriter();
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const ctx = this.loadRunContext(runId);
			if (!ctx) {
				this.rollbackQuietly();
				failClosed("NOT_FOUND", `run ${runId} not found`);
			}
			// decode strictly (via the reducer's classifier) and dedup first.
			const event = rawEvent as WorkflowV2Value;
			const hostEventId = String(event.hostEventId);
			const dup = this.db.prepare("SELECT 1 AS x FROM host_inbox WHERE host_event_id = ?").get(hostEventId) as
				| Record<string, unknown>
				| undefined;
			if (dup) {
				const cursor = ctx.aggregate.hostCursor;
				this.db.exec("COMMIT");
				return { applied: false, hostCursor: cursor };
			}
			const state = reduceFact(ctx.aggregate, event, ctx.graph);
			this.db
				.prepare(
					"INSERT INTO host_inbox (host_event_id, run_id, host_cursor, type, fact_json, created_at) VALUES (?, ?, ?, ?, ?, ?)",
				)
				.run(
					hostEventId,
					runId,
					String(event.hostCursor),
					String(event.type),
					JSON.stringify(event),
					nowIso(this.clock),
				);
			// persist settlement evidence + text bytes for TurnSettled.
			if (event.type === "TurnSettled") {
				this.persistSettlement(runId, event);
			}
			this.upsertRunRows(state, nowIso(this.clock));
			this.db
				.prepare("UPDATE host_streams SET host_cursor = ?, updated_at = ? WHERE run_id = ?")
				.run(state.hostCursor, nowIso(this.clock), runId);
			this.commitAndSync();
			return { applied: true, hostCursor: state.hostCursor };
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	private persistSettlement(runId: string, event: WorkflowV2Value): void {
		const data = event.data as WorkflowV2Value;
		const settlement = data.settlement as WorkflowV2Value;
		const digest = assertDigest(String(settlement.settlementDigest), "settlementDigest");
		const result = settlement.result as WorkflowV2Value;
		const usage = settlement.usage as WorkflowV2Value | undefined;
		const resultKind = String(result.kind);
		const resultBytes = resultKind === "none" ? null : Number(result.utf8Bytes);
		const resultSha = resultKind === "text" || resultKind === "too_large" ? String(result.sha256) : null;
		this.db
			.prepare(
				"INSERT OR IGNORE INTO settlements (settlement_digest, run_id, attempt_id, outcome, result_kind, result_utf8_bytes, result_sha256, usage_json, settlement_json, created_at) " +
					"VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
			)
			.run(
				digest,
				runId,
				assertId(String(settlement.attemptId), "attemptId"),
				String(settlement.outcome),
				resultKind,
				resultBytes,
				resultSha,
				JSON.stringify(usage ?? null),
				JSON.stringify(settlement),
				nowIso(this.clock),
			);
		if (resultKind === "text") this.addTextBytes(Number(result.utf8Bytes));
	}

	// ---- outbox ---------------------------------------------------------------

	/**
	 * §7 outbox claim. Claims pending / retry_wait / lease-expired operations for
	 * the current writer epoch, sets an owner + lease deadline, and returns the
	 * SAME canonical request + host request ID. Claim expiry redelivers the same
	 * logical effect; it never creates a new one.
	 */
	claimOutbox(ownerId: string, leaseMs: number, limit: number): ClaimedOperation[] {
		this.assertOpen();
		const epoch = this.requireWriter();
		const owner = assertId(ownerId, "ownerId");
		const now = nowIso(this.clock);
		const expiresAt = new Date(Date.parse(now) + Math.max(0, leaseMs)).toISOString();
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const rows = this.db
				.prepare(
					"SELECT * FROM operations WHERE phase = 'pending' OR phase = 'retry_wait' OR (phase = 'claimed' AND claim_expires_at < ?) ORDER BY rowid LIMIT ?",
				)
				.all(now, Math.max(0, limit));
			const claimed: ClaimedOperation[] = [];
			for (const raw of rows) {
				const row = raw as Record<string, unknown>;
				const conditions = ["claim_held", "receipt_absent", "effect_unclassified"];
				validateProjectionSemantics("operation", {
					phase: "claimed",
					intent: String(row.intent),
					outcome: null,
					conditions,
				});
				this.db
					.prepare(
						"UPDATE operations SET phase = 'claimed', claim_epoch = ?, claim_owner = ?, claim_expires_at = ?, conditions = ?, updated_at = ? WHERE operation_id = ?",
					)
					.run(epoch, owner, expiresAt, JSON.stringify(conditions), now, String(row.operation_id));
				claimed.push({
					operationId: String(row.operation_id),
					runId: String(row.run_id),
					attemptId: row.attempt_id === null ? null : String(row.attempt_id),
					kind: String(row.kind),
					requestId: String(row.request_id),
					canonicalRequest: String(row.canonical_request),
					hostRequestId: row.host_request_id === null ? null : String(row.host_request_id),
					claimEpoch: epoch,
					claimExpiresAt: expiresAt,
				});
			}
			this.commitAndSync();
			return claimed;
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	/**
	 * §7 outbox acknowledge. Only the current epoch's claim owner may resolve an
	 * operation; the host request ID binds once (a different later ID conflicts).
	 * Terminalizes the operation with a classified effect + immutable receipt.
	 */
	acknowledgeOutbox(input: {
		operationId: string;
		owner: string;
		hostRequestId: string;
		outcome: "succeeded" | "failed" | "ambiguous";
		receipt: WorkflowV2Value;
	}): void {
		this.assertOpen();
		const epoch = this.requireWriter();
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const row = this.db.prepare("SELECT * FROM operations WHERE operation_id = ?").get(input.operationId) as
				| Record<string, unknown>
				| undefined;
			if (!row) {
				this.rollbackQuietly();
				failClosed("NOT_FOUND", `operation ${input.operationId} not found`);
			}
			if (Number(row.claim_epoch) !== epoch || String(row.claim_owner) !== input.owner) {
				this.rollbackQuietly();
				failClosed("STALE_CLAIM", `operation ${input.operationId} claim is stale`);
			}
			if (row.host_request_id !== null && String(row.host_request_id) !== input.hostRequestId) {
				this.rollbackQuietly();
				failClosed("OPERATION_HOST_ID_CONFLICT", "host request id rebinding is forbidden");
			}
			const conditions = ["claim_held", "receipt_present", "effect_classified"];
			validateProjectionSemantics("operation", {
				phase: "terminal",
				intent: String(row.intent),
				outcome: input.outcome,
				conditions,
			});
			this.db
				.prepare(
					"UPDATE operations SET phase = 'terminal', outcome = ?, conditions = ?, host_request_id = ?, receipt_json = ?, updated_at = ? WHERE operation_id = ?",
				)
				.run(
					input.outcome,
					JSON.stringify(conditions),
					assertId(input.hostRequestId, "hostRequestId"),
					JSON.stringify(input.receipt),
					nowIso(this.clock),
					input.operationId,
				);
			this.commitAndSync();
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	// ---- bounded reads (diagnostic; the public status/events API is slice 8) ---

	getRunProjection(runId: string): RunAggregate["run"] | null {
		this.assertOpen();
		const ctx = this.loadRunContext(runId);
		return ctx ? ctx.aggregate.run : null;
	}

	loadAggregate(runId: string): RunAggregate | null {
		this.assertOpen();
		const ctx = this.loadRunContext(runId);
		return ctx ? ctx.aggregate : null;
	}

	getCommandReceipt(requestDigest: string): CommandReceipt | null {
		this.assertOpen();
		const row = this.db.prepare("SELECT receipt_json FROM commands WHERE request_digest = ?").get(requestDigest) as
			| Record<string, unknown>
			| undefined;
		return row ? (JSON.parse(String(row.receipt_json)) as CommandReceipt) : null;
	}

	listEvents(runId: string, afterSequence: number, limit: number): Array<Record<string, unknown>> {
		this.assertOpen();
		const snapRow = this.db.prepare("SELECT value FROM store_meta WHERE key = ?").get(`snapshot:${runId}`) as
			| Record<string, unknown>
			| undefined;
		if (snapRow) {
			const snap = JSON.parse(String(snapRow.value)) as { throughSequence: number };
			if (afterSequence < snap.throughSequence)
				failClosed("SNAPSHOT_REQUIRED", `events before ${snap.throughSequence} are compacted`);
		}
		const capped = Math.min(Math.max(1, limit), 500);
		return this.db
			.prepare(
				"SELECT sequence, event_id, revision, type, controller_epoch, cancel_epoch, recorded_at, data_json, digest FROM events WHERE run_id = ? AND sequence > ? ORDER BY sequence LIMIT ?",
			)
			.all(runId, afterSequence, capped) as Array<Record<string, unknown>>;
	}

	getOperation(operationId: string): Record<string, unknown> | null {
		this.assertOpen();
		const row = this.db.prepare("SELECT * FROM operations WHERE operation_id = ?").get(operationId) as
			| Record<string, unknown>
			| undefined;
		return row ?? null;
	}

	getHostCursor(runId: string): string | null {
		this.assertOpen();
		const row = this.db.prepare("SELECT host_cursor FROM host_streams WHERE run_id = ?").get(runId) as
			| Record<string, unknown>
			| undefined;
		return row && row.host_cursor !== null ? String(row.host_cursor) : null;
	}

	/** Persisted RunTerminalized outcome for a run (survives compaction), or null. */
	private persistedTerminalOutcome(runId: string): string | null {
		const row = this.db
			.prepare(
				"SELECT data_json FROM events WHERE run_id = ? AND type = 'RunTerminalized' ORDER BY sequence DESC LIMIT 1",
			)
			.get(runId) as Record<string, unknown> | undefined;
		if (!row) return null;
		const data = JSON.parse(String(row.data_json)) as Record<string, unknown>;
		return data.outcome === undefined || data.outcome === null ? null : String(data.outcome);
	}

	/**
	 * §11/§9 recovery reconciliation: durably quarantine a terminal run whose
	 * persisted RunTerminalized outcome disagrees with the normalized runs.outcome.
	 * Reads raw rows (bypasses the loadRunContext fail-closed guard), transitions the
	 * run to `quarantined` with `integrity_failed`, clears the terminal outcome, and
	 * appends a gap-free RunQuarantined fact. Returns whether a quarantine occurred.
	 */
	reconcileTerminalMismatch(runId: string): { quarantined: boolean } {
		this.assertOpen();
		this.requireWriter();
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const runRow = this.db.prepare("SELECT * FROM runs WHERE run_id = ?").get(runId) as
				| Record<string, unknown>
				| undefined;
			if (!runRow) {
				this.rollbackQuietly();
				failClosed("NOT_FOUND", `run ${runId} not found`);
			}
			if (String(runRow.phase) !== "terminal") {
				this.db.exec("COMMIT");
				return { quarantined: false };
			}
			const persisted = this.persistedTerminalOutcome(runId);
			const normalized = runRow.outcome === null ? null : String(runRow.outcome);
			const columnAgrees = runRow.terminal_outcome === null || String(runRow.terminal_outcome) === normalized;
			if (persisted === normalized && columnAgrees) {
				this.db.exec("COMMIT");
				return { quarantined: false };
			}
			const now = nowIso(this.clock);
			const priorConditions = this.parseConditions(String(runRow.conditions)).filter(
				(c) => c !== "integrity_verified",
			);
			const quarantined = validateProjectionSemantics("run", {
				phase: "quarantined",
				intent: String(runRow.intent),
				outcome: null,
				conditions: [...new Set([...priorConditions, "integrity_failed"])],
			}) as RunAggregate["run"];
			this.db
				.prepare(
					"UPDATE runs SET phase = 'quarantined', outcome = NULL, conditions = ?, terminal_outcome = NULL, updated_at = ? WHERE run_id = ?",
				)
				.run(JSON.stringify(quarantined.conditions), now, runId);
			const nextSeq =
				Number(
					(
						this.db
							.prepare("SELECT COALESCE(MAX(sequence), 0) AS m FROM events WHERE run_id = ?")
							.get(runId) as Record<string, unknown>
					).m,
				) + 1;
			const data = { evidenceDigest: sha256Hex(`${runId}:${nextSeq}:quarantine`) };
			this.db
				.prepare(
					"INSERT INTO events (run_id, sequence, event_id, revision, type, controller_epoch, cancel_epoch, recorded_at, data_json, digest) VALUES (?, ?, ?, ?, 'RunQuarantined', ?, ?, ?, ?, ?)",
				)
				.run(
					runId,
					nextSeq,
					`reconcile-quarantine-${runId}-${nextSeq}`,
					Number(runRow.revision) + 1,
					Number(runRow.controller_epoch),
					Number(runRow.cancel_epoch),
					now,
					JSON.stringify(data),
					sha256Hex(JSON.stringify(data)),
				);
			this.commitAndSync();
			return { quarantined: true };
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	// ---- retention: erasure, fenced compaction, backup (§7) -------------------

	/**
	 * §7 text erasure. Replaces private result text with an erasure marker that
	 * keeps the original UTF-8 byte count + SHA-256, then decrements the text
	 * counter. Digests, usage, receipts, terminal facts, and topology/tombstone
	 * evidence remain. Refused while the run is nonterminal or quarantined (or when
	 * removal would drop bytes needed to verify an unresolved effect).
	 */
	eraseRunText(runId: string, policyBasis: string): { erased: boolean; freedBytes: number } {
		this.assertOpen();
		this.requireWriter();
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const runRow = this.db.prepare("SELECT phase FROM runs WHERE run_id = ?").get(runId) as
				| Record<string, unknown>
				| undefined;
			if (!runRow) {
				this.rollbackQuietly();
				failClosed("NOT_FOUND", `run ${runId} not found`);
			}
			if (String(runRow.phase) !== "terminal") {
				this.rollbackQuietly();
				failClosed("ERASURE_REFUSED", `run ${runId} is not terminal (${String(runRow.phase)})`);
			}
			const now = nowIso(this.clock);
			let freed = 0;
			const rows = this.db
				.prepare(
					"SELECT settlement_digest, result_kind, result_utf8_bytes, settlement_json FROM settlements WHERE run_id = ?",
				)
				.all(runId);
			for (const raw of rows) {
				const row = raw as Record<string, unknown>;
				if (String(row.result_kind) !== "text") continue;
				const settlement = JSON.parse(String(row.settlement_json)) as WorkflowV2Value;
				const result = settlement.result as WorkflowV2Value;
				if (!result || typeof result.text !== "string") continue;
				const bytes = Number(row.result_utf8_bytes);
				settlement.result = { kind: "erased", utf8Bytes: bytes, sha256: result.sha256, erasedAt: now, policyBasis };
				this.db
					.prepare("UPDATE settlements SET settlement_json = ? WHERE settlement_digest = ?")
					.run(JSON.stringify(settlement), String(row.settlement_digest));
				freed += bytes;
			}
			this.db.prepare("UPDATE runs SET erased = 1, updated_at = ? WHERE run_id = ?").run(now, runId);
			if (freed > 0) this.addTextBytes(-freed);
			this.commitAndSync();
			return { erased: freed > 0, freedBytes: freed };
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	/**
	 * §7 fenced compaction. Under the writer fence, after a checksummed normalized
	 * snapshot is recorded, compacts nonterminal controller-event payloads up to
	 * `throughSequence`. Immutable evidence (terminal/quarantine facts) is never
	 * compacted; all run operations must be terminal first. The recorded snapshot
	 * makes any later read of a compacted suffix return SNAPSHOT_REQUIRED.
	 */
	compactEvents(runId: string, throughSequence: number, snapshotDigest: string): { compacted: number } {
		this.assertOpen();
		this.requireWriter();
		assertDigest(snapshotDigest, "snapshotDigest");
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const runRow = this.db.prepare("SELECT host_cursor FROM runs WHERE run_id = ?").get(runId) as
				| Record<string, unknown>
				| undefined;
			if (!runRow) {
				this.rollbackQuietly();
				failClosed("NOT_FOUND", `run ${runId} not found`);
			}
			const pendingOps = Number(
				(
					this.db
						.prepare("SELECT COUNT(*) AS c FROM operations WHERE run_id = ? AND phase != 'terminal'")
						.get(runId) as Record<string, unknown>
				).c,
			);
			if (pendingOps > 0) {
				this.rollbackQuietly();
				failClosed("COMPACTION_REFUSED", `run ${runId} has ${pendingOps} nonterminal operations`);
			}
			const result = this.db
				.prepare(
					"UPDATE events SET compacted = 1, data_json = '{}' WHERE run_id = ? AND sequence <= ? AND type NOT IN ('RunTerminalized', 'RunQuarantined') AND compacted = 0",
				)
				.run(runId, throughSequence);
			// The effective snapshot boundary is the highest sequence actually compacted;
			// a read whose cursor is BELOW it can no longer be served and needs a snapshot.
			const effectiveThrough = Number(
				(
					this.db
						.prepare("SELECT COALESCE(MAX(sequence), 0) AS m FROM events WHERE run_id = ? AND compacted = 1")
						.get(runId) as Record<string, unknown>
				).m,
			);
			this.writeMeta(
				`snapshot:${runId}`,
				JSON.stringify({
					throughSequence: effectiveThrough,
					requestedThrough: throughSequence,
					snapshotDigest,
					hostCursor: runRow.host_cursor === null ? null : String(runRow.host_cursor),
				}),
			);
			this.commitAndSync();
			return { compacted: Number(result.changes) };
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	/**
	 * §7 backup boundary. Writes an owner-only consistent database image (a
	 * `serialize()` snapshot taken under the writer fence) plus a manifest binding
	 * the database SHA-256, schema/migration registry digests, root-session scope,
	 * controller epoch, host cursor set, creation time, and identity tags. Restore
	 * is offline and must reconcile before admission (slice 7); this only produces
	 * the manifest-bound image.
	 */
	backupTo(destPath: string): Record<string, unknown> {
		this.assertOpen();
		const epoch = this.requireWriter();
		const image = (this.db as unknown as { serialize(): Uint8Array }).serialize();
		const buffer = Buffer.from(image);
		mkdirSyncOwnerOnly(join(destPath, ".."));
		const { writeFileSync } = createRequire(import.meta.url)("node:fs") as typeof import("node:fs");
		writeFileSync(destPath, buffer, { mode: STORE_FILE_MODE });
		const migrationRegistryDigest = sha256Hex(MIGRATIONS.map((m) => migrationChecksum(m)).join("\n"));
		const hostCursors: Record<string, string | null> = {};
		for (const raw of this.db.prepare("SELECT run_id, host_cursor FROM host_streams").all()) {
			const row = raw as Record<string, unknown>;
			hostCursors[String(row.run_id)] = row.host_cursor === null ? null : String(row.host_cursor);
		}
		const manifest = {
			protocol: "prime.workflow.store-backup/v2-slice4",
			databaseSha256: `sha256:${createHash("sha256").update(buffer).digest("hex")}`,
			applicationId: STORE_APPLICATION_ID,
			userVersion: STORE_USER_VERSION,
			migrationRegistryDigest,
			rootScopeDigest: this.rootScopeDigest,
			controllerEpoch: epoch,
			hostCursors,
			createdAt: nowIso(this.clock),
		};
		writeFileSync(`${destPath}.manifest.json`, JSON.stringify(manifest, null, 2), { mode: STORE_FILE_MODE });
		return manifest;
	}

	tableCount(table: (typeof REQUIRED_TABLES)[number]): number {
		this.assertOpen();
		if (!REQUIRED_TABLES.includes(table)) failClosed("store_unknown_table", `unknown table ${table}`);
		return this.count(table);
	}
}
