import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { withSpan } from "@earendil-works/pi-ai";
import { getAgentDir } from "../../config.js";
import {
	installPythonSkillPackage,
	type PythonSkillPackageInstaller,
	type PythonSkillPackageInstallResult,
} from "../kernel/bootstrap.js";
import type { HostRequestHandlers } from "../kernel/shared.js";
import type { ReplayCase, ReplayOutcome } from "../ravo/referee.js";
import { runReplayCase } from "../ravo/referee-runner.js";
import { resolveKernelPython } from "../refinement/skill-dry-run.js";
import {
	appendToolforgeRecord,
	loadToolforgeLedger,
	nextToolforgeVersion,
	type ToolforgeGatePhase,
	type ToolforgeGateRun,
	type ToolforgePublishStatus,
	type ToolforgeRecord,
	toolforgeContentSha,
	toolforgeDir,
	toolforgeLedgerPath,
	toolforgeSrcPath,
} from "./ledger.js";

/**
 * Toolforge: the path by which the agent writes durable capability.
 *
 * `RefinementEdit` has no field that can carry source code, and both skill
 * screens (`skill-dry-run.ts` `checkReference`, `harness.py`
 * `_validate_python_skill_reference`) require a module that already imports —
 * so a refinement proposing genuinely new code is guaranteed to fail its own
 * screen. Toolforge goes the other way round: the source arrives first, is
 * staged as a real package, and only becomes a skill after it survives a gate
 * it cannot grade itself.
 *
 * THE GATE IS A DOUBLE RUN. The exit test is executed twice in the subprocess
 * runner the referee already owns: once against a stub whose every attribute
 * raises `NotImplementedError`, which MUST fail, and once against the real
 * staged package, which MUST pass. "Fails without, passes with" is the whole
 * claim a new capability makes, and this is that claim made executable. A run
 * that could not be performed is never read as a pass (same polarity as
 * `refereeOpponentPassed`, opposite to the fast screen).
 *
 * Only then is the package promoted by rename into `<agentDir>/skills/<name>`,
 * where `collectAutoSkillEntries` finds it, `syncPythonSkills` editable-installs
 * it and `buildRlmBootstrapCode` binds it — the cross-session half that already
 * worked and had nothing feeding it.
 */

export const MAX_TOOLFORGE_SOURCE_CHARS = 64_000;
export const MAX_TOOLFORGE_EXIT_TEST_CHARS = 16_000;
export const MAX_TOOLFORGE_DOC_CHARS = 1_000;
export const MAX_TOOLFORGE_NAME_LENGTH = 48;
export const DEFAULT_TOOLFORGE_GATE_TIMEOUT_MS = 30_000;

/**
 * Names a published skill may not take. `validateName` (`skills.ts`) checks the
 * charset only, so a skill called `bash`, `open` or `json` is legal today and
 * wins the `globals()` lookup in `buildRlmBootstrapCode` over `rlm.bash`, over
 * builtins, and over the stdlib import of that name in every subprocess that
 * gets the package root on `sys.path`.
 *
 * Skill names are lowercase `[a-z0-9-]`, so only lowercase collisions are
 * reachable: the lowercase half of `sys.stdlib_module_names`, of `dir(builtins)`
 * and of `keyword.kwlist` + `keyword.softkwlist` on Python 3.11, plus the names
 * the kernel bootstrap itself binds (`repl.py` `_ALWAYS_SKIP`). A stdlib module
 * added in a later Python is the one residual gap; it can only ever grow.
 */
