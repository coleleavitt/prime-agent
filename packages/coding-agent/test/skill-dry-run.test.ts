import { execSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
	countValidRefinementEdits,
	type RefinementEdit,
	type RefinementProposal,
} from "../src/core/refinement/index.js";
import {
	dryRunSkillEdits,
	resolveKernelPython,
	type SkillDryRunResult,
	screenValidEdits,
	skippedSkillDryRun,
} from "../src/core/refinement/skill-dry-run.js";

const python3 = execSync("which python3", { encoding: "utf8" }).trim();

let tmp: string;
beforeAll(() => {
	tmp = mkdtempSync(path.join(os.tmpdir(), "skill-dry-run-"));
	writeFileSync(
		path.join(tmp, "good_skill.py"),
		"def run(x):\n    return x\n\nclass Tool:\n    def call(self):\n        pass\n\nNOT_CALLABLE = 42\n",
	);
	writeFileSync(path.join(tmp, "bad_import_skill.py"), "raise ImportError('missing dependency frobnicate')\n");
	writeFileSync(path.join(tmp, "slow_skill.py"), "import time\ntime.sleep(30)\ndef run():\n    pass\n");
	writeFileSync(
		path.join(tmp, "side_effect_skill.py"),
		"import sys\ndef run():\n    sys.stdout.write('CALLED')\n    raise SystemExit(0)\n",
	);
});
afterAll(() => {
	rmSync(tmp, { recursive: true, force: true });
});

function skillEdit(
	reference: Record<string, unknown> | undefined,
	overrides: Partial<RefinementEdit> = {},
): RefinementEdit {
	return {
		action: "create",
		kind: "skill",
		title: "Skill",
		content: "Call `await run(...)`.",
		reference,
		arguments: {},
		...overrides,
	};
}

function proposal(edits: RefinementEdit[]): RefinementProposal {
	return { summary: "s", rationale: "r", edits, expectedOutcome: "e" };
}

function run(
	edits: RefinementEdit[],
	extra: { timeoutMs?: number; signal?: AbortSignal } = {},
): Promise<SkillDryRunResult[]> {
	return dryRunSkillEdits(proposal(edits), { pythonPath: python3, env: { PYTHONPATH: tmp }, ...extra });
}

