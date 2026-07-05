//! Engine adapter contract + factory (crewd Fase 2 Task 8; AUDIT2 R2 B2/M3/M5).
//!
//! An adapter wraps ONE engine process for ONE active cell (created by the
//! supervisor in `supervisor.rs`). The contract is **non-blocking after
//! Accepted** (AUDIT2 B2): `start_turn` writes the turn request and returns
//! immediately; `EngineEvent`s (accepted/note/final/failed) are drained via
//! `poll_events` on later scheduler ticks. This lets the scheduler keep
//! ticking, detect per-turn timeouts and process death, and honour `interrupt`
//! while a turn is genuinely in flight — instead of blocking inside the adapter
//! (the old model, which only worked for the in-process fake).
//!
//! Resume is **typed** (AUDIT2 M3): `resume_thread` (codex `thread/resume`,
//! keyed by `engine_thread_id`) and `resume_session` (claude SDK resume, keyed
//! by `engine_session_id`) never share a parameter, so an id domain can never
//! be silently forwarded into the wrong field.

pub mod child;
pub mod claude;
pub mod codex;
pub mod fake;
pub mod pi;

use crewd_core::cells::EngineKind;
use crewd_core::engine::{EngineCaps, EngineEvent};
use crewd_core::error::BusError;

/// SPEC §20.3: engine process state, independent from ThreadState and JobState.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineProcState {
    Up,
    Down,
}

/// An adapter = one active CELL (one process). Created by the supervisor.
pub trait EngineAdapter: Send {
    fn caps(&self) -> EngineCaps;

    /// Begin a turn. **Non-blocking** (AUDIT2 B2): writes the request and
    /// returns. Events become available via `poll_events`. An error here means
    /// the request could not even be written (process already down).
    fn start_turn(&mut self, payload: &str) -> Result<(), BusError>;

    /// Drain the engine events observed since the last poll. Never blocks;
    /// returns an empty vec when nothing new arrived. A terminal event
    /// (`Final`/`Failed`) marks the turn complete.
    fn poll_events(&mut self) -> Vec<EngineEvent>;

    /// Best-effort interrupt of the in-flight turn (bounded, non-blocking).
    fn interrupt(&mut self) -> Result<(), BusError>;

    /// Resume by `engine_thread_id` (codex `thread/resume`). Honest resume
    /// (SPEC §20.6): re-opens the thread for a follow-up, never replays the
    /// lost turn. Default: engine has no thread resume.
    fn resume_thread(&mut self, _engine_thread_id: &str) -> Result<(), BusError> {
        Err(BusError::ThreadNotResumable(
            "engine has no thread resume".into(),
        ))
    }

    /// Resume by `engine_session_id` (claude SDK resume). Honest resume
    /// (SPEC §20.6). Default: engine has no session resume.
    fn resume_session(&mut self, _engine_session_id: &str) -> Result<(), BusError> {
        Err(BusError::ThreadNotResumable(
            "engine has no session resume".into(),
        ))
    }

    /// The engine's own thread id, once known (codex `thread/start`). Distinct
    /// from `crewd_thread_id` (SPEC §20.2). Default: none.
    fn engine_thread_id(&self) -> Option<String> {
        None
    }

    /// The engine's own session id, once observed (claude `final.session_id`).
    /// Used to persist the resume target. Default: none.
    fn engine_session_id(&self) -> Option<String> {
        None
    }

    fn proc_state(&self) -> EngineProcState;
    fn shutdown(&mut self); // kill process-group, best-effort
}

/// Engine spawn parameters (used by the real adapters Task 11-13; the in-process
/// `FakeEngine` ignores them).
#[derive(Debug, Clone, Default)]
pub struct EngineSpawnCfg {
    pub cwd: String,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub timeout_secs: u64,
    /// Override of the engine binary (contract/chaos tests, mock NDJSON).
    pub bin_override: Option<String>,
    /// When true, `bin_override` is spawned **directly** as a native executable
    /// (with `shim_args`), instead of via `node` (claude) / `app-server`
    /// (codex). The chaos suite uses this to run the native `crewd-fake-engine`
    /// so it can be `kill -9`'d (AUDIT2 R2 / Task 15).
    pub spawn_direct: bool,
    /// Path to the keys file (e.g. `~/.config/keys/ai.env`) for Z.AI profiles;
    /// `None` → `zai-*` profiles fail `E_ENGINE_DOWN` (no HOME read).
    pub keys_env_path: Option<String>,
    /// Extra args after the binary path (test/chaos hook, e.g. `--hang`).
    pub shim_args: Vec<String>,
}

/// Normative factory: maps `EngineKind` → concrete adapter. Unimplemented kinds
/// fail clear (`E_INTERNAL`) rather than degrade: the daemon must never
/// silently mount the wrong adapter.
pub fn make_adapter(
    kind: EngineKind,
    cfg: &EngineSpawnCfg,
) -> Result<Box<dyn EngineAdapter>, BusError> {
    match kind {
        EngineKind::Fake => Ok(Box::new(fake::FakeEngine::new())),
        EngineKind::Codex => Ok(Box::new(codex::CodexAdapter::new(cfg)?)),
        EngineKind::Claude => Ok(Box::new(claude::ClaudeAdapter::new(cfg)?)),
        EngineKind::Pi => Ok(Box::new(pi::PiAdapter::new(cfg)?)),
    }
}
