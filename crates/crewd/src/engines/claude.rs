//! engine-claude adapter (crewd Phase 2 Task 12-rust; poll model).
//! Spawns `node <shim>` (default `shim/claude-shim.mjs`, override via
//! `EngineSpawnCfg.bin_override`; `spawn_direct` runs a native binary instead —
//! the chaos suite's `crewd-fake-engine`) speaking the NDJSON protocol:
//!
//!   crewd -> shim: `{"op":"turn","prompt":"…","resume_session":null|"sid"}`
//!                 `{"op":"abort"}` · `{"op":"exit"}`
//!   shim  -> crewd: `{"ev":"ready","session_id":null}` ·
//!                   `{"ev":"accepted","engine_turn_id":"t-<n>"}` ·
//!                   `{"ev":"note","text":"…"}` ·
//!                   `{"ev":"final","final_answer":"…","session_id":"sid"}` ·
//!                   `{"ev":"error","error":"…"}`
//!
//! The turn is **non-blocking**: `start_turn` writes the op and
//! returns; events arrive on the shared `LineChild` reader thread and are
//! drained by `poll_events`. `interrupt` writes `abort` and returns at once —
//! the shim (fixed for M5) processes it mid-turn.
//!
//! SPEC §20.7 (normative): the child environment is ALLOWLIST-ONLY, built per
//! profile. `max` = passthrough of allowlist vars from the daemon env;
//! `zai-a`/`zai-p` = like `max` but `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/
//! `ANTHROPIC_MODEL` are derived from `keys_env_path`. Secrets never appear in
//! argv/logs; error messages carry a ≤8-char prefix.
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use crewd_core::engine::{EngineCaps, EngineEvent};
use crewd_core::error::BusError;

use crate::engines::child::{safe_prefix, LineChild};
use crate::engines::{EngineAdapter, EngineProcState, EngineSpawnCfg};

/// SPEC §20.7 EXACT env allowlist for engine-claude (normative).
pub const ENV_ALLOWLIST: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "HOME",
    "PATH",
    "NODE_OPTIONS",
    "TMPDIR",
    "LANG",
    "TERM",
];

/// Z.AI anthropic-compatible base URL; model for `zai-*` profiles.
const ZAI_BASE_URL: &str = "https://api.z.ai/api/anthropic";
const ZAI_MODEL: &str = "glm-5.2";

#[derive(Debug)]
pub struct ClaudeAdapter {
    child: LineChild,
    pending_resume: Option<String>,
    /// Latest session id observed in a `final` event — updated by the reader
    /// thread; read for the resume target (SPEC §20.6).
    session: Arc<Mutex<Option<String>>>,
}

impl ClaudeAdapter {
    /// Build the adapter, build the allowlisted env, and spawn the shim.
    /// For `zai-a`/`zai-p` profiles `keys_env_path` MUST be set and parseable,
    /// else `E_ENGINE_DOWN` is returned (no `HOME` is read).
    pub fn new(cfg: &EngineSpawnCfg) -> Result<Self, BusError> {
        let env = build_env(cfg)?;
        let mut cmd = if cfg.spawn_direct {
            // Native binary (chaos `crewd-fake-engine`): run it directly.
            let bin = cfg
                .bin_override
                .as_deref()
                .ok_or_else(|| BusError::Internal("spawn_direct without bin_override".into()))?;
            Command::new(bin)
        } else {
            let shim_path = cfg
                .bin_override
                .as_deref()
                .unwrap_or("shim/claude-shim.mjs");
            // smoke-T16: the shim path must be resolved to ABSOLUTE before
            // current_dir (relative to the daemon's cwd, not the child's requested one).
            let shim_abs = std::fs::canonicalize(shim_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| shim_path.to_string());
            let mut c = Command::new("node");
            c.arg(shim_abs);
            c
        };
        for a in &cfg.shim_args {
            cmd.arg(a);
        }
        // smoke-T16 fix: the engine child MUST run in the requested cwd —
        // without this it inherits the daemon's cwd and the cell operates on
        // the wrong repository/worktree.
        cmd.current_dir(&cfg.cwd);
        cmd.env_clear();
        cmd.envs(env);

        let session: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let session_reader = session.clone();
        let parse = move |v: serde_json::Value, tx: &std::sync::mpsc::Sender<EngineEvent>| {
            let ev = v.get("ev").and_then(|x| x.as_str()).unwrap_or("");
            match ev {
                "accepted" => {
                    let id = v
                        .get("engine_turn_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Accepted { engine_turn_id: id });
                }
                "note" => {
                    let text = v
                        .get("text")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Note { text });
                }
                "final" => {
                    if let Some(sid) = v.get("session_id").and_then(|x| x.as_str()) {
                        if let Ok(mut g) = session_reader.lock() {
                            *g = Some(sid.to_string());
                        }
                    }
                    let fa = v
                        .get("final_answer")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Final { final_answer: fa });
                }
                "error" => {
                    let err = v
                        .get("error")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(EngineEvent::Failed { error: err });
                }
                _ => {} // ready / unknown: ignore
            }
        };

