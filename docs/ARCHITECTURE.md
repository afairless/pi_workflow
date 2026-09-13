# Architecture — pi-plan

## Overview

`pi-plan` is a deterministic orchestrator that supervises the TODO.md
workflow. For each open row it spawns a fresh `pi --mode rpc` worker
process, monitors the worker's JSONL event stream, answers permission
dialogs inline over the extension-UI sub-protocol, and decides completion
from git history (never from worker claims alone).

The orchestrator is plain code — no LLM sits in the control loop. A stop is
just a stop. The repository (`TODO.md`, `docs/research/*.md`, git history)
is the source of truth; `pi-plan` only reads it and writes one small state
file (`supervisor-state.json`) for crash recovery. All run state — the
state file, per-worker session JSONL, the worker stderr log, and the
one-shot stop control — lives **outside** the repository under
`~/.pi-plan/<project-key>/` (`src/storage.rs`), so a supervised plan leaves
the project directory clean.

## Module map

```text
src/
  main.rs        binary entry: clap dispatch + wiring; the
                 QuestionPause seam impl and the render task
                 (the interactive loop driver lives in
                 supervise/)
  config.rs      supervisor.config.json schema + precedence   ✔ Step 3
  todo.rs        TODO.md row contract parser                  ✔ Step 2
  git.rs         git facade + git-keyed completion matcher    ✔ Step 2
  prompt.rs      worker prompt builder (ASK contract)         ✔ Step 3
  state.rs       supervisor-state.json (crash recovery)       ✔ Step 3
  storage.rs     pure run-state root resolution (external
                 ~/.pi-plan/<project-key>/) + the atomic
                 worker-stats.jsonl audit writer — a leaf
                 (std + serde + sha2 only)
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
3. **Worker prompt contract** — the prompt (`src/prompt.rs`) names the
   project, plan source, TODO.md path, the exact row text, and the planned
   commit message. The worker ends its final message with
   `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` (plus `QUESTION: <text>` when
   asking). The marker is a **hint** — completion classification stays
   git-keyed and questions are classified from the ASK marker + question
   line.
4. **Run/retry/ask state machine** — see below (`src/supervise/`).
5. **Crash recovery** — `supervisor-state.json` under the external
   `~/.pi-plan/<project-key>/` root (`src/storage.rs` resolves the same key
   for every command; `$PI_PLAN_STATE_DIR` overrides the base), written
   atomically (temp file + rename), best-effort (a failed write never
   crashes the loop).

## The supervise loop (Contract 4)

Per-row budget: **2 runs** (initial + one automatic retry). A question
pause, a stop, and a restart spend nothing; every other terminal event
spends one run.

```
spawn worker (fresh pi --mode rpc process)
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
       save; retry fresh worker if runsUsed < 2, else stop + report
```

An operator-provided answer is a user-driven continuation and always gets
its run, even after the budget is exhausted. A restart keeps the row's
budget untouched (the fresh worker still runs under the same attempt).

## Dirty-WIP gate (Step 7): refuse strays, resume owned WIP

At the top of every attempt the loop reads `git status --short`:

- **Clean tree** — no gate.
- **Dirty tree, no owner** — no recovered state, or the recovered state
  does not name this row, or its `lastOutcome` is a legacy refusal marker
  (`dirty` / `spawn-error`). The loop hands the tree to a dedicated
  **clean-worktree agent** instead of aborting on sight (section below);
  a clean pass that fails — or a missing clean-worktree skill — refuses
  with `DirtyWorktree`, spawns nothing, and writes **no state**, so
  repeated refusals cannot inflate `runsUsed`.
- **Dirty tree, owned by this row** — the recovered state names this row
  with a live outcome (`running`, `question`, `failed`, ...). The loop
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
                                  // near-miss|stopped|spawn-error|dirty|budget
  "adjudicated": [7],             // rows the human marked done (preserved
                                  // across every loop save)
  "agentId":     "0",             // optional; last live worker (spawn-time)
  "startedAt":   1750000000000    // optional; epoch ms of that spawn
}
```

Where it lives — every command resolves the same external root
(`~/.pi-plan/<project-key>/`, or `$PI_PLAN_STATE_DIR` when set) with
`ProjectStorage::resolve` (`src/storage.rs`): the project key is the
sanitized cwd basename plus the first 8 hex chars of sha256 over the
canonicalized cwd, so symlinked views of one project share a key and the
project directory stays clean (no `supervisor-state.json`, `.pi-plan/`, or
`.pi-plan-stop` ever appear in the repo). The state file, per-worker
session JSONL (`--session-dir`), the spawned `pi` stderr log
(`worker-stderr.log`), and the one-shot `.pi-plan-stop` control file all
live under that root.

Recovery rules (`src/state.rs`):

- No file / unparseable / wrong shape → recompute from git (never throws).
- `planHash` mismatch → recompute (the plan changed).
- The state's `currentRow` is already matched in git → recompute (a stale
  file must not contradict the repo).
- Otherwise resume with `runsUsed` intact.

Writes are atomic and best-effort: a kill mid-write cannot corrupt the
recovery input.

## Sourcing and data flow

```
repository (TODO.md · docs/research · git history)
   │  read: todo.rs → row contract · git.rs → completion facts
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

Workers run with the same global permission config as the interactive pi
(`~/.pi/agent/extensions/…`, `settings.json` packages) — dialogs are never
auto-approved. When a worker's permission system resolves an `ask`:

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

## Operator surface (Step 8)

The CLI (`cli.rs` + `main.rs`) is clap-derived: `supervise [--row N]
[--answer "…"] [--config PATH]`, `status`, `stop`, `mark <n> done`,
`step N [--answer …]`.

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

**Live-only stats and the idle line.** The persistent TUI header/footer
(`header_lines` / `format_footer_line`) show statistics for the **live**
worker only. `WorkerSnapshot.terminal` exposes liveness, `WorkerView.live`
follows it, and `tui_update_view` skips completed workers (a snapshot with
`terminal` set never overwrites the slot), so a finished worker stops
driving the display. The idle transition itself is driven by the
row-terminal hook, not a late snapshot: `SuperviseServices.on_row_terminal`
(one structured call site in `report_terminal`, so line and TUI mode both
get every terminal event) wakes `TuiState::note_row_terminal`, which
stores `last_terminal: (row, completed|failed)` and **flips the displayed
view not-live**. Between rows the footer becomes a supervisor-status line
`idle · last: row N <kind> · next: row M — unit` (next row's unit from the
plan-seeded `TuiState.plan_units`; `next: row N` only when unknown), and
the header drops the stats context while keeping the step banner
(row/total/unit). Multiple concurrently **running** workers remain
last-writer-wins (a valid rotation); a round-robin tick over live workers
is future work.

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
with work outstanding (stopped / question / near-miss / budget).

## Limitations / future work

- **Sequential rows only** — no parallel workers (matches the TS
  supervisor).
- **Shelling to `git`** — the facade (`src/git.rs`) mirrors the TS
  `spawnSync`; `git2`/`gix` is a deferred swap.
- **No auto mid-step respawn from context%** — compaction banner + manual
  `restart` is the v1 bridge.
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
