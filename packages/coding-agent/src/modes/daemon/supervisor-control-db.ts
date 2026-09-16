/**
 * Workflow V2 Slice 3 — OS-fenced supervisor ownership Control DB (Layer B).
 *
 * Normative authority: docs/WORKFLOW-V2-SLICE3-OSFENCE.md (pi-plugin-workflow),
 * closed shapes docs/api/workflow-v2-slice3-osfence.schema.json.
 *
 * This module is the SOLE owner of the owner-only SQLite `control.db`: it opens
 * and hardens the file, runs every `BEGIN IMMEDIATE` compare-and-advance (CAS)
 * mutation of the monotonic supervisor generation (Layer B), reserves monotonic
 * worker generations and route revisions, binds the listening-endpoint identity
 * (Layer A, supplied by the caller), fsyncs durably, and evaluates the
 * synchronous `assertWriterFence()` (Form 2) that gates native-topology appends.
 *
 * Two-phase no-regression boundary (§7): this module is built DORMANT. It is
 * authority ONLY on the unreachable V2 composite-admission path. The live V1
 * daemon acquisition/socket/ledger bytes are untouched — no live module imports
 * this file. Capability negotiation stays CAPABILITY_UNAVAILABLE. Nothing here
 * enables the capability or flips global ownership authority.
 *
 * Layer A (exclusive listening-endpoint fd possession, death-released) lives in
 * daemon-socket.ts and is passed into `assertWriterFence()` as OS facts; this
 * module owns Layer B and the fence logic. The synchronous fence (Form 2) is
 * sound only under the death-released possession invariant (§3.1/§3.3): while
 * the caller holds the listening fd, no successor can advance the generation.
 */

