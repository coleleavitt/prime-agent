/**
 * Public surface of the Dream-RSI subsystem.
 *
 * The CLI and any in-session caller import from this barrel. It deliberately does
 * NOT re-export `llm.ts` (the flag-gated LLM proposer/dreamer): the sync,
 * zero-token path is reachable through this module without pulling the child-agent
 * call path, so a standalone `import` here can never spend a token or open a
 * socket. The LLM seam is reached only by callers that import `./llm.js` directly
 * and hold a `RunAgentHandler`.
 *
 * `./llm.js` now exists but stays omitted from this barrel BY DESIGN — adding it
 * here would leak the token-spending child-agent path into every plain import.
 * `./experiment.js` (the fixed-exploration control and its runner core) IS here:
 * it drives arms through `runDreamLoop` or an injected `ExperimentArmRunner` and
 * never imports `./llm.js`; the LLM arm runner is an in-session caller's to build.
 */

export * from "./experiment.js";
export * from "./improve.js";
export * from "./interpreter.js";
export * from "./loop.js";
export * from "./objective.js";
export * from "./observation.js";
export * from "./policy.js";
export * from "./proposer.js";
export * from "./replay.js";
export * from "./rng.js";
export * from "./rollout.js";
export * from "./store.js";
export * from "./task.js";
export * from "./tasks/index.js";
export * from "./tree.js";
export * from "./types.js";
