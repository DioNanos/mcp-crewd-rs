//! `crew` operator client (SPEC §15): `status`, `inspect`, `audit verify`.
//!
//! Connects to `crewd` over the Unix socket as a `read_audit`-capable cell and
//! renders the §15 output contract: `audit verify` prints exactly
//! `OK <head_hash>` on an intact chain and `BROKEN at <event_id>` on the first
//! broken event.
use std::path::Path;

use crewd_core::principal::ClientProof;
use crewd_core::types::SPEC_VERSION;
use crewd_core::wire::{WireError, WireRequest, WireResponse};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const MAX_FRAME: usize = 192 * 1024;

fn wire_err(code: &str, msg: impl Into<String>) -> WireError {
    WireError {
        code: code.into(),
        message: msg.into(),
    }
}

async fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>, WireError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
        if n == 0 {
            break;
        }
        if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..pos]);
            break;
        } else {
            buf.extend_from_slice(&chunk[..n]);
        }
        if buf.len() > MAX_FRAME {
            return Err(wire_err("E_INTERNAL", "frame too large"));
        }
    }
    Ok(buf)
}

pub(crate) async fn connect(
    runtime_dir: &Path,
    cell: &str,
    token: &str,
) -> Result<UnixStream, WireError> {
    let sock = runtime_dir.join("crewd.sock");
    let mut stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    let proof = ClientProof {
        cell_id: cell.into(),
        token: token.into(),
        spec_version: SPEC_VERSION.into(),
    };
    let req = WireRequest {
        id: 1,
        method: "handshake".into(),
        params: serde_json::to_value(&proof).map_err(|e| wire_err("E_INTERNAL", e.to_string()))?,
    };
    let mut bytes =
        serde_json::to_vec(&req).map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    let raw = read_line(&mut stream).await?;
    let resp: WireResponse =
        serde_json::from_slice(&raw).map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    if let Some(err) = resp.error {
        return Err(err);
    }
    Ok(stream)
}

pub(crate) async fn call(
    stream: &mut UnixStream,
    method: &str,
    params: Value,
) -> Result<Value, WireError> {
    let req = WireRequest {
        id: 2,
        method: method.into(),
        params,
    };
    let mut bytes =
        serde_json::to_vec(&req).map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    let raw = read_line(stream).await?;
    let resp: WireResponse =
        serde_json::from_slice(&raw).map_err(|e| wire_err("E_INTERNAL", e.to_string()))?;
    if let Some(err) = resp.error {
        return Err(err);
    }
    Ok(resp.result.unwrap_or(Value::Null))
}

/// §15 contract: `OK <head_hash>` on an intact chain, `BROKEN at <event_id>`
/// on the first broken event. A `read_audit` gate failure surfaces as `Err`.
pub async fn audit_verify(
    runtime_dir: &Path,
    cell: &str,
    token: &str,
) -> Result<String, String> {
    let mut stream = connect(runtime_dir, cell, token)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    let res = call(&mut stream, "op_audit_verify", json!({}))
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    let status = res.get("status").and_then(|v| v.as_str()).unwrap_or("");
    match status {
        "ok" => {
            let head = res.get("head_hash").and_then(|v| v.as_str()).unwrap_or("");
            Ok(format!("OK {head}"))
        }
        "broken" => {
            let event_id = res
                .get("event_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Ok(format!("BROKEN at {event_id}"))
        }
        other => Err(format!("unexpected op_audit_verify status: {other}")),
    }
}

/// Daemon health snapshot (head hash, queue depths, open asks), pretty JSON.
pub async fn status(runtime_dir: &Path, cell: &str, token: &str) -> Result<String, String> {
    let mut stream = connect(runtime_dir, cell, token)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    let res = call(&mut stream, "op_status", json!({}))
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    Ok(serde_json::to_string_pretty(&res).unwrap_or_default())
}

/// Envelope + ask + matching audit events for `id`, pretty JSON.
pub async fn inspect(
    runtime_dir: &Path,
    cell: &str,
    token: &str,
    id: &str,
) -> Result<String, String> {
    let mut stream = connect(runtime_dir, cell, token)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    let res = call(&mut stream, "op_inspect", json!({ "id": id }))
        .await
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    Ok(serde_json::to_string_pretty(&res).unwrap_or_default())
}
