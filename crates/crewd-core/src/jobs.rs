//! Persistent per-cell FIFO job queue with leasing (SPEC §20.5). Queue depth
//! cap (default 8) → `E_QUEUE_FULL` (counts queued+leased+started). Head-of-line
//! blocking: no lease while a started job exists for the cell. The
//! `accepted_by_engine_at` timestamp is the redelivery boundary: a leased job
//! whose lease expires **before** engine acceptance is re-queued; once
//! accepted, a job is **never** auto-retried (SPEC §20.5).
use crate::error::BusError;
use crate::store::Store;
use crate::types::{new_uuidv7, now_rfc3339, rfc3339_after};
use rusqlite::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Leased,
    Started,
    Finished,
    Cancelled,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Started => "started",
            Self::Finished => "finished",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "leased" => Some(Self::Leased),
            "started" => Some(Self::Started),
            "finished" => Some(Self::Finished),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CellJob {
    pub job_id: String,
    pub crewd_thread_id: String,
    pub cell_name: String,
    pub payload: String,
    pub state: JobState,
    pub lease_expires_at: Option<String>,
    pub accepted_by_engine_at: Option<String>,
    pub engine_turn_id: Option<String>,
    pub created_at: String,
    /// Per-cell monotonic enqueue order (SPEC §20.5 FIFO tie-breaker; assigned
    /// under BEGIN IMMEDIATE so it is deterministic even within one second).
    pub enqueue_seq: u64,
}

/// SPEC §20.5: default queue depth cap per named cell.
pub const QUEUE_DEPTH_DEFAULT: u32 = 8;

/// Columns of `cell_jobs` in the order every SELECT below uses.
const JOB_COLS: &str = "job_id, crewd_thread_id, cell_name, payload, state, lease_expires_at,\
     accepted_by_engine_at, engine_turn_id, created_at, enqueue_seq";

/// DB-typed mirror of `CellJob` (`state` still TEXT), converted once the state
/// discriminator is validated. See `threads::ThreadRaw` for the same pattern.
#[derive(Debug, Clone)]
struct JobRaw {
    job_id: String,
    crewd_thread_id: String,
    cell_name: String,
    payload: String,
    state: String,
    lease_expires_at: Option<String>,
    accepted_by_engine_at: Option<String>,
    engine_turn_id: Option<String>,
    created_at: String,
    enqueue_seq: i64,
}

impl JobRaw {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(JobRaw {
            job_id: row.get(0)?,
            crewd_thread_id: row.get(1)?,
            cell_name: row.get(2)?,
            payload: row.get(3)?,
            state: row.get(4)?,
            lease_expires_at: row.get(5)?,
            accepted_by_engine_at: row.get(6)?,
            engine_turn_id: row.get(7)?,
            created_at: row.get(8)?,
            enqueue_seq: row.get(9)?,
        })
    }

    fn into_job(self) -> Result<CellJob, BusError> {
        let state = JobState::from_db_str(&self.state)
            .ok_or_else(|| BusError::Internal(format!("unknown job state: {}", self.state)))?;
        Ok(CellJob {
            job_id: self.job_id,
            crewd_thread_id: self.crewd_thread_id,
            cell_name: self.cell_name,
            payload: self.payload,
            state,
            lease_expires_at: self.lease_expires_at,
            accepted_by_engine_at: self.accepted_by_engine_at,
            engine_turn_id: self.engine_turn_id,
            created_at: self.created_at,
            enqueue_seq: self.enqueue_seq as u64,
        })
    }
}

impl Store {
    pub fn job_enqueue(
        &self,
        thread_id: &str,
        cell: &str,
        payload: &str,
        depth_cap: u32,
    ) -> Result<CellJob, BusError> {
        let tx = immediate_tx(&self.0)?;
        let job = self.job_enqueue_inner(thread_id, cell, payload, depth_cap)?;
        tx.commit()?;
        Ok(job)
    }

