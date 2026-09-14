# Implementation Plan: Worker spawn fixes (determinism flags, spawn-error budget semantics)

Source: `docs/research/plan-worker-spawn-fixes.md`

The `pi-plan` worker argv currently carries six pi-lens-owned determinism
flags (`--no-lsp --no-lens --no-tests --no-autoformat --no-autofix
--no-opengrep`) next to `--no-extensions -e <permission-system>`. With
extension discovery disabled those flags do not exist in pi 0.85.1's
parser, so every worker spawn dies with `Error: Unknown options: …` and
the row reports `spawn-error` → "stopped — budget exhausted" although no
agent run ever happened (`tag_tool` row 8 failure, 2026-09-14).

This plan (a) drops the six flags, (b) makes spawn errors spend **no**
run budget with a bounded respawn (3 attempts total, 2 s fixed backoff)
and a distinct `RowOutcome::SpawnError` stop, (c) surfaces the spawned
pi's own stderr in the report tail, and (d) updates the docs. Four
commits.

The commit messages in the table below are **exact** — taken verbatim from
the source plan. Workflow per step: implement → `cargo test` → `cargo fmt
--check` → `cargo clippy --all-targets --all-features -- -D warnings` →
commit with the table's message → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `fix: drop pi-lens determinism flags from the worker argv` | argv fix | `src/worker.rs`: remove `DETERMINISM_FLAGS` + append loop, rewrite Contract 3b doc comment (bare-worker invariant) | Unit: argv shape — six flags absent, `-ne -e` pair present; report tests untouched |
| 2 | `feat: respawn worker spawns without spending the row budget` | spawn-error semantics + distinct outcome | `src/supervise/mod.rs`: `SPAWN_RETRY_LIMIT`/`_BACKOFF`, shared `spawn_worker_retrying` (internal retry loop, returns collected `WorkerError`s) used by `run_row` + clean pass, intermediate failures write no state, spawn-error stop keeps `runs_used` + writes `last_outcome = "spawn-error"`, resume discount; `RowOutcome::SpawnError` + `describe_outcome`/`outcome_label`/`outcome_row_number`/`outcome_records` arms; banner + report line | Unit (fake port): respawn-then-stop (≤ 3 attempts), budget unchanged, intermediate failures write no state, resume discount, over-grant re-invocation edge (real fail + 3× spawn fail → re-invocation discounts to 0), clean-pass retry, distinct label + report rendering with `run N: spawn-error` records |
| 3 | `feat: surface worker stderr in spawn-error report tails` | diagnostics | `WorkerSpawnOpts.stderr_path`, `SuperviseServices.stderr_path`, read-last-lines appended to spawn-error record tail for `WorkerError::Prompt`-class failures only (clean-pass opts included) | Unit: tail bounds + gated presence/absence with stub logs |
| 4 | `docs: document the bare-worker determinism invariant and spawn-error contract` | docs | Contract 3b/4 rewrites in `docs/research/plan-rust-orchestrator.md`; `docs/ARCHITECTURE.md` argv/budget/gate/resume prose; `README.md` Worker contract argv + budget paragraph + Troubleshooting row; comment touch-ups | `cargo fmt --check` (doc-only otherwise) |

## Locked decisions (from the source plan)

- Scope: both issues + diagnostics; no startup compat probe (declined).
- A spawn failure is not an agent run: it spends **no** run-budget; the row
  respawns automatically (3 attempts total, 2 s fixed backoff); a
  persistent failure stops with a distinct "worker spawn failed" outcome.
- Resuming a row whose last outcome is `spawn-error` restores the full
  budget (`runs_used = 0`); this exactly matches all legacy states written
  by the old binary and self-heals the `tag_tool` state file.
- Report tails include the spawned pi's stderr (bounded: 6 lines / 400
  chars) on spawn errors whose class proves the child wrote it
  (`WorkerError::Prompt`-class only; `Spawn`-class exec failures append no
  tail). Intermediate spawn failures persist no state — only the final
  stop writes `last_outcome = "spawn-error"`. Exit code 2 for the new
  outcome.
- Determinism is provided by the worker's extension set (`--no-extensions`
  - permission-system `-e` + `--tools` allowlist), never by pi-lens flags
  that cannot exist in a bare worker.

## Invariants to preserve

- Per-row budget stays 2 real runs (initial + one automatic retry); a
  user-provided answer always gets its run (Contract 4).
- `restart` spends nothing; stop/restart interrupts win over every
  classification and are re-checked between attempts (backoff sleep ≤ 2 s
  delay at most).
- `supervisor-state.json` shape is unchanged
  (`{ planHash, currentRow, runsUsed, lastOutcome, adjudicated, agentId?,
  startedAt? }`); no migration needed — the resume discount reads existing
  fields. Persisted state always reflects the last **real** terminal event:
  intermediate spawn failures never hit the disk, so a crash mid-backoff
  cannot lose or inflate budget information.
