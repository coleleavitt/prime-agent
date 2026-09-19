import { spawn, spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import type { ToolResultMessage, Usage } from "@earendil-works/pi-ai";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import {
	ORPHAN_PROCESS_JOURNAL_ENV,
	readActiveOrphanProcesses,
	reapOrphanProcesses,
} from "../src/core/orphan-process-journal.js";
import { authorizeAssistedRavo } from "../src/core/ravo/authority.js";
import {
	applyReplayVerifications,
	extractFailures,
	type FailureLedger,
	type FailureRecord,
	failureOpponentId,
	fingerprintFailure,
	updateFailureLedger,
} from "../src/core/ravo/failure-ledger.js";
import type { JsonValue } from "../src/core/ravo/reducer.js";
import {
	deriveReplayCase,
	failureOpponentPassed,
	REPLAY_MODULE_DENYLIST,
	type RefereeVerdict,
	type RefereeVerdictStatus,
	type ReplayCase,
	refereeOpponentId,
	refereeOpponentPassed,
	refereeVerdictIsEvidence,
	replayAppliesToSkillImports,
	replayCaseOf,
	replayProbeOf,
	skillImportsOf,
	verdictFromOutcome,
} from "../src/core/ravo/referee.js";
import {
	adjudicateFailureClaims,
	captureReplayCase,
	runReplayCase,
	verifyObservedReplayCases,
} from "../src/core/ravo/referee-runner.js";
import {
	type RavoRunRequest,
	RavoRunService,
	type RavoRunServiceDeps,
	type RavoRunStatus,
} from "../src/core/ravo/run-service.js";
import type { HarnessState, RefinementProposal } from "../src/core/refinement/refinement.js";
import { screenRefinementProposal } from "../src/core/refinement/skill-dry-run.js";
import type { RunAgentHandler, RunAgentResult } from "../src/core/run-agent.js";
import { saveToolforgeLedger, toolforgeLedgerPath, toolforgeSrcRoots } from "../src/core/toolforge/ledger.js";

function resolvePython(): string | undefined {
	for (const candidate of ["python3", "python"]) {
		const probe = spawnSync(candidate, ["-c", "import sys; sys.stdout.write(sys.executable)"], { encoding: "utf8" });
		if (probe.status === 0 && probe.stdout.trim()) return probe.stdout.trim();
	}
	return undefined;
}

const python = resolvePython();

/**
 * The plan's exit test names `import paramiko`. Some machines (this one) have
 * paramiko installed, and the case has to actually raise for the "flaw upheld"
 * half to mean anything, so the first name the interpreter cannot import wins.
 */
function resolveMissingModule(interpreter: string): string {
	for (const candidate of ["paramiko", "paramiko_missing_dep"]) {
		const probe = spawnSync(interpreter, ["-I", "-c", `import ${candidate}`], { encoding: "utf8" });
		if (probe.status !== 0) return candidate;
	}
	throw new Error("no importable-free module name available for the replay case");
}

const REFEREE_RUNNER_MODULE = path.resolve(__dirname, "../src/core/ravo/referee-runner.js");
const TSX_TSCONFIG_PATH = path.resolve(__dirname, "../../../tsconfig.json");

/**
 * Child-process fixture: one replay that never ends on its own, then the exit or signal the scenario names.
 * `exit-cwd` runs it in a caller's working directory instead of a temporary one of its own.
 */
const SIGNAL_FIXTURE = `
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { runReplayCase } from ${JSON.stringify(REFEREE_RUNNER_MODULE)};

const [, , root, mode, python] = process.argv;
// Listeners the import graph registered (signal-exit) stay for SIGTERM; for SIGINT the replay's listener is left alone.
if (mode === "default-sigint") process.removeAllListeners("SIGINT");
if (mode === "handled-sigterm") process.on("SIGTERM", () => writeFileSync(root + "/handled", "1"));
if (mode === "exit" || mode === "exit-cwd") {
	setInterval(() => {
		if (existsSync(root + "/ready")) process.exit(7);
	}, 20);
}
if (mode === "exit-cwd") mkdirSync(root + "/caller-cwd");
void runReplayCase(
	{ language: "python", source: "import prime_agent_signal_probe", exceptionClass: "ModuleNotFoundError" },
	{
		pythonPath: python,
		sysPath: [root],
		timeoutMs: 60000,
		...(mode === "exit-cwd" ? { cwd: root + "/caller-cwd" } : {}),
	},
).then((outcome) => writeFileSync(root + "/outcome.json", JSON.stringify(outcome)));
`;

let missingModule = "paramiko";
let stubRoot = "";
const cleanup: Array<() => void> = [];
const previousEnv = {
	kernelPython: process.env.PRIME_AGENT_KERNEL_PYTHON,
	pythonPath: process.env.PYTHONPATH,
	agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
};

beforeAll(async () => {
	if (!python) return;
	missingModule = resolveMissingModule(python);
	stubRoot = await mkdtemp(path.join(tmpdir(), "referee-stub-"));
	await writeFile(path.join(stubRoot, `${missingModule}.py`), "VERSION = '1.0'\n", "utf8");
	process.env.PRIME_AGENT_KERNEL_PYTHON = python;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = await mkdtemp(path.join(tmpdir(), "referee-agent-"));
});

afterEach(() => {
	while (cleanup.length > 0) cleanup.pop()?.();
	if (previousEnv.pythonPath === undefined) delete process.env.PYTHONPATH;
	else process.env.PYTHONPATH = previousEnv.pythonPath;
});

afterAll(() => {
	if (previousEnv.kernelPython === undefined) delete process.env.PRIME_AGENT_KERNEL_PYTHON;
	else process.env.PRIME_AGENT_KERNEL_PYTHON = previousEnv.kernelPython;
	if (previousEnv.agentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousEnv.agentDir;
});

function tracebackLines(module: string): string[] {
	return [
		"Traceback (most recent call last):",
		'  File "<ipython-input-3>", line 1, in <module>',
		`    import ${module}`,
		`ModuleNotFoundError: No module named '${module}'`,
	];
}

function tracebackText(module: string): string {
	return ["Executing cell...", ...tracebackLines(module), ""].join("\n");
}

/** What the ipython tool returns for a cell that raised: the kernel's own traceback is in `details.error`. */
function ipythonErrorResult(module: string, overrides: Partial<ToolResultMessage> = {}): ToolResultMessage {
	return {
		role: "toolResult",
		toolCallId: "call-1",
		toolName: "ipython",
		content: [{ type: "text", text: tracebackText(module) }],
		details: {
			status: "error",
			errorEname: "ModuleNotFoundError",
			error: {
				ename: "ModuleNotFoundError",
				evalue: `No module named '${module}'`,
				traceback: tracebackLines(module).map((line) => `${line}\n`),
			},
		},
		isError: false,
		timestamp: 1,
		...overrides,
	};
}

function missingModuleFingerprint(module: string) {
	return fingerprintFailure("python_exception", "ipython", "ModuleNotFoundError", `No module named '${module}'`);
}

function verifiedCase(module: string): ReplayCase {
	return {
		language: "python",
		source: `import ${module}`,
		exceptionClass: "ModuleNotFoundError",
		verifiedAt: "2026-09-15T00:00:00.000Z",
	};
}

function recurringRecord(module: string, replayCases: ReplayCase | readonly ReplayCase[] | undefined): FailureRecord {
	const cases = replayCases === undefined ? [] : Array.isArray(replayCases) ? [...replayCases] : [replayCases];
	return {
		fingerprint: missingModuleFingerprint(module),
		count: 3,
		firstSeenTurn: 2,
		lastSeenTurn: 6,
		firstSeenAt: "2026-09-15T00:00:00.000Z",
		lastSeenAt: "2026-09-15T00:05:00.000Z",
		excerpt: `ModuleNotFoundError: No module named '${module}'`,
		addressedByProposalIds: [],
		...(cases.length === 0 ? {} : { replayCases: cases }),
	};
}

function verdict(status: RefereeVerdictStatus): RefereeVerdict {
	return { fingerprintId: "f", status, detail: status };
}

function skillWrite(module: string, action: "create" | "update" = "create") {
	return {
		action,
		kind: "skill",
		title: module,
		content: `Call ${module}.`,
		arguments: {},
		reference: { type: "python", import: module, callable: "run" },
	};
}

describe("referee opponent predicates", () => {
	const statuses: Array<RefereeVerdictStatus | undefined> = [
		undefined,
		"not_applicable",
		"cleared",
		"no_evidence",
		"upheld",
		"unverifiable",
	];

	it("failure:<fp> passes only a claim that no replay refutes, and fails closed without expected evidence", () => {
		const table = statuses.map((status) => [
			status ?? "none",
			failureOpponentPassed(true, status && verdict(status)),
			failureOpponentPassed(false, status && verdict(status)),
		]);
		expect(table).toEqual([
			["none", true, false],
			["not_applicable", true, false],
			["cleared", true, false],
			["no_evidence", false, false],
			["upheld", false, false],
			["unverifiable", false, false],
		]);
	});

	it("referee:<fp> charges nothing unclaimed, and fails a claimed no_evidence, upheld, or unverifiable verdict", () => {
		const table = statuses.map((status) => [
			status ?? "none",
			refereeOpponentPassed(true, status && verdict(status)),
			refereeOpponentPassed(false, status && verdict(status)),
		]);
		expect(table).toEqual([
			["none", true, true],
			["not_applicable", true, true],
			["cleared", true, true],
			["no_evidence", false, true],
			["upheld", false, true],
			["unverifiable", false, true],
		]);
	});

	it("admits only adjudicated verdicts to the referee pool", () => {
		expect(statuses.filter((status) => refereeVerdictIsEvidence(status && verdict(status)))).toEqual([
			"cleared",
			"upheld",
			"unverifiable",
		]);
	});

	it("reads skill imports from skill writes only", () => {
		expect(skillImportsOf([{ action: "create", kind: "memory" }])).toEqual([]);
		expect(skillImportsOf([{ ...skillWrite("paramiko"), action: "delete" }])).toEqual([]);
		expect(
			skillImportsOf([
				{ action: "update", kind: "prompt" },
				skillWrite("pkg.sub", "update"),
				{ ...skillWrite("ignored"), reference: { type: "shell", import: "ignored" } },
				{ ...skillWrite("legacy"), reference: { type: "python", python_import: " legacy.mod ", callable: "x" } },
				{ ...skillWrite("bad"), reference: { type: "python", import: "not a module" } },
				skillWrite("pkg.sub"),
			]),
		).toEqual(["pkg.sub", "legacy.mod"]);
		expect(skillImportsOf([])).toEqual([]);
	});

	it("applies only missing-module and missing-distribution probes that name a skill import", () => {
		const applies = (source: string, imports: string[]) => replayAppliesToSkillImports({ source }, imports);
		expect(applies("import paho.mqtt", ["paho"])).toBe(true);
		expect(applies("import paho", ["paho.mqtt.client"])).toBe(true);
		expect(applies("import pahox", ["paho"])).toBe(false);
		// A name probe is retired: when X imports, a skill cannot make `from X import n` succeed.
		expect(applies("from pkg.sub import thing", ["pkg"])).toBe(false);
		expect(applies("from json import load_string", ["json"])).toBe(false);
		expect(applies('import importlib.metadata\nimportlib.metadata.version("paho-mqtt")', ["paho.mqtt.client"])).toBe(
			true,
		);
		expect(
			applies('import importlib.metadata\nimportlib.metadata.version("requests_toolbelt")', ["requests_toolbelt"]),
		).toBe(true);
		expect(applies('import importlib.metadata\nimportlib.metadata.version("pyyaml")', ["yaml"])).toBe(false);
		// A skill registers a reference: it changes no attribute and installs no program.
		expect(applies('import json\ngetattr(json, "load_string")', ["json"])).toBe(false);
		expect(
			applies('import shutil\nif shutil.which("rg") is None:\n    raise FileNotFoundError("rg")', ["shutil"]),
		).toBe(false);
		// A source that is not exactly a rendered probe never applies.
		expect(applies("import paramiko; import os", ["paramiko"])).toBe(false);
		expect(applies("import paramiko", [])).toBe(false);
	});
});

describe("environment replay derivation", () => {
	const fingerprint = (exceptionClass: string, message: string) =>
		fingerprintFailure("python_exception", "ipython", exceptionClass, message);

	it("probes a missing distribution through importlib.metadata", () => {
		const modern = deriveReplayCase(
			fingerprint("importlib.metadata.PackageNotFoundError", "No package metadata was found for paho-mqtt"),
			'  File "/usr/lib/python3.13/importlib/metadata/__init__.py", line 409, in from_name\nimportlib.metadata.PackageNotFoundError: No package metadata was found for paho-mqtt',
		);
		expect(modern).toEqual({
			language: "python",
			source: 'import importlib.metadata\nimportlib.metadata.version("paho-mqtt")',
			exceptionClass: "PackageNotFoundError",
		});
		const legacy = deriveReplayCase(
			fingerprint("PackageNotFoundError", "requests_toolbelt"),
			"importlib.metadata.PackageNotFoundError: requests_toolbelt",
		);
		expect(legacy?.source).toBe('import importlib.metadata\nimportlib.metadata.version("requests_toolbelt")');
	});

	it("derives no probe a skill write could never be adjudicated against: a missing attribute, name or program", () => {
		const fromSpawn = (frame: string, name: string) =>
			`  File "${frame}", line 1955, in _execute_child\nFileNotFoundError: [Errno 2] No such file or directory: '${name}'`;
		const missing = (name: string) =>
			fingerprint("FileNotFoundError", `[Errno 2] No such file or directory: '${name}'`);
		for (const name of ["rg", "python3.12", "release.2", "backup.2024", "v1.2", "config"]) {
			expect(
				deriveReplayCase(missing(name), fromSpawn("/usr/lib/python3.13/subprocess.py", name)),
				name,
			).toBeUndefined();
			expect(deriveReplayCase(missing(name), fromSpawn("/home/u/proj/subprocess.py", name)), name).toBeUndefined();
		}
		for (const [module, name] of [
			["json", "load_string"],
			["agent_observe", "recent"],
		]) {
			const excerpt = `AttributeError: module '${module}' has no attribute '${name}'`;
			expect(deriveReplayCase(fingerprint("AttributeError", excerpt.slice(16)), excerpt), module).toBeUndefined();
			const importName = `ImportError: cannot import name '${name}' from '${module}' (/usr/lib/python3.13/${module}/__init__.py)`;
			expect(deriveReplayCase(fingerprint("ImportError", importName.slice(13)), importName), module).toBeUndefined();
		}
		const lines = [
			"Traceback (most recent call last):",
			'  File "<ipython-input-2>", line 1, in <module>',
			"ImportError: cannot import name 'load_string' from 'json' (/usr/lib/python3.13/json/__init__.py)",
		];
		const cell: ToolResultMessage = {
			role: "toolResult",
			toolCallId: "call-1",
			toolName: "ipython",
			content: [{ type: "text", text: lines.join("\n") }],
			details: { status: "error", error: { ename: "ImportError", evalue: "", traceback: lines } },
			isError: false,
			timestamp: 1,
		};
		const [observed] = extractFailures([cell], { fromEntryIndex: 0, turn: 1 });
		expect(observed.fingerprint.exceptionClass).toBe("ImportError");
		expect(observed.replayCase).toBeUndefined();
	});

	it("derives nothing for file paths, user data, or API misuse", () => {
		const missingFile = fingerprint("FileNotFoundError", "[Errno 2] No such file or directory: '/home/u/repo'");
		expect(
			deriveReplayCase(
				missingFile,
				"  File \"<ipython-input-4>\", line 1, in <module>\nFileNotFoundError: [Errno 2] No such file or directory: '/home/u/repo'",
			),
		).toBeUndefined();
		expect(
			deriveReplayCase(
				missingFile,
				"  File \"/usr/lib/python3.13/subprocess.py\", line 1955, in _execute_child\nFileNotFoundError: [Errno 2] No such file or directory: './scripts/run.sh'",
			),
		).toBeUndefined();
		// A bare relative file name opened by user code is data, not an executable.
		expect(
			deriveReplayCase(
				missingFile,
				"  File \"<ipython-input-4>\", line 1, in <module>\nFileNotFoundError: [Errno 2] No such file or directory: 'notes.txt'",
			),
		).toBeUndefined();
		expect(
			deriveReplayCase(
				fingerprint("TypeError", "bash() got an unexpected keyword argument 'timeout'"),
				"TypeError: bash() got an unexpected keyword argument 'timeout'",
			),
		).toBeUndefined();
		expect(
			deriveReplayCase(
				fingerprint("NameError", "name 'agent_observe' is not defined"),
				"NameError: name 'agent_observe' is not defined",
			),
		).toBeUndefined();
	});

	it("refuses private module segments and modules that act on import", () => {
		const missing = (module: string) =>
			deriveReplayCase(
				fingerprint("ModuleNotFoundError", `No module named '${module}'`),
				`ModuleNotFoundError: No module named '${module}'`,
			);
		for (const module of ["venv.__main__", "pkg._private", "_socket", "antigravity", "idlelib.idle", "http.server"]) {
			expect(missing(module), module).toBeUndefined();
		}
		for (const module of REPLAY_MODULE_DENYLIST) {
			expect(missing(module), module).toBeUndefined();
			expect(missing(`${module}.sub`), module).toBeUndefined();
		}
		expect(
			deriveReplayCase(
				fingerprint("ImportError", "cannot import name '__main__' from 'venv'"),
				"ImportError: cannot import name '__main__' from 'pkg'",
			),
		).toBeUndefined();
		expect(
			deriveReplayCase(
				fingerprint("AttributeError", "module 'webbrowser' has no attribute 'x'"),
				"AttributeError: module 'webbrowser' has no attribute 'x'",
			),
		).toBeUndefined();
		expect(missing("paramiko.transport")?.source).toBe("import paramiko.transport");
	});

	it("recognizes only exactly rendered, valid probes as runnable", () => {
		expect(replayProbeOf({ source: "import paramiko" })).toEqual({ kind: "module", module: "paramiko" });
		expect(replayProbeOf({ source: 'import importlib.metadata\nimportlib.metadata.version("paho-mqtt")' })).toEqual({
			kind: "distribution",
			distribution: "paho-mqtt",
		});
		for (const source of [
			// Retired probe kinds, as a ledger written before they were retired stores them.
			'import json\ngetattr(json, "loads")',
			"from paho import mqtt",
			"from json import load_string",
			'import shutil\nif shutil.which("rg") is None:\n    raise FileNotFoundError("rg")',
			"import venv.__main__",
			"import antigravity",
			"import os; os.system('true')",
			"import os\nopen('x', 'w')",
			'import json\ngetattr(os, "loads")',
			'import shutil\nif shutil.which("a") is None:\n    raise FileNotFoundError("b")',
			'import shutil\nif shutil.which("notes.txt") is None:\n    raise FileNotFoundError("notes.txt")',
			"raise ValueError('changed')",
		]) {
			expect(replayProbeOf({ source }), source).toBeUndefined();
		}
	});

	it("reads legacy and current case lists and prefers the newest verified case as evidence", () => {
		const record = recurringRecord("paramiko", undefined);
		const unverified: ReplayCase = { language: "python", source: "import a" };
		const older: ReplayCase = { ...unverified, source: "import b", verifiedAt: "2026-09-15T00:00:00.000Z" };
		const newer: ReplayCase = { ...unverified, source: "import c", verifiedAt: "2026-09-15T01:00:00.000Z" };
		expect(replayCaseOf({ ...record, replayCase: unverified })).toBeUndefined();
		expect(replayCaseOf({ ...record, replayCase: older })).toEqual(older);
		expect(replayCaseOf({ ...record, replayCases: [older, newer, unverified] })).toEqual(newer);
		const attribute: ReplayCase = {
			language: "python",
			source: 'import json\ngetattr(json, "load_string")',
			verifiedAt: "2026-09-15T02:00:00.000Z",
		};
		expect(replayCaseOf({ ...record, replayCases: [older, attribute] })).toEqual(older);
		expect(replayCaseOf({ ...record, replayCases: [attribute] })).toBeUndefined();
		const name: ReplayCase = {
			language: "python",
			source: "from json import load_string",
			exceptionClass: "ImportError",
			verifiedAt: "2026-09-15T03:00:00.000Z",
		};
		expect(replayCaseOf({ ...record, replayCases: [older, name] })).toEqual(older);
		expect(replayCaseOf({ ...record, replayCases: [name] })).toBeUndefined();
	});
});

describe("replay derivation source", () => {
	it("derives a case only from the kernel's own traceback of an ipython cell that raised", () => {
		const [kernel] = extractFailures([ipythonErrorResult("paramiko")], { fromEntryIndex: 0, turn: 1 });
		expect(kernel.replayCase).toEqual({
			language: "python",
			source: "import paramiko",
			exceptionClass: "ModuleNotFoundError",
		});
	});

	it("still fingerprints a traceback other tools return, but derives no case from it", () => {
		const fetched = extractFailures(
			[
				{
					role: "toolResult",
					toolCallId: "call-1",
					toolName: "web_fetch",
					content: [{ type: "text", text: tracebackText("paramiko") }],
					details: {
						status: "error",
						error: { ename: "ModuleNotFoundError", traceback: tracebackLines("paramiko") },
					},
					isError: false,
					timestamp: 1,
				},
			],
			{ fromEntryIndex: 0, turn: 1 },
		);
		expect(fetched).toHaveLength(1);
		expect(fetched[0].fingerprint.exceptionClass).toBe("ModuleNotFoundError");
		expect(fetched[0].replayCase).toBeUndefined();
	});

	it("derives nothing from a traceback an ipython cell merely printed", () => {
		const printed = ipythonErrorResult("paramiko", {
			details: { status: "ok", stdout: tracebackText("paramiko") },
		});
		const [observation] = extractFailures([printed], { fromEntryIndex: 0, turn: 1 });
		expect(observation.fingerprint.exceptionClass).toBe("ModuleNotFoundError");
		expect(observation.replayCase).toBeUndefined();

		// Output that ends in a crafted traceback names nothing a case imports: the case comes from the kernel's error.
		const sameFingerprint = ipythonErrorResult("paramiko", {
			content: [{ type: "text", text: `${tracebackText("paramiko")}\n${tracebackText("venv_payload")}` }],
		});
		const [kernelOnly] = extractFailures([sameFingerprint], { fromEntryIndex: 0, turn: 1 });
		expect(kernelOnly.fingerprint.id).toBe(missingModuleFingerprint("venv_payload").id);
		expect(kernelOnly.replayCase?.source).toBe("import paramiko");
		const otherFailure = ipythonErrorResult("paramiko", {
			content: [
				{
					type: "text",
					text: `${tracebackText("paramiko")}\nTraceback (most recent call last):\n  File "x", line 1\nAttributeError: module 'webbrowser' has no attribute 'open_new'\n`,
				},
			],
		});
		const [mismatch] = extractFailures([otherFailure], { fromEntryIndex: 0, turn: 1 });
		expect(mismatch.fingerprint.exceptionClass).toBe("AttributeError");
		expect(mismatch.replayCase).toBeUndefined();

		// The kernel reported a different exception class than the traceback names.
		const wrongName = ipythonErrorResult("paramiko");
		const details = wrongName.details as { error: { ename: string } };
		details.error.ename = "ImportError";
		expect(extractFailures([wrongName], { fromEntryIndex: 0, turn: 1 })[0].replayCase).toBeUndefined();
	});
});

describe("adjudication without an applicable skill import", () => {
	it("returns not_applicable for every claimed fingerprint without running anything", async () => {
		const refuted = recurringRecord("paramiko", verifiedCase("paramiko"));
		const blind = { ...recurringRecord("other_missing", undefined), fingerprint: missingModuleFingerprint("other") };
		// An interpreter that does not exist: any run would come back unverifiable.
		for (const skillImports of [[], ["json"], ["paramikox"]]) {
			const verdicts = await adjudicateFailureClaims(
				[refuted, blind],
				[refuted.fingerprint.id, blind.fingerprint.id],
				{ skillImports, pythonPath: "/nonexistent/prime-agent-referee-python" },
			);
			expect(verdicts.map((item) => [item.fingerprintId, item.status])).toEqual([
				[refuted.fingerprint.id, "not_applicable"],
				[blind.fingerprint.id, "not_applicable"],
			]);
			expect(verdicts.every((item) => failureOpponentPassed(true, item) && refereeOpponentPassed(true, item))).toBe(
				true,
			);
		}
	});

	it("never replays an attribute or name probe for a skill write, even one importing the probed module", async () => {
		const verifiedAt = "2026-09-15T00:00:00.000Z";
		const attribute: ReplayCase = {
			language: "python",
			source: 'import json\ngetattr(json, "load_string")',
			exceptionClass: "AttributeError",
			verifiedAt,
		};
		const name: ReplayCase = {
			language: "python",
			source: "from json import load_string",
			exceptionClass: "ImportError",
			verifiedAt,
		};
		for (const replay of [attribute, name]) {
			const record = recurringRecord("json", replay);
			const [result] = await adjudicateFailureClaims([record], [record.fingerprint.id], {
				skillImports: skillImportsOf([
					{ ...skillWrite("json"), reference: { type: "python", import: "json", callable: "loads" } },
				]),
				pythonPath: "/nonexistent/prime-agent-referee-python",
			});
			expect(result.status, replay.source).toBe("not_applicable");
			expect(result.detail, replay.source).toContain("no replay case derivable");
			expect(failureOpponentPassed(true, result) && refereeOpponentPassed(true, result)).toBe(true);
			expect(refereeVerdictIsEvidence(result)).toBe(false);
		}
	});
});

function ledgerOf(record: FailureRecord): FailureLedger {
	return { schema: 1, lastScannedEntryIndex: 3, failures: { [record.fingerprint.id]: record } };
}

describe.skipIf(!python)("referee replay cases", () => {
	it("derives an executable case from a missing-module traceback and stores it on the ledger record", () => {
		const fingerprint = missingModuleFingerprint("paramiko");
		const derived = deriveReplayCase(fingerprint, "ModuleNotFoundError: No module named 'paramiko'");
		expect(derived).toEqual({ language: "python", source: "import paramiko", exceptionClass: "ModuleNotFoundError" });
		expect(derived?.verifiedAt).toBeUndefined();

		const observations = extractFailures([ipythonErrorResult("paramiko")], { fromEntryIndex: 0, turn: 3 });
		expect(observations[0]?.replayCase?.source).toBe("import paramiko");
		const ledger = updateFailureLedger({ schema: 1, failures: {}, lastScannedEntryIndex: 0 }, observations).ledger;
		expect(ledger.failures[observations[0].fingerprint.id].replayCases?.map((item) => item.source)).toEqual([
			"import paramiko",
		]);
	});

	it("derives nothing for a failure with no side-effect-free reproduction", () => {
		const fingerprint = fingerprintFailure("python_exception", "ipython", "KeyError", "'results'");
		expect(deriveReplayCase(fingerprint, "KeyError: 'results'")).toBeUndefined();
		expect(deriveReplayCase({ ...fingerprint, kind: "tool_error" }, "No module named 'paramiko'")).toBeUndefined();
	});

	it("re-executes the case in a subprocess: raises without the module, clean with a scratch root on sys.path", async () => {
		const replay = verifiedCase(missingModule);
		const raised = await runReplayCase(replay);
		expect(raised).toMatchObject({ kind: "raised", exceptionClass: "ModuleNotFoundError" });
		expect(verdictFromOutcome(replay, raised)).toBe("upheld");

		const clean = await runReplayCase(replay, { sysPath: [stubRoot] });
		expect(clean.kind).toBe("clean");
		expect(verdictFromOutcome(replay, clean)).toBe("cleared");

		// The inherited PYTHONPATH is not the case's environment: only explicit roots are.
		process.env.PYTHONPATH = stubRoot;
		expect(await runReplayCase(replay)).toMatchObject({ kind: "raised", exceptionClass: "ModuleNotFoundError" });
	});

	it("keeps a captured case only when the self-check reproduces the recorded exception", async () => {
		const [observation] = extractFailures([ipythonErrorResult(missingModule)], { fromEntryIndex: 0, turn: 1 });
		const captured = await captureReplayCase(observation, { now: () => "2026-09-15T12:00:00.000Z" });
		expect(captured).toMatchObject({ source: `import ${missingModule}`, verifiedAt: "2026-09-15T12:00:00.000Z" });
		// The same derivation against an environment where the import works never
		// reproduced, so it is not recorded as evidence.
		expect(await captureReplayCase(observation, { sysPath: [stubRoot] })).toBeUndefined();
		// It never derives a case of its own from an excerpt.
		expect(await captureReplayCase({ ...observation, replayCase: undefined })).toBeUndefined();
	});

	it("fails closed when the verification cannot run, or when an applicable case never reproduced", async () => {
		const replay = verifiedCase(missingModule);
		const outcome = await runReplayCase(replay, { pythonPath: path.join(stubRoot, "no-such-python") });
		expect(outcome.kind).toBe("unrunnable");
		const unverifiable = verdictFromOutcome(replay, outcome);
		expect(unverifiable).toBe("unverifiable");
		const unrunnable = { fingerprintId: "f", status: unverifiable, detail: outcome.detail };
		expect(failureOpponentPassed(true, unrunnable)).toBe(false);
		expect(refereeOpponentPassed(true, unrunnable)).toBe(false);

		// A case that never reproduced is not an oracle, so a clean run of it
		// says nothing; the evidence the gate expects is missing.
		const unverified: ReplayCase = { language: "python", source: `import ${missingModule}` };
		expect(verdictFromOutcome(unverified, { kind: "clean", detail: "" })).toBe("no_evidence");
		const neverReproduced = recurringRecord(missingModule, unverified);
		const [noEvidence] = await adjudicateFailureClaims([neverReproduced], [neverReproduced.fingerprint.id], {
			skillImports: [missingModule],
		});
		expect(noEvidence.status).toBe("no_evidence");
		expect(failureOpponentPassed(true, noEvidence)).toBe(false);
		expect(refereeOpponentPassed(true, noEvidence)).toBe(false);

		// No derivable case at all: nothing a replay could say, so the claim stands.
		const blind = recurringRecord(missingModule, undefined);
		const [notApplicable] = await adjudicateFailureClaims([blind], [blind.fingerprint.id], {
			skillImports: [missingModule],
		});
		expect(notApplicable.status).toBe("not_applicable");
		expect(failureOpponentPassed(true, notApplicable)).toBe(true);
		expect(failureOpponentPassed(false, notApplicable)).toBe(false);
	});

	it("adjudicates only the fingerprints the proposal claims", async () => {
		const record = recurringRecord(missingModule, verifiedCase(missingModule));
		expect(await adjudicateFailureClaims([record], [], { skillImports: [missingModule] })).toEqual([]);
		const [adjudicated] = await adjudicateFailureClaims([record], [record.fingerprint.id], {
			skillImports: [missingModule],
		});
		expect(adjudicated).toMatchObject({ fingerprintId: record.fingerprint.id, status: "upheld" });
	});

	it("runs only the applicable verified cases: any reproduction upholds, a stray class is unverifiable, only all-clean clears", async () => {
		const stamp = "2026-09-15T00:00:00.000Z";
		const reproduces = verifiedCase(missingModule);
		const clean: ReplayCase = { ...reproduces, source: "import os" };
		const strayClass: ReplayCase = {
			...reproduces,
			source: `import ${missingModule}_stray`,
			exceptionClass: "AttributeError",
		};
		const unverifiedRaise: ReplayCase = { language: "python", source: `import ${missingModule}_other` };
		const adjudicate = async (cases: ReplayCase[], skillImports: string[]) => {
			const record = recurringRecord(missingModule, cases);
			const [result] = await adjudicateFailureClaims([record], [record.fingerprint.id], { skillImports });
			return result.status;
		};
		const everything = [missingModule, `${missingModule}_other`, `${missingModule}_stray`, "os", "json", "sys"];

		expect(await adjudicate([clean, reproduces], everything)).toBe("upheld");
		expect(await adjudicate([strayClass, reproduces], everything)).toBe("upheld");
		expect(await adjudicate([clean, strayClass], everything)).toBe("unverifiable");
		expect(await adjudicate([clean, { ...clean, source: "import sys", verifiedAt: stamp }], everything)).toBe(
			"cleared",
		);
		// Unverified cases are not evidence and never run, even when they would raise.
		expect(await adjudicate([clean, unverifiedRaise], everything)).toBe("cleared");
		// A reproducing case no skill import names is not run at all.
		expect(await adjudicate([clean, reproduces], ["os"])).toBe("cleared");
		// A source that is not a valid probe never runs, whatever it would do.
		expect(await adjudicate([clean, { ...reproduces, source: "raise ModuleNotFoundError('x')" }], everything)).toBe(
			"cleared",
		);
	});

	it("verifies observed cases once each and merges the verification into the ledger", async () => {
		const observations = extractFailures(
			[ipythonErrorResult(missingModule), ipythonErrorResult(missingModule, { toolCallId: "call-2", timestamp: 2 })],
			{ fromEntryIndex: 0, turn: 1 },
		);
		expect(observations).toHaveLength(2);
		const fingerprintId = observations[0].fingerprint.id;
		const ledger = updateFailureLedger({ schema: 1, failures: {}, lastScannedEntryIndex: 0 }, observations).ledger;
		expect(replayCaseOf(ledger.failures[fingerprintId])).toBeUndefined();

		const verifications = await verifyObservedReplayCases(observations, { now: () => "2026-09-15T12:00:00.000Z" });
		expect(verifications).toEqual([
			{ fingerprintId, source: `import ${missingModule}`, verifiedAt: "2026-09-15T12:00:00.000Z" },
		]);
		const verified = applyReplayVerifications(ledger, verifications);
		expect(replayCaseOf(verified.failures[fingerprintId])).toMatchObject({
			source: `import ${missingModule}`,
			verifiedAt: "2026-09-15T12:00:00.000Z",
		});
		expect(replayCaseOf(ledger.failures[fingerprintId])).toBeUndefined();

		// Records work too, and a case that has already reproduced is not re-run.
		expect(await verifyObservedReplayCases(Object.values(verified.failures))).toEqual([]);
		// A derivation that does not reproduce in this environment is never marked.
		expect(await verifyObservedReplayCases(observations, { sysPath: [stubRoot] })).toEqual([]);
		// A stored source that is not a valid probe is never run, even though it would raise.
		const tampered = recurringRecord(missingModule, [
			{ language: "python", source: "raise ModuleNotFoundError('x')", exceptionClass: "ModuleNotFoundError" },
			{ language: "python", source: "import venv.__main__", exceptionClass: "ModuleNotFoundError" },
		]);
		expect(await verifyObservedReplayCases([tampered])).toEqual([]);
	});

	it("reproduces a missing-distribution derivation", async () => {
		const distribution = deriveReplayCase(
			fingerprintFailure("python_exception", "ipython", "PackageNotFoundError", "x"),
			"importlib.metadata.PackageNotFoundError: No package metadata was found for prime-agent-no-such-dist",
		);
		expect(distribution).toBeTruthy();
		if (!distribution) return;
		expect(verdictFromOutcome(distribution, await runReplayCase(distribution))).toBe("upheld");
	});

	it("never runs a stored case of a retired probe kind, even one that would reproduce", async () => {
		const attribute: ReplayCase = {
			language: "python",
			source: 'import json\ngetattr(json, "prime_agent_no_such_attribute")',
			exceptionClass: "AttributeError",
		};
		const executable: ReplayCase = {
			language: "python",
			source:
				'import shutil\nif shutil.which("prime-agent-no-such-exe") is None:\n    raise FileNotFoundError("prime-agent-no-such-exe")',
			exceptionClass: "FileNotFoundError",
		};
		const name: ReplayCase = {
			language: "python",
			source: "from json import prime_agent_no_such_name",
			exceptionClass: "ImportError",
		};
		// Run directly, all three would uphold: only the probe gate keeps them from running.
		expect(verdictFromOutcome(attribute, await runReplayCase(attribute))).toBe("upheld");
		expect(verdictFromOutcome(executable, await runReplayCase(executable))).toBe("upheld");
		expect(verdictFromOutcome(name, await runReplayCase(name))).toBe("upheld");

		const record = recurringRecord("json", [attribute, executable, name]);
		expect(await verifyObservedReplayCases([record])).toEqual([]);
		const observation = {
			fingerprint: record.fingerprint,
			excerpt: record.excerpt,
			entryIndex: 0,
			turn: 1,
			at: "t1",
		};
		expect(await captureReplayCase({ ...observation, replayCase: attribute })).toBeUndefined();
		expect(await captureReplayCase({ ...observation, replayCase: executable })).toBeUndefined();
		expect(await captureReplayCase({ ...observation, replayCase: name })).toBeUndefined();

		const verifiedAt = "2026-09-15T00:00:00.000Z";
		const legacy = recurringRecord("json", [
			{ ...attribute, verifiedAt },
			{ ...executable, verifiedAt },
			{ ...name, verifiedAt },
		]);
		const [verdict] = await adjudicateFailureClaims([legacy], [legacy.fingerprint.id], {
			skillImports: ["json", "shutil"],
			pythonPath: "/nonexistent/prime-agent-referee-python",
		});
		expect(verdict).toMatchObject({ status: "not_applicable" });
		expect(verdict.detail).toContain("no replay case derivable");
	});

	it("agrees with the skill dry-run about a module importable only through the inherited PYTHONPATH", async () => {
		const moduleName = "prime_agent_inherited_only_module";
		const root = await mkdtemp(path.join(tmpdir(), "referee-inherited-"));
		cleanup.push(() => rmSync(root, { recursive: true, force: true }));
		await writeFile(path.join(root, `${moduleName}.py`), "def run():\n    return 1\n", "utf8");
		const edits = [skillWrite(moduleName)];
		const proposal = { summary: "s", rationale: "r", expectedOutcome: "o", edits } as unknown as RefinementProposal;
		const record = recurringRecord(moduleName, verifiedCase(moduleName));
		const adjudicate = async () => {
			const [result] = await adjudicateFailureClaims([record], [record.fingerprint.id], {
				skillImports: skillImportsOf(edits),
				sysPath: toolforgeSrcRoots(),
			});
			return result.status;
		};
		const unverified = recurringRecord(moduleName, {
			language: "python",
			source: `import ${moduleName}`,
			exceptionClass: "ModuleNotFoundError",
		});

		process.env.PYTHONPATH = root;
		const screened = await screenRefinementProposal(proposal);
		expect(screened.dryRun.map((result) => [result.ok, result.detail.split(" in ")[0]])).toEqual([
			[true, `imported ${moduleName}.run`],
		]);
		expect(await adjudicate()).toBe("cleared");
		// A case derived from tool output is still self-checked without the host's PYTHONPATH.
		expect((await verifyObservedReplayCases([unverified])).map((item) => item.source)).toEqual([
			`import ${moduleName}`,
		]);

		delete process.env.PYTHONPATH;
		expect((await screenRefinementProposal(proposal)).dryRun.map((result) => result.ok)).toEqual([false]);
		expect(await adjudicate()).toBe("upheld");
	});
});

describe.skipIf(!python)("replay sandbox", () => {
	/** A module whose import records where and how it ran, and tries to write into its working directory. */
	async function probeModule(name: string, body: string[]): Promise<{ root: string; report: string }> {
		const root = await mkdtemp(path.join(tmpdir(), "referee-sandbox-"));
		const report = path.join(root, "report.json");
		await writeFile(path.join(root, `${name}.py`), [...body, ""].join("\n"), "utf8");
		cleanup.push(() => rmSync(root, { recursive: true, force: true }));
		return { root, report };
	}

	it("runs a verification in a fresh working directory that is removed, with a minimal environment", async () => {
		const marker = `prime-agent-replay-marker-${process.pid}-${Date.now()}`;
		const { root, report } = await probeModule("prime_agent_cwd_probe", [
			"import json, os, sys",
			`open(${JSON.stringify(marker)}, "w").write("written")`,
			"report = {'cwd': os.getcwd(), 'argv': sys.argv, 'env': sorted(os.environ), 'bytecode': sys.dont_write_bytecode, 'path': sys.path}",
			"json.dump(report, open(os.path.join(os.path.dirname(__file__), 'report.json'), 'w'))",
			"raise ModuleNotFoundError(\"No module named 'prime_agent_cwd_probe_missing'\")",
		]);
		cleanup.push(() => rmSync(path.join(process.cwd(), marker), { force: true }));
		process.env.PYTHONPATH = "/prime-agent-inherited-pythonpath";
		process.env.PYTHONSTARTUP = path.join(root, "startup.py");
		cleanup.push(() => {
			delete process.env.PYTHONSTARTUP;
		});

		const record = recurringRecord("prime_agent_cwd_probe", {
			language: "python",
			source: "import prime_agent_cwd_probe",
			exceptionClass: "ModuleNotFoundError",
		});
		const verifications = await verifyObservedReplayCases([record], { sysPath: [root] });
		expect(verifications.map((item) => item.source)).toEqual(["import prime_agent_cwd_probe"]);

		const ran = JSON.parse(readFileSync(report, "utf8")) as {
			cwd: string;
			argv: string[];
			env: string[];
			bytecode: boolean;
			path: string[];
		};
		expect(path.resolve(ran.cwd)).not.toBe(path.resolve(process.cwd()));
		expect(existsSync(ran.cwd)).toBe(false);
		expect(existsSync(path.join(process.cwd(), marker))).toBe(false);
		expect(ran.argv).toEqual(["-"]);
		expect(ran.bytecode).toBe(true);
		expect(existsSync(path.join(root, "__pycache__"))).toBe(false);
		expect(ran.env.filter((key) => key.startsWith("PYTHON"))).toEqual(["PYTHONPATH"]);
		expect(ran.env).not.toContain("PRIME_AGENT_KERNEL_PYTHON");
		expect(ran.env).not.toContain("PRIME_AGENT_CODING_AGENT_DIR");
		expect(ran.path).toContain(root);
		expect(ran.path).not.toContain("/prime-agent-inherited-pythonpath");
	});

	it("adjudicates in the sanitized environment with the host's PYTHONPATH entries as roots", async () => {
		const { root, report } = await probeModule("prime_agent_adjudication_probe", [
			"import json, os, sys",
			"json.dump({'env': sorted(os.environ), 'path': sys.path}, open(os.path.join(os.path.dirname(__file__), 'report.json'), 'w'))",
		]);
		process.env.PYTHONPATH = ["/prime-agent-inherited-first", root].join(path.delimiter);
		const record = recurringRecord("prime_agent_adjudication_probe", verifiedCase("prime_agent_adjudication_probe"));
		const [result] = await adjudicateFailureClaims([record], [record.fingerprint.id], {
			skillImports: ["prime_agent_adjudication_probe"],
		});
		expect(result.status).toBe("cleared");
		const ran = JSON.parse(readFileSync(report, "utf8")) as { env: string[]; path: string[] };
		expect(ran.env.filter((key) => key.startsWith("PYTHON"))).toEqual(["PYTHONPATH"]);
		expect(ran.env).not.toContain("PRIME_AGENT_KERNEL_PYTHON");
		expect(ran.env).not.toContain("PRIME_AGENT_CODING_AGENT_DIR");
		expect(ran.path.slice(0, 2)).toEqual(["/prime-agent-inherited-first", root]);
	});

	it("journals a replay group while it runs and clears the record when the run ends", async () => {
		const journal = path.join(await mkdtemp(path.join(tmpdir(), "referee-journal-")), "orphans.jsonl");
		cleanup.push(() => rmSync(path.dirname(journal), { recursive: true, force: true }));
		const previous = process.env[ORPHAN_PROCESS_JOURNAL_ENV];
		cleanup.push(() => {
			if (previous === undefined) delete process.env[ORPHAN_PROCESS_JOURNAL_ENV];
			else process.env[ORPHAN_PROCESS_JOURNAL_ENV] = previous;
		});
		process.env[ORPHAN_PROCESS_JOURNAL_ENV] = journal;
		const { root } = await probeModule("prime_agent_journal_probe", [
			"import os, time",
			"open(os.path.join(os.path.dirname(__file__), 'ready'), 'w').write(str(os.getpid()))",
			"time.sleep(30)",
		]);
		const controller = new AbortController();
		const running = runReplayCase(
			{ language: "python", source: "import prime_agent_journal_probe", exceptionClass: "ModuleNotFoundError" },
			{ sysPath: [root], signal: controller.signal },
		);
		const ready = path.join(root, "ready");
		await vi.waitFor(() => expect(existsSync(ready) && readFileSync(ready, "utf8").length > 0).toBe(true), {
			timeout: 10_000,
			interval: 20,
		});
		const replayPid = Number(readFileSync(ready, "utf8"));
		const active = readActiveOrphanProcesses(journal, process.pid);
		expect(active.map((orphan) => orphan.pid)).toEqual([replayPid]);
		expect(active[0].processStartId).toBeTypeOf("string");
		controller.abort();
		expect(await running).toMatchObject({ kind: "unrunnable", detail: "replay case aborted" });
		expect(readActiveOrphanProcesses(journal, process.pid)).toEqual([]);

		// A run that ends on its own clears its record too.
		expect(await runReplayCase(verifiedCase(missingModule))).toMatchObject({ kind: "raised" });
		const records = readFileSync(journal, "utf8")
			.trim()
			.split("\n")
			.map((line) => JSON.parse(line) as { pid: number; active: boolean });
		expect(records.map((record) => record.active)).toEqual([true, false, true, false]);
		expect(records[2].pid).toBe(records[3].pid);
		expect(readActiveOrphanProcesses(journal, process.pid)).toEqual([]);
	});

	it("kills the whole process group on timeout, so a grandchild cannot outlive the run", async () => {
		const { root } = await probeModule("prime_agent_group_probe", [
			"import os, subprocess, sys, time",
			"late = os.path.join(os.path.dirname(__file__), 'late')",
			"subprocess.Popen([sys.executable, '-I', '-c', 'import sys, time; time.sleep(1.5); open(sys.argv[1], \"w\").write(\"late\")', late])",
			"time.sleep(30)",
		]);
		const started = Date.now();
		const outcome = await runReplayCase(
			{ language: "python", source: "import prime_agent_group_probe", exceptionClass: "ModuleNotFoundError" },
			{ sysPath: [root], timeoutMs: 700 },
		);
		expect(outcome).toMatchObject({ kind: "unrunnable" });
		expect(outcome.detail).toContain("timed out");
		expect(Date.now() - started).toBeLessThan(5000);
		await new Promise((resolve) => setTimeout(resolve, 2000));
		expect(existsSync(path.join(root, "late"))).toBe(false);
	});

	it.skipIf(process.platform === "win32")(
		"kills live replay groups when the process exits or is signalled, and leaves the signal's effect alone",
		async () => {
			const script = path.join(await mkdtemp(path.join(tmpdir(), "referee-signal-fixture-")), "fixture.ts");
			cleanup.push(() => rmSync(path.dirname(script), { recursive: true, force: true }));
			await writeFile(script, SIGNAL_FIXTURE, "utf8");
			const scenario = async (
				mode:
					| "default-sigterm"
					| "default-sigint"
					| "default-sigquit"
					| "handled-sigterm"
					| "exit"
					| "exit-cwd"
					| "sigkill-group",
			) => {
				const { root } = await probeModule("prime_agent_signal_probe", [
					"import os, subprocess, sys, time",
					"root = os.path.dirname(__file__)",
					"subprocess.Popen([sys.executable, '-I', '-c', 'import sys, time; time.sleep(3); open(sys.argv[1], \"w\").write(\"late\")', os.path.join(root, 'late')])",
					"open(os.path.join(root, 'cwd'), 'w').write(os.getcwd())",
					"open(os.path.join(root, 'replay.pid'), 'w').write(str(os.getpid()))",
					"open(os.path.join(root, 'ready'), 'w').write('1')",
					"time.sleep(60)",
				]);
				const journal = path.join(root, "orphans.jsonl");
				// The worker leads its own group, as a supervisor starts it. `exec` keeps the pid the signal and the
				// journal owner name, and the core-size limit keeps SIGQUIT from writing a core file.
				const child = spawn(
					"/bin/sh",
					[
						"-c",
						'ulimit -c 0; exec "$@"',
						"sh",
						process.execPath,
						"--import",
						"tsx",
						script,
						root,
						mode,
						python as string,
					],
					{
						env: { ...process.env, TSX_TSCONFIG_PATH, [ORPHAN_PROCESS_JOURNAL_ENV]: journal },
						stdio: ["ignore", "ignore", "pipe"],
						detached: true,
					},
				);
				const workerPid = child.pid as number;
				let stderr = "";
				child.stderr.on("data", (chunk) => {
					stderr += chunk.toString();
				});
				const closed = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve) => {
					child.once("close", (code, signal) => resolve({ code, signal }));
				});
				await vi.waitFor(() => expect(existsSync(path.join(root, "ready")), stderr).toBe(true), {
					timeout: 30_000,
					interval: 20,
				});
				const replayPid = Number(readFileSync(path.join(root, "replay.pid"), "utf8"));
				const workdir = readFileSync(path.join(root, "cwd"), "utf8");
				cleanup.push(() => rmSync(workdir, { recursive: true, force: true }));
				if (mode === "default-sigterm" || mode === "handled-sigterm") child.kill("SIGTERM");
				if (mode === "default-sigint") child.kill("SIGINT");
				if (mode === "default-sigquit") child.kill("SIGQUIT");
				// No listener runs: only the journal can still name the replay group.
				if (mode === "sigkill-group") process.kill(-workerPid, "SIGKILL");
				const exit = await closed;
				const records = (existsSync(journal) ? readFileSync(journal, "utf8") : "")
					.trim()
					.split("\n")
					.filter((line) => line.length > 0)
					.map((line) => JSON.parse(line) as { pid: number; ownerPid: number; active: boolean });
				const journaled = [
					...new Set(
						records
							.filter((record) => record.active && record.ownerPid === workerPid)
							.map((record) => record.pid),
					),
				];
				const left = readActiveOrphanProcesses(journal, workerPid);
				// What supervisor recovery does with a dead worker's journal.
				const reaped = reapOrphanProcesses(left).map((result) => result.outcome);
				await new Promise((resolve) => setTimeout(resolve, 4000));
				return {
					root,
					exit,
					stderr,
					replayPid,
					journaled,
					left: left.map((orphan) => orphan.pid),
					reaped,
					late: existsSync(path.join(root, "late")),
					workdir: path.basename(workdir),
					workdirLeft: existsSync(workdir),
				};
			};
			const [sigterm, sigint, sigquit, handled, exited, exitedInCwd, killed] = await Promise.all([
				scenario("default-sigterm"),
				scenario("default-sigint"),
				scenario("default-sigquit"),
				scenario("handled-sigterm"),
				scenario("exit"),
				scenario("exit-cwd"),
				scenario("sigkill-group"),
			]);
			expect(sigterm.exit, sigterm.stderr).toEqual({ code: null, signal: "SIGTERM" });
			expect(sigint.exit, sigint.stderr).toEqual({ code: null, signal: "SIGINT" });
			expect(sigquit.exit, sigquit.stderr).toEqual({ code: null, signal: "SIGQUIT" });
			expect(exited.exit, exited.stderr).toEqual({ code: 7, signal: null });
			expect(exitedInCwd.exit, exitedInCwd.stderr).toEqual({ code: 7, signal: null });
			expect(killed.exit, killed.stderr).toEqual({ code: null, signal: "SIGKILL" });
			// Another listener owns SIGTERM and keeps the process alive: only the replay group dies.
			expect(handled.exit, handled.stderr).toEqual({ code: 0, signal: null });
			expect(existsSync(path.join(handled.root, "handled"))).toBe(true);
			expect(JSON.parse(readFileSync(path.join(handled.root, "outcome.json"), "utf8"))).toMatchObject({
				kind: "unrunnable",
				detail: "replay case killed by SIGKILL",
			});
			const all = [sigterm, sigint, sigquit, handled, exited, exitedInCwd, killed];
			// Every live replay group is journaled; a listener that killed it also cleared its record.
			expect(all.map((run) => run.journaled)).toEqual(all.map((run) => [run.replayPid]));
			expect(all.map((run) => run.left)).toEqual([[], [], [], [], [], [], [killed.replayPid]]);
			expect(killed.reaped).toEqual(["reaped"]);
			expect(all.map((run) => run.late)).toEqual([false, false, false, false, false, false, false]);
			// A listener that killed the group also removed the run's temporary directory, but never a caller's
			// working directory. With no listener run, the directory is left behind.
			expect(all.map((run) => run.workdir.replace(/^prime-agent-replay-\w+$/, "temporary"))).toEqual([
				"temporary",
				"temporary",
				"temporary",
				"temporary",
				"temporary",
				"caller-cwd",
				"temporary",
			]);
			expect(all.map((run) => run.workdirLeft)).toEqual([false, false, false, false, false, true, true]);
		},
		60_000,
	);
});

