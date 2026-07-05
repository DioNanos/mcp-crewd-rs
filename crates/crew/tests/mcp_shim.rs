//! `crew mcp` MCP stdio shim — in-process via `tokio::io::duplex` (one pair for
//! the shim's stdin, one for stdout) with a real daemon spawned through
//! `crewd::testkit`. The third test is the Fase 1 smoke: two shims do
//! `cell_ask` → `cell_inbox` → `cell_reply` → `cell_await` end-to-end.
use crewd::testkit::{connect_as, spawn_daemon};
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

async fn write_req<W: AsyncWrite + Unpin>(w: &mut W, id: u64, method: &str, params: Value) {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let mut b = serde_json::to_vec(&req).unwrap();
    b.push(b'\n');
    w.write_all(&b).await.unwrap();
    w.flush().await.unwrap();
}

async fn read_resp<R: AsyncBufRead + Unpin>(r: &mut R) -> Value {
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

async fn call<W: AsyncWrite + Unpin, R: AsyncBufRead + Unpin>(
    w: &mut W,
    r: &mut R,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    write_req(w, id, method, params).await;
    read_resp(r).await
}

/// Parse the `text` field of a `tools/call` result as JSON.
fn text_json(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

/// Spawn a shim on fresh duplexes; returns the test-facing (writer, reader).
fn spawn_shim(
    rt: &Path,
    cell: &str,
    token: &str,
) -> (
    tokio::io::DuplexStream,
    BufReader<tokio::io::DuplexStream>,
) {
    spawn_shim_mode(rt, cell, token, false)
}

/// Spawn a shim with an explicit `worker_mode` (SPEC §20.7 nesting OFF).
fn spawn_shim_mode(
    rt: &Path,
    cell: &str,
    token: &str,
    worker_mode: bool,
) -> (
    tokio::io::DuplexStream,
    BufReader<tokio::io::DuplexStream>,
) {
    let (req_tx, req_rx) = tokio::io::duplex(8192);
    let (resp_tx, resp_rx) = tokio::io::duplex(8192);
    tokio::spawn(crew::mcp_shim::serve(
        BufReader::new(req_rx),
        resp_tx,
        rt.to_path_buf(),
        cell.to_string(),
        token.to_string(),
        worker_mode,
    ));
    (req_tx, BufReader::new(resp_rx))
}

#[tokio::test]
async fn tools_list_exposes_seven_cell_tools_with_hints() {
    let dir = tempfile::tempdir().unwrap();
    let h = spawn_daemon(dir.path()).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let (mut w, mut r) = spawn_shim(dir.path(), "dev-senior", &token);
    call(&mut w, &mut r, 1, "initialize", json!({})).await;
    let list = call(&mut w, &mut r, 2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    // 7 Fase 1 bus tools + fabric tools (Task 14); assert at least the 7 original.
    assert!(tools.len() >= 7, "expected at least 7 tools, got {}", tools.len());
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "cell_send",
        "cell_ask",
        "cell_await",
        "cell_reply",
        "cell_inbox",
        "cell_list",
        "cell_broadcast",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    let ro = |n: &str| -> Option<bool> {
        tools
            .iter()
            .find(|t| t["name"] == n)
            .and_then(|t| t["annotations"]["readOnlyHint"].as_bool())
    };
    assert_eq!(ro("cell_await"), Some(true));
    assert_eq!(ro("cell_inbox"), Some(true));
    assert_eq!(ro("cell_list"), Some(true));
    assert_eq!(ro("cell_send"), None); // not read-only
    h.shutdown().await;
}

#[tokio::test]
async fn shim_rejects_from_cell_param() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path();
    let h = spawn_daemon(rt).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let (mut w, mut r) = spawn_shim(rt, "dev-senior", &token);
    call(&mut w, &mut r, 1, "initialize", json!({})).await;
    let res = call(
        &mut w,
        &mut r,
        2,
        "tools/call",
        json!({"name":"cell_send","arguments":{
            "to_cell":"codex-audit","body":"hi","from_cell":"evil"}}),
    )
    .await;
    assert_eq!(res["result"]["isError"], true);
    let text = res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("from_cell"), "got {text}");
    // Nothing was forwarded: codex-audit's inbox is empty.
    let tok_b = h.issued_tokens.get("codex-audit").unwrap().clone();
    let mut c = connect_as(rt, "codex-audit", &tok_b).await.unwrap();
    let inbox = c.call("cell_inbox", json!({})).await.unwrap();
    assert!(inbox["messages"].as_array().unwrap().is_empty());
    h.shutdown().await;
}

#[tokio::test]
async fn end_to_end_ask_reply_via_two_shims() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path();
    let h = spawn_daemon(rt).await;
    let tok_a = h.issued_tokens.get("dev-senior").unwrap().clone();
    let tok_b = h.issued_tokens.get("codex-audit").unwrap().clone();

    let (mut aw, mut ar) = spawn_shim(rt, "dev-senior", &tok_a);
    let (mut bw, mut br) = spawn_shim(rt, "codex-audit", &tok_b);
    call(&mut aw, &mut ar, 1, "initialize", json!({})).await;
    call(&mut bw, &mut br, 1, "initialize", json!({})).await;

    // A asks B.
    let ask = call(
        &mut aw,
        &mut ar,
        2,
        "tools/call",
        json!({"name":"cell_ask","arguments":{"to_cell":"codex-audit","body":"ping?"}}),
    )
    .await;
    assert_eq!(ask["result"]["isError"], false);
    let ask_id = {
        let aj = text_json(&ask);
        aj["ask_id"].as_str().unwrap().to_string()
    };

    // B pulls the ask from its inbox.
    let inbox = call(
        &mut bw,
        &mut br,
        2,
        "tools/call",
        json!({"name":"cell_inbox","arguments":{}}),
    )
    .await;
    let ib = text_json(&inbox);
    let msgs = ib["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "B sees the ask");
    assert_eq!(msgs[0]["body"], "ping?");

    // B replies.
    let reply = call(
        &mut bw,
        &mut br,
        3,
        "tools/call",
        json!({"name":"cell_reply","arguments":{"ask_id":&ask_id,"body":"pong"}}),
    )
    .await;
    let rj = text_json(&reply);
    assert_eq!(rj["status"], "recorded");

    // A awaits the reply.
    let awaited = call(
        &mut aw,
        &mut ar,
        3,
        "tools/call",
        json!({"name":"cell_await","arguments":{"ask_id":&ask_id,"timeout_ms":3000}}),
    )
    .await;
    let a = text_json(&awaited);
    assert_eq!(a["status"], "answered", "got {a}");
    assert_eq!(a["reply"]["body"], "pong");
    h.shutdown().await;
}

// --- Task 14: fabric tools + worker-mode (nesting OFF §20.7) ---

#[tokio::test]
async fn tools_list_default_exposes_six_fabric_tools() {
    let dir = tempfile::tempdir().unwrap();
    let h = spawn_daemon(dir.path()).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let (mut w, mut r) = spawn_shim(dir.path(), "dev-senior", &token);
    call(&mut w, &mut r, 1, "initialize", json!({})).await;
    let list = call(&mut w, &mut r, 2, "tools/list", json!({})).await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "cell_list",
        "cell_spawn",
        "cell_send_task",
        "cell_status",
        "cell_result",
        "cell_cancel",
    ] {
        assert!(
            names.contains(&expected),
            "default mode missing fabric tool {expected}: {names:?}"
        );
    }
    h.shutdown().await;
}

