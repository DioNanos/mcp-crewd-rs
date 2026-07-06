//! Job scheduler + per-turn timeout (crewd Phase 2 Task 9).
//!
//! `tick` is synchronous and driven by a dedicated daemon thread (see
//! `run_loop`). The adapters are now **non-blocking after Accepted**:
//! the scheduler starts a turn, then drains events across ticks via
//! `poll_events`, so it can persist per-turn timeouts, observe engine process
//! death, and honour explicit cancels while a turn is genuinely in flight.
//!
//! Per tick, in order:
//! 1. **Cancels**: interrupt + kill the engine of each cancelled
//!    cell and drop its in-flight turn (the handler already persisted state).
//! 2. **Requeue** leases expired pre-acceptance (`job_requeue_expired_leases`).
//! 3. **Drain** events for in-flight turns (accepted/note/final/failed).
//! 4. **Death + timeout** for turns still in flight from a previous tick:
//!    engine `Down` or `now-started > timeout` → honest terminal outcome.
//! 5. **Start** new turns for cells with a queued job.
//! 6. **Drain** again, so an in-process fake turn can complete within one tick.
//!
//! Job terminal outcome: an **accepted** turn that fails or times
//! out marks its job `finished` (never requeued, cell freed for follow-up); a
//! **non-accepted** failure re-queues the job (redelivery boundary, SPEC §20.5).
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crewd_core::audit::{AuditChain, AuditEventDraft};
use crewd_core::cells::{CellDef, EngineKind};
use crewd_core::engine::EngineEvent;
use crewd_core::error::BusError;
use crewd_core::store::Store;
use crewd_core::threads::{CellThread, ThreadState};

use crate::engines::{EngineAdapter, EngineSpawnCfg};
use crate::supervisor::EngineSupervisor;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Test/chaos hook: run the native `crewd-fake-engine` (claude protocol) as the
/// engine for every cell, so a real daemon can drive a killable turn. Populated
/// from `CREWD_FAKE_ENGINE_BIN` / `CREWD_FAKE_ENGINE_ARGS`; unset in production.
#[derive(Debug, Clone)]
struct FakeEngineOverride {
    bin: String,
    args: Vec<String>,
}

struct InFlightTurn {
    job_id: String,
    cell: String,
    started_at_nanos: u64,
    engine_turn_id: Option<String>,
    accepted: bool,
}

pub struct Scheduler {
    store: Arc<Mutex<Store>>,
    audit: Arc<Mutex<AuditChain>>,
    supervisor: EngineSupervisor,
    lease_secs: u64,
    timeout_secs: u64,
    in_flight: HashMap<String, InFlightTurn>,
    cancel_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    fake_engine: Option<FakeEngineOverride>,
    /// Keys file forwarded to engine adapters for non-`max` profiles (from
    /// `CrewdConfig::keys_env_path`); `None` → such profiles fail
    /// `E_ENGINE_DOWN` at spawn.
    keys_env_path: Option<String>,
    /// Declared engine profiles (from `CrewdConfig::profiles`), resolved into
    /// `EngineSpawnCfg.profile_def` at spawn. Empty by default.
    profiles: HashMap<String, crate::config::ProfileDef>,
}

impl Scheduler {
    pub fn new(store: Arc<Mutex<Store>>, audit: Arc<Mutex<AuditChain>>) -> Self {
        let supervisor = EngineSupervisor::new(audit.clone());
        let fake_engine = std::env::var("CREWD_FAKE_ENGINE_BIN").ok().map(|bin| {
            let raw = std::env::var("CREWD_FAKE_ENGINE_ARGS").unwrap_or_default();
            let args = if raw.is_empty() {
                Vec::new()
            } else {
                raw.split(',').map(|s| s.to_string()).collect()
            };
            FakeEngineOverride { bin, args }
        });
        Self {
            store,
            audit,
            supervisor,
            lease_secs: 30,
            timeout_secs: 1800,
            in_flight: HashMap::new(),
            cancel_rx: None,
            fake_engine,
            keys_env_path: None,
            profiles: HashMap::new(),
        }
    }

