# End-to-End Acceptance — pi-plan

Run the orchestrator against `test-fixtures/spike/` (3 rows) with the
**real** `pi --mode rpc` worker, real permission forwarding answered inline,
and real commits. Section K additionally covers the documented **ungated
degrade** when the permission system is not installed, and the opt-in strict
mode that refuses such a run.

**Status:** ⏳ PENDING — first run after Steps 1–8 landed. Record each run in
the Results table at the bottom.

## Scope

This is the acceptance gate for the `pi-plan` orchestrator (Step 9 of
`docs/research/plan-rust-orchestrator.md`). It proves the whole wiring under
one roof:

- the real `pi-plan` binary (clap CLI: `supervise`/`status`/`stop`/`mark`/
  `step`/`reset-permissions`)
- a fresh `pi --mode rpc` worker per row (Contract 3b argv: bare
  `--no-extensions`, plus `-e <permission-system>` only when the package
  resolves — otherwise no `-e` at all — + persona, determinism flags,
  session dir)
- inline `extension_ui_request` dialogs answered over the RPC extension-UI
  sub-protocol (decision D9 — the operator answers in pi-plan's own
  terminal, never in a hidden prompt)
- the ASK marker contract (`PI_WORKER_STATUS: ASK` + `QUESTION:` answered at
  an `answer>` prompt)
- git-keyed row completion, the 2-run retry budget, crash recovery

Unit coverage for every individual path lives in each module's tests plus
`tests/rpc_fake_pi.rs`; this procedure only exercises what unit tests
cannot: the real worker process, the real permission system, and real
commits.

## Prerequisites

- `cargo build` at the repo root; the binary is `target/debug/pi-plan`.
- Spike inner repo seeded (`cd test-fixtures/spike && git init && git add .
  && git commit -m "chore: seed spike fixture"`).
- Run state lives **outside** the repo under `~/.pi-plan/<key>/` (key =
  sanitized `test-fixtures/spike` basename + sha256-8 of the canonical
  cwd). For a clean run, delete that directory — or, cleaner, isolate the
  run with `export PI_PLAN_STATE_DIR=/tmp/pi-plan-e2e` for every command.
  The project directory must stay clean: no `supervisor-state.json`,
  `.pi-plan/`, `.pi-plan-stop`, or `.pi/` ever appear in `test-fixtures/spike/`.
- The `implement-from-plan` skill is installed for pi (default
  `~/.pi/agent/skills/implement-from-plan/`, or `$PI_PLAN_SKILL`);
  `supervise` hard-errors without it (the body is injected into every
  worker's first prompt — see checklist G).
- The `clean-worktree` skill is installed for pi (default
  `~/.pi/agent/skills/clean-worktree/`, or `$PI_PLAN_CLEAN_SKILL`) for
  checklist H items 1–2 and 4; H.3 exercises the documented fallback when
  it is absent.
- The permission-system package is a **soft** prerequisite at runtime:
  workers spawn with `--no-extensions` and no guardrails, plus
  `-e <dir>` only when the package resolves, where `<dir>` is the first
  existing candidate of `$PI_PLAN_PERMISSION_EXTENSION`, then
  `~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`, then
  `~/.pi/agent/npm/node_modules/pi-permission-system`, then
  `~/.pi/agent/extensions/pi-permission-system`. I.1's first out-of-cwd
  prompt is the live proof it still loads and gates; section K proves the
  run also works when none resolves (and that strict mode refuses).
- The verifier persona is optional: `prompts/verifier-persona.md` in the
  working directory is used when present, `$PI_PLAN_VERIFIER_PERSONA`
  overrides the path, and an inline adversarial fallback applies otherwise.
  Section J therefore never hard-requires a separate install.
- `pi` 0.85.1 on `PATH` (or point the worker at it); a configured model.
- The operator works at `test-fixtures/spike/` for every `pi-plan` command.

## Checklist

Each item is a pass/fail.

### A. Happy path through the plan

1. **[ ]** `pi-plan supervise` starts from `test-fixtures/spike/`; a live
   tail (stderr) streams row-1 worker events; a status line prints
   (`row 1 · agent … · turns … · ctx …`).
2. **[ ]** Row 1's in-cwd asks (`mkdir -p out`, then the `write` tool) are
   **auto-approved under always-grant #1**: **no inline dialog** appears
   and stderr shows one auto-approval log line per covered ask. (A bash
   ask with no concrete flagged path — e.g. `git status` — is not covered
   and still prompts; see I.4.)
   **Failure mode:** row 1 completes with no auto-approval log lines, or
   an in-cwd **path** ask that rule #1 covers still renders a dialog —
   STOP and investigate before continuing.
3. **[ ]** The worker commits `feat: add hello file`; `out/hello.txt` exists.
4. **[ ]** The loop advances: a spawn line prints for row 2
   (`docs: finish spike`) with a fresh agent id.
5. **[ ]** Row 2's in-cwd asks auto-approve (no dialog, auto-approval log
   lines); row 2 commits `docs: finish spike`; the README gains the
   spike-run notes.
