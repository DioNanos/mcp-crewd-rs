//! SQLite store: envelopes, deliveries (§7.1 state machine), asks (single
//! reply), submission dedupe, per-pair seq counters.
use crate::error::BusError;
use crate::state::{transition_allowed, DeliveryState};
use crate::types::{now_rfc3339, Envelope, Kind, MsgType};
use rand::Rng;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Result<T> = std::result::Result<T, BusError>;

pub struct Store(pub(crate) Connection);

#[derive(Debug, Clone)]
pub struct DedupeHit {
    pub message_id: String,
    pub ask_id: Option<String>,
    pub body_sha256: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AskRow {
    pub ask_id: String,
    pub message_id: String,
    pub from_cell: String,
    pub to_cell: String,
    pub expires_at: String,
    pub state: String,
    pub reply_message_id: Option<String>,
}

/// Outcome of `Store::record_reply` (SPEC §6.3 / §5.4).
pub enum ReplyOutcome {
    /// First well-formed reply recorded.
    Recorded,
    /// A reply with identical content is already on record.
    DuplicateIdentical { message_id: String },
    /// A differing reply is already recorded — caller maps this to E_REPLY_EXISTS.
    Conflict,
}

fn db_err(e: rusqlite::Error) -> BusError {
    BusError::Internal(e.to_string())
}

fn body_sha256(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn parse_kind(s: &str) -> Kind {
    match s {
        "ask" => Kind::Ask,
        "reply" => Kind::Reply,
        "broadcast" => Kind::Broadcast,
        "notice" => Kind::Notice,
        _ => Kind::Send,
    }
}

fn parse_msg_type(s: &str) -> MsgType {
    match s {
        "note" => MsgType::Note,
        "task" => MsgType::Task,
        "ask" => MsgType::Ask,
        "reply" => MsgType::Reply,
        "evidence" => MsgType::Evidence,
        "admin_request" => MsgType::AdminRequest,
        _ => MsgType::Task,
    }
}

/// Read an envelope from the 16 envelope columns in canonical order
/// (message_id, spec_version, ask_id, from_cell, to_cell, kind, msg_type,
/// principal_capabilities, created_at, expires_at, seq, idempotency_key, body,
/// file_refs, taint, reply_to), starting at column index 0.
fn envelope_from_row(row: &rusqlite::Row) -> rusqlite::Result<Envelope> {
    let kind_s: String = row.get(5)?;
    let mt_s: String = row.get(6)?;
    let caps: String = row.get(7)?;
    let refs: String = row.get(13)?;
    let seq_i: i64 = row.get(10)?;
    Ok(Envelope {
        message_id: row.get(0)?,
        spec_version: row.get(1)?,
        ask_id: row.get(2)?,
        from_cell: row.get(3)?,
        to_cell: row.get(4)?,
        kind: parse_kind(&kind_s),
        msg_type: parse_msg_type(&mt_s),
        principal_capabilities: serde_json::from_str(&caps).unwrap_or_default(),
        created_at: row.get(8)?,
        expires_at: row.get(9)?,
        seq: seq_i as u64,
        idempotency_key: row.get(11)?,
        body: row.get(12)?,
        file_refs: serde_json::from_str(&refs).unwrap_or_default(),
        taint: row.get(14)?,
        reply_to: row.get(15)?,
    })
}

/// Envelope columns list, reused across SELECTs to fix column order/index.
const ENV_COLS: &str = "e.message_id,e.spec_version,e.ask_id,e.from_cell,e.to_cell,\
     e.kind,e.msg_type,e.principal_capabilities,e.created_at,e.expires_at,e.seq,\
     e.idempotency_key,e.body,e.file_refs,e.taint,e.reply_to";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(db_err)?;
        // SPEC §17.3 / H-09: the DB file MUST be 0600 inside the 0700 runtime
        // dir. `.mode()` on create is not enough (an existing DB keeps its
        // prior perms), so chmod explicitly. WAL keeps the audit-vs-delivery
        // ordering consistent across a crash; NORMAL sync is the durability
        // policy paired with the audit chain's own per-event fsync (§12.3).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(path) {
                let mut perms = md.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        // SPEC §20.5 concurrency: let SQLite block (not error) when a second
        // writer hits the IMMEDIATE lock, so journal/FIFO/queue writers
        // serialize cleanly across file-backed multi-connection stores.
        conn.pragma_update(None, "busy_timeout", "5000")
            .map_err(db_err)?;
        let s = Store(conn);
        s.create_schema()?;
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        let s = Store(conn);
        s.create_schema()?;
        Ok(s)
    }

