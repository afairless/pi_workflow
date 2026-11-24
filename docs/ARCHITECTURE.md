# Architecture — pi-plan

## Overview

`pi-plan` is a deterministic orchestrator that supervises the TODO.md
workflow. For each open row it spawns a fresh `pi --mode rpc` worker
process, monitors the worker's JSONL event stream, answers permission
dialogs inline over the extension-UI sub-protocol, and decides completion
from git history (never from worker claims alone).

The orchestrator is plain code — no LLM sits in the control loop. A stop is
just a stop. The one sanctioned exception is the read-only **verifier**: one
contained LLM call reached only after the deterministic hole detector says a
row is a hole, whose entire output funnels into *skip or escalate* (see
"Unrecognised rows and the verifier" below). It cannot edit, commit, run
tests, or write state, and every other decision stays plain code. The
repository (`TODO.md`, `docs/research/*.md`, git history) is the source of
truth; `pi-plan` only reads it and writes small state and audit files
(`supervisor-state.json` for crash recovery, `permissions.json` for the
dialogs' session grants, and `worker-stats.jsonl` / `decisions.jsonl` for
the audit trail). All run state — the state files, per-worker session JSONL,
the worker stderr log, and the one-shot stop control — lives **outside** the
repository under `~/.pi-plan/<project-key>/` (`src/storage.rs`), so a
supervised plan leaves the project directory clean.

## Module map

```text
src/
  main.rs        binary entry: clap dispatch + wiring; the
                 QuestionPause seam impl and the render task
                 (the interactive loop driver lives in
                 supervise/)
  config.rs      supervisor.config.json schema + precedence; the
                 global scaffold pin DEFAULT_GLOBAL_MODEL and
                 default_global_config_json builder          ✔ Step 3
  todo.rs        TODO.md row contract parser                  ✔ Step 2
  git.rs         git facade + git-keyed completion matcher    ✔ Step 2
  hole.rs        deterministic TODO-hole detector (C) + the
                 subject-drift/evidence bundle (B) + the
                 guarded reword + the verifier verdict parser
                 (pure; tests inline)
  prompt.rs      worker prompt builder (ASK contract) + the
                 verifier prompt + the WorkRow hole section     ✔ Step 3
  state.rs       supervisor-state.json (crash recovery)       ✔ Step 3
  storage.rs     pure run-state root resolution (external
                 ~/.pi-plan/<project-key>/) + the atomic
                 worker-stats.jsonl audit writer — a leaf
                 (std + serde + sha2 only)
  permissions.rs the durable permission store (D3 record shape,
                 v:1 json, atomic rewrite, dedupe, corrupt→
                 empty-warn) + the match core (option-label
                 parser, per-surface containment, always-grants
                 #1–3, auto-reply verdict, record filters)    ✔ steps 1–2
  rpc.rs         pi RPC client: JSONL framing, commands,
                 events, extension-UI dialogs; owns the
                 PendingTool event type                      ✔ Step 4
  worker.rs      WorkerPort trait + RPC worker impl
                 + stall ceiling                              ✔ Step 5
  supervise/     run/retry/ask state machine + interactive
                 driver behind the QuestionPause seam +
                 worker-stats mapping (tests in
                 supervise/tests.rs)
  cli.rs         subcommand parsing + control-file IPC +
                 status/final-report builders                 ✔ Step 8
  ui.rs          plain-terminal renderer (live tail, status
                 line, dialogs, line commands)                ✔ Step 8
  tui/           full-terminal renderer: backend, frame
                 builders, TuiState, input task (tests in
                 tui/tests.rs)
  theme.rs       palette loader + Stylize SGR styling
```

Implemented modules are testable in isolation: the supervise loop depends on
seams (`GitFacts`, `WorkerPort`, and recover/save/clear/report closures), so
its whole behavior matrix is unit-tested with a scripted fake worker and
staged fake git.

## Contracts

1. **TODO.md row contract** — `## Steps` table, `Source:` line,
   `## Prerequisites`, `## Done`. Rows carry no progress annotations; the
   parser only extracts structure (`src/todo.rs`).
2. **Completion, git-keyed** — a planned commit message is matched against
   `git log` subjects by similarity tiers over normalized strings:
   `exact` (equal) ≥ `similar` (ratio ≥ 0.9) ≥ `candidate`
   (prefix or ratio ≥ 0.8, near-miss). A row is done iff `exact` or
   `similar` matches; `candidate` stops for human adjudication
   (`pi-plan mark <n> done`). The worker never edits `TODO.md`.

   **B is evidence, not a tier.** The subject-drift report (`hole::SubjectDiff`:
   closest subject/hash, tier, 2dp ratio, extra/missing words) and the
   evidence bundle built from it change nothing here. B's `tier` is the plain
   `git::match_tier` verdict for the pair; `candidate` still never
   auto-completes, no new tier exists, and B is built only for hole rows. The
   only *automatic* non-git way a row becomes done is the verifier's
   authoritative `COMPLETE` (below), which writes provenance rather than
   changing the matcher (a human `mark <n> done` is the other non-git path).
