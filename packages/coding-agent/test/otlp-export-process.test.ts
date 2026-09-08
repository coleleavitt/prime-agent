import { type ChildProcess, spawn } from "node:child_process";
import { createServer, type Server } from "node:http";
import { resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

const fixturePath = resolve(__dirname, "fixtures/otlp-runtime-fixture.ts");
const children = new Set<ChildProcess>();
const servers = new Set<Server>();

afterEach(() => {
	for (const child of children) {
		if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
	}
	children.clear();
	for (const server of servers) {
		server.closeAllConnections();
		server.close();
	}
	servers.clear();
});

async function listen(server: Server): Promise<number> {
	servers.add(server);
	await new Promise<void>((resolvePromise, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolvePromise);
	});
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("collector did not bind TCP");
	return address.port;
}

function spawnFixture(endpoint: string, args: string[]): ChildProcess {
	const child = spawn(process.execPath, ["--import", "tsx", fixturePath, ...args], {
		env: {
			...process.env,
			OTEL_EXPORTER_OTLP_ENDPOINT: endpoint,
			TSX_TSCONFIG_PATH: resolve(__dirname, "../../../tsconfig.json"),
		},
		stdio: ["ignore", "pipe", "pipe"],
	});
	children.add(child);
	return child;
}

async function waitForExit(child: ChildProcess): Promise<{ code: number | null; stderr: string }> {
	let stderr = "";
	child.stderr?.on("data", (chunk) => {
		stderr += chunk.toString();
	});
	return await new Promise((resolvePromise, reject) => {
		const timeout = setTimeout(() => reject(new Error(`OTLP fixture timed out: ${stderr}`)), 5_000);
		child.once("error", reject);
		child.once("close", (code) => {
			clearTimeout(timeout);
			children.delete(child);
			resolvePromise({ code, stderr });
		});
	});
}

async function waitForReady(child: ChildProcess): Promise<void> {
	await new Promise<void>((resolvePromise, reject) => {
		let stdout = "";
		const timeout = setTimeout(() => reject(new Error("OTLP fixture did not become ready")), 5_000);
		child.stdout?.on("data", (chunk) => {
			stdout += chunk.toString();
			if (stdout.includes("READY")) {
				clearTimeout(timeout);
				resolvePromise();
			}
		});
	});
}

function acceptingCollector(payloads: Array<{ url: string; body: unknown }>): Server {
	return createServer((request, response) => {
		const chunks: Buffer[] = [];
		request.on("data", (chunk) => chunks.push(chunk));
		request.on("end", () => {
			payloads.push({ url: request.url ?? "", body: JSON.parse(Buffer.concat(chunks).toString()) });
			response.writeHead(200).end();
		});
	});
}

describe("OTLP process lifecycle", () => {
	it.each([
		["normal completion", false],
		["SIGTERM", true],
	] as const)("exports real payloads on %s", async (_label, signal) => {
		const payloads: Array<{ url: string; body: unknown }> = [];
		const port = await listen(acceptingCollector(payloads));
		const child = spawnFixture(`http://127.0.0.1:${port}`, [
			"process-span",
			...(signal ? ["--wait-for-signal"] : []),
		]);
		if (signal) {
			await waitForReady(child);
			child.kill("SIGTERM");
		}
		const result = await waitForExit(child);
		expect(result).toEqual({ code: 0, stderr: "" });
		expect(payloads.map(({ url }) => url).sort()).toEqual(["/v1/metrics", "/v1/traces"]);
		const traces = payloads.find(({ url }) => url === "/v1/traces")?.body;
		expect(JSON.stringify(traces)).toContain("process-span");
		expect(JSON.stringify(traces)).toContain("fixture-version");
	});

	it("bounds shutdown when the collector never responds", async () => {
		const port = await listen(createServer((_request, _response) => {}));
		const child = spawnFixture(`http://127.0.0.1:${port}`, ["hanging-span"]);
		const started = Date.now();
		const result = await waitForExit(child);
		const elapsed = Date.now() - started;
		expect(result).toEqual({ code: 0, stderr: "" });
		expect(elapsed).toBeGreaterThanOrEqual(800);
		expect(elapsed).toBeLessThan(3_000);
	});
});
