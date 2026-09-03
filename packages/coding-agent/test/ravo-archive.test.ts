import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
	RavoArchive,
	RavoCorruptionError,
	RavoStaleCommitError,
	RavoTruncatedTailError,
} from "../src/core/ravo/archive.js";
import { sha256 } from "../src/core/ravo/canonical-json.js";
import type { RavoFaultPoint } from "../src/core/ravo/types.js";

const roots: string[] = [];
async function root(): Promise<string> {
	const value = await mkdtemp(path.join(tmpdir(), "ravo-"));
	roots.push(value);
	return value;
}
afterEach(async () => Promise.all(roots.splice(0).map((value) => rm(value, { recursive: true, force: true }))));

describe("RavoArchive", () => {
	it("writes canonical chained events and rebuilds state idempotently", async () => {
		const archive = new RavoArchive({ artifactRoot: await root(), now: () => new Date(0) });
		await archive.initialize();
		const first = await archive.append("run", { z: 1, a: "x" });
		const second = await archive.append("reject", { reason: "weak" });
		expect(first.seq).toBe(1);
		expect(second.prevDigest).toBe(first.digest);
		const before = await readFile(archive.eventsPath, "utf8");
		const state = await archive.recover();
		expect(await archive.recover()).toEqual(state);
		expect(await readFile(archive.eventsPath, "utf8")).toBe(before);
		expect(state).toMatchObject({ eventCount: 2, revision: 0, championDigest: null });
	});

	it("stores content-addressed blobs idempotently and detects tampering", async () => {
		const archive = new RavoArchive({ artifactRoot: await root() });
		await archive.initialize();
		const content = Buffer.from("proposal");
		const digest = await archive.putBlob(content);
		expect(digest).toBe(sha256(content));
		expect(await archive.putBlob(content)).toBe(digest);
		await writeFile(path.join(archive.blobsDirectory, digest), "tampered");
		await expect(archive.getBlob(digest)).rejects.toBeInstanceOf(RavoCorruptionError);
	});

	it("serializes instances and rejects a stale champion CAS without appending", async () => {
		const artifactRoot = await root();
		const left = new RavoArchive({ artifactRoot });
		const right = new RavoArchive({ artifactRoot });
		await left.initialize();
		const base = { revision: 0, championDigest: null };
		const one = "1".repeat(64);
		const two = "2".repeat(64);
		const results = await Promise.allSettled([
			left.accept({ proposal: "a" }, base, one),
			right.accept({ proposal: "b" }, base, two),
		]);
		expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(1);
		const failure = results.find((result) => result.status === "rejected");
		expect(failure?.status === "rejected" && failure.reason).toBeInstanceOf(RavoStaleCommitError);
		const state = await left.recover();
		expect(state.eventCount).toBe(1);
		expect(state.revision).toBe(1);
	});

	it("reject never changes the champion", async () => {
		const archive = new RavoArchive({ artifactRoot: await root() });
		await archive.initialize();
		const champion = "a".repeat(64);
		await archive.accept({}, { revision: 0, championDigest: null }, champion);
		await archive.append("reject", { proposal: "bad" });
		expect(await archive.recover()).toMatchObject({ revision: 1, championDigest: champion });
	});

	it("fails closed on tampering before the tail", async () => {
		const archive = new RavoArchive({ artifactRoot: await root() });
		await archive.initialize();
		await archive.append("run", {});
		await archive.append("stop", {});
		const log = await readFile(archive.eventsPath, "utf8");
		await writeFile(archive.eventsPath, log.replace('"type":"run"', '"type":"proposal"'));
		await expect(archive.recover({ repairTruncatedTail: true })).rejects.toBeInstanceOf(RavoCorruptionError);
	});

	it("requires explicit truncated-tail repair", async () => {
		const archive = new RavoArchive({ artifactRoot: await root() });
		await archive.initialize();
		await archive.append("run", {});
		await writeFile(archive.eventsPath, `${await readFile(archive.eventsPath, "utf8")}{"seq":2`);
		await expect(archive.recover()).rejects.toBeInstanceOf(RavoTruncatedTailError);
		const recovered = await archive.recover({ repairTruncatedTail: true });
		expect(recovered.eventCount).toBe(1);
		expect((await readFile(archive.eventsPath, "utf8")).endsWith("\n")).toBe(true);
	});

	it("recovers committed log records across injected crash points", async () => {
		for (const point of [
			"after-log-write",
			"after-log-fsync",
			"after-state-temp-write",
			"after-state-temp-fsync",
			"after-state-rename",
		] as RavoFaultPoint[]) {
			const artifactRoot = await root();
			let armed = false;
			let occurrences = 0;
			const archive = new RavoArchive({
				artifactRoot,
				fault: (seen) => {
					if (armed && seen === point) {
						occurrences += 1;
						const statePoint = point.startsWith("after-state-");
						if (!statePoint || occurrences === 2) throw new Error("crash");
					}
				},
			});
			await archive.initialize();
			armed = true;
			await expect(archive.append("pressure", { point })).rejects.toThrow("crash");
			const recovered = await new RavoArchive({ artifactRoot }).recover();
			expect(recovered.eventCount).toBe(1);
		}
	});

	it("rejects likely secrets before writing an event", async () => {
		const archive = new RavoArchive({ artifactRoot: await root() });
		await archive.initialize();
		await expect(archive.append("run", { nested: { api_token: "do-not-store" } })).rejects.toThrow("Sensitive");
		expect((await archive.recover()).eventCount).toBe(0);
	});

	it("rejects archive paths outside the injected root", async () => {
		const artifactRoot = await root();
		expect(() => new RavoArchive({ artifactRoot, archivePath: "../escape" })).toThrow("escapes");
		expect(() => new RavoArchive({ artifactRoot, archivePath: path.resolve(artifactRoot, "absolute") })).toThrow(
			"relative",
		);
	});
});
