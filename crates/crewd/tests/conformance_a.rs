//! Conformance suite §19.1 — part A (SPEC.md v0.1).
//!
//! Categories: envelope authority, ask/reply lifecycle (F1), ACL capability
//! (F9), protected cells (D2), broadcast-to-protected (G1-01), quotas,
//! submission dedupe. Every test spawns a fresh daemon through the testkit;
//! nothing here depends on Claude Code, Codex, tmux, or any external runtime.

use std::time::{Duration, Instant};

use serde_json::json;

use crewd::testkit::{connect_as, spawn_daemon, spawn_daemon_with_quota};
use crewd_core::quota::QuotaConfig;

/// Read the daemon audit chain (JSONL) and return the parsed events.
fn audit_events(runtime_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(runtime_dir.join("audit.jsonl")).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("audit line is JSON"))
        .collect()
}

fn has_event(runtime_dir: &std::path::Path, kind: &str) -> bool {
    audit_events(runtime_dir).iter().any(|e| e["kind"] == kind)
}

// ---------------------------------------------------------------- envelope authority

#[tokio::test]
async fn envelope_authority_rejects_client_identity() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let tok = h.issued_tokens["dev-senior"].clone();
    let mut a = connect_as(&rt, "dev-senior", &tok).await.unwrap();

    // Client-supplied message_id MUST be rejected with E_INTERNAL (SPEC §3.1).
    let err = a
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "x", "message_id": "m-evil"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_INTERNAL");

    // Client-supplied from_cell MUST be ignored-and-rejected (I3.2).
    let err = a
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "x", "from_cell": "coordinator"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_INTERNAL");

    // A clean send derives identity from the connection: the recipient sees
    // from_cell == the authenticated sender, and a daemon-assigned UUIDv7.
    let r = a
        .call("cell_send", json!({"to_cell": "codex-audit", "body": "hello"}))
        .await
        .unwrap();
    let mid = r["message_id"].as_str().unwrap().to_string();
    assert_eq!(mid.split('-').map(|s| s.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);

    let tok_b = h.issued_tokens["codex-audit"].clone();
    let mut b = connect_as(&rt, "codex-audit", &tok_b).await.unwrap();
    let inbox = b.call("cell_inbox", json!({})).await.unwrap();
    let msgs = inbox["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["from_cell"], "dev-senior");
    assert_eq!(msgs[0]["message_id"], mid.as_str());
    assert_eq!(msgs[0]["taint"], "peer_untrusted");
    // I3.7: principal_capabilities is daemon-derived from the sender's grants.
    let caps: Vec<String> = msgs[0]["principal_capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(caps.contains(&"send".to_string()), "sender caps asserted: {caps:?}");
    h.shutdown().await;
}

// ---------------------------------------------------------------- ask/reply (F1)

#[tokio::test]
async fn ask_returns_pending_within_2s_and_reply_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();

    let t0 = Instant::now();
    let r = a
        .call("cell_ask", json!({"to_cell": "codex-audit", "body": "safe to merge?"}))
        .await
        .unwrap();
    assert!(t0.elapsed() < Duration::from_secs(2), "cell_ask MUST return within 2s");
    assert_eq!(r["status"], "pending");
    let ask_id = r["ask_id"].as_str().unwrap().to_string();

    let inbox = b.call("cell_inbox", json!({})).await.unwrap();
    assert_eq!(inbox["messages"][0]["ask_id"], ask_id.as_str());
    assert_eq!(inbox["messages"][0]["kind"], "ask");

    let rr = b
        .call("cell_reply", json!({"ask_id": ask_id, "body": "yes, after CI"}))
        .await
        .unwrap();
    assert_eq!(rr["status"], "recorded");

    let aw = a
        .call("cell_await", json!({"ask_id": ask_id, "timeout_ms": 5000}))
        .await
        .unwrap();
    assert_eq!(aw["status"], "answered");
    // SPEC §5.3: the reply is the full envelope of kind=reply.
    assert_eq!(aw["reply"]["kind"], "reply");
    assert_eq!(aw["reply"]["body"], "yes, after CI");
    assert_eq!(aw["reply"]["from_cell"], "codex-audit");
    assert_eq!(aw["reply"]["reply_to"].as_str().unwrap(), r["message_id"].as_str().unwrap());
    h.shutdown().await;
}

#[tokio::test]
async fn second_differing_reply_rejected_e_reply_exists() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();

    let r = a
        .call("cell_ask", json!({"to_cell": "codex-audit", "body": "?"}))
        .await
        .unwrap();
    let ask_id = r["ask_id"].as_str().unwrap().to_string();
    b.call("cell_reply", json!({"ask_id": ask_id, "body": "first"})).await.unwrap();

    let err = b
        .call("cell_reply", json!({"ask_id": ask_id, "body": "second, different"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_REPLY_EXISTS");
    assert!(has_event(&rt, "duplicate_reply"), "duplicate_reply audited");

    // Identical re-post is an idempotent success, not an error (SPEC §5.4).
    let dup = b
        .call("cell_reply", json!({"ask_id": ask_id, "body": "first"}))
        .await
        .unwrap();
    assert_eq!(dup["status"], "duplicate");
    h.shutdown().await;
}

#[tokio::test]
async fn await_non_owned_ask_immediate_e_ask_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();

    let r = a
        .call("cell_ask", json!({"to_cell": "codex-audit", "body": "?"}))
        .await
        .unwrap();
    let ask_id = r["ask_id"].as_str().unwrap().to_string();

    // The responder is NOT the asker: immediate rejection, no wait.
    let t0 = Instant::now();
    let err = b
        .call("cell_await", json!({"ask_id": ask_id, "timeout_ms": 60000}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ASK_NOT_FOUND");
    assert!(t0.elapsed() < Duration::from_secs(1), "rejected before any wait");

    // Unknown ask_id: same immediate error.
    let err = a
        .call("cell_await", json!({"ask_id": "01980000-0000-7000-8000-000000000000"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ASK_NOT_FOUND");
    h.shutdown().await;
}

#[tokio::test]
async fn deadlock_fires_only_at_await_not_at_ask() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();

    // Both asks open fine: creation NEVER fails with E_WOULD_DEADLOCK (§6.1).
    let r1 = a
        .call("cell_ask", json!({"to_cell": "codex-audit", "body": "a->b"}))
        .await
        .unwrap();
    let ask_ab = r1["ask_id"].as_str().unwrap().to_string();
    let r2 = b
        .call("cell_ask", json!({"to_cell": "dev-senior", "body": "b->a"}))
        .await
        .unwrap();
    let ask_ba = r2["ask_id"].as_str().unwrap().to_string();

    // A blocks awaiting (a -> b) on a background task.
    let rt_clone = rt.clone();
    let tok_a = h.issued_tokens["dev-senior"].clone();
    let waiter = tokio::spawn(async move {
        let mut a2 = connect_as(&rt_clone, "dev-senior", &tok_a).await.unwrap();
        a2.call("cell_await", json!({"ask_id": ask_ab, "timeout_ms": 4000}))
            .await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // While (a -> b) is active, activating (b -> a) closes the cycle.
    let err = b
        .call("cell_await", json!({"ask_id": ask_ba, "timeout_ms": 4000}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_WOULD_DEADLOCK");
    assert!(has_event(&rt, "deadlock_prevented"), "deadlock_prevented audited");

    let waited = waiter.await.unwrap().unwrap();
    assert_eq!(waited["status"], "pending", "the awaiting side just times out");

    // Once no awaiter is active, the same await succeeds (edge released).
    let ok = b
        .call("cell_await", json!({"ask_id": ask_ba, "timeout_ms": 100}))
        .await
        .unwrap();
    assert_eq!(ok["status"], "pending");
    h.shutdown().await;
}

// ---------------------------------------------------------------- ACL (F9)

#[tokio::test]
async fn default_deny_broadcast_rejected_without_grant() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();
    let err = b
        .call("cell_broadcast", json!({"body": "hi all"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    h.shutdown().await;
}

#[tokio::test]
async fn admin_request_msg_type_requires_admin_registry() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;

    // dev-senior does NOT hold admin_registry.
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let err = a
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "drain please", "msg_type": "admin_request"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");

    // coordinator holds admin_registry: accepted, and the envelope carries it.
    let mut c = connect_as(&rt, "coordinator", &h.issued_tokens["coordinator"]).await.unwrap();
    let r = c
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "drain please", "msg_type": "admin_request"}),
        )
        .await
        .unwrap();
    assert_eq!(r["status"], "enqueued");
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();
    let inbox = b.call("cell_inbox", json!({})).await.unwrap();
    assert_eq!(inbox["messages"][0]["msg_type"], "admin_request");
    h.shutdown().await;
}

#[tokio::test]
async fn attach_files_requires_capability() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    // dev-senior does NOT hold attach_files.
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let err = a
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "see diff", "file_refs": ["x.diff"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    h.shutdown().await;
}

// ---------------------------------------------------------------- protected cells (D2, G1-01)

#[tokio::test]
async fn protected_send_ask_gated_by_per_target_grant() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;

    // codex-audit has NO send_to_protected/ask_protected grants.
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"]).await.unwrap();
    let err = b
        .call("cell_send", json!({"to_cell": "coordinator", "body": "hi"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    let err = b
        .call("cell_ask", json!({"to_cell": "coordinator", "body": "hi?"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    assert!(has_event(&rt, "protected_access_denied"), "denials audited");

    // dev-senior holds the per-target grants toward coordinator: allowed.
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let r = a
        .call("cell_send", json!({"to_cell": "coordinator", "body": "hi"}))
        .await
        .unwrap();
    assert_eq!(r["status"], "enqueued");
    let r = a
        .call("cell_ask", json!({"to_cell": "coordinator", "body": "hi?"}))
        .await
        .unwrap();
    assert_eq!(r["status"], "pending");

    // cell_reply by the legitimate addressee works regardless of protection:
    // coordinator (protected) asked dev-senior; dev-senior replies by ownership.
    let mut c = connect_as(&rt, "coordinator", &h.issued_tokens["coordinator"]).await.unwrap();
    let r = c
        .call("cell_ask", json!({"to_cell": "codex-audit", "body": "status?"}))
        .await
        .unwrap();
    let ask_id = r["ask_id"].as_str().unwrap().to_string();
    let rr = b
        .call("cell_reply", json!({"ask_id": ask_id, "body": "all green"}))
        .await
        .unwrap();
    assert_eq!(rr["status"], "recorded");
    h.shutdown().await;
}

#[tokio::test]
async fn broadcast_omits_protected_without_grant_and_audits() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;

    // coordinator holds broadcast but has NO send_to_protected toward vault.
    let mut c = connect_as(&rt, "coordinator", &h.issued_tokens["coordinator"]).await.unwrap();
    let r = c.call("cell_broadcast", json!({"body": "maintenance 18:00"})).await.unwrap();
    let recipients: Vec<String> = r["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(recipients.contains(&"dev-senior".to_string()));
    assert!(recipients.contains(&"codex-audit".to_string()));
    assert!(
        !recipients.contains(&"vault".to_string()),
        "G1-01: broadcast alone never reaches a protected cell"
    );
    let denied = audit_events(&rt)
        .into_iter()
        .filter(|e| e["kind"] == "protected_access_denied")
        .collect::<Vec<_>>();
    assert!(
        denied.iter().any(|e| e["to"] == "vault"),
        "denied protected recipient audited by name: {denied:?}"
    );
    assert!(has_event(&rt, "broadcast_fanned_out"));

    // dev-senior holds broadcast AND send_to_protected toward vault+coordinator:
    // the fan-out includes them.
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let r = a.call("cell_broadcast", json!({"body": "ping"})).await.unwrap();
    let recipients: Vec<String> = r["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(recipients.contains(&"vault".to_string()), "grant includes vault: {recipients:?}");
    assert!(recipients.contains(&"coordinator".to_string()));
    h.shutdown().await;
}

// ---------------------------------------------------------------- quotas

#[tokio::test]
async fn quota_pending_ask_cap_10() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    for i in 0..10 {
        a.call("cell_ask", json!({"to_cell": "codex-audit", "body": format!("q{i}")}))
            .await
            .unwrap();
    }
    let err = a
        .call("cell_ask", json!({"to_cell": "codex-audit", "body": "q10"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_QUOTA");
    assert!(has_event(&rt, "quota_exceeded"));
    h.shutdown().await;
}

#[tokio::test]
async fn queue_overflow_rejects_new_e_quota() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon_with_quota(
        &rt,
        QuotaConfig {
            queue_depth: 5,
            sender_per_min: 1000,
            ..Default::default()
        },
    )
    .await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    for i in 0..5 {
        a.call("cell_send", json!({"to_cell": "codex-audit", "body": format!("m{i}")}))
            .await
            .unwrap();
    }
    let err = a
        .call("cell_send", json!({"to_cell": "codex-audit", "body": "overflow"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_QUOTA", "reject-new, never oldest-drop");
    h.shutdown().await;
}

// ---------------------------------------------------------------- dedupe

#[tokio::test]
async fn same_idempotency_key_same_message_id() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let key = "0198aaaa-bbbb-7ccc-8ddd-eeeeffff0000";
    let r1 = a
        .call("cell_send", json!({"to_cell": "codex-audit", "body": "same", "idempotency_key": key}))
        .await
        .unwrap();
    let r2 = a
        .call("cell_send", json!({"to_cell": "codex-audit", "body": "same", "idempotency_key": key}))
        .await
        .unwrap();
    assert_eq!(r1["message_id"], r2["message_id"], "retries collapse");
    h.shutdown().await;
}

#[tokio::test]
async fn same_key_different_body_e_dup() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"]).await.unwrap();
    let key = "0198aaaa-bbbb-7ccc-8ddd-eeeeffff0001";
    a.call("cell_send", json!({"to_cell": "codex-audit", "body": "one", "idempotency_key": key}))
        .await
        .unwrap();
    let err = a
        .call("cell_send", json!({"to_cell": "codex-audit", "body": "two", "idempotency_key": key}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_DUP", "key bound to first submission's content");
    h.shutdown().await;
}