3. **Worker prompt contract** — the prompt (`src/prompt.rs`) names the
   project, plan source, TODO.md path, the exact row text, and the planned
   commit message. The worker spawns with a pinned argv (Contract 3b,
   `src/worker.rs`): `--mode rpc --session-dir <root>/sessions
   --name pi-plan-row-<n> --model <model> --thinking high --approve
   --tools read,grep,find,ls,bash,edit,write --skill <implement-from-plan>
   --no-extensions [-e <permission-system dir>]` plus
   `--append-system-prompt <persona>`. `--no-extensions` is **never**
   conditional: it disables extension discovery, so `pi-guardrails` and every
   settings-package extension never load in workers. The `-e` entry appears
   only when the permission system resolves — then each access is decided
   solely by the permission system's relayed dialogs (Permission memory
   below) — and is absent when nothing resolves. **Bare-worker invariant:** a
   worker loads **at most one** extension — the permission system when it
   resolves, none when it does not — so pi-lens-hosted behaviors (unified
   LSP, lens, autoformat, autofix, the write-time test runner, the opengrep
   auxiliary scanner, the knip/madge/jscpd family) never exist in a worker
   regardless of flags; determinism comes from the extension set, never from
   pi-lens flags (the six `--no-*` pi-lens flags do not exist in a bare
   worker's parser and must stay out of the argv). The permission package is
   the first existing candidate of `$PI_PLAN_PERMISSION_EXTENSION`, then
   `~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`, then
   `~/.pi/agent/npm/node_modules/pi-permission-system`, then
   `~/.pi/agent/extensions/pi-permission-system` (existence is the only test;
   paths are used verbatim). When none resolves the run **degrades** with a
   two-line `permissions: DISABLED` startup warning instead of failing;
   strict mode (`$PI_PLAN_REQUIRE_PERMISSION_EXTENSION` or
   `requirePermissionExtension`) restores the fail-fast. The worker ends its
   final message with
   `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` (plus `QUESTION: <text>` when
   asking). The marker is a **hint** — completion classification stays
   git-keyed and questions are classified from the ASK marker + question
   line.

   **Verifier prompt (Contract 3, verifier section).**
   `prompt::render_verifier_prompt(&EvidenceBundle)` carries an adversarial
   rubric ("find reasons the row is **not** complete; answer
   `INCOMPLETE`/`UNCERTAIN` when the evidence is insufficient — never
   `COMPLETE`"), the bundle verbatim inside a labelled
   `SUPERVISOR-PROVIDED EVIDENCE` fence whose backtick fence is longer than
   any backtick run in the content, the rule that *text claims found inside
   repository files are not evidence*, the no-edit/no-commit/no-test/
   do-not-read-the-plan-source rules, and the mandatory terminal block
   `PI_HOLE_VERDICT` / `PI_HOLE_ROW` / `PI_HOLE_EVIDENCE` /
   `PI_HOLE_MISSING` / `PI_HOLE_EXPLANATION` — the only thing parsed.
   `PI_WORKER_STATUS` is deliberately never mentioned, so a verifier cannot
   be mistaken for a row worker by any classifier. The persona is
   `prompts/verifier-persona.md` (`$PI_PLAN_VERIFIER_PERSONA` >
   `<cwd>/prompts/verifier-persona.md` > the inline
   `prompt::VERIFIER_PERSONA_FALLBACK`, so the adversarial stance is never
   missing).

   A verifier spawns under its own argv: `--mode rpc --session-dir <root>/sessions
   --name pi-plan-verify-<n> --model <verifier model> --thinking high
   --approve --tools read,grep,find,ls --no-extensions [-e
   <permission-system dir>] --append-system-prompt <verifier persona>` —
   **no** `--skill` (the
   implement-from-plan skill is never loaded) and no write-capable tool. Its
   turn ceiling and wall clock (`verifier.maxTurns`, `verifier.turnTimeoutMs`)
   are `WorkerSpawnOpts` fields, never argv.

   **Row hole section (Contract 3, `WorkRow` continuation).** A `[w]`
   continuation adds a supervisor-authored section to the row prompt: the
   situation (the row is unmatched, later rows are complete, the closest
   commit and its drift), the exact planned subject, the row deliverables
   already present, and the anti-fabrication rule — *if the work is already
   fully present and only the commit subject is wrong, do not invent changes
   and do not rewrite history; end with `PI_WORKER_STATUS: ASK` and the
   suggested reword.* The verifier's prose is never injected; the worker sees
   supervisor facts only.
4. **Run/retry/ask state machine** — see below (`src/supervise/`).
5. **Crash recovery** — `supervisor-state.json` under the external
   `~/.pi-plan/<project-key>/` root (`src/storage.rs` resolves the same key
   for every command; `$PI_PLAN_STATE_DIR` overrides the base), written
   atomically (temp file + rename), best-effort (a failed write never
   crashes the loop).

## The supervise loop (Contract 4)

Config is resolved once at startup (`cmd_supervise`) in four tiers:
`--config PATH` (absolute) > `<cwd>/supervisor.config.json` (presence-based)
> the XDG global `~/.config/pi-plan/supervisor.config.json` (auto-created
when absent, see below) > built-in defaults. `resolve_config`
(`src/cli.rs`) returns a `ResolvedConfig { config, source }`; the startup
line prints the resolved model + source. `status`/`stop`/`mark` never
resolve config. An optional `verifier` block
(`{ "model", "maxTurns": 20, "turnTimeoutMs": 600000 }`) bounds the
read-only verifier: its model falls back to the worker `model` and then
`DEFAULT_MODEL`, its caps apply as spawn options, and it has no per-row
override (`verifier` is never a row). The global file is configuration (XDG,
`$XDG_CONFIG_HOME` else `~/.config`), deliberately separate from the
run-state root `~/.pi-plan/` / `$PI_PLAN_STATE_DIR` (`src/storage.rs`);
the auto-create is best-effort (never overwrites a user edit, never fails a
run on a write error) and the scaffold pins
`DEFAULT_GLOBAL_MODEL` (the dated snapshot
`openrouter/deepseek/deepseek-v4-flash-0731`) while the in-code fallback
`DEFAULT_MODEL` stays the rolling alias.

Per-row budget: **2 runs** (initial + one automatic retry). A question
pause, a stop, and a restart spend nothing; a **spawn error** spends
nothing too — it respawns (3 attempts total, 2 s fixed backoff) and a
persistent failure stops with the distinct "worker spawn failed" outcome;
every other terminal event spends one run.

```text
spawn worker (fresh pi --mode rpc process; on failure, respawn — up to
  3 attempts total with 2 s fixed backoff; exhausted → stop with
  "worker spawn failed": runsUsed untouched, NO intermediate state)
  └─ saveState({currentRow, runsUsed, lastOutcome: "running",
                agentId, startedAt})            ← spawn-time save
await terminal (agent_settled, or a stall-ceiling abort
                enforced worker-side: turn_end count, wall clock)
classify, checked in this order:
  ├─ operator interrupts (stop / restart) — win over everything
  ├─ ASK?  last assistant text has PI_WORKER_STATUS: ASK
  │        → not spent; save "question"; print the question,
  │          wait for the answer → fresh worker with the answer folded
  ├─ git exact/similar → row done; clearState
  ├─ git candidate → near-miss; save; stop for `mark <n> done`
  ├─ dirty tree at the boundary, no row owner → clean-worktree agent
  │        (budget-free, one-shot; interrupts win, git-keyed success);
  │        on failure or a missing skill, refuse (DirtyWorktree) —
  │        the refusal writes NO state (cannot spend or inflate budget)
  └─ otherwise spent (failed / complete-without-commit /
           STUCK-without-question):
       save; retry fresh worker if runsUsed < 2, else ask the operator
```

An operator-provided answer is a user-driven continuation and always gets
its run, even after the budget is exhausted. A restart keeps the row's
budget untouched (the fresh worker still runs under the same attempt).

The budget gate (either exit — the resume-blocked check at the top of an
attempt loop and the live check after the second spent run) never
auto-stops a row: it yields `RowOutcome::BudgetChoice` and the interactive
driver asks the operator (`BudgetPrompt` seam, mirror of `QuestionPause`)
whether to **reset the row's budget** (`runsUsed` → `0`, full 2-run budget
again) or **decline** and get today's stop-with-report-plus-exit-2 bytes.
`y`/`yes` writes a reset state (`state_file` with `lastOutcome:
"budget-reset"`, `agentId`/`startedAt` cleared, `adjudicated` preserved,
`planHash` recomputed) and re-runs the row at full budget; blank, EOF, or
`stop` declines — the final report is byte-identical to the old auto-stop.
Resets are unlimited and operator-driven (each an explicit keystroke; no
counter, no schema change), the prompt fires both on re-start and live
mid-session, and EOF/closed stdin at the prompt declines, so scripted and
CI invocations keep the exact exit-2 behavior with no flag surface.
`spawn-error` keeps its full-budget discount and never prompts; a carried
answer (`--answer` or a prior answered question) still bypasses the gate
and re-folds into post-reset attempts unchanged.

### Hole pre-pass and the decision-vs-budget order

One pass of `run_plan` begins with the **hole pre-pass**, before `next_row`
and therefore before any `run_row`/budget gate — a hole outranks a spent
budget structurally, not by ordering inside `run_row`:

```text
read subjects + plan + provenance
  └─ detect holes (deterministic, C)
      └─ for each hole row, ascending:
          ├─ cached verdict for (row, HEAD)? → reuse; no spawn
          │    ├─ COMPLETE     → verified provenance, continue
          │    └─ non-COMPLETE → re-present to the operator
          ├─ triage            → plainly unimplemented → escalate (no spawn)
          ├─ evidence bundle (B)
          │    └─ one read-only verifier
          │        ├─ COMPLETE → verified provenance, continue   ← only skip
          │        └─ else     → escalate to UnrecognisedRow
          └─ escalation stops the pass for the operator prompt
  └─ next_row (skips verified rows) → run_row as before
```

Detection uses one `subjects()` snapshot per pass; a hole's successors must be
`git_done` (a `verified`/`adjudicated` row without a commit is not proof the
plan advanced). A `[w]ork row` continuation owns its row for that
pass, so the pre-pass skips the hole the operator just chose to work.

**Decision vs budget ordering.** Inside `run_row`, when the budget gate would
fire (`attempt >= BUDGET_PER_ROW`) and no continuation is carried, the loop
re-derives the row's tier from a fresh `subjects()` snapshot. A `Candidate`
row returns `RowOutcome::UnrecognisedRow` (near-miss reason) instead of
`BudgetChoice`: a spent budget on a row with a plausible commit *is* the dead
end, and resetting it can only re-run a row that cannot produce a commit.
Policy consequence: **the budget prompt is unreachable for a candidate row**;
the operator can always choose `[w]ork row`, which is budget-bypassing for
one run (one explicit keystroke, same posture as the unlimited reset). Every
non-candidate spent row still reaches `BudgetChoice` unchanged.

## Dirty-WIP gate (Step 7): refuse strays, resume owned WIP

At the top of every attempt the loop reads `git status --short`:

- **Clean tree** — no gate.
- **Dirty tree, no owner** — no recovered state, or the recovered state
  does not name this row, or its `lastOutcome` is a non-owner marker. For
  `spawn-error` the marker does double duty: it is also a
  **budget-discount marker** on resume — a row whose last invocation never
  completed a real run regains its full budget (recovery rules below). The
  loop hands the tree to a dedicated
  **clean-worktree agent** instead of aborting on sight (section below);
  a clean pass that fails — or a missing clean-worktree skill — refuses
  with `DirtyWorktree`, spawns nothing, and writes **no state**, so
  repeated refusals cannot inflate `runsUsed`.
- **Dirty tree, owned by this row** — the recovered state names this row
  with a live outcome (`running`, `question`, `failed`, `budget-reset`,
  ...). `budget-reset` (written when the operator resets an exhausted
  row's budget) is an **owner marker** like `running`/`failed` —
  deliberately not `dirty`/`spawn-error`, so an owned dirty tree resumes
  with the resuming note and never routed to the clean-worktree agent. The
  loop
  resumes: the worker's prompt carries the `resumeDirtyWip` note
  ("fold the previous attempt's work into your commit; never commit
  unrelated strays"), and the spawn report line shows
  `· resuming dirty WIP`.

The state read at the gate is fresh per check — the loop's own terminal
saves and restarts update `supervisor-state.json` between attempts, so
ownership is never decided on a stale snapshot.

### Clean-worktree pass (dirty-gate recovery)

An owner-less dirty tree is a final safety net, not a dead end. Inside the
`!owned` branch of `run_row`, before the row worker's prompt build, the
loop resolves the clean-worktree skill **lazily** (`$PI_PLAN_CLEAN_SKILL`
> `~/.pi/agent/skills/clean-worktree` > `None`; `None` → today's abort with
a "clean-worktree skill not installed" tail) and spawns a dedicated
`pi-plan-clean-<n>` agent with **both** skills: `--skill
implement-from-plan` (navigation context) plus the resolved clean-worktree
skill. The clean prompt frames the clean-worktree body as the agent's
**only operating instruction** with an authoritative operative-line pin
(reference context must not implement the row), names the row prepared
next, and states the success criterion (`git status --short` empty) and the
`PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` marker contract. The skill, not
Rust, encodes how to recognize build artifacts and formatting churn.

The pass is **budget-free and one-shot**: it never touches
`attempt`/`runsUsed` and writes no state (a failure keeps the refusal's
no-state-write invariant). After the await, classification is
**interrupts-first** — stop/^D-kill → `Stopped` (with `kill_requested`
preserved for `stop_was_kill`), restart → re-fire the gate — then
git-keyed:

- **Success** ⇔ terminal completed **and** `PI_WORKER_STATUS: COMPLETE`
  **and** `git status --short` empty **and** no commit matching the row's
  planned message (a one-call `subjects()` check — the next worker owns
  that commit). The row proceeds as if the tree had been clean; the
  success record (attempt marker `0`, tail "worktree cleaned") lands in
  the report.
- **ASK** → `RowOutcome::CleanQuestionPause` — the run pauses
  interactively exactly like a row question (the same TUI modal /
  line-mode `answer>` prompt), and the human's answer is carried as a
  `CleanContinuation` into a re-generated clean agent whose prompt folds
  it in (one answered continuation per pause chain; a second consecutive
  ASK terminates). A carried answer that cannot be consumed — the tree is
  already clean, ASK-after-cleaning or the human cleaned manually — is
  `RowOutcome::CleanAnswerOrphaned`: exit 2, the original question and a
  never-consumable tail in the report, never a silent drop.
- **Failure** (STUCK, process exit, missing/unknown marker, a still-dirty
  tree, or a tripwire commit) → a clean record (attempt marker `0`) plus
  the existing `DirtyWorktree` abort, exactly as before the change.

The carried clean answer lives **exactly one continuation**: the
supervise loop (`keep_clean_continuation` in `main.rs`) clears the channel
after every pass whose last outcome is not `CleanQuestionPause`, so a
stale answer can never fold into an unrelated later row's clean pass — the
recurring `target/`-style dirt makes the gate fire on nearly every row.

## supervisor-state.json (Contract 5)

```jsonc
{
  "planHash":    "<sha256 of TODO.md content>",
  "currentRow":  3,
  "runsUsed":    1,
  "lastOutcome": "failed",        // running|question|failed|no-commit|
                                  // near-miss|stopped|spawn-error|dirty|
                                  // budget-reset|verified|hole-escalated|
                                  // verifier-failed
  "adjudicated": [7],             // rows the human marked done (preserved
                                  // across every loop save)
  "verified":     [              // rows the verifier authoritatively skipped
    { "row": 8, "head": "6bf2c8e", "verdict": "complete",
      "commit": "fc5ea15", "at": 1795360341333,
      "evidence": ["tests/test_decision_training_leakage.py",
                   "commit fc5ea15"] }
  ],
  "verifiedCache": [              // every settled verdict by (row, HEAD),
                                  // newest 64
    { "row": 9, "head": "6bf2c8e", "verdict": "incomplete",
      "at": 1795360341333 }
  ],
  "agentId":     "0",             // optional; last live worker (spawn-time)
  "startedAt":   1750000000000    // optional; epoch ms of that spawn
}
```

`verified` is authoritative provenance: it feeds the done set exactly like
`adjudicated`, so a restart or a changed plan hash cannot resurrect a row the
verifier skipped. `verifiedCache` is *not* provenance — it only prevents a
re-spawn for an unchanged `HEAD`, is bounded to the newest 64 entries (LRU by
`at`, then row, then head), and a cached non-`COMPLETE` verdict is re-presented
to the operator rather than silently skipped. `HEAD` moving invalidates the
cache entry, never the `verified` row. Both fields are backward compatible
(absent → empty) and are coerced elementwise; malformed members are dropped
and wrong-typed fields read as empty, exactly like `adjudicated`.

Where it lives — every command resolves the same external root
(`~/.pi-plan/<project-key>/`, or `$PI_PLAN_STATE_DIR` when set) with
`ProjectStorage::resolve` (`src/storage.rs`): the project key is the
sanitized cwd basename plus the first 8 hex chars of sha256 over the
canonicalized cwd, so symlinked views of one project share a key and the
project directory stays clean (no `supervisor-state.json`, `.pi-plan/`, or
`.pi-plan-stop` ever appear in the repo). The state file, per-worker
session JSONL (`--session-dir`), the spawned `pi` stderr log
(`worker-stderr.log`), the one-shot `.pi-plan-stop` control file, the
permission store (`permissions.json` — Permission memory), the run audit log
(`worker-stats.jsonl`), and the decision audit log (`decisions.jsonl`) all
live under that root.

Recovery rules (`src/state.rs`):

- No file / unparseable / wrong shape → recompute from git (never throws).
- `planHash` mismatch → recompute (the plan changed).
- The state's `currentRow` is already matched in git → recompute (a stale
  file must not contradict the repo).
- Otherwise resume with `runsUsed` intact.
- `lastOutcome = "spawn-error"` is the one discount: the row resumes with
  its **full budget** (`runsUsed = 0`). A spawn error never completes a
  real run, and every legacy `spawn-error` state the old binary wrote had
  a `runsUsed` that came entirely from failed spawns — the discount
  self-heals those files (and at worst over-grants one run after a real
  spent failure followed by exhausted spawn retries; it never
  under-grants).

**Provenance merge on save.** `save_state_file` reads the file immediately
before writing and merges `adjudicated`/`verified`/`verifiedCache` from disk
into the state being saved, under a process-wide lock: `adjudicated` is a
union, `verified` is keyed by row, `verifiedCache` by `(row, HEAD)`, both
keeping the newest `at`. The merge is additive and plan-hash independent, so
a `pi-plan mark <n> done` cannot be lost to a concurrent supervise save and a
`TODO.md` edit no longer erases provenance (the old defect-4 dead end). The
wiring in `main.rs` stops deriving provenance from the recovered snapshot.
`clear_state()` still deletes the state at plan completion; the append-only
`decisions.jsonl` remains the durable audit trail.

**Decisions log.** `<root>/decisions.jsonl` gets one `v: 1` record per
decision via `storage::append_decision`: `kind` ∈ `hole-verified`,
`hole-escalated`, `verifier-failed`, `operator-complete`, `operator-work`,
`operator-reword`, `reword-failed`, plus the row, the `HEAD` it saw, the
successors, the planned/best subject, the tier and 2dp ratio, the verdict,
the verifier run facts, the chosen action, and a refusal reason. It uses the
same temp-then-rename, full-file-rewrite, best-effort discipline as
`worker-stats.jsonl`; a failed write never fails the run.

Writes are atomic and best-effort: a kill mid-write cannot corrupt the
recovery input.

## Unrecognised rows and the verifier (the one LLM step)

Contract 2 is unchanged: a row is done on a git `exact`/`similar` match, a
human `mark <n> done`, or a verifier `COMPLETE` (below). A row that cannot be
matched while a *later* row is git-matched is a **hole**; the deterministic
detector, the evidence bundle, the verifier, and the operator decision exist
so a drifted commit subject can never block a plan forever.

**C — the deterministic gate (`hole::detect_holes`).** A hole is an unmatched
row with at least one **git-matched** higher-numbered row. Trailing unmatched
rows are not holes (the plan merely stopped), and a successor resolved by
`adjudicated`/`verified` is not evidence: only a commit proves the plan
advanced. Detection runs once per pass against one `subjects()` snapshot,
before `next_row`, so a hole can never be pre-empted by a spent budget. The
return value is row numbers ascending, document order as the tiebreak; the
unnumbered row sorts last, so it is never a hole and never a successor.
`supervise --row N` and `step N` never run hole logic — the operator asked
for that row.

**B — the evidence bundle (`hole::build_bundle`).** For each hole row the
supervisor builds a deterministic, capped report: the row cells, the
subject-drift report (`SubjectDiff`: closest subject/hash, Contract 2 tier,
2dp ratio, extra/missing words), the successors, up to three `git show
--stat` candidates (2 KiB each), the closest commit's patch (8 KiB), the
row's deliverable paths with worktree presence (a token that is not a path is
*inconclusive*, never "missing"), and `git status --short`. Per-section caps
and a 24 KiB whole-bundle cap make truncation reproducible (`truncated`
names the trimmed sections in fixed order); `render_bundle` is
snapshot-tested. The plan source is deliberately not included. B is
**evidence, not a tier**: it never completes a row, it is built only for
hole rows, and Contract 2's matcher is untouched.

**Triage before any spawn.** `hole::triage` walks a fixed ladder: a row that
is git-done is `Impossible` (an internal error reported as a failed
verification, never a skip); no candidate commit **and** no present
deliverable is `PlainlyUnimplemented` (escalate without spending an LLM
call); otherwise `RunVerifier`.

**The verifier.** One fresh `pi --mode rpc` process per hole row, spawned
under its own name (`pi-plan-verify-<row>`), with `--tools read,grep,find,ls`
(**no** `bash`/`edit`/`write`), **no skills**, the verifier persona, and its
own caps (`verifier.maxTurns` 20, `verifier.turnTimeoutMs` 600000 as
`WorkerSpawnOpts` fields, not argv). Model precedence is `verifier.model` >
`model` > `DEFAULT_MODEL`. The prompt carries the adversarial rubric (above),
the bundle inside a labelled `SUPERVISOR-PROVIDED EVIDENCE` fence, the rule
that text claims in repository files are not evidence, the read-only rules,
and the mandatory terminal block; `hole::parse_verdict` accepts exactly one
occurrence of each marker, a matching row, and one of the three verdicts. A
missing/duplicated/unparseable marker or a row mismatch is `Unparseable`, and
markers inside a fenced code block are ignored. `Complete` is the only skip;
`Incomplete`, `Uncertain`, `Unparseable`, and `NotRun` (spawn error,
wall/turn ceiling, ASK, port error) all escalate. A verifier record lands in
`worker-stats.jsonl` as `"kind":"verification"` and is never charged to a
row's budget.

**The verdict surface.** `COMPLETE` writes `verified` provenance, a decisions
record, and `RowOutcome::Verified`; the pass continues to the next unmatched
row with no operator keystroke, no budget reset, and no row worker. Anything
else becomes `RowOutcome::UnrecognisedRow` and the driver asks the
`UnrecognisedPrompt` seam: `m` writes `adjudicated`, `w` runs the row once
more with the supervisor hole section (budget-bypassing), `r` performs a
guarded reword, `s`/EOF stops with the exit-2 report. A row escalated for a
near-miss only (a `candidate` commit with a spent budget) routes through the
same surface. The label is supervisor-authored; the model's prose appears
only in a fenced, labelled section and its verdict is echoed verbatim. TUI
mode opens `Modal::Unrecognised` with the same `m`/`w`/`r`/`s` vocabulary
(blank stops); line mode prints the block and reads the same keys.

**Caching.** Settled verdicts are cached by `(row, HEAD)` in `verifiedCache`.
A restart on an unchanged `HEAD` reuses the cached verdict and never re-spawns:
a cached `COMPLETE` is silent, a cached non-`COMPLETE` is re-presented to the
operator. `NotRun` is never cached — nothing settled, so the row is a failed
verification and a restart may retry it. `HEAD` moving invalidates the cache,
never the `verified` provenance.

**The reword.** `[r]` is offered only when eight deterministic guards pass,
in a fixed order: the worktree is clean; `HEAD` is on a real branch; the
branch is not `main`/`master`; the branch has no configured remote and no
upstream; the target is the row's best candidate and its tier is not `Exact`;
there is no merge between the target and `HEAD`; the target itself is not a
merge; and the planned message is a single non-empty line. The first failed
guard is named in the label (`Reword unavailable: …`) and the action is
absent. The operation records the target's tree, detaches at the target,
amends with the planned subject, proves the tree is unchanged,
`rebase --onto`s the later commits, restores the branch, and re-checks that
the planned subject now matches. Any failure aborts the rebase, restores the
recorded head, prints git's stderr, writes a `reword-failed` decision, and
ends the run with the report. There is no editor and no interactive rebase.

**`WorkRow` priming.** A `[w]` continuation folds the supervisor hole section
into the row prompt (the situation, the exact planned subject, the present
deliverables, and the anti-fabrication rule). If the work is already present
and only the subject is wrong, the worker must end `PI_WORKER_STATUS: ASK`
with the suggested reword; because the continuation is a `WorkRow` one, that
ASK returns `UnrecognisedRow` (never a free-text question pause), with the
worker's question fenced as prose. `[w]` is offered at most once per row per
run; a second ASK presents `[m]`/`[r]`/`[s]` with "the worker asked again;
re-running it will not help."

**Containment argument.** The LLM is reachable only after C says a hole
exists, only for that row, only with read tools, only with the bundle the
supervisor built, and its entire output funnels into skip-or-escalate. It
cannot manufacture a commit (no write tool, no bash), cannot edit the plan,
cannot run tests, cannot write state, and cannot be reached for a non-hole
row or for `--row N`/`step N`. Every fact shown to the operator is
supervisor-authored; the only model text is the fenced explanation.

**Residual risks (accepted).**

- An authoritative `COMPLETE` can still be wrong. The adversarial rubric,
  the triage ladder, the actual diffs in the bundle, and the loud report line
  reduce but cannot eliminate a false skip.
- Prompt injection from repository text is contained (the bundle is
  authoritative, text claims in files are not evidence, the plan source is
  absent, no bash) but not proven impossible: injected content is still
  visible to the verifier.
- A verified row is a **durable adjudication**, not re-evaluated when `HEAD`
  moves. v1 has no `pi-plan unverify N`; undoing it means editing
  `supervisor-state.json` (a tracked follow-up).
- Rewording rewrites every later commit hash on the branch. Matching is
  subject-based so the plan still resolves, and the upstream/merge/protected-
  branch guards refuse the operation, but clones or open PRs referencing the
  old hashes are the operator's concern.
- The verifier does not run the project's test suite (a possible later
  escalation path), so "tests pass" is never part of a verdict.
