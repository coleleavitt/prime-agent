import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";
import { afterEach, describe, expect, it } from "vitest";
import { appendRotatingLog, redactLocalLog } from "../src/config.js";

let tempDir: string | undefined;

afterEach(() => {
	if (tempDir) {
		chmodSync(tempDir, 0o700);
		rmSync(tempDir, { recursive: true, force: true });
		tempDir = undefined;
	}
});

function logPath(): string {
	tempDir = mkdtempSync(join(tmpdir(), "prime-agent-local-log-"));
	mkdirSync(join(tempDir, "logs"));
	return join(tempDir, "logs", "agent.log");
}

describe("local diagnostic logs", () => {
	it("redacts only high-confidence credential forms", () => {
		const input = [
			"Authorization: Bearer header.payload.signature",
			"api_key=sk-abcdefghijklmnopqrstuvwxyz",
			"refreshToken: secret-value",
			"github ghp_1234567890abcdefghijklmnop",
			"Basic dXNlcjpwYXNz",
			"Cookie: session=abc123",
			"https://user:pass@example.test/path?token=query-secret&ok=1",
			"ordinary identifier sk-short remains",
		].join("\n");

		const output = redactLocalLog(input);

		expect(output).not.toContain("header.payload.signature");
		expect(output).not.toContain("sk-abcdefghijklmnopqrstuvwxyz");
		expect(output).not.toContain("secret-value");
		expect(output).not.toContain("ghp_1234567890abcdefghijklmnop");
		expect(output).not.toContain("dXNlcjpwYXNz");
		expect(output).not.toContain("abc123");
		expect(output).not.toContain("user:pass");
		expect(output).not.toContain("query-secret");
		expect(output).toContain("sk-short remains");
	});

	it.runIf(process.platform !== "win32")("creates owner-only directories and files", () => {
		const path = logPath();
		appendRotatingLog(path, "apiKey=super-secret");

		expect(statSync(join(tempDir!, "logs")).mode & 0o777).toBe(0o700);
		expect(statSync(path).mode & 0o777).toBe(0o600);
		expect(readFileSync(path, "utf8")).toBe("apiKey=[REDACTED]\n");
	});

	it("retains a bounded newest .old file and gzip-compressed older generations", () => {
		const path = logPath();
		writeFileSync(path, "first-generation");

		appendRotatingLog(path, "second", 1, 3);
		appendRotatingLog(path, "third", 1, 3);
		appendRotatingLog(path, "fourth", 1, 3);
		appendRotatingLog(path, "fifth", 1, 3);

		expect(readFileSync(path, "utf8")).toBe("fifth\n");
		expect(readFileSync(`${path}.old`, "utf8")).toBe("fourth\n");
		expect(gunzipSync(readFileSync(`${path}.old.1.gz`)).toString()).toBe("third\n");
		expect(() => statSync(`${path}.old.2.gz`)).toThrow();
	});

	it("removes excess compressed generations when retention is lowered", () => {
		const path = logPath();
		writeFileSync(path, "one");
		for (const line of ["two", "three", "four"]) appendRotatingLog(path, line, 1, 4);
		expect(statSync(`${path}.old.2.gz`).isFile()).toBe(true);

		appendRotatingLog(path, "five", 1, 1);

		expect(() => statSync(`${path}.old`)).toThrow();
		expect(() => statSync(`${path}.old.1.gz`)).toThrow();
		expect(() => statSync(`${path}.old.2.gz`)).toThrow();
	});
});