describe("dryRunSkillEdits", () => {
	it("passes a module whose callable exists (PYTHONPATH honored under -I)", async () => {
		const results = await run([skillEdit({ type: "python", import: "good_skill", callable: "run" })]);
		expect(results).toHaveLength(1);
		expect(results[0].editIndex).toBe(0);
		expect(results[0].ok).toBe(true);
		expect(results[0].detail).toContain("good_skill.run");
		expect(results[0].durationMs).toBeGreaterThanOrEqual(0);
	});

	it("resolves dotted callables and derives the callable from call_pattern", async () => {
		const results = await run([
			skillEdit({ type: "python", import: "good_skill", callable: "Tool.call" }),
			skillEdit({ type: "python", python_import: "good_skill", call_pattern: "await good_skill.run(x)" }),
		]);
		expect(results.map((r) => r.ok)).toEqual([true, true]);
	});

	it("reports the ImportError text for a module that fails to import", async () => {
		const results = await run([skillEdit({ type: "python", import: "bad_import_skill", callable: "run" })]);
		expect(results[0].ok).toBe(false);
		expect(results[0].detail).toContain("ImportError");
		expect(results[0].detail).toContain("missing dependency frobnicate");
	});

	it("fails on a module that does not exist", async () => {
		const results = await run([skillEdit({ type: "python", import: "does_not_exist_skill_xyz", callable: "run" })]);
		expect(results[0].ok).toBe(false);
		expect(results[0].detail).toContain("ModuleNotFoundError");
	});

	it("fails when the attribute is missing or not callable", async () => {
		const results = await run([
			skillEdit({ type: "python", import: "good_skill", callable: "nope" }),
			skillEdit({ type: "python", import: "good_skill", callable: "NOT_CALLABLE" }),
		]);
		expect(results[0].ok).toBe(false);
		expect(results[0].detail).toContain("AttributeError");
		expect(results[1].ok).toBe(false);
		expect(results[1].detail).toContain("not callable");
	});

	it("kills a module that sleeps past the timeout", async () => {
		const started = Date.now();
		const results = await run([skillEdit({ type: "python", import: "slow_skill", callable: "run" })], {
			timeoutMs: 500,
		});
		expect(results[0].ok).toBe(false);
		expect(results[0].detail).toContain("timeout");
		expect(Date.now() - started).toBeLessThan(10_000);
	});

	it("never calls the callable", async () => {
		const results = await run([skillEdit({ type: "python", import: "side_effect_skill", callable: "run" })]);
		expect(results[0].ok).toBe(true);
		expect(results[0].detail).not.toContain("CALLED");
	});

	it("rejects malformed references and non-object arguments without spawning", async () => {
		const results = await dryRunSkillEdits(
			proposal([
				skillEdit(undefined),
				skillEdit({ type: "shell", import: "good_skill", callable: "run" }),
				skillEdit({ type: "python", callable: "run" }),
				skillEdit({ type: "python", import: "good_skill" }),
				skillEdit({ type: "python", import: "good skill; rm -rf /", callable: "run" }),
				skillEdit(
					{ type: "python", import: "good_skill", callable: "run" },
					{ arguments: [] as unknown as Record<string, unknown> },
				),
				skillEdit({ type: "python", import: "good_skill", callable: "run" }, { arguments: undefined }),
			]),
			{ pythonPath: "/nonexistent/python-should-not-run" },
		);
		expect(results).toHaveLength(7);
		expect(results.every((r) => !r.ok)).toBe(true);
		expect(results.every((r) => r.durationMs === 0)).toBe(true);
		expect(results[0].detail).toContain("reference");
		expect(results[1].detail).toContain("python");
		expect(results[2].detail).toContain("import");
		expect(results[3].detail).toContain("callable");
		expect(results[4].detail).toContain("dotted module");
		expect(results[5].detail).toContain("arguments");
		expect(results[6].detail).toContain("arguments");
	});

	it("reports a spawn failure when the interpreter is missing", async () => {
		const results = await dryRunSkillEdits(
			proposal([skillEdit({ type: "python", import: "good_skill", callable: "run" })]),
			{
				pythonPath: "/nonexistent/python-binary",
				timeoutMs: 2000,
			},
		);
		expect(results[0].ok).toBe(false);
		expect(results[0].detail).toContain("spawn failed");
	});

	it("skips non-skill edits and skill deletes, keeping proposal edit indexes", async () => {
		const results = await run([
			{ action: "create", kind: "memory", title: "m", content: "c" },
			{ action: "delete", kind: "skill", id: "old" },
			skillEdit({ type: "python", import: "good_skill", callable: "run" }),
			{ action: "update", kind: "prompt", id: "p", title: "p", content: "c" },
			skillEdit({ type: "python", import: "bad_import_skill", callable: "run" }, { action: "update", id: "x" }),
		]);
		expect(results.map((r) => [r.editIndex, r.ok])).toEqual([
			[2, true],
			[4, false],
		]);
	});

	it("runs many edits with a bounded concurrency and returns them in order", async () => {
		const edits = Array.from({ length: 9 }, (_, i) =>
			skillEdit({ type: "python", import: i % 3 === 1 ? "bad_import_skill" : "good_skill", callable: "run" }),
		);
		const results = await run(edits);
		expect(results.map((r) => r.editIndex)).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8]);
		expect(results.map((r) => r.ok)).toEqual([true, false, true, true, false, true, true, false, true]);
	});

	it("honors an already-aborted signal", async () => {
		const controller = new AbortController();
		controller.abort();
		const results = await run([skillEdit({ type: "python", import: "slow_skill", callable: "run" })], {
			signal: controller.signal,
			timeoutMs: 5000,
		});
		expect(results[0].ok).toBe(false);
		expect(results[0].detail).toContain("aborted");
	});
});

