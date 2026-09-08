import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.js";
import { installOtlpExporterFromEnv } from "../src/core/otlp-export.js";
import { fatalCrashFields, installFatalCrashHandlers } from "../src/core/process-crash.js";

const fixturePath = resolve(__dirname, "fixtures/process-crash-fixture.ts");
const tempDirs: string[] = [];

afterEach(() => {
	for (const directory of tempDirs.splice(0)) rmSync(directory, { recursive: true, force: true });
});

async function runCrashFixture(kind: "uncaughtException" | "unhandledRejection") {
	const agentDir = mkdtempSync(join(tmpdir(), "prime-crash-fixture-"));
	tempDirs.push(agentDir);
	return await new Promise<{ code: number | null; stderr: string; log: string }>((resolvePromise, reject) => {
		const child = spawn(process.execPath, ["--import", "tsx", fixturePath, kind], {
			env: {
				...process.env,
				[ENV_AGENT_DIR]: agentDir,
				TSX_TSCONFIG_PATH: resolve(__dirname, "../../../tsconfig.json"),
			},
			stdio: ["ignore", "ignore", "pipe"],
		});
		let stderr = "";
		child.stderr.on("data", (chunk) => {
			stderr += chunk.toString();
		});
		child.once("error", reject);
		child.once("close", (code) => {
			resolvePromise({ code, stderr, log: readFileSync(join(agentDir, "logs", "agent.jsonl"), "utf8") });
		});
	});
}

describe("fatal crash diagnostics", () => {
	it("serializes errors with a stable structured shape", () => {
		const error = new TypeError("broken");
		expect(fatalCrashFields("top_level_rejection", error)).toMatchObject({
			kind: "top_level_rejection",
			errorName: "TypeError",
			errorMessage: "broken",
			stack: expect.stringContaining("TypeError: broken"),
		});
	});

	it("serializes non-Error rejection reasons", () => {
		expect(fatalCrashFields("unhandled_rejection", "nope")).toEqual({
			kind: "unhandled_rejection",
			errorMessage: "nope",
		});
	});

	it("drains the installed OTLP exporter before exiting", async () => {
		let releaseShutdown: (() => void) | undefined;
		const shutdown = vi.fn(
			() =>
				new Promise<void>((resolve) => {
					releaseShutdown = resolve;
				}),
		);
		installOtlpExporterFromEnv({
			version: "test",
			env: { OTEL_EXPORTER_OTLP_ENDPOINT: "http://collector.invalid" },
			shutdownTimeoutMs: 5_000,
			createExporter: () => ({
				sink: () => undefined,
				flush: async () => undefined,
				shutdown,
				abort: () => undefined,
				metrics: () => [],
				stats: () => ({ queued: 0, dropped: 0, exportErrors: 0 }),
			}),
		});
		const exit = vi.spyOn(process, "exit").mockImplementation((() => undefined) as never);
		const cleanup = installFatalCrashHandlers();
		process.emit("uncaughtException", new Error("drain before exit"));
		await vi.waitFor(() => expect(shutdown).toHaveBeenCalledOnce());
		expect(exit).not.toHaveBeenCalled();
		releaseShutdown?.();
		await vi.waitFor(() => expect(exit).toHaveBeenCalledWith(1));
		cleanup();
		exit.mockRestore();
	});

	it.each([
		["uncaughtException", "uncaught_exception", "fixture uncaught exception"],
		["unhandledRejection", "unhandled_rejection", "fixture unhandled rejection"],
	] as const)("logs and exits for a real %s", async (trigger, kind, message) => {
		const result = await runCrashFixture(trigger);
		expect(result.code).toBe(1);
		expect(result.stderr).toContain(message);
		const entries: Array<Record<string, unknown>> = result.log
			.trim()
			.split("\n")
			.map((line) => JSON.parse(line));
		expect(entries).toContainEqual(
			expect.objectContaining({
				level: "error",
				component: "process",
				msg: "fatal_crash",
				kind,
				errorMessage: message,
			}),
		);
	});
});
