//! Envelope types, limit constants, and RFC 3339 time helpers (SPEC §3, §4).
//!
//! Names are copied verbatim from `SPEC.md`. The two axes of message typing —
//! transport `Kind` and semantic `MsgType` — are independent (SPEC §3, I3.6).
use crate::error::BusError;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// SPEC §3 envelope `spec_version` for this implementation.
pub const SPEC_VERSION: &str = "0.1";
/// SPEC §3 `body` limit (64 KiB).
pub const MAX_BODY_BYTES: usize = 65_536;
/// SPEC §3 / §8.1 client-supplied idempotency key limit (128 bytes UTF-8).
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// SPEC §3.1 TTL ceilings (CLAMP, never reject).
pub const TTL_CEILING_ASK_SECS: u64 = 900;
pub const TTL_CEILING_SEND_SECS: u64 = 86_400; // send / reply / notice
pub const TTL_CEILING_BROADCAST_SECS: u64 = 3_600;
/// SPEC §3 constant envelope taint in v0.
pub const TAINT: &str = "peer_untrusted";

/// Transport kind (SPEC §4 / §3 I3.3). Daemon-derived from the tool path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Send,
    Ask,
    Reply,
    Broadcast,
    Notice,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Send => "send",
            Kind::Ask => "ask",
            Kind::Reply => "reply",
            Kind::Broadcast => "broadcast",
            Kind::Notice => "notice",
        }
    }
}

/// Semantic type (SPEC §4.1). Independent of `Kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgType {
    Note,
    Task,
    Ask,
    Reply,
    Evidence,
    AdminRequest,
}

impl MsgType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Task => "task",
            Self::Ask => "ask",
            Self::Reply => "reply",
            Self::Evidence => "evidence",
            Self::AdminRequest => "admin_request",
        }
    }
}

/// The normative message record (SPEC §3). Produced and validated by the
/// daemon; the sender supplies addressing + body, the daemon fills the rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub spec_version: String,
    pub message_id: String,
    pub ask_id: Option<String>,
    pub from_cell: String,
    pub to_cell: Option<String>,
    pub kind: Kind,
    pub msg_type: MsgType,
    pub principal_capabilities: Vec<String>,
    pub created_at: String,
    pub expires_at: String,
    pub seq: u64,
    pub idempotency_key: String,
    pub body: String,
    pub file_refs: Vec<String>,
    pub taint: String,
    pub reply_to: Option<String>,
}

impl Envelope {
    /// SPEC §3.2 authority invariants: I3.4 (reply ⇒ reply_to + ask_id),
    /// I3.5 (broadcast ⇔ null to_cell), plus body size (§3) and constant
    /// taint. Structural violations map to `E_INTERNAL`; a reply missing its
    /// correlation maps to `E_ASK_NOT_FOUND`; an oversized body to
    /// `E_BODY_TOO_LARGE`.
    pub fn validate_invariants(&self) -> Result<(), BusError> {
        if self.taint != TAINT {
            return Err(BusError::Internal(format!("taint must be {TAINT}")));
        }
        match self.kind {
            Kind::Broadcast => {
                if self.to_cell.is_some() {
                    return Err(BusError::Internal(
                        "broadcast must have null to_cell (I3.5)".into(),
                    ));
                }
            }
            _ => {
                if self.to_cell.is_none() {
                    return Err(BusError::Internal(
                        "non-broadcast must have non-null to_cell (I3.5)".into(),
                    ));
                }
            }
        }
        if self.kind == Kind::Reply && (self.reply_to.is_none() || self.ask_id.is_none()) {
            return Err(BusError::AskNotFound(
                "reply requires reply_to + ask_id (I3.4)".into(),
            ));
        }
        if self.body.len() > MAX_BODY_BYTES {
            return Err(BusError::BodyTooLarge(format!(
                "body {} > {} bytes",
                self.body.len(),
                MAX_BODY_BYTES
            )));
        }
        Ok(())
    }

    /// Test-only constructor: a valid envelope coherent with `kind`
    /// (broadcast ⇒ null to_cell; reply ⇒ reply_to + ask_id set; ask ⇒ ask_id).
    #[cfg(test)]
    pub fn test_fixture(kind: Kind) -> Self {
        let to_cell = if matches!(kind, Kind::Broadcast) {
            None
        } else {
            Some("codex-audit".into())
        };
        let (ask_id, reply_to) = match kind {
            Kind::Reply => (Some(new_uuidv7()), Some(new_uuidv7())),
            Kind::Ask => (Some(new_uuidv7()), None),
            _ => (None, None),
        };
        Envelope {
            spec_version: SPEC_VERSION.into(),
            message_id: new_uuidv7(),
            ask_id,
            from_cell: "dev-senior".into(),
            to_cell,
            kind,
            msg_type: MsgType::Task,
            principal_capabilities: vec!["send".into(), "ask".into()],
            created_at: now_rfc3339(),
            expires_at: rfc3339_after(3600),
            seq: 1,
            idempotency_key: new_uuidv7(),
            body: "hello".into(),
            file_refs: vec![],
            taint: TAINT.into(),
            reply_to,
        }
    }
}

