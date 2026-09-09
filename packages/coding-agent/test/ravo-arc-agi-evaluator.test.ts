import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
	type ArcRunner,
	type ArcRunnerArgs,
	arcScorecardScore,
	createArcAgiEvaluator,
	evaluateArcAgent,
	installArcAgent,
	interpretArcRun,
	parseArcScorecard,
} from "../src/core/ravo/arc-agi-evaluator.js";

const FIXTURE = path.join(import.meta.dirname, "fixtures", "arc-agi-scorecard-ls20.txt");
const INIT_PY = [
	"from .agent import Agent, Playback",
	"from .templates.random_agent import Random",
	"",
	"AVAILABLE_AGENTS = {cls.__name__.lower(): cls for cls in Agent.__subclasses__() if cls.__name__ != 'Playback'}",
	"",
	'__all__ = ["Agent", "AVAILABLE_AGENTS"]',
	"",
].join("\n");
const SOURCE = [
	"from arcengine import FrameData, GameAction, GameState",
	"from ..agent import Agent",
	"",
	"class AlwaysAction1(Agent):",
	"    MAX_ACTIONS = 40",
	"",
	"    def is_done(self, frames, latest_frame):",
	"        return latest_frame.state is GameState.WIN",
	"",
	"    def choose_action(self, frames, latest_frame):",
	"        if latest_frame.state in (GameState.NOT_PLAYED, GameState.GAME_OVER):",
	"            return GameAction.RESET",
	"        return GameAction.ACTION1",
	"",
].join("\n");

async function fakeRepo(): Promise<string> {
	const dir = await mkdtemp(path.join(tmpdir(), "ravo-arc-"));
	await mkdir(path.join(dir, "agents", "templates"), { recursive: true });
	await writeFile(path.join(dir, "agents", "__init__.py"), INIT_PY, "utf8");
	return dir;
}

function scorecardStdout(levelsCompleted: number, totalLevels: number, exit = "FINAL"): string {
	const card = {
		card_id: "card",
		environments: [
			{
				id: "ls20-9607627b",
				runs: [{ state: levelsCompleted === totalLevels ? "WIN" : "NOT_FINISHED" }],
				levels_completed: levelsCompleted,
				level_count: totalLevels,
				actions: 12,
				completed: levelsCompleted === totalLevels,
			},
		],
		total_levels_completed: levelsCompleted,
		total_levels: totalLevels,
		total_actions: 12,
	};
	return [
		"2026-09-08 13:46:02,022 | INFO | Finishing: agent took 12 actions",
		`2026-09-08 13:46:02,022 | INFO | --- ${exit} SCORECARD REPORT ---`,
		`2026-09-08 13:46:02,022 | INFO | ${JSON.stringify(card, null, 2)}`,
		"2026-09-08 13:46:02,022 | INFO | Received SIGINT, exiting...",
		"",
	].join("\n");
}

describe("parseArcScorecard", () => {
	it("parses the scorecard logged by a real `uv run main.py` run", async () => {
		const stdout = await readFile(FIXTURE, "utf8");
		const card = parseArcScorecard(stdout);
		expect(card).toMatchObject({
			cardId: "6ab903a6-3ba7-4bc4-9d77-0d7325dd793d",
			levelsCompleted: 0,
			totalLevels: 7,
			actions: 81,
		});
		expect(card?.environments).toEqual([
			{
				id: "ls20-9607627b",
				levelsCompleted: 0,
				levelCount: 7,
				actions: 81,
				completed: false,
				state: "NOT_FINISHED",
			},
		]);
		expect(arcScorecardScore(card as NonNullable<typeof card>)).toBe(0);
	});

	it("returns undefined without a scorecard report or with broken JSON", () => {
		expect(parseArcScorecard("no report here")).toBeUndefined();
		expect(parseArcScorecard("--- FINAL SCORECARD REPORT ---\n{ not json")).toBeUndefined();
		expect(parseArcScorecard('--- FINAL SCORECARD REPORT ---\n{"score": 1}')).toBeUndefined();
	});

	it("derives totals from environments when the totals are null and rounds the score", () => {
		const stdout = scorecardStdout(3, 7).replace('"total_levels": 7', '"total_levels": null');
		const card = parseArcScorecard(stdout);
		expect(card?.totalLevels).toBe(7);
		expect(arcScorecardScore(card as NonNullable<typeof card>)).toBe(43);
		expect(arcScorecardScore({ levelsCompleted: 0, totalLevels: 0, actions: 0, environments: [] })).toBe(0);
		expect(arcScorecardScore({ levelsCompleted: 7, totalLevels: 7, actions: 0, environments: [] })).toBe(100);
	});

	it("uses the last report when both an existing and a final report are printed", () => {
		const stdout = `${scorecardStdout(1, 7, "EXISTING")}${scorecardStdout(2, 7)}`;
		expect(parseArcScorecard(stdout)?.levelsCompleted).toBe(2);
	});
});