- `decisions.jsonl` and `worker-stats.jsonl` are full-file rewrites at this
  volume; revisit if the logs grow.

## Sourcing and data flow

```text
repository (TODO.md · docs/research · git history)
   │  read: todo.rs → row contract · git.rs → completion facts
   ├─ hole pre-pass: hole.rs → detect_holes → triage → verifier (read-only)
   │    └─ COMPLETE → verified provenance; otherwise UnrecognisedPrompt
   ▼
pi-plan supervise loop (supervise/)
   │  spawn: worker.rs → rpc.rs → `pi --mode rpc` with the Contract 3
   │         prompt, persona preamble, pinned flags
   ▼
pi worker (fresh process + context, one TODO row only)
   │  events: message_update · tool_execution_* · turn_end ·
   │           agent_settled · extension_ui_request (permission asks)
   │  ├─ permission ask ──► rendered dialog + extension_ui_response
   │  └─ PI_WORKER_STATUS: ASK ──► question pause → answer folded in
   ▼
git commit → next row (or report + stop)
```

Run state is never written back into the repository: state, sessions,
logs, and the stop control all live under `~/.pi-plan/<project-key>/`
(`src/storage.rs`), so a supervised plan leaves `git status` clean.

## The extension-UI permission sub-protocol (decision D9)

