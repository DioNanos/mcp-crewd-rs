//! Tool-level smoke tests for the 7 `cell_*` tools via the in-process daemon
//! (T10b). The full conformance suite (§19.1) lands in Task 11–12; these cover
//! the happy paths and the most load-bearing rejections.

use serde_json::json;

fn token<'a>(h: &'a crewd::testkit::DaemonHandle, cell: &str) -> &'a str {
    h.issued_tokens
        .get(cell)
        .map(|s| s.as_str())
        .unwrap_or_else(|| panic!("no token issued for {cell}"))
}

#[tokio::test]
async fn cell_send_then_inbox_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let mut dev = crewd::testkit::connect_as(&rt, "dev-senior", token(&h, "dev-senior"))
        .await
        .unwrap();
    let mut aud = crewd::testkit::connect_as(&rt, "codex-audit", token(&h, "codex-audit"))
        .await
        .unwrap();
    dev.call("cell_send", json!({"to_cell":"codex-audit","body":"hi"}))
        .await
        .unwrap();
    let r = aud.call("cell_inbox", json!({})).await.unwrap();
    let msgs = r["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["body"], "hi");
    assert_eq!(msgs[0]["from_cell"], "dev-senior");
    assert_eq!(msgs[0]["taint"], "peer_untrusted");
    h.shutdown().await;
}

#[tokio::test]
async fn cell_ask_reply_await_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let mut dev = crewd::testkit::connect_as(&rt, "dev-senior", token(&h, "dev-senior"))
        .await
        .unwrap();
    let mut aud = crewd::testkit::connect_as(&rt, "codex-audit", token(&h, "codex-audit"))
        .await
        .unwrap();
    let ask = dev
        .call("cell_ask", json!({"to_cell":"codex-audit","body":"ping?"}))
        .await
        .unwrap();
    assert_eq!(ask["status"], "pending");
    let ask_id = ask["ask_id"].as_str().unwrap().to_string();
    // responder pulls the ask, then replies.
    aud.call("cell_inbox", json!({})).await.unwrap();
    let rep = aud
        .call("cell_reply", json!({"ask_id":&ask_id,"body":"pong"}))
        .await
        .unwrap();
    assert_eq!(rep["status"], "recorded");
    // await after the reply is in: returns answered immediately.
    let aw = dev
        .call("cell_await", json!({"ask_id":&ask_id,"timeout_ms":2000}))
        .await
        .unwrap();
    assert_eq!(aw["status"], "answered");
    h.shutdown().await;
}

#[tokio::test]
async fn cell_list_returns_names_and_engines_only() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let mut dev = crewd::testkit::connect_as(&rt, "dev-senior", token(&h, "dev-senior"))
        .await
        .unwrap();
    let r = dev.call("cell_list", json!({})).await.unwrap();
    let cells = r["cells"].as_array().unwrap();
    let names: Vec<&str> = cells.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"dev-senior"));
    assert!(names.contains(&"codex-audit"));
    assert!(names.contains(&"coordinator"));
    // no capability/token/liveness fields leaked
    for c in cells {
        assert!(c.get("capabilities").is_none());
        assert!(c.get("protected").is_none());
    }
    h.shutdown().await;
}

#[tokio::test]
async fn broadcast_without_capability_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let mut aud = crewd::testkit::connect_as(&rt, "codex-audit", token(&h, "codex-audit"))
        .await
        .unwrap();
    let err = aud
        .call("cell_broadcast", json!({"body":"hi"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    h.shutdown().await;
}

#[tokio::test]
async fn broadcast_fans_out_to_reachable_recipients() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let mut coord = crewd::testkit::connect_as(&rt, "coordinator", token(&h, "coordinator"))
        .await
        .unwrap();
    let r = coord
        .call("cell_broadcast", json!({"body":"fleet-wide"}))
        .await
        .unwrap();
    let recipients: Vec<&str> = r["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(recipients.contains(&"dev-senior"));
    assert!(recipients.contains(&"codex-audit"));
    assert!(!recipients.contains(&"coordinator"), "sender excluded");
    // both reachable recipients pull it via inbox
    let mut dev = crewd::testkit::connect_as(&rt, "dev-senior", token(&h, "dev-senior"))
        .await
        .unwrap();
    let mut aud = crewd::testkit::connect_as(&rt, "codex-audit", token(&h, "codex-audit"))
        .await
        .unwrap();
    assert_eq!(
        dev.call("cell_inbox", json!({})).await.unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        aud.call("cell_inbox", json!({})).await.unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.shutdown().await;
}

#[tokio::test]
async fn await_non_owned_ask_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    // dev-senior opens an ask to codex-audit.
    let mut dev = crewd::testkit::connect_as(&rt, "dev-senior", token(&h, "dev-senior"))
        .await
        .unwrap();
    let ask = dev
        .call("cell_ask", json!({"to_cell":"codex-audit","body":"?"}))
        .await
        .unwrap();
    let ask_id = ask["ask_id"].as_str().unwrap().to_string();
    // codex-audit (the responder, NOT the asker) tries to await its own ask → denied.
    let mut aud = crewd::testkit::connect_as(&rt, "codex-audit", token(&h, "codex-audit"))
        .await
        .unwrap();
    let err = aud
        .call("cell_await", json!({"ask_id":&ask_id,"timeout_ms":500}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ASK_NOT_FOUND");
    h.shutdown().await;
}

#[tokio::test]
async fn send_to_unknown_cell_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let mut dev = crewd::testkit::connect_as(&rt, "dev-senior", token(&h, "dev-senior"))
        .await
        .unwrap();
    let err = dev
        .call("cell_send", json!({"to_cell":"ghost","body":"x"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_UNKNOWN_CELL");
    h.shutdown().await;
}
