//! `EngineSupervisor` (crewd Fase 2 Task 8): gestisce 1 adapter (= 1 processo
//! engine) per cella attiva. `ensure` e' idempotente (riusa l'adapter esistente);
//! `stop` fa shutdown + audit `engine_stopped`; `respawn_backoff_secs` ritorna il
//! backoff esponenziale capped (riusa la formula Fase 1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crewd_core::audit::{AuditChain, AuditEventDraft};
use crewd_core::cells::EngineKind;
use crewd_core::engine::EngineEvent;
use crewd_core::error::BusError;

use crate::engines::{make_adapter, EngineAdapter, EngineProcState, EngineSpawnCfg};

/// SPEC §20.9 clamp max per timeout; riusato come cap del backoff di respawn.
const RESPAWN_BACKOFF_CAP_SECS: u64 = 60;

struct SupervisedEngine {
    #[allow(dead_code)] // usato dai adapters reali per il respawn con kind corretto
    kind: EngineKind,
    adapter: Box<dyn EngineAdapter>,
}

/// Supervisor: una entry per cella attiva + stato backoff per cella.
pub struct EngineSupervisor {
    engines: HashMap<String, SupervisedEngine>,
    /// Step di respawn corrente per cella (incrementato a ogni `respawn_backoff_secs`).
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

    /// Idempotente: se la cella ha gia' un adapter attivo lo riusa (1 processo per
    /// cella, SPEC §20 v0); altrimenti lo crea via `make_adapter`.
    pub fn ensure(
        &mut self,
        cell: &str,
        kind: EngineKind,
        cfg: &EngineSpawnCfg,
    ) -> Result<&mut Box<dyn EngineAdapter>, BusError> {
        if !self.engines.contains_key(cell) {
            let adapter = make_adapter(kind, cfg)?;
            self.engines.insert(
                cell.to_string(),
                SupervisedEngine { kind, adapter },
            );
        }
        Ok(&mut self
            .engines
            .get_mut(cell)
            .expect("just inserted or pre-existing")
            .adapter)
    }

    /// Inietta un adapter pre-costruito per una cella (test override / bin_override
    /// di mock NDJSON): sara' riusato da `ensure`. Sostituisce un eventuale
    /// adapter preesistente per la cella.
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

    /// True se la cella ha un adapter attivo.
    pub fn has(&self, cell: &str) -> bool {
        self.engines.contains_key(cell)
    }

    /// Avvia un turno sull'adapter della cella (non-bloccante dopo Accepted).
    pub fn start_turn(&mut self, cell: &str, payload: &str) -> Result<(), BusError> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.start_turn(payload),
            None => Err(BusError::EngineDown(format!("no engine for cell {cell}"))),
        }
    }

    /// Drena gli eventi dell'engine della cella (non-bloccante). Vuoto se la
    /// cella non e' attiva.
    pub fn poll(&mut self, cell: &str) -> Vec<EngineEvent> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.poll_events(),
            None => Vec::new(),
        }
    }

    /// Stato del processo engine della cella (None se non attiva).
    pub fn proc_state(&self, cell: &str) -> Option<EngineProcState> {
        self.engines.get(cell).map(|se| se.adapter.proc_state())
    }

    /// Interrupt best-effort del turno in corso sulla cella (AUDIT2 M2).
    pub fn interrupt(&mut self, cell: &str) -> Result<(), BusError> {
        match self.engines.get_mut(cell) {
            Some(se) => se.adapter.interrupt(),
            None => Ok(()),
        }
    }

    /// Session id dell'engine (claude) per la cella, se noto.
    pub fn engine_session_id(&self, cell: &str) -> Option<String> {
        self.engines.get(cell).and_then(|se| se.adapter.engine_session_id())
    }

    /// Thread id dell'engine (codex) per la cella, se noto.
    pub fn engine_thread_id(&self, cell: &str) -> Option<String> {
        self.engines.get(cell).and_then(|se| se.adapter.engine_thread_id())
    }

    /// Resume onesto per la cella: per thread id (codex) o session id (claude),
    /// scelto dal chiamante (SPEC §20.6 / AUDIT2 M3). No-op se non attiva.
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

    /// Shutdown dell'adapter della cella + audit `engine_stopped` (best-effort).
    /// No-op se la cella non e' attiva.
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

    /// Backoff di respawn per cella: `1,2,4,...,cap 60`. Ogni chiamata incrementa
    /// lo step e ritorna `min(60, 1 << step)`. Riutilizza la formula Fase 1
    /// (`min(cap, base << (n-1))`, store.rs::record_attempt_failure) con base 1.
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

    /// Supervisor + chain su tempdir (il TempDir vive finche' il test lo tiene).
    fn fresh() -> (EngineSupervisor, Arc<Mutex<AuditChain>>) {
        let dir = tempdir().unwrap();
        let chain = AuditChain::open(&dir.path().join("audit.jsonl")).unwrap();
        let chain = Arc::new(Mutex::new(chain));
        // Leghiamo la vita del tempdir a quella della chain: il file viene aperto/
        // chiuso ad ogni append, quindi serve che la directory sopravviva. Lo
        // leak-iamo intenzionalmente (e' un test breve).
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
        assert_eq!(sup.respawn_backoff_secs("b"), 1); // cella diversa -> ricomincia
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
