import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { ExtensionContext } from "../src/core/extensions/types.js";
import type { KernelClient } from "../src/core/kernel/index.js";
import {
	executedBashCommands,
	parseKernelBashCommands,
	REPL_PROTOCOL_VERSION,
	type ReplExecuteResult,
	ReplKernelManager,
} from "../src/core/kernel/repl-manager.js";
import { createIpythonToolDefinition, type IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

function resolveReplPython(): string | null {
	const candidates = [
		process.env.PRIME_AGENT_KERNEL_PYTHON,
		resolve(__dirname, "..", "..", "..", "prime-agent-runtime", ".venv", "bin", "python"),
		join(homedir(), ".prime", "agent", "kernel-venv", "bin", "python"),
	].filter((p): p is string => Boolean(p));
	for (const python of candidates) {
		if (!existsSync(python)) continue;
		const check = spawnSync(python, ["-c", "import rlm.repl, dill"], { encoding: "utf8" });
		if (check.status === 0) return python;
	}
	return null;
}

/** A runtime that answers `with-commands` with a done frame carrying bashCommands, and everything else without. */
function writeFakeRuntime(path: string): void {
	writeFileSync(
		path,
		`#!/usr/bin/env node
const readline = require("node:readline");
const emit = (event) => process.stdout.write(JSON.stringify(event) + "\\n");
emit({ event: "ready", protocol: ${REPL_PROTOCOL_VERSION}, python: process.version });
readline.createInterface({ input: process.stdin }).on("line", (line) => {
  const request = JSON.parse(line);
  if (request.type === "shutdown") {
    emit({ event: "done", id: request.id, status: "ok" });
    process.exit(0);
  }
  if (request.type !== "execute") return;
  if (request.code === "with-commands") {
    emit({
      event: "done",
      id: request.id,
      status: "ok",
      bashCommands: [
        { command: "npx tsgo --noEmit", exitCode: 0, startedAt: "2026-09-16T00:00:00+00:00", endedAt: "2026-09-16T00:00:01+00:00" },
        { command: "cargo test", exitCode: 101, commandTruncated: true },
        { command: 42, exitCode: 0 },
        { command: "exit 1.5", exitCode: 1.5 },
        "not a record",
      ],
    });
    return;
  }
  emit({ event: "done", id: request.id, status: "ok" });
});
`,
	);
	chmodSync(path, 0o755);
}

describe("bash commands on the cell's done frame", () => {
	let dir = "";
	let manager: ReplKernelManager | undefined;

	beforeEach(() => {
		dir = mkdtempSync(join(tmpdir(), "prime-agent-repl-bash-commands-"));
	});

	afterEach(async () => {
		await manager?.shutdown({ snapshot: false });
		manager = undefined;
		rmSync(dir, { recursive: true, force: true });
	});

	it("keeps well-formed entries and tolerates a runtime that sends none", async () => {
		const runtime = join(dir, "fake-runtime.js");
		writeFakeRuntime(runtime);
		manager = new ReplKernelManager({ python: runtime, cwd: dir });

		const withCommands = await manager.execute("with-commands");
		expect(withCommands.bashCommands).toEqual([
			{
				command: "npx tsgo --noEmit",
				exitCode: 0,
				startedAt: "2026-09-16T00:00:00+00:00",
				endedAt: "2026-09-16T00:00:01+00:00",
			},
			{ command: "cargo test", exitCode: 101, commandTruncated: true },
		]);
		expect(executedBashCommands(withCommands)).toEqual(withCommands.bashCommands);

		const without = await manager.execute("plain");
		expect(without.status).toBe("ok");
		expect("bashCommands" in without).toBe(false);
		expect(executedBashCommands(without)).toBeUndefined();
	});

	it("parses only arrays of records with a string command and an integer exit code", () => {
		expect(parseKernelBashCommands(undefined)).toBeUndefined();
		expect(parseKernelBashCommands({ command: "make", exitCode: 0 })).toBeUndefined();
		expect(parseKernelBashCommands([{ command: "make" }])).toBeUndefined();
		expect(parseKernelBashCommands([{ command: "make", exitCode: -9, commandTruncated: "yes" }])).toEqual([
			{ command: "make", exitCode: -9 },
		]);
		const many = Array.from({ length: 70 }, (_, index) => ({ command: `step ${index}`, exitCode: 0 }));
		const parsed = parseKernelBashCommands(many);
		expect(parsed).toHaveLength(64);
		expect(parsed?.at(-1)?.command).toBe("step 69");
	});

	it("exposes the commands as ipython details, and leaves details alone without them", async () => {
		const results: ReplExecuteResult[] = [
			{
				stdout: "built",
				stderr: "",
				status: "ok",
				durationMs: 1,
				bashCommands: [
					{ command: "npx tsgo --noEmit", exitCode: 0, startedAt: "2026-09-16T00:00:00+00:00" },
					{ command: "cargo test", exitCode: 101, commandTruncated: true },
				],
			},
			{ stdout: "plain", stderr: "", status: "ok", durationMs: 1 },
		];
		const kernel = { execute: async () => results.shift()! } as unknown as KernelClient;
		const provisioner = {
			ensure: async () => kernel,
			kill: async () => {},
			takeUnreportedExit: () => undefined,
		} as unknown as IpythonKernelProvisioner;
		const tool = createIpythonToolDefinition(dir, { provisioner });

		const built = await tool.execute("call-1", { code: "build" }, undefined, undefined, {} as ExtensionContext);
		expect(built.details.bashCommands).toEqual([
			{ command: "npx tsgo --noEmit", exitCode: 0 },
			{ command: "cargo test", exitCode: 101, commandTruncated: true },
		]);
		const plain = await tool.execute("call-2", { code: "plain" }, undefined, undefined, {} as ExtensionContext);
		expect(plain.details.bashCommands).toBeUndefined();
		expect("bashCommands" in plain.details).toBe(false);
	});

	const python = resolveReplPython();
	it.skipIf(!python)(
		"reports bash() commands a real kernel finished inside the cell",
		async () => {
			manager = new ReplKernelManager({ python: python as string, cwd: dir });
			const result = await manager.execute(
				"from rlm import bash\nawait bash('printf built')\nawait bash('exit 4')\nhandle = bash('sleep 0.2')",
			);
			expect(result.status).toBe("ok");
			expect(executedBashCommands(result)?.map(({ command, exitCode }) => ({ command, exitCode }))).toEqual([
				{ command: "printf built", exitCode: 0 },
				{ command: "exit 4", exitCode: 4 },
			]);
			const next = await manager.execute("import asyncio\nawait asyncio.sleep(0.4)\n(await handle).exit_code");
			expect(next.result).toBe("0");
			expect(executedBashCommands(next)).toBeUndefined();
		},
		30_000,
	);
});
