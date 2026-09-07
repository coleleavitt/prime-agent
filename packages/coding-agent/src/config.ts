import { spawnSync } from "child_process";
import { createHash } from "crypto";
import {
	accessSync,
	appendFileSync,
	chmodSync,
	constants,
	existsSync,
	mkdirSync,
	readFileSync,
	realpathSync,
	renameSync,
	rmSync,
	statSync,
	writeFileSync,
} from "fs";
import { homedir } from "os";
import { basename, dirname, join, resolve, sep, win32 } from "path";
import { lockSync } from "proper-lockfile";
import { fileURLToPath } from "url";
import { gzipSync } from "zlib";
import { shouldUseWindowsShell } from "./utils/child-process.js";
import { normalizeSocketPath } from "./utils/daemon-socket-path.js";

// =============================================================================
// Package Detection
// =============================================================================

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

/**
 * Detect if we're running as a Bun compiled binary.
 * Bun binaries have import.meta.url containing "$bunfs", "~BUN", or "%7EBUN" (Bun's virtual filesystem path)
 */
export const isBunBinary =
	import.meta.url.includes("$bunfs") || import.meta.url.includes("~BUN") || import.meta.url.includes("%7EBUN");

/** Detect if Bun is the runtime (compiled binary or bun run) */
export const isBunRuntime = !!process.versions.bun;

export const SELF_UPDATE_INTERACTIVE_CHILD_ENV = "PRIME_AGENT_INTERACTIVE_SELF_UPDATE";
export const SELF_UPDATE_NOT_ATTEMPTED_EXIT_CODE = 75;

// =============================================================================
// Install Method Detection
// =============================================================================

export type InstallMethod = "bun-binary" | "homebrew" | "npm" | "pnpm" | "yarn" | "bun" | "unknown";

interface SelfUpdateCommandStep {
	command: string;
	args: string[];
	display: string;
}

export interface SelfUpdateCommand extends SelfUpdateCommandStep {
	steps?: SelfUpdateCommandStep[];
}

function makeSelfUpdateCommand(
	installStep: SelfUpdateCommandStep,
	uninstallStep?: SelfUpdateCommandStep,
	options: { uninstallAfterInstall?: boolean } = {},
): SelfUpdateCommand {
	if (!uninstallStep) return installStep;
	if (options.uninstallAfterInstall) {
		return {
			...installStep,
			display: `${installStep.display} && ${uninstallStep.display}`,
			steps: [installStep, uninstallStep],
		};
	}
	return {
		...installStep,
		display: `${uninstallStep.display} && ${installStep.display}`,
		steps: [uninstallStep, installStep],
	};
}

function makeSelfUpdateCommandStep(command: string, args: string[]): SelfUpdateCommandStep {
	return {
		command,
		args,
		display: [command, ...args].map((arg) => (/\s/.test(arg) ? `"${arg}"` : arg)).join(" "),
	};
}

export function detectInstallMethod(): InstallMethod {
	if (isBunBinary) {
		return "bun-binary";
	}
	if (isHomebrewInstall()) {
		return "homebrew";
	}

	const resolvedPath = `${__dirname}\0${process.execPath || ""}`.toLowerCase().replace(/\\/g, "/");

	if (resolvedPath.includes("/pnpm/") || resolvedPath.includes("/.pnpm/")) {
		return "pnpm";
	}
	if (resolvedPath.includes("/yarn/") || resolvedPath.includes("/.yarn/")) {
		return "yarn";
	}
	if (isBunRuntime || resolvedPath.includes("/install/global/node_modules/")) {
		return "bun";
	}
	if (resolvedPath.includes("/npm/") || resolvedPath.includes("/node_modules/")) {
		return "npm";
	}

	return "unknown";
}

function isHomebrewInstall(): boolean {
	const packageDir = getPackageDir().toLowerCase().replace(/\\/g, "/");
	return packageDir.includes("/cellar/") && packageDir.includes("/libexec/lib/node_modules/");
}

