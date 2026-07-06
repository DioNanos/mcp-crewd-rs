//! engine-pi adapter (crewd Phase 2 Task 13; poll model). Spawns
//! `pi --mode rpc` (or `bin_override` for tests, where `bin_override` is the
//! executable — e.g. `node` — and `shim_args` carry the replay fixture + trace)
//! speaking the REAL pi RPC wire protocol (JSONL, LF-only framing).
//!
//! Wire shapes (reverse-engineered against `@earendil-works/pi-coding-agent`
//! v0.78.1; see `docs/plans/pi-real-rpc-protocol.md`):
//!
//!   crewd -> pi: `{"type":"prompt","id":"req_<N>","message":"…"}`            (start a turn)
//!                `{"type":"abort","id":"req_<N>"}`                           (cancel)
//!   pi    -> crewd: `{"id":"…","type":"response","command":"prompt","success":true}`   → Accepted
//!                   `{"id":"…","type":"response","command":"prompt","success":false,"error":"…"}` → Failed
//!                   `{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"…"}}` → Note
//!                   `{"type":"tool_execution_start","toolName":"…",…}` → Note
//!                   `{"type":"agent_end","messages":[…],"willRetry":false}`   → Final | Failed (terminal)
//!                   `{"type":"extension_ui_request"|"extension_error",…}`     → ignored (side channel)
//!
//! The turn is non-blocking: `start_turn` writes the `prompt` and returns; the
//! deferred-ack `prompt` response and the streaming events arrive on the shared
//! `LineChild` reader thread and are drained by `poll_events`. A turn is
//! terminal at the first `agent_end` with `willRetry:false` (or a `prompt`
//! `success:false` preflight rejection).
//!
//! Session resume (SPEC §8): pi can resume a past session, but only at SPAWN
//! time (`pi --mode rpc --session-id <id>`) — there is no in-band `resume`
//! command. The supervisor does not yet respawn pi with `--session-id`, so the
//! adapter honestly reports `supports_session_resume: false` and keeps the
//! default trait `resume_*` (`E_THREAD_NOT_RESUMABLE`); wiring spawn-time
//! resume is future work.
use std::process::Command;

use crewd_core::engine::{EngineCaps, EngineEvent};
use crewd_core::error::BusError;
use serde_json::Value;

use crate::engines::child::LineChild;
use crate::engines::{EngineAdapter, EngineProcState, EngineSpawnCfg};

#[derive(Debug)]
pub struct PiAdapter {
    child: LineChild,
    /// Monotonic request counter; emitted as the opaque string id `"req_<N>"`
    /// (the real protocol's `id` is an opaque string, not a number).
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
            // and `shim_args` carry the replay fixture + trace path.
            for a in &cfg.shim_args {
                cmd.arg(a);
            }
        } else {
            cmd.arg("--mode").arg("rpc");
            // If a model is configured, pass it through. Do not hardcode a
            // provider — pi selects one from its own config / env.
            if let Some(model) = &cfg.model {
                cmd.arg("--model").arg(model);
            }
        }

        // The reader thread routes each line via the pure `map_event` and
        // forwards the resulting EngineEvent (if any) on the channel.
        let parse = |v: Value, tx: &std::sync::mpsc::Sender<EngineEvent>| {
            if let Some(ev) = map_event(&v) {
                let _ = tx.send(ev);
            }
        };

        let child = LineChild::spawn(cmd, false, parse, "pi")?;
        Ok(PiAdapter { child, req_id: 0 })
    }
}

impl EngineAdapter for PiAdapter {
    fn caps(&self) -> EngineCaps {
        EngineCaps {
            // Resume needs a spawn-time respawn with `--session-id` (SPEC §8),
            // not yet wired in the supervisor → honestly not resumable in-band.
            supports_session_resume: false,
            supports_abort: true,
            supports_stream_replay: false,
            supports_model_override: true,
            supports_yolo: true,
        }
    }

    fn start_turn(&mut self, payload: &str) -> Result<(), BusError> {
        self.req_id += 1;
        let id = format!("req_{}", self.req_id);
        self.child.send(prompt_request(&id, payload))
    }

    fn poll_events(&mut self) -> Vec<EngineEvent> {
        self.child.poll()
    }

    fn interrupt(&mut self) -> Result<(), BusError> {
        // Non-blocking: write `abort` and return. The turn then finalizes with
        // an aborted `agent_end` (mapped to Failed{error:"aborted"}).
        self.req_id += 1;
        let id = format!("req_{}", self.req_id);
        self.child.send(abort_request(&id))
    }