import type { Stats } from "node:fs";
import { chmodSync, closeSync, fsyncSync, lstatSync, mkdirSync, openSync, realpathSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import type { DatabaseSync } from "node:sqlite";

// Shared OS-fence control-DB contract (single authority). This module conforms to it: the
// SupervisorControlDb class implements the durable Layer B, and SupervisorControlDbOsfAdapter
// exposes it as the OsfControlDb seam the daemon-osfence consumer injects.
import type {
	AcquisitionRecord,
	AdmissionUnknownReason,
	EndpointIdentity,
	FenceResultAdmissionUnknown,
	FenceResultGenerationStale,
	FenceResultOk,
	FenceResultOwnerMismatch,
	FenceResultOwnerUnavailable,
	FenceResultRouteStale,
	OsfControlDb,
	OsfControlDbAcquireParams,
	OsfenceAuthoritySignal,
	OsfenceCapability,
	OsfencePlatform,
	WorkerRouteRow as OsfWorkerRouteRow,
	RevocationProof,
	RouteState,
	RouteTuple,
	WriterFenceAssertion,
	WriterFenceCallSite,
} from "./osf-control-db.js";
import {
	OSFENCE_CONTROL_DB_APPLICATION_ID,
	OSFENCE_FENCE_RESULT_PROTOCOL,
	OSFENCE_MAX_GENERATION,
} from "./osf-control-db.js";

export type {
	AcquisitionRecord,
	AdmissionUnknownReason,
	EndpointIdentity,
	FenceResult,
	FenceResultAdmissionUnknown,
	FenceResultGenerationStale,
	FenceResultOk,
	FenceResultOwnerMismatch,
	FenceResultOwnerUnavailable,
	FenceResultRouteStale,
	RevocationProof,
	RouteState,
	RouteTuple,
	WriterFenceAssertion,
	WriterFenceCallSite,
} from "./osf-control-db.js";

/** SQLite `application_id` tag: 0x57325344 = "W2SD" (Workflow V2 Slice 3 DB). Shared authority. */
export const CONTROL_DB_APPLICATION_ID = OSFENCE_CONTROL_DB_APPLICATION_ID;
/** Migration counter (`PRAGMA user_version`). A newer value than this fails closed. */
export const CONTROL_DB_USER_VERSION = 1;
/** Closed-schema generation ceiling = Number.MAX_SAFE_INTEGER; overflow fails closed. Shared authority. */
export const MAX_GENERATION = OSFENCE_MAX_GENERATION;
const MAX_GENERATION_BIG = 9007199254740991n;

export const CONTROL_DB_FILE_NAME = "control.db";
const CONTROL_DB_DIR_MODE = 0o700;
const CONTROL_DB_FILE_MODE = 0o600;
/** Bounded, fixed-count (never time-based) retry for the benign first-init race only (§3.5). */
const FIRST_INIT_BUSY_RETRIES = 8;

const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const DIGEST_PATTERN = /^sha256:[0-9a-f]{64}$/;

// Aliases to the shared contract vocabulary (single authority in osf-control-db.ts). Kept as local
// names because this module's body and existing importers reference them.
export type ControlDbCapability = OsfenceCapability;
export type ControlDbPlatform = OsfencePlatform;
export type SupervisorPhase = "starting" | "owner" | "stopping";
export type AuthoritySignal = OsfenceAuthoritySignal;
// RouteState, RevocationProof, WriterFenceCallSite, EndpointIdentity, AcquisitionRecord, and
// WriterFenceAssertion are imported and re-exported from ./osf-control-db.js (see top of file).

export interface ControlDbCapabilityProbe {
	capability: ControlDbCapability;
	/** Machine reason for an `unavailable` verdict; `null` when available. */
	reason: string | null;
}

/**
 * Fail-closed error for every Control-DB hardening/integrity/mutation violation.
 * Open-time driver/flag/WAL absence does NOT throw this — it is reported as a
 * capability verdict so the live path never sees a throw (§7.4).
 */
export class SupervisorControlDbError extends Error {
	constructor(
		readonly code: string,
		message: string,
	) {
		super(message);
		this.name = "SupervisorControlDbError";
	}
}

/**
 * Raised by a Form-1 zero-row generation self-fence and by a failed Form-2
 * writer fence. Reuses the existing `supervisor_generation_stale` code (§3.4) so
 * callers that already branch on it keep working.
 */
export class SupervisorControlDbGenerationStaleError extends Error {
	readonly code = "supervisor_generation_stale" as const;

	constructor(
		readonly observedGeneration: number,
		readonly adoptedGeneration: number,
		detail?: string,
	) {
		super(
			`Supervisor generation ${adoptedGeneration} is stale; control DB observed ${observedGeneration}` +
				(detail ? ` (${detail})` : ""),
		);
		this.name = "SupervisorControlDbGenerationStaleError";
	}
}

/** Raised when the synchronous writer fence (Form 2) fails; zero native effect. */
export class SupervisorWriterFenceError extends Error {
	readonly code = "supervisor_generation_stale" as const;

	constructor(
		readonly reason: string,
		readonly assertion: WriterFenceAssertion,
	) {
		super(`Supervisor writer fence failed: ${reason}`);
		this.name = "SupervisorWriterFenceError";
	}
}

// AcquisitionRecord is imported and re-exported from ./osf-control-db.js (shared authority).

/** Closed supervisor_state row projection (schema `supervisorStateRow`). */
export interface SupervisorStateRow {
	row: "supervisor_state";
	singleton: 1;
	generation: number;
	incarnationId: string;
	endpoint: string;
	endpointDev: number | null;
	endpointIno: number | null;
	phase: SupervisorPhase;
	schemaDigest: string;
	pid: number | null;
	processStartId: string | null;
	updatedAt: string;
}

/**
 * Closed worker_routes row projection (schema `workerRouteRow`). Structurally extends the shared
 * lean {@link OsfWorkerRouteRow} the consumer reads, adding the durable `row` discriminant and the
 * diagnostic `updatedAt` this implementation persists.
 */
export interface WorkerRouteRow extends OsfWorkerRouteRow {
	row: "worker_routes";
	updatedAt: string;
}

// WriterFenceAssertion is imported and re-exported from ./osf-control-db.js (shared authority).

interface SqliteModule {
	DatabaseSync: new (path: string, options?: Record<string, unknown>) => DatabaseSync;
}

let cachedSqlite: SqliteModule | null | undefined;

/** Lazily load `node:sqlite` without throwing into a live path (§7.4). */
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
export function resetSupervisorControlDbDriverCacheForTest(): void {
	cachedSqlite = undefined;
}

/**
 * Capability probe (§7.4, BLOCK-1/BLOCK-4). Attempts to load `node:sqlite`, open
 * a throwaway in-memory WAL database with `synchronous = FULL`, and run a
 * `BEGIN IMMEDIATE` write. On ANY failure — driver absent, flag-gated, or WAL /
 * FULL / IMMEDIATE not honored — it returns `unavailable`; it NEVER throws and
 * NEVER falls back to a JSON-file/mutex or a demoted lock. Windows is
 * `unavailable` unconditionally (first-instance pipe exclusivity unproven).
 */
export function probeSupervisorControlDbCapability(): ControlDbCapabilityProbe {
	if (process.platform === "win32") {
		return { capability: "unavailable", reason: "windows_unsupported" };
	}
	const sqlite = loadSqlite();
	if (!sqlite) {
		return { capability: "unavailable", reason: "driver_absent" };
	}
	let db: DatabaseSync | undefined;
	let probeDir: string | undefined;
	try {
		// A WAL journal on `:memory:` reports `memory`, not `wal`; probe a real file
		// under an owner-only temp dir so the WAL/FULL/IMMEDIATE path is exercised.
		probeDir = mkdirSyncOwnerOnly(join(tmpProbeRoot(), `wf-osfence-probe-${process.pid}-${Date.now()}`));
		const probePath = join(probeDir, CONTROL_DB_FILE_NAME);
		db = new sqlite.DatabaseSync(probePath, { timeout: 0 });
		const journal = readScalar(db as DatabaseSync, "PRAGMA journal_mode = WAL", "journal_mode");
		if (String(journal).toLowerCase() !== "wal") {
			return { capability: "unavailable", reason: "wal_unavailable" };
		}
		(db as DatabaseSync).exec("PRAGMA synchronous = FULL");
		const sync = readScalar(db as DatabaseSync, "PRAGMA synchronous", "synchronous");
		if (Number(sync) !== 2) {
			return { capability: "unavailable", reason: "synchronous_full_unavailable" };
		}
		(db as DatabaseSync).exec("CREATE TABLE probe (a INTEGER PRIMARY KEY, b INTEGER NOT NULL) STRICT");
		(db as DatabaseSync).exec("BEGIN IMMEDIATE");
		const changes = (db as DatabaseSync).prepare("INSERT INTO probe (a, b) VALUES (1, 1)").run().changes;
		if (Number(changes) !== 1) {
			(db as DatabaseSync).exec("ROLLBACK");
			return { capability: "unavailable", reason: "immediate_write_unavailable" };
		}
		(db as DatabaseSync).exec("COMMIT");
		try {
			db.close();
		} finally {
			db = undefined;
		}
		return { capability: "available", reason: null };
	} catch {
		return { capability: "unavailable", reason: "probe_failed" };
	} finally {
		if (db) {
			try {
				db.close();
			} catch {
				// The probe verdict already stands; a close failure is not authority.
			}
		}
		if (probeDir) removeTreeQuietly(probeDir);
	}
}

// ---------------------------------------------------------------------------
// Filesystem hardening + durability helpers (self-contained; no live-path deps)
// ---------------------------------------------------------------------------

function tmpProbeRoot(): string {
	const suffix = typeof process.getuid === "function" ? String(process.getuid()) : "user";
	return join(tmpdir(), `prime-agent-${suffix}`);
}

function mkdirSyncOwnerOnly(path: string): string {
	mkdirSync(path, { recursive: true, mode: CONTROL_DB_DIR_MODE });
	return path;
}

function removeTreeQuietly(path: string): void {
	try {
		rmSync(path, { recursive: true, force: true });
	} catch {
		// Best effort: a throwaway probe tree left behind is not authority.
	}
}

function readScalar(db: DatabaseSync, sql: string, column: string): unknown {
	const row = db.prepare(sql).get();
	return row ? (row as Record<string, unknown>)[column] : undefined;
}

function isErrnoCode(error: unknown, code: string): boolean {
	return (error as NodeJS.ErrnoException | undefined)?.code === code;
}

/** SQLITE_BUSY (errcode 5): a racing BEGIN IMMEDIATE holds the write lock. */
function isErrnoSqliteBusy(error: unknown): boolean {
	return (error as { errcode?: number } | undefined)?.errcode === 5;
}

function failClosed(code: string, message: string): never {
	throw new SupervisorControlDbError(code, message);
}

/**
 * Canonicalize an existing directory to its physical path so an escape check
 * cannot be defeated by a symlinked ancestor.
 */
function canonicalizeExistingDir(path: string): string {
	return realpathSync.native(resolve(path));
}

/**
 * Reject a symlink planted at any control-root child component. The root's own
 * ancestors are already canonical (the root is realpath'd before this runs); only
 * the entries the daemon creates under it are checked.
 */
function assertNoSymlinkUnderRoot(canonicalRoot: string, target: string): void {
	const relative = resolve(target).slice(canonicalRoot.length);
	if (!resolve(target).startsWith(canonicalRoot + sep) && resolve(target) !== canonicalRoot) {
		failClosed("control_db_path_escape", `Control DB path escapes control root: ${target}`);
	}
	const components = relative.split(sep).filter((part) => part.length > 0);
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
		if (stat.isSymbolicLink()) {
			failClosed("control_db_symlink_component", `Symlink component under control root: ${current}`);
		}
	}
}

