//! Contract tests for the CodexAdapter (crewd Phase 2 Task 11).
//!
//! Verifies against a mock app-server (tests/fixtures/mock-codex-appserver.mjs)
//! that CodexAdapter serializes the EXACT JSON-RPC v2 protocol fields
//! (approvalPolicy, sandbox, sandboxPolicy.type, optOutNotificationMethods)
//! and that the YOLO fail-clear check (§20.7) rejects a response with
//! approvalPolicy:"untrusted" → E_POLICY_DENIED. Also structurally verifies
//! the field names against the generated schema (CODEX_SCHEMA_DIR, default
//! ~/Dev/60_toolchains/codex-vl/codex-rs/app-server-protocol/schema/typescript/v2);
//! if the schema is absent the check is skipped with a warning.

use std::path::PathBuf;
use std::sync::Mutex;

use crewd::engines::codex::CodexAdapter;
use crewd::engines::{EngineAdapter, EngineSpawnCfg};
use crewd_core::engine::EngineEvent;
use tempfile::tempdir;

/// Path of the mock app-server fixture.
fn mock_appserver() -> String {
    format!(
        "{}/../../tests/fixtures/mock-codex-appserver.mjs",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Default schema dir (env CODEX_SCHEMA_DIR overrides).
fn schema_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CODEX_SCHEMA_DIR") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
    }
    // No hardcoded default: without CODEX_SCHEMA_DIR the contract test is
    // skipped (the schema dir is developer-machine specific).
    None
}

/// One CodexAdapter at a time to avoid clashes on the log file (Mutex).
static ADAPTER_LOCK: Mutex<()> = Mutex::new(());

fn cfg_with_log(_log_path: &str) -> EngineSpawnCfg {
    EngineSpawnCfg {
        cwd: "/tmp".into(),
        bin_override: Some("node".into()),
        shim_args: vec![mock_appserver()],
        keys_env_path: None,
        ..Default::default()
    }
}

