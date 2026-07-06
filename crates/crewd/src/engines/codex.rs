//! engine-codex adapter (crewd Phase 2 Task 11). Spawns
//! `codex app-server` (or `bin_override`, used by the contract test with a mock
//! NDJSON server) and speaks JSON-RPC 2.0 NDJSON over stdio.
//!
//! Turn shape: `start_turn` sends `turn/start` and blocks only for
//! its immediate ack (the turn id → `Accepted`); the long `turn/completed`
//! notification is drained asynchronously by `poll_events`, so the scheduler
//! keeps ticking and can time the turn out / observe process death.
//!
//! SPEC §20.7 (explicit YOLO): the adapter sends `approvalPolicy:"never"` and a
//! full-access sandbox at `thread/start`, `thread/resume` and `turn/start`, and
//! **verifies the response** (`approvalPolicy=="never"` + `sandbox.type==
//! "dangerFullAccess"`) whenever the server echoes it; a mismatch fails clear
//! with `E_POLICY_DENIED` (never degrades). A JSON-RPC `error` object is
//! preserved and mapped to a deterministic `BusError` (M4), never dropped.
//!
//! SPEC §20.6 (honest resume): `resume_thread(engine_thread_id)` re-opens the
//! thread for a follow-up (never a replay). The parameter is typed as a thread
//! id and wired to the `threadId` field — an `engine_session_id` can never be
//! forwarded here (M3).
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crewd_core::engine::{EngineCaps, EngineEvent};
use crewd_core::error::BusError;
use serde_json::Value;

use crate::engines::{EngineAdapter, EngineProcState, EngineSpawnCfg};

/// SPEC §20.7: the four reasoning/delta notification methods crewd opts out of.
const OPT_OUT_METHODS: &[&str] = &[
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
];

/// Round-trip timeout for a single JSON-RPC request/response (seconds).
const RPC_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub struct CodexAdapter {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// Full JSON-RPC responses (with `id`), including any `error` object (M4).
    resp_rx: Receiver<(i64, Value)>,
    notif_rx: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    proc_state: EngineProcState,
    next_id: i64,
    engine_thread_id: Option<String>,
    turn_in_flight: Option<String>,
    /// Events produced by `start_turn` (the `Accepted`), drained by `poll_events`.
    pending: VecDeque<EngineEvent>,
}