/** Directory must be an owner-only, non-symlinked real directory. */
function assertHardenedControlDir(root: string): string {
	let stat: Stats;
	try {
		stat = lstatSync(root);
	} catch (error) {
		if (isErrnoCode(error, "ENOENT")) {
			failClosed("control_db_root_missing", `Control root missing: ${root}`);
		}
		throw error;
	}
	if (stat.isSymbolicLink()) {
		failClosed("control_db_root_symlink", `Control root is a symlink: ${root}`);
	}
	if (!stat.isDirectory()) {
		failClosed("control_db_root_not_dir", `Control root is not a directory: ${root}`);
	}
	assertOwnedPrivate(root, stat, CONTROL_DB_DIR_MODE);
	return canonicalizeExistingDir(root);
}

/** File (when it exists) must be an owner-only, non-symlinked regular file. */
function assertHardenedControlFile(canonicalRoot: string, filePath: string): void {
	assertNoSymlinkUnderRoot(canonicalRoot, filePath);
	let stat: Stats;
	try {
		stat = lstatSync(filePath);
	} catch (error) {
		if (isErrnoCode(error, "ENOENT")) return;
		throw error;
	}
	if (stat.isSymbolicLink()) {
		failClosed("control_db_file_symlink", `Control DB is a symlink: ${filePath}`);
	}
	if (!stat.isFile()) {
		failClosed("control_db_not_regular_file", `Control DB is not a regular file: ${filePath}`);
	}
	assertOwnedPrivate(filePath, stat, CONTROL_DB_FILE_MODE);
	const canonicalFile = realpathSync.native(filePath);
	if (canonicalFile !== join(canonicalRoot, CONTROL_DB_FILE_NAME)) {
		failClosed("control_db_path_escape", `Control DB realpath escapes control root: ${canonicalFile}`);
	}
}

function assertOwnedPrivate(path: string, stat: Stats, allowedMode: number): void {
	if (typeof process.getuid === "function" && stat.uid !== process.getuid()) {
		failClosed("control_db_wrong_owner", `Control path not owned by current user: ${path}`);
	}
	// No group/other bits: everything outside the owner triad must be clear.
	if ((stat.mode & 0o077) !== 0) {
		failClosed("control_db_mode_too_open", `Control path is group/other-accessible: ${path}`);
	}
	if ((stat.mode & 0o777 & ~allowedMode) !== 0) {
		failClosed("control_db_mode_too_open", `Control path mode exceeds ${allowedMode.toString(8)}: ${path}`);
	}
}

/** fsync the containing directory so file/WAL-sidecar names are durable (§3.5). */
function fsyncDir(dir: string): void {
	let descriptor: number | undefined;
	try {
		descriptor = openSync(dir, "r");
		fsyncSync(descriptor);
	} finally {
		if (descriptor !== undefined) closeSync(descriptor);
	}
}

function nowIso(): string {
	return new Date().toISOString();
}

function assertId(value: string, field: string): string {
	if (!ID_PATTERN.test(value)) failClosed("control_db_invalid_id", `Invalid ${field}: ${value}`);
	return value;
}

function assertDigest(value: string, field: string): string {
	if (!DIGEST_PATTERN.test(value)) failClosed("control_db_invalid_digest", `Invalid ${field}: ${value}`);
	return value;
}

function assertGenerationInRange(value: number, field: string): number {
	if (!Number.isInteger(value) || value < 1 || value > MAX_GENERATION) {
		failClosed("control_db_generation_range", `${field} out of range [1, ${MAX_GENERATION}]: ${value}`);
	}
	return value;
}

/** A prior/observed counter read from the DB: 0 (no prior) up to MAX_GENERATION. */
function assertGenerationInRangeOrZero(value: number): number {
	if (!Number.isInteger(value) || value < 0 || value > MAX_GENERATION) {
		failClosed("control_db_generation_range", `Stored counter out of range [0, ${MAX_GENERATION}]: ${value}`);
	}
	return value;
}

// ---------------------------------------------------------------------------
// SupervisorControlDb — Layer B authority owner
// ---------------------------------------------------------------------------

export type DurabilityEvent = { op: "commit" | "fsync_dir"; path: string };

export interface OpenControlDbOptions {
	/** Control root: `<registryRoot>/<endpointKey>/` (owner-only). */
	root: string;
	/** sha256 of the closed capability/schema descriptor bound into the row. */
	schemaDigest: string;
	/** Random per-process incarnation; diagnostic/channel binding only. */
	incarnationId: string;
	/** Bound listening-endpoint identity (Layer A fact). win32 fails closed. */
	endpointIdentity: EndpointIdentity;
	/** DIAGNOSTIC ONLY. */
	pid?: number | null;
	/** DIAGNOSTIC ONLY. */
	processStartId?: string | null;
	/** Test/audit hook to observe durable-write ordering (commit then fsync_dir). */
	onDurabilityEvent?: (event: DurabilityEvent) => void;
}

export interface TakeoverProof {
	revocationProof: RevocationProof;
}

export interface AcquireOptions {
	/** Present for a takeover; absent for first init. Enforced against DB state. */
	takeover?: TakeoverProof;
}

export interface WriteRouteInput {
	rootSessionId: string;
	directParentSessionId: string;
	workerId: string;
	workerGeneration: number;
	state: RouteState;
	descriptorDigest: string;
}

export interface WriterFenceInput {
	endpointPossessed: boolean;
	endpointLeaseCompromised: boolean;
	observedEndpointIdentity: EndpointIdentity;
	callSite: WriterFenceCallSite;
}

const DDL = [
	"CREATE TABLE supervisor_state (",
	"  singleton        INTEGER PRIMARY KEY CHECK (singleton = 1),",
	"  generation       INTEGER NOT NULL CHECK (generation >= 1 AND generation <= 9007199254740991),",
	"  incarnation_id   TEXT    NOT NULL,",
	"  endpoint         TEXT    NOT NULL,",
	"  endpoint_dev     INTEGER,",
	"  endpoint_ino     INTEGER,",
	"  phase            TEXT    NOT NULL CHECK (phase IN ('starting','owner','stopping')),",
	"  schema_digest    TEXT    NOT NULL,",
	"  pid              INTEGER,",
	"  process_start_id TEXT,",
	"  updated_at       TEXT    NOT NULL",
	") STRICT;",
	"CREATE TABLE worker_generation_counters (",
	"  worker_id        TEXT PRIMARY KEY,",
	"  last_generation  INTEGER NOT NULL CHECK (last_generation >= 1 AND last_generation <= 9007199254740991)",
	") STRICT;",
	"CREATE TABLE worker_routes (",
	"  root_session_id          TEXT NOT NULL,",
	"  direct_parent_session_id TEXT NOT NULL,",
	"  worker_id                TEXT NOT NULL,",
	"  worker_generation        INTEGER NOT NULL CHECK (worker_generation >= 1),",
	"  route_revision           INTEGER NOT NULL CHECK (route_revision >= 1),",
	"  state                    TEXT NOT NULL CHECK (state IN ('starting','ready','recovering','stopping','failed')),",
	"  descriptor_digest        TEXT NOT NULL,",
	"  updated_by_generation    INTEGER NOT NULL,",
	"  updated_at               TEXT NOT NULL,",
	"  PRIMARY KEY (root_session_id, direct_parent_session_id)",
	") STRICT;",
	"CREATE INDEX worker_routes_by_worker ON worker_routes (worker_id, worker_generation);",
].join("\n");

