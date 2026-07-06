#!/usr/bin/env node
// pi-replay.mjs — fixture for the PiAdapter tests (crewd Phase 2 Task 13).
//
// Replays a recorded NDJSON trace (tests/fixtures/pi-trace.ndjson) speaking
// the pi RPC protocol: one request (dir=req) received on stdin drains all
// responses (dir=res) up to the next terminal event (final/aborted/error) in
// the trace. `_comment` lines are ignored. No npm dependencies, no network
// calls: the tests run without the `pi` binary installed.

import { readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const tracePath = process.argv[2];
if (!tracePath) {
  process.stderr.write("pi-replay: missing trace path (argv[2])\n");
  process.exit(2);
}

const responses = [];
for (const line of readFileSync(tracePath, "utf8").split("\n")) {
  if (!line.trim()) continue;
  let obj;
  try {
    obj = JSON.parse(line);
  } catch {
    continue;
  }
  if (!obj || obj._comment) continue;
  if (obj.dir === "res") responses.push(obj);
}

const log = (o) => process.stdout.write(JSON.stringify(o) + "\n");

function isTerminal(r) {
  if (r.error) return true;
  const s = r.result && r.result.status;
  return s === "final" || s === "aborted";
}

let idx = 0;
const rl = createInterface({ input: process.stdin, terminal: false });
for await (const line of rl) {
  // consume one request: emit all res up to the next terminal one
  while (idx < responses.length) {
    const r = responses[idx++];
    log(r);
    if (isTerminal(r)) break;
  }
}
process.exit(0);
