import { describe, expect, it } from "vitest";
import { createArcAgiEvaluator } from "../src/core/ravo/arc-agi-evaluator.js";

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
