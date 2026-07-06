//! Tool handlers (SPEC §5) and the shared `DaemonState`. The outbound check
//! order is normative (SPEC §5 / plan T10b): capability → recipient exists →
//! protected reach → body/key size → quota (sender + capability) → dedupe →
//! seq + persist + audit `enqueued`. `cell_await` activates a wait-for edge
//! only while blocked and releases it on every path; `cell_reply` enforces
//! ownership and wakes the asker.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crewd_core::acl::{AclHolder, Capability, ReachVia};
use crewd_core::adapter::DeliveryAdapter;
use crewd_core::audit::{AuditChain, AuditEventDraft, VerifyResult};
use crewd_core::cells::{resolve_spawn_target, EngineKind, SpawnTarget};
use crewd_core::engine::CellResult;
use crewd_core::error::BusError;
use crewd_core::jobs::{JobState, QUEUE_DEPTH_DEFAULT};
use crewd_core::principal::CellPrincipal;
use crewd_core::quota::{QuotaConfig, QuotaTracker};
use crewd_core::spawn::{SpawnOutcome, SpawnRequest};
use crewd_core::state::DeliveryState;
use crewd_core::store::{ReplyOutcome, Store};
use crewd_core::threads::{CellThread, ThreadState};
use crewd_core::tickets::WaitForGraph;
use crewd_core::types::{
    clamp_ttl, new_uuidv7, now_rfc3339, rfc3339_after, Envelope, Kind, MsgType, SPEC_VERSION, TAINT,
};
use crewd_core::wire::{
    AskParams, AwaitParams, BroadcastParams, CellCancelParams, CellResultParams,
    CellSendTaskParams, CellSpawnParams, CellStatusParams, InboxParams, ReplyParams, SendParams,
    WireRequest, WireResponse,
};

use crate::config::CrewdConfig;
use crate::server::SessionRegistry;

/// Shared daemon state, accessed by the dispatch loop and the delivery task.
/// `Store` is `Send` but not `Sync` (rusqlite), so every access goes through a
/// `Mutex`; locks are held for short, bounded critical sections.
pub struct DaemonState {
    /// Shared with the scheduler thread: one `Store` handle behind
    /// one mutex, so daemon dispatch and the engine loop see the same rows.
    pub store: Arc<Mutex<Store>>,
    pub acl: AclHolder,
    pub quota: Mutex<QuotaTracker>,
    pub quota_cfg: QuotaConfig,
    pub graph: Mutex<WaitForGraph>,
    pub adapters: Mutex<HashMap<String, Arc<dyn DeliveryAdapter>>>,
    pub ask_wakers: Mutex<HashMap<String, Arc<Notify>>>,
    pub audit: Arc<Mutex<AuditChain>>,
    pub registry: Arc<SessionRegistry>,
    pub cfg: CrewdConfig,
    /// Cancel control channel to the scheduler: the cell name of a
    /// cancelled thread is pushed here so the scheduler interrupts + kills the
    /// engine. `None` when no scheduler is wired (unit tests).
    pub cancel_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl DaemonState {
    /// Append + fsync a security-relevant audit event, **fail-closed** (SPEC
    /// §12.3: "MUST fsync each appended event before acknowledging the action
    /// it records"). A caller that acks an action to the client MUST propagate
    /// this `Result` with `?`, so no success is returned when the durable audit
    /// write fails. Rejection paths propagate it too (an audit-write failure
    /// then supersedes the domain error as `E_INTERNAL`).
    pub fn audit(
        &self,
        kind: &str,
        message_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        outcome: &str,
        detail: Option<Value>,
    ) -> Result<(), BusError> {
        let mut chain = self
            .audit
            .lock()
            .map_err(|_| BusError::Internal("audit chain lock poisoned".into()))?;
        chain
            .append(AuditEventDraft::new(
                kind, message_id, from, to, outcome, detail,
            ))
            .map(|_| ())
            .map_err(|e| BusError::Internal(format!("audit append/fsync failed: {e}")))
    }

    /// Best-effort audit for connection-teardown paths (handshake rejections)
    /// where there is no success ack to gate and the connection is being
    /// dropped regardless. The `Result` is intentionally discarded.
    pub fn audit_best_effort(
        &self,
        kind: &str,
        message_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        outcome: &str,
        detail: Option<Value>,
    ) {
        let _ = self.audit(kind, message_id, from, to, outcome, detail);
    }
}

// ---- helpers ----

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn body_sha256(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    hex(&h.finalize())
}

fn check_body(body: &str) -> Result<(), BusError> {
    if body.len() > crewd_core::types::MAX_BODY_BYTES {
        return Err(BusError::BodyTooLarge(format!(
            "body {} > {} bytes",
            body.len(),
            crewd_core::types::MAX_BODY_BYTES
        )));
    }
    Ok(())
}

/// SPEC §3.1 / I3.1 / I3.2: daemon-authoritative envelope fields supplied by
/// the client MUST be rejected with `E_INTERNAL` (`message_id`) / rejected
/// (`from_cell` et al.). `extra_allowed` lists tool-legitimate identifier
/// params (e.g. `ask_id` on `cell_reply`/`cell_await`).
fn reject_client_identity_keys(params: &Value, extra_allowed: &[&str]) -> Result<(), BusError> {
    const FORBIDDEN: &[&str] = &[
        "from_cell",
        "message_id",
        "ask_id",
        "seq",
        "kind",
        "taint",
        "created_at",
        "expires_at",
        "spec_version",
        "reply_to",
        "principal_capabilities",
    ];
    if let Some(obj) = params.as_object() {
        for key in obj.keys() {
            if FORBIDDEN.contains(&key.as_str()) && !extra_allowed.contains(&key.as_str()) {
                return Err(BusError::Internal(format!(
                    "daemon-authoritative field '{key}' must not be client-supplied (SPEC §3.1)"
                )));
            }
        }
    }
    Ok(())
}

/// SPEC §4.1: validate the semantic `msg_type` against the sender's
/// capabilities (`admin_request` → `admin_registry`; `ask` → `ask`;
/// `reply` → `reply`).
fn check_msg_type(state: &DaemonState, from: &str, msg_type: MsgType) -> Result<(), BusError> {
    let acl = state.acl.current();
    match msg_type {
        MsgType::AdminRequest => acl.check(from, Capability::AdminRegistry),
        MsgType::Ask => acl.check(from, Capability::Ask),
        MsgType::Reply => acl.check(from, Capability::Reply),
        _ => Ok(()),
    }
}

fn check_idem(key: &str) -> Result<(), BusError> {
    if key.len() > crewd_core::types::MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(BusError::BodyTooLarge(format!(
            "idempotency_key {} > {} bytes",
            key.len(),
            crewd_core::types::MAX_IDEMPOTENCY_KEY_BYTES
        )));
    }
    Ok(())
}

