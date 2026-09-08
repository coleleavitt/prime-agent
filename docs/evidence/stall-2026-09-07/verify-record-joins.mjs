import fs from "node:fs";
import assert from "node:assert/strict";
const here = new URL("./", import.meta.url);
const readLines = (name) => fs.readFileSync(new URL(name, here), "utf8").trim().split("\n").map(JSON.parse);
const sessions = readLines("session-records.jsonl");
const traces = readLines("trace-records.jsonl");
const orphans = readLines("orphan-journal-records.jsonl");
const callId = "call_0e26efe63a1c53178b2b0cbf6abb5465";
const traceId = "416be8b73b78b1265c020008db687e70";
const assistant = sessions.find((entry) => JSON.stringify(entry).includes(callId) && entry.message?.role === "assistant");
const result = sessions.find((entry) => entry.message?.toolCallId === callId);
assert(assistant, "assistant tool call is persisted");
assert(result, "tool result is persisted");
assert.equal(assistant.traceId, traceId);
assert.equal(result.traceId, traceId);
const tool = traces.find((entry) => entry.name === "tool.execute" && entry.attrs?.["tool.call_id"] === callId);
assert(tool, "tool.execute join exists");
const kernel = traces.find((entry) => entry.name === "kernel.execute" && entry.parentSpanId === tool.spanId);
assert(kernel, "kernel.execute child exists");
const cell = traces.find((entry) => entry.name === "kernel.cell" && entry.parentSpanId === kernel.spanId);
assert(cell, "kernel.cell child exists");
const bash = traces.find((entry) => entry.name === "bash.command" && entry.parentSpanId === cell.spanId);
assert(bash, "bash.command child exists");
assert.equal(bash.attrs?.["bash.pid"], 1022336);
assert(orphans.some((entry) => entry.pid === 1022336 && entry.active === true), "PID enrollment exists");
assert(orphans.some((entry) => entry.pid === 1022336 && entry.active === false), "PID cleanup exists");
const raw = [sessions, traces, orphans].flat().map(JSON.stringify).join("\n");
assert(!raw.includes('"traceparent"'), "raw records did not persist traceparent");
assert(!raw.includes('"traceFlags"'), "raw records did not persist trace flags");
assert(!raw.includes('"pgid"'), "original raw records did not persist PGID");
console.log(JSON.stringify({
  status: "pass",
  childSessionId: bash.sessionId,
  toolCallId: callId,
  traceId,
  spanChain: [tool.spanId, kernel.spanId, cell.spanId, bash.spanId],
  pid: bash.attrs["bash.pid"],
  negativeAssertions: ["traceparent", "traceFlags", "pgid"],
}, null, 2));
