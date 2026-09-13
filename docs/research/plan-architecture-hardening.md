# Plan: architecture hardening — follow-ups from the 2026-09-13 review

## Status

Follow-up proposal for the `pi-plan` orchestrator, derived from a full
architecture review of the codebase (2026-09-13, HEAD `b45ebcf`). The review
found the architecture sound overall — clean acyclic module DAG, testable
seams (`WorkerPort`, `GitFacts`, closure-injected `SuperviseServices`), strong
test discipline (341 tests, property tests on the match-tier partition), zero
`unsafe`/`unwrap`/`expect`/`panic!` in production — and surfaced a small set
of concrete corrections. This plan implements the corrections that are worth
acting on, in small green commits, and ends with a documentation-fidelity pass.

Pre-implementation: regenerate `TODO.md` from this document (the
`write-todo-from-plan` workflow) so the commit-by-commit table below is the
actionable plan; the messages in the table are the exact commit messages.

## Scope and provenance

Implemented (10 commits):

1. Remove the dead `anyhow` dependency (pinned in `Cargo.toml:23`, referenced
   nowhere in `src/` or `tests/`).
2. Repair the `storage → supervise` layering inversion (`src/storage.rs:19`
   imports `RunRecord`/`run_outcome_label` from the state machine).
3. Move `PendingTool` out of `worker.rs` into `rpc.rs` so `ui.rs`/`tui.rs`
   stop depending on worker infrastructure for a rendering type.
4. Split the oversized `supervise.rs` (4,609 lines, 72% tests) and `tui.rs`
   (4,240 lines, 53% tests) by relocating their `#[cfg(test)] mod tests`
   bodies to sibling files.
5. De-duplicate and make testable the binary's interactive loop: extract the
   duplicated row-vs-clean question-pause handler, then hoist the driver into
   the library behind a `QuestionPause` seam with unit tests (the loop is
   currently `main.rs` glue, 92% production, 3 unit tests — the least-tested
   region of the codebase).
6. Fail `mark <n> bogus` usage errors at clap parse time instead of runtime.
7. Sync `docs/ARCHITECTURE.md`, `README.md`, and verify
   `docs/acceptance-e2e.md` for stale path references (none today) with the
   refactors.
8. Align `AGENTS.md` error-handling guidance with the crate's actual state
   (its "anyhow for the binary" rule becomes false once `anyhow` is removed).

Considered and rejected — **not** in the commit table (see
"Considered and not planned"): `next_row` precomputation, threading
`plan_hash` through saves, switching `worker-stats.jsonl` to true append,
unifying clap `Supervise`/`Step`, splitting `tui.rs` production code further,
and fixing the pre-existing restart-at-ASK quirk.

## The problem being solved (evidence)

- **Dead dependency.** `Cargo.toml:23` pins `anyhow = "=1.0.104"`; `grep -rn
  "anyhow" src/ tests/` returns nothing. The binary reports errors as
  `Result<u8, String>` (`main.rs`). The pin violates the repo's own
  pinned-dependency hygiene (every `Cargo.toml` entry should be in use).
- **Layering inversion.** `src/storage.rs:19`:
  `use crate::supervise::{RunRecord, run_outcome_label};`. `storage` is the
  low-level external-root/persistence module (sibling of the leaf `state.rs`);
  `supervise` is the highest-level module. It compiles only because
  `supervise` never imports `storage`; the mapping function
  `worker_stats_from_run(record: &RunRecord) -> Option<WorkerStatsRecord>`
  belongs with the run-record owner, not the file layer.
- **Wrong home for a rendering type.** `PendingTool` (decoded from the RPC
  `tool_execution_start` `args`) is defined in `worker.rs` but consumed by the
  renderers: `ui.rs:20` and `tui.rs:76` import it from `crate::worker`. The
  type is event-derived data and belongs beside `ToolExecutionStart` in
  `rpc.rs`; after the move `ui.rs` imports only `{rpc, theme}`.
- **Untested binary glue + real duplication.** `main.rs` (1,457 lines) holds
  the interactive supervise loop; 92% of the file is production code with 3
  unit tests. The row-question pause handler (`main.rs:~484`) and the
  clean-question pause handler (`main.rs:~583`) are two near-identical ~100
  line blocks (TUI modal + line-mode prompt variants each). This is the
  region where an interactive regression would land silently.
