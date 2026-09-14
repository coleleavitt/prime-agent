#!/usr/bin/env node
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { canonicalJson, normalizeEvidence, sha256 } from "./lib/workflow-v1-evidence.mjs";

const root = resolve(new URL("..", import.meta.url).pathname);
const args = process.argv.slice(2);
const outputAt = args.indexOf("--output");
const output = outputAt >= 0 && args[outputAt + 1] ? resolve(args[outputAt + 1]) : null;
if (!output) {
  console.error("usage: node scripts/verify-python-runtime-isolated.mjs --output <empty-directory>");
  process.exit(2);
}
mkdirSync(output, { recursive: true });
if (readdirSync(output).length !== 0) {
  console.error("--output directory must be empty");
  process.exit(2);
}
const state = mkdtempSync(join(tmpdir(), "prime-python-isolated-"));
const testDir = join(root, "prime-agent-runtime", "test");
const timeoutMs = 120_000;
const allowedEnvironmentNames = [
  "PATH", "LANG", "LC_ALL", "LC_CTYPE", "SHELL", "TERM", "USER", "LOGNAME",
  "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
];
const baseEnv = Object.fromEntries(
  allowedEnvironmentNames
    .filter((name) => process.env[name] !== undefined)
    .map((name) => [name, process.env[name]]),
);
const makeEnv = (id) => {
  const home = join(state, id);
  const paths = {
    HOME: home,
    TMPDIR: join(home, "tmp"),
    XDG_CACHE_HOME: join(home, "cache"),
    XDG_CONFIG_HOME: join(home, "config"),
    XDG_DATA_HOME: join(home, "data"),
    XDG_STATE_HOME: join(home, "state"),
  };
  for (const path of Object.values(paths)) mkdirSync(path, { recursive: true });
  return { ...baseEnv, ...paths, CI: "1", TZ: "UTC", NO_COLOR: "1", FORCE_COLOR: "0" };
};
const run = (id, commandArgs) => spawnSync("uv", commandArgs, {
  cwd: root,
  env: makeEnv(id),
  encoding: "utf8",
  timeout: timeoutMs,
  killSignal: "SIGKILL",
  maxBuffer: 128 * 1024 * 1024,
});
const execute = (id, commandArgs) => {
  const result = run(id, commandArgs);
  const log = normalizeEvidence(`${result.stdout ?? ""}${result.stderr ?? ""}`, [[root, "<CHECKOUT>"], [state, "<STATE>"]]);
  const logFile = `${id}.log`;
  writeFileSync(join(output, logFile), log);
  return {
    id,
    command: ["uv", ...commandArgs],
    exitCode: result.status ?? 255,
    signal: result.signal ?? null,
    timedOut: result.error?.code === "ETIMEDOUT",
    timeoutMs,
    log: logFile,
    normalizedLogSha256: sha256(log),
  };
};
const discoverCode = String.raw`
import ast, json, pathlib, sys, unittest
path = pathlib.Path("prime-agent-runtime/test").resolve()
sys.path.insert(0, str(path))

def flatten(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from flatten(item)
        else:
            yield item.id()

all_ids = sorted(flatten(unittest.defaultTestLoader.discover(str(path), pattern="test_*.py", top_level_dir=str(path))))
non_bash_ids = [test_id for test_id in all_ids if not test_id.startswith("test_bash.")]
runtime_bash_ids = [test_id for test_id in all_ids if test_id.startswith("test_bash.")]
source = path.joinpath("test_bash.py").read_text(encoding="utf-8")
tree = ast.parse(source, filename=str(path.joinpath("test_bash.py")))

def base_name(node):
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        return base_name(node.value) + "." + node.attr
    return ""

bash_ids = []
for node in tree.body:
    if not isinstance(node, ast.ClassDef):
        continue
    bases = {base_name(base).rsplit(".", 1)[-1] for base in node.bases}
    if not bases.intersection({"TestCase", "IsolatedAsyncioTestCase"}):
        continue
    for member in node.body:
        if isinstance(member, (ast.FunctionDef, ast.AsyncFunctionDef)) and member.name.startswith("test"):
            bash_ids.append(f"test_bash.{node.name}.{member.name}")
bash_ids.sort()
if bash_ids != runtime_bash_ids:
    raise SystemExit("AST test_bash inventory differs from unittest discovery: " + json.dumps({"ast": bash_ids, "runtime": runtime_bash_ids}, sort_keys=True))
print(json.dumps({"nonBashIds": non_bash_ids, "bashIds": bash_ids}, separators=(",", ":"), sort_keys=True))
`;
const discovery = run("discovery", ["run", "--project", "prime-agent-runtime", "python", "-c", discoverCode]);
if (discovery.status !== 0) {
  console.error(`${discovery.stdout ?? ""}${discovery.stderr ?? ""}` || "runtime test discovery failed");
  rmSync(state, { recursive: true, force: true });
  process.exit(1);
}
let inventory;
try {
  inventory = JSON.parse(discovery.stdout.trim());
} catch {
  rmSync(state, { recursive: true, force: true });
  throw new Error("runtime test discovery did not emit valid JSON");
}
const validIds = (ids, prefixRequired, label) => {
  if (!Array.isArray(ids) || ids.length === 0 || new Set(ids).size !== ids.length || ids.some((id) => typeof id !== "string" || (prefixRequired && !id.startsWith(prefixRequired)))) {
    throw new Error(`${label} discovery returned an invalid, empty, or duplicate test list`);
  }
};
const nonBashIds = inventory.nonBashIds;
const bashIds = inventory.bashIds;
validIds(nonBashIds, null, "non-test_bash");
validIds(bashIds, "test_bash.", "test_bash AST");
if (nonBashIds.some((id) => id.startsWith("test_bash."))) throw new Error("non-test_bash inventory contains test_bash ID");
const nonBashFiles = readdirSync(testDir).filter((name) => /^test_.*\.py$/.test(name) && name !== "test_bash.py").sort();
if (nonBashFiles.length === 0) throw new Error("no non-test_bash runtime test files discovered");
const aggregateCode = String.raw`
import pathlib, sys, unittest
path = pathlib.Path("prime-agent-runtime/test").resolve()
sys.path.insert(0, str(path))
loader = unittest.defaultTestLoader
suite = unittest.TestSuite()
for filename in sys.argv[1:]:
    suite.addTests(loader.discover(str(path), pattern=filename, top_level_dir=str(path)))
result = unittest.TextTestRunner(verbosity=1).run(suite)
raise SystemExit(0 if result.wasSuccessful() else 1)
`;
const singleCode = String.raw`
import pathlib, sys, unittest
path = pathlib.Path("prime-agent-runtime/test").resolve()
sys.path.insert(0, str(path))
suite = unittest.defaultTestLoader.loadTestsFromName(sys.argv[1])
result = unittest.TextTestRunner(verbosity=1).run(suite)
raise SystemExit(0 if result.wasSuccessful() else 1)
`;
const results = [];
results.push(execute("non-bash-runtime", ["run", "--project", "prime-agent-runtime", "python", "-c", aggregateCode, ...nonBashFiles]));
for (let index = 0; index < bashIds.length; index += 1) {
  results.push(execute(`bash-${String(index + 1).padStart(3, "0")}`, [
    "run", "--project", "prime-agent-runtime", "python", "-c", singleCode, bashIds[index],
  ]));
}
const passed = results.filter((result) => result.exitCode === 0 && !result.timedOut).length;
const manifest = {
  format: "prime.python-runtime.isolated-acceptance/v1",
  policy: {
    retries: 0,
    allowlist: [],
    forbiddenEnvironmentPrefix: "PRIME_AGENT_",
    isolation: "all non-test_bash tests run normally together; every AST-listed test_bash unittest method runs exactly once in its own fresh process and unique HOME/XDG/TMP",
    timeoutMs,
  },
  inventory: {
    nonBashFiles,
    nonBashFileCount: nonBashFiles.length,
    nonBashTestCount: nonBashIds.length,
    nonBashIdsSha256: sha256(`${nonBashIds.join("\n")}\n`),
    nonBashIds,
    testBashSourceSha256: sha256(readFileSync(join(testDir, "test_bash.py"))),
    testBashMethodCount: bashIds.length,
    testBashIdsSha256: sha256(`${bashIds.join("\n")}\n`),
    testBashIds: bashIds,
    processCount: results.length,
  },
  counts: {
    processes: { passed, failed: results.length - passed, total: results.length },
    tests: { nonBash: nonBashIds.length, testBash: bashIds.length, total: nonBashIds.length + bashIds.length },
  },
  verdict: passed === results.length ? "PASS" : "FAIL",
  results,
};
const evidence = canonicalJson(manifest);
writeFileSync(join(output, "python-isolated.json"), evidence);
writeFileSync(join(output, "python-isolated.json.sha256"), `${sha256(evidence)}  python-isolated.json\n`);
writeFileSync(join(output, "logs.sha256"), `${results.map((result) => `${result.normalizedLogSha256}  ${result.log}`).join("\n")}\n`);
rmSync(state, { recursive: true, force: true });
console.log(`python isolated acceptance: ${manifest.verdict} (${passed}/${results.length} processes; ${nonBashIds.length} non-test_bash tests; ${bashIds.length} test_bash methods)`);
process.exit(manifest.verdict === "PASS" ? 0 : 1);