6. **[ ]** The loop advances to row 3 (`feat: add bye file`); approve the
   dialog; `out/bye.txt` appears; the commit lands.
7. **[ ]** The loop reports the plan complete and exits 0; the final report
   lists all three rows `done` with a `done: 3/3 rows` summary.

### B. Visibility

1. **[ ]** While a worker runs, `pi-plan status` (a second shell) prints the
   live row, agent id, git match state, persisted state, and worktree
   cleanliness.
2. **[ ]** Dialog output goes to stdout; traces, banners, and reports go to
   stderr (the operator can `2>trace.log` and still answer dialogs).
3. **[ ]** The final report includes the result tail and the transcript
   path for each attempt (under `~/.pi-plan/<key>/sessions/`).

### C. Retry, budget, report

1. **[ ]** Force a spent failure: with a worker mid-row, kill the worker
   process (SIGKILL) so the row's commit never lands. The loop records a
   spent run and retries with a **fresh** worker.
2. **[ ]** If a row exhausts its 2-run budget, the loop stops with a report:
   row id, runs used, last outcome, result tails, transcript paths.
3. **[ ]** A completed-without-commit worker (worker ends its turn without
   committing, no ASK) counts as a spent run and triggers the retry.

### D. Interrupts, questions, adjudication

1. **[ ]** `pi-plan stop` mid-run from a second shell: the loop aborts the
   worker at the next boundary and ends with a "stopped" outcome; no further
   worker spawns. A stale `.pi-plan-stop` (under `~/.pi-plan/<key>/`) from a
   killed run is discarded on the next `supervise` start.
2. **[ ]** `restart` as a line command at any prompt aborts the running
   worker and a fresh worker is spawned for the **same** row; the state file
   does not count the interrupted run as spent.
3. **[ ]** ASK question: when a worker ends with `PI_WORKER_STATUS: ASK` and
   a `QUESTION:` line, the loop prints an `answer>` prompt (stdout); the
   answer is folded into a **fresh** worker's prompt (confirm it appears in
   that worker's transcript). Answering after budget exhaustion still works.
4. **[ ]** `pi-plan step <n>` runs only row n; `pi-plan step <n> --answer
   "…"` pre-folds the answer into the first worker.
5. **[ ]** `pi-plan mark <n> done` after a near-miss (or without any commit)
   records the row as adjudicated; the next `supervise` skips it.

### E. Crash recovery

1. **[ ]** Mid-run (worker live), SIGKILL the `pi-plan` binary. Restart
   `pi-plan supervise`.
2. **[ ]** The loop resumes: the in-flight row restarts with `runsUsed`
   intact (a 2-run-per-row budget now spends its second run), or — if the
   row's commit landed before the kill — the loop skips it and continues.
3. **[ ]** Corrupt the state file (`echo '{' >
   ~/.pi-plan/<key>/supervisor-state.json`, or `$PI_PLAN_STATE_DIR/…` when
   overridden) and rerun `supervise` — the loop recomputes from git and
   does not throw.

### F. Project directory stays clean

1. **[ ]** Mid-run (worker live), `ls -a` in `test-fixtures/spike/` shows
   **no** `supervisor-state.json`, `.pi-plan/`, or `.pi-plan-stop`; the run
   state lives under `~/.pi-plan/<key>/` instead.
2. **[ ]** After the run, `git status --short` in `test-fixtures/spike/` is
   clean, and `git status --porcelain --ignored` lists no `!! .pi-plan/`
   entry — the repo needs no runtime-artifact ignore.
3. **[ ]** `pi-plan status` from a second shell reads
   `~/.pi-plan/<key>/supervisor-state.json`, and `pi-plan stop` writes the
   control file there; neither touches the repository.

### G. Skill injection

1. **[ ]** The first worker's transcript (path printed in the final report,
   under `~/.pi-plan/<key>/sessions/`) contains the framed-section phrase
   `has been loaded for you automatically` — the implement-from-plan body
   reached the worker.
2. **[ ]** (optional) The same transcript contains no `read` tool call for
   the `SKILL.md` path — the persona's "do not read the skill file again"
   held.