Workers run **bare**: at most the resolved permission system is loaded
(`--no-extensions [-e <package>]`), so nothing else — no guardrails, no
ambient extension set — participates in worker access. When the package
resolves, one permission system gates every worker access: covered asks are
auto-approved by the supervisor (Permission memory below), everything else
is relayed — never auto-approved. When it does not resolve the run is
**ungated** — the startup line says `permissions: DISABLED — …`, no
`extension_ui_request` is ever emitted, and every tool call is allowed (the
documented degrade; strict mode refuses instead). When a worker's permission
system resolves an `ask`:

1. The worker (a `--mode rpc` process, `ctx.hasUI = true`) emits an
   `extension_ui_request` JSONL frame on **stdout** (method `select` /
   `confirm` / `input` / `editor`, no `timeout`).
2. `rpc.rs` decodes it as `RpcEvent::ExtensionUiRequest` and publishes it
   on the worker's broadcast channel.
3. The operator-UI tail (`main.rs` `worker_tail` → `dialog_roundtrip`)
   renders the dialog to **stdout** (heading, message, numbered options /
   y-n hints / placeholder) and reads a reply from stdin.
4. `reply_from_input` maps the reply to a `UiReply` (`Value`/`Confirmed`/
   `Cancelled`); `worker.rs` `reply_extension_ui` sends
   `extension_ui_response` back to the worker over stdin.
