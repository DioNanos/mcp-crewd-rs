//! Per-cell MCP stdio shim (`crew mcp`). Speaks MCP JSON-RPC 2.0 over stdio,
//! translating the 7 `cell_*` tools (SPEC §5) into wire calls to `crewd`. The
//! sender identity (`from_cell`) is NEVER accepted from the client — it is
//! derived daemon-side from this shim's authenticated connection (SPEC §3.2).
//!
//! Hand-rolled (like the other `mcp-*-rs` shims): `initialize` (protocol
//! "2024-11-05"), `tools/list` (with `inputSchema` per §5 and
//! `annotations.readOnlyHint` for `cell_await`/`cell_inbox`/`cell_list`),
//! `tools/call` → wire. `BusError` → MCP `isError: true` with `{code,message}`.
use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::ops;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Tools hidden in `--worker-mode` (SPEC §20.7 nesting OFF): the spawn surface
/// must never be exposed to a cell that is itself a worker, recursively.
const WORKER_HIDDEN: &[&str] = &["cell_spawn", "cell_send_task", "cell_cancel"];

fn is_worker_hidden(name: &str) -> bool {
    WORKER_HIDDEN.contains(&name)
}

/// Tool descriptors: the 7 `cell_*` bus tools (SPEC §5) + the 6 fabric tools
/// (SPEC §20.4). In `worker_mode` the 3 spawn tools are NOT listed (nesting OFF,
/// §20.7) — read-only fabric (`cell_status`/`cell_result`) and bus tools stay.
fn tools_list(worker_mode: bool) -> Vec<Value> {
    let s = json!({"type":"string"});
    let arr_str = json!({"type":"array","items":{"type":"string"}});
    let num = json!({"type":"integer"});
    let engine_enum = json!({"type":"string","enum":["codex","claude","pi"]});
    let profile_enum = json!({"type":"string","description":"engine profile name defined in crewd.toml [profile.<name>], or \"max\" for the host default credentials"});
    let mode_enum = json!({"type":"string","enum":["background","wait"],"default":"background"});
    let mut tools = vec![
        json!({
            "name":"cell_send",
            "description":"Send a fire-and-forget message to one cell.",
            "inputSchema":{"type":"object","properties":{
                "to_cell":s,"body":s,"file_refs":arr_str,"idempotency_key":s,
                "ttl_seconds":num,"msg_type":s
            },"required":["to_cell","body"]}
        }),
        json!({
            "name":"cell_ask",
            "description":"Open an ask ticket (non-blocking) expecting one reply.",
            "inputSchema":{"type":"object","properties":{
                "to_cell":s,"body":s,"file_refs":arr_str,"idempotency_key":s,
                "ttl_seconds":num
            },"required":["to_cell","body"]}
        }),
        json!({
            "name":"cell_await",
            "description":"Long-poll one ask reply (read-only).",
            "inputSchema":{"type":"object","properties":{
                "ask_id":s,"timeout_ms":num
            },"required":["ask_id"]},
            "annotations":{"readOnlyHint":true}
        }),
        json!({
            "name":"cell_reply",
            "description":"Post the single reply to an ask addressed to this cell.",
            "inputSchema":{"type":"object","properties":{
                "ask_id":s,"body":s,"file_refs":arr_str,"idempotency_key":s
            },"required":["ask_id","body"]}
        }),
        json!({
            "name":"cell_inbox",
            "description":"Pull pending messages addressed to this cell (read-only).",
            "inputSchema":{"type":"object","properties":{"limit":num}},
            "annotations":{"readOnlyHint":true}
        }),
        json!({
            "name":"cell_list",
            "description":"List registered cells + fabric section (read-only).",
            "inputSchema":{"type":"object","properties":{}},
            "annotations":{"readOnlyHint":true}
        }),
        json!({
            "name":"cell_broadcast",
            "description":"Fan-out to permitted recipients (explicit broadcast grant).",
            "inputSchema":{"type":"object","properties":{
                "body":s,"file_refs":arr_str,"idempotency_key":s,
                "ttl_seconds":num,"msg_type":s
            },"required":["body"]}
        }),
        // --- fabric: read-only (always exposed) ---
        json!({
            "name":"cell_status",
            "description":"Thread state + jobs + engine_proc_state (read-only).",
            "inputSchema":{"type":"object","properties":{
                "crewd_thread_id":s
            }},
            "annotations":{"readOnlyHint":true}
        }),
        json!({
            "name":"cell_result",
            "description":"Structured result of a thread, identity fields separate (read-only).",
            "inputSchema":{"type":"object","properties":{
                "crewd_thread_id":s
            },"required":["crewd_thread_id"]},
            "annotations":{"readOnlyHint":true}
        }),
    ];
    // --- fabric: spawn surface (HIDDEN in worker_mode, §20.7 nesting OFF) ---
    if !worker_mode {
        tools.push(json!({
            "name":"cell_spawn",
            "description":"Spawn/launch a cell worker thread (engine + profile + task).",
            "inputSchema":{"type":"object","properties":{
                "cell":s,"engine":engine_enum,"model":s,"profile":profile_enum,
                "cwd":s,"worktree":s,"task":s,"idempotency_key":s,"mode":mode_enum
            },"required":["task","idempotency_key"]}
        }));
        tools.push(json!({
            "name":"cell_send_task",
            "description":"Send a follow-up task to an existing thread.",
            "inputSchema":{"type":"object","properties":{
                "crewd_thread_id":s,"message":s,"idempotency_key":s
            },"required":["crewd_thread_id","message"]}
        }));
        tools.push(json!({
            "name":"cell_cancel",
            "description":"Cancel in-flight jobs + interrupt a thread.",
            "inputSchema":{"type":"object","properties":{
                "crewd_thread_id":s
            },"required":["crewd_thread_id"]}
        }));
    }
    tools
}

