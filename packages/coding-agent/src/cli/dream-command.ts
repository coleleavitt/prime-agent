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
	DreamStoreError,
	type DreamTaskId,
	EXPERIMENT_ARMS,
	type ExperimentArm,
	type ExperimentArmResult,
	ExperimentArmUnavailableError,
	type ExperimentResult,
	type ExperimentSpec,
	type ExploreResult,
	experimentResultPath,
	GUIDED_ARM_REJECTION_MESSAGE,
	getDreamDir,
	isExperimentArm,
	LOCAL_EXPERIMENT_ARMS,
	listExperimentIds,
	listTrees,
	policyId,
	poolScoreScale,
	type RecordedTree,
	type ReplayObjectiveConfig,
	type ReplayResult,
	readTree,
	resolveTask,
	runDreaming,
	runDreamLoop,
	runExperiment,
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
 * uses no network. `experiment` runs the paper's controlled comparison: the
 * dreaming arm against the Recursive Fixed Exploration control (`fixed`), both
 * from the same policy, seed and budget, into `<dir>/experiments/<id>/<arm>`.
 * The `--llm-proposer` / `--llm-dreamer` flags and the guided arms require an
 * in-session agent handler and are rejected by this standalone CLI, which imports
 * only `core/dream/index.js` (never the LLM path) and so cannot spend a token or
 * open a socket.
 *
 * `DREAM_USAGE` is the one usage string: `cli/command-registry.ts` imports it for
 * `help dream`, and the Options rows there must name every flag it lists (a test
 * in `test/dream-command.test.ts` checks both against the parser).
 */

const DEFAULT_TASK: DreamTaskId = "circle-packing";
const DEFAULT_N = 26;
const ACCEPTED_CIRCLE_N = new Set([26, 32]);

export const DREAM_USAGE = `dream [rollout|replay|improve|loop|experiment|status|show] [--task <${DREAM_TASK_IDS.join("|")}>] [--n <26|32>] [--seed <n>] [--seeds <a,b,c>] [--workers <n>] [--k1 <n>] [--k2 <n>] [--dreams <n>] [--beta1 <x>] [--beta2 <x>] [--iterations <n>] [--rounds <n>] [--arms <dream,fixed>] [--overwrite] [--tree <id>] [--dir <path>] [--llm-proposer] [--llm-dreamer] [--json]`;

const DEFAULT_EXPERIMENT_ROUNDS = 4;
const DEFAULT_EXPERIMENT_ARMS: readonly ExperimentArm[] = LOCAL_EXPERIMENT_ARMS;

const LLM_REJECTION_MESSAGE =
	"LLM proposer/dreamer run only in-session, where an agent handler exists: start them with /dream --llm-proposer or /dream --llm-dreamer. The standalone CLI has no handler, so it runs the default local proposer at zero tokens.";

export type DreamSubcommand = "rollout" | "replay" | "improve" | "loop" | "experiment" | "status" | "show";

const SUBCOMMAND_ALIASES: Record<string, DreamSubcommand> = {
	rollout: "rollout",
	propose: "rollout",
	replay: "replay",
	simulate: "replay",
	improve: "improve",
	loop: "loop",
	experiment: "experiment",
	compare: "experiment",
	status: "status",
	show: "show",
	inspect: "show",
};

