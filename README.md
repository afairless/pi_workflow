# pi-plan

A deterministic, TS-free orchestrator for the TODO.md workflow. `pi-plan` is a
single Rust binary that supervises TODO.md rows by spawning a fresh
`pi --mode rpc` worker process per row, enforcing a per-row run budget,
answering permission dialogs inline over the RPC extension-UI sub-protocol,
and classifying completion git-keyed. No LLM sits in the control loop; the
repository stays the source of truth.

## Layout

```text
pi_plan_workflow/
  Cargo.toml            binary crate: pi-plan (pinned deps)
  rust-toolchain.toml   channel = "1.91.1"
  src/                  main, cli, config, todo, git, prompt, state,
                        worker, rpc, supervise, ui
  test-fixtures/        fake pi RPC peer + manual E2E spike repo
  prompts/              worker persona preamble
  docs/                 architecture + acceptance + research plans
```

## Quick start

```bash
cargo build --release
cargo test
```

See `docs/ARCHITECTURE.md` for the system overview and `TODO.md` for the
commit-by-commit implementation plan.
