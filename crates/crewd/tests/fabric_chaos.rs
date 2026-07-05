//! Chaos matrix + crash consistency (crewd Fase 2 Task 15). One test per row:
//!
//! (a) `kill -9` the engine process during an accepted turn → next tick: thread
//!     `failed_unknown`, accepted job NOT requeued, journal intact, audit
//!     `cell_turn_failed`, supervisor respawn, explicit follow-up works.
//! (b) `kill -9` the daemon during a turn → restart: cell_threads/cell_jobs/
//!     spawn_requests intact on disk, orphaned running thread → `interrupted`
//!     at boot, FIFO queue preserved in order.
//! (c) double primary: same idempotency_key on the same cell → same
//!     crewd_thread_id (Created + Replayed); different key/principal →
//!     `E_CELL_BUSY`, never two active threads on one cell.
//! (d) client disconnect after background spawn → the turn still completes;
//!     `cell_result` is readable from a NEW connection.
//! (e) after crash + boot recovery: `op_audit_verify` returns ok — the
//!     hash-chain is intact through the crash.
//!
//! The engine is the native `crewd-fake-engine` (claude NDJSON protocol) run via
//! `spawn_direct`, so it can be `kill -9`'d. The daemon-level tests drive the
//! real `crewd` binary as a child process so it can be killed for real.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crewd::engines::claude::ClaudeAdapter;
use crewd::engines::EngineSpawnCfg;
use crewd::scheduler::Scheduler;
use crewd_core::audit::{AuditChain, VerifyResult};
use crewd_core::cells::{CellDef, EngineKind};
use crewd_core::jobs::JobState;
use crewd_core::store::Store;
use crewd_core::threads::{CellThread, ThreadState};
use crewd_core::types::{new_uuidv7, now_rfc3339};

const FAKE_ENGINE: &str = env!("CARGO_BIN_EXE_crewd-fake-engine");
const CREWD_BIN: &str = env!("CARGO_BIN_EXE_crewd");

// ===========================================================================
// (a) kill -9 engine during accepted turn — scheduler level
// ===========================================================================

fn insert_thread(store: &Store, cell: &str, engine: EngineKind) -> String {
    let id = new_uuidv7();
    store
        .thread_insert(&CellThread {
            crewd_thread_id: id.clone(),
            cell_name: cell.into(),
            engine_kind: engine,
            model: None,
        profile: None,
        engine_process_id: None,
            engine_thread_id: None,
            engine_turn_id: None,
            engine_session_id: None,
            cwd: "/tmp".into(),
            worktree_path: None,
            state: ThreadState::Spawning,
            generation: 0,
            created_by_principal: "dag".into(),
            idempotency_key: new_uuidv7(),
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        })
        .unwrap();
    id
}