/// Deserialize typed tool params, mapping any JSON error to `E_INTERNAL`.
fn parse_params<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, BusError> {
    serde_json::from_value(v).map_err(|e| BusError::Internal(format!("bad params: {e}")))
}

/// `build_envelope` + daemon-derived `principal_capabilities` from the ACL
/// (SPEC §3.1, I3.7).
#[allow(clippy::too_many_arguments)]
fn build_envelope_with_caps(
    state: &DaemonState,
    kind: Kind,
    msg_type: MsgType,
    from: &str,
    to: Option<&str>,
    body: &str,
    file_refs: &[String],
    ask_id: Option<&str>,
    reply_to: Option<&str>,
    idempotency_key: &str,
    ttl_secs: u64,
    seq: u64,
) -> Envelope {
    let mut env = build_envelope(
        kind,
        msg_type,
        from,
        to,
        body,
        file_refs,
        ask_id,
        reply_to,
        idempotency_key,
        ttl_secs,
        seq,
    );
    env.principal_capabilities = state.acl.current().capabilities_of(from);
    env
}

#[allow(clippy::too_many_arguments)]
fn build_envelope(
    kind: Kind,
    msg_type: MsgType,
    from: &str,
    to: Option<&str>,
    body: &str,
    file_refs: &[String],
    ask_id: Option<&str>,
    reply_to: Option<&str>,
    idempotency_key: &str,
    ttl_secs: u64,
    seq: u64,
) -> Envelope {
    Envelope {
        spec_version: SPEC_VERSION.into(),
        message_id: new_uuidv7(),
        ask_id: ask_id.map(String::from),
        from_cell: from.into(),
        to_cell: to.map(String::from),
        kind,
        msg_type,
        // Populated by build_envelope_with_caps (ACL-derived, I3.7); empty
        // only for daemon-internal envelopes with no principal.
        principal_capabilities: vec![],
        created_at: now_rfc3339(),
        expires_at: rfc3339_after(ttl_secs),
        seq,
        idempotency_key: idempotency_key.into(),
        body: body.into(),
        file_refs: file_refs.to_vec(),
        taint: TAINT.into(),
        reply_to: reply_to.map(String::from),
    }
}

// ---- dispatch ----

pub async fn dispatch(
    state: Arc<DaemonState>,
    principal: &CellPrincipal,
    req: WireRequest,
) -> WireResponse {
    let from = principal.cell_id.clone();
    let res = match req.method.as_str() {
        "cell_send" => handle_send(&state, &from, req.params).await,
        "cell_ask" => handle_ask(&state, &from, req.params).await,
        "cell_await" => handle_await(state.clone(), &from, req.params).await,
        "cell_reply" => handle_reply(&state, &from, req.params).await,
        "cell_inbox" => handle_inbox(&state, &from, req.params).await,
        "cell_list" => handle_list(&state, &from, req.params).await,
        "cell_broadcast" => handle_broadcast(&state, &from, req.params).await,
        "cell_spawn" => handle_spawn(&state, &from, req.params).await,
        "cell_send_task" => handle_send_task(&state, &from, req.params).await,
        "cell_status" => handle_status(&state, &from, req.params).await,
        "cell_result" => handle_result(&state, &from, req.params).await,
        "cell_cancel" => handle_cancel(&state, &from, req.params).await,
        "report_tool_correlation" => {
            handle_report_tool_correlation(&state, &from, req.params).await
        }
        "op_status" => handle_op_status(&state, &from, req.params).await,
        "op_inspect" => handle_op_inspect(&state, &from, req.params).await,
        "op_audit_verify" => handle_op_audit_verify(&state, &from, req.params).await,
        other => Err(BusError::Internal(format!("unknown method: {other}"))),
    };
    match res {
        Ok(v) => WireResponse::ok(req.id, v),
        Err(e) => WireResponse::err(req.id, &e),
    }
}