function getInferredNpmInstall(): { root: string; prefix: string } | undefined {
	const packageDir = getPackageDir();
	const path = process.platform === "win32" || packageDir.includes("\\") ? win32 : { basename, dirname };
	const parent = path.dirname(packageDir);
	let root: string | undefined;
	if (path.basename(parent).startsWith("@") && path.basename(path.dirname(parent)) === "node_modules") {
		root = path.dirname(parent);
	} else if (path.basename(parent) === "node_modules") {
		root = parent;
	}
	if (!root) return undefined;
	const rootParent = path.dirname(root);
	if (path.basename(rootParent) === "lib") return { root, prefix: path.dirname(rootParent) };
	// Windows global npm prefixes use `<prefix>\\node_modules`, which is
	// indistinguishable from local project installs by path shape alone. Do not
	// infer unsupported Windows custom prefixes without `npm root -g` evidence.
	return undefined;
}

function isDirectPackageArtifactSpec(updateSpec: string): boolean {
	const spec = updateSpec.trim().toLowerCase();
	return (
		spec.startsWith("http://") ||
		spec.startsWith("https://") ||
		spec.startsWith("file:") ||
		spec.endsWith(".tgz") ||
		spec.endsWith(".tar.gz")
	);
}

function getDefaultUpdatePackageName(installedPackageName: string, updateSpec: string): string {
	if (isDirectPackageArtifactSpec(updateSpec)) {
		return installedPackageName;
	}
	return updateSpec;
}

function getSelfUpdateCommandForMethod(
	method: InstallMethod,
	installedPackageName: string,
	updateSpec = installedPackageName,
	npmCommand?: string[],
	updatePackageName = getDefaultUpdatePackageName(installedPackageName, updateSpec),
): SelfUpdateCommand | undefined {
	const uninstallAfterInstall = isDirectPackageArtifactSpec(updateSpec);
	switch (method) {
		case "bun-binary":
		case "homebrew":
			return undefined;
		case "pnpm":
			return makeSelfUpdateCommand(
				makeSelfUpdateCommandStep("pnpm", ["install", "-g", updateSpec]),
				updatePackageName === installedPackageName
					? undefined
					: makeSelfUpdateCommandStep("pnpm", ["remove", "-g", installedPackageName]),
				{ uninstallAfterInstall },
			);
		case "yarn":
			return makeSelfUpdateCommand(
				makeSelfUpdateCommandStep("yarn", ["global", "add", updateSpec]),
				updatePackageName === installedPackageName
					? undefined
					: makeSelfUpdateCommandStep("yarn", ["global", "remove", installedPackageName]),
				{ uninstallAfterInstall },
			);
		case "bun":
			return makeSelfUpdateCommand(
				makeSelfUpdateCommandStep("bun", ["install", "-g", updateSpec]),
				updatePackageName === installedPackageName
					? undefined
					: makeSelfUpdateCommandStep("bun", ["uninstall", "-g", installedPackageName]),
				{ uninstallAfterInstall },
			);
		case "npm": {
			const [command = "npm", ...npmArgs] = npmCommand ?? [];
			const inferred = npmCommand?.length ? undefined : getInferredNpmInstall();
			const prefixArgs = [...npmArgs, ...(inferred ? ["--prefix", inferred.prefix] : [])];
			const installStep = makeSelfUpdateCommandStep(command, [...prefixArgs, "install", "-g", updateSpec]);
			const uninstallStep =
				updatePackageName === installedPackageName
					? undefined
					: makeSelfUpdateCommandStep(command, [...prefixArgs, "uninstall", "-g", installedPackageName]);
			return makeSelfUpdateCommand(installStep, uninstallStep, { uninstallAfterInstall });
		}
		case "unknown":
			return undefined;
	}
}

