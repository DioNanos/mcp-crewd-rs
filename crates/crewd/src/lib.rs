//! `crewd` library target. The daemon logic is exposed as a library so that
//! the integration tests (`tests/`) and the binary (`main.rs`) share one code
//! path. `pub mod testkit` is always compiled (used by the test suite).
pub mod auth;
pub mod config;
pub mod delivery;
pub mod engines;
pub mod handlers;
pub mod scheduler;
pub mod server;
pub mod supervisor;
pub mod testkit;
