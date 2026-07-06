//! Fabric core integration tests (crewd Phase 2). SUPERVISOR section (Task 8):
//! - `ensure` twice on the same cell -> same adapter (1 process per cell)
//! - a full FakeEngine turn produces Accepted -> Final in order
//! - `stop` + `ensure` -> fresh adapter
//! - `respawn_backoff_secs` grows and caps at 60
//!
//! "Same adapter identity" is verified behaviorally via the FakeEngine
//! turn-id: a reused adapter continues its turn counter (`fake-turn-1`,
//! then `fake-turn-2`); a fresh adapter restarts from `fake-turn-1`.

use std::sync::{Arc, Mutex};

use crewd::engines::fake::FakeEngine;
use crewd::engines::EngineSpawnCfg;
use crewd::scheduler::Scheduler;
use crewd::supervisor::EngineSupervisor;
use crewd_core::audit::AuditChain;
use crewd_core::cells::CellDef;
use crewd_core::cells::EngineKind;
use crewd_core::engine::EngineEvent;
use crewd_core::jobs::JobState;
use crewd_core::store::Store;
use crewd_core::threads::{CellThread, ThreadState};
use crewd_core::types::{new_uuidv7, now_rfc3339};
use tempfile::tempdir;

fn make_supervisor() -> EngineSupervisor {
    let dir = tempdir().unwrap();
    let chain = AuditChain::open(&dir.path().join("audit.jsonl")).unwrap();
    // The file is opened/closed on every append; the directory must outlive
    // the whole test. Intentional leak (short-lived test).
    std::mem::forget(dir);
    EngineSupervisor::new(Arc::new(Mutex::new(chain)))
}

fn spawn_cfg() -> EngineSpawnCfg {
    EngineSpawnCfg {
        cwd: "/tmp".into(),
        timeout_secs: 1800,
        ..Default::default()
    }
}

/// Runs a start_turn and returns the `engine_turn_id` of the Accepted event
/// (poll model: events arrive via `poll_events`).
fn accepted_turn_id(adapter: &mut dyn crewd::engines::EngineAdapter, payload: &str) -> String {
    adapter.start_turn(payload).expect("start_turn ok");
    for ev in adapter.poll_events() {
        if let EngineEvent::Accepted { engine_turn_id } = ev {
            return engine_turn_id;
        }
    }
    panic!("Accepted emitted")
}

#[test]
fn ensure_twice_same_cell_reuses_adapter() {
    let mut sup = make_supervisor();
    let cfg = spawn_cfg();

    // First ensure + turn -> fake-turn-1 (adapter counter = 1)
    {
        let a = sup.ensure("cell-a", EngineKind::Fake, &cfg).unwrap();
        assert_eq!(accepted_turn_id(&mut **a, "x"), "fake-turn-1");
    }
    // Second ensure: SAME adapter -> the next turn is fake-turn-2
    // (a fresh adapter would have restarted the counter from fake-turn-1).
    {
        let a = sup.ensure("cell-a", EngineKind::Fake, &cfg).unwrap();
        assert_eq!(accepted_turn_id(&mut **a, "y"), "fake-turn-2");
    }
}

#[test]
fn fake_engine_turn_emits_accepted_then_final_in_order() {
    let mut sup = make_supervisor();
    let cfg = spawn_cfg();
    let a = sup.ensure("cell-b", EngineKind::Fake, &cfg).unwrap();

    a.start_turn("hello").unwrap();
    let events: Vec<EngineEvent> = a.poll_events();

    assert_eq!(events.len(), 2, "expected Accepted then Final");
    assert!(matches!(
        &events[0],
        EngineEvent::Accepted { engine_turn_id } if engine_turn_id == "fake-turn-1"
    ));
    assert!(matches!(
        &events[1],
        EngineEvent::Final { final_answer } if final_answer == "done: hello"
    ));
}

#[test]
fn stop_then_ensure_creates_new_adapter() {
    let mut sup = make_supervisor();
    let cfg = spawn_cfg();

    {
        let a = sup.ensure("cell-c", EngineKind::Fake, &cfg).unwrap();
        accepted_turn_id(&mut **a, "first"); // fake-turn-1
    }
    sup.stop("cell-c");

    // After stop, ensure recreates the adapter: the counter restarts from fake-turn-1.
    {
        let a = sup.ensure("cell-c", EngineKind::Fake, &cfg).unwrap();
        assert_eq!(
            accepted_turn_id(&mut **a, "second"),
            "fake-turn-1",
            "adapter should be fresh after stop"
        );
    }
}