/// Daemon-assigned message identifier (UUIDv7, lowercase hyphenated).
pub fn new_uuidv7() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// SPEC §3.1: a requested TTL is clamped to the daemon ceiling for the kind,
/// never rejected.
pub fn clamp_ttl(kind: Kind, requested_secs: Option<u64>) -> u64 {
    let ceiling = match kind {
        Kind::Ask => TTL_CEILING_ASK_SECS,
        Kind::Broadcast => TTL_CEILING_BROADCAST_SECS,
        Kind::Send | Kind::Reply | Kind::Notice => TTL_CEILING_SEND_SECS,
    };
    match requested_secs {
        None => ceiling,
        Some(s) => s.min(ceiling),
    }
}

/// Current time as RFC 3339 UTC (`...Z`), formatted with no extra dependency.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix_secs(secs)
}

/// Time `secs` in the future as RFC 3339 UTC, for `expires_at` computation.
pub fn rfc3339_after(secs: u64) -> String {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix_secs(base.saturating_add(secs))
}

fn rfc3339_from_unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
}

/// Civil (proleptic Gregorian) date from days since 1970-01-01.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_codes_are_exact_set() {
        use crate::error::BusError as E;
        let codes: Vec<&str> = vec![
            E::AclDenied(String::new()).code(), E::AuthRejected(String::new()).code(),
            E::UnknownCell(String::new()).code(), E::TtlExpired(String::new()).code(),
            E::WouldDeadlock(String::new()).code(), E::Quota(String::new()).code(),
            E::BodyTooLarge(String::new()).code(), E::Dup(String::new()).code(),
            E::AskNotFound(String::new()).code(), E::ReplyExists(String::new()).code(),
            E::UnsupportedSpecVersion(String::new()).code(), E::Internal(String::new()).code(),
        ];
        assert_eq!(codes, vec!["E_ACL_DENIED","E_AUTH_REJECTED","E_UNKNOWN_CELL","E_TTL_EXPIRED",
            "E_WOULD_DEADLOCK","E_QUOTA","E_BODY_TOO_LARGE","E_DUP","E_ASK_NOT_FOUND",
            "E_REPLY_EXISTS","E_UNSUPPORTED_SPEC_VERSION","E_INTERNAL"]);
    }
    #[test]
    fn kind_and_msg_type_serde_names() {
        assert_eq!(serde_json::to_string(&Kind::Broadcast).unwrap(), "\"broadcast\"");
        assert_eq!(serde_json::to_string(&MsgType::AdminRequest).unwrap(), "\"admin_request\"");
    }
    #[test]
    fn ttl_clamped_never_rejected() {
        assert_eq!(clamp_ttl(Kind::Ask, Some(999_999)), TTL_CEILING_ASK_SECS);
        assert_eq!(clamp_ttl(Kind::Ask, Some(60)), 60);
        assert_eq!(clamp_ttl(Kind::Send, None), TTL_CEILING_SEND_SECS);
        assert_eq!(clamp_ttl(Kind::Broadcast, None), TTL_CEILING_BROADCAST_SECS);
    }
    #[test]
    fn broadcast_must_have_null_to_cell() {
        let mut e = Envelope::test_fixture(Kind::Broadcast);
        e.to_cell = Some("x".into());
        assert_eq!(e.validate_invariants().unwrap_err().code(), "E_INTERNAL");
    }
    #[test]
    fn reply_requires_reply_to() {
        let mut e = Envelope::test_fixture(Kind::Reply);
        e.reply_to = None;
        assert_eq!(e.validate_invariants().unwrap_err().code(), "E_ASK_NOT_FOUND");
    }
    #[test]
    fn body_over_64k_rejected() {
        let mut e = Envelope::test_fixture(Kind::Send);
        e.body = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(e.validate_invariants().unwrap_err().code(), "E_BODY_TOO_LARGE");
    }
    #[test]
    fn uuidv7_is_lowercase_hyphenated() {
        let u = new_uuidv7();
        assert_eq!(u, u.to_lowercase());
        assert_eq!(u.split('-').map(|s| s.len()).collect::<Vec<_>>(), vec![8,4,4,4,12]);
    }
    #[test]
    fn now_rfc3339_ends_with_z() {
        assert!(now_rfc3339().ends_with('Z'));
    }
}
