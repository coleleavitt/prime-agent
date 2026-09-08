import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants, type Dirent, existsSync, readdirSync, readFileSync } from "node:fs";
import { access, lstat, mkdir, readdir, readFile, realpath, rename, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { stderr, stdin } from "node:process";
import { createInterface } from "node:readline/promises";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { getLogger, withSpan } from "@earendil-works/pi-ai";
import { getPackageDir } from "../../config.js";
import { getProcessStartId } from "../session-lease.js";
import type { PythonSkillRuntimeInfo } from "../skills.js";

const BOOTSTRAP_SCHEMA = 9;
const PYTHON_VERSION = "3.11";
const RUNTIME_REQUIREMENT = "prime-agent-runtime";
// Serializes the kernel's user namespace so it can be revived across session
// resume. Internal-only; intentionally not surfaced to the model as an import.
const STATE_SNAPSHOT_REQUIREMENT = "dill";
const DEFAULT_RLM_EXTRA_PACKAGES = [
	{ uvArg: "requests", importName: "requests", promptLabel: "requests" },
	{ uvArg: "httpx", importName: "httpx", promptLabel: "httpx" },
	{ uvArg: "pyyaml", importName: "yaml", promptLabel: "yaml (PyYAML)" },
	{ uvArg: "tomli", importName: "tomli", promptLabel: "tomli" },
	{ uvArg: "python-dotenv", importName: "dotenv", promptLabel: "dotenv (python-dotenv)" },
	{ uvArg: "pandas", importName: "pandas", promptLabel: "pandas" },
	{ uvArg: "numpy", importName: "numpy", promptLabel: "numpy" },
	{ uvArg: "scipy", importName: "scipy", promptLabel: "scipy" },
	{ uvArg: "beautifulsoup4", importName: "bs4", promptLabel: "bs4 (Beautiful Soup)" },
	{ uvArg: "lxml", importName: "lxml", promptLabel: "lxml" },
	{ uvArg: "pydantic", importName: "pydantic", promptLabel: "pydantic" },
	{ uvArg: "tyro", importName: "tyro", promptLabel: "tyro" },
];
export const DEFAULT_RLM_EXTRA_UV_ARGS = DEFAULT_RLM_EXTRA_PACKAGES.map((pkg) => pkg.uvArg);
export const DEFAULT_RLM_EXTRA_IMPORT_NAMES = DEFAULT_RLM_EXTRA_PACKAGES.map((pkg) => pkg.importName);
export const DEFAULT_RLM_EXTRA_IMPORT_LABELS = DEFAULT_RLM_EXTRA_PACKAGES.map((pkg) => pkg.promptLabel);
const UV_INSTALL_COMMAND = "curl -LsSf https://astral.sh/uv/install.sh | sh";
const REQUIRED_HARNESS_METHODS = [
	"create_memory",
	"update_memory",
	"delete_memory",
	"create_skill",
	"update_skill",
	"delete_skill",
	"create_subagent",
	"update_subagent",
	"delete_subagent",
	"create_prompt_note",
	"update_prompt_note",
	"delete_prompt_note",
	"record_refinement",
];
const RUNTIME_READY_CHECK = `import inspect; import rlm; from rlm import McpIntegration; import rlm.mcp as mcp; from rlm.harness import HarnessEntry; _harness_methods = ${JSON.stringify(REQUIRED_HARNESS_METHODS)}; assert callable(mcp.list_tools); assert callable(mcp.call_tool); assert hasattr(rlm, 'run'); assert callable(rlm); assert hasattr(rlm, 'rlm'); assert callable(rlm.rlm); assert callable(rlm.host_request); assert callable(rlm.find_models); assert callable(rlm.rlm.find_models); assert hasattr(rlm, 'harness'); assert hasattr(rlm, 'get_harness_state'); assert hasattr(rlm.rlm, 'harness'); assert hasattr(rlm.rlm, 'get_harness_state'); assert all(callable(getattr(_harness, _method, None)) for _harness in (rlm.harness, rlm.rlm.harness) for _method in _harness_methods); assert 'reference' in HarnessEntry.__dataclass_fields__; assert 'scope' in HarnessEntry.__dataclass_fields__; assert 'reference' in inspect.signature(rlm.harness.create_skill).parameters; assert 'reference' in inspect.signature(rlm.harness.update_skill).parameters; assert 'global_' in inspect.signature(rlm.harness.create_memory).parameters; assert 'global_' in inspect.signature(rlm.get_harness_state).parameters; assert not hasattr(rlm, 'background'); assert not hasattr(rlm.rlm, 'background'); from rlm.bash import BashHandle, BashResult; assert callable(rlm.bash); assert all(callable(getattr(BashHandle, _m, None)) for _m in ('tail', 'output', 'poll', 'kill')); assert {'exit_code', 'output', 'duration'} <= set(BashResult.__dataclass_fields__); import rlm.repl as _repl; assert callable(_repl.main); assert callable(_repl.emit); assert callable(_repl.host_request); assert callable(_repl.is_active); assert _repl.PROTOCOL_VERSION == 3; assert callable(rlm.emit); assert not hasattr(rlm, 'HOST_COMM_TARGET'); assert not hasattr(mcp, 'install_shutdown_hook')`;
const BOOTSTRAP_VERSION_FILE = ".bootstrap-version";
const BOOTSTRAP_LOCK_NAME = ".bootstrap.lock";
const BOOTSTRAP_LOCK_RETRY_MS = 100;
const BOOTSTRAP_LOCK_PROGRESS_INTERVAL_MS = 5_000;
const DEFAULT_BOOTSTRAP_LOCK_TIMEOUT_MS = 120_000;
const BOOTSTRAP_LOCK_STALE_WITHOUT_OWNER_MS = 30_000;
const bootstrapLog = getLogger("kernel.bootstrap");
// Sibling of .bootstrap-version: records the venv state under which RUNTIME_READY_CHECK
// last passed, so a warm start can skip the interpreter spawn (see venvRuntimeReady).
const RUNTIME_READY_STAMP_FILE = ".runtime-ready";
const RUNTIME_READY_STAMP_SCHEMA = 1;
const RUNTIME_READY_CHECK_HASH = `sha256:${createHash("sha256").update(RUNTIME_READY_CHECK).digest("hex")}`;

let inFlightEnsureKernelPython: { key: string; promise: Promise<string> } | null = null;
// Per-process memo of the last venv state that passed RUNTIME_READY_CHECK (python path ->
// serialized signature). Consulted after the signature is recomputed, so an external
// rebuild of the venv is still noticed within the process.
const verifiedRuntimeSignatures = new Map<string, string>();
// Per-process memo of the runtime source hash, keyed by the source dir's stat signature
// (paths, sizes, mtimes): the content is re-hashed only when a file changed.
let runtimeIdentityCache: { sourceDir: string; statSignature: string; identity: string } | null = null;

export type KernelPythonSkill = PythonSkillRuntimeInfo;
export type KernelBootstrapProgressHandler = (message: string) => void;

/**
 * How ensureKernelPython arrived at its interpreter:
 * - `override`: PRIME_AGENT_KERNEL_PYTHON was validated and returned.
 * - `stamped`: the venv matched .bootstrap-version and the .runtime-ready stamp; no
 *   interpreter was spawned.
 * - `verified`: the venv matched .bootstrap-version and RUNTIME_READY_CHECK was run.
 * - `synced`: the base venv was current but Python skills were (re)installed.
 * - `bootstrapped`: the venv was created or rebuilt.
 */
export type KernelPythonResolution = "override" | "stamped" | "verified" | "synced" | "bootstrapped";
export type KernelPythonResolvedHandler = (resolution: KernelPythonResolution) => void;

export interface EnsureKernelPythonOptions {
	pythonSkills?: readonly KernelPythonSkill[];
	onProgress?: KernelBootstrapProgressHandler;
	/** Reports which path resolved the interpreter (diagnostics only). */
	onResolved?: KernelPythonResolvedHandler;
}

interface BootstrapPythonSkill {
	importName: string;
	packagePath: string;
	pyprojectPath: string;
	pyprojectHash: string;
}

interface BootstrapVersion {
	schema: number;
	runtime?: string;
	snapshot?: string;
	extraUvArgs?: string[];
	pythonSkills?: BootstrapPythonSkill[];
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function isNodeError(error: unknown, code: string): boolean {
	return error instanceof Error && "code" in error && error.code === code;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function exists(filePath: string): Promise<boolean> {
	try {
		await access(filePath);
		return true;
	} catch {
		return false;
	}
}

async function isExecutable(filePath: string): Promise<boolean> {
	try {
		await access(filePath, constants.X_OK);
		return true;
	} catch {
		return false;
	}
}

function expandHome(filePath: string): string {
	if (filePath === "~") return os.homedir();
	if (filePath.startsWith("~/")) return path.join(os.homedir(), filePath.slice(2));
	return filePath;
}

function fileContentHash(filePath: string): string {
	try {
		return `sha256:${createHash("sha256").update(readFileSync(filePath)).digest("hex")}`;
	} catch {
		return "unreadable";
	}
}

function normalizePythonSkills(pythonSkills: readonly KernelPythonSkill[] | undefined): BootstrapPythonSkill[] {
	const byKey = new Map<string, BootstrapPythonSkill>();
	const addSkill = (skill: Pick<KernelPythonSkill, "importName" | "packagePath" | "pyprojectPath">): void => {
		const packagePath = path.resolve(skill.packagePath);
		const pyprojectPath = path.resolve(skill.pyprojectPath);
		const key = `${skill.importName}\0${packagePath}`;
		if (byKey.has(key)) {
			return;
		}
		const bootstrapSkill: BootstrapPythonSkill = {
			importName: skill.importName,
			packagePath,
			pyprojectPath,
			pyprojectHash: fileContentHash(pyprojectPath),
		};
		byKey.set(key, bootstrapSkill);
		for (const dependencyName of readPythonSkillDependencyNames(bootstrapSkill)) {
			const siblingDependency = resolveSiblingPythonSkillDependency(bootstrapSkill, dependencyName);
			if (siblingDependency) {
				addSkill(siblingDependency);
			}
		}
	};
	for (const skill of pythonSkills ?? []) {
		addSkill(skill);
	}
	return [...byKey.values()].sort((a, b) => {
		const packageCompare = a.packagePath.localeCompare(b.packagePath);
		if (packageCompare !== 0) return packageCompare;
		return a.importName.localeCompare(b.importName);
	});
}

function readTomlProjectSection(pyprojectPath: string): string | undefined {
	try {
		const text = readFileSync(pyprojectPath, "utf-8");
		const match = text.match(/^\s*\[project\]\s*$/m);
		if (!match || match.index === undefined) {
			return undefined;
		}
		const sectionStart = match.index + match[0].length;
		const rest = text.slice(sectionStart);
		const nextSection = rest.search(/^\s*\[/m);
		return nextSection >= 0 ? rest.slice(0, nextSection) : rest;
	} catch {
		return undefined;
	}
}

function readPythonSkillProjectName(skill: BootstrapPythonSkill): string {
	const projectSection = readTomlProjectSection(skill.pyprojectPath);
	const name = projectSection?.match(/^\s*name\s*=\s*["']([^"']+)["']/m)?.[1];
	return name?.trim() || skill.importName.replaceAll("_", "-");
}

function parseDependencyPackageName(dependency: string): string | undefined {
	const withoutMarker = dependency.split(";")[0]?.trim() ?? "";
	if (!withoutMarker) {
		return undefined;
	}
	const match = withoutMarker.match(/^([A-Za-z0-9_.-]+)/);
	return match?.[1]?.replaceAll("_", "-").toLowerCase();
}

function findTomlArrayEnd(text: string, startIndex: number): number {
	let inQuote: '"' | "'" | undefined;
	let escaped = false;
	for (let index = startIndex; index < text.length; index++) {
		const char = text[index];
		if (inQuote) {
			if (escaped) {
				escaped = false;
				continue;
			}
			if (char === "\\") {
				escaped = true;
				continue;
			}
			if (char === inQuote) {
				inQuote = undefined;
			}
			continue;
		}
		if (char === '"' || char === "'") {
			inQuote = char;
			continue;
		}
		if (char === "]") {
			return index;
		}
	}
	return -1;
}

function readPythonSkillDependencyNames(skill: BootstrapPythonSkill): Set<string> {
	const projectSection = readTomlProjectSection(skill.pyprojectPath);
	if (!projectSection) {
		return new Set();
	}
	const dependenciesStart = projectSection.search(/^\s*dependencies\s*=\s*\[/m);
	if (dependenciesStart < 0) {
		return new Set();
	}
	const arrayStart = projectSection.indexOf("[", dependenciesStart);
	if (arrayStart < 0) {
		return new Set();
	}
	const arrayEnd = findTomlArrayEnd(projectSection, arrayStart + 1);
	if (arrayEnd < 0) {
		return new Set();
	}
	const dependenciesArray = projectSection.slice(arrayStart, arrayEnd + 1);
	const dependencies = new Set<string>();
	const dependencyPattern = /"([^"\\]*(?:\\.[^"\\]*)*)"|'([^'\\]*(?:\\.[^'\\]*)*)'/g;
	for (const match of dependenciesArray.matchAll(dependencyPattern)) {
		const dependency = (match[1] ?? match[2] ?? "").replaceAll('\\"', '"').replaceAll("\\'", "'");
		const name = parseDependencyPackageName(dependency);
		if (name) {
			dependencies.add(name);
		}
	}
	return dependencies;
}

function resolveSiblingPythonSkillDependency(
	skill: BootstrapPythonSkill,
	dependencyName: string,
): BootstrapPythonSkill | undefined {
	const siblingsDir = path.dirname(skill.packagePath);
	for (const entry of readdirSync(siblingsDir, { withFileTypes: true })) {
		if (!entry.isDirectory()) {
			continue;
		}
		const packagePath = path.join(siblingsDir, entry.name);
		const pyprojectPath = path.join(packagePath, "pyproject.toml");
		if (!existsSync(pyprojectPath)) {
			continue;
		}
		const dependency: BootstrapPythonSkill = {
			importName: entry.name.replaceAll("-", "_"),
			packagePath,
			pyprojectPath,
			pyprojectHash: fileContentHash(pyprojectPath),
		};
		if (readPythonSkillProjectName(dependency).replaceAll("_", "-").toLowerCase() === dependencyName) {
			return dependency;
		}
	}
	return undefined;
}

function sortPythonSkillsForInstall(pythonSkills: readonly BootstrapPythonSkill[]): BootstrapPythonSkill[] {
	const byProjectName = new Map<string, BootstrapPythonSkill>();
	const originalIndex = new Map<BootstrapPythonSkill, number>();
	for (const [index, skill] of pythonSkills.entries()) {
		originalIndex.set(skill, index);
		byProjectName.set(readPythonSkillProjectName(skill).replaceAll("_", "-").toLowerCase(), skill);
	}

	const dependenciesBySkill = new Map<BootstrapPythonSkill, BootstrapPythonSkill[]>();
	for (const skill of pythonSkills) {
		dependenciesBySkill.set(
			skill,
			[...readPythonSkillDependencyNames(skill)]
				.map(
					(dependencyName) =>
						byProjectName.get(dependencyName) ?? resolveSiblingPythonSkillDependency(skill, dependencyName),
				)
				.filter((dependency): dependency is BootstrapPythonSkill => Boolean(dependency)),
		);
	}

	const pending = new Set(pythonSkills);
	const sorted: BootstrapPythonSkill[] = [];
	while (pending.size > 0) {
		let progressed = false;
		for (const skill of [...pending].sort((a, b) => (originalIndex.get(a) ?? 0) - (originalIndex.get(b) ?? 0))) {
			const dependencies = dependenciesBySkill.get(skill) ?? [];
			if (dependencies.some((dependency) => pending.has(dependency))) {
				continue;
			}
			sorted.push(skill);
			pending.delete(skill);
			progressed = true;
		}
		if (!progressed) {
			// Cyclic local skill dependencies cannot be topologically ordered; keep a
			// deterministic order and let uv surface the packaging error if needed.
			sorted.push(...[...pending].sort((a, b) => a.packagePath.localeCompare(b.packagePath)));
			break;
		}
	}
	return sorted;
}

function formatPythonSkillInstallArgs(skill: BootstrapPythonSkill): string[] {
	return ["--editable", skill.packagePath];
}

function ensureKernelPythonKey(pythonSkills: readonly BootstrapPythonSkill[]): string {
	return [
		process.env.PRIME_AGENT_KERNEL_PYTHON ?? "",
		process.env.PRIME_AGENT_KERNEL_VENV ?? "",
		process.env.HOME ?? "",
		process.env.XDG_DATA_HOME ?? "",
		JSON.stringify(pythonSkills),
	].join("\0");
}

export function getKernelVenvDir(): string {
	const override = process.env.PRIME_AGENT_KERNEL_VENV;
	if (override) return path.resolve(expandHome(override));
	return path.join(os.homedir(), ".prime", "agent", "kernel-venv");
}

function getXdgKernelVenvDir(): string {
	const dataHome = process.env.XDG_DATA_HOME
		? path.resolve(expandHome(process.env.XDG_DATA_HOME))
		: path.join(os.homedir(), ".local", "share");
	return path.join(dataHome, "prime", "agent", "kernel-venv");
}

async function resolveWritableKernelVenvDir(): Promise<string> {
	const primary = getKernelVenvDir();
	try {
		await mkdir(path.dirname(primary), { recursive: true });
		return primary;
	} catch (primaryError) {
		if (process.env.PRIME_AGENT_KERNEL_VENV) {
			throw new Error(`couldn't create kernel venv parent directory for ${primary}: ${errorMessage(primaryError)}`);
		}

		const fallback = getXdgKernelVenvDir();
		try {
			await mkdir(path.dirname(fallback), { recursive: true });
			return fallback;
		} catch (fallbackError) {
			throw new Error(
				`couldn't create kernel venv directory at ${primary} or ${fallback}; set PRIME_AGENT_KERNEL_PYTHON to a python with a current prime-agent-runtime installed. ${errorMessage(fallbackError)}`,
			);
		}
	}
}

function run(command: string, args: string[], options: { stdio?: "ignore" | "inherit" } = {}): Promise<void> {
	return new Promise((resolve, reject) => {
		const child = spawn(command, args, {
			env: process.env,
			stdio: options.stdio ?? "ignore",
		});
		child.on("error", reject);
		child.on("exit", (code, signal) => {
			if (code === 0) {
				resolve();
				return;
			}
			const reason = signal ? `signal ${signal}` : `exit code ${code}`;
			reject(new Error(`${command} ${args.join(" ")} failed with ${reason}`));
		});
	});
}

async function pythonImports(python: string, moduleName: string): Promise<boolean> {
	try {
		await run(python, ["-c", `import ${moduleName}`], { stdio: "ignore" });
		return true;
	} catch {
		return false;
	}
}

async function hasPrimeAgentRuntime(python: string): Promise<boolean> {
	try {
		await run(python, ["-c", RUNTIME_READY_CHECK], { stdio: "ignore" });
		return true;
	} catch {
		return false;
	}
}

interface RuntimeReadySignature {
	schema: number;
	/** Hash of RUNTIME_READY_CHECK: a stricter check in a newer build re-verifies. */
	check: string;
	runtime: string;
	/** realpath, size and mtime of the interpreter behind <venv>/bin/python. */
	python: string;
	/** Content hash of pyvenv.cfg: a recreated venv never matches an old stamp. */
	venvConfig: string;
	/** Hash of the site-packages listing: any package install/uninstall invalidates the stamp. */
	sitePackages: string;
	/** The installed prime-agent-runtime dist-info and the hash of its RECORD. */
	runtimeInstall: string;
}

function sha256(content: string | Buffer): string {
	return `sha256:${createHash("sha256").update(content).digest("hex")}`;
}

async function findSitePackagesDirs(venv: string): Promise<string[]> {
	const dirs: string[] = [];
	for (const libName of ["lib", "Lib"]) {
		const libDir = path.join(venv, libName);
		let entries: Dirent[];
		try {
			entries = await readdir(libDir, { withFileTypes: true });
		} catch {
			continue;
		}
		if (entries.some((entry) => entry.name === "site-packages")) {
			dirs.push(path.join(libDir, "site-packages"));
		}
		for (const entry of entries) {
			if (entry.isDirectory() && entry.name.startsWith("python")) {
				dirs.push(path.join(libDir, entry.name, "site-packages"));
			}
		}
	}
	return dirs;
}

async function runtimeInstallSignature(venv: string): Promise<string> {
	for (const siteDir of await findSitePackagesDirs(venv)) {
		let entries: string[];
		try {
			entries = await readdir(siteDir);
		} catch {
			continue;
		}
		const distInfo = entries.filter((entry) => /^prime_agent_runtime-.*\.dist-info$/.test(entry)).sort();
		if (distInfo.length === 0) continue;
		const records = await Promise.all(
			distInfo.map(async (name) => {
				try {
					return `${name}\0${sha256(await readFile(path.join(siteDir, name, "RECORD")))}`;
				} catch {
					return `${name}\0unreadable`;
				}
			}),
		);
		return `${path.relative(venv, siteDir)}\0${records.join("\0")}`;
	}
	return "missing";
}

async function computeRuntimeReadySignature(
	python: string,
	venv: string,
	runtimeIdentity: string,
): Promise<RuntimeReadySignature | null> {
	try {
		const [link, target, targetStat, venvConfig, runtimeInstall] = await Promise.all([
			lstat(python),
			realpath(python),
			stat(python),
			readFile(path.join(venv, "pyvenv.cfg")),
			runtimeInstallSignature(venv),
		]);
		const siteDirs = await findSitePackagesDirs(venv);
		const siteStats = await Promise.all(
			siteDirs.map(async (dir) => {
				try {
					// Entry names, not the directory mtime: the first interpreter run adds
					// __pycache__, which would otherwise invalidate every fresh stamp once.
					const entries = (await readdir(dir)).filter((entry) => entry !== "__pycache__").sort();
					return `${path.relative(venv, dir)}\0${sha256(entries.join("\n"))}`;
				} catch {
					return `${path.relative(venv, dir)}\0missing`;
				}
			}),
		);
		return {
			schema: RUNTIME_READY_STAMP_SCHEMA,
			check: RUNTIME_READY_CHECK_HASH,
			runtime: runtimeIdentity,
			python: `${target}\0${targetStat.size}\0${targetStat.mtimeMs}\0${link.mtimeMs}`,
			venvConfig: sha256(venvConfig),
			sitePackages: siteStats.join("\n"),
			runtimeInstall,
		};
	} catch {
		// Anything unreadable (no interpreter, no pyvenv.cfg): never trust or write a stamp.
		return null;
	}
}

function serializeRuntimeReadySignature(signature: RuntimeReadySignature): string {
	return JSON.stringify({
		schema: signature.schema,
		check: signature.check,
		runtime: signature.runtime,
		python: signature.python,
		venvConfig: signature.venvConfig,
		sitePackages: signature.sitePackages,
		runtimeInstall: signature.runtimeInstall,
	});
}

async function readRuntimeReadyStamp(venv: string): Promise<string | null> {
	try {
		const parsed: unknown = JSON.parse(await readFile(path.join(venv, RUNTIME_READY_STAMP_FILE), "utf8"));
		if (!isRecord(parsed) || parsed.schema !== RUNTIME_READY_STAMP_SCHEMA) return null;
		const fields = ["check", "runtime", "python", "venvConfig", "sitePackages", "runtimeInstall"] as const;
		if (!fields.every((field) => typeof parsed[field] === "string")) return null;
		return serializeRuntimeReadySignature(parsed as unknown as RuntimeReadySignature);
	} catch {
		return null;
	}
}

async function writeRuntimeReadyStamp(venv: string, serialized: string): Promise<void> {
	// Best effort and atomic: a concurrent reader sees either the old or the new stamp,
	// and a read-only venv simply keeps paying for the interpreter check.
	const target = path.join(venv, RUNTIME_READY_STAMP_FILE);
	const temp = `${target}.${process.pid}.tmp`;
	try {
		await writeFile(temp, `${serialized}\n`, "utf8");
		await rename(temp, target);
	} catch {
		await rm(temp, { force: true }).catch(() => undefined);
	}
}

/**
 * RUNTIME_READY_CHECK for the managed venv, skipping the interpreter spawn (~50 ms+)
 * when the venv is byte-for-byte in the state under which the check last passed. The
 * signature covers the interpreter, pyvenv.cfg, the site-packages listing, the installed
 * runtime's RECORD, the runtime identity and the check text itself: any package
 * install, venv rebuild, runtime or build change re-runs the real check. A venv that
 * decays without touching any of those still fails fast at the kernel protocol
 * handshake; only the automatic rebuild for that case moves behind a stamp mismatch.
 */
async function venvRuntimeReady(
	python: string,
	venv: string,
	runtimeIdentity: string,
): Promise<{ ready: boolean; stamped: boolean }> {
	const signature = await computeRuntimeReadySignature(python, venv, runtimeIdentity);
	// Not a venv layout this stamp understands: the interpreter check decides alone.
	if (!signature) return { ready: await hasPrimeAgentRuntime(python), stamped: false };
	const serialized = serializeRuntimeReadySignature(signature);
	if (verifiedRuntimeSignatures.get(python) === serialized) return { ready: true, stamped: true };
	if ((await readRuntimeReadyStamp(venv)) === serialized) {
		verifiedRuntimeSignatures.set(python, serialized);
		return { ready: true, stamped: true };
	}
	if (!(await hasPrimeAgentRuntime(python))) return { ready: false, stamped: false };
	// Re-read: the check may itself have been raced by a concurrent bootstrap.
	const after = await computeRuntimeReadySignature(python, venv, runtimeIdentity);
	if (after && serializeRuntimeReadySignature(after) === serialized) {
		verifiedRuntimeSignatures.set(python, serialized);
		await writeRuntimeReadyStamp(venv, serialized);
	}
	return { ready: true, stamped: false };
}

async function missingRlmExtraImportLabels(python: string): Promise<string[]> {
	const missing: string[] = [];
	for (const pkg of DEFAULT_RLM_EXTRA_PACKAGES) {
		if (!(await pythonImports(python, pkg.importName))) {
			missing.push(pkg.promptLabel);
		}
	}
	return missing;
}

async function missingPythonSkillImportLabels(
	python: string,
	pythonSkills: readonly KernelPythonSkill[],
): Promise<string[]> {
	const missing: string[] = [];
	for (const skill of pythonSkills) {
		if (!(await pythonImports(python, skill.importName))) {
			missing.push(`${skill.name} (${skill.importName})`);
		}
	}
	return missing;
}

function reportProgress(options: EnsureKernelPythonOptions, message: string): void {
	if (options.onProgress) {
		options.onProgress(message);
		return;
	}
	process.stderr.write(`${message}\n`);
}

function bootstrapLockDir(venv: string): string {
	return path.join(path.dirname(venv), `${path.basename(venv)}${BOOTSTRAP_LOCK_NAME}`);
}

interface BootstrapLockOwner {
	version: 1;
	token: string;
	pid: number;
	processStartId?: string;
	createdAt: string;
}

function processIsRunning(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch (error) {
		return isNodeError(error, "EPERM");
	}
}

async function readBootstrapLockOwner(lockDir: string): Promise<BootstrapLockOwner | null> {
	try {
		const parsed: unknown = JSON.parse(await readFile(path.join(lockDir, "owner.json"), "utf8"));
		if (
			!isRecord(parsed) ||
			parsed.version !== 1 ||
			typeof parsed.token !== "string" ||
			typeof parsed.pid !== "number" ||
			!Number.isInteger(parsed.pid) ||
			parsed.pid <= 0 ||
			typeof parsed.createdAt !== "string" ||
			(parsed.processStartId !== undefined && typeof parsed.processStartId !== "string")
		) {
			return null;
		}
		return parsed as unknown as BootstrapLockOwner;
	} catch {
		return null;
	}
}

async function lockWithoutOwnerIsStale(lockDir: string): Promise<boolean> {
	try {
		const lockStat = await stat(lockDir);
		return Date.now() - lockStat.mtimeMs > BOOTSTRAP_LOCK_STALE_WITHOUT_OWNER_MS;
	} catch {
		return false;
	}
}

function bootstrapLockOwnerIsRunning(owner: BootstrapLockOwner): boolean {
	if (!processIsRunning(owner.pid)) return false;
	return owner.processStartId === undefined || getProcessStartId(owner.pid) === owner.processStartId;
}

function bootstrapLockTimeoutMs(): number {
	const raw = process.env.PRIME_AGENT_INTERNAL_KERNEL_BOOTSTRAP_LOCK_TIMEOUT_MS;
	if (raw === undefined) return DEFAULT_BOOTSTRAP_LOCK_TIMEOUT_MS;
	if (!/^(0|[1-9]\d*)$/.test(raw)) {
		throw new Error(`Invalid PRIME_AGENT_INTERNAL_KERNEL_BOOTSTRAP_LOCK_TIMEOUT_MS: ${raw}`);
	}
	const configured = Number(raw);
	if (!Number.isSafeInteger(configured)) {
		throw new Error(`Invalid PRIME_AGENT_INTERNAL_KERNEL_BOOTSTRAP_LOCK_TIMEOUT_MS: ${raw}`);
	}
	return configured;
}

function describeBootstrapLockOwner(owner: BootstrapLockOwner | null, ageMs: number): string {
	const identity = owner
		? `pid ${owner.pid}${owner.processStartId ? ` (start ${owner.processStartId})` : ""}`
		: "unknown owner";
	return `${identity}, started ${owner?.createdAt ?? "unknown"}, age ${Math.max(0, Math.round(ageMs))}ms`;
}

async function acquireBootstrapLock(venv: string, options: EnsureKernelPythonOptions): Promise<() => Promise<void>> {
	return withSpan("kernel.bootstrap_lock", { "kernel.venv": venv }, async (span) => {
		const lockDir = bootstrapLockDir(venv);
		const startedAt = Date.now();
		const timeoutMs = bootstrapLockTimeoutMs();
		let nextProgressAt = startedAt;
		await mkdir(path.dirname(lockDir), { recursive: true });

		for (;;) {
			const token = randomUUID();
			try {
				await mkdir(lockDir);
				const processStartId = getProcessStartId(process.pid);
				const owner: BootstrapLockOwner = {
					version: 1,
					token,
					pid: process.pid,
					...(processStartId ? { processStartId } : {}),
					createdAt: new Date().toISOString(),
				};
				await writeFile(path.join(lockDir, "owner.json"), `${JSON.stringify(owner)}\n`, "utf8");
				const waitedMs = Date.now() - startedAt;
				span.setAttributes({ "kernel.lock_wait_ms": waitedMs, "kernel.lock_owner_pid": process.pid });
				bootstrapLog.info("kernel bootstrap lock acquired", { venv, waitedMs, ownerPid: process.pid });
				return async () => {
					const current = await readBootstrapLockOwner(lockDir);
					if (current?.token === token) await rm(lockDir, { recursive: true, force: true });
				};
			} catch (error) {
				if (!isNodeError(error, "EEXIST")) {
					await rm(lockDir, { recursive: true, force: true }).catch(() => undefined);
					throw error;
				}

				const owner = await readBootstrapLockOwner(lockDir);
				if (owner ? !bootstrapLockOwnerIsRunning(owner) : await lockWithoutOwnerIsStale(lockDir)) {
					// Re-read immediately before removal. Another contender may have reclaimed
					// the stale directory and acquired it since our first observation.
					const currentOwner = await readBootstrapLockOwner(lockDir);
					if (currentOwner?.token !== owner?.token) continue;
					bootstrapLog.warn("reclaiming stale kernel bootstrap lock", {
						venv,
						ownerPid: owner?.pid,
						ownerProcessStartId: owner?.processStartId,
						ownerStartedAt: owner?.createdAt,
					});
					await rm(lockDir, { recursive: true, force: true });
					continue;
				}

				const now = Date.now();
				const waitedMs = now - startedAt;
				const ownerStarted = owner ? Date.parse(owner.createdAt) : Number.NaN;
				const ownerAgeMs = Number.isFinite(ownerStarted) ? now - ownerStarted : waitedMs;
				if (now >= nextProgressAt) {
					const description = describeBootstrapLockOwner(owner, ownerAgeMs);
					reportProgress(options, `› waiting for python kernel setup lock (${description})…`);
					bootstrapLog.info("waiting for kernel bootstrap lock", {
						venv,
						waitedMs,
						ownerPid: owner?.pid,
						ownerProcessStartId: owner?.processStartId,
						ownerStartedAt: owner?.createdAt,
						ownerAgeMs,
					});
					nextProgressAt = now + BOOTSTRAP_LOCK_PROGRESS_INTERVAL_MS;
				}
				if (waitedMs >= timeoutMs) {
					span.setAttributes({ "kernel.lock_wait_ms": waitedMs, "kernel.lock_timeout": true });
					throw new Error(
						`Timed out after ${waitedMs}ms waiting for python kernel setup lock at ${lockDir} (${describeBootstrapLockOwner(owner, ownerAgeMs)}).`,
					);
				}
				await sleep(Math.min(BOOTSTRAP_LOCK_RETRY_MS, timeoutMs - waitedMs));
			}
		}
	});
}

async function findExecutable(name: string): Promise<string | null> {
	const pathValue = process.env.PATH;
	if (!pathValue) return null;
	const candidates = process.platform === "win32" ? [name, `${name}.exe`] : [name];
	for (const dir of pathValue.split(path.delimiter)) {
		if (!dir) continue;
		for (const candidate of candidates) {
			const fullPath = path.join(dir, candidate);
			if (await isExecutable(fullPath)) return fullPath;
		}
	}
	return null;
}

async function ensureUv(options: EnsureKernelPythonOptions): Promise<string> {
	const fromPath = await findExecutable("uv");
	if (fromPath) return fromPath;

	const localUv = path.join(os.homedir(), ".local", "bin", process.platform === "win32" ? "uv.exe" : "uv");
	if (await isExecutable(localUv)) return localUv;

	const shouldInstallUv =
		process.env.PRIME_AGENT_INSTALL_UV === "1" || (!options.onProgress && (await confirmUvInstall()));
	if (!shouldInstallUv) {
		throw new Error(
			`uv is required to set up the Python kernel. Install uv yourself: ${UV_INSTALL_COMMAND}, ` +
				"or set PRIME_AGENT_INSTALL_UV=1 to let prime-agent run that installer.",
		);
	}

	reportProgress(options, "› installing uv (one-time)…");
	try {
		await run("sh", ["-c", UV_INSTALL_COMMAND], { stdio: options.onProgress ? "ignore" : "inherit" });
	} catch (error) {
		throw new Error(
			`couldn't install uv from astral.sh; install it yourself: ${UV_INSTALL_COMMAND}, then re-run prime-agent. ${errorMessage(error)}`,
		);
	}

	if (await isExecutable(localUv)) return localUv;
	const installedFromPath = await findExecutable("uv");
	if (installedFromPath) return installedFromPath;
	throw new Error("uv install completed but binary not found at ~/.local/bin/uv");
}

async function confirmUvInstall(): Promise<boolean> {
	if (process.env.PRIME_AGENT_INSTALL_UV === "0") return false;
	if (!stdin.isTTY || !stderr.isTTY) return false;

	const rl = createInterface({ input: stdin, output: stderr });
	try {
		const answer = (await rl.question("Prime Agent needs uv to set up Python. Install uv from astral.sh now? [Y/n] "))
			.trim()
			.toLowerCase();
		return answer !== "n" && answer !== "no";
	} finally {
		rl.close();
	}
}

async function readBootstrapVersion(venv: string): Promise<BootstrapVersion | null> {
	try {
		const raw = await readFile(path.join(venv, BOOTSTRAP_VERSION_FILE), "utf8");
		const parsed: unknown = JSON.parse(raw);
		if (!isRecord(parsed) || typeof parsed.schema !== "number") return null;
		const extraUvArgs =
			Array.isArray(parsed.extraUvArgs) &&
			parsed.extraUvArgs.every((v: unknown): v is string => typeof v === "string")
				? (parsed.extraUvArgs as string[])
				: undefined;
		let pythonSkills: BootstrapPythonSkill[] | undefined;
		if (Array.isArray(parsed.pythonSkills)) {
			if (
				!parsed.pythonSkills.every((v: unknown): v is BootstrapPythonSkill => {
					if (!isRecord(v)) return false;
					return (
						typeof v.importName === "string" &&
						typeof v.packagePath === "string" &&
						typeof v.pyprojectPath === "string" &&
						typeof v.pyprojectHash === "string"
					);
				})
			) {
				return null;
			}
			pythonSkills = parsed.pythonSkills as BootstrapPythonSkill[];
		}
		return {
			schema: parsed.schema,
			runtime: typeof parsed.runtime === "string" ? parsed.runtime : undefined,
			snapshot: typeof parsed.snapshot === "string" ? parsed.snapshot : undefined,
			extraUvArgs,
			pythonSkills,
		};
	} catch {
		return null;
	}
}

function extraUvArgsMatch(a: string[] | undefined, b: string[] | undefined): boolean {
	if (a === b) return true;
	if (!a || !b) return false;
	if (a.length !== b.length) return false;
	return a.every((v, i) => v === b[i]);
}

function pythonSkillKey(skill: Pick<BootstrapPythonSkill, "importName" | "packagePath">): string {
	return `${skill.importName}\0${skill.packagePath}`;
}

function pythonSkillInstalled(installed: BootstrapPythonSkill | undefined, expected: BootstrapPythonSkill): boolean {
	return (
		installed !== undefined &&
		installed.pyprojectPath === expected.pyprojectPath &&
		installed.pyprojectHash === expected.pyprojectHash
	);
}

// The manifest records what is installed in the venv; a session needs its skills to be
// a subset of that, not the exact set. Sessions differ in visible skills (goal, compact,
// refine, agent-message… are feature-gated), so exact matching made every alternation
// between two sessions rewrite the manifest and reinstall the skills the other dropped.
function pythonSkillsInstalled(
	installed: readonly BootstrapPythonSkill[] | undefined,
	expected: readonly BootstrapPythonSkill[],
): boolean {
	const byKey = new Map((installed ?? []).map((skill) => [pythonSkillKey(skill), skill]));
	return expected.every((skill) => pythonSkillInstalled(byKey.get(pythonSkillKey(skill)), skill));
}

function bootstrapVersionCurrent(
	version: BootstrapVersion | null,
	runtimeIdentity: string,
	pythonSkills: readonly BootstrapPythonSkill[],
): boolean {
	return (
		version !== null &&
		bootstrapBaseVersionCurrent(version, runtimeIdentity) &&
		pythonSkillsInstalled(version.pythonSkills, pythonSkills)
	);
}

function bootstrapBaseVersionCurrent(version: BootstrapVersion | null, runtimeIdentity: string): boolean {
	return (
		version?.schema === BOOTSTRAP_SCHEMA &&
		version.runtime === runtimeIdentity &&
		version.snapshot === STATE_SNAPSHOT_REQUIREMENT &&
		extraUvArgsMatch(version.extraUvArgs, DEFAULT_RLM_EXTRA_UV_ARGS)
	);
}

async function writeBootstrapVersion(
	venv: string,
	runtimeIdentity: string,
	pythonSkills: readonly BootstrapPythonSkill[],
): Promise<void> {
	const version: BootstrapVersion = {
		schema: BOOTSTRAP_SCHEMA,
		runtime: runtimeIdentity,
		snapshot: STATE_SNAPSHOT_REQUIREMENT,
		extraUvArgs: DEFAULT_RLM_EXTRA_UV_ARGS,
		pythonSkills: [...pythonSkills],
	};
	await writeFile(path.join(venv, BOOTSTRAP_VERSION_FILE), `${JSON.stringify(version)}\n`, "utf8");
}

function runtimeCandidateDirs(): string[] {
	const moduleDir = path.dirname(fileURLToPath(import.meta.url));
	// dist/prime-agent-runtime is listed first deliberately: it is the only path stable
	// across every shipped layout (dist/, dist/bundle/, bun), where import.meta.url-relative
	// resolution breaks. `npm run build` rebuilds it from live source (copy-assets does
	// rm -rf + cp), so the staleness hash still refreshes on every build. The relative
	// paths below cover running from source (tsx) where dist/ hasn't been built.
	const distRuntime = path.join(getPackageDir(), "dist", "prime-agent-runtime");
	const sourceRuntime = path.resolve(moduleDir, "..", "..", "..", "..", "..", "prime-agent-runtime");
	// Running from source (tsx/vitest): moduleDir is <package>/src/core/kernel. A stale
	// dist/ copy left by an older build must not win here, or a source run installs the
	// old runtime into the shared venv (and the installed CLI rebuilds it right back).
	if (path.basename(path.resolve(moduleDir, "..", "..")) === "src") {
		return [sourceRuntime, distRuntime, path.resolve(moduleDir, "..", "..", "prime-agent-runtime")];
	}
	return [distRuntime, path.resolve(moduleDir, "..", "..", "prime-agent-runtime"), sourceRuntime];
}

async function resolveRuntimeSourceDir(): Promise<string | null> {
	for (const candidate of runtimeCandidateDirs()) {
		if (await exists(path.join(candidate, "pyproject.toml"))) {
			return candidate;
		}
	}
	return null;
}

// Identity of the runtime to be installed. For a local source checkout this is a
// content hash of every rlm/*.py file plus pyproject.toml, so any runtime code or
// dependency change invalidates an existing venv automatically. Falls back to the
// bare package name when the runtime resolves to a registry install (no local source).
export async function resolveRuntimeIdentity(): Promise<string> {
	const sourceDir = await resolveRuntimeSourceDir();
	if (!sourceDir) return RUNTIME_REQUIREMENT;
	const files = await listRuntimeSourceFiles(sourceDir);
	const statSignature = await runtimeSourceStatSignature(sourceDir, files);
	if (runtimeIdentityCache?.sourceDir === sourceDir && runtimeIdentityCache.statSignature === statSignature) {
		return runtimeIdentityCache.identity;
	}
	const identity = await hashRuntimeSource(sourceDir, files);
	runtimeIdentityCache = { sourceDir, statSignature, identity };
	return identity;
}

// Throws if the local source can't be read. A failure here must surface rather than
// fall back to RUNTIME_REQUIREMENT: that constant is the registry-install identity, and
// recording it for a local checkout would permanently mask later source changes.
async function listRuntimeSourceFiles(sourceDir: string): Promise<string[]> {
	const rlmDir = path.join(sourceDir, "src", "rlm");
	const files: string[] = [path.join(sourceDir, "pyproject.toml")];
	async function collect(dir: string): Promise<void> {
		const entries = await readdir(dir, { withFileTypes: true });
		for (const entry of entries) {
			const full = path.join(dir, entry.name);
			if (entry.isDirectory()) {
				await collect(full);
			} else if (entry.isFile() && entry.name.endsWith(".py")) {
				files.push(full);
			}
		}
	}
	await collect(rlmDir);
	files.sort();
	return files;
}

async function runtimeSourceStatSignature(sourceDir: string, files: readonly string[]): Promise<string> {
	const stats = await Promise.all(files.map((file) => stat(file)));
	return stats
		.map((fileStat, index) => `${path.relative(sourceDir, files[index])}\0${fileStat.size}\0${fileStat.mtimeMs}`)
		.join("\n");
}

async function hashRuntimeSource(sourceDir: string, files: readonly string[]): Promise<string> {
	const hash = createHash("sha256");
	for (const file of files) {
		hash.update(path.relative(sourceDir, file));
		hash.update("\0");
		hash.update(await readFile(file));
		hash.update("\0");
	}
	return `sha256:${hash.digest("hex")}`;
}

async function bootstrapVenv(
	venv: string,
	pythonSkills: readonly BootstrapPythonSkill[],
	options: EnsureKernelPythonOptions,
): Promise<void> {
	await mkdir(path.dirname(venv), { recursive: true });
	const uv = await ensureUv(options);
	const python = path.join(venv, "bin", "python");
	const sourceDir = await resolveRuntimeSourceDir();
	const runtimeRequirement = sourceDir ?? RUNTIME_REQUIREMENT;
	const runtimeIdentity = await resolveRuntimeIdentity();

	await run(uv, ["python", "install", PYTHON_VERSION]);
	await run(uv, ["venv", venv, "--python", PYTHON_VERSION, "--seed"]);
	await run(uv, [
		"pip",
		"install",
		"--python",
		python,
		runtimeRequirement,
		STATE_SNAPSHOT_REQUIREMENT,
		...DEFAULT_RLM_EXTRA_UV_ARGS,
	]);
	await syncPythonSkills(uv, venv, python, runtimeIdentity, pythonSkills, options);
}

async function syncPythonSkills(
	uv: string,
	venv: string,
	python: string,
	runtimeIdentity: string,
	pythonSkills: readonly BootstrapPythonSkill[],
	options: EnsureKernelPythonOptions,
): Promise<void> {
	const version = await readBootstrapVersion(venv);
	const installedPythonSkills: BootstrapPythonSkill[] = [];
	const currentPythonSkills = new Map((version?.pythonSkills ?? []).map((skill) => [pythonSkillKey(skill), skill]));
	const pythonSkillsByProjectName = new Map(
		pythonSkills.map((skill) => [readPythonSkillProjectName(skill).replaceAll("_", "-").toLowerCase(), skill]),
	);
	const dependenciesBySkill = new Map(
		pythonSkills.map((skill) => [
			skill,
			[...readPythonSkillDependencyNames(skill)]
				.map(
					(dependencyName) =>
						pythonSkillsByProjectName.get(dependencyName) ??
						resolveSiblingPythonSkillDependency(skill, dependencyName),
				)
				.filter((dependency): dependency is BootstrapPythonSkill => Boolean(dependency)),
		]),
	);

	for (const skill of sortPythonSkillsForInstall(pythonSkills)) {
		if (pythonSkillInstalled(currentPythonSkills.get(pythonSkillKey(skill)), skill)) {
			installedPythonSkills.push(skill);
			continue;
		}

		const localDependencies = dependenciesBySkill.get(skill) ?? [];
		const localDependencyArgs = localDependencies
			.filter((dependency) => {
				const installedThisSync = installedPythonSkills.some(
					(installed) =>
						pythonSkillKey(installed) === pythonSkillKey(dependency) &&
						pythonSkillInstalled(installed, dependency),
				);
				return !(
					installedThisSync ||
					pythonSkillInstalled(currentPythonSkills.get(pythonSkillKey(dependency)), dependency)
				);
			})
			.flatMap(formatPythonSkillInstallArgs);

		try {
			await run(uv, [
				"pip",
				"install",
				"--python",
				python,
				...formatPythonSkillInstallArgs(skill),
				...localDependencyArgs,
			]);
			installedPythonSkills.push(
				skill,
				...localDependencies.filter((dependency) => !installedPythonSkills.includes(dependency)),
			);
		} catch (error) {
			reportProgress(
				options,
				`Warning: Python skill ${skill.importName} failed to install and will be unavailable: ${errorMessage(error)}`,
			);
		}
	}
	await writeBootstrapVersion(
		venv,
		runtimeIdentity,
		mergeInstalledPythonSkills(version?.pythonSkills ?? [], installedPythonSkills),
	);
}

/**
 * Manifest entries to record after a sync: everything installed this time plus the
 * previously recorded skills this session did not ask for, which are still installed.
 * A previous entry is dropped when a skill installed now displaces it in the venv:
 * same key (a changed pyproject), same import name, or same project name.
 */
function mergeInstalledPythonSkills(
	previous: readonly BootstrapPythonSkill[],
	installed: readonly BootstrapPythonSkill[],
): BootstrapPythonSkill[] {
	const installedKeys = new Set(installed.map(pythonSkillKey));
	const installedImportNames = new Set(installed.map((skill) => skill.importName));
	const installedProjectNames = new Set(installed.map((skill) => normalizedPythonSkillProjectName(skill)));
	const kept = previous.filter(
		(skill) =>
			!installedKeys.has(pythonSkillKey(skill)) &&
			!installedImportNames.has(skill.importName) &&
			!installedProjectNames.has(normalizedPythonSkillProjectName(skill)),
	);
	return [...kept, ...installed].sort((a, b) => {
		const packageCompare = a.packagePath.localeCompare(b.packagePath);
		if (packageCompare !== 0) return packageCompare;
		return a.importName.localeCompare(b.importName);
	});
}

function normalizedPythonSkillProjectName(skill: BootstrapPythonSkill): string {
	return readPythonSkillProjectName(skill).replaceAll("_", "-").toLowerCase();
}

type KernelReadiness = { ready: false } | { ready: true; stamped: boolean };

// Manifest first (a few file reads), interpreter check last: a stale manifest means a
// sync or rebuild follows anyway, so the spawn would be wasted.
async function kernelBaseReady(python: string, venv: string, runtimeIdentity: string): Promise<KernelReadiness> {
	if (!bootstrapBaseVersionCurrent(await readBootstrapVersion(venv), runtimeIdentity)) return { ready: false };
	return venvRuntimeReady(python, venv, runtimeIdentity);
}

async function kernelReady(
	python: string,
	venv: string,
	runtimeIdentity: string,
	pythonSkills: readonly BootstrapPythonSkill[],
): Promise<KernelReadiness> {
	if (!bootstrapVersionCurrent(await readBootstrapVersion(venv), runtimeIdentity, pythonSkills)) {
		return { ready: false };
	}
	return venvRuntimeReady(python, venv, runtimeIdentity);
}

function formatBootstrapFailure(error: unknown): Error {
	return new Error(
		`Failed to set up the Python kernel runtime. ${errorMessage(error)}\n` +
			"First-time setup needs internet to install uv, Python, prime-agent-runtime, and default Python packages; once set up, prime-agent runs offline. " +
			"Set PRIME_AGENT_KERNEL_PYTHON to a Python with a current prime-agent-runtime and default Python packages installed to skip auto-bootstrap.",
	);
}

async function ensureKernelPythonUncached(
	options: EnsureKernelPythonOptions,
	pythonSkills: readonly BootstrapPythonSkill[],
): Promise<string> {
	const override = process.env.PRIME_AGENT_KERNEL_PYTHON;
	if (override) {
		const python = path.resolve(expandHome(override));
		const missing: string[] = [];
		if (!(await hasPrimeAgentRuntime(python))) {
			missing.push(
				"a current prime-agent-runtime with callable rlm.run, rlm.host_request, and explicit harness CRUD methods",
			);
		}
		if (missing.length === 0) {
			const missingExtraImports = await missingRlmExtraImportLabels(python);
			if (missingExtraImports.length > 0) {
				missing.push(`default Python packages (${missingExtraImports.join(", ")})`);
			}
		}
		if (missing.length === 0 && pythonSkills.length > 0) {
			const missingPythonSkills = await missingPythonSkillImportLabels(python, options.pythonSkills ?? []);
			if (missingPythonSkills.length > 0) {
				reportProgress(
					options,
					`Warning: Python skills unavailable in PRIME_AGENT_KERNEL_PYTHON and will be disabled: ${missingPythonSkills.join(", ")}`,
				);
			}
		}
		if (missing.length === 0) {
			options.onResolved?.("override");
			return python;
		}
		throw new Error(`PRIME_AGENT_KERNEL_PYTHON points to a Python missing ${missing.join(" and ")}: ${python}`);
	}

	const venv = await resolveWritableKernelVenvDir();
	const python = path.join(venv, "bin", "python");
	const runtimeIdentity = await resolveRuntimeIdentity();
	const readiness = await kernelReady(python, venv, runtimeIdentity, pythonSkills);
	if (readiness.ready) {
		options.onResolved?.(readiness.stamped ? "stamped" : "verified");
		return python;
	}

	const releaseLock = await acquireBootstrapLock(venv, options);
	try {
		const lockedReadiness = await kernelReady(python, venv, runtimeIdentity, pythonSkills);
		if (lockedReadiness.ready) {
			options.onResolved?.(lockedReadiness.stamped ? "stamped" : "verified");
			return python;
		}
		if ((await kernelBaseReady(python, venv, runtimeIdentity)).ready) {
			await syncPythonSkills(await ensureUv(options), venv, python, runtimeIdentity, pythonSkills, options);
			options.onResolved?.("synced");
			return python;
		}

		const hadVenv = existsSync(venv);
		reportProgress(options, "› setting up python kernel (one-time, ~30s)…");
		if (hadVenv) {
			reportProgress(options, "rebuilding kernel venv");
			await rm(venv, { recursive: true, force: true });
		}

		await bootstrapVenv(venv, pythonSkills, options);
	} catch (error) {
		throw formatBootstrapFailure(error);
	} finally {
		await releaseLock().catch(() => undefined);
	}

	reportProgress(options, "✓ ready");
	options.onResolved?.("bootstrapped");
	return python;
}

export function ensureKernelPython(options: EnsureKernelPythonOptions = {}): Promise<string> {
	const pythonSkills = normalizePythonSkills(options.pythonSkills);
	const key = ensureKernelPythonKey(pythonSkills);
	if (inFlightEnsureKernelPython?.key === key) return inFlightEnsureKernelPython.promise;

	const promise = ensureKernelPythonUncached(options, pythonSkills).finally(() => {
		if (inFlightEnsureKernelPython?.promise === promise) inFlightEnsureKernelPython = null;
	});
	inFlightEnsureKernelPython = { key, promise };
	return promise;
}
