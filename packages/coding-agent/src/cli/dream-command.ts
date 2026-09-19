import { APP_NAME } from "../config.js";
import {
	computeObjective,
	createSeededRng,
	DEFAULT_OBJECTIVE,
	DEFAULT_POLICY,
	DREAM_TASK_IDS,
	type DreamClock,
	type DreamLoopResult,
	type DreamResult,
	type DreamTaskId,
	type ExploreResult,
	getDreamDir,
	listTrees,
	policyId,
	type RecordedTree,
	type ReplayResult,
	readTree,
	resolveTask,
	runDreaming,
	runDreamLoop,
	runOnlineExploration,
	simulatePolicyWithSpan,
	type TreeSummary,
} from "../core/dream/index.js";

/**
 * `prime-agent dream` runs the Dream-RSI loop (Zheng et al., 2026) over a local,
 * deterministic scored task: it grows a discovery tree, freezes each tree into a
 * zero-cost replay simulator, and improves a typed, serializable exploration
 * policy by local search over its parameters.
 *
 * Everything here is argv parsing and printing; all of the framework lives under
 * `core/dream/`. The default `loop` on circle-packing spends no model tokens and
 * uses no network. The `--llm-proposer` / `--llm-dreamer` flags require an
 * in-session agent handler and are rejected by this standalone CLI, which imports
 * only `core/dream/index.js` (never the LLM path) and so cannot spend a token or
 * open a socket.
 */

const DEFAULT_TASK: DreamTaskId = "circle-packing";
const DEFAULT_N = 26;
const ACCEPTED_CIRCLE_N = new Set([26, 32]);

const DREAM_USAGE =
	"dream [rollout|replay|improve|loop|status|show] [--task <circle-packing|sum-difference>] [--n <26|32>] [--seed <n>] [--workers <n>] [--k1 <n>] [--k2 <n>] [--dreams <n>] [--iterations <n>] [--tree <id>] [--dir <path>] [--llm-proposer] [--llm-dreamer] [--json]";

const LLM_REJECTION_MESSAGE =
	"LLM proposer/dreamer run only in-session, where an agent handler exists: start them with /dream --llm-proposer or /dream --llm-dreamer. The standalone CLI has no handler, so it runs the default local proposer at zero tokens.";

export type DreamSubcommand = "rollout" | "replay" | "improve" | "loop" | "status" | "show";

const SUBCOMMAND_ALIASES: Record<string, DreamSubcommand> = {
	rollout: "rollout",
	propose: "rollout",
	replay: "replay",
	simulate: "replay",
	improve: "improve",
	loop: "loop",
	status: "status",
	show: "show",
	inspect: "show",
};

export interface DreamCommandOptions {
	subcommand: DreamSubcommand;
	task: DreamTaskId;
	n: number | undefined;
	seed: number;
	workers: number;
	k1: number;
	k2: number;
	dreams: number;
	iterations: number;
	tree: string;
	dir: string | undefined;
	json: boolean;
	llmProposer: boolean;
	llmDreamer: boolean;
}

export interface DreamCommandIo {
	stdout(line: string): void;
	stderr(line: string): void;
	now?(): number;
}

export class DreamCommandUsageError extends Error {}

function isDreamTaskId(value: string): value is DreamTaskId {
	return (DREAM_TASK_IDS as readonly string[]).includes(value);
}

