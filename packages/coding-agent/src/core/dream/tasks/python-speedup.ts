/**
 * Python speedup (a self-contained code-optimization scored task).
 *
 * The artifact is a Python 3 program that reads an array of integers from
 * standard input and writes one integer to standard output: the sum over all
 * unordered pairs (i < j) of |a_i - a_j|. The root is a CORRECT but deliberately
 * slow reference (a quadratic core, run a few redundant times), so there is real
 * headroom to improve. The score is a correctness-gated speedup: a candidate that
 * fails any hidden test scores 0, and a candidate that passes them all scores
 * `baselineTime / candidateTime` clamped to a cap.
 *
 * Cheat resistance is structural, not cooperative: correctness is checked FIRST,
 * against HIDDEN tests the proposer never sees (it is given only the contract and
 * a couple of PUBLIC examples), so a solution that hard-codes the public answers,
 * swallows exceptions, or prints a plausible constant is rejected before any
 * timing happens. The subprocess runs `python3 -I -B` (isolated, no bytecode) with
 * a minimal environment and a strict per-run timeout, so a hang becomes an invalid
 * `timeout` rather than a stuck evaluation and no ambient config (proxies, user
 * site, PYTHONPATH) leaks in. This is a trust boundary on what may enter the tree,
 * NOT a security sandbox: the child runs with the user's own permissions, exactly
 * as `bash()` does elsewhere in the harness (see `.claude/rules/security.md`).
 *
 * DETERMINISM CAVEAT: `evaluate` is wall-clock timed, so this one task's scores
 * are not byte-deterministic. The framework core stays deterministic (seeded rng +
 * injected clock), and because replay and selection read RECORDED node scores,
 * tree persistence, scoring and replay remain deterministic given the recorded
 * outcomes. `root`, `propose`, `serialize` and `deserialize` are fully
 * deterministic; only `evaluate` reads the clock, by design.
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { SeededRng } from "../rng.js";
import type { DreamFailClass, Evaluation, ScoredTask } from "../task.js";

export interface PythonSpeedupArtifact {
	source: string;
}

/** The interpreter and the isolation flags every candidate is run under. */
export const PYTHON_BIN = "python3";
/** `-I` isolated mode (no PYTHONPATH / user site / cwd on the path); `-B` no .pyc. */
export const PYTHON_RUN_FLAGS = ["-I", "-B"] as const;

/** A candidate source over this many bytes is rejected on parse. */
export const MAX_SOURCE_BYTES = 64 * 1024;
/** Ceiling on a candidate's stdout, so a runaway printer fails instead of buffering forever. */
export const MAX_OUTPUT_BYTES = 4 * 1024 * 1024;
/** Strict per-run wall-clock limit; a slower run is killed and counts as a timeout. */
export const PER_TEST_TIMEOUT_MS = 3_000;
/** Timed runs per candidate; the minimum total is taken to damp scheduler noise. */
export const TIMING_REPEATS = 3;
/** The speedup score is clamped to this, so a near-zero candidate time cannot explode it. */
export const SCORE_CAP = 1_000;
/** Floor on a measured time (ns), guarding the ratio against division by zero. */
const MIN_TIME_NANOS = 1;

/**
 * A correct but deliberately slow baseline. The `reps` constant recomputes the
 * answer a few times and keeps only the last pass: harmless redundancy that gives
 * the zero-token local proposer a small, safe, structured lever (lower `reps`,
 * which never changes the result), while the quadratic core beneath it is what a
 * stronger proposer rewrites. For any `reps >= 1` the output is identical.
 */
