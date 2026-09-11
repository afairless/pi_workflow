# Architecture — pi-plan

## Overview

`pi-plan` is a deterministic orchestrator that supervises the TODO.md
workflow. For each open row it spawns a fresh `pi --mode rpc` worker
process, monitors the worker's JSONL event stream, answers permission
dialogs inline over the extension-UI sub-protocol, and decides completion
from git history (never from worker claims alone).

The orchestrator is plain code — no LLM sits in the control loop. A stop is
just a stop. The repository (`TODO.md`, `docs/research/*.md`, git history)
is the source of truth; `pi-plan` only reads it and writes one small state
file (`supervisor-state.json`) for crash recovery.

## Module map

```text
src/
  main.rs        binary entry (Step 1 placeholder; clap CLI lands in Step 8)
  config.rs      supervisor.config.json schema + precedence   ✔ Step 3
  todo.rs        TODO.md row contract parser                  ✔ Step 2
  git.rs         git facade + git-keyed completion matcher    ✔ Step 2
  prompt.rs      worker prompt builder (ASK contract)         ✔ Step 3
  state.rs       supervisor-state.json (crash recovery)       ✔ Step 3
  rpc.rs         pi RPC client: JSONL framing, commands,
                 events, extension-UI dialogs                 ✔ Step 4
  worker.rs      WorkerPort trait + RPC worker impl
                 + stall ceiling                              ✔ Step 5
  supervise.rs   run/retry/ask state machine +
                 scenario-aware dirty-WIP gate                ✔ Steps 6–7
  cli.rs         subcommand parsing (supervise/status/stop/
                 mark/step)                                   planned (Step 8)
  ui.rs          plain-terminal renderer (live tail, status
                 line, dialogs)                               planned (Step 8)
```

Implemented modules are testable in isolation: the supervise loop depends on
seams (`GitFacts`, `WorkerPort`, and recover/save/clear/report closures), so
its whole behavior matrix is unit-tested with a scripted fake worker and
staged fake git.

## Contracts

1. **TODO.md row contract** — `## Steps` table, `Source:` line,
   `## Prerequisites`, `## Done`. Rows carry no progress annotations; the
   parser only extracts structure (`src/todo.rs`).
2. **Completion, git-keyed** — a planned commit message is matched against
   `git log` subjects by similarity tiers over normalized strings:
   `exact` (equal) ≥ `similar` (ratio ≥ 0.9) ≥ `candidate`
   (prefix or ratio ≥ 0.8, near-miss). A row is done iff `exact` or
   `similar` matches; `candidate` stops for human adjudication
   (`pi-plan mark <n> done`). The worker never edits `TODO.md`.
3. **Worker prompt contract** — the prompt (`src/prompt.rs`) names the
   project, plan source, TODO.md path, the exact row text, and the planned
   commit message. The worker ends its final message with
   `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` (plus `QUESTION: <text>` when
   asking). The marker is a **hint** — completion classification stays
   git-keyed and questions are classified from the ASK marker + question
   line.
4. **Run/retry/ask state machine** — see below (`src/supervise.rs`).
5. **Crash recovery** — `supervisor-state.json` in the project root
   (bare filename, gitignored), written atomically (temp file + rename),
   best-effort (a failed write never crashes the loop).

## The supervise loop (Contract 4)

Per-row budget: **2 runs** (initial + one automatic retry). A question
pause, a stop, and a restart spend nothing; every other terminal event
spends one run.

```
spawn worker (fresh pi --mode rpc process)
  └─ saveState({currentRow, runsUsed, lastOutcome: "running",
                agentId, startedAt})            ← spawn-time save
await terminal (agent_settled, or a stall-ceiling abort
                enforced worker-side: turn_end count, wall clock)
classify, checked in this order:
  ├─ operator interrupts (stop / restart) — win over everything
  ├─ ASK?  last assistant text has PI_WORKER_STATUS: ASK
  │        → not spent; save "question"; print the question,
  │          wait for the answer → fresh worker with the answer folded
  ├─ git exact/similar → row done; clearState
  ├─ git candidate → near-miss; save; stop for `mark <n> done`
  ├─ dirty tree at the boundary, no row owner → refuse (DirtyWorktree);
  │        the refusal writes NO state (it cannot spend or inflate budget)
  └─ otherwise spent (failed / complete-without-commit /
           STUCK-without-question):
       save; retry fresh worker if runsUsed < 2, else stop + report
```