5. The worker resumes; the loop never sees the dialogs as terminal events.

In TUI mode the dialog is not painted on stdout: `worker_tail` opens it as
`Modal::Dialog` and the modal box over the trace viewport presents the
same extension request as a **focusable list** — ↑/↓ move a row highlight
(Select: options 1..N then `(c) cancel`; Confirm: `(n) no`, `(y) yes`,
`(c) cancel`), Enter submits the highlighted row, and the first row is
pre-highlighted on open (bare Enter → option 1, or **no** on Confirm).
Focus wraps at both ends (↑ from the first row → last). The highlight is
pure TUI state (`TuiState.modal_focus`), owned by `input_task`/
`open_modal`, reset by `close_modal`, and repainted by the frame
task's 120 ms full-frame redraw — line mode, typed replies, and the line
commands are untouched.

**The dialog renders the full ask.** Both line mode (`dialog_lines`) and
TUI mode (`modal_dialog_rows`, drawn by `modal_box`) split the request's
multi-line `title` on `\n` (stripping a trailing `\r` per line so aligned
`label : value` facts keep their width) and wrap **every** content row to
the box's text-cell width (`inner − 1`) with the shared `wrap_text` — the
heading, the `tool`/`rule`/`command`/`full command` facts, any `message`,
and the options all wrap identically and nothing ellipsizes, so a long
`command : rm -rf …` never hides its target path. All title/message lines
are unfocused content rows and never count toward `dialog_item_count`, so
option numbering, the focus model, and `item_reply` are untouched; the
`▸` marker belongs to the focused row's first wrapped chunk only, and
`clip_modal_rows` now honors `focus_line` to keep that first chunk visible
when the bottom-anchored window can include it.

