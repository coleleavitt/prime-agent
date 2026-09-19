import path from "node:path";
import { toolforgeSrcRoots } from "../toolforge/ledger.js";

/**
 * Environments for the interpreters the refinement loop spawns. The program is
 * always run in isolated mode, which ignores every `PYTHON*` variable, so each
 * wrapper re-applies PYTHONPATH itself and the roots below are the whole of
 * `sys.path` beyond the interpreter's own. Roots are resolved to absolute paths
 * against `cwd` (default: this process's working directory) before they are
 * joined, so the directory the interpreter itself runs in never changes what a
 * relative root names.
 *
 * Three shapes, by who wrote the program and what it is asked:
 *
 * - `sanitizedPythonEnvironment`: a probe derived from tool output
 *   (`verifyObservedReplayCases`). PATH, HOME and LANG (on win32 also what
 *   CPython needs to start and open sockets, `WINDOWS_PYTHON_ENV_KEYS`), and
 *   only the explicit roots: nothing inherited can decide whether an observed
 *   failure reproduces.
 * - `skillImportEnvironment`: whether a skill import resolves, asked by the
 *   dry-run screen and by referee adjudication alike. The sanitized base, with
 *   the toolforge source roots and the host's own PYTHONPATH entries as
 *   explicit roots, because the kernel that will import the skill sees both.
 *   Both callers also run the interpreter in a fresh temporary working
 *   directory, so neither the environment nor the working directory can make
 *   them disagree about whether an import resolves.
 * - `inheritedPythonEnvironment`: a program the model wrote to run in the user's
 *   real environment (the toolforge exit test). The host environment, roots
 *   first on PYTHONPATH.
 */

export const SANITIZED_PYTHON_ENV_KEYS = ["PATH", "HOME", "LANG"] as const;

/**
 * Passed on win32 as well: without SYSTEMROOT, Winsock initialization fails
 * (WinError 10106) for any import that pulls in `socket` or `asyncio`, and HOME
 * is usually unset there.
 */
export const WINDOWS_PYTHON_ENV_KEYS = [
	"SYSTEMROOT",
	"WINDIR",
	"USERPROFILE",
	"TEMP",
	"TMP",
	"COMSPEC",
	"PATHEXT",
] as const;

export interface PythonEnvironmentOptions {
	/** The directory relative roots resolve against. Default `process.cwd()`. */
	cwd?: string;
	/** Default `process.platform`. On win32 variable names match case-insensitively, as Windows reads them. */
	platform?: NodeJS.Platform;
}

function pathModule(platform: NodeJS.Platform): path.PlatformPath {
	return platform === "win32" ? path.win32 : path.posix;
}

function sameKey(key: string, name: string, platform: NodeJS.Platform): boolean {
	return platform === "win32" ? key.toUpperCase() === name : key === name;
}

/** The value of `name` in `host`; on win32 the last spelling of it wins, as a later override would. */
function hostValue(host: NodeJS.ProcessEnv, name: string, platform: NodeJS.Platform): string | undefined {
	let value: string | undefined;
	for (const [key, entry] of Object.entries(host)) {
		if (entry !== undefined && sameKey(key, name, platform)) value = entry;
	}
	return value;
}

export function pythonPathEntries(value: string | undefined, platform: NodeJS.Platform = process.platform): string[] {
	return (value ?? "").split(pathModule(platform).delimiter).filter((entry) => entry.length > 0);
}

function explicitPythonPath(roots: readonly string[], options: PythonEnvironmentOptions): string | undefined {
	const platform = options.platform ?? process.platform;
	const paths = pathModule(platform);
	const base = options.cwd ?? process.cwd();
	const resolved = [...new Set(roots.filter((root) => root.length > 0).map((root) => paths.resolve(base, root)))];
	return resolved.length > 0 ? resolved.join(paths.delimiter) : undefined;
}

export function sanitizedPythonEnvironment(
	roots: readonly string[],
	host: NodeJS.ProcessEnv = process.env,
	options: PythonEnvironmentOptions = {},
): NodeJS.ProcessEnv {
	const platform = options.platform ?? process.platform;
	const keys =
		platform === "win32" ? [...SANITIZED_PYTHON_ENV_KEYS, ...WINDOWS_PYTHON_ENV_KEYS] : SANITIZED_PYTHON_ENV_KEYS;
	const env: NodeJS.ProcessEnv = {};
	for (const key of keys) {
		const value = hostValue(host, key, platform);
		if (value !== undefined) env[key] = value;
	}
	const pythonPath = explicitPythonPath(roots, options);
	if (pythonPath !== undefined) env.PYTHONPATH = pythonPath;
	return env;
}

export function skillImportEnvironment(
	roots: readonly string[] = [],
	host: NodeJS.ProcessEnv = process.env,
	options: PythonEnvironmentOptions = {},
): NodeJS.ProcessEnv {
	const platform = options.platform ?? process.platform;
	return sanitizedPythonEnvironment(
		[...roots, ...toolforgeSrcRoots(), ...pythonPathEntries(hostValue(host, "PYTHONPATH", platform), platform)],
		host,
		options,
	);
}

export function inheritedPythonEnvironment(
	roots: readonly string[],
	host: NodeJS.ProcessEnv = process.env,
	options: PythonEnvironmentOptions = {},
): NodeJS.ProcessEnv {
	const platform = options.platform ?? process.platform;
	const inherited = hostValue(host, "PYTHONPATH", platform);
	const env = Object.fromEntries(Object.entries(host).filter(([key]) => !sameKey(key, "PYTHONPATH", platform)));
	const pythonPath = explicitPythonPath([...roots, ...pythonPathEntries(inherited, platform)], options);
	return pythonPath === undefined ? env : { ...env, PYTHONPATH: pythonPath };
}
