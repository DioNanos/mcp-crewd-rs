//! Wire protocol framing — newline-delimited JSON-RPC over the Unix domain
//! socket (SPEC §17.1, §5). `read_frame` / `write_frame` are sync helpers used
//! by the daemon server, the `crew` client, and contract tests.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::error::BusError;

/// A request frame: `{"id","method","params"}`. `method` is one of the tool
/// names (`cell_send`, …), `handshake`, or operator RPCs (`op_status`,
/// `op_inspect`, `op_audit_verify`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRequest {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

/// A response frame: exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

/// Stable error carried in a response frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

impl WireResponse {
    /// Successful response carrying `result`.
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response built from a `BusError`; `code` is the stable `E_*`
    /// string (SPEC §13).
    pub fn err(id: u64, e: &BusError) -> Self {
        Self {
            id,
            result: None,
            error: Some(WireError {
                code: e.code().to_string(),
                message: format!("{e}"),
            }),
        }
    }
}

/// Write one JSON-RPC frame terminated by `\n`.
pub fn write_frame<W: Write>(writer: &mut W, req: &WireRequest) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(req).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Read one JSON-RPC frame (up to the next `\n`) and parse it.
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<WireRequest> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte)?;
        if n == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }
    if buf.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty frame (no line)",
        ));
    }
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ---- typed params for each tool (SPEC §5) ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendParams {
    pub to_cell: String,
    pub body: String,
    #[serde(default)]
    pub file_refs: Vec<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// Semantic type (SPEC §4.1), client-supplied and daemon-validated
    /// against the sender's capabilities. Default: `task`.
    #[serde(default)]
    pub msg_type: Option<crate::types::MsgType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskParams {
    pub to_cell: String,
    pub body: String,
    #[serde(default)]
    pub file_refs: Vec<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitParams {
    pub ask_id: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyParams {
    pub ask_id: String,
    pub body: String,
    #[serde(default)]
    pub file_refs: Vec<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxParams {
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroadcastParams {
    pub body: String,
    #[serde(default)]
    pub file_refs: Vec<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// Semantic type (SPEC §4.1), client-supplied and daemon-validated.
    /// Default: `note`.
    #[serde(default)]
    pub msg_type: Option<crate::types::MsgType>,
}

// ---- Cell fabric tool params (SPEC §20.4) ----

/// `cell_spawn` (SPEC §20.4). `cell` None → ephemeral (`~ephemeral-<uuid8>`);
/// `engine` mandatory on ephemeral, forbidden on named (`resolve_spawn_target`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSpawnParams {
    #[serde(default)]
    pub cell: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    pub task: String,
    pub idempotency_key: String,
    /// `background|wait` (default `background`).
    #[serde(default)]
    pub mode: Option<String>,
}

/// `cell_send_task`: follow-up job on a (possibly terminal) thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSendTaskParams {
    pub crewd_thread_id: String,
    pub message: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// `cell_status`. `crewd_thread_id` None → all threads owned by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellStatusParams {
    #[serde(default)]
    pub crewd_thread_id: Option<String>,
}

/// `cell_result`: structured result of a thread (SPEC §20.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellResultParams {
    pub crewd_thread_id: String,
}

/// `cell_cancel`: cancel in-flight jobs + interrupt + thread→interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellCancelParams {
    pub crewd_thread_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_error_carries_stable_code() {
        let r = WireResponse::err(7, &crate::error::BusError::Quota("slow down".into()));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"E_QUOTA\""));
        assert!(s.contains("\"id\":7"));
    }

    #[test]
    fn frame_roundtrip() {
        let req = WireRequest {
            id: 1,
            method: "cell_send".into(),
            params: serde_json::json!({"to_cell":"b","body":"hi"}),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        assert!(buf.ends_with(b"\n"));
        let back: WireRequest = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back.method, "cell_send");
    }

    #[test]
    fn send_params_defaults() {
        let p: SendParams =
            serde_json::from_value(serde_json::json!({"to_cell":"b","body":"x"})).unwrap();
        assert!(p.file_refs.is_empty());
        assert!(p.idempotency_key.is_none());
        assert!(p.ttl_seconds.is_none());
    }

    #[test]
    fn ask_params_defaults() {
        let p: AskParams =
            serde_json::from_value(serde_json::json!({"to_cell":"b","body":"?"})).unwrap();
        assert!(p.file_refs.is_empty());
        assert!(p.idempotency_key.is_none());
        assert!(p.ttl_seconds.is_none());
    }

    #[test]
    fn await_params_defaults() {
        let p: AwaitParams = serde_json::from_value(serde_json::json!({"ask_id":"a1"})).unwrap();
        assert!(p.timeout_ms.is_none());
    }

    #[test]
    fn reply_params_defaults() {
        let p: ReplyParams =
            serde_json::from_value(serde_json::json!({"ask_id":"a1","body":"ok"})).unwrap();
        assert!(p.file_refs.is_empty());
        assert!(p.idempotency_key.is_none());
    }

    #[test]
    fn inbox_params_defaults() {
        let p: InboxParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(p.limit.is_none());
    }

    #[test]
    fn broadcast_params_defaults() {
        let p: BroadcastParams = serde_json::from_value(serde_json::json!({"body":"hi"})).unwrap();
        assert!(p.file_refs.is_empty());
        assert!(p.idempotency_key.is_none());
        assert!(p.ttl_seconds.is_none());
    }
}
