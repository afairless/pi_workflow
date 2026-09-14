# Plan: worker spawn fixes — pi-lens determinism flags and spawn-error budget semantics

## Status

Fix plan for the `pi-plan` orchestrator, prompted by the `tag_tool` supervise
failure (2026-09-14):

1. **Worker spawn argv is incompatible with pi 0.85.1.** Since commit
   `e144eaa` the worker argv carries `--no-extensions -e <permission-system>`
   *next to* six pi-lens-owned determinism flags (`--no-lsp --no-lens
   --no-tests --no-autoformat --no-autofix --no-opengrep`). With extension
   discovery disabled those flags do not exist in pi's parser, so every
   spawn dies at parse time with `Error: Unknown options: …` and pi-plan
   reports a `spawn-error` / "peer closed the RPC stream" run.
2. **A spawn error is misclassified as budget exhaustion and consumes the
   row's run budget.** `run_row`'s spawn-error arm records an attempt,
   increments `runsUsed` in state, and immediately returns
   `RowOutcome::BudgetExhausted` — no automatic retry, a report line that
   reads "stopped — budget exhausted", and (across invocations) an
   accumulating `runsUsed` that can hard-gate the row with zero real agent
   runs ever having happened.

Decisions locked with the user on 2026-09-14 (see Locked decisions): fix both
issues; spawn errors spend **no** run budget, respawn a bounded number of
times with a 2 s fixed backoff (3 attempts total), and stop with a **distinct
"worker spawn failed" outcome**; the report tail surfaces the spawned pi's own
stderr on spawn errors; the design docs and README are updated. A startup
compat probe was considered and declined. Deriving `TODO.md` from this plan is
a follow-up step, not part of this document.

## The problem being solved (evidence)

### The user-visible failure

`pi-plan supervise` in `~/Documents/tag_tool` (row 8, after the user
committed step 7 manually):

```
── pi-plan report ──
  row 8: stopped — budget exhausted
    run 1: spawn-error
      the prompt command was rejected: peer closed the RPC stream
done: 0/21 rows
```

The report reads "budget exhausted" although the worker never started; the
user reasonably concluded the 2-run budget had carried over from step 7.
It did not — the budget is per-row and resets when the row advances
(`run_row` restores `runs_used` only when `persisted.current_row ==
row.number`, `src/supervise/mod.rs`). The state file proves the point:

```json
{ "planHash": "3b5764da…", "currentRow": 8, "runsUsed": 1,
  "lastOutcome": "spawn-error", "adjudicated": [], … }
```

Row 8 used 1 run, and that run was a transport failure, not agent work.

### Root cause 1 — the argv