export function parseDreamCommandArgs(args: string[]): DreamCommandOptions {
	let subcommand: DreamSubcommand | undefined;
	let task: DreamTaskId = DEFAULT_TASK;
	let n: number | undefined;
	let seed = 1;
	let workers = 4;
	let k1 = 12;
	let k2 = 24;
	let dreams = 16;
	let iterations = 3;
	let tree = "latest";
	let dir: string | undefined;
	let json = false;
	let llmProposer = false;
	let llmDreamer = false;

	const takeValue = (index: number, option: string): string => {
		const value = args[index];
		if (value === undefined || value.startsWith("-")) {
			throw new DreamCommandUsageError(`${option} requires a value.`);
		}
		return value;
	};
	const positiveInteger = (raw: string, option: string): number => {
		const parsed = Number.parseInt(raw, 10);
		if (!Number.isSafeInteger(parsed) || parsed <= 0 || String(parsed) !== raw.trim()) {
			throw new DreamCommandUsageError(`${option} requires a positive integer.`);
		}
		return parsed;
	};
	const nonNegativeInteger = (raw: string, option: string): number => {
		const parsed = Number.parseInt(raw, 10);
		if (!Number.isSafeInteger(parsed) || parsed < 0 || String(parsed) !== raw.trim()) {
			throw new DreamCommandUsageError(`${option} requires a non-negative integer.`);
		}
		return parsed;
	};
	const setTask = (raw: string): void => {
		if (!isDreamTaskId(raw)) {
			throw new DreamCommandUsageError(`Unknown task: ${raw}. Use one of ${DREAM_TASK_IDS.join(", ")}.`);
		}
		task = raw;
	};
	const setN = (raw: string): void => {
		const parsed = positiveInteger(raw, "--n");
		if (!ACCEPTED_CIRCLE_N.has(parsed)) {
			throw new DreamCommandUsageError("--n must be one of the paper values 26 or 32.");
		}
		n = parsed;
	};

	for (let index = 0; index < args.length; index++) {
		const arg = args[index]!;
		const eq = arg.indexOf("=");
		const flag = arg.startsWith("--") && eq !== -1 ? arg.slice(0, eq) : arg;
		const inlineValue = arg.startsWith("--") && eq !== -1 ? arg.slice(eq + 1) : undefined;
		const value = (option: string): string => {
			if (inlineValue !== undefined) {
				if (inlineValue === "") throw new DreamCommandUsageError(`${option} requires a value.`);
				return inlineValue;
			}
			return takeValue(++index, option);
		};
		switch (flag) {
			case "--json":
				json = true;
				break;
			case "--llm-proposer":
				llmProposer = true;
				break;
			case "--llm-dreamer":
				llmDreamer = true;
				break;
			case "--task":
				setTask(value("--task"));
				break;
			case "--n":
				setN(value("--n"));
				break;
			case "--seed":
				seed = nonNegativeInteger(value("--seed"), "--seed");
				break;
			case "--workers":
				workers = positiveInteger(value("--workers"), "--workers");
				break;
			case "--k1":
				k1 = positiveInteger(value("--k1"), "--k1");
				break;
			case "--k2":
				k2 = positiveInteger(value("--k2"), "--k2");
				break;
			case "--dreams":
				dreams = positiveInteger(value("--dreams"), "--dreams");
				break;
			case "--iterations":
				iterations = positiveInteger(value("--iterations"), "--iterations");
				break;
			case "--tree":
				tree = value("--tree");
				break;
			case "--dir":
				dir = value("--dir");
				break;
			default: {
				if (arg.startsWith("-")) {
					throw new DreamCommandUsageError(`Unknown option for dream: ${arg}`);
				}
				if (subcommand !== undefined) {
					throw new DreamCommandUsageError(`dream takes a single subcommand: unexpected ${arg}`);
				}
				const resolved = SUBCOMMAND_ALIASES[arg];
				if (resolved === undefined) {
					throw new DreamCommandUsageError(`Unknown dream subcommand: ${arg}`);
				}
				subcommand = resolved;
			}
		}
	}

	return {
		subcommand: subcommand ?? "loop",
		task,
		n,
		seed,
		workers,
		k1,
		k2,
		dreams,
		iterations,
		tree,
		dir,
		json,
		llmProposer,
		llmDreamer,
	};
}

function fmtScore(value: number): string {
	return Number.isFinite(value) ? value.toFixed(6) : "-";
}

function circleN(options: DreamCommandOptions): number | undefined {
	return options.task === "circle-packing" ? (options.n ?? DEFAULT_N) : undefined;
}

interface RoundLine {
	iteration: number;
	treeId: string;
	bestScore: number;
	probes: number;
}

function roundLines(storeDir: string, treeIds: readonly string[]): RoundLine[] {
	const byId = new Map(listTrees(storeDir).map((summary) => [summary.treeId, summary]));
	return treeIds.map((treeId, iteration) => {
		const summary = byId.get(treeId);
		return {
			iteration,
			treeId,
			bestScore: summary ? summary.bestScore : Number.NaN,
			probes: summary ? Math.max(0, summary.nodeCount - 1) : 0,
		};
	});
}

function resolveTreeId(storeDir: string, requested: string): string | undefined {
	const summaries = listTrees(storeDir);
	if (summaries.length === 0) return undefined;
	if (requested === "latest") {
		return summaries.reduce((latest, summary) =>
			summary.createdTs > latest.createdTs ||
			(summary.createdTs === latest.createdTs && summary.treeId > latest.treeId)
				? summary
				: latest,
		).treeId;
	}
	return summaries.some((summary) => summary.treeId === requested) ? requested : undefined;
}

