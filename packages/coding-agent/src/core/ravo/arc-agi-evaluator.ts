import { execFile, spawn } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { normalizeRefinementProposal } from "../refinement/refinement.js";
import { resolveKernelPython } from "../refinement/skill-dry-run.js";
import type { ControllerProposal, EvaluationAdapter } from "./controller.js";
import type { ExternalEvaluatorSuite } from "./external-evaluator.js";
import type { GateStatus, JsonValue } from "./reducer.js";

/**
 * ARC-AGI-3 deep evaluator.
 *
 * The artifact under evaluation is a Python `Agent` subclass for the
 * arcprize/ARC-AGI-3-Agents harness. Evaluation is outcome-based: the agent is
 * registered in the clone, one game is played through `uv run main.py`, and
 * the final scorecard decides the verdict. The deep score is the percentage of
 * levels completed, which is the natural-number `deep : A -> nat` oracle the
 * reducer ratchets on (see docs/ravo-arc-agi-evaluator.md).
 *
 * This benchmark is not part of the RAVO gate. It reaches `RavoRunService`
 * only as an `ExternalEvaluatorSuite`, so the whole ARC surface is this one
 * file plus the `{ kind: "arc-agi" }` member of `RavoRunRequest["evaluator"]`.
 */

export type ArcAgentArtifact = {
	/** Python module name written to `agents/templates/<agentName>.py`; also the `--agent` selector. */
	agentName: string;
	/** Full module source defining exactly one direct `Agent` subclass. */
	source: string;
};

export interface ArcRunnerArgs {
	command: string;
	args: string[];
	cwd: string;
	timeoutMs: number;
	signal: AbortSignal;
}

export interface ArcRunnerResult {
	stdout: string;
	stderr?: string;
	exitCode: number;
}

export type ArcRunner = (args: ArcRunnerArgs) => Promise<ArcRunnerResult>;

export interface ArcAgiEvaluatorOptions {
	/** Root of a working ARC-AGI-3-Agents clone (`main.py`, `agents/`, `.env`). */
	repoDir: string;
	/** Game id prefix passed as `--game` (for example `ls20`). */
	game: string;
	timeoutMs?: number;
	runner?: ArcRunner;
	id?: string;
}

export interface ArcEnvironmentScore {
	id: string;
	levelsCompleted: number;
	levelCount: number;
	actions: number;
	completed: boolean;
	state?: string;
}

export interface ArcScorecard {
	cardId?: string;
	levelsCompleted: number;
	totalLevels: number;
	actions: number;
	environments: ArcEnvironmentScore[];
}

export interface ArcEvaluationResult {
	status: GateStatus;
	score?: number;
	detail?: string;
	/** Parsed scorecard when the run produced one; absent on `error`. */
	scorecard?: ArcScorecard;
}

export const DEFAULT_ARC_TIMEOUT_MS = 10 * 60 * 1000;
const AGENT_NAME_PATTERN = /^[a-z][a-z0-9_]*$/;
const MANAGED_BEGIN = "# >>> ravo-arc-agi managed agent (generated; do not edit)";
const MANAGED_END = "# <<< ravo-arc-agi managed agent";
const CANDIDATE_HEADER = "# ravo-arc-agi candidate";
const SCORECARD_MARKERS = ["--- FINAL SCORECARD REPORT ---", "--- EXISTING SCORECARD REPORT ---"];
const CLASS_PATTERN = /^class\s+([A-Za-z_]\w*)\s*\(\s*(?:[\w.]+\.)?Agent\s*\)\s*:/m;

export class ArcEvaluationError extends Error {}

