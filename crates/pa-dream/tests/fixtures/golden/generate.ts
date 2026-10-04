// The generator of the TS goldens in this directory (`tests/golden.rs` replays
// every scenario in Rust and compares results and file digests).
//
// To regenerate: copy `packages/coding-agent/src/core/dream/` (without llm.ts,
// experiment-llm.ts, run-service.ts), `src/core/ravo/canonical-json.ts` and
// `src/cli/dream-command.ts` from `perf/session-catalog-resume` into a scratch
// tree laid out as `dream/`, `ravo/`, `cli/` beside this file; rewrite the
// relative `.js` imports to `.ts`; stub `@earendil-works/pi-ai` (withSpan,
// startSpan, currentTraceContext, runWithTraceContext as no-ops) and
// `../config.js` (APP_NAME, expandTildePath, getAgentDir); then
//   esbuild generate.ts --bundle --platform=node --format=esm --outfile=gen.mjs
//   node gen.mjs <this directory>
// Node is required: its V8 computes Math.log/Math.cos with fdlibm, which
// `js_math.rs` reproduces; another engine would draw different gaussians.
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { createSeededRng } from "./dream/rng.ts";
import { DEFAULT_POLICY, PRIMING_DIVERSE } from "./dream/policy.ts";
import { runOnlineExploration } from "./dream/rollout.ts";
import { runDreamLoop } from "./dream/loop.ts";
import { runExperiment } from "./dream/experiment.ts";
import { resolveTask } from "./dream/tasks/index.ts";
import { runDreamCommand } from "./cli/dream-command.ts";

const OUT = process.argv[2]!;
mkdirSync(OUT, { recursive: true });
const CLOCK = 1_700_000_000_000;
const clock = () => CLOCK;

function manifest(dir: string): Record<string, string> {
  const out: Record<string, string> = {};
  const walk = (d: string) => {
    for (const name of readdirSync(d).sort()) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else out[relative(dir, p)] = createHash("sha256").update(readFileSync(p)).digest("hex");
    }
  };
  walk(dir);
  return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}
function fresh(): string { return mkdtempSync(join(tmpdir(), "dream-golden-")); }
function save(name: string, value: unknown) { writeFileSync(join(OUT, name), `${JSON.stringify(value, undefined, 2)}\n`); }

