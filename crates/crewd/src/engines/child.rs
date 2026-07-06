//! Shared line-delimited (NDJSON) child-process transport for the engine
//! adapters. A background reader thread parses each stdout line
//! into zero or more `EngineEvent`s via a per-engine closure and forwards them
//! on an `mpsc` channel; the adapter drains them non-blocking with `poll`.
//!
//! On EOF — the child exiting or being `kill -9`'d — the reader sets `alive`
//! false and exits, so `proc_state` reports `Down`. This is what makes the
//! chaos matrix testable: the scheduler observes the death between ticks and
//! fails the in-flight turn honestly, instead of the old design where the
//! adapter blocked inside `start_turn` until a terminal event.
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use crewd_core::engine::EngineEvent;
use crewd_core::error::BusError;

use crate::engines::EngineProcState;

/// A child speaking a line-delimited protocol, parsed into `EngineEvent`s.
pub(crate) struct LineChild {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: Receiver<EngineEvent>,
    reader: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
    pid: u32,
}

impl std::fmt::Debug for LineChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineChild")
            .field("pid", &self.pid)
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .finish()
    }
}

impl LineChild {
    /// Spawn `cmd`, optionally read one synchronous first line (`wait_first_line`
    /// — e.g. the claude shim `ready`, EOF there is `E_ENGINE_DOWN`), then start
    /// the reader thread. `parse` maps one JSON value to zero+ events (side
    /// effects allowed, e.g. capturing a session id into a shared cell).
    pub fn spawn(
        mut cmd: Command,
        wait_first_line: bool,
        mut parse: impl FnMut(serde_json::Value, &Sender<EngineEvent>) + Send + 'static,
        label: &'static str,
    ) -> Result<Self, BusError> {
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // setsid: child leads a new process group so shutdown can kill the
            // whole tree (interpreter + any grandchild) via the group id.
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| {
            BusError::EngineDown(format!("{label} spawn: {}", safe_prefix(&e.to_string())))
        })?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let mut stdout = child
            .stdout
            .take()
            .map(BufReader::new)
            .ok_or_else(|| BusError::Internal(format!("{label} stdout missing")))?;

        if wait_first_line {
            let mut first = String::new();
            let n = stdout.read_line(&mut first).map_err(|e| {
                BusError::EngineDown(format!("{label} read: {}", safe_prefix(&e.to_string())))
            })?;
            if n == 0 {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BusError::EngineDown(format!("{label} closed before ready")));
            }
        }

        let (tx, rx) = mpsc::channel::<EngineEvent>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_reader = alive.clone();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match stdout.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // EOF / read error → child gone
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                            parse(v, &tx);
                        }
                    }
                }
            }
            alive_reader.store(false, Ordering::SeqCst);
        });

        Ok(LineChild {
            child: Some(child),
            stdin,
            rx,
            reader: Some(reader),
            alive,
            pid,
        })
    }

    /// OS pid of the child (its own process-group leader). Chaos tests use it to
    /// `kill -9` the whole group.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn send(&mut self, v: serde_json::Value) -> Result<(), BusError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| BusError::Internal("child stdin gone".into()))?;
        let mut s = serde_json::to_string(&v).map_err(|e| BusError::Internal(e.to_string()))?;
        s.push('\n');
        stdin.write_all(s.as_bytes()).map_err(|e| {
            BusError::EngineDown(format!("child write: {}", safe_prefix(&e.to_string())))
        })?;
        stdin.flush().map_err(|e| {
            BusError::EngineDown(format!("child flush: {}", safe_prefix(&e.to_string())))
        })?;
        Ok(())
    }

    /// Drain all events available now (never blocks).
    pub fn poll(&mut self) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }

    pub fn proc_state(&self) -> EngineProcState {
        if self.alive.load(Ordering::SeqCst) {
            EngineProcState::Up
        } else {
            EngineProcState::Down
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            #[cfg(unix)]
            {
                // child leads its own pgroup (setsid) → kill the whole tree
                // with one direct syscall; no external `kill` binary (which
                // may be a limited applet on Android/Termux).
                if let Some(pgid) = rustix::process::Pid::from_raw(pid as i32) {
                    let _ =
                        rustix::process::kill_process_group(pgid, rustix::process::Signal::Kill);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

impl Drop for LineChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.shutdown();
        }
    }
}

/// ≤8-char single-line prefix so a foreign error string never leaks secrets
/// into logs/argv (SPEC §20.7 hardening, uniform across engine adapters).
pub(crate) fn safe_prefix(s: &str) -> String {
    s.chars()
        .take(8)
        .collect::<String>()
        .replace(['\n', '\r'], " ")
}