/// Poll model: drain events until a terminal one arrives or timeout.
fn drain_until_terminal(a: &mut CodexAdapter, deadline: std::time::Duration) -> Vec<EngineEvent> {
    let start = std::time::Instant::now();
    let mut all = Vec::new();
    while start.elapsed() < deadline {
        for ev in a.poll_events() {
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

/// Reads the request log written by the mock (one {method,params} per line).
fn read_mock_log(path: &std::path::Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

#[test]
fn codex_serializes_exact_protocol_fields_and_turn_completes() {
    let _guard = ADAPTER_LOCK.lock().unwrap();
    let dir = tempdir().unwrap();
    let log = dir.path().join("req.jsonl");
    std::env::set_var("CODEX_MOCK_LOG", &log);
    std::env::remove_var("CODEX_MOCK_POLICY");

    let cfg = cfg_with_log(log.to_string_lossy().as_ref());
    let mut a = CodexAdapter::new(&cfg).expect("adapter up");

    a.start_turn("elenca").expect("turn/start ack");
    let events = drain_until_terminal(&mut a, std::time::Duration::from_secs(5));
    assert!(
        events.iter().any(
            |e| matches!(e, EngineEvent::Accepted { engine_turn_id } if engine_turn_id == "t-1")
        ),
        "Accepted t-1 emitted: {events:?}"
    );
    let final_answer = events.iter().find_map(|e| match e {
        EngineEvent::Final { final_answer } => Some(final_answer.clone()),
        _ => None,
    });
    assert_eq!(final_answer.as_deref(), Some("done: elenca"));

    a.shutdown();
    std::env::remove_var("CODEX_MOCK_LOG");

    // Verify EXACT fields serialized by the adapter.
    let reqs = read_mock_log(&log);
    let init = reqs
        .iter()
        .find(|v| v["method"] == "initialize")
        .expect("initialize");
    let opt = init["params"]["capabilities"]["optOutNotificationMethods"]
        .as_array()
        .expect("optOut array");
    let opt_vals: Vec<&str> = opt.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        opt_vals,
        [
            "item/agentMessage/delta",
            "item/reasoning/summaryTextDelta",
            "item/reasoning/summaryPartAdded",
            "item/reasoning/textDelta"
        ]
    );
    assert_eq!(init["params"]["clientInfo"]["name"], "crewd");
    assert_eq!(init["params"]["clientInfo"]["title"], "crewd cell fabric");

    let tstart = reqs
        .iter()
        .find(|v| v["method"] == "thread/start")
        .expect("thread/start");
    assert_eq!(tstart["params"]["approvalPolicy"], "never");
    assert_eq!(tstart["params"]["sandbox"], "danger-full-access");

    let turn = reqs
        .iter()
        .find(|v| v["method"] == "turn/start")
        .expect("turn/start");
    assert_eq!(turn["params"]["approvalPolicy"], "never");
    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
    assert_eq!(turn["params"]["input"][0]["type"], "text");
}

#[test]
fn codex_resume_verifies_yolo() {
    let _guard = ADAPTER_LOCK.lock().unwrap();
    let dir = tempdir().unwrap();
    let log = dir.path().join("req.jsonl");
    std::env::set_var("CODEX_MOCK_LOG", &log);
    std::env::remove_var("CODEX_MOCK_POLICY");

    let cfg = cfg_with_log(log.to_string_lossy().as_ref());
    let mut a = CodexAdapter::new(&cfg).expect("adapter up");
    // resume is TYPED by engine THREAD id and wired to `threadId`.
    a.resume_thread("th-1").expect("resume verifies YOLO");
    a.shutdown();
    std::env::remove_var("CODEX_MOCK_LOG");

    let reqs = read_mock_log(&log);
    let r = reqs
        .iter()
        .find(|v| v["method"] == "thread/resume")
        .expect("thread/resume");
    assert_eq!(r["params"]["approvalPolicy"], "never");
    assert_eq!(r["params"]["sandbox"], "danger-full-access");
    assert_eq!(r["params"]["threadId"], "th-1");
}

#[test]
fn codex_resume_session_never_forwards_into_threadid() {
    // an engine SESSION id must never reach the `threadId` field. The
    // codex adapter has no session resume, so `resume_session` fails honestly
    // and NO thread/resume request is sent with the session id.
    let _guard = ADAPTER_LOCK.lock().unwrap();
    let dir = tempdir().unwrap();
    let log = dir.path().join("req.jsonl");
    std::env::set_var("CODEX_MOCK_LOG", &log);
    std::env::remove_var("CODEX_MOCK_POLICY");

    let cfg = cfg_with_log(log.to_string_lossy().as_ref());
    let mut a = CodexAdapter::new(&cfg).expect("adapter up");
    let err = a.resume_session("sess-should-not-leak").unwrap_err();
    assert_eq!(err.code(), "E_THREAD_NOT_RESUMABLE");
    a.shutdown();
    std::env::remove_var("CODEX_MOCK_LOG");

    let reqs = read_mock_log(&log);
    assert!(
        !reqs.iter().any(|v| v["method"] == "thread/resume"),
        "session id must not trigger a thread/resume"
    );
    assert!(
        !raw_log_contains(&log, "sess-should-not-leak"),
        "session id must never appear in any request"
    );
}

/// True if the raw request log contains `needle` anywhere.
fn raw_log_contains(path: &std::path::Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .contains(needle)
}

#[test]
fn codex_turn_start_error_fails_clear() {
    // a JSON-RPC error on turn/start must fail clear, not degrade to
    // Null. `start_turn` blocks only for the ack, so the error surfaces there.
    let _guard = ADAPTER_LOCK.lock().unwrap();
    std::env::remove_var("CODEX_MOCK_POLICY");
    std::env::remove_var("CODEX_MOCK_LOG");
    std::env::set_var("CODEX_MOCK_TURN_ERROR", "1");
    let cfg = EngineSpawnCfg {
        cwd: "/tmp".into(),
        bin_override: Some("node".into()),
        shim_args: vec![mock_appserver()],
        ..Default::default()
    };
    let mut a = CodexAdapter::new(&cfg).expect("adapter up");
    let err = a.start_turn("x").unwrap_err();
    assert_eq!(err.code(), "E_ENGINE_DOWN");
    a.shutdown();
    std::env::remove_var("CODEX_MOCK_TURN_ERROR");
}

#[test]
fn codex_turn_start_policy_downgrade_is_policy_denied() {
    // a downgraded policy echoed on turn/start must be rejected.
    let _guard = ADAPTER_LOCK.lock().unwrap();
    std::env::remove_var("CODEX_MOCK_POLICY");
    std::env::remove_var("CODEX_MOCK_LOG");
    std::env::set_var("CODEX_MOCK_TURN_POLICY", "untrusted");
    let cfg = EngineSpawnCfg {
        cwd: "/tmp".into(),
        bin_override: Some("node".into()),
        shim_args: vec![mock_appserver()],
        ..Default::default()
    };
    let mut a = CodexAdapter::new(&cfg).expect("adapter up");
    let err = a.start_turn("x").unwrap_err();
    assert_eq!(err.code(), "E_POLICY_DENIED");
    a.shutdown();
    std::env::remove_var("CODEX_MOCK_TURN_POLICY");
}

#[test]
fn codex_policy_mismatch_is_policy_denied() {
    let _guard = ADAPTER_LOCK.lock().unwrap();
    std::env::set_var("CODEX_MOCK_POLICY", "untrusted");
    let cfg = EngineSpawnCfg {
        cwd: "/tmp".into(),
        bin_override: Some("node".into()),
        shim_args: vec![mock_appserver()],
        ..Default::default()
    };
    let err = CodexAdapter::new(&cfg).unwrap_err();
    assert_eq!(err.code(), "E_POLICY_DENIED");
    std::env::remove_var("CODEX_MOCK_POLICY");
}

#[test]
fn codex_field_names_exist_in_schema() {
    // Structural check: the field names the adapter serializes must exist in the
    // generated schema. Skip with a printed warning if the schema is unavailable.
    let dir = match schema_dir() {
        Some(d) => d,
        None => {
            eprintln!("warn: CODEX_SCHEMA_DIR not found — skipping structural schema check");
            return;
        }
    };
    let blob = {
        let mut s = String::new();
        for entry in std::fs::read_dir(&dir).expect("schema dir") {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) == Some("ts") {
                s.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
        // Fallback corpus: some runtime fields (e.g. `optOutNotificationMethods`,
        // documented in app-server/README.md §initialize for 0.142.5) are not
        // yet in the generated TS types. The installed app-server README is a
        // secondary normative source: schema types FIRST, README as fallback.
        // Path: <schema>/typescript/v2 -> ../../../../app-server/README.md
        // (ancestors(): 0=v2, 1=typescript, 2=schema, 3=app-server-protocol, 4=codex-rs)
        if let Some(readme) = dir
            .ancestors()
            .nth(4)
            .map(|root| root.join("app-server").join("README.md"))
            .filter(|p| p.is_file())
        {
            s.push_str(&std::fs::read_to_string(&readme).unwrap_or_default());
        } else {
            eprintln!("warn: app-server README fallback not found next to schema dir");
        }
        s
    };
    // Names used verbatim by the adapter.
    for name in [
        "approvalPolicy",
        "sandboxPolicy",
        "optOutNotificationMethods",
        "dangerFullAccess",
        "danger-full-access",
        "threadId",
        "turnId",
        "clientInfo",
    ] {
        assert!(
            blob.contains(name),
            "schema is missing field name {name:?} — adapter drift"
        );
    }
}