async fn handle_send(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: SendParams = parse_params(params)?;
    let msg_type = p.msg_type.unwrap_or(MsgType::Task);
    // 1. Authorization first (G3-03): capability → recipient → protected reach.
    check_msg_type(state, from, msg_type)?;
    authorize_outbound(state, from, &p.to_cell, Capability::Send, ReachVia::Send)?;
    // 2. Input validation.
    check_idem(p.idempotency_key.as_deref().unwrap_or(""))?;
    check_body(&p.body)?;
    if !p.file_refs.is_empty() {
        state.acl.current().check(from, Capability::AttachFiles)?;
    }
    // 3. Admission control (queue depth + rate).
    admit_outbound(state, from, &p.to_cell, Capability::Send, 1)?;
    let key = p.idempotency_key.unwrap_or_else(new_uuidv7);
    let body_h = body_sha256(&p.body);
    let message_id = {
        let st = state.store.lock().expect("store");
        if let Some(hit) = st.dedupe_lookup(from, &key)? {
            if hit.body_sha256 == body_h {
                return Ok(json!({"message_id": hit.message_id, "status": "enqueued"}));
            } else {
                return Err(BusError::Dup(
                    "idempotency key reused with different body".into(),
                ));
            }
        }
        let seq = st.next_seq(from, &p.to_cell)?;
        let ttl = clamp_ttl(Kind::Send, p.ttl_seconds);
        let env = build_envelope_with_caps(
            state,
            Kind::Send,
            msg_type,
            from,
            Some(&p.to_cell),
            &p.body,
            &p.file_refs,
            None,
            None,
            &key,
            ttl,
            seq,
        );
        let mid = env.message_id.clone();
        st.insert_envelope(&env)?;
        // Audit BEFORE the message becomes deliverable: the envelope row alone
        // is inert (inbox joins on a delivery row). If the durable audit fails,
        // no delivery row / dedupe binding is written, so nothing can be
        // consumed without an `enqueued` event (G3-01). A retry re-creates a
        // fresh, fully-audited message; the orphan envelope is never delivered.
        state.audit(
            "enqueued",
            Some(&mid),
            Some(from),
            Some(&p.to_cell),
            "ok",
            None,
        )?;
        st.insert_delivery(&mid, &p.to_cell)?;
        st.dedupe_insert(from, &key, &mid, None, &body_h)?;
        mid
    };
    Ok(json!({"message_id": message_id, "status": "enqueued"}))
}

async fn handle_ask(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: AskParams = parse_params(params)?;
    // 1. Authorization first (G3-03): capability → recipient → protected reach.
    authorize_outbound(state, from, &p.to_cell, Capability::Ask, ReachVia::Ask)?;
    // 2. Input validation.
    check_idem(p.idempotency_key.as_deref().unwrap_or(""))?;
    check_body(&p.body)?;
    if !p.file_refs.is_empty() {
        state.acl.current().check(from, Capability::AttachFiles)?;
    }
    // 3. Admission control (queue depth + rate + pending-ask cap).
    admit_outbound(state, from, &p.to_cell, Capability::Ask, 1)?;
    // pending-ask cap (SPEC §14) — enforced before creating the ticket.
    let open = state
        .store
        .lock()
        .expect("store")
        .pending_asks_count(from)?;
    if let Err(e) = state
        .quota
        .lock()
        .expect("quota")
        .check_pending_ask_cap(open)
    {
        state.audit(
            "quota_exceeded",
            None,
            Some(from),
            Some(&p.to_cell),
            "rejected",
            None,
        )?;
        return Err(e);
    }
    let key = p.idempotency_key.unwrap_or_else(new_uuidv7);
    let body_h = body_sha256(&p.body);
    let ask_id = new_uuidv7();
    let ttl = clamp_ttl(Kind::Ask, p.ttl_seconds);
    let message_id = {
        let st = state.store.lock().expect("store");
        if let Some(hit) = st.dedupe_lookup(from, &key)? {
            if hit.body_sha256 == body_h {
                return Ok(
                    json!({"ask_id": hit.ask_id, "message_id": hit.message_id, "status": "pending"}),
                );
            } else {
                return Err(BusError::Dup(
                    "idempotency key reused with different body".into(),
                ));
            }
        }
        let seq = st.next_seq(from, &p.to_cell)?;
        let env = build_envelope_with_caps(
            state,
            Kind::Ask,
            MsgType::Ask,
            from,
            Some(&p.to_cell),
            &p.body,
            &p.file_refs,
            Some(&ask_id),
            None,
            &key,
            ttl,
            seq,
        );
        let mid = env.message_id.clone();
        st.insert_envelope(&env)?;
        // Audit BEFORE the ask becomes deliverable/awaitable (G3-01): no
        // delivery row, ask ticket, or dedupe binding is written until both
        // audit events are durable.
        state.audit(
            "enqueued",
            Some(&mid),
            Some(from),
            Some(&p.to_cell),
            "ok",
            None,
        )?;
        state.audit(
            "ask_opened",
            Some(&mid),
            Some(from),
            Some(&p.to_cell),
            "ok",
            Some(json!({"ask_id": ask_id})),
        )?;
        st.insert_delivery(&mid, &p.to_cell)?;
        st.insert_ask(&ask_id, &mid, from, &p.to_cell, &rfc3339_after(ttl))?;
        st.dedupe_insert(from, &key, &mid, Some(&ask_id), &body_h)?;
        mid
    };
    Ok(json!({"ask_id": ask_id, "message_id": message_id, "status": "pending"}))
}

/// RAII wrapper around a wait-for edge (SPEC §6.4). Releasing on `Drop`
/// guarantees the edge is removed on **every** exit path — including tokio
/// future cancellation (client disconnect while suspended on the await), which
/// a post-`.await` manual release cannot cover (G3-05).
struct AwaitEdgeGuard {
    state: Arc<DaemonState>,
    guard: Option<crewd_core::tickets::AwaitGuard>,
}

impl Drop for AwaitEdgeGuard {
    fn drop(&mut self) {
        if let Some(g) = self.guard.take() {
            if let Ok(mut graph) = self.state.graph.lock() {
                graph.release(g);
            }
        }
    }
}

