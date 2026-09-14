#!/usr/bin/env node
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const root = resolve(new URL("..", import.meta.url).pathname);
const authorityIndex = process.argv.indexOf("--authority");
const authorityPath = resolve(authorityIndex >= 0 && process.argv[authorityIndex + 1]
  ? process.argv[authorityIndex + 1]
  : join(root, "scripts/fixtures/workflow-v1-normative-authority.json"));
const EXPECTED = Object.freeze({
  format: "prime.workflow-v1-normative-authority/v1",
  repository: "coleleavitt/pi-plugin-workflow",
  commit: "1fa3ffc7372815e0f358e0e1854bba2396d40325",
  tree: "f9b12a14af53a1da5f00e2070c528e7b9923b478",
  bundleSha256: "b9bc393a60f3cb8439e65d9fffc00fc1c4c3916c3963c1c74b4f2c3e9c324dad",
  schemas: Object.freeze({
    "workflow-v1.schema.json": Object.freeze({ path: "docs/api/workflow-v1.schema.json", blob: "8bf5fd80b9e6a1525ec4c73e5f1a9ca4149e6830", sha256: "db3aa583523d4374e5ef455b1ada26744e0cd862d43384b86210e4c39bbf8663" }),
    "workflow-native-host-v1.schema.json": Object.freeze({ path: "docs/api/workflow-native-host-v1.schema.json", blob: "18eef21d5126ade6989016591a4e2fb0b8907b94", sha256: "08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a" }),
  }),
});
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const fail = (message) => { throw new Error(message); };
const readAuthority = (path) => {
  const value = JSON.parse(readFileSync(path, "utf8"));
  if (value.format !== EXPECTED.format || value.repository !== EXPECTED.repository || value.commit !== EXPECTED.commit || value.tree !== EXPECTED.tree) fail("normative authority identity mismatch");
  if (value.bundle?.sha256 !== EXPECTED.bundleSha256 || value.bundle?.ref !== "refs/authority/workflow-v1-1fa3ffc") fail("normative authority bundle identity mismatch");
  const schemas = Object.fromEntries((value.schemas ?? []).map((schema) => [schema.name, schema]));
  if (Object.keys(schemas).sort().join("\0") !== Object.keys(EXPECTED.schemas).sort().join("\0")) fail("normative authority schema set mismatch");
  for (const [name, expected] of Object.entries(EXPECTED.schemas)) {
    const schema = schemas[name];
    if (schema.path !== expected.path || schema.blob !== expected.blob || schema.sha256 !== expected.sha256) fail(`normative authority schema mismatch: ${name}`);
  }
  return value;
};
const authority = readAuthority(authorityPath);
const bundlePath = join(root, authority.bundle.path);
if (sha256(readFileSync(bundlePath)) !== EXPECTED.bundleSha256) fail("normative authority bundle digest mismatch");
const state = mkdtempSync(join(tmpdir(), "wv1-authority-"));
try {
  const run = (args, encoding = "utf8") => {
    const result = spawnSync("git", ["-C", state, ...args], { encoding, maxBuffer: 32 * 1024 * 1024 });
    if (result.status !== 0) fail(result.stderr?.toString() || `git ${args.join(" ")} failed`);
    return result.stdout;
  };
  run(["init", "-q"]);
  run(["fetch", "-q", bundlePath, authority.bundle.ref]);
  const commit = run(["rev-parse", "FETCH_HEAD^{commit}"]).trim();
  const tree = run(["rev-parse", "FETCH_HEAD^{tree}"]).trim();
  if (commit !== EXPECTED.commit || tree !== EXPECTED.tree) fail("normative authority commit/tree mismatch");
  for (const [name, expected] of Object.entries(EXPECTED.schemas)) {
    const blob = run(["rev-parse", `FETCH_HEAD:${expected.path}`]).trim();
    const bytes = run(["show", `FETCH_HEAD:${expected.path}`], null);
    if (blob !== expected.blob || sha256(bytes) !== expected.sha256) fail(`normative authority Git object mismatch: ${name}`);
    for (const copy of [join(root, "scripts/fixtures", name), join(root, "prime-agent-runtime/schemas", name)]) {
      if (sha256(readFileSync(copy)) !== expected.sha256) fail(`normative authority local copy mismatch: ${copy}`);
    }
  }
  // Required mutants: identity, bundle digest, and each schema binding fail closed.
  const mutants = [structuredClone(authority), structuredClone(authority), ...authority.schemas.map(() => structuredClone(authority))];
  mutants[0].commit = "0".repeat(40);
  mutants[1].bundle.sha256 = "0".repeat(64);
  authority.schemas.forEach((_, index) => { mutants[index + 2].schemas[index].sha256 = "0".repeat(64); });
  for (const mutant of mutants) {
    const path = join(state, `mutant-${Math.random().toString(16).slice(2)}.json`);
    writeFileSync(path, JSON.stringify(mutant));
    let rejected = false;
    try { readAuthority(path); } catch { rejected = true; }
    if (!rejected) fail("normative authority mutant was accepted");
  }
  console.log(JSON.stringify({ format: EXPECTED.format, repository: EXPECTED.repository, commit, tree, bundleSha256: EXPECTED.bundleSha256, schemas: EXPECTED.schemas, mutantsRejected: mutants.length }));
} finally { rmSync(state, { recursive: true, force: true }); }
