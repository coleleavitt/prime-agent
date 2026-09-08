import { describe, expect, it } from "vitest";
import type { DaemonClient, DaemonClientRequestOptions } from "../src/modes/daemon/daemon-client.js";
import type { DaemonCommand, DaemonResponse } from "../src/modes/daemon/daemon-protocol.js";
import {
	deleteDaemonSavedSession,
	fetchDaemonSavedSessionSearchText,
	listDaemonSavedSessions,
	renameDaemonSavedSession,
} from "../src/modes/daemon/saved-session-catalog.js";
import { serializeSavedSessionInfo } from "../src/modes/daemon/saved-session-info.js";

class FakeDaemonClient {
	readonly commands: DaemonCommand[] = [];
	constructor(readonly capabilities: readonly string[] = []) {}

	supportsServerCapability(capability: string): boolean {
		return this.capabilities.includes(capability);
	}

	async request(
		command: DaemonCommand,
		_timeoutMs = 30000,
		options: DaemonClientRequestOptions = {},
	): Promise<DaemonResponse> {
		this.commands.push(command);
		if (command.type === "list_saved_sessions") {
			options.onProgress?.({
				type: "session_list_progress",
				command: "list_saved_sessions",
				loaded: 1,
				total: 1,
			});
			options.onProgress?.({
				type: "session_list_item",
				command: "list_saved_sessions",
				session: {
					path: "/tmp/sessions/one.jsonl",
					id: "one",
					cwd: "/tmp/project",
					parentSessionPath: "/tmp/sessions/parent.jsonl",
					rlmDepth: 1,
					created: "2026-01-01T00:00:00.000Z",
					modified: "2026-01-02T00:00:00.000Z",
					messageCount: 1,
					firstMessage: "hello",
					allMessagesText: "hello",
					agentStatus: {
						summary: "Finished the task",
						taskState: "completed",
						basedOnMessageCount: 1,
					},
				},
			});
			return {
				type: "response",
				command: "list_saved_sessions",
				success: true,
				data: {
					sessions: [
						{
							path: "/tmp/sessions/one.jsonl",
							id: "one",
							cwd: "/tmp/project",
							parentSessionPath: "/tmp/sessions/parent.jsonl",
							rlmDepth: 1,
							created: "2026-01-01T00:00:00.000Z",
							modified: "2026-01-02T00:00:00.000Z",
							messageCount: 1,
							firstMessage: "hello",
							allMessagesText: "hello",
							agentStatus: {
								summary: "Finished the task",
								taskState: "completed",
								basedOnMessageCount: 1,
							},
						},
					],
				},
			};
		}
		if (command.type === "get_saved_session_search_text") {
			return {
				type: "response",
				command: "get_saved_session_search_text",
				success: true,
				data: {
					entries: command.paths.map((path) => ({ path, allMessagesText: `corpus for ${path}` })),
				},
			};
		}
		if (command.type === "delete_saved_session") {
			return {
				type: "response",
				command: "delete_saved_session",
				success: true,
				data: { ok: true, method: "trash" },
			};
		}
		return { type: "response", command: command.type, success: true };
	}
}

function asDaemonClient(client: FakeDaemonClient): DaemonClient {
	return client as unknown as DaemonClient;
}