const artifact = {
	summary: "Vendor the missing dependency",
	edits: [
		{
			action: "create",
			kind: "skill",
			title: "deps",
			content: "Call json.dumps",
			arguments: {},
			reference: { type: "python", import: "json", callable: "dumps" },
		},
	],
} as unknown as JsonValue;
const memoryArtifact = {
	summary: "Note the missing dependency",
	edits: [{ action: "create", kind: "memory", title: "deps", content: "remember the import" }],
} as unknown as JsonValue;
const baseline = { entries: { memory: {} }, refinements: [] } as unknown as JsonValue;

describe.skipIf(!python)("assisted gate with the referee", () => {
	it("rejects a refuted claim naming the failure opponent, and commits the same claim once the case runs clean", async () => {
		const record = recurringRecord(missingModule, verifiedCase(missingModule));
		const fingerprintId = record.fingerprint.id;
		const observation = {
			status: "pass" as const,
			score: 90,
			failedCriteria: [],
			addressedFingerprints: [fingerprintId],
		};
		const skillImports = skillImportsOf([skillWrite(missingModule)]);

		const refuted = await adjudicateFailureClaims([record], [fingerprintId], { skillImports });
		expect(refuted[0].status).toBe("upheld");
		const rejected = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation,
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: refuted,
			epsilon: 1,
			turn: 3,
		});
		expect(rejected.certificate.committed).toBe(false);
		expect(rejected.certificate.rejection).toBe("opponents");
		expect(rejected.certificate.missedCriterionIds).toEqual([
			failureOpponentId(fingerprintId),
			refereeOpponentId(fingerprintId),
		]);
		expect(rejected.certificate.missedCurrentWeight).toBe(2);

		// The module is importable from a source root the referee is given (toolforge's, at the call sites).
		const cleared = await adjudicateFailureClaims([record], [fingerprintId], { skillImports, sysPath: [stubRoot] });
		expect(cleared[0].status).toBe("cleared");
		const committed = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation,
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: cleared,
			epsilon: 1,
			turn: 3,
		});
		expect(committed.certificate.committed).toBe(true);
		expect(committed.certificate.missedCriterionIds).toEqual([]);
		expect(committed.nextState.lineage.at(-1)).toMatchObject({ claimedFingerprints: [fingerprintId] });
	});

	it("leaves the gate exactly as it was for a fingerprint with no replay case", async () => {
		const record = recurringRecord(missingModule, undefined);
		const fingerprintId = record.fingerprint.id;
		const verdicts = await adjudicateFailureClaims([record], [fingerprintId], { skillImports: [missingModule] });
		expect(verdicts[0].status).toBe("not_applicable");
		const authorized = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: [fingerprintId] },
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: verdicts,
			epsilon: 1,
			turn: 3,
		});
		expect(authorized.certificate.committed).toBe(true);
		expect(authorized.certificate.criteria.map((item) => item.criterionId)).not.toContain(
			refereeOpponentId(fingerprintId),
		);
	});

	it("does not let a replay refute a memory-only fix, which the provisional window referees instead", async () => {
		const record = recurringRecord(missingModule, verifiedCase(missingModule));
		const fingerprintId = record.fingerprint.id;
		const verdicts = await adjudicateFailureClaims([record], [fingerprintId], {
			skillImports: skillImportsOf((memoryArtifact as { edits: { kind: string; action: string }[] }).edits),
		});
		expect(verdicts[0].status).toBe("not_applicable");
		const authorized = authorizeAssistedRavo({
			proposalId: "p1",
			artifact: memoryArtifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: [fingerprintId] },
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: verdicts,
			epsilon: 1,
			turn: 3,
		});
		expect(authorized.certificate.committed).toBe(true);
		expect(authorized.certificate.criteria.map((item) => item.criterionId)).not.toContain(
			refereeOpponentId(fingerprintId),
		);
	});

	it("fails a claim closed when an applicable case never reproduced", async () => {
		const record = recurringRecord(missingModule, { language: "python", source: `import ${missingModule}` });
		const fingerprintId = record.fingerprint.id;
		const verdicts = await adjudicateFailureClaims([record], [fingerprintId], { skillImports: [missingModule] });
		expect(verdicts[0].status).toBe("no_evidence");
		const rejected = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: [fingerprintId] },
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: verdicts,
			epsilon: 0,
			turn: 3,
		});
		expect(rejected.certificate.committed).toBe(false);
		expect(rejected.certificate.rejection).toBe("opponents");
		expect(rejected.certificate.missedCriterionIds).toEqual([failureOpponentId(fingerprintId)]);
	});
});

