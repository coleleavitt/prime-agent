import { randomUUID } from "node:crypto";
import { constants } from "node:fs";
import { appendFile, mkdir, open, readFile, realpath, rename, rm, truncate } from "node:fs/promises";
import path from "node:path";
import lockfile from "proper-lockfile";
import { canonicalJson, sha256 } from "./canonical-json.js";
import {
	type ChampionCas,
	type JsonValue,
	RAVO_EVENT_TYPES,
	type RavoEvent,
	type RavoEventType,
	type RavoFaultPoint,
	type RavoState,
} from "./types.js";

const ZERO_STATE: RavoState = { revision: 0, championDigest: null, eventCount: 0, headDigest: null };
const eventTypes = new Set<RavoEventType>(RAVO_EVENT_TYPES);

export class RavoCorruptionError extends Error {}
export class RavoTruncatedTailError extends Error {}
export class RavoStaleCommitError extends Error {}

export interface RavoArchiveOptions {
	artifactRoot: string;
	archivePath?: string;
	now?: () => Date;
	fault?: (point: RavoFaultPoint) => void | Promise<void>;
}

export interface RecoverOptions {
	repairTruncatedTail?: boolean;
}

export class RavoArchive {
	readonly directory: string;
	readonly eventsPath: string;
	readonly blobsDirectory: string;
	readonly statePath: string;
	private readonly artifactRoot: string;
	private readonly now: () => Date;
	private readonly fault?: (point: RavoFaultPoint) => void | Promise<void>;
	private static readonly queues = new Map<string, Promise<void>>();

	constructor(options: RavoArchiveOptions) {
		const root = path.resolve(options.artifactRoot);
		const relative = options.archivePath ?? "ravo";
		if (path.isAbsolute(relative)) throw new Error("archivePath must be relative to artifactRoot");
		const directory = path.resolve(root, relative);
		if (directory !== root && !directory.startsWith(`${root}${path.sep}`)) {
			throw new Error("archivePath escapes artifactRoot");
		}
		this.artifactRoot = root;
		this.directory = directory;
		this.eventsPath = path.join(directory, "events.jsonl");
		this.blobsDirectory = path.join(directory, "blobs");
		this.statePath = path.join(directory, "state.json");
		this.now = options.now ?? (() => new Date());
		this.fault = options.fault;
	}

	async initialize(): Promise<RavoState> {
		await this.ensureContainedDirectory();
		return this.serialized(() =>
			this.withFilesystemLock(async () => {
				await mkdir(this.blobsDirectory, { recursive: true });
				const handle = await open(this.eventsPath, "a");
				await handle.close();
				return this.recoverUnlocked();
			}),
		);
	}

	async recover(options: RecoverOptions = {}): Promise<RavoState> {
		await this.ensureContainedDirectory();
		return this.serialized(() => this.withFilesystemLock(() => this.recoverUnlocked(options)));
	}

	private async recoverUnlocked(options: RecoverOptions = {}): Promise<RavoState> {
		await mkdir(this.blobsDirectory, { recursive: true });
		let bytes: Buffer;
		try {
			bytes = await readFile(this.eventsPath);
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
			bytes = Buffer.alloc(0);
			await appendFile(this.eventsPath, bytes);
		}
		if (bytes.length > 0 && bytes.at(-1) !== 0x0a) {
			if (!options.repairTruncatedTail) {
				throw new RavoTruncatedTailError("events.jsonl has an incomplete final record");
			}
			const newline = bytes.lastIndexOf(0x0a);
			const validLength = newline < 0 ? 0 : newline + 1;
			await truncate(this.eventsPath, validLength);
			bytes = bytes.subarray(0, validLength);
			const handle = await open(this.eventsPath, "r+");
			await handle.sync();
			await handle.close();
		}
		let state = { ...ZERO_STATE };
		const lines = bytes.toString("utf8").split("\n");
		lines.pop();
		for (const [index, line] of lines.entries()) {
			let event: RavoEvent;
			try {
				event = JSON.parse(line) as RavoEvent;
			} catch {
				throw new RavoCorruptionError(`Invalid JSON at event ${index + 1}`);
			}
			this.validateEvent(event, state);
			state = applyEvent(state, event);
		}
		await this.writeState(state);
		return state;
	}

