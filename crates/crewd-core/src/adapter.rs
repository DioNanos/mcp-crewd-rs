//! Delivery adapter contract (SPEC §10.1, verbatim) + the REQUIRED fake
//! adapter for contract tests (SPEC §10.2).
//!
//! Adapters are implementations of this contract; the bus data model and all
//! delivery guarantees live in `crewd`, never in an adapter (F4). `deliver`
//! MUST return within 250 ms; a slow handoff returns `NotReady` and is
//! re-invoked with backoff.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::state::DeliveryState;
use crate::types::Envelope;

/// SPEC §10.1 trait, mirrored exactly.
pub trait DeliveryAdapter: Send + Sync {
    /// Attempt to hand `envelope` to the recipient cell engine.
    /// Must be non-blocking beyond a bounded, short duration (250 ms); long
    /// waits are expressed by returning `NotReady` and being re-invoked.
    fn deliver(&self, envelope: &Envelope) -> DeliveryOutcome;
}

/// SPEC §10.1 outcome enum, mirrored exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Handoff succeeded; record -> delivered.
    Delivered,
    /// Handoff + consumption ack; record -> acked.
    Acked,
    /// Retryable; record -> queued (backoff).
    TransientFailure,
    /// Non-retryable; record -> failed.
    PermanentFailure,
    /// Cell not currently receivable; retry later (backoff).
    NotReady,
}

/// Normative mapping `DeliveryOutcome` -> Section 7 state (SPEC §10.1).
pub fn outcome_to_state(o: &DeliveryOutcome) -> DeliveryState {
    match o {
        DeliveryOutcome::Delivered => DeliveryState::Delivered,
        DeliveryOutcome::Acked => DeliveryState::Acked,
        DeliveryOutcome::TransientFailure | DeliveryOutcome::NotReady => DeliveryState::Queued,
        DeliveryOutcome::PermanentFailure => DeliveryState::Failed,
    }
}

#[derive(Default)]
struct FakeAdapterInner {
    script: VecDeque<DeliveryOutcome>,
    delivered: Vec<Envelope>,
    attempts: u64,
}

/// In-process fake adapter (SPEC §10.2, REQUIRED): scriptable outcomes FIFO
/// (default `Delivered` once the script is exhausted) and an inspectable log
/// of the envelopes it accepted. Cloneable and shareable across the daemon
/// and the test body.
#[derive(Clone, Default)]
pub struct FakeAdapter {
    inner: Arc<Mutex<FakeAdapterInner>>,
}

impl FakeAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the next outcome to return (FIFO).
    pub fn push_script(&self, outcome: DeliveryOutcome) {
        self.inner.lock().unwrap().script.push_back(outcome);
    }

    /// Envelopes for which this adapter returned `Delivered`/`Acked`.
    pub fn delivered(&self) -> Vec<Envelope> {
        self.inner.lock().unwrap().delivered.clone()
    }

    /// Total `deliver` invocations (redelivery observability for
    /// at-least-once tests).
    pub fn attempts(&self) -> u64 {
        self.inner.lock().unwrap().attempts
    }
}

impl DeliveryAdapter for FakeAdapter {
    fn deliver(&self, envelope: &Envelope) -> DeliveryOutcome {
        let mut inner = self.inner.lock().unwrap();
        inner.attempts += 1;
        let outcome = inner
            .script
            .pop_front()
            .unwrap_or(DeliveryOutcome::Delivered);
        if matches!(outcome, DeliveryOutcome::Delivered | DeliveryOutcome::Acked) {
            inner.delivered.push(envelope.clone());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DeliveryState as S;
    use crate::types::{Envelope, Kind};

    #[test]
    fn outcome_state_mapping_is_normative() {
        assert_eq!(outcome_to_state(&DeliveryOutcome::Delivered), S::Delivered);
        assert_eq!(outcome_to_state(&DeliveryOutcome::Acked), S::Acked);
        assert_eq!(outcome_to_state(&DeliveryOutcome::TransientFailure), S::Queued);
        assert_eq!(outcome_to_state(&DeliveryOutcome::NotReady), S::Queued);
        assert_eq!(outcome_to_state(&DeliveryOutcome::PermanentFailure), S::Failed);
    }

    #[test]
    fn fake_adapter_scripts_and_records() {
        let fa = FakeAdapter::new();
        fa.push_script(DeliveryOutcome::TransientFailure);
        let e = Envelope::test_fixture(Kind::Send);
        assert!(matches!(fa.deliver(&e), DeliveryOutcome::TransientFailure));
        assert!(
            matches!(fa.deliver(&e), DeliveryOutcome::Delivered),
            "script esaurito → default"
        );
        assert_eq!(fa.delivered().len(), 1);
        assert_eq!(fa.attempts(), 2);
    }

    #[test]
    fn fake_adapter_clones_share_state() {
        let fa = FakeAdapter::new();
        let fa2 = fa.clone();
        let e = Envelope::test_fixture(Kind::Send);
        fa.deliver(&e);
        assert_eq!(fa2.delivered().len(), 1);
    }
}
