# Implementation Plan: Clean-worktree agent recovery at the dirty gate

Source: `docs/research/plan-clean-worktree-agent.md`

Recovery from the 2026-09-13 tag_tool incident: the supervise run died with
exit 2 because row 2's dirty-worktree gate found an owner-less `?? target/`
(the worker's own cargo build in a repo with no `.gitignore`). The gate stays
the conservative final safety net, but its *action* changes: instead of
aborting, the supervisor spawns a dedicated **clean-worktree agent** that
applies a git-safe playbook (ignore expected build artifacts, discard only
verified formatting churn, delete only confirmed junk, ask when in doubt),
then the row proceeds on a clean tree — or the run aborts exactly as today
if the clean pass fails.

Locked decisions (Q&A 2026-09-13, incl. post-review 6–8): success is
**git-keyed** (COMPLETE marker AND empty `git status --short` AND no
accidental commit matching the row's planned message); the clean attempt is
**budget-free and one-shot** (never touches `attempt`/`runs_used`, no state
written); only the **abort path** routes through the clean agent (the
owned-dirty resume path is untouched); the skill resolves **lazily** with
graceful fallback to today's abort when not installed; a clean-agent **ASK
pauses interactively** — the human's answer is folded into a re-generated
clean agent, bounded to one answered continuation like row questions;
**operator interrupts win** over the clean classification (stop/^D-kill →
`Stopped`; restart → re-fires the gate); a carried answer lives exactly one
continuation (ASK-after-cleaning → `CleanAnswerOrphaned`, exit 2, no silent
drop).

Step 1 is committed in the **external** skills repo
(`/home/tr/Documents/skills`, its own git repo) and mirrored to the live
registry; this crate's quality gates do not apply to it (verification is
frontmatter parse + byte-identical mirror). Steps 2–7 in this crate. Workflow
per step: implement → `cargo test` → `cargo fmt --check` → `cargo clippy
--all-targets --all-features -- -D warnings` → commit with the message in the
table → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | (external repo `/home/tr/Documents/skills`) `feat: add clean-worktree skill` + mirror to `~/.pi/agent/skills/` | Skill authoring | `skills/clean-worktree/SKILL.md` (frontmatter `name: clean-worktree` + body outline from Change 1); live copy at `~/.pi/agent/skills/clean-worktree/SKILL.md`, identical bytes | Frontmatter parses; `name` matches the parent dir; description non-empty; both copies byte-identical (`diff -r`); body contains the verification gate and the "never destroy meaningful work" rule |
| 2 | `feat: add lazy clean-worktree skill resolution seams` | Resolution (behavior-neutral) | `src/cli.rs`: `resolve_clean_skill_path` (`$PI_PLAN_CLEAN_SKILL` > `~/.pi/agent/skills/clean-worktree` > `None`) + tolerant body loader (`None` on any failure, unlike implement-from-plan's fail-fast); `src/supervise.rs`: `CleanSkillFn` alias + `SuperviseServices.clean_skill: Option<Box<CleanSkillFn>>` (all test constructions gain `None`); `src/main.rs`: wire a closure in `cmd_supervise` that resolves the path + body on call | Unit: env override wins, home default found, missing → `None`, malformed SKILL.md → `None` (never a startup error), closure returns the path+body pair once per call (path feeds the spawn argv, body feeds the prompt); full suite green |
| 3 | `feat: support multiple worker skills in the spawn argv` | Multi-skill argv | `src/worker.rs`: `WorkerSpawnOpts.skill_path: Option<PathBuf>` → `skills: Vec<PathBuf>`; `build_worker_args` emits one `--skill` per entry; row spawns pass the single implement-from-plan skill | Unit: single-skill argv byte-identical to today's; two entries emit two `--skill` flags in order; empty list emits none; fixture churn mechanical |
| 4 | `feat: render the clean-worktree agent prompt with both skill bodies` | Clean prompt (pure, unused) | `src/prompt.rs`: `render_clean_prompt` — frames clean-worktree body (operative) + implement-from-plan body (context), `Project:`/`TODO.md:`/`Plan source:` lines, next-row context with an explicit "do not implement the row" line, success criterion (`git status --short` empty), the authoritative operative-line pin, `PI_WORKER_STATUS` contract, optional `continuation: Option<&CleanContinuation>` folding the answered-question block (escaped via `format_answer_block`) | Unit: both bodies framed with distinct headers; the operative-line pin present; no-row-implementation instruction present; success criterion (`git status --short` empty) present; marker contract present; answered-question block present/absent; missing clean body → terse fallback line; body embedded verbatim (no escape corruption); existing prompts untouched |
| 5 | `feat: spawn a clean-worktree agent when the dirty gate would abort` | Gate wiring | `src/supervise.rs::run_row` `!owned` branch: lazy body resolve → `render_clean_prompt` → spawn `pi-plan-clean-<row>` (name/model/turns/skills/persona), `on_spawn` fired, await → **interrupts first** (stop/kill → `Stopped` with `kill_requested`; restart → re-fire the gate) → git-keyed success (COMPLETE + empty `status_short()` + newest subject does not match the row's planned message) → report + record (attempt marker `0`) + continue; `ASK` → new `RowOutcome::CleanQuestionPause` (question + agent_id, records) with `clean_continuation: Option<&CleanContinuation>` threaded through `run_plan`/`run_row` into the next clean prompt; carried continuation with a clean tree → new `RowOutcome::CleanAnswerOrphaned` (original question embedded); any other failure → clean record + existing `RowOutcome::DirtyWorktree` tail naming the reason (incl. "clean-worktree skill not installed"); budget untouched; no state written by the clean pass; `carried_clean` clears after any non-`CleanQuestionPause` pass; all `RowOutcome` match sites (`describe_outcome`, `cli.rs` report helpers) gain the variants | Unit (fakes): clean success lets the row worker spawn; COMPLETE-but-dirty → failure abort; newest subject = row's planned message → failure abort (tripwire); stop/kill injected mid-clean → `Stopped` (never a clean-failure abort); restart injected mid-clean → gate re-fires; ASK → `CleanQuestionPause` carrying the parsed question + agent id; ASK-while-clean + carried continuation → `CleanAnswerOrphaned` embedding the original question; STUCK/ProcessExit/spawn error → failure abort; missing clean skill → abort with the not-installed tail; the carried answer reaches the re-generated prompt and never a later row's clean prompt; clean records carry attempt marker `0`; `runs_used`/state unaffected; abort path keeps the no-state-write invariant; success record carries "worktree cleaned" |
| 6 | `feat: answer clean-worktree agent questions interactively` | Interactive clean-ASK loop | `src/main.rs`: `last_clean_question` helper; `carried_clean` channel in the supervise loop; when a clean question appears without a carried answer — reuse `Modal::Ask` (TUI) / `ask_lines` + `stdin_read_line` (line mode) with the same `stop`/`restart`/`status` commands; set `carried_clean`, `continue`; stop/blank/^D/EOF → exit 2 with the question in the report; a second consecutive ASK after an answered continuation terminates (parity with rows) | Unit/integration: question surfaces in both TUI-modal and line-mode shapes; answer folds into the re-generated clean prompt on re-run; `carried_clean` clears after a pass whose last outcome is not `CleanQuestionPause`; `CleanAnswerOrphaned` ends the run exit 2 with the question and the never-consumable tail before any row worker spawns; blank/^D stop prints the question in the final report; second-ASK terminates; no row worker is spawned before the clean succeeds |
| 7 | `docs: document the clean-worktree agent flow` | User docs | `README.md` worker-contract (incl. the interactive ASK flow) + soft-prerequisite + `$PI_PLAN_CLEAN_SKILL` + troubleshooting; `docs/ARCHITECTURE.md` clean-pass paragraph; `docs/acceptance-e2e.md` dirty-tree recovery check (expect `pi-plan-clean-*` session, ignore-chore commit, clean tree, row progression) and an ASK-answer check | `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`; docs read cleanly; e2e checks execute both paths |

### Step 1 notes (skill authoring — external repo boundary)

- The skill commit lives in `/home/tr/Documents/skills` (its own git repo), not
  this crate — this crate's quality gates do not apply to it; verification is
  frontmatter parse + byte-identical mirror. The subsequent crate commits do
  not depend on the skill's exact wording, only on the resolver finding
  `SKILL.md` with resolvable frontmatter.
- Keep the two homes in sync: after authoring, `cp -r` the skill dir into
  `~/.pi/agent/skills/` and `diff -r` to confirm.

### Step 5 notes (gate wiring details)

- Insertion point: the `if !owned { … return RowOutcome::DirtyWorktree }`
  block in `run_row` (src/supervise.rs lines ~546-571). The clean attempt
  runs entirely before the row worker's prompt build; `resume_note` stays
  `None` on the clean path.
- Clean success must be **re-verified against git**, not just the marker —
  the agent may *think* it cleaned; `status_short().is_empty()` is the
  classifier (same spirit as the row classifier never trusting the marker).
- On success the row proceeds with a fresh (now-clean) spawn; if the worker
  then fails and the retry re-enters the gate, re-evaluation is identical
  (the state file now names the row → owned → resume path, unchanged).
- `describe_outcome(RowOutcome::DirtyWorktree)` keeps its current wording
  ("paused — working tree not clean"); the appended clean-attempt record
  supplies the detail in the final report.
- The clean classification must not shadow operator interrupts: after the
  clean await, consume the control flags exactly like the row flow (stop/kill
  → `Stopped`; restart → re-fire the gate) before any Success/Question/
  Failure mapping — a ^D kill mid-clean is an operator stop, never a clean
  failure.
- Clean records use attempt marker `0` so no report line can read as a spent
  budgeted run; `attempt`/`runs_used` are never touched by the clean pass.
- `carried_clean` is a main-loop channel cleared after every pass whose last
  outcome is not `CleanQuestionPause`. The recurrence of `target/`-style
  dirt (tag_tool pattern) makes the gate fire on nearly every row, so a
  stale answer must never be folded into an unrelated later clean pass.
- A carried continuation that cannot be consumed — the gate does not fire
  because the tree is already clean (ASK-after-cleaning, or the human
  cleaned manually while answering) — is an anomaly:
  `RowOutcome::CleanAnswerOrphaned`, exit 2, the question and a
  never-consumable tail in the report.

### Open items carried from the plan review

- Whether the clean prompt should also receive the prior row's context (e.g.
  the previously completed row's commit message) to help the agent judge
  whether stray work belongs to history — left out for v1; the
  implement-from-plan body already lets it read the repo and TODO.md itself.
  **Review disposition (2026-09-13): kept out for v1.** The recurring dirt
  the gate exists for (the tag_tool `target/` pattern) never needs history
  context, and meaningful-work judgment falls back to the skill's "never
  guess — ask" rule instead.
