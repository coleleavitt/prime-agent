import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.js";
import type { PythonSkillPackageInstaller } from "../src/core/kernel/bootstrap.js";
import { ReplKernelManager } from "../src/core/kernel/index.js";
import { getPythonSkillRuntimeInfo, loadSkills } from "../src/core/skills.js";
import { loadToolforgeLedger, toolforgeLedgerPath, toolforgeSrcRoots } from "../src/core/toolforge/ledger.js";
import { publishToolforgeSkill, validateToolforgeName } from "../src/core/toolforge/publish.js";
import { buildRlmBootstrapCode, IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

const RUNTIME_SRC = resolve(__dirname, "..", "..", "..", "prime-agent-runtime", "src");

const SLUGIFY_SOURCE = `"""Turn a string into a URL slug."""

import re


def run(text: str) -> str:
    """Lowercase text and join its word characters with hyphens."""
    return re.sub(r"[^a-z0-9]+", "-", str(text).lower()).strip("-")
`;

const SLUGIFY_DOC = "Turn arbitrary text into a lowercase hyphenated slug.";

const SLUGIFY_EXIT_TEST = `import slugify

assert slugify.run("A B") == "a-b", slugify.run("A B")
assert slugify.run("Hello,  World!") == "hello-world", slugify.run("Hello,  World!")
`;

/** A python that can import the edited runtime, or null to skip the kernel half. */
function resolveTestPython(): string | null {
	const candidates = [
		process.env.PRIME_AGENT_KERNEL_PYTHON,
		resolve(__dirname, "..", "..", "..", "prime-agent-runtime", ".venv", "bin", "python"),
		join(homedir(), ".prime", "agent", "kernel-venv", "bin", "python"),
	].filter((candidate): candidate is string => Boolean(candidate));
	for (const python of candidates) {
		if (!existsSync(python)) continue;
		const check = spawnSync(python, ["-c", "import rlm.toolforge; assert callable(rlm.toolforge.publish)"], {
			encoding: "utf8",
			env: { ...process.env, PYTHONPATH: RUNTIME_SRC },
		});
		if (check.status === 0) return python;
	}
	return null;
}

const python = resolveTestPython();

/** Promotes exactly like the real installer but never touches the shared kernel venv. */
function recordingInstaller(installs: string[]): PythonSkillPackageInstaller {
	return async (request) => {
		await request.beforeInstall?.();
		installs.push(request.packagePath);
		return { installed: false, detail: "test installer: promoted without installing", durationMs: 0 };
	};
}

describe("toolforge name validation", () => {
	it("accepts a name nothing else answers to", () => {
		expect(validateToolforgeName("slugify")).toEqual({ importName: "slugify" });
		expect(validateToolforgeName("two-words")).toEqual({ importName: "two_words" });
	});

	it("rejects names that would win the globals() lookup over the kernel, builtins or the stdlib", () => {
		for (const name of ["bash", "rlm", "mcp", "open", "print", "json", "os", "sys", "import", "type"]) {
			const check = validateToolforgeName(name);
			expect(check.importName, `${name} must be rejected`).toBeUndefined();
			expect(check.error).toMatch(/collides with a Python builtin, keyword, stdlib module or kernel-bound name/);
		}
	});

	it("rejects a collision with a skill already loaded in this kernel", () => {
		expect(validateToolforgeName("edit", ["edit", "goal"]).error).toMatch(/collides with the loaded skill edit/);
		expect(validateToolforgeName("slugify", ["edit", "goal"])).toEqual({ importName: "slugify" });
	});

	it("keeps the charset and shape rules", () => {
		expect(validateToolforgeName("Slugify").error).toMatch(/lowercase/);
		expect(validateToolforgeName("-lead").error).toMatch(/hyphen/);
		expect(validateToolforgeName("double--hyphen").error).toMatch(/hyphen/);
		expect(validateToolforgeName("").error).toMatch(/non-empty/);
	});
});

const describeIfPython = python ? describe : describe.skip;

describeIfPython("toolforge double-run gate", () => {
	let agentDir: string;
	let skillsDir: string;
	let installs: string[];

	beforeEach(() => {
		agentDir = mkdtempSync(join(tmpdir(), "pi-toolforge-gate-"));
		skillsDir = join(agentDir, "skills");
		installs = [];
	});

	const publish = (overrides: Partial<{ name: string; source: string; doc: string; exitTest: string }> = {}) =>
		publishToolforgeSkill(
			{
				name: "slugify",
				source: SLUGIFY_SOURCE,
				doc: SLUGIFY_DOC,
				exitTest: SLUGIFY_EXIT_TEST,
				...overrides,
			},
			{
				skillsDir,
				ledgerPath: toolforgeLedgerPath(agentDir),
				stagingDir: join(agentDir, "toolforge", "staging"),
				pythonPath: python as string,
				installPackage: recordingInstaller(installs),
			},
		);

	it("publishes when the exit test fails without the package and passes with it", async () => {
		const result = await publish();

		expect(result.reason).toBeUndefined();
		expect(result.status).toBe("published");
		expect(result.importName).toBe("slugify");
		expect(result.version).toBe(1);
		expect(result.gate.map((run) => [run.phase, run.outcome, run.ok])).toEqual([
			["negative", "raised", true],
			["positive", "clean", true],
		]);

		const packageRoot = join(skillsDir, "slugify");
		expect(installs).toEqual([packageRoot]);
		expect(existsSync(join(packageRoot, "SKILL.md"))).toBe(true);
		expect(existsSync(join(packageRoot, "pyproject.toml"))).toBe(true);
		expect(readFileSync(join(packageRoot, "src", "slugify", "__init__.py"), "utf-8")).toContain("def run(");
		expect(readFileSync(join(packageRoot, "_exit_test.py"), "utf-8")).toContain("slugify.run");

		const ledger = loadToolforgeLedger(toolforgeLedgerPath(agentDir));
		expect(ledger.records).toHaveLength(1);
		expect(ledger.records[0]).toMatchObject({ name: "slugify", status: "published", version: 1 });

		const previousAgentDir = process.env[ENV_AGENT_DIR];
		process.env[ENV_AGENT_DIR] = agentDir;
		try {
			expect(toolforgeSrcRoots()).toEqual([join(packageRoot, "src")]);
		} finally {
			if (previousAgentDir === undefined) delete process.env[ENV_AGENT_DIR];
			else process.env[ENV_AGENT_DIR] = previousAgentDir;
		}
	}, 60_000);

	it("rejects an exit test that passes without the implementation", async () => {
		const result = await publish({ exitTest: "import slugify\n\nassert True\n" });

		expect(result.status).toBe("rejected");
		expect(result.reason).toMatch(/negative run did not fail/);
		expect(result.gate).toHaveLength(1);
		expect(result.gate[0]).toMatchObject({ phase: "negative", outcome: "clean", ok: false });
		expect(existsSync(join(skillsDir, "slugify"))).toBe(false);
		expect(installs).toEqual([]);
		expect(loadToolforgeLedger(toolforgeLedgerPath(agentDir)).records[0]).toMatchObject({ status: "rejected" });
	}, 60_000);

	it("rejects an implementation that does not satisfy its own exit test", async () => {
		const result = await publish({ source: 'def run(text):\n    return "nope"\n' });

		expect(result.status).toBe("rejected");
		expect(result.reason).toMatch(/positive run did not pass/);
		expect(result.gate.map((run) => [run.phase, run.ok])).toEqual([
			["negative", true],
			["positive", false],
		]);
		expect(existsSync(join(skillsDir, "slugify"))).toBe(false);
	}, 60_000);

	it("runs the exit test in the inherited environment: variables and the user's PYTHONPATH reach it", async () => {
		const saved = { probe: process.env.PRIME_AGENT_TOOLFORGE_GATE_PROBE, pythonPath: process.env.PYTHONPATH };
		const userRoot = mkdtempSync(join(tmpdir(), "pi-toolforge-user-path-"));
		writeFileSync(join(userRoot, "prime_agent_user_only_helper.py"), 'EXPECTED = "a-b"\n', "utf-8");
		process.env.PRIME_AGENT_TOOLFORGE_GATE_PROBE = "inherited";
		process.env.PYTHONPATH = userRoot;
		try {
			const result = await publish({
				exitTest: [
					"import os",
					"import prime_agent_user_only_helper",
					"import slugify",
					"",
					'assert os.environ.get("PRIME_AGENT_TOOLFORGE_GATE_PROBE") == "inherited", sorted(os.environ)',
					'assert slugify.run("A B") == prime_agent_user_only_helper.EXPECTED',
					"",
				].join("\n"),
			});
			expect(result.reason).toBeUndefined();
			expect(result.status).toBe("published");
			expect(result.gate.map((run) => [run.phase, run.outcome, run.ok])).toEqual([
				["negative", "raised", true],
				["positive", "clean", true],
			]);
		} finally {
			if (saved.probe === undefined) delete process.env.PRIME_AGENT_TOOLFORGE_GATE_PROBE;
			else process.env.PRIME_AGENT_TOOLFORGE_GATE_PROBE = saved.probe;
			if (saved.pythonPath === undefined) delete process.env.PYTHONPATH;
			else process.env.PYTHONPATH = saved.pythonPath;
			rmSync(userRoot, { recursive: true, force: true });
		}
	}, 60_000);

	it("rejects a shadowing name before spawning anything", async () => {
		const result = await publish({ name: "json" });

		expect(result.status).toBe("rejected");
		expect(result.reason).toMatch(/collides with a Python builtin/);
		expect(result.gate).toEqual([]);
		expect(existsSync(skillsDir)).toBe(false);
	});

	it("replaces a previously published package and bumps its version", async () => {
		expect((await publish()).status).toBe("published");
		const second = await publish({ source: `${SLUGIFY_SOURCE}\n\nMARKER = "second"\n` });

		expect(second.status).toBe("published");
		expect(second.version).toBe(2);
		expect(readFileSync(join(skillsDir, "slugify", "src", "slugify", "__init__.py"), "utf-8")).toContain(
			'MARKER = "second"',
		);
	}, 90_000);
});

const describeIfKernel = python ? describe : describe.skip;

describeIfKernel("toolforge publish reaches the live kernel and the next session", { tags: ["kernel-heavy"] }, () => {
	let agentDir: string;
	let skillsDir: string;
	let workDir: string;
	let provisioner: IpythonKernelProvisioner | undefined;
	const installs: string[] = [];

	beforeAll(() => {
		agentDir = mkdtempSync(join(tmpdir(), "pi-toolforge-kernel-"));
		skillsDir = join(agentDir, "skills");
		workDir = mkdtempSync(join(tmpdir(), "pi-toolforge-cwd-"));
	});

	afterAll(async () => {
		await provisioner?.dispose({ snapshot: false });
		provisioner = undefined;
		rmSync(agentDir, { recursive: true, force: true });
		rmSync(workDir, { recursive: true, force: true });
	});

	it("session A: the published name is callable in the same cell that published it", async () => {
		provisioner = new IpythonKernelProvisioner(workDir, {
			python: python as string,
			env: { PYTHONPATH: RUNTIME_SRC },
			sessionId: "toolforge-session-a",
			toolforge: {
				skillsDir,
				ledgerPath: toolforgeLedgerPath(agentDir),
				stagingDir: join(agentDir, "toolforge", "staging"),
				pythonPath: python as string,
				installPackage: recordingInstaller(installs),
			},
		});

		const manager = await provisioner.ensure();
		const cell = await manager.execute(`
import json
_published = await rlm.toolforge.publish(
    "slugify",
    ${JSON.stringify(SLUGIFY_SOURCE)},
    ${JSON.stringify(SLUGIFY_DOC)},
    ${JSON.stringify(SLUGIFY_EXIT_TEST)},
)
print(json.dumps({
    "same_cell": slugify.run("A B") == "a-b",
    "import_name": _published.import_name,
    "version": _published.version,
    "gate": [[run.phase, run.outcome, run.ok] for run in _published.gate],
}, sort_keys=True))
`);

		expect(cell.status, `${cell.stderr}\n${cell.error?.traceback.join("\n") ?? ""}`).toBe("ok");
		expect(JSON.parse(cell.stdout.trim())).toEqual({
			same_cell: true,
			import_name: "slugify",
			version: 1,
			gate: [
				["negative", "raised", true],
				["positive", "clean", true],
			],
		});
		expect(installs).toEqual([join(skillsDir, "slugify")]);

		const rejected = await manager.execute(`
try:
    await rlm.toolforge.publish("json", ${JSON.stringify(SLUGIFY_SOURCE)}, "doc", ${JSON.stringify(SLUGIFY_EXIT_TEST)})
except rlm.toolforge.ToolforgeRejected as error:
    print("rejected:", error.reason)
`);
		expect(rejected.status).toBe("ok");
		expect(rejected.stdout).toContain("collides with a Python builtin");
	}, 180_000);

	it("session B: a new kernel with no snapshot binds it with no human step", async () => {
		const packageRoot = join(skillsDir, "slugify");
		expect(existsSync(packageRoot), "session A must have promoted the package").toBe(true);

		const { skills } = loadSkills({ cwd: workDir, agentDir, skillPaths: [], includeDefaults: true });
		const pythonSkills = getPythonSkillRuntimeInfo(skills);
		expect(pythonSkills.map((skill) => skill.importName)).toContain("slugify");

		const manager = new ReplKernelManager({
			python: python as string,
			cwd: workDir,
			sessionId: "toolforge-session-b",
			// Stands in for the editable install the test installer skipped; the real
			// installer puts the same package on the same interpreter's import path.
			env: { PYTHONPATH: [RUNTIME_SRC, join(packageRoot, "src")].join(":") },
			pythonSkills,
		});
		try {
			await manager.start();
			const bootstrap = await manager.execute(buildRlmBootstrapCode(pythonSkills));
			expect(bootstrap.status, bootstrap.error?.traceback.join("\n") ?? "").toBe("ok");

			const result = await manager.execute('print(slugify.run("A B") == "a-b")');
			expect(result.status, result.error?.traceback.join("\n") ?? "").toBe("ok");
			expect(result.stdout.trim()).toBe("True");
		} finally {
			await manager.shutdown({ snapshot: false, drainHostRequests: true });
		}
	}, 180_000);

	it("a skill that blocks at import does not stop the kernel from starting", async () => {
		const blockingDir = mkdtempSync(join(tmpdir(), "pi-toolforge-blocking-"));
		const manager = new ReplKernelManager({
			python: python as string,
			cwd: workDir,
			sessionId: "toolforge-blocking",
			env: { PYTHONPATH: [RUNTIME_SRC, join(blockingDir, "src")].join(":") },
		});
		try {
			mkdirSync(join(blockingDir, "src", "blocker"), { recursive: true });
			writeFileSync(
				join(blockingDir, "src", "blocker", "__init__.py"),
				"import threading\n\nthreading.Event().wait()\n\n\ndef run():\n    return 1\n",
				"utf-8",
			);
			await manager.start();
			const bootstrap = await manager.execute(
				buildRlmBootstrapCode([
					{
						name: "blocker",
						importName: "blocker",
						packagePath: join(blockingDir, "src", "blocker"),
						pyprojectPath: join(blockingDir, "pyproject.toml"),
					},
				]),
			);
			expect(bootstrap.status, bootstrap.error?.traceback.join("\n") ?? "").toBe("ok");
			const bound = await manager.execute("print(type(blocker).__name__)");
			expect(bound.stdout.trim()).toBe("_PrimeAgentLazySkill");
		} finally {
			await manager.shutdown({ snapshot: false, drainHostRequests: true });
			rmSync(blockingDir, { recursive: true, force: true });
		}
	}, 60_000);
});