/// Tick until `pred(store)` holds or the deadline elapses. Sleeps between ticks
/// so a real child engine has time to emit its NDJSON events.
fn tick_until<F: Fn(&Store) -> bool>(
    sched: &mut Scheduler,
    store: &Arc<Mutex<Store>>,
    pred: F,
    deadline: Duration,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        sched.tick().unwrap();
        if pred(&store.lock().unwrap()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn chaos_a_kill9_engine_during_accepted_turn() {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(Mutex::new(AuditChain::open(&audit_path).unwrap()));
    std::mem::forget(dir);

    // Respawn (the explicit follow-up) routes through a non-hanging fake engine.
    let mut sched = Scheduler::new(store.clone(), audit.clone()).with_fake_engine(FAKE_ENGINE, &[]);

    let tid = {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        s.cell_register(
            &CellDef {
                name: "chaos-a".into(),
                engine: EngineKind::Claude,
                model: None,
                profile: None,
                cwd: "/tmp".into(),
                worktree_default: false,
                memory_device: None,
                created_at: now_rfc3339(),
            },
            &mut ch,
        )
        .unwrap();
        let tid = insert_thread(&s, "chaos-a", EngineKind::Claude);
        s.job_enqueue(&tid, "chaos-a", "hang-task", 8).unwrap();
        tid
    };

    // Inject a HANGING native fake engine (accepted, then no final) and keep its
    // pid so the test can kill -9 the whole process group.
    let hang_cfg = EngineSpawnCfg {
        cwd: "/tmp".into(),
        bin_override: Some(FAKE_ENGINE.to_string()),
        spawn_direct: true,
        shim_args: vec!["--hang".into()],
        ..Default::default()
    };
    let adapter = ClaudeAdapter::new(&hang_cfg).unwrap();
    let engine_pid = adapter.pid();
    sched.inject_adapter("chaos-a", Box::new(adapter));

    // Drive until the turn is accepted (engine_turn_id persisted).
    let accepted = tick_until(
        &mut sched,
        &store,
        |s| {
            s.thread_get(&tid)
                .unwrap()
                .map(|t| t.engine_turn_id.is_some())
                .unwrap_or(false)
        },
        Duration::from_secs(10),
    );
    assert!(accepted, "engine turn must be accepted before the kill");
    {
        let s = store.lock().unwrap();
        let jobs = s.jobs_for_thread(&tid).unwrap();
        assert!(
            jobs[0].accepted_by_engine_at.is_some(),
            "job accepted before the kill"
        );
    }

    // kill -9 the engine: the process group (negative pid, it is a setsid group
    // leader) and the pid itself, belt-and-suspenders across `kill` variants.
    let g = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{engine_pid}"))
        .stderr(Stdio::null())
        .status();
    let p = Command::new("kill")
        .arg("-KILL")
        .arg(engine_pid.to_string())
        .stderr(Stdio::null())
        .status();
    assert!(
        g.map(|s| s.success()).unwrap_or(false) || p.map(|s| s.success()).unwrap_or(false),
        "kill -9 of the engine pid {engine_pid} must succeed"
    );

    // Next ticks: the scheduler observes the death → failed_unknown.
    let failed = tick_until(
        &mut sched,
        &store,
        |s| s.thread_get(&tid).unwrap().unwrap().state == ThreadState::FailedUnknown,
        Duration::from_secs(10),
    );
    assert!(failed, "thread must become failed_unknown after engine kill");

    {
        let s = store.lock().unwrap();
        let jobs = s.jobs_for_thread(&tid).unwrap();
        // accepted job NOT requeued (SPEC §20.5 / AUDIT2 M1): finished, cell freed.
        assert_ne!(jobs[0].state, JobState::Queued, "accepted job must never be requeued");
        assert_eq!(jobs[0].state, JobState::Finished);
        // journal intact with the last seq (the acceptance was journaled).
        let tail = s.journal_tail(&tid, 50).unwrap();
        assert!(!tail.is_empty(), "journal must retain the accepted entry");
        assert!(tail.iter().any(|l| l.contains("accepted:")), "journal tail: {tail:?}");
    }
    // audit cell_turn_failed present + chain verifies through the crash.
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    assert!(raw.contains("\"kind\":\"cell_turn_failed\""), "missing cell_turn_failed: {raw}");
    assert!(matches!(AuditChain::verify(&audit_path), VerifyResult::Ok { .. }));

    // Explicit follow-up (cell_send_task equivalent) must work: a new job on the
    // failed thread respawns the engine (backoff) and completes.
    {
        let s = store.lock().unwrap();
        s.job_enqueue(&tid, "chaos-a", "followup", 8).unwrap();
    }
    let recovered = tick_until(
        &mut sched,
        &store,
        |s| s.thread_get(&tid).unwrap().unwrap().state == ThreadState::Idle,
        Duration::from_secs(10),
    );
    assert!(recovered, "explicit follow-up must run on a respawned engine");
    {
        let s = store.lock().unwrap();
        let jobs = s.jobs_for_thread(&tid).unwrap();
        let follow = jobs.iter().find(|j| j.payload == "followup").unwrap();
        assert_eq!(follow.state, JobState::Finished, "follow-up job completed");
    }
}

// ===========================================================================
// Real-daemon helpers (rows b, e)
// ===========================================================================

const PRIMARY_ACL: &str = r#"
[cell.primary]
engine = "claude"
capabilities = ["send","read_inbox","list_cells","spawn","read_audit","admin_registry"]
"#;

/// Prepare a runtime dir for the real `crewd` binary: acl, issued token, config,
/// and a pre-seeded named claude cell in the registry. Returns the token.
fn setup_real_runtime(rt: &Path, cell: &str) -> String {
    use crewd::auth::FileIssuer;
    use crewd::config::CrewdConfig;
    use crewd_core::principal::{CredentialIssuer, CredentialScope};

    std::fs::create_dir_all(rt).unwrap();
    let acl_path = rt.join("acl.toml");
    std::fs::write(&acl_path, PRIMARY_ACL).unwrap();
    let cfg = CrewdConfig {
        runtime_dir: rt.to_path_buf(),
        acl_path: acl_path.clone(),
        lease_secs: 30,
        max_attempts: 10,
        backoff_base_secs: 1,
        backoff_cap_secs: 60,
        keys_env_path: None,
    };
    let issuer = FileIssuer::new(cfg.tokens_dir()).unwrap();
    let token = issuer
        .issue("primary", CredentialScope { ttl_secs: 86_400 })
        .unwrap()
        .token;

    // Pre-seed the registry (Task 16 style: named cells are seeded out-of-band).
    {
        let store = Store::open(&cfg.db_path()).unwrap();
        let mut audit = AuditChain::open(&cfg.audit_path()).unwrap();
        store
            .cell_register(
                &CellDef {
                    name: cell.into(),
                    engine: EngineKind::Claude,
                    model: None,
                    profile: None,
                    cwd: "/tmp".into(),
                    worktree_default: false,
                    memory_device: None,
                    created_at: now_rfc3339(),
                },
                &mut audit,
            )
            .unwrap();
    }

    let cfg_path = rt.join("crewd.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "runtime_dir = \"{}\"\nacl_path = \"{}\"\n",
            rt.display(),
            acl_path.display()
        ),
    )
    .unwrap();
    token
}

/// Spawn the real `crewd` daemon child. When `hang`, the scheduler runs the
/// native fake engine in `--hang` mode so a turn stays in flight.
fn spawn_daemon_proc(rt: &Path, hang: bool) -> Child {
    let cfg_path = rt.join("crewd.toml");
    let mut cmd = Command::new(CREWD_BIN);
    cmd.arg("--config").arg(&cfg_path);
    if hang {
        cmd.env("CREWD_FAKE_ENGINE_BIN", FAKE_ENGINE);
        cmd.env("CREWD_FAKE_ENGINE_ARGS", "--hang");
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn().expect("spawn crewd daemon")
}

/// Wait until the daemon socket is connectable, or panic after the deadline.
async fn wait_socket(rt: &Path) {
    let sock = rt.join("crewd.sock");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if sock.exists() && tokio::net::UnixStream::connect(&sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon socket never came up at {}", sock.display());
}

fn kill9(child: &mut Child) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(child.id().to_string())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

// ===========================================================================
// (b) kill -9 daemon during a turn
// ===========================================================================

#[tokio::test]
async fn chaos_b_kill9_daemon_during_turn() {
    use crewd::testkit::connect_as;

    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join("run");
    let token = setup_real_runtime(&rt, "chaosb");
    let db_path = rt.join("crewd.db");

    let mut daemon = spawn_daemon_proc(&rt, true);
    wait_socket(&rt).await;

    // Spawn a hanging turn, then queue two follow-ups behind it (FIFO).
    let tid = {
        let mut c = connect_as(&rt, "primary", &token).await.unwrap();
        let r = c
            .call(
                "cell_spawn",
                serde_json::json!({"cell":"chaosb","task":"t1","idempotency_key":"k1"}),
            )
            .await
            .unwrap();
        let tid = r["crewd_thread_id"].as_str().unwrap().to_string();
        for msg in ["t2", "t3"] {
            c.call(
                "cell_send_task",
                serde_json::json!({"crewd_thread_id": tid, "message": msg}),
            )
            .await
            .unwrap();
        }
        tid
    };

    // Wait until the turn is in flight (thread running).
    wait_thread_state(&rt, "primary", &token, &tid, "running", Duration::from_secs(10)).await;

    // kill -9 the daemon mid-turn.
    kill9(&mut daemon);

    // On-disk state must be intact (fsync): read the DB directly while dead.
    {
        let store = Store::open(&db_path).unwrap();
        let t = store.thread_get(&tid).unwrap().unwrap();
        assert_eq!(t.state, ThreadState::Running, "orphan still running on disk");
        let jobs = store.jobs_for_thread(&tid).unwrap();
        assert_eq!(jobs.len(), 3, "all 3 jobs persisted");
        // FIFO order preserved (enqueue order t1, t2, t3).
        let payloads: Vec<&str> = jobs.iter().map(|j| j.payload.as_str()).collect();
        assert_eq!(payloads, ["t1", "t2", "t3"], "queue FIFO order preserved");
        assert_eq!(jobs[1].state, JobState::Queued);
        assert_eq!(jobs[2].state, JobState::Queued);
    }

    // Restart the daemon → boot recovery marks the orphan interrupted.
    let mut daemon2 = spawn_daemon_proc(&rt, false);
    wait_socket(&rt).await;

    wait_thread_state(&rt, "primary", &token, &tid, "interrupted", Duration::from_secs(10)).await;

    // spawn_requests intact: the same key replays to the SAME thread.
    {
        let mut c = connect_as(&rt, "primary", &token).await.unwrap();
        let r = c
            .call(
                "cell_spawn",
                serde_json::json!({"cell":"chaosb","task":"t1","idempotency_key":"k1"}),
            )
            .await
            .unwrap();
        assert_eq!(r["crewd_thread_id"].as_str().unwrap(), tid, "replay same thread");
        assert_eq!(r["replayed"], serde_json::json!(true));
    }

    kill9(&mut daemon2);
}

/// Poll `cell_status` for `tid` until its state equals `want` or the deadline.
async fn wait_thread_state(
    rt: &Path,
    principal: &str,
    token: &str,
    tid: &str,
    want: &str,
    deadline: Duration,
) {
    use crewd::testkit::connect_as;
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Ok(mut c) = connect_as(rt, principal, token).await {
            if let Ok(v) = c
                .call("cell_status", serde_json::json!({"crewd_thread_id": tid}))
                .await
            {
                if let Some(state) = v["threads"][0]["state"].as_str() {
                    if state == want {
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("thread {tid} never reached state {want}");
}

// ===========================================================================
// (c) double primary
// ===========================================================================

fn register_fake_cell(state: &crewd::handlers::DaemonState, name: &str) {
    let s = state.store.lock().unwrap();
    let mut ch = state.audit.lock().unwrap();
    s.cell_register(
        &CellDef {
            name: name.into(),
            engine: EngineKind::Fake,
            model: None,
            profile: None,
            cwd: "/tmp".into(),
            worktree_default: false,
            memory_device: None,
            created_at: now_rfc3339(),
        },
        &mut ch,
    )
    .unwrap();
}

#[tokio::test]
async fn chaos_c_double_primary_idempotency_and_busy() {
    use crewd::testkit::{connect_as, spawn_daemon};

    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join("run");
    // No scheduler: the thread stays `spawning` (active) so the busy check bites.
    let h = spawn_daemon(&rt).await;
    register_fake_cell(&h.state, "chaos-c");

    let tok_dev = h.issued_tokens["dev-senior"].clone();
    let tok_codex = h.issued_tokens["codex-audit"].clone();

    // Two connections, same principal + same key → Created then Replayed.
    let mut c1 = connect_as(&rt, "dev-senior", &tok_dev).await.unwrap();
    let mut c2 = connect_as(&rt, "dev-senior", &tok_dev).await.unwrap();
    let r1 = c1
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"chaos-c","task":"x","idempotency_key":"k1"}),
        )
        .await
        .unwrap();
    let tid = r1["crewd_thread_id"].as_str().unwrap().to_string();
    assert_eq!(r1["replayed"], serde_json::json!(false));
    let r2 = c2
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"chaos-c","task":"x","idempotency_key":"k1"}),
        )
        .await
        .unwrap();
    assert_eq!(r2["crewd_thread_id"].as_str().unwrap(), tid, "same thread on replay");
    assert_eq!(r2["replayed"], serde_json::json!(true));

    // A different primary with a different key → E_CELL_BUSY (never a 2nd active
    // thread on the same cell).
    let mut c3 = connect_as(&rt, "codex-audit", &tok_codex).await.unwrap();
    let err = c3
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"chaos-c","task":"y","idempotency_key":"k2"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_CELL_BUSY");

    // Exactly one active thread on the cell, and it is the original.
    {
        let s = h.state.store.lock().unwrap();
        let active = s.thread_active_for_cell("chaos-c").unwrap().unwrap();
        assert_eq!(active.crewd_thread_id, tid);
    }
    h.shutdown().await;
}

// ===========================================================================
// (d) client disconnect after background spawn
// ===========================================================================

#[tokio::test]
async fn chaos_d_disconnect_turn_completes_result_from_new_conn() {
    use crewd::testkit::{connect_as, spawn_daemon_with_scheduler};

    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join("run");
    let h = spawn_daemon_with_scheduler(&rt).await;
    register_fake_cell(&h.state, "chaos-d");

    let tok = h.issued_tokens["dev-senior"].clone();

    // Spawn in background, then DROP the connection (disconnect).
    let tid = {
        let mut c1 = connect_as(&rt, "dev-senior", &tok).await.unwrap();
        let r = c1
            .call(
                "cell_spawn",
                serde_json::json!({"cell":"chaos-d","task":"bg-work","idempotency_key":"kd","mode":"background"}),
            )
            .await
            .unwrap();
        r["crewd_thread_id"].as_str().unwrap().to_string()
    }; // c1 dropped here

    // A NEW connection reads the result; the turn completed despite the drop.
    let mut c2 = connect_as(&rt, "dev-senior", &tok).await.unwrap();
    let start = Instant::now();
    let mut done = false;
    let mut tail_has_final = false;
    while start.elapsed() < Duration::from_secs(10) {
        let r = c2
            .call("cell_result", serde_json::json!({"crewd_thread_id": tid}))
            .await
            .unwrap();
        if r["exit_status"] == serde_json::json!("done") {
            done = true;
            tail_has_final = r["event_tail"]
                .as_array()
                .map(|a| a.iter().any(|l| l.as_str().unwrap_or("").contains("final:")))
                .unwrap_or(false);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "background turn must complete after client disconnect");
    assert!(tail_has_final, "result event_tail must contain the final answer");
    h.shutdown().await;
}

// ===========================================================================
// (e) audit chain verify through the crash
// ===========================================================================

#[tokio::test]
async fn chaos_e_audit_chain_verifies_through_crash() {
    use crewd::testkit::connect_as;

    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join("run");
    let token = setup_real_runtime(&rt, "chaose");
    let audit_path = rt.join("audit.jsonl");

    let mut daemon = spawn_daemon_proc(&rt, true);
    wait_socket(&rt).await;

    let tid = {
        let mut c = connect_as(&rt, "primary", &token).await.unwrap();
        let r = c
            .call(
                "cell_spawn",
                serde_json::json!({"cell":"chaose","task":"hang-it","idempotency_key":"ke"}),
            )
            .await
            .unwrap();
        r["crewd_thread_id"].as_str().unwrap().to_string()
    };
    wait_thread_state(&rt, "primary", &token, &tid, "running", Duration::from_secs(10)).await;

    // Crash mid-turn, then restart (boot recovery appends to the same chain).
    kill9(&mut daemon);
    let mut daemon2 = spawn_daemon_proc(&rt, false);
    wait_socket(&rt).await;
    wait_thread_state(&rt, "primary", &token, &tid, "interrupted", Duration::from_secs(10)).await;

    // `op_audit_verify` (the RPC the crew CLI uses) must report ok — the
    // hash-chain is intact across the crash + boot recovery.
    {
        let mut c = connect_as(&rt, "primary", &token).await.unwrap();
        let v = c.call("op_audit_verify", serde_json::json!({})).await.unwrap();
        assert_eq!(v["status"], serde_json::json!("ok"), "audit verify: {v}");
    }
    // Belt and suspenders: verify the on-disk chain directly too.
    assert!(matches!(
        AuditChain::verify(&audit_path),
        VerifyResult::Ok { .. }
    ));

    kill9(&mut daemon2);
}
