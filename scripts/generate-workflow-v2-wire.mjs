#!/usr/bin/env node
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { dirname, join, relative } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const schemaPath = join(root, "scripts/fixtures/workflow-v2.schema.json");
const wirePath = join(root, "packages/coding-agent/src/core/workflow-v2-wire.ts");
const begin = "// BEGIN GENERATED WORKFLOW V2 DEFS — scripts/generate-workflow-v2-wire.mjs";
const end = "// END GENERATED WORKFLOW V2 DEFS";
const source = readFileSync(schemaPath);
const schema = JSON.parse(source.toString("utf8"));
if (!schema || typeof schema !== "object" || !schema.$defs || typeof schema.$defs !== "object" || Array.isArray(schema.$defs)) {
  throw new Error(`${relative(root, schemaPath)} does not contain an object $defs`);
}
const hash = createHash("sha256").update(source).digest("hex");
const generated = `${begin}
// Source: scripts/fixtures/workflow-v2.schema.json sha256:${hash}
const defs = ${JSON.stringify(schema.$defs, null, "\t")} as const;
${end}`;
const current = readFileSync(wirePath, "utf8");
const start = current.indexOf(begin);
const finish = current.indexOf(end);
if (start < 0 || finish < start || current.indexOf(begin, start + begin.length) >= 0 || current.indexOf(end, finish + end.length) >= 0) {
  throw new Error(`${relative(root, wirePath)} must contain exactly one ordered generated marker pair`);
}
const unformatted = current.slice(0, start) + generated + current.slice(finish + end.length);
const formatter = spawnSync(join(root, "node_modules/.bin/biome"), ["format", "--stdin-file-path", wirePath], {
  cwd: root,
  input: unformatted,
  encoding: "utf8",
});
if (formatter.status !== 0) throw new Error(formatter.stderr || "Biome failed to format generated wire source");
const next = formatter.stdout;
if (process.argv.includes("--check")) {
  if (next !== current) {
    console.error(`${relative(root, wirePath)} generated definitions are stale`);
    process.exitCode = 1;
  }
} else if (next !== current) {
  writeFileSync(wirePath, next);
}