export interface DreamCommandOptions {
	subcommand: DreamSubcommand;
	task: DreamTaskId;
	n: number | undefined;
	seed: number;
	/** `--seeds a,b,c`: one experiment per seed, sequentially; overrides `--seed`. */
	seeds: number[] | undefined;
	workers: number;
	k1: number;
	k2: number;
	dreams: number;
	/** The replay objective's `beta1`/`beta2` (`--beta1`/`--beta2`), defaulting to `DEFAULT_OBJECTIVE`. */
	objective: ReplayObjectiveConfig;
	iterations: number;
	/** Rollouts per experiment arm. */
	rounds: number;
	arms: ExperimentArm[];
	overwrite: boolean;
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
	let beta1 = DEFAULT_OBJECTIVE.beta1;
	let beta2 = DEFAULT_OBJECTIVE.beta2;
	let iterations = 3;
	let iterationsGiven = false;
	let rounds = DEFAULT_EXPERIMENT_ROUNDS;
	let arms: ExperimentArm[] = [...DEFAULT_EXPERIMENT_ARMS];
	let seeds: number[] | undefined;
	let overwrite = false;
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
	const nonNegativeNumber = (raw: string, option: string): number => {
		const trimmed = raw.trim();
		const parsed = trimmed.length > 0 ? Number(trimmed) : Number.NaN;
		if (!Number.isFinite(parsed) || parsed < 0) {
			throw new DreamCommandUsageError(`${option} requires a finite non-negative number.`);
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
	const setArms = (raw: string): void => {
		const names = raw
			.split(",")
			.map((name) => name.trim())
			.filter((name) => name.length > 0);
		if (names.length === 0) throw new DreamCommandUsageError("--arms requires a comma-separated list of arms.");
		const parsed: ExperimentArm[] = [];
		for (const name of names) {
			if (!isExperimentArm(name)) {
				throw new DreamCommandUsageError(`Unknown arm: ${name}. Use one of ${EXPERIMENT_ARMS.join(", ")}.`);
			}
			if (parsed.includes(name)) throw new DreamCommandUsageError(`--arms lists ${name} twice.`);
			parsed.push(name);
		}
		arms = parsed;
	};
	const setSeeds = (raw: string): void => {
		const parts = raw
			.split(",")
			.map((part) => part.trim())
			.filter((part) => part.length > 0);
		if (parts.length === 0) throw new DreamCommandUsageError("--seeds requires a comma-separated list of seeds.");
		const parsed = parts.map((part) => nonNegativeInteger(part, "--seeds"));
		if (new Set(parsed).size !== parsed.length) throw new DreamCommandUsageError("--seeds must be distinct.");
		seeds = parsed;
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
			case "--beta1":
				beta1 = nonNegativeNumber(value("--beta1"), "--beta1");
				break;
			case "--beta2":
				beta2 = nonNegativeNumber(value("--beta2"), "--beta2");
				break;
			case "--iterations":
				iterations = positiveInteger(value("--iterations"), "--iterations");
				iterationsGiven = true;
				break;
			case "--rounds":
				rounds = positiveInteger(value("--rounds"), "--rounds");
				break;
			case "--arms":
				setArms(value("--arms"));
				break;
			case "--seeds":
				setSeeds(value("--seeds"));
				break;
			case "--overwrite":
				overwrite = true;
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

	const resolvedSubcommand = subcommand ?? "loop";
	if (resolvedSubcommand === "experiment" && iterationsGiven) {
		throw new DreamCommandUsageError("experiment takes --rounds (rollouts per arm), not --iterations.");
	}

	return {
		subcommand: resolvedSubcommand,
		task,
		n,
		seed,
		seeds,
		workers,
		k1,
		k2,
		dreams,
		objective: { beta1, beta2 },
		iterations,
		rounds,
		arms,
		overwrite,
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

function roundLines(result: DreamLoopResult): RoundLine[] {
	return result.rounds.map((record) => ({
		iteration: record.iteration,
		treeId: record.treeId,
		bestScore: record.roundBest,
		probes: record.probes,
	}));
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
		objective: options.objective,
	};
	const result: DreamLoopResult = runDreamLoop(loopOptions);
	const rounds = roundLines(result);
	if (options.json) {
		io.stdout(JSON.stringify({ ...result, dir: storeDir, rounds }, undefined, 2));
		return 0;
	}
	io.stdout(`dream loop  ${storeDir}`);
	io.stdout(
		`  task ${result.task}  seed ${result.seed}  mode ${result.mode}  W ${options.workers}  k1 ${options.k1}  k2 ${options.k2}  M ${options.dreams}  beta1 ${options.objective.beta1}  beta2 ${options.objective.beta2}  iterations ${result.iterations}`,
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

function fmtMultiplier(value: number | null): string {
	return value === null ? "-" : `${value.toFixed(2)}x`;
}

function printExperiment(result: ExperimentResult, io: DreamCommandIo, storeDir: string, resultPath: string): void {
	const budget = result.budget;
	io.stdout(`dream experiment  ${result.experimentId}`);
	io.stdout(
		`  task ${result.task}${result.n !== undefined ? ` n ${result.n}` : ""}  scoring ${result.scoring}  seed ${result.seed}  rounds ${result.rounds}  W ${budget.workers}  k1 ${budget.k1}  k2 ${budget.k2}  M ${budget.dreams}  beta1 ${result.objective.beta1}  beta2 ${result.objective.beta2}  arms ${result.arms.map((arm) => arm.arm).join(",")}`,
	);
	io.stdout(`  initial policy ${result.initialPolicyId}`);
	for (const arm of result.arms) {
		printArm(arm, io);
	}
	printHeadline(result, io);
	for (const note of result.notes) io.stdout(`  note: ${note}`);
	io.stdout(`  store ${storeDir}`);
	io.stdout(`  results ${resultPath}`);
}

function printArm(arm: ExperimentArmResult, io: DreamCommandIo): void {
	io.stdout(
		`  arm ${arm.arm}  proposer ${arm.mode.proposer}  dreamer ${arm.mode.dreamer}  fixed ${arm.fixedPolicy}  guided ${arm.guided}  run ${arm.runId}`,
	);
	io.stdout("    round | best | cum best | probes | cum probes | policy");
	for (const row of arm.rounds) {
		io.stdout(
			`    ${String(row.round).padStart(5)} | ${fmtScore(row.roundBest)} | ${fmtScore(row.cumulativeBest)} | ${String(row.probes).padStart(6)} | ${String(row.cumulativeProbes).padStart(10)} | ${row.policyId}${row.dreaming ? `  dreamed ${fmtScore(row.dreaming.currentScore)} -> ${fmtScore(row.dreaming.chosenScore)} improved ${row.dreaming.improved}` : ""}`,
		);
	}
	// `final policy` is the last row's policy (what the arm last ran); `selected` is the post-hoc pool winner.
	io.stdout(
		`    final policy ${arm.finalPolicyId}  changes ${arm.policyChanges}  selected policy ${arm.selectedPolicyId}  own-pool score ${fmtScore(arm.policyScoreOnOwnPool.initial)} -> ${fmtScore(arm.policyScoreOnOwnPool.final)}  final best ${fmtScore(arm.totals.finalBest)}  probes ${arm.totals.probes}  handler calls ${arm.totals.handlerCalls}  tokens ${arm.totals.tokens}`,
	);
}

function printHeadline(result: ExperimentResult, io: DreamCommandIo): void {
	const headline = result.headline;
	if (!headline) {
		io.stdout("  headline: not comparable (no fixed arm)");
		return;
	}
	const fixedProbes = headline.probesToTarget[headline.reference];
	io.stdout(
		`  headline vs ${headline.reference}: target ${fmtScore(headline.target)}${fixedProbes === null || fixedProbes === undefined ? "" : ` reached at ${fixedProbes} probes`}; equal budget ${headline.equalBudget} probes`,
	);
	for (const arm of result.arms) {
		if (arm.arm === headline.reference) continue;
		const probes = headline.probesToTarget[arm.arm] ?? null;
		const calls = headline.callsMultiplier[arm.arm] ?? null;
		const best = headline.bestAtBudget[arm.arm] ?? null;
		const score = headline.scoreMultiplier[arm.arm] ?? null;
		const delta = headline.deltaBest[arm.arm] ?? 0;
		const reach =
			probes === null
				? "target not reached"
				: `target at ${probes} probes -> ${calls === null ? "not comparable" : `${fmtMultiplier(calls)} fewer calls`}`;
		const budget =
			best === null
				? "at equal budget: not comparable"
				: `at equal budget: ${fmtScore(best)} vs ${fmtScore(headline.bestAtBudget[headline.reference] ?? Number.NaN)} -> ${score === null ? "not comparable" : `${fmtMultiplier(score)} score`}`;
		io.stdout(`    ${arm.arm}: ${reach}; ${budget}; delta best ${delta >= 0 ? "+" : ""}${fmtScore(delta)}`);
	}
}

function runExperimentCommand(
	options: DreamCommandOptions,
	io: DreamCommandIo,
	storeDir: string,
	clock: DreamClock,
): number {
	const seeds = options.seeds ?? [options.seed];
	const results: ExperimentResult[] = [];
	for (const seed of seeds) {
		const spec: ExperimentSpec = {
			task: options.task,
			...(circleN(options) !== undefined ? { n: circleN(options) } : {}),
			seed,
			rounds: options.rounds,
			budget: { workers: options.workers, k1: options.k1, k2: options.k2, dreams: options.dreams },
			arms: options.arms,
			objective: options.objective,
		};
		let result: ExperimentResult;
		try {
			result = runExperiment(spec, { dir: storeDir, clock, overwrite: options.overwrite });
		} catch (error) {
			if (
				error instanceof ExperimentArmUnavailableError ||
				error instanceof DreamStoreError ||
				error instanceof RangeError
			) {
				io.stderr(`Error: ${error.message}`);
				return 2;
			}
			throw error;
		}
		results.push(result);
		if (!options.json) {
			printExperiment(result, io, storeDir, experimentResultPath(storeDir, result.experimentId));
		}
	}
	if (options.json) {
		io.stdout(JSON.stringify(options.seeds ? results : results[0], undefined, 2));
	}
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
	const result: ReplayResult = simulatePolicyWithSpan(
		recorded,
		DEFAULT_POLICY,
		{ k1: options.k1, k2: options.k2 },
		options.objective,
	);
	const v = computeObjective(result, options.objective, poolScoreScale([recorded]), {
		workers: recorded.header.w,
		k1: options.k1,
	});
	if (options.json) {
		io.stdout(JSON.stringify({ ...result, v }, undefined, 2));
		return 0;
	}
	io.stdout(`dream replay  ${result.treeId}`);
	io.stdout(`  policy ${result.policyId}`);
	io.stdout(
		`  revealed N ${result.N}  rounds ${result.rounds}  best ${fmtScore(result.bestScore)}  out-of-support ${result.outOfSupportRounds}`,
	);
	io.stdout(
		`  V ${fmtScore(v)}  (beta1 ${options.objective.beta1}  beta2 ${options.objective.beta2}  budget ${recorded.header.w}x${options.k1})`,
	);
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
		k1: options.k1,
		k2: options.k2,
		rng,
		objective: options.objective,
	};
	const result: DreamResult = runDreaming(dreamingOptions);
	if (options.json) {
		io.stdout(JSON.stringify({ ...result, currentPolicyId: policyId(DEFAULT_POLICY), dir: storeDir }, undefined, 2));
		return 0;
	}
	io.stdout(`dream improve  ${storeDir}`);
	io.stdout(`  pool ${result.poolSize}  candidates ${result.scoredCount}`);
	io.stdout(
		`  current policy ${policyId(DEFAULT_POLICY)}  current score ${fmtScore(result.currentScore)}  quality ${fmtScore(result.currentQuality)}`,
	);
	io.stdout(
		`  chosen  policy ${result.chosenPolicyId}  chosen score ${fmtScore(result.chosenScore)}  quality ${fmtScore(result.chosenQuality)}  improved ${result.improved}  quality-rejected ${result.qualityRejected}`,
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
	experimentCount: number;
	experimentIds: string[];
}

function storeStatus(
	storeDir: string,
	summaries: readonly TreeSummary[],
	experimentIds: readonly string[],
): StoreStatus {
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
		experimentCount: experimentIds.length,
		experimentIds: [...experimentIds],
	};
}

function runStatus(options: DreamCommandOptions, io: DreamCommandIo, storeDir: string): number {
	const summaries = listTrees(storeDir);
	const experimentIds = listExperimentIds(storeDir);
	const status = storeStatus(storeDir, summaries, experimentIds);
	if (summaries.length === 0 && experimentIds.length === 0) {
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
	io.stdout(`  experiments ${status.experimentCount}`);
	for (const experimentId of status.experimentIds) io.stdout(`    ${experimentId}`);
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
	if (options.subcommand === "experiment" && options.arms.some((arm) => arm.endsWith("-guided"))) {
		io.stderr(GUIDED_ARM_REJECTION_MESSAGE);
		return 2;
	}
	const nowMs = io.now?.() ?? Date.now();
	const clock: DreamClock = () => nowMs;
	const storeDir = options.dir ?? getDreamDir();
	switch (options.subcommand) {
		case "loop":
			return runLoop(options, io, storeDir, clock);
		case "experiment":
			return runExperimentCommand(options, io, storeDir, clock);
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