export const RESERVED_TOOLFORGE_IMPORT_NAMES: ReadonlySet<string> = new Set(
	`
	abc abs aifc aiter all and anext antigravity any argparse array as ascii assert ast async asynchat asyncio
	asyncore atexit audioop await base64 bash bdb bin binascii bisect bool break breakpoint builtins bytearray
	bytes bz2 calendar callable case cgi cgitb chr chunk class classmethod cmath cmd code codecs codeop collections
	colorsys compile compileall complex concurrent configparser contextlib contextvars continue copy copyreg
	copyright credits crypt csv ctypes curses dataclasses datetime dbm decimal def del delattr dict difflib dir dis
	distutils divmod doctest elif else email encodings ensurepip enum enumerate errno eval except exec exit
	faulthandler fcntl filecmp fileinput filter finally float fnmatch for format fractions from frozenset ftplib
	functools gc genericpath get_ipython getattr getopt getpass gettext glob global globals graphlib grp gzip
	hasattr hash hashlib heapq help hex hmac html http id idlelib if imaplib imghdr imp import importlib in input
	inspect int io ipaddress is isinstance issubclass iter itertools json keyword lambda len lib2to3 license
	linecache list locale locals logging lzma mailbox mailcap map marshal match math max mcp memoryview mimetypes
	min mmap modulefinder msilib msvcrt multiprocessing netrc next nis nntplib nonlocal not nt ntpath nturl2path
	numbers object oct opcode open operator optparse or ord os ossaudiodev out pass pathlib pdb pickle pickletools
	pipes pkgutil platform plistlib poplib posix posixpath pow pprint print profile property pstats pty pwd
	py_compile pyclbr pydoc pydoc_data pyexpat queue quit quopri raise random range re readline repr reprlib
	resource return reversed rlcompleter rlm round runpy sched secrets select selectors set setattr shelve shlex
	shutil signal site slice smtpd smtplib sndhdr socket socketserver sorted spwd sqlite3 sre_compile sre_constants
	sre_parse ssl stat staticmethod statistics str string stringprep struct subprocess sum sunau super symtable sys
	sysconfig syslog tabnanny tarfile telnetlib tempfile termios textwrap this threading time timeit tkinter token
	tokenize tomllib trace traceback tracemalloc try tty tuple turtle turtledemo type types typing unicodedata
	unittest urllib uu uuid vars venv warnings wave weakref webbrowser while winreg winsound with wsgiref xdrlib
	xml xmlrpc yield zip zipapp zipfile zipimport zlib zoneinfo
`
		.split(/\s+/)
		.filter(Boolean),
);

export interface ToolforgePublishRequest {
	name: string;
	/** Body of `src/<import>/__init__.py`. Must define a callable `run`. */
	source: string;
	/** One-paragraph description; becomes the SKILL.md frontmatter description. */
	doc: string;
	/** Program that must fail against a stub and pass against the real package. */
	exitTest: string;
}

export interface ToolforgePublishOptions {
	/** Where accepted packages are promoted to. Default `<agentDir>/skills`. */
	skillsDir?: string;
	ledgerPath?: string;
	/** Scratch root for staged packages and negative stubs. Default `<agentDir>/toolforge/staging`. */
	stagingDir?: string;
	/** Interpreter for both gate runs. Default `resolveKernelPython()`. */
	pythonPath?: string;
	/** Import names already bound in this kernel; a publish may not shadow one. */
	reservedImportNames?: readonly string[];
	/** Promote + editable install. Default `installPythonSkillPackage`. */
	installPackage?: PythonSkillPackageInstaller;
	timeoutMs?: number;
	signal?: AbortSignal;
	sessionId?: string;
	now?: () => string;
}

export interface ToolforgePublishResult {
	status: ToolforgePublishStatus;
	name: string;
	importName: string;
	packagePath: string;
	srcPath: string;
	version: number;
	installed: boolean;
	gate: ToolforgeGateRun[];
	reason?: string;
	installDetail?: string;
}

export interface ToolforgeNameCheck {
	importName?: string;
	error?: string;
}

function nonEmptyString(value: unknown): value is string {
	return typeof value === "string" && value.trim().length > 0;
}

/**
 * Charset and shape rules, then the collision rules `validateName` does not
 * have. A rejection here costs one round trip and no subprocess.
 */
