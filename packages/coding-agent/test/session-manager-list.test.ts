import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

const scan = vi.hoisted(() => ({
	active: 0,
	maxActive: 0,
	delays: new Map<string, number>(),
}));

vi.mock("../src/utils/file-lines.js", async (importOriginal) => {
	const actual = await importOriginal<typeof import("../src/utils/file-lines.js")>();
	return {
		...actual,
		async *readLinesAsBuffers(filePath: string): AsyncGenerator<Buffer> {
			scan.active++;
			scan.maxActive = Math.max(scan.maxActive, scan.active);
			try {
				await new Promise((resolve) => setTimeout(resolve, scan.delays.get(filePath) ?? 0));
				for (const line of readFileSync(filePath, "utf8").trimEnd().split("\n")) {
					yield Buffer.from(line);
				}
			} finally {
				scan.active--;
			}
		},
	};
});

import { SessionManager } from "../src/core/session-manager.js";

const tempDirs: string[] = [];

afterEach(() => {
	scan.active = 0;
	scan.maxActive = 0;
	scan.delays.clear();
	while (tempDirs.length > 0) {
		rmSync(tempDirs.pop()!, { recursive: true, force: true });
	}
});

function createSession(dir: string, index: number): string {
	const id = `session-${index.toString().padStart(2, "0")}`;
	const file = join(dir, `${id}.jsonl`);
	writeFileSync(
		file,
		`${JSON.stringify({
			type: "session",
			version: 3,
			id,
			timestamp: new Date(index * 1000).toISOString(),
			cwd: "/tmp/project",
		})}\n`,
	);
	return file;
}

describe("SessionManager saved-session listing", () => {
	it("bounds concurrent JSONL scans and preserves callback order", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-list-"));
		tempDirs.push(dir);
		const files = Array.from({ length: 12 }, (_, index) => createSession(dir, index));
		for (const [index, file] of files.entries()) {
			// Finish scans in reverse order inside each batch. Callback order must
			// still follow the stable directory order rather than completion order.
			scan.delays.set(file, (files.length - index) * 2);
		}
		const discovered: string[] = [];
		const progress: Array<[number, number]> = [];

		await SessionManager.listAll(
			{
				onSession: (session) => discovered.push(session.id),
				onProgress: (loaded, total) => progress.push([loaded, total]),
			},
			dir,
		);

		expect(scan.maxActive).toBeGreaterThan(1);
		expect(scan.maxActive).toBeLessThanOrEqual(8);
		expect(discovered).toEqual(files.map((file) => file.slice(file.lastIndexOf("/") + 1, -".jsonl".length)));
		expect(progress).toEqual(files.map((_, index) => [index + 1, files.length]));
	});
});
