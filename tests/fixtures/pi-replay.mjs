#!/usr/bin/env node
// pi-replay.mjs — fixture for the PiAdapter tests.
//
// Replays a recorded NDJSON trace (tests/fixtures/pi-trace.ndjson) speaking the
// REAL pi `--mode rpc` wire protocol (LF-only JSONL). Each client command read
// on stdin drains the trace's pi->client lines up to and including the next
// terminal one. `_comment` lines are ignored. No npm deps, no network: the
// tests run without the `pi` binary installed.
//
// Terminal line = `agent_end` (unless willRetry:true) OR a `prompt` response
// with success:false (a preflight rejection ends the turn immediately).

import { readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const tracePath = process.argv[2];
if (!tracePath) {
  process.stderr.write("pi-replay: missing trace path (argv[2])\n");
  process.exit(2);
}

const lines = [];
for (const line of readFileSync(tracePath, "utf8").split("\n")) {
  if (!line.trim()) continue;
  let obj;
  try {
    obj = JSON.parse(line);
  } catch {
    continue;
  }
  if (!obj || obj._comment) continue;
  lines.push(obj);
}

const out = (o) => process.stdout.write(JSON.stringify(o) + "\n");

function isTerminal(o) {
  if (o.type === "agent_end") return o.willRetry !== true;
  if (o.type === "response" && o.command === "prompt" && o.success === false) {
    return true;
  }
  return false;
}

let idx = 0;
const rl = createInterface({ input: process.stdin, terminal: false });
for await (const _cmd of rl) {
  // one client command -> emit pi->client lines up to the next terminal one
  while (idx < lines.length) {
    const o = lines[idx++];
    out(o);
    if (isTerminal(o)) break;
  }
}
process.exit(0);