    /// Enqueue logic without opening its own transaction, so it can run inside
    /// a caller's `BEGIN IMMEDIATE` (e.g. `spawn_idempotent`). When called
    /// standalone, the depth cap and the `enqueue_seq` assignment are still
    /// correct under autocommit, but the caller should wrap in `BEGIN
    /// IMMEDIATE` (`job_enqueue`) for concurrent determinism (SPEC §20.5).
    pub(crate) fn job_enqueue_inner(
        &self,
        thread_id: &str,
        cell: &str,
        payload: &str,
        depth_cap: u32,
    ) -> Result<CellJob, BusError> {
        let count: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM cell_jobs WHERE cell_name = ?1 AND state IN ('queued','leased','started')",
                params![cell],
                |row| row.get(0),
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if count >= depth_cap as i64 {
            return Err(BusError::QueueFull(format!(
                "queue depth cap reached for cell {cell}"
            )));
        }
        // Per-cell monotonic FIFO sequence (SPEC §20.5: the FIFO tie-breaker is
        // this counter, not the second-granularity created_at). Deterministic
        // when the caller holds a BEGIN IMMEDIATE write lock.
        let enqueue_seq: i64 = self
            .0
            .query_row(
                "SELECT COALESCE(MAX(enqueue_seq), 0) + 1 FROM cell_jobs WHERE cell_name = ?1",
                params![cell],
                |row| row.get(0),
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let job = CellJob {
            job_id: new_uuidv7(),
            crewd_thread_id: thread_id.into(),
            cell_name: cell.into(),
            payload: payload.into(),
            state: JobState::Queued,
            lease_expires_at: None,
            accepted_by_engine_at: None,
            engine_turn_id: None,
            created_at: now_rfc3339(),
            enqueue_seq: enqueue_seq as u64,
        };
        self.0
            .execute(
                "INSERT INTO cell_jobs (job_id, crewd_thread_id, cell_name, payload, state, lease_expires_at, accepted_by_engine_at, engine_turn_id, created_at, enqueue_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    &job.job_id,
                    &job.crewd_thread_id,
                    &job.cell_name,
                    &job.payload,
                    job.state.as_str(),
                    &job.lease_expires_at,
                    &job.accepted_by_engine_at,
                    &job.engine_turn_id,
                    &job.created_at,
                    enqueue_seq,
                ],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(job)
    }

    /// FIFO per `created_at`, head-of-line: returns `None` if any `started` job
    /// exists for the cell. Leasing runs inside `BEGIN IMMEDIATE` so the
    /// per-cell write lock is acquired before reading the candidate (SPEC §20.5).
    pub fn job_lease_next(&self, cell: &str, lease_secs: u64) -> Result<Option<CellJob>, BusError> {
        let tx = immediate_tx(&self.0)?;
        let started: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM cell_jobs WHERE cell_name = ?1 AND state = 'started'",
                params![cell],
                |row| row.get(0),
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if started > 0 {
            // head-of-line: a started job occupies the cell. tx drops → rollback.
            return Ok(None);
        }
        let candidate: Option<String> = match self.0.query_row(
            "SELECT job_id FROM cell_jobs WHERE cell_name = ?1 AND state = 'queued' ORDER BY enqueue_seq ASC LIMIT 1",
            params![cell],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => Some(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(BusError::Internal(e.to_string())),
        };
        let Some(job_id) = candidate else {
            return Ok(None);
        };
        let expires = rfc3339_after(lease_secs);
        self.0
            .execute(
                "UPDATE cell_jobs SET state = 'leased', lease_expires_at = ?2 WHERE job_id = ?1",
                params![&job_id, &expires],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        tx.commit()?;
        let sql = format!("SELECT {JOB_COLS} FROM cell_jobs WHERE job_id = ?1");
        let raw = self
            .0
            .query_row(&sql, params![&job_id], JobRaw::from_row)
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(Some(raw.into_job()?))
    }

    pub fn job_mark_started(&self, job_id: &str) -> Result<(), BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_jobs SET state = 'started' WHERE job_id = ?1",
                params![job_id],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("job not found: {job_id}")));
        }
        Ok(())
    }

    pub fn job_mark_accepted(&self, job_id: &str, engine_turn_id: &str) -> Result<(), BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_jobs SET accepted_by_engine_at = ?2, engine_turn_id = ?3 WHERE job_id = ?1",
                params![job_id, now_rfc3339(), engine_turn_id],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("job not found: {job_id}")));
        }
        Ok(())
    }

    pub fn job_finish(&self, job_id: &str) -> Result<(), BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_jobs SET state = 'finished' WHERE job_id = ?1",
                params![job_id],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("job not found: {job_id}")));
        }
        Ok(())
    }

    pub fn job_cancel(&self, job_id: &str) -> Result<(), BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_jobs SET state = 'cancelled' WHERE job_id = ?1",
                params![job_id],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(BusError::Internal(format!("job not found: {job_id}")));
        }
        Ok(())
    }

    /// SPEC §20.5 redelivery boundary (explicit path): re-queue a specific job
    /// **only if it was never accepted** (`accepted_by_engine_at IS NULL`). The
    /// scheduler calls this when a non-accepted turn fails or its engine dies,
    /// so the cell's head-of-line frees immediately instead of waiting for the
    /// lease to expire. An accepted job matches 0 rows here — the "never
    /// auto-retry after acceptance" invariant is enforced in SQL, not by the
    /// caller.
    pub fn job_requeue(&self, job_id: &str) -> Result<bool, BusError> {
        let n = self
            .0
            .execute(
                "UPDATE cell_jobs SET state = 'queued', lease_expires_at = NULL \
                 WHERE job_id = ?1 AND accepted_by_engine_at IS NULL \
                 AND state IN ('leased','started')",
                params![job_id],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(n > 0)
    }

    /// SPEC §20.5 redelivery boundary: re-queue jobs whose lease expired
    /// **before** engine acceptance. `accepted_by_engine_at` non-NULL (or a
    /// terminal state) is never re-queued — agentic double-turns are damage.
    pub fn job_requeue_expired_leases(&self, now: &str) -> Result<Vec<String>, BusError> {
        let tx = immediate_tx(&self.0)?;
        let mut stmt = self
            .0
            .prepare(
                "SELECT job_id FROM cell_jobs WHERE lease_expires_at IS NOT NULL AND lease_expires_at < ?1 AND accepted_by_engine_at IS NULL AND state IN ('leased','started')",
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![now], |row| row.get::<_, String>(0))
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let mut ids: Vec<String> = Vec::new();
        for r in rows {
            // fail-closed: a corrupt row surfaces as BusError, never a silent skip.
            ids.push(r.map_err(|e| BusError::Internal(e.to_string()))?);
        }
        drop(stmt);
        for id in &ids {
            self.0
                .execute(
                    "UPDATE cell_jobs SET state = 'queued', lease_expires_at = NULL WHERE job_id = ?1",
                    params![id],
                )
                .map_err(|e| BusError::Internal(e.to_string()))?;
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn jobs_for_thread(&self, thread_id: &str) -> Result<Vec<CellJob>, BusError> {
        let sql = format!(
            "SELECT {JOB_COLS} FROM cell_jobs WHERE crewd_thread_id = ?1 ORDER BY enqueue_seq ASC"
        );
        let mut stmt = self
            .0
            .prepare(&sql)
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![thread_id], JobRaw::from_row)
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let raw = r.map_err(|e| BusError::Internal(e.to_string()))?;
            out.push(raw.into_job()?);
        }
        Ok(out)
    }

    /// SPEC §20.4 fabric: count of active (`queued|leased|started`) jobs for
    /// `cell` — the queue depth surfaced in the `cell_list` fabric section.
    /// Read-only.
    pub fn job_active_count_for_cell(&self, cell: &str) -> Result<u32, BusError> {
        let count: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM cell_jobs WHERE cell_name = ?1 \
                 AND state IN ('queued','leased','started')",
                params![cell],
                |r| r.get(0),
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(count as u32)
    }
}