`~/.pi-plan/tag_tool-45c3ea24/worker-stderr.log` (102 bytes, the spawned
worker's captured stderr):

```
Error: Unknown options: --no-lsp, --no-lens, --no-tests, --no-autoformat, --no-autofix, --no-opengrep
```

Reproduced byte-for-byte:

```
$ pi --mode rpc --no-extensions --no-lsp --no-lens --no-tests \
      --no-autoformat --no-autofix --no-opengrep </dev/null
Error: Unknown options: --no-lsp, --no-lens, --no-tests, --no-autoformat, --no-autofix, --no-opengrep
```

Source-level: the pi 0.85.1 core bundle (`dist/bundle/cli.js`) contains
**zero** references to `lens`, `opengrep`, `autoformat`, or `autofix`. All of
those toggles — and the CLI flags that set them — are registered by the
**pi-lens extension**
(`~/.pi/agent/npm/node_modules/pi-lens/dist/clients/lens-flag-registry.js`;
`pi-lens/docs/globalconfig.md` documents each flag↔config pair). `--no-extensions`
disables extension discovery, so pi-lens never loads, so its flags do not
exist in the parser. The six flags are therefore *both* fatal (parse error)
*and* redundant (their behaviors — LSP, lens, autoformat, autofix,
test-runner-on-write, the opengrep scanner — are all extension behavior and
are already absent).

The regression landed in `e144eaa` ("feat: spawn workers without
pi-guardrails (-ne + -e permission-system)"), which added the `--no-extensions
-e` pair to an argv that already contained the determinism flags (from
`c0ce0e6`). Before `e144eaa`, extensions loaded, pi-lens registered the flags,
and the argv parsed.

This is a pi-plan bug against its own pinned pi (`README`: requires
`pi ≥ 0.85.1`; the installed pi is 0.85.1).

### Root cause 2 — spawn-error handling

In `run_row` (`src/supervise/mod.rs`), the spawn `Err` arm:

- pushes a `RunRecord` with `attempt + 1`,
- persists state with `runs_used = attempt + 1`, `last_outcome = "spawn-error"`,
- returns `spent_outcome(…)` → `RowOutcome::BudgetExhausted` **immediately**.

Consequences:

- No automatic retry, even though the row's budget (2 runs) may have both
  runs unused. The automatic-retry path only exists for terminal spends
  after a successful spawn. A single transient RPC failure therefore ends
  the invocation at "run 1".
- Repetition accumulates `runsUsed`: the old binary writes `runsUsed = 1`
  per failed invocation, so two doomed invocations leave `runsUsed = 2` and a
  *third* invocation hard-blocks on the budget gate (`attempt >=
  BUDGET_PER_ROW`) **without attempting a spawn at all** — the "still
  budget exhausted" dead-end.
- The report cannot distinguish "the agent genuinely used its 2 runs" from
  "the environment refused to launch a worker", which is the exact
  confusion this plan fixes.

## Verified technical facts (pi 0.85.1, this repo, installed extensions)

| Fact | Where verified |
|---|---|
| The six determinism flags are registered by the pi-lens extension, not core pi; the core bundle references none of lens/opengrep/autoformat/autofix. | `grep` over `dist/bundle/cli.js` (0 hits); `pi-lens/dist/clients/lens-flag-registry.js`; `pi --help` "Extension CLI Flags" section. |
| `--no-extensions` disables extension discovery; explicit `-e` paths still load. With it, pi 0.85.1 rejects the six flags with `Unknown options` and exits before the RPC stream settles. | `pi --help`; reproduction command above. |
| The determinism behaviors (LSP, lens, autoformat, autofix, test-runner-on-write, opengrep, knip, …) are all pi-lens features. They cannot run when pi-lens does not load, so removing the flags loses no guarantee. | `pi-lens/dist/**`; `pi-lens/docs/globalconfig.md` table. |
| A corrected argv — the current one minus the six flags — parses and starts correctly: pi loads the permission extension and the skill and proceeds to model resolution. | Probe with the real paths: `pi --mode rpc … --no-extensions -e ~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system …` reaches `Error: Model "…" not found` (no `Unknown options`). |
| `worker-stderr.log` is opened append-only by the RPC client (`rpc.rs`, `OpenOptions::append(true)`), so the failing spawn's stderr is the log tail at failure time. | `src/rpc.rs` spawn stderr wiring. |
| `tokio::time::sleep` is available for backoff. | Used in `src/worker.rs` stats/retry loops. |
| The spawn `Err` arm is the single point that decides spawn-failure behavior for row workers; the clean-worktree pass has its own smaller spawn-error handling (`run_row_clean_pass` → `CleanVerdict` abort). | `src/supervise/mod.rs`. |
| Supervise exit codes: `2 = supervise ended with work outstanding (stopped / question / near-miss / budget)`. A spawn-failure stop is "work outstanding"; keep exit 2. | `src/main.rs` module doc. |

## Design

### 1. Worker argv: drop the pi-lens determinism flags

Remove `DETERMINISM_FLAGS` and the loop that appends them in
`src/worker.rs` (`build_worker_args`), and remove the flags from the
argv-shape doc comment above it. The new pinned worker argv (Contract 3b):

```text
pi --mode rpc \
   --session-dir <runDir>/sessions \
   --name pi-plan-row-<n> \
   --model <model> \
   --thinking high \
   --approve \
   --tools read,grep,find,ls,bash,edit,write \
   --skill <implement-from-plan dir> \
   --no-extensions -e <permission-system dir> \
   --append-system-prompt <persona>
```

`read`/`write` and the whole tool surface stay gated by the `--tools`
allowlist; everything the old flags disabled is disabled already by
extension absence.

**New invariant (documented, not just encoded):** with `--no-extensions`,
a worker loads exactly one extension — the permission system — so
pi-lens-hosted behaviors (unified LSP, lens, autoformat at `agent_end`,
autofix, the write-time test runner, the opengrep auxiliary scanner, the
knip/madge/jscpd family) never exist in a worker regardless of flags. The
determinism guarantee comes from the extension set, not from flags. This
applies to the row workers and the clean-worktree agents alike (shared
builder).

If a future pi release moves any of these behaviors into core (or a future
contract loads pi-lens in workers deliberately), the flags can return
*alongside* a `-e <pi-lens>` and only when extensions are loadable —
calling this out explicitly prevents a silent re-introduction of the
same incompatibility.

### 2. Spawn errors spend no run budget, respawn with backoff

New constants in `src/supervise/mod.rs`:

```rust
/// Spawn attempts per row attempt (initial + 2 respawns), bounded so a
/// deterministic environment failure cannot hang the loop.
pub const SPAWN_RETRY_LIMIT: u32 = 3;
/// Fixed backoff between spawn attempts.
pub const SPAWN_RETRY_BACKOFF: Duration = Duration::from_secs(2);
```

The spawn call in `run_row` and in `run_row_clean_pass` is replaced by one
shared helper:

```rust
/// Attempts the spawn up to `SPAWN_RETRY_LIMIT` times, sleeping
/// `SPAWN_RETRY_BACKOFF` between tries. Returns the worker id or the
/// collected errors (one per failed attempt) so callers can keep one
/// respawn record per attempt and decide their own stop semantics.
async fn spawn_worker_retrying<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    prompt: &str,
    opts: &WorkerSpawnOpts,
) -> Result<WorkerId, Vec<WorkerError>>
```

`run_row`'s `Err` arm:

1. Push one `RunRecord` per collected error (`RunOutcomeKind::SpawnError`,
   `attempt + 1`, error tail) — records are report history, not budget.
   **Intermediate failures write no state**: the last truthful persisted
   state stands, so a crash during the ≤ 2 s backoff can never lose or
   inflate budget information.
2. Stop with a **distinct** outcome (`RowOutcome::SpawnError`, see Design 3);
   the state save uses `runs_used = attempt` **unchanged** with
   `last_outcome = "spawn-error"`. No budgeted run was consumed; a
   re-invocation starts from the same `runs_used` it had before the spawn
   trouble began and the resume discount below restores the full budget.

`run_row_clean_pass` uses the same helper; on persistent failure it keeps
today's abort semantics (`CleanVerdict::outcome(DirtyWorktree)`) with the
respawn records included. The budget gate, boundary interrupts, and the
dirty gate are re-checked on the next outer loop iteration as today; the
helper's in-loop sleep means a stop/restart lands within ≤ 2 s at most.

**Resume discount (also self-heals legacy state):** the restore at the top
of `run_row` becomes

```rust
runs_used = if p.current_row == row.number && p.last_outcome != "spawn-error"
    { p.runs_used.min(BUDGET_PER_ROW) } else { 0 };
```

A `last_outcome == "spawn-error"` state means no real run completed, so the
row resumes with its full budget. This is *exactly* right for every legacy
state (the old binary only ever wrote `spawn-error` states whose `runsUsed`
came entirely from failed spawns) and at worst over-grants a single run in
the post-fix edge case where a real spent failure is followed by exhausted
spawn retries — benign for a supervisor budget, never under-grants.

### 3. Distinct "worker spawn failed" outcome

Add `RowOutcome::SpawnError { row, records }` and wire it through the four
exhaustive matches:

| Surface | Today | New |
|---|---|---|
| `describe_outcome` (`supervise/mod.rs`) banner | budget-exhausted text | `"stopped — worker spawn failed (run budget untouched)"` |
| `outcome_label` (`cli.rs`) report line | `"stopped — budget exhausted"` | `"stopped — worker spawn failed"` |
| `outcome_row_number` / `outcome_records` (`cli.rs`) | — | add the variant (compiler-enforced) |

Exit code stays 2 (work outstanding; the row is not done). The report keeps
showing `run N: spawn-error` records as today — history is preserved even
though the budget is not spent. Up to `SPAWN_RETRY_LIMIT` such lines may
share the same `run N` (the budget attempt they belong to): a repeated
`run N: spawn-error` line is one of the 2 s-backoff respawns, **not** a
spent run — the report renders one line per actual spawn attempt.

### 4. Surface the worker's own stderr in the report

Thread the worker stderr log path into the spawn path so the error tail can
carry the pi process's last words:

- Add `stderr_path: Option<PathBuf>` to `WorkerSpawnOpts` and set it from
  `spawn_opts_for_row` via a new `SuperviseServices.stderr_path` field
  (wired in `cmd_supervise`, which already computes the log path —
  `root.join("worker-stderr.log")`). The clean pass's own opts get the
  same field, so its spawn-error records surface the tail too.
- In the spawn `Err` arm, after the failure, read the last lines of the log
  through the same bounding as run tails (`result_tail`, 6 lines / 400
  chars) and append to the record tail — **gated on the error class**.
  `WorkerError::Prompt`-class failures (the child started, parsed, and
  wrote its own stderr: `Unknown options`, credential errors, RPC stream
  death) get the tail; `WorkerError::Spawn`-class failures (binary
  missing, OS-level exec failure — the child never wrote a byte) skip it,
  because the append-only log tail would be a *previous* attempt's stderr
  and would actively mislead. The helper returns `WorkerError` values, so
  the `Err` arm discriminates on the last collected error's variant:

  ```
  the prompt command was rejected: peer closed the RPC stream
  worker stderr (last lines):
    Error: Unknown options: --no-lsp, …
  ```

  This turns the exact failure the user hit into an in-report diagnosis
  with no extra investigation.

### 5. Documentation

- `docs/research/plan-rust-orchestrator.md` §Contract 3b: replace the argv
  block and the "pi-lens determinism flags are deliberate" paragraph with
  the bare-worker invariant and the compatibility note; §Contract 4: add the
  spawn-error rows (spend nothing, 3 × 2 s respawn, distinct stop, resume
  discount).
- `docs/ARCHITECTURE.md`: §3 worker prompt — drop the six flags from the
  pinned argv and add the bare-worker invariant sentence; "The supervise
  loop (Contract 4)" — extend the spend list (a spawn error spends nothing
  and respawns 3 × 2 s; persistent failure stops with `SpawnError`) and add
  the respawn arm to the loop diagram; "Dirty-WIP gate" — `spawn-error` is
  now a budget-discount-and-non-owner marker on resume, not merely a legacy
  refusal marker; recovery prose — the resume read gives the full budget
  when `lastOutcome == "spawn-error"`.
- `README.md`: the **Worker contract** argv block drops the six flags and
  gains the bare-worker invariant sentence; its "Per-row budget" paragraph
  gains "a spawn error spends nothing (respawns 3 × 2 s, then stops with a
  distinct outcome)"; Troubleshooting gains a row for `spawn-error` /
  `peer closed the RPC stream` / `Unknown options` on the spawned pi,
  pointing at `~/.pi-plan/<key>/worker-stderr.log` and at the worker
  extension set invariant.
- The `worker.rs` argv comment and the `SuperviseServices.permission_extension`
  doc get the invariant sentence.

## Code map (integration points)

| File | Change |
|---|---|
| `src/worker.rs` | Remove `DETERMINISM_FLAGS` + append loop; rewrite argv doc comment; add `stderr_path` to `WorkerSpawnOpts`; update argv unit tests (`--no-lsp present` → absent; `-ne`/`-e` pair and ordering assertions kept/adapted). |
| `src/supervise/mod.rs` | `SPAWN_RETRY_LIMIT`/`SPAWN_RETRY_BACKOFF`; shared `spawn_worker_retrying` (row + clean pass); intermediate failures write no state; spawn-error stop saves with unchanged `runs_used`; resume discount; `RowOutcome::SpawnError` + `describe_outcome` arm; `stderr_path` on services + `spawn_opts_for_row` (+ clean-pass opts). |
| `src/supervise/tests.rs` | Flip `run_row_spawn_error_stops_the_row_immediately` to the new contract; new tests: respawn retries then stops, budget untouched on stop, intermediate failures write no state, resume discount (+ over-grant re-invocation edge), distinct outcome label, stderr tail in record (present for `Prompt`-class, absent for `Spawn`-class), clean-pass retry path. |
| `src/cli.rs` | `outcome_label`, `outcome_row_number`, `outcome_records` matches + report-format tests. |
| `src/main.rs` | Wire `stderr_path` into services/opts; exit code unchanged. |
| `docs/research/plan-rust-orchestrator.md`, `docs/ARCHITECTURE.md`, `README.md` | Design 5 updates. |

## Step table (commit-by-commit)

Quality gates after every commit: `cargo test` → `cargo fmt --check` →
`cargo clippy --all-targets --all-features -- -D warnings` (repo AGENTS.md);
each commit leaves the suite green.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|
| 1 | `fix: drop pi-lens determinism flags from the worker argv` | argv fix | `worker.rs`: remove `DETERMINISM_FLAGS` + append loop, rewrite Contract 3b doc comment (bare-worker invariant) | Unit: argv shape — six flags absent, `-ne -e` pair present; report tests untouched |
| 2 | `feat: respawn worker spawns without spending the row budget` | spawn-error semantics + distinct outcome | `supervise/mod.rs`: `SPAWN_RETRY_LIMIT`/`_BACKOFF`, shared `spawn_worker_retrying` (internal retry loop, returns collected `WorkerError`s) used by `run_row` + clean pass, intermediate failures write no state, spawn-error stop keeps `runs_used` + writes `last_outcome = "spawn-error"`, resume discount; `RowOutcome::SpawnError` + `describe_outcome`/`outcome_label`/`outcome_row_number`/`outcome_records` arms; banner + report line | Unit (fake port): respawn-then-stop (≤ 3 attempts), budget unchanged, intermediate failures write no state, resume discount, over-grant re-invocation edge (real fail + 3× spawn fail → re-invocation discounts to 0), clean-pass retry, distinct label + report rendering with `run N: spawn-error` records |
| 3 | `feat: surface worker stderr in spawn-error report tails` | diagnostics | `WorkerSpawnOpts.stderr_path`, `SuperviseServices.stderr_path`, read-last-lines appended to spawn-error record tail for `WorkerError::Prompt`-class failures only (clean-pass opts included) | Unit: tail bounds + gated presence/absence with stub logs |
| 4 | `docs: document the bare-worker determinism invariant and spawn-error contract` | docs | Contract 3b/4 rewrites in `plan-rust-orchestrator.md`; ARCHITECTURE.md argv/budget/gate/resume prose; README Worker contract argv + budget paragraph + Troubleshooting row; comment touch-ups | `cargo fmt --check` (doc-only otherwise) |

Steps 1–3 are each independently mergable and green; step 1 alone fixes the
user's blocker, steps 2–3 harden the semantics and the report. (Step 2 lands
the outcome variant with its semantics in one commit, so no intermediate
commit ever reports a spawn failure as `budget exhausted`.)

## Locked decisions

- Scope: both issues + diagnostics; no startup compat probe (declined).
- A spawn failure is not an agent run: it spends **no** run-budget, the row
  respawns automatically (3 attempts total, 2 s fixed backoff), and a
  persistent failure stops with a distinct "worker spawn failed" outcome.
- Resuming a row whose last outcome is `spawn-error` restores the full
  budget (`runs_used = 0`); this exactly matches all legacy states written
  by the old binary and self-heals the user's current
  `tag_tool` state file.
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

## Risks / pitfalls

- **A future pi loading pi-lens despite `--no-extensions`** would re-arm
  autoformat/opengrep silently — the flags are gone. Mitigation: the
  Contract 3b doc pins the invariant and the "flags return only with a
  deliberate `-e <pi-lens>`" note; acceptance re-probes the argv against
  the installed pi.
- **stderr-tail attribution:** the shared `worker-stderr.log` is append-only
  and the child writes to it directly, so a `Prompt`-class death (child
  started, parsed, wrote its error) is attributable and gets the gated
  tail. A `Spawn`-class exec failure writes nothing — the plan appends
  **no** tail there rather than risk a previous attempt's lines. Reads stay
  bounded (file may be large; start from the tail, cap bytes) — note both
  in the implementation.
- **Backoff vs. Ctrl-C:** at most a 2 s stop latency; the loop re-polls the
  control flags immediately after each sleep.
- **Test churn:** `run_row_spawn_error_stops_the_row_immediately`,
  `clean_spawn_error_fails_and_aborts`, the argv-shape tests, and the
  `SuperviseServices`/`WorkerSpawnOpts` literals in tests all change in
  steps that touch them — keep each commit's churn contained to its unit.
- **Over-grant edge case** (real spent failure then exhausted spawn retries
  then a re-invocation): the resume discount gives the row one extra run.
  Benign by design; documented in Design 2.

## Acceptance

1. **Unit:** argv tests assert the six flags are gone and `-ne`/`-e` remain;
   spawn-error tests assert: ≤ 3 spawn attempts per row attempt, state
   `runs_used` unchanged on spawn-error stop, intermediate failures write
   no state, resume discount (incl. the real-fail + spawn-exhaustion
   over-grant edge), distinct label, stderr tail present and bounded for
   `Prompt`-class failures and **absent** for `Spawn`-class failures.
2. **Probe against the real pi (this machine):** the production argv with
   the six lens flags **removed** — `pi --mode rpc --session-dir …
   --no-extensions -e ~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system
   --skill ~/.pi/agent/skills/implement-from-plan --append-system-prompt …`
   — yields no `Unknown options` and reaches model resolution (already
   verified during planning). Negative control: adding back **any** of the
   six flags onto the same `--no-extensions` argv must fail with
   `Unknown option: --no-…`, confirming the flags stay gone.
3. **tag_tool recovery:** after merge, `pi-plan supervise` in
   `~/Documents/tag_tool` resumes row 8 with the full 2-run budget (the
   stale `runsUsed: 1, lastOutcome: "spawn-error"` state discounts to 0)
   and the worker actually spawns.
4. **Regression:** a fully spent row (2 real terminal failures) still stops
   with `stopped — budget exhausted`; `done: x/y` and per-run records render
   as before for non-spawn outcomes.
