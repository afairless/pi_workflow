# Plan: Ask the operator to reset an exhausted row budget and resume

## Status

A research-to-build plan for `pi_plan_workflow`, a follow-up to the
Contract 4 run/retry/ask state machine (`src/supervise/mod.rs`, ported from
`supervise.ts`). It changes one operator-visible behavior of `supervise`
(and `step` / `supervise --row N`, which share the same driver):

**When a row's budget is exhausted, the supervisor exits with a report (exit
2). Re-starting `supervise` immediately re-hits the budget gate, reports
"budget exhausted" again, and exits — there is no way to continue the row
short of editing the state file.** This plan makes the gate yield an
interactive choice instead: the operator may **reset the row's budget**
(`runs_used` back to 0, full 2-run budget) and resume, or decline and get
today's stop-with-report-plus-exit-2 behavior byte-for-byte.

Design decisions were locked with the user on 2026-09-27 via the Q&A below.
No code was changed during this investigation.

## Goal (user requirements)

> "When the budget on a step runs out, the supervise command refuses to work
> any further on that step and exits the program. When the user re-starts
> supervise, the tool reports that the budget is exhausted and exits again.
> Instead of automatically exiting upon re-start, supervise should give the
> user the option of resetting the budget for that step and resuming the
> work."

## Locked decisions (Q&A, 2026-09-27)

