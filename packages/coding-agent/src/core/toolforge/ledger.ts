import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { getLogger } from "@earendil-works/pi-ai";
import { getAgentDir } from "../../config.js";
import { sha256 } from "../ravo/canonical-json.js";

/**
 * Durable record of every toolforge publish attempt, kept next to the skills it
 * creates (`<agentDir>/toolforge/ledger.json`).
 *
 * It exists because the double-run gate is the only place in the system where
 * "did this work" has a machine-checkable answer, and that answer has to
 * outlive the session that produced it: a rejected publish must stay rejected
 * with its reason, and a published package must be re-findable by the next
 * process (the refinement screen reads `toolforgeSrcRoots()` so a skill edit
 * naming a freshly published module is not screened out before the editable
 * install lands).
 *
 * Writes are read-modify-write with temp + rename, mirroring `saveHarnessState`.
 */

const log = getLogger("coding-agent.toolforge");

export const TOOLFORGE_LEDGER_SCHEMA = 1;
/** Oldest records are dropped past this; the ledger is a record, not an archive. */
export const MAX_TOOLFORGE_RECORDS = 500;

export type ToolforgeGatePhase = "negative" | "positive";
export type ToolforgePublishStatus = "published" | "rejected";

export interface ToolforgeGateRun {
	phase: ToolforgeGatePhase;
	/** `raised` / `clean` / `unrunnable`, mirrored from the replay outcome. */
	outcome: string;
	detail: string;
	durationMs: number;
	/** Whether this run met what the phase requires (negative: raised, positive: clean). */
	ok: boolean;
}

export interface ToolforgeRecord {
	name: string;
	importName: string;
	packagePath: string;
	sourceSha: string;
	exitTestSha: string;
	status: ToolforgePublishStatus;
	/** Set when `status` is `rejected`: why the gate or validation refused it. */
	reason?: string;
	gate: ToolforgeGateRun[];
	/** Whether the editable install into the kernel venv succeeded at publish time. */
	installed: boolean;
	sessionId?: string;
	at: string;
	/** 1 for the first accepted publish of this name, incrementing on each republish. */
	version: number;
}

export interface ToolforgeLedger {
	schema: number;
	records: ToolforgeRecord[];
}

export interface PublishedToolforgePackage {
	name: string;
	importName: string;
	packagePath: string;
	srcPath: string;
	version: number;
}

export function emptyToolforgeLedger(): ToolforgeLedger {
	return { schema: TOOLFORGE_LEDGER_SCHEMA, records: [] };
}

export function toolforgeDir(agentDir: string = getAgentDir()): string {
	return path.join(agentDir, "toolforge");
}

export function toolforgeLedgerPath(agentDir: string = getAgentDir()): string {
	return path.join(toolforgeDir(agentDir), "ledger.json");
}

/** Source directory of a published package, the root its import name resolves under. */
export function toolforgeSrcPath(packagePath: string): string {
	return path.join(packagePath, "src");
}

