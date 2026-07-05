//! Engine protocol types (SPEC §20.10) and thread journal. These are the pure
//! `crewd-core` types shared with the daemon adapters (Task 8): `EngineCaps`,
//! the `EngineEvent` stream (`accepted`/`note`/`final`/`failed`), and
//! `CellResult`. The `thread_journal` table holds the per-thread monotonic
//! event log whose tail is surfaced (seq-prefixed) in `cell_result.event_tail`.
use crate::error::BusError;
use crate::jobs::immediate_tx;
use crate::store::Store;
use crate::types::now_rfc3339;
use rusqlite::params;

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct EngineCaps {
    pub supports_session_resume: bool,
    pub supports_abort: bool,
    pub supports_stream_replay: bool,
    pub supports_model_override: bool,
    pub supports_yolo: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum EngineEvent {
    Accepted { engine_turn_id: String },
    /// Progress / journal line.
    Note { text: String },
    Final { final_answer: String },
    Failed { error: String },
}

/// SPEC §20.10: structured result. The `event_tail` (≤50, seq-prefixed) is a
/// view over the journal, **not** the result itself.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CellResult {
    pub final_answer: Option<String>,
    pub event_tail: Vec<String>,
    pub artifact_refs: Vec<String>,
    /// `done|interrupted|timeout|failed_unknown|cancelled`
    pub exit_status: String,
    pub crewd_thread_id: String,
    pub engine_process_id: Option<i64>,
    pub engine_thread_id: Option<String>,
    pub engine_turn_id: Option<String>,
    pub engine_session_id: Option<String>,
}

impl CellResult {
    /// SPEC §20.2: all identity domains are populated from the `CellThread`.
    /// The caller fills `final_answer`, `event_tail`, `artifact_refs`,
    /// `exit_status` for the actual result view.
    pub fn from_thread(thread: &crate::threads::CellThread) -> Self {
        CellResult {
            final_answer: None,
            event_tail: Vec::new(),
            artifact_refs: Vec::new(),
            exit_status: String::new(),
            crewd_thread_id: thread.crewd_thread_id.clone(),
            engine_process_id: thread.engine_process_id,
            engine_thread_id: thread.engine_thread_id.clone(),
            engine_turn_id: thread.engine_turn_id.clone(),
            engine_session_id: thread.engine_session_id.clone(),
        }
    }
}