describe("interpretArcRun", () => {
	it("passes with the outcome score on a clean run", () => {
		const result = interpretArcRun({ stdout: scorecardStdout(2, 7), exitCode: 0 }, "ls20");
		expect(result).toMatchObject({
			status: "pass",
			score: 29,
			detail: "2/7 levels in 12 actions for ls20",
		});
		expect(result.scorecard).toMatchObject({ levelsCompleted: 2, totalLevels: 7, actions: 12 });
	});

	it("fails on a non-zero exit while keeping the score", () => {
		const result = interpretArcRun({ stdout: scorecardStdout(1, 7), stderr: "boom", exitCode: 3 }, "ls20");
		expect(result.status).toBe("fail");
		expect(result.score).toBe(14);
		expect(result.detail).toContain("run exited 3");
		expect(result.detail).toContain("boom");
	});

	it("fails when the agent raised inside the harness", () => {
		const stdout = [
			"Traceback (most recent call last):",
			'  File "agents/templates/x.py", line 9, in choose_action',
			"KeyError: 'missing'",
			scorecardStdout(0, 7),
		].join("\n");
		const result = interpretArcRun({ stdout, exitCode: 0 }, "ls20");
		expect(result).toMatchObject({
			status: "fail",
			score: 0,
			detail: "agent raised KeyError: 'missing'; 0/7 levels in 12 actions for ls20",
		});
		expect(result.scorecard?.levelsCompleted).toBe(0);
	});

	it("errors when no scorecard was produced", () => {
		const result = interpretArcRun(
			{ stdout: "Traceback (most recent call last):\nImportError: no module", exitCode: 1 },
			"ls20",
		);
		expect(result.status).toBe("error");
		expect(result.score).toBeUndefined();
		expect(result.detail).toContain("no scorecard in output for game ls20 (exit 1)");
		expect(result.detail).toContain("ImportError: no module");
	});
});

