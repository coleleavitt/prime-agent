#!/usr/bin/env node
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const root = resolve(new URL("..", import.meta.url).pathname);
const state = mkdtempSync(join(tmpdir(), "prime-v2-packed-"));
const hostile = join(state, "hostile");
const prefix = join(state, "prefix");
const pack = join(state, "pack");
for (const path of [hostile, prefix, pack]) mkdirSync(path, { recursive: true });
const env = Object.fromEntries(["PATH", "LANG", "LC_ALL", "LC_CTYPE", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS"]
  .filter((name) => process.env[name] !== undefined).map((name) => [name, process.env[name]]));
Object.assign(env, { HOME: join(state, "home"), TMPDIR: join(state, "tmp"), XDG_CACHE_HOME: join(state, "cache"),
  XDG_CONFIG_HOME: join(state, "config"), XDG_DATA_HOME: join(state, "data"), XDG_STATE_HOME: join(state, "state"),
  UV_CACHE_DIR: join(state, "uv-cache"), CI: "1", TZ: "UTC", NO_COLOR: "1", FORCE_COLOR: "0" });
for (const name of ["HOME", "TMPDIR", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "UV_CACHE_DIR"]) mkdirSync(env[name], { recursive: true });
const run = (command, args, cwd = hostile) => {
  const result = spawnSync(command, args, { cwd, env, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed (${result.status}):\n${result.stdout ?? ""}${result.stderr ?? ""}`);
  return result.stdout.trim();
};
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const files = {
  "schemas/workflow-v2.schema.json": "1f9088eca248f86bdfce97e23eb15f393ffc329a8fce9b33257729e3369b4a4a",
  "src/rlm/workflow_v2.py": null,
};
const sourceRuntime = join(root, "prime-agent-runtime");
files["src/rlm/workflow_v2.py"] = sha256(readFileSync(join(sourceRuntime, "src/rlm/workflow_v2.py")));

function verifyRuntime(runtime, phase) {
  const hashes = {};
  for (const [relative, expected] of Object.entries(files)) {
    const path = join(runtime, relative);
    if (!existsSync(path)) throw new Error(`${phase}: missing ${relative}`);
    const actual = sha256(readFileSync(path));
    if (actual !== expected) throw new Error(`${phase}: ${relative} expected ${expected}, got ${actual}`);
    hashes[relative] = actual;
  }
  return hashes;
}

try {
  const builtRuntime = join(root, "packages/coding-agent/dist/prime-agent-runtime");
  const phases = { source: verifyRuntime(sourceRuntime, "source"), built: verifyRuntime(builtRuntime, "built") };
  const packageStage = join(state, "package");
  cpSync(join(root, "packages/coding-agent"), packageStage, { recursive: true, filter: (source) => !source.includes(`${join("", "node_modules")}`) });
  const stagedJsonPath = join(packageStage, "package.json");
  const stagedJson = JSON.parse(readFileSync(stagedJsonPath, "utf8"));
  for (const workspace of ["ai", "tui", "agent"]) {
    const workspaceDir = join(root, "packages", workspace);
    const workspaceJson = JSON.parse(readFileSync(join(workspaceDir, "package.json"), "utf8"));
    const output = JSON.parse(run("npm", ["pack", workspaceDir, "--pack-destination", pack, "--json"]));
    stagedJson.dependencies[workspaceJson.name] = `file:${join(pack, output[0].filename)}`;
  }
  writeFileSync(stagedJsonPath, `${JSON.stringify(stagedJson, null, 2)}\n`);
  const packed = JSON.parse(run("npm", ["pack", packageStage, "--pack-destination", pack, "--json"]));
  const tarball = join(pack, packed[0].filename);
  const listing = run("tar", ["-tzf", tarball]).split("\n");
  for (const relative of Object.keys(files)) {
    const member = `package/dist/prime-agent-runtime/${relative}`;
    if (listing.filter((entry) => entry === member).length !== 1) throw new Error(`tarball member count is not one: ${member}`);
    const bytes = spawnSync("tar", ["-xOf", tarball, member], { cwd: hostile, env, encoding: null }).stdout;
    if (sha256(bytes) !== files[relative]) throw new Error(`tarball byte mismatch: ${member}`);
  }
  run("npm", ["install", "--prefix", prefix, "--ignore-scripts", "--no-audit", "--no-fund", tarball]);
  const installed = join(prefix, "node_modules", stagedJson.name);
  phases.installed = verifyRuntime(join(installed, "dist/prime-agent-runtime"), "installed");
  const probe = String.raw`import asyncio, json
from rlm import workflow_v2
request={"protocol":"prime.workflow.request/v2","requestId":"packed","action":"create","definition":{"protocol":"prime.workflow.definition/v2","nodes":[{"nodeId":"n","kind":"agent","prompt":"x","dependsOn":[],"model":"m","maxTurns":1,"tools":"none","maxTokens":1}],"outputs":["n"],"budget":{"maxConcurrentAttempts":1,"maxTotalTokens":1,"semantics":"soft_admission"}}}
async def main():
    try: await workflow_v2.request(request=request)
    except workflow_v2.CapabilityUnavailable as exc:
        assert exc.code == "CAPABILITY_UNAVAILABLE"
        print(json.dumps({"workflowV2":"capability-unavailable","lazyImport":True})); return
    raise SystemExit("Workflow V2 unexpectedly available")
asyncio.run(main())`;
  const output = run("uv", ["run", "--project", join(installed, "dist/prime-agent-runtime"), "python", "-c", probe]);
  const result = JSON.parse(output.split("\n").at(-1));
  console.log(JSON.stringify({ package: stagedJson.name, version: stagedJson.version, normalBuild: true, siblingTarballFeed: true,
    tarballMembersVerified: Object.keys(files), phases, installedProbe: result }, null, 2));
} finally {
  rmSync(state, { recursive: true, force: true });
}