        // The shim emits `{"ev":"ready",…}` first: consume it synchronously so a
        // shim that never comes up surfaces as `E_ENGINE_DOWN` at construction.
        let child = LineChild::spawn(cmd, true, parse, "node")?;
        Ok(ClaudeAdapter {
            child,
            pending_resume: None,
            session,
        })
    }

    /// Last `session_id` observed in a `final` event (test/observability helper).
    pub fn last_session_id(&self) -> Option<String> {
        self.session.lock().ok().and_then(|g| g.clone())
    }

    /// OS pid of the shim's process group (chaos tests `kill -9` it).
    pub fn pid(&self) -> u32 {
        self.child.pid()
    }
}

impl EngineAdapter for ClaudeAdapter {
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
        let op = serde_json::json!({
            "op": "turn",
            "prompt": payload,
            "resume_session": self.pending_resume.take(),
        });
        self.child.send(op)
    }

    fn poll_events(&mut self) -> Vec<EngineEvent> {
        self.child.poll()
    }

    fn interrupt(&mut self) -> Result<(), BusError> {
        // Non-blocking: write `abort` and return. The shim processes
        // it mid-turn; the resulting `error` event arrives via `poll_events`.
        self.child.send(serde_json::json!({"op":"abort"}))
    }

    fn resume_session(&mut self, engine_session_id: &str) -> Result<(), BusError> {
        // Honest resume (SPEC §20.6): pass the session id to the NEXT turn as
        // `resume_session`; never claim to replay the lost turn.
        self.pending_resume = Some(engine_session_id.to_string());
        Ok(())
    }

    fn engine_session_id(&self) -> Option<String> {
        self.last_session_id()
    }

    fn proc_state(&self) -> EngineProcState {
        self.child.proc_state()
    }

    fn shutdown(&mut self) {
        self.child.shutdown();
    }
}

/// Build the ALLOWLIST-ONLY child env for the shim (SPEC §20.7).
fn build_env(cfg: &EngineSpawnCfg) -> Result<HashMap<String, String>, BusError> {
    let mut env: HashMap<String, String> = HashMap::new();
    for k in ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                env.insert((*k).to_string(), v);
            }
        }
    }
    match cfg.profile.as_deref() {
        None | Some("max") => {}
        Some(p) if p == "zai-a" || p == "zai-p" => {
            let path = cfg.keys_env_path.as_ref().ok_or_else(|| {
                BusError::EngineDown(format!("zai profile {p:?} requires keys_env_path"))
            })?;
            let wanted = if p == "zai-a" {
                "ZAI_API_KEY_A"
            } else {
                "ZAI_API_KEY_P"
            };
            let key = parse_key_file(path, wanted)?;
            env.insert("ANTHROPIC_BASE_URL".into(), ZAI_BASE_URL.into());
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key);
            env.insert("ANTHROPIC_MODEL".into(), ZAI_MODEL.into());
        }
        Some(other) => {
            return Err(BusError::EngineDown(format!(
                "unknown claude profile: {}",
                safe_prefix(other)
            )));
        }
    }
    Ok(env)
}

/// Parse a key file (`KEY=value` lines, optional `export ` prefix, optional
/// surrounding quotes) and return the value for `wanted`. The value never
/// appears in any error message.
fn parse_key_file(path: &str, wanted: &str) -> Result<String, BusError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        BusError::EngineDown(format!(
            "keys file unreadable: {}",
            safe_prefix(&e.to_string())
        ))
    })?;
    for raw in content.lines() {
        let l = raw.trim();
        let l = l.strip_prefix("export ").unwrap_or(l);
        if let Some((k, v)) = l.split_once('=') {
            if k.trim() == wanted {
                let val = v.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return Ok(val.to_string());
                }
            }
        }
    }
    Err(BusError::EngineDown(format!(
        "keys file missing entry: {wanted}"
    )))
}
