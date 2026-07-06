#!/usr/bin/env node
// env-dump.mjs — fixture for the ClaudeAdapter env-leak test (Task 12 Step 1b).
//
// Speaks the minimal NDJSON protocol the adapter requires (ready + accepted +
// final) and in final_answer reports ONLY the NAMES of the env vars present
// (sorted, comma-separated). NEVER the values. The test verifies that the set
// of names is a subset of the env allowlist (SPEC §20.7) and that the token
// does not appear in the logs.

import { createInterface } from "node:readline";

const log = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

log({ ev: "ready", session_id: null });

const rl = createInterface({ input: process.stdin, terminal: false });

for await (const line of rl) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    continue;
  }
  if (!msg || typeof msg !== "object") continue;
  if (msg.op === "turn") {
    log({ ev: "accepted", engine_turn_id: "t-1" });
    const names = Object.keys(process.env).sort().join(",");
    log({ ev: "final", final_answer: names, session_id: null });
    continue;
  }
  if (msg.op === "exit") {
    process.exit(0);
  }
  // abort / unknown: ignored.
}

process.exit(0);
