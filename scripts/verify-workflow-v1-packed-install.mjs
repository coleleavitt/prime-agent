#!/usr/bin/env node
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const root = resolve(new URL("..", import.meta.url).pathname);
const state = mkdtempSync(join(tmpdir(), "prime-packed-"));
const hostile = join(state, "hostile");
const prefix = join(state, "prefix");
const pack = join(state, "pack");
for (const path of [hostile, prefix, pack]) mkdirSync(path, { recursive: true });
const cleanEnv = Object.fromEntries(["PATH", "LANG", "LC_ALL", "LC_CTYPE", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS"]
  .filter((name) => process.env[name] !== undefined).map((name) => [name, process.env[name]]));
Object.assign(cleanEnv, { HOME: join(state, "home"), TMPDIR: join(state, "tmp"), XDG_CACHE_HOME: join(state, "cache"),
  XDG_CONFIG_HOME: join(state, "config"), XDG_DATA_HOME: join(state, "data"), XDG_STATE_HOME: join(state, "state"),
  UV_CACHE_DIR: join(state, "uv-cache"), CI: "1", TZ: "UTC", NO_COLOR: "1", FORCE_COLOR: "0" });
for (const name of ["HOME", "TMPDIR", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "UV_CACHE_DIR"]) mkdirSync(cleanEnv[name], { recursive: true });
const run = (command, args, cwd = hostile) => {
  const result = spawnSync(command, args, { cwd, env: cleanEnv, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed (${result.status}):\n${result.stdout ?? ""}${result.stderr ?? ""}`);
  return result.stdout.trim();
};
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const expected = {
  "workflow-v1.schema.json": "79913bb20831758935910a0a49b2ddaf40299c283f876b081cd75f21791f3b27",
  "workflow-native-host-v1.schema.json": "08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a",
};
const assertDigest = (path, digest) => {
  const bytes = readFileSync(path);
  const actual = sha256(bytes);
  if (actual !== digest) throw new Error(`${path}: expected ${digest}, got ${actual}`);
  const mutated = Buffer.from(bytes);
  mutated[Math.max(0, mutated.length - 2)] ^= 1;
  if (sha256(mutated) === digest) throw new Error(`${path}: digest mutant was accepted`);
};
try {
  // Verify every repository-owned and normally built schema copy. The verifier never
  // injects schemas into staging: normal copy-assets is the delivery authority.
  for (const [name, digest] of Object.entries(expected)) {
    for (const schemaPath of [
      join(root, "scripts", "fixtures", name),
      join(root, "prime-agent-runtime", "schemas", name),
      join(root, "packages", "coding-agent", "dist", "prime-agent-runtime", "schemas", name),
    ]) assertDigest(schemaPath, digest);
  }
  const packageJson = JSON.parse(readFileSync(join(root, "packages", "coding-agent", "package.json"), "utf8"));
  // Stage the exact already-built package outside the checkout without adding files.
  const packageStage = join(state, "package");
  cpSync(join(root, "packages", "coding-agent"), packageStage, { recursive: true, filter: (source) => !source.includes(`${join("", "node_modules")}`) });
  for (const [name, digest] of Object.entries(expected))
    assertDigest(join(packageStage, "dist", "prime-agent-runtime", "schemas", name), digest);
  // Pack internal workspaces too and make the staged CLI depend on those exact tarballs.
  const stagedJsonPath = join(packageStage, "package.json");
  const stagedJson = JSON.parse(readFileSync(stagedJsonPath, "utf8"));
  for (const workspace of ["ai", "tui", "agent"]) {
    const workspaceDir = join(root, "packages", workspace);
    const workspaceJson = JSON.parse(readFileSync(join(workspaceDir, "package.json"), "utf8"));
    const output = JSON.parse(run("npm", ["pack", workspaceDir, "--pack-destination", pack, "--json"], hostile));
    if (!Array.isArray(output) || output.length !== 1) throw new Error(`npm pack ${workspace} did not return one artifact`);
    stagedJson.dependencies[workspaceJson.name] = `file:${join(pack, output[0].filename)}`;
  }
  writeFileSync(stagedJsonPath, `${JSON.stringify(stagedJson, null, 2)}\n`);
  const tarballName = run("npm", ["pack", packageStage, "--pack-destination", pack, "--json"], hostile);
  const packed = JSON.parse(tarballName);
  if (!Array.isArray(packed) || packed.length !== 1) throw new Error("npm pack did not return exactly one artifact");
  const tarball = join(pack, packed[0].filename);
  run("npm", ["install", "--prefix", prefix, "--ignore-scripts", "--no-audit", "--no-fund", tarball]);
  const installed = join(prefix, "node_modules", packageJson.name);
  const installedPackage = JSON.parse(readFileSync(join(installed, "package.json"), "utf8"));
  if (installedPackage.name !== packageJson.name || installedPackage.version !== packageJson.version) throw new Error("installed package identity/version mismatch");
  for (const rel of ["dist/bundle/cli.js", "dist/prime-agent-runtime/src/rlm/workflow.py"]) {
    assertDigest(join(installed, rel), sha256(readFileSync(join(root, "packages", "coding-agent", rel))));
  }
  for (const [name, digest] of Object.entries(expected))
    assertDigest(join(installed, "dist", "prime-agent-runtime", "schemas", name), digest);
  const versionProbe = spawnSync(process.execPath, [join(installed, "dist", "bundle", "cli.js"), "--version"], { cwd: hostile, env: cleanEnv, encoding: "utf8" });
  const version = `${versionProbe.stdout ?? ""}${versionProbe.stderr ?? ""}`.trim();
  if (versionProbe.status !== 0 || !version.includes(packageJson.version)) throw new Error(`installed CLI version mismatch: ${version}`);
  const python = String.raw`
import asyncio, json
from rlm.workflow import run_agent, CapabilityUnavailable
request={"protocol":"prime.workflow.run-agent/v1","requestId":"packed-hostile","nodeId":"n","prompt":"x","model":None,"maxTurns":1,"maxResultUtf8Bytes":10,"drainTimeoutMs":10,"tools":"none"}
async def main():
    try: await run_agent(request)
    except CapabilityUnavailable: print(json.dumps({"installedPythonWorkflow":"capability-unavailable","requestValidated":True})); return
    raise SystemExit("packed runtime unexpectedly found a host")
asyncio.run(main())`;
  const pythonOut = run("uv", ["run", "--project", join(installed, "dist", "prime-agent-runtime"), "python", "-c", python]);
  const result = JSON.parse(pythonOut.split("\n").at(-1));
  if (result.installedPythonWorkflow !== "capability-unavailable" || result.requestValidated !== true) throw new Error("installed Python Workflow call failed validation");
  console.log(JSON.stringify({ package: installedPackage.name, version: installedPackage.version, hostileCwd: true,
    cleanEnvironment: true, bundleIdentity: true, schemas: expected, schemaCopiesVerified: 5,
    digestMutantsRejected: Object.keys(expected).length * 5, pythonWorkflow: result }, null, 2));
} finally { rmSync(state, { recursive: true, force: true }); }