- **Oversized module files.** `src/supervise.rs`: production 1..1304, tests
  starting line 1305 (≈3,300 lines of test code in one file). `src/tui.rs`:
  production 1..1990, tests starting line 1991 (≈2,250 lines). Reviewing
  either module means wading through the fakes first.
- **Runtime-validated CLI syntax.** `Command::Mark { done: String }`
  (`cli.rs:61–66`) accepts any literal; `cmd_mark` (`main.rs:183–186`)
  rejects `done != "done"` at runtime with `Err(...)` (exit 1). A clap
  `ValueEnum` can reject `mark 4 bogus` at parse time with clap's own usage
  error.
- **Docs drift.** `docs/ARCHITECTURE.md`'s module map predates `tui.rs` and
  `theme.rs` (they are absent), lists `supervise.rs` as a single file, and
  `AGENTS.md`'s "anyhow for the binary" rule is already false today.

## Decisions (locked)

- **D1 — Remove `anyhow`; do not convert the binary to it.** The binary's
  `Result<u8, String>` error strings are already user-facing; converting to
  `anyhow` would add churn with no user-visible gain. Removal is the whole
  change. (`AGENTS.md` is updated to match — see step 8.)
- **D2 — Move `worker_stats_from_run` (and its tests) from `storage.rs` into
  `supervise.rs`.** `storage.rs` keeps the pure external-root/key math, the
  `WorkerStatsRecord` serde shape, `append_worker_stats`, and
  `WORKER_STATS_FILE_NAME` — a leaf again (std + serde + sha2 only).
  `supervise.rs` gains `pub fn worker_stats_from_run` +
  `use crate::storage::WorkerStatsRecord;` — the correct high→low direction.
  `main.rs`'s `append_stats` closure is untouched (both symbols still exist).
- **D3 — Move `PendingTool` to `rpc.rs`** beside `ToolExecutionStart`, with
  its doc comment. `worker.rs` imports it from `crate::rpc` (it already
  imports the RPC types); `ui.rs:20` and `tui.rs:76` switch their import
  source. No field or construction-site change.
- **D4 — Relocate test modules to sibling files; do not split production
  code further.** `git mv src/supervise.rs src/supervise/mod.rs`, move the
  `#[cfg(test)] mod tests { … }` body to `src/supervise/tests.rs`, declare
  `#[cfg(test)] mod tests;` in the new `mod.rs`. Same for `tui.rs`. `lib.rs`
  is unchanged (`pub mod supervise;` resolves to the directory module). A
  finer production split of `tui.rs` (backend/frame/state/input) is deferred:
  at ~1,990 production lines it is readable once the tests are out of the
  file, and a split requires `pub(crate)`/visibility churn.
- **D5 — Interactive loop: extract, then hoist behind a seam.** Two commits.
  (a) Extract the duplicated pause/answer handling into one binary helper
  (byte-for-byte behavior, including the pre-existing quirks listed in
  "Invariants to preserve"). (b) Move the driver (`run_plan` outer loop,
  `last_question`, `last_clean_question`, `keep_clean_continuation`) from
  `main.rs` into `supervise.rs` as `run_plan_interactive(…)` with the
  `QuestionPause` trait + `PauseOutcome` verdict; `main.rs` keeps only setup,
  teardown, and a thin seam implementation (TUI modal / line-mode stdin).
  The driver becomes unit-testable with a scripted pause seam.
- **D6 — `mark` word becomes a clap `ValueEnum`.** `enum MarkWord { Done }`;
  `Command::Mark { row, done: MarkWord }`; `cmd_mark` drops the runtime
  string check. Usage errors now exit via clap's parse path (exit 2) instead
  of the runtime `Err` path (exit 1); the README notes the nuance.
- **D7 — Documentation sync is the final two commits.** Steps 7–8 update
  exactly what the refactors changed: the `ARCHITECTURE.md` module map
  (including the missing `tui`/`theme` entries), the `README.md` layout
  block, `AGENTS.md`'s dependency/error rules, and stale
  `docs/acceptance-e2e.md` path references.

### Considered and not planned

- **`next_row` precomputation** (`todo.rs`): the pending-index sort is
  O(n log n) over a plan's row count once per row — trivially small. Defer
  until profiling shows it matters.
