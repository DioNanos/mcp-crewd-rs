#!/usr/bin/env node
// pi-replay.mjs — fixture per i test del PiAdapter (crewd Fase 2 Task 13).
//
// Rigioca una trace NDJSON registrata (tests/fixtures/pi-trace.ndjson) parlando
// il protocollo RPC pi: una richiesta (dir=req) ricevuta su stdin satura tutte
// le risposte (dir=res) fino al prossimo evento terminale (final/aborted/error)
// nella trace. Le righe `_comment` sono ignorate. Nessuna dipendenza npm, nessuna
// chiamata di rete: i test girano senza il binario `pi` installato.

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
  // consume una richiesta: emetti tutte le res fino al prossimo terminale
  while (idx < responses.length) {
    const r = responses[idx++];
    log(r);
    if (isTerminal(r)) break;
  }
}
process.exit(0);