#[test]
fn respawn_backoff_grows_and_caps_at_60() {
    let mut sup = make_supervisor();
    let seq: Vec<u64> = (0..8).map(|_| sup.respawn_backoff_secs("cell-d")).collect();
    // 1,2,4,8,16,32,60,60
    assert_eq!(seq, vec![1, 2, 4, 8, 16, 32, 60, 60]);
}

// ---------------------------------------------------------------------------
// SCHEDULER section (Task 9): happy path + audit order, fail (accepted job
// NOT requeued), timeout (FakeEngine hang + timeout_secs=0).
// ---------------------------------------------------------------------------

fn scheduler_fixture() -> (
    Scheduler,
    Arc<Mutex<Store>>,
    Arc<Mutex<AuditChain>>,
    std::path::PathBuf,
) {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let dir = tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let chain = Arc::new(Mutex::new(AuditChain::open(&audit_path).unwrap()));
    std::mem::forget(dir); // the file lives for the whole test
    let sched = Scheduler::new(store.clone(), chain.clone());
    (sched, store, chain, audit_path)
}

fn register_cell(store: &Store, audit: &mut AuditChain, name: &str, engine: EngineKind) {
    store
        .cell_register(
            &CellDef {
                name: name.into(),
                engine,
                model: None,
                profile: None,
                cwd: "/tmp".into(),
                worktree_default: false,
                memory_device: None,
                created_at: now_rfc3339(),
            },
            audit,
        )
        .unwrap();
}

fn spawn_thread(store: &Store, cell: &str) -> String {
    let id = new_uuidv7();
    store
        .thread_insert(&CellThread {
            crewd_thread_id: id.clone(),
            cell_name: cell.into(),
            engine_kind: EngineKind::Fake,
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
            created_by_principal: "operator".into(),
            idempotency_key: new_uuidv7(),
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        })
        .unwrap();
    id
}

#[test]
fn scheduler_happy_path_completes_turn_and_audit_order() {
    let (mut sched, store, audit, audit_path) = scheduler_fixture();
    let thread_id = {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        register_cell(&s, &mut ch, "c1", EngineKind::Fake);
        let tid = spawn_thread(&s, "c1");
        s.job_enqueue(&tid, "c1", "do work", 8).unwrap();
        tid
    };

    sched.tick().unwrap();

    let s = store.lock().unwrap();
    // thread -> idle, job -> finished + accepted
    let t = s.thread_get(&thread_id).unwrap().unwrap();
    assert_eq!(t.state, ThreadState::Idle);
    assert!(t.engine_turn_id.is_some(), "engine_turn_id set on accept");
    let jobs = s.jobs_for_thread(&thread_id).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Finished);
    assert!(jobs[0].accepted_by_engine_at.is_some());
    // the journal records the final
    let tail = s.journal_tail(&thread_id, 10).unwrap();
    assert!(
        tail.iter().any(|l| l.contains("final:")),
        "journal must record final: {tail:?}"
    );

    // audit order: cell_turn_started BEFORE cell_turn_completed
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    let started = raw.find("\"kind\":\"cell_turn_started\"");
    let completed = raw.find("\"kind\":\"cell_turn_completed\"");
    assert!(
        started.is_some() && completed.is_some(),
        "audit events missing: {raw}"
    );
    assert!(
        started.unwrap() < completed.unwrap(),
        "cell_turn_started must precede cell_turn_completed"
    );
}

#[test]
fn scheduler_fail_path_job_accepted_is_not_requeued() {
    let (mut sched, store, audit, _audit_path) = scheduler_fixture();
    let thread_id = {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        register_cell(&s, &mut ch, "c2", EngineKind::Fake);
        let tid = spawn_thread(&s, "c2");
        s.job_enqueue(&tid, "c2", "fail-me", 8).unwrap();
        tid
    };
    // FakeEngine that emits Accepted then Failed.
    sched.inject_adapter("c2", Box::new(FakeEngine::new().with_fail_next()));

    sched.tick().unwrap();

    let s = store.lock().unwrap();
    let t = s.thread_get(&thread_id).unwrap().unwrap();
    assert_eq!(t.state, ThreadState::FailedUnknown);
    assert_eq!(t.engine_turn_id.as_deref(), Some("fake-turn-1"));
    let jobs = s.jobs_for_thread(&thread_id).unwrap();
    assert_eq!(jobs.len(), 1);
    // accepted (before the Failed) and NOT requeued
    assert!(
        jobs[0].accepted_by_engine_at.is_some(),
        "job was accepted before failure"
    );
    assert_ne!(
        jobs[0].state,
        JobState::Queued,
        "accepted job must never be auto-requeued (SPEC §20.5)"
    );
}