3. **[ ]** `supervise` with `PI_PLAN_SKILL=/nonexistent` fails fast before
   any spawn with `cannot read the implement-from-plan skill: …` — and
   `status`/`stop`/`mark` still run (they never load the skill).

### H. Dirty-tree recovery (clean-worktree agent)

1. **[ ]** Seed an owner-less stray in `test-fixtures/spike/` — the
tag_tool shape: an untracked expected build artifact in a repo with no
`.gitignore` (`mkdir -p out && touch out/fuzz.dat`), with no
`supervisor-state.json` naming an owner — then run `pi-plan supervise`.
Expect a **`pi-plan-clean-*`** session under
`~/.pi-plan/<key>/sessions/` (count it before the row worker spawns).
2. **[ ]** The clean agent adds a `.gitignore` rule for the artifact and
commits it as a small `chore:` message; `git status --short` is clean
afterwards, the final report shows a `worktree cleaned` attempt record
(marker `0`), and the row then proceeds to its own worker + commit.
3. **[ ]** Missing-skill fallback: run with `PI_PLAN_CLEAN_SKILL=/nonexistent`
**and** no `~/.pi/agent/skills/clean-worktree/` (temporarily moved aside).
The gate aborts exactly as before: the report says "working tree not
clean" with a `clean-worktree skill not installed` tail, nothing spawns,
and no state is written.
4. **[ ]** ASK-answer round trip: force the clean agent to end
`PI_WORKER_STATUS: ASK` with a `QUESTION:` line (e.g. an untracked
`.env`-shaped file the skill cannot attribute — `touch .env` in the spike
repo). The supervise loop prints the `answer>` prompt (TUI modal in a
tty); answer it; a **fresh** `pi-plan-clean-*` session appears whose
prompt contains the folded answer (`The human answered a previous
clean-worktree agent's question:`), the pass completes, and the row
proceeds.

### I. Permission memory and the bare worker set

Run these from `test-fixtures/spike/` with `PI_PLAN_STATE_DIR` isolated
and a fresh state root (or `pi-plan reset-permissions --yes` first).

1. **[ ]** Worker argv is bare: the row worker's spawn line / transcript
   shows `--no-extensions` and `-e <permission-system>` and **never**
   references guardrails (an early out-of-cwd ask prompting is the live
   proof the permission system still loads and gates workers).
2. **[ ]** Out-of-cwd read round trip: one worker runs `cat
   ~/text_file.txt`, a later worker `cd ~; cat text_file.txt`. The first
   ask **prompts**; answer the **"…for this session"** option. The second
   worker's ask is **auto-approved** (no prompt) and the command succeeds;
   `permissions.json` holds **one deduped grant**, and the auto-approval
   did **not** add a second record (D10).
3. **[ ]** In-cwd write (always-grant #1): a covered in-cwd path ask (e.g.
   a `write` inside the repo) is auto-approved — no prompt, one
   auto-approval log line — and `permissions.json` stays unchanged (no
   operator grant accrued). Both TUI and line mode.
4. **[ ]** Non-vacuous guard: alongside the in-cwd path ask, a bash ask
   with **no concrete flagged path** (e.g. `git status`) still prompts —
   never auto-approved (D7).