#[tokio::test]
async fn tools_list_worker_mode_hides_spawn_surface() {
    let dir = tempfile::tempdir().unwrap();
    let h = spawn_daemon(dir.path()).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let (mut w, mut r) = spawn_shim_mode(dir.path(), "dev-senior", &token, true);
    call(&mut w, &mut r, 1, "initialize", json!({})).await;
    let list = call(&mut w, &mut r, 2, "tools/list", json!({})).await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // SPEC §20.7 nesting OFF: the spawn surface is hidden in worker-mode.
    for hidden in ["cell_spawn", "cell_send_task", "cell_cancel"] {
        assert!(
            !names.contains(&hidden),
            "worker-mode must NOT expose {hidden}: {names:?}"
        );
    }
    // read-only fabric + bus tools stay available.
    for ro in ["cell_status", "cell_result", "cell_list", "cell_inbox"] {
        assert!(
            names.contains(&ro),
            "worker-mode must still expose read-only {ro}: {names:?}"
        );
    }
    h.shutdown().await;
}

#[tokio::test]
async fn cell_spawn_in_worker_mode_is_method_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let h = spawn_daemon(dir.path()).await;
    let token = h.issued_tokens.get("dev-senior").unwrap().clone();
    let (mut w, mut r) = spawn_shim_mode(dir.path(), "dev-senior", &token, true);
    call(&mut w, &mut r, 1, "initialize", json!({})).await;
    let resp = call(
        &mut w,
        &mut r,
        2,
        "tools/call",
        json!({"name":"cell_spawn","arguments":{
            "task":"x","idempotency_key":"k","engine":"codex"
        }}),
    )
    .await;
    // Nesting OFF is STRUCTURAL (§20.7), not documentary: a hidden tool called
    // in worker-mode is rejected as JSON-RPC method-not-found (-32601).
    assert_eq!(resp["error"]["code"], -32601, "got {resp}");
    h.shutdown().await;
}