const REQUIRED_TABLES = ["supervisor_state", "worker_generation_counters", "worker_routes"] as const;

function endpointIdentityEquals(a: EndpointIdentity, b: EndpointIdentity): boolean {
	return (
		a.endpoint === b.endpoint &&
		a.endpointDev === b.endpointDev &&
		a.endpointIno === b.endpointIno &&
		a.platform === b.platform
	);
}

function assertUsableEndpointIdentity(identity: EndpointIdentity): EndpointIdentity {
	if (!identity.endpoint || identity.endpoint.length === 0) {
		failClosed("control_db_invalid_endpoint", "Endpoint identity is missing an endpoint name");
	}
	if (identity.platform === "win32") {
		// BLOCK-4 / §2.1 / §11.8: first-instance pipe exclusivity is unproven.
		failClosed("control_db_windows_unavailable", "Slice 3 is unavailable on Windows (BLOCK-4)");
	}
	if (identity.endpointDev === null || identity.endpointIno === null) {
		failClosed("control_db_invalid_endpoint", "Unix endpoint identity requires dev and ino");
	}
	return identity;
}

export class SupervisorControlDb {
	private closed = false;
	private adoptedGeneration: number | undefined;

	private constructor(
		private readonly db: DatabaseSync,
		private readonly root: string,
		private readonly dbPath: string,
		private readonly schemaDigest: string,
		private readonly incarnationId: string,
		private readonly endpointIdentity: EndpointIdentity,
		private readonly pid: number | null,
		private readonly processStartId: string | null,
		private readonly onDurabilityEvent: ((event: DurabilityEvent) => void) | undefined,
	) {}

	/**
	 * Open + harden + validate the Control DB. Fails closed (throws
	 * SupervisorControlDbError) on any hardening/integrity/foreign/torn/overflow
	 * violation. Only reachable on the V2 path; callers gate on
	 * probeSupervisorControlDbCapability() first so the live path never throws.
	 */
	static open(options: OpenControlDbOptions): SupervisorControlDb {
		const endpointIdentity = assertUsableEndpointIdentity(options.endpointIdentity);
		const schemaDigest = assertDigest(options.schemaDigest, "schemaDigest");
		const incarnationId = assertId(options.incarnationId, "incarnationId");

		const sqlite = loadSqlite();
		if (!sqlite) {
			failClosed("control_db_driver_unavailable", "node:sqlite is unavailable (probe should have gated this)");
		}

		mkdirSyncOwnerOnly(options.root);
		const canonicalRoot = assertHardenedControlDir(options.root);
		const dbPath = join(canonicalRoot, CONTROL_DB_FILE_NAME);
		assertHardenedControlFile(canonicalRoot, dbPath);

		const db = new sqlite.DatabaseSync(dbPath, { timeout: 0 });
		try {
			SupervisorControlDb.applyPragmas(db);
			// Enforce owner-only mode after WAL sidecars are created.
			for (const suffix of ["", "-wal", "-shm"]) {
				try {
					chmodSync(`${dbPath}${suffix}`, CONTROL_DB_FILE_MODE);
				} catch (error) {
					if (!isErrnoCode(error, "ENOENT")) throw error;
				}
			}
			SupervisorControlDb.assertIntegrity(db);
			SupervisorControlDb.initializeOrValidateSchema(db);
			SupervisorControlDb.assertStoredGenerationInRange(db);
		} catch (error) {
			try {
				db.close();
			} catch {
				// Fail-closed already; a close error must not mask the original cause.
			}
			throw error;
		}

		const instance = new SupervisorControlDb(
			db,
			canonicalRoot,
			dbPath,
			schemaDigest,
			incarnationId,
			endpointIdentity,
			options.pid ?? (typeof process.pid === "number" ? process.pid : null),
			options.processStartId ?? null,
			options.onDurabilityEvent,
		);
		instance.emitDurability({ op: "fsync_dir", path: canonicalRoot });
		fsyncDir(canonicalRoot);
		return instance;
	}

	private static applyPragmas(db: DatabaseSync): void {
		const journal = readScalar(db, "PRAGMA journal_mode = WAL", "journal_mode");
		if (String(journal).toLowerCase() !== "wal") {
			failClosed("control_db_wal_unavailable", `journal_mode is ${String(journal)}, not wal`);
		}
		db.exec("PRAGMA synchronous = FULL");
		if (Number(readScalar(db, "PRAGMA synchronous", "synchronous")) !== 2) {
			failClosed("control_db_synchronous_unavailable", "synchronous is not FULL");
		}
		db.exec("PRAGMA foreign_keys = ON");
		db.exec("PRAGMA busy_timeout = 0");
		db.exec("PRAGMA wal_autocheckpoint = 256");
		db.exec("PRAGMA cell_size_check = ON");
	}

	private static assertIntegrity(db: DatabaseSync): void {
		const result = readScalar(db, "PRAGMA integrity_check", "integrity_check");
		if (String(result) !== "ok") {
			failClosed("control_db_integrity_failed", `integrity_check returned ${String(result)}`);
		}
	}

	private static tableNames(db: DatabaseSync): Set<string> {
		const rows = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all();
		return new Set(rows.map((row) => String((row as Record<string, unknown>).name)));
	}

	private static initializeOrValidateSchema(db: DatabaseSync): void {
		const applicationId = Number(readScalar(db, "PRAGMA application_id", "application_id"));
		const tables = SupervisorControlDb.tableNames(db);
		if (applicationId === 0) {
			// A pristine DB has application_id 0 and no user tables. Anything else with
			// a zero tag is a foreign or torn file — fail closed, never adopt it.
			const userTables = [...tables].filter((name) => !name.startsWith("sqlite_"));
			if (userTables.length > 0) {
				failClosed("control_db_foreign", "Untagged control DB already has tables; refusing to adopt");
			}
			db.exec("BEGIN IMMEDIATE");
			try {
				db.exec(DDL);
				db.exec(`PRAGMA application_id = ${CONTROL_DB_APPLICATION_ID}`);
				db.exec(`PRAGMA user_version = ${CONTROL_DB_USER_VERSION}`);
				db.exec("COMMIT");
			} catch (error) {
				try {
					db.exec("ROLLBACK");
				} catch {
					// Best effort; the outer open() closes and rethrows.
				}
				throw error;
			}
			return;
		}
		if (applicationId !== CONTROL_DB_APPLICATION_ID) {
			failClosed("control_db_foreign", `Foreign application_id ${applicationId}`);
		}
		const userVersion = Number(readScalar(db, "PRAGMA user_version", "user_version"));
		if (userVersion > CONTROL_DB_USER_VERSION) {
			failClosed(
				"control_db_user_version_newer",
				`user_version ${userVersion} is newer than ${CONTROL_DB_USER_VERSION}`,
			);
		}
		if (userVersion !== CONTROL_DB_USER_VERSION) {
			failClosed(
				"control_db_user_version_torn",
				`user_version ${userVersion} does not match ${CONTROL_DB_USER_VERSION}`,
			);
		}
		for (const required of REQUIRED_TABLES) {
			if (!tables.has(required)) {
				failClosed("control_db_torn_schema", `Missing required table ${required}`);
			}
		}
	}