/// `BEGIN IMMEDIATE` with RAII rollback. rusqlite 0.32 only exposes
/// `transaction_with_behavior` through `&mut self`, but the `Store` methods are
/// `&self`, so the per-cell write lock is driven manually on the shared
/// connection (SPEC §20.5 / §20.7: BEGIN IMMEDIATE, no volatile mutex).
pub(crate) fn immediate_tx(conn: &rusqlite::Connection) -> Result<ImmediateTx<'_>, BusError> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| BusError::Internal(e.to_string()))?;
    Ok(ImmediateTx {
        conn,
        committed: false,
    })
}

pub(crate) struct ImmediateTx<'a> {
    conn: &'a rusqlite::Connection,
    committed: bool,
}

impl ImmediateTx<'_> {
    pub(crate) fn commit(mut self) -> Result<(), BusError> {
        self.conn
            .execute_batch("COMMIT")
            .map_err(|e| BusError::Internal(e.to_string()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for ImmediateTx<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::EngineKind;
    use crate::threads::{CellThread, ThreadState};
    use crate::types::{new_uuidv7, now_rfc3339};
    use rusqlite::params;

    /// Inserts a minimal thread for `cell` and returns its `crewd_thread_id`,
    /// so jobs can reference a real thread row.
    fn insert_thread(s: &Store, cell: &str) -> String {
        let id = new_uuidv7();
        let t = CellThread {
            crewd_thread_id: id.clone(),
            cell_name: cell.into(),
            engine_kind: EngineKind::Claude,
            model: None,
            profile: None,
            engine_process_id: None,
            engine_thread_id: None,
            engine_turn_id: None,
            engine_session_id: None,
            cwd: "/tmp/w".into(),
            worktree_path: None,
            state: ThreadState::Spawning,
            generation: 0,
            created_by_principal: "operator".into(),
            idempotency_key: new_uuidv7(),
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        };
        s.thread_insert(&t).unwrap();
        id
    }

    #[test]
    fn fifo_leasing_and_redelivery_boundary() {
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        let j1 = s.job_enqueue(&tid, "c1", "task-1", 8).unwrap();
        let _j2 = s.job_enqueue(&tid, "c1", "task-2", 8).unwrap();
        let l = s.job_lease_next("c1", 30).unwrap().unwrap();
        assert_eq!(l.job_id, j1.job_id); // FIFO: oldest first
        s.job_mark_started(&l.job_id).unwrap();
        assert!(s.job_lease_next("c1", 30).unwrap().is_none()); // head-of-line

        // Boundary 1 — expired lease, pre-acceptance → re-queue.
        s.0.execute(
            "UPDATE cell_jobs SET lease_expires_at = '2020-01-01T00:00:00Z' WHERE job_id = ?1",
            params![&l.job_id],
        )
        .unwrap();
        let re = s.job_requeue_expired_leases(&now_rfc3339()).unwrap();
        assert_eq!(re, vec![l.job_id.clone()]);
        let after = s.jobs_for_thread(&tid).unwrap();
        let j1b = after.iter().find(|j| j.job_id == l.job_id).unwrap();
        assert_eq!(j1b.state, JobState::Queued);
        assert!(j1b.lease_expires_at.is_none());

        // re-lease, start, accept
        let l2 = s.job_lease_next("c1", 30).unwrap().unwrap();
        assert_eq!(l2.job_id, j1.job_id);
        s.job_mark_started(&l2.job_id).unwrap();
        s.job_mark_accepted(&l2.job_id, "turn-9").unwrap();

        // Boundary 2 — accepted job is NEVER re-queued, even with an expired lease.
        s.0.execute(
            "UPDATE cell_jobs SET lease_expires_at = '2020-01-01T00:00:00Z' WHERE job_id = ?1",
            params![&l2.job_id],
        )
        .unwrap();
        let re2 = s.job_requeue_expired_leases(&now_rfc3339()).unwrap();
        assert!(re2.is_empty());
    }

    #[test]
    fn job_requeue_respects_acceptance_boundary() {
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        let j = s.job_enqueue(&tid, "c1", "p", 8).unwrap();
        s.job_lease_next("c1", 30).unwrap();
        s.job_mark_started(&j.job_id).unwrap();
        // pre-acceptance: requeue allowed
        assert!(s.job_requeue(&j.job_id).unwrap());
        let after = s.jobs_for_thread(&tid).unwrap();
        assert_eq!(after[0].state, JobState::Queued);
        // re-lease + accept, then requeue must be refused (never after acceptance)
        s.job_lease_next("c1", 30).unwrap();
        s.job_mark_started(&j.job_id).unwrap();
        s.job_mark_accepted(&j.job_id, "turn-1").unwrap();
        assert!(!s.job_requeue(&j.job_id).unwrap());
        let after2 = s.jobs_for_thread(&tid).unwrap();
        assert_ne!(after2[0].state, JobState::Queued);
    }

    #[test]
    fn queue_depth_cap() {
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        for i in 0..8 {
            s.job_enqueue(&tid, "c1", &format!("t{i}"), 8).unwrap();
        }
        let e = s.job_enqueue(&tid, "c1", "t9", 8);
        assert!(matches!(e, Err(x) if x.code() == "E_QUEUE_FULL"));
    }

    #[test]
    fn job_finish_and_cancel_transitions() {
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        let j = s.job_enqueue(&tid, "c1", "p", 8).unwrap();
        s.job_finish(&j.job_id).unwrap();
        let got = s.jobs_for_thread(&tid).unwrap();
        assert_eq!(
            got.iter().find(|x| x.job_id == j.job_id).unwrap().state,
            JobState::Finished
        );
        let j2 = s.job_enqueue(&tid, "c1", "p2", 8).unwrap();
        s.job_cancel(&j2.job_id).unwrap();
        let got2 = s.jobs_for_thread(&tid).unwrap();
        assert_eq!(
            got2.iter().find(|x| x.job_id == j2.job_id).unwrap().state,
            JobState::Cancelled
        );
    }

    #[test]
    fn lease_next_skips_cancelled_and_finished() {
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        let j1 = s.job_enqueue(&tid, "c1", "p1", 8).unwrap();
        s.job_cancel(&j1.job_id).unwrap();
        let j2 = s.job_enqueue(&tid, "c1", "p2", 8).unwrap();
        s.job_finish(&j2.job_id).unwrap();
        let j3 = s.job_enqueue(&tid, "c1", "p3", 8).unwrap();
        let l = s.job_lease_next("c1", 30).unwrap().unwrap();
        assert_eq!(l.job_id, j3.job_id);
    }

    #[test]
    fn mark_accepted_sets_engine_turn_and_timestamp() {
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        let j = s.job_enqueue(&tid, "c1", "p", 8).unwrap();
        s.job_lease_next("c1", 30).unwrap();
        s.job_mark_started(&j.job_id).unwrap();
        s.job_mark_accepted(&j.job_id, "turn-42").unwrap();
        let got = s.jobs_for_thread(&tid).unwrap();
        let job = got.iter().find(|x| x.job_id == j.job_id).unwrap();
        assert!(job.accepted_by_engine_at.is_some());
        assert_eq!(job.engine_turn_id.as_deref(), Some("turn-42"));
    }

    #[test]
    fn fifo_deterministic_on_same_second_uses_enqueue_seq() {
        // created_at is second-granular; two jobs in the same second
        // must still lease in enqueue order via the per-cell enqueue_seq.
        let s = Store::open_in_memory().unwrap();
        let tid = insert_thread(&s, "c1");
        let j1 = s.job_enqueue(&tid, "c1", "a", 8).unwrap();
        let j2 = s.job_enqueue(&tid, "c1", "b", 8).unwrap();
        let j3 = s.job_enqueue(&tid, "c1", "c", 8).unwrap();
        assert!((j1.enqueue_seq, j2.enqueue_seq, j3.enqueue_seq) == (1, 2, 3));
        // defeat any created_at ordering: force all rows to the same timestamp
        s.0.execute(
            "UPDATE cell_jobs SET created_at = '2026-01-01T00:00:00Z' WHERE cell_name = 'c1'",
            params![],
        )
        .unwrap();
        let l1 = s.job_lease_next("c1", 30).unwrap().unwrap();
        assert_eq!(l1.job_id, j1.job_id);
        s.job_finish(&l1.job_id).unwrap();
        let l2 = s.job_lease_next("c1", 30).unwrap().unwrap();
        assert_eq!(l2.job_id, j2.job_id);
        s.job_finish(&l2.job_id).unwrap();
        let l3 = s.job_lease_next("c1", 30).unwrap().unwrap();
        assert_eq!(l3.job_id, j3.job_id);
    }
}