function asRecord(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function asNumber(value: unknown): number | undefined {
	return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function extractJsonObject(text: string, from: number): string | undefined {
	const start = text.indexOf("{", from);
	if (start < 0) return undefined;
	let depth = 0;
	let inString = false;
	let escaped = false;
	for (let index = start; index < text.length; index++) {
		const char = text[index];
		if (inString) {
			if (escaped) escaped = false;
			else if (char === "\\") escaped = true;
			else if (char === '"') inString = false;
			continue;
		}
		if (char === '"') inString = true;
		else if (char === "{") depth++;
		else if (char === "}") {
			depth--;
			if (depth === 0) return text.slice(start, index + 1);
		}
	}
	return undefined;
}

function parseEnvironment(value: unknown): ArcEnvironmentScore | undefined {
	const record = asRecord(value);
	if (!record || typeof record.id !== "string") return undefined;
	const runs = Array.isArray(record.runs) ? record.runs.map(asRecord) : [];
	const lastRun = runs.length > 0 ? runs[runs.length - 1] : undefined;
	return {
		id: record.id,
		levelsCompleted: asNumber(record.levels_completed) ?? 0,
		levelCount: asNumber(record.level_count) ?? 0,
		actions: asNumber(record.actions) ?? 0,
		completed: record.completed === true,
		...(lastRun && typeof lastRun.state === "string" ? { state: lastRun.state } : {}),
	};
}

/**
 * Parse the scorecard JSON that `main.py` logs after the game finishes.
 * Returns `undefined` when no scorecard report is present in the output.
 */
export function parseArcScorecard(stdout: string): ArcScorecard | undefined {
	let markerIndex = -1;
	for (const marker of SCORECARD_MARKERS) {
		const index = stdout.lastIndexOf(marker);
		if (index > markerIndex) markerIndex = index;
	}
	if (markerIndex < 0) return undefined;
	const json = extractJsonObject(stdout, markerIndex);
	if (!json) return undefined;
	let parsed: unknown;
	try {
		parsed = JSON.parse(json);
	} catch {
		return undefined;
	}
	const record = asRecord(parsed);
	if (!record || !Array.isArray(record.environments)) return undefined;
	const environments = record.environments
		.map(parseEnvironment)
		.filter((item): item is ArcEnvironmentScore => item !== undefined);
	const summedLevels = environments.reduce((sum, item) => sum + item.levelCount, 0);
	const summedCompleted = environments.reduce((sum, item) => sum + item.levelsCompleted, 0);
	const summedActions = environments.reduce((sum, item) => sum + item.actions, 0);
	return {
		...(typeof record.card_id === "string" ? { cardId: record.card_id } : {}),
		levelsCompleted: asNumber(record.total_levels_completed) ?? summedCompleted,
		totalLevels: asNumber(record.total_levels) ?? summedLevels,
		actions: asNumber(record.total_actions) ?? summedActions,
		environments,
	};
}

/** Deep score: whole-percent levels completed, a safe natural in [0, 100]. */
export function arcScorecardScore(scorecard: ArcScorecard): number {
	if (scorecard.totalLevels <= 0) return 0;
	const ratio = Math.min(scorecard.levelsCompleted, scorecard.totalLevels) / scorecard.totalLevels;
	return Math.round(100 * ratio);
}

/** The `--agent` selector `main.py` accepts for a candidate module. */
export function arcAgentSelector(agentName: string): string {
	return agentName.toLowerCase();
}

/** Structural check of a candidate: name pattern, non-empty source, one direct `Agent` subclass. Throws ArcEvaluationError. */
export function validateArcArtifact(artifact: unknown): ArcAgentArtifact {
	const record = asRecord(artifact);
	if (!record) throw new ArcEvaluationError("artifact must be an object with agentName and source");
	const { agentName, source } = record;
	if (typeof agentName !== "string" || !AGENT_NAME_PATTERN.test(agentName))
		throw new ArcEvaluationError("artifact.agentName must match /^[a-z][a-z0-9_]*$/");
	if (typeof source !== "string" || source.trim().length === 0)
		throw new ArcEvaluationError("artifact.source must be non-empty Python source");
	if (!CLASS_PATTERN.test(source))
		throw new ArcEvaluationError("artifact.source must define a direct `Agent` subclass (class X(Agent):)");
	return { agentName, source };
}

function agentClassName(source: string): string {
	const match = CLASS_PATTERN.exec(source);
	if (!match?.[1]) throw new ArcEvaluationError("artifact.source must define a direct `Agent` subclass");
	return match[1];
}

function managedBlock(agentName: string, className: string): string {
	const alias = `_RavoArcAgent_${agentName}`;
	return [
		MANAGED_BEGIN,
		`from .templates.${agentName} import ${className} as ${alias}`,
		`AVAILABLE_AGENTS["${arcAgentSelector(agentName)}"] = ${alias}`,
		MANAGED_END,
		"",
	].join("\n");
}

function replaceManagedBlock(existing: string, block: string): string {
	const begin = existing.indexOf(MANAGED_BEGIN);
	const end = existing.indexOf(MANAGED_END);
	if (begin >= 0 && end > begin) {
		const after = existing.indexOf("\n", end);
		const tail = after < 0 ? "" : existing.slice(after + 1);
		return `${existing.slice(0, begin)}${block}${tail}`;
	}
	const body = existing.endsWith("\n") || existing.length === 0 ? existing : `${existing}\n`;
	return `${body}\n${block}`;
}

/**
 * Write the candidate module and register it in `agents/__init__.py` so
 * `main.py --agent=<agentName>` resolves it. Returns the module path.
 */
export async function installArcAgent(repoDir: string, artifact: ArcAgentArtifact): Promise<string> {
	const templatesDir = path.join(repoDir, "agents", "templates");
	const initPath = path.join(repoDir, "agents", "__init__.py");
	const modulePath = path.join(templatesDir, `${artifact.agentName}.py`);
	let existing: string | undefined;
	try {
		existing = await readFile(modulePath, "utf8");
	} catch {
		existing = undefined;
	}
	if (existing !== undefined && !existing.startsWith(CANDIDATE_HEADER))
		throw new ArcEvaluationError(`refusing to overwrite non-candidate module ${modulePath}`);
	let init: string;
	try {
		init = await readFile(initPath, "utf8");
	} catch (error) {
		throw new ArcEvaluationError(
			`ARC-AGI-3 clone at ${repoDir} has no agents/__init__.py: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
	await mkdir(templatesDir, { recursive: true });
	const header = `${CANDIDATE_HEADER}: ${artifact.agentName}\n`;
	const source = artifact.source.endsWith("\n") ? artifact.source : `${artifact.source}\n`;
	await writeFile(modulePath, `${header}${source}`, "utf8");
	await writeFile(
		initPath,
		replaceManagedBlock(init, managedBlock(artifact.agentName, agentClassName(source))),
		"utf8",
	);
	return modulePath;
}

const TRACEBACK_PATTERN =
	/Traceback \(most recent call last\):[\s\S]*?^(\w+(?:\.\w+)*(?:Error|Exception|Warning)): (.*)$/m;

function tail(text: string | undefined, lines = 12): string {
	if (!text) return "";
	const parts = text.trimEnd().split("\n");
	return parts.slice(-lines).join("\n");
}

/** Map a finished runner invocation to a gate verdict and outcome score. */
export function interpretArcRun(result: ArcRunnerResult, game: string): ArcEvaluationResult {
	const scorecard = parseArcScorecard(result.stdout);
	const combined = `${result.stdout}\n${result.stderr ?? ""}`;
	const traceback = TRACEBACK_PATTERN.exec(combined);
	if (!scorecard) {
		const reason = traceback ? `${traceback[1]}: ${traceback[2]}` : tail(result.stderr) || tail(result.stdout);
		return {
			status: "error",
			detail: `no scorecard in output for game ${game} (exit ${result.exitCode})${reason ? `: ${reason}` : ""}`,
		};
	}
	const score = arcScorecardScore(scorecard);
	const summary = `${scorecard.levelsCompleted}/${scorecard.totalLevels} levels in ${scorecard.actions} actions for ${game}`;
	if (result.exitCode !== 0) {
		return {
			status: "fail",
			score,
			detail: `run exited ${result.exitCode}; ${summary}; ${tail(result.stderr, 5)}`,
			scorecard,
		};
	}
	if (traceback) {
		return { status: "fail", score, detail: `agent raised ${traceback[1]}: ${traceback[2]}; ${summary}`, scorecard };
	}
	return { status: "pass", score, detail: summary, scorecard };
}

/** Default runner: `uv run main.py ...` in the clone, killed on timeout or abort. */
export const defaultArcRunner: ArcRunner = ({ command, args, cwd, timeoutMs, signal }) =>
	new Promise<ArcRunnerResult>((resolve, reject) => {
		if (signal.aborted) {
			reject(new ArcEvaluationError(`${command} aborted`));
			return;
		}
		const child = spawn(command, args, { cwd, stdio: ["ignore", "pipe", "pipe"], env: process.env });
		let stdout = "";
		let stderr = "";
		let timedOut = false;
		let settled = false;
		const timer = setTimeout(() => {
			timedOut = true;
			child.kill("SIGKILL");
		}, timeoutMs);
		const onAbort = () => child.kill("SIGKILL");
		signal.addEventListener("abort", onAbort, { once: true });
		const finish = (fn: () => void) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			signal.removeEventListener("abort", onAbort);
			fn();
		};
		child.stdout.on("data", (chunk: Buffer) => {
			stdout += chunk.toString();
		});
		child.stderr.on("data", (chunk: Buffer) => {
			stderr += chunk.toString();
		});
		child.on("error", (error) => finish(() => reject(error)));
		child.on("close", (code, signalName) =>
			finish(() => {
				if (timedOut) reject(new ArcEvaluationError(`${command} timed out after ${timeoutMs}ms`));
				else if (signal.aborted) reject(new ArcEvaluationError(`${command} aborted`));
				else resolve({ stdout, stderr, exitCode: code ?? (signalName ? 128 : 1) });
			}),
		);
	});

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, signal: AbortSignal): Promise<T> {
	return new Promise<T>((resolve, reject) => {
		if (signal.aborted) {
			reject(new ArcEvaluationError("ARC-AGI-3 run aborted"));
			return;
		}
		let done = false;
		const timer = setTimeout(() => {
			if (done) return;
			done = true;
			reject(new ArcEvaluationError(`ARC-AGI-3 run timed out after ${timeoutMs}ms`));
		}, timeoutMs);
		const onAbort = () => {
			if (done) return;
			done = true;
			clearTimeout(timer);
			reject(new ArcEvaluationError("ARC-AGI-3 run aborted"));
		};
		signal.addEventListener("abort", onAbort, { once: true });
		promise.then(
			(value) => {
				if (done) return;
				done = true;
				clearTimeout(timer);
				signal.removeEventListener("abort", onAbort);
				resolve(value);
			},
			(error: unknown) => {
				if (done) return;
				done = true;
				clearTimeout(timer);
				signal.removeEventListener("abort", onAbort);
				reject(error);
			},
		);
	});
}

/**
 * Evaluate one candidate agent by playing `game` for real. No LLM is involved:
 * the child call spends zero tokens and its verdict is the recorded outcome.
 */
export async function evaluateArcAgent(
	options: ArcAgiEvaluatorOptions,
	artifact: unknown,
	signal: AbortSignal,
): Promise<ArcEvaluationResult> {
	const timeoutMs = options.timeoutMs ?? DEFAULT_ARC_TIMEOUT_MS;
	const runner = options.runner ?? defaultArcRunner;
	try {
		const validated = validateArcArtifact(artifact);
		if (signal.aborted) throw new ArcEvaluationError("ARC-AGI-3 run aborted");
		await installArcAgent(options.repoDir, validated);
		if (signal.aborted) throw new ArcEvaluationError("ARC-AGI-3 run aborted");
		const result = await withTimeout(
			runner({
				command: "uv",
				args: ["run", "main.py", `--agent=${arcAgentSelector(validated.agentName)}`, `--game=${options.game}`],
				cwd: options.repoDir,
				timeoutMs,
				signal,
			}),
			timeoutMs,
			signal,
		);
		return interpretArcRun(result, options.game);
	} catch (error) {
		return { status: "error", detail: error instanceof Error ? error.message : String(error) };
	}
}

export function createArcAgiEvaluator(options: ArcAgiEvaluatorOptions): EvaluationAdapter<ArcAgentArtifact> {
	return {
		id: options.id ?? `arc-agi:${options.game}`,
		kind: "deep",
		evaluate: async ({ proposal }, callOptions) => ({
			status: "completed",
			value: await evaluateArcAgent(options, proposal.artifact, callOptions.signal),
			tokens: 0,
		}),
	};
}

/* ---------------------------------------------------------------------------
 * ExternalEvaluatorSuite: the whole ARC-AGI-3 surface RavoRunService consumes.
 * ------------------------------------------------------------------------ */

const ARC_CRITERION_IDS = ["arc:no-crash", "arc:all-levels"] as const;
const ARC_REFERENCE_FILES = ["agents/agent.py", "agents/templates/random_agent.py"] as const;
const ARC_REFERENCE_MAX_CHARS = 16_000;
const ARC_PROMPT_INSTRUCTION =
	'The candidate is a Python ARC-AGI-3 agent module: include "arcAgent": { "agentName": "snake_case_module_name", "source": "full module source" }. The module lives in agents/templates/, so import the base class with `from ..agent import Agent` exactly as the reference does. "edits" may be [].';

export interface ArcEvaluatorSuiteOptions extends ArcAgiEvaluatorOptions {
	/** Fast-screen pass mark, taken from the reducer config. */
	screenThreshold: number;
}

function recordOf(value: unknown): Record<string, unknown> {
	return asRecord(value) ?? {};
}

/** The ARC agent module a proposal carries: an explicit `arcAgent`, else the first skill edit's content. */
export function arcArtifactOfProposal(artifact: JsonValue): ArcAgentArtifact | undefined {
	const arcAgent = recordOf(recordOf(artifact).arcAgent);
	if (typeof arcAgent.agentName === "string" && typeof arcAgent.source === "string") {
		return { agentName: arcAgent.agentName, source: arcAgent.source };
	}
	const skill = normalizeRefinementProposal(artifact).edits.find(
		(edit) => edit.kind === "skill" && edit.action !== "delete" && edit.content,
	);
	if (!skill?.content) return undefined;
	return { agentName: (skill.id ?? skill.title ?? "agent").replace(/[^a-zA-Z0-9_]+/g, "_"), source: skill.content };
}

type ArcRunFn = (proposal: ControllerProposal<JsonValue>, signal: AbortSignal) => Promise<ArcEvaluationResult>;

/** One real game per proposal, shared by the deep gate and the outcome opponents. */
function memoizedArcRun(options: ArcAgiEvaluatorOptions): ArcRunFn {
	const pending = new Map<string, Promise<ArcEvaluationResult>>();
	return (proposal, signal) => {
		let shared = pending.get(proposal.id);
		if (!shared) {
			const artifact = arcArtifactOfProposal(proposal.artifact);
			shared = artifact
				? evaluateArcAgent(options, artifact, signal)
				: Promise.resolve({
						status: "error" as const,
						detail: "proposal carries no ARC agent source (arcAgent or a skill edit)",
					});
			pending.set(proposal.id, shared);
		}
		return shared;
	};
}

async function pythonSyntaxCheck(source: string, signal: AbortSignal): Promise<{ ok: boolean; detail: string }> {
	const python = resolveKernelPython() ?? "python3";
	return new Promise((resolve) => {
		const child = execFile(
			python,
			["-I", "-c", "import ast,sys; ast.parse(sys.stdin.read())"],
			{ timeout: 10_000, signal },
			(error, _stdout, stderr) => {
				if (!error) return resolve({ ok: true, detail: "ok" });
				const lines = String(stderr).trim().split("\n");
				resolve({ ok: false, detail: lines.at(-1) || error.message });
			},
		);
		child.stdin?.end(source);
	});
}

/** Deterministic screen for an ARC candidate: artifact shape plus a Python syntax check. No game is played. */
function arcFastAdapter(screenThreshold: number): EvaluationAdapter<JsonValue> {
	return {
		id: "fast:arc-artifact",
		kind: "fast",
		evaluate: async ({ proposal }, options) => {
			const artifact = arcArtifactOfProposal(proposal.artifact);
			let detail: string;
			let ok = false;
			try {
				if (!artifact) throw new Error("proposal carries no ARC agent source (arcAgent or a skill edit)");
				validateArcArtifact(artifact);
				const syntax = await pythonSyntaxCheck(artifact.source, options.signal);
				ok = syntax.ok;
				detail = syntax.ok ? `agent ${artifact.agentName} parses` : `agent ${artifact.agentName}: ${syntax.detail}`;
			} catch (error) {
				detail = error instanceof Error ? error.message : String(error);
			}
			const score = ok ? 100 : 0;
			return {
				status: "completed",
				value: { status: score >= screenThreshold ? "pass" : "fail", score, detail },
				tokens: 0,
			};
		},
	};
}

function arcDeepAdapter(run: ArcRunFn): EvaluationAdapter<JsonValue> {
	return {
		id: "deep:arc-agi",
		kind: "deep",
		evaluate: async ({ proposal }, options) => {
			const result = await run(proposal, options.signal);
			return {
				status: "completed",
				value: {
					status: result.status,
					...(result.score === undefined ? {} : { score: result.score }),
					...(result.detail === undefined ? {} : { detail: result.detail }),
				},
				tokens: 0,
			};
		},
	};
}

/**
 * Outcome opponents derived from the same game run: the agent must not crash,
 * and it must finish every level. Missing `arc:all-levels` costs one weight
 * unit at first; weakness pressure doubles it after a champion is accepted
 * without finishing, so later candidates cannot keep winning on partial games.
 */
function arcOpponents(run: ArcRunFn): EvaluationAdapter<JsonValue>[] {
	return [
		{
			id: "opponent:arc:no-crash",
			kind: "opponent",
			criterionId: "arc:no-crash",
			evaluate: async ({ proposal }, options) => {
				const result = await run(proposal, options.signal);
				return {
					status: "completed",
					value: { status: result.status === "pass" ? "pass" : "fail", detail: result.detail ?? result.status },
					tokens: 0,
				};
			},
		},
		{
			id: "opponent:arc:all-levels",
			kind: "opponent",
			criterionId: "arc:all-levels",
			evaluate: async ({ proposal }, options) => {
				const result = await run(proposal, options.signal);
				const card = result.scorecard;
				const done = card !== undefined && card.totalLevels > 0 && card.levelsCompleted === card.totalLevels;
				return {
					status: "completed",
					value: {
						status: done ? "pass" : "fail",
						detail: card
							? `${card.levelsCompleted}/${card.totalLevels} levels`
							: (result.detail ?? "no scorecard"),
					},
					tokens: 0,
				};
			},
		},
	];
}

/** The harness interface a candidate must satisfy, read from the clone so the child never has to search for it. */
async function arcPromptSection(repoDir: string): Promise<string> {
	const sections: string[] = [];
	let budget = ARC_REFERENCE_MAX_CHARS;
	for (const relative of ARC_REFERENCE_FILES) {
		try {
			const text = await readFile(path.join(repoDir, relative), "utf8");
			const clipped = text.length > budget ? `${text.slice(0, budget)}\n# ... clipped` : text;
			budget -= clipped.length;
			sections.push(`## ${relative}\n${clipped}`);
		} catch {
			sections.push(`## ${relative}\n(not readable)`);
		}
		if (budget <= 0) break;
	}
	return `<arc_reference>\n${sections.join("\n\n")}\n</arc_reference>\n\n${ARC_PROMPT_INSTRUCTION}`;
}

async function persistArcAgent(baseDir: string, runId: string, artifact: JsonValue): Promise<void> {
	const agent = arcArtifactOfProposal(artifact);
	if (!agent) return;
	const dir = path.join(baseDir, "ravo", "arc");
	await mkdir(dir, { recursive: true });
	await writeFile(path.join(dir, `${runId}-${agent.agentName}.py`), agent.source, "utf8");
}

/** Wire ARC-AGI-3 into a RAVO run as one external evaluator among many. */
export function createArcEvaluatorSuite(options: ArcEvaluatorSuiteOptions): ExternalEvaluatorSuite {
	const run = memoizedArcRun(options);
	return {
		criterionIds: ARC_CRITERION_IDS,
		fast: arcFastAdapter(options.screenThreshold),
		deep: arcDeepAdapter(run),
		opponents: arcOpponents(run),
		promptSection: () => arcPromptSection(options.repoDir),
		artifactFields: (record) => {
			const arcAgent = recordOf(record.arcAgent);
			return typeof arcAgent.agentName === "string" && typeof arcAgent.source === "string"
				? { arcAgent: { agentName: arcAgent.agentName, source: arcAgent.source } }
				: undefined;
		},
		persistCommitted: (baseDir, runId, artifact) => persistArcAgent(baseDir, runId, artifact),
	};
}