export function validateToolforgeName(name: unknown, reservedImportNames: readonly string[] = []): ToolforgeNameCheck {
	if (!nonEmptyString(name)) return { error: "toolforge name must be a non-empty string" };
	const trimmed = name.trim();
	if (trimmed.length > MAX_TOOLFORGE_NAME_LENGTH) {
		return { error: `toolforge name exceeds ${MAX_TOOLFORGE_NAME_LENGTH} characters (${trimmed.length})` };
	}
	if (!/^[a-z0-9-]+$/.test(trimmed)) {
		return { error: `toolforge name ${JSON.stringify(trimmed)} must be lowercase a-z, 0-9 and hyphens only` };
	}
	if (trimmed.startsWith("-") || trimmed.endsWith("-") || trimmed.includes("--")) {
		return { error: `toolforge name ${JSON.stringify(trimmed)} must not start, end or double up on a hyphen` };
	}
	const importName = trimmed.replaceAll("-", "_");
	if (!/^[a-z][a-z0-9_]*$/.test(importName)) {
		return { error: `toolforge import name ${JSON.stringify(importName)} is not a valid Python identifier` };
	}
	if (RESERVED_TOOLFORGE_IMPORT_NAMES.has(importName)) {
		return {
			error: `toolforge name ${JSON.stringify(trimmed)} collides with a Python builtin, keyword, stdlib module or kernel-bound name (${importName}); pick a name nothing else answers to`,
		};
	}
	if (reservedImportNames.includes(importName)) {
		return { error: `toolforge name ${JSON.stringify(trimmed)} collides with the loaded skill ${importName}` };
	}
	return { importName };
}

function validateRequest(request: ToolforgePublishRequest): string | undefined {
	if (!nonEmptyString(request.source)) return "toolforge source must be a non-empty string";
	if (!nonEmptyString(request.exitTest)) return "toolforge exit_test must be a non-empty string";
	if (!nonEmptyString(request.doc)) return "toolforge doc must be a non-empty string";
	if (request.source.length > MAX_TOOLFORGE_SOURCE_CHARS) {
		return `toolforge source exceeds ${MAX_TOOLFORGE_SOURCE_CHARS} characters (${request.source.length})`;
	}
	if (request.exitTest.length > MAX_TOOLFORGE_EXIT_TEST_CHARS) {
		return `toolforge exit_test exceeds ${MAX_TOOLFORGE_EXIT_TEST_CHARS} characters (${request.exitTest.length})`;
	}
	return undefined;
}

function skillDescription(doc: string): string {
	const collapsed = doc.replace(/\s+/g, " ").trim();
	return collapsed.length > MAX_TOOLFORGE_DOC_CHARS
		? `${collapsed.slice(0, MAX_TOOLFORGE_DOC_CHARS - 1)}…`
		: collapsed;
}

export function toolforgeSkillMarkdown(name: string, importName: string, doc: string): string {
	return [
		"---",
		`name: ${name}`,
		`description: ${JSON.stringify(skillDescription(doc))}`,
		"---",
		"",
		`# ${name}`,
		"",
		`Published by toolforge. Call it from the Python REPL as \`${importName}.run(...)\`.`,
		"",
		doc.trim(),
		"",
	].join("\n");
}

export function toolforgePyproject(name: string, importName: string, doc: string): string {
	return [
		"# Generated by toolforge. The kernel venv always has prime-agent-runtime",
		"# installed before skills, so it is intentionally not a declared dependency.",
		"[project]",
		`name = ${JSON.stringify(name)}`,
		'version = "0.1.0"',
		`description = ${JSON.stringify(skillDescription(doc))}`,
		'requires-python = ">=3.10"',
		"dependencies = []",
		"",
		"[build-system]",
		'requires = ["hatchling"]',
		'build-backend = "hatchling.build"',
		"",
		"[tool.hatch.build.targets.wheel]",
		`packages = [${JSON.stringify(`src/${importName}`)}]`,
		"",
	].join("\n");
}

/**
 * The negative half of the gate. Every attribute resolves to something that
 * raises `NotImplementedError`, so an exit test that genuinely exercises the
 * capability fails, and one that does not exercise it runs clean and is
 * rejected for being vacuous.
 */
