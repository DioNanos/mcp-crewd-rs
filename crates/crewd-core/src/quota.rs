//! Quotas & backpressure (SPEC §14).
//!
//! Sliding-window rate limits per sender and per `(sender, capability)`,
//! pending-ask cap, and queue-depth overflow. Overflow always rejects new
//! messages with `E_QUOTA`; there is no oldest-drop policy (SPEC §14).

use std::collections::{HashMap, VecDeque};

use crate::acl::Capability;
use crate::error::BusError;

const WINDOW_MS: u64 = 60_000;

/// Normative defaults from SPEC §14 (all daemon-configurable).
#[derive(Debug, Clone)]
pub struct QuotaConfig {
    pub sender_per_min: u32,
    pub ask_per_min: u32,
    pub broadcast_per_min: u32,
    pub attach_files_per_min: u32,
    pub wake_per_min: u32,
    pub admin_registry_per_min: u32,
    pub pending_ask_cap: u32,
    pub queue_depth: u32,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            sender_per_min: 60,
            ask_per_min: 20,
            broadcast_per_min: 5,
            attach_files_per_min: 10,
            wake_per_min: 10,
            admin_registry_per_min: 10,
            pending_ask_cap: 10,
            queue_depth: 1000,
        }
    }
}

/// In-memory sliding-window tracker. The daemon owns one instance.
pub struct QuotaTracker {
    config: QuotaConfig,
    per_sender: HashMap<String, VecDeque<u64>>,
    per_capability: HashMap<(String, Capability), VecDeque<u64>>,
}

fn prune(window: &mut VecDeque<u64>, now_ms: u64) {
    while let Some(&front) = window.front() {
        if now_ms.saturating_sub(front) >= WINDOW_MS {
            window.pop_front();
        } else {
            break;
        }
    }
}

impl QuotaTracker {
    pub fn new(config: QuotaConfig) -> Self {
        Self {
            config,
            per_sender: HashMap::new(),
            per_capability: HashMap::new(),
        }
    }

    pub fn config(&self) -> &QuotaConfig {
        &self.config
    }

    fn capability_ceiling(&self, cap: Capability) -> Option<u32> {
        match cap {
            Capability::Ask => Some(self.config.ask_per_min),
            Capability::Broadcast => Some(self.config.broadcast_per_min),
            Capability::AttachFiles => Some(self.config.attach_files_per_min),
            Capability::Wake => Some(self.config.wake_per_min),
            Capability::AdminRegistry => Some(self.config.admin_registry_per_min),
            // send/reply are governed by the per-sender ceiling (SPEC §14).
            _ => None,
        }
    }

    /// Count `fanout_units` against the per-sender 60/min sliding window
    /// (broadcast counts one unit per fan-out recipient, SPEC §14).
    pub fn note_and_check_sender(
        &mut self,
        from: &str,
        now_ms: u64,
        fanout_units: u32,
    ) -> Result<(), BusError> {
        let window = self.per_sender.entry(from.to_string()).or_default();
        prune(window, now_ms);
        let projected = window.len() as u64 + u64::from(fanout_units);
        if projected > u64::from(self.config.sender_per_min) {
            return Err(BusError::Quota(format!(
                "sender {from} exceeded {}/min",
                self.config.sender_per_min
            )));
        }
        for _ in 0..fanout_units {
            window.push_back(now_ms);
        }
        Ok(())
    }

    /// Per-`(from_cell, capability)` ceiling, independent from the sender axis.
    pub fn note_and_check_capability(
        &mut self,
        from: &str,
        cap: Capability,
        now_ms: u64,
    ) -> Result<(), BusError> {
        let Some(ceiling) = self.capability_ceiling(cap) else {
            return Ok(());
        };
        let window = self
            .per_capability
            .entry((from.to_string(), cap))
            .or_default();
        prune(window, now_ms);
        if window.len() as u64 >= u64::from(ceiling) {
            return Err(BusError::Quota(format!(
                "capability {} for {from} exceeded {ceiling}/min",
                cap.as_str()
            )));
        }
        window.push_back(now_ms);
        Ok(())
    }

    /// Queue-depth overflow: reject new (`E_QUOTA`), never drop the oldest.
    pub fn check_queue_depth(&self, pending: u32) -> Result<(), BusError> {
        if pending >= self.config.queue_depth {
            return Err(BusError::Quota(format!(
                "recipient queue full ({pending} pending)"
            )));
        }
        Ok(())
    }

    /// Pending-ask cap (default 10) for a requester.
    pub fn check_pending_ask_cap(&self, open_asks: u32) -> Result<(), BusError> {
        if open_asks >= self.config.pending_ask_cap {
            return Err(BusError::Quota(format!(
                "pending-ask cap reached ({open_asks} open)"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::Capability;

    #[test]
    fn sender_rate_60_per_min_sliding() {
        let mut q = QuotaTracker::new(QuotaConfig::default());
        let t0 = 1_000_000u64;
        for i in 0..60 {
            assert!(q.note_and_check_sender("a", t0 + i, 1).is_ok());
        }
        assert_eq!(
            q.note_and_check_sender("a", t0 + 60, 1).unwrap_err().code(),
            "E_QUOTA"
        );
        assert!(
            q.note_and_check_sender("a", t0 + 61_000, 1).is_ok(),
            "finestra scorsa"
        );
        assert!(
            q.note_and_check_sender("b", t0, 1).is_ok(),
            "per-sender indipendente"
        );
    }

    #[test]
    fn capability_ceiling_independent_from_sender() {
        let mut q = QuotaTracker::new(QuotaConfig::default());
        let t0 = 0u64;
        for i in 0..20 {
            assert!(q
                .note_and_check_capability("a", Capability::Ask, t0 + i)
                .is_ok());
        }
        assert_eq!(
            q.note_and_check_capability("a", Capability::Ask, t0 + 20)
                .unwrap_err()
                .code(),
            "E_QUOTA"
        );
        assert!(
            q.note_and_check_capability("a", Capability::Broadcast, t0).is_ok(),
            "asse separato"
        );
    }

    #[test]
    fn broadcast_fanout_counts_units() {
        let mut q = QuotaTracker::new(QuotaConfig::default());
        assert!(q.note_and_check_sender("a", 0, 59).is_ok());
        assert_eq!(
            q.note_and_check_sender("a", 1, 2).unwrap_err().code(),
            "E_QUOTA"
        );
    }

    #[test]
    fn queue_overflow_rejects_never_drops() {
        let q = QuotaTracker::new(QuotaConfig {
            queue_depth: 5,
            ..Default::default()
        });
        assert!(q.check_queue_depth(4).is_ok());
        assert_eq!(q.check_queue_depth(5).unwrap_err().code(), "E_QUOTA");
    }

    #[test]
    fn pending_ask_cap_10() {
        let q = QuotaTracker::new(QuotaConfig::default());
        assert!(q.check_pending_ask_cap(9).is_ok());
        assert_eq!(q.check_pending_ask_cap(10).unwrap_err().code(), "E_QUOTA");
    }
}
