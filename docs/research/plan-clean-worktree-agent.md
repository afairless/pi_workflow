# Plan: Recover from dirty worktrees by spawning a clean-worktree agent

## Status

Follow-up proposal for the `pi-plan` orchestrator. Triggered by the 2026-09-13
tag_tool incident: the supervisor completed row 1, then refused to spawn row 2
because the working tree was dirty (`?? target/` — the worker's own cargo build
left an untracked `target/` in a repo with **no `.gitignore`**) and the dirt was
not "owned" by the next row. The whole supervise run terminated with exit 2 and
work outstanding.

User change request (2026-09-13):

1. **New skill** — a `clean-worktree` skill describing the goal of restoring a
   clean worktree and how to handle common cases (notably: git-ignoring
   expected build artifacts; pi-lens post-commit reformatting that is
   formatting-only and can be discarded via `git reset --hard` after
   verification). Registered both as a general skill for any agent
   (`~/.pi/agent/skills/`) and in the version-controlled skills repo
   (`/home/tr/Documents/skills/skills/`).
2. **Supervisor behavior** — when the dirty-worktree gate detects an
   owner-less dirty tree, **launch a clean-worktree agent instead of
   aborting**. Inject the clean-worktree skill (and the implement-from-plan
   skill, so the clean agent shares the usual worker's project-navigation
   context) into that agent's initial prompt. If the clean agent succeeds and
   terminates properly, the supervisor proceeds with the row. If it fails, the
   supervisor aborts as a last resort (today's behavior).

Locked decisions (Q&A 2026-09-13): success is **git-keyed**; the clean attempt
is **budget-free and one-shot** (automatic attempts; a user-answered
continuation always runs — Contract 4 parity); only the **abort path** routes
through the clean agent (the owned-dirty resume path is untouched); the skill
is resolved **lazily** with graceful fallback to today's abort when it is not
installed; and a clean-agent **ASK pauses interactively** — the human's answer
is folded into a re-generated clean agent that retries the pass with that
guidance (bounded to one answered continuation, like row questions).

## The problem being solved (evidence)

- **A single owner-less stray kills the whole run.** In the tag_tool incident
  (`~/.pi-plan/tag_tool-45c3ea24/`): worker `pi-plan-row-1` committed row 1
  (`32a8180 chore: scaffold cargo crate with module skeleton`), the supervisor
  classified `RowOutcome::Done` and cleared state, then row 2's dirty gate saw
  `git status --short` → `?? target/`, `recover_state()` → `None` (state was
  cleared), so `owned == false` → `RowOutcome::DirtyWorktree` → exit 2. No
  `pi-plan-row-2` session ever existed (verified: only two `pi-plan-row-1`
  sessions under `sessions/`). The tree is still dirty today.
- **The gate is intentionally conservative, and that is worth keeping.**
  `dirty_tree_without_an_owner_refuses_without_writing_state` pins the rule:
  an owner-less dirty tree refuses to spawn a worker and writes NO state.
  Relaxing the gate with per-case rules would be brittle — new unanticipated
  dirty-tree shapes (`.ruff_cache`, `dist/`, an untracked config the agent
  keeps regenerating…) would keep surprising the supervisor. The robust fix is
  to hand the case to an agent that can *understand* the dirt, with a skill
  teaching the safe playbook, rather than codify every artifact type in Rust.
- **pi-lens reformatting is a recurring, low-risk case.** The extension can
  reformat files after the agent has committed, leaving formatting-only diff
  churn. This is exactly the case where `git reset --hard` is safe — but it
  must be gated on verification that the changes really are cosmetic and
  carry no functional content. The skill must emphasize that discipline.

## Verified technical facts (pi 0.85.1, this repo)

| Fact | Where verified |
|---|---|
| `pi`'s `--skill <path>` **can be used multiple times** ("can be used multiple times"), and so can `--append-system-prompt`. | `pi --help` (lines 21, 43) |
| The supervisor's argv builder currently emits exactly **one** `--skill` flag: `WorkerSpawnOpts.skill_path: Option<PathBuf>` → `build_worker_args` pushes a single `--skill`. Supporting both skills on the clean agent requires widening this to a `Vec<PathBuf>`. | `src/worker.rs` (`build_worker_args`) |
| The dirty gate sits in `run_row` before the prompt build: `tree_is_dirty = !status_short().is_empty()`; `owned` = recovered state names **this row** with a live outcome; `!owned` → `RowOutcome::DirtyWorktree` with no state write. | `src/supervise.rs` lines 527–575; test `dirty_tree_without_an_owner_refuses_without_writing_state` |
| Clean `git status` currently reaches the loop via the `GitFacts::status_short()` seam (already faked in the supervise tests). | `src/supervise.rs` (`GitFacts` trait) |
| Completion classification is git-keyed today: exact/similar commit match → `Done`; the worker's `PI_WORKER_STATUS: COMPLETE` marker is a hint only. The clean agent's success must follow the same discipline: **COMPLETE marker AND `status_short()` empty after the run**. | `src/supervise.rs` (`match_planned`, parse_worker_status) |
| Worker status markers are parsed by `parse_worker_status` (`PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>`) and`parse_question`(`QUESTION:` line); both pure and unit-tested. | `src/worker.rs` |
| `SuperviseServices` is a plain struct of seams (closures + traits); adding a lazy clean-skill resolver as a `Box<dyn Fn() -> Option<String>>` matches the existing `RecoverFn`/`SaveStateFn` style. | `src/supervise.rs` (`SuperviseServices`) |
| The implement-from-plan skill is loaded **fail-fast** at startup (`load_skill_body` errors hard). The clean-worktree skill needs the **opposite** posture: resolve lazily, return `None` on any failure, fall back to the current abort with a clear tail. | `src/cli.rs` (`load_skill_body`, `resolve_skill_path`) |
| The versioned skills repo exists and mirrors the live registry: `/home/tr/Documents/skills/` is a git repo; `skills/` contains one dir per skill; `~/.pi/agent/skills/` holds the live copies (implement-from-plan is present in both, in sync). | filesystem listing |
| Budget: `BUDGET_PER_ROW = 2`; the budget gate runs **before** the dirty gate in `run_row`, so a clean attempt inserted inside the `!owned` branch is naturally free as long as it does not mutate `attempt`/`runs_used`. | `src/supervise.rs` |

## Non-goals

- **No relaxation of the gate itself.** The dirty gate stays as the final
  safety net; only its *action* changes (try a clean agent first, abort only
  if that fails).
- **No change to the owned-dirty resume path** (`resumeDirtyWip`): a tree
  owned by the current row still resumes with the existing note/banner flow.
- **No in-Rust artifact catalog.** The skill, not Rust code, encodes how to
  recognize build artifacts and formatting churn.
- **No change to the row workers' ASK contract, session layout, or the tool
  allowlist.** The clean agent's ASK gains an interactive answer flow (the
  new behavior); row-worker ASK semantics are untouched.