export function toolforgeStubSource(importName: string): string {
	return [
		`"""Toolforge negative-run stub for ${importName}; every attribute raises."""`,
		"",
		"",
		"class _NotBuilt:",
		"    def __init__(self, attribute):",
		"        self._attribute = attribute",
		"",
		"    def __call__(self, *args, **kwargs):",
		`        raise NotImplementedError(f"toolforge negative run: ${importName}.{self._attribute} is not implemented")`,
		"",
		"",
		"def run(*args, **kwargs):",
		`    raise NotImplementedError("toolforge negative run: ${importName}.run is not implemented")`,
		"",
		"",
		"def __getattr__(name):",
		'    if name.startswith("__"):',
		"        raise AttributeError(name)",
		"    return _NotBuilt(name)",
		"",
	].join("\n");
}

function writePackage(root: string, importName: string, files: Record<string, string>): string {
	const srcDir = path.join(root, "src", importName);
	mkdirSync(srcDir, { recursive: true });
	for (const [relative, content] of Object.entries(files)) {
		const target = path.join(root, relative);
		mkdirSync(path.dirname(target), { recursive: true });
		writeFileSync(target, content, "utf-8");
	}
	return toolforgeSrcPath(root);
}

/** Stage the real package under `root`; returns its `src` directory. */
export function stageToolforgePackage(root: string, importName: string, request: ToolforgePublishRequest): string {
	const source = request.source.endsWith("\n") ? request.source : `${request.source}\n`;
	return writePackage(root, importName, {
		"SKILL.md": toolforgeSkillMarkdown(request.name, importName, request.doc),
		"pyproject.toml": toolforgePyproject(request.name, importName, request.doc),
		[path.join("src", importName, "__init__.py")]: source,
		"_exit_test.py": request.exitTest.endsWith("\n") ? request.exitTest : `${request.exitTest}\n`,
	});
}

function stageToolforgeStub(root: string, importName: string): string {
	return writePackage(root, importName, {
		[path.join("src", importName, "__init__.py")]: toolforgeStubSource(importName),
	});
}

function gateRunOk(phase: ToolforgeGatePhase, outcome: ReplayOutcome): boolean {
	return phase === "negative" ? outcome.kind === "raised" : outcome.kind === "clean";
}

async function runGatePhase(
	phase: ToolforgeGatePhase,
	exitTest: string,
	srcPath: string,
	options: ToolforgePublishOptions,
	pythonPath: string | undefined,
): Promise<ToolforgeGateRun> {
	const replay: ReplayCase = { language: "python", source: exitTest, sysPath: [srcPath] };
	const started = Date.now();
	const outcome = await runReplayCase(replay, {
		...(pythonPath ? { pythonPath } : {}),
		timeoutMs: options.timeoutMs ?? DEFAULT_TOOLFORGE_GATE_TIMEOUT_MS,
		...(options.signal ? { signal: options.signal } : {}),
		cwd: path.dirname(srcPath),
	});
	return {
		phase,
		outcome: outcome.kind,
		detail: outcome.detail,
		durationMs: Date.now() - started,
		ok: gateRunOk(phase, outcome),
	};
}

function gateRejection(gate: readonly ToolforgeGateRun[]): string | undefined {
	const negative = gate.find((run) => run.phase === "negative");
	if (negative && !negative.ok) {
		return `negative run did not fail: the exit test must raise against a stub that implements nothing, but it ${negative.outcome === "clean" ? "passed" : negative.outcome}. ${negative.detail}`;
	}
	const positive = gate.find((run) => run.phase === "positive");
	if (positive && !positive.ok) {
		return `positive run did not pass: the exit test must succeed against the real package, but it ${positive.outcome}. ${positive.detail}`;
	}
	return undefined;
}

/**
 * Replace `target` with `staged` without ever leaving `target` absent for
 * longer than one rename. A directory already at `target` is moved aside first
 * (rename onto a non-empty directory is `ENOTEMPTY`), then deleted once the new
 * one is in place. Runs while the kernel bootstrap lock is held.
 */
