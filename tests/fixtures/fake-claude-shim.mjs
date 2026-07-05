#!/usr/bin/env node
// fake-claude-shim.mjs — fixture per i test del ClaudeAdapter (crewd Fase 2).
//
// Parla lo STESSO protocollo NDJSON di shim/claude-shim.mjs SENZA usare Agent SDK,
// cosi' i test possono girare senza dipendenza npm e senza chiamate di rete.
// Espone anche i flag --hang (accepted senza final) e --fail (accepted poi error)
// per la chaos suite (Task 15).
//
// Protocollo (normativo, piano Fase 2 Task 12):
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
    // accepted senza final: simula un turno appeso (per timeout/abort/chaos test).
    return;
  }
  if (FAIL) {
    log({ ev: "error", error: "fail-flag" });
    return;
  }

  // final: riflette resume_session se presente, altrimenti session fittizia.
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
