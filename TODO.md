# Implementation Plan: Architecture hardening — follow-ups from the 2026-09-13 review

Source: `docs/research/plan-architecture-hardening.md`

A set of small, green architecture-hardening commits for the `pi-plan`
orchestrator, derived from the full architecture review at HEAD `b45ebcf`
(sound overall: clean acyclic module DAG, testable seams, 341 tests, zero
`unsafe`/`unwrap`/`expect`/`panic!`). Ten commits: dep hygiene, a layering
fix, an interface cleanup, two test-module relocations, de-duplication +
testability of the interactive loop, a CLI parse-time validation fix, and a
two-commit documentation-fidelity pass.

The commit messages in the table below are **exact** — taken verbatim from
the source plan. Workflow per step: implement → `cargo test` → `cargo fmt
--check` → `cargo clippy --all-targets --all-features -- -D warnings` →
commit with the table's message → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `chore: drop the unused anyhow dependency` | Dep hygiene | `Cargo.toml`: remove `anyhow = "=1.0.104"` (line 23); `Cargo.lock` pruned on next build | `cargo build` + full suite green; `grep -rn anyhow Cargo.toml src/ tests/` zero hits |
| 2 | `refactor: move worker-stats mapping to the run-record owner` | Layering fix | `src/storage.rs` (remove `worker_stats_from_run`, helper, 2 tests, supervise/worker imports); `src/supervise.rs` (add `pub fn worker_stats_from_run` + 2 moved tests); `src/main.rs` import block | Unit (moved tests), full suite |
| 3 | `refactor: move PendingTool to the RPC event module` | Interface cleanup | `src/rpc.rs` (add `pub struct PendingTool` beside `ToolExecutionStart`); `src/worker.rs` (delete struct, import from rpc); `src/ui.rs:20`, `src/tui.rs:76` (import source + `tool_context_lines` doc comment) | Every `PendingTool` site compiles; `src/ui.rs` imports = `{rpc, theme}` only; full suite |
| 4 | `refactor: split supervise module tests to a sibling file` | File organization | `git mv src/supervise.rs src/supervise/mod.rs`; tests body → `src/supervise/tests.rs`; `#[cfg(test)] mod tests;` in `mod.rs` | Same 327 unit tests + 341 total; fmt; clippy |
| 5 | `refactor: split tui module tests to a sibling file` | File organization | `git mv src/tui.rs src/tui/mod.rs`; tests body → `src/tui/tests.rs`; `#[cfg(test)] mod tests;` in `mod.rs` | Same 341 tests incl. `tui::decode_key` doctest; fmt; clippy |
| 6 | `refactor: extract the shared interactive question-pause handler` | De-duplicate main.rs pauses | `src/main.rs`: one `async fn run_question_pause(…) -> Option<String>` replacing both the row (~line 484) and clean (~line 583) TUI+line-mode pause blocks; status via a small closure; `stop_was_kill` branch preserved | Full suite green; behavior byte-identical (invariants table); manual acceptance: ASK pause → answer, stop, ^D kill in both modes |
| 7 | `feat: hoist the interactive driver into the library behind a pause seam` | Testable interactive loop | `src/supervise.rs`: `pub enum PauseOutcome`, `#[allow(async_fn_in_trait)] pub trait QuestionPause`, `pub async fn run_plan_interactive(…)`, private `last_question`/`last_clean_question`/`keep_clean_continuation` (+ their tests) moved from `main.rs`; `src/main.rs` → setup + seam impl; `Err("supervise ended without a result")` path preserved | New driver unit tests (scripted `FakePause`): answered row question folds next pass; row `NoAnswer`/non-kill stop → `None`; kill-stop → `Some(result)`; clean answered → one continuation; second clean ASK → terminate `Some(result)`; clean stop / orphan / plan-to-done → `Some(result)`; full suite green |
| 8 | `refactor: fail mark <row> usage errors at clap parse time` | CLI polish | `src/cli.rs`: `#[derive(…, clap::ValueEnum)] pub enum MarkWord { Done }`; `Command::Mark { row: u64, done: MarkWord }`. `src/main.rs::cmd_mark`: take the enum, drop the runtime `done != "done"` check | `pi-plan mark 4 bogus` → clap usage error (exit 2); `mark 4 done` unchanged; `parse_mark_requires_the_done_word` asserts `MarkWord::Done`; `mark_rejects_any_other_written_argument` flips to a parse-rejection test; full suite |
| 9 | `docs: sync ARCHITECTURE.md and README with the refactor changes` | Doc fidelity | `docs/ARCHITECTURE.md` (module map: `supervise/`, missing `tui/` + `theme.rs`, `storage.rs` purity, `main.rs` as wiring + pause seam; prose for `PendingTool` provenance and worker-stats mapping); `README.md` (repo layout `src/` line; `mark` usage-error exit-code note); `docs/acceptance-e2e.md` scope check | fmt/test/clippy green; grep clean over living docs; `docs/research/` exempt (archival) |
| 10 | `docs: align AGENTS.md error-handling guidance with the crate state` | Doc fidelity | `AGENTS.md` "Rust style rules": drop "`anyhow` for the binary"; state the actual rule: `thiserror` for library errors, plain `Result<u8, String>` for binary command errors | n/a (docs only); full suite green |

