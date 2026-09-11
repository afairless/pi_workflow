# Implementation Plan: pi-plan — a Rust Orchestrator for the TODO.md Workflow

Source: `docs/research/plan-rust-orchestrator.md`

Plan: a deterministic, TS-free orchestrator binary (`pi-plan`) that supervises
TODO.md rows by spawning a fresh `pi --mode rpc` worker per row, enforcing a
per-row run/budget, answering permission dialogs `extension_ui_request` inline
over the RPC extension-UI sub-protocol, and classifying completion
git-keyed. No LLM sits in the control loop; the repository stays the source
of truth.

Workflow per step: implement → `cargo test` → `cargo fmt --check` →
`cargo clippy --all-targets --all-features -- -D warnings` → commit with the
message in the table → stop. Integration tests land in Step 4 and the manual
E2E gate in Step 9; docs are the final step.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `chore: initialize pi_plan_workflow repository` | Repo scaffolding | `README.md`, `.gitignore`, `AGENTS.md`, `rust-toolchain.toml` (1.91.1), `Cargo.toml` (pinned deps), `src/main.rs` placeholder, `docs/ARCHITECTURE.md` skeleton; plan + research docs committed | smoke |
| 2 | `feat: parse TODO.md steps and match rows against git history` | Parser + matcher | `src/todo.rs`, `src/git.rs` (ports of `todo.ts`, `git.ts`) | unit, property |
| 3 | `feat: add worker prompt builder with ASK contract and durable state` | Prompt + state + config | `src/prompt.rs`, `src/state.rs`, `src/config.rs` | unit |
| 4 | `feat: add pi RPC client over JSONL with extension-UI dialog answering` | RPC client | `src/rpc.rs`, `test-fixtures/rpc-peer/` (fake pi JSONL peer) | unit, integration |
| 5 | `feat: add worker port over the pi RPC client` | Worker adapter | `src/worker.rs` (WorkerPort trait, RpcWorker, stall ceiling, argv builder) | unit |
| 6 | `feat: implement supervise loop with retries, question pause, and crash recovery` | Orchestration loop | `src/supervise.rs` (run/retry/ask state machine) | unit |
| 7 | `feat: gate spawns behind scenario-aware dirty-worktree check and persist in-progress state` | Dirty-WIP gate + in-progress state | `src/state.rs` (`agentId`/`startedAt`), `src/supervise.rs`, `docs/ARCHITECTURE.md` | unit |
| 8 | `feat: surface worker traces and inline permission dialogs in a terminal UI` | Operator UI + CLI | `src/ui.rs`, `src/cli.rs`, `src/main.rs` (clap: supervise/status/stop/mark/step) | unit, smoke |
| 9 | `test: add end-to-end acceptance procedure and fixture` | E2E gate | `test-fixtures/spike/` (3-row inner repo), `docs/acceptance-e2e.md` | manual E2E |
| 10 | `docs: document orchestrator usage, config, and migration from /supervise` | User docs + migration | `README.md`, `docs/ARCHITECTURE.md` (refresh) | — |
