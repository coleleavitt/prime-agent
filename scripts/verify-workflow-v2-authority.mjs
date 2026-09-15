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
  : join(root, "scripts/fixtures/workflow-v2-schema-authority.json"));
const EXPECTED = Object.freeze({
  format: "prime.workflow-v2-schema-authority/v1",
  repository: "coleleavitt/pi-plugin-workflow",
  commit: "af31f5e5e6e28cfd2577ca10012d20c4075fe564",
  tree: "1151a584836dc255f99ef0f84e79540148a264ed",
  bundleSha256: "00a33330b6bdcd7e99b929ed0216acc575a26b09ec73e218c42b6e76807380cb",
  schemas: Object.freeze({
    "workflow-v2.schema.json": Object.freeze({ path: "docs/api/workflow-v2.schema.json", blob: "461a8e9ef7b44127567822979ce938f60cabb385", sha256: "1f9088eca248f86bdfce97e23eb15f393ffc329a8fce9b33257729e3369b4a4a" }),
  }),
});
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const fail = (message) => { throw new Error(message); };
const readAuthority = (path) => {
  const value = JSON.parse(readFileSync(path, "utf8"));
  if (value.format !== EXPECTED.format || value.repository !== EXPECTED.repository || value.commit !== EXPECTED.commit || value.tree !== EXPECTED.tree) fail("Workflow V2 schema authority identity mismatch");
  if (value.bundle?.path !== "scripts/fixtures/pi-plugin-workflow-af31f5e.bundle" || value.bundle?.sha256 !== EXPECTED.bundleSha256 || value.bundle?.ref !== "refs/authority/workflow-v2-af31f5e") fail("Workflow V2 schema authority bundle identity mismatch");
  const schemaEntries = value.schemas ?? [];
  const schemas = Object.fromEntries(schemaEntries.map((schema) => [schema.name, schema]));
  if (schemaEntries.length !== Object.keys(EXPECTED.schemas).length || Object.keys(schemas).sort().join("\0") !== Object.keys(EXPECTED.schemas).sort().join("\0")) fail("Workflow V2 schema authority schema set mismatch");
  for (const [name, expected] of Object.entries(EXPECTED.schemas)) {
    const schema = schemas[name];
    if (schema.path !== expected.path || schema.blob !== expected.blob || schema.sha256 !== expected.sha256) fail(`Workflow V2 schema authority schema mismatch: ${name}`);
  }
  return value;
};
const authority = readAuthority(authorityPath);
const bundlePath = join(root, authority.bundle.path);
if (sha256(readFileSync(bundlePath)) !== EXPECTED.bundleSha256) fail("Workflow V2 schema authority bundle digest mismatch");
const state = mkdtempSync(join(tmpdir(), "wv2-authority-"));
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
  if (commit !== EXPECTED.commit || tree !== EXPECTED.tree) fail("Workflow V2 schema authority commit/tree mismatch");
  for (const [name, expected] of Object.entries(EXPECTED.schemas)) {
    const blob = run(["rev-parse", `FETCH_HEAD:${expected.path}`]).trim();
    const bytes = run(["show", `FETCH_HEAD:${expected.path}`], null);
    if (blob !== expected.blob || sha256(bytes) !== expected.sha256) fail(`Workflow V2 schema authority Git object mismatch: ${name}`);
    for (const copy of [join(root, "scripts/fixtures", name), join(root, "prime-agent-runtime/schemas", name)]) {
      if (sha256(readFileSync(copy)) !== expected.sha256) fail(`Workflow V2 schema authority local copy mismatch: ${copy}`);
    }
  }
  // Exercise every authority binding plus closed-set and omission behavior.
  const mutate = (change) => { const mutant = structuredClone(authority); change(mutant); return mutant; };
  const mutants = [
    mutate((value) => { value.format = "prime.workflow-v2-schema-authority/v2"; }),
    mutate((value) => { value.repository = "other/repository"; }),
    mutate((value) => { value.commit = "0".repeat(40); }),
    mutate((value) => { value.tree = "0".repeat(40); }),
    mutate((value) => { value.bundle.path = "scripts/fixtures/other.bundle"; }),
    mutate((value) => { value.bundle.ref = "refs/authority/other"; }),
    mutate((value) => { value.bundle.sha256 = "0".repeat(64); }),
    mutate((value) => { value.schemas[0].name = "other.schema.json"; }),
    mutate((value) => { value.schemas[0].path = "docs/api/other.schema.json"; }),
    mutate((value) => { value.schemas[0].blob = "0".repeat(40); }),
    mutate((value) => { value.schemas[0].sha256 = "0".repeat(64); }),
    mutate((value) => { value.schemas = []; }),
    mutate((value) => { value.schemas.push(structuredClone(value.schemas[0])); }),
  ];
  for (const mutant of mutants) {
    const path = join(state, `mutant-${Math.random().toString(16).slice(2)}.json`);
    writeFileSync(path, JSON.stringify(mutant));
    let rejected = false;
    try { readAuthority(path); } catch { rejected = true; }
    if (!rejected) fail("Workflow V2 schema authority mutant was accepted");
  }
  console.log(JSON.stringify({ format: EXPECTED.format, repository: EXPECTED.repository, commit, tree, bundleSha256: EXPECTED.bundleSha256, schemas: EXPECTED.schemas, mutantsRejected: mutants.length }));
} finally { rmSync(state, { recursive: true, force: true }); }