## Locked decisions (from the source plan)

- **D1** Remove `anyhow`; do **not** convert the binary to it (`Result<u8, String>` is already user-facing).
- **D2** Move `worker_stats_from_run` (+ its tests) from `storage.rs` into `supervise.rs`; `storage.rs` becomes a leaf again (std + serde + sha2 only).
- **D3** Move `PendingTool` to `rpc.rs` beside `ToolExecutionStart`; no field or construction-site change (`worker.rs` already imports the RPC types).
- **D4** Relocate test modules to sibling files only — do **not** split production code further (`lib.rs` unchanged; `pub mod supervise;`/`pub mod tui;` resolve to the directory modules).
- **D5** Interactive loop: (a) extract the duplicated pause/answer handling as pure `main.rs` surgery, then (b) hoist the driver into `supervise.rs` behind the `QuestionPause` seam.
- **D6** `mark` word becomes a clap `ValueEnum`; usage errors exit via clap's parse path (exit 2) instead of the runtime `Err` path (exit 1); README notes the nuance.
- **D7** Documentation sync is the final two commits, updating exactly what the refactors changed.

### Considered and not planned (do not re-propose)

`next_row` precomputation, threading `plan_hash` through saves
(intentionally hashes *current* TODO.md), `worker-stats.jsonl` true append
(full-file rewrite is the deliberate atomicity mechanism), unifying clap
`Supervise`/`Step`, splitting `tui.rs` production further, and fixing the
pre-existing restart-at-ASK quirk (preserved byte-for-byte; recorded as a
follow-up under "Known discrepancies").

## Invariants to preserve (steps 6–7)

The extracted pause path must reproduce current behavior exactly; confirm
each row after step 6 **and** again after step 7:

| Situation (ASK-pause flow) | Current behavior | Must stay |
|---|---|---|
| TUI, answer typed / selected | `carried = Some(answer)` → loop re-runs `run_plan` (row) or re-fires the gate (clean) | answer folds into the next pass |
| TUI, `AskAnswer(None)` (blank/^D in modal) | row → break without `final_result`; clean → `final_result = Some(result)` | row → `None` (Err path, exit 1); clean → `Some(result)` (report, exit 2) |
| TUI, `Stop` (Ctrl-C / stop file) | `stop_requested`; `final_result` stays `None` unless `stop_was_kill` | row: `None` unless kill (kill → `Some(result)`); clean: `Some(result)` always |
| TUI, `Restart` | `restart_requested`; no answer → break (existing quirk) | preserve exactly (driver returns `None`); do **not** "fix" restart-at-ASK |
| Line mode, valid answer | `carried = Some(input)` | same |
| Line mode, `stop` | `stop_requested`; done, no answer | same |
| Line mode, `status` | reprints `format_status_report(...)` and re-prompts | same via the injected status closure |
| Line mode, blank line / EOF | no answer | same |
| Line mode, literal `restart` | falls to the `_` arm → folded **as the answer text** (pre-existing quirk) | preserve exactly; do NOT treat as a control |
| Clean question, second consecutive ASK | terminates with `final_result = Some(result)` (one answered continuation per chain) | driver returns `Some(result)` without pausing again |
| Clean answer, tree already clean | `run_row` returns `CleanAnswerOrphaned` (already tested in supervise) | driver sees no question → returns `Some(result)` |

The driver's return is `Option<RunPlanResult>`: `None` reproduces the
existing `Err("supervise ended without a result")` exit-1 path; `Some(..)`
reproduces the final-report exit-2/0 path.

## Step notes

### Step 1 (anyhow)

