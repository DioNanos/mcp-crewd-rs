//! Integration tests for the `crewd-fake-engine` binary (Task 15-pre).
//!
//! Spawns the binary via `std::process::Command` (path provided by Cargo at
//! compile time: `env!("CARGO_BIN_EXE_crewd-fake-engine")` — Cargo 1.85 sets
//! the var keeping the dashes in the name) and verifies the NDJSON protocol:
//! turn roundtrip (+ resume), --fail, --hang. No new dependencies
//! (std + serde_json, already in `crewd`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// Timeout for each expected line (the fake engine is local and responsive).
const TIMEOUT: Duration = Duration::from_secs(2);

fn spawn(args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_crewd-fake-engine"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crewd-fake-engine")
}

/// Non-blocking stdout line reader: a thread pumps lines into a channel,
/// the test consumes them with a timeout.
struct Lines {
    rx: mpsc::Receiver<String>,
}

impl Lines {
    fn new(child: &mut Child) -> Self {
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { rx }
    }

    /// Next line within TIMEOUT, parsed as JSON; panics on timeout/invalid JSON.
    fn expect(&self, label: &str) -> Value {
        let line = self
            .rx
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|e| panic!("timeout waiting for {label}: {e}"));
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON for {label}: {line} ({e})"))
    }

    /// Asserts NO line within TIMEOUT (for the --hang case).
    fn expect_none(&self, label: &str) {
        match self.rx.recv_timeout(TIMEOUT) {
            Ok(extra) => panic!("unexpected output for {label}: {extra}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("stdout closed early for {label} (process exited unexpectedly)");
            }
        }
    }
}

/// Cleanup: kill + wait (ignores errors if the child already exited).
fn cleanup(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn turn_roundtrip_and_resume() {
    let mut child = spawn(&[]);
    let lines = Lines::new(&mut child);
    // Keep the stdin handle open for two consecutive turns.
    let mut stdin = child.stdin.take().expect("stdin piped");

    // ready
    let ready = lines.expect("ready");
    assert_eq!(ready["ev"].as_str(), Some("ready"));
    assert!(ready["session_id"].is_null());

    // turn 1: base -> accepted(t-1) + final(fake-sess-1)
    writeln!(stdin, r#"{{"op":"turn","prompt":"ciao"}}"#).unwrap();
    stdin.flush().unwrap();
    let acc = lines.expect("accepted");
    assert_eq!(acc["ev"].as_str(), Some("accepted"));
    assert_eq!(acc["engine_turn_id"].as_str(), Some("t-1"));
    let fin = lines.expect("final");
    assert_eq!(fin["ev"].as_str(), Some("final"));
    assert_eq!(fin["final_answer"].as_str(), Some("fake: ciao"));
    assert_eq!(fin["session_id"].as_str(), Some("fake-sess-1"));

    // turn 2: resume_session reflected -> accepted(t-2) + final(reflected session_id)
    writeln!(
        stdin,
        r#"{{"op":"turn","prompt":"ancora","resume_session":"sess-x"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let acc2 = lines.expect("accepted2");
    assert_eq!(acc2["engine_turn_id"].as_str(), Some("t-2"));
    let fin2 = lines.expect("final2");
    assert_eq!(fin2["final_answer"].as_str(), Some("fake: ancora"));
    assert_eq!(fin2["session_id"].as_str(), Some("sess-x"));

    drop(stdin);
    cleanup(child);
}

#[test]
fn fail_flag_emits_error_after_accepted() {
    let mut child = spawn(&["--fail"]);
    let lines = Lines::new(&mut child);
    let mut stdin = child.stdin.take().expect("stdin piped");

    let _ = lines.expect("ready");
    writeln!(stdin, r#"{{"op":"turn","prompt":"x"}}"#).unwrap();
    stdin.flush().unwrap();
    let acc = lines.expect("accepted");
    assert_eq!(acc["ev"].as_str(), Some("accepted"));
    let err = lines.expect("error");
    assert_eq!(err["ev"].as_str(), Some("error"));
    assert_eq!(err["error"].as_str(), Some("engine-failure"));

    drop(stdin);
    cleanup(child);
}

#[test]
fn hang_flag_no_output_after_accepted() {
    let mut child = spawn(&["--hang"]);
    let lines = Lines::new(&mut child);
    let mut stdin = child.stdin.take().expect("stdin piped");

    let _ = lines.expect("ready");
    writeln!(stdin, r#"{{"op":"turn","prompt":"x"}}"#).unwrap();
    stdin.flush().unwrap();
    let acc = lines.expect("accepted");
    assert_eq!(acc["ev"].as_str(), Some("accepted"));

    // --hang: after accepted NO further output within 2s.
    lines.expect_none("post-accepted (hang)");

    drop(stdin);
    cleanup(child);
}