- Clean-pass semantics: successful clean → row proceeds; persistent clean
  spawn failure → same `DirtyWorktree` abort as today, no state written.
- Report `done: x/y rows` counts completed rows per invocation, as today.
- No `unsafe`, no `unwrap()`/`expect()`/`panic!()` in application logic;
  thiserror errors; unit tests next to code (repo AGENTS.md).

## Step notes

### Step 1 (argv fix)

- `src/worker.rs`: delete the `DETERMINISM_FLAGS` const (`:36`) and the
  append loop inside `build_worker_args` (`:224`); rewrite the Contract 3b
  argv doc comment (`:189`) to the pinned bare-worker argv (Design 1 of the
  plan) and to state the **invariant**: with `--no-extensions` a worker
  loads exactly one extension — the permission system — so pi-lens-hosted
  behaviors (unified LSP, lens, autoformat at `agent_end`, autofix, the
  write-time test runner, the opengrep auxiliary scanner, the
  knip/madge/jscpd family) never exist in a worker regardless of flags. The
  determinism guarantee comes from the extension set, not from flags; if a
  future pi release moves any of these behaviors into core (or a contract
  deliberately loads pi-lens), the flags may return **alongside** a
  `-e <pi-lens>` and only when extensions are loadable.
- The `-ne -e` pair and every other argv element stay byte-identical; only
  the six flags disappear. `--tools` keep-gating the whole tool surface is
  untouched.
- Tests: update `build_worker_args_matches_contract_3b_shape`
  (`src/worker.rs:873`) and the per-flag presence loop (`:909`) to assert
  the six flags are gone; keep/adapt `build_worker_args_emits_bare_extension_flags`
  (`:924`). Report-format tests are untouched (this step changes no
  outcome).

### Step 2 (spawn-error semantics + distinct outcome)

- `src/supervise/mod.rs`: new constants `SPAWN_RETRY_LIMIT: u32 = 3` and
  `SPAWN_RETRY_BACKOFF: Duration = Duration::from_secs(2)` (doc: bounded so
  a deterministic environment failure cannot hang the loop).
- New shared helper (module-private, near `run_row`):

  ```rust
  async fn spawn_worker_retrying<'a, G: GitFacts, W: WorkerPort>(
      services: &SuperviseServices<'a, G, W>,
      prompt: &str,
      opts: &WorkerSpawnOpts,
  ) -> Result<WorkerId, Vec<WorkerError>>
  ```

  Used by both `run_row` (`:1089`) and `run_row_clean_pass` (`:609`).
  Backoff sleeps keep the stop/restart control flags re-polled right after
  each sleep (≤ 2 s stop latency).
- `run_row` spawn `Err` arm becomes:
  1. Push one `RunRecord` per collected error (`RunOutcomeKind::SpawnError`,
     `attempt + 1`, error tail) into the local outcome — records are report
     history, not budget. **No state write on intermediate failures**: the
     last truthful persisted state stands (a crash during the ≤ 2 s backoff
     can never lose or inflate budget info).
  2. Stop with `RowOutcome::SpawnError { row, records }`; the state save
     uses `runs_used` **unchanged** with `last_outcome = "spawn-error"`. Do
     **not** route through `spent_outcome` (`:546`), which increments and
     returns `BudgetExhausted` — that is the bug being fixed.
- Resume discount at the top-of-`run_row` restore:

  ```rust
  runs_used = if p.current_row == row.number && p.last_outcome != "spawn-error"
      { p.runs_used.min(BUDGET_PER_ROW) } else { 0 };
  ```

  Exactly right for every legacy state (old binary only ever wrote
  `spawn-error` states whose `runsUsed` came entirely from failed spawns);
  at worst over-grants one run in the post-fix edge (real spent failure,
  then exhausted spawn retries, then re-invocation) — benign, never
  under-grants.
- `RowOutcome::SpawnError { row, records }` + the four exhaustive matches:
  `describe_outcome` (`:367`) → `"stopped — worker spawn failed (run budget
  untouched)"`; `outcome_label` (`src/cli.rs:494`) → `"stopped — worker
  spawn failed"`; `outcome_row_number` (`src/cli.rs:518`) and
  `outcome_records` (`src/cli.rs:532`) arms added (compiler-enforced on the
  enum). Exit code stays 2 (work outstanding — the row is not done; see
  `src/main.rs` module doc). Up to `SPAWN_RETRY_LIMIT` of the same
  `run N: spawn-error` lines may render — one line per actual spawn
  attempt, not per spent run.
- Clean pass: same helper; persistent failure keeps today's abort
  (`CleanVerdict::outcome(RowOutcome::DirtyWorktree { .. })`, `:630`) with
  the respawn records carried; no state written.