async fn handle_await(
    state: Arc<DaemonState>,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &["ask_id"])?;
    let p: AwaitParams = parse_params(params)?;
    let ask = state
        .store
        .lock()
        .expect("store")
        .get_ask(&p.ask_id)?
        .ok_or_else(|| BusError::AskNotFound(format!("ask {} not found", p.ask_id)))?;
    // Ownership: only the asker may await (SPEC §5.3).
    if ask.from_cell != from {
        return Err(BusError::AskNotFound("await by non-asker".into()));
    }
    if ask.state == "answered" {
        return Ok(answered(&state, &ask));
    }
    if ask.state == "expired" {
        return Ok(json!({"status": "expired", "reply": Value::Null}));
    }
    // Activate the wait-for edge; cycle → E_WOULD_DEADLOCK (SPEC §6.4).
    let core_guard = match state
        .graph
        .lock()
        .expect("graph")
        .try_activate_await(from, &ask.to_cell)
    {
        Ok(g) => g,
        Err(e) => {
            state.audit(
                "deadlock_prevented",
                None,
                Some(from),
                Some(&ask.to_cell),
                "rejected",
                None,
            )?;
            return Err(e);
        }
    };
    // RAII: the edge is released when `_edge` drops — on return, on timeout, or
    // on future cancellation. No path can leak an active edge (G3-05).
    let _edge = AwaitEdgeGuard {
        state: state.clone(),
        guard: Some(core_guard),
    };
    let notify = {
        let mut w = state.ask_wakers.lock().expect("wakers");
        w.entry(p.ask_id.clone())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    };
    let timeout_ms = p.timeout_ms.unwrap_or(120_000).min(120_000);
    let res = tokio::time::timeout(Duration::from_millis(timeout_ms), notify.notified()).await;
    let ask2 = state
        .store
        .lock()
        .expect("store")
        .get_ask(&p.ask_id)?
        .unwrap_or(ask);
    match res {
        Ok(()) => match ask2.state.as_str() {
            "answered" => Ok(answered(&state, &ask2)),
            "expired" => Ok(json!({"status": "expired", "reply": Value::Null})),
            _ => Ok(json!({"status": "pending", "reply": Value::Null})),
        },
        Err(_) => Ok(json!({"status": "pending", "reply": Value::Null})),
    }
}

/// SPEC §5.3: the `reply` in an `answered` result is the FULL envelope of
/// kind = reply, not a stub. Falls back to the id-stub only if the envelope
/// row is unexpectedly missing.
fn answered(state: &DaemonState, ask: &crewd_core::store::AskRow) -> Value {
    let full = ask.reply_message_id.as_deref().and_then(|mid| {
        state
            .store
            .lock()
            .expect("store")
            .get_envelope(mid)
            .ok()
            .flatten()
    });
    match full {
        Some(env) => json!({
            "status": "answered",
            "reply": serde_json::to_value(&env).unwrap_or(Value::Null),
        }),
        None => json!({
            "status": "answered",
            "reply": {
                "message_id": ask.reply_message_id,
                "ask_id": ask.ask_id,
            }
        }),
    }
}

async fn handle_reply(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &["ask_id"])?;
    let p: ReplyParams = parse_params(params)?;
    check_idem(p.idempotency_key.as_deref().unwrap_or(""))?;
    check_body(&p.body)?;
    if !p.file_refs.is_empty() {
        state.acl.current().check(from, Capability::AttachFiles)?;
    }
    state.acl.current().check(from, Capability::Reply)?;
    let ask = state
        .store
        .lock()
        .expect("store")
        .get_ask(&p.ask_id)?
        .ok_or_else(|| BusError::AskNotFound(format!("ask {} not found", p.ask_id)))?;
    // Ownership: only the responder (ask.to_cell) may reply.
    if ask.to_cell != from {
        return Err(BusError::AskNotFound("reply by non-responder".into()));
    }
    let key = p.idempotency_key.unwrap_or_else(new_uuidv7);
    let ttl = clamp_ttl(Kind::Reply, None);
    let outcome_message_id;
    {
        let st = state.store.lock().expect("store");
        let seq = st.next_seq(from, &ask.from_cell)?;
        let env = build_envelope_with_caps(
            state,
            Kind::Reply,
            MsgType::Reply,
            from,
            Some(&ask.from_cell),
            &p.body,
            &p.file_refs,
            Some(&p.ask_id),
            Some(&ask.message_id),
            &key,
            ttl,
            seq,
        );
        match st.reply_precheck(&p.ask_id, &env)? {
            ReplyOutcome::Recorded => {
                let mid = env.message_id.clone();
                st.insert_envelope(&env)?;
                // Audit BEFORE committing the answer: `commit_reply` is what
                // makes the ask `answered` (consumable by `cell_await`), so the
                // durable `enqueued` event must land first. If it fails, the
                // ask stays `pending` and the asker keeps waiting — no answered
                // ask ever lacks a reply audit event (G3-01 / §12.3).
                state.audit(
                    "enqueued",
                    Some(&mid),
                    Some(from),
                    Some(&ask.from_cell),
                    "ok",
                    None,
                )?;
                st.commit_reply(&p.ask_id, &env)?;
                st.insert_delivery(&mid, &ask.from_cell)?;
                outcome_message_id = mid;
            }
            ReplyOutcome::DuplicateIdentical { message_id } => {
                return Ok(json!({"message_id": message_id, "status": "duplicate"}));
            }
            ReplyOutcome::Conflict => {
                drop(st);
                state.audit(
                    "duplicate_reply",
                    None,
                    Some(from),
                    Some(&ask.from_cell),
                    "dropped",
                    None,
                )?;
                return Err(BusError::ReplyExists(format!(
                    "differing reply for {}",
                    p.ask_id
                )));
            }
        }
    }
    // Wake the asker's await.
    if let Some(n) = state.ask_wakers.lock().expect("wakers").get(&p.ask_id) {
        n.notify_one();
    }
    Ok(json!({"message_id": outcome_message_id, "status": "recorded"}))
}