impl CodexAdapter {
    pub fn new(cfg: &EngineSpawnCfg) -> Result<Self, BusError> {
        let bin = cfg.bin_override.as_deref().unwrap_or("codex");
        let mut cmd = Command::new(bin);
        if cfg.bin_override.is_none() {
            cmd.arg("app-server");
        }
        for a in &cfg.shim_args {
            cmd.arg(a);
        }
        // smoke-T16 fix: run the engine child in the requested cwd.
        cmd.current_dir(&cfg.cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| {
            BusError::EngineDown(format!("codex spawn: {}", safe_msg(&e.to_string())))
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);

        let (resp_tx, resp_rx) = mpsc::channel::<(i64, Value)>();
        let (notif_tx, notif_rx) = mpsc::channel::<Value>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_reader = alive.clone();
        let reader = stdout.map(|mut br| {
            thread::spawn(move || {
                loop {
                    let mut line = String::new();
                    match br.read_line(&mut line) {
                        Ok(0) | Err(_) => break, // EOF / error → stop
                        Ok(_) => {
                            let v: Value = match serde_json::from_str(&line) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if v.get("id").is_some() && v.get("method").is_none() {
                                let id = v["id"].as_i64().unwrap_or(0);
                                // Forward the FULL message (M4: keep `error`).
                                let _ = resp_tx.send((id, v));
                            } else if v.get("method").is_some() {
                                let _ = notif_tx.send(v);
                            }
                        }
                    }
                }
                alive_reader.store(false, Ordering::SeqCst);
            })
        });

        let mut a = CodexAdapter {
            child: Some(child),
            stdin,
            resp_rx,
            notif_rx,
            reader,
            alive,
            proc_state: EngineProcState::Up,
            next_id: 0,
            engine_thread_id: None,
            turn_in_flight: None,
            pending: VecDeque::new(),
        };
        a.initialize()?;
        a.thread_start(cfg)?;
        Ok(a)
    }

    fn send(&mut self, v: Value) -> Result<(), BusError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| BusError::Internal("codex stdin gone".into()))?;
        let mut s = serde_json::to_string(&v).map_err(|e| BusError::Internal(e.to_string()))?;
        s.push('\n');
        stdin.write_all(s.as_bytes()).map_err(|e| {
            BusError::EngineDown(format!("codex write: {}", safe_msg(&e.to_string())))
        })?;
        stdin.flush().map_err(|e| {
            BusError::EngineDown(format!("codex flush: {}", safe_msg(&e.to_string())))
        })?;
        Ok(())
    }

    /// One JSON-RPC request → its `result`. A JSON-RPC `error` object fails
    /// clear with a deterministic `BusError` (M4), never a silent `Null`.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, BusError> {
        self.next_id += 1;
        let id = self.next_id;
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.send(req)?;
        loop {
            let (rid, msg) = self
                .resp_rx
                .recv_timeout(RPC_TIMEOUT)
                .map_err(|e| BusError::EngineDown(format!("codex rpc timeout ({method}): {e}")))?;
            if rid != id {
                continue; // mis-routed (shouldn't happen single-thread)
            }
            return response_result(method, &msg);
        }
    }

    fn initialize(&mut self) -> Result<(), BusError> {
        let params = serde_json::json!({
            "clientInfo": {
                "name": "crewd",
                "title": "crewd cell fabric",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": false,
                "requestAttestation": false,
                "optOutNotificationMethods": OPT_OUT_METHODS,
            },
        });
        let _ = self.call("initialize", params)?;
        Ok(())
    }

    fn thread_start(&mut self, cfg: &EngineSpawnCfg) -> Result<(), BusError> {
        let params = serde_json::json!({
            "cwd": cfg.cwd,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
            "model": cfg.model,
        });
        let resp = self.call("thread/start", params)?;
        verify_yolo(&resp)?;
        self.engine_thread_id = resp
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        Ok(())
    }
}

impl EngineAdapter for CodexAdapter {
    fn caps(&self) -> EngineCaps {
        EngineCaps {
            supports_session_resume: true,
            supports_abort: true,
            supports_stream_replay: false,
            supports_model_override: true,
            supports_yolo: true,
        }
    }