**Focused-row styling.** The focused dialog row is drawn as `accent`
(amber) **bold text** on the normal panel fill (`bg: fill`, `bold: true`)
— not a full-width amber band. `StyledLine` gained a `bold` flag and
`Stylize::bold()` returns the `\e[1m` SGR, emitted by `render_task` after
the fg/bg codes and cleared by the per-row `\e[0m` reset; the two
`compose_frame` re-pad copies preserve it. The `▸` marker stays as the
row marker when color is off. The Ask-question modal passes `focus: None`
and is unchanged.

**Pending-tool context.** `rpc.rs::decode_event` additionally decodes the
pending tool call's `args` from the `tool_execution_start` frame (pi
emits it with `args` **before** the gate's `extension_ui_request`).
`SnapshotAcc.pending_tool` (`PendingTool { tool_call_id, tool_name, args
}`) is set on start and cleared on `ToolExecutionEnd` — never on the UI
request, so the call survives the whole gate — and carried on
`WorkerSnapshot`/`WorkerView`. When a dialog opens, both modes read the
worker's snapshot and pass the pending tool into the renderer, whose
`tool_context_lines` emits `tool: <name> (<call_id>)` and, for a `bash`
call, `$ <command>` (other tools get a bounded compact JSON preview),
between the message rows and the options. Empty context renders nothing,
so third-party ASK/input dialogs are unchanged.

