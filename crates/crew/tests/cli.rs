//! `crew` operator CLI (SPEC §15). The daemon is spawned in-process via
//! `crewd::testkit`; the `crew::ops` functions render the §15 output contract
//! (`OK <head_hash>` / `BROKEN at <event_id>`).
use std::path::Path;

use crewd::testkit::{connect_as, spawn_daemon};
use serde_json::json;

async fn two_sends(rt: &Path, cell: &str, token: &str, to: &str) {
    let mut c = connect_as(rt, cell, token).await.expect("connect");
    c.call("cell_send", json!({"to_cell": to, "body": "one"}))
        .await
        .expect("send1");
    c.call("cell_send", json!({"to_cell": to, "body": "two"}))
        .await
        .expect("send2");
}

#[tokio::test]
async fn audit_verify_prints_ok_then_broken_after_flip() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path();
    let h = spawn_daemon(rt).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    two_sends(rt, "dev-senior", &token, "codex-audit").await;

    let ok = crew::ops::audit_verify(rt, "dev-senior", &token)
        .await
        .unwrap();
    assert!(ok.starts_with("OK "), "got {ok}");

    // Flip one byte in the on-disk chain; verify must now fail closed.
    let audit_path = rt.join("audit.jsonl");
    let mut raw = std::fs::read_to_string(&audit_path).unwrap();
    raw = raw.replacen("\"outcome\":\"ok\"", "\"outcome\":\"OK\"", 1);
    std::fs::write(&audit_path, raw).unwrap();

    let broken = crew::ops::audit_verify(rt, "dev-senior", &token)
        .await
        .unwrap();
    assert!(broken.starts_with("BROKEN at "), "got {broken}");
    h.shutdown().await;
}

#[tokio::test]
async fn status_reports_head_hash_and_queues() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path();
    let h = spawn_daemon(rt).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let mut c = connect_as(rt, "dev-senior", &token).await.unwrap();
    c.call("cell_send", json!({"to_cell": "codex-audit", "body": "x"}))
        .await
        .unwrap();
    let out = crew::ops::status(rt, "dev-senior", &token).await.unwrap();
    assert!(out.contains("head_hash"), "{out}");
    assert!(out.contains("pending_deliveries"), "{out}");
    h.shutdown().await;
}

#[tokio::test]
async fn inspect_returns_envelope_and_audit_events() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path();
    let h = spawn_daemon(rt).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let mut c = connect_as(rt, "dev-senior", &token).await.unwrap();
    let res = c
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "hello"}),
        )
        .await
        .unwrap();
    let mid = res
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let out = crew::ops::inspect(rt, "dev-senior", &token, &mid)
        .await
        .unwrap();
    assert!(out.contains("hello"), "envelope body round-trips: {out}");
    assert!(out.contains("audit_events"), "{out}");
    h.shutdown().await;
}

#[tokio::test]
async fn cli_requires_read_audit() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path();
    let h = spawn_daemon(rt).await;
    // codex-audit has NO read_audit capability in the test ACL.
    let token = h.issued_tokens.get("codex-audit").unwrap().clone();
    let err = crew::ops::audit_verify(rt, "codex-audit", &token)
        .await
        .unwrap_err();
    assert!(err.contains("E_ACL_DENIED"), "got {err}");
    h.shutdown().await;
}