function runLoop(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string, clock: DreamClock): number {
	const task = resolveTask({ task: options.task, n: circleN(options) });
	const loopOptions = {
		task,
		taskId: options.task,
		n: circleN(options),
		seed: options.seed,
		clock,
		workers: options.workers,
		k1: options.k1,
		k2: options.k2,
		dreams: options.dreams,
		iterations: options.iterations,
		dir: storeDir,
	};
	const result: DreamLoopResult = runDreamLoop(loopOptions);
	const rounds = roundLines(storeDir, result.treeIds);
	if (options.json) {
		io.stdout(JSON.stringify({ ...result, dir: storeDir, rounds }, undefined, 2));
		return 0;
	}
	io.stdout(`dream loop  ${storeDir}`);
	io.stdout(
		`  task ${result.task}  seed ${result.seed}  mode ${result.mode}  W ${options.workers}  k1 ${options.k1}  k2 ${options.k2}  M ${options.dreams}  iterations ${result.iterations}`,
	);
	for (const round of rounds) {
		io.stdout(
			`  round ${round.iteration}: best ${fmtScore(round.bestScore)}  probes ${round.probes}  tree ${round.treeId}`,
		);
	}
	io.stdout(`  initial policy ${result.initialPolicyId}  score ${fmtScore(result.initialPolicyScore)}`);
	io.stdout(
		`  final   policy ${result.finalPolicyId}  score ${fmtScore(result.finalPolicyScore)}  improved ${result.improved}`,
	);
	io.stdout(`  best node score ${fmtScore(result.bestNodeScore)}  tokens ${result.tokens}`);
	return 0;
}

function runRollout(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string, clock: DreamClock): number {
	const task = resolveTask({ task: options.task, n: circleN(options) });
	const rng = createSeededRng(options.seed);
	const exploreOptions = {
		task,
		taskId: options.task,
		n: circleN(options),
		seed: options.seed,
		rng,
		clock,
		workers: options.workers,
		k1: options.k1,
		dir: storeDir,
		policy: DEFAULT_POLICY,
		iteration: 0,
	};
	// runOnlineExploration grows and persists the tree; the CLI only prints it.
	const result: ExploreResult = runOnlineExploration(exploreOptions);
	if (options.json) {
		io.stdout(
			JSON.stringify(
				{
					treeId: result.treeId,
					policyId: policyId(DEFAULT_POLICY),
					rounds: result.rounds,
					revealedCount: result.revealedCount,
					bestScore: result.bestScore,
					bestNodeId: result.bestNodeId,
					rootScore: result.rootScore,
					tokens: result.tokens,
					dir: storeDir,
				},
				undefined,
				2,
			),
		);
		return 0;
	}
	io.stdout(`dream rollout  ${storeDir}`);
	io.stdout(`  tree ${result.treeId}  policy ${policyId(DEFAULT_POLICY)}`);
	io.stdout(
		`  rounds ${result.rounds}  revealed ${result.revealedCount}  best ${fmtScore(result.bestScore)}  best node ${result.bestNodeId}  tokens ${result.tokens}`,
	);
	return 0;
}

function runReplay(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string): number {
	const treeId = resolveTreeId(storeDir, options.tree);
	if (treeId === undefined) {
		io.stderr(`Error: no ${options.tree === "latest" ? "recorded trees" : `tree ${options.tree}`} in ${storeDir}`);
		return 2;
	}
	const recorded: RecordedTree = readTree(treeId, storeDir);
	const result: ReplayResult = simulatePolicyWithSpan(recorded, DEFAULT_POLICY, { k2: options.k2 }, DEFAULT_OBJECTIVE);
	const v = computeObjective(result, DEFAULT_OBJECTIVE);
	if (options.json) {
		io.stdout(JSON.stringify({ ...result, v }, undefined, 2));
		return 0;
	}
	io.stdout(`dream replay  ${result.treeId}`);
	io.stdout(`  policy ${result.policyId}`);
	io.stdout(
		`  revealed N ${result.N}  rounds ${result.rounds}  best ${fmtScore(result.bestScore)}  out-of-support ${result.outOfSupportRounds}`,
	);
	io.stdout(`  V ${fmtScore(v)}`);
	return 0;
}

function runImprove(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string): number {
	const summaries = listTrees(storeDir).filter((summary) => summary.taskId === options.task);
	if (summaries.length === 0) {
		io.stderr(`Error: no recorded ${options.task} trees in ${storeDir}; run "dream rollout" first`);
		return 2;
	}
	const pool: RecordedTree[] = summaries.map((summary) => readTree(summary.treeId, storeDir));
	const rng = createSeededRng(options.seed);
	const dreamingOptions = {
		current: DEFAULT_POLICY,
		pool,
		dreams: options.dreams,
		k2: options.k2,
		rng,
		objective: DEFAULT_OBJECTIVE,
	};
	const result: DreamResult = runDreaming(dreamingOptions);
	if (options.json) {
		io.stdout(JSON.stringify({ ...result, currentPolicyId: policyId(DEFAULT_POLICY), dir: storeDir }, undefined, 2));
		return 0;
	}
	io.stdout(`dream improve  ${storeDir}`);
	io.stdout(`  pool ${result.poolSize}  candidates ${result.scoredCount}`);
	io.stdout(`  current policy ${policyId(DEFAULT_POLICY)}  current score ${fmtScore(result.currentScore)}`);
	io.stdout(
		`  chosen  policy ${result.chosenPolicyId}  chosen score ${fmtScore(result.chosenScore)}  improved ${result.improved}`,
	);
	for (const candidate of result.candidatePolicyIds) {
		io.stdout(`    candidate ${candidate}`);
	}
	return 0;
}

