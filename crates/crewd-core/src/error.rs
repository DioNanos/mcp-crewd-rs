//! `BusError` — the 12 stable error codes of the bus protocol (SPEC §13),
//! verbatim. Each variant carries a human-readable `message`; `code()` returns
//! the stable wire string that contract tests assert exactly.
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BusError {
    #[error("E_ACL_DENIED: {0}")]
    AclDenied(String),
    #[error("E_AUTH_REJECTED: {0}")]
    AuthRejected(String),
    #[error("E_UNKNOWN_CELL: {0}")]
    UnknownCell(String),
    #[error("E_TTL_EXPIRED: {0}")]
    TtlExpired(String),
    #[error("E_WOULD_DEADLOCK: {0}")]
    WouldDeadlock(String),
    #[error("E_QUOTA: {0}")]
    Quota(String),
    #[error("E_BODY_TOO_LARGE: {0}")]
    BodyTooLarge(String),
    #[error("E_DUP: {0}")]
    Dup(String),
    #[error("E_ASK_NOT_FOUND: {0}")]
    AskNotFound(String),
    #[error("E_REPLY_EXISTS: {0}")]
    ReplyExists(String),
    #[error("E_UNSUPPORTED_SPEC_VERSION: {0}")]
    UnsupportedSpecVersion(String),
    // --- Phase 2 (SPEC §20.10): 7 new error codes ---
    #[error("E_CELL_BUSY: {0}")]
    CellBusy(String),
    #[error("E_QUEUE_FULL: {0}")]
    QueueFull(String),
    #[error("E_ENGINE_DOWN: {0}")]
    EngineDown(String),
    #[error("E_THREAD_NOT_RESUMABLE: {0}")]
    ThreadNotResumable(String),
    #[error("E_POLICY_DENIED: {0}")]
    PolicyDenied(String),
    #[error("E_TIMEOUT: {0}")]
    Timeout(String),
    #[error("E_CANCELLED: {0}")]
    Cancelled(String),
    #[error("E_INTERNAL: {0}")]
    Internal(String),
}

impl BusError {
    /// Stable string code, part of the protocol contract (SPEC §13). Adding a
    /// code is a minor bump; renaming/changing one is a major bump (SPEC §16).
    pub fn code(&self) -> &'static str {
        match self {
            Self::AclDenied(_) => "E_ACL_DENIED",
            Self::AuthRejected(_) => "E_AUTH_REJECTED",
            Self::UnknownCell(_) => "E_UNKNOWN_CELL",
            Self::TtlExpired(_) => "E_TTL_EXPIRED",
            Self::WouldDeadlock(_) => "E_WOULD_DEADLOCK",
            Self::Quota(_) => "E_QUOTA",
            Self::BodyTooLarge(_) => "E_BODY_TOO_LARGE",
            Self::Dup(_) => "E_DUP",
            Self::AskNotFound(_) => "E_ASK_NOT_FOUND",
            Self::ReplyExists(_) => "E_REPLY_EXISTS",
            Self::UnsupportedSpecVersion(_) => "E_UNSUPPORTED_SPEC_VERSION",
            Self::CellBusy(_) => "E_CELL_BUSY",
            Self::QueueFull(_) => "E_QUEUE_FULL",
            Self::EngineDown(_) => "E_ENGINE_DOWN",
            Self::ThreadNotResumable(_) => "E_THREAD_NOT_RESUMABLE",
            Self::PolicyDenied(_) => "E_POLICY_DENIED",
            Self::Timeout(_) => "E_TIMEOUT",
            Self::Cancelled(_) => "E_CANCELLED",
            Self::Internal(_) => "E_INTERNAL",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §20.10: the 7 new Phase 2 error codes, verbatim. Each variant
    /// carries a human-readable message, like the Phase 1 variants.
    #[test]
    fn phase2_error_codes_exact() {
        for (e, s) in [
            (BusError::CellBusy("c".into()), "E_CELL_BUSY"),
            (BusError::QueueFull("c".into()), "E_QUEUE_FULL"),
            (BusError::EngineDown("c".into()), "E_ENGINE_DOWN"),
            (
                BusError::ThreadNotResumable("t".into()),
                "E_THREAD_NOT_RESUMABLE",
            ),
            (BusError::PolicyDenied("c".into()), "E_POLICY_DENIED"),
            (BusError::Timeout("t".into()), "E_TIMEOUT"),
            (BusError::Cancelled("t".into()), "E_CANCELLED"),
        ] {
            assert_eq!(e.code(), s);
        }
    }
}
