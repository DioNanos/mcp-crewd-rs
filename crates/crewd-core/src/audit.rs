//! Audit chain (SPEC §12): append-only JSONL, SHA-256 hash-linked, fsync on
//! append, machine-verifiable end-to-end.
//!
//! `hash = hex(sha256(canonical_json(event_without_hash) + prev_hash))`; the
//! genesis `prev_hash` is 64 zeros. The daemon owns the only writer; recipients
//! and operators verify.
use crate::canonical::to_canonical_json;
use crate::error::BusError;
use crate::types::{new_uuidv7, now_rfc3339};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The 29 stable audit event kinds (SPEC §12.2 + §20.10), verbatim: the 17
/// Phase 1 kinds followed by the 12 Phase 2 cell-fabric kinds.
pub const AUDIT_KINDS: [&str; 29] = [
    // --- 17 Phase 1 kinds (SPEC §12.2) ---
    "enqueued",
    "delivered",
    "acked",
    "delivery_failed",
    "expired",
    "ask_opened",
    "ask_expired",
    "duplicate_reply",
    "acl_changed",
    "quota_exceeded",
    "deadlock_prevented",
    "broadcast_fanned_out",
    "auth_rejected",
    "spec_version_rejected",
    "protected_access_denied",
    "registry_changed",
    "token_revoked",
    // --- 12 Phase 2 kinds (SPEC §20.10) ---
    "cell_registered",
    "cell_updated",
    "cell_spawn_requested",
    "cell_turn_started",
    "cell_turn_completed",
    "cell_turn_failed",
    "cell_cancelled",
    "cell_timeout",
    "engine_started",
    "engine_stopped",
    "worktree_created",
    "worktree_cleanup",
];

/// Genesis `prev_hash` (all-zero), per SPEC §12.1.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub ts: String,
    pub kind: String,
    pub message_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub outcome: String,
    pub detail: Option<Value>,
    pub prev_hash: String,
    pub hash: String,
}

impl AuditEvent {
    /// SPEC §12.2: `kind` must be one of the 17 stable kinds.
    pub fn assert_kind(kind: &str) -> Result<(), BusError> {
        if AUDIT_KINDS.contains(&kind) {
            Ok(())
        } else {
            Err(BusError::Internal(format!("unknown audit kind: {kind}")))
        }
    }
}

/// Input to `AuditChain::append`; the daemon assigns `event_id`, `ts`,
/// `prev_hash`, and `hash`.
#[derive(Debug, Clone)]
pub struct AuditEventDraft {
    pub kind: String,
    pub message_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub outcome: String,
    pub detail: Option<Value>,
}

impl AuditEventDraft {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: &str,
        message_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        outcome: &str,
        detail: Option<Value>,
    ) -> Self {
        AuditEventDraft {
            kind: kind.to_string(),
            message_id: message_id.map(|s| s.to_string()),
            from: from.map(|s| s.to_string()),
            to: to.map(|s| s.to_string()),
            outcome: outcome.to_string(),
            detail,
        }
    }
}

/// Result of `AuditChain::verify`.
pub enum VerifyResult {
    /// Chain is internally consistent: every hash recomputes and linkage holds.
    Ok { head_hash: String, events: u64 },
    /// First broken event (content-hash mismatch or `prev_hash` linkage break).
    Broken { event_id: String, index: u64 },
}

pub struct AuditChain {
    path: PathBuf,
    head_hash: String,
}