	private static assertStoredGenerationInRange(db: DatabaseSync): void {
		const statement = db.prepare("SELECT generation FROM supervisor_state WHERE singleton = 1");
		statement.setReadBigInts(true);
		let row: Record<string, unknown> | undefined;
		try {
			row = statement.get();
		} catch {
			failClosed("control_db_generation_range", "Stored generation is not representable");
		}
		if (!row) return;
		const value = row.generation as bigint;
		if (typeof value !== "bigint" || value < 1n || value > MAX_GENERATION_BIG) {
			failClosed("control_db_generation_range", `Stored generation out of range: ${String(value)}`);
		}
	}

	private emitDurability(event: DurabilityEvent): void {
		try {
			this.onDurabilityEvent?.(event);
		} catch {
			// Durability observation is diagnostic; it must never affect the commit.
		}
	}

	private assertOpen(): void {
		if (this.closed) failClosed("control_db_closed", "Control DB handle is closed");
	}

	private requireAdopted(): number {
		if (this.adoptedGeneration === undefined) {
			failClosed("control_db_not_acquired", "Ownership has not been acquired on this handle");
		}
		return this.adoptedGeneration;
	}

	private readGenerationBig(): bigint | null {
		const statement = this.db.prepare("SELECT generation FROM supervisor_state WHERE singleton = 1");
		statement.setReadBigInts(true);
		const row = statement.get();
		if (!row) return null;
		const value = (row as Record<string, unknown>).generation as bigint;
		if (typeof value !== "bigint" || value < 1n || value > MAX_GENERATION_BIG) {
			failClosed("control_db_generation_range", `Stored generation out of range: ${String(value)}`);
		}
		return value;
	}

	/** Single-row generation read, no transaction; used by Form-2 cross-check. */
	readGenerationUnchecked(): number {
		this.assertOpen();
		const value = this.readGenerationBig();
		if (value === null) failClosed("control_db_uninitialized", "supervisor_state row is missing");
		return Number(value);
	}