    fn start_turn(&mut self, payload: &str) -> Result<(), BusError> {
        let thread_id = self
            .engine_thread_id
            .clone()
            .ok_or_else(|| BusError::Internal("codex: no engine thread".into()))?;
        let params = serde_json::json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": payload, "text_elements": [] }],
            "approvalPolicy": "never",
            "sandboxPolicy": { "type": "dangerFullAccess" },
        });
        // Block only for the turn/start ack (→ Accepted); the turn body completes
        // asynchronously via `poll_events`. A turn/start `error`
        // object surfaces as a deterministic `BusError` (M4) — a non-accepted
        // failure — instead of a dropped/degraded response.
        let resp = self.call("turn/start", params)?;
        // M4: verify YOLO on turn/start too, whenever the server echoes it.
        verify_yolo_if_present(&resp)?;
        let turn_id = resp
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.pending.push_back(EngineEvent::Accepted {
            engine_turn_id: turn_id.clone(),
        });
        self.turn_in_flight = Some(turn_id);
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<EngineEvent> {
        let mut out: Vec<EngineEvent> = self.pending.drain(..).collect();
        // Drain any turn notifications that arrived since the last poll.
        while let Ok(notif) = self.notif_rx.try_recv() {
            let method = notif.get("method").and_then(|v| v.as_str()).unwrap_or("");
            if method == "turn/completed" {
                let turn = notif
                    .get("params")
                    .and_then(|p| p.get("turn"))
                    .cloned()
                    .unwrap_or(Value::Null);
                out.push(EngineEvent::Final {
                    final_answer: extract_final_answer(&turn),
                });
                self.turn_in_flight = None;
            } else if method == "turn/failed" {
                let err = notif
                    .get("params")
                    .and_then(|p| p.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("turn failed")
                    .to_string();
                out.push(EngineEvent::Failed { error: err });
                self.turn_in_flight = None;
            }
            // other notifications (item/*, deltas): ignored at v0
        }
        out
    }

    fn interrupt(&mut self) -> Result<(), BusError> {
        // Non-blocking: fire `turn/interrupt` and return (do not await a reply).
        let Some(thread_id) = self.engine_thread_id.clone() else {
            return Ok(());
        };
        let Some(turn_id) = self.turn_in_flight.clone() else {
            return Ok(());
        };
        self.next_id += 1;
        let id = self.next_id;
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":id,"method":"turn/interrupt",
            "params": {"threadId": thread_id, "turnId": turn_id }
        });
        let _ = self.send(req);
        self.turn_in_flight = None;
        Ok(())
    }

    fn resume_thread(&mut self, engine_thread_id: &str) -> Result<(), BusError> {
        // Honest resume (SPEC §20.6): re-open the thread with the SAME policy,
        // verify YOLO again, never claim to replay a lost turn. Keyed strictly
        // by the engine THREAD id — never a session id (M3).
        let params = serde_json::json!({
            "threadId": engine_thread_id,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
        });
        let resp = self.call("thread/resume", params)?;
        verify_yolo(&resp)?;
        self.engine_thread_id = Some(engine_thread_id.to_string());
        Ok(())
    }

    fn engine_thread_id(&self) -> Option<String> {
        self.engine_thread_id.clone()
    }

    fn proc_state(&self) -> EngineProcState {
        if self.alive.load(Ordering::SeqCst) {
            self.proc_state
        } else {
            EngineProcState::Down
        }
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            #[cfg(unix)]
            {
                // whole pgroup (setsid), one direct syscall — no external
                // `kill` binary (limited applet on Android/Termux).
                if let Some(pgid) = rustix::process::Pid::from_raw(pid as i32) {
                    let _ =
                        rustix::process::kill_process_group(pgid, rustix::process::Signal::Kill);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.stdin = None;
        self.alive.store(false, Ordering::SeqCst);
        self.proc_state = EngineProcState::Down;
    }
}

/// Map a full JSON-RPC response to its `result`, failing clear on an `error`
/// object (M4). The error message is reduced to a safe ≤8-char prefix.
fn response_result(method: &str, msg: &Value) -> Result<Value, BusError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let m = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Err(BusError::EngineDown(format!(
            "codex {method} error {code}: {}",
            safe_msg(m)
        )));
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

/// SPEC §20.7 explicit-YOLO verify (mandatory): the response MUST carry
/// `approvalPolicy == "never"` and `sandbox.type == "dangerFullAccess"`; any
/// mismatch is `E_POLICY_DENIED` (fail clear, never degrade).
fn verify_yolo(resp: &Value) -> Result<(), BusError> {
    let ap = resp.get("approvalPolicy").and_then(|v| v.as_str());
    let st = resp
        .get("sandbox")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str());
    if ap != Some("never") || st != Some("dangerFullAccess") {
        return Err(BusError::PolicyDenied(format!(
            "codex YOLO mismatch: approvalPolicy={ap:?} sandbox.type={st:?}"
        )));
    }
    Ok(())
}

/// M4: verify YOLO only when the server echoes the policy fields (a `turn/start`
/// response is normally just the turn id). If either field is present it MUST be
/// the YOLO value, else `E_POLICY_DENIED` — a downgraded echo never slips by.
fn verify_yolo_if_present(resp: &Value) -> Result<(), BusError> {
    let has_ap = resp.get("approvalPolicy").is_some();
    let has_sandbox = resp.get("sandbox").is_some();
    if has_ap || has_sandbox {
        return verify_yolo(resp);
    }
    Ok(())
}

/// Extract the assistant final answer from a `turn/completed` turn: the text
/// of the LAST `agentMessage` item (a real turn emits several agentMessage —
/// progress notes first, final reply last). App-server v2 schema
/// (`ThreadItem::agentMessage`) carries the text in the flat `text` field;
/// the legacy `content[].{type:"text"}` block shape is kept as fallback.
/// Empty if no agentMessage is present. (Regression 2026-07-05: the previous
/// content[]-only parsing returned empty on every real v2 turn.)
fn extract_final_answer(turn: &Value) -> String {
    let Some(items) = turn.get("items").and_then(|i| i.as_array()) else {
        return String::new();
    };
    items
        .iter()
        .rev()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("agentMessage"))
        .map(|item| {
            // v2: flat `text` field.
            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                return t.to_string();
            }
            // Fallback legacy: content[] text blocks.
            let mut out = String::new();
            if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                            out.push_str(t);
                        }
                    }
                }
            }
            out
        })
        .unwrap_or_default()
}