// Retry-storm regression (smoke 2026-07-05): a PRE-acceptance spawn failure
// (`engine_spawn`, e.g. zai-a profile without keys_env_path → E_ENGINE_DOWN)
// must not be re-enqueued. The old `job_requeue` path put it back to 'queued'
// on every tick → hot-loop bloating audit.jsonl/WAL. It is now terminal.
#[test]
fn engine_spawn_failure_is_terminal_not_requeued() {
    let (mut sched, store, audit, audit_path) = scheduler_fixture();
    let thread_id = {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        // claude cell with zai-a profile but no keys_env_path wired:
        // make_adapter → ClaudeAdapter::new → build_env fails E_ENGINE_DOWN.
        s.cell_register(
            &CellDef {
                name: "zai".into(),
                engine: EngineKind::Claude,
                model: None,
                profile: Some("zai-a".into()),
                cwd: "/tmp".into(),
                worktree_default: false,
                memory_device: None,
                created_at: now_rfc3339(),
            },
            &mut ch,
        )
        .unwrap();
        let tid = spawn_thread(&s, "zai");
        s.job_enqueue(&tid, "zai", "task", 8).unwrap();
        tid
    };

    // tick 1: the engine spawn fails before acceptance.
    sched.tick().unwrap();
    {
        let s = store.lock().unwrap();
        let t = s.thread_get(&thread_id).unwrap().unwrap();
        assert_eq!(t.state, ThreadState::FailedUnknown, "thread must fail");
        let jobs = s.jobs_for_thread(&thread_id).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_ne!(
            jobs[0].state,
            JobState::Queued,
            "a non-accepted spawn failure must NOT go back to 'queued' (avoids hot-loop)"
        );
    }

    // tick 2: no new attempt — a single cell_turn_started in the audit.
    sched.tick().unwrap();
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    let started = raw.matches("\"kind\":\"cell_turn_started\"").count();
    assert_eq!(
        started, 1,
        "a single spawn attempt, not a retry-storm; cell_turn_started found: {started}"
    );
}

#[test]
fn scheduler_timeout_hang_persists_engine_turn_id_before_interrupt() {
    let (mut sched, store, audit, audit_path) = scheduler_fixture();
    let thread_id = {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        register_cell(&s, &mut ch, "c3", EngineKind::Fake);
        let tid = spawn_thread(&s, "c3");
        s.job_enqueue(&tid, "c3", "hang", 8).unwrap();
        tid
    };
    sched = sched.with_timeout_secs(0);
    sched.inject_adapter("c3", Box::new(FakeEngine::new().with_hang()));

    // tick 1: starts the turn (hang: Accepted only) -> thread running, in_flight
    sched.tick().unwrap();
    {
        let s = store.lock().unwrap();
        let t = s.thread_get(&thread_id).unwrap().unwrap();
        assert_eq!(t.state, ThreadState::Running, "turn started after tick 1");
        assert_eq!(t.engine_turn_id.as_deref(), Some("fake-turn-1"));
    }

    // tick 2: timeout (delta > 0 with timeout_secs=0) -> thread timeout
    sched.tick().unwrap();
    let s = store.lock().unwrap();
    let t = s.thread_get(&thread_id).unwrap().unwrap();
    assert_eq!(t.state, ThreadState::Timeout);
    assert_eq!(
        t.engine_turn_id.as_deref(),
        Some("fake-turn-1"),
        "engine_turn_id persisted before interrupt (SPEC §20.9)"
    );
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        raw.contains("\"kind\":\"cell_timeout\""),
        "missing cell_timeout audit: {raw}"
    );
}