	get adopted(): number | undefined {
		return this.adoptedGeneration;
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
			// A rollback on an already-aborted transaction is a no-op we can ignore.
		}
	}

	/**
	 * §4.1–§4.2 endpoint→control-DB acquisition. first_init writes generation 1;
	 * takeover advances exactly one generation and REQUIRES a §4.3 revocation proof.
	 * Runs entirely inside one BEGIN IMMEDIATE; the CAS `WHERE generation = prior`
	 * makes a lost race abort with zero effect. A benign first-init SQLITE_BUSY is
	 * retried a fixed number of times (never time-based); any other loss fails closed.
	 */
	acquire(options: AcquireOptions = {}): AcquisitionRecord {
		this.assertOpen();
		if (this.adoptedGeneration !== undefined) {
			failClosed("control_db_already_acquired", "This handle already acquired ownership");
		}
		for (let attempt = 0; ; attempt++) {
			try {
				return this.acquireOnce(options);
			} catch (error) {
				if (isErrnoSqliteBusy(error) && attempt < FIRST_INIT_BUSY_RETRIES && !this.rowExistsSafe()) {
					this.rollbackQuietly();
					continue;
				}
				this.rollbackQuietly();
				throw error;
			}
		}
	}

	private rowExistsSafe(): boolean {
		try {
			return this.readGenerationBig() !== null;
		} catch {
			return false;
		}
	}

	private acquireOnce(options: AcquireOptions): AcquisitionRecord {
		const now = nowIso();
		this.db.exec("BEGIN IMMEDIATE");
		const prior = this.readGenerationBig();
		if (prior === null) {
			if (options.takeover) {
				this.rollbackQuietly();
				failClosed("control_db_first_init_with_proof", "first_init cannot carry a revocation proof");
			}
			const changes = this.db
				.prepare(
					"INSERT INTO supervisor_state " +
						"(singleton, generation, incarnation_id, endpoint, endpoint_dev, endpoint_ino, phase, schema_digest, pid, process_start_id, updated_at) " +
						"VALUES (1, 1, :inc, :endpoint, :dev, :ino, 'starting', :digest, :pid, :startId, :now)",
				)
				.run({
					inc: this.incarnationId,
					endpoint: this.endpointIdentity.endpoint,
					dev: this.endpointIdentity.endpointDev,
					ino: this.endpointIdentity.endpointIno,
					digest: this.schemaDigest,
					pid: this.pid,
					startId: this.processStartId,
					now,
				}).changes;
			if (Number(changes) !== 1) {
				this.rollbackQuietly();
				failClosed("control_db_first_init_failed", `first_init affected ${String(changes)} rows`);
			}
			this.commitAndSync();
			this.adoptedGeneration = 1;
			return this.buildAcquisitionRecord("first_init", 0, 1, null, now);
		}

		// takeover
		if (!options.takeover) {
			this.rollbackQuietly();
			failClosed("control_db_takeover_without_proof", "takeover requires a §4.3 revocation proof");
		}
		const priorNumber = Number(prior);
		if (priorNumber + 1 > MAX_GENERATION) {
			this.rollbackQuietly();
			failClosed("control_db_generation_overflow", "generation would overflow MAX_SAFE_INTEGER");
		}
		const next = priorNumber + 1;
		const changes = this.db
			.prepare(
				"UPDATE supervisor_state SET generation = :next, incarnation_id = :inc, endpoint = :endpoint, " +
					"endpoint_dev = :dev, endpoint_ino = :ino, phase = 'starting', schema_digest = :digest, " +
					"pid = :pid, process_start_id = :startId, updated_at = :now " +
					"WHERE singleton = 1 AND generation = :prior",
			)
			.run({
				next,
				inc: this.incarnationId,
				endpoint: this.endpointIdentity.endpoint,
				dev: this.endpointIdentity.endpointDev,
				ino: this.endpointIdentity.endpointIno,
				digest: this.schemaDigest,
				pid: this.pid,
				startId: this.processStartId,
				now,
				prior: priorNumber,
			}).changes;
		if (Number(changes) !== 1) {
			this.rollbackQuietly();
			throw new SupervisorControlDbGenerationStaleError(priorNumber, priorNumber, "takeover CAS matched no row");
		}
		this.commitAndSync();
		this.adoptedGeneration = next;
		return this.buildAcquisitionRecord("takeover", priorNumber, next, options.takeover.revocationProof, now);
	}

	private buildAcquisitionRecord(
		kind: "first_init" | "takeover",
		priorGeneration: number,
		generation: number,
		revocationProof: RevocationProof | null,
		acquiredAt: string,
	): AcquisitionRecord {
		return {
			protocol: "prime.workflow.osfence-acquisition/v2-slice3",
			kind,
			priorGeneration,
			generation,
			endpointIdentity: this.endpointIdentity,
			incarnationId: this.incarnationId,
			schemaDigest: this.schemaDigest,
			authoritySignal: "control_db_generation_cas",
			revocationProof,
			capability: "available",
			acquiredAt,
		};
	}

	/**
	 * Form 1 (§3.3): the BEGIN IMMEDIATE compare-and-write for a control-DB
	 * mutation. The generation self-fence and the payload write live in one
	 * transaction, so a stale generation cannot commit the payload. A zero-row
	 * self-fence rolls back and raises GENERATION_STALE with zero effect.
	 */
	private fencedMutation<T>(mutate: (db: DatabaseSync, now: string) => T): T {
		this.assertOpen();
		if (this.adoptedGeneration === undefined) {
			failClosed("control_db_not_acquired", "Cannot run a fenced mutation before acquire()");
		}
		const adopted = this.adoptedGeneration;
		const now = nowIso();
		this.db.exec("BEGIN IMMEDIATE");
		try {
			const observed = this.readGenerationBig();
			if (observed === null) {
				this.rollbackQuietly();
				failClosed("control_db_uninitialized", "supervisor_state row is missing");
			}
			const observedNumber = Number(observed);
			if (observedNumber !== adopted) {
				this.rollbackQuietly();
				throw new SupervisorControlDbGenerationStaleError(observedNumber, adopted, "fenced mutation");
			}
			const selfFence = this.db
				.prepare("UPDATE supervisor_state SET updated_at = :now WHERE singleton = 1 AND generation = :adopted")
				.run({ now, adopted }).changes;
			if (Number(selfFence) !== 1) {
				this.rollbackQuietly();
				throw new SupervisorControlDbGenerationStaleError(observedNumber, adopted, "self-fence matched no row");
			}
			const result = mutate(this.db, now);
			this.commitAndSync();
			return result;
		} catch (error) {
			this.rollbackQuietly();
			throw error;
		}
	}

	/**
	 * §5.3 W1: reserve `last_generation + 1` for a worker inside the writer fence,
	 * before a replacement worker starts. Monotone per worker; overflow fails closed.
	 */
	reserveWorkerGeneration(workerId: string): number {
		const id = assertId(workerId, "workerId");
		return this.fencedMutation((db) => {
			const existing = db
				.prepare("SELECT last_generation FROM worker_generation_counters WHERE worker_id = :id")
				.get({ id });
			const priorValue = existing ? Number((existing as Record<string, unknown>).last_generation) : 0;
			assertGenerationInRangeOrZero(priorValue);
			const next = priorValue + 1;
			if (next > MAX_GENERATION) failClosed("control_db_generation_overflow", "worker generation overflow");
			const changes = db
				.prepare(
					"INSERT INTO worker_generation_counters (worker_id, last_generation) VALUES (:id, :next) " +
						"ON CONFLICT (worker_id) DO UPDATE SET last_generation = :next",
				)
				.run({ id, next }).changes;
			if (Number(changes) !== 1)
				failClosed("control_db_worker_gen_failed", `worker counter affected ${String(changes)} rows`);
			return next;
		});
	}

	/**
	 * §5.3 R1: transactionally write/advance a route. route_revision is monotone
	 * per (root, directParent); every mutation bumps it. updated_by_generation
	 * records the adopting supervisor generation.
	 */
	writeRoute(input: WriteRouteInput): WorkerRouteRow {
		const rootSessionId = assertId(input.rootSessionId, "rootSessionId");
		const directParentSessionId = assertId(input.directParentSessionId, "directParentSessionId");
		const workerId = assertId(input.workerId, "workerId");
		const workerGeneration = assertGenerationInRange(input.workerGeneration, "workerGeneration");
		const descriptorDigest = assertDigest(input.descriptorDigest, "descriptorDigest");
		const state = input.state;
		const adopted = this.requireAdopted();
		return this.fencedMutation((db, now) => {
			const existing = db
				.prepare(
					"SELECT route_revision FROM worker_routes WHERE root_session_id = :root AND direct_parent_session_id = :parent",
				)
				.get({ root: rootSessionId, parent: directParentSessionId });
			const priorRevision = existing ? Number((existing as Record<string, unknown>).route_revision) : 0;
			assertGenerationInRangeOrZero(priorRevision);
			const routeRevision = priorRevision + 1;
			if (routeRevision > MAX_GENERATION) failClosed("control_db_generation_overflow", "route revision overflow");
			const changes = db
				.prepare(
					"INSERT INTO worker_routes " +
						"(root_session_id, direct_parent_session_id, worker_id, worker_generation, route_revision, state, descriptor_digest, updated_by_generation, updated_at) " +
						"VALUES (:root, :parent, :worker, :wgen, :rev, :state, :digest, :adopted, :now) " +
						"ON CONFLICT (root_session_id, direct_parent_session_id) DO UPDATE SET " +
						"worker_id = :worker, worker_generation = :wgen, route_revision = :rev, state = :state, " +
						"descriptor_digest = :digest, updated_by_generation = :adopted, updated_at = :now",
				)
				.run({
					root: rootSessionId,
					parent: directParentSessionId,
					worker: workerId,
					wgen: workerGeneration,
					rev: routeRevision,
					state,
					digest: descriptorDigest,
					adopted,
					now,
				}).changes;
			if (Number(changes) !== 1)
				failClosed("control_db_route_write_failed", `route write affected ${String(changes)} rows`);
			return {
				row: "worker_routes",
				rootSessionId,
				directParentSessionId,
				workerId,
				workerGeneration,
				routeRevision,
				state,
				descriptorDigest,
				updatedByGeneration: adopted,
				updatedAt: now,
			};
		});
	}

	/** §4.1 step 6 / §4.5: move the supervisor phase via a fenced mutation. */
	setPhase(phase: SupervisorPhase): void {
		this.fencedMutation((db, now) => {
			const changes = db
				.prepare(
					"UPDATE supervisor_state SET phase = :phase, updated_at = :now WHERE singleton = 1 AND generation = :adopted",
				)
				.run({ phase, now, adopted: this.requireAdopted() }).changes;
			if (Number(changes) !== 1) failClosed("control_db_phase_failed", "phase update matched no row");
		});
	}

	/**
	 * Form 2 (§3.3): the synchronous writer fence that gates a native-topology
	 * append (the RlmSpawnLedger O_APPEND write that cannot join a SQL
	 * transaction). It is re-evaluated at the actual write boundary, after any
	 * await, at both call sites (`pre_replay_verify`, `pre_durable_append`).
	 *
	 * Soundness rests on the death-released possession invariant (§3.1): while the
	 * caller holds the listening endpoint fd, no successor can advance the
	 * generation, so check (a) — possession + identity — is a complete proof. Check
	 * (b) — the durable generation cross-check — is defense in depth and fails
	 * closed loudly on an out-of-band edit. Returns the pass assertion; throws
	 * SupervisorWriterFenceError (zero native effect) on any failure.
	 */
	assertWriterFence(input: WriterFenceInput): WriterFenceAssertion {
		this.assertOpen();
		if (this.adoptedGeneration === undefined) {
			failClosed("control_db_not_acquired", "Cannot assert the writer fence before acquire()");
		}
		const adoptedGeneration = this.adoptedGeneration;
		const observedGeneration = this.readGenerationUnchecked();
		const boundEndpointIdentity = this.endpointIdentity;
		const observedEndpointIdentity = input.observedEndpointIdentity;
		const possessionOk = input.endpointPossessed && !input.endpointLeaseCompromised;
		const identityOk = endpointIdentityEquals(boundEndpointIdentity, observedEndpointIdentity);
		const generationOk = adoptedGeneration === observedGeneration;
		const pass = possessionOk && identityOk && generationOk;
		const assertion: WriterFenceAssertion = {
			protocol: "prime.workflow.osfence-writer-fence/v2-slice3",
			form: "synchronous",
			endpointPossessed: input.endpointPossessed,
			endpointLeaseCompromised: input.endpointLeaseCompromised,
			boundEndpointIdentity,
			observedEndpointIdentity,
			adoptedGeneration,
			observedGeneration,
			callSite: input.callSite,
			outcome: pass ? "pass" : "fail",
			assertedAt: nowIso(),
		};
		if (!pass) {
			const reason = !input.endpointPossessed
				? "endpoint possession lost"
				: input.endpointLeaseCompromised
					? "endpoint lease compromised"
					: !identityOk
						? "endpoint identity changed"
						: "supervisor generation advanced";
			throw new SupervisorWriterFenceError(reason, assertion);
		}
		return assertion;
	}

	/** Diagnostic snapshot of the single supervisor_state row. */
	readSupervisorStateRow(): SupervisorStateRow | null {
		this.assertOpen();
		const statement = this.db.prepare(
			"SELECT generation, incarnation_id, endpoint, endpoint_dev, endpoint_ino, phase, schema_digest, pid, process_start_id, updated_at " +
				"FROM supervisor_state WHERE singleton = 1",
		);
		statement.setReadBigInts(true);
		const row = statement.get() as Record<string, unknown> | undefined;
		if (!row) return null;
		const generation = row.generation as bigint;
		if (typeof generation !== "bigint" || generation < 1n || generation > MAX_GENERATION_BIG) {
			failClosed("control_db_generation_range", "Stored generation out of range");
		}
		return {
			row: "supervisor_state",
			singleton: 1,
			generation: Number(generation),
			incarnationId: String(row.incarnation_id),
			endpoint: String(row.endpoint),
			endpointDev: row.endpoint_dev === null ? null : Number(row.endpoint_dev),
			endpointIno: row.endpoint_ino === null ? null : Number(row.endpoint_ino),
			phase: String(row.phase) as SupervisorPhase,
			schemaDigest: String(row.schema_digest),
			pid: row.pid === null ? null : Number(row.pid),
			processStartId: row.process_start_id === null ? null : String(row.process_start_id),
			updatedAt: String(row.updated_at),
		};
	}

	/** Diagnostic snapshot of one route row, or null when absent. */
	readWorkerRoute(rootSessionId: string, directParentSessionId: string): WorkerRouteRow | null {
		this.assertOpen();
		const row = this.db
			.prepare(
				"SELECT root_session_id, direct_parent_session_id, worker_id, worker_generation, route_revision, state, descriptor_digest, updated_by_generation, updated_at " +
					"FROM worker_routes WHERE root_session_id = :root AND direct_parent_session_id = :parent",
			)
			.get({ root: rootSessionId, parent: directParentSessionId }) as Record<string, unknown> | undefined;
		if (!row) return null;
		return {
			row: "worker_routes",
			rootSessionId: String(row.root_session_id),
			directParentSessionId: String(row.direct_parent_session_id),
			workerId: String(row.worker_id),
			workerGeneration: Number(row.worker_generation),
			routeRevision: Number(row.route_revision),
			state: String(row.state) as RouteState,
			descriptorDigest: String(row.descriptor_digest),
			updatedByGeneration: Number(row.updated_by_generation),
			updatedAt: String(row.updated_at),
		};
	}

	/** Last-generation counter for a worker, or 0 when none is reserved yet. */
	readWorkerGeneration(workerId: string): number {
		this.assertOpen();
		const row = this.db
			.prepare("SELECT last_generation FROM worker_generation_counters WHERE worker_id = :id")
			.get({ id: workerId }) as Record<string, unknown> | undefined;
		return row ? Number(row.last_generation) : 0;
	}

	/**
	 * Assert the consumer's per-call acquire params agree with the identity/incarnation/schema bound
	 * at open time (§4.2). Defense in depth for the {@link SupervisorControlDbOsfAdapter}: the durable
	 * generation advance must never run under a mismatched endpoint identity. Fails closed.
	 */
	assertBoundAcquireParams(endpointIdentity: EndpointIdentity, incarnationId: string, schemaDigest: string): void {
		this.assertOpen();
		if (!endpointIdentityEquals(this.endpointIdentity, endpointIdentity)) {
			failClosed(
				"control_db_endpoint_identity_mismatch",
				"acquire endpoint identity does not match the open handle",
			);
		}
		if (this.incarnationId !== incarnationId) {
			failClosed("control_db_incarnation_mismatch", "acquire incarnation does not match the open handle");
		}
		if (this.schemaDigest !== schemaDigest) {
			failClosed("control_db_schema_digest_mismatch", "acquire schema digest does not match the open handle");
		}
	}

	/**
	 * Assert the consumer's adopted generation equals this handle's adopted generation before a
	 * Form-1 self-fenced mutation (§5.3). The DB re-fences durably inside the transaction regardless;
	 * this catches a caller/DB divergence early and fails closed.
	 */
	assertAdoptedGeneration(adoptedGeneration: number): void {
		const adopted = this.requireAdopted();
		if (adopted !== adoptedGeneration) {
			throw new SupervisorControlDbGenerationStaleError(
				adopted,
				adoptedGeneration,
				"adopted generation mismatch at seam",
			);
		}
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		try {
			this.db.close();
		} catch {
			// A close failure is not authority; the handle is unusable regardless.
		}
	}
}

