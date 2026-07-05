//! In-process daemon test harness with full tool dispatch. `spawn_daemon`
//! builds the shared `DaemonState`, bootstraps the hardened listener, and
//! spawns the serve + delivery loops. `connect_as` performs the UDS handshake;
//! `CellConn::call` issues a tool call. `fake_adapter` is exposed (not installed
//! by default, so messages stay inbox-pullable); tests that drive delivery
//! install it via `install_adapter`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crewd_core::acl::AclHolder;
use crewd_core::adapter::{DeliveryAdapter, FakeAdapter};
use crewd_core::audit::AuditChain;
use crewd_core::principal::{ClientProof, CredentialIssuer, CredentialScope};
use crewd_core::quota::{QuotaConfig, QuotaTracker};
use crewd_core::store::Store;
use crewd_core::tickets::WaitForGraph;
use crewd_core::types::SPEC_VERSION;
use crewd_core::wire::{WireError, WireRequest, WireResponse};

use serde_json::json;

use crate::auth::{FileIssuer, L0TokenBackend};
use crate::config::CrewdConfig;
use crate::delivery;
use crate::handlers::DaemonState;
use crate::server;
use crate::server::SessionRegistry;

const TEST_ACL: &str = r#"
[cell.dev-senior]
engine = "claude"
capabilities = ["send","ask","reply","broadcast","read_inbox","list_cells","read_audit","spawn"]
send_to_protected = ["coordinator","vault"]
ask_protected = ["coordinator"]
[cell.codex-audit]
engine = "codex"
capabilities = ["send","ask","reply","read_inbox","list_cells","attach_files","spawn"]
[cell.coordinator]
engine = "claude"
protected = true
capabilities = ["send","ask","reply","broadcast","read_inbox","list_cells","attach_files","admin_registry","read_audit"]
[cell.vault]
engine = "claude"
protected = true
capabilities = ["send","ask","reply","read_inbox","list_cells"]
"#;

const TEST_CELLS: &[&str] = &["dev-senior", "codex-audit", "coordinator", "vault"];

pub struct DaemonHandle {
    pub issued_tokens: HashMap<String, String>,
    pub fake_adapter: FakeAdapter,
    pub state: Arc<DaemonState>,
    issuer: Arc<FileIssuer>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    shutdown_deliver_tx: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
    join_deliver: Option<JoinHandle<()>>,
    sched_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    sched_join: Option<std::thread::JoinHandle<()>>,
    runtime_dir: PathBuf,
}

impl DaemonHandle {
    /// Install an adapter for `cell` in the delivery registry.
    pub fn install_adapter(&self, cell: &str, adapter: Arc<dyn DeliveryAdapter>) {
        self.state
            .adapters
            .lock()
            .expect("adapters")
            .insert(cell.to_string(), adapter);
    }

    /// Revoke the live credential of `cell`: marks the on-disk record, closes
    /// every live session using that `token_id`, and appends a `token_revoked`
    /// audit event (SPEC §17.2 MUST; CR-B-07).
    pub fn revoke(&self, cell: &str) -> Result<(), crewd_core::error::BusError> {
        let token_id = self.issuer.current_token_id(cell).ok_or_else(|| {
            crewd_core::error::BusError::Internal(format!("no token record for {cell}"))
        })?;
        self.issuer.revoke(cell, &token_id)?;
        self.state.registry.revoke(&token_id);
        self.state.audit(
            "token_revoked",
            None,
            Some(cell),
            None,
            "revoked",
            Some(json!({"token_id": token_id})),
        )?;
        Ok(())
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.shutdown_deliver_tx.take() {
            let _ = tx.send(());
        }
        if let Some(stop) = self.sched_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(j) = self.sched_join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
        if let Some(j) = self.join_deliver.take() {
            let _ = j.await;
        }
        let _ = std::fs::remove_file(self.runtime_dir.join("crewd.sock"));
    }
}

fn io_err<E: std::fmt::Debug>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, format!("{e:?}"))
}

async fn build_and_start(runtime_dir: &Path) -> std::io::Result<DaemonHandle> {
    build_and_start_with_quota(runtime_dir, QuotaConfig::default()).await
}

