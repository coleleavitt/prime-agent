import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { getProcessStartId, isProcessIdentityAlive } from "../src/core/session-lease.js";
import { parseOwner, tryAcquireDirLock } from "../src/utils/dir-lock.js";

let directory: string;
let lockPath: string;

beforeEach(() => {
	directory = mkdtempSync(join(tmpdir(), "dir-lock-"));
	lockPath = join(directory, "resource.lock");
});

afterEach(() => {
	rmSync(directory, { recursive: true, force: true });
});

/** The production judgement both lock callers use. */
const judgeByIdentity = (ownerPid: number | undefined, ownerStartId: string | undefined) =>
	ownerPid !== undefined && isProcessIdentityAlive(ownerPid, ownerStartId);

describe("dir-lock owner content", () => {
	it("records the pid and the start identity on separate lines", async () => {
		await expect(tryAcquireDirLock(lockPath, judgeByIdentity, { ownerStartId: "proc:12345" })).resolves.toBe(
			"acquired",
		);
		expect(readFileSync(lockPath, "utf8")).toBe(`${process.pid}\nproc:12345\n`);
		expect(parseOwner(readFileSync(lockPath, "utf8"))).toEqual({ pid: process.pid, startId: "proc:12345" });
	});

	// The macOS/BSD start identity is `ps` lstart output. A space-delimited format would have
	// split it, and the old /^\d+$/ pid parser would have read every new lock as ownerless.
	it("keeps a start identity that contains spaces intact", async () => {
		const lstart = "ps:Wed Sep 16 04:10:01 2026";
		await tryAcquireDirLock(lockPath, judgeByIdentity, { ownerStartId: lstart });
		expect(parseOwner(readFileSync(lockPath, "utf8"))).toEqual({ pid: process.pid, startId: lstart });
	});

	it("still reads a pid-only lock written before start identities existed", () => {
		expect(parseOwner(`${process.pid}\n`)).toEqual({ pid: process.pid, startId: undefined });
		expect(parseOwner(`${process.pid}`)).toEqual({ pid: process.pid, startId: undefined });
	});

	it("omits a start identity that would corrupt the line format", async () => {
		await tryAcquireDirLock(lockPath, judgeByIdentity, { ownerStartId: "bad\ninjected" });
		expect(readFileSync(lockPath, "utf8")).toBe(`${process.pid}\n`);
	});

	it("treats non-numeric pid content as ownerless", () => {
		expect(parseOwner("not-a-pid\nproc:1\n").pid).toBeUndefined();
		expect(parseOwner(undefined)).toEqual({ pid: undefined, startId: undefined });
	});
});

describe("dir-lock pid reuse", () => {
	// This process is alive, so a pid-only check always calls the lock held. Recording a start
	// identity that is not this process stands in for the OS having recycled a dead owner's pid.
	it("reclaims a lock whose live pid now belongs to a different process", async () => {
		writeFileSync(lockPath, `${process.pid}\nproc:not-the-process-that-took-this-lock\n`);
		await expect(tryAcquireDirLock(lockPath, judgeByIdentity)).resolves.toBe("reclaimed");
		expect(existsSync(lockPath)).toBe(false);
	});

	it("keeps holding a lock whose pid and start identity still match", async () => {
		const startId = getProcessStartId(process.pid);
		if (startId === undefined) return; // No readable identity on this platform: nothing to compare.
		writeFileSync(lockPath, `${process.pid}\n${startId}\n`);
		await expect(tryAcquireDirLock(lockPath, judgeByIdentity)).resolves.toBe("held");
		expect(existsSync(lockPath)).toBe(true);
	});

	it("falls back to the pid when a lock recorded no start identity", async () => {
		writeFileSync(lockPath, `${process.pid}\n`);
		await expect(tryAcquireDirLock(lockPath, judgeByIdentity)).resolves.toBe("held");
	});

	it("still reads the owner from a legacy directory lock", async () => {
		mkdirSync(lockPath);
		writeFileSync(join(lockPath, "pid"), `${process.pid}\nproc:not-the-process-that-took-this-lock\n`);
		await expect(tryAcquireDirLock(lockPath, judgeByIdentity)).resolves.toBe("reclaimed");
	});
});
