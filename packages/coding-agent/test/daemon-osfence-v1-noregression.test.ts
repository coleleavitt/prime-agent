import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { negotiateWorkflowV2Capability } from "../src/core/workflow-v2-capability.js";
import { resolveOsfenceMode } from "../src/modes/daemon/daemon-osfence.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC = join(HERE, "../src/modes/daemon");
const read = (name: string) => readFileSync(join(SRC, name), "utf8");

// --------------------------------------------------------------------------- base-off guard is off in production

describe("Workflow V2 Slice 3 base-off guard defaults off (§7.1)", () => {
	it("negotiateWorkflowV2Capability returns CAPABILITY_UNAVAILABLE", () => {
		expect(negotiateWorkflowV2Capability(undefined)).toEqual({ available: false, code: "CAPABILITY_UNAVAILABLE" });
	});
	it("the fence resolves disabled under the production capability verdict", () => {
		const mode = resolveOsfenceMode({
			capabilityAvailable: (negotiateWorkflowV2Capability(undefined) as { available: boolean }).available,
			platform: process.platform,
			controlDb: undefined,
			sqliteProbeOk: false,
			controlRootIsLocal: true,
		});
		expect(mode.enabled).toBe(false);
		expect(mode.capability).toBe("unavailable");
	});
});

// --------------------------------------------------------------------------- O4 static authority gate (doc §8 O4)

describe("O4 static authority gate — the V2 authority modules cite no demoted signal", () => {
	// The V2 ownership authority now spans the consumer (daemon-osfence.ts) and the shared control-DB
	// contract (osf-control-db.ts) it and supervisor-control-db.ts both conform to. The gate covers
	// BOTH files so the extraction cannot smuggle a demoted signal into the shared authority.
	const stripComments = (authority: string) =>
		authority
			.replace(/\/\*[\s\S]*?\*\//g, "")
			.split("\n")
			.map((line) => line.replace(/\/\/.*$/, ""))
			.join("\n");
	const authorityFiles = ["daemon-osfence.ts", "osf-control-db.ts"] as const;
	const codeByFile = new Map(authorityFiles.map((name) => [name, stripComments(read(name))]));

	it("imports no proper-lockfile", () => {
		for (const [name, code] of codeByFile) {
			expect(code, `${name} must not reference proper-lockfile`).not.toMatch(/proper-lockfile/);
			expect(code, `${name} must not import proper-lockfile`).not.toMatch(/from "proper-lockfile"/);
		}
	});

	it("calls no PID/liveness/mtime/TTL predicate on the authority path", () => {
		for (const banned of [
			"isProcessIdentityAlive",
			"isOwnerProcessAlive",
			"matchesExactProcessIdentity",
			"isProcessAlive",
			"processIdExists",
			"isZombieProcess",
			"getProcessStartId",
			"mtime",
			"mtimeMs",
			"expiresAt",
			"Date.now(",
			"setTimeout",
			"setInterval",
		]) {
			for (const [name, code] of codeByFile) {
				expect(code.includes(banned), `${name} must not use ${banned} in code`).toBe(false);
			}
		}
	});
	it("only endpoint_possession and control_db_generation_cas may authorize", () => {
		// No demoted signal name may appear as an authority value anywhere in either authority module.
		for (const demoted of ["pid_liveness", "proper_lockfile", "mtime_lease", "ttl_lease", "process_start_id"]) {
			for (const [name, code] of codeByFile) {
				expect(
					code.includes(`"${demoted}"`),
					`demoted signal ${demoted} must never be an authority value in ${name}`,
				).toBe(false);
			}
		}
		// The authority-signal type union is exactly the two OS-fence signals, declared once in the
		// shared authority module.
		const shared = codeByFile.get("osf-control-db.ts") ?? "";
		expect(shared).toMatch(/OsfenceAuthoritySignal = "endpoint_possession" \| "control_db_generation_cas";/);
		// The consumer re-exports it and never redeclares a competing union.
		expect(codeByFile.get("daemon-osfence.ts") ?? "").not.toMatch(/OsfenceAuthoritySignal =\s*"/);
	});
});

// --------------------------------------------------------------------------- V1 path byte-identity (behavioral+static)

describe("V1 daemon path is byte-identical while the fence is dormant (§7.1 C2)", () => {
	const supervisor = read("daemon-supervisor.ts");
	const mode = read("daemon-mode.ts");

	it("the wire generation falls back to the V1 random-UUID generation when the fence is off", () => {
		// wireGeneration() returns this.osfence?.adoptedGenerationString ?? this.generation.
		expect(supervisor).toMatch(/return this\.osfence\?\.adoptedGenerationString \?\? this\.generation;/);
		// this.osfence is only ever assigned inside elevateEndpointPossession, which is guarded by the mode.
		expect(supervisor).toMatch(
			/private elevateEndpointPossession\(\): void \{[\s\S]*?if \(!this\.osfenceMode\.enabled\) \{[\s\S]*?return;/,
		);
		// The generation field is still the per-process random UUID on the V1 path.
		expect(supervisor).toMatch(/private readonly generation = randomUUID\(\);/);
	});

	it("the supervisor claim and daemon_hello route through wireGeneration(), not a raw fence value", () => {
		const claims = [...supervisor.matchAll(/supervisorGeneration: this\.wireGeneration\(\),/g)];
		expect(claims.length).toBe(2); // supervisorAuthenticationClaim + daemon_hello
		expect(supervisor).not.toMatch(/supervisorGeneration: this\.generation,/);
	});

	it("every osfence hook in the owned daemon files is guarded by the disabled-by-default mode", () => {
		// Layer A elevation, the operator-fence replacement, and the worker recheck repoint are all gated.
		expect(supervisor).toMatch(/if \(this\.osfenceMode\.enabled\) \{\n\t\t\t\tthis\.elevateEndpointPossession\(\);/);
		expect(supervisor).toMatch(/if \(this\.osfenceMode\.enabled\) \{[\s\S]*?operator fence/);
		expect(mode).toMatch(/if \(this\.osfenceWorkerMode\.enabled && this\.osfenceControlDbReader\) \{/);
		expect(mode).toMatch(/if \(this\.osfenceWorkerMode\.enabled && parsed\.osfenceOffer\) \{/);
	});

	it("the V1 owner-current fence recheck (assertDaemonSupervisorOwnerCurrent) is retained as the default", () => {
		// The V1 diagnostic path is still the else branch of the repointed recheck.
		expect(mode).toMatch(/assertDaemonSupervisorOwnerCurrent/);
		expect(mode).toMatch(
			/} else \{\n\t\t\t\t\tboundClaim\.ownerFingerprint = await this\.assertSupervisorClaimCurrent\(/,
		);
	});
});