Line commands (`stop` / `restart` / `status`) are accepted at any dialog
prompt; `stop`/`restart` abort the worker after answering nothing more.
`Ctrl-D` at the prompt dismisses the dialog (reply `Cancelled`) and lets
the worker's own ceiling decide next — the Ctrl-D **kill switch** is TUI
mode only: there it SIGKILLs every supervise-spawned worker, unwinds the
TUI, prints the final report, and exits 2 (the closed-stdin EOF path is
unchanged). This is the same ask shape `infinity`
/`ask_parent`-style harnesses lacked — the pi-plan process is the sole
terminal authorizer for worker permission asks.

## Permission memory (the proxy, decisions D3/D7–D12)

`pi-plan` is a **permission proxy** over the dialogs it relays: it records
the operator's "…for this session" grants durably and auto-approves later
asks those grants (or the always-grants) cover. All of it lives in
`src/permissions.rs`, split into three pure layers — the store, the match
core, and the record filters.

**The store (D3/D5).** A grant is permission-shaped, never a command
string: `(surface-family, direction, pattern, width)` plus an `id`,
`worker`, and RFC3339 `createdAt`. Serialization is a `v: 1` envelope at
`<root>/permissions.json`, rewritten atomically (temp file + rename, the
same mechanism as `worker-stats.jsonl`); dedupe is on
`(family, direction, pattern)` with a coalesced `None` direction for
verb-less surfaces, so re-grants are idempotent. A missing file is a
healthy empty store; a corrupt one degrades to empty with a stderr warning
(fail-closed on permits, never blocks the run) and is repaired on the next
save. There is no expiry by default — grants accrue until reset (D5).

**The match core (D7/D9/D11).** Every relayed dialog first builds an ask
view from the request's own facts (flagged paths from the `path : …` core
fact, `external path` evidence lines, and the quoted glob in the session
option — pi-plan never parses shell). The supervisor then decides in
order: always-grant #1 (all flagged paths within the project root, any
direction) → #2 (all within the derived skills root, read direction only)
→ #3 (bash command targeting `<skills root>/**/scripts/**`, plain `Yes`
one-time) → stored-grant containment. Each always-grant requires ≥ 1
concrete flagged path, so a path-less bash ask is never auto-approved
(non-vacuous guard); catch-all `*` patterns and unparseable labels degrade
to prompt. Containment follows each surface's grammar: path glob (`*`
crosses `/`, trailing `~/a/*` covers the subtree), bash token-segment
prefix (`git status *` ⊇ `git status --short`, never `git push`), and
skill exact equality.

**Auto-approval vs. recording (D10).** When a match covers the ask, the
proxy `reply_extension_ui`s with the session option (or plain `Yes` for
path-covered but pattern-less asks) — tagged machine-generated — so the
worker stops re-asking mid-run, and logs one line per event. Those
auto-replies are **never written to `permissions.json`**: durable
recording captures only **operator-chosen** session-grant options from the
human dialog path, and additionally requires a select dialog whose option
set byte-contains the replied label. Plain `Yes`, denials, pattern-less
labels, multi-path / direction-disagreeing asks, `mcp`, and any bare-`*`
pattern are never recorded (D9/D11). The guarantee this buys: the store
never feeds its own broad grants — auto-approvals cannot accrue
precedents the operator never made, and the `status` grant count and the
keep/reset prompt stay operator-meaningful (D8).

**Reset UX (D6/D12).** At `supervise`/`step` startup a HEALTHY store with
≥ 1 grant opens a keep/reset prompt through the `QuestionPause` seam (the
same modal in TUI, the byte-exact stdout prompt in line mode): keep
(default) / `reset` (clears the store and proceeds) / `stop` (cancels the
run start, exit 2) / `restart` (keep-and-proceed, D12); EOF keeps.
`pi-plan reset-permissions [--yes]` clears the store from a shell — it
asks for confirmation unless `--yes`, then prints how many grants were
removed. `status` never prompts and shows the stored grant count; a
corrupt store skips the start-of-run prompt and surfaces one warning line
in the final report. `step` shares the startup path; `status`/`mark` never
prompt by construction.

## Operator surface (Step 8)

The CLI (`cli.rs` + `main.rs`) is clap-derived: `supervise [--row N]
[--answer "…"] [--config PATH]`, `status`, `stop`, `mark <n> done`,
`reset-permissions [--yes]`, `step N [--answer …]`.

- `stop` writes a one-shot `.pi-plan-stop` control file under the external
  run-state root (`~/.pi-plan/<project-key>/`); a stale file from a killed
  run is discarded at the next `supervise` start (never inherited), and
  while running a watcher task consumes it at the next boundary.
- The supervise loop renders: live worker traces on **stderr** (bounded
  ring buffer per worker so `bash_execution_update` chunks cannot flood the
  terminal), a periodic status line (`row · agent · turns/max · ctx% ·
  duration`) to stderr, and dialogs/ASK questions to **stdout** — the
  operator can `2>trace.log` and still answer dialogs.
- The final report (stderr) lists every row's outcome with per-attempt
  result tail and transcript path (`~/.pi-plan/<project-key>/sessions/`).
  A `verified` outcome also names its supervisor-authored evidence lines and
  the verifier's run facts (verdict, agent id, `HEAD`, turns, cost, tokens,
  transcript); an `UnrecognisedRow` names the deterministic label, the
  fenced verifier explanation or worker question, and the operator's chosen
  action; a near-miss now prints its subject, hash, tier, and ratio instead
  of a bare "needs adjudication". `status` prints a `holes: [..]` line,
  labels a verified row `verified (no matching commit)` and a cached verdict
  `verified (cached: <verdict>, HEAD <hash>)`, shows `verified: [..]` in the
  state block, and derives its next-action hint from the same pure helpers
  the operator prompt uses. `worker-stats.jsonl` is `v: 2` with
  `"kind": "run" | "verification"` (a v1 record without `kind` reads as a
  run).

