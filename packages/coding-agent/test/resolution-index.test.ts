import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { ResolutionIndex } from "../src/core/distill/resolution-index.js";
import type { ExtensionContext } from "../src/core/extensions/types.js";
import type { ExecuteResult, KernelClient } from "../src/core/kernel/index.js";
import { extractFailures } from "../src/core/ravo/failure-ledger.js";
import { createIpythonToolDefinition, type IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

const FAILING_CELL = "agents = client.list_agents()";
const FIX_CELL = 'agents = client.agents()\nprint(f"{len(agents)} agents")';
const RECURRENCE_CELL = "for agent in client.list_agents():\n    print(agent.name)";

function attributeErrorTraceback(line: number, source: string): string[] {
	return [
		"Traceback (most recent call last):",
		`  File "<ipython-input-${line}>", line 1, in <module>`,
		`    ${source}`,
		"AttributeError: 'AgentClient' object has no attribute 'list_agents'",
	];
}

function okResult(stdout: string): ExecuteResult {
	return { stdout, stderr: "", status: "ok", durationMs: 1 };
}

function errorResult(traceback: readonly string[]): ExecuteResult {
	const last = traceback[traceback.length - 1] ?? "";
	const colon = last.indexOf(": ");
	return {
		stdout: "",
		stderr: "",
		status: "error",
		durationMs: 1,
		error: {
			ename: colon === -1 ? last : last.slice(0, colon),
			evalue: colon === -1 ? "" : last.slice(colon + 2),
			traceback: [...traceback],
		},
	};
}

function textOf(result: { content: Array<{ type: string; text?: string }> }): string {
	return result.content[0]?.type === "text" ? (result.content[0].text ?? "") : "";
}

interface ReplayedCell {
	code: string;
	result: ExecuteResult;
}

/** Run a list of cells through the real ipython tool with a kernel that just replays canned results. */
async function replaySession(
	cwd: string,
	cells: readonly ReplayedCell[],
): Promise<Array<{ text: string; isError: boolean | undefined; stdout: string | undefined }>> {
	let next = 0;
	const manager = {
		execute: async (): Promise<ExecuteResult> => {
			const cell = cells[next++];
			if (!cell) throw new Error("replay ran out of cells");
			return cell.result;
		},
	} as unknown as KernelClient;
	const provisioner = {
		ensure: async () => manager,
		kill: async () => {},
		takeUnreportedExit: () => undefined,
	} as unknown as IpythonKernelProvisioner;
	const tool = createIpythonToolDefinition(cwd, { provisioner });

	const out: Array<{ text: string; isError: boolean | undefined; stdout: string | undefined }> = [];
	for (const [index, cell] of cells.entries()) {
		const result = await tool.execute(
			`call-${index}`,
			{ code: cell.code },
			undefined,
			undefined,
			{} as ExtensionContext,
		);
		out.push({
			text: textOf(result),
			isError: (result as { isError?: boolean }).isError,
			stdout: result.details.stdout,
		});
	}
	return out;
}

let tempDir = "";
let previousAgentDir: string | undefined;

beforeAll(() => {
	tempDir = mkdtempSync(join(tmpdir(), "resolution-index-"));
	previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = tempDir;
});

afterAll(() => {
	if (previousAgentDir === undefined) {
		delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	} else {
		process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	}
	rmSync(tempDir, { recursive: true, force: true });
});

describe("ipython resolution index", () => {
	it("annotates a recurring fingerprint with the cell that fixed it earlier in the session", async () => {
		const recurrence = errorResult(attributeErrorTraceback(3, "for agent in client.list_agents():"));
		const results = await replaySession(tempDir, [
			{ code: FAILING_CELL, result: errorResult(attributeErrorTraceback(1, FAILING_CELL)) },
			{ code: FIX_CELL, result: okResult("3 agents") },
			{ code: RECURRENCE_CELL, result: recurrence },
		]);

		// First hit: nothing learned yet.
		expect(results[0].isError).toBe(true);
		expect(results[0].text).not.toContain("<ipython_resolution_hint>");
		expect(results[0].text).toContain("AttributeError: 'AgentClient' object has no attribute 'list_agents'");

		// The cell that fixed it is not itself annotated.
		expect(results[1].text).toBe("3 agents");

		// Recurrence: the tool result carries the text of the cell that resolved it.
		expect(results[2].isError).toBe(true);
		expect(results[2].text).toContain("You hit this before; this fixed it:");
		expect(results[2].text).toContain("agents = client.agents()");
		expect(results[2].text).toContain('print(f"{len(agents)} agents")');
		// The traceback still comes first; the hint is appended after it.
		expect(results[2].text.indexOf("<ipython_resolution_hint>")).toBeGreaterThan(
			results[2].text.indexOf("AttributeError"),
		);
		// The annotation is model-facing only; structured details stay untouched.
		expect(results[2].stdout).toBe("");
	});

	it("fingerprints a cell exactly the way the failure ledger fingerprints its tool result", () => {
		const traceback = attributeErrorTraceback(1, FAILING_CELL).join("\n");
		const message: AgentMessage = {
			role: "toolResult",
			toolCallId: "call-0",
			toolName: "ipython",
			content: [{ type: "text", text: traceback }],
			isError: true,
			timestamp: 0,
		};
		const observed = extractFailures([message], { fromEntryIndex: 0, turn: 1 });
		expect(observed).toHaveLength(1);

		const index = new ResolutionIndex();
		index.observe({ code: FAILING_CELL, output: traceback, isError: true });
		expect(index.unresolved()).toEqual([observed[0].fingerprint.id]);
	});

	it("does not treat an unrelated clean cell as the fix", () => {
		const index = new ResolutionIndex();
		index.observe({
			code: FAILING_CELL,
			output: attributeErrorTraceback(1, FAILING_CELL).join("\n"),
			isError: true,
		});
		index.observe({ code: "import pandas as pd", output: "", isError: false });

		expect(index.records()).toEqual([]);
		expect(index.unresolved()).toHaveLength(1);
	});

	it("does not treat a cell that itself failed as the fix", () => {
		const index = new ResolutionIndex();
		const failure = attributeErrorTraceback(1, FAILING_CELL).join("\n");
		const typeError = [
			"Traceback (most recent call last):",
			'  File "<ipython-input-2>", line 1, in <module>',
			"    agents = client.agents()",
			"TypeError: agents() takes 0 positional arguments but 1 was given",
		].join("\n");
		index.observe({ code: FAILING_CELL, output: failure, isError: true });
		index.observe({ code: FIX_CELL, output: typeError, isError: true });
		expect(index.records()).toEqual([]);

		index.observe({ code: FIX_CELL, output: "3 agents", isError: false });
		expect(index.records().map((record) => record.failed)).toEqual([FAILING_CELL]);

		const hint = index.observe({ code: RECURRENCE_CELL, output: failure, isError: true });
		expect(hint?.record.fix).toBe(FIX_CELL);
	});

	it("forgets a failure no cell inside the window resolved", () => {
		const index = new ResolutionIndex({ window: 1 });
		index.observe({
			code: FAILING_CELL,
			output: attributeErrorTraceback(1, FAILING_CELL).join("\n"),
			isError: true,
		});
		index.observe({ code: "import pandas as pd", output: "", isError: false });
		index.observe({ code: FIX_CELL, output: "3 agents", isError: false });

		expect(index.records()).toEqual([]);
		expect(index.unresolved()).toEqual([]);
	});

	it("drops a recorded fix once that very cell reproduces the fingerprint", () => {
		const index = new ResolutionIndex();
		const failure = attributeErrorTraceback(1, FAILING_CELL).join("\n");
		index.observe({ code: FAILING_CELL, output: failure, isError: true });
		index.observe({ code: FIX_CELL, output: "3 agents", isError: false });
		expect(index.records()).toHaveLength(1);

		const hint = index.observe({ code: FIX_CELL, output: failure, isError: true });
		expect(hint).toBeUndefined();
		expect(index.records()).toEqual([]);
	});
});