export function toolforgeContentSha(content: string): string {
	return sha256(content).slice(0, 16);
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function normalizeGateRun(value: unknown): ToolforgeGateRun | undefined {
	if (!isRecord(value)) return undefined;
	if (value.phase !== "negative" && value.phase !== "positive") return undefined;
	if (typeof value.outcome !== "string") return undefined;
	return {
		phase: value.phase,
		outcome: value.outcome,
		detail: typeof value.detail === "string" ? value.detail : "",
		durationMs: typeof value.durationMs === "number" && Number.isFinite(value.durationMs) ? value.durationMs : 0,
		ok: value.ok === true,
	};
}

function normalizeRecord(value: unknown): ToolforgeRecord | undefined {
	if (!isRecord(value)) return undefined;
	const { name, importName, packagePath, status } = value;
	if (typeof name !== "string" || !name) return undefined;
	if (typeof importName !== "string" || !importName) return undefined;
	if (typeof packagePath !== "string" || !packagePath) return undefined;
	if (status !== "published" && status !== "rejected") return undefined;
	const gate = Array.isArray(value.gate)
		? value.gate.flatMap((entry) => {
				const run = normalizeGateRun(entry);
				return run ? [run] : [];
			})
		: [];
	return {
		name,
		importName,
		packagePath,
		sourceSha: typeof value.sourceSha === "string" ? value.sourceSha : "",
		exitTestSha: typeof value.exitTestSha === "string" ? value.exitTestSha : "",
		status,
		...(typeof value.reason === "string" && value.reason ? { reason: value.reason } : {}),
		gate,
		installed: value.installed === true,
		...(typeof value.sessionId === "string" && value.sessionId ? { sessionId: value.sessionId } : {}),
		at: typeof value.at === "string" ? value.at : new Date(0).toISOString(),
		version:
			typeof value.version === "number" && Number.isInteger(value.version) && value.version > 0 ? value.version : 1,
	};
}

export function normalizeToolforgeLedger(value: unknown): ToolforgeLedger {
	if (!isRecord(value) || !Array.isArray(value.records)) return emptyToolforgeLedger();
	const records = value.records.flatMap((entry) => {
		const record = normalizeRecord(entry);
		return record ? [record] : [];
	});
	return { schema: TOOLFORGE_LEDGER_SCHEMA, records };
}

/**
 * Read the ledger. A missing or unparseable file yields an empty ledger and a
 * warning, never a throw: publishing must not be blocked by a corrupt record of
 * past publishes.
 */
export function loadToolforgeLedger(ledgerPath: string = toolforgeLedgerPath()): ToolforgeLedger {
	let raw: string;
	try {
		raw = readFileSync(ledgerPath, "utf-8");
	} catch {
		return emptyToolforgeLedger();
	}
	try {
		return normalizeToolforgeLedger(JSON.parse(raw));
	} catch (error) {
		log.warn("toolforge.ledger.corrupt", {
			path: ledgerPath,
			bytes: raw.length,
			error: error instanceof Error ? error.message : String(error),
		});
		return emptyToolforgeLedger();
	}
}

export function saveToolforgeLedger(ledger: ToolforgeLedger, ledgerPath: string = toolforgeLedgerPath()): void {
	mkdirSync(path.dirname(ledgerPath), { recursive: true });
	const temp = `${ledgerPath}.${process.pid}.${randomUUID()}.tmp`;
	try {
		writeFileSync(temp, `${JSON.stringify({ ...ledger, schema: TOOLFORGE_LEDGER_SCHEMA }, null, 2)}\n`, "utf-8");
		renameSync(temp, ledgerPath);
	} catch (error) {
		rmSync(temp, { force: true });
		throw error;
	}
}

/** The most recent record for a name, published or rejected. */
export function latestToolforgeRecord(ledger: ToolforgeLedger, name: string): ToolforgeRecord | undefined {
	for (let index = ledger.records.length - 1; index >= 0; index--) {
		if (ledger.records[index].name === name) return ledger.records[index];
	}
	return undefined;
}

/** Next version number for a name: one past the highest accepted publish of it. */
export function nextToolforgeVersion(ledger: ToolforgeLedger, name: string): number {
	let highest = 0;
	for (const record of ledger.records) {
		if (record.name === name && record.status === "published" && record.version > highest) {
			highest = record.version;
		}
	}
	return highest + 1;
}

export function appendToolforgeRecord(
	record: ToolforgeRecord,
	ledgerPath: string = toolforgeLedgerPath(),
): ToolforgeLedger {
	const ledger = loadToolforgeLedger(ledgerPath);
	ledger.records.push(record);
	if (ledger.records.length > MAX_TOOLFORGE_RECORDS) {
		ledger.records.splice(0, ledger.records.length - MAX_TOOLFORGE_RECORDS);
	}
	saveToolforgeLedger(ledger, ledgerPath);
	return ledger;
}

/**
 * Packages this machine has published and still has on disk, newest accepted
 * record per name. A name whose directory was deleted by hand is dropped rather
 * than reported, so callers never put a dead root on `sys.path`.
 */
export function publishedToolforgePackages(agentDir: string = getAgentDir()): PublishedToolforgePackage[] {
	const ledger = loadToolforgeLedger(toolforgeLedgerPath(agentDir));
	const byName = new Map<string, PublishedToolforgePackage>();
	for (const record of ledger.records) {
		if (record.status !== "published") continue;
		const srcPath = toolforgeSrcPath(record.packagePath);
		if (!existsSync(srcPath)) {
			byName.delete(record.name);
			continue;
		}
		byName.set(record.name, {
			name: record.name,
			importName: record.importName,
			packagePath: record.packagePath,
			srcPath,
			version: record.version,
		});
	}
	return [...byName.values()];
}

/**
 * `sys.path` roots for every published package. The refinement fast screen
 * (`skill-dry-run.ts`) prepends these so a skill edit naming a module toolforge
 * just created is screened against the real package instead of failing the
 * import probe for the window before the editable install is visible.
 */
export function toolforgeSrcRoots(agentDir: string = getAgentDir()): string[] {
	return publishedToolforgePackages(agentDir).map((pkg) => pkg.srcPath);
}
