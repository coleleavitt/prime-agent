import { describe, expect, it } from "vitest";
import { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";

interface ClientInternals {
	socket?: { destroyed: boolean; destroy: () => void };
	channel?: { send: (...args: unknown[]) => Promise<void>; close: () => void };
	close(): void;
	request(command: { type: string }, timeoutMs: number): Promise<unknown>;
}

function makeClient(sendDelayMs: number): ClientInternals {
	const client = new DaemonWorkerClient("/tmp/prime-agent-test-never.sock") as unknown as ClientInternals;
	client.socket = { destroyed: false, destroy: () => {} };
	// A busy worker stops reading its socket, so the write callback that backs
	// channel.send only fires once the kernel buffer drains.
	client.channel = {
		send: () => new Promise<void>((resolve) => setTimeout(resolve, sendDelayMs)),
		close: () => {},
	};
	return client;
}

async function collectUnhandledRejections(run: () => Promise<void>): Promise<unknown[]> {
	const rejections: unknown[] = [];
	const listener = (reason: unknown) => rejections.push(reason);
	process.on("unhandledRejection", listener);
	try {
		await run();
	} finally {
		process.off("unhandledRejection", listener);
	}
	return rejections;
}

describe("daemon worker client backpressure", () => {
	it("does not raise an unhandled rejection when a request times out mid-send", async () => {
		const client = makeClient(120);
		const rejections = await collectUnhandledRejections(async () => {
			await expect(client.request({ type: "list" }, 20)).rejects.toThrow(
				"Timed out waiting for daemon worker response to list",
			);
			await new Promise((resolve) => setTimeout(resolve, 50));
		});

		expect(rejections).toEqual([]);
	});

	it("does not raise an unhandled rejection when close lands mid-send", async () => {
		const client = makeClient(120);
		const rejections = await collectUnhandledRejections(async () => {
			const pending = client.request({ type: "list" }, 5000);
			await new Promise((resolve) => setTimeout(resolve, 20));
			client.close();
			await expect(pending).rejects.toThrow("Daemon worker client closed");
			await new Promise((resolve) => setTimeout(resolve, 50));
		});

		expect(rejections).toEqual([]);
	});
});