const usage = (totalTokens: number): Usage => ({
	input: totalTokens,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});

function fauxRunAgent(proposal: unknown): RunAgentHandler {
	return async (request) => {
		const role = /^# RAVO (\w+)/.exec(request.prompt)?.[1];
		const value =
			role === "inspect"
				? { summary: "the dependency is missing", facts: ["the import fails"] }
				: role === "plan"
					? { steps: ["record the dependency"] }
					: role === "implement" || role === "repair"
						? proposal
						: role === "judge"
							? { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [], rationale: "ok" }
							: { intervene: false };
		const result: RunAgentResult = {
			status: "completed",
			output: JSON.stringify(value),
			messages: [],
			model: "faux/child",
			turns: 1,
			toolCalls: 0,
			usage: usage(10),
		};
		return result;
	};
}

async function runService(failures: FailureLedger, proposal: unknown) {
	const harnessDir = await mkdtemp(path.join(tmpdir(), "referee-run-"));
	let state: HarnessState = {
		schema: 1,
		entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
		refinements: [],
		failures,
	};
	const saveState = vi.fn((_scope: string, next: HarnessState) => {
		state = structuredClone(next);
	});
	const updates: RavoRunStatus[] = [];
	const deps: RavoRunServiceDeps = {
		runAgent: fauxRunAgent(proposal),
		harnessDir,
		loadState: () => structuredClone(state),
		saveState,
		withStateLock: (_scope, fn) => fn(),
		onUpdate: (status) => updates.push(status),
	};
	return { service: new RavoRunService(deps), saveState, state: () => state };
}

