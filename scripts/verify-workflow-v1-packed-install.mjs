#!/usr/bin/env node
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
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
const spawn = (command, args, cwd = hostile) => spawnSync(command, args, { cwd, env: cleanEnv, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
const run = (command, args, cwd = hostile) => {
  const result = spawn(command, args, cwd);
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed (${result.status}):\n${result.stdout ?? ""}${result.stderr ?? ""}`);
  return result.stdout.trim();
};
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const expected = {
  "workflow-v1.schema.json": "79913bb20831758935910a0a49b2ddaf40299c283f876b081cd75f21791f3b27",
  "workflow-native-host-v1.schema.json": "08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a",
};
const tarMember = (name) => `package/dist/prime-agent-runtime/schemas/${name}`;
const evidence = [];

function validateSchemaBytes(name, bytes, location) {
  const actual = sha256(bytes);
  if (actual !== expected[name]) throw new Error(`${location}: digest mismatch: expected ${expected[name]}, got ${actual}`);
  return actual;
}
function validateSchemaDirectory(schemaDir) {
  const hashes = {};
  for (const name of Object.keys(expected)) {
    const path = join(schemaDir, name);
    if (!existsSync(path)) throw new Error(`${path}: required schema missing`);
    hashes[name] = validateSchemaBytes(name, readFileSync(path), path);
  }
  return hashes;
}
function inspectSchemaTarball(tarball) {
  // Do not trust npm's JSON file list. Enumerate the generated tgz itself, then
  // extract and hash each required member directly from that same archive.
  const listing = run("tar", ["-tzf", tarball]).split("\n").filter(Boolean);
  const hashes = {};
  for (const name of Object.keys(expected)) {
    const member = tarMember(name);
    const count = listing.filter((entry) => entry === member).length;
    if (count !== 1) throw new Error(`${tarball}: required member ${member} count was ${count}, expected 1`);
    const extracted = spawnSync("tar", ["-xOf", tarball, member], { cwd: hostile, env: cleanEnv, encoding: null, maxBuffer: 256 * 1024 * 1024 });
    if (extracted.status !== 0) throw new Error(`${tarball}: could not extract ${member}: ${String(extracted.stderr ?? "")}`);
    hashes[name] = validateSchemaBytes(name, extracted.stdout, `${tarball}:${member}`);
  }
  return { hashes, members: Object.keys(expected).map(tarMember) };
}
function expectRejected(phase, name, mutation, validate) {
  try {
    validate();
  } catch (error) {
    evidence.push({ phase, schema: name, mutation, rejected: true, reason: String(error.message ?? error) });
    return;
  }
  throw new Error(`${phase}/${name}/${mutation}: mutant was accepted`);
}
function exerciseDirectoryMutants(phase, schemaDir) {
  for (const name of Object.keys(expected)) {
    for (const mutation of ["alter", "omit"]) {
      const mutantDir = join(state, "mutants", phase, name, mutation);
      mkdirSync(mutantDir, { recursive: true });
      cpSync(schemaDir, mutantDir, { recursive: true });
      const target = join(mutantDir, name);
      if (mutation === "alter") {
        const bytes = Buffer.from(readFileSync(target));
        bytes[Math.max(0, bytes.length - 2)] ^= 1;
        writeFileSync(target, bytes);
      } else unlinkSync(target);
      expectRejected(phase, name, mutation, () => validateSchemaDirectory(mutantDir));
    }
  }
}
function exerciseTarballMutants(tarball) {
  const originalDigest = sha256(readFileSync(tarball));
  for (const name of Object.keys(expected)) {
    for (const mutation of ["alter", "omit"]) {
      const work = join(state, "tar-mutants", name, mutation);
      mkdirSync(work, { recursive: true });
      run("tar", ["-xzf", tarball, "-C", work]);
      const target = join(work, tarMember(name));
      if (mutation === "alter") {
        const bytes = Buffer.from(readFileSync(target));
        bytes[Math.max(0, bytes.length - 2)] ^= 1;
        writeFileSync(target, bytes);
      } else unlinkSync(target);
      const mutant = join(state, "tar-mutants", `${name}-${mutation}.tgz`);
      run("tar", ["-czf", mutant, "-C", work, "package"]);
      expectRejected("generated-tarball", name, mutation, () => inspectSchemaTarball(mutant));
    }
  }
  if (sha256(readFileSync(tarball)) !== originalDigest) throw new Error("generated npm tarball changed while constructing mutants");
}

try {
  const sourceSchemas = join(root, "prime-agent-runtime", "schemas");
  const builtSchemas = join(root, "packages", "coding-agent", "dist", "prime-agent-runtime", "schemas");
  // Fixture parity is an additional authority check, not a delivery phase.
  validateSchemaDirectory(join(root, "scripts", "fixtures"));
  const phaseHashes = {
    "repository-source": validateSchemaDirectory(sourceSchemas),
    "normal-built-dist": validateSchemaDirectory(builtSchemas),
  };
  exerciseDirectoryMutants("repository-source", sourceSchemas);
  exerciseDirectoryMutants("normal-built-dist", builtSchemas);

  const packageJson = JSON.parse(readFileSync(join(root, "packages", "coding-agent", "package.json"), "utf8"));
  // Stage the exact already-built package outside the checkout. Only dependency URLs
  // are rewritten to packed internal workspaces; no schemas or verifier are injected.
  const packageStage = join(state, "package");
  cpSync(join(root, "packages", "coding-agent"), packageStage, { recursive: true, filter: (source) => !source.includes(`${join("", "node_modules")}`) });
  validateSchemaDirectory(join(packageStage, "dist", "prime-agent-runtime", "schemas"));
  const stagedJsonPath = join(packageStage, "package.json");
  const stagedJson = JSON.parse(readFileSync(stagedJsonPath, "utf8"));
  for (const workspace of ["ai", "tui", "agent"]) {
    const workspaceDir = join(root, "packages", workspace);
    const workspaceJson = JSON.parse(readFileSync(join(workspaceDir, "package.json"), "utf8"));
    const output = JSON.parse(run("npm", ["pack", workspaceDir, "--pack-destination", pack, "--json"]));
    if (!Array.isArray(output) || output.length !== 1) throw new Error(`npm pack ${workspace} did not return one artifact`);
    stagedJson.dependencies[workspaceJson.name] = `file:${join(pack, output[0].filename)}`;
  }
  writeFileSync(stagedJsonPath, `${JSON.stringify(stagedJson, null, 2)}\n`);
  const packed = JSON.parse(run("npm", ["pack", packageStage, "--pack-destination", pack, "--json"]));
  if (!Array.isArray(packed) || packed.length !== 1) throw new Error("npm pack did not return exactly one artifact");
  const tarball = join(pack, packed[0].filename);
  const tarInspection = inspectSchemaTarball(tarball);
  phaseHashes["generated-tarball"] = tarInspection.hashes;
  exerciseTarballMutants(tarball);

  run("npm", ["install", "--prefix", prefix, "--ignore-scripts", "--no-audit", "--no-fund", tarball]);
  const installed = join(prefix, "node_modules", packageJson.name);
  const installedPackage = JSON.parse(readFileSync(join(installed, "package.json"), "utf8"));
  if (installedPackage.name !== packageJson.name || installedPackage.version !== packageJson.version) throw new Error("installed package identity/version mismatch");
  for (const rel of ["dist/bundle/cli.js", "dist/prime-agent-runtime/src/rlm/workflow.py"]) {
    const source = join(root, "packages", "coding-agent", rel);
    const destination = join(installed, rel);
    const expectedFileDigest = sha256(readFileSync(source));
    const installedFileDigest = sha256(readFileSync(destination));
    if (installedFileDigest !== expectedFileDigest) throw new Error(`${destination}: expected ${expectedFileDigest}, got ${installedFileDigest}`);
  }
  const installedSchemas = join(installed, "dist", "prime-agent-runtime", "schemas");
  phaseHashes["installed-package"] = validateSchemaDirectory(installedSchemas);
  exerciseDirectoryMutants("installed-package", installedSchemas);

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
  if (evidence.length !== 16 || evidence.some((item) => item.rejected !== true)) throw new Error(`expected 16 rejected phase mutants, got ${evidence.length}`);
  console.log(JSON.stringify({ package: installedPackage.name, version: installedPackage.version, hostileCwd: true,
    cleanEnvironment: true, bundleIdentity: true, schemas: expected, phaseHashes,
    tarballMembersEnumeratedAndExtracted: tarInspection.members, schemaMutationEvidence: evidence,
    schemaMutantsRejected: evidence.length, pythonWorkflow: result }, null, 2));
} finally { rmSync(state, { recursive: true, force: true }); }
