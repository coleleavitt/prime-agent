// The real `config.ts` path formulas; the agent dir comes from the generator.
import { join } from "node:path";
export const APP_NAME = "prime-agent";
const agentDir = (): string => process.env.GOLDEN_AGENT_DIR!;
export function getAgentLogPath(): string {
	return join(agentDir(), "logs", "agent.jsonl");
}
export function getLearningDir(dir: string = agentDir()): string {
	return join(dir, "learning");
}
export function getLearningIndexDir(dir: string = agentDir()): string {
	return join(getLearningDir(dir), "days");
}
export function getTrajectoryIndexPath(dir: string = agentDir()): string {
	return join(getLearningDir(dir), "trajectory.json");
}
export function getTrajectoryBackfillDir(dir: string = agentDir()): string {
	return join(getLearningDir(dir), "backfill");
}