async fn handle_inbox(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: InboxParams = parse_params(params)?;
    state.acl.current().check(from, Capability::ReadInbox)?;
    let limit = p.limit.unwrap_or(50).min(200) as usize;
    let (msgs, more) = state.store.lock().expect("store").inbox(from, limit)?;
    let arr: Vec<Value> = msgs
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();
    // No audit on successful read (SPEC §5.8).
    Ok(json!({"messages": arr, "has_more": more}))
}

async fn handle_list(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    state.acl.current().check(from, Capability::ListCells)?;
    let cells: Vec<Value> = state
        .acl
        .current()
        .cells()
        .into_iter()
        .map(|(n, e)| json!({"name": n, "engine": e}))
        .collect();
    // SPEC §20.4 fabric section: per-cell active thread + queue depth.
    let fabric: Vec<Value> = {
        let st = state.store.lock().expect("store");
        st.cell_list_defs()?
            .into_iter()
            .map(|d| {
                let active = st.thread_active_for_cell(&d.name).ok().flatten();
                let queue_depth = st.job_active_count_for_cell(&d.name).unwrap_or(0);
                json!({
                    "cell": d.name,
                    "active_thread": active.map(|t| t.crewd_thread_id),
                    "queue_depth": queue_depth,
                })
            })
            .collect()
    };
    Ok(json!({"cells": cells, "fabric": fabric}))
}

// ---- Cell fabric handlers (SPEC §20.4 / §20.7) ----

/// `cell_spawn`: resolve target (named/ephemeral) + idempotent spawn. The
/// `spawn_idempotent` primitive audits `cell_spawn_requested` (fsync) BEFORE
/// any consumable record (SPEC §20.10 audit-before-mutation, already R1).
async fn handle_spawn(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: CellSpawnParams = parse_params(params)?;
    state.acl.current().check(from, Capability::Spawn)?;
    check_idem(&p.idempotency_key)?;

    // clients MUST NOT supply the reserved `~ephemeral-` (or any
    // `~`) prefix — only the daemon mints ephemeral names below.
    if let Some(c) = &p.cell {
        if c.starts_with('~') {
            return Err(BusError::PolicyDenied(format!(
                "cell name '{c}' uses the reserved '~' prefix (daemon-only)"
            )));
        }
    }
    let named = match &p.cell {
        Some(c) => state.store.lock().expect("store").cell_get(c)?,
        None => None,
    };
    let req_engine = match p.engine.as_deref() {
        None => None,
        Some(s) => Some(
            EngineKind::from_db_str(s)
                .ok_or_else(|| BusError::Internal(format!("unknown engine: {s}")))?,
        ),
    };
    let target = resolve_spawn_target(
        named,
        req_engine,
        p.model.clone(),
        p.profile.clone(),
        p.cwd.clone(),
    )?;
    let (cell_name, engine, model, profile, cwd) = match target {
        SpawnTarget::Named(def) => (def.name, def.engine, def.model, def.profile, def.cwd),
        SpawnTarget::Ephemeral {
            engine,
            model,
            profile,
            cwd,
        } => (ephemeral_cell_name(), engine, model, profile, cwd),
    };
    let worktree = p.worktree.as_deref();
    let req = SpawnRequest {
        caller: from,
        cell_name: &cell_name,
        idempotency_key: &p.idempotency_key,
        payload: &p.task,
    };
    let outcome = {
        let st = state.store.lock().expect("store");
        let mut chain = state.audit.lock().expect("audit");
        st.spawn_idempotent(
            &req,
            engine,
            model.as_deref(),
            profile.as_deref(),
            &cwd,
            worktree,
            QUEUE_DEPTH_DEFAULT,
            &mut chain,
        )?
    };
    let (tid, replayed) = match outcome {
        SpawnOutcome::Created(t) => (t.crewd_thread_id, false),
        SpawnOutcome::Replayed(t) => (t.crewd_thread_id, true),
    };
    Ok(json!({"crewd_thread_id": tid, "replayed": replayed}))
}

/// `cell_send_task`: follow-up job on a thread (terminal too). On a terminal
/// thread + engine without session resume (pi v0) → E_THREAD_NOT_RESUMABLE.
async fn handle_send_task(
    state: &DaemonState,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: CellSendTaskParams = parse_params(params)?;
    state.acl.current().check(from, Capability::Spawn)?;
    let st = state.store.lock().expect("store");
    let thread = st
        .thread_get(&p.crewd_thread_id)?
        .ok_or_else(|| BusError::UnknownCell(format!("thread not found: {}", p.crewd_thread_id)))?;
    let terminal = matches!(
        thread.state,
        ThreadState::Done
            | ThreadState::Interrupted
            | ThreadState::Timeout
            | ThreadState::FailedUnknown
    );
    if terminal && thread.engine_kind == EngineKind::Pi {
        return Err(BusError::ThreadNotResumable(format!(
            "pi v0 has no session resume for thread {}",
            p.crewd_thread_id
        )));
    }
    let job = st.job_enqueue(
        &p.crewd_thread_id,
        &thread.cell_name,
        &p.message,
        QUEUE_DEPTH_DEFAULT,
    )?;
    Ok(json!({"job_id": job.job_id}))
}

/// Ownership gate (SPEC §20.7): `created_by_principal` OPPURE `admin_registry`.
fn check_thread_owner(
    state: &DaemonState,
    from: &str,
    thread: &CellThread,
) -> Result<(), BusError> {
    if thread.created_by_principal == from {
        return Ok(());
    }
    state.acl.current().check(from, Capability::AdminRegistry)
}

fn thread_status_value(st: &Store, t: &CellThread) -> Result<Value, BusError> {
    let jobs = st.jobs_for_thread(&t.crewd_thread_id)?;
    Ok(json!({
        "crewd_thread_id": t.crewd_thread_id,
        "cell": t.cell_name,
        "state": t.state,
        "engine_proc_state": if matches!(t.state, ThreadState::Spawning | ThreadState::Running) { "up" } else { "down" },
        "jobs": jobs.len(),
        "engine_thread_id": t.engine_thread_id,
        "engine_turn_id": t.engine_turn_id,
        "engine_session_id": t.engine_session_id,
    }))
}