- **Threading `plan_hash` through saves** (`supervise::state_file` re-reads
  TODO.md per save): behavior-sensitive. `state_file` intentionally hashes
  the *current* TODO.md content so a mid-run edit flips the saved hash and
  forces the documented "plan changed → recompute from git" recovery.
  Caching a startup hash would silently change that recovery path.
- **`worker-stats.jsonl` true append** (`storage.rs`): the full-file rewrite
  is the deliberate atomicity mechanism (write-temp-then-rename). A raw
  append could leave a truncated JSON line after a crash. At one record per
  run attempt the O(n²) cumulative cost is acceptable; revisit only if a
  project accumulates thousands of attempts.
- **Unify clap `Supervise`/`Step`:** they differ in UX (`--row` flag vs
  positional), both documented commands, and the shared dispatch already
  collapses them in `main.rs`. The 12-line clap saving is not worth breaking
  CLI compatibility.
- **Fix the restart-at-ASK quirk** (line mode folds the literal `restart`
  into the answer; TUI `Restart` at an ASK pause ends the run without an
  answer): pre-existing behavior, deliberately preserved byte-for-byte by
  this plan (see invariants). Recorded under "Known discrepancies" — a
  separate follow-up is recommended, not bundled here.
- **Full `tui.rs` production split:** deferred (see D4).

