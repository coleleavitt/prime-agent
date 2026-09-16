#!/usr/bin/env node
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const generator = join(root, "scripts/generate-workflow-v2-wire.mjs");
const wire = join(root, "packages/coding-agent/src/core/workflow-v2-wire.ts");
const schema = join(root, "scripts/fixtures/workflow-v2.schema.json");
const originalWire = readFileSync(wire, "utf8");
const originalSchema = readFileSync(schema, "utf8");
try {
  execFileSync(process.execPath, [generator, "--check"], { cwd: root });
  const handwritten = originalWire.slice(originalWire.indexOf("// END GENERATED WORKFLOW V2 DEFS") + "// END GENERATED WORKFLOW V2 DEFS".length);
  writeFileSync(wire, originalWire.replace('pattern: "^[A-Za-z0-9]', 'pattern: "^MUTATED[A-Za-z0-9]'));
  assert.notEqual(spawnSync(process.execPath, [generator, "--check"], { cwd: root }).status, 0);
  execFileSync(process.execPath, [generator], { cwd: root });
  assert.equal(readFileSync(wire, "utf8").slice(readFileSync(wire, "utf8").indexOf("// END GENERATED WORKFLOW V2 DEFS") + "// END GENERATED WORKFLOW V2 DEFS".length), handwritten);
  const parsed = JSON.parse(originalSchema); parsed.$defs.id.pattern = "^schema-mutation$";
  writeFileSync(schema, JSON.stringify(parsed, null, 2) + "\n");
  assert.notEqual(spawnSync(process.execPath, [generator, "--check"], { cwd: root }).status, 0);
} finally {
  writeFileSync(schema, originalSchema);
  writeFileSync(wire, originalWire);
}
console.log("workflow-v2 wire generator mutation checks passed");