/** Publish `importName` as a toolforge package in the test agent dir, so its source root is on the referee's path. */
async function publishToolforgePackage(importName: string, files: Record<string, string>): Promise<string> {
	const agentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR as string;
	const packagePath = path.join(agentDir, "toolforge", "packages", importName);
	for (const [relative, content] of Object.entries(files)) {
		const target = path.join(packagePath, "src", importName, relative);
		await mkdir(path.dirname(target), { recursive: true });
		await writeFile(target, content, "utf8");
	}
	saveToolforgeLedger(
		{
			schema: 1,
			records: [
				{
					name: importName,
					importName,
					packagePath,
					sourceSha: "",
					exitTestSha: "",
					status: "published",
					gate: [],
					installed: false,
					at: "2026-09-15T00:00:00.000Z",
					version: 1,
				},
			],
		},
		toolforgeLedgerPath(agentDir),
	);
	return packagePath;
}

describe.skipIf(!python)("RavoRunService with the referee", () => {
	const request: RavoRunRequest = { task: "stop the import failure", maxRounds: 2, maxRepairs: 0 };

	it("refuses the skill fix whose claim the replay case refutes, and accepts it byte-identical once the case runs clean", async () => {
		const importName = "prime_agent_referee_pkg";
		const packagePath = await publishToolforgePackage(importName, { "__init__.py": "def run():\n    return 1\n" });
		cleanup.push(() => {
			rmSync(packagePath, { recursive: true, force: true });
			rmSync(toolforgeLedgerPath(process.env.PRIME_AGENT_CODING_AGENT_DIR as string), { force: true });
		});
		const submodule = `${importName}.transport`;
		const record = recurringRecord(submodule, verifiedCase(submodule));
		const fingerprintId = record.fingerprint.id;
		const proposal = {
			summary: "Ship the dependency as a skill",
			rationale: "The import failed three times.",
			expectedOutcome: "No more import failures.",
			addressedFingerprints: [fingerprintId],
			edits: [skillWrite(importName)],
		};

		const refuted = await runService(ledgerOf(record), proposal);
		const rejectedRun = await refuted.service.start(request);
		expect(rejectedRun.stopReason).not.toBe("accepted");
		expect(rejectedRun.lastCertificate?.status).toBe("reject_criteria");
		expect(rejectedRun.lastCertificate?.missed).toEqual([
			failureOpponentId(fingerprintId),
			refereeOpponentId(fingerprintId),
		]);
		expect(refuted.saveState).not.toHaveBeenCalled();

		// Same ledger, same proposal object; only the published package changed.
		await writeFile(path.join(packagePath, "src", importName, "transport.py"), "OPEN = True\n", "utf8");
		const cleared = await runService(ledgerOf(record), proposal);
		const acceptedRun = await cleared.service.start(request);
		expect(acceptedRun.stopReason).toBe("accepted");
		expect(acceptedRun.lastCertificate?.status).toBe("commit");
		expect(acceptedRun.lastCertificate?.missed).toEqual([]);
		expect(cleared.saveState).toHaveBeenCalledOnce();
		expect(cleared.state().ravo?.lineage.at(-1)).toMatchObject({ claimedFingerprints: [fingerprintId] });
		await rm(path.join(packagePath, "src", importName, "transport.py"), { force: true });
	});

	it("rejects at the deep gate when the judge itself returns a fail verdict", async () => {
		const record = recurringRecord(missingModule, undefined);
		const proposal = {
			summary: "Remember the dependency",
			rationale: "The import failed three times.",
			expectedOutcome: "No more import failures.",
			addressedFingerprints: [record.fingerprint.id],
			edits: [{ action: "create", kind: "memory", title: "Missing dependency", content: "install it" }],
		};
		const harnessDir = await mkdtemp(path.join(tmpdir(), "referee-run-"));
		const state: HarnessState = {
			schema: 1,
			entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
			refinements: [],
			failures: ledgerOf(record),
		};
		const saveState = vi.fn(() => {});
		const base = fauxRunAgent(proposal);
		const service = new RavoRunService({
			runAgent: async (req, options) => {
				if (!req.prompt.startsWith("# RAVO judge")) return base(req, options);
				return {
					status: "completed",
					output: JSON.stringify({ verdict: "fail", score: 95, failedCriteria: [], rationale: "worse" }),
					messages: [],
					model: "faux/child",
					turns: 1,
					toolCalls: 0,
					usage: usage(10),
				};
			},
			harnessDir,
			loadState: () => structuredClone(state),
			saveState,
			withStateLock: (_scope, fn) => fn(),
			onUpdate: () => {},
		});
		const terminal = await service.start(request);
		expect(terminal.stopReason).not.toBe("accepted");
		expect(terminal.lastCertificate?.status).toBe("reject_deep");
		expect(terminal.lastCertificate?.deepScore).toBe(95);
		expect(saveState).not.toHaveBeenCalled();
	});
});
