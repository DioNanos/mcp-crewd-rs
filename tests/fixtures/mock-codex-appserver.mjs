#!/usr/bin/env node
// mock-codex-appserver.mjs — fixture per i contract test del CodexAdapter
// (crewd Fase 2 Task 11). Parla JSON-RPC 2.0 NDJSON su stdio SENZA il binario
// `codex` installato. Risponde a initialize / thread/start / turn/start /
// turn/interrupt / thread/resume con fixture deterministiche; emette la
// notification turn/completed dopo turn/start.
//
//   CODEX_MOCK_LOG=<path>   se impostato, appende una riga JSON {method,params}
//                           per ogni richiesta ricevuta (per la verify dei campi
//                           serializzati dal test).
//   CODEX_MOCK_POLICY=untrusted  fa rispondere thread/start e thread/resume con
//                           approvalPolicy:"untrusted" per il test fail-clear
//                           (l'adapter deve rifiutare con E_POLICY_DENIED).

import { createInterface } from "node:readline";
import { appendFileSync } from "node:fs";

const logPath = process.env.CODEX_MOCK_LOG;
const policy = process.env.CODEX_MOCK_POLICY === "untrusted" ? "untrusted" : "never";
// AUDIT2 M4 knobs (turn/start level):
//   CODEX_MOCK_TURN_ERROR=1        -> turn/start replies with a JSON-RPC error
//                                     object (adapter must fail clear, not Null).
//   CODEX_MOCK_TURN_POLICY=untrusted -> turn/start result echoes a downgraded
//                                     approvalPolicy (adapter must E_POLICY_DENIED).
const turnError = process.env.CODEX_MOCK_TURN_ERROR === "1";
const turnPolicy = process.env.CODEX_MOCK_TURN_POLICY === "untrusted" ? "untrusted" : null;

const log = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
const logReq = (method, params) => {
  if (logPath) appendFileSync(logPath, JSON.stringify({ method, params }) + "\n");
};

const threadStartResponse = {
  thread: {
    id: "th-1",
    sessionId: "s-1",
    forkedFromId: null,
    parentThreadId: null,
    preview: "",
    ephemeral: false,
    modelProvider: "x",
    createdAt: 0,
    updatedAt: 0,
    recencyAt: null,
    status: "active",
    path: null,
    cwd: "/tmp",
    cliVersion: "x",
    source: "x",
    threadSource: null,
    agentNickname: null,
    agentRole: null,
    gitInfo: null,
    name: null,
    turns: [],
  },
  model: "x",
  modelProvider: "x",
  serviceTier: null,
  cwd: "/tmp",
  instructionSources: [],
  approvalPolicy: policy,
  approvalsReviewer: null,
  sandbox: { type: "dangerFullAccess" },
  reasoningEffort: null,
};

const rl = createInterface({ input: process.stdin, terminal: false });
for await (const line of rl) {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    continue;
  }
  if (!req || req.id === undefined) continue;
  logReq(req.method, req.params);
  switch (req.method) {
    case "initialize":
      log({
        jsonrpc: "2.0",
        id: req.id,
        result: { serverInfo: { name: "mock" }, protocolVersion: "v2", capabilities: {} },
      });
      break;
    case "thread/start":
      log({ jsonrpc: "2.0", id: req.id, result: threadStartResponse });
      break;
    case "turn/start": {
      const prompt =
        (req.params && req.params.input && req.params.input[0] && req.params.input[0].text) || "";
      if (turnError) {
        // M4: a JSON-RPC error object on turn/start MUST fail clear.
        log({ jsonrpc: "2.0", id: req.id, error: { code: -32000, message: "turn refused" } });
        break;
      }
      const turnResult = { turn: { id: "t-1", items: [], status: "running" } };
      if (turnPolicy) {
        // M4: downgraded policy echoed on turn/start MUST be rejected.
        turnResult.approvalPolicy = turnPolicy;
        turnResult.sandbox = { type: "readOnly" };
      }
      log({ jsonrpc: "2.0", id: req.id, result: turnResult });
      if (turnPolicy) break; // adapter rejects; no completion follows
      // notification: turn/completed carries the final answer in turn.items.
      log({
        jsonrpc: "2.0",
        method: "turn/completed",
        params: {
          threadId: "th-1",
          turn: {
            id: "t-1",
            items: [{ type: "agentMessage", content: [{ type: "text", text: `done: ${prompt}` }] }],
            status: "completed",
          },
        },
      });
      break;
    }
    case "turn/interrupt":
      log({ jsonrpc: "2.0", id: req.id, result: {} });
      break;
    case "thread/resume":
      log({ jsonrpc: "2.0", id: req.id, result: threadStartResponse });
      break;
    default:
      log({ jsonrpc: "2.0", id: req.id, result: {} });
  }
}
process.exit(0);