- Edit `Cargo.toml` only; next build regenerates `Cargo.lock` and prunes the
  `anyhow` package entry (currently `Cargo.lock:56`). Verify the lock diff
  removes exactly `anyhow` (+ any crates pulled solely by it; none expected).
- Do not convert `Result<u8, String>` in this step (D1).

### Step 2 (worker-stats mapping)

- Moved items: `pub fn worker_stats_from_run`, tests
  `worker_stats_from_run_skips_snapshot_less_runs`,
  `worker_stats_from_run_maps_the_terminal_snapshot_fields`, and the
  `run_record(snapshot)` helper. In `supervise.rs` the helper slots next to
  `report_terminal`/`RunRecord`.
- `storage.rs` keeps `append_worker_stats_writes_one_json_line_per_record…`
  (its `stats_record()` helper stays local). The supervise test module needs
  no new imports beyond `super::*` — verify with the compiler.
- `main.rs`'s `append_stats` closure stays as-is; its import block: line ~44
  sheds `worker_stats_from_run` (joins the `pi_plan::supervise::{…}` list).
- Pitfall: don't leave a dangling `use crate::worker::{Tokens,
  WorkerSnapshot}` in `storage.rs`'s test module — clippy `-D warnings`
  catches it.

### Step 3 (PendingTool)

- Keep field names and `#[derive(Debug, Clone, PartialEq)]` verbatim; move
  the doc comment with the type.
- Grep every reference first: `src/worker.rs` (struct def, `SnapshotAcc`,
  `note_event`, tests), `src/ui.rs` (`tool_context_lines`), `src/tui.rs`
  (`open_modal`, `ModalBoxOpts`, `WorkerView`, `TuiState`, tests). `main.rs`
  names no `PendingTool` type directly — no change expected; verify.
- After the move, `src/ui.rs` imports must be exactly `crate::rpc::{…}` and
  `crate::theme::{…}` (+ proptest in tests).

### Steps 4–5 (test-module relocation)

- `git mv` (not delete+create) so history records the rename.
- `lib.rs` is unchanged. In `tests.rs`, `use super::*;` keeps working (sibling
  child module); do **not** re-wrap in `mod tests { … }`.
- Pitfall: `src/supervise.rs` and `src/supervise/mod.rs` both existing at once
  is a compile error — the rename must land in the same commit.
- The `tui::decode_key` doctest compiles from its new path automatically;
  headless `tests/tui_backend.rs` still self-skips.

### Step 6 (pause-handler extraction)

- Extract first, hoist later: step 6 is pure `main.rs` surgery, step 7 is the
  move. Keeping them separate isolates risk.
- Recommended signature: `async fn run_question_pause(tui_state: &Arc<Mutex<TuiState>>, control: &RunControl, tui_active: bool, status: &dyn Fn(), question: &str) -> Option<String>` — `None` = stopped/blank/EOF; the caller decides row-vs-clean routing (`carried`/`carried_clean`), kept in the driver loop.
- `stop_was_kill` (read from `control`) reproduces the kill nuance via the
  helper returning `None` while the caller inspects the flag.
- Verify against the invariants table + manual acceptance items.

### Step 7 (driver hoist)

- Place `QuestionPause`/`PauseOutcome` near `run_plan`; use the existing
  `#[allow(async_fn_in_trait)]` precedent (same rationale as `WorkerPort`).
- `run_plan_interactive` owns `carried`/`carried_clean` internally (signature
  in the source plan; `services`, `plan`, `answer: Option<&str>`,
  `clean_continuation: Option<&CleanContinuation>`, `pause: &dyn QuestionPause`).
- `PauseOutcome` is the exhaustive verdict set: `Answer(String)`,
  `NoAnswer`, `Stopped { kill: bool }`. Seam mapping: TUI `AskAnswer(Some(a))`
  → `Answer(a)`; line-mode non-command input → `Answer(input)` (including the
  literal `restart`/`resume` quirk — `line_command` classifies `Restart` but
  the ASK pause matches only `Stop`/`Status`, so it folds as the answer
  text); TUI `AskAnswer(None)` → `NoAnswer`; TUI `Restart` flips
  `restart_requested` then `NoAnswer`; line `stop` flips `stop_requested`
  then `NoAnswer`; line blank/EOF → `NoAnswer`; TUI `Stop` flips
  `stop_requested` then `Stopped { kill: stop_was_kill(control) }` (flag
  reading moves into the seam at step 7). `status` re-prints and re-prompts
  inside the seam; never surfaces to the driver.