5. **[ ]** Skills-root read (#2): a read inside the derived skills root
   auto-approves; a **write into the skills tree** still prompts.
6. **[ ]** Skill script (#3): a bash command targeting
   `<skills root>/**/scripts/**` approves once with a plain `Yes` (never
   recorded); a stray script outside the skills tree prompts.
7. **[ ]** Verb-less round trip (D11): a `git status --short` ask granted
   "for this session" records the pattern `git status *`; a later
   worker's `git status` (any spelling) ask auto-approves while a
   `git push` under the same session still does **not** auto-approve from
   that grant.
8. **[ ]** Startup keep/reset prompt (D12): with ≥ 1 grant on file,
   `supervise` opens it — `stop` cancels the run start and exits **2**;
   `restart` is keep-and-proceed; EOF in line mode keeps; `status`/`mark`
   never prompt; a **corrupt** `permissions.json` skips the prompt and
   prints one warn line in the final report.
9. **[ ]** `pi-plan status` shows the stored grant count;
   `pi-plan reset-permissions --yes` removes all grants and prints the
   removed count; without `--yes` a non-`yes` answer cancels and removes
   nothing; a fresh `supervise` then offers **no** keep/reset prompt.

### J. Hole resolution and the reword

Run this in a **scratch** repository (not the spike fixture) with an isolated
state root, e.g. `export PI_PLAN_STATE_DIR=/tmp/pi-plan-hole-e2e` and a fresh
`rm -rf "$PI_PLAN_STATE_DIR"`. The fixture is the incident replay: a 13-row
`TODO.md` whose rows 1–7 and 9–12 are committed with their exact planned
subjects, row 8's commit carries a **drifted** subject (one inserted word)
but its deliverable file exists, and row 13 is still open. Work on a clean
local branch with **no upstream** (e.g. `agent/hole-e2e`).

1. **[ ]** `pi-plan supervise` prints `row 8: hole detected — 4 later
   row(s) complete, verifying`, then `row 8: verified without a commit
   (verifier COMPLETE)`, and proceeds to row 13. **No operator keystroke,
   no budget prompt, no `pi-plan-row-8` session** — count sessions under
   `$PI_PLAN_STATE_DIR/<key>/sessions/` before/after and expect exactly one
   `pi-plan-verify-8`.
2. **[ ]** `supervisor-state.json` contains a `verified` entry for row 8
   (head, verdict `complete`, best commit, evidence) and a `verifiedCache`
   entry; `decisions.jsonl` has one `hole-verified` record with the
   successors, planned/best subject, tier, ratio, verdict, verifier facts,
   and `action: skip`; `worker-stats.jsonl` has a `v: 2` record with
   `"kind": "verification"` and **no** row-budget change.
3. **[ ]** `pi-plan status` shows `holes: [..]` gone, `row 8 …: verified (no
   matching commit)`, `verified: [8]`, and a next-action hint for the open
   row 13.
4. **[ ]** Restart idempotence on the unchanged `HEAD`: a second
   `supervise` spawns **no** verifier and asks nothing for the cached
   `COMPLETE`; `status` still shows row 8 verified with its evidence.
5. **[ ]** Cached non-`COMPLETE`: force an `INCOMPLETE`/`UNCERTAIN` verdict
   (e.g. drift the subject and remove the row-8 deliverable so triage still
   runs the verifier), then restart on the same `HEAD`. Expect **no spawn**
   and **one** operator prompt re-presenting the cached verdict; `s`/EOF
   exits 2. Moving `HEAD` invalidates the cache and a new spawn is allowed.
6. **[ ]** Escalation ordering: with a candidate commit, row 8's budget
   already spent, and the verifier escalating, the **unrecognised-row
   prompt appears before any budget prompt** and offers `m`/`w`/`r`/`s`.
   `[w]` runs row 8 exactly once with the supervisor hole section in its
   prompt; if the worker ends `PI_WORKER_STATUS: ASK`, the deterministic
   label returns with `[r]` and **no free-text answer**; a second `[w]` is
   not offered ("the worker asked again; re-running it will not help").
7. **[ ]** `[m]` writes `adjudicated` and continues; `[w]` spends no row
   budget; `[s]`/blank/EOF stops with the exit-2 report and the chosen
   action named.
8. **[ ]** Reword, happy path: on the clean local branch, `[r]` rewrites
   exactly the drifted row-8 commit. `git log --format=%s` still contains
   the planned row-8 subject and every later row's subject; the target's
   tree is byte-identical before/after (`git diff <old>^{tree}
   <new>^{tree}` is empty); the final report/`decisions.jsonl` records the
   action; the next pass matches row 8 via git with **no** `verified` entry.
9. **[ ]** Reword refusals: on `main`/`master`, with an upstream, with a
   merge in range, with a merge target, and with a dirty tree, the `[r]`
   action is absent and the label names the failed guard (`Reword
   unavailable: …`). A mid-sequence failure (e.g. a failing hook) aborts
   the rebase, restores the recorded head, prints git's stderr, writes a
   `reword-failed` decision, and stops with the report.
10. **[ ]** No false holes: a plan whose trailing rows are unmatched
    (rows 1–8 matched, 9–12 open) reports no hole; a fully matched plan
    reports no hole; a plan with no `## Steps` table is unaffected.
11. **[ ]** Plainly unimplemented: make row 8 have no candidate commit and
    no existing deliverable, then supervise. Expect **no verifier spawn**
    and a label saying row 8 does not appear to be implemented (the
    `PlainlyUnimplemented` variant).
12. **[ ]** Verifier failure: force a verifier spawn failure or its ceiling
    (e.g. an unusable verifier model). Expect no state skip, the label
    "the verifier could not run: …", the operator choices, and `s`/EOF at
    exit 2.
13. **[ ]** Provenance durability (defect-4 replay): `pi-plan mark 8 done`
    (or `[m]`), then edit `TODO.md` (any change), then run `supervise` —
    row 8 stays adjudicated and is not re-asked.
14. **[ ]** `pi-plan supervise --row 9` and `pi-plan step 9` never enter the
    hole path: row 8 is not detected, no verifier spawns, and the requested
    row is the only one run.

### K. Working without a permission system

Run in `test-fixtures/spike/` with `PI_PLAN_STATE_DIR` isolated. **Creating
the absent-package condition (D13):** on a machine that *does* have the
package, simulate absence with a throwaway home — a bare temp `$HOME` still
needs the hard-required skill, so point `$PI_PLAN_SKILL` at the real one.
Capture `$RealSkill` in a preceding command, **not** inline as
`PI_PLAN_SKILL="$HOME/…"`: bash applies prefix assignments left to right, so
an inline `$HOME` there already expands to `FakeHome` and the run would fail
the skill prerequisite before reaching K.1.

```bash
FakeHome=$(mktemp -d)
RealSkill="$HOME/.pi/agent/skills/implement-from-plan"   # expand BEFORE overriding HOME
HOME="$FakeHome" \
  PI_PLAN_SKILL="$RealSkill" \
  PI_PLAN_STATE_DIR=/tmp/pi-plan-e2e \
  pi-plan supervise
```

Every item names its condition: **absent-home** = the command above,
**bad-env** = the real home with `PI_PLAN_PERMISSION_EXTENSION=/tmp/nope`.

1. **[ ]** K.1 *(absent-home, `PI_PLAN_PERMISSION_EXTENSION` unset)*
   `pi-plan supervise` prints `model: …` then
   `permissions: DISABLED — no pi-permission-system package found (searched
   …)` plus the indented ungated line naming `bash/edit/write`, and
   **proceeds** (no exit 1). No permission dialog ever appears; rows complete
   on their commits.
2. **[ ]** K.2 *(bad-env)* → `permissions: DISABLED —
   $PI_PLAN_PERMISSION_EXTENSION points at /tmp/nope, which does not exist`,
   run proceeds.
3. **[ ]** K.2b *(real home, package installed,
   `PI_PLAN_PERMISSION_EXTENSION=/tmp/nope`)* → `permissions: gated (the
   pi-permission-system package at …; … points at /tmp/nope, which does not
   exist and was ignored)` — the ignored env override is named (D11).
4. **[ ]** K.3 *(absent-home, `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=1`)* →
   `pi-plan: cannot resolve the permission-system extension: …` on stderr,
   exit 1, **no** worker spawned, no session/file created, **no**
   `permissions:` line printed.
5. **[ ]** K.4 Strict precedence: a project `supervisor.config.json` with
   `{"requirePermissionExtension": true}` + `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=0`
   → degrades (env wins); the same config alone → refuses; with the package
   installed the gated line reads `…; strict` when the config key is `true`.
6. **[ ]** K.5 Regression with the package installed: `permissions: gated (the
   pi-permission-system package at …)` and an out-of-cwd ask still relays a
   dialog (the existing I.1 item).
7. **[ ]** K.6 `pi-plan status` / `stop` / `mark` / `reset-permissions` are
   unaffected in degraded mode (no new output, no new failure); `step N` and
   `supervise --row N` (same `cmd_supervise`) print the same `permissions:`
   line as `supervise`.
8. **[ ]** K.7 *(absent-home, `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=ture`)* →
   the run degrades **and** prints an extra line naming the value:
   `permissions: ignoring unrecognized
   $PI_PLAN_REQUIRE_PERMISSION_EXTENSION=ture (strict mode follows the
   config: off)` (D10).
9. **[ ]** K.8 *(absent-home with ≥ 1 grant on file from an earlier gated
   run)* the startup keep/reset prompt still appears; `restart` proceeds and
   the grants are inert (no dialog, no auto-approval); a later gated run still
   honours them.

## Pass criteria

- Every checklist item passes with the real worker, real forwarding, real
  commits.
- Degraded mode (section K) prints the two `permissions: DISABLED` lines,
  spawns **no** permission dialog, and completes rows ungated; strict mode
  refuses with the old message prefix, exit 1, and no worker, session, or
  `permissions:` line.
- No worker process is left behind after the run (check with
  `ps aux | grep 'pi --mode rpc'`).
- The run-state file (`supervisor-state.json` under `~/.pi-plan/<key>/`)
  never contradicts git history (verify with `git log --oneline` after each
  section).

## Results

| Date | Operator | Result | Notes |
| --- | --- | --- | --- |
| — | — | ⏳ PENDING — first run after Steps 1–8 landed | — |
| — | — | ⏳ PENDING — section J after hole resolution landed | Scratch repo, incident replay + reword |
| — | — | ⏳ PENDING — section K after the optional permission system landed | Absent-home degrade, bad env, strict precedence, ungated grants |