	async append(type: Exclude<RavoEventType, "accept">, payload: { [key: string]: JsonValue }): Promise<RavoEvent> {
		assertNoSecrets(payload);
		await this.ensureContainedDirectory();
		return this.serialized(() =>
			this.withFilesystemLock(async () => {
				const state = await this.recoverUnlocked();
				return this.appendUnderLock(type, payload, state);
			}),
		);
	}

	async accept(
		payload: { [key: string]: JsonValue },
		expected: ChampionCas,
		championDigest: string,
	): Promise<RavoEvent> {
		if (!/^[a-f0-9]{64}$/.test(championDigest)) throw new Error("championDigest must be a SHA-256 digest");
		assertNoSecrets(payload);
		await this.ensureContainedDirectory();
		return this.serialized(() =>
			this.withFilesystemLock(async () => {
				const state = await this.recoverUnlocked();
				if (state.revision !== expected.revision || state.championDigest !== expected.championDigest) {
					throw new RavoStaleCommitError("Champion changed since evaluation");
				}
				return this.appendUnderLock("accept", { ...payload, championDigest }, state);
			}),
		);
	}

	async putBlob(content: Uint8Array): Promise<string> {
		await this.ensureContainedDirectory();
		const digest = sha256(content);
		await mkdir(this.blobsDirectory, { recursive: true });
		const target = path.join(this.blobsDirectory, digest);
		try {
			const existing = await readFile(target);
			if (sha256(existing) !== digest) throw new RavoCorruptionError(`Blob ${digest} is corrupt`);
			return digest;
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
		}
		const temporary = path.join(this.blobsDirectory, `.${digest}.${process.pid}.${randomUUID()}.tmp`);
		const handle = await open(temporary, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600);
		try {
			await handle.writeFile(content);
			await this.hit("after-blob-temp-write");
			await handle.sync();
			await this.hit("after-blob-temp-fsync");
		} finally {
			await handle.close();
		}
		try {
			await rename(temporary, target);
			await this.hit("after-blob-rename");
			await syncDirectory(this.blobsDirectory);
		} finally {
			await rm(temporary, { force: true });
		}
		return digest;
	}

	async getBlob(digest: string): Promise<Buffer> {
		await this.ensureContainedDirectory();
		if (!/^[a-f0-9]{64}$/.test(digest)) throw new Error("Invalid blob digest");
		const content = await readFile(path.join(this.blobsDirectory, digest));
		if (sha256(content) !== digest) throw new RavoCorruptionError(`Blob ${digest} is corrupt`);
		return content;
	}

	private async ensureContainedDirectory(): Promise<void> {
		const root = this.artifactRoot;
		await mkdir(root, { recursive: true });
		await mkdir(this.directory, { recursive: true });
		const [realRoot, realDirectory] = await Promise.all([realpath(root), realpath(this.directory)]);
		if (realDirectory !== realRoot && !realDirectory.startsWith(`${realRoot}${path.sep}`)) {
			throw new Error("archivePath resolves outside artifactRoot");
		}
	}

	private async appendUnderLock(
		type: RavoEventType,
		payload: { [key: string]: JsonValue },
		state: RavoState,
	): Promise<RavoEvent> {
		const unsigned = {
			seq: state.eventCount + 1,
			type,
			timestamp: this.now().toISOString(),
			payload,
			prevDigest: state.headDigest,
		};
		const digest = sha256(canonicalJson(unsigned));
		const event: RavoEvent = { ...unsigned, digest };
		const handle = await open(this.eventsPath, "a", 0o600);
		try {
			await handle.writeFile(`${canonicalJson(event)}\n`);
			await this.hit("after-log-write");
			await handle.sync();
			await this.hit("after-log-fsync");
		} finally {
			await handle.close();
		}
		await this.writeState(applyEvent(state, event));
		return event;
	}

