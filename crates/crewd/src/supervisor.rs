//! `EngineSupervisor` (crewd Phase 2 Task 8): manages 1 adapter (= 1 engine
//! process) per active cell. `ensure` is idempotent (reuses the existing
//! adapter); `stop` performs shutdown + audit `engine_stopped`;
//! `respawn_backoff_secs` returns the capped exponential backoff (reuses the
//! Phase 1 formula).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crewd_core::audit::{AuditChain, AuditEventDraft};
use crewd_core::cells::EngineKind;
use crewd_core::engine::EngineEvent;
use crewd_core::error::BusError;

use crate::engines::{make_adapter, EngineAdapter, EngineProcState, EngineSpawnCfg};

/// SPEC §20.9 max clamp for timeouts; reused as the respawn backoff cap.
const RESPAWN_BACKOFF_CAP_SECS: u64 = 60;

struct SupervisedEngine {
    #[allow(dead_code)] // used by real adapters to respawn with the right kind
    kind: EngineKind,
    adapter: Box<dyn EngineAdapter>,
}

/// Supervisor: one entry per active cell + per-cell backoff state.
pub struct EngineSupervisor {
    engines: HashMap<String, SupervisedEngine>,
    /// Current respawn step per cell (incremented on each `respawn_backoff_secs`).
    backoff_step: HashMap<String, u32>,
    audit: Arc<Mutex<AuditChain>>,
}

impl EngineSupervisor {
    pub fn new(audit: Arc<Mutex<AuditChain>>) -> Self {
        Self {
            engines: HashMap::new(),
            backoff_step: HashMap::new(),
            audit,
        }
    }

    /// Idempotent: if the cell already has an active adapter it is reused
    /// (1 process per cell, SPEC §20 v0); otherwise it is created via `make_adapter`.
    pub fn ensure(
        &mut self,
        cell: &str,
        kind: EngineKind,
        cfg: &EngineSpawnCfg,
    ) -> Result<&mut Box<dyn EngineAdapter>, BusError> {
        if !self.engines.contains_key(cell) {
            let adapter = make_adapter(kind, cfg)?;
            self.engines
                .insert(cell.to_string(), SupervisedEngine { kind, adapter });
        }
        Ok(&mut self
            .engines
            .get_mut(cell)
            .expect("just inserted or pre-existing")
            .adapter)
    }

    /// Injects a pre-built adapter for a cell (test override / NDJSON mock
    /// bin_override): it will be reused by `ensure`. Replaces any pre-existing
    /// adapter for the cell.
    pub fn insert(&mut self, cell: &str, adapter: Box<dyn EngineAdapter>) {
        self.engines.insert(
            cell.to_string(),
            SupervisedEngine {
                kind: EngineKind::Fake,
                adapter,
            },
        );
        self.backoff_step.entry(cell.to_string()).or_insert(0);
    }

    /// True if the cell has an active adapter.
    pub fn has(&self, cell: &str) -> bool {
        self.engines.contains_key(cell)
    }