/// ≤8-char single-line prefix so a foreign error string never leaks secrets
/// into logs/argv (SPEC §20.7 hardening, uniform across engine adapters).
fn safe_msg(s: &str) -> String {
    s.chars()
        .take(8)
        .collect::<String>()
        .replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REAL app-server v2 schema (`ThreadItem::agentMessage`): the text lives
    /// in the flat `text` field, NOT in block-based `content[]`. Regression
    /// 2026-07-05: the adapter only looked at `content[]` → `final_answer`
    /// always empty on real codex turns (auditor report lost).
    #[test]
    fn extract_final_answer_from_v2_agent_message_text_field() {
        let turn = serde_json::json!({
            "items": [{ "type": "agentMessage", "id": "i1", "text": "hello", "phase": null }]
        });
        assert_eq!(extract_final_answer(&turn), "hello");
    }

    /// Real turns emit MULTIPLE agentMessage items (progress notes + final
    /// answer): the final answer is the LAST one, not the concat of all.
    #[test]
    fn extract_final_answer_takes_last_agent_message() {
        let turn = serde_json::json!({
            "items": [
                { "type": "agentMessage", "id": "i1", "text": "progress note" },
                { "type": "commandExecution", "id": "i2" },
                { "type": "agentMessage", "id": "i3", "text": "final report" }
            ]
        });
        assert_eq!(extract_final_answer(&turn), "final report");
    }

    /// Legacy fallback: block-based `content[].{type:text,text}` shape still
    /// accepted if the flat `text` field is missing.
    #[test]
    fn extract_final_answer_from_agent_message() {
        let turn = serde_json::json!({
            "items": [{ "type": "agentMessage", "content": [{ "type": "text", "text": "hello" }] }]
        });
        assert_eq!(extract_final_answer(&turn), "hello");
    }

    #[test]
    fn extract_final_answer_empty_when_no_agent_message() {
        let turn = serde_json::json!({ "items": [{ "type": "command_output", "text": "x" }] });
        assert_eq!(extract_final_answer(&turn), "");
    }

    #[test]
    fn verify_yolo_rejects_untrusted() {
        let resp = serde_json::json!({"approvalPolicy": "untrusted", "sandbox": {"type": "dangerFullAccess"}});
        assert_eq!(verify_yolo(&resp).unwrap_err().code(), "E_POLICY_DENIED");
    }

    #[test]
    fn verify_yolo_accepts_never_danger() {
        let resp =
            serde_json::json!({"approvalPolicy": "never", "sandbox": {"type": "dangerFullAccess"}});
        assert!(verify_yolo(&resp).is_ok());
    }

    #[test]
    fn verify_yolo_if_present_ignores_absent_but_catches_downgrade() {
        // absent → ok (turn/start normally echoes nothing)
        assert!(verify_yolo_if_present(&serde_json::json!({"turn": {"id": "t1"}})).is_ok());
        // present + downgraded → E_POLICY_DENIED (M4)
        let bad = serde_json::json!({"approvalPolicy": "untrusted"});
        assert_eq!(
            verify_yolo_if_present(&bad).unwrap_err().code(),
            "E_POLICY_DENIED"
        );
    }

    #[test]
    fn response_result_maps_error_object_to_buserror() {
        let msg = serde_json::json!({"id": 1, "error": {"code": -32000, "message": "boom"}});
        let e = response_result("turn/start", &msg).unwrap_err();
        assert_eq!(e.code(), "E_ENGINE_DOWN");
    }

    #[test]
    fn opt_out_methods_are_the_four_deltas() {
        assert_eq!(
            OPT_OUT_METHODS,
            &[
                "item/agentMessage/delta",
                "item/reasoning/summaryTextDelta",
                "item/reasoning/summaryPartAdded",
                "item/reasoning/textDelta"
            ]
        );
    }
}
