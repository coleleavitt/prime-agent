import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { getLearningDir, getTrajectoryIndexPath } from "../src/config.js";
import {
	boundStoreFile,
	DEFAULT_MAX_TRAJECTORY_WINDOWS,
	readTrajectoryIndex,
	TRAJECTORY_STORE_VERSION,
	type TrajectoryStoreFile,
	type TrajectoryWindow,
	writeTrajectoryIndex,
} from "../src/core/distill/trajectory-index.js";

let agentDir = "";

function primeWindow(key: string): TrajectoryWindow {
	return {
		schema: TRAJECTORY_STORE_VERSION,
		window: key,
		sealedAt: "",
		days: [],
		turns: 0,
		corpus: "prime",
		fingerprints: [],
	};
}

function fileWith(windows: TrajectoryWindow[]): TrajectoryStoreFile {
	return {
		version: TRAJECTORY_STORE_VERSION,
		sealedAt: new Date(0).toISOString(),
		windowsObserved: windows.filter((w) => w.corpus === "prime").length,
		minWindows: 4,
		windows,
		labels: [
			{
				fingerprint: "fp1",
				name: "kernel.cell",
				message: "boom",
				corpus: "prime",
				label: "persists",
				sinceWindow: windows[0]?.window ?? "",
				lastWindow: windows.at(-1)?.window ?? "",
				windowsPresent: windows.length,
				windowsRecurring: windows.length,
				claimedByRefinement: false,
				domainActive: true,
				securityClass: false,
				confounds: ["task-mix"],
			},
		],
		rate: [],
	};
}

beforeEach(() => {
	agentDir = realpathSync(mkdtempSync(join(tmpdir(), "trajectory-store-")));
});

afterEach(() => {
	rmSync(agentDir, { recursive: true, force: true });
});

describe("trajectory store", () => {
	it("round-trips a saved file and writes it owner-only", () => {
		const file = fileWith([primeWindow("2025-W02"), primeWindow("2025-W03")]);
		writeTrajectoryIndex(file, agentDir);
		const storePath = getTrajectoryIndexPath(agentDir);
		expect(existsSync(storePath)).toBe(true);
		expect(statSync(storePath).mode & 0o777).toBe(0o600);
		const loaded = readTrajectoryIndex(agentDir);
		expect(loaded?.labels[0]?.fingerprint).toBe("fp1");
		expect(loaded?.windows.map((w) => w.window)).toEqual(["2025-W02", "2025-W03"]);
	});

	it("writes a local .gitignore that excludes trajectory.json and backfill/", () => {
		writeTrajectoryIndex(fileWith([primeWindow("2025-W02")]), agentDir);
		const gitignore = readFileSync(join(getLearningDir(agentDir), ".gitignore"), "utf-8");
		expect(gitignore).toContain("trajectory.json");
		expect(gitignore).toContain("backfill/");
	});

	it("degrades to undefined on corrupt, oversized, or wrong-version files", () => {
		const storePath = getTrajectoryIndexPath(agentDir);
		writeTrajectoryIndex(fileWith([primeWindow("2025-W02")]), agentDir);

		writeFileSync(storePath, "{ not json at all");
		expect(readTrajectoryIndex(agentDir)).toBeUndefined();

		writeFileSync(storePath, JSON.stringify({ ...fileWith([primeWindow("2025-W02")]), version: 999 }));
		expect(readTrajectoryIndex(agentDir)).toBeUndefined();

		writeFileSync(storePath, `${"x".repeat(9 * 1024 * 1024)}`);
		expect(readTrajectoryIndex(agentDir)).toBeUndefined();
	});

	it("bounds the persisted windows at the newest cap and drops every backfill datum", () => {
		const windows: TrajectoryWindow[] = [];
		for (let year = 2020; year <= 2021; year++) {
			for (let week = 1; week <= 40; week++) {
				windows.push(primeWindow(`${year}-W${String(week).padStart(2, "0")}`));
			}
		}
		// Sprinkle in backfill windows that must never be persisted.
		const backfill: TrajectoryWindow = { ...primeWindow("2021-W41"), corpus: "backfill:opencode" };
		const file: TrajectoryStoreFile = {
			...fileWith(windows),
			windows: [...windows, backfill],
			labels: [
				...fileWith(windows).labels,
				{
					fingerprint: "bf1",
					name: "bash",
					message: "sh:permission-denied",
					corpus: "backfill:opencode",
					label: "persists",
					sinceWindow: "2021-W41",
					lastWindow: "2021-W41",
					windowsPresent: 1,
					windowsRecurring: 1,
					claimedByRefinement: false,
					domainActive: true,
					securityClass: false,
					confounds: ["task-mix", "tool-surface", "measurement-instrument"],
				},
			],
		};
		writeTrajectoryIndex(file, agentDir);
		const loaded = readTrajectoryIndex(agentDir);
		expect(loaded?.windows).toHaveLength(DEFAULT_MAX_TRAJECTORY_WINDOWS);
		expect(loaded?.windows.every((w) => w.corpus === "prime")).toBe(true);
		expect(loaded?.labels.every((label) => label.corpus === "prime")).toBe(true);
		expect(readFileSync(getTrajectoryIndexPath(agentDir), "utf-8")).not.toContain("backfill:");
		// Newest 80 windows total (40+40); bound keeps the last 52 by ISO week key.
		expect(loaded?.windows[0]?.window).toBe("2020-W29");
		expect(loaded?.windows.at(-1)?.window).toBe("2021-W40");
	});

	it("boundStoreFile is pure and filters backfill without touching the input", () => {
		const bounded = boundStoreFile({
			...fileWith([primeWindow("2025-W02")]),
			windows: [primeWindow("2025-W02"), { ...primeWindow("2025-W03"), corpus: "backfill:claude" }],
		});
		expect(bounded.windows.map((w) => w.corpus)).toEqual(["prime"]);
	});

	it("survives a second save and returns the latest file", () => {
		writeTrajectoryIndex(fileWith([primeWindow("2025-W02")]), agentDir);
		writeTrajectoryIndex(
			fileWith([primeWindow("2025-W02"), primeWindow("2025-W03"), primeWindow("2025-W04")]),
			agentDir,
		);
		const loaded = readTrajectoryIndex(agentDir);
		expect(loaded?.windows).toHaveLength(3);
	});
});