function promoteStagedPackage(staged: string, target: string): void {
	mkdirSync(path.dirname(target), { recursive: true });
	const displaced = existsSync(target) ? `${target}.replaced-${randomUUID()}` : undefined;
	if (displaced) {
		renameSync(target, displaced);
	}
	try {
		renameSync(staged, target);
	} catch (error) {
		if (displaced) renameSync(displaced, target);
		throw error;
	}
	if (displaced) rmSync(displaced, { recursive: true, force: true });
}

async function installStagedPackage(
	staged: string,
	target: string,
	importName: string,
	options: ToolforgePublishOptions,
): Promise<PythonSkillPackageInstallResult> {
	const install = options.installPackage ?? installPythonSkillPackage;
	return install({
		packagePath: target,
		importName,
		beforeInstall: async () => {
			promoteStagedPackage(staged, target);
		},
		...(options.signal ? { signal: options.signal } : {}),
	});
}

function recordOf(
	request: ToolforgePublishRequest,
	importName: string,
	packagePath: string,
	status: ToolforgePublishStatus,
	version: number,
	gate: readonly ToolforgeGateRun[],
	installed: boolean,
	reason: string | undefined,
	options: ToolforgePublishOptions,
): ToolforgeRecord {
	const now = options.now ?? (() => new Date().toISOString());
	return {
		name: request.name,
		importName,
		packagePath,
		sourceSha: toolforgeContentSha(request.source ?? ""),
		exitTestSha: toolforgeContentSha(request.exitTest ?? ""),
		status,
		...(reason ? { reason } : {}),
		gate: [...gate],
		installed,
		...(options.sessionId ? { sessionId: options.sessionId } : {}),
		at: now(),
		version,
	};
}

/**
 * Stage, gate, promote, install. Never throws for a rejection: a refusal is a
 * result with a reason, recorded in the ledger exactly like an acceptance, so a
 * failed capability attempt survives the session that made it.
 */