/// `cell_status`: state of a thread (owned/admin). Without `crewd_thread_id` →
/// empty list in v0 (requires per-principal enumeration, out of scope).
async fn handle_status(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: CellStatusParams = parse_params(params)?;
    let st = state.store.lock().expect("store");
    match &p.crewd_thread_id {
        Some(tid) => {
            let t = st
                .thread_get(tid)?
                .ok_or_else(|| BusError::UnknownCell(format!("thread not found: {tid}")))?;
            check_thread_owner(state, from, &t)?;
            Ok(json!({"threads": [thread_status_value(&st, &t)?]}))
        }
        None => {
            // v0: per-principal thread enumeration deferred; returns the
            // caller's active cell if present.
            let mut out = Vec::new();
            for d in st.cell_list_defs()? {
                if let Some(t) = st.thread_active_for_cell(&d.name)? {
                    if t.created_by_principal == from {
                        out.push(thread_status_value(&st, &t)?);
                    }
                }
            }
            Ok(json!({"threads": out}))
        }
    }
}

/// `cell_result`: structured result of a thread (SPEC §20.10), with separate
/// ID fields (crewd_thread_id ≠ engine_thread_id ≠ engine_session_id).
async fn handle_result(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: CellResultParams = parse_params(params)?;
    let st = state.store.lock().expect("store");
    let t = st
        .thread_get(&p.crewd_thread_id)?
        .ok_or_else(|| BusError::UnknownCell(format!("thread not found: {}", p.crewd_thread_id)))?;
    check_thread_owner(state, from, &t)?;
    let tail = st.journal_tail(&p.crewd_thread_id, 50)?;
    let exit_status = match t.state {
        ThreadState::Idle | ThreadState::Done => "done",
        ThreadState::Interrupted => "interrupted",
        ThreadState::Timeout => "timeout",
        ThreadState::FailedUnknown => "failed_unknown",
        ThreadState::Spawning | ThreadState::Running => "running",
    };
    let result = CellResult {
        final_answer: journal_final_answer(&tail), // smoke-T16 fix: SPEC §20.10, the tail is NOT the result
        event_tail: tail,
        artifact_refs: vec![],
        exit_status: exit_status.into(),
        crewd_thread_id: t.crewd_thread_id.clone(),
        engine_process_id: t.engine_process_id,
        engine_thread_id: t.engine_thread_id.clone(),
        engine_turn_id: t.engine_turn_id.clone(),
        engine_session_id: t.engine_session_id.clone(),
    };
    Ok(serde_json::to_value(&result).unwrap_or_else(|_| json!({})))
}

/// `cell_cancel`: job cancelled + thread→interrupted + audit cell_cancelled,
/// then a best-effort engine interrupt/kill via the scheduler control channel
/// Ownership-gated like status/result: the cancel is a spawn-class
/// mutation but a caller may only cancel its own thread (or hold admin).
async fn handle_cancel(state: &DaemonState, from: &str, params: Value) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: CellCancelParams = parse_params(params)?;
    state.acl.current().check(from, Capability::Spawn)?;
    let (cancelled_jobs, cell_name);
    {
        let st = state.store.lock().expect("store");
        let thread = st.thread_get(&p.crewd_thread_id)?.ok_or_else(|| {
            BusError::UnknownCell(format!("thread not found: {}", p.crewd_thread_id))
        })?;
        check_thread_owner(state, from, &thread)?;
        let jobs = st.jobs_for_thread(&p.crewd_thread_id)?;
        let mut n = 0u32;
        for j in &jobs {
            if matches!(
                j.state,
                JobState::Queued | JobState::Leased | JobState::Started
            ) {
                st.job_cancel(&j.job_id)?;
                n += 1;
            }
        }
        if thread.state == ThreadState::Running {
            st.thread_transition(&p.crewd_thread_id, ThreadState::Interrupted)?;
        }
        cancelled_jobs = n;
        cell_name = thread.cell_name.clone();
    }
    state.audit(
        "cell_cancelled",
        Some(&p.crewd_thread_id),
        Some(from),
        None,
        "cancelled",
        None,
    )?;
    // signal the scheduler to interrupt + kill the engine process.
    if let Some(tx) = &state.cancel_tx {
        let _ = tx.send(cell_name);
    }
    Ok(json!({"cancelled_jobs": cancelled_jobs}))
}