    /// Wire the keys file path (`CrewdConfig::keys_env_path`) forwarded to
    /// engine adapters for non-`max` profiles. Without it, such cells fail
    /// `E_ENGINE_DOWN` at spawn.
    pub fn with_keys_env_path(mut self, path: Option<String>) -> Self {
        self.keys_env_path = path;
        self
    }

    /// Wire the declared engine profiles (`CrewdConfig::profiles`). Each is
    /// resolved into `EngineSpawnCfg.profile_def` for the matching cell profile
    /// at spawn; a profile with no matching entry fails `E_ENGINE_DOWN`.
    pub fn with_profiles(mut self, profiles: HashMap<String, crate::config::ProfileDef>) -> Self {
        self.profiles = profiles;
        self
    }

    /// Test observability: whether the cell currently has a live
    /// engine adapter. A cancel interrupts + stops it, so this flips to false.
    pub fn engine_active(&self, cell: &str) -> bool {
        self.supervisor.has(cell)
    }

    /// Per-turn timeout (SPEC §20.9: default 1800, clamp 7200).
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs.clamp(0, 7200);
        self
    }

    /// Wire the cancel control channel: the handler sends the cell
    /// name of a cancelled thread and the scheduler interrupts + kills the
    /// engine on the next tick.
    pub fn with_cancel_channel(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<String>) -> Self {
        self.cancel_rx = Some(rx);
        self
    }

    /// Inject an adapter for a cell (test/bin_override): reused by `ensure`,
    /// enabling configurable FakeEngines.
    pub fn inject_adapter(&mut self, cell: &str, adapter: Box<dyn EngineAdapter>) {
        self.supervisor.insert(cell, adapter);
    }

    /// Test/chaos: route respawns through the native `crewd-fake-engine`
    /// (claude protocol) with `args`, mirroring the `CREWD_FAKE_ENGINE_*` env
    /// hook without touching process-global env.
    pub fn with_fake_engine(mut self, bin: &str, args: &[&str]) -> Self {
        self.fake_engine = Some(FakeEngineOverride {
            bin: bin.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        });
        self
    }

    pub fn tick(&mut self) -> Result<(), BusError> {
        let now = crewd_core::types::now_rfc3339();
        let now_nanos = system_now_nanos();

        // 1. Cancels.
        self.drain_cancels()?;

        // 2. Requeue leases expired pre-acceptance.
        {
            let store = lock_store(&self.store)?;
            store.job_requeue_expired_leases(&now)?;
        }

        // 3. Drain events for existing in-flight turns.
        let existing: Vec<String> = self.in_flight.keys().cloned().collect();
        for tid in &existing {
            self.drain_turn(tid)?;
        }

        // 4. Death + timeout for turns still in flight from a previous tick.
        let still: Vec<String> = self.in_flight.keys().cloned().collect();
        for tid in still {
            self.check_death_and_timeout(&tid, now_nanos)?;
        }

        // 5. Start new turns. Registry cells + ephemeral cells reconstructed
        // from thread launch params (smoke-T16 fix: ephemeral threads are NOT
        // in the registry and would otherwise never be scheduled).
        let cells: Vec<CellDef> = {
            let store = lock_store(&self.store)?;
            let mut defs = store.cell_list_defs()?;
            defs.extend(store.ephemeral_startable_defs()?);
            defs
        };
        for def in &cells {
            if self.in_flight.values().any(|t| t.cell == def.name) {
                continue;
            }
            self.try_start_turn(def, now_nanos)?;
        }

        // 6. Drain again so an in-process fake turn completes within one tick.
        let fresh: Vec<String> = self.in_flight.keys().cloned().collect();
        for tid in &fresh {
            if existing.contains(tid) {
                continue; // already drained in step 3
            }
            self.drain_turn(tid)?;
        }
        Ok(())
    }

    fn drain_cancels(&mut self) -> Result<(), BusError> {
        let mut cells: Vec<String> = Vec::new();
        if let Some(rx) = self.cancel_rx.as_mut() {
            while let Ok(cell) = rx.try_recv() {
                cells.push(cell);
            }
        }
        for cell in cells {
            let _ = self.supervisor.interrupt(&cell);
            self.supervisor.stop(&cell); // kill-tree + engine_stopped audit
            self.in_flight.retain(|_, t| t.cell != cell);
        }
        Ok(())
    }

    /// Poll and process the engine events for one in-flight thread.
    fn drain_turn(&mut self, thread_id: &str) -> Result<(), BusError> {
        let cell = match self.in_flight.get(thread_id) {
            Some(t) => t.cell.clone(),
            None => return Ok(()),
        };
        let events = self.supervisor.poll(&cell);
        for ev in events {
            match ev {
                EngineEvent::Accepted { engine_turn_id } => {
                    let job_id = self.in_flight.get(thread_id).map(|t| t.job_id.clone());
                    if let Some(job_id) = job_id {
                        let store = lock_store(&self.store)?;
                        store.job_mark_accepted(&job_id, &engine_turn_id)?;
                        store.thread_set_engine_ids(
                            thread_id,
                            None,
                            None,
                            Some(&engine_turn_id),
                            None,
                        )?;
                        store.journal_append(thread_id, &format!("accepted: {engine_turn_id}"))?;
                    }
                    if let Some(t) = self.in_flight.get_mut(thread_id) {
                        t.engine_turn_id = Some(engine_turn_id);
                        t.accepted = true;
                    }
                }
                EngineEvent::Note { text } => {
                    let store = lock_store(&self.store)?;
                    store.journal_append(thread_id, &text)?;
                }
                EngineEvent::Final { final_answer } => {
                    let job_id = self.in_flight.get(thread_id).map(|t| t.job_id.clone());
                    self.persist_engine_ids(thread_id, &cell)?;
                    {
                        let store = lock_store(&self.store)?;
                        store.journal_append(thread_id, &format!("final: {final_answer}"))?;
                        if let Some(job_id) = &job_id {
                            store.job_finish(job_id)?;
                        }
                        store.thread_transition(thread_id, ThreadState::Idle)?;
                    }
                    audit_append(
                        &self.audit,
                        "cell_turn_completed",
                        Some(thread_id),
                        None,
                        Some(&cell),
                        "completed",
                        None,
                    )?;
                    self.in_flight.remove(thread_id);
                    return Ok(());
                }
                EngineEvent::Failed { error } => {
                    let (job_id, accepted) = self
                        .in_flight
                        .get(thread_id)
                        .map(|t| (t.job_id.clone(), t.accepted))
                        .unwrap_or((String::new(), false));
                    self.fail_turn(thread_id, &cell, &job_id, accepted, &error, "engine_failed")?;
                    self.in_flight.remove(thread_id);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Engine death (process `Down`) or per-turn timeout for a still-in-flight
    /// turn. Death → honest terminal outcome + respawn cleanup; timeout →
    /// persist engine_turn_id/outcome BEFORE interrupt (SPEC §20.9).
    fn check_death_and_timeout(&mut self, thread_id: &str, now_nanos: u64) -> Result<(), BusError> {
        let Some(turn) = self.in_flight.get(thread_id) else {
            return Ok(());
        };
        let cell = turn.cell.clone();
        let accepted = turn.accepted;
        let started = turn.started_at_nanos;
        let job_id = turn.job_id.clone();

        // Engine process death (kill -9 of the engine): fail the turn honestly.
        if self.supervisor.proc_state(&cell) == Some(crate::engines::EngineProcState::Down) {
            self.fail_turn(
                thread_id,
                &cell,
                &job_id,
                accepted,
                "engine process died",
                "engine_death",
            )?;
            self.in_flight.remove(thread_id);
            self.supervisor.stop(&cell); // audit engine_stopped + drop dead adapter
            let _ = self.supervisor.respawn_backoff_secs(&cell); // advance backoff
            return Ok(());
        }

        // Per-turn timeout.
        let elapsed = now_nanos.saturating_sub(started);
        if elapsed > self.timeout_secs.saturating_mul(NANOS_PER_SEC) {
            self.handle_timeout(thread_id)?;
        }
        Ok(())
    }

    fn handle_timeout(&mut self, thread_id: &str) -> Result<(), BusError> {
        let Some(turn) = self.in_flight.remove(thread_id) else {
            return Ok(());
        };
        // PERSIST engine_turn_id + outcome BEFORE interrupt (SPEC §20.9).
        {
            let store = lock_store(&self.store)?;
            if let Some(etid) = &turn.engine_turn_id {
                store.thread_set_engine_ids(thread_id, None, None, Some(etid), None)?;
            }
            store.thread_transition(thread_id, ThreadState::Timeout)?;
            // Free the cell: accepted → finished; else requeue.
            if turn.accepted {
                store.job_finish(&turn.job_id)?;
            } else {
                let _ = store.job_requeue(&turn.job_id);
            }
        }
        // Interrupt best-effort (outcome already persisted; errors ignored).
        let _ = self.supervisor.interrupt(&turn.cell);
        audit_append(
            &self.audit,
            "cell_timeout",
            Some(thread_id),
            None,
            Some(&turn.cell),
            "timeout",
            turn.engine_turn_id
                .as_ref()
                .map(|t| serde_json::json!({"engine_turn_id": t})),
        )?;
        Ok(())
    }

    fn try_start_turn(&mut self, def: &CellDef, now_nanos: u64) -> Result<(), BusError> {
        let job = {
            let store = lock_store(&self.store)?;
            match store.job_lease_next(&def.name, self.lease_secs)? {
                Some(j) => j,
                None => return Ok(()),
            }
        };
        let thread_id = job.crewd_thread_id.clone();
        let job_id = job.job_id.clone();
        let payload = job.payload.clone();

        let thread = {
            let store = lock_store(&self.store)?;
            store.thread_get(&thread_id)?
        };
        let Some(thread) = thread else { return Ok(()) };
        if !crewd_core::threads::transition_allowed(thread.state, ThreadState::Running) {
            return Ok(()); // leased job retries on next tick / lease expiry
        }

        {
            let store = lock_store(&self.store)?;
            store.job_mark_started(&job_id)?;
            store.thread_transition(&thread_id, ThreadState::Running)?;
        }

        // Audit cell_turn_started (fsync) BEFORE start_turn.
        audit_append(
            &self.audit,
            "cell_turn_started",
            Some(&thread_id),
            None,
            Some(&def.name),
            "started",
            None,
        )?;

        let cfg = self.spawn_cfg(def);
        let engine = def.engine;
        match self.supervisor.ensure(&def.name, engine, &cfg) {
            Ok(_) => {}
            Err(e) => {
                self.fail_turn(
                    &thread_id,
                    &def.name,
                    &job_id,
                    false,
                    &e.to_string(),
                    "engine_spawn",
                )?;
                return Ok(());
            }
        }

        // Honest resume (SPEC §20.6 / M3): reattach a follow-up on materialized
        // history when this thread carries a prior engine id from a dead process.
        self.maybe_resume(def, &thread)?;

        if let Err(e) = self.supervisor.start_turn(&def.name, &payload) {
            self.fail_turn(
                &thread_id,
                &def.name,
                &job_id,
                false,
                &e.to_string(),
                "start_turn",
            )?;
            return Ok(());
        }

        self.in_flight.insert(
            thread_id.clone(),
            InFlightTurn {
                job_id,
                cell: def.name.clone(),
                started_at_nanos: now_nanos,
                engine_turn_id: None,
                accepted: false,
            },
        );
        Ok(())
    }

    /// Typed honest resume: codex reattaches by `engine_thread_id`,
    /// session-resume engines (claude) by `engine_session_id`. An id domain is
    /// never forwarded into the wrong field.
    fn maybe_resume(&mut self, def: &CellDef, thread: &CellThread) -> Result<(), BusError> {
        if def.engine == EngineKind::Codex {
            if let Some(tid) = &thread.engine_thread_id {
                let _ = self.supervisor.resume_thread(&def.name, tid);
            }
        } else if let Some(sid) = &thread.engine_session_id {
            let _ = self.supervisor.resume_session(&def.name, sid);
        }
        Ok(())
    }

    /// Persist the engine's own ids (session/thread) from the live adapter into
    /// the thread record, keeping the id domains distinct (SPEC §20.2).
    fn persist_engine_ids(&mut self, thread_id: &str, cell: &str) -> Result<(), BusError> {
        let sid = self.supervisor.engine_session_id(cell);
        let etid = self.supervisor.engine_thread_id(cell);
        if sid.is_some() || etid.is_some() {
            let store = lock_store(&self.store)?;
            store.thread_set_engine_ids(thread_id, None, etid.as_deref(), None, sid.as_deref())?;
        }
        Ok(())
    }

    /// `accepted` = an Accepted arrived before the failure. Accepted job →
    /// `finished` (never requeued, cell freed); non-accepted →
    /// requeued (redelivery boundary, SPEC §20.5). Thread → failed_unknown +
    /// audit cell_turn_failed.
    #[allow(clippy::too_many_arguments)]
    fn fail_turn(
        &mut self,
        thread_id: &str,
        cell: &str,
        job_id: &str,
        accepted: bool,
        error: &str,
        reason: &str,
    ) -> Result<(), BusError> {
        {
            let store = lock_store(&self.store)?;
            if !job_id.is_empty() {
                if accepted {
                    store.job_finish(job_id)?;
                } else if reason == "engine_spawn" {
                    // Pre-process spawn failure (E_ENGINE_DOWN: bad/missing
                    // config — e.g. a zai profile without keys_env_path) is NOT
                    // a transient redelivery case: an immediate requeue
                    // hot-loops the ephemeral cell every tick (audit.jsonl/WAL
                    // blowup observed in the 2026-07-05 smoke). Terminal
                    // instead — the honest failure lives in the thread state +
                    // the cell_turn_failed audit. Bounded retry-with-backoff
                    // (wiring max_attempts/backoff_* into the scheduler) is a
                    // Phase 3 follow-up.
                    store.job_finish(job_id)?;
                } else {
                    let _ = store.job_requeue(job_id);
                }
            }
            store.thread_transition(thread_id, ThreadState::FailedUnknown)?;
        }
        audit_append(
            &self.audit,
            "cell_turn_failed",
            Some(thread_id),
            None,
            Some(cell),
            "failed",
            Some(serde_json::json!({ "accepted": accepted, "error": error, "reason": reason })),
        )?;
        Ok(())
    }

    fn spawn_cfg(&self, def: &CellDef) -> EngineSpawnCfg {
        if let Some(fe) = &self.fake_engine {
            return EngineSpawnCfg {
                cwd: def.cwd.clone(),
                model: def.model.clone(),
                profile: def.profile.clone(),
                timeout_secs: self.timeout_secs,
                bin_override: Some(fe.bin.clone()),
                spawn_direct: true,
                shim_args: fe.args.clone(),
                ..Default::default()
            };
        }
        EngineSpawnCfg {
            cwd: def.cwd.clone(),
            model: def.model.clone(),
            profile: def.profile.clone(),
            timeout_secs: self.timeout_secs,
            keys_env_path: self.keys_env_path.clone(),
            profile_def: def
                .profile
                .as_deref()
                .and_then(|p| self.profiles.get(p).cloned()),
            ..Default::default()
        }
    }
}

/// Boot recovery (chaos §20 test b): after a crash, threads left in
/// `running` are orphaned (their engine died with the daemon) and are marked
/// `interrupted`; threads left in `spawning` never ran and become
/// `failed_unknown`. Each is audited `cell_turn_failed` (reason `boot_recovery`)
/// so the hash-chain records the honest outcome. Returns recovered thread ids.
pub fn boot_recovery(store: &Store, audit: &mut AuditChain) -> Result<Vec<String>, BusError> {
    let mut recovered = Vec::new();
    for t in store.threads_in_states(&[ThreadState::Running])? {
        audit
            .append(AuditEventDraft::new(
                "cell_turn_failed",
                Some(&t.crewd_thread_id),
                None,
                Some(&t.cell_name),
                "interrupted",
                Some(serde_json::json!({"reason": "boot_recovery", "previous_state": "running"})),
            ))
            .map_err(|e| BusError::Internal(format!("audit boot_recovery: {e}")))?;
        store.thread_transition(&t.crewd_thread_id, ThreadState::Interrupted)?;
        recovered.push(t.crewd_thread_id);
    }
    for t in store.threads_in_states(&[ThreadState::Spawning])? {
        audit
            .append(AuditEventDraft::new(
                "cell_turn_failed",
                Some(&t.crewd_thread_id),
                None,
                Some(&t.cell_name),
                "failed_unknown",
                Some(serde_json::json!({"reason": "boot_recovery", "previous_state": "spawning"})),
            ))
            .map_err(|e| BusError::Internal(format!("audit boot_recovery: {e}")))?;
        store.thread_transition(&t.crewd_thread_id, ThreadState::FailedUnknown)?;
        recovered.push(t.crewd_thread_id);
    }
    Ok(recovered)
}

/// Run the scheduler loop on the current (dedicated) thread until `stop` is set.
/// Ticks every `interval`; a tick error is logged and the loop continues.
pub fn run_loop(mut scheduler: Scheduler, interval: Duration, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        if let Err(e) = scheduler.tick() {
            eprintln!("scheduler tick error: {e:?}");
        }
        std::thread::sleep(interval);
    }
}

fn lock_store(store: &Arc<Mutex<Store>>) -> Result<std::sync::MutexGuard<'_, Store>, BusError> {
    store.lock().map_err(|e| BusError::Internal(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn audit_append(
    audit: &Arc<Mutex<AuditChain>>,
    kind: &str,
    message_id: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    outcome: &str,
    detail: Option<serde_json::Value>,
) -> Result<(), BusError> {
    let mut chain = audit
        .lock()
        .map_err(|e| BusError::Internal(e.to_string()))?;
    chain
        .append(AuditEventDraft::new(
            kind, message_id, from, to, outcome, detail,
        ))
        .map_err(|e| BusError::Internal(e.to_string()))?;
    Ok(())
}

fn system_now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crewd_core::audit::AuditChain;
    use crewd_core::cells::{CellDef, EngineKind};
    use crewd_core::store::Store;
    use crewd_core::types::now_rfc3339;
    use std::sync::{Arc, Mutex};

    fn fixture() -> Scheduler {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let dir = tempfile::tempdir().unwrap();
        let audit = Arc::new(Mutex::new(
            AuditChain::open(&dir.path().join("audit.jsonl")).unwrap(),
        ));
        std::mem::forget(dir); // the file must exist for the whole test
        Scheduler::new(store, audit)
    }

    fn profiled_def() -> CellDef {
        CellDef {
            name: "acme".into(),
            engine: EngineKind::Claude,
            model: None,
            profile: Some("acme".into()),
            cwd: "/tmp".into(),
            worktree_default: false,
            memory_device: None,
            created_at: now_rfc3339(),
        }
    }

    #[test]
    fn spawn_cfg_carries_keys_env_path_from_builder() {
        let sched = fixture().with_keys_env_path(Some("/etc/crewd/keys.env".into()));
        let cfg = sched.spawn_cfg(&profiled_def());
        assert_eq!(
            cfg.keys_env_path.as_deref(),
            Some("/etc/crewd/keys.env"),
            "the scheduler must forward keys_env_path into the EngineSpawnCfg"
        );
        assert_eq!(cfg.profile.as_deref(), Some("acme"));
    }

    #[test]
    fn spawn_cfg_keys_env_path_defaults_to_none() {
        let sched = fixture();
        let cfg = sched.spawn_cfg(&profiled_def());
        assert_eq!(cfg.keys_env_path, None);
    }

    #[test]
    fn spawn_cfg_resolves_profile_def_from_profiles_map() {
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "acme".to_string(),
            crate::config::ProfileDef {
                base_url: "https://example/anthropic".into(),
                model: "some-model".into(),
                api_key_env: "ACME_KEY".into(),
            },
        );
        let sched = fixture().with_profiles(profiles);
        let cfg = sched.spawn_cfg(&profiled_def());
        let def = cfg
            .profile_def
            .as_ref()
            .expect("profile_def must be resolved from the profiles map");
        assert_eq!(def.base_url, "https://example/anthropic");
        assert_eq!(def.model, "some-model");
        assert_eq!(def.api_key_env, "ACME_KEY");
    }

    #[test]
    fn spawn_cfg_profile_def_none_when_not_declared() {
        // No profiles wired: a non-`max` profile resolves to no profile_def,
        // which the adapter turns into a clear "unknown claude profile" error.
        let sched = fixture();
        let cfg = sched.spawn_cfg(&profiled_def());
        assert!(cfg.profile_def.is_none());
    }
}
