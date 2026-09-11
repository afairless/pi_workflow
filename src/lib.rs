//! pi-plan library crate — the orchestrator modules.
//!
//! Declared here (not in the binary) so each module carries per-module unit
//! tests without the dead-code lint firing on a single-binary package.

pub mod git;
pub mod todo;
