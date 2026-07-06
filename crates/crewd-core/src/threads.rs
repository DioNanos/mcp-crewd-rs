//! Cell thread lifecycle (SPEC §20.2–20.3): `ThreadState` (7-state machine),
//! `CellThread`, the `cell_threads` table backed by `Store`, and the normative
//! `transition_allowed` matrix. Identity domains are separate and never
//! interchangeable: `crewd_thread_id` (UUIDv7, ours) vs `engine_process_id`
//! vs `engine_thread_id` vs `engine_turn_id` vs `engine_session_id`.
use crate::cells::EngineKind;
use crate::error::BusError;
use crate::store::Store;
use crate::types::now_rfc3339;
use rusqlite::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Spawning,
    Running,
    Idle,
    Interrupted,
    Timeout,
    FailedUnknown,
    Done,
}

impl ThreadState {
    /// Stable TEXT form stored in the `cell_threads.state` column; matches the
    /// serde `snake_case` form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Interrupted => "interrupted",
            Self::Timeout => "timeout",
            Self::FailedUnknown => "failed_unknown",
            Self::Done => "done",
        }
    }

    /// Inverse of `as_str`; `None` on an unknown discriminator.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "spawning" => Some(Self::Spawning),
            "running" => Some(Self::Running),
            "idle" => Some(Self::Idle),
            "interrupted" => Some(Self::Interrupted),
            "timeout" => Some(Self::Timeout),
            "failed_unknown" => Some(Self::FailedUnknown),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CellThread {
    pub crewd_thread_id: String,
    /// Per effimere: `~ephemeral-<uuid8>`.
    pub cell_name: String,
    pub engine_kind: EngineKind,
    /// Launch params (ephemeral cells carry them on the thread; named cells
    /// resolve them from the registry — here they mirror the launch values).
    pub model: Option<String>,
    pub profile: Option<String>,
    pub engine_process_id: Option<i64>,
    pub engine_thread_id: Option<String>,
    pub engine_turn_id: Option<String>,
    pub engine_session_id: Option<String>,
    pub cwd: String,
    pub worktree_path: Option<String>,
    pub state: ThreadState,
    pub generation: u32,
    pub created_by_principal: String,
    pub idempotency_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// SPEC §20.3 normative transition matrix. Allowed:
/// `spawning→running|failed_unknown`; `running→idle|interrupted|timeout|
/// failed_unknown|done`; any of `idle|interrupted|timeout|failed_unknown|done
/// →running` (only via explicit `cell_send_task`). No others (self-loops
/// included).
pub fn transition_allowed(from: ThreadState, to: ThreadState) -> bool {
    use ThreadState::*;
    matches!(
        (from, to),
        (Spawning, Running)
            | (Spawning, FailedUnknown)
            | (Running, Idle)
            | (Running, Interrupted)
            | (Running, Timeout)
            | (Running, FailedUnknown)
            | (Running, Done)
            | (Idle, Running)
            | (Interrupted, Running)
            | (Timeout, Running)
            | (FailedUnknown, Running)
            | (Done, Running)
    )
}

/// Columns of `cell_threads` in the order every SELECT below uses.
const THREAD_COLS: &str =
    "crewd_thread_id, cell_name, engine_kind, model, profile, engine_process_id,\
     engine_thread_id, engine_turn_id, engine_session_id, cwd, worktree_path, state,\
     generation, created_by_principal, idempotency_key, created_at, updated_at";

/// DB-typed mirror of `CellThread` (engine_kind/state still as TEXT): lets a row
/// be read without bespoke error boxing, then converted into `CellThread` once
/// the TEXT discriminators are validated.
#[derive(Debug, Clone)]
struct ThreadRaw {
    crewd_thread_id: String,
    cell_name: String,
    engine_kind: String,
    model: Option<String>,
    profile: Option<String>,
    engine_process_id: Option<i64>,
    engine_thread_id: Option<String>,
    engine_turn_id: Option<String>,
    engine_session_id: Option<String>,
    cwd: String,
    worktree_path: Option<String>,
    state: String,
    generation: i64,
    created_by_principal: String,
    idempotency_key: String,
    created_at: String,
    updated_at: String,
}

impl ThreadRaw {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(ThreadRaw {
            crewd_thread_id: row.get(0)?,
            cell_name: row.get(1)?,
            engine_kind: row.get(2)?,
            model: row.get(3)?,
            profile: row.get(4)?,
            engine_process_id: row.get(5)?,
            engine_thread_id: row.get(6)?,
            engine_turn_id: row.get(7)?,
            engine_session_id: row.get(8)?,
            cwd: row.get(9)?,
            worktree_path: row.get(10)?,
            state: row.get(11)?,
            generation: row.get(12)?,
            created_by_principal: row.get(13)?,
            idempotency_key: row.get(14)?,
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
        })
    }

    fn into_thread(self) -> Result<CellThread, BusError> {
        let engine_kind = EngineKind::from_db_str(&self.engine_kind).ok_or_else(|| {
            BusError::Internal(format!("unknown engine kind: {}", self.engine_kind))
        })?;
        let state = ThreadState::from_db_str(&self.state)
            .ok_or_else(|| BusError::Internal(format!("unknown thread state: {}", self.state)))?;
        Ok(CellThread {
            crewd_thread_id: self.crewd_thread_id,
            cell_name: self.cell_name,
            engine_kind,
            model: self.model,
            profile: self.profile,
            engine_process_id: self.engine_process_id,
            engine_thread_id: self.engine_thread_id,
            engine_turn_id: self.engine_turn_id,
            engine_session_id: self.engine_session_id,
            cwd: self.cwd,
            worktree_path: self.worktree_path,
            state,
            generation: self.generation as u32,
            created_by_principal: self.created_by_principal,
            idempotency_key: self.idempotency_key,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

impl Store {
    pub fn thread_insert(&self, t: &CellThread) -> Result<(), BusError> {
        self.0
            .execute(
                "INSERT INTO cell_threads (crewd_thread_id, cell_name, engine_kind, model, profile,\
                 engine_process_id, engine_thread_id, engine_turn_id, engine_session_id, cwd,\
                 worktree_path, state, generation, created_by_principal, idempotency_key,\
                 created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,\
                 ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    &t.crewd_thread_id,
                    &t.cell_name,
                    t.engine_kind.as_str(),
                    &t.model,
                    &t.profile,
                    t.engine_process_id,
                    &t.engine_thread_id,
                    &t.engine_turn_id,
                    &t.engine_session_id,
                    &t.cwd,
                    &t.worktree_path,
                    t.state.as_str(),
                    t.generation as i64,
                    &t.created_by_principal,
                    &t.idempotency_key,
                    &t.created_at,
                    &t.updated_at,
                ],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(())
    }

    pub fn thread_get(&self, id: &str) -> Result<Option<CellThread>, BusError> {
        let sql = format!("SELECT {THREAD_COLS} FROM cell_threads WHERE crewd_thread_id = ?1");
        let res = self.0.query_row(&sql, params![id], ThreadRaw::from_row);
        match res {
            Ok(raw) => Ok(Some(raw.into_thread()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BusError::Internal(e.to_string())),
        }
    }

    /// "Active" = `state IN ('spawning','running')` (engine occupation).
    pub fn thread_active_for_cell(&self, cell: &str) -> Result<Option<CellThread>, BusError> {
        let sql = format!(
            "SELECT {THREAD_COLS} FROM cell_threads \
             WHERE cell_name = ?1 AND state IN ('spawning','running') \
             ORDER BY updated_at DESC LIMIT 1"
        );
        let res = self.0.query_row(&sql, params![cell], ThreadRaw::from_row);
        match res {
            Ok(raw) => Ok(Some(raw.into_thread()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BusError::Internal(e.to_string())),
        }
    }

    pub fn thread_transition(&self, id: &str, to: ThreadState) -> Result<(), BusError> {
        let cur: String = match self.0.query_row(
            "SELECT state FROM cell_threads WHERE crewd_thread_id = ?1",
            params![id],
            |row| row.get(0),
        ) {
            Ok(s) => s,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(BusError::Internal(format!("thread not found: {id}")));
            }
            Err(e) => return Err(BusError::Internal(e.to_string())),
        };
        let from = ThreadState::from_db_str(&cur)
            .ok_or_else(|| BusError::Internal(format!("unknown thread state: {cur}")))?;
        if !transition_allowed(from, to) {
            return Err(BusError::Internal(format!(
                "invalid thread transition: {from:?} -> {to:?}"
            )));
        }
        let n = self
            .0
            .execute(
                "UPDATE cell_threads SET state = ?2, updated_at = ?3 WHERE crewd_thread_id = ?1",
                params![id, to.as_str(), now_rfc3339()],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("thread not found: {id}")));
        }
        Ok(())
    }

    /// Sets only the `Some` fields; `None` leaves the column intact (COALESCE).
    pub fn thread_set_engine_ids(
        &self,
        id: &str,
        process_id: Option<i64>,
        thread_id: Option<&str>,
        turn_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<(), BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_threads SET \
                   engine_process_id = COALESCE(?2, engine_process_id),\
                   engine_thread_id = COALESCE(?3, engine_thread_id),\
                   engine_turn_id = COALESCE(?4, engine_turn_id),\
                   engine_session_id = COALESCE(?5, engine_session_id),\
                   updated_at = ?6 \
                 WHERE crewd_thread_id = ?1",
                params![
                    id,
                    process_id,
                    thread_id,
                    turn_id,
                    session_id,
                    now_rfc3339()
                ],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("thread not found: {id}")));
        }
        Ok(())
    }

    /// Boot recovery (chaos §20 test b): list every thread whose
    /// stored state is one of `states` (TEXT form). Used at daemon boot to find
    /// orphaned `running`/`spawning` threads left by a crash.
    pub fn threads_in_states(&self, states: &[ThreadState]) -> Result<Vec<CellThread>, BusError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = states
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT {THREAD_COLS} FROM cell_threads WHERE state IN ({placeholders}) \
             ORDER BY created_at ASC"
        );
        let mut stmt = self
            .0
            .prepare(&sql)
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let strs: Vec<&'static str> = states.iter().map(|s| s.as_str()).collect();
        let params: Vec<&dyn rusqlite::ToSql> =
            strs.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(params.as_slice(), ThreadRaw::from_row)
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let raw = r.map_err(|e| BusError::Internal(e.to_string()))?;
            out.push(raw.into_thread()?);
        }
        Ok(out)
    }

    pub fn thread_bump_generation(&self, id: &str) -> Result<u32, BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_threads SET generation = generation + 1, updated_at = ?2\
                 WHERE crewd_thread_id = ?1",
                params![id, now_rfc3339()],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("thread not found: {id}")));
        }
        let g: i64 = self
            .0
            .query_row(
                "SELECT generation FROM cell_threads WHERE crewd_thread_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(g as u32)
    }
}