describe("screenValidEdits", () => {
	const good = skillEdit({ type: "python", import: "good_skill", callable: "run" });
	const memory: RefinementEdit = { action: "create", kind: "memory", title: "m", content: "c" };

	it("subtracts only failed dry-runs from the structural count", () => {
		const p = proposal([memory, good, good, good]);
		const structural = countValidRefinementEdits(p);
		expect(structural).toBe(4);
		const dryRun: SkillDryRunResult[] = [
			{ editIndex: 1, ok: true, detail: "", durationMs: 1 },
			{ editIndex: 2, ok: false, detail: "ImportError", durationMs: 1 },
			{ editIndex: 3, ok: false, detail: "timeout", durationMs: 1 },
		];
		expect(screenValidEdits(p, structural, dryRun)).toBe(2);
		expect(screenValidEdits(p, structural, [])).toBe(4);
		expect(screenValidEdits(p, structural, skippedSkillDryRun(p))).toBe(4);
	});

	it("does not double-subtract edits that already failed structural validation", () => {
		const structurallyInvalid = skillEdit(undefined);
		const p = proposal([structurallyInvalid, good]);
		const structural = countValidRefinementEdits(p);
		expect(structural).toBe(1);
		const dryRun: SkillDryRunResult[] = [
			{ editIndex: 0, ok: false, detail: "requires reference", durationMs: 0 },
			{ editIndex: 1, ok: false, detail: "ImportError", durationMs: 1 },
		];
		expect(screenValidEdits(p, structural, dryRun)).toBe(0);
	});

	it("ignores duplicate, non-skill and out-of-range results and never goes negative", () => {
		const p = proposal([memory, good]);
		const dryRun: SkillDryRunResult[] = [
			{ editIndex: 1, ok: false, detail: "x", durationMs: 1 },
			{ editIndex: 1, ok: false, detail: "x", durationMs: 1 },
			{ editIndex: 0, ok: false, detail: "x", durationMs: 1 },
			{ editIndex: 9, ok: false, detail: "x", durationMs: 1 },
		];
		expect(screenValidEdits(p, 2, dryRun)).toBe(1);
		expect(screenValidEdits(p, 0, dryRun)).toBe(0);
	});
});

describe("resolveKernelPython", () => {
	const saved = { ...process.env };
	afterAll(() => {
		process.env.PRIME_AGENT_KERNEL_PYTHON = saved.PRIME_AGENT_KERNEL_PYTHON;
		process.env.PRIME_AGENT_KERNEL_VENV = saved.PRIME_AGENT_KERNEL_VENV;
		process.env.XDG_DATA_HOME = saved.XDG_DATA_HOME;
		process.env.HOME = saved.HOME;
		for (const key of ["PRIME_AGENT_KERNEL_PYTHON", "PRIME_AGENT_KERNEL_VENV", "XDG_DATA_HOME"]) {
			if (saved[key] === undefined) delete process.env[key];
		}
	});

	it("returns the PRIME_AGENT_KERNEL_PYTHON override when it exists on disk", () => {
		process.env.PRIME_AGENT_KERNEL_PYTHON = python3;
		expect(resolveKernelPython()).toBe(python3);
		process.env.PRIME_AGENT_KERNEL_PYTHON = "/nonexistent/python";
		expect(resolveKernelPython()).toBeUndefined();
	});

	it("returns the kernel venv interpreter when present and undefined otherwise", () => {
		delete process.env.PRIME_AGENT_KERNEL_PYTHON;
		const venv = path.join(tmp, "venv");
		process.env.PRIME_AGENT_KERNEL_VENV = venv;
		process.env.XDG_DATA_HOME = path.join(tmp, "xdg-empty");
		expect(resolveKernelPython()).toBeUndefined();
		execSync(
			`mkdir -p ${JSON.stringify(path.join(venv, "bin"))} && touch ${JSON.stringify(path.join(venv, "bin", "python"))}`,
		);
		expect(resolveKernelPython()).toBe(path.join(venv, "bin", "python"));
	});

	it("skippedSkillDryRun marks skill edits ok with the documented detail", () => {
		const p = proposal([
			{ action: "create", kind: "memory", title: "m", content: "c" },
			skillEdit({ type: "python", import: "good_skill", callable: "run" }),
		]);
		expect(skippedSkillDryRun(p)).toEqual([
			{ editIndex: 1, ok: true, detail: "dry-run skipped: no kernel python", durationMs: 0 },
		]);
	});
});