impl AuditChain {
    /// Open (or create) the chain at `path`, resuming from the last event's
    /// hash if the file already has content.
    pub fn open(path: &Path) -> io::Result<Self> {
        let head_hash = if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            match raw.lines().rfind(|l| !l.trim().is_empty()) {
                Some(line) => {
                    let val: Value = serde_json::from_str(line)?;
                    val.get("hash")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| GENESIS_HASH.to_string())
                }
                None => GENESIS_HASH.to_string(),
            }
        } else {
            GENESIS_HASH.to_string()
        };
        // SPEC §17.3 / H-09: ensure the audit store is 0600 even if it existed
        // with wider perms (append's create-mode only covers first creation).
        #[cfg(unix)]
        if path.exists() {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(path) {
                let mut perms = md.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
        Ok(AuditChain {
            path: path.to_path_buf(),
            head_hash,
        })
    }

    /// Append an event: assign id/ts, set `prev_hash` to the current head,
    /// compute `hash = hex(sha256(canonical_json(event_without_hash) +
    /// prev_hash))`, write the JSONL line, **fsync**, advance the head.
    /// Current head hash (the last appended event's hash, or the genesis hash).
    pub fn head_hash(&self) -> &str {
        &self.head_hash
    }

    pub fn append(&mut self, draft: AuditEventDraft) -> io::Result<AuditEvent> {
        AuditEvent::assert_kind(&draft.kind)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

        let event_id = new_uuidv7();
        let ts = now_rfc3339();
        let prev_hash = self.head_hash.clone();

        let event_for_hash = AuditEvent {
            event_id,
            ts,
            kind: draft.kind,
            message_id: draft.message_id,
            from: draft.from,
            to: draft.to,
            outcome: draft.outcome,
            detail: draft.detail,
            prev_hash: prev_hash.clone(),
            hash: String::new(),
        };
        // Hash is over the event with the `hash` field removed.
        let mut val = serde_json::to_value(&event_for_hash).unwrap();
        if let Value::Object(ref mut m) = val {
            m.remove("hash");
        }
        let canonical = to_canonical_json(&val);
        let hash = hex_sha256(&(canonical + &prev_hash));

        let event = AuditEvent {
            hash: hash.clone(),
            ..event_for_hash
        };
        let line = serde_json::to_string(&event).unwrap() + "\n";
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        // SPEC §17.3 / H-09: audit store MUST be 0600 (applies on create).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?; // fsync before the action is acknowledged (SPEC §12.3)
        self.head_hash = hash.clone();
        Ok(event)
    }

    /// Walk the chain end-to-end, recomputing every hash and checking linkage.
    /// The first broken event stops the walk.
    pub fn verify(path: &Path) -> VerifyResult {
        // A genuinely absent chain is a valid genesis (no events yet). An
        // EXISTING chain that cannot be read MUST fail closed: returning Ok
        // would let a corrupted or permission-stripped audit file pass as
        // intact (fail-open state drift).
        if !path.exists() {
            return VerifyResult::Ok {
                head_hash: GENESIS_HASH.to_string(),
                events: 0,
            };
        }
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                return VerifyResult::Broken {
                    event_id: String::new(),
                    index: 0,
                }
            }
        };
        let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut prev_event_hash = GENESIS_HASH.to_string();
        for (i, line) in lines.iter().enumerate() {
            let mut val: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    return VerifyResult::Broken {
                        event_id: String::new(),
                        index: i as u64,
                    }
                }
            };
            let hash = val
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let prev_hash = val
                .get("prev_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let event_id = val
                .get("event_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Value::Object(ref mut m) = val {
                m.remove("hash");
            }
            let canonical = to_canonical_json(&val);
            let recomputed = hex_sha256(&(canonical + &prev_hash));
            if recomputed != hash || prev_hash != prev_event_hash {
                return VerifyResult::Broken {
                    event_id,
                    index: i as u64,
                };
            }
            prev_event_hash = hash;
        }
        VerifyResult::Ok {
            head_hash: prev_event_hash,
            events: lines.len() as u64,
        }
    }
}

