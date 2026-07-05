//! crewd-fake-engine — binario NDJSON per chaos test (crewd Fase 2 Task 15-pre).
//!
//! Riproduce in Rust lo stesso protocollo del Node shim claude
//! (`shim/claude-shim.mjs` + `tests/fixtures/fake-claude-shim.mjs`), per poter
//! essere kill -9 nei chaos test di Fase 2 (kill -9 matrix, crash consistency).
//!
//! Protocollo NDJSON (normativo, piano Fase 2 Task 12):
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
//! Flag CLI:
//!   --hang  dopo accepted NON emette final: resta appeso leggendo stdin, cosi'
//!           un `kill -9` lo termina e la morte del parent (stdin chiuso → EOF)
//!           lo fa uscire pulito senza lasciare orfani (test di kill/timeout)
//!   --fail  dopo accepted emette {"ev":"error","error":"engine-failure"}
//!
//! Solo std + serde_json (gia' dipendenze di `crewd`). Ogni riga stdout e' flushata
//! subito perche' i test leggono via pipe (line-buffered esplicito).

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use serde_json::{json, Value};

/// Stampa una riga NDJSON su stdout e flusha subito (line-buffered esplicito).
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

    // Ready iniziale (prima di leggere qualsiasi op).
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
            Err(_) => continue, // riga non JSON: ignorata (protocollo robusto a rumore)
        };
        let op = msg.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "turn" => {
                turn_counter += 1;
                let engine_turn_id = format!("t-{turn_counter}");
                emit(json!({ "ev": "accepted", "engine_turn_id": engine_turn_id }));

                if hang {
                    // Accettato ma nessun final: resta appeso. NON dorme in un
                    // loop cieco (lascerebbe orfani alla morte del parent):
                    // continua a leggere stdin, cosi' l'EOF (parent morto) lo fa
                    // uscire, mentre `kill -9` lo termina comunque.
                    continue;
                }
                if fail {
                    emit(json!({ "ev": "error", "error": "engine-failure" }));
                    continue;
                }

                let prompt = msg
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // resume_session riflesso nel final (null/assente => fake-sess-1).
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
                // op sconosciuta: ignorata (forward-compat).
            }
        }
    }

    // stdin chiuso senza op:exit: uscita pulita.
    ExitCode::SUCCESS
}