    /// Starts a turn on the cell's adapter (non-blocking after Accepted).
    pub fn start_turn(&mut self, cell: &str, payload: &str) -> Result<(), BusError> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.start_turn(payload),
            None => Err(BusError::EngineDown(format!("no engine for cell {cell}"))),
        }
    }

    /// Drains the cell's engine events (non-blocking). Empty if the cell is
    /// not active.
    pub fn poll(&mut self, cell: &str) -> Vec<EngineEvent> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.poll_events(),
            None => Vec::new(),
        }
    }

    /// State of the cell's engine process (None if not active).
    pub fn proc_state(&self, cell: &str) -> Option<EngineProcState> {
        self.engines.get(cell).map(|se| se.adapter.proc_state())
    }

    /// Best-effort interrupt of the cell's in-flight turn.
    pub fn interrupt(&mut self, cell: &str) -> Result<(), BusError> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.interrupt(),
            None => Ok(()),
        }
    }

    /// Engine (claude) session id for the cell, if known.
    pub fn engine_session_id(&self, cell: &str) -> Option<String> {
        self.engines
            .get(cell)
            .and_then(|se| se.adapter.engine_session_id())
    }

    /// Engine (codex) thread id for the cell, if known.
    pub fn engine_thread_id(&self, cell: &str) -> Option<String> {
        self.engines
            .get(cell)
            .and_then(|se| se.adapter.engine_thread_id())
    }

    /// Honest resume for the cell: by thread id (codex) or session id (claude),
    /// chosen by the caller (SPEC §20.6). No-op if not active.
    pub fn resume_session(&mut self, cell: &str, session_id: &str) -> Result<(), BusError> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.resume_session(session_id),
            None => Ok(()),
        }
    }

    pub fn resume_thread(&mut self, cell: &str, thread_id: &str) -> Result<(), BusError> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.resume_thread(thread_id),
            None => Ok(()),
        }
    }

    /// Shutdown of the cell's adapter + audit `engine_stopped` (best-effort).
    /// No-op if the cell is not active.
    pub fn stop(&mut self, cell: &str) {
        if let Some(mut se) = self.engines.remove(cell) {
            se.adapter.shutdown();
            if let Ok(mut chain) = self.audit.lock() {
                let _ = chain.append(AuditEventDraft::new(
                    "engine_stopped",
                    None,
                    None,
                    Some(cell),
                    "stopped",
                    None,
                ));
            }
        }
    }

    /// Per-cell respawn backoff: `1,2,4,...,cap 60`. Each call increments the
    /// step and returns `min(60, 1 << step)`. Reuses the Phase 1 formula
    /// (`min(cap, base << (n-1))`, store.rs::record_attempt_failure) with base 1.
    pub fn respawn_backoff_secs(&mut self, cell: &str) -> u64 {
        let step = self.backoff_step.entry(cell.to_string()).or_insert(0);
        let exp = 1u64.checked_shl(*step).unwrap_or(u64::MAX);
        let delay = RESPAWN_BACKOFF_CAP_SECS.min(exp);
        *step = step.saturating_add(1);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crewd_core::audit::VerifyResult;
    use tempfile::tempdir;

    fn cfg() -> EngineSpawnCfg {
        EngineSpawnCfg {
            cwd: "/tmp".into(),
            timeout_secs: 1800,
            ..Default::default()
        }
    }

    /// Supervisor + chain on a tempdir (the TempDir lives as long as the test holds it).
    fn fresh() -> (EngineSupervisor, Arc<Mutex<AuditChain>>) {
        let dir = tempdir().unwrap();
        let chain = AuditChain::open(&dir.path().join("audit.jsonl")).unwrap();
        let chain = Arc::new(Mutex::new(chain));
        // Tie the tempdir's lifetime to the chain's: the file is opened/closed
        // on every append, so the directory must survive. We leak it
        // intentionally (short-lived test).
        std::mem::forget(dir);
        (EngineSupervisor::new(chain.clone()), chain)
    }

    #[test]
    fn respawn_backoff_grows_and_caps_at_60() {
        let (mut sup, _chain) = fresh();
        let seq: [u64; 8] = [
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
            sup.respawn_backoff_secs("c"),
        ];
        // 1,2,4,8,16,32,60,60
        assert_eq!(seq, [1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn backoff_is_per_cell_independent() {
        let (mut sup, _chain) = fresh();
        assert_eq!(sup.respawn_backoff_secs("a"), 1);
        assert_eq!(sup.respawn_backoff_secs("b"), 1); // different cell -> starts over
        assert_eq!(sup.respawn_backoff_secs("a"), 2);
    }

    #[test]
    fn stop_records_engine_stopped_in_audit_and_chain_verifies() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let chain = Arc::new(Mutex::new(AuditChain::open(&path).unwrap()));
        let mut sup = EngineSupervisor::new(chain);
        sup.ensure("cell-x", EngineKind::Fake, &cfg()).unwrap();
        sup.stop("cell-x");

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"kind\":\"engine_stopped\""),
            "missing engine_stopped event: {raw}"
        );
        assert!(raw.contains("cell-x"), "missing cell name in event: {raw}");
        match AuditChain::verify(&path) {
            VerifyResult::Ok { events, .. } => assert_eq!(events, 1),
            _ => panic!("audit chain must verify after engine_stopped"),
        }
    }
}