function readCommandOutput(
	command: string,
	args: string[],
	options: { requireSuccess?: boolean } = {},
): string | undefined {
	const result = spawnSync(command, args, {
		encoding: "utf-8",
		stdio: ["ignore", "pipe", "pipe"],
		shell: shouldUseWindowsShell(command),
	});
	if (result.status === 0) return result.stdout.trim() || undefined;
	if (options.requireSuccess) {
		const reason = result.error?.message || result.stderr.trim() || `exit code ${result.status ?? "unknown"}`;
		throw new Error(`Failed to run ${[command, ...args].join(" ")}: ${reason}`);
	}
	return undefined;
}

function getGlobalPackageRoots(method: InstallMethod, _packageName: string, npmCommand?: string[]): string[] {
	switch (method) {
		case "npm": {
			const configured = !!npmCommand?.length;
			const [command = "npm", ...npmArgs] = npmCommand ?? [];
			if (configured && command === "bun") {
				const bunBin = readCommandOutput(command, [...npmArgs, "pm", "bin", "-g"], {
					requireSuccess: true,
				});
				const roots = [join(homedir(), ".bun", "install", "global", "node_modules")];
				if (bunBin) {
					roots.push(join(dirname(bunBin), "install", "global", "node_modules"));
				}
				return roots;
			}
			const root = readCommandOutput(command, [...npmArgs, "root", "-g"], {
				requireSuccess: configured,
			});
			const inferred = configured ? undefined : getInferredNpmInstall();
			return [root, inferred?.root].filter((x): x is string => !!x);
		}
		case "pnpm": {
			const root = readCommandOutput("pnpm", ["root", "-g"]);
			return root ? [root, dirname(root)] : [];
		}
		case "yarn": {
			const dir = readCommandOutput("yarn", ["global", "dir"]);
			return dir ? [dir, join(dir, "node_modules")] : [];
		}
		case "bun": {
			const bunBin = readCommandOutput("bun", ["pm", "bin", "-g"]);
			const roots = [join(homedir(), ".bun", "install", "global", "node_modules")];
			if (bunBin) {
				roots.push(join(dirname(bunBin), "install", "global", "node_modules"));
			}
			return roots;
		}
		case "bun-binary":
		case "homebrew":
		case "unknown":
			return [];
	}
}

function normalizeExistingPathForComparison(path: string): string | undefined {
	const resolvedPath = resolve(path);
	if (!existsSync(resolvedPath)) {
		return undefined;
	}
	let normalizedPath: string;
	try {
		normalizedPath = realpathSync(resolvedPath);
	} catch {
		return undefined;
	}
	if (process.platform === "win32") {
		normalizedPath = normalizedPath.toLowerCase();
	}
	return normalizedPath;
}

function isSelfUpdatePathWritable(): boolean {
	const packageDir = getPackageDir();
	try {
		accessSync(packageDir, constants.W_OK);
		accessSync(dirname(packageDir), constants.W_OK);
		return true;
	} catch {
		return false;
	}
}

function isManagedByGlobalPackageManager(method: InstallMethod, packageName: string, npmCommand?: string[]): boolean {
	const packageDir = normalizeExistingPathForComparison(getPackageDir());
	return (
		!!packageDir &&
		getGlobalPackageRoots(method, packageName, npmCommand).some((root) => {
			const normalizedRoot = normalizeExistingPathForComparison(root);
			return (
				!!normalizedRoot &&
				packageDir.startsWith(normalizedRoot.endsWith(sep) ? normalizedRoot : `${normalizedRoot}${sep}`)
			);
		})
	);
}

export function getSelfUpdateCommand(
	packageName: string,
	npmCommand?: string[],
	updateSpec = packageName,
	updatePackageName = getDefaultUpdatePackageName(packageName, updateSpec),
): SelfUpdateCommand | undefined {
	const method = detectInstallMethod();
	const command = getSelfUpdateCommandForMethod(method, packageName, updateSpec, npmCommand, updatePackageName);
	if (!command || !isManagedByGlobalPackageManager(method, packageName, npmCommand) || !isSelfUpdatePathWritable()) {
		return undefined;
	}
	return command;
}

