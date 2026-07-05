#!/usr/bin/env node
// claude-shim.mjs — crewd cell fabric engine-claude shim.
//
// Protocollo NDJSON (normativo: piano Fase 2 Task 12 + SPEC.md §20.6-20.7):
//
//   crewd -> shim (stdin, una riga JSON per messaggio):
//     {"op":"turn","prompt":"...","resume_session":null|"<sid>"}
//     {"op":"abort"}
//     {"op":"exit"}
//
//   shim -> crewd (stdout, una riga JSON per messaggio):
//     {"ev":"ready","session_id":null}
//     {"ev":"accepted","engine_turn_id":"t-<n>"}
//     {"ev":"note","text":"..."}
//     {"ev":"final","final_answer":"...","session_id":"<sid>"}
//     {"ev":"error","error":"..."}
//
// Hardening:
//   - MAI stampare env o segreti su stdout/stderr; gli errori riportano al piu'
//     un prefix <=8 char del messaggio (Global Constraints / SPEC §20.7).
//   - L'env allowlist ESATTA e' applicata dal ClaudeAdapter Rust prima di
//     spawnare il processo; questo shim eredita solo cio' che il parent passa.
//   - Se il modulo @anthropic-ai/claude-agent-sdk manca, emette
//     {"ev":"error","error":"sdk-missing"} ed esce con codice 3 (degrado pulito).

import { createInterface } from "node:readline";

const log = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

// Riduce un errore a forma sicura (no segreti, prefix <=8 char, no newline).
function safeErr(e) {
  const raw = e && e.message ? String(e.message) : String(e);
  const prefix = raw.slice(0, 8).replace(/[\r\n]+/g, " ");
  return { error: prefix };
}

// Import dell'SDK in try/catch: se il modulo manca, degrado pulito (exit 3).
let queryFn = null;
try {
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  queryFn = sdk.query;
} catch {
  log({ ev: "error", error: "sdk-missing" });
  process.exit(3);
}
if (typeof queryFn !== "function") {
  log({ ev: "error", error: "sdk-missing" });
  process.exit(3);
}

// engine_turn_id monotonic ("t-<n>"); AbortController del turno in corso.
let turnCounter = 0;
let currentAbort = null;

function queryOptions(resumeSession) {
  const opts = {
    permissionMode: "bypassPermissions",
    allowDangerouslySkipPermissions: true,
    cwd: process.cwd(),
  };
  // Model override solo se l'env allowlist lo fornisce (ANTHROPIC_MODEL).
  if (process.env.ANTHROPIC_MODEL) opts.model = process.env.ANTHROPIC_MODEL;
  // Resume onesto (SPEC §20.6): valorizzato => follow-up su storia materializzata.
  if (resumeSession) opts.resume = resumeSession;
  return opts;
}

async function runTurn({ prompt, resume_session }) {
  turnCounter += 1;
  const engine_turn_id = `t-${turnCounter}`;
  log({ ev: "accepted", engine_turn_id });

  const ac = new AbortController();
  currentAbort = ac;

  let finalAnswer = "";
  let sessionId = null;
  try {
    const stream = queryFn({
      prompt,
      options: { ...queryOptions(resume_session), abortController: ac },
    });
    for await (const message of stream) {
      if (!message || typeof message !== "object") continue;
      if (message.type === "assistant" && Array.isArray(message.message?.content)) {
        for (const block of message.message.content) {
          if (block && typeof block === "object" && "text" in block && block.text) {
            // Progresso significativo: testo assistente (non ogni delta).
            log({ ev: "note", text: block.text });
          }
        }
      } else if (message.type === "result") {
        if (typeof message.result === "string") finalAnswer = message.result;
        if (typeof message.session_id === "string") sessionId = message.session_id;
      }
    }
    log({ ev: "final", final_answer: finalAnswer, session_id: sessionId });
  } catch (e) {
    if (ac.signal.aborted) {
      log({ ev: "error", error: "aborted" });
    } else {
      log({ ev: "error", ...safeErr(e) });
    }
  } finally {
    currentAbort = null;
  }
}

function parseLine(line) {
  try {
    const msg = JSON.parse(line);
    return msg && typeof msg === "object" ? msg : null;
  } catch {
    // Riga non JSON: ignorata (protocollo robusto a rumore di riga).
    return null;
  }
}

// --- main loop ---------------------------------------------------------------
log({ ev: "ready", session_id: null });

const rl = createInterface({ input: process.stdin, terminal: false });

// AUDIT2 M5: the turn runs WITHOUT awaiting inside the stdin loop, so an
// `abort` arriving mid-turn is processed immediately (it aborts the in-flight
// AbortController) instead of being queued behind a blocking `await runTurn`.
let running = null;

for await (const line of rl) {
  const msg = parseLine(line);
  if (!msg) continue;
  switch (msg.op) {
    case "turn":
      // One turn at a time (the adapter is head-of-line): if a prior turn is
      // still running, wait for it before starting the next — but the loop
      // itself never blocks on the CURRENT turn, so abort stays live.
      running = (running ? running.catch(() => {}) : Promise.resolve())
        .then(() => runTurn(msg))
        .finally(() => {
          running = null;
        });
      break;
    case "abort":
      if (currentAbort) currentAbort.abort();
      break;
    case "exit":
      if (currentAbort) currentAbort.abort();
      rl.close();
      process.exit(0);
      break;
    default:
      // op sconosciuta: ignorata (forward-compat).
      break;
  }
}

// stdin chiuso senza op:exit: attende l'eventuale turno in corso e poi esce.
if (running) {
  try {
    await running;
  } catch {
    // ignora: l'esito e' gia' stato loggato da runTurn.
  }
}
process.exit(0);
