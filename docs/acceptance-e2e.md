# End-to-End Acceptance — pi-plan

Run the orchestrator against `test-fixtures/spike/` (3 rows) with the
**real** `pi --mode rpc` worker, real permission forwarding answered inline,
and real commits.

**Status:** ⏳ PENDING — first run after Steps 1–8 landed. Record each run in
the Results table at the bottom.

## Scope

This is the acceptance gate for the `pi-plan` orchestrator (Step 9 of
`docs/research/plan-rust-orchestrator.md`). It proves the whole wiring under
one roof:

- the real `pi-plan` binary (clap CLI: `supervise`/`status`/`stop`/`mark`/
  `step`/`reset-permissions`)
- a fresh `pi --mode rpc` worker per row (Contract 3b argv: bare
  `--no-extensions -e <permission-system>` + persona, determinism flags,
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
- The permission-system package resolves for the **bare worker set**:
  workers spawn with `--no-extensions -e <dir>` and no guardrails, where
  `<dir>` is `$PI_PLAN_PERMISSION_EXTENSION` or the default install at
  `~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`. I.1's
  first out-of-cwd prompt is the live proof it still loads and gates.
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

## Pass criteria

- Every checklist item passes with the real worker, real forwarding, real
  commits.
- No worker process is left behind after the run (check with
  `ps aux | grep 'pi --mode rpc'`).
- The run-state file (`supervisor-state.json` under `~/.pi-plan/<key>/`)
  never contradicts git history (verify with `git log --oneline` after each
  section).

## Results

| Date | Operator | Result | Notes |
| --- | --- | --- | --- |
| — | — | ⏳ PENDING — first run after Steps 1–8 landed | — |
