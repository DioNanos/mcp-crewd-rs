//! Hardening conformance (SPEC §17.3). These tests spawn the daemon in
//! process via `crewd::testkit` and assert the runtime-directory / socket
//! permission model, the symlink-refusal bind guard, and the L0 token check
//! at handshake.

use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn runtime_dir_0700_socket_0600() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let md = std::fs::metadata(&rt).unwrap();
    assert_eq!(md.permissions().mode() & 0o777, 0o700);
    let ms = std::fs::metadata(rt.join("crewd.sock")).unwrap();
    assert_eq!(ms.permissions().mode() & 0o777, 0o600);
    h.shutdown().await;
}

#[tokio::test]
async fn refuses_to_bind_on_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    std::fs::create_dir_all(&rt).unwrap();
    std::os::unix::fs::symlink("/tmp/elsewhere", rt.join("crewd.sock")).unwrap();
    assert!(crewd::testkit::try_spawn_daemon(&rt).await.is_err());
}

/// Regressione framing 2026-07-05 (audit pre-publish): `read_bounded_line`
/// scartava i byte DOPO il `\n` nello stesso `read()`, quindi due frame NDJSON
/// coalesced in un solo chunk (handshake + prima richiesta pipelined) perdevano
/// il secondo frame e il client restava appeso senza risposta.
#[tokio::test]
async fn coalesced_frames_in_one_write_are_both_served() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();

    let mut stream = tokio::net::UnixStream::connect(rt.join("crewd.sock"))
        .await
        .unwrap();
    // Handshake + cell_list in un UNICO write (frame coalesced).
    let handshake = serde_json::json!({
        "id": 1, "method": "handshake",
        "params": {"cell_id": "dev-senior", "token": token,
                    "spec_version": crewd_core::types::SPEC_VERSION}
    });
    let list = serde_json::json!({"id": 2, "method": "cell_list", "params": {}});
    let payload = format!("{handshake}\n{list}\n");
    stream.write_all(payload.as_bytes()).await.unwrap();

    // Devono arrivare DUE risposte newline-delimited entro il timeout.
    let mut buf = Vec::new();
    let deadline = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.iter().filter(|&&b| b == b'\n').count() >= 2 {
                break;
            }
        }
        buf
    })
    .await;
    let buf = deadline.expect(
        "seconda risposta mai arrivata: frame coalesced perso (bug read_bounded_line)",
    );
    let lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "attese 2 risposte, arrivate {}", lines.len());
    let r2: serde_json::Value = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(r2["id"], 2, "la seconda risposta deve essere il cell_list");
    assert!(r2["result"].is_object(), "cell_list deve avere result: {r2}");
    h.shutdown().await;
}

#[tokio::test]
async fn wrong_token_rejected_at_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    let h = crewd::testkit::spawn_daemon(&rt).await;
    let err = crewd::testkit::connect_as(&rt, "dev-senior", "WRONG_TOKEN")
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_AUTH_REJECTED");
    h.shutdown().await;
}
