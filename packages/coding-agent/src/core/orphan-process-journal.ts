import { spawnSync } from "node:child_process";
import { closeSync, fsyncSync, openSync, readFileSync, rmSync, writeSync } from "node:fs";
import { win32 } from "node:path";
import { getLogger } from "@earendil-works/pi-ai";
import { getProcessStartId } from "./session-lease.js";

export const ORPHAN_PROCESS_JOURNAL_ENV = "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL";

const orphanJournalLog = getLogger("orphan-process-journal");

interface OrphanProcessRecord {
	version: 1;
	pid: number;
	ownerPid: number;
	/** Set on records written by a kernel (e.g. bash() children) so the host can reap per kernel. */
	kernelPid?: number;
	processStartId?: string;
	active: boolean;
	recordedAt: string;
}

export interface ActiveOrphanProcess {
	pid: number;
	kernelPid?: number;
	/** Missing on identity-free records: old journals or host writes whose start-id query failed (kernels no longer write pid-only records). */
	processStartId?: string;
}

export function recordOrphanProcessState(pid: number, active: boolean): void {
	const path = process.env[ORPHAN_PROCESS_JOURNAL_ENV];
	if (!path || !Number.isInteger(pid) || pid <= 0) {
		return;
	}
	const processStartId = active ? getProcessStartId(pid) : undefined;
	const record: OrphanProcessRecord = {
		version: 1,
		pid,
		ownerPid: process.pid,
		...(processStartId ? { processStartId } : {}),
		active,
		recordedAt: new Date().toISOString(),
	};
	try {
		const descriptor = openSync(path, "a", 0o600);
		try {
			writeSync(descriptor, `${JSON.stringify(record)}\n`);
			fsyncSync(descriptor);
		} finally {
			closeSync(descriptor);
		}
	} catch (error) {
		// Process tracking must not make a successfully spawned command fail, but
		// losing the durable cleanup record must remain visible to operators.
		orphanJournalLog.error("failed to append orphan process journal", {
			path,
			pid,
			active,
			error: error instanceof Error ? error.message : String(error),
		});
	}
}

export class OrphanProcessJournalIntegrityError extends Error {
	readonly code = "orphan_process_journal_integrity" as const;

	constructor(
		readonly journalPath: string,
		readonly lineNumber: number,
		reason: string,
	) {
		super(`Invalid orphan process journal ${journalPath} at line ${lineNumber}: ${reason}`);
		this.name = "OrphanProcessJournalIntegrityError";
	}
}

export function readActiveOrphanProcesses(path: string, ownerPid: number): ActiveOrphanProcess[] {
	let contents: string;
	try {
		contents = readFileSync(path, "utf8");
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") {
			return [];
		}
		throw error;
	}
	const latest = new Map<number, OrphanProcessRecord>();
	const lines = contents.split("\n");
	for (const [index, line] of lines.entries()) {
		if (!line) continue;
		let record: Partial<OrphanProcessRecord>;
		try {
			record = JSON.parse(line) as Partial<OrphanProcessRecord>;
		} catch {
			throw new OrphanProcessJournalIntegrityError(path, index + 1, "malformed JSON");
		}
		if (
			record.version !== 1 ||
			!Number.isInteger(record.pid) ||
			(record.pid ?? 0) <= 0 ||
			!Number.isInteger(record.ownerPid) ||
			(record.ownerPid ?? 0) <= 0 ||
			typeof record.active !== "boolean" ||
			typeof record.recordedAt !== "string" ||
			(record.kernelPid !== undefined && (!Number.isInteger(record.kernelPid) || record.kernelPid <= 0)) ||
			(record.processStartId !== undefined && typeof record.processStartId !== "string")
		) {
			throw new OrphanProcessJournalIntegrityError(path, index + 1, "record failed schema validation");
		}
		if (record.ownerPid === ownerPid) latest.set(record.pid!, record as OrphanProcessRecord);
	}
	// Pid-only actives (no processStartId) still surface from old journals or
	// host writes whose start-id query failed; reapers decide per-platform.
	return [...latest.values()]
		.filter(
			(record) =>
				record.active && (record.processStartId === undefined || typeof record.processStartId === "string"),
		)
		.map((record) => ({
			pid: record.pid,
			...(Number.isInteger(record.kernelPid) ? { kernelPid: record.kernelPid } : {}),
			...(typeof record.processStartId === "string" ? { processStartId: record.processStartId } : {}),
		}));
}

