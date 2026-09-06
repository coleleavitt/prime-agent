import { installDefaultSpanSink, type SpanEndRecord, setSpanSink } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
	checkForNewPiVersion,
	comparePackageVersions,
	getLatestPiRelease,
	getLatestPiVersion,
	isNewerPackageVersion,
} from "../src/utils/version-check.js";

const defaultPrimeAgentDownloadBaseUrl = "https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev";
const originalSkipVersionCheck = process.env.PI_SKIP_VERSION_CHECK;
const originalOffline = process.env.PI_OFFLINE;
const originalPrimeAgentDownloadBaseUrl = process.env.PRIME_AGENT_DOWNLOAD_BASE_URL;

function restoreEnv(name: string, value: string | undefined): void {
	if (value === undefined) {
		delete process.env[name];
		return;
	}
	process.env[name] = value;
}

afterEach(() => {
	installDefaultSpanSink();
	vi.unstubAllGlobals();
	restoreEnv("PI_SKIP_VERSION_CHECK", originalSkipVersionCheck);
	restoreEnv("PI_OFFLINE", originalOffline);
	restoreEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", originalPrimeAgentDownloadBaseUrl);
});

describe("version checks", () => {
	it("compares package versions", () => {
		expect(comparePackageVersions("0.70.6", "0.70.5")).toBeGreaterThan(0);
		expect(comparePackageVersions("0.70.5", "0.70.5")).toBe(0);
		expect(comparePackageVersions("0.70.4", "0.70.5")).toBeLessThan(0);
		expect(comparePackageVersions("0.70.5-beta.10.1.abcdef0", "0.70.5-beta.9.1.1234567")).toBeGreaterThan(0);
		expect(isNewerPackageVersion("0.70.5", "0.70.5")).toBe(false);
		expect(isNewerPackageVersion("0.70.6", "0.70.5")).toBe(true);
	});

	it("returns only newer versions", async () => {
		const fetchMock = vi.fn(async () => Response.json({ version: "v1.2.3" }));
		vi.stubGlobal("fetch", fetchMock);

		await expect(checkForNewPiVersion("1.2.3")).resolves.toBeUndefined();
		await expect(checkForNewPiVersion("1.2.2")).resolves.toBe("1.2.3");
	});

	it("uses the Prime Agent release manifest with a Prime Agent user agent", async () => {
		const fetchMock = vi.fn(async () => Response.json({ version: "v1.2.4" }));
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestPiVersion("1.2.3")).resolves.toBe("1.2.4");
		expect(fetchMock).toHaveBeenCalledWith(
			`${defaultPrimeAgentDownloadBaseUrl}/latest.json`,
			expect.objectContaining({
				headers: expect.objectContaining({
					"User-Agent": expect.stringMatching(/^prime-agent\/1\.2\.3 /),
					accept: "application/json",
				}),
			}),
		);
	});

	it("keeps beta installations on the beta release manifest", async () => {
		const fetchMock = vi.fn(async () => Response.json({ version: "v1.2.4-beta.124.1.abcdef0" }));
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestPiVersion("1.2.4-beta.123.1.1234567")).resolves.toBe("1.2.4-beta.124.1.abcdef0");
		expect(fetchMock).toHaveBeenCalledWith(`${defaultPrimeAgentDownloadBaseUrl}/beta.json`, expect.any(Object));
	});

	it("returns the active package and tarball install spec from the release manifest", async () => {
		const fetchMock = vi.fn(async () =>
			Response.json({
				package: "prime-agent",
				tarball: "releases/v1.2.4/prime-agent-1.2.4.tgz",
				version: "v1.2.4",
			}),
		);
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestPiRelease("1.2.3")).resolves.toEqual({
			installSpec: `${defaultPrimeAgentDownloadBaseUrl}/releases/v1.2.4/prime-agent-1.2.4.tgz`,
			packageName: "prime-agent",
			version: "1.2.4",
		});
	});

	it("skips api calls when version checks are disabled", async () => {
		process.env.PI_SKIP_VERSION_CHECK = "1";
		const fetchMock = vi.fn();
		vi.stubGlobal("fetch", fetchMock);

		await expect(getLatestPiVersion("1.2.3")).resolves.toBeUndefined();
		expect(fetchMock).not.toHaveBeenCalled();
	});
});

describe("update.check span", () => {
	const spans: SpanEndRecord[] = [];

	function collectSpans(): void {
		spans.length = 0;
		setSpanSink((record) => spans.push(record));
	}

	it("records the manifest lookup with status, latest version and availability", async () => {
		collectSpans();
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "v1.2.4" })),
		);

		await expect(getLatestPiRelease("1.2.3")).resolves.toEqual({ version: "1.2.4" });

		expect(spans.map((span) => span.name)).toEqual(["update.check"]);
		expect(spans[0]).toMatchObject({
			status: "ok",
			attrs: { "update.current": "1.2.3", "update.latest": "1.2.4", "update.available": true, "http.status": 200 },
		});
	});

	it("reports an up-to-date install as not available", async () => {
		collectSpans();
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "1.2.3" })),
		);

		await expect(checkForNewPiVersion("1.2.3")).resolves.toBeUndefined();
		expect(spans[0]).toMatchObject({
			name: "update.check",
			status: "ok",
			attrs: { "update.latest": "1.2.3", "update.available": false, "http.status": 200 },
		});
	});

	it("keeps the http status on a non-ok manifest response without failing the span", async () => {
		collectSpans();
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => new Response("missing", { status: 404 })),
		);

		await expect(getLatestPiRelease("1.2.3")).resolves.toBeUndefined();
		expect(spans[0]).toMatchObject({ name: "update.check", status: "ok", attrs: { "http.status": 404 } });
		expect(spans[0].attrs).not.toHaveProperty("update.latest");
	});

	it("marks a network failure as a span error while callers keep swallowing it", async () => {
		collectSpans();
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Promise.reject(new Error("network unavailable"))),
		);

		// getLatestPiRelease re-throws exactly as before...
		await expect(getLatestPiRelease("1.2.3")).rejects.toThrow("network unavailable");
		expect(spans[0]).toMatchObject({ name: "update.check", status: "error", error: "network unavailable" });
		expect(spans[0].attrs).not.toHaveProperty("http.status");

		// ...and checkForNewPiVersion still swallows the failure.
		spans.length = 0;
		await expect(checkForNewPiVersion("1.2.3")).resolves.toBeUndefined();
		expect(spans[0]).toMatchObject({ name: "update.check", status: "error" });
	});

	it("opens no span when version checks are disabled", async () => {
		collectSpans();
		process.env.PI_OFFLINE = "1";
		vi.stubGlobal("fetch", vi.fn());

		await expect(getLatestPiRelease("1.2.3")).resolves.toBeUndefined();
		expect(spans).toEqual([]);
	});
});
