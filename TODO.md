# Implementation Plan: Ask the operator to reset an exhausted row budget and resume

Source: `docs/research/plan-budget-reset-prompt.md`

Today, when a row's budget is exhausted, `supervise` (and `step` /
`supervise --row N`, which share the `cmd_supervise` driver) exits with a
final report (exit 2). Re-starting immediately re-hits the budget gate
(`run_row`'s first attempt-loop check, `src/supervise/mod.rs` ~line 1245,
when the persisted `runs_used == 2`), reports "budget exhausted" again, and
exits — the only way to continue the row is to edit the state file.

This plan makes the gate yield an interactive choice instead: `RowOutcome::
BudgetChoice` replaces `BudgetExhausted` (compiler-enforced rename), and
`run_plan_interactive` gains a `BudgetPrompt` seam (mirror of `QuestionPause`)
that asks the operator to **reset the row's budget** (`runs_used` → 0, full
2-run budget, `budget-reset` marker) and resume, or decline and get today's
stop-with-report-plus-exit-2 behavior byte-for-byte. The prompt fires both on
re-start and live mid-session (second attempt fails); resets are unlimited and
operator-driven; EOF / closed stdin / blank declines. Line mode and the TUI
both get the prompt (`CliBudgetPrompt` over the stdout prompt block /
`Modal::Budget`). Three commits.

The commit messages in the table are **exact** — taken verbatim from the source
plan. Workflow per step: implement → `cargo test` (410+ tests) → `cargo fmt
--check` → `cargo clippy --all-targets --all-features -- -D warnings` → commit
with the table's message → stop. One step at a time.

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `feat: ask the operator to reset an exhausted row budget and resume` | orchestration + seam + CLI line-mode prompt | `src/supervise/mod.rs`: `BudgetDecision` + `BudgetPrompt`, `RowOutcome::BudgetChoice` replacing `BudgetExhausted` (both exits — gate ~1245 and post-spent ~1648), `spent_outcome` → `budget_choice`, `run_plan_interactive` gains `budget_prompt: &B` + the BudgetChoice dispatch (Reset → `state_file(..., 0, "budget-reset", ...)` + `continue`; Decline → `Some(result)`); `src/ui.rs`: `budget_choice_lines`; `src/main.rs`: `CliBudgetPrompt` (line mode live; TUI branch returns `Decline` until commit 2); update every `BudgetExhausted` consumer (`describe_outcome`, `outcome_label`/`outcome_row_number`/`outcome_records` in `src/cli.rs`, `format_final_report` fixtures) | unit: budget test → `BudgetChoice`; resume-blocked `run_row` returns `BudgetChoice` with zero spawns and no extra save; gate `last_outcome` provenance (`near-miss`/`stopped` persisted states); `FakeBudgetPrompt` driver tests (Reset writes `runs_used=0` + `budget-reset` and re-runs the pass; Decline returns the `BudgetChoice` result; seam skipped on every non-budget outcome; carried answer re-folds); line-mode loop tests (garbage/`restart` re-prompt, `stop`/`status`/blank/EOF); dirty-WIP `budget-reset` owner-marker test; proptest reset-invariant; `budget_choice_lines` pinned |
| 2 | `feat: render the budget-reset prompt in the supervise TUI` | TUI modal | `src/tui/mod.rs`: `Modal::Budget { row, runs_used, last_outcome }`, `ModalOutcome::BudgetReset`/`BudgetDecline`, dispatch/submit/EOF arms, `modal_box` arm; `src/main.rs`: `CliBudgetPrompt` TUI branch opens the modal and maps outcomes | unit: dispatch (`y`/`yes`/`stop`/`status`/blank/garbage), `eof_outcome`, `modal_box` rendering; pure `CliBudgetPrompt` TUI-mapping unit tests (extracted fn: `Stop` → `stop_requested` + `Decline`, closed-modal `None` → `Decline`, `BudgetReset`/`BudgetDecline` 1:1); modal session stays in the acceptance-e2e manual gate |
| 3 | `docs: document the budget-reset prompt and its state marker` | docs | `docs/ARCHITECTURE.md` (Contract 4 budget flow, outcome-kind list, work-outstanding summary, dirty-WIP owner markers); `README.md` (supervise row, budget narrative, exit-2 note) | `cargo fmt --check` (doc-only), suite green |

## Locked decisions (from the source plan)

- **When the prompt fires:** also mid-session — whenever the budget gate would
  block a row, both at (re)start on a spent row **and** live when the second
  attempt fails. No asked-this-row latch: every prompt is a blocking operator
  question, so an automatic prompt loop is impossible by construction.
- **Budget restored:** full — `runs_used` resets to `0`, giving the row its
  full `BUDGET_PER_ROW` (2) again (initial + one automatic retry).