- Tests (`src/supervise/tests.rs`): flip
  `run_row_spawn_error_stops_the_row_immediately` (`:574`) to the new
  contract; new tests — respawn retries then stops (≤ 3 attempts), budget
  untouched on stop, intermediate failures write no state, resume discount
  (+ over-grant re-invocation edge), distinct outcome label, clean-pass
  retry (`clean_spawn_error_fails_and_aborts`, `:1756`), report rendering
  with repeated `run N: spawn-error` records. Report-format tests in
  `src/cli.rs` for the new label.

### Step 3 (stderr diagnostics)

- `src/worker.rs`: add `stderr_path: Option<PathBuf>` to `WorkerSpawnOpts`
  (doc: append-only shared log; only meaningful for spawn errors).
- `src/supervise/mod.rs`: add `stderr_path` to `SuperviseServices` and set
  it on `spawn_opts_for_row` and the clean-pass opts; wired from
  `cmd_supervise` (`src/main.rs`), which already computes
  `root.join("worker-stderr.log")` (the RPC client opens it append-only —
  `src/rpc.rs`, `OpenOptions::append(true)` — so the failing spawn's stderr
  is the log tail at failure time).
- In the spawn-`Err` arm (both row and clean pass): after the retry loop
  exhausts, read the last lines of the log through the same bounding as run
  tails (`result_tail`: 6 lines / 400 chars, read from the tail, cap bytes)
  and append to the record tail — **gated on the last collected error's
  variant**: `WorkerError::Prompt`-class (child started, parsed, wrote its
  own stderr: `Unknown options`, credential errors, RPC stream death) gets
  the tail; `WorkerError::Spawn`-class (binary missing, OS-level exec
  failure — child never wrote a byte) skips it, because the append-only log
  tail would be a *previous* attempt's stderr and mislead.
- Expected rendering in the report:

  ```text
  the prompt command was rejected: peer closed the RPC stream
  worker stderr (last lines):
    Error: Unknown options: --no-lsp, …
  ```

- Tests: tail bounds; tail present for a `Prompt`-class failure with a stub
  log; absent for `Spawn`-class; clean-pass opts include the path.

### Step 4 (docs)

- `docs/research/plan-rust-orchestrator.md`: §Contract 3b — replace the
  argv block and the "pi-lens determinism flags are deliberate" paragraph
  with the bare-worker invariant + compatibility note (step 1's comment
  text is the source of truth); §Contract 4 — add the spawn-error rows
  (spend nothing, 3 × 2 s respawn, distinct stop, resume discount).
- `docs/ARCHITECTURE.md`: §3 worker prompt — drop the six flags from the
  pinned argv, add the invariant sentence; "The supervise loop (Contract
  4)" — extend the spend list and add the respawn arm to the loop diagram;
  "Dirty-WIP gate" — `spawn-error` is now a budget-discount-and-non-owner
  marker on resume, not merely a legacy refusal marker; recovery prose —
  the resume read gives the full budget when
  `lastOutcome == "spawn-error"`.
- `README.md`: **Worker contract** argv block drops the six flags + gains
  the invariant sentence; "Per-row budget" gains "a spawn error spends
  nothing (respawns 3 × 2 s, then stops with a distinct outcome)";
  Troubleshooting gains a row for `spawn-error` / `peer closed the RPC
  stream` / `Unknown options` on the spawned pi, pointing at
  `~/.pi-plan/<key>/worker-stderr.log` and the worker extension-set
  invariant.
- Comment touch-up outside step 1's argv comment: the
  `SuperviseServices.permission_extension` doc gets the invariant sentence.
  No doc-comment work duplicated with step 1.

## Acceptance criteria (end state)

- `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings` all green after each commit, final commit
  included.
- **Unit:** argv tests assert the six flags are gone and `-ne -e` remain;
  spawn-error tests assert: ≤ 3 spawn attempts per row attempt, state
  `runs_used` unchanged on spawn-error stop, intermediate failures write no
  state, resume discount (incl. the real-fail + spawn-exhaustion over-grant
  edge), distinct label, stderr tail present and bounded for
  `Prompt`-class failures and **absent** for `Spawn`-class failures.
- **Probe against the real pi (this machine):** the production argv with
  the six lens flags **removed** — `pi --mode rpc --session-dir …
  --no-extensions -e ~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system
  --skill ~/.pi/agent/skills/implement-from-plan --append-system-prompt …`
  — yields no `Unknown options` and reaches model resolution. Negative
  control: adding back **any** of the six flags onto the same
  `--no-extensions` argv must fail with `Unknown option: --no-…`,
  confirming the flags stay gone.
- **tag_tool recovery:** after merge, `pi-plan supervise` in
  `~/Documents/tag_tool` resumes row 8 with the full 2-run budget (the
  stale `runsUsed: 1, lastOutcome: "spawn-error"` state discounts to 0)
  and the worker actually spawns.
- **Regression:** a fully spent row (2 real terminal failures) still stops
  with `stopped — budget exhausted`; `done: x/y` and per-run records render
  as before for non-spawn outcomes.
