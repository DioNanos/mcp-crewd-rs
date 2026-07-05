//! Delivery state machine (SPEC §7.1). The transition table is exact and
//! contract-tested; changing an edge is a spec-level change.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryState {
    Queued,
    Claimed,
    Delivered,
    Acked,
    Failed,
    Expired,
}

impl DeliveryState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::Delivered => "delivered",
            Self::Acked => "acked",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "claimed" => Self::Claimed,
            "delivered" => Self::Delivered,
            "acked" => Self::Acked,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => return None,
        })
    }
}

/// SPEC §7.1 transition table — EXACT. Allowed edges:
/// queued→claimed, claimed→delivered, claimed→queued, claimed→failed,
/// delivered→acked, queued→failed, queued→expired, claimed→expired.
/// Everything else (terminal exits, jumps like queued→delivered) is disallowed.
pub fn transition_allowed(from: DeliveryState, to: DeliveryState) -> bool {
    use DeliveryState::*;
    matches!(
        (from, to),
        (Queued, Claimed)
            | (Claimed, Delivered)
            | (Claimed, Queued)
            | (Claimed, Failed)
            | (Delivered, Acked)
            | (Queued, Failed)
            | (Queued, Expired)
            | (Claimed, Expired)
    )
}
