# Architecture — pi-plan

Skeleton (Step 1). This document grows with the implementation; it is updated
in the same commit as the changes it describes (see `AGENTS.md`).

## Overview

`pi-plan` is a deterministic orchestrator that supervises the TODO.md
workflow. For each open row it spawns a fresh `pi --mode rpc` worker
process, monitors the worker's JSONL event stream, answers permission
dialogs inline over the extension-UI sub-protocol, and decides completion
from git history (never from worker claims alone).

The orchestrator is plain code — no LLM sits in the control loop. A stop is
just a stop.

## Module map (planned)

```text
src/
  main.rs        binary entry
  cli.rs         subcommand parsing (supervise/status/stop/mark/step)
  config.rs      supervisor.config.json schema + precedence
  todo.rs        TODO.md row contract parser
  git.rs         git facade + git-keyed completion matcher
  prompt.rs      worker prompt builder (ASK contract)
  state.rs       supervisor-state.json (crash recovery)
  worker.rs      WorkerPort trait + RPC worker impl + stall ceiling
  rpc.rs         pi RPC client: JSONL framing, commands, events, dialogs
  supervise.rs   run/retry/ask state machine
  ui.rs          plain-terminal renderer (live tail, status line, dialogs)
```

## Contracts (details land with their modules)

1. **TODO.md row contract** — `## Steps` table, `Source:` line, `## Done`.
2. **Completion, git-keyed** — planned message vs `git log` subjects via
   similarity tiers (`exact ≥ 0.9 ≥ similar ≥ 0.8`, candidate near-miss).
3. **Worker prompt contract** — row text + commit message; worker ends with
   `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` (+ `QUESTION:` when asking).
4. **Run/retry/ask state machine** — 2 runs/row budget, question pause,
   crash recovery via `supervisor-state.json`.
