# End-to-End Acceptance — pi-plan

Run the orchestrator against `test-fixtures/spike/` (3 rows) with the
**real** `pi --mode rpc` worker, real permission forwarding answered inline,
and real commits.

**Status:** ⏳ PENDING — first run after Steps 1–8 landed. Record each run in
the Results table at the bottom.

## Scope

This is the acceptance gate for the `pi-plan` orchestrator (Step 9 of
`docs/research/plan-rust-orchestrator.md`). It proves the whole wiring under
one roof:

- the real `pi-plan` binary (clap CLI: `supervise`/`status`/`stop`/`mark`/
  `step`)
- a fresh `pi --mode rpc` worker per row (Contract 3b argv: persona,
  determinism flags, session dir)
- inline `extension_ui_request` dialogs answered over the RPC extension-UI
  sub-protocol (decision D9 — the operator answers in pi-plan's own
  terminal, never in a hidden prompt)
- the ASK marker contract (`PI_WORKER_STATUS: ASK` + `QUESTION:` answered at
  an `answer>` prompt)
- git-keyed row completion, the 2-run retry budget, crash recovery

Unit coverage for every individual path lives in each module's tests plus
`tests/rpc_fake_pi.rs`; this procedure only exercises what unit tests
cannot: the real worker process, the real permission system, and real
commits.

## Prerequisites

- `cargo build` at the repo root; the binary is `target/debug/pi-plan`.
- Spike inner repo seeded (`cd test-fixtures/spike && git init && git add .
  && git commit -m "chore: seed spike fixture"`). Delete any leftover run
  state for a clean run (`supervisor-state.json`, `.pi-plan-stop`, `.pi-plan/`,
  `.pi/`).
- `pi` 0.85.1 on `PATH` (or point the worker at it); a configured model.
- The operator works at `test-fixtures/spike/` for every `pi-plan` command.

## Checklist

Each item is a pass/fail.

### A. Happy path through the plan

1. **[ ]** `pi-plan supervise` starts from `test-fixtures/spike/`; a live
   tail (stderr) streams row-1 worker events; a status line prints
   (`row 1 · agent … · turns … · ctx …`).
2. **[ ]** An inline dialog (stdout) appears for the gated op (row 1:
   `mkdir -p out`, then the `write` tool). Approve both inline.
   **Failure mode:** row 1 completes without any dialog — the gating
   assumption broke; STOP and investigate before continuing.
3. **[ ]** The worker commits `feat: add hello file`; `out/hello.txt` exists.
4. **[ ]** The loop advances: a spawn line prints for row 2
   (`docs: finish spike`) with a fresh agent id.
5. **[ ]** Approve any dialogs; row 2 commits `docs: finish spike`; the
   README gains the spike-run notes.
6. **[ ]** The loop advances to row 3 (`feat: add bye file`); approve the
   dialog; `out/bye.txt` appears; the commit lands.
7. **[ ]** The loop reports the plan complete and exits 0; the final report
   lists all three rows `done` with a `done: 3/3 rows` summary.

### B. Visibility

1. **[ ]** While a worker runs, `pi-plan status` (a second shell) prints the
   live row, agent id, git match state, persisted state, and worktree
   cleanliness.
2. **[ ]** Dialog output goes to stdout; traces, banners, and reports go to
   stderr (the operator can `2>trace.log` and still answer dialogs).
3. **[ ]** The final report includes the result tail and the transcript
   path for each attempt (under `.pi-plan/sessions/`).

### C. Retry, budget, report

1. **[ ]** Force a spent failure: with a worker mid-row, kill the worker
   process (SIGKILL) so the row's commit never lands. The loop records a
   spent run and retries with a **fresh** worker.
2. **[ ]** If a row exhausts its 2-run budget, the loop stops with a report:
   row id, runs used, last outcome, result tails, transcript paths.
3. **[ ]** A completed-without-commit worker (worker ends its turn without
   committing, no ASK) counts as a spent run and triggers the retry.

### D. Interrupts, questions, adjudication

1. **[ ]** `pi-plan stop` mid-run from a second shell: the loop aborts the
   worker at the next boundary and ends with a "stopped" outcome; no further
   worker spawns. A stale `.pi-plan-stop` from a killed run is discarded on
   the next `supervise` start.
2. **[ ]** `restart` as a line command at any prompt aborts the running
   worker and a fresh worker is spawned for the **same** row; the state file
   does not count the interrupted run as spent.
3. **[ ]** ASK question: when a worker ends with `PI_WORKER_STATUS: ASK` and
   a `QUESTION:` line, the loop prints an `answer>` prompt (stdout); the
   answer is folded into a **fresh** worker's prompt (confirm it appears in
   that worker's transcript). Answering after budget exhaustion still works.
4. **[ ]** `pi-plan step <n>` runs only row n; `pi-plan step <n> --answer
   "…"` pre-folds the answer into the first worker.
5. **[ ]** `pi-plan mark <n> done` after a near-miss (or without any commit)
   records the row as adjudicated; the next `supervise` skips it.

### E. Crash recovery

1. **[ ]** Mid-run (worker live), SIGKILL the `pi-plan` binary. Restart
   `pi-plan supervise`.
2. **[ ]** The loop resumes: the in-flight row restarts with `runsUsed`
   intact (a 2-run-per-row budget now spends its second run), or — if the
   row's commit landed before the kill — the loop skips it and continues.
3. **[ ]** Corrupt the state file (`echo '{' > supervisor-state.json`) and
   rerun `supervise` — the loop recomputes from git and does not throw.

## Pass criteria

- Every checklist item passes with the real worker, real forwarding, real
  commits.
- No worker process is left behind after the run (check with
  `ps aux | grep 'pi --mode rpc'`).
- `supervisor-state.json` never contradicts git history (verify with
  `git log --oneline` after each section).

## Results

| Date | Operator | Result | Notes |
| --- | --- | --- | --- |
| — | — | ⏳ PENDING — first run after Steps 1–8 landed | — |
