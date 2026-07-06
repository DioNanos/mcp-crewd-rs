//! Regression tests for the Phase 1 gate findings.
//! Each test locks in the fix for one finding so it cannot silently regress.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::json;

use crewd::testkit::{connect_as, spawn_daemon, spawn_daemon_with_quota};
use crewd_core::quota::QuotaConfig;

fn audit_events(runtime_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(runtime_dir.join("audit.jsonl")).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("audit line is JSON"))
        .collect()
}

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---- G3-01 — audit is fail-closed: no OK to client without a durable audit ----

#[tokio::test]
async fn g3_01_audit_write_failure_yields_no_ok() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"])
        .await
        .unwrap();

    // A first send succeeds and creates the audit chain file.
    a.call(
        "cell_send",
        json!({"to_cell": "codex-audit", "body": "one"}),
    )
    .await
    .unwrap();

    // Make the audit append impossible: replace the audit file with a directory.
    // `OpenOptions::append` on a directory path always fails (EISDIR), so the
    // fsync-before-ack contract (SPEC §12.3) forces the handler to return an
    // error instead of acking success.
    let audit_path = rt.join("audit.jsonl");
    std::fs::remove_file(&audit_path).unwrap();
    std::fs::create_dir(&audit_path).unwrap();

    let err = a
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "two"}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code, "E_INTERNAL",
        "no OK when the durable audit write fails"
    );

    // Stronger property (G3-01 re-verify): the un-audited message must never be
    // made deliverable. Reads are not audited (§5.8), so the recipient's inbox
    // works even with the audit path broken — and it must contain ONLY the
    // first, audited message. The delivery row for "two" was never written.
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"])
        .await
        .unwrap();
    let inbox = b.call("cell_inbox", json!({})).await.unwrap();
    let bodies: Vec<String> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        bodies,
        vec!["one".to_string()],
        "the un-audited send was never made consumable"
    );
    h.shutdown().await;
}

// ---- G3-04 — operator RPCs also reject daemon-authoritative params ----

