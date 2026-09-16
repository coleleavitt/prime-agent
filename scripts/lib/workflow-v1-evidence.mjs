import { createHash } from "node:crypto";

export const sha256 = (value) => createHash("sha256").update(value).digest("hex");

export function normalizeEvidence(value, replacements = []) {
  let normalized = value.replaceAll("\r\n", "\n").replace(/\x1b\[[0-?]*[ -\/]*[@-~]/g, "");
  for (const [actual, marker] of replacements) normalized = normalized.replaceAll(actual, marker);
  return normalized
    .replace(/\b(?:Duration|duration|Time|time):?\s+\d+(?:\.\d+)?(?:ms|s)\b/g, "duration: <ELAPSED>")
    .replace(/\bStart at\s+\d{2}:\d{2}:\d{2}\b/g, "Start at <TIME>")
    .replace(/\bin \d+(?:\.\d+)?s\b/g, "in <ELAPSED>")
    .replace(/\(\d+(?:\.\d+)?m?s\)/g, "(<ELAPSED>)")
    .replace(/[ \t]+$/gm, "")
    .replace(/\n+$/g, "\n");
}

export function canonicalJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}