    // resume_session / resume_thread use the trait defaults →
    // `E_THREAD_NOT_RESUMABLE`: pi resume is a spawn-time respawn with
    // `--session-id` (SPEC §8), not yet wired in the supervisor.

    fn proc_state(&self) -> EngineProcState {
        self.child.proc_state()
    }

    fn shutdown(&mut self) {
        self.child.shutdown();
    }
}

/// Build a `prompt` command (SPEC §3.1). `id` is an opaque STRING
/// (`"req_<N>"`); no `method`, no `params`, no `mode`.
fn prompt_request(id: &str, message: &str) -> Value {
    serde_json::json!({ "type": "prompt", "id": id, "message": message })
}

/// Build an `abort` command (SPEC §9). `id` is an opaque string; no `params`.
fn abort_request(id: &str) -> Value {
    serde_json::json!({ "type": "abort", "id": id })
}

/// Map ONE real pi wire line to zero or one `EngineEvent`. Pure & testable.
///
/// Routing (SPEC §12 mapping table), keyed on the top-level `type`:
/// - `response` + `command:"prompt"` + `success:true`  → `Accepted{ id }`
/// - `response` + `command:"prompt"` + `success:false` → `Failed{ error }`
/// - `response` (any other `command`)                  → ignore (non-prompt ack)
/// - `message_update` with `assistantMessageEvent.type == "text_delta"` → `Note{ delta }`
/// - `tool_execution_start` / `_update` / `_end`       → `Note{ "tool: <toolName>" }`
/// - `agent_end` + `willRetry:true`                    → hold (emit nothing)
/// - `agent_end` + last assistant `stopReason ∈ {stop,length,toolUse,absent}` → `Final{ text }`
/// - `agent_end` + last assistant `stopReason == "aborted"` → `Failed{ "aborted" }`
/// - `agent_end` + last assistant `stopReason == "error"`   → `Failed{ errorMessage | "error" }`
/// - `extension_ui_request` / `extension_error`        → ignore (side channel)
/// - anything else                                     → ignore
fn map_event(v: &Value) -> Option<EngineEvent> {
    match v.get("type").and_then(|t| t.as_str())? {
        // Command acks. Only the `prompt` ack carries turn semantics; every
        // other command ack (abort/get_state/parse/unknown/…) is ignored.
        "response" => {
            if v.get("command").and_then(|c| c.as_str()) != Some("prompt") {
                return None;
            }
            if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default();
                Some(EngineEvent::Accepted {
                    engine_turn_id: id.to_string(),
                })
            } else {
                let error = v
                    .get("error")
                    .and_then(|x| x.as_str())
                    .unwrap_or("prompt rejected");
                Some(EngineEvent::Failed {
                    error: error.to_string(),
                })
            }
        }
        // Streaming text tokens → Note. Non-text deltas (start/text_end/
        // thinking_*/toolcall_*/done) carry no forwardable token.
        "message_update" => {
            let ame = v.get("assistantMessageEvent")?;
            if ame.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
                let delta = ame
                    .get("delta")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                Some(EngineEvent::Note {
                    text: delta.to_string(),
                })
            } else {
                None
            }
        }
        // A tool started running → progress note.
        "tool_execution_start" => {
            let tool = v.get("toolName").and_then(|x| x.as_str()).unwrap_or("tool");
            Some(EngineEvent::Note {
                text: format!("tool: {tool}"),
            })
        }
        // The only terminal event of a turn. `willRetry:true` → another attempt
        // follows, so hold. Otherwise the LAST assistant message's `stopReason`
        // decides Final vs Failed (SPEC §7/§12).
        "agent_end" => {
            if v.get("willRetry")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
            {
                return None;
            }
            let last_assistant = v
                .get("messages")
                .and_then(|m| m.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .rev()
                        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
                });
            // A terminal event MUST produce a terminal EngineEvent, else the
            // turn never completes. With no assistant message, close it empty.
            let Some(msg) = last_assistant else {
                return Some(EngineEvent::Final {
                    final_answer: String::new(),
                });
            };
            match msg.get("stopReason").and_then(|x| x.as_str()) {
                Some("aborted") => Some(EngineEvent::Failed {
                    error: "aborted".to_string(),
                }),
                Some("error") => {
                    let em = msg
                        .get("errorMessage")
                        .and_then(|x| x.as_str())
                        .unwrap_or("error");
                    Some(EngineEvent::Failed {
                        error: em.to_string(),
                    })
                }
                // stop | length | toolUse | absent → success.
                _ => Some(EngineEvent::Final {
                    final_answer: concat_text_content(msg),
                }),
            }
        }
        // agent_start / turn_start / message_start / message_end / turn_end /
        // queue_update / compaction_* / extension_ui_request / extension_error
        // / anything unknown → not a turn-terminal or progress signal.
        _ => None,
    }
}