interface StoreStatus {
	dir: string;
	treeCount: number;
	taskCounts: Record<string, number>;
	bestNodeScore: number;
	lastPolicyId: string | null;
}

function storeStatus(storeDir: string, summaries: readonly TreeSummary[]): StoreStatus {
	const taskCounts: Record<string, number> = {};
	let bestNodeScore = 0;
	let seenBest = false;
	let latest: TreeSummary | undefined;
	for (const summary of summaries) {
		taskCounts[summary.taskId] = (taskCounts[summary.taskId] ?? 0) + 1;
		if (Number.isFinite(summary.bestScore) && (!seenBest || summary.bestScore > bestNodeScore)) {
			bestNodeScore = summary.bestScore;
			seenBest = true;
		}
		if (latest === undefined || summary.createdTs > latest.createdTs) latest = summary;
	}
	return {
		dir: storeDir,
		treeCount: summaries.length,
		taskCounts,
		bestNodeScore,
		lastPolicyId: latest ? latest.policyId : null,
	};
}

function runStatus(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string): number {
	const summaries = listTrees(storeDir);
	const status = storeStatus(storeDir, summaries);
	if (summaries.length === 0) {
		if (options.json) io.stdout(JSON.stringify(status, undefined, 2));
		else io.stderr(`Error: dream store is empty (${storeDir})`);
		return 2;
	}
	if (options.json) {
		io.stdout(JSON.stringify(status, undefined, 2));
		return 0;
	}
	io.stdout(`dream store  ${status.dir}`);
	io.stdout(`  trees ${status.treeCount}  best node score ${fmtScore(status.bestNodeScore)}`);
	for (const [taskId, count] of Object.entries(status.taskCounts)) {
		io.stdout(`    ${taskId}: ${count}`);
	}
	io.stdout(`  last policy ${status.lastPolicyId ?? "-"}`);
	return 0;
}

function runShow(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string): number {
	const treeId = resolveTreeId(storeDir, options.tree);
	if (treeId === undefined) {
		io.stderr(`Error: no ${options.tree === "latest" ? "recorded trees" : `tree ${options.tree}`} in ${storeDir}`);
		return 2;
	}
	const recorded: RecordedTree = readTree(treeId, storeDir);
	if (options.json) {
		io.stdout(
			JSON.stringify({ header: recorded.header, nodes: recorded.nodes, reveals: recorded.reveals }, undefined, 2),
		);
		return 0;
	}
	const header = recorded.header;
	io.stdout(`dream tree  ${header.treeId}`);
	io.stdout(
		`  task ${header.taskId}${header.n !== undefined ? ` n ${header.n}` : ""}  W ${header.w}  seed ${header.seed}  policy ${header.policyId}  iteration ${header.iteration}`,
	);
	for (const reveal of recorded.reveals) {
		io.stdout(`  round ${reveal.round}: reveal ${reveal.ids.join(", ")}`);
	}
	for (const node of recorded.nodes) {
		io.stdout(
			`  ${node.id}  parent ${node.parentId ?? "-"}  branch ${node.branch}  seq ${node.seq}  round ${node.round}  score ${fmtScore(node.score)}  valid ${node.valid}${node.failClass ? `  fail ${node.failClass}` : ""}`,
		);
	}
	return 0;
}

export function runDreamCommand(args: string[], io: DreamCommandIo): number {
	let options: DreamCommandOptions;
	try {
		options = parseDreamCommandArgs(args);
	} catch (error) {
		if (!(error instanceof DreamCommandUsageError)) throw error;
		io.stderr(`Error: ${error.message}`);
		io.stderr(`Usage: ${APP_NAME} ${DREAM_USAGE}`);
		return 1;
	}
	if (options.llmProposer || options.llmDreamer) {
		io.stderr(LLM_REJECTION_MESSAGE);
		return 2;
	}
	const nowMs = io.now?.() ?? Date.now();
	const clock: DreamClock = () => nowMs;
	const storeDir = options.dir ?? getDreamDir();
	switch (options.subcommand) {
		case "loop":
			return runLoop(options, io, storeDir, clock);
		case "rollout":
			return runRollout(options, io, storeDir, clock);
		case "replay":
			return runReplay(options, io, storeDir);
		case "improve":
			return runImprove(options, io, storeDir);
		case "status":
			return runStatus(options, io, storeDir);
		case "show":
			return runShow(options, io, storeDir);
	}
}