## Change 1 — the `clean-worktree` skill

A new skill authored in the versioned skills repo and mirrored to the live
registry:

- **Canonical home (versioned):** `/home/tr/Documents/skills/skills/clean-worktree/SKILL.md`
  (committed in that repo — a separate commit from this crate's, per the repo
  boundary).
- **Live registry:** `~/.pi/agent/skills/clean-worktree/` (the mirror the
  supervisor resolves; kept in sync with the versioned copy).
- **Frontmatter:** `name: clean-worktree`, one-line `description` covering
  what it does *and* when to use it (general-skill registration for any
  agent), matching `how-to-write-a-skill` constraints (name matches the parent
  dir; description ≤ 1024 chars).

### Required skill body (outline for implementation)

1. **Goal.** Make `git status --porcelain` empty again, without losing
   meaningful work. The agent must end with `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>`.
2. **Discover first, act second.** Always run `git status`, `git diff`, and
   `git diff --cached`; classify every change before touching anything.
3. **Common case A — expected build artifacts not ignored** (most common;
   the tag_tool incident). Examples: `target/`, `.ruff_cache/`,
   `node_modules/`, `dist/`, `*.pyc`, `__pycache__/`, `*.orig`, coverage
   output, editor swap files. Fix: add the git-ignore rules (`.gitignore` or
   `git update-index --skip-worktree` where a tracked file is regenerated),
   then **commit the ignore change** with a small `chore:` message so the tree
   is clean and the rule is durable. Do not commit any *content* changes.
4. **Common case B — formatting-only churn (pi-lens post-commit reformat).**
   A tracked file shows a diff that is purely whitespace/reflow/quote-style
   (verify with `git diff` — the diff must carry zero functional content; if
   unsure, treat as meaningful). Then (and only then) `git reset --hard` is an
   acceptable way to restore the committed state. **Verify before destroying:
   never `git reset --hard` over a diff that contains real edits, new
   semantics, or files you cannot attribute.**
