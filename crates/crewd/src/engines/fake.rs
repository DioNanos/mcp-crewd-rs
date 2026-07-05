//! In-process `FakeEngine` (crewd Fase 2 Task 8; AUDIT2 R2 poll model). No child
//! process, no I/O: `start_turn` queues `EngineEvent`s that `poll_events`
//! drains (matching the non-blocking child adapters). Used by the scheduler
//! (Task 9) and handler tests; the chaos suite uses the native
//! `crewd-fake-engine` binary via a real child adapter instead.
//!
//! Behaviour:
//! - `start_turn` queues `Accepted{engine_turn_id:"fake-turn-<n>"}`, then:
//!   - `hang`      → only Accepted (turn stays in flight: timeout/cancel tests)
//!   - `fail_next` → `Failed{error:"fake-failure"}` (consumed once)
//!   - otherwise   → `Final{final_answer:"done: <payload>"}`
//! - `interrupt` marks the in-flight turn interrupted (observable via a flag).
//! - `caps`: all true except `supports_stream_replay`.
use std::collections::VecDeque;

use crewd_core::engine::{EngineCaps, EngineEvent};
use crewd_core::error::BusError;

use crate::engines::{EngineAdapter, EngineProcState};

#[derive(Debug)]
pub struct FakeEngine {
    /// If true, the next `start_turn` queues `Failed` instead of `Final`.
    pub fail_next: bool,
    /// If true, `start_turn` queues only `Accepted` (turn left in flight).
    pub hang: bool,
    /// The last `interrupt` marked an in-flight turn (observable in tests).
    pub interrupted: bool,
    turn_counter: u64,
    in_flight: bool,
    queued: VecDeque<EngineEvent>,
    proc: EngineProcState,
    last_session: Option<String>,
}

impl FakeEngine {
    pub fn new() -> Self {
        Self {
            fail_next: false,
            hang: false,
            interrupted: false,
            turn_counter: 0,
            in_flight: false,
            queued: VecDeque::new(),
            proc: EngineProcState::Up,
            last_session: None,
        }
    }

    /// Next `start_turn` queues `Failed` instead of `Final`.
    pub fn with_fail_next(mut self) -> Self {
        self.fail_next = true;
        self
    }

    /// `start_turn` queues only `Accepted` (no `Final`).
    pub fn with_hang(mut self) -> Self {
        self.hang = true;
        self
    }
}

impl Default for FakeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineAdapter for FakeEngine {
    fn caps(&self) -> EngineCaps {
        EngineCaps {
            supports_session_resume: true,
            supports_abort: true,
            supports_stream_replay: false,
            supports_model_override: true,
            supports_yolo: true,
        }
    }

    fn start_turn(&mut self, payload: &str) -> Result<(), BusError> {
        if self.proc == EngineProcState::Down {
            return Err(BusError::EngineDown("fake engine down".into()));
        }
        self.interrupted = false;
        self.turn_counter += 1;
        let turn_id = format!("fake-turn-{}", self.turn_counter);
        self.queued.push_back(EngineEvent::Accepted { engine_turn_id: turn_id });
        self.in_flight = true;

        if self.hang {
            return Ok(()); // only Accepted; turn stays in flight
        }
        if self.fail_next {
            self.queued.push_back(EngineEvent::Failed { error: "fake-failure".into() });
            self.fail_next = false; // consumed
            self.in_flight = false;
            return Ok(());
        }
        self.last_session = Some("fake-sess-1".into());
        self.queued
            .push_back(EngineEvent::Final { final_answer: format!("done: {payload}") });
        self.in_flight = false;
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<EngineEvent> {
        self.queued.drain(..).collect()
    }

    fn interrupt(&mut self) -> Result<(), BusError> {
        self.interrupted = self.in_flight;
        self.in_flight = false;
        Ok(())
    }

    fn resume_session(&mut self, engine_session_id: &str) -> Result<(), BusError> {
        // supports_session_resume == true; honest resume (SPEC §20.6): the
        // follow-up runs on materialized history, never replays the lost turn.
        self.last_session = Some(engine_session_id.to_string());
        Ok(())
    }

    fn engine_session_id(&self) -> Option<String> {
        self.last_session.clone()
    }

    fn proc_state(&self) -> EngineProcState {
        self.proc
    }

    fn shutdown(&mut self) {
        self.proc = EngineProcState::Down;
        self.in_flight = false;
        self.queued.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(e: &mut FakeEngine, payload: &str) -> Vec<EngineEvent> {
        e.start_turn(payload).unwrap();
        e.poll_events()
    }

    #[test]
    fn happy_path_accepted_then_final_in_order() {
        let mut e = FakeEngine::new();
        let evs = run(&mut e, "hello");
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], EngineEvent::Accepted { engine_turn_id } if engine_turn_id == "fake-turn-1"));
        assert!(matches!(&evs[1], EngineEvent::Final { final_answer } if final_answer == "done: hello"));
    }

    #[test]
    fn fail_next_emits_failed_and_consumes_flag() {
        let mut e = FakeEngine::new().with_fail_next();
        let evs = run(&mut e, "x");
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[1], EngineEvent::Failed { error } if error == "fake-failure"));
        let evs2 = run(&mut e, "y");
        assert!(matches!(evs2.last(), Some(EngineEvent::Final { .. })));
    }

    #[test]
    fn hang_emits_only_accepted_and_stays_in_flight() {
        let mut e = FakeEngine::new().with_hang();
        let evs = run(&mut e, "x");
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], EngineEvent::Accepted { .. }));
        e.interrupt().unwrap();
        assert!(e.interrupted);
    }

    #[test]
    fn caps_all_true_except_stream_replay() {
        let c = FakeEngine::new().caps();
        assert!(c.supports_session_resume);
        assert!(c.supports_abort);
        assert!(!c.supports_stream_replay);
        assert!(c.supports_model_override);
        assert!(c.supports_yolo);
    }

    #[test]
    fn shutdown_marks_proc_down() {
        let mut e = FakeEngine::new();
        assert_eq!(e.proc_state(), EngineProcState::Up);
        e.shutdown();
        assert_eq!(e.proc_state(), EngineProcState::Down);
    }

    #[test]
    fn resume_session_is_ok_for_fake() {
        let mut e = FakeEngine::new();
        assert!(e.resume_session("any-sid").is_ok());
        assert_eq!(e.engine_session_id().as_deref(), Some("any-sid"));
    }
}