impl Store {
    /// SPEC §20.5-style determinism: assign the next per-thread `seq` inside a
    /// `BEGIN IMMEDIATE` write transaction, so concurrent appenders cannot read
    /// the same `MAX(seq)` and collide on `PRIMARY KEY(thread_id, seq)`.
    pub fn journal_append(&self, thread_id: &str, line: &str) -> Result<u64, BusError> {
        let tx = immediate_tx(&self.0)?;
        let next: i64 = self
            .0
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM thread_journal WHERE thread_id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        self.0
            .execute(
                "INSERT INTO thread_journal (thread_id, seq, line, at) VALUES (?1, ?2, ?3, ?4)",
                params![thread_id, next, line, now_rfc3339()],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        tx.commit()?;
        Ok(next as u64)
    }

    /// SPEC §20.10: last `n` journal lines (ascending seq), each prefixed with
    /// its zero-padded 4-digit seq (`"0007 …"`). Fail-closed: a corrupt row
    /// surfaces as `BusError`, never a silently truncated tail.
    pub fn journal_tail(&self, thread_id: &str, n: usize) -> Result<Vec<String>, BusError> {
        let mut stmt = self
            .0
            .prepare(
                "SELECT seq, line FROM thread_journal WHERE thread_id = ?1 ORDER BY seq DESC LIMIT ?2",
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let rows = stmt
            .query_map(params![thread_id, n as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let mut out: Vec<(i64, String)> = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| BusError::Internal(e.to_string()))?);
        }
        out.reverse(); // ascending seq in the returned tail
        Ok(out.into_iter().map(|(seq, line)| format!("{seq:04} {line}")).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn journal_seq_monotonic_and_tail_prefix() {
        let s = Store::open_in_memory().unwrap();
        let s1 = s.journal_append("th", "line-a").unwrap();
        let s2 = s.journal_append("th", "line-b").unwrap();
        let s3 = s.journal_append("th", "line-c").unwrap();
        assert_eq!((s1, s2, s3), (1, 2, 3));
        let tail = s.journal_tail("th", 2).unwrap();
        assert_eq!(tail, vec!["0002 line-b".to_string(), "0003 line-c".to_string()]);
        // seq is per-thread: another thread restarts at 1
        assert_eq!(s.journal_append("other", "x").unwrap(), 1);
    }

    #[test]
    fn journal_tail_caps_at_n_and_zero_pads() {
        let s = Store::open_in_memory().unwrap();
        for i in 1..=12 {
            s.journal_append("th", &format!("l{i}")).unwrap();
        }
        let tail = s.journal_tail("th", 5).unwrap();
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0], "0008 l8");
        assert_eq!(tail[4], "0012 l12");
    }

    #[test]
    fn engine_event_serde_roundtrip_all_tags() {
        let cases = vec![
            EngineEvent::Accepted { engine_turn_id: "t1".into() },
            EngineEvent::Note { text: "progress".into() },
            EngineEvent::Final { final_answer: "done".into() },
            EngineEvent::Failed { error: "boom".into() },
        ];
        for ev in &cases {
            let s = serde_json::to_string(ev).unwrap();
            let back: EngineEvent = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
        assert!(serde_json::to_string(&EngineEvent::Accepted { engine_turn_id: "x".into() })
            .unwrap()
            .contains("\"ev\":\"accepted\""));
        assert!(serde_json::to_string(&EngineEvent::Note { text: "n".into() })
            .unwrap()
            .contains("\"ev\":\"note\""));
        assert!(serde_json::to_string(&EngineEvent::Final { final_answer: "f".into() })
            .unwrap()
            .contains("\"ev\":\"final\""));
        assert!(serde_json::to_string(&EngineEvent::Failed { error: "e".into() })
            .unwrap()
            .contains("\"ev\":\"failed\""));
    }

    #[test]
    fn cell_result_serializes_all_id_fields_distinct() {
        let r = CellResult {
            final_answer: Some("ans".into()),
            event_tail: vec!["0001 a".into()],
            artifact_refs: vec!["art".into()],
            exit_status: "done".into(),
            crewd_thread_id: "crew-1".into(),
            engine_process_id: Some(4242),
            engine_thread_id: Some("eng-th-1".into()),
            engine_turn_id: Some("eng-turn-9".into()),
            engine_session_id: Some("eng-sess-1".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        let obj = v.as_object().unwrap();
        // all FIVE identity domains present and distinct (SPEC §20.2)
        assert_eq!(obj["crewd_thread_id"], serde_json::json!("crew-1"));
        assert_eq!(obj["engine_process_id"], serde_json::json!(4242));
        assert_eq!(obj["engine_thread_id"], serde_json::json!("eng-th-1"));
        assert_eq!(obj["engine_turn_id"], serde_json::json!("eng-turn-9"));
        assert_eq!(obj["engine_session_id"], serde_json::json!("eng-sess-1"));
        assert!(obj.contains_key("event_tail"));
        assert!(obj.contains_key("artifact_refs"));
        assert!(obj.contains_key("exit_status"));
        assert!(obj.contains_key("final_answer"));
    }

    #[test]
    fn cell_result_from_thread_copies_all_ids() {
        use crate::cells::EngineKind;
        use crate::threads::{CellThread, ThreadState};
        let t = CellThread {
            crewd_thread_id: "crew-9".into(),
            cell_name: "c".into(),
            engine_kind: EngineKind::Fake,
            model: None,
            profile: None,
            engine_process_id: Some(7),
            engine_thread_id: Some("eth".into()),
            engine_turn_id: Some("etu".into()),
            engine_session_id: Some("ese".into()),
            cwd: "/w".into(),
            worktree_path: None,
            state: ThreadState::Done,
            generation: 0,
            created_by_principal: "p".into(),
            idempotency_key: "k".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let r = CellResult::from_thread(&t);
        assert_eq!(r.crewd_thread_id, "crew-9");
        assert_eq!(r.engine_process_id, Some(7));
        assert_eq!(r.engine_thread_id.as_deref(), Some("eth"));
        assert_eq!(r.engine_turn_id.as_deref(), Some("etu"));
        assert_eq!(r.engine_session_id.as_deref(), Some("ese"));
    }

    #[test]
    fn journal_concurrent_file_backed_no_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("j.db");
        {
            let _ = Store::open(&db).unwrap(); // init schema on the shared file
        }
        let n_threads = 4usize;
        let per = 25usize;
        let total = (n_threads * per) as u64;
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                let s = Store::open(&db).unwrap();
                let mut seqs = Vec::new();
                for _ in 0..per {
                    seqs.push(s.journal_append("th", "x").unwrap());
                }
                seqs
            }));
        }
        let mut all: Vec<u64> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        assert_eq!(all.len(), total as usize);
        all.sort_unstable();
        let expected: Vec<u64> = (1..=total).collect();
        assert_eq!(all, expected, "journal seq must be gap-free + unique under concurrency");
    }

    #[test]
    fn engine_caps_serializes_all_fields() {
        let caps = EngineCaps {
            supports_session_resume: true,
            supports_abort: false,
            supports_stream_replay: true,
            supports_model_override: false,
            supports_yolo: true,
        };
        let s = serde_json::to_string(&caps).unwrap();
        assert!(s.contains("supports_session_resume"));
        assert!(s.contains("supports_abort"));
        assert!(s.contains("supports_stream_replay"));
        assert!(s.contains("supports_model_override"));
        assert!(s.contains("supports_yolo"));
    }
}