export const REFERENCE_SOLUTION = `import sys


def solve(nums):
    n = len(nums)
    reps = 4
    total = 0
    for _ in range(reps):
        total = 0
        for i in range(n):
            ai = nums[i]
            for j in range(i + 1, n):
                d = ai - nums[j]
                total += d if d >= 0 else -d
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

/**
 * Public contract and examples handed to an LLM proposer (never the hidden tests).
 * Callers that drive the flag-gated LLM path pass this as the proposer prompt
 * context; it is exported here so the task owns its own contract.
 */
export const PYTHON_SPEEDUP_PROMPT_CONTEXT = [
	"Task: rewrite the Python 3 program to run faster while staying correct.",
	"Contract:",
	"- Read all input from standard input. The first integer is n; the next n integers are the array a.",
	"- Write ONE integer to standard output: the sum over every unordered pair (i < j) of abs(a[i] - a[j]).",
	"- Use only the standard library. Do not read files, use the network, or read anything but stdin.",
	"Public examples (stdin -> stdout):",
	'  "3\\n1 2 3" -> "4"',
	'  "4\\n-3 7 0 2" -> "32"',
	'Exact output shape: {"source": "<the complete Python 3 program as one JSON string>"}, a JSON object with exactly this one key.',
	"Return the complete program as the source; it is validated as a single non-empty string.",
].join("\n");

interface HiddenTest {
	input: string;
	expected: string;
	timed: boolean;
}

function formatInput(nums: number[]): string {
	return `${nums.length}\n${nums.join(" ")}\n`;
}

/** Exact reference score in BigInt, used only to precompute expected outputs. */
function pairwiseAbsSum(nums: number[]): bigint {
	const sorted = [...nums].sort((a, b) => a - b);
	let total = 0n;
	let prefix = 0n;
	for (let i = 0; i < sorted.length; i++) {
		const value = BigInt(sorted[i]!);
		total += value * BigInt(i) - prefix;
		prefix += value;
	}
	return total;
}

function hidden(nums: number[], timed: boolean): HiddenTest {
	return { input: formatInput(nums), expected: pairwiseAbsSum(nums).toString(), timed };
}

/** Deterministic large input so the timed case is fixed without a huge literal. */
function largeInput(): number[] {
	const count = 700;
	const nums = new Array<number>(count);
	// A fixed 32-bit LCG seed; the concrete input is deterministic and the expected
	// output is derived from it, so the exact seed is immaterial.
	let state = 0x9e3779b9 >>> 0;
	for (let i = 0; i < count; i++) {
		state = (Math.imul(state, 1103515245) + 12345) >>> 0;
		nums[i] = state % 1001;
	}
	return nums;
}

/**
 * Small varied cases catch cheats (all-equal, negatives, a two-element set), and
 * one large case carries the timing signal. Correctness runs every case; timing
 * runs only the timed one.
 */
const HIDDEN_TESTS: readonly HiddenTest[] = [
	hidden([1, 2, 3], false),
	hidden([5, 5, 5, 5], false),
	hidden([-3, 7, 0, 2], false),
	hidden([10, -10], false),
	hidden([0, 1000, 500, 250, 750, 125], false),
	hidden(largeInput(), true),
];

const TIMED_TESTS: readonly HiddenTest[] = HIDDEN_TESTS.filter((test) => test.timed);

/** Only the interpreter needs finding; nothing else is inherited, so no proxy or path config leaks in. */
function buildSandboxEnv(): NodeJS.ProcessEnv {
	const env: NodeJS.ProcessEnv = {};
	if (process.env.PATH !== undefined) env.PATH = process.env.PATH;
	return env;
}

type RunResult = { kind: "ok"; stdout: string; nanos: number } | { kind: "timeout" } | { kind: "error" };

function runProgram(file: string, input: string): RunResult {
	const start = process.hrtime.bigint();
	const result = spawnSync(PYTHON_BIN, [...PYTHON_RUN_FLAGS, file], {
		input,
		timeout: PER_TEST_TIMEOUT_MS,
		killSignal: "SIGKILL",
		maxBuffer: MAX_OUTPUT_BYTES,
		encoding: "utf8",
		env: buildSandboxEnv(),
	});
	const nanos = Number(process.hrtime.bigint() - start);
	const errorCode = result.error ? (result.error as NodeJS.ErrnoException).code : undefined;
	// Check timeout indicators first: a run our own timeout killed surfaces as the
	// kill signal and/or an ETIMEDOUT error, and must not be misread as a crash.
	if (errorCode === "ETIMEDOUT" || result.signal === "SIGKILL") return { kind: "timeout" };
	if (result.error) return { kind: "error" };
	if (result.signal) return { kind: "error" };
	if (result.status !== 0) return { kind: "error" };
	return { kind: "ok", stdout: result.stdout ?? "", nanos };
}

function failClassOf(result: Exclude<RunResult, { kind: "ok" }>): DreamFailClass {
	return result.kind === "timeout" ? "timeout" : "runtime-error";
}

/** Run every hidden test once; return the first failure's class, or null when all pass. */
function checkCorrectness(file: string): DreamFailClass | null {
	for (const test of HIDDEN_TESTS) {
		const result = runProgram(file, test.input);
		if (result.kind !== "ok") return failClassOf(result);
		if (result.stdout.trim() !== test.expected) return "incorrect";
	}
	return null;
}

/** Minimum total wall time (ns) over the timed cases across K repeats; null if any run fails. */
function timeProgram(file: string): number | null {
	let best = Number.POSITIVE_INFINITY;
	for (let repeat = 0; repeat < TIMING_REPEATS; repeat++) {
		let total = 0;
		for (const test of TIMED_TESTS) {
			const result = runProgram(file, test.input);
			if (result.kind !== "ok") return null;
			total += result.nanos;
		}
		if (total < best) best = total;
	}
	return Number.isFinite(best) ? best : null;
}

function withSourceFile<T>(source: string, run: (file: string) => T): T {
	const dir = mkdtempSync(join(tmpdir(), "dream-pyspeed-"));
	try {
		const file = join(dir, "solution.py");
		writeFileSync(file, source, "utf8");
		return run(file);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

function clamp(value: number, low: number, high: number): number {
	if (value < low) return low;
	if (value > high) return high;
	return value;
}

function invalid(failClass: DreamFailClass): Evaluation {
	return { valid: false, score: 0, failClass };
}

/** Deterministic structured mutation: lower the redundant `reps` constant. */
function lowerReps(source: string, rng: SeededRng): string {
	const match = source.match(/^[ \t]*reps\s*=\s*(\d+)\b/m);
	if (!match) return source;
	const current = Number.parseInt(match[1]!, 10);
	if (current <= 1) return source;
	const next = rng.nextInt(2) === 0 ? current - 1 : Math.max(1, Math.floor(current / 2));
	return source.replace(/^([ \t]*reps\s*=\s*)\d+\b/m, `$1${next}`);
}

export function createPythonSpeedupTask(): ScoredTask<PythonSpeedupArtifact> {
	// Cached once per task instance: null once measured-but-unrunnable (e.g. no python3).
	let baselineNanos: number | null | undefined;

	function ensureBaseline(): number | null {
		if (baselineNanos !== undefined) return baselineNanos;
		baselineNanos = withSourceFile(REFERENCE_SOLUTION, (file) => timeProgram(file));
		return baselineNanos;
	}

	return {
		id: "python-speedup",
		root(_rng) {
			return { source: REFERENCE_SOLUTION };
		},
		propose(parent, _params, rng, _round) {
			const base = parent?.source ?? REFERENCE_SOLUTION;
			// One draw picks identity vs. a structured edit, so the zero-token path both
			// explores and can settle; a second draw (inside lowerReps) sizes the edit.
			if (rng.nextInt(3) === 0) return { source: base };
			return { source: lowerReps(base, rng) };
		},
		evaluate(candidate) {
			const source = candidate?.source;
			if (typeof source !== "string" || source.length === 0) return invalid("runtime-error");
			return withSourceFile(source, (file) => {
				const failClass = checkCorrectness(file);
				if (failClass !== null) return invalid(failClass);
				const baseline = ensureBaseline();
				if (baseline === null) return invalid("runtime-error");
				const candidateNanos = timeProgram(file);
				if (candidateNanos === null) return invalid("runtime-error");
				const ratio = baseline / Math.max(candidateNanos, MIN_TIME_NANOS);
				const score = clamp(ratio, 0, SCORE_CAP);
				return { valid: true, score };
			});
		},
		serialize(candidate) {
			return { source: candidate.source };
		},
		deserialize(value) {
			if (typeof value !== "object" || value === null) {
				throw new TypeError("python-speedup artifact must be an object");
			}
			const source = (value as Record<string, unknown>).source;
			if (typeof source !== "string" || source.length === 0) {
				throw new TypeError("python-speedup artifact must have a non-empty source string");
			}
			if (Buffer.byteLength(source, "utf8") > MAX_SOURCE_BYTES) {
				throw new TypeError(`python-speedup source exceeds ${MAX_SOURCE_BYTES} bytes`);
			}
			return { source };
		},
	};
}