An operator-provided answer is a user-driven continuation and always gets
its run, even after the budget is exhausted. A restart keeps the row's
budget untouched (the fresh worker still runs under the same attempt).

## Dirty-WIP gate (Step 7): refuse strays, resume owned WIP

At the top of every attempt the loop reads `git status --short`:

- **Clean tree** — no gate.
- **Dirty tree, no owner** — no recovered state, or the recovered state
  does not name this row, or its `lastOutcome` is a legacy refusal marker
  (`dirty` / `spawn-error`). The loop refuses with `DirtyWorktree`,
  spawns nothing, and writes **no state**, so repeated refusals cannot
  inflate `runsUsed`.
- **Dirty tree, owned by this row** — the recovered state names this row
  with a live outcome (`running`, `question`, `failed`, ...). The loop
  resumes: the worker's prompt carries the `resumeDirtyWip` note
  ("fold the previous attempt's work into your commit; never commit
  unrelated strays"), and the spawn report line shows
  `· resuming dirty WIP`.

The state read at the gate is fresh per check — the loop's own terminal
saves and restarts update `supervisor-state.json` between attempts, so
ownership is never decided on a stale snapshot.

## supervisor-state.json (Contract 5)

```jsonc
{
  "planHash":    "<sha256 of TODO.md content>",
  "currentRow":  3,
  "runsUsed":    1,
  "lastOutcome": "failed",        // running|question|failed|no-commit|
                                  // near-miss|stopped|spawn-error|dirty|budget
  "adjudicated": [7],             // rows the human marked done (preserved
                                  // across every loop save)
  "agentId":     "0",             // optional; last live worker (spawn-time)
  "startedAt":   1750000000000    // optional; epoch ms of that spawn
}
```

Recovery rules (`src/state.rs`):

- No file / unparseable / wrong shape → recompute from git (never throws).
- `planHash` mismatch → recompute (the plan changed).
- The state's `currentRow` is already matched in git → recompute (a stale
  file must not contradict the repo).
- Otherwise resume with `runsUsed` intact.

Writes are atomic and best-effort: a kill mid-write cannot corrupt the
recovery input.

## Sourcing and data flow

```
repository (TODO.md · docs/research · git history)
   │  read: todo.rs → row contract · git.rs → completion facts
   ▼
pi-plan supervise loop (supervise.rs)
   │  spawn: worker.rs → rpc.rs → `pi --mode rpc` with the Contract 3
   │         prompt, persona preamble, pinned flags
   ▼
pi worker (fresh process + context, one TODO row only)
   │  events: message_update · tool_execution_* · turn_end ·
   │           agent_settled · extension_ui_request (permission asks)
   │  ├─ permission ask ──► (Step 8) rendered dialog + extension_ui_response
   │  └─ PI_WORKER_STATUS: ASK ──► question pause → answer folded in
   ▼
git commit → next row (or report + stop)
```

## Interrupts, operator commands (Step 8 preview)

The loop consumes `RunControl` flags (`restart_requested`,
`stop_requested`) at every boundary and terminal event. The Step 8 CLI
(`stop` / `restart` line commands, `pi-plan stop`) sets them; the CLI
surface for `supervise` / `status` / `stop` / `mark` / `step` lands with
`src/cli.rs`.

## Limitations / future work

- **No full-screen TUI** — plain terminal rendering lands in Step 8;
  ratatui is deferred.
- **Sequential rows only** — no parallel workers (matches the TS
  supervisor).
- **Shelling to `git`** — the facade (`src/git.rs`) mirrors the TS
  `spawnSync`; `git2`/`gix` is a deferred swap.
- **No auto mid-step respawn from context%** — compaction banner + manual
  `restart` is the v1 bridge.
- **`ui.rs` / `cli.rs` not yet implemented** — Steps 8–10 remain.
