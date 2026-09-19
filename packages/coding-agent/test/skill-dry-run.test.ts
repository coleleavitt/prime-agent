import { execSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import {
	sanitizedPythonEnvironment,
	skillImportEnvironment,
	WINDOWS_PYTHON_ENV_KEYS,
} from "../src/core/ravo/python-environment.js";
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
import { saveToolforgeLedger, toolforgeLedgerPath } from "../src/core/toolforge/ledger.js";

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
	writeFileSync(
		path.join(tmp, "env_probe_skill.py"),
		[
			"import json, os",
			"leaked = sorted(key for key in os.environ if key not in ('PATH', 'HOME', 'LANG', 'PYTHONPATH', 'LC_CTYPE'))",
			"if leaked:",
			"    raise ImportError('inherited ' + json.dumps(leaked))",
			"def run():",
			"    pass",
			"",
		].join("\n"),
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

describe("dry-run environment", () => {
	const saved = {
		pythonPath: process.env.PYTHONPATH,
		agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
		leak: process.env.PRIME_AGENT_DRY_RUN_LEAK,
	};
	const restore = (key: string, value: string | undefined) => {
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	};
	afterEach(() => {
		restore("PYTHONPATH", saved.pythonPath);
		restore("PRIME_AGENT_CODING_AGENT_DIR", saved.agentDir);
		restore("PRIME_AGENT_DRY_RUN_LEAK", saved.leak);
	});

	it("passes the interpreter only PATH, HOME, LANG and the PYTHONPATH roots", async () => {
		process.env.PRIME_AGENT_DRY_RUN_LEAK = "1";
		const results = await dryRunSkillEdits(
			proposal([skillEdit({ type: "python", import: "env_probe_skill", callable: "run" })]),
			{ pythonPath: python3, env: { PYTHONPATH: tmp, PRIME_AGENT_DRY_RUN_OVERRIDE: "1" } },
		);
		expect(results.map((result) => [result.ok, result.detail])).toEqual([
			[true, expect.stringContaining("imported")],
		]);
	});

	it("imports through the host's PYTHONPATH entries and every toolforge source root", async () => {
		const edits = [
			skillEdit({ type: "python", import: "good_skill", callable: "run" }),
			skillEdit({ type: "python", import: "prime_agent_dry_run_forged", callable: "run" }),
		];
		const agentDir = mkdtempSync(path.join(os.tmpdir(), "skill-dry-run-agent-"));
		try {
			process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
			const packagePath = path.join(agentDir, "skills", "prime-agent-dry-run-forged");
			mkdirSync(path.join(packagePath, "src", "prime_agent_dry_run_forged"), { recursive: true });
			writeFileSync(
				path.join(packagePath, "src", "prime_agent_dry_run_forged", "__init__.py"),
				"def run():\n    return 1\n",
			);
			saveToolforgeLedger(
				{
					schema: 1,
					records: [
						{
							name: "prime-agent-dry-run-forged",
							importName: "prime_agent_dry_run_forged",
							packagePath,
							sourceSha: "",
							exitTestSha: "",
							status: "published",
							gate: [],
							installed: false,
							at: "2026-09-16T00:00:00.000Z",
							version: 1,
						},
					],
				},
				toolforgeLedgerPath(agentDir),
			);
			process.env.PYTHONPATH = ["/prime-agent-dry-run-missing-root", tmp].join(path.delimiter);
			expect((await dryRunSkillEdits(proposal(edits), { pythonPath: python3 })).map((result) => result.ok)).toEqual([
				true,
				true,
			]);
			delete process.env.PYTHONPATH;
			expect((await dryRunSkillEdits(proposal(edits), { pythonPath: python3 })).map((result) => result.ok)).toEqual([
				false,
				true,
			]);
		} finally {
			rmSync(agentDir, { recursive: true, force: true });
		}
	});

	it("runs each probe in a fresh temporary directory, removed afterwards, whatever cwd the caller passes", async () => {
		const root = mkdtempSync(path.join(os.tmpdir(), "skill-dry-run-cwd-"));
		try {
			const callerDir = path.join(root, "caller");
			const moduleDir = path.join(root, "modules");
			mkdirSync(callerDir);
			mkdirSync(moduleDir);
			writeFileSync(path.join(callerDir, "prime_agent_cwd_config.json"), "{}\n");
			writeFileSync(
				path.join(moduleDir, "prime_agent_cwd_skill.py"),
				[
					"import json, os",
					"json.dump({'cwd': os.getcwd()}, open(os.path.join(os.path.dirname(__file__), 'report.json'), 'w'))",
					"open('prime_agent_cwd_config.json').close()",
					"def run():",
					"    pass",
					"",
				].join("\n"),
			);
			const results = await dryRunSkillEdits(
				proposal([skillEdit({ type: "python", import: "prime_agent_cwd_skill", callable: "run" })]),
				{ pythonPath: python3, cwd: callerDir, env: { PYTHONPATH: moduleDir } },
			);
			// The referee adjudicates in a fresh directory too, so an import that needs the caller's cwd fails both.
			expect(results.map((result) => result.ok)).toEqual([false]);
			expect(results[0].detail).toContain("FileNotFoundError");
			const { cwd } = JSON.parse(readFileSync(path.join(moduleDir, "report.json"), "utf8")) as { cwd: string };
			expect(path.resolve(cwd)).not.toBe(path.resolve(callerDir));
			expect(path.resolve(cwd)).not.toBe(path.resolve(process.cwd()));
			expect(path.basename(cwd)).toMatch(/^prime-agent-dry-run-/);
			expect(existsSync(cwd)).toBe(false);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	it("resolves relative sysPath and PYTHONPATH roots against the caller's cwd", async () => {
		const root = mkdtempSync(path.join(os.tmpdir(), "skill-dry-run-relroot-"));
		try {
			mkdirSync(path.join(root, "relroot"));
			mkdirSync(path.join(root, "envroot"));
			writeFileSync(path.join(root, "relroot", "prime_agent_relroot_skill.py"), "def run():\n    pass\n");
			writeFileSync(path.join(root, "envroot", "prime_agent_envroot_skill.py"), "def run():\n    pass\n");
			const edits = [
				skillEdit({ type: "python", import: "prime_agent_relroot_skill", callable: "run" }),
				skillEdit({ type: "python", import: "prime_agent_envroot_skill", callable: "run" }),
			];
			const options = { pythonPath: python3, sysPath: ["relroot"], env: { PYTHONPATH: "envroot" } };
			expect(
				(await dryRunSkillEdits(proposal(edits), { ...options, cwd: root })).map((result) => result.ok),
			).toEqual([true, true]);
			expect((await dryRunSkillEdits(proposal(edits), options)).map((result) => result.ok)).toEqual([false, false]);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});

describe("python environment builder", () => {
	const windowsHost: NodeJS.ProcessEnv = {
		Path: "C:\\Windows\\system32;C:\\Python312",
		SystemRoot: "C:\\Windows",
		windir: "C:\\Windows",
		USERPROFILE: "C:\\Users\\u",
		TEMP: "C:\\Users\\u\\AppData\\Local\\Temp",
		TMP: "C:\\Users\\u\\AppData\\Local\\Temp",
		ComSpec: "C:\\Windows\\system32\\cmd.exe",
		PATHEXT: ".COM;.EXE;.BAT",
		LANG: "en_US.UTF-8",
		PythonPath: "C:\\host\\lib;relative\\lib",
		PRIME_AGENT_SECRET: "leak",
		PYTHONSTARTUP: "C:\\startup.py",
	};

	it("passes what CPython on win32 needs to start and open sockets, matching names case-insensitively", () => {
		expect(WINDOWS_PYTHON_ENV_KEYS).toEqual([
			"SYSTEMROOT",
			"WINDIR",
			"USERPROFILE",
			"TEMP",
			"TMP",
			"COMSPEC",
			"PATHEXT",
		]);
		expect(
			sanitizedPythonEnvironment(["C:\\roots\\a", "rel"], windowsHost, { platform: "win32", cwd: "C:\\work" }),
		).toEqual({
			PATH: "C:\\Windows\\system32;C:\\Python312",
			LANG: "en_US.UTF-8",
			SYSTEMROOT: "C:\\Windows",
			WINDIR: "C:\\Windows",
			USERPROFILE: "C:\\Users\\u",
			TEMP: "C:\\Users\\u\\AppData\\Local\\Temp",
			TMP: "C:\\Users\\u\\AppData\\Local\\Temp",
			COMSPEC: "C:\\Windows\\system32\\cmd.exe",
			PATHEXT: ".COM;.EXE;.BAT",
			PYTHONPATH: "C:\\roots\\a;C:\\work\\rel",
		});
		// A later spelling is the override, as `{ ...process.env, ...options.env }` writes it.
		expect(sanitizedPythonEnvironment([], { ...windowsHost, PATH: "C:\\override" }, { platform: "win32" }).PATH).toBe(
			"C:\\override",
		);
		expect(skillImportEnvironment([], windowsHost, { platform: "win32", cwd: "C:\\work" }).PYTHONPATH).toBe(
			"C:\\host\\lib;C:\\work\\relative\\lib",
		);
	});

	it("passes only PATH, HOME and LANG, by exact name, anywhere else", () => {
		const host: NodeJS.ProcessEnv = { ...windowsHost, PATH: "/usr/bin", HOME: "/home/u", PYTHONPATH: "/host:rel" };
		expect(sanitizedPythonEnvironment(["rel"], host, { platform: "linux", cwd: "/work" })).toEqual({
			PATH: "/usr/bin",
			HOME: "/home/u",
			LANG: "en_US.UTF-8",
			PYTHONPATH: "/work/rel",
		});
		expect(skillImportEnvironment([], host, { platform: "linux", cwd: "/work" }).PYTHONPATH).toBe("/host:/work/rel");
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
