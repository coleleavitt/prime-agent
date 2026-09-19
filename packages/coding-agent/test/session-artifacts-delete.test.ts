import { existsSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.js";
import { getLocalRefinementHistoryDir } from "../src/core/refinement/history-paths.js";
import { deleteSessionArtifacts, deleteSessionFile } from "../src/core/session-file-actions.js";

let root = "";
let previousAgentDir: string | undefined;

describe("deleteSessionFile removes the session artifact directory", () => {
	beforeEach(() => {
		root = mkdtempSync(join(tmpdir(), "prime-agent-session-delete-"));
		// Redirect durable refinement logs into the sandbox so delete never touches ~/.prime/agent.
		previousAgentDir = process.env[ENV_AGENT_DIR];
		process.env[ENV_AGENT_DIR] = join(root, "agent");
	});

	afterEach(() => {
		if (previousAgentDir === undefined) delete process.env[ENV_AGENT_DIR];
		else process.env[ENV_AGENT_DIR] = previousAgentDir;
		previousAgentDir = undefined;
		if (root) rmSync(root, { recursive: true, force: true });
		root = "";
	});

	it("permanently deletes <root>/session-artifacts/<id> alongside the session file", async () => {
		const sessionId = "session-xyz";
		const sessionsDir = join(root, "sessions");
		mkdirSync(sessionsDir, { recursive: true });
		const sessionPath = join(sessionsDir, `${sessionId}.jsonl`);
		writeFileSync(sessionPath, '{"type":"session"}\n');

		const artifactDir = join(root, "session-artifacts", sessionId);
		mkdirSync(artifactDir, { recursive: true });
		writeFileSync(join(artifactDir, "kernel-state.dill"), "payload");
		writeFileSync(join(artifactDir, "kernel-state.json"), "{}");
		writeFileSync(join(artifactDir, "scheduled-jobs.json"), '{"jobs":[],"dispatches":[]}\n');

		const result = await deleteSessionFile(sessionPath);

		expect(result.ok).toBe(true);
		expect(existsSync(artifactDir)).toBe(false);
		expect(existsSync(sessionPath)).toBe(false);
	});

	it("runs the file-removed callback before deleting artifacts", async () => {
		const sessionId = "session-with-callback";
		const sessionsDir = join(root, "sessions");
		mkdirSync(sessionsDir, { recursive: true });
		const sessionPath = join(sessionsDir, `${sessionId}.jsonl`);
		writeFileSync(sessionPath, '{"type":"session"}\n');

		const artifactDir = join(root, "session-artifacts", sessionId);
		mkdirSync(artifactDir, { recursive: true });
		writeFileSync(join(artifactDir, "kernel-state.dill"), "payload");
		let wasSessionRemovedBeforeCallback = false;
		let wereArtifactsPresentDuringCallback = false;

		const result = await deleteSessionFile(sessionPath, {
			afterFileRemoved: () => {
				wasSessionRemovedBeforeCallback = !existsSync(sessionPath);
				wereArtifactsPresentDuringCallback = existsSync(artifactDir);
			},
		});

		expect(result.ok).toBe(true);
		expect(wasSessionRemovedBeforeCallback).toBe(true);
		expect(wereArtifactsPresentDuringCallback).toBe(true);
		expect(existsSync(artifactDir)).toBe(false);
	});

	it("never removes the artifacts root for a degenerate session file name", async () => {
		const sessionsDir = join(root, "sessions");
		mkdirSync(sessionsDir, { recursive: true });
		const artifactsRoot = join(root, "session-artifacts");
		mkdirSync(join(artifactsRoot, "live-session"), { recursive: true });

		// Basename ".jsonl" strips to an empty id, which would resolve to the root.
		await deleteSessionArtifacts(join(sessionsDir, ".jsonl"));

		expect(existsSync(join(artifactsRoot, "live-session"))).toBe(true);
	});

	it("succeeds when the session has no artifact directory", async () => {
		const sessionsDir = join(root, "sessions");
		mkdirSync(sessionsDir, { recursive: true });
		const sessionPath = join(sessionsDir, "no-artifacts.jsonl");
		writeFileSync(sessionPath, "{}\n");

		const result = await deleteSessionFile(sessionPath);
		expect(result.ok).toBe(true);
	});

	it("does not follow a symlinked directory when collecting child session ids", async () => {
		const sessionId = "session-with-symlink";
		const sessionsDir = join(root, "sessions");
		mkdirSync(sessionsDir, { recursive: true });
		const sessionPath = join(sessionsDir, `${sessionId}.jsonl`);
		writeFileSync(sessionPath, '{"type":"session"}\n');

		const artifactDir = join(root, "session-artifacts", sessionId);

		// A real nested child transcript: its id must be collected and its refinement log removed.
		const realChildId = "real-child-1";
		const realSubDir = join(artifactDir, "sub-11111111");
		mkdirSync(realSubDir, { recursive: true });
		writeFileSync(join(realSubDir, "transcript.jsonl"), `{"type":"session","id":"${realChildId}"}\n`);

		// A transcript reachable only by following a symlink. The link is named like a child
		// session dir, so a symlink-following walk would collect the target's id and delete its
		// refinement log — the escape this guards against.
		const leakedChildId = "leaked-child-1";
		const outsideDir = join(root, "outside");
		mkdirSync(outsideDir, { recursive: true });
		const leakedTranscript = join(outsideDir, "transcript.jsonl");
		writeFileSync(leakedTranscript, `{"type":"session","id":"${leakedChildId}"}\n`);
		symlinkSync(outsideDir, join(artifactDir, "sub-deadbeef"), "dir");

		const refinementDir = getLocalRefinementHistoryDir();
		mkdirSync(refinementDir, { recursive: true });
		const realChildHistory = join(refinementDir, `${realChildId}.jsonl`);
		const leakedChildHistory = join(refinementDir, `${leakedChildId}.jsonl`);
		writeFileSync(realChildHistory, "{}\n");
		writeFileSync(leakedChildHistory, "{}\n");

		const result = await deleteSessionFile(sessionPath);

		expect(result.ok).toBe(true);
		expect(existsSync(artifactDir)).toBe(false);
		// The real child's log is gone; the symlinked-out child's log is untouched.
		expect(existsSync(realChildHistory)).toBe(false);
		expect(existsSync(leakedChildHistory)).toBe(true);
		// Removing the artifact dir unlinks the symlink, never its target.
		expect(existsSync(leakedTranscript)).toBe(true);
	});
});
