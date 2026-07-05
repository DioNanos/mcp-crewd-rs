//! `crewd-core` — pure, in-process-testable protocol library for the
//! mcp-crewd-rs bus. No sockets, no spawned processes (those live in the
//! `crewd` daemon and the `crew` CLI/shim). Names are copied verbatim from
//! `SPEC.md` v0.1.
pub mod types;
pub mod error;
pub mod canonical;
pub mod audit;
pub mod acl;
pub mod principal;
pub mod store;
pub mod cells;
pub mod threads;
pub mod jobs;
pub mod spawn;
pub mod engine;
pub mod validators;
pub mod state;
pub mod tickets;
pub mod quota;
pub mod adapter;
pub mod wire;