describe("installArcAgent", () => {
	it("writes the module under agents/templates and registers it in agents/__init__.py", async () => {
		const repo = await fakeRepo();
		const modulePath = await installArcAgent(repo, { agentName: "ravo_candidate", source: SOURCE });
		expect(modulePath).toBe(path.join(repo, "agents", "templates", "ravo_candidate.py"));
		const written = await readFile(modulePath, "utf8");
		expect(written.startsWith("# ravo-arc-agi candidate: ravo_candidate\n")).toBe(true);
		expect(written.endsWith(SOURCE)).toBe(true);
		const init = await readFile(path.join(repo, "agents", "__init__.py"), "utf8");
		expect(init.startsWith(INIT_PY)).toBe(true);
		expect(init).toContain("from .templates.ravo_candidate import AlwaysAction1 as _RavoArcAgent_ravo_candidate");
		expect(init).toContain('AVAILABLE_AGENTS["ravo_candidate"] = _RavoArcAgent_ravo_candidate');
	});

	it("replaces the managed block instead of appending on reinstall", async () => {
		const repo = await fakeRepo();
		await installArcAgent(repo, { agentName: "first", source: SOURCE });
		await installArcAgent(repo, { agentName: "second", source: SOURCE.replace("AlwaysAction1", "Second") });
		const init = await readFile(path.join(repo, "agents", "__init__.py"), "utf8");
		expect(init.match(/ravo-arc-agi managed agent \(generated/g)).toHaveLength(1);
		expect(init).not.toContain("templates.first");
		expect(init).toContain('AVAILABLE_AGENTS["second"] = _RavoArcAgent_second');
	});

	it("refuses to overwrite a module that is not a candidate", async () => {
		const repo = await fakeRepo();
		await writeFile(path.join(repo, "agents", "templates", "random_agent.py"), "class Random(Agent): ...\n");
		await expect(installArcAgent(repo, { agentName: "random_agent", source: SOURCE })).rejects.toThrow(
			/refusing to overwrite/,
		);
	});
});

describe("createArcAgiEvaluator", () => {
	const proposal = (artifact: { agentName: string; source: string }) => ({
		id: "p1",
		parentId: null,
		repairOf: null,
		artifact,
	});
	const context = { items: [], droppedItems: 0, totalTokens: 0, totalBytes: 0 } as never;
	const callOptions = () => ({ signal: new AbortController().signal, tokenBudget: 0 });

	it("is a deep, token-free adapter that runs the candidate and reports the outcome score", async () => {
		const repo = await fakeRepo();
		const calls: ArcRunnerArgs[] = [];
		const runner: ArcRunner = async (args) => {
			calls.push(args);
			return { stdout: scorecardStdout(3, 7), exitCode: 0 };
		};
		const adapter = createArcAgiEvaluator({ repoDir: repo, game: "ls20", runner, timeoutMs: 5_000 });
		expect(adapter.kind).toBe("deep");
		expect(adapter.id).toBe("arc-agi:ls20");
		const result = await adapter.evaluate(
			{ proposal: proposal({ agentName: "ravo_candidate", source: SOURCE }), context },
			callOptions(),
		);
		expect(result).toMatchObject({
			status: "completed",
			tokens: 0,
			value: { status: "pass", score: 43, detail: "3/7 levels in 12 actions for ls20" },
		});
		expect(calls).toHaveLength(1);
		expect(calls[0]).toMatchObject({
			command: "uv",
			args: ["run", "main.py", "--agent=ravo_candidate", "--game=ls20"],
			cwd: repo,
			timeoutMs: 5_000,
		});
		await expect(readFile(path.join(repo, "agents", "templates", "ravo_candidate.py"), "utf8")).resolves.toContain(
			"class AlwaysAction1(Agent):",
		);
	});

	it("maps runner failures and invalid artifacts to error verdicts", async () => {
		const repo = await fakeRepo();
		const runner = vi.fn(async () => {
			throw new Error("uv: command not found");
		});
		const failing = createArcAgiEvaluator({ repoDir: repo, game: "ls20", runner });
		const result = await failing.evaluate(
			{ proposal: proposal({ agentName: "ravo_candidate", source: SOURCE }), context },
			callOptions(),
		);
		expect(result).toEqual({
			status: "completed",
			tokens: 0,
			value: { status: "error", detail: "uv: command not found" },
		});
		const invalid = await evaluateArcAgent(
			{ repoDir: repo, game: "ls20", runner },
			{ agentName: "Bad-Name", source: SOURCE },
			new AbortController().signal,
		);
		expect(invalid.status).toBe("error");
		expect(invalid.detail).toContain("agentName");
		const noClass = await evaluateArcAgent(
			{ repoDir: repo, game: "ls20", runner },
			{ agentName: "ok", source: "print('hi')" },
			new AbortController().signal,
		);
		expect(noClass.status).toBe("error");
		expect(noClass.detail).toContain("Agent");
		expect(runner).toHaveBeenCalledTimes(1);
	});

	it("respects the timeout even when the runner ignores it", async () => {
		const repo = await fakeRepo();
		let seenTimeout = 0;
		const runner: ArcRunner = ({ timeoutMs }) => {
			seenTimeout = timeoutMs;
			return new Promise(() => {});
		};
		const adapter = createArcAgiEvaluator({ repoDir: repo, game: "ls20", runner, timeoutMs: 40 });
		const result = await adapter.evaluate(
			{ proposal: proposal({ agentName: "ravo_candidate", source: SOURCE }), context },
			callOptions(),
		);
		expect(seenTimeout).toBe(40);
		expect(result).toEqual({
			status: "completed",
			tokens: 0,
			value: { status: "error", detail: "ARC-AGI-3 run timed out after 40ms" },
		});
	});

	it("aborts through the call signal, before and during the run", async () => {
		const repo = await fakeRepo();
		const early = new AbortController();
		early.abort();
		const runner = vi.fn<ArcRunner>(
			({ signal }) =>
				new Promise((_, reject) => signal.addEventListener("abort", () => reject(new Error("killed")))),
		);
		const adapter = createArcAgiEvaluator({ repoDir: repo, game: "ls20", runner });
		await expect(
			adapter.evaluate(
				{ proposal: proposal({ agentName: "ravo_candidate", source: SOURCE }), context },
				{ signal: early.signal, tokenBudget: 0 },
			),
		).resolves.toMatchObject({ value: { status: "error", detail: "ARC-AGI-3 run aborted" } });
		expect(runner).not.toHaveBeenCalled();

		const late = new AbortController();
		const pending = adapter.evaluate(
			{ proposal: proposal({ agentName: "ravo_candidate", source: SOURCE }), context },
			{ signal: late.signal, tokenBudget: 0 },
		);
		await vi.waitFor(() => expect(runner).toHaveBeenCalledTimes(1));
		late.abort();
		await expect(pending).resolves.toMatchObject({ value: { status: "error", detail: "ARC-AGI-3 run aborted" } });
	});
});