## Commit-by-commit plan

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit with the message
in the table → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `chore: drop the unused anyhow dependency` | Dep hygiene | `Cargo.toml`: remove `anyhow = "=1.0.104"` (line 23); `Cargo.lock` prunes the package on the next build | `cargo build` + `cargo test` green; `grep -rn anyhow Cargo.toml src/ tests/` zero hits |
| 2 | `refactor: move worker-stats mapping to the run-record owner` | Layering fix | `src/storage.rs`: remove `worker_stats_from_run`, the `run_record()` test helper, their two tests, the top-level `use crate::supervise::{RunRecord, run_outcome_label}` and the test-module `supervise/worker/todo` imports; keep resolution math, `WorkerStatsRecord`, `WORKER_STATS_FILE_NAME`, `append_worker_stats` + its tests. `src/supervise.rs`: add `use crate::storage::WorkerStatsRecord;` + `pub fn worker_stats_from_run` + the two moved tests in `mod tests` | Full suite green; `cargo clippy -D warnings`; `src/storage.rs` imports = std + serde + sha2 only |
| 3 | `refactor: move PendingTool to the RPC event module` | Interface cleanup | `src/rpc.rs`: add `pub struct PendingTool` beside `ToolExecutionStart` with the moved doc comment. `src/worker.rs`: delete the struct, add `PendingTool` to its `use crate::rpc::{…}` import list. `src/ui.rs:20` and `src/tui.rs:76`: import `PendingTool` from `crate::rpc`; also update `tool_context_lines`' doc comment (`worker::PendingTool` → `rpc::PendingTool`, same cycle rationale) | Every `PendingTool` site compiles; `src/ui.rs` imports = `{rpc, theme}` only; full suite green |
| 4a | `refactor: split supervise module tests to a sibling file` | File organization | `git mv src/supervise.rs src/supervise/mod.rs`; move the `#[cfg(test)] mod tests { … }` body (production 1..1304, tests 1305..EOF) to `src/supervise/tests.rs`; `src/supervise/mod.rs` gains `#[cfg(test)] mod tests;` | `cargo test`: the same 327 unit tests + 341 total; fmt; clippy |
| 4b | `refactor: split tui module tests to a sibling file` | File organization | `git mv src/tui.rs src/tui/mod.rs`; tests body (production 1..1990, tests 1991..EOF) to `src/tui/tests.rs`; `#[cfg(test)] mod tests;` in `mod.rs`; the `tui::decode_key` doctest moves with the fn | `cargo test`: same 341 tests incl. the doctest; fmt; clippy |
| 5a | `refactor: extract the shared interactive question-pause handler` | De-duplicate main.rs pauses | `src/main.rs`: one `async fn run_question_pause(…) -> Option<String>` (answer or `None` = stopped/blank/EOF) replacing both the row (≈line 484) and clean (≈line 583) TUI+line-mode pause blocks; `status` handling via a small closure; `stop_was_kill` branch preserved | Full suite green; behavior byte-identical (invariants table below); manual acceptance: ASK pause → answer, stop, ^D kill in both modes |
| 5b | `feat: hoist the interactive driver into the library behind a pause seam` | Testable interactive loop | `src/supervise.rs`: `pub enum PauseOutcome { Answer(String), NoAnswer, Stopped { kill: bool } }`, `#[allow(async_fn_in_trait)] pub trait QuestionPause { async fn pause(&self, question: &str) -> PauseOutcome; }`, `pub async fn run_plan_interactive(services, plan, answer, clean_continuation, pause) -> Option<RunPlanResult>`, and private `last_question`/`last_clean_question`/`keep_clean_continuation` moved from `main.rs` (with their tests). `src/main.rs`: loop collapses to setup + seam impl; `Err("supervise ended without a result")` path preserved | New driver tests (scripted pause seam): answered row question folds next pass; row-question stop and blank/^D → `None` (exit-1 Err path); kill-stop → `Some(result)`; clean-question answered → one answered continuation; second clean ASK → terminate with `Some(result)`; clean stop → `Some(result)`; orphan path → `Some(result)`; plan-to-done → `Some(result)`; full suite green |
| 6 | `refactor: fail mark <row> usage errors at clap parse time` | CLI polish | `src/cli.rs`: `#[derive(Debug, Clone, Copy, clap::ValueEnum)] pub enum MarkWord { Done }`; `Command::Mark { row: u64, done: MarkWord }`. `src/main.rs::cmd_mark`: take the enum, drop the `done != "done"` check | `pi-plan mark 4 bogus` → clap usage error at parse; `pi-plan mark 4 done` unchanged; `parse_mark_requires_the_done_word` asserts `MarkWord::Done`; `mark_rejects_any_other_written_argument` flips to a parse-rejection test; full suite green |
| 7 | `docs: sync ARCHITECTURE.md and README with the refactor changes` | Doc fidelity | `docs/ARCHITECTURE.md`: module map (`supervise.rs` → `supervise/` state machine + interactive driver + tests; add the missing `tui/` and `theme.rs` entries; `storage.rs` described as pure external-root + audit-log writer; `main.rs` kept as wiring + pause seam); prose that names `PendingTool`/`storage` responsibilities. `README.md`: repo-layout `src/` line; exit-code/usage note for `mark`. `docs/acceptance-e2e.md`: no stale path references exist there today (its only `.rs` mention is the live `tests/rpc_fake_pi.rs`) — **scope the stale-reference probe to living docs** (`docs/ARCHITECTURE.md`, `README.md`, `docs/acceptance-e2e.md`, `AGENTS.md`); `docs/research/` is archival and exempt (`plan-rust-orchestrator.md` and this plan intentionally name `anyhow`/`supervise.rs`) | fmt/test/clippy green; grep clean over living docs |
| 8 | `docs: align AGENTS.md error-handling guidance with the crate state` | Doc fidelity | `AGENTS.md` "Rust style rules": drop the "`anyhow` for the binary" clause (the dependency is removed); state the actual rule: `thiserror` for library errors, plain `Result<u8, String>` for binary command errors | n/a (docs only); full suite green |

## Invariants to preserve (steps 5a/5b)

The extracted pause path must reproduce the current behavior exactly. Read
from `main.rs` at the time of writing; the implementer must confirm each row
after 5a and again after 5b:

| Situation (ASK-pause flow) | Current behavior | Must stay |
|---|---|---|
| TUI, answer typed / selected | `carried = Some(answer)` → loop re-runs `run_plan` (row) or re-fires the gate (clean) | answer folds into the next pass |
| TUI, `AskAnswer(None)` (blank/^D in modal) | no answer; row → break without `final_result`; clean → `final_result = Some(result)` | row → `None` (Err path, exit 1); clean → `Some(result)` (report, exit 2) |
| TUI, `Stop` (Ctrl-C / stop file) | sets `stop_requested`; `final_result` stays `None` unless `stop_was_kill` | row: `None` unless kill (kill → `Some(result)`); clean: `Some(result)` always |
| TUI, `Restart` | sets `restart_requested`; no answer → break (existing quirk: no `final_result`) | preserve exactly (driver returns `None`); do NOT "fix" restart-at-ASK in this plan |
| Line mode, valid answer | `carried = Some(input)` | same |
| Line mode, `stop` | sets `stop_requested`; done, no answer | same |
| Line mode, `status` | reprints `format_status_report(...)` and re-prompts | same via the injected status closure |
| Line mode, blank line / EOF | no answer | same |
| Line mode, literal `restart` | falls to the `_` arm → folded  **as the answer text** (pre-existing quirk) | preserve exactly; do NOT treat as a control here |
| Clean question, second consecutive ASK | terminates with `final_result = Some(result)` (one answered continuation per chain) | driver returns `Some(result)` without pausing again |
| Clean answer, tree already clean | `run_row` returns `CleanAnswerOrphaned` (already tested in supervise) | driver sees no question → returns `Some(result)` |

