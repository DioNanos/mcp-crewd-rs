//! `crewd-core` — pure, in-process-testable protocol library for the
//! mcp-crewd-rs bus. No sockets, no spawned processes (those live in the
//! `crewd` daemon and the `crew` CLI/shim). Names are copied verbatim from
//! `SPEC.md` v0.1.
pub mod acl;
pub mod adapter;
pub mod audit;
pub mod canonical;
pub mod cells;
pub mod engine;
pub mod error;
pub mod jobs;
pub mod principal;
pub mod quota;
pub mod spawn;
pub mod state;
pub mod store;
pub mod threads;
pub mod tickets;
pub mod types;
pub mod validators;
pub mod wire;