- **Resets capped:** no — unlimited, each an explicit keystroke; no new state
  field or reset counter (no schema change to `supervisor-state.json`).
- **Non-interactive flag:** no — prompt only. Closed stdin / EOF at the prompt
  **declines**, so scripted and CI invocations get exactly today's exit-2
  behavior with no flag surface.
- **Scope:** line mode and the TUI both get the prompt (mirroring the existing
  ASK `QuestionPause` seam). The final-report labels for the decline path stay
  byte-identical to today.
- **The one deliberate byte change:** the transient resume-blocked banner's
  `last_outcome` argument — today's gate hardcodes `"budget"`
  (`row 3: stopped after 2 run(s) (budget)`); it now surfaces the persisted
  value (`row 3: stopped after 2 run(s) (failed)`), because the prompt's
  context line uses the same field and hardcoding would make banner and prompt
  disagree. `describe_outcome`'s string is unchanged; the final report is
  untouched (Resume Risks).

## Invariants to preserve

- `QuestionPause` / `PauseOutcome` and the ASK flow are untouched; `BudgetPrompt`
  mirrors the same seam (`#[allow(async_fn_in_trait)]`, never dynamic dispatch).
- The final report (decline path) is byte-identical: `outcome_label` /
  `outcome_row_number` / `outcome_records` strings (`"stopped after {runs_used}
  run(s) ({last_outcome})"` / `"stopped — budget exhausted"`) do not change —
  only the variant name does.
- `spawn-error` keeps its full-budget discount and never prompts.
- Carry semantics unchanged: a carried answer (`--answer` / prior answered
  question) survives across passes for the same row only and deliberately
  re-folds into post-reset attempts; the gate already never fires while
  `carried.is_some()`, so `supervise --answer X` on an exhausted row still runs
  immediately.
- No schema change: the reset write reuses `state_file`, recomputes `plan_hash`,
  preserves `adjudicated`, clears `agent_id`/`started_at`; `budget-reset` is
  deliberately **not** `dirty`/`spawn-error`, so the dirty-WIP gate keeps
  ownership (owned dirty tree resumes with the resuming note).
- No `unsafe`, no `unwrap()`/`expect()`/`panic!()` in application logic.
- The TUI stays supervise-free: `Modal::Budget` carries plain data
  (`{ row: u64, runs_used: u32, last_outcome: String }`), no `supervise` import.
- Per commit: `cargo test` green (410+ tests), `cargo fmt --check` clean,
  `cargo clippy --all-targets --all-features -- -D warnings` clean.

## Step notes

### Commit 1 (orchestration + seam + CLI line-mode prompt)

- `RowOutcome::BudgetChoice { row, runs_used, last_outcome, records }` replaces
  `BudgetExhausted` (`src/supervise/mod.rs` ~line 147); `spent_outcome` becomes
  `budget_choice(...)` with identical fields. Both budget exits return it:
  - the **gate** (~line 1245) — resume-blocked case; `last_outcome` from the
    persisted state (`p.last_outcome`), defensive `"budget"` fallback (gate can
    only fire from a persisted state with `runs_used = 2`); writes **no state**
    (spent data already on disk). Banner `last_outcome` argument becomes the
    persisted value (the only pre-existing-output byte change; see Risks).
  - the **post-spent check** (~line 1648) — live exhaustion; `last_outcome` is
    the true `spent_kind`; also no new state write.
- `BudgetDecision { Reset, Decline }`; `BudgetPrompt::prompt(row_number,
  runs_used, last_outcome)`. `run_plan_interactive` gains `budget_prompt: &B`
  (two call sites: `src/main.rs` ~641, `src/supervise/tests.rs`); dispatch on
  the pass's last outcome before `return Some(result)`:
  - `Reset` → write the reset state (`state_file(services, &choice.row, 0,
    "budget-reset", persisted.as_ref(), None)`, `save_state`) then `continue`
    (re-run the pass; resume reads `runs_used = 0`).
  - `Decline` → `return Some(result)` — report + exit 2 exactly as today.
- `src/ui.rs`: `budget_choice_lines` beside `ask_lines` (~line 864), pinned:
  `── budget exhausted ──` / `row 3 · 2 run(s) used · last outcome: failed` /
  `reset the budget for row 3 and resume? [y]es / [Enter] to stop`.