The driver's return is `Option<RunPlanResult>`: `None` reproduces the
existing `Err("supervise ended without a result")` exit-1 path; `Some(..)`
reproduces the final-report exit-2/0 path.

## Step notes

### Step 1 (anyhow)

- Edit `Cargo.toml` only; the next `cargo build`/`cargo test` regenerates
  `Cargo.lock` and prunes the `anyhow` package entry. Verify the lock diff
  removes exactly `anyhow` (its transitive crates, if any were pulled solely
  by it, also vanish — check the diff; none is expected since `anyhow` pulls
  nothing else).
- Do not convert the binary's `Result<u8, String>` in this step (D1).

### Step 2 (worker-stats mapping)

- Moved items: `pub fn worker_stats_from_run`, tests
  `worker_stats_from_run_skips_snapshot_less_runs`,
  `worker_stats_from_run_maps_the_terminal_snapshot_fields`, and the
  `run_record(snapshot)` helper. In `supervise.rs` the helper slots naturally
  next to `report_terminal`/`RunRecord`.
- `storage.rs` keeps `append_worker_stats_writes_one_json_line_per_record…`
  (its `stats_record()` helper stays local).
- The supervise test module needs no new imports beyond what `super::*`
  provides once `supervise.rs` imports `WorkerStatsRecord` (private `use`
  names are visible through the glob) — verify with the compiler.
- `main.rs`'s `append_stats` closure (`worker_stats_from_run` →
  `append_worker_stats`) stays as-is, but its import block edits: line ~44's
  `use pi_plan::storage::{ProjectStorage, append_worker_stats,
  worker_stats_from_run}` sheds `worker_stats_from_run`, which joins the
  `pi_plan::supervise::{…}` list (the closure body itself is untouched).
- Pitfall: don't leave a dangling `use crate::worker::{Tokens, WorkerSnapshot}`
  in `storage.rs`'s test module — both become unused after the move; clippy
  with `-D warnings` catches it.

### Step 3 (PendingTool)

- Keep the existing field names and `#[derive(Debug, Clone, PartialEq)]`
  verbatim; move the doc comment with the type.
- Grep every reference first: `src/worker.rs` (struct def, `SnapshotAcc`,
  `note_event`, tests), `src/ui.rs` (`tool_context_lines`), `src/tui.rs`
  (`open_modal`, `ModalBoxOpts`, `WorkerView`, `TuiState`, tests). `main.rs`
  names no `PendingTool` type directly (inferred through the snapshot) — no
  main.rs change expected; verify.
- After the move, `src/ui.rs` imports must be exactly `crate::rpc::{…}`
  and `crate::theme::{…}` (+ proptest in tests).

### Steps 4a/4b (test-module relocation)

- `git mv` (not delete+create) so history records the rename.
- `lib.rs` is unchanged; `pub mod supervise;`/`pub mod tui;` resolve to
  `src/supervise/mod.rs`/`src/tui/mod.rs`.
- In `tests.rs`, `use super::*;` keeps working (the sibling file is a child
  module of the parent). Do not re-wrap in `mod tests { … }`.
- Pitfall: both `src/supervise.rs` and `src/supervise/mod.rs` existing at
  once is a compile error — the rename must land in the same commit.
- The `tui::decode_key` doctest compiles from its new path automatically;
  headless `tests/tui_backend.rs` still self-skips.

### Step 5a (pause-handler extraction)

- Extract first, hoist later: 5a is pure `main.rs` surgery with zero
  structural change; 5b is the move. Keeping them separate isolates risk.