describe("saved session catalog", () => {
	it("streams detached saved sessions through the daemon catalog", async () => {
		const fakeClient = new FakeDaemonClient();
		const progress: Array<[number, number]> = [];
		const discovered: string[] = [];

		const sessions = await listDaemonSavedSessions(
			asDaemonClient(fakeClient),
			{ cwd: "/tmp/project", sessionDir: "/tmp/sessions" },
			"current",
			{
				onProgress: (loaded, total) => progress.push([loaded, total]),
				onSession: (session) => discovered.push(session.id),
			},
		);

		expect(fakeClient.commands[0]).toEqual({
			type: "list_saved_sessions",
			cwd: "/tmp/project",
			sessionDir: "/tmp/sessions",
			scope: "current",
		});
		expect(progress).toEqual([[1, 1]]);
		expect(discovered).toEqual(["one"]);
		expect(sessions).toEqual([
			{
				path: "/tmp/sessions/one.jsonl",
				id: "one",
				cwd: "/tmp/project",
				parentSessionPath: "/tmp/sessions/parent.jsonl",
				rlmDepth: 1,
				created: new Date("2026-01-01T00:00:00.000Z"),
				modified: new Date("2026-01-02T00:00:00.000Z"),
				messageCount: 1,
				firstMessage: "hello",
				allMessagesText: "hello",
				agentStatus: {
					summary: "Finished the task",
					taskState: "completed",
					basedOnMessageCount: 1,
				},
			},
		]);
	});

	it("mutates detached saved sessions without an active session id", async () => {
		const fakeClient = new FakeDaemonClient();
		const context = { cwd: "/tmp/project", sessionDir: "/tmp/sessions" };

		await renameDaemonSavedSession(asDaemonClient(fakeClient), context, "/tmp/sessions/one.jsonl", "One");
		await expect(
			deleteDaemonSavedSession(asDaemonClient(fakeClient), context, "/tmp/sessions/one.jsonl"),
		).resolves.toEqual({
			ok: true,
			method: "trash",
		});

		expect(fakeClient.commands).toEqual([
			{ type: "rename_saved_session", sessionPath: "/tmp/sessions/one.jsonl", name: "One" },
			{ type: "delete_saved_session", sessionPath: "/tmp/sessions/one.jsonl" },
		]);
	});

	it("keeps sending the transcript corpus to a daemon without the capability", async () => {
		const fakeClient = new FakeDaemonClient();

		await listDaemonSavedSessions(
			asDaemonClient(fakeClient),
			{ cwd: "/tmp/project", sessionDir: "/tmp/sessions" },
			"current",
			undefined,
			{ includeSearchText: false },
		);

		// Opting out of a daemon that cannot serve the corpus separately would
		// lose it, not defer it.
		expect(fakeClient.commands[0]).not.toHaveProperty("includeSearchText");
		expect(await fetchDaemonSavedSessionSearchText(asDaemonClient(fakeClient), ["/tmp/sessions/one.jsonl"])).toEqual(
			new Map(),
		);
		expect(fakeClient.commands).toHaveLength(1);
	});

	it("defers the transcript corpus against a daemon that advertises the capability", async () => {
		const fakeClient = new FakeDaemonClient(["deferred_session_search_text"]);

		await listDaemonSavedSessions(
			asDaemonClient(fakeClient),
			{ cwd: "/tmp/project", sessionDir: "/tmp/sessions" },
			"current",
			undefined,
			{ includeSearchText: false },
		);
		const corpora = await fetchDaemonSavedSessionSearchText(asDaemonClient(fakeClient), ["/tmp/sessions/one.jsonl"]);

		expect(fakeClient.commands[0]).toMatchObject({ type: "list_saved_sessions", includeSearchText: false });
		expect(fakeClient.commands[1]).toMatchObject({
			type: "get_saved_session_search_text",
			paths: ["/tmp/sessions/one.jsonl"],
		});
		expect(corpora.get("/tmp/sessions/one.jsonl")).toBe("corpus for /tmp/sessions/one.jsonl");
	});

	it("serializes the corpus unless the client opted out", () => {
		const session = {
			path: "/tmp/sessions/one.jsonl",
			id: "one",
			cwd: "/tmp/project",
			rlmDepth: 0,
			created: new Date("2026-01-01T00:00:00.000Z"),
			modified: new Date("2026-01-02T00:00:00.000Z"),
			messageCount: 1,
			firstMessage: "hello",
			allMessagesText: "hello transcript",
		};

		// Absent options is the old-client path and must still carry the corpus.
		expect(serializeSavedSessionInfo(session).allMessagesText).toBe("hello transcript");
		expect(serializeSavedSessionInfo(session, { includeSearchText: true }).allMessagesText).toBe("hello transcript");
		expect(serializeSavedSessionInfo(session, { includeSearchText: false }).allMessagesText).toBe("");
	});
});