// ---------------------------------------------------------------------------
// Fence-result vocabulary (§5.5). The closed shapes now live in the shared authority module
// ./osf-control-db.js; this module keeps only its typed constructor functions below and re-exports
// the shapes so existing importers of supervisor-control-db.js keep their names.
// ---------------------------------------------------------------------------

const FENCE_RESULT_PROTOCOL = OSFENCE_FENCE_RESULT_PROTOCOL;

export function fenceResultOk(input: {
	observedGeneration: number;
	adoptedGeneration: number;
	route: RouteTuple;
	authoritySignal: AuthoritySignal;
	capability?: ControlDbCapability;
}): FenceResultOk {
	if (input.observedGeneration !== input.adoptedGeneration) {
		failClosed("fence_result_ok_generation", "OK requires observedGeneration == adoptedGeneration");
	}
	return {
		protocol: FENCE_RESULT_PROTOCOL,
		code: "OK",
		zeroEffect: false,
		capability: input.capability ?? "available",
		resolvedAt: nowIso(),
		observedGeneration: input.observedGeneration,
		adoptedGeneration: input.adoptedGeneration,
		route: input.route,
		authoritySignal: input.authoritySignal,
	};
}

export function fenceResultGenerationStale(input: {
	observedGeneration: number;
	adoptedGeneration: number;
	capability?: ControlDbCapability;
}): FenceResultGenerationStale {
	if (!(input.observedGeneration > input.adoptedGeneration)) {
		failClosed("fence_result_stale_generation", "GENERATION_STALE requires observedGeneration > adoptedGeneration");
	}
	return {
		protocol: FENCE_RESULT_PROTOCOL,
		code: "GENERATION_STALE",
		zeroEffect: true,
		capability: input.capability ?? "available",
		resolvedAt: nowIso(),
		observedGeneration: input.observedGeneration,
		adoptedGeneration: input.adoptedGeneration,
	};
}