	private validateEvent(event: RavoEvent, state: RavoState): void {
		if (!event || typeof event !== "object" || !eventTypes.has(event.type)) {
			throw new RavoCorruptionError("Unknown or malformed event");
		}
		if (event.seq !== state.eventCount + 1 || event.prevDigest !== state.headDigest) {
			throw new RavoCorruptionError(`Broken hash chain at sequence ${event.seq}`);
		}
		const { digest, ...unsigned } = event;
		if (digest !== sha256(canonicalJson(unsigned))) {
			throw new RavoCorruptionError(`Digest mismatch at sequence ${event.seq}`);
		}
		if (event.type === "accept" && !/^[a-f0-9]{64}$/.test(String(event.payload.championDigest))) {
			throw new RavoCorruptionError(`Invalid champion at sequence ${event.seq}`);
		}
	}

	private async writeState(state: RavoState): Promise<void> {
		const temporary = `${this.statePath}.${process.pid}.${randomUUID()}.tmp`;
		const handle = await open(temporary, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600);
		try {
			await handle.writeFile(`${canonicalJson(state)}\n`);
			await this.hit("after-state-temp-write");
			await handle.sync();
			await this.hit("after-state-temp-fsync");
		} finally {
			await handle.close();
		}
		try {
			await rename(temporary, this.statePath);
			await this.hit("after-state-rename");
			await syncDirectory(this.directory);
		} finally {
			await rm(temporary, { force: true });
		}
	}

	private async withFilesystemLock<T>(operation: () => Promise<T>): Promise<T> {
		const release = await lockfile.lock(this.directory, {
			lockfilePath: path.join(this.directory, ".archive.lock"),
			realpath: false,
			stale: 30_000,
			update: 10_000,
			retries: { retries: 100, factor: 1.2, minTimeout: 10, maxTimeout: 250 },
		});
		try {
			return await operation();
		} finally {
			await release();
		}
	}

	private serialized<T>(operation: () => Promise<T>): Promise<T> {
		const previous = RavoArchive.queues.get(this.directory) ?? Promise.resolve();
		const result = previous.then(operation, operation);
		const settled = result.then(
			() => undefined,
			() => undefined,
		);
		RavoArchive.queues.set(this.directory, settled);
		void settled.finally(() => {
			if (RavoArchive.queues.get(this.directory) === settled) RavoArchive.queues.delete(this.directory);
		});
		return result;
	}

	private async hit(point: RavoFaultPoint): Promise<void> {
		await this.fault?.(point);
	}
}

function applyEvent(state: RavoState, event: RavoEvent): RavoState {
	return {
		revision: state.revision + (event.type === "accept" ? 1 : 0),
		championDigest: event.type === "accept" ? String(event.payload.championDigest) : state.championDigest,
		eventCount: event.seq,
		headDigest: event.digest,
	};
}

async function syncDirectory(directory: string): Promise<void> {
	const handle = await open(directory, "r");
	try {
		await handle.sync();
	} finally {
		await handle.close();
	}
}

const SENSITIVE_KEY =
	/(?:^|_)(?:api[_-]?key|authorization|cookie|credential|password|private[_-]?key|secret|token)(?:$|_)/i;

function assertNoSecrets(value: JsonValue, pathParts: string[] = []): void {
	if (value === null || typeof value !== "object") return;
	if (Array.isArray(value)) {
		for (const [index, item] of value.entries()) assertNoSecrets(item, [...pathParts, String(index)]);
		return;
	}
	for (const [key, item] of Object.entries(value)) {
		if (SENSITIVE_KEY.test(key))
			throw new Error(`Sensitive field is not allowed in RAVO archive: ${[...pathParts, key].join(".")}`);
		assertNoSecrets(item, [...pathParts, key]);
	}
}
