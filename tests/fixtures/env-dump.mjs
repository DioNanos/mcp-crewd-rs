#!/usr/bin/env node
// env-dump.mjs — fixture per l'env-leak test del ClaudeAdapter (Task 12 Step 1b).
//
// Parla il protocollo NDJSON minimo richiesto dall'adapter (ready + accepted +
// final) e nel final_answer riporta SOLO i NOMI delle env var presenti (ordinate,
// separate da virgola). MAI i valori. Il test verifica che l'insieme dei nomi sia
// subset dell'env allowlist (SPEC §20.7) e che il token non compaia nei log.

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
  // abort / unknown: ignorati.
}

process.exit(0);