export function getSelfUpdateUnavailableInstruction(
	packageName: string,
	npmCommand?: string[],
	updateSpec = packageName,
	updatePackageName = getDefaultUpdatePackageName(packageName, updateSpec),
): string {
	const method = detectInstallMethod();
	if (method === "bun-binary") {
		return `Download from: https://github.com/PrimeIntellect-ai/prime-agent/releases/latest`;
	}
	if (method === "homebrew") {
		return `Update with: brew upgrade ${APP_NAME}`;
	}
	const command = getSelfUpdateCommandForMethod(method, packageName, updateSpec, npmCommand, updatePackageName);
	if (command) {
		if (isManagedByGlobalPackageManager(method, packageName, npmCommand) && !isSelfUpdatePathWritable()) {
			return `This installation is managed by a global ${method} install, but the install path is not writable. Update it yourself with: ${command.display}`;
		}
		return `This installation is not managed by a global ${method} install. Update it with the package manager, wrapper, or source checkout that provides it.`;
	}
	return `Update ${updateSpec} using the package manager, wrapper, or source checkout that provides this installation.`;
}

export function getUpdateInstruction(packageName: string): string {
	const method = detectInstallMethod();
	const command = getSelfUpdateCommandForMethod(method, packageName);
	if (command) {
		return `Run: ${command.display}`;
	}
	return getSelfUpdateUnavailableInstruction(packageName);
}

// =============================================================================
// Package Asset Paths (shipped with executable)
// =============================================================================

/**
 * Get the base directory for resolving package assets (themes, package.json, README.md, CHANGELOG.md).
 * - For Bun binary: returns the directory containing the executable
 * - For Node.js (dist/): returns __dirname (the dist/ directory)
 * - For tsx (src/): returns parent directory (the package root)
 */
export function getPackageDir(): string {
	// Allow override via environment variable (useful for Nix/Guix where store paths tokenize poorly)
	const envDir = process.env.PI_PACKAGE_DIR;
	if (envDir) {
		if (envDir === "~") return homedir();
		if (envDir.startsWith("~/")) return homedir() + envDir.slice(1);
		return envDir;
	}

	if (isBunBinary) {
		// Bun binary: process.execPath points to the compiled executable
		return dirname(process.execPath);
	}
	// Node.js: walk up from __dirname until we find package.json
	let dir = __dirname;
	while (dir !== dirname(dir)) {
		if (existsSync(join(dir, "package.json"))) {
			return dir;
		}
		dir = dirname(dir);
	}
	// Fallback (shouldn't happen)
	return __dirname;
}

/**
 * Get path to built-in themes directory (shipped with package)
 * - For Bun binary: theme/ next to executable
 * - For Node.js (dist/): dist/modes/interactive/theme/
 * - For tsx (src/): src/modes/interactive/theme/
 */
export function getThemesDir(): string {
	if (isBunBinary) {
		return join(getPackageDir(), "theme");
	}
	// Theme is in modes/interactive/theme/ relative to src/ or dist/
	const packageDir = getPackageDir();
	const srcOrDist = existsSync(join(packageDir, "src")) ? "src" : "dist";
	return join(packageDir, srcOrDist, "modes", "interactive", "theme");
}

/**
 * Get path to HTML export template directory (shipped with package)
 * - For Bun binary: export-html/ next to executable
 * - For Node.js (dist/): dist/core/export-html/
 * - For tsx (src/): src/core/export-html/
 */
export function getExportTemplateDir(): string {
	if (isBunBinary) {
		return join(getPackageDir(), "export-html");
	}
	const packageDir = getPackageDir();
	const srcOrDist = existsSync(join(packageDir, "src")) ? "src" : "dist";
	return join(packageDir, srcOrDist, "core", "export-html");
}

/** Get path to package.json */
export function getPackageJsonPath(): string {
	return join(getPackageDir(), "package.json");
}

/** Get path to docs directory */
export function getDocsPath(): string {
	return resolve(join(getPackageDir(), "docs"));
}

