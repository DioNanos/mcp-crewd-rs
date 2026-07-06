//! engine-pi adapter (crewd Phase 2 Task 13; poll model). Spawns
//! `pi --mode rpc` (or `bin_override` for tests, where `bin_override` is the
//! executable — e.g. `node` — and `shim_args` carry the replay fixture + trace)
//! speaking the pi RPC NDJSON protocol:
//!
//!   crewd -> pi: `{"dir":"req","id":N,"method":"run","params":{"prompt":"…",…}}`
//!   pi -> crewd: `{"dir":"res","id":N,"result":{"status":"accepted|progress|
//!                 final|aborted",…}}` or
//!                `{"dir":"res","id":N,"error":{"code":…,"message":…}}`
//!
//! The turn is non-blocking: events arrive on the `LineChild`
//! reader thread and are drained by `poll_events`.
//!
//! SPEC §20.6 (last clause): pi v0 has **no session resume** — `resume_*` fail
//! `E_THREAD_NOT_RESUMABLE` (honest, never claims to replay).
use std::process::Command;

use crewd_core::engine::{EngineCaps, EngineEvent};
use crewd_core::error::BusError;

use crate::engines::child::LineChild;
use crate::engines::{EngineAdapter, EngineProcState, EngineSpawnCfg};

#[derive(Debug)]
pub struct PiAdapter {
    child: LineChild,
    req_id: u64,
}

impl PiAdapter {
    pub fn new(cfg: &EngineSpawnCfg) -> Result<Self, BusError> {
        let bin = cfg.bin_override.as_deref().unwrap_or("pi");
        let mut cmd = Command::new(bin);
        // smoke-T16 fix: run the engine child in the requested cwd.
        cmd.current_dir(&cfg.cwd);
        if cfg.bin_override.is_some() {
            // Test/replay mode: the override is the executable (e.g. `node`),
            // and `shim_args` carry the fixture + trace path.
            for a in &cfg.shim_args {
                cmd.arg(a);
            }
        } else {
            cmd.arg("--mode").arg("rpc");
        }
        let parse = |v: serde_json::Value, tx: &std::sync::mpsc::Sender<EngineEvent>| {
            if v.get("dir").and_then(|x| x.as_str()) != Some("res") {
                return;
            }
            if let Some(err) = v.get("error") {
                let code = err.get("code").and_then(|x| x.as_str()).unwrap_or("error");
                let _ = tx.send(EngineEvent::Failed {
                    error: code.to_string(),
                });
                return;
            }
            let Some(result) = v.get("result") else {
                return;
            };
            let status = result.get("status").and_then(|x| x.as_str()).unwrap_or("");
            match status {
                "accepted" => {
                    let turn_id = result
                        .get("engine_turn_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Accepted {
                        engine_turn_id: turn_id,
                    });
                }
                "progress" => {
                    let text = result
                        .get("note")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Note { text });
                }
                "final" => {
                    let fa = result
                        .get("final_answer")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Final { final_answer: fa });
                }
                "aborted" => {
                    let _ = tx.send(EngineEvent::Failed {
                        error: "aborted".into(),
                    });
                }
                _ => {}
            }
        };
        let child = LineChild::spawn(cmd, false, parse, "pi")?;
        Ok(PiAdapter { child, req_id: 0 })
    }
}

impl EngineAdapter for PiAdapter {
    fn caps(&self) -> EngineCaps {
        EngineCaps {
            supports_session_resume: false,
            supports_abort: true,
            supports_stream_replay: false,
            supports_model_override: true,
            supports_yolo: true,
        }
    }

    fn start_turn(&mut self, payload: &str) -> Result<(), BusError> {
        self.req_id += 1;
        let req = serde_json::json!({
            "dir": "req",
            "id": self.req_id,
            "method": "run",
            "params": { "prompt": payload, "mode": "background" }
        });
        self.child.send(req)
    }

    fn poll_events(&mut self) -> Vec<EngineEvent> {
        self.child.poll()
    }

    fn interrupt(&mut self) -> Result<(), BusError> {
        self.req_id += 1;
        self.child.send(serde_json::json!({
            "dir": "req",
            "id": self.req_id,
            "method": "abort",
            "params": {}
        }))
    }

    // resume_thread / resume_session use the trait defaults →
    // `E_THREAD_NOT_RESUMABLE` (SPEC §20.6: pi v0 has no session resume).

    fn proc_state(&self) -> EngineProcState {
        self.child.proc_state()
    }

    fn shutdown(&mut self) {
        self.child.shutdown();
    }
}
