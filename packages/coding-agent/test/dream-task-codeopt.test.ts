import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createSeededRng } from "../src/core/dream/rng.js";
import {
	createPythonSpeedupTask,
	MAX_SOURCE_BYTES,
	PYTHON_RUN_FLAGS,
	type PythonSpeedupArtifact,
	REFERENCE_SOLUTION,
} from "../src/core/dream/tasks/python-speedup.js";
import { canonicalJson } from "../src/core/ravo/canonical-json.js";

const PROPOSE = { stepScale: 0.1, refineDepth: 2, branchWidth: 2 } as const;

const HAS_PYTHON3 = spawnSync("python3", ["-I", "-B", "-c", "print(1)"], { encoding: "utf8" }).status === 0;

// A genuinely faster, still-correct rewrite: O(n log n) via sort + prefix sums.
const FAST_SOLUTION = `import sys


def solve(nums):
    nums = sorted(nums)
    total = 0
    prefix = 0
    for i, v in enumerate(nums):
        total += v * i - prefix
        prefix += v
    return total


def main():
    data = sys.stdin.buffer.read().split()
    if not data:
        return
    n = int(data[0])
    nums = [int(x) for x in data[1 : 1 + n]]
    sys.stdout.write(str(solve(nums)))
    sys.stdout.write("\\n")


main()
`;

// Always prints 0: correct for the all-equal case, wrong for the first hidden test.
const INCORRECT_SOLUTION = `import sys


def main():
    sys.stdin.buffer.read()
    sys.stdout.write("0\\n")


main()
`;

const TIMEOUT_SOLUTION = `while True:
    pass
`;

const SYNTAX_ERROR_SOLUTION = `def solve(:
    return 0
`;

let priorDreamDir: string | undefined;
let priorAgentDir: string | undefined;
let dreamDir: string;
let agentDir: string;

beforeAll(() => {
	// Never touch the live agent state, even though the task uses its own temp dir.
	priorDreamDir = process.env.PRIME_AGENT_DREAM_DIR;
	priorAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	dreamDir = mkdtempSync(join(tmpdir(), "dream-codeopt-"));
	agentDir = mkdtempSync(join(tmpdir(), "dream-codeopt-agent-"));
	process.env.PRIME_AGENT_DREAM_DIR = dreamDir;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
});

