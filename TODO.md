# Implementation Plan: Resolve TODO-row holes with an authoritative verifier

Source: `docs/research/plan-hole-detection-and-verification.md`

Adds a deterministic **hole** detector (C) for TODO rows that are unmatched
while a later row is git-matched, a deterministic **evidence bundle** (B, the
subject-drift report), one contained read-only **verifier** LLM step that may
only skip or escalate, a **verified** provenance record with a verdict cache,
an operator **unrecognised-row** decision (`m`/`w`/`r`/`s`), a guarded
deterministic **reword**, a primed row worker for `w`, and the surfaces/docs
for all of it. Contract 2's match tiers are unchanged.

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit with the table's
message → stop. One step at a time.

**Sequence note.** The source plan's 10-commit table is split and reordered
here (16 steps, decided 2026-11-22) so each commit introduces its types before
its consumers and leaves the tree compiling: the pure hole/evidence/reword
modules land first, then state and the audit log, then the verifier, then the
outcome model and label, then the pre-pass/cache, then the operator surfaces,
then the primed worker, then reporting, then docs.

## Steps

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | feat: add deterministic TODO-hole detection | Done set + hole predicate | `src/hole.rs` (new): `git_done`, `is_done(row, subjects, adjudicated, verified)`, `detect_holes` returning ascending row numbers (document order tiebreak, `UNNUMBERED_ROW_NUMBER` sorts last, successor evidence must be `git_done`); `src/git.rs`: `CommitRef { hash, subject, authored_at }` + `GitCommands::history()` (`git log --format=%H%x1f%aI%x1f%s`) with `subjects()` on top; `GitFacts::history` in `src/supervise/mod.rs` + `FakeGit`; `pub mod hole` in `src/lib.rs` | unit, property-based |
| 2 | feat: diff a row subject against git history | Subject-drift report (B) | `src/hole.rs`: `diff_subject(planned, history) -> SubjectDiff` with `best_subject`, `best_commit`, `tier`, `ratio` (2dp), `extra_tokens`, `missing_tokens`; normalization and tokenization reuse `git::normalize_message` rules; the tier comes from `git::match_tier` (Contract 2 untouched) | unit |
| 3 | feat: build the deterministic hole evidence bundle | Evidence bundle + triage | `src/hole.rs`: `EvidenceBundle`, `CommitStat`, `deliverable_paths` (backticked tokens containing `/`, `dir/{a,b}` expansion, non-path tokens and zero tokens → inconclusive), ≤3 candidate commits via `git show --stat` (2 KiB each), `best_patch` via `git show --patch` (8 KiB, UTF-8-safe), per-section then 24 KiB whole-bundle cap with `truncated` in fixed order, `render_bundle`, `triage` (`Impossible` / `PlainlyUnimplemented` / `RunVerifier`); `src/git.rs`: best-effort `show_stat`/`show_patch` | unit |
| 4 | feat: persist verified-row provenance across restarts | State schema + merge-on-save | `src/state.rs`: `VerifiedRow`, `CachedVerdict`, `verified`/`verifiedCache` fields, elementwise coercion (absent → empty, wrong types rejected like `adjudicated`), 64-entry cache bound (LRU by `at`, row, head); additive provenance merge on save under a process-wide lock (plan-hash mismatch keeps provenance); `src/main.rs`: `save_state` reads-modifies-writes; `state_file()` stops deriving provenance from the recovered snapshot | unit |
| 5 | feat: append hole decisions to the audit log | Decisions log | `src/storage.rs`: `DECISIONS_FILE_NAME = "decisions.jsonl"`, `DecisionRecord` (`v:1`, kinds `hole-verified`/`hole-escalated`/`verifier-failed`/`operator-*`/`reword-failed`), `append_decision` with the temp-then-rename, full-file-rewrite, best-effort discipline of `append_worker_stats` | unit |
| 6 | feat: render the verifier prompt and parse its verdict | Verifier contract | `prompts/verifier-persona.md` (new, adversarial rubric, inline fallback); `src/prompt.rs`: `render_verifier_prompt(&EvidenceBundle)` with the fenced `SUPERVISOR-PROVIDED EVIDENCE` block, the "text claims in files are not evidence" rule, no-edit/no-commit/no-plan-source rules, and the mandatory `PI_HOLE_VERDICT`/`PI_HOLE_ROW`/`PI_HOLE_EVIDENCE`/`PI_HOLE_MISSING`/`PI_HOLE_EXPLANATION` terminal block; `src/hole.rs`: `Verdict`, `parse_verdict(text, row)` (duplicate/unparseable/mismatched markers → `Unparseable`, fenced fake markers ignored); `src/cli.rs`: verifier persona resolution with fallback | unit |
| 7 | feat: run a read-only verifier for a hole row | Verifier spawn | `src/supervise/mod.rs`: `VerifierReport`, `run_verifier(services, &bundle)` mirroring `run_row`'s spawn→await→abort→parse path; spawn opts `--name pi-plan-verify-<row>`, `--tools read,grep,find,ls`, no skills, verifier persona, `max_turns`/`turn_timeout` as `WorkerSpawnOpts` fields; `NotRun` on spawn error, wall timeout, turn ceiling, ASK, port error; `src/config.rs`: `VerifierConfig` (`model`, `maxTurns` 20, `turnTimeoutMs` 600000) + model precedence verifier → worker → `DEFAULT_MODEL`; `src/storage.rs`: `WorkerStatsRecord` `v:2` with `kind: "run" \| "verification"` and v1 tolerance; `src/main.rs`: wire config and persona | unit |
| 8 | feat: guard and offer a commit-subject reword | Reword guards | `src/git.rs`: `GitCommands::{rev_parse, config_get, rev_list_merges, checkout, commit_amend, rebase_onto, branch_force, reset_hard, show_tree_of}` on `GitRunner`; `src/hole.rs`: `RewordRunner`, `RewordOffer`, `offer_reword` applying all eight guards (clean worktree, real branch, not `main`/`master`, no upstream, no merges in range, target not a merge, target is the row's best candidate, single-line planned message) and returning `None` plus the guard reason | unit |
| 9 | feat: reword a drifted commit subject without an editor | Reword sequence | `src/hole.rs`: `reword` (record head, detach at target, amend with planned subject, verify tree equality, `rebase --onto`, restore branch, verify the planned subject matches) and the failure path (`rebase --abort`, checkout branch, `reset --hard` recorded head, stderr in the report); `RewordOutcome::{Reworded, Refused, Failed}`; `src/storage.rs`: `reword-failed` decision; `tests/reword_real_git.rs` (new) | unit, integration |
| 10 | feat: model the unrecognised-row decision and its label | Decision model + label | `src/supervise/mod.rs`: `UnrecognisedReason`, `UnrecognisedContext` (plain data), `UnrecognisedDecision`, `UnrecognisedPrompt` trait, `RowContinuation::{Answer, WorkRow}`; `RowOutcome::{UnrecognisedRow, Verified}`; `src/ui.rs`: `unrecognised_lines` — supervisor-authored block, fenced verifier prose, variants for `PlainlyUnimplemented`, `NotRun`, near-miss-only, both reasons, and each unavailable-reword guard reason | unit |
| 11 | feat: cache verdicts and run the hole pre-pass | Hole pre-pass | `src/supervise/mod.rs`: hole detection once per pass before `next_row`/`run_row`, per-row triage → cached `(row, HEAD)` reuse (no spawn), `COMPLETE` → `verified` + `hole-verified` decision + `RowOutcome::Verified`, otherwise `UnrecognisedRow`; `hole-escalated`/`verifier-failed` decisions; `src/state.rs`: cache lookup/insert helpers if not already there | unit, property-based |
| 12 | feat: ask the operator to resolve an unrecognised row | Operator seam (line mode) | `src/supervise/mod.rs`: `UnrecognisedPrompt` dispatch in `run_plan_interactive`; `[w]` as a one-run budget-bypassing `RowContinuation::WorkRow`; near-miss decision derived from a fresh `subjects()` snapshot outranks the budget gate inside `run_row`; `src/main.rs`: `CliUnrecognisedPrompt` + `unrecognised_prompt_loop` (`m`/`w`/`r`/`s`/`status`; EOF or blank stops, never work/mark); `[r]` calls the reword seam | unit |
| 13 | feat: render the unrecognised-row decision in the TUI | TUI modal | `src/tui/mod.rs`: `Modal::Unrecognised { row, label_lines, explanation_lines, can_reword }`, `ModalOutcome::{UnrecognisedComplete, UnrecognisedWork, UnrecognisedReword, Stop}`, dispatch/submit/EOF arms, `modal_box` arm, pure `map_unrecognised_modal_outcome`; `src/main.rs`: `CliUnrecognisedPrompt` TUI branch | unit |
| 14 | feat: prime the row worker for a hole row | Primed worker + ASK routing | `src/prompt.rs`: hole section for `WorkRow` continuations (situation, exact planned subject, anti-fabrication rule ending `PI_WORKER_STATUS: ASK` when only the subject is wrong); `src/supervise/mod.rs`: a `WorkRow` continuation ending ASK returns `UnrecognisedRow` with `verifier: None` (ordinary rows keep `QuestionPause`), worker question fenced in the explanation, `[w]` offered at most once per row per pass | unit |
| 15 | feat: surface holes and verified rows in status | Status + report surfaces | `src/cli.rs`: `status` holes line, `verified (no matching commit)` / cached-verdict row labels, state `verified: [..]`, next-action hint from the prompt helpers, final report `Verified` line with evidence and verifier facts, near-miss subject/hash/tier/ratio, `UnrecognisedRow` label + chosen action; `src/supervise/mod.rs`: `hole detected` and `verified without a commit` banner lines, `describe_outcome`/`banner_label` arms | unit |
| 16 | docs: document hole resolution and the verifier | Documentation | `docs/ARCHITECTURE.md`: `src/hole.rs` in the module map, Contract 2 "B is evidence, not a tier", Contract 3 verifier prompt/argv + row hole section, Contract 4 pre-pass and decision-vs-budget ordering, Contract 5 `verified`/`verifiedCache`/merge-on-save/`decisions.jsonl`, new "Unrecognised rows and the verifier" subsection with residual risks; `README.md`: narrative, prompts, reword guards, troubleshooting, the one sanctioned LLM-in-the-loop exception; `docs/acceptance-e2e.md`: section J | — |

## Constraints carried from the source plan

- **No hole, no verifier.** C is deterministic and gates every LLM call; the
  verifier is read-only, cannot commit, and its whole discretion is
  skip-or-escalate.
- **B changes nothing in Contract 2.** It is an evidence report for the
  verifier and the operator label only.
- **A cached `COMPLETE` is silent; a cached non-`COMPLETE` re-prompts the
  operator** without a spawn while `HEAD` is unchanged. `HEAD` moving
  invalidates the cache, never the `verified` provenance.
- **Facts shown to the operator are supervisor-authored.** LLM prose appears
  only inside a fenced, labelled section.
- **Zero operator keystrokes for the incident case**: hole → triage →
  verifier `COMPLETE` → row verified → the run advances to the next unmatched
  row.
- **`supervise --row N` and `step N` never run hole logic.**
- **No `pi-plan reword N` / `unverify N` commands, no automatic reword, no
  verifier writes or test execution, no free-text answer for a `WorkRow` ASK**
  (source-plan non-goals).

## Acceptance criteria (from the source plan)

1. Incident replay: one verifier spawn, `COMPLETE`, `verified` written, one
   decisions-log record, the run proceeds — no keystroke, no budget reset, no
   row worker.
2. Restart idempotence on unchanged `HEAD`: no spawn, no prompt for a
   `COMPLETE` verdict; a cached non-`COMPLETE` re-prompts once.
3. Escalation: the operator prompt appears before any budget prompt and
   offers `m`/`w`/`r`/`s`; `w` spends nothing when the work is present, `r`
   rewrites the subject, `m` writes `adjudicated`, `s`/EOF stop with exit 2.
4. Reword safety: exactly the drifted commit is rewritten, trees unchanged,
   before/after printed; refused on `main`/upstream/merge/target-merge/dirty
   tree with the guard named in the label.
5. No false holes (trailing unmatched rows, fully matched plans, plans with no
   `## Steps` table).
6. Provenance durability: `mark 8 done` then a `TODO.md` edit then `supervise`
   keeps row 8 adjudicated.
7. Deterministic label; the verdict is echoed verbatim.
8. No regression outside the intended budget-gate reachability and near-miss
   report text.
9. `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets
   --all-features -- -D warnings` green.
10. Manual gate recorded in `docs/acceptance-e2e.md` §J.
