//! pi-plan library crate — the orchestrator modules.
//!
//! Declared here (not in the binary) so each module carries per-module unit
//! tests without the dead-code lint firing on a single-binary package.

pub mod config;
pub mod git;
pub mod prompt;
pub mod rpc;
pub mod state;
pub mod todo;
pub mod worker;