afterAll(() => {
	if (priorDreamDir === undefined) delete process.env.PRIME_AGENT_DREAM_DIR;
	else process.env.PRIME_AGENT_DREAM_DIR = priorDreamDir;
	if (priorAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = priorAgentDir;
	rmSync(dreamDir, { recursive: true, force: true });
	rmSync(agentDir, { recursive: true, force: true });
});

describe("python-speedup task (pure, no subprocess)", () => {
	it("has a stable id and a valid-shape reference root", () => {
		const task = createPythonSpeedupTask();
		expect(task.id).toBe("python-speedup");
		const root = task.root(createSeededRng(1));
		expect(root.source).toBe(REFERENCE_SOLUTION);
	});

	it("proposes deterministically for identical seeds and only edits the source shape", () => {
		const task = createPythonSpeedupTask();
		const root = task.root(createSeededRng(1));
		const first = task.propose(root, PROPOSE, createSeededRng(42), 1);
		const second = task.propose(root, PROPOSE, createSeededRng(42), 1);
		expect(canonicalJson(task.serialize(first))).toBe(canonicalJson(task.serialize(second)));
		expect(typeof first.source).toBe("string");
		expect(first.source.length).toBeGreaterThan(0);
	});

	it("lowers the redundant reps constant for at least one seed (a small structured edit)", () => {
		const task = createPythonSpeedupTask();
		const root = task.root(createSeededRng(1));
		let sawLoweredReps = false;
		for (let seed = 0; seed < 32 && !sawLoweredReps; seed++) {
			const child = task.propose(root, PROPOSE, createSeededRng(seed), 1);
			const match = child.source.match(/^[ \t]*reps\s*=\s*(\d+)\b/m);
			if (match && Number.parseInt(match[1]!, 10) < 4) sawLoweredReps = true;
		}
		expect(sawLoweredReps).toBe(true);
	});

	it("round-trips through serialize/deserialize and rejects bad shapes", () => {
		const task = createPythonSpeedupTask();
		const artifact: PythonSpeedupArtifact = { source: REFERENCE_SOLUTION };
		expect(task.deserialize(task.serialize(artifact))).toEqual(artifact);
		expect(() => task.deserialize({ source: 42 })).toThrow();
		expect(() => task.deserialize({ source: "" })).toThrow();
		expect(() => task.deserialize({})).toThrow();
		expect(() => task.deserialize(null)).toThrow();
		const oversized = "x".repeat(MAX_SOURCE_BYTES + 1);
		expect(() => task.deserialize({ source: oversized })).toThrow();
	});

	it("runs the interpreter in isolated mode and opens no network sockets from the harness", () => {
		// python3 -I isolates the child from PYTHONPATH / user site / cwd, and -B skips .pyc.
		expect([...PYTHON_RUN_FLAGS]).toEqual(["-I", "-B"]);
		const source = readFileSync(new URL("../src/core/dream/tasks/python-speedup.ts", import.meta.url), "utf8");
		// The harness itself never touches the network: no node networking modules are imported.
		expect(source).not.toMatch(/from ["']node:(net|http|https|dgram|dns|tls)["']/);
		// The subprocess env is minimal (only PATH), so no proxy or ambient config leaks in.
		expect(source).toMatch(/env\.PATH = process\.env\.PATH/);
		expect(source).not.toMatch(/env:\s*process\.env\b/);
	});
});

describe.runIf(HAS_PYTHON3)("python-speedup task (subprocess-scored)", () => {
	it("scores the reference root valid and finite (the baseline, near 1)", () => {
		const task = createPythonSpeedupTask();
		const evaluation = task.evaluate({ source: REFERENCE_SOLUTION });
		expect(evaluation.valid).toBe(true);
		expect(evaluation.score).toBeGreaterThan(0);
		expect(Number.isFinite(evaluation.score)).toBe(true);
	}, 60_000);

	it("scores a correct fast solution strictly higher than a correct slow one", () => {
		const task = createPythonSpeedupTask();
		const slow = task.evaluate({ source: REFERENCE_SOLUTION });
		const fast = task.evaluate({ source: FAST_SOLUTION });
		expect(slow.valid).toBe(true);
		expect(fast.valid).toBe(true);
		expect(fast.score).toBeGreaterThan(slow.score);
		expect(fast.score).toBeGreaterThan(1);
	}, 60_000);

	it("scores an incorrect solution 0 with failClass 'incorrect'", () => {
		const task = createPythonSpeedupTask();
		const evaluation = task.evaluate({ source: INCORRECT_SOLUTION });
		expect(evaluation.valid).toBe(false);
		expect(evaluation.score).toBe(0);
		expect(evaluation.failClass).toBe("incorrect");
	}, 60_000);

	it("treats a non-terminating solution as an invalid timeout, not a hang", () => {
		const task = createPythonSpeedupTask();
		const start = Date.now();
		const evaluation = task.evaluate({ source: TIMEOUT_SOLUTION });
		const elapsed = Date.now() - start;
		expect(evaluation.valid).toBe(false);
		expect(evaluation.score).toBe(0);
		expect(evaluation.failClass).toBe("timeout");
		// The strict per-run timeout bounds it; it must not run away.
		expect(elapsed).toBeLessThan(30_000);
	}, 60_000);

	it("treats a syntax-error solution as an invalid runtime-error", () => {
		const task = createPythonSpeedupTask();
		const evaluation = task.evaluate({ source: SYNTAX_ERROR_SOLUTION });
		expect(evaluation.valid).toBe(false);
		expect(evaluation.score).toBe(0);
		expect(evaluation.failClass).toBe("runtime-error");
	}, 60_000);
});