- Recommended signature (bind the mode-specific pieces as a small struct or
  closure to keep parameters manageable):
  `async fn run_question_pause(tui_state: &Arc<Mutex<TuiState>>, control: &RunControl, tui_active: bool, status: &dyn Fn(), question: &str) -> Option<String>`
  — `None` = stopped/blank/EOF; the caller decides row-vs-clean routing
  (`carried`/`carried_clean`), which the plan keeps in the driver loop
  (then in `run_plan_interactive` in 5b).
- The `stop_was_kill` nuance is reproducible by the helper returning `None`
  while the caller inspects `stop_was_kill(control)` — keep the flag reading
  in whatever layer mirrors the current code so both semantics survive 5b.
- Verify against the invariants table; run the manual acceptance items.

### Step 5b (driver hoist)

- Place `QuestionPause`/`PauseOutcome` near `run_plan`; use the existing
  `#[allow(async_fn_in_trait)]` precedent (same rationale as `WorkerPort`).
- `run_plan_interactive` owns `carried`/`carried_clean` internally:

  ```rust
  pub async fn run_plan_interactive<'a, G: GitFacts, W: WorkerPort>(
      services: &SuperviseServices<'a, G, W>,
      plan: &TodoPlan,
      answer: Option<&str>,
      clean_continuation: Option<&CleanContinuation>,
      pause: &dyn QuestionPause,
  ) -> Option<RunPlanResult>
  ```

- `PauseOutcome` is the exhaustive verdict set the invariants table
  requires; the seam maps the pause outcomes 1:1. TUI `AskAnswer(Some(a))`
  → `Answer(a)`; line-mode non-command input → `Answer(input)` — including
  the literal `restart`/`resume` quirk, which `line_command` classifies as
  `LineCommand::Restart` but the ASK pause matches only `Stop`/`Status`, so
  it folds as the answer text; TUI `AskAnswer(None)` (blank/^D) →
  `NoAnswer`; TUI `Restart` flips `restart_requested` then `NoAnswer`;
  line-mode `stop` flips `stop_requested` then `NoAnswer`; line-mode blank /
  EOF → `NoAnswer`; TUI `Stop` (Ctrl-C / stop file) flips `stop_requested`
  then returns `Stopped { kill: stop_was_kill(control) }` (the flag reading
  moves into the seam at 5b). `status` re-prints and re-prompts inside the
  seam; it never surfaces to the driver.
- Driver routing (final-result semantics per the invariants table):
  row-question `Answer` folds into the next pass; row `NoAnswer` /
  `Stopped { kill: false }` → return `None` (exit-1 `Err` path); row
  `Stopped { kill: true }` → return `Some(result)`; clean `Answer` sets
  exactly one continuation; clean `NoAnswer` / `Stopped { … }` → return
  `Some(result)`, as does a second consecutive clean ASK while
  `carried_clean` is already set.
- The new driver tests reuse `FakeGit`/`FakeWorkerPort`/`clean_services` and
  add a scripted `FakePause` (a queue of `PauseOutcome`). Because
  `QuestionPause::pause` is a trait method, the fake is trivial.
- Do not change `stop_was_kill`, business of `RunControl`, or the kill-watcher
  ordering (`kill_requested` before `stop_requested`) — untouched by this
  step.
- `main.rs` keeps: `stdin_read_line`, `ask_lines` call, the status closure,
  the seam impl (TUI modal open/await + control-flag flipping), setup,
  `workers.dispose()`, render-task teardown, and the final
  `match final_result { Some => report; None => Err(...) }`.

### Step 6 (clap mark word)

- `MarkWord` kebab-cases to `done` automatically; clap's usage error exits
  with clap's parse code (2) via `Cli::parse()` in `main()` — no run-path
  change. Note the exit-code nuance for README step 7.
- Update the two `cli.rs` unit tests that destructure
  `Command::Mark { row, done }` (`done` becomes `MarkWord`).
  `parse_mark_requires_the_done_word` keeps its shape and now asserts
  `done == MarkWord::Done`. `mark_rejects_any_other_written_argument`
  INVERTS: today it asserts
  `Cli::try_parse_from(["pi-plan", "mark", "4", "bogus"]).expect(
  "parse mark")` succeeds (that was the whole point — the runtime check
  caught it); after the ValueEnum change the parse fails, so the test
  becomes a parse-rejection test: `Cli::try_parse_from(...)` returns `Err`
  with clap's usage error. Do not keep a success assertion here.