// A. rng
{
  const draws = (seed: number | string) => {
    const r = createSeededRng(seed);
    const f = r.fork("cand:3");
    return { next: [r.next(), r.next(), r.next()], gaussian: [r.nextGaussian(), r.nextGaussian()], int: [r.nextInt(81), r.nextInt(6)], fork: [f.next(), f.nextGaussian(), f.fork("retry:1").next()] };
  };
  save("rng.json", { seed7: draws(7), seedText: draws("abc"), seed0: draws(0) });
}
// A2. Math.log / Math.cos as V8 computes them, over the dream core's domain and fdlibm's branch edges.
{
  const inputs: number[] = [];
  let state = 0x2545f4914f6cdd1dn;
  const M = (1n << 64n) - 1n;
  for (let i = 0; i < 256; i++) {
    state ^= (state << 13n) & M; state ^= state >> 7n; state ^= (state << 17n) & M;
    inputs.push(Number(state >> 11n) / 2 ** 53);
  }
  const edges = [1, 2, 0.5, 1 + 2 ** -20, 1 - 2 ** -21, 2 ** -1074, 2 ** -1022, 1e-300, 1e300, 0.7853981633974483, 1.5707963267948966, 2.356194490192345, 3.141592653589793, 4.71238898038469, 6.283185307179586, 1e-9, 1e6, -2.5, -0.1];
  const logIn = [...inputs, ...edges.filter((x) => x > 0)];
  const cosIn = [...inputs.map((u) => 2 * Math.PI * u), ...edges];
  save("math.json", { log: logIn.map((x) => [x, Math.log(x)]), cos: cosIn.map((x) => [x, Math.cos(x)]) });
}
// B. rollout
{
  const dir = fresh();
  const r = runOnlineExploration({ task: resolveTask({ task: "circle-packing", n: 26 }), taskId: "circle-packing", n: 26, seed: 7, rng: createSeededRng(7), clock, workers: 4, k1: 12, dir, policy: DEFAULT_POLICY, iteration: 0 });
  save("rollout-circle-packing.json", { result: { treeId: r.treeId, rounds: r.rounds, revealedCount: r.revealedCount, bestScore: r.bestScore, bestNodeId: r.bestNodeId, probesToBest: r.probesToBest, improvements: r.improvements, rootScore: r.rootScore }, files: manifest(dir) });
  rmSync(dir, { recursive: true, force: true });
}
// C/D. loops
function loopGolden(name: string, task: "sum-difference" | "circle-packing" | "autocorrelation", n: number | undefined, seed: number, extra: Record<string, unknown>) {
  const dir = fresh();
  const result = runDreamLoop({ task: resolveTask({ task, ...(n !== undefined ? { n } : {}) }), taskId: task, ...(n !== undefined ? { n } : {}), seed, clock, dir, ...extra } as never);
  save(name, { result, files: manifest(dir) });
  rmSync(dir, { recursive: true, force: true });
}
loopGolden("loop-sum-difference.json", "sum-difference", undefined, 3, { workers: 3, k1: 6, k2: 12, dreams: 8, iterations: 3 });
loopGolden("loop-circle-packing-primed.json", "circle-packing", 8, 7, { workers: 3, k1: 6, k2: 12, dreams: 6, iterations: 2, primingPolicies: [...PRIMING_DIVERSE] });
loopGolden("loop-circle-packing-adopting.json", "circle-packing", 10, 8, { workers: 3, k1: 8, k2: 16, dreams: 8, iterations: 4 });
loopGolden("loop-default.json", "circle-packing", 26, 7, { workers: 4, k1: 12, k2: 24, dreams: 16, iterations: 3 });
loopGolden("loop-autocorrelation.json", "autocorrelation", 32, 11, { workers: 3, k1: 8, k2: 16, dreams: 6, iterations: 3 });
// E. experiment
{
  const dir = fresh();
  const result = runExperiment({ task: "autocorrelation", n: 32, seed: 7, rounds: 3, budget: { workers: 3, k1: 6, k2: 12, dreams: 6 }, arms: ["dream", "fixed"] }, { dir, clock });
  save("experiment-autocorrelation.json", { result, files: manifest(dir) });
  rmSync(dir, { recursive: true, force: true });
}
// F. CLI transcripts
{
  const transcripts: unknown[] = [];
  const base = fresh();
  const run = (args: string[]) => {
    const stdout: string[] = []; const stderr: string[] = [];
    const exit = runDreamCommand(args, { stdout: (l) => stdout.push(l), stderr: (l) => stderr.push(l), now: () => CLOCK });
    const scrub = (l: string) => l.split(base).join("<DIR>");
    transcripts.push({ args: args.map(scrub), exit, stdout: stdout.map(scrub), stderr: stderr.map(scrub) });
  };
  const d = join(base, "store");
  run(["status", "--dir", d]);
  run(["loop", "--task", "sum-difference", "--seed", "3", "--workers", "3", "--k1", "6", "--k2", "12", "--dreams", "8", "--iterations", "2", "--dir", d]);
  run(["status", "--dir", d]);
  run(["status", "--json", "--dir", d]);
  run(["show", "--dir", d]);
  run(["show", "--json", "--tree", "sum-difference-s3-i1-1700000000000", "--dir", d]);
  run(["replay", "--dir", d, "--k1", "6", "--k2", "12"]);
  run(["replay", "--json", "--dir", d, "--k1", "6", "--k2", "12"]);
  run(["improve", "--task", "sum-difference", "--dir", d, "--dreams", "5", "--k1", "6", "--k2", "12"]);
  run(["improve", "--json", "--task", "sum-difference", "--dir", d, "--dreams", "3", "--k1", "6", "--k2", "12"]);
  run(["improve", "--dir", d]);
  run(["loop", "--json", "--task", "circle-packing", "--n", "6", "--seed", "2", "--workers", "2", "--k1", "4", "--k2", "8", "--dreams", "4", "--iterations", "1", "--priming", "diverse", "--beta1", "0.1", "--dir", join(base, "loopjson")]);
  const e = join(base, "exp");
  run(["experiment", "--task", "autocorrelation", "--n", "32", "--rounds", "3", "--workers", "3", "--k1", "6", "--k2", "12", "--dreams", "6", "--seed", "7", "--dir", e]);
  run(["experiment", "--task", "autocorrelation", "--n", "32", "--rounds", "3", "--workers", "3", "--k1", "6", "--k2", "12", "--dreams", "6", "--seed", "7", "--dir", e]);
  run(["compare", "--json", "--overwrite", "--task", "sum-difference", "--rounds", "2", "--workers", "3", "--k1", "5", "--k2", "10", "--dreams", "4", "--seeds", "1,2", "--dir", e]);
  run(["experiment", "--task", "circle-packing", "--n", "5", "--rounds", "2", "--workers", "2", "--k1", "4", "--k2", "8", "--dreams", "3", "--arms", "dream", "--dir", e]);
  run(["status", "--dir", e]);
  run(["rollout", "--task", "circle-packing", "--seed", "5", "--dir", join(base, "r")]);
  run(["rollout", "--json", "--task", "autocorrelation", "--seed", "5", "--workers", "2", "--k1", "3", "--dir", join(base, "r")]);
  run(["show", "--dir", join(base, "r")]);
  run(["--task", "autocorrelation", "--n", "5"]);
  run(["--llm-proposer"]);
  run(["experiment", "--arms", "dream,dream-guided", "--dir", e]);
  run(["experiment", "--iterations", "2"]);
  run(["bogus"]);
  run(["--seed", "-1"]);
  run(["--beta3", "2"]);
  run(["--k1=0"]);
  run(["--arms", "dream,dream"]);
  run(["--seeds", "1,1"]);
  run(["loop", "replay"]);
  run(["--frobnicate"]);
  run(["--task"]);
  run(["replay", "--tree", "nope", "--dir", d]);
  save("cli.json", transcripts);
  rmSync(base, { recursive: true, force: true });
}
console.log("ok");