fn hex_sha256(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chain_appends_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let mut c = AuditChain::open(&p).unwrap();
        let e1 = c
            .append(AuditEventDraft::new(
                "enqueued",
                Some("m1"),
                Some("a"),
                Some("b"),
                "ok",
                None,
            ))
            .unwrap();
        assert_eq!(e1.prev_hash, "0".repeat(64));
        let e2 = c
            .append(AuditEventDraft::new(
                "delivered",
                Some("m1"),
                Some("a"),
                Some("b"),
                "ok",
                None,
            ))
            .unwrap();
        assert_eq!(e2.prev_hash, e1.hash);
        match AuditChain::verify(&p) {
            VerifyResult::Ok { head_hash, events } => {
                assert_eq!(head_hash, e2.hash);
                assert_eq!(events, 2);
            }
            _ => panic!("chain must verify"),
        }
    }
    #[test]
    fn one_byte_flip_breaks_verification() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let mut c = AuditChain::open(&p).unwrap();
        c.append(AuditEventDraft::new(
            "enqueued",
            Some("m1"),
            Some("a"),
            Some("b"),
            "ok",
            None,
        ))
        .unwrap();
        let ev2 = c
            .append(AuditEventDraft::new(
                "delivered",
                Some("m1"),
                Some("a"),
                Some("b"),
                "ok",
                None,
            ))
            .unwrap();
        let mut raw = std::fs::read_to_string(&p).unwrap();
        raw = raw.replacen("\"outcome\":\"ok\"", "\"outcome\":\"OK\"", 1); // flip in the first event
        std::fs::write(&p, raw).unwrap();
        match AuditChain::verify(&p) {
            VerifyResult::Broken { index, .. } => assert_eq!(index, 0),
            _ => panic!("must be broken"),
        }
        let _ = ev2;
    }
    #[test]
    fn unknown_kind_rejected() {
        assert!(AuditEvent::assert_kind("enqueued").is_ok());
        assert!(AuditEvent::assert_kind("made_up").is_err());
    }
    #[test]
    fn reopen_resumes_head() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let h1 = {
            let mut c = AuditChain::open(&p).unwrap();
            c.append(AuditEventDraft::new(
                "enqueued", None, None, None, "ok", None,
            ))
            .unwrap()
            .hash
        };
        let mut c2 = AuditChain::open(&p).unwrap();
        let e2 = c2
            .append(AuditEventDraft::new(
                "expired", None, None, None, "expired", None,
            ))
            .unwrap();
        assert_eq!(e2.prev_hash, h1);
    }
    #[test]
    fn verify_missing_path_is_genesis_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does-not-exist.jsonl");
        match AuditChain::verify(&p) {
            VerifyResult::Ok { events: 0, .. } => {}
            _ => panic!("non-existent path must be genesis Ok"),
        }
    }
    #[test]
    fn verify_fails_closed_on_unreadable_existing_path() {
        // An existing-but-unreadable path (here: a directory) MUST NOT pass as
        // an empty/intact chain — that would be fail-open state drift.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path(); // exists, but read_to_string fails
        match AuditChain::verify(p) {
            VerifyResult::Broken { .. } => {}
            VerifyResult::Ok { .. } => panic!("must fail closed, not Ok"),
        }
    }

    /// SPEC §20.10: the 12 new Phase 2 audit event kinds are accepted by the
    /// chain; hash-linkage and end-to-end verification stay intact.
    #[test]
    fn phase2_audit_kinds_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let mut c = AuditChain::open(&p).unwrap();
        let kinds = [
            "cell_registered",
            "cell_updated",
            "cell_spawn_requested",
            "cell_turn_started",
            "cell_turn_completed",
            "cell_turn_failed",
            "cell_cancelled",
            "cell_timeout",
            "engine_started",
            "engine_stopped",
            "worktree_created",
            "worktree_cleanup",
        ];
        let mut prev = "0".repeat(64);
        for k in kinds {
            let ev = c
                .append(AuditEventDraft::new(
                    k,
                    Some("m1"),
                    Some("a"),
                    Some("b"),
                    "ok",
                    None,
                ))
                .unwrap();
            assert_eq!(ev.kind, k);
            assert_eq!(ev.prev_hash, prev);
            prev = ev.hash;
        }
        match AuditChain::verify(&p) {
            VerifyResult::Ok { events, .. } => assert_eq!(events, kinds.len() as u64),
            _ => panic!("chain must verify with phase2 kinds"),
        }
    }
}
