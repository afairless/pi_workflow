//! pi-plan — deterministic orchestrator for the TODO.md workflow.
//!
//! Step 1 placeholder: the real clap CLI (`supervise` / `status` / `stop` /
//! `mark` / `step`) lands in a later step. Keeping a compiling binary here
//! means `cargo build` / `cargo test` work from the first commit.

fn main() {
    println!(
        "pi-plan {} — TODO.md orchestrator (scaffold stage)",
        env!("CARGO_PKG_VERSION")
    );
}