/** Get path to CHANGELOG.md */
export function getChangelogPath(): string {
	return resolve(join(getPackageDir(), "CHANGELOG.md"));
}

/**
 * Get path to built-in interactive assets directory.
 * - For Bun binary: assets/ next to executable
 * - For Node.js (dist/): dist/modes/interactive/assets/
 * - For tsx (src/): src/modes/interactive/assets/
 */
export function getInteractiveAssetsDir(): string {
	if (isBunBinary) {
		return join(getPackageDir(), "assets");
	}
	const packageDir = getPackageDir();
	const srcOrDist = existsSync(join(packageDir, "src")) ? "src" : "dist";
	return join(packageDir, srcOrDist, "modes", "interactive", "assets");
}

/** Get path to a bundled interactive asset */
export function getBundledInteractiveAssetPath(name: string): string {
	return join(getInteractiveAssetsDir(), name);
}

/**
 * Get the directory containing built-in skills shipped with the package.
 * - For Bun binary: skills/ next to executable
 * - For Node.js (dist/): dist/skills/
 * - For tsx (src/): skills/ at the package root
 */
export function getBundledSkillsDir(): string {
	if (isBunBinary) {
		return join(getPackageDir(), "skills");
	}
	const packageDir = getPackageDir();
	// Source checkouts (tsx) keep built-in skills at the package root; built
	// packages copy them to dist/skills. Decide by whether src/ is present so a
	// stale dist/ from a prior build never shadows live source edits.
	const isSourceCheckout = existsSync(join(packageDir, "src"));
	return isSourceCheckout ? join(packageDir, "skills") : join(packageDir, "dist", "skills");
}

// =============================================================================
// App Config (from package.json piConfig)
// =============================================================================

interface PackageJson {
	name?: string;
	version?: string;
	piConfig?: {
		name?: string;
		configDir?: string;
	};
}

const pkg = JSON.parse(readFileSync(getPackageJsonPath(), "utf-8")) as PackageJson;

const piConfigName: string | undefined = pkg.piConfig?.name;
const envPrefix =
	(piConfigName || "pi")
		.toUpperCase()
		.replace(/[^A-Z0-9]+/g, "_")
		.replace(/^_+|_+$/g, "") || "PI";
export const PACKAGE_NAME: string = pkg.name || "@earendil-works/pi-coding-agent";
export const APP_NAME: string = piConfigName || "pi";
export const APP_TITLE: string = piConfigName ? APP_NAME : "π";
export const CONFIG_DIR_NAME: string = pkg.piConfig?.configDir || ".prime/agent";
export const VERSION: string = pkg.version || "0.0.0";

// e.g., PI_CODING_AGENT_DIR or PRIME_AGENT_CODING_AGENT_DIR
export const ENV_AGENT_DIR = `${envPrefix}_CODING_AGENT_DIR`;
export const ENV_SESSION_DIR = `${envPrefix}_SESSION_DIR`;
export const ENV_LEGACY_SESSION_DIR = `${envPrefix}_CODING_AGENT_SESSION_DIR`;

export function expandTildePath(path: string): string {
	if (path === "~") return homedir();
	if (path.startsWith("~/")) return homedir() + path.slice(1);
	return path;
}

const DEFAULT_SHARE_VIEWER_URL = "https://pi.dev/session/";

/** Get the share viewer URL for a gist ID */
export function getShareViewerUrl(gistId: string): string {
	const baseUrl = process.env.PI_SHARE_VIEWER_URL || DEFAULT_SHARE_VIEWER_URL;
	return `${baseUrl}#${gistId}`;
}

// =============================================================================
// User Config Paths (~/.prime/agent/*)
// =============================================================================

/** Get the agent config directory (e.g., ~/.prime/agent/) */
export function getAgentDir(): string {
	const envDir = process.env[ENV_AGENT_DIR];
	if (envDir) {
		return expandTildePath(envDir);
	}
	return join(homedir(), CONFIG_DIR_NAME);
}