#[test]
fn scheduler_cancel_interrupts_and_stops_engine() {
    // a cancel signalled on the control channel makes the scheduler
    // interrupt AND kill the engine of that cell (best-effort, non-blocking).
    let (mut sched, store, audit, _audit_path) = scheduler_fixture();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    sched = sched.with_cancel_channel(rx);
    {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        register_cell(&s, &mut ch, "cx", EngineKind::Fake);
        let tid = spawn_thread(&s, "cx");
        s.job_enqueue(&tid, "cx", "hang", 8).unwrap();
    }
    // A hanging engine leaves the turn in flight with a live adapter.
    sched.inject_adapter("cx", Box::new(FakeEngine::new().with_hang()));
    sched.tick().unwrap();
    assert!(
        sched.engine_active("cx"),
        "engine live after the hang turn starts"
    );

    // Signal cancel for the cell → next tick interrupts + stops the engine.
    tx.send("cx".into()).unwrap();
    sched.tick().unwrap();
    assert!(
        !sched.engine_active("cx"),
        "cancel must interrupt + stop the cell engine"
    );
}

// ---------------------------------------------------------------------------
// CLAUDE section (Task 12-rust): ClaudeAdapter via node shim. Uses bin_override
// onto tests/fixtures/* so the tests run without Agent SDK / network.
// ---------------------------------------------------------------------------

use crewd::engines::claude::{ClaudeAdapter, ENV_ALLOWLIST};
use crewd::engines::{EngineAdapter, EngineProcState};