/// Run the MCP shim: read newline-delimited JSON-RPC requests from `reader`,
/// write responses to `writer`, translating `tools/call` into wire calls to
/// `crewd` authenticated as `cell`/`token` under `runtime_dir`.
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    runtime_dir: PathBuf,
    cell: String,
    token: String,
    worker_mode: bool,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut stream = ops::connect(&runtime_dir, &cell, &token)
        .await
        .map_err(|e| io_err(format!("{}: {}", e.code, e.message)))?;
    let mut reader = reader;
    let mut writer = writer;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // client closed stdin
        }
        let req: Value = match serde_json::from_str(line.trim_end()) {
            Ok(v) => v,
            Err(_) => continue, // ignore malformed lines
        };
        let id = req.get("id").cloned();
        let method = req
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let resp: Option<Value> = match method.as_str() {
            "initialize" => Some(json!({
                "jsonrpc":"2.0","id":id,
                "result":{
                    "protocolVersion":PROTOCOL_VERSION,
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"crew-mcp","version":"0.1"}
                }
            })),
            "tools/list" => Some(json!({
                "jsonrpc":"2.0","id":id,
                "result":{"tools":tools_list(worker_mode)}
            })),
            "tools/call" => match id.clone() {
                Some(idv) => Some(handle_tools_call(&mut stream, &params, idv, worker_mode).await),
                None => None, // notification: no response
            },
            _ => {
                // Unknown method: respond error if it has an id; else ignore.
                id.map(|idv| {
                    json!({"jsonrpc":"2.0","id":idv,
                           "error":{"code":-32601,"message":format!("method not found: {method}")}})
                })
            }
        };

        if let Some(r) = resp {
            let mut bytes = serde_json::to_vec(&r).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }
    }
    Ok(())
}

async fn handle_tools_call(
    stream: &mut tokio::net::UnixStream,
    params: &Value,
    id: Value,
    worker_mode: bool,
) -> Value {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // SPEC §20.7 nesting OFF: a hidden tool called in worker-mode is structurally
    // rejected as JSON-RPC method-not-found (-32601), not merely unlisted.
    if worker_mode && is_worker_hidden(&name) {
        return json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32601,"message":format!("method not found: {name}")}});
    }
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    // SPEC §3.2 (I3.2): `from_cell` is daemon-derived; never accepted.
    if let Some(obj) = arguments.as_object() {
        if obj.contains_key("from_cell") {
            return error_text(
                id,
                "E_INTERNAL",
                "from_cell is daemon-derived and must not be supplied (SPEC I3.2)",
            );
        }
    }
    match ops::call(stream, &name, arguments).await {
        Ok(result) => {
            let text = serde_json::to_string(&result).unwrap_or_else(|_| "null".into());
            json!({
                "jsonrpc":"2.0","id":id,
                "result":{"content":[{"type":"text","text":text}],"isError":false}
            })
        }
        Err(e) => {
            let text = json!({"code":e.code,"message":e.message}).to_string();
            json!({
                "jsonrpc":"2.0","id":id,
                "result":{"content":[{"type":"text","text":text}],"isError":true}
            })
        }
    }
}

fn error_text(id: Value, code: &str, message: &str) -> Value {
    let text = json!({"code":code,"message":message}).to_string();
    json!({
        "jsonrpc":"2.0","id":id,
        "result":{"content":[{"type":"text","text":text}],"isError":true}
    })
}

fn io_err(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}