**Live-only stats and the idle line.** The persistent TUI header/footer
show statistics for **live workers only**, tracked in a live-worker
registry rather than a single slot: `TuiState.workers` holds one
`WorkerEntry` (`worker_id`, `row`, `WorkerView`) per supervised worker.
`tail_task` **seeds** an entry at the spawn notice (`seed_worker` — row,
agent id, and the resolved max-turns ceiling, so a fresh worker's status
`row N · agent X · turns 0/40 …` is visible from its first frame);
`worker_tail` **upserts** it on every tail refresh (`update_worker` from
the snapshot; `WorkerView.live` follows `snapshot.terminal`, so a late
terminal snapshot upserts a not-live entry that rotation skips); and the
entry is **removed on every tail exit** (`remove_worker` — the stream
close, the subscribe-fail early return, and the dialog
`stop`/`restart`/`^D` break all route through one removal, so a killed
worker can never leave a frozen "live" entry behind). The row-terminal
hook, `SuperviseServices.on_row_terminal` (one structured call site in
`report_terminal`, so line and TUI mode both get every terminal event),
wakes `TuiState::note_row_terminal`, which now stores only
`last_terminal: (row, completed|failed)` — the not-live transition lives
in the registry.

The header/footer **rotate** through the live entries with a time-sliced
cursor (`RotationCursor` + the pure `select_live_worker`): each live
worker stays displayed `ROTATION_HOLD_MS = 3000` ms, then the selection
advances circularly to the next live worker, a cursor whose worker
vanished advances, and a non-live entry is never displayed. Header line 2
and the footer show the **same** worker at every instant; the modal
`status` note uses the same selection (`displayed_worker`), so the note,
header, and footer always agree. The footer's `row N` is the entry's
`TodoRow.number` from the spawn notice, so `--row N` mode and
non-contiguous numbering show the plan's own row (the idle footer always
did) instead of the single-row position. The supervisor idle line
(`idle · last: row N <kind> · next: row M — unit`, next row's unit from
the plan-seeded `TuiState.plan_units`; `next: row N` only when unknown)
renders **iff zero entries are live** — between rows, at the end of the
run, and in the brief 0-worker window between a terminal and the retry's
spawn — and the header then drops the stats context while keeping the
step banner (row/total/unit). Rotation is display-only: every tail keeps
updating its own entry independently, so a long-held worker's numbers are
fresh when its turn resumes. Clean-worktree agents seed and rotate a
registry entry like any row worker (they fire `on_spawn` too).

**Reporting + durable log.** Every terminated worker's final statistics are
printed in the ending `--pi-plan report`: one stats line per attempt
(`worker: <id> · cost $X.XX · N tokens · ctx P% · T turns · duration`, all
values from the terminal `RunRecord.snapshot`), and a compact stats suffix
(`· cost $X · N tokens · T turns`) is appended to each worker's terminal
report line so line mode records them too. A durable audit log is written
under the run-state root at `<root>/worker-stats.jsonl` by
`storage.rs::append_worker_stats` — a pure leaf writer; the record is
built by `supervise::worker_stats_from_run` from the terminal
`RunRecord` (`WorkerStatsRecord`, `v: 1` schema marker), using the same
write-temp-then-rename atomicity as
`save_state_file` and `create_dir_all(root)` before the first write —
invoked once per finalized `RunRecord` from a `SuperviseServices
.append_stats` closure that `cmd_supervise` supplies from the resolved run
root. Only runs with a terminal snapshot write a record (question pauses /
abort-before-stats write nothing), and each record is a full-file rewrite
— an audit log, not a live tail.

Exit codes: 0 = requested rows completed; 1 = error; 2 = supervise ended
with work outstanding (stopped / question / near-miss / budget-choice).

## Limitations / future work

- **Sequential rows only** — no parallel workers (matches the TS
  supervisor).
- **Shelling to `git`** — the facade (`src/git.rs`) mirrors the TS
  `spawnSync`; `git2`/`gix` is a deferred swap.
- **No auto mid-step respawn from context%** — compaction banner + manual
  `restart` is the v1 bridge.
- **No `pi-plan reword N` / `unverify N`** — the reword is only an operator
  prompt action (`[r]`), and a bad `verified` decision can only be undone by
  editing the state file (a tracked follow-up).
- **No verifier writes or test execution** — the verifier is read-only and
  its whole discretion is skip-or-escalate.
- **No hole logic for `--row N` / `step N`** — the operator asked for that
  row.
- **End-to-end gate pending** — `docs/acceptance-e2e.md` holds the manual
  checklist against `test-fixtures/spike/`; run it before relying on the
  binary for real plans.

## Migration from the TS `/supervise` extension (decision D7)

`pi_workflow/supervisor/` (the TypeScript `/supervise` extension, the
`ask_parent`-based supervisor) **remains the supported supervisor until
pi-plan demonstrates E2E parity** — that is, until `docs/acceptance-e2e.md`
passes end to end with real workers, real permission forwarding, and real
commits. The two implementations coexist during the transition and share
conventions so the swap is mechanical:

- **TODO row contract** — identical `## Steps` table, `Source:` line,
  `## Prerequisites`, `## Done` semantics (`todo.rs` ⇄ `todo.ts`).
- **Git-keyed completion** — the same tiered matcher (`exact` / `similar` /
  `candidate` thresholds 0.9 / 0.8) over normalized subjects, plus the
  `mark <n> done` adjudication surface (`git.rs` ⇄ `git.ts`).
- **Durable state** — a `supervisor-state.json` with the same recovery
  shape (`planHash` + `runsUsed` + `lastOutcome`), atomically written.

Differences to plan for when retiring the TS extension:

- Worker persona is delivered via **CLI flags + repo-versioned preamble**
  (`prompts/worker-persona.md`), not a global agent file; the ASK contract
  replaces `ask_parent` with the `PI_WORKER_STATUS: ASK` marker.
- Permission asks are answered **in pi-plan's own terminal** (decision D9)
  rather than forwarded to the root session's UI.
- The retire decision is a tracked follow-up after E2E parity — no code
  from the TS extension is removed in this repository.