fn fixture(name: &str) -> String {
    format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Poll a child adapter (poll model) until a terminal `Final`/`Failed`
/// arrives or the deadline elapses, collecting all events seen.
fn drain_until_terminal(
    adapter: &mut dyn EngineAdapter,
    deadline: std::time::Duration,
) -> Vec<EngineEvent> {
    let start = std::time::Instant::now();
    let mut all = Vec::new();
    while start.elapsed() < deadline {
        for ev in adapter.poll_events() {
            let terminal = matches!(ev, EngineEvent::Final { .. } | EngineEvent::Failed { .. });
            all.push(ev);
            if terminal {
                return all;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    all
}

fn claude_cfg(shim: &str, profile: Option<&str>, keys: Option<String>) -> EngineSpawnCfg {
    EngineSpawnCfg {
        cwd: "/tmp".into(),
        profile: profile.map(Into::into),
        bin_override: Some(shim.to_string()),
        keys_env_path: keys,
        ..Default::default()
    }
}

#[test]
fn claude_roundtrip_accepted_final_and_resume_reflects_session() {
    let cfg = claude_cfg(&fixture("fake-claude-shim.mjs"), Some("max"), None);
    let mut a = ClaudeAdapter::new(&cfg).unwrap();
    a.start_turn("hello").unwrap();
    let events = drain_until_terminal(&mut a, std::time::Duration::from_secs(5));
    assert!(matches!(
        &events[0],
        EngineEvent::Accepted { engine_turn_id } if engine_turn_id == "t-1"
    ));
    assert!(matches!(
        events.last(),
        Some(EngineEvent::Final { final_answer }) if final_answer == "fake: hello"
    ));
    assert_eq!(a.last_session_id().as_deref(), Some("fake-sess-1"));

    // resume: the next turn reflects the session id passed to the shim (typed
    // resume_session).
    a.resume_session("my-resume-sid").unwrap();
    a.start_turn("again").unwrap();
    let events2 = drain_until_terminal(&mut a, std::time::Duration::from_secs(5));
    assert!(matches!(events2.last(), Some(EngineEvent::Final { .. })));
    assert_eq!(a.last_session_id().as_deref(), Some("my-resume-sid"));

    a.shutdown();
    assert_eq!(a.proc_state(), EngineProcState::Down);
}

#[test]
fn claude_env_allowlist_no_token_leak() {
    let keys_dir = tempdir().unwrap();
    let keys_path = keys_dir.path().join("ai.env");
    std::fs::write(
        &keys_path,
        "ZAI_API_KEY_A=zai-secret-do-not-leak-xyz\nZAI_API_KEY_P=other-p\n",
    )
    .unwrap();
    std::mem::forget(keys_dir); // the keys file outlives the test

    let cfg = claude_cfg(
        &fixture("env-dump.mjs"),
        Some("zai-a"),
        Some(keys_path.to_string_lossy().into()),
    );
    let mut a = ClaudeAdapter::new(&cfg).unwrap();
    a.start_turn("dump").unwrap();
    let events = drain_until_terminal(&mut a, std::time::Duration::from_secs(5));
    a.shutdown();

    let fa = match events.into_iter().find_map(|e| match e {
        EngineEvent::Final { final_answer } => Some(final_answer),
        _ => None,
    }) {
        Some(fa) => fa,
        None => panic!("final answer (env-dump names) not received"),
    };
    let names: Vec<&str> = fa.split(',').filter(|s| !s.is_empty()).collect();
    // 1. every env var name seen by the shim MUST be in the allowlist (§20.7)
    for name in &names {
        // macOS libSystem injects __CF_USER_TEXT_ENCODING into every spawned
        // process (posix_spawn), regardless of env_clear — OS noise outside
        // the daemon's control, not an allowlist leak.
        if *name == "__CF_USER_TEXT_ENCODING" {
            continue;
        }
        assert!(
            ENV_ALLOWLIST.contains(name),
            "env var {name:?} leaked to shim (not in allowlist): {fa}"
        );
    }
    // 2. the RAW key `ZAI_API_KEY_A` must NOT reach the child (only
    //    its mapping onto the allowlisted `ANTHROPIC_AUTH_TOKEN`), and the
    //    mapping MUST have happened — no longer tautological.
    assert!(
        !names.contains(&"ZAI_API_KEY_A") && !names.contains(&"ZAI_API_KEY_P"),
        "raw ZAI key var leaked to the child env: {fa}"
    );
    assert!(
        names.contains(&"ANTHROPIC_AUTH_TOKEN"),
        "zai-a profile must map the key into ANTHROPIC_AUTH_TOKEN: {fa}"
    );
    // 3. the token value MUST never appear as an env var NAME
    assert!(
        !fa.contains("zai-secret-do-not-leak-xyz"),
        "token value leaked in final_answer: {fa}"
    );
}

#[test]
fn claude_zai_profile_without_keys_is_engine_down() {
    let cfg = claude_cfg(&fixture("env-dump.mjs"), Some("zai-a"), None);
    let err = ClaudeAdapter::new(&cfg).unwrap_err();
    assert_eq!(err.code(), "E_ENGINE_DOWN");
}

#[test]
fn claude_abort_emits_error_and_shutdown_terminates() {
    // The fake shim answers `abort` with an `error:"aborted"` event; with the
    // poll model the event arrives asynchronously.
    let cfg = claude_cfg(&fixture("fake-claude-shim.mjs"), Some("max"), None);
    let mut a = ClaudeAdapter::new(&cfg).unwrap();
    a.start_turn("p").unwrap();
    let _ = drain_until_terminal(&mut a, std::time::Duration::from_secs(5)); // Accepted + Final
    a.interrupt().unwrap(); // abort → shim emits error "aborted"
    let evs = drain_until_terminal(&mut a, std::time::Duration::from_secs(5));
    assert!(
        evs.iter()
            .any(|e| matches!(e, EngineEvent::Failed { error } if error == "aborted")),
        "abort should surface an error event: {evs:?}"
    );
    a.shutdown();
    assert_eq!(a.proc_state(), EngineProcState::Down);
}

// ---------------------------------------------------------------------------
// HANDLERS section (Task 10): spawn→status→result; ACL spawn; status of other
// principals; audit cell_spawn_requested BEFORE the thread record (hash-chain).
// ---------------------------------------------------------------------------

fn register_named_cell(state: &crewd::handlers::DaemonState, name: &str, engine: EngineKind) {
    let st = state.store.lock().unwrap();
    let mut ch = state.audit.lock().unwrap();
    st.cell_register(
        &CellDef {
            name: name.into(),
            engine,
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
async fn handler_spawn_status_result_roundtrip() {
    let dir = tempdir().unwrap();
    let handle = crewd::testkit::spawn_daemon(dir.path()).await;
    register_named_cell(&handle.state, "worker-a", EngineKind::Fake);
    let token = handle.issued_tokens["dev-senior"].clone();
    let mut conn = crewd::testkit::connect_as(dir.path(), "dev-senior", &token)
        .await
        .unwrap();

    let r = conn
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"worker-a","task":"do x","idempotency_key":"k1"}),
        )
        .await
        .unwrap();
    let tid = r["crewd_thread_id"].as_str().unwrap().to_string();
    assert_eq!(r["replayed"], false, "first spawn is Created, not replayed");

    let s = conn
        .call("cell_status", serde_json::json!({"crewd_thread_id": tid}))
        .await
        .unwrap();
    assert_eq!(s["threads"][0]["crewd_thread_id"], tid);
    assert_eq!(s["threads"][0]["cell"], "worker-a");
    let state = s["threads"][0]["state"].as_str().unwrap();
    assert!(
        ["spawning", "running", "idle"].contains(&state),
        "state should be non-terminal (scheduler may run): {state}"
    );

    let res = conn
        .call("cell_result", serde_json::json!({"crewd_thread_id": tid}))
        .await
        .unwrap();
    assert_eq!(res["crewd_thread_id"], tid);
    // Separate ID fields (SPEC §20.2): always present, even if None pre-scheduling.
    assert!(res.get("engine_thread_id").is_some());
    assert!(res.get("engine_turn_id").is_some());
    assert!(res.get("engine_session_id").is_some());
    let exit = res["exit_status"].as_str().unwrap();
    assert!(
        [
            "done",
            "interrupted",
            "timeout",
            "failed_unknown",
            "running",
            "spawning"
        ]
        .contains(&exit),
        "exit_status should be a stable value: {exit}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn handler_spawn_denied_without_spawn_capability() {
    let dir = tempdir().unwrap();
    let handle = crewd::testkit::spawn_daemon(dir.path()).await;
    // vault has no "spawn" capability in the test ACL.
    let token = handle.issued_tokens["vault"].clone();
    let mut conn = crewd::testkit::connect_as(dir.path(), "vault", &token)
        .await
        .unwrap();
    let err = conn
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"x","task":"y","idempotency_key":"k"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    handle.shutdown().await;
}

#[tokio::test]
async fn handler_status_other_thread_denied_without_admin() {
    let dir = tempdir().unwrap();
    let handle = crewd::testkit::spawn_daemon(dir.path()).await;
    register_named_cell(&handle.state, "worker-a", EngineKind::Fake);
    // dev-senior (with spawn) creates the thread → created_by_principal = dev-senior.
    let token_d = handle.issued_tokens["dev-senior"].clone();
    let mut conn_d = crewd::testkit::connect_as(dir.path(), "dev-senior", &token_d)
        .await
        .unwrap();
    let r = conn_d
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"worker-a","task":"x","idempotency_key":"k"}),
        )
        .await
        .unwrap();
    let tid = r["crewd_thread_id"].as_str().unwrap().to_string();

    // codex-audit (has spawn, NOT admin_registry) → someone else's status = E_ACL_DENIED.
    let token_c = handle.issued_tokens["codex-audit"].clone();
    let mut conn_c = crewd::testkit::connect_as(dir.path(), "codex-audit", &token_c)
        .await
        .unwrap();
    let err = conn_c
        .call("cell_status", serde_json::json!({"crewd_thread_id": tid}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "E_ACL_DENIED");
    handle.shutdown().await;
}

#[tokio::test]
async fn handler_spawn_audit_cell_spawn_requested_present_and_chain_verifies() {
    let dir = tempdir().unwrap();
    let handle = crewd::testkit::spawn_daemon(dir.path()).await;
    register_named_cell(&handle.state, "worker-a", EngineKind::Fake);
    let token = handle.issued_tokens["dev-senior"].clone();
    let mut conn = crewd::testkit::connect_as(dir.path(), "dev-senior", &token)
        .await
        .unwrap();
    let r = conn
        .call(
            "cell_spawn",
            serde_json::json!({"cell":"worker-a","task":"x","idempotency_key":"k"}),
        )
        .await
        .unwrap();
    let tid = r["crewd_thread_id"].as_str().unwrap();

    // The cell_spawn_requested event is audited (fsync) BEFORE the thread
    // record, and the hash-linked chain verifies end-to-end.
    let audit_path = handle.state.cfg.audit_path();
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        raw.contains("\"kind\":\"cell_spawn_requested\""),
        "missing cell_spawn_requested: {raw}"
    );
    assert!(raw.contains(tid) || raw.contains("worker-a"));
    match crewd_core::audit::AuditChain::verify(&audit_path) {
        crewd_core::audit::VerifyResult::Ok { events, .. } => {
            assert!(events >= 1, "at least one audit event");
        }
        _ => panic!("audit chain must verify"),
    }
    handle.shutdown().await;
}

// ---------------------------------------------------------------------------
// PI section (Task 13): PiAdapter via trace replay (no session resume).
// ---------------------------------------------------------------------------

use crewd::engines::pi::PiAdapter;

fn pi_replay_cfg() -> EngineSpawnCfg {
    EngineSpawnCfg {
        cwd: "/tmp".into(),
        bin_override: Some("node".into()),
        shim_args: vec![fixture("pi-replay.mjs"), fixture("pi-trace.ndjson")],
        ..Default::default()
    }
}

#[test]
fn pi_trace_replay_produces_accepted_and_final() {
    let mut a = PiAdapter::new(&pi_replay_cfg()).unwrap();
    a.start_turn("elenca i file").unwrap();
    let events = drain_until_terminal(&mut a, std::time::Duration::from_secs(5));
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineEvent::Accepted { .. })));
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::Final { final_answer } if final_answer.contains("file_a.txt")
    )));
    a.shutdown();
}