#[allow(clippy::too_many_arguments)]
async fn build_and_start_full(
    runtime_dir: &Path,
    quota_cfg: QuotaConfig,
    max_attempts: u32,
    backoff_base_secs: u64,
    backoff_cap_secs: u64,
    with_scheduler: bool,
) -> std::io::Result<DaemonHandle> {
    std::fs::create_dir_all(runtime_dir)?;
    let acl_path = runtime_dir.join("acl.toml");
    std::fs::write(&acl_path, TEST_ACL)?;
    let cfg = CrewdConfig {
        runtime_dir: runtime_dir.to_path_buf(),
        acl_path: acl_path.clone(),
        lease_secs: 30,
        max_attempts,
        backoff_base_secs,
        backoff_cap_secs,
        keys_env_path: None,
    };
    let issuer = Arc::new(FileIssuer::new(cfg.tokens_dir())?);
    let mut issued_tokens = HashMap::new();
    for cell in TEST_CELLS {
        let c = issuer
            .issue(cell, CredentialScope { ttl_secs: 86_400 })
            .map_err(io_err)?;
        issued_tokens.insert((*cell).to_string(), c.token);
    }
    let store_owned = Store::open(&cfg.db_path()).map_err(io_err)?;
    let mut audit_owned = AuditChain::open(&cfg.audit_path())?;
    // Boot recovery (AUDIT2 B1): resolve orphaned running/spawning threads.
    crate::scheduler::boot_recovery(&store_owned, &mut audit_owned).map_err(io_err)?;
    let acl_holder = AclHolder::new();
    acl_holder.reload_from_file(&acl_path).map_err(io_err)?;
    let store = Arc::new(Mutex::new(store_owned));
    let audit = Arc::new(Mutex::new(audit_owned));
    let (cancel_tx, cancel_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let state = Arc::new(DaemonState {
        store: store.clone(),
        acl: acl_holder,
        quota: Mutex::new(QuotaTracker::new(quota_cfg.clone())),
        quota_cfg,
        graph: Mutex::new(WaitForGraph::new()),
        adapters: Mutex::new(HashMap::new()),
        ask_wakers: Mutex::new(HashMap::new()),
        audit: audit.clone(),
        registry: Arc::new(SessionRegistry::new()),
        cfg: cfg.clone(),
        cancel_tx: if with_scheduler { Some(cancel_tx) } else { None },
    });

    // Bootstrap may fail (e.g. symlinked socket) BEFORE spawning the task.
    let listener: UnixListener = server::bootstrap_hardening(&cfg)?;
    let backend = Arc::new(L0TokenBackend::new(issuer.clone()));
    let (txs, rxs) = oneshot::channel::<()>();
    let state_serve = state.clone();
    let join = tokio::spawn(async move {
        let _ = server::serve(listener, backend, state_serve, rxs).await;
    });
    let (txd, rxd) = oneshot::channel::<()>();
    let state_deliver = state.clone();
    let join_deliver = tokio::spawn(async move {
        delivery::run_delivery_loop(state_deliver, rxd).await;
    });
    let (sched_stop, sched_join) = if with_scheduler {
        let sched = crate::scheduler::Scheduler::new(store, audit).with_cancel_channel(cancel_rx);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let j = std::thread::spawn(move || {
            crate::scheduler::run_loop(sched, std::time::Duration::from_millis(20), stop2);
        });
        (Some(stop), Some(j))
    } else {
        (None, None)
    };
    Ok(DaemonHandle {
        issued_tokens,
        fake_adapter: FakeAdapter::new(),
        state,
        issuer,
        shutdown_tx: Some(txs),
        shutdown_deliver_tx: Some(txd),
        join: Some(join),
        join_deliver: Some(join_deliver),
        sched_stop,
        sched_join,
        runtime_dir: cfg.runtime_dir.clone(),
    })
}

async fn build_and_start_with_quota(
    runtime_dir: &Path,
    quota_cfg: QuotaConfig,
) -> std::io::Result<DaemonHandle> {
    build_and_start_full(runtime_dir, quota_cfg, 10, 1, 60, false).await
}

/// Spawn the daemon on `runtime_dir`. Panics if bootstrap fails.
pub async fn spawn_daemon(runtime_dir: &Path) -> DaemonHandle {
    build_and_start(runtime_dir)
        .await
        .expect("daemon spawn failed")
}

/// Spawn the daemon WITH the engine scheduler loop wired (AUDIT2 B1). Used by
/// the fabric e2e / chaos tests that need `cell_spawn` → engine turn →
/// `cell_result`.
pub async fn spawn_daemon_with_scheduler(runtime_dir: &Path) -> DaemonHandle {
    build_and_start_full(runtime_dir, QuotaConfig::default(), 10, 1, 60, true)
        .await
        .expect("daemon spawn failed")
}

/// Spawn attempt; returns `Err` if bootstrap fails (symlink-refusal test).
pub async fn try_spawn_daemon(runtime_dir: &Path) -> std::io::Result<DaemonHandle> {
    build_and_start(runtime_dir).await
}

/// Spawn the daemon with a custom quota config (conformance quota tests).
pub async fn spawn_daemon_with_quota(runtime_dir: &Path, quota: QuotaConfig) -> DaemonHandle {
    build_and_start_with_quota(runtime_dir, quota)
        .await
        .expect("daemon spawn failed")
}

/// Spawn with a custom delivery backoff/budget (conformance delivery tests
/// that need fast retry cadence).
pub async fn spawn_daemon_with_delivery(
    runtime_dir: &Path,
    max_attempts: u32,
    backoff_base_secs: u64,
    backoff_cap_secs: u64,
) -> DaemonHandle {
    build_and_start_full(
        runtime_dir,
        QuotaConfig::default(),
        max_attempts,
        backoff_base_secs,
        backoff_cap_secs,
        false,
    )
    .await
    .expect("daemon spawn failed")
}

/// A live authenticated connection.
#[derive(Debug)]
pub struct CellConn {
    stream: UnixStream,
    next_id: u64,
}

impl CellConn {
    /// Issue a tool call; returns the `result` object or the daemon's error.
    pub async fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, WireError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = WireRequest {
            id,
            method: method.into(),
            params,
        };
        let mut bytes = serde_json::to_vec(&req).map_err(|e| WireError {
            code: "E_INTERNAL".into(),
            message: format!("{e}"),
        })?;
        bytes.push(b'\n');
        self.stream
            .write_all(&bytes)
            .await
            .map_err(|e| WireError {
                code: "E_INTERNAL".into(),
                message: format!("{e}"),
            })?;
        let resp = read_response(&mut self.stream)
            .await
            .map_err(|e| WireError {
                code: "E_INTERNAL".into(),
                message: format!("{e}"),
            })?;
        if let Some(err) = resp.error {
            return Err(err);
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

/// Connect to the daemon socket and perform the handshake as `cell` with
/// `token`. Returns the authenticated connection on success, or the daemon's
/// `WireError` on failure.
pub async fn connect_as(
    runtime_dir: &Path,
    cell: &str,
    token: &str,
) -> Result<CellConn, WireError> {
    let sock = runtime_dir.join("crewd.sock");
    let mut stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| WireError {
            code: "E_INTERNAL".into(),
            message: format!("{e}"),
        })?;
    let proof = ClientProof {
        cell_id: cell.into(),
        token: token.into(),
        spec_version: SPEC_VERSION.into(),
    };
    let req = WireRequest {
        id: 1,
        method: "handshake".into(),
        params: serde_json::to_value(&proof).expect("serializable"),
    };
    let mut bytes = serde_json::to_vec(&req).expect("serializable");
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| WireError {
            code: "E_INTERNAL".into(),
            message: format!("{e}"),
        })?;
    let resp = read_response(&mut stream)
        .await
        .map_err(|e| WireError {
            code: "E_INTERNAL".into(),
            message: format!("{e}"),
        })?;
    if let Some(err) = resp.error {
        return Err(err);
    }
    Ok(CellConn { stream, next_id: 2 })
}

async fn read_response(stream: &mut UnixStream) -> std::io::Result<WireResponse> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..pos]);
            break;
        } else {
            buf.extend_from_slice(&chunk[..n]);
        }
        if buf.len() > 192 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response too large",
            ));
        }
    }
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
