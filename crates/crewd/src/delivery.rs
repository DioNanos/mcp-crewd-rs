//! Delivery loop: a periodic task that expires asks/deliveries past their
//! TTL, claims due deliveries, and drives each recipient's adapter with a
//! 250 ms timeout, applying the §7.1 state machine and the audit events
//! (SPEC §7, §10). Cells without an installed adapter fall back to inbox-pull
//! delivery (the message stays claimable by `cell_inbox`).
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::sync::oneshot;

use crewd_core::adapter::{outcome_to_state, DeliveryAdapter, DeliveryOutcome};
use crewd_core::types::now_rfc3339;

use crate::handlers::DaemonState;

pub async fn run_delivery_loop(state: Arc<DaemonState>, mut shutdown: oneshot::Receiver<()>) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = interval.tick() => { let _ = tick(&state).await; }
        }
    }
}

async fn tick(state: &DaemonState) -> Result<(), ()> {
    let now_rfc = now_rfc3339();
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Expire pending asks (TTL elapsed) → wake any awaiter.
    let expired_asks = state
        .store
        .lock()
        .expect("store")
        .expire_asks_due(&now_rfc)
        .map_err(|_| ())?;
    for id in &expired_asks {
        state.audit_best_effort(
            "ask_expired",
            None,
            None,
            None,
            "expired",
            Some(json!({"ask_id": id})),
        );
        if let Some(n) = state.ask_wakers.lock().expect("wakers").get(id) {
            n.notify_one();
        }
    }

    // Expire non-terminal deliveries (TTL elapsed) → expired.
    let expired = state
        .store
        .lock()
        .expect("store")
        .expire_due(&now_rfc)
        .map_err(|_| ())?;
    for (mid, rec) in &expired {
        state.audit_best_effort("expired", Some(mid), None, Some(rec), "expired", None);
    }

    // Claim due deliveries (lapsed leases are released first) and drive adapters.
    let claimed = state
        .store
        .lock()
        .expect("store")
        .claim_due(now_unix, state.cfg.lease_secs as i64, 50)
        .map_err(|_| ())?;
    let adapters: HashMap<String, Arc<dyn DeliveryAdapter>> =
        state.adapters.lock().expect("adapters").clone();
    for (env, recipient) in claimed {
        let outcome = match adapters.get(&recipient) {
            Some(a) => deliver_with_timeout(a.clone(), &env).await,
            None => DeliveryOutcome::NotReady,
        };
        apply_outcome(state, &env.message_id, &recipient, outcome);
    }
    Ok(())
}

async fn deliver_with_timeout(
    adapter: Arc<dyn DeliveryAdapter>,
    env: &crewd_core::types::Envelope,
) -> DeliveryOutcome {
    let env2 = env.clone();
    match tokio::time::timeout(
        Duration::from_millis(250),
        tokio::task::spawn_blocking(move || adapter.deliver(&env2)),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(_)) | Err(_) => DeliveryOutcome::NotReady,
    }
}

fn apply_outcome(state: &DaemonState, message_id: &str, recipient: &str, outcome: DeliveryOutcome) {
    use DeliveryOutcome::*;
    let st = state.store.lock().expect("store");
    match outcome {
        Delivered | Acked => {
            let target = outcome_to_state(&outcome);
            let _ = st.transition(message_id, recipient, target);
            drop(st);
            state.audit_best_effort(
                "delivered",
                Some(message_id),
                None,
                Some(recipient),
                "ok",
                None,
            );
        }
        PermanentFailure => {
            let _ = st.transition(message_id, recipient, outcome_to_state(&outcome));
            drop(st);
            state.audit_best_effort(
                "delivery_failed",
                Some(message_id),
                None,
                Some(recipient),
                "failed",
                None,
            );
        }
        TransientFailure => {
            let exhausted = st
                .record_attempt_failure(
                    message_id,
                    recipient,
                    state.cfg.max_attempts,
                    state.cfg.backoff_base_secs,
                    state.cfg.backoff_cap_secs,
                )
                .ok();
            drop(st);
            if exhausted == Some(true) {
                state.audit_best_effort(
                    "delivery_failed",
                    Some(message_id),
                    None,
                    Some(recipient),
                    "failed",
                    None,
                );
            }
        }
        NotReady => {
            // Inbox-pull mode or transiently not receivable: leave claimed; the
            // lease lapses back to queued on its own, and `cell_inbox` sees
            // queued|claimed so the recipient can still pull it.
        }
    }
}