export function isOrphanProcessIdentityCurrent(orphan: ActiveOrphanProcess): boolean {
	// Pid-only records can never claim identity (undefined === undefined must not match).
	return orphan.processStartId !== undefined && getProcessStartId(orphan.pid) === orphan.processStartId;
}

/**
 * Identity-free records cannot prove the pid still names the journaled process.
 * On win32 the kernel's kill-on-close job already reaped its tree when it died,
 * so a bare-pid taskkill only risks killing a reused pid. POSIX keeps the
 * best-effort kill (group-scoped, and the spawn gate makes pid-only actives
 * host-written rarities there).
 */
export function shouldReapOrphanProcess(orphan: ActiveOrphanProcess): boolean {
	if (orphan.processStartId === undefined) return false;
	return isOrphanProcessIdentityCurrent(orphan);
}

export function clearOrphanProcessJournal(path: string): void {
	rmSync(path, { force: true });
}

export type OrphanProcessReapOutcome =
	| { pid: number; outcome: "reaped" }
	| { pid: number; outcome: "skipped_identity_mismatch" }
	| { pid: number; outcome: "failed" };

export function reapOrphanProcesses(orphans: readonly ActiveOrphanProcess[]): OrphanProcessReapOutcome[] {
	return orphans.map((orphan) => {
		if (!shouldReapOrphanProcess(orphan)) {
			return { pid: orphan.pid, outcome: "skipped_identity_mismatch" };
		}
		return killOrphanProcess(orphan.pid)
			? { pid: orphan.pid, outcome: "reaped" }
			: { pid: orphan.pid, outcome: "failed" };
	});
}

// Kills still-active bash() children journaled by the given kernel pid; sibling kernels' records are untouched.
export function reapKernelOrphanProcesses(kernelPid: number): void {
	const path = process.env[ORPHAN_PROCESS_JOURNAL_ENV];
	if (!path || !Number.isInteger(kernelPid) || kernelPid <= 0) {
		return;
	}
	let orphans: ActiveOrphanProcess[];
	try {
		orphans = readActiveOrphanProcesses(path, process.pid);
	} catch (error) {
		orphanJournalLog.error("refusing kernel orphan reap because journal integrity could not be verified", {
			path,
			kernelPid,
			error: error instanceof Error ? error.message : String(error),
		});
		return;
	}
	const candidates = orphans.filter((orphan) => orphan.kernelPid === kernelPid && orphan.pid !== kernelPid);
	for (const result of reapOrphanProcesses(candidates)) {
		orphanJournalLog.info("kernel orphan reap outcome", {
			kernelPid,
			orphanPid: result.pid,
			outcome: result.outcome,
		});
		if (result.outcome === "reaped") recordOrphanProcessState(result.pid, false);
	}
}

// Hardened cross-platform tree kill for journaled orphans: absolute System32
// taskkill /T on win32 (a bare name could resolve a planted CWD taskkill.exe),
// process-group then pid SIGKILL elsewhere.
export function killOrphanProcess(pid: number): boolean {
	if (process.platform === "win32") {
		// In-kernel bash() kill paths use taskkill /T; the reaper must kill the same tree, not just the shell pid.
		const result = spawnSync(
			win32.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "taskkill.exe"),
			["/F", "/T", "/PID", String(pid)],
			{
				stdio: "ignore",
				timeout: 10_000,
				env: { ...process.env, NoDefaultCurrentDirectoryInExePath: "1" },
			},
		);
		return result.status === 0;
	}
	try {
		process.kill(-pid, "SIGKILL");
		return true;
	} catch {
		try {
			process.kill(pid, "SIGKILL");
			return true;
		} catch {
			// The orphan may already have exited.
		}
	}
	return false;
}