- `cmd_mark(cwd, row, _done: MarkWord)` — or drop the parameter and match
  the variant once; keep the `main.rs` signature change minimal.

### Steps 7/8 (documentation fidelity)

- `docs/ARCHITECTURE.md` module map — current text lists
  `main.rs … the interactive supervise loop`, `storage.rs … external run-state
  root resolution`, `supervise.rs … run/retry/ask state machine …`;
  **`tui.rs` and `theme.rs` are missing entirely**. Update to:
  - `supervise/` — run/retry/ask state machine, the interactive driver
    (`run_plan_interactive`) behind the `QuestionPause` seam, and the
    worker-stats record mapping; tests in `src/supervise/tests.rs`.
  - `tui/` — full-screen renderer (backend, frame builders, `TuiState`,
    input task); tests in `src/tui/tests.rs`.
  - `theme.rs` — palette loader + `Stylize`.
  - `storage.rs` — pure external run-state root resolution
    (`~/.pi-plan/<key>/`) and the atomic `worker-stats.jsonl` audit-log
    writer (no supervise/worker imports).
  - `main.rs` — binary dispatch + the interactive pause seam implementation.
  - Resume the doc's "Sourcing and data flow" and "Operator surface" prose
    for `PendingTool` provenance (rpc layer) and worker-stats mapping
    location.
- `README.md` — repo layout `src/` line: `main`, `cli`, `config`, `todo`,
  `git`, `prompt`, `rpc`, `state`, `storage`, `theme`, `worker`,
  `supervise/` (+ tests), `tui/` (+ tests), `ui`. Add a half-line to
  "Commands"/"Exit codes" noting usage errors (e.g. `mark 4 bogus`) fail at
  clap parse (exit 2), distinct from supervise outcomes.
- `docs/acceptance-e2e.md` — grep for stale `src/…rs` path references (none
  today; the only `.rs` mention is the live `tests/rpc_fake_pi.rs`);
  behavior steps unchanged.
- `AGENTS.md` — replace "`thiserror` for library errors, `anyhow` for the
  binary" with the true rule: `thiserror` for library errors; the binary uses
  `Result<u8, String>` for command errors (no `anyhow` dependency, no
  `panic!`/`unwrap`/`expect` in production).
- Keep the doc commits green per the quality gates (they are, trivially).

## Known discrepancies recorded during planning (follow-ups, out of scope)

- **README vs code: restart at answer prompts.** README ("Permissions
  behavior") states `restart` is accepted at any dialog/answer prompt; in
  line mode an ASK-pause literal `restart` is folded into the worker's prompt
  as the answer text, and in the TUI a `Restart` verdict at an ASK pause ends
  the run without an answer. This plan preserves both quirks byte-for-byte
  (invariants table) and does not touch README's wording. Recommend a
  dedicated follow-up to define restart-at-ASK semantics and then sync the
  README.
- These are the only doc/behavior mismatches found during planning; steps
  7–8 fix everything else (missing `tui.rs`/`theme.rs` in the module map, the
  stale `anyhow` rule, stale file paths).

## Acceptance criteria (end state)

- `cargo test` (341 tests), `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings` all green after the final commit.
- `grep -rn anyhow Cargo.toml src/ tests/` → zero hits; the same probe
  over living docs (`docs/ARCHITECTURE.md`, `README.md`,
  `docs/acceptance-e2e.md`, `AGENTS.md`) → zero hits (`docs/research/` is
  archival — `plan-rust-orchestrator.md` and this plan intentionally mention
  `anyhow`).
- `src/storage.rs` has no imports from `crate::supervise` or `crate::worker`.
- `src/supervise.rs` no longer exists (directory module + `tests.rs`);
  `src/tui.rs` likewise.
- `main.rs` no longer contains `last_question`/`last_clean_question`/
  `keep_clean_continuation` or duplicated pause blocks; the interactive
  driver has new unit tests in `src/supervise/tests.rs`.
- `docs/` file names/paths and the `AGENTS.md` rules match the codebase.
- Manual: `pi-plan step N` with a scripted ASK — answer, stop, and ^D behave
  exactly as before the refactor (invariants table).