    fn create_schema(&self) -> Result<()> {
        self.0
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS envelopes(\
                   message_id TEXT PRIMARY KEY,\
                   spec_version TEXT NOT NULL,\
                   ask_id TEXT,\
                   from_cell TEXT NOT NULL,\
                   to_cell TEXT,\
                   kind TEXT NOT NULL,\
                   msg_type TEXT NOT NULL,\
                   principal_capabilities TEXT NOT NULL,\
                   created_at TEXT NOT NULL,\
                   expires_at TEXT NOT NULL,\
                   seq INTEGER NOT NULL,\
                   idempotency_key TEXT NOT NULL,\
                   body TEXT NOT NULL,\
                   file_refs TEXT NOT NULL,\
                   taint TEXT NOT NULL,\
                   reply_to TEXT);\
                 CREATE TABLE IF NOT EXISTS deliveries(\
                   envelope_id TEXT NOT NULL,\
                   recipient TEXT NOT NULL,\
                   state TEXT NOT NULL,\
                   attempts INTEGER NOT NULL DEFAULT 0,\
                   lease_until INTEGER,\
                   next_retry_at INTEGER NOT NULL DEFAULT 0,\
                   PRIMARY KEY(envelope_id, recipient));\
                 CREATE TABLE IF NOT EXISTS asks(\
                   ask_id TEXT PRIMARY KEY,\
                   message_id TEXT NOT NULL,\
                   from_cell TEXT NOT NULL,\
                   to_cell TEXT NOT NULL,\
                   expires_at TEXT NOT NULL,\
                   state TEXT NOT NULL CHECK(state IN ('pending','answered','expired')),\
                   reply_message_id TEXT,\
                   reply_body_sha256 TEXT);\
                 CREATE TABLE IF NOT EXISTS dedupe(\
                   from_cell TEXT NOT NULL,\
                   idempotency_key TEXT NOT NULL,\
                   message_id TEXT NOT NULL,\
                   ask_id TEXT,\
                   body_sha256 TEXT NOT NULL,\
                   created_at TEXT NOT NULL,\
                   PRIMARY KEY(from_cell, idempotency_key));\
                 CREATE TABLE IF NOT EXISTS seq_counters(\
                   from_cell TEXT NOT NULL,\
                   to_cell TEXT NOT NULL,\
                   next_seq INTEGER NOT NULL,\
                   PRIMARY KEY(from_cell, to_cell));\
                 CREATE TABLE IF NOT EXISTS cells(\
                   name TEXT PRIMARY KEY,\
                   engine TEXT NOT NULL,\
                   model TEXT,\
                   profile TEXT,\
                   cwd TEXT NOT NULL,\
                   worktree_default INTEGER NOT NULL,\
                   memory_device TEXT,\
                   created_at TEXT NOT NULL);\
                 CREATE TABLE IF NOT EXISTS cell_threads(\
                   crewd_thread_id TEXT PRIMARY KEY,\
                   cell_name TEXT NOT NULL,\
                   engine_kind TEXT NOT NULL,\
                   model TEXT,\
                   profile TEXT,\
                   engine_process_id INTEGER,\
                   engine_thread_id TEXT,\
                   engine_turn_id TEXT,\
                   engine_session_id TEXT,\
                   cwd TEXT NOT NULL,\
                   worktree_path TEXT,\
                   state TEXT NOT NULL,\
                   generation INTEGER NOT NULL,\
                   created_by_principal TEXT NOT NULL,\
                   idempotency_key TEXT NOT NULL,\
                   created_at TEXT NOT NULL,\
                   updated_at TEXT NOT NULL);\
                 CREATE INDEX IF NOT EXISTS idx_cell_threads_cell_state ON cell_threads(cell_name, state);\
                 CREATE TABLE IF NOT EXISTS cell_jobs(\
                   job_id TEXT PRIMARY KEY,\
                   crewd_thread_id TEXT NOT NULL,\
                   cell_name TEXT NOT NULL,\
                   payload TEXT NOT NULL,\
                   state TEXT NOT NULL,\
                   lease_expires_at TEXT,\
                   accepted_by_engine_at TEXT,\
                   engine_turn_id TEXT,\
                   created_at TEXT NOT NULL,\
                   enqueue_seq INTEGER NOT NULL);\
                 CREATE INDEX IF NOT EXISTS idx_cell_jobs_cell_seq ON cell_jobs(cell_name, enqueue_seq);\
                 CREATE TABLE IF NOT EXISTS spawn_requests(\
                   caller TEXT NOT NULL,\
                   cell_name TEXT NOT NULL,\
                   idempotency_key TEXT NOT NULL,\
                   crewd_thread_id TEXT NOT NULL,\
                   created_at TEXT NOT NULL,\
                   UNIQUE(caller, cell_name, idempotency_key));\
                 CREATE TABLE IF NOT EXISTS cell_locks(cell_name TEXT PRIMARY KEY);\
                 CREATE TABLE IF NOT EXISTS worktrees(\
                   canonical_path TEXT PRIMARY KEY,\
                   created_by_thread TEXT NOT NULL,\
                   state TEXT NOT NULL,\
                   created_at TEXT NOT NULL);\
                 CREATE TABLE IF NOT EXISTS thread_journal(\
                   thread_id TEXT NOT NULL,\
                   seq INTEGER NOT NULL,\
                   line TEXT NOT NULL,\
                   at TEXT NOT NULL,\
                   PRIMARY KEY(thread_id, seq));",
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Transactional per-pair monotonic counter, starting at 1.
    pub fn next_seq(&self, from: &str, to: &str) -> Result<u64> {
        let tx = self.0.unchecked_transaction().map_err(db_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO seq_counters(from_cell,to_cell,next_seq) VALUES(?1,?2,1)",
            params![from, to],
        )
        .map_err(db_err)?;
        let seq_i: i64 = tx
            .query_row(
                "SELECT next_seq FROM seq_counters WHERE from_cell=?1 AND to_cell=?2",
                params![from, to],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?;
        tx.execute(
            "UPDATE seq_counters SET next_seq=next_seq+1 WHERE from_cell=?1 AND to_cell=?2",
            params![from, to],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(seq_i as u64)
    }

    pub fn insert_envelope(&self, env: &Envelope) -> Result<()> {
        self.0
            .execute(
                "INSERT OR REPLACE INTO envelopes(message_id,spec_version,ask_id,from_cell,\
                 to_cell,kind,msg_type,principal_capabilities,created_at,expires_at,seq,\
                 idempotency_key,body,file_refs,taint,reply_to)\
                 VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    env.message_id,
                    env.spec_version,
                    env.ask_id,
                    env.from_cell,
                    env.to_cell,
                    env.kind.as_str(),
                    env.msg_type.as_str(),
                    serde_json::to_string(&env.principal_capabilities).unwrap(),
                    env.created_at,
                    env.expires_at,
                    env.seq,
                    env.idempotency_key,
                    env.body,
                    serde_json::to_string(&env.file_refs).unwrap(),
                    env.taint,
                    env.reply_to,
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Fetch one envelope by `message_id` (e.g. the full reply envelope for
    /// `cell_await`, SPEC §5.3).
    pub fn get_envelope(&self, message_id: &str) -> Result<Option<Envelope>> {
        let sql = format!("SELECT {ENV_COLS} FROM envelopes e WHERE e.message_id=?1");
        let mut stmt = self.0.prepare(&sql).map_err(db_err)?;
        let mut rows = stmt
            .query_map(params![message_id], envelope_from_row)
            .map_err(db_err)?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(db_err)?)),
            None => Ok(None),
        }
    }

    pub fn insert_delivery(&self, message_id: &str, recipient: &str) -> Result<()> {
        self.0
            .execute(
                "INSERT OR REPLACE INTO deliveries(envelope_id,recipient,state,attempts,\
                 lease_until,next_retry_at) VALUES(?,?,'queued',0,NULL,0)",
                params![message_id, recipient],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Count of non-terminal (pending) deliveries for `recipient`, for the
    /// queue-depth overflow check (SPEC §14).
    pub fn pending_deliveries_count(&self, recipient: &str) -> Result<u32> {
        let n: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM deliveries WHERE recipient=?1 \
                 AND state IN ('queued','claimed')",
                params![recipient],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        Ok(n as u32)
    }

    /// Total non-terminal deliveries across all recipients (for `op_status`).
    pub fn pending_deliveries_total(&self) -> Result<u32> {
        let n: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM deliveries WHERE state IN ('queued','claimed')",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        Ok(n as u32)
    }

    /// Total open (pending) asks across all requesters (for `op_status`).
    pub fn open_asks_total(&self) -> Result<u32> {
        let n: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM asks WHERE state='pending'",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        Ok(n as u32)
    }

    pub fn delivery_state(&self, message_id: &str, recipient: &str) -> Result<DeliveryState> {
        let s: String = self
            .0
            .query_row(
                "SELECT state FROM deliveries WHERE envelope_id=?1 AND recipient=?2",
                params![message_id, recipient],
                |r| r.get::<_, String>(0),
            )
            .map_err(db_err)?;
        DeliveryState::parse(&s).ok_or_else(|| BusError::Internal(format!("bad state {s}")))
    }

    /// Apply a §7.1 transition; illegal edges → E_INTERNAL.
    pub fn transition(
        &self,
        message_id: &str,
        recipient: &str,
        to: DeliveryState,
    ) -> Result<()> {
        let from_str: String = self
            .0
            .query_row(
                "SELECT state FROM deliveries WHERE envelope_id=?1 AND recipient=?2",
                params![message_id, recipient],
                |r| r.get::<_, String>(0),
            )
            .map_err(db_err)?;
        let from = DeliveryState::parse(&from_str)
            .ok_or_else(|| BusError::Internal(format!("bad state {from_str}")))?;
        if !transition_allowed(from, to) {
            return Err(BusError::Internal(format!(
                "illegal transition {:?}->{:?}",
                from, to
            )));
        }
        self.0
            .execute(
                "UPDATE deliveries SET state=?3 WHERE envelope_id=?1 AND recipient=?2",
                params![message_id, recipient, to.as_str()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Release claimed leases that elapsed, then claim due `queued` deliveries
    /// up to `max`. Returns (envelope, recipient) pairs now in `claimed`.
    pub fn claim_due(
        &self,
        now_unix: i64,
        lease_secs: i64,
        max: usize,
    ) -> Result<Vec<(Envelope, String)>> {
        // 1. release claimed whose lease has elapsed -> queued (at-least-once)
        self.0
            .execute(
                "UPDATE deliveries SET state='queued', lease_until=NULL \
                 WHERE state='claimed' AND lease_until IS NOT NULL AND lease_until <= ?1",
                params![now_unix],
            )
            .map_err(db_err)?;
        // 2. select queued due (next_retry_at <= now)
        let sql = format!(
            "SELECT {ENV_COLS}, d.recipient FROM deliveries d \
             JOIN envelopes e ON e.message_id=d.envelope_id \
             WHERE d.state='queued' AND d.next_retry_at <= ?1 \
             ORDER BY d.rowid LIMIT ?2"
        );
        let mut stmt = self.0.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(params![now_unix, max as i64], |row| {
                let env = envelope_from_row(row)?;
                let recipient: String = row.get(16)?;
                Ok((env, recipient))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            let (env, recipient) = r.map_err(db_err)?;
            self.0
                .execute(
                    "UPDATE deliveries SET state='claimed', lease_until=?3 \
                     WHERE envelope_id=?1 AND recipient=?2",
                    params![env.message_id, recipient, now_unix + lease_secs],
                )
                .map_err(db_err)?;
            out.push((env, recipient));
        }
        Ok(out)
    }

    /// Record a retryable delivery failure; increments attempts, applies backoff
    /// `min(cap, base*2^(attempts-1)) + jitter(0..1s)` and re-queues, or marks
    /// `failed` once attempts >= max_attempts. Returns true when exhausted.
    pub fn record_attempt_failure(
        &self,
        message_id: &str,
        recipient: &str,
        max_attempts: u32,
        backoff_base_secs: u64,
        backoff_cap_secs: u64,
    ) -> Result<bool> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let attempts_i: i64 = self
            .0
            .query_row(
                "SELECT attempts FROM deliveries WHERE envelope_id=?1 AND recipient=?2",
                params![message_id, recipient],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?;
        let new_attempts = (attempts_i as u32) + 1;
        if new_attempts >= max_attempts {
            self.0
                .execute(
                    "UPDATE deliveries SET state='failed', attempts=?3 \
                     WHERE envelope_id=?1 AND recipient=?2",
                    params![message_id, recipient, new_attempts],
                )
                .map_err(db_err)?;
            Ok(true)
        } else {
            let exp = backoff_base_secs
                .checked_shl((new_attempts - 1) as u32)
                .unwrap_or(u64::MAX);
            let delay = backoff_cap_secs.min(exp);
            let jitter = rand::thread_rng().gen_range(0..=1i64);
            let next = now + delay as i64 + jitter;
            self.0
                .execute(
                    "UPDATE deliveries SET state='queued', attempts=?3, lease_until=NULL, \
                     next_retry_at=?4 WHERE envelope_id=?1 AND recipient=?2",
                    params![message_id, recipient, new_attempts, next],
                )
                .map_err(db_err)?;
            Ok(false)
        }
    }

    /// Expire non-terminal deliveries whose envelope TTL has elapsed.
    /// Per §7.1 only `queued`/`claimed` may transition to `expired`; a delivery
    /// already `delivered` stays delivered (the handoff happened; consumer
    /// dedupes). Returns (message_id, recipient) pairs expired.
    pub fn expire_due(&self, now_rfc3339: &str) -> Result<Vec<(String, String)>> {
        let sql = "SELECT d.envelope_id, d.recipient, d.state FROM deliveries d \
                   JOIN envelopes e ON e.message_id=d.envelope_id \
                   WHERE d.state IN ('queued','claimed','delivered') AND e.expires_at <= ?1";
        let mut stmt = self.0.prepare(sql).map_err(db_err)?;
        let rows = stmt
            .query_map(params![now_rfc3339], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            let (mid, rec, state) = r.map_err(db_err)?;
            if let Some(from) = DeliveryState::parse(&state) {
                if transition_allowed(from, DeliveryState::Expired) {
                    self.0
                        .execute(
                            "UPDATE deliveries SET state='expired' \
                             WHERE envelope_id=?1 AND recipient=?2",
                            params![mid, rec],
                        )
                        .map_err(db_err)?;
                    out.push((mid, rec));
                }
            }
        }
        Ok(out)
    }

    /// SPEC §5.5 inbox pull: return deliverable messages for `recipient`
    /// ordered by ascending seq, each transitioning to `delivered` (pull is the
    /// terminal positive delivery). Returns (messages, has_more).
    pub fn inbox(&self, recipient: &str, limit: usize) -> Result<(Vec<Envelope>, bool)> {
        let fetch = (limit + 1) as i64;
        let sql = format!(
            "SELECT {ENV_COLS} FROM deliveries d \
             JOIN envelopes e ON e.message_id=d.envelope_id \
             WHERE d.recipient=?1 AND d.state IN ('queued','claimed') \
             ORDER BY e.seq ASC LIMIT ?2"
        );
        let mut stmt = self.0.prepare(&sql).map_err(db_err)?;
        let rows: Vec<Envelope> = stmt
            .query_map(params![recipient, fetch], envelope_from_row)
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;
        let more = rows.len() > limit;
        let chosen: Vec<Envelope> = rows.into_iter().take(limit).collect();
        for env in &chosen {
            self.0
                .execute(
                    "UPDATE deliveries SET state='delivered', lease_until=NULL \
                     WHERE envelope_id=?1 AND recipient=?2",
                    params![env.message_id, recipient],
                )
                .map_err(db_err)?;
        }
        Ok((chosen, more))
    }

    // ---- submission dedupe (SPEC §8.1) ----

    pub fn dedupe_lookup(&self, from: &str, key: &str) -> Result<Option<DedupeHit>> {
        let r = self.0.query_row(
            "SELECT message_id, ask_id, body_sha256 FROM dedupe \
             WHERE from_cell=?1 AND idempotency_key=?2",
            params![from, key],
            |row| {
                Ok(DedupeHit {
                    message_id: row.get::<_, String>(0)?,
                    ask_id: row.get::<_, Option<String>>(1)?,
                    body_sha256: row.get::<_, String>(2)?,
                })
            },
        );
        match r {
            Ok(h) => Ok(Some(h)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(db_err(e)),
        }
    }

    pub fn dedupe_insert(
        &self,
        from: &str,
        key: &str,
        message_id: &str,
        ask_id: Option<&str>,
        body_sha256: &str,
    ) -> Result<()> {
        let now = now_rfc3339();
        self.0
            .execute(
                "INSERT OR REPLACE INTO dedupe(from_cell,idempotency_key,message_id,ask_id,\
                 body_sha256,created_at) VALUES(?,?,?,?,?,?)",
                params![from, key, message_id, ask_id, body_sha256, now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    // ---- ask tickets (SPEC §6) ----

    pub fn insert_ask(
        &self,
        ask_id: &str,
        message_id: &str,
        from: &str,
        to: &str,
        expires_at: &str,
    ) -> Result<()> {
        self.0
            .execute(
                "INSERT OR REPLACE INTO asks(ask_id,message_id,from_cell,to_cell,expires_at,\
                 state,reply_message_id,reply_body_sha256) VALUES(?,?,?,?,?,'pending',NULL,NULL)",
                params![ask_id, message_id, from, to, expires_at],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn get_ask(&self, ask_id: &str) -> Result<Option<AskRow>> {
        let r = self.0.query_row(
            "SELECT ask_id,message_id,from_cell,to_cell,expires_at,state,reply_message_id \
             FROM asks WHERE ask_id=?1",
            params![ask_id],
            |row| {
                Ok(AskRow {
                    ask_id: row.get::<_, String>(0)?,
                    message_id: row.get::<_, String>(1)?,
                    from_cell: row.get::<_, String>(2)?,
                    to_cell: row.get::<_, String>(3)?,
                    expires_at: row.get::<_, String>(4)?,
                    state: row.get::<_, String>(5)?,
                    reply_message_id: row.get::<_, Option<String>>(6)?,
                })
            },
        );
        match r {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(db_err(e)),
        }
    }

    /// Record the single reply to an ask (SPEC §6.3). `Conflict` → caller
    /// maps to `E_REPLY_EXISTS`; a missing/expired ask also returns `Conflict`
    /// (callers are expected to check ownership/state first via `get_ask`).
    /// Read-only classification of a candidate reply **without committing**
    /// the answer: returns `Recorded` when the ask is `pending` and this reply
    /// would win, `DuplicateIdentical` / `Conflict` otherwise. Pairing this
    /// with [`commit_reply`](Self::commit_reply) lets the caller audit the reply
    /// durably *before* the ask becomes `answered` (consumable by `cell_await`),
    /// so no reply is observable without a durable audit event (G3-01).
    pub fn reply_precheck(&self, ask_id: &str, reply_env: &Envelope) -> Result<ReplyOutcome> {
        let row = self.0.query_row(
            "SELECT state, reply_message_id, reply_body_sha256 FROM asks WHERE ask_id=?1",
            params![ask_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        );
        let (state, reply_msg, reply_hash) = match row {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(ReplyOutcome::Conflict),
            Err(e) => return Err(db_err(e)),
        };
        if state != "pending" {
            let h = body_sha256(&reply_env.body);
            return if reply_hash.as_deref() == Some(h.as_str()) {
                Ok(ReplyOutcome::DuplicateIdentical {
                    message_id: reply_msg.unwrap_or_default(),
                })
            } else {
                Ok(ReplyOutcome::Conflict)
            };
        }
        Ok(ReplyOutcome::Recorded)
    }

    /// Commit the answer: mark the ask `answered` and record the reply id +
    /// body hash. Call only after [`reply_precheck`](Self::reply_precheck)
    /// returned `Recorded` (and, for the fail-closed contract, after the
    /// reply's audit event is durable).
    pub fn commit_reply(&self, ask_id: &str, reply_env: &Envelope) -> Result<()> {
        let h = body_sha256(&reply_env.body);
        self.0
            .execute(
                "UPDATE asks SET state='answered', reply_message_id=?2, reply_body_sha256=?3 \
                 WHERE ask_id=?1",
                params![ask_id, reply_env.message_id, h],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Atomic check-and-commit (precheck + commit) — retained for the store's
    /// own unit tests. Handlers use the split form so they can interpose the
    /// durable audit between the check and the commit (G3-01).
    pub fn record_reply(&self, ask_id: &str, reply_env: &Envelope) -> Result<ReplyOutcome> {
        match self.reply_precheck(ask_id, reply_env)? {
            ReplyOutcome::Recorded => {
                self.commit_reply(ask_id, reply_env)?;
                Ok(ReplyOutcome::Recorded)
            }
            other => Ok(other),
        }
    }

    pub fn pending_asks_count(&self, requester: &str) -> Result<u32> {
        let n: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM asks WHERE from_cell=?1 AND state='pending'",
                params![requester],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?;
        Ok(n as u32)
    }

    pub fn open_asks_where_responder(&self, cell: &str) -> Result<u32> {
        let n: i64 = self
            .0
            .query_row(
                "SELECT COUNT(*) FROM asks WHERE to_cell=?1 AND state='pending'",
                params![cell],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)?;
        Ok(n as u32)
    }

    pub fn expire_asks_due(&self, now_rfc3339: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .0
            .prepare("SELECT ask_id FROM asks WHERE state='pending' AND expires_at <= ?1")
            .map_err(db_err)?;
        let ids: Vec<String> = stmt
            .query_map(params![now_rfc3339], |r| r.get::<_, String>(0))
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;
        for id in &ids {
            self.0
                .execute(
                    "UPDATE asks SET state='expired' WHERE ask_id=?1",
                    params![id],
                )
                .map_err(db_err)?;
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DeliveryState as S;
    use crate::types::*;
    #[test]
    fn transition_table_is_exact() {
        let allowed = [
            (S::Queued, S::Claimed),
            (S::Claimed, S::Delivered),
            (S::Claimed, S::Queued),
            (S::Claimed, S::Failed),
            (S::Delivered, S::Acked),
            (S::Queued, S::Failed),
            (S::Queued, S::Expired),
            (S::Claimed, S::Expired),
        ];
        for (f, t) in allowed {
            assert!(crate::state::transition_allowed(f, t), "{f:?}->{t:?}");
        }
        for f in [S::Acked, S::Failed, S::Expired] {
            // terminali: nessuna uscita
            for t in [S::Queued, S::Claimed, S::Delivered, S::Acked, S::Failed, S::Expired] {
                assert!(!crate::state::transition_allowed(f, t));
            }
        }
        assert!(!crate::state::transition_allowed(S::Queued, S::Delivered)); // niente salti
        assert!(!crate::state::transition_allowed(S::Delivered, S::Queued));
    }
    #[test]
    fn seq_is_per_pair_monotonic() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.next_seq("a", "b").unwrap(), 1);
        assert_eq!(s.next_seq("a", "b").unwrap(), 2);
        assert_eq!(s.next_seq("a", "c").unwrap(), 1); // coppia diversa, contatore proprio
    }
    #[test]
    fn inbox_pull_marks_delivered_and_orders_by_seq() {
        let s = Store::open_in_memory().unwrap();
        for i in 0..3 {
            let mut e = Envelope::test_fixture(Kind::Send);
            e.message_id = new_uuidv7();
            e.to_cell = Some("bob".into());
            e.seq = s.next_seq("alice", "bob").unwrap();
            s.insert_envelope(&e).unwrap();
            s.insert_delivery(&e.message_id, "bob").unwrap();
            let _ = i;
        }
        let (msgs, more) = s.inbox("bob", 10).unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(!more);
        assert!(msgs.windows(2).all(|w| w[0].seq < w[1].seq));
        let (again, _) = s.inbox("bob", 10).unwrap();
        assert!(again.is_empty(), "pull is terminal: already delivered");
    }
    #[test]
    fn lease_lapse_releases_for_redelivery() {
        let s = Store::open_in_memory().unwrap();
        let mut e = Envelope::test_fixture(Kind::Send);
        e.message_id = new_uuidv7();
        e.to_cell = Some("bob".into());
        s.insert_envelope(&e).unwrap();
        s.insert_delivery(&e.message_id, "bob").unwrap();
        let now = 1_000_000i64;
        let c1 = s.claim_due(now, 30, 10).unwrap();
        assert_eq!(c1.len(), 1);
        let c2 = s.claim_due(now + 5, 30, 10).unwrap();
        assert!(c2.is_empty(), "lease attiva");
        let c3 = s.claim_due(now + 31, 30, 10).unwrap();
        assert_eq!(c3.len(), 1, "lease scaduta → re-claim (at-least-once)");
    }
    #[test]
    fn retry_budget_exhaustion_fails() {
        let s = Store::open_in_memory().unwrap();
        let mut e = Envelope::test_fixture(Kind::Send);
        e.message_id = new_uuidv7();
        e.to_cell = Some("bob".into());
        s.insert_envelope(&e).unwrap();
        s.insert_delivery(&e.message_id, "bob").unwrap();
        let mut exhausted = false;
        for _ in 0..10 {
            exhausted = s.record_attempt_failure(&e.message_id, "bob", 10, 1, 60).unwrap();
        }
        assert!(exhausted);
        assert_eq!(s.delivery_state(&e.message_id, "bob").unwrap(), S::Failed);
    }
    #[test]
    fn single_reply_per_ask() {
        let s = Store::open_in_memory().unwrap();
        s.insert_ask("ask1", "m1", "alice", "bob", "2999-01-01T00:00:00Z")
            .unwrap();
        let mut r = Envelope::test_fixture(Kind::Reply);
        r.message_id = new_uuidv7();
        r.ask_id = Some("ask1".into());
        r.reply_to = Some("m1".into());
        assert!(matches!(
            s.record_reply("ask1", &r).unwrap(),
            ReplyOutcome::Recorded
        ));
        let mut r2 = r.clone();
        r2.message_id = new_uuidv7();
        r2.body = "different".into();
        assert!(matches!(
            s.record_reply("ask1", &r2).unwrap(),
            ReplyOutcome::Conflict
        ));
        assert!(matches!(
            s.record_reply("ask1", &r).unwrap(),
            ReplyOutcome::DuplicateIdentical { .. }
        ));
    }
    #[test]
    fn dedupe_binds_key_to_content() {
        let s = Store::open_in_memory().unwrap();
        s.dedupe_insert("alice", "key1", "m1", None, "hashA").unwrap();
        let hit = s.dedupe_lookup("alice", "key1").unwrap().unwrap();
        assert_eq!(hit.message_id, "m1");
        assert!(s.dedupe_lookup("bob", "key1").unwrap().is_none(), "scoped per sender");
    }
}