/** Get path to user's custom themes directory */
export function getCustomThemesDir(): string {
	return join(getAgentDir(), "themes");
}

/** Directory where daemon and client diagnostic logs are written (e.g. ~/.prime/agent/logs/). */
export function getLogsDir(): string {
	return join(getAgentDir(), "logs");
}

/** Log file capturing client-side agent-open failures. */
export function getClientErrorLogPath(): string {
	return join(getLogsDir(), "client-errors.log");
}

export function getAgentTracesLogPath(): string {
	return join(getLogsDir(), "agent-traces.log");
}

/** Shared structured (JSON lines) log for client, daemon, and provider diagnostics. */
export function getAgentLogPath(): string {
	return join(getLogsDir(), "agent.jsonl");
}

/**
 * Log file for a daemon. The basename keeps it readable; a hash of the full
 * socket path makes it unique so two sockets that share a basename (e.g.
 * daemon.sock in different dirs) don't interleave into one file.
 */
export function getDaemonLogPath(socketPath: string): string {
	const normalized = normalizeSocketPath(socketPath);
	const hash = createHash("sha256").update(normalized).digest("hex").slice(0, 8);
	return join(getLogsDir(), `${basename(normalized)}.${hash}.log`);
}

export function getDaemonUpdateRestartManifestPath(socketPath: string, agentDir: string = getAgentDir()): string {
	const normalizedSocketPath = normalizeSocketPath(socketPath);
	const socketHash = createHash("sha256").update(normalizedSocketPath).digest("hex");
	return join(agentDir, "daemon-update-restarts", `${socketHash}.json`);
}

export function getLegacyDaemonUpdateRestartManifestPath(agentDir: string = getAgentDir()): string {
	return join(agentDir, "daemon-update-restart.json");
}

const MAX_LOG_BYTES = 5 * 1024 * 1024;
const DEFAULT_LOG_RETENTION = 5;
const MAX_LOG_RETENTION = 100;
const LOG_FILE_MODE = 0o600;
const LOG_DIRECTORY_MODE = 0o700;
const REDACTED = "[REDACTED]";

function configuredLogRetention(): number {
	const configured = process.env.PRIME_AGENT_LOG_RETENTION;
	if (configured === undefined) return DEFAULT_LOG_RETENTION;
	const parsed = Number.parseInt(configured, 10);
	if (!Number.isFinite(parsed)) return DEFAULT_LOG_RETENTION;
	return Math.min(MAX_LOG_RETENTION, Math.max(1, parsed));
}

