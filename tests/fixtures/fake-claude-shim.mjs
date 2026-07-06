#!/usr/bin/env node
// fake-claude-shim.mjs — fixture for the ClaudeAdapter tests (crewd Phase 2).
//
// Speaks the SAME NDJSON protocol as shim/claude-shim.mjs WITHOUT the Agent
// SDK, so the tests can run with no npm dependency and no network calls.
// Also exposes the --hang (accepted without final) and --fail (accepted then
// error) flags for the chaos suite (Task 15).
//
// Protocol (normative, Phase 2 Task 12 plan):
//   in  -> {"op":"turn","prompt":"...","resume_session":null|"<sid>"}
//          {"op":"abort"}
//          {"op":"exit"}
//   out -> {"ev":"ready","session_id":null}
//          {"ev":"accepted","engine_turn_id":"t-<n>"}
//          {"ev":"final","final_answer":"...","session_id":"<sid>"}
//          {"ev":"error","error":"..."}

import { createInterface } from "node:readline";

const argv = new Set(process.argv.slice(2));
const HANG = argv.has("--hang");
const reportCwd = process.argv.includes('--cwd');
const FAIL = argv.has("--fail");

const log = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

let turnCounter = 0;

function fakeTurn({ prompt, resume_session }) {
  turnCounter += 1;
  const engine_turn_id = `t-${turnCounter}`;
  log({ ev: "accepted", engine_turn_id });

  if (HANG) {
    // accepted without final: simulates a hung turn (for timeout/abort/chaos tests).
    return;
  }
  if (FAIL) {
    log({ ev: "error", error: "fail-flag" });
    return;
  }

  // final: reflects resume_session if present, otherwise a dummy session.
  const sessionId = resume_session || "fake-sess-1";
  const answer = process.argv.includes("--cwd") ? process.cwd() : `fake: ${prompt}`;
  log({ ev: "final", final_answer: answer, session_id: sessionId });
}

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
  switch (msg.op) {
    case "turn":
      fakeTurn(msg);
      break;
    case "abort":
      log({ ev: "error", error: "aborted" });
      break;
    case "exit":
      rl.close();
      process.exit(0);
      break;
    default:
      break;
  }
}

process.exit(0);