async fn handle_broadcast(
    state: &DaemonState,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    let p: BroadcastParams = parse_params(params)?;
    let broadcast_msg_type = p.msg_type.unwrap_or(MsgType::Note);
    check_msg_type(state, from, broadcast_msg_type)?;
    check_idem(p.idempotency_key.as_deref().unwrap_or(""))?;
    check_body(&p.body)?;
    if !p.file_refs.is_empty() {
        state.acl.current().check(from, Capability::AttachFiles)?;
    }
    let acl = state.acl.current();
    acl.check(from, Capability::Broadcast)?;
    let (included, denied) = acl.recipients_for_broadcast(from);
    let now = now_ms();
    let units = included.len().max(1) as u32;
    {
        let sender_res = state
            .quota
            .lock()
            .expect("quota")
            .note_and_check_sender(from, now, units);
        if let Err(e) = sender_res {
            state.audit("quota_exceeded", None, Some(from), None, "rejected", None)?;
            return Err(e);
        }
        let cap_res = state
            .quota
            .lock()
            .expect("quota")
            .note_and_check_capability(from, Capability::Broadcast, now);
        if let Err(e) = cap_res {
            state.audit("quota_exceeded", None, Some(from), None, "rejected", None)?;
            return Err(e);
        }
    }
    let key = p.idempotency_key.unwrap_or_else(new_uuidv7);
    let body_h = body_sha256(&p.body);
    let message_id = {
        let st = state.store.lock().expect("store");
        if let Some(hit) = st.dedupe_lookup(from, &key)? {
            if hit.body_sha256 == body_h {
                return Ok(
                    json!({"message_id": hit.message_id, "recipients": included, "status": "enqueued"}),
                );
            } else {
                return Err(BusError::Dup(
                    "idempotency key reused with different body".into(),
                ));
            }
        }
        let ttl = clamp_ttl(Kind::Broadcast, p.ttl_seconds);
        let env = build_envelope_with_caps(
            state,
            Kind::Broadcast,
            broadcast_msg_type,
            from,
            None,
            &p.body,
            &p.file_refs,
            None,
            None,
            &key,
            ttl,
            0,
        );
        let mid = env.message_id.clone();
        st.insert_envelope(&env)?;
        // Audit the fan-out (per-recipient protected denials + the fanned-out
        // event) BEFORE inserting any delivery row (G3-01): no recipient
        // becomes deliverable until the audit is durable.
        for d in &denied {
            state.audit(
                "protected_access_denied",
                Some(&mid),
                Some(from),
                Some(d),
                "denied",
                None,
            )?;
        }
        state.audit(
            "broadcast_fanned_out",
            Some(&mid),
            Some(from),
            None,
            "ok",
            Some(json!({"recipients": included.clone(), "denied": denied.clone()})),
        )?;
        for r in &included {
            st.insert_delivery(&mid, r)?;
        }
        st.dedupe_insert(from, &key, &mid, None, &body_h)?;
        mid
    };
    Ok(json!({"message_id": message_id, "recipients": included, "status": "enqueued"}))
}

#[derive(serde::Deserialize)]
struct ReportCorrelationParams {
    message_id: String,
    tool: String,
    high_risk: bool,
    outcome: String,
}

/// `report_tool_correlation` — recipient-side consumption report wiring the
/// audit causal chain (THREAT_MODEL §8.5; CR-B-04). Gated: only the
/// authenticated cell that actually received `message_id` may report. The
/// audit `msg_type` is daemon-authoritative, read from the stored envelope.
///
/// Mapping-decision v0.1: the causal record reuses the closed-set kind
/// `acked` (SPEC §12.2) — semantically a consumption-report of the recipient.
/// A dedicated `causal` kind would require a minor spec bump (§16) and is
/// deferred.
async fn handle_report_tool_correlation(
    state: &DaemonState,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &["message_id"])?;
    let p: ReportCorrelationParams = parse_params(params)?;
    // Gating (G3-08): the caller must have actually CONSUMED the message, not
    // merely be a registered recipient. Only a `delivered` or `acked` delivery
    // qualifies — a recipient cannot forge a causal record before pulling the
    // body via `cell_inbox`.
    let dstate = state
        .store
        .lock()
        .expect("store")
        .delivery_state(&p.message_id, from);
    if !matches!(
        dstate,
        Ok(DeliveryState::Delivered) | Ok(DeliveryState::Acked)
    ) {
        return Err(BusError::AclDenied(format!(
            "{from} has not consumed {} (delivery not in delivered/acked)",
            p.message_id
        )));
    }
    let env = state
        .store
        .lock()
        .expect("store")
        .get_envelope(&p.message_id)?
        .ok_or_else(|| BusError::Internal("envelope vanished".into()))?;
    state.audit(
        "acked",
        Some(&p.message_id),
        Some(from),
        None,
        "ok",
        Some(json!({
            "causal": true,
            "message_id": p.message_id,
            "tool": p.tool,
            "high_risk": p.high_risk,
            "outcome": p.outcome,
            "msg_type": env.msg_type.as_str(),
        })),
    )?;
    Ok(json!({"recorded": true}))
}

/// Authorization gate (SPEC §5 / §11): capability → recipient exists →
/// protected reach (audited denial). This MUST run **before** any size/quota
/// check so that a send/ask to a protected recipient without the per-target
/// grant always yields `E_ACL_DENIED` + `protected_access_denied`, never masked
/// by `E_QUOTA` / `E_BODY_TOO_LARGE` (G3-03). `ReachVia` selects Send/Ask.
fn authorize_outbound(
    state: &DaemonState,
    from: &str,
    to: &str,
    cap: Capability,
    via: ReachVia,
) -> Result<(), BusError> {
    let acl = state.acl.current();
    acl.check(from, cap)?;
    if !acl.is_registered(to) {
        return Err(BusError::UnknownCell(format!("{to} not registered")));
    }
    if let Err(e) = acl.check_reach(from, to, via) {
        state.audit(
            "protected_access_denied",
            None,
            Some(from),
            Some(to),
            "denied",
            None,
        )?;
        return Err(e);
    }
    Ok(())
}

/// Admission control (SPEC §14): queue-depth overflow (reject-new, never
/// oldest-drop) then per-sender + per-capability rate. Runs after
/// authorization and input validation. `units` is the sender-rate fan-out
/// count.
fn admit_outbound(
    state: &DaemonState,
    from: &str,
    to: &str,
    cap: Capability,
    units: u32,
) -> Result<(), BusError> {
    let pending = state
        .store
        .lock()
        .expect("store")
        .pending_deliveries_count(to)?;
    if let Err(e) = state
        .quota
        .lock()
        .expect("quota")
        .check_queue_depth(pending)
    {
        state.audit(
            "quota_exceeded",
            None,
            Some(from),
            Some(to),
            "rejected",
            None,
        )?;
        return Err(e);
    }
    let now = now_ms();
    let sender_res = state
        .quota
        .lock()
        .expect("quota")
        .note_and_check_sender(from, now, units);
    if let Err(e) = sender_res {
        state.audit(
            "quota_exceeded",
            None,
            Some(from),
            Some(to),
            "rejected",
            None,
        )?;
        return Err(e);
    }
    let cap_res = state
        .quota
        .lock()
        .expect("quota")
        .note_and_check_capability(from, cap, now);
    if let Err(e) = cap_res {
        state.audit(
            "quota_exceeded",
            None,
            Some(from),
            Some(to),
            "rejected",
            None,
        )?;
        return Err(e);
    }
    Ok(())
}

