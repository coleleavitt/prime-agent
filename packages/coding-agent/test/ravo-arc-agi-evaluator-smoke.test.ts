import { existsSync, readFileSync } from "node:fs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { createArcAgiEvaluator } from "../src/core/ravo/arc-agi-evaluator.js";
import { RavoRunService, type RavoRunStatus } from "../src/core/ravo/run-service.js";
import type { HarnessState } from "../src/core/refinement/refinement.js";
import type { RunAgentHandler } from "../src/core/run-agent.js";

/**
 * Real end-to-end smoke: plays one ARC-AGI-3 game through `uv run main.py` in
 * a local ARC-AGI-3-Agents clone. Skipped unless ARC_SMOKE=1; needs `uv`, the
 * clone at ARC_AGI_REPO (default /tmp/arc-agi-3) with a configured `.env`.
 * No LLM is involved: the candidate is a fixed-policy agent.
 */
const enabled = process.env.ARC_SMOKE === "1";
const repoDir = process.env.ARC_AGI_REPO ?? "/tmp/arc-agi-3";
const game = process.env.ARC_AGI_GAME ?? "ls20";

const SOURCE = [
	"from arcengine import FrameData, GameAction, GameState",
	"",
	"from ..agent import Agent",
	"",
	"",
	"class RavoSmokeAction1(Agent):",
	'    """Trivial candidate: RESET when needed, otherwise always ACTION1."""',
	"",
	"    MAX_ACTIONS = 40",
	"",
	"    def is_done(self, frames: list[FrameData], latest_frame: FrameData) -> bool:",
	"        return latest_frame.state is GameState.WIN",
	"",
	"    def choose_action(self, frames: list[FrameData], latest_frame: FrameData) -> GameAction:",
	"        if latest_frame.state in (GameState.NOT_PLAYED, GameState.GAME_OVER):",
	"            return GameAction.RESET",
	"        return GameAction.ACTION1",
	"",
].join("\n");

describe.skipIf(!enabled)("ARC-AGI-3 evaluator smoke (ARC_SMOKE=1)", () => {
	it(
		"plays a real game and reports the outcome score",
		async () => {
			const adapter = createArcAgiEvaluator({ repoDir, game, timeoutMs: 5 * 60 * 1000 });
			const result = await adapter.evaluate(
				{
					proposal: {
						id: "smoke",
						parentId: null,
						repairOf: null,
						artifact: { agentName: "ravo_smoke_action1", source: SOURCE },
					},
					context: { items: [], droppedItems: 0, totalTokens: 0, totalBytes: 0 } as never,
				},
				{ signal: new AbortController().signal, tokenBudget: 0 },
			);
			console.log(`ARC_SMOKE result: ${JSON.stringify(result)}`);
			expect(result.status).toBe("completed");
			if (result.status !== "completed") return;
			expect(result.tokens).toBe(0);
			expect(["pass", "fail"]).toContain(result.value.status);
			expect(result.value.score).toBeGreaterThanOrEqual(0);
			expect(result.value.score).toBeLessThanOrEqual(100);
		},
		6 * 60 * 1000,
	);
});

/**
 * Full loop through RavoRunService against the real harness: scripted
 * (LLM-free) children propose an agent, the game is played for real, and the
 * gate ratchets on the observed outcome. Skipped unless ARC_SMOKE=1.
 */
describe.skipIf(!enabled)("RavoRunService x ARC-AGI-3 end to end (ARC_SMOKE=1)", () => {
	it(
		"runs inspect -> plan -> implement -> real game -> gate and persists the accepted agent",
		async () => {
			const harnessDir = await mkdtemp(path.join(tmpdir(), "ravo-arc-e2e-"));
			let state: HarnessState = {
				schema: 1,
				entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
				refinements: [],
			};
			const updates: RavoRunStatus[] = [];
			const scripted: Record<string, unknown> = {
				inspect: { summary: "ls20 is unsolved; any agent that plays without crashing is a baseline", facts: [] },
				plan: { steps: ["submit the fixed-policy RESET/ACTION1 agent as the first champion"] },
				implement: {
					summary: "Fixed-policy baseline agent",
					rationale: "Establish a non-crashing champion so later candidates ratchet on levels.",
					expectedOutcome: "0 or more levels, no traceback",
					addressedFingerprints: [],
					edits: [],
					arcAgent: {
						agentName: "ravo_e2e_action1",
						source: SOURCE.replace("RavoSmokeAction1", "RavoE2eAction1"),
					},
				},
				supervisor: { intervene: false },
			};
			const runAgent: RunAgentHandler = async (request) => {
				const role = /^# RAVO (\w+)/.exec(request.prompt)?.[1] ?? "";
				const value = scripted[role];
				if (value === undefined) throw new Error(`unexpected child role ${role}`);
				return {
					status: "completed",
					output: JSON.stringify(value),
					messages: [],
					model: "scripted/child",
					turns: 1,
					toolCalls: 0,
					usage: {
						input: 1,
						output: 1,
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 2,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					},
				};
			};
			const service = new RavoRunService({
				runAgent,
				harnessDir,
				loadState: async () => structuredClone(state),
				saveState: async (next) => {
					state = structuredClone(next);
				},
				onUpdate: (status) => updates.push(status),
			});
			const terminal = await service.start({
				task: `play ${game} without crashing`,
				maxRounds: 1,
				maxRepairs: 0,
				deadlineMs: 5 * 60 * 1000,
				evaluator: { kind: "arc-agi", repoDir, game },
			});
			console.log(`ARC_E2E terminal: ${JSON.stringify(terminal)}`);
			expect(terminal.stopReason).toBe("accepted");
			expect(terminal.lastCertificate).toMatchObject({ status: "commit", screenScore: 100 });
			expect(terminal.lastCertificate?.deepScore).toBeGreaterThanOrEqual(0);
			expect(updates.filter((u) => u.lastEvent?.type === "phase").map((u) => u.phase)).toEqual([
				"inspect",
				"plan",
				"implement",
				"evaluate",
				"commit_gate",
			]);
			expect(state.ravo?.lineage).toHaveLength(1);
			expect(state.ravo?.opponents.criteria.map((c) => c.id)).toEqual(
				expect.arrayContaining(["arc:no-crash", "arc:all-levels"]),
			);
			const persisted = path.join(harnessDir, "ravo", "arc", `${terminal.runId}-ravo_e2e_action1.py`);
			expect(existsSync(persisted)).toBe(true);
			expect(readFileSync(persisted, "utf8")).toContain("class RavoE2eAction1(Agent)");
		},
		6 * 60 * 1000,
	);
});