#[tokio::test]
async fn g3_04_op_rpcs_reject_identity_params() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    // coordinator holds read_audit.
    let mut c = connect_as(&rt, "coordinator", &h.issued_tokens["coordinator"])
        .await
        .unwrap();

    // A daemon-authoritative field smuggled into an operator RPC must be
    // rejected at the wire boundary, not silently ignored.
    let err = c
        .call("op_status", json!({"from_cell": "vault"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_INTERNAL");
    let err = c
        .call(
            "op_audit_verify",
            json!({"principal_capabilities": ["admin_registry"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_INTERNAL");
    // A legitimate op_inspect call (only `id`) still works.
    let ok = c.call("op_status", json!({})).await.unwrap();
    assert!(ok.get("head_hash").is_some());
    h.shutdown().await;
}

// ---- G3-02 — auth_rejected is audited on a bad-token handshake ----

#[tokio::test]
async fn g3_02_wrong_token_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;

    let err = connect_as(&rt, "dev-senior", "WRONG_TOKEN")
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_AUTH_REJECTED");

    // Give the best-effort audit a moment to land, then assert the event.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let evs = audit_events(&rt);
    assert!(
        evs.iter()
            .any(|e| e["kind"] == "auth_rejected" && e["from"] == "dev-senior"),
        "auth_rejected event present for the rejected handshake: {evs:?}"
    );
    h.shutdown().await;
}

// ---- G3-03 — protected default-deny precedes size/quota checks ----

#[tokio::test]
async fn g3_03_protected_deny_precedes_body_too_large() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;

    // codex-audit has NO grant toward coordinator (protected). An oversized
    // body would trip E_BODY_TOO_LARGE if size were checked first; the deny
    // MUST win (G3-03).
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"])
        .await
        .unwrap();
    let huge = "x".repeat(70_000); // > 64 KiB
    let err = b
        .call("cell_send", json!({"to_cell": "coordinator", "body": huge}))
        .await
        .unwrap_err();
    assert_eq!(
        err.code, "E_ACL_DENIED",
        "protected deny precedes size check"
    );
    assert!(
        audit_events(&rt)
            .iter()
            .any(|e| e["kind"] == "protected_access_denied"),
        "denial audited"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn g3_03_protected_deny_precedes_queue_full() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    // Tiny queue so the protected recipient can be filled by a granted sender.
    let h = spawn_daemon_with_quota(
        &rt,
        QuotaConfig {
            queue_depth: 3,
            sender_per_min: 1000,
            ..Default::default()
        },
    )
    .await;

    // dev-senior IS granted toward coordinator: fill its queue.
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"])
        .await
        .unwrap();
    for i in 0..3 {
        a.call(
            "cell_send",
            json!({"to_cell": "coordinator", "body": format!("m{i}")}),
        )
        .await
        .unwrap();
    }
    // codex-audit is NOT granted. Even with the queue full, it must see the
    // authorization denial, not a queue-state oracle (E_QUOTA).
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"])
        .await
        .unwrap();
    let err = b
        .call(
            "cell_send",
            json!({"to_cell": "coordinator", "body": "probe"}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code, "E_ACL_DENIED",
        "protected deny precedes queue-depth check"
    );
    h.shutdown().await;
}

// ---- G3-05 — the wait-for edge is released after an await ends (RAII) ----

#[tokio::test]
async fn g3_05_edge_released_after_await_ends() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"])
        .await
        .unwrap();
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"])
        .await
        .unwrap();

    let r1 = a
        .call(
            "cell_ask",
            json!({"to_cell": "codex-audit", "body": "a->b"}),
        )
        .await
        .unwrap();
    let ask_ab = r1["ask_id"].as_str().unwrap().to_string();
    let r2 = b
        .call("cell_ask", json!({"to_cell": "dev-senior", "body": "b->a"}))
        .await
        .unwrap();
    let ask_ba = r2["ask_id"].as_str().unwrap().to_string();

    // A awaits a->b and times out; the RAII guard MUST release the edge on the
    // (drop) path so a subsequent reverse await sees no spurious cycle.
    let aw = a
        .call("cell_await", json!({"ask_id": ask_ab, "timeout_ms": 250}))
        .await
        .unwrap();
    assert_eq!(aw["status"], "pending");

    // With the edge released, b can now await b->a without E_WOULD_DEADLOCK.
    let ok = b
        .call("cell_await", json!({"ask_id": ask_ba, "timeout_ms": 100}))
        .await
        .unwrap();
    assert_eq!(
        ok["status"], "pending",
        "edge from the prior await was released"
    );
    h.shutdown().await;
}

// ---- G3-06 — DB / audit / token files are 0600 ----

#[tokio::test]
async fn g3_06_runtime_files_are_0600() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"])
        .await
        .unwrap();
    a.call("cell_send", json!({"to_cell": "codex-audit", "body": "x"}))
        .await
        .unwrap();

    assert_eq!(mode_of(&rt.join("crewd.db")), 0o600, "DB 0600");
    assert_eq!(mode_of(&rt.join("audit.jsonl")), 0o600, "audit store 0600");
    assert_eq!(
        mode_of(&rt.join("tokens/dev-senior.token")),
        0o600,
        "token 0600"
    );
    // WAL sidecar exists (journal_mode=WAL applied).
    assert!(rt.join("crewd.db-wal").exists(), "SQLite WAL enabled");
    h.shutdown().await;
}

// ---- G3-07 — bind refuses a foreign (non-socket) file at the socket path ----

#[tokio::test]
async fn g3_07_refuses_regular_file_at_socket_path() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::write(rt.join("crewd.sock"), b"not a socket").unwrap();
    assert!(
        crewd::testkit::try_spawn_daemon(&rt).await.is_err(),
        "a regular file at the socket path must be refused, not unlinked"
    );
}

// ---- G3-08 — causal report requires the message to be consumed first ----

#[tokio::test]
async fn g3_08_report_before_consume_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = spawn_daemon(&rt).await;
    let mut a = connect_as(&rt, "dev-senior", &h.issued_tokens["dev-senior"])
        .await
        .unwrap();
    let mut b = connect_as(&rt, "codex-audit", &h.issued_tokens["codex-audit"])
        .await
        .unwrap();

    let r = a
        .call(
            "cell_send",
            json!({"to_cell": "codex-audit", "body": "do X", "msg_type": "task"}),
        )
        .await
        .unwrap();
    let mid = r["message_id"].as_str().unwrap().to_string();

    // Report BEFORE pulling the inbox: delivery is still queued → denied.
    let err = b
        .call(
            "report_tool_correlation",
            json!({"message_id": mid, "tool": "deploy", "high_risk": true, "outcome": "ok"}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code, "E_ACL_DENIED",
        "cannot forge causality before consuming"
    );

    // After consuming via inbox (→ delivered), the report is accepted.
    b.call("cell_inbox", json!({})).await.unwrap();
    let ok = b
        .call(
            "report_tool_correlation",
            json!({"message_id": mid, "tool": "deploy", "high_risk": true, "outcome": "ok"}),
        )
        .await
        .unwrap();
    assert_eq!(ok["recorded"], true);
    h.shutdown().await;
}
