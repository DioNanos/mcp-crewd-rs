//! crewd-fake-engine — NDJSON binary for chaos tests (crewd Phase 2 Task 15-pre).
//!
//! Reproduces in Rust the same protocol as the Node claude shim
//! (`shim/claude-shim.mjs` + `tests/fixtures/fake-claude-shim.mjs`), so it can
//! be kill -9'd in the Phase 2 chaos tests (kill -9 matrix, crash consistency).
//!
//! NDJSON protocol (normative, Phase 2 Task 12 plan):
//!
//!   in  -> {"op":"turn","prompt":"...","resume_session":null|"<sid>"}
//!          {"op":"abort"}
//!          {"op":"exit"}
//!
//!   out -> {"ev":"ready","session_id":null}
//!          {"ev":"accepted","engine_turn_id":"t-<n>"}
//!          {"ev":"final","final_answer":"...","session_id":"<sid>"}
//!          {"ev":"error","error":"..."}
//!
//! CLI flags:
//!   --hang  after accepted it does NOT emit final: it stays hung reading
//!           stdin, so a `kill -9` terminates it and parent death (stdin
//!           closed → EOF) makes it exit cleanly without leaving orphans
//!           (kill/timeout tests)
//!   --fail  after accepted it emits {"ev":"error","error":"engine-failure"}
//!
//! Only std + serde_json (already `crewd` dependencies). Every stdout line is
//! flushed immediately because tests read via pipe (explicit line-buffering).

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use serde_json::{json, Value};

/// Prints one NDJSON line to stdout and flushes immediately (explicit line-buffering).
fn emit(obj: Value) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{obj}");
    let _ = lock.flush();
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let hang = args.iter().any(|a| a.as_str() == "--hang");
    let fail = args.iter().any(|a| a.as_str() == "--fail");

    let mut turn_counter: u64 = 0;

    // Initial ready (before reading any op).
    emit(json!({ "ev": "ready", "session_id": null }));

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON line: ignored (protocol robust to noise)
        };
        let op = msg.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "turn" => {
                turn_counter += 1;
                let engine_turn_id = format!("t-{turn_counter}");
                emit(json!({ "ev": "accepted", "engine_turn_id": engine_turn_id }));

                if hang {
                    // Accepted but no final: stays hung. It does NOT sleep in a
                    // blind loop (that would leave orphans on parent death):
                    // it keeps reading stdin, so EOF (dead parent) makes it
                    // exit, while `kill -9` terminates it anyway.
                    continue;
                }
                if fail {
                    emit(json!({ "ev": "error", "error": "engine-failure" }));
                    continue;
                }

                let prompt = msg.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                // resume_session reflected in the final (null/absent => fake-sess-1).
                let session_id = msg
                    .get("resume_session")
                    .and_then(|v| v.as_str())
                    .unwrap_or("fake-sess-1");
                emit(json!({
                    "ev": "final",
                    "final_answer": format!("fake: {prompt}"),
                    "session_id": session_id,
                }));
            }
            "abort" => {
                emit(json!({ "ev": "error", "error": "aborted" }));
            }
            "exit" => {
                return ExitCode::SUCCESS;
            }
            _ => {
                // unknown op: ignored (forward-compat).
            }
        }
    }

    // stdin closed without op:exit: clean exit.
    ExitCode::SUCCESS
}