// ---- operator RPCs (SPEC §15): status / inspect / audit verify ----
// All gated on `read_audit`. The operator is an authenticated principal (a
// cell with the `read_audit` capability).

async fn handle_op_status(
    state: &DaemonState,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    state.acl.current().check(from, Capability::ReadAudit)?;
    let (pending, open) = {
        let st = state.store.lock().expect("store");
        (st.pending_deliveries_total()?, st.open_asks_total()?)
    };
    let head = state.audit.lock().expect("audit").head_hash().to_string();
    Ok(json!({"head_hash": head, "pending_deliveries": pending, "open_asks": open}))
}

#[derive(serde::Deserialize)]
struct InspectParams {
    id: String,
}

async fn handle_op_inspect(
    state: &DaemonState,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &["id"])?;
    state.acl.current().check(from, Capability::ReadAudit)?;
    let p: InspectParams = parse_params(params)?;
    let envelope = state.store.lock().expect("store").get_envelope(&p.id)?;
    let ask = state
        .store
        .lock()
        .expect("store")
        .get_ask(&p.id)
        .ok()
        .flatten();
    let events = read_audit_events_for(&state.cfg.audit_path(), &p.id)?;
    Ok(json!({"envelope": envelope, "ask": ask, "audit_events": events}))
}

async fn handle_op_audit_verify(
    state: &DaemonState,
    from: &str,
    params: Value,
) -> Result<Value, BusError> {
    reject_client_identity_keys(&params, &[])?;
    state.acl.current().check(from, Capability::ReadAudit)?;
    match AuditChain::verify(&state.cfg.audit_path()) {
        VerifyResult::Ok { head_hash, events } => {
            Ok(json!({"status": "ok", "head_hash": head_hash, "events": events}))
        }
        VerifyResult::Broken { event_id, index } => {
            Ok(json!({"status": "broken", "event_id": event_id, "index": index}))
        }
    }
}

/// Read the on-disk audit chain snapshot and return the events whose
/// `message_id` matches `id` (read-only; does not touch the live chain).
fn read_audit_events_for(path: &std::path::Path, id: &str) -> Result<Vec<Value>, BusError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| BusError::Internal(format!("audit read: {e}")))?;
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("message_id").and_then(|m| m.as_str()) == Some(id) {
                out.push(v);
            }
        }
    }
    Ok(out)
}

/// smoke-T16 fix (SPEC §20.10): `final_answer` is the payload of the last
/// `final:` event in the journal ("NNNN final: <text>" lines), not the raw tail.
/// SPEC §20.1: `~ephemeral-<uuid8>`. The suffix comes from the LAST 8 hex
/// digits of the UUIDv7 (random bits), NOT the first 8: those are the high
/// bits of the ms timestamp and stay identical for ~65s, making two
/// back-to-back spawns collide on the same cell name (`E_CELL_BUSY`,
/// regression 2026-07-05).
fn ephemeral_cell_name() -> String {
    let uuid = new_uuidv7();
    let short: String = uuid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("~ephemeral-{short}")
}

fn journal_final_answer(tail: &[String]) -> Option<String> {
    tail.iter().rev().find_map(|line| {
        let body = line.split_once(' ')?.1; // strip "NNNN "
        body.strip_prefix("final: ").map(|s| s.to_string())
    })
}

#[cfg(test)]
mod ephemeral_name_tests {
    use super::ephemeral_cell_name;

    /// Regression 2026-07-05: the first 8 hex digits of a UUIDv7 are the HIGH
    /// bits of the timestamp (they change every ~65s), so two back-to-back
    /// ephemeral spawns generated the SAME cell name → `E_CELL_BUSY` on the
    /// second one (parallel fan-out impossible). The suffix must come from
    /// the random bits.
    #[test]
    fn two_back_to_back_ephemeral_names_are_distinct() {
        let a = ephemeral_cell_name();
        let b = ephemeral_cell_name();
        assert_ne!(
            a, b,
            "ephemeral names within the same timestamp window must differ"
        );
        for n in [&a, &b] {
            assert!(n.starts_with("~ephemeral-"), "expected prefix: {n}");
            let suffix = n.strip_prefix("~ephemeral-").unwrap();
            assert_eq!(suffix.len(), 8, "uuid8 suffix: {n}");
            assert!(
                crewd_core::validators::validate_cell_name_allowing_ephemeral(n).is_ok(),
                "must pass the validator: {n}"
            );
        }
    }
}

#[cfg(test)]
mod phase2_result_tests {
    use super::journal_final_answer;

    /// smoke-T16: `final_answer` estratto dall'ultimo `final:` del journal.
    #[test]
    fn final_answer_from_journal_tail() {
        let tail = vec![
            "0001 accepted: t-1".to_string(),
            "0002 note: lavoro".to_string(),
            "0003 final: risultato vero".to_string(),
        ];
        assert_eq!(
            journal_final_answer(&tail).as_deref(),
            Some("risultato vero")
        );
        assert_eq!(
            journal_final_answer(&["0001 accepted: t-1".to_string()]),
            None
        );
        assert_eq!(journal_final_answer(&[]), None);
    }
}
