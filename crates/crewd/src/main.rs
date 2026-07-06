//! crewd daemon binary. Reads `--config`, builds the shared `DaemonState`,
//! bootstraps the §17.3-hardened listener, and runs the serve + delivery loops
//! until killed. Graceful signal-based shutdown is deferred.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;

use crewd_core::acl::AclHolder;
use crewd_core::audit::AuditChain;
use crewd_core::quota::{QuotaConfig, QuotaTracker};
use crewd_core::store::Store;
use crewd_core::tickets::WaitForGraph;

use crewd::auth::{FileIssuer, L0TokenBackend};
use crewd::config::CrewdConfig;
use crewd::delivery;
use crewd::handlers::DaemonState;
use crewd::scheduler::{self, Scheduler};
use crewd::server;

#[derive(Debug, Parser)]
#[command(name = "crewd", version, about = "mcp-crewd-rs daemon")]
struct Args {
    /// Explicit path to crewd.toml (never cwd-derived).
    #[arg(long)]
    config: PathBuf,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let cfg = match CrewdConfig::from_toml_file(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let issuer = match FileIssuer::new(cfg.tokens_dir()) {
            Ok(i) => Arc::new(i),
            Err(e) => {
                eprintln!("issuer init: {e}");
                return;
            }
        };
        let store = match Store::open(&cfg.db_path()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("store: {e:?}");
                return;
            }
        };
        let mut audit = match AuditChain::open(&cfg.audit_path()) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("audit: {e}");
                return;
            }
        };
        // Boot recovery (chaos test b): orphaned running/spawning
        // threads from a previous crash are resolved BEFORE serving.
        if let Err(e) = scheduler::boot_recovery(&store, &mut audit) {
            eprintln!("boot recovery: {e:?}");
            return;
        }
        let acl_holder = AclHolder::new();
        if let Err(e) = acl_holder.reload_from_file(&cfg.acl_path) {
            eprintln!("acl load: {e:?}");
            return;
        }
        let quota_cfg = QuotaConfig::default();
        let store = Arc::new(Mutex::new(store));
        let audit = Arc::new(Mutex::new(audit));
        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let state = Arc::new(DaemonState {
            store: store.clone(),
            acl: acl_holder,
            quota: Mutex::new(QuotaTracker::new(quota_cfg.clone())),
            quota_cfg,
            graph: Mutex::new(WaitForGraph::new()),
            adapters: Mutex::new(std::collections::HashMap::new()),
            ask_wakers: Mutex::new(std::collections::HashMap::new()),
            audit: audit.clone(),
            registry: Arc::new(server::SessionRegistry::new()),
            cfg: cfg.clone(),
            cancel_tx: Some(cancel_tx),
        });

        let listener = match server::bootstrap_hardening(&cfg) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bootstrap: {e}");
                return;
            }
        };
        let backend = Arc::new(L0TokenBackend::new(issuer));
        let (_txs, rxs) = tokio::sync::oneshot::channel::<()>();
        let (_txd, rxd) = tokio::sync::oneshot::channel::<()>();
        let state_serve = state.clone();
        tokio::spawn(async move {
            let _ = server::serve(listener, backend, state_serve, rxs).await;
        });
        let state_deliver = state.clone();
        tokio::spawn(async move {
            delivery::run_delivery_loop(state_deliver, rxd).await;
        });
        // Scheduler loop on a dedicated OS thread: drives engine
        // turns for spawned cells. It owns child engine processes, so it lives
        // outside the tokio worker pool.
        let sched = Scheduler::new(store, audit)
            .with_cancel_channel(cancel_rx)
            .with_keys_env_path(cfg.keys_env_path.clone());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sched_stop = stop.clone();
        std::thread::spawn(move || {
            scheduler::run_loop(sched, std::time::Duration::from_millis(100), sched_stop);
        });
        // Run until killed (no graceful signal shutdown in T10b).
        std::future::pending::<()>().await;
    });
    std::process::ExitCode::SUCCESS
}