- `src/main.rs`: `CliBudgetPrompt` (line mode live). Loop reads a line: `y`/
  `yes` → `Reset`; `stop` → flip `stop_requested`, `Decline`; `status` →
  reprint live status + re-prompt; `restart`/garbage → re-prompt; blank /
  EOF (`None`) → `Decline` (today's bytes + exit 2). **TUI branch temporarily
  returns `Decline`** (documented) so every commit stays green and TUI behavior
  is unchanged until commit 2.
- Every `BudgetExhausted` consumer updates: `describe_outcome` (~396),
  `outcome_label`/`outcome_row_number`/`outcome_records` (`src/cli.rs` 637/662/
  677), `format_final_report` fixtures (~1804), five supervise tests
  (~610, 838, 1358, 1423, 3726). grep for `BudgetExhausted` after this commit:
  no hits outside docs.

### Commit 2 (TUI modal)

- `src/tui/mod.rs` (~line 266): `Modal::Budget { row, runs_used, last_outcome }`
  and `ModalOutcome::BudgetReset` / `ModalOutcome::BudgetDecline` (~line 278).
- `dispatch_modal_line` / `dispatch_modal_submit` (~315, 381): `y`/`yes` →
  `Close(BudgetReset)`; `stop` → `Close(Stop)`; `status` → `Keep(Status)`;
  `restart` → `Keep(InvalidReply)` (no worker at a budget gate; re-prompt
  parity with line mode); empty submit → `Close(BudgetDecline)`; anything else
  → `Keep(InvalidReply)`.
- `eof_outcome` (~415): `Modal::Budget(_) => BudgetDecline`.
- `modal_box` (~493): new arm rendering the context rows + prompt label
  (`reset? [y]es / [Enter] stop>`), styled like the ASK box.
- `src/main.rs`: TUI branch opens the modal, awaits `await_modal_outcome`,
  maps `BudgetReset` → `Reset`, `BudgetDecline` → `Decline`, `Stop` →
  `stop_requested` + `Decline`, `None` → `Decline`. The mapping is extracted as
  a **pure function** and unit-tested.

### Commit 3 (docs)

- `docs/ARCHITECTURE.md`: Contract 4 budget section (~130–175) — replace
  "stop + report" with the prompt flow (gate → ask → reset `runs_used=0` with
  the `budget-reset` marker / decline → report + exit 2); outcome-kind list
  (~246) and work-outstanding summary (~524) gain `budget-choice`; dirty-WIP
  section notes the `budget-reset` marker is an owner marker.
- `README.md`: supervise table row (~48), budget narrative (~165), exit-2 note
  (~342).

## Pitfalls (from the source plan)

- **The resume-blocked banner's `last_outcome` argument changes** — deliberate
  and the **only** pre-existing-output byte change (final-report label is fixed
  and the resume-blocked `records` list is empty). Persisted `"running"`
  (spawn-time save after a hard kill) can surface verbatim — harmless, same
  mechanism. The `"budget"` fallback is unreachable in practice but stays.
- **`records` reset after a reset** — post-reset attempts replace pre-reset
  records in `run_row`'s fresh list; the final report shows only post-reset
  attempts. Same lifetime contract as an answered question pause.
- **Best-effort reset write** — `save_state_file` swallows errors; a failed
  write re-hits the gate next pass and prompts again (each prompt blocking →
  degrades to "operator keeps being asked", never an automatic loop).
- **Owner-marker semantics of `budget-reset`** — must NOT be `dirty`/
  `spawn-error` or the dirty-WIP gate would lose ownership; only consumed by
  the resume clamp (treats non-`spawn-error` as spent).
- **`yes | pi-plan supervise` loops indefinitely** — reset → run → exhaust →
  prompt (unbounded worker spawns + git churn); today it exits 2 at once. The
  prompt is blocking per keystroke, so only an explicit pipe loops it; docs
  commit records the stdin semantics (Q1/Q3: unlimited operator resets).
- **`--answer X` prompts mid-session too** — gate bypass unchanged, but a
  failing answer run on an exhausted row lands on the post-spent check and
  prompts: interactive terminals get a prompt where they used to exit; closed
  stdin declines to today's bytes.
- **A carried answer cannot be cleared at the budget prompt** — Reset re-folds
  the same carried answer into every post-reset attempt; the operator can only
  decline or complete the row. Question-pause carry parity.
- **Exhaustive matches** — the rename is compiler-enforced; grep for
  `BudgetExhausted` after commit 1 must return no hits outside docs.
- **No `unwrap()`/`expect()`/`panic!()` in application logic**; the seam uses
  `#[allow(async_fn_in_trait)]` exactly like `QuestionPause`.

## Acceptance criteria (end state)

1. A re-start whose next unmatched row has a spent budget (any spent
   `last_outcome` with `runs_used = 2`) prompts instead of exiting.
2. A live mid-session exhaustion (2nd attempt fails) prompts instead of
   exiting.
3. `y`/`yes` at the prompt resets `runs_used` to 0 (state file shows
   `last_outcome: budget-reset`, agent fields cleared, `adjudicated`
   preserved) and the row resumes with its full 2-run budget.
4. Declining (blank, EOF, or `stop`) prints the byte-identical final report
   and exits 2 (the transient resume-blocked banner's `last_outcome` argument
   is the persisted value — see Risks).
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