export function fenceResultRouteStale(input: {
	route: RouteTuple;
	currentRouteRevision: number;
	capability?: ControlDbCapability;
}): FenceResultRouteStale {
	if (input.route.routeRevision === input.currentRouteRevision) {
		failClosed("fence_result_route_revision", "ROUTE_STALE requires a route revision mismatch");
	}
	return {
		protocol: FENCE_RESULT_PROTOCOL,
		code: "ROUTE_STALE",
		zeroEffect: true,
		capability: input.capability ?? "available",
		resolvedAt: nowIso(),
		route: input.route,
		currentRouteRevision: input.currentRouteRevision,
	};
}

export function fenceResultOwnerMismatch(input: {
	requestedRoute: RouteTuple;
	resolvedWorkerId: string;
	resolvedWorkerGeneration: number;
	capability?: ControlDbCapability;
}): FenceResultOwnerMismatch {
	if (input.resolvedWorkerId === input.requestedRoute.workerId) {
		failClosed("fence_result_owner_worker", "OWNER_MISMATCH requires a worker mismatch");
	}
	return {
		protocol: FENCE_RESULT_PROTOCOL,
		code: "OWNER_MISMATCH",
		zeroEffect: true,
		capability: input.capability ?? "available",
		resolvedAt: nowIso(),
		requestedRoute: input.requestedRoute,
		resolvedWorkerId: input.resolvedWorkerId,
		resolvedWorkerGeneration: input.resolvedWorkerGeneration,
	};
}

export function fenceResultOwnerUnavailable(input: {
	route: RouteTuple;
	capability?: ControlDbCapability;
}): FenceResultOwnerUnavailable {
	return {
		protocol: FENCE_RESULT_PROTOCOL,
		code: "OWNER_UNAVAILABLE",
		zeroEffect: true,
		capability: input.capability ?? "available",
		resolvedAt: nowIso(),
		route: input.route,
		routeState: "recovering",
	};
}

export function fenceResultAdmissionUnknown(input: {
	reason: AdmissionUnknownReason;
	evidenceDigest: string;
	capability?: ControlDbCapability;
}): FenceResultAdmissionUnknown {
	return {
		protocol: FENCE_RESULT_PROTOCOL,
		code: "ADMISSION_UNKNOWN",
		zeroEffect: true,
		capability: input.capability ?? "available",
		resolvedAt: nowIso(),
		reason: input.reason,
		evidenceDigest: assertDigest(input.evidenceDigest, "evidenceDigest"),
	};
}

// ---------------------------------------------------------------------------
// OsfControlDb conformance adapter (§9). Exposes SupervisorControlDb as the shared seam the
// daemon-osfence consumer injects, mapping the consumer's per-call params to the open-time-bound
// identity and the DB's richer rows to the lean shared shapes. It adds no ownership authority.
// ---------------------------------------------------------------------------

export class SupervisorControlDbOsfAdapter implements OsfControlDb {
	constructor(private readonly db: SupervisorControlDb) {}

	/** The wrapped durable Control DB, for direct Form-2 writer-fence assertions (§3.3). */
	get controlDb(): SupervisorControlDb {
		return this.db;
	}

	acquire(params: OsfControlDbAcquireParams): AcquisitionRecord {
		// The endpoint identity, incarnation, and schema digest are bound at open time; assert the
		// consumer's params agree before the durable generation advance (fail closed on mismatch).
		this.db.assertBoundAcquireParams(params.endpointIdentity, params.incarnationId, params.schemaDigest);
		return params.revocationProof === null
			? this.db.acquire()
			: this.db.acquire({ takeover: { revocationProof: params.revocationProof } });
	}

	readGenerationUnchecked(): number {
		return this.db.readGenerationUnchecked();
	}

	reserveWorkerGeneration(adoptedGeneration: number, workerId: string): number {
		this.db.assertAdoptedGeneration(adoptedGeneration);
		return this.db.reserveWorkerGeneration(workerId);
	}

	writeRoute(adoptedGeneration: number, route: RouteTuple, state: RouteState, descriptorDigest: string): void {
		this.db.assertAdoptedGeneration(adoptedGeneration);
		this.db.writeRoute({
			rootSessionId: route.rootSessionId,
			directParentSessionId: route.directParentSessionId,
			workerId: route.workerId,
			workerGeneration: route.workerGeneration,
			state,
			descriptorDigest,
		});
	}

	readRoute(rootSessionId: string, directParentSessionId: string): OsfWorkerRouteRow | undefined {
		return this.db.readWorkerRoute(rootSessionId, directParentSessionId) ?? undefined;
	}

	release(): void {
		this.db.close();
	}
}
