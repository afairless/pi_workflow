# Architecture — pi-plan

## Overview

`pi-plan` is a deterministic orchestrator that supervises the TODO.md
workflow. For each open row it spawns a fresh `pi --mode rpc` worker
process, monitors the worker's JSONL event stream, answers permission
dialogs inline over the extension-UI sub-protocol, and decides completion
from git history (never from worker claims alone).

The orchestrator is plain code — no LLM sits in the control loop. A stop is
just a stop. The repository (`TODO.md`, `docs/research/*.md`, git history)
is the source of truth; `pi-plan` only reads it and writes two small state
files (`supervisor-state.json` for crash recovery, `permissions.json` for
the dialogs' session grants — Permission memory below). All run state —
the state files, per-worker session JSONL, the worker stderr log, and the
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
  config.rs      supervisor.config.json schema + precedence; the
                 global scaffold pin DEFAULT_GLOBAL_MODEL and
                 default_global_config_json builder          ✔ Step 3
  todo.rs        TODO.md row contract parser                  ✔ Step 2
  git.rs         git facade + git-keyed completion matcher    ✔ Step 2
  prompt.rs      worker prompt builder (ASK contract)         ✔ Step 3
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
3. **Worker prompt contract** — the prompt (`src/prompt.rs`) names the
   project, plan source, TODO.md path, the exact row text, and the planned
   commit message. The worker spawns with a pinned argv (Contract 3b,
   `src/worker.rs`): `--mode rpc --session-dir <root>/sessions
   --name pi-plan-row-<n> --model <model> --thinking high --approve
   --tools read,grep,find,ls,bash,edit,write --skill <implement-from-plan>
   --no-extensions -e <permission-system dir>` plus `--append-system-prompt
   <persona>`. `--no-extensions` disables extension discovery while the
   explicit `-e` still loads the permission system, so `pi-guardrails` and
   every settings-package extension never load in workers — each access is
   decided solely by the permission system's relayed dialogs (Permission
   memory below). **Bare-worker invariant:** with `--no-extensions` a worker
   loads exactly one extension — the permission system — so pi-lens-hosted
   behaviors (unified LSP, lens, autoformat, autofix, the write-time test
   runner, the opengrep auxiliary scanner, the knip/madge/jscpd family)
   never exist in a worker regardless of flags; determinism comes from the
   extension set, never from pi-lens flags (the six `--no-*` pi-lens flags
   do not exist in a bare worker's parser and must stay out of the argv). The permission package
   resolves `$PI_PLAN_PERMISSION_EXTENSION` first, else the default install
   under `~/.pi/agent/npm/node_modules/@gotgenes`; an unresolvable package
   fails the run fast before any worker spawns (mirrors the skill
   prerequisite). The worker ends its final message with
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

Config is resolved once at startup (`cmd_supervise`) in four tiers:
`--config PATH` (absolute) > `<cwd>/supervisor.config.json` (presence-based)
> the XDG global `~/.config/pi-plan/supervisor.config.json` (auto-created
when absent, see below) > built-in defaults. `resolve_config`
(`src/cli.rs`) returns a `ResolvedConfig { config, source }`; the startup
line prints the resolved model + source. `status`/`stop`/`mark` never
resolve config. The global file is configuration (XDG,
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
       save; retry fresh worker if runsUsed < 2, else stop + report
```

An operator-provided answer is a user-driven continuation and always gets
its run, even after the budget is exhausted. A restart keeps the row's
budget untouched (the fresh worker still runs under the same attempt).

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
(`worker-stderr.log`), the one-shot `.pi-plan-stop` control file, and the
permission store (`permissions.json` — Permission memory) all live under
that root.

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

Writes are atomic and best-effort: a kill mid-write cannot corrupt the
recovery input.

## Sourcing and data flow

```text
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

Workers run **bare**: only the resolved permission system is loaded
(`--no-extensions -e <package>`), so one permission system — and nothing
else — gates every worker access. Covered asks are auto-approved by the
supervisor (Permission memory below); everything else is relayed — never
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