#[test]
fn pi_resume_is_thread_not_resumable() {
    // Both typed resume paths are refused honestly (SPEC §20.6).
    let mut a = PiAdapter::new(&pi_replay_cfg()).unwrap();
    assert_eq!(
        a.resume_session("any-sid").unwrap_err().code(),
        "E_THREAD_NOT_RESUMABLE"
    );
    assert_eq!(
        a.resume_thread("any-tid").unwrap_err().code(),
        "E_THREAD_NOT_RESUMABLE"
    );
    a.shutdown();
}

/// smoke-T16 regression: an EPHEMERAL cell (`~ephemeral-*`, not in the
/// registry) with a queued job MUST be discovered and scheduled by the tick —
/// before the fix the tick only iterated `cell_list_defs()` and ephemeral
/// threads stayed `spawning` forever.
#[test]
fn scheduler_starts_ephemeral_cells_not_in_registry() {
    let (mut sched, store, audit, _audit_path) = scheduler_fixture();
    let thread_id = {
        let s = store.lock().unwrap();
        let mut ch = audit.lock().unwrap();
        // NO register_cell: the target is ephemeral, via real spawn_idempotent.
        let req = crewd_core::spawn::SpawnRequest {
            caller: "cli:primary",
            cell_name: "~ephemeral-regress1",
            idempotency_key: "smoke-regress-1",
            payload: "do ephemeral work",
        };
        let out = s
            .spawn_idempotent(&req, EngineKind::Fake, None, None, "/tmp", None, 8, &mut ch)
            .unwrap();
        match out {
            crewd_core::spawn::SpawnOutcome::Created(t) => t.crewd_thread_id,
            _ => panic!("expected Created"),
        }
    };

    sched.tick().unwrap();

    let s = store.lock().unwrap();
    let t = s.thread_get(&thread_id).unwrap().unwrap();
    assert_eq!(
        t.state,
        ThreadState::Idle,
        "ephemeral thread must be scheduled and complete (was stuck spawning pre-fix)"
    );
    let jobs = s.jobs_for_thread(&thread_id).unwrap();
    assert_eq!(jobs[0].state, JobState::Finished);
}

/// smoke-T16 regression: the child engine MUST run in the requested cwd
/// (without `current_dir` it inherited the daemon's cwd and the cell worked
/// on the wrong repository).
#[test]
fn claude_adapter_runs_child_in_requested_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let want = dir.path().canonicalize().unwrap();
    let cfg = EngineSpawnCfg {
        cwd: want.to_string_lossy().into_owned(),
        bin_override: Some(fixture("fake-claude-shim.mjs")),
        shim_args: vec!["--cwd".into()],
        ..Default::default()
    };
    let mut a = ClaudeAdapter::new(&cfg).unwrap();
    a.start_turn("dove sei?").unwrap();
    let evs = drain_until_terminal(&mut a, std::time::Duration::from_secs(10));
    let final_answer = evs
        .iter()
        .find_map(|e| match e {
            EngineEvent::Final { final_answer } => Some(final_answer.clone()),
            _ => None,
        })
        .expect("final event");
    assert_eq!(
        std::path::Path::new(&final_answer).canonicalize().unwrap(),
        want,
        "child must run in cfg.cwd"
    );
    a.shutdown();
}