- Driver routing: row `Answer` folds into the next pass; row `NoAnswer` /
  `Stopped { kill: false }` → `None`; row `Stopped { kill: true }` →
  `Some(result)`; clean `Answer` sets exactly one continuation; clean
  `NoAnswer` / `Stopped` → `Some(result)`, as does a second consecutive clean
  ASK while `carried_clean` is set.
- New driver tests reuse `FakeGit`/`FakeWorkerPort`/`clean_services`, plus a
  scripted `FakePause` (queue of `PauseOutcome`).
- Do not change `stop_was_kill`, `RunControl`'s business, or kill-watcher
  ordering (`kill_requested` before `stop_requested`).
- `main.rs` keeps: `stdin_read_line`, `ask_lines` call, the status closure,
  the seam impl (TUI modal open/await + control-flag flipping), setup,
  `workers.dispose()`, render-task teardown, and the final
  `match final_result { Some => report; None => Err(...) }`.

### Step 8 (clap mark word)

- `MarkWord` kebab-cases to `done` automatically; usage errors exit with
  clap's parse code (2) via `Cli::parse()` in `main()` — no run-path change.
- Update the two `cli.rs` unit tests that destructure
  `Command::Mark { row, done }`. `parse_mark_requires_the_done_word` now
  asserts `done == MarkWord::Done`.
  `mark_rejects_any_other_written_argument` **inverts** — today it asserts
  `Cli::try_parse_from(["pi-plan", "mark", "4", "bogus"])` **succeeds** (the
  runtime check's whole point); after the `ValueEnum` change it becomes a
  parse-rejection test (`try_parse_from` returns `Err` with clap's usage
  error). Do not keep a success assertion.
- Keep the `cmd_mark(cwd, row, _done: MarkWord)` signature change minimal.

### Steps 9–10 (documentation fidelity)

- `docs/ARCHITECTURE.md` module map — `tui.rs` and `theme.rs` are missing
  entirely today. Update to: `supervise/` (state machine + interactive
  driver behind `QuestionPause` + worker-stats mapping; tests in
  `src/supervise/tests.rs`); `tui/` (renderer: backend, frame builders,
  `TuiState`, input task; tests in `src/tui/tests.rs`); `theme.rs` (palette
  loader + `Stylize`); `storage.rs` (pure external root resolution +
  atomic `worker-stats.jsonl` writer); `main.rs` (dispatch + pause seam).
  Resume "Sourcing and data flow" / "Operator surface" prose for
  `PendingTool` provenance and worker-stats mapping location.
- `README.md`: repo-layout `src/` line gains `theme`, `supervise/` (+tests),
  `tui/` (+tests); half-line in "Commands"/"Exit codes" noting usage errors
  (e.g. `mark 4 bogus`) fail at clap parse (exit 2), distinct from supervise
  outcomes.
- `docs/acceptance-e2e.md`: grep for stale `src/…rs` path references (none
  today; the only `.rs` mention is the live `tests/rpc_fake_pi.rs`) — scope
  the stale-reference probe to **living docs** (`docs/ARCHITECTURE.md`,
  `README.md`, `docs/acceptance-e2e.md`, `AGENTS.md`); `docs/research/` is
  archival and exempt (this plan and `plan-rust-orchestrator.md`
  intentionally name `anyhow`/`supervise.rs`).

## Acceptance criteria (end state)

- `cargo test` (341 tests), `cargo fmt --check`, `cargo clippy
  --all-targets --all-features -- -D warnings` all green after the final
  commit.
- `grep -rn anyhow Cargo.toml src/ tests/` → zero hits; the same probe over
  living docs (`docs/ARCHITECTURE.md`, `README.md`, `docs/acceptance-e2e.md`,
  `AGENTS.md`) → zero hits (`docs/research/` is archival).
- `src/storage.rs` has no imports from `crate::supervise` or `crate::worker`.
- `src/supervise.rs` no longer exists (directory module + `tests.rs`);
  `src/tui.rs` likewise.
- `main.rs` no longer contains `last_question`/`last_clean_question`/
  `keep_clean_continuation` or duplicated pause blocks; the interactive
  driver has new unit tests in `src/supervise/tests.rs`.
- `docs/` file names/paths and the `AGENTS.md` rules match the codebase.
- Manual: `pi-plan step N` with a scripted ASK — answer, stop, and ^D behave
  exactly as before the refactor (invariants table).
