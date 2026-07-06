#!/usr/bin/env node
// claude-shim.mjs — crewd cell fabric engine-claude shim.
//
// NDJSON protocol (normative: Phase 2 Task 12 plan + SPEC.md §20.6-20.7):
//
//   crewd -> shim (stdin, one JSON line per message):
//     {"op":"turn","prompt":"...","resume_session":null|"<sid>"}
//     {"op":"abort"}
//     {"op":"exit"}
//
//   shim -> crewd (stdout, one JSON line per message):
//     {"ev":"ready","session_id":null}
//     {"ev":"accepted","engine_turn_id":"t-<n>"}
//     {"ev":"note","text":"..."}
//     {"ev":"final","final_answer":"...","session_id":"<sid>"}
//     {"ev":"error","error":"..."}
//
// Hardening:
//   - NEVER print env or secrets on stdout/stderr; errors report at most an
//     <=8 char prefix of the message (Global Constraints / SPEC §20.7).
//   - The EXACT env allowlist is enforced by the Rust ClaudeAdapter before
//     spawning the process; this shim only inherits what the parent passes.
//   - If the @anthropic-ai/claude-agent-sdk module is missing, it emits
//     {"ev":"error","error":"sdk-missing"} and exits with code 3 (clean degrade).

import { createInterface } from "node:readline";

const log = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

// Reduces an error to a safe shape (no secrets, <=8 char prefix, no newline).
function safeErr(e) {
  const raw = e && e.message ? String(e.message) : String(e);
  const prefix = raw.slice(0, 8).replace(/[\r\n]+/g, " ");
  return { error: prefix };
}

// SDK import in try/catch: if the module is missing, clean degrade (exit 3).
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

// Monotonic engine_turn_id ("t-<n>"); AbortController of the in-flight turn.
let turnCounter = 0;
let currentAbort = null;

function queryOptions(resumeSession) {
  const opts = {
    permissionMode: "bypassPermissions",
    allowDangerouslySkipPermissions: true,
    cwd: process.cwd(),
  };
  // Model override only if the env allowlist provides it (ANTHROPIC_MODEL).
  if (process.env.ANTHROPIC_MODEL) opts.model = process.env.ANTHROPIC_MODEL;
  // Honest resume (SPEC §20.6): set => follow-up on materialized history.
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
            // Meaningful progress: assistant text (not every delta).
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
    // Non-JSON line: ignored (protocol robust to line noise).
    return null;
  }
}

// --- main loop ---------------------------------------------------------------
log({ ev: "ready", session_id: null });

const rl = createInterface({ input: process.stdin, terminal: false });

// the turn runs WITHOUT awaiting inside the stdin loop, so an
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
      // unknown op: ignored (forward-compat).
      break;
  }
}

// stdin closed without op:exit: waits for any in-flight turn, then exits.
if (running) {
  try {
    await running;
  } catch {
    // ignore: the outcome was already logged by runTurn.
  }
}
process.exit(0);