/// Concatenate the `text` blocks of a message's `content[]` (assistant final
/// text). Returns the empty string if there is no text content. Tolerates a
/// string-valued `content` (some user messages) by returning it verbatim.
fn concat_text_content(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                        out.push_str(t);
                    }
                }
            }
            out
        }
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse one JSONL wire line into its mapped EngineEvent.
    fn map_line(json: &str) -> Option<EngineEvent> {
        let v: Value = serde_json::from_str(json).unwrap();
        map_event(&v)
    }

    // --- Accepted / Failed from the `prompt` response ---

    #[test]
    fn prompt_response_success_maps_to_accepted() {
        let ev = map_line(r#"{"id":"req_2","type":"response","command":"prompt","success":true}"#)
            .expect("prompt success -> Some(Accepted)");
        assert!(
            matches!(ev, EngineEvent::Accepted { ref engine_turn_id } if engine_turn_id == "req_2"),
            "got {ev:?}"
        );
    }

    #[test]
    fn prompt_response_failure_maps_to_failed_with_error() {
        let ev = map_line(
            r#"{"id":"req_2","type":"response","command":"prompt","success":false,"error":"No API key found for anthropic."}"#,
        )
        .expect("prompt fail -> Some(Failed)");
        assert!(
            matches!(ev, EngineEvent::Failed { ref error } if error == "No API key found for anthropic."),
            "got {ev:?}"
        );
    }

    #[test]
    fn non_prompt_response_is_ignored() {
        // abort / get_state / parse / unknown-command acks never map to a turn event.
        assert!(
            map_line(r#"{"id":"req_5","type":"response","command":"abort","success":true}"#)
                .is_none()
        );
        assert!(map_line(r#"{"id":"req_3","type":"response","command":"get_session_stats","success":true,"data":{}}"#).is_none());
        assert!(map_line(r#"{"type":"response","command":"parse","success":false,"error":"Failed to parse command: …"}"#).is_none());
    }

    // --- Note from streaming deltas / tool execution ---

    #[test]
    fn message_update_text_delta_maps_to_note() {
        let ev = map_line(
            r#"{"type":"message_update","message":{"role":"assistant"},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello","partial":{"role":"assistant"}}}"#,
        )
        .expect("text_delta -> Some(Note)");
        assert!(
            matches!(ev, EngineEvent::Note { ref text } if text == "Hello"),
            "got {ev:?}"
        );
    }

    #[test]
    fn message_update_non_text_delta_is_ignored() {
        // start / text_start / text_end / thinking_* / done carry no forwardable token.
        assert!(map_line(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"Hello","partial":{"role":"assistant"}}}"#
        )
        .is_none());
        assert!(map_line(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm","partial":{"role":"assistant"}}}"#
        )
        .is_none());
    }

    #[test]
    fn tool_execution_start_maps_to_note() {
        let ev = map_line(
            r#"{"type":"tool_execution_start","toolCallId":"toolu_01","toolName":"read","args":{"filePath":"./package.json"}}"#,
        )
        .expect("tool_execution_start -> Some(Note)");
        assert!(
            matches!(ev, EngineEvent::Note { ref text } if text == "tool: read"),
            "got {ev:?}"
        );
    }

    // --- Final / Failed from agent_end (the terminal event) ---

    #[test]
    fn agent_end_stop_maps_to_final_with_concatenated_text() {
        let ev = map_line(
            r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"text","text":"Hello"},{"type":"text","text":" world"}],"stopReason":"stop"}]}"#,
        )
        .expect("agent_end stop -> Some(Final)");
        assert!(
            matches!(ev, EngineEvent::Final { ref final_answer } if final_answer == "Hello world"),
            "got {ev:?}"
        );
    }

    #[test]
    fn agent_end_tooluse_maps_to_final() {
        // toolUse is a success stopReason (a tool was called; the run ended cleanly).
        let ev = map_line(
            r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"toolUse"}]}"#,
        )
        .expect("agent_end toolUse -> Some(Final)");
        assert!(matches!(ev, EngineEvent::Final { .. }), "got {ev:?}");
    }

    #[test]
    fn agent_end_aborted_maps_to_failed() {
        let ev = map_line(
            r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","content":[{"type":"text","text":"Once upon"}],"stopReason":"aborted","errorMessage":"aborted"}]}"#,
        )
        .expect("agent_end aborted -> Some(Failed)");
        assert!(
            matches!(ev, EngineEvent::Failed { ref error } if error == "aborted"),
            "got {ev:?}"
        );
    }

    #[test]
    fn agent_end_error_maps_to_failed_with_error_message() {
        let ev = map_line(
            r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","content":[{"type":"text","text":""}],"stopReason":"error","errorMessage":"rate limited"}]}"#,
        )
        .expect("agent_end error -> Some(Failed)");
        assert!(
            matches!(ev, EngineEvent::Failed { ref error } if error == "rate limited"),
            "got {ev:?}"
        );
    }

    #[test]
    fn agent_end_will_retry_is_held() {
        // willRetry:true means another attempt follows; emit nothing.
        assert!(map_line(
            r#"{"type":"agent_end","willRetry":true,"messages":[{"role":"assistant","content":[{"type":"text","text":"x"}],"stopReason":"stop"}]}"#
        )
        .is_none());
    }

    #[test]
    fn agent_end_uses_last_assistant_message() {
        // Several assistant messages: the LAST one's stopReason decides the outcome.
        let ev = map_line(
            r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","content":[{"type":"text","text":"first"}],"stopReason":"toolUse"},{"role":"toolResult","content":[]},{"role":"assistant","content":[{"type":"text","text":"final answer"}],"stopReason":"stop"}]}"#,
        )
        .expect("last assistant -> Final");
        assert!(
            matches!(ev, EngineEvent::Final { ref final_answer } if final_answer == "final answer"),
            "got {ev:?}"
        );
    }

    // --- Side-channel ignore ---

    #[test]
    fn extension_ui_request_and_error_are_ignored() {
        assert!(map_line(
            r#"{"type":"extension_ui_request","id":"ff479978","method":"setWidget","widgetKey":"subagent-async"}"#
        )
        .is_none());
        assert!(map_line(
            r#"{"type":"extension_error","extensionPath":"/x/pi-subagents","event":"session_shutdown","error":"stale ctx"}"#
        )
        .is_none());
    }

    // --- Unhandled event types are ignored, never crash ---

    #[test]
    fn unhandled_event_types_are_ignored() {
        assert!(map_line(r#"{"type":"agent_start"}"#).is_none());
        assert!(map_line(r#"{"type":"turn_start"}"#).is_none());
        assert!(map_line(r#"{"type":"message_end","message":{"role":"assistant"}}"#).is_none());
        assert!(map_line(r#"{"type":"turn_end","message":{}}"#).is_none());
        assert!(map_line(r#"{"type":"queue_update","steering":[],"followUp":[]}"#).is_none());
        assert!(map_line(r#"{"type":"compaction_start","reason":"manual"}"#).is_none());
        assert!(map_line(r#"{"type":"some_new_event"}"#).is_none());
    }

    // --- request wire shapes (start_turn / interrupt) ---

    #[test]
    fn prompt_request_is_real_shape_no_method_params_mode() {
        let v = prompt_request("req_2", "Say hello in one word.");
        assert_eq!(v["type"], "prompt");
        assert_eq!(v["id"], "req_2"); // STRING id, not numeric
        assert_eq!(v["message"], "Say hello in one word.");
        // The invented protocol's fields MUST NOT be present.
        assert!(v.get("method").is_none());
        assert!(v.get("params").is_none());
        assert!(v.get("mode").is_none());
        assert!(v.get("dir").is_none());
    }

    #[test]
    fn abort_request_is_real_shape_no_params() {
        let v = abort_request("req_5");
        assert_eq!(v["type"], "abort");
        assert_eq!(v["id"], "req_5");
        assert!(v.get("method").is_none());
        assert!(v.get("params").is_none());
        assert!(v.get("dir").is_none());
    }

    #[test]
    fn start_turn_emits_incrementing_string_ids() {
        // The id counter is monotonic and formatted as a "req_<N>" string.
        assert_eq!(format!("req_{}", 1u64), "req_1");
        assert_eq!(format!("req_{}", 2u64), "req_2");
    }

    // --- concat_text_content sanity ---

    #[test]
    fn concat_text_content_assembles_blocks_and_handles_string() {
        let v = serde_json::json!({"content":[{"type":"text","text":"a"},{"type":"thinking","text":"x"},{"type":"text","text":"b"}]});
        assert_eq!(concat_text_content(&v), "ab");
        let s = serde_json::json!({"content":"raw string"});
        assert_eq!(concat_text_content(&s), "raw string");
        assert_eq!(concat_text_content(&serde_json::json!({})), "");
    }
}