export function publishToolforgeSkill(
	request: ToolforgePublishRequest,
	options: ToolforgePublishOptions = {},
): Promise<ToolforgePublishResult> {
	return withSpan("toolforge.publish", { "toolforge.name": String(request.name ?? "") }, async (span) => {
		const agentDir = getAgentDir();
		const skillsDir = options.skillsDir ?? path.join(agentDir, "skills");
		const ledgerPath = options.ledgerPath ?? toolforgeLedgerPath(agentDir);
		const stagingRoot = options.stagingDir ?? path.join(toolforgeDir(agentDir), "staging");
		const nameCheck = validateToolforgeName(request.name, options.reservedImportNames ?? []);
		if (!nameCheck.importName) {
			span.setAttributes({ "toolforge.status": "rejected", "toolforge.reason": "name" });
			const rejected: ToolforgePublishResult = {
				status: "rejected",
				name: typeof request.name === "string" ? request.name : "",
				importName: "",
				packagePath: "",
				srcPath: "",
				version: 0,
				installed: false,
				gate: [],
				reason: nameCheck.error ?? "invalid toolforge name",
			};
			return rejected;
		}
		const importName = nameCheck.importName;
		const target = path.join(skillsDir, request.name.trim());
		span.setAttributes({ "toolforge.import": importName });

		const shapeError = validateRequest(request);
		if (shapeError) {
			span.setAttributes({ "toolforge.status": "rejected", "toolforge.reason": "shape" });
			appendToolforgeRecord(
				recordOf(request, importName, target, "rejected", 0, [], false, shapeError, options),
				ledgerPath,
			);
			const rejected: ToolforgePublishResult = {
				status: "rejected",
				name: request.name,
				importName,
				packagePath: target,
				srcPath: toolforgeSrcPath(target),
				version: 0,
				installed: false,
				gate: [],
				reason: shapeError,
			};
			return rejected;
		}

		const attemptId = randomUUID();
		const staged = path.join(stagingRoot, `${importName}-${attemptId}`);
		const stub = path.join(stagingRoot, `${importName}-${attemptId}.stub`);
		const pythonPath = options.pythonPath ?? resolveKernelPython();
		const version = nextToolforgeVersion(loadToolforgeLedger(ledgerPath), request.name);
		let gate: ToolforgeGateRun[] = [];
		let reason: string | undefined;
		let installed = false;
		let installDetail: string | undefined;
		let status: ToolforgePublishStatus = "rejected";
		try {
			const stagedSrc = stageToolforgePackage(staged, importName, request);
			const stubSrc = stageToolforgeStub(stub, importName);
			gate = await withSpan("toolforge.gate", { "toolforge.name": request.name }, async (gateSpan) => {
				const negative = await runGatePhase("negative", request.exitTest, stubSrc, options, pythonPath);
				const runs = [negative];
				if (negative.ok) {
					runs.push(await runGatePhase("positive", request.exitTest, stagedSrc, options, pythonPath));
				}
				gateSpan.setAttributes({
					"toolforge.negative": negative.outcome,
					"toolforge.positive": runs[1]?.outcome ?? "skipped",
					"toolforge.passed": runs.every((run) => run.ok) && runs.length === 2,
				});
				return runs;
			});
			reason = gateRejection(gate) ?? (gate.length === 2 ? undefined : "gate did not complete both runs");
			if (!reason) {
				const outcome = await installStagedPackage(staged, target, importName, options);
				installed = outcome.installed;
				installDetail = outcome.detail;
				status = "published";
			}
		} catch (error) {
			reason = `toolforge publish failed: ${error instanceof Error ? error.message : String(error)}`;
		} finally {
			rmSync(staged, { recursive: true, force: true });
			rmSync(stub, { recursive: true, force: true });
		}

		appendToolforgeRecord(
			recordOf(
				request,
				importName,
				target,
				status,
				status === "published" ? version : 0,
				gate,
				installed,
				reason,
				options,
			),
			ledgerPath,
		);
		span.setAttributes({
			"toolforge.status": status,
			"toolforge.installed": installed,
			"toolforge.gate_runs": gate.length,
			...(reason ? { "toolforge.reason": reason.slice(0, 200) } : {}),
		});
		return {
			status,
			name: request.name,
			importName,
			packagePath: target,
			srcPath: toolforgeSrcPath(target),
			version: status === "published" ? version : 0,
			installed,
			gate,
			...(reason ? { reason } : {}),
			...(installDetail ? { installDetail } : {}),
		};
	});
}

function stringField(payload: Record<string, unknown>, key: string): string {
	const value = payload[key];
	return typeof value === "string" ? value : "";
}

/**
 * The kernel-facing handler. Serializes concurrent publishes from one session:
 * two at once would race over the same staging root and the same bootstrap lock
 * for no benefit.
 */
export function createToolforgeHostHandlers(options: () => ToolforgePublishOptions = () => ({})): HostRequestHandlers {
	let queue: Promise<unknown> = Promise.resolve();
	return {
		"toolforge.publish": async (payload) => {
			const run = queue.then(
				() => undefined,
				() => undefined,
			);
			const result = run.then(() =>
				publishToolforgeSkill(
					{
						name: stringField(payload, "name"),
						source: stringField(payload, "source"),
						doc: stringField(payload, "doc"),
						exitTest: stringField(payload, "exit_test"),
					},
					options(),
				),
			);
			queue = result;
			const published = await result;
			return {
				status: published.status,
				name: published.name,
				import_name: published.importName,
				package_path: published.packagePath,
				src_path: published.srcPath,
				version: published.version,
				installed: published.installed,
				gate: published.gate.map((run) => ({
					phase: run.phase,
					outcome: run.outcome,
					detail: run.detail,
					duration_ms: run.durationMs,
					ok: run.ok,
				})),
				...(published.reason ? { reason: published.reason } : {}),
				...(published.installDetail ? { install_detail: published.installDetail } : {}),
			};
		},
	};
}