/** Redact a small set of high-confidence credential forms before local persistence. */
export function redactLocalLog(message: string): string {
	return message
		.replace(/(\b(?:Bearer|Basic)\s+)[A-Za-z0-9._~+/=-]+/gi, `$1${REDACTED}`)
		.replace(
			/(\b(?:authorization|api[-_]?key|access[-_]?token|refresh[-_]?token|client[-_]?secret|password|token|cookie|set-cookie|code)\b\s*["']?\s*[:=]\s*["']?)([^\s"',;}]+)/gi,
			`$1${REDACTED}`,
		)
		.replace(/([?&](?:token|access_token|refresh_token|api_key|code)=)[^&#\s]+/gi, `$1${REDACTED}`)
		.replace(/(https?:\/\/)[^/@\s]+@/gi, `$1${REDACTED}@`)
		.replace(/\b(?:sk-(?:ant-)?[A-Za-z0-9_-]{16,}|gh[opusr]_[A-Za-z0-9]{16,})\b/g, REDACTED)
		.replace(/\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/g, REDACTED);
}

function prepareSecureLog(logPath: string): void {
	const logsDir = dirname(logPath);
	mkdirSync(logsDir, { recursive: true, mode: LOG_DIRECTORY_MODE });
	chmodSync(logsDir, LOG_DIRECTORY_MODE);
	if (!existsSync(logPath)) {
		try {
			writeFileSync(logPath, "", { flag: "wx", mode: LOG_FILE_MODE });
		} catch (error) {
			if (!existsSync(logPath)) throw error;
		}
	}
	chmodSync(logPath, LOG_FILE_MODE);
}

function rotateLog(logPath: string, retention: number): void {
	const compressedGenerations = Math.max(0, retention - 2);
	for (let generation = compressedGenerations + 1; generation < MAX_LOG_RETENTION; generation++) {
		rmSync(`${logPath}.old.${generation}.gz`, { force: true });
	}
	for (let generation = compressedGenerations - 1; generation >= 1; generation--) {
		const source = `${logPath}.old.${generation}.gz`;
		if (existsSync(source)) {
			const target = `${logPath}.old.${generation + 1}.gz`;
			renameSync(source, target);
			chmodSync(target, LOG_FILE_MODE);
		}
	}
	const previous = `${logPath}.old`;
	if (compressedGenerations > 0 && existsSync(previous)) {
		const compressed = `${logPath}.old.1.gz`;
		writeFileSync(compressed, gzipSync(readFileSync(previous)), { mode: LOG_FILE_MODE });
		chmodSync(compressed, LOG_FILE_MODE);
	}
	rmSync(previous, { force: true });
	if (retention > 1) {
		renameSync(logPath, previous);
		chmodSync(previous, LOG_FILE_MODE);
	} else {
		rmSync(logPath, { force: true });
	}
	writeFileSync(logPath, "", { flag: "wx", mode: LOG_FILE_MODE });
}

/**
 * Append a redacted line to a local diagnostic log. Writes and rotation share a
 * cross-process lock, files are owner-only, and retained generations are bounded.
 * The newest rotated file remains `<path>.old`; older generations are gzip files.
 * Best-effort: diagnostics must never throw into the caller.
 */
export function appendRotatingLog(
	logPath: string,
	message: string,
	maxBytes: number = MAX_LOG_BYTES,
	retention: number = configuredLogRetention(),
): void {
	let release: (() => void) | undefined;
	try {
		const lockTarget = `${logPath}.rotation-lock`;
		prepareSecureLog(lockTarget);
		for (let attempt = 0; attempt < 5; attempt++) {
			try {
				release = lockSync(lockTarget, { realpath: false, stale: 10_000 });
				break;
			} catch {
				if (attempt === 4) return;
				Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5 * (attempt + 1));
			}
		}
		prepareSecureLog(logPath);
		if (statSync(logPath).size > maxBytes) {
			rotateLog(logPath, Math.min(MAX_LOG_RETENTION, Math.max(1, Math.trunc(retention))));
		}
		appendFileSync(logPath, `${redactLocalLog(message)}\n`, { mode: LOG_FILE_MODE });
		chmodSync(logPath, LOG_FILE_MODE);
	} catch {
		// A read-only directory or unavailable lock must never break the caller.
	} finally {
		try {
			release?.();
		} catch {
			// Lock cleanup is best-effort with the diagnostic write.
		}
	}
}

/** Get path to auth.json */
export function getAuthPath(): string {
	return join(getAgentDir(), "auth.json");
}

/** Get path to cron jobs store */
export function getCronJobsPath(agentDir: string = getAgentDir()): string {
	return join(agentDir, "cron-jobs.json");
}

/** Get path to managed binaries directory (fd, rg) */
export function getBinDir(): string {
	return join(getAgentDir(), "bin");
}

/** Get path to sessions directory */
export function getSessionsDir(agentDir: string = getAgentDir()): string {
	const envDir = getSessionDirEnvOverride();
	if (envDir) {
		return envDir;
	}
	return join(agentDir, "sessions");
}

export function getSessionDirEnvOverride(): string | undefined {
	const envDir = process.env[ENV_SESSION_DIR] ?? process.env[ENV_LEGACY_SESSION_DIR];
	return envDir ? expandTildePath(envDir) : undefined;
}

/** Get path to debug log file */
export function getDebugLogPath(): string {
	return join(getAgentDir(), `${APP_NAME}-debug.log`);
}