5. **Common case C — junk untracked files.** Editor/OS droppings
   (`*.swp`, `.DS_Store`, `core` dumps, stray logs): delete them, or run
   `git clean -n` to preview then `git clean -fd` scoped to the confirmed
   junk. Never `git clean -fd` without reviewing `-n` output.
6. **Never guess on meaningful work.** If the dirt looks like real work that
   is not yours to fold in (uncommitted source edits, staged work, a `.env`,
   work from another tool), **do not discard it**. Report **STUCK** when no
   human decision would help; report **ASK with a precise question** when a
   human decision would unblock the clean — the supervisor pauses for the
   human's answer and re-generates the clean agent with it folded into the
   prompt. The safety property is "never lose a change"; a slow stop beats a
   destructive clean. Never end COMPLETE with work stashed: a stash is
   invisible to `git status --short`, so every future dirty gate would miss
   it — if you stashed anything meaningful, restore it, report STUCK (or
   ASK), and say so.
7. **Verification.** Before COMPLETE: `git status --short` (the
   `--porcelain`-equivalent output the supervisor's seam reads) is empty, and
   `git diff`/`git diff --cached` are clean. If verification fails, report
   STUCK.
8. **Context.** The skill is written so any agent (not just the supervisor's
   clean worker) can use it: it names the common cases, the verification
   gate, and the explicit "when in doubt, ask" rule.

## Change 2 — lazy clean-worktree skill resolution

Today `resolve_skill_path` (implement-from-plan) hard-errors at startup via
`load_skill_body`. The clean-worktree skill gets the **opposite** posture:

- `src/cli.rs`: `resolve_clean_skill_path(env: Option<&str>, home: Option<&str>)`
  — `$PI_PLAN_CLEAN_SKILL` wins, else `<home>/.pi/agent/skills/clean-worktree`
  when it exists, else `None`. Tolerant loader: read + `strip_skill_frontmatter`
  → `Option<(PathBuf, String)>`; any failure returns `None` (never aborts
  startup).
- `src/supervise.rs`: `SuperviseServices` gains
  `clean_skill: Option<Box<dyn Fn() -> Option<(PathBuf, String)>>>` (a
  `CleanSkillFn` alias, same seam shape as `RecoverFn`) returning the
  resolved **path and body together** — the path fills the clean spawn's
  `--skill` argv entry, the body is framed into the prompt — so the skill is
  read **only when the gate fires** (never on a healthy run) and `run_row`
  needs no second resolution.
- `src/main.rs`: wire the closure in `cmd_supervise`; constructing it cannot
  fail. `supervise`/`step` never depend on the skill being installed.
- At the gate, `clean_skill` returns `None` → fall back to today's
  `RowOutcome::DirtyWorktree` abort, with a record tail naming the missing
  skill (`clean-worktree skill not installed — set PI_PLAN_CLEAN_SKILL or
  install it to ~/.pi/agent/skills/clean-worktree`).

## Change 3 — multi-skill worker argv

The clean agent needs **both** skills registered (`--skill` × 2 exists in pi)
and **both** bodies injected into its prompt:

- `src/worker.rs`: `WorkerSpawnOpts.skill_path: Option<PathBuf>` →
  `skills: Vec<PathBuf>`; `build_worker_args` emits one `--skill` per entry.
- Row workers keep today's exact argv (single `--skill implement-from-plan`);
  the multi-`--skill` path is exercised only by clean spawns. A test pins that
  the single-skill argv is byte-identical to today's.
- Mechanical blast radius: every `WorkerSpawnOpts` construction/tests fixture
  (`supervise.rs` fakes, `worker.rs` tests) updates; no behavior change for
  rows.

## Change 4 — spawn the clean-worktree agent at the abort gate

In `src/supervise.rs::run_row`, inside the `!owned` branch (the only place
that currently returns `RowOutcome::DirtyWorktree`):

1. Resolve the clean skill lazily (`services.clean_skill()` → `Option<(PathBuf, String)>`). `None`
   → the existing abort, with the missing-skill tail (Change 2).
2. Build the clean prompt with a new pure `render_clean_prompt` in
   `src/prompt.rs`: frames the clean-worktree skill body (operative) and the
   implement-from-plan body (context/navigation), carries `Project:` /
   `TODO.md:` / `Plan source:` so the agent can orient itself, names the row
   being prepared next (`Do not implement the row — the next worker will do
   that. Only restore a clean worktree.`), states the success criterion
   (`git status --short` — the seam's `--porcelain` equivalent — empty), and
   repeats the `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` marker contract. It
   opens with an authoritative **operative-line pin** — the clean-worktree
   body is the agent's *only* operating instruction for this run and the
   implement-from-plan body is reference context only ("Do not follow it") —
   the override that keeps the shared row persona (which tells workers to
   proceed step by step through the plan) from pushing the clean agent into
   implementing the row; the pin's presence is asserted by a Step-4 unit
   test.
3. Spawn via the existing `WorkerPort` with `WorkerSpawnOpts`:
   `name = "pi-plan-clean-<row>"`, model/turns from the same config
   resolution (`resolve_model`/`resolve_max_turns`), `skills = [<implement-from-plan
   path>, <clean-worktree path>]` (the clean path comes from the lazy seam's
   resolved pair), the same persona (`--append-system-prompt`; the step-2
   operative pin neutralizes its implement-the-plan instructions), same session dir
   (sessions land beside row sessions, distinguishable by name), same
   determinism flags/tools. Fire `on_spawn` so the trace/UI shows the clean
   pass. **Do not** touch `attempt`/`runs_used` (budget-free, one attempt per
   gate hit — locked).
4. Await the terminal (same `await_terminal` bound). Then classify,
   **interrupts first** — mirroring the row flow's order so an operator
   command during the clean pass wins over any clean result:
   - **Stop / kill (^D)** — `control.stop_requested` is set after the await
     (the kill watcher already SIGKILLed the clean agent): classify the pass
     as aborted, save the "stopped" state exactly as the row flow does
     (`kill_requested` preserved for `stop_was_kill`), and return
     `RowOutcome::Stopped` — never a clean-failure abort.
   - **Restart** — `control.restart_requested` is set: abort the clean agent
     best-effort and `continue` (re-fires the gate and the clean pass; the
     restart spends nothing and `attempt`/`runs_used` stay untouched).
   Then classify the clean result **git-keyed**:
   - **Success** ⇔ terminal `is_completed()` **and**
     `parse_worker_status(text) == "COMPLETE"` **and**
     `services.git.status_short().is_empty()` after the run **and** the
     newest git subject does **not** match the current row's planned commit
     message (one `subjects()` call — a matching subject means the clean
     agent committed something it must not have; treat as failure instead).
     Emit a report (`working tree cleaned by <agent>`), append a `RunRecord`
     for the clean attempt (outcome `Completed`, tail `"worktree cleaned"`),
     and **continue past the gate** — the row worker spawns as if the tree
     had been clean.
   - **Question** ⇔ terminal `is_completed()` **and**
     `parse_worker_status(text) == "ASK"` → return the new
     `RowOutcome::CleanQuestionPause { row, question, agent_id, records }`.
     A question wins over a clean `status_short()`: if the pass got the tree
     clean and then asked anyway, the pause still happens (the human's
     answer matters), and the re-run must handle the gate no longer firing —
     see the anomaly rule in the ASK flow below. The pause and
     re-generated continuation are the locked flow described below.
   - **Failure** (anything else: STUCK, bad marker, process exit, timeout, a
     non-empty `status_short()` after the run, or a tripwire hit) → append
     the clean attempt's record (outcome `Failed`, tail from the result / the
     missing marker / the still-dirty status / the tripwire), then return the
     existing `RowOutcome::DirtyWorktree` exactly as today — the
     abort-as-last-resort path, with the clean attempt visible in the final
     report.
   Clean records use **attempt marker `0`** (distinct from budgeted run
   attempts), so the final report can never read as if a clean pass spent a
   run of the row's budget.
5. **No state is written by the clean attempt** (preserves the "a refusal
   never inflates runsUsed" invariant; the subsequent row spawn's existing
   "running" save still happens if the clean succeeded).

### Clean-agent ASK → interactive human answer (locked)

A clean agent that ends `PI_WORKER_STATUS: ASK` pauses the run so the human
can supply an answer, and the **re-generated** clean agent completes the pass
with that guidance:

- **New variant.** `RowOutcome::CleanQuestionPause { row, question, agent_id,
  records }` — deliberately distinct from the row `QuestionPause` so the
  answer routes to the clean pass, never to a row worker.
- **Carried channel.** `run_plan`/`run_row` gain a `clean_continuation:
  Option<&CleanContinuation>` parameter — a small struct carrying the paused
  question *and* its answer (the question travels so the anomaly outcome can
  still report it), a second carried channel separate from the row answer
  (which is cleared when its row completes). The channel lives for exactly
  one continuation: `run_row` folds the answer into the next clean agent's
  prompt, and the supervise loop clears `carried_clean` after any `run_plan`
  pass whose last outcome is **not** `CleanQuestionPause` — the recurring
  `target/`-style dirt makes the gate fire on nearly every row, so a stale
  answer must never be replayed into an unrelated later clean pass.
- **Interactive surface.** `main.rs`'s supervise loop, after `run_plan`, asks
  when `last_clean_question(&result.outcomes)` is `Some` and `carried_clean`
  is `None` — reusing the exact row-ASK surface (TUI `Modal::Ask` /
  line-mode `ask_lines` + `stdin_read_line`, same `stop`/`restart`/`status`
  line commands). A stop/blank/^D/EOF answer ends the run (exit 2) with the
  question in the final report.
- **Re-generated agent.** With the continuation carried, `run_plan` re-enters
  the same row; the dirty gate re-fires (tree still dirty, still unowned)
  and a **fresh** `pi-plan-clean-<row>` worker is spawned whose prompt folds
  the answer in (`render_clean_prompt` gains `continuation:
  Option<&CleanContinuation>`; the `The human answered a previous
  clean-worktree agent's question:` block, escaped via the existing
  `format_answer_block`). That agent attempts to finish with the guidance.
- **Anomaly — ASK after cleaning (locked, review).** A clean agent can end
  ASK with the tree already clean (it cleaned, then asked something extra).
  The pause still happens in pass 1. On the re-run, `run_row` enters the
  gate carrying a continuation but the gate cannot fire — the tree is clean,
  there is no dirt to own — so the answer is unconsumable. That must not be
  a silent success or a silent drop: `run_row` detects the
  carried-continuation-with-clean-tree combination and returns the new
  `RowOutcome::CleanAnswerOrphaned { row, question, records }` embedding the
  original question. `carried_clean` is already set, so the loop guard fails
  and the run ends exit 2 with the question and a tail explaining the
  guidance was never consumable in the final report — no re-ask, no silent
  drop. The same applies if the tree became clean while the human was
  answering (e.g. they cleaned it manually).
- **Budget.** The answer is a user-driven continuation (Contract 4 parity):
  it always gets its fresh clean agent and spends nothing — `attempt` /
  `runs_used` are never touched.
- **Bound.** Mirrors the row flow's existing single-answer behavior: if the
  re-generated clean agent ends **ASK again**, `carried_clean` is already
  set, so the guard fails and the run terminates (exit 2) with the second
  question in the report. No infinite ping-pong; the loop is human-paced, one
  answered continuation per pause chain.
- **Recovery.** A killed run mid-pause loses the typed answer (same as row
  questions today); the clean pass is stateless, so the next `supervise`
  simply re-fires the clean agent from scratch.

## Change 5 — documentation and acceptance

- `README.md`: "Worker contract" gains a paragraph on the clean-worktree
  agent (when it runs, what it carries, git-keyed success, how an ASK pauses
  for a human answer that is folded into a re-generated clean agent, abort
  fallback);
  "Install" notes the clean-worktree skill as a **soft** prerequisite
  (`$PI_PLAN_CLEAN_SKILL` override documented); Troubleshooting gains the
  `clean-worktree skill not installed` guidance.
- `docs/ARCHITECTURE.md`: one paragraph on the clean-pass flow replacing the
  bare dirty-gate refusal.
- `docs/acceptance-e2e.md`: a manual/E2E check where an untracked expected
  build artifact (`target/`) is present before `supervise` — expect a
  `pi-plan-clean-*` session, a `chore:` ignore commit, a cleaned tree, and row
  progression (or, on the missing-skill path, the documented abort).
- `docs/research/plan-clean-worktree-agent.md` (this document) is
  self-describing; historical research docs stay untouched.

## Questions & Answers (locked 2026-09-13)

| # | Question | Locked answer |
|---|---|---|
| 1 | How does the supervisor decide the clean agent succeeded? | **Git-keyed**: COMPLETE marker AND `git status --porcelain` empty afterward — consistent with the row classifier; the skill instructs the agent to verify with `git status` before COMPLETE. |
| 2 | Does the clean attempt spend the row's run budget, and does a failed clean retry? | **Free and one-shot**: cleaning is a precondition, never a budgeted run; one clean agent per gate hit; failure → abort (existing DirtyWorktree), reported. |
| 3 | Which dirty cases route through the clean agent? | **Only the abort path** (owner-less dirt). The owned-dirty resume path (`resumeDirtyWip`) is untouched. |
| 4 | Must supervise hard-require the clean-worktree skill at startup? | **No — lazy with graceful fallback**: resolved only when the gate fires; missing → today's abort with a clear tail. Healthy runs never depend on it. |
| 5 | A clean-worktree agent ends with `ASK`? | **Interactive pause (locked)**: the supervisor surfaces the question (TUI modal / line-mode stdin), folds the human's answer into a **re-generated** clean-worktree agent, and re-runs the clean pass with that guidance. The continuation is user-driven, so it always runs and spends nothing (Contract 4 parity); a second consecutive ASK after an answered continuation ends the run (exit 2) with the question in the report — consistent with how row questions behave today. |

### Post-review locked decisions (2026-09-13, plan review)

| # | Decision | Locked answer |
|---|---|---|
| 6 | May an operator interrupt a clean pass? | **Yes — interrupts win over the clean classification** (F1): after the clean agent's await, `stop`/^D-kill classify as `RowOutcome::Stopped` with the `kill_requested` disambiguation, exactly like the row flow — never as a clean failure; `restart` aborts the clean agent and re-fires the gate. |
| 7 | How long does a carried clean answer live? | **Exactly one continuation** (F2): the loop clears `carried_clean` after any pass whose last outcome is not `CleanQuestionPause`, so a consumed answer can never fold into a later row's clean pass. |
| 8 | A clean agent ends ASK after already cleaning the tree? | **Anomaly abort** (F2): pause as usual; on the re-run a carried continuation with a clean tree is unconsumable → new `RowOutcome::CleanAnswerOrphaned`, exit 2, the question and a never-consumable tail in the final report. No re-ask, no silent drop. |

## Implementation plan

Ordering: skill first (external repo + live mirror), then seams (resolution,
argv), then the renderer, then the gate wiring, then the interactive answer
loop, then docs. Every crate commit
passes `cargo test`, `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, and is one logical
unit; implement → verify → commit with the table message → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
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

## Open items carried into review

- Whether the clean prompt should also receive the prior row's context (e.g.
  the previously completed row's commit message) to help the agent judge
  whether stray work belongs to history — left out for v1; the
  implement-from-plan body already lets it read the repo and TODO.md itself.
  **Review disposition (2026-09-13): kept out for v1.** The recurring dirt
  the gate exists for (the tag_tool `target/` pattern) never needs history
  context, and meaningful-work judgment falls back to the skill's "never
  guess — ask" rule instead.

## Review trail

Reviewed 2026-09-13 (second pass, after the clean-ASK Q&A the same day). All
seven findings were baked into the plan text in this pass; the original five
Q&A decisions stand unchanged, and this review adds decisions 6–8 in the
post-review block above:

- F1 — operator interrupts (stop/restart/^D-kill) win over the clean
  classification, mirroring the row flow (Change 4 step 4).
- F2 — `carried_clean` lives for exactly one continuation and clears after
  any pass that does not end in a clean question (Change 4 / interactive
  flow); the ASK-after-cleaning edge aborts as `CleanAnswerOrphaned` instead
  of silently dropping the answer.
- F3 — `render_clean_prompt` opens with an authoritative operative-line pin
  so the shared row persona cannot push the clean agent into implementing
  the row (Change 4 step 2; Step-4 test pin).
- F4 — the lazy seam returns path+body together (`Option<(PathBuf, String)>`),
  so `run_row` needs no second resolution path (Change 2).
- F5 — success is further gated on the newest git subject not matching the
  current row's planned message; the skill forbids ending COMPLETE with
  work stashed (Change 4 step 4; Change 1 items 6–7).
- F6 — clean records use attempt marker `0` so no report line can read as a
  spent budgeted run (Change 4 step 4).
- F7 — verification terminology aligned on `git status --short` (the seam's
  `--porcelain` equivalent) (Change 1 item 7).

Confirmed in review: the one-round ASK bound holds via the
`carried_clean.is_none()` guard; `CleanQuestionPause`/`CleanAnswerOrphaned`
need fresh `describe_outcome`/`cli.rs` report arms; the multi-`--skill` argv
blast radius is ~24 `SuperviseServices` fixture sites (acknowledged in step
3); the external-repo vs crate commit split in step 1 is sound.