| # | Question | Decision |
| --- | --- | --- |
| 1 | When should the budget-reset prompt appear? | **Also mid-session.** The prompt fires whenever the budget gate would block a row — both at (re)start on a row whose budget is already spent **and** live, when the second attempt of a row fails mid-run. No asked-this-row latch is needed: every prompt is a blocking operator question, so an automatic prompt loop is impossible by construction (a repeated "yes" is the operator's deliberate, unlimited choice). |
| 2 | How much budget does a reset restore? | **Full budget** — `runs_used` resets to `0`, giving the row its full `BUDGET_PER_ROW` (2) again (initial + one automatic retry). |
| 3 | Are resets capped? | **Unlimited.** Each reset is an explicit operator keystroke; no new state field or reset counter is added (no schema change to `supervisor-state.json`). |
| 4 | Add a non-interactive flag? | **No.** Prompt only. A closed stdin / EOF at the prompt **declines**, so scripted and CI invocations get exactly today's exit-2 behavior with no flag surface. |

Scope note: line mode and the TUI both get the prompt (mirroring the existing
ASK `QuestionPause` seam). The final-report labels for the decline path stay
byte-identical to today.

## Current state (evidence)

All references are to `HEAD` (`7cd899f`). The baseline suite is green
(410 unit + 10 integration tests).

### The budget gate fires on resume with no interactive surface

- `run_row` (`src/supervise/mod.rs` ~line 1214) restores the persisted
  `runs_used` at entry: when the recovered state names the current row and
  its `last_outcome` is not `spawn-error`, `runs_used =
  p.runs_used.min(BUDGET_PER_ROW)` (~lines 1223–1237). `spawn-error` is the
  only budget-discount marker.
- The **budget gate** is the first check of every attempt loop iteration
  (~lines 1243–1246):

  ```rust
  if attempt >= BUDGET_PER_ROW && carried.is_none() {
      return spent_outcome(row, records, attempt, "budget");
  }
  ```

  On a re-start after exhaustion, `attempt == runs_used == 2 ≥ BUDGET_PER_ROW`
  and `carried.is_none()`, so the gate fires on the **first** iteration with
  an empty `records` list and no report lines — `supervise` immediately
  reports "budget exhausted" and exits 2.
- A second, separate budget exit sits at the bottom of the attempt loop
  (~lines 1646–1650): after a spent run, `if spent >= BUDGET_PER_ROW {
  return spent_outcome(row, records, spent, spent_kind); }` — this is the
  mid-session exhaustion (the 2nd attempt just failed). Both exits produce
  `RowOutcome::BudgetExhausted` via the `spent_outcome` helper
  (~line 614).
- `run_plan` (`src/supervise/mod.rs` ~line 1038) stops at the first
  non-`Done` outcome and returns it; `run_plan_interactive` (~line 1096) is
  the only interactive consumer. It dispatches on the pass's last outcome:
  row question (`last_question`, answer folds into the next pass), clean
  question (`last_clean_question`), else `return Some(result)` — the
  budget-exhausted pass therefore returns immediately, prints the final
  report, exit 2. There is **no seam** at the budget gate today.
- `recover_state` (`src/state.rs` ~line 138) returns the persisted state when
  the plan hash matches and the current row is not done; `runs_used` is
  preserved across invocations. A reset therefore only needs to **write a
  state with `runs_used = 0`** — `run_row`'s resume logic already clamps and
  honors it (a `last_outcome` of `"budget-reset"` is not `spawn-error`, so
  `runs_used = min(0, BUDGET_PER_ROW) = 0`).
- The interactive ASK seam to mirror: `QuestionPause` trait + `PauseOutcome`
  (`src/supervise/mod.rs` ~lines 1008–1034), implemented by
  `CliQuestionPause` in `src/main.rs` (~line 683) over the TUI modal
  (`Modal::Ask`, `src/tui/mod.rs` ~line 266) / the byte-exact stdout prompt
  (`ask_lines`, `src/ui.rs` ~line 864), and scripted by `FakePause` in
  `src/supervise/tests.rs` (~line 3949).

### Ripple surface of `RowOutcome::BudgetExhausted`

Replacing the variant (below) is compiler-enforced; today's consumers:
`describe_outcome` (`supervise/mod.rs` ~line 396), `outcome_label` /
`outcome_row_number` / `outcome_records` (`src/cli.rs` ~lines 637, 662, 677),
`format_final_report` test fixtures (`src/cli.rs` ~line 1804), and five
supervise tests (~lines 610, 838, 1358, 1423, 3726).

## Proposed design

### 1. Orchestration: `RowOutcome::BudgetChoice` replaces `BudgetExhausted`

- Rename `RowOutcome::BudgetExhausted { row, runs_used, last_outcome,
  records }` → `RowOutcome::BudgetChoice { row, runs_used, last_outcome,
  records }` (`src/supervise/mod.rs` ~line 147). The `spent_outcome` helper
  becomes `budget_choice(...)` with identical fields. The row can no longer
  auto-stop at the gate; the only way forward is operator intervention, which
  is exactly what the prompt provides.
- Both budget exits return it:
  - the **gate** (~line 1245) — resume-blocked case. `last_outcome` here
    comes from the **persisted state** (`p.last_outcome`, e.g. `failed`,
    `no-commit`, `near-miss`, `stopped`), falling back to `"budget"` (a
    defensive default — the gate can only fire from a persisted state with
    `runs_used = 2`, so the fallback is unreachable in practice); the gate
    writes **no state** (unchanged — the spent data is already on disk from
    the last session). **Banner note:** this changes the transient
    resume-blocked banner from today's hardcoded
    `row 3: stopped after 2 run(s) (budget)` to
    `row 3: stopped after 2 run(s) (failed)` — deliberate, because the
    persisted value is the truthful one and the prompt's context line uses
    the same field (see Risks).
  - the **post-spent** check (~line 1646) — live exhaustion; `last_outcome`
    is the true `spent_kind` (`failed` / `no-commit`), also no new state
    write (the spent attempt already saved itself).
- `run_plan`, `RunPlanResult`, `all_done`, `describe_outcome`, `outcome_label`,
  `outcome_row_number`, `outcome_records` all keep their **exact strings**
  (`"stopped after {runs_used} run(s) ({last_outcome})"` / `"stopped — budget
  exhausted"`), so the **final report** on the decline path is byte-identical
  to today. Only the variant name changes — and, at the resume-blocked gate
  only, the `last_outcome` **argument** the banner interpolates (see the gate
  bullet and Risks).

### 2. The budget prompt seam (mirror of `QuestionPause`)

```rust
/// Operator verdict at a budget-exhausted row.
pub enum BudgetDecision {
    /// Reset the row's budget (runs_used → 0) and resume.
    Reset,
    /// Stop as today: report + exit 2.
    Decline,
}

#[allow(async_fn_in_trait)]
pub trait BudgetPrompt {
    async fn prompt(&self, row_number: u64, runs_used: u32, last_outcome: &str)
        -> BudgetDecision;
}
```

- `run_plan_interactive` gains a `budget_prompt: &B` generic parameter (it is
  already generic over `P: QuestionPause`; there are exactly two call sites —
  `src/main.rs` ~line 641 and the `src/supervise/tests.rs` interactive suite).
- After the pass result, before the `return Some(result)`, dispatch on the
  last outcome: `BudgetChoice { row, runs_used, last_outcome, .. }` →
  `budget_prompt.prompt(row.number, runs_used, &last_outcome).await`.
  - `BudgetDecision::Reset` → write the reset state, then `continue` (re-run
    the pass):
    ```rust
    let persisted = (services.recover_state)();
    let st = state_file(services, &choice.row, 0, "budget-reset", persisted.as_ref(), None);
    (services.save_state)(&st);
    ```
    `state_file` recomputes `plan_hash` and preserves `adjudicated` from the
    persisted state; `agent_id`/`started_at` are `None` (no live worker). The
    next pass's `run_row` resume reads `runs_used = 0` and the dirty-WIP gate
    treats `"budget-reset"` as an owner marker (it is neither `"dirty"` nor
    `"spawn-error"`), so an owned dirty tree resumes with the resuming note.
  - `BudgetDecision::Decline` → `return Some(result)` — the final report and
    exit 2 render exactly as today.
- **Carry semantics are unchanged.** A carried answer (from `--answer` or a
  prior answered question) survives across passes "for the continuation of the
  SAME row only" — it deliberately re-folds into post-reset attempts
  (consistent with the existing question-pause carry). The gate already never
  fires while `carried.is_some()`, so `supervise --answer X` on an
  exhausted row still runs immediately (existing contract).
- **Within-session cadence:** each exhaustion yields exactly one blocking
  prompt; a "yes" resets and the row runs its full budget live again. If it
  exhausts again the operator is asked again (unlimited per Q1/Q3). Decline /
  EOF / stop ends the run as today; there is no automatic prompt loop (the
  prompt is blocking).

### 3. CLI: `CliBudgetPrompt` (TUI modal + line-mode prompt)

`src/main.rs` gains `CliBudgetPrompt`, built from the same parts as
`CliQuestionPause` (`tui_state`, `control`, `tui_active`, `status`).

- **TUI mode:** open `Modal::Budget { row, runs_used, last_outcome }`, await
  `await_modal_outcome`, map:
  - `ModalOutcome::BudgetReset` → `BudgetDecision::Reset`;
  - `ModalOutcome::BudgetDecline` → `BudgetDecision::Decline`;
  - `ModalOutcome::Stop` → set `stop_requested` (already set by the input
    task's line-command arm) and `Decline`;
  - `None` (modal closed without an outcome) → `Decline`.
- **Line mode:** print the prompt block (below), then loop reading a line:
  - `y` / `yes` (case-insensitive) → `Reset`;
  - `stop` → flip `stop_requested`, `Decline`;
  - `status` → reprint the live status (`(self.status)()`) and **re-prompt**;
  - `restart` → **re-prompt** (no-op: no worker is running at a budget gate;
    parity with the TUI's `Keep(InvalidReply)` arm);
  - any other non-empty line → **re-prompt** (TUI parity: invalid input keeps
    the prompt open, so a typo like `n` never ends the run; same loop the
    `status` arm already uses);
  - blank line / EOF → `Decline` (EOF and blank give today's bytes on stderr
    + exit 2). Only these close the loop.
  - The prompt text (new `budget_choice_lines` helper beside `ask_lines` in
    `src/ui.rs`, unit-pinned):
    ```text
    ── budget exhausted ──
    row 3 · 2 run(s) used · last outcome: failed
    reset the budget for row 3 and resume? [y]es / [Enter] to stop
    ```
- **EOF / closed stdin at re-start:** the line-mode read returns `None` →
  `Decline` → today's report + exit 2. Scripted/CI behavior is unchanged.
- Commit 1 wires `CliBudgetPrompt` with the line-mode prompt working and the
  **TUI branch temporarily returning `Decline`** (documented; the TUI modal
  lands in commit 2). This keeps every commit green and TUI behavior
  unchanged until the modal exists.

### 4. TUI: `Modal::Budget` (commit 2)

`src/tui/mod.rs` (stays supervise-free — no new import of `supervise`):

- `Modal::Budget { row: u64, runs_used: u32, last_outcome: String }`
  (~line 266), plus two outcome variants next to `Stop`/`Restart`:
  `ModalOutcome::BudgetReset` and `ModalOutcome::BudgetDecline` (~line 278).
- `dispatch_modal_line` / `dispatch_modal_submit` (~lines 315, 381): `y` /
  `yes` → `Close(BudgetReset)`; `stop` → `Close(Stop)` (existing semantics);
  `status` → `Keep(Status)`; `restart` → `Keep(InvalidReply)` (no-op with a
  hint — the budget gate has no worker to restart; line mode re-prompts on
  it, so both surfaces keep the prompt open); empty submit →
  `Close(BudgetDecline)`; anything else → `Keep(InvalidReply)`.
- `eof_outcome` (~line 415): `Modal::Budget(_) => BudgetDecline` (closed
  stdin declines — parity with line mode).
- `modal_box` (~line 493): new arm rendering the context rows + a prompt
  label (`reset? [y]es / [Enter] stop>`), styled like the ASK box.

### 5. Docs (commit 3)

- `docs/ARCHITECTURE.md`: Contract 4 budget section (~lines 130–175) —
  replace "stop + report" on exhaustion with the prompt flow (gate → ask →
  reset `runs_used=0` with the `budget-reset` marker / decline → report +
  exit 2); outcome-kind list (~line 246) and the work-outstanding summary
  (~line 524) gain `budget-choice`; the dirty-WIP section notes the
  `budget-reset` marker is an owner marker (like `running`/`failed`).
- `README.md`: supervise table row (~line 48), budget narrative (~line 165),
  and the exit-2 note (~line 342).

### Behavior matrix

| Situation | Today | After |
| --- | --- | --- |
| Re-start; next row's state has `runs_used = 2` | report + exit 2 | **prompt** → reset + resume, or Decline = today's final report (the transient banner's `last_outcome` arg becomes the persisted value; see Risks) |
| 2nd attempt fails live | report + exit 2 | **prompt** → reset + resume (same row, fresh `records`), or Decline = today's bytes |
| Row exhausts after a reset | — | prompt again (operator-driven, unlimited) |
| `supervise --answer X` on exhausted row | runs immediately (answer bypasses gate) | gate bypass unchanged; a failing answer run still hits the mid-session prompt (interactive: prompt; scripted: closed stdin declines to today's bytes) |
| EOF / closed stdin / blank at the prompt | n/a | Decline = today's bytes + exit 2 |
| `stop` at the prompt | n/a | stoppish Decline; final report reads "stopped — budget exhausted", exit 2 |
| `status` / `restart` / garbage at the prompt | n/a | re-prompt (both modes); only blank, EOF, or `stop` end the run |
| `step N` / `supervise --row N` on exhausted row | report + exit 2 | same prompt flow (shared `cmd_supervise` driver) |
| Other outcomes (`spawn-error`, `question`, `near-miss`, `stopped`) | unchanged | unchanged (`spawn-error` keeps its budget discount) |

## Risks and pitfalls

- **The resume-blocked banner's `last_outcome` argument changes** — today's
  gate hardcodes `"budget"`, so a re-start banner reads
  `row 3: stopped after 2 run(s) (budget)`; the gate now surfaces the
  persisted value, so it reads `row 3: stopped after 2 run(s) (failed)`.
  Deliberate (`describe_outcome`'s string is unchanged; only the interpolated
  argument is truthful now — the prompt's context line uses the same field,
  so hardcoding `"budget"` would make banner and prompt disagree). This is
  the **only** pre-existing-output byte change: the final report's label is a
  fixed string and the resume-blocked `records` list is empty. A persisted
  `"running"` (spawn-time save after a hard kill) can surface verbatim —
  harmless, same mechanism. The `"budget"` fallback at the gate is
  unreachable by construction (the gate fires only after restoring
  `runs_used = 2` from a `Some` persisted state) and stays as a defensive
  default.
- **`records` reset after a reset** — post-reset attempts replace pre-reset
  records in `run_row`'s fresh list; the final report shows only post-reset
  attempts (the failures were already on screen). Same lifetime contract as
  an answered question pause (review F2 parity).
- **Best-effort reset write** — `save_state_file` swallows write errors; if
  the reset write fails, the next pass re-hits the gate and prompts again
  (each prompt is blocking, so this degrades to "operator keeps being asked",
  never an automatic loop; blank/EOF declines out). No new durability
  guarantee introduced.
- **`last_outcome` sources differ** — resume-blocked gate uses the persisted
  `last_outcome`, live exhaustion uses `spent_kind`. Both feed the same
  context line; the variant name changes but the field semantics are
  identical to today's `BudgetExhausted` in each location.
- **Owner-marker semantics of `budget-reset`** — it must NOT be named
  `dirty`/`spawn-error` or the dirty-WIP gate would lose ownership; the
  `budget-reset` marker is a deliberate new live-outcome string (only
  consumed by the resume clamp, which treats non-`spawn-error` as spent).
- **A stream of `y`s is honored** — `yes | pi-plan supervise` on an
  exhausted row cycles reset → run → exhaust → prompt indefinitely
  (unbounded worker spawns + git churn), where today it exits 2 at once.
  The prompt is blocking per keystroke, so only an explicit stream loops
  it; the docs commit records this stdin semantics (Q1/Q3 decided
  unlimited operator resets; the pipe is the only way to automate them).
- **`--answer X` prompts mid-session too** — the gate bypass is unchanged,
  but a failing answer run on an exhausted row lands on the post-spent
  check and now prompts: interactive terminals get a prompt where they
  used to exit; closed stdin declines to today's bytes.
- **A carried answer cannot be cleared at the budget prompt** — Reset
  re-folds the same carried answer (`--answer` or a prior answered
  question) into every post-reset attempt; the operator can only decline
  or complete the row. Locked with the question-pause carry contract
  (review F2 parity).
- **Exhaustive matches** — the rename is compiler-enforced (every
  `BudgetExhausted` arm updates); grep for `BudgetExhausted` after commit 1
  must return no hits outside docs.
- **No `unwrap()`/`expect()`/`panic!()` in application logic**; the seam uses
  `#[allow(async_fn_in_trait)]` exactly like `QuestionPause` (never used
  through dynamic dispatch).

## Test plan

Unit (`src/supervise/tests.rs`, via the existing `FakeWorkerPort` /
`SuperviseServices` harness):

1. `run_row_spends_both_runs_and_stops_at_the_budget` (rename
   `...and_asks_at_the_budget`) — two failed attempts → `BudgetChoice {
   runs_used: 2, last_outcome: "failed", records: 2 }`, two spawns, and the
   gate wrote no extra state.
2. **New** resume-blocked test — `recover_state` returns a state with
   `runs_used = 2, last_outcome = "failed"` for the row; `run_row` returns
   `BudgetChoice` on the first iteration with **zero** spawns and no new
   save.
3. **New** gate `last_outcome` provenance — persisted `near-miss` (and
   `stopped`) with `runs_used = 2` surface in `BudgetChoice.last_outcome`.
4. Interactive driver (`FakeBudgetPrompt`, scripted queue like `FakePause`):
   - `Reset` → driver writes a state with `runs_used = 0`, `last_outcome =
     "budget-reset"`, `current_row` intact, `adjudicated` preserved; the
     pass re-runs (watch: the row spawns again with the full budget).
   - `Decline` → `Some(result)` whose last outcome is `BudgetChoice`
     (final-report label byte-identical via `outcome_label`).
   - Prompt skipped for `Done`/question/near-miss/stopped passes (counter
     stays 0 — the seam only fires on `BudgetChoice`).
   - A carried answer re-folds into the post-reset pass (carry contract).
5. `src/ui.rs`: `budget_choice_lines` renders the pinned prompt block.
6. Line-mode prompt loop (`CliBudgetPrompt`, scripted stdin lines): `y` →
   `Reset`; `stop` → `Decline` with `stop_requested` set; `status` reprints
   and re-prompts; `restart` and garbage (`n`, `banana`) **re-prompt** (the
   prompt-line count grows, the run never ends); blank / EOF → `Decline`.
7. TUI dispatch (`src/tui/tests.rs`): `dispatch_modal_line` /
   `dispatch_modal_submit` for `Modal::Budget` — `y`/`yes` → `BudgetReset`;
   `stop` → `Stop`; `status` → keep + note; `restart` → `Keep(InvalidReply)`;
   empty submit → `BudgetDecline`; garbage → `InvalidReply`.
8. `eof_outcome(Modal::Budget) == BudgetDecline`.
9. `modal_box` renders the context rows + prompt label (width-bound formats).
10. `CliBudgetPrompt`'s TUI mapping (modal outcome → `BudgetDecision` + flag
    flips) is extracted into a **pure function** and unit-tested: `Stop` →
    `stop_requested` set + `Decline`; closed-modal `None` → `Decline`;
    `BudgetReset` / `BudgetDecline` map 1:1.
11. **New** dirty-WIP owner marker — a recovered state with
    `last_outcome = "budget-reset"`, `runs_used = 0`, `current_row` intact
    resumes through the dirty-WIP gate as an owned row (resuming note; never
    routed to the clean-worktree agent).
12. **New** property test (`proptest`, already used in `src/ui.rs`):
    state-machine invariant — for any valid recovered state, `run_row` hits
    the budget gate iff `attempt ≥ BUDGET_PER_ROW && carried.is_none()`, and
    every reset write satisfies exactly
    `{runs_used: 0, last_outcome: "budget-reset"}`.
13. Integration — line mode's exhausted-row flow is covered end-to-end by the
    acceptance-e2e manual gate (see below): prompt on stdout, `y` resumes
    (spawn count increments, `runs_used` state file reads 0), EOF exits 2
    with the byte-identical final report. No full-session harness exists in
    `tests/` (`rpc_fake_pi` covers the RPC half, `tui_backend` only the
    terminal backend), so no new pty harness is built — the scripted
    line-mode loop (item 6) and the driver tests pin the logic headlessly,
    and the TUI session stays in manual verification.

Per commit: `cargo test` green (410+), `cargo fmt --check` clean, `cargo
clippy --all-targets --all-features -- -D warnings` clean.

## Acceptance criteria (end state)

1. A re-start whose next unmatched row has a spent budget (any spent
   `last_outcome` with `runs_used = 2`) prompts instead of exiting.
2. A live mid-session exhaustion (2nd attempt fails) prompts instead of
   exiting.
3. `y`/`yes` at the prompt resets `runs_used` to 0 (state file shows
   `last_outcome: budget-reset`, agent fields cleared, `adjudicated`
   preserved) and the row resumes with its full 2-run budget.
4. Declining (blank, EOF, or `stop`) prints the byte-identical final report
   and exits 2 (the transient resume-blocked banner's `last_outcome`
   argument is the persisted value — see Risks).
5. Prompt fires at most once per exhaustion; unlimited resets are
   operator-driven; no automatic prompt loop.
6. `supervise --answer X` on an exhausted row still runs immediately.
7. `spawn-error` keeps its full-budget discount (never prompts).
8. TUI mode shows a bottom-anchored budget modal; line mode shows the prompt
   block and stays byte-exact for all pre-existing output except the
   resume-blocked banner's `last_outcome` argument (deliberate; Risks).
9. `grep -rn "BudgetExhausted" src/` returns nothing (docs may keep prose).
10. Per commit: `cargo test` green (410+), `cargo fmt --check` clean,
    `cargo clippy --all-targets --all-features -- -D warnings` clean.
11. Non-`y`/`yes` non-empty input at the prompt (`status`, `restart`, any
    garbage) re-prompts in both modes and never ends the run; only blank,
    EOF, or `stop` decline.

**Manual verification** (mirrors `docs/acceptance-e2e.md` style): run
`pi-plan supervise` against a TODO whose first row already exhausted its
budget (delete the row's commit, keep `supervisor-state.json` with
`runs_used: 2`) — confirm the prompt appears, `y` resumes from run 1 of 2,
`Enter` exits 2 with the standard report; run a live row that fails twice —
confirm the prompt appears after the second failure; close the terminal's
stdin at the prompt (or run with `< /dev/null`) — confirm it exits 2 without
hanging.

## Commit-by-commit (exact messages, in order)

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `feat: ask the operator to reset an exhausted row budget and resume` | orchestration + seam + CLI line-mode prompt | `src/supervise/mod.rs`: `BudgetDecision` + `BudgetPrompt`, `RowOutcome::BudgetChoice` replacing `BudgetExhausted` (both exits), `spent_outcome` → `budget_choice`, `run_plan_interactive` gains `budget_prompt: &B` + the BudgetChoice dispatch (Reset → `state_file(..., 0, "budget-reset", ...)` + `continue`; Decline → `Some(result)`); `src/ui.rs`: `budget_choice_lines`; `src/main.rs`: `CliBudgetPrompt` (line mode live; TUI branch returns `Decline` until commit 2); update every `BudgetExhausted` consumer (`describe_outcome`, `outcome_label`/`outcome_row_number`/`outcome_records` in `src/cli.rs`, `format_final_report` fixtures) | unit: budget test → `BudgetChoice`; resume-blocked `run_row` returns `BudgetChoice` with zero spawns and no extra save; gate `last_outcome` provenance (`near-miss`/`stopped` persisted states); `FakeBudgetPrompt` driver tests (Reset writes `runs_used=0` + `budget-reset` and re-runs the pass; Decline returns the `BudgetChoice` result; seam skipped on every non-budget outcome; carried answer re-folds); line-mode loop tests (garbage/`restart` re-prompt, `stop`/`status`/blank/EOF); dirty-WIP `budget-reset` owner-marker test; proptest reset-invariant; `budget_choice_lines` pinned |
| 2 | `feat: render the budget-reset prompt in the supervise TUI` | TUI modal | `src/tui/mod.rs`: `Modal::Budget { row, runs_used, last_outcome }`, `ModalOutcome::BudgetReset`/`BudgetDecline`, dispatch/submit/EOF arms, `modal_box` arm; `src/main.rs`: `CliBudgetPrompt` TUI branch opens the modal and maps outcomes | TUI unit: dispatch (`y`/`yes`/`stop`/`status`/blank/garbage), `eof_outcome`, `modal_box` rendering; pure `CliBudgetPrompt` TUI-mapping unit tests (extracted fn: `Stop` → `stop_requested` + `Decline`, closed-modal `None` → `Decline`, `BudgetReset`/`BudgetDecline` 1:1); the live modal session stays in the acceptance-e2e manual gate |
| 3 | `docs: document the budget-reset prompt and its state marker` | docs | `docs/ARCHITECTURE.md` (Contract 4 budget flow, outcome-kind list, work-outstanding summary, dirty-WIP owner markers); `README.md` (supervise row, budget narrative, exit-2 note) | `cargo fmt --check` (doc-only), suite green |