/// Fabric scheduler discovery (smoke-T16 fix): distinct NON-registry cells
/// (`~ephemeral-*` or unregistered) that have at least one `queued` job —
/// reconstructed as `CellDef` from the launch params persisted on the thread.
/// Registry cells are excluded: the scheduler already iterates the registry.
impl Store {
    pub fn ephemeral_startable_defs(&self) -> Result<Vec<crate::cells::CellDef>, BusError> {
        let sql = format!(
            "SELECT {THREAD_COLS} FROM cell_threads t WHERE t.cell_name NOT IN (SELECT name FROM cells) \
             AND EXISTS (SELECT 1 FROM cell_jobs j WHERE j.cell_name = t.cell_name AND j.state = 'queued') \
             GROUP BY t.cell_name HAVING MAX(t.updated_at)"
        );
        let mut stmt = self
            .0
            .prepare(&sql)
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let rows: Result<Vec<ThreadRaw>, _> = stmt
            .query_map([], ThreadRaw::from_row)
            .map_err(|e| BusError::Internal(e.to_string()))?
            .collect();
        let raws = rows.map_err(|e| BusError::Internal(e.to_string()))?;
        let mut defs = Vec::with_capacity(raws.len());
        for raw in raws {
            let t = raw.into_thread()?;
            defs.push(crate::cells::CellDef {
                name: t.cell_name,
                engine: t.engine_kind,
                model: t.model,
                profile: t.profile,
                cwd: t.cwd,
                worktree_default: false,
                memory_device: None,
                created_at: t.created_at,
            });
        }
        Ok(defs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{new_uuidv7, now_rfc3339};

    fn sample_thread(state: ThreadState) -> CellThread {
        CellThread {
            crewd_thread_id: new_uuidv7(),
            cell_name: "glm-worker-a".into(),
            engine_kind: EngineKind::Claude,
            model: None,
            profile: None,
            engine_process_id: None,
            engine_thread_id: None,
            engine_turn_id: None,
            engine_session_id: None,
            cwd: "/tmp/w".into(),
            worktree_path: None,
            state,
            generation: 0,
            created_by_principal: "operator".into(),
            idempotency_key: "k1".into(),
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        }
    }

    #[test]
    fn thread_state_machine_normative() {
        use ThreadState::*;
        let ok = [
            (Spawning, Running),
            (Spawning, FailedUnknown),
            (Running, Idle),
            (Running, Interrupted),
            (Running, Timeout),
            (Running, FailedUnknown),
            (Running, Done),
            (Idle, Running),
            (Interrupted, Running),
            (Timeout, Running),
            (FailedUnknown, Running),
            (Done, Running),
        ];
        for (f, t) in ok {
            assert!(transition_allowed(f, t), "{f:?}->{t:?} must be allowed");
        }
        let no = [
            (Spawning, Idle),
            (Spawning, Done),
            (Idle, Done),
            (Done, Idle),
            (Timeout, Done),
            (Idle, Idle),
        ];
        for (f, t) in no {
            assert!(!transition_allowed(f, t), "{f:?}->{t:?} must be rejected");
        }
    }

    #[test]
    fn thread_store_roundtrip_ids_separated() {
        let s = Store::open_in_memory().unwrap();
        let t = sample_thread(ThreadState::Spawning);
        s.thread_insert(&t).unwrap();
        s.thread_transition(&t.crewd_thread_id, ThreadState::Running)
            .unwrap();
        assert!(s
            .thread_transition(&t.crewd_thread_id, ThreadState::Spawning)
            .is_err());
        s.thread_set_engine_ids(
            &t.crewd_thread_id,
            Some(4242),
            Some("eng-th-1"),
            Some("eng-turn-1"),
            Some("eng-sess-1"),
        )
        .unwrap();
        let got = s.thread_get(&t.crewd_thread_id).unwrap().unwrap();
        // identity domains are separate
        assert_ne!(got.crewd_thread_id, got.engine_thread_id.clone().unwrap());
        assert_eq!(got.engine_session_id.as_deref(), Some("eng-sess-1"));
        assert_eq!(s.thread_bump_generation(&got.crewd_thread_id).unwrap(), 1);
    }

    #[test]
    fn thread_active_for_cell_only_spawning_running() {
        let s = Store::open_in_memory().unwrap();
        // idle thread for the cell is NOT active
        let idle = sample_thread(ThreadState::Idle);
        s.thread_insert(&idle).unwrap();
        assert!(s.thread_active_for_cell("glm-worker-a").unwrap().is_none());
        // a running thread is active
        let run = sample_thread(ThreadState::Running);
        s.thread_insert(&run).unwrap();
        let act = s.thread_active_for_cell("glm-worker-a").unwrap().unwrap();
        assert_eq!(act.state, ThreadState::Running);
        // a done thread on another query is not returned as active
        let done = sample_thread(ThreadState::Done);
        s.thread_insert(&done).unwrap();
        let still = s.thread_active_for_cell("glm-worker-a").unwrap().unwrap();
        assert_eq!(still.state, ThreadState::Running);
    }

    #[test]
    fn thread_set_engine_ids_partial_leaves_others_intact() {
        let s = Store::open_in_memory().unwrap();
        let t = sample_thread(ThreadState::Running);
        s.thread_insert(&t).unwrap();
        // set only process_id and session_id; thread/turn stay None
        s.thread_set_engine_ids(&t.crewd_thread_id, Some(99), None, None, Some("sess-x"))
            .unwrap();
        let got = s.thread_get(&t.crewd_thread_id).unwrap().unwrap();
        assert_eq!(got.engine_process_id, Some(99));
        assert_eq!(got.engine_session_id.as_deref(), Some("sess-x"));
        assert!(got.engine_thread_id.is_none());
        assert!(got.engine_turn_id.is_none());
    }

    #[test]
    fn thread_get_missing_returns_none() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.thread_get("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn threads_in_states_selects_running_and_spawning() {
        let s = Store::open_in_memory().unwrap();
        let mut running = sample_thread(ThreadState::Running);
        running.cell_name = "c-run".into();
        let mut spawning = sample_thread(ThreadState::Spawning);
        spawning.cell_name = "c-spawn".into();
        let mut idle = sample_thread(ThreadState::Idle);
        idle.cell_name = "c-idle".into();
        s.thread_insert(&running).unwrap();
        s.thread_insert(&spawning).unwrap();
        s.thread_insert(&idle).unwrap();
        let orphans = s
            .threads_in_states(&[ThreadState::Running, ThreadState::Spawning])
            .unwrap();
        assert_eq!(orphans.len(), 2);
        assert!(orphans.iter().all(|t| t.state != ThreadState::Idle));
        assert!(s.threads_in_states(&[]).unwrap().is_empty());
    }
}
