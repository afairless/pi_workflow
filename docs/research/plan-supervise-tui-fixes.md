# Plan: Supervise TUI fixes — Ctrl-D kill switch, flowing token stream, bottom-anchored permission prompts

## Status

A research-to-build plan for `pi_plan_workflow`, a follow-up to
`docs/research/plan-tui-display.md` (steps 1–7 of which are implemented and
merged). It fixes three operator-visible defects in the full-screen
supervise TUI (`src/tui.rs` + wiring in `src/main.rs`/`src/ui.rs`):

1. **Kill switch** — Ctrl-D must terminate the TUI *and every Pi session
   supervise initiated* (leaving unrelated Pi sessions alone), matching
   Pi's own Ctrl-D-quits behavior.
2. **Flowing tokens** — token entries must continue *on the same line* as
   the previous entry (no per-token rows, no `⟦thinking: …⟧` wrappers), and
   the display must respect `\n` characters in the generated text (matching
   Pi's line flow).
3. **Bottom-anchored permission prompts** — a permission dialog must appear
   at the *bottom* of the screen, directly above the persistent footer, with
   the most recently generated text visible above it while the operator
   decides. Today the centered modal replaces the trace, hiding the text.

Design decisions were locked with the user on 2026-09-12 via the Q&A below.
No code was changed during this investigation.

## Goal (user requirements)

1. "Add a 'kill switch' that terminates the TUI and any and all running Pi
   sessions that it is running (but leave Pi sessions that supervise did
   not initiate alone). To match Pi, the user can terminate the application
   with Ctrl-d."
2. "Each new token entry should be displayed immediately to the right and
   on the same line as the previous entry, so that the user can read a
   sentence as unbroken text. … the generated text has newline characters
   (which are not now explicitly displayed) … The TUI display should respect
   those."
3. "When a permission prompt appears, it should appear at the bottom of the
   screen (above the existing footers, so it does not occlude them), and the
   most recently generated text should appear above it."

## Locked decisions (Q&A, 2026-09-12)

| # | Question | Decision |
|---|---|---|
| 1 | When a permission/ASK modal is open, what does Ctrl-D do? | **Always kill.** Ctrl-D anywhere in the TUI (modal open or not) terminates the app + all supervise-spawned workers. Dialog dismissal moves to typing `c`/`cancel` (already supported by `ui::reply_from_input`; `c`/`cancel` → `UiReply::Cancelled`). The TUI's `^D → Cancelled` EOF-parity guarantee is replaced by the kill switch. |
| 2 | After Ctrl-D kills the workers and restores the terminal, how does the process exit? | **Final report + exit code 2.** Print the standard final report (rows aborted/stopped) on the primary buffer, exit 2 (work outstanding) — consistent with today's graceful stop. |
| 3 | How should thinking deltas be displayed once tokens flow? | **Separate gray block.** Thinking tokens form their own gray block (no `⟦thinking: …⟧` wrapper); when the thinking run ends, the message text starts on a **fresh line**. Prose stays unbroken within each block; reasoning stays visually separate from the answer (color remains the sole distinction: `thinkingText` gray vs `text` default). |
| 4 | Should inline `[tool: …]` delta markers surface now? | **Keep tool rows.** `ToolCallStart/Delta/End` deltas stay suppressed (as today); tools keep appearing as separate `tool: name (id)` rows (`LineKind::Tool*`) between text blocks. |

Scope note: the kill switch is **TUI mode only** (the requirement says "the
TUI"). Line mode keeps today's byte-identical behavior, where Ctrl-D at a
prompt is EOF (dismiss dialog / stop without an answer). Line-mode output
is never altered by any of the three fixes.

## Current state (evidence)

All references are to `HEAD` (`ce29de8 polish: apply theme styling…`).

### Issue 1 — no kill switch; Ctrl-D only dismisses modals

- `src/tui.rs::input_task` (~line 1328) is the **single stdin owner** in TUI
  mode. Its `Keystroke::CtrlD` arm closes any open modal with `eof_outcome`
  (dialog → `UiReply::Cancelled`, ASK → no answer). With **no modal open,
  Ctrl-D does nothing** (`if let Some(outcome) = outcome` is `None`).
  The `got == 0` arm (~line 1273) keeps EOF parity for a closed stdin, and
  must stay that way (the terminal went away; the run keeps supervising).
- Ctrl-C today is the graceful path: `input_task` flips a `ctrl_c: Arc<AtomicBool>`
  which `main.rs::ctrl_c_watcher` mirrors into `RunControl.stop_requested`.
  A mid-stream Ctrl-C does **not** abort the worker — it takes effect at the
  next terminal event or boundary (`src/supervise.rs` ~lines 494/671). The
  kill switch is precisely the missing *hard* stop.
- Killing a worker already exists and is process-group-scoped:
  - `RpcClient::spawn` runs `cmd.process_group(0)` (`src/rpc.rs` ~line 266),
    so each `pi --mode rpc` peer leads its own process group.
  - `RpcClient::kill()` → `killpg(peer_pid, SIGKILL)`; `Drop for RpcClient`
    does the same synchronously (`src/rpc.rs` ~lines 352–390). Killing the
    group kills the peer and every descendant (bash, editors, nested tools),
    and **only** that group — Pi sessions supervise did not spawn are
    unrelated process groups and are untouched, satisfying requirement 1's
    "leave Pi sessions that supervise did not initiate alone" by construction.
  - `WorkerPort::dispose()` (`src/worker.rs` ~line 721) iterates every live
    worker and kills it; `cmd_supervise` already calls `workers.dispose()`
    on teardown (idempotent — kills whatever is still live).
- `RunControl` (`src/supervise.rs` ~line 125) holds only
  `restart_requested`/`stop_requested`; no kill flag exists. A field can be
  added safely: every construction site is `RunControl::new()` (verified:
  no struct-literal sites outside the type itself).
- Exit path on stopping during an ASK pause: `cmd_supervise` breaks with
  `final_result` still `None` → `Err("supervise ended without a result")`
  (a pre-existing quirk in both modes, preserved so far). The kill path
  must not land there.

### Issue 2 — each token chunk becomes its own row

- `src/ui.rs::apply_delta` (per `MessageDelta`) returns one `TuiLine` per
  delta: `TextDelta` → `LineKind::Text` with the raw chunk; `ThinkingDelta`
  → `LineKind::Thinking` with `thinking_chunk(delta)` = `⟦thinking: …⟧`
  (the metadata *and* the brackets the user sees).
- `src/main.rs::worker_tail` (`RpcEvent::MessageUpdate` arm ~line 718) calls
  `state_mut.push_line(chunk)` per delta in TUI mode.
- `TuiState::push_line` appends each `TuiLine` to the ring
  (`src/tui.rs` ~line 1062); `trace_lines` renders **every ring entry on its
  own row** (`src/tui.rs` ~line 159). Hence one row per token.
- `wrap_text` (`src/tui.rs` ~line 506) splits words on any ASCII whitespace
  **including `\n`** — existing newlines in generated text are silently
  collapsed to single spaces (the "not now explicitly displayed" part), and
  consecutive newlines (paragraph breaks) lose their blank-row gap.
- The `text` buffer inside `worker_tail` is maintained by `apply_delta` and
  is not otherwise consumed in TUI mode (the visible footer view comes from
  the port's own `SnapshotAcc`); it can stay as-is.

### Issue 3 — modal replaces the trace

- `compose_frame` (`src/tui.rs` ~line 1128): when `state.modal` is `Some`,
  the **entire viewport** is replaced by `modal_box(...)` output — the trace
  "disappears" while the operator decides. `modal_box` (~line 390) centers
  the box vertically (`top = (height - box_h) / 2`) and pads to fill the
  viewport.
- The persistent footer is rendered after the viewport, so it is *not*
  occluded today — but the dialog sits **in the middle**, and no text is
  visible above it, which is exactly the complaint.
- `dialog_box` (~line 198) is a second centered-box helper used only by its
  own tests (production `compose_frame` uses `modal_box`); it is dead code
  and should be removed along with this change rather than maintained.

## Design

### Fix 1 — Ctrl-D kill switch

New state and a watcher, following the existing Ctrl-C wire pattern:

- **`RunControl` gains `kill_requested: AtomicBool`** (initialized `false`
  in `RunControl::new`). It is the durable "the operator pressed ^D"
  record: written by the kill watcher and read **once** by the ASK pause
  flow to distinguish a kill-powered `Stop` (this Ctrl-D) from a graceful
  one (Ctrl-C / the `.pi-plan-stop` file) — it is that disambiguation's
  only reader, which is what earns the field. No clearing is needed: the
  ASK flow that reads it breaks the loop immediately after.
- **A `kill: Arc<AtomicBool>`** is created in `cmd_supervise` and passed to
  `input_task` (alongside the existing `ctrl_c`).
- **`input_task`'s `Keystroke::CtrlD` arm** becomes: set `kill` (once);
  if a modal is open, close it with `ModalDecision::Close(ModalOutcome::Stop)`
  so the awaiting flows (dialog round trip, ASK pause) unwind on the
  existing stop path and never deadlock; otherwise push a banner
  (`"^D — killing the run…"`) into the ring. The `got == 0` (stdin closed)
  arm keeps its current EOF parity — that is the "terminal went away" case,
  not the operator's key. Note the modal is closed with the **same** `Stop`
  the Ctrl-C arm uses; the ASK flow disambiguates via `kill_requested`
  (below), so Ctrl-C at an ASK pause stays byte-identical (review F2).
- **A new `main.rs::kill_watcher(kill, control, workers)` task** (spawned
  after `workers` exists, beside `stop_watcher`): on the `kill` flag it sets
  `control.kill_requested` and then `control.stop_requested` **before**
  `workers.dispose().await` — SIGKILLing **every** supervise-spawned worker
  process group — then returns. **Flag order matters (review F1):** with
  `stop_requested` flipped first, the boundary check (`supervise.rs` ~line
  494) blocks any new spawn before the kill lands, and the interrupt check
  (~line 671) classifies the killed worker's `ProcessExit` as
  `Stopped`/`Aborted` instead of an unforced `Failed`/`Spent` that would
  spend budget and mislabel the report row. Disposing first (the draft's
  original order) left a window where the death was classified before the
  flag flipped. Poll cadence ~50 ms, so **at most one spawn can slip in
  inside that single poll window**; a fresh spawn already in flight is
  killed by the watcher's own `dispose`, and the teardown
  `workers.dispose()` in `cmd_supervise` closes the last gap — note in the
  commit.
- **Mid-stream kill works because `dispose` SIGKILLs the peer**: the RPC
  read loop ends, `pump_task` records `TerminalEvent::ProcessExit`,
  `await_terminal` resolves, and the interrupt check sees
  `stop_requested` → `RowOutcome::Stopped` → `run_plan` unwinds.
- **Exit behavior** (locked Q&A 2 — final report + exit 2):
  - Kill during a row: `run_plan` returns a result with stopped outcomes →
    `final_result = Some(result)` → normal report, exit code 2. Unchanged
    code path.
  - Kill during an ASK pause: the modal closes with `Stop`, the ASK handler
    sets `stop_requested` and breaks. **Fix the pre-existing quirk, scoped
    to the kill only (review F2):** before `break`, when the modal outcome
    was `Stop` **and `control.kill_requested` is set**, set
    `final_result = Some(result)` (the result is in scope and holds the
    `QuestionPause` outcome) so the report prints and exit is 2 instead of
    the `Err("supervise ended without a result")` path. Because Ctrl-C and
    the `.pi-plan-stop` file close the ASK modal with the *same* `Stop`
    outcome, the `kill_requested` check is what keeps them byte-identical
    — Ctrl-C at an ASK pause keeps today's `Err` path (locked decision,
    review F2). Plain EOF/blank at an ASK pause keeps today's behavior
    byte-for-byte.
- **Why "leave Pi sessions supervise did not initiate alone" holds**:
  `dispose` → `RpcClient::kill` → `killpg` only on the process groups
  created by `RpcClient::spawn` (`process_group(0)`). Interactive Pi
  sessions, other supervisor runs, and arbitrary user processes live in
  other groups and are never signaled. The **supervise process itself does
  not die** on Ctrl-D — it unwinds the TUI, prints the report, and exits 2,
  which is what "terminates the TUI … Ctrl-d" means for our binary (Pi's
  Ctrl-D quits the app; ours quits the supervise app after a clean report).
- **Line mode**: unchanged (documented scope). Ctrl-D at a line-mode prompt
  remains EOF.

### Fix 2 — flowing token stream (separate gray thinking block)

Replace "one ring row per delta" with "one **open line** that tokens append
to, closing at `\n` and at kind changes":

- **`TuiState` gains `stream: Option<StreamLine>`** where
  `StreamLine { kind: LineKind, text: String }` and `kind ∈ {Thinking,
  Text}`. The open line is the *unfinished* tail of the trace.
- **New `TuiState::append_stream(kind, text)`** (pure, unit-tested):
  1. Kind change vs. the open line's kind (**Thinking ↔ Text**) → flush the
     current open line into the ring first and start a fresh open line of
     the new kind (locked Q&A 3: separate blocks; message text always
     starts on a new line after a thinking block).
  2. Split `text` on `\n` (normalize `\r\n` → `\n`): each non-final segment
     closes the current line (min length 0 — an empty segment produces a
     blank ring row, preserving paragraph gaps and rendering trailing
     newlines); the final segment becomes the new open line of the current
     kind.
  3. Cap the open line's length (e.g. 10 000 chars for a `\n`-less blob):
     flush a truncated line + push a `Banner` note ("…truncated"), reopening
     empty. Guards ring lines from growing pathological in memory.
- **Flush rule**: `push_line` and `push_banner` (and `set_plan`, per-row
  boundary) first flush the open line, so tool rows / turn separators /
  banners / row boundaries never interleave mid-stream. `open_modal`/
  `close_modal` do **not** flush — the in-progress text stays visible above
  the prompt (required by fix 3).
- **`worker_tail` routing** (TUI branch of `RpcEvent::MessageUpdate`):
  replace `state_mut.push_line(chunk)` with
  `state_mut.append_stream(kind, raw_delta)` where kind/raw come from a new
  pure helper **`ui::stream_part(delta) -> Option<(LineKind, String)>`**
  returning the *unwrapped* text for `TextDelta`/`ThinkingDelta` (no
  `⟦thinking: …⟧` wrapper; the wrapper helper `thinking_chunk` and
  `apply_delta` stay untouched for line mode, which keeps its byte-exact
  `eprintln` per chunk). The per-tail `text` buffer continues to be
  maintained through `apply_delta` (harmless, already unused in TUI mode).
- **Rendering**: `compose_frame` appends the open line as a virtual last
  logical line (`ring + [stream]` when `stream` is `Some`), then the
  existing `trace_lines` word-wraps each logical line by kind. Because a
  logical line never mixes kinds (the flush-on-kind-change rule), each
  logical line has **one** style — `trace_lines`/`style_for_kind` need no
  mixed-segment support. `wrap_text`'s `\n` word-split becomes inert for
  stream lines (they have no `\n` by construction); its width wrap remains
  the "right edge" break.
- **Blank lines**: `wrap_text("")` returns `[]` today; `trace_lines` must
  emit a blank padded row for empty logical lines so paragraph gaps (double
  `\n`) actually show. Property tests already guarantee wrapped rows never
  exceed `width`; keep them.
- **Idle tail**: after the final terminal event the open line is flushed by
  the after-flush events (`TurnEnd`/`AgentSettled` banners); a residual
  empty open line renders as a blank row until the run ends — cosmetic and
  matches the text's own trailing newline. Cosmetic caveat (review F5):
  text ending in `\n\n` renders **two** blank rows (one ring blank from the
  empty non-final segment + the residual open empty line) where the
  paragraph break suggests one — confirmed at the manual gate.

### Fix 3 — bottom-anchored permission prompts

Split the modal from the trace instead of overlaying it:

- **`modal_box` becomes bottom-anchored**: it returns *exactly* the box's
  `box_h = content.len() + 4` rows (top border + content + note row + input
  row + bottom border), truncated from the top when `box_h > height` (so it
  still never exceeds the viewport), with no vertical centering and no
  viewport padding. The "reserved dim note row, always present so the box
  never jumps" behavior is kept.
- **`compose_frame` modal branch** becomes two stacked pieces:
  - `trace` rows for the first `max(viewport_h - box_h, 0)` rows —
    bottom-anchored (`trace_lines` window), so the **most recently generated
    text (including the open stream line) sits directly above the box**;
  - the `modal_box` rows (possibly with leading blank-fill rows) for the
    remaining `min(box_h, viewport_h)` rows, placed at the **bottom** of the
    viewport region, i.e. directly above the 1-row footer.
  - When `box_h >= viewport_h` the box fills the viewport (no trace above —
    unavoidable, dialogs are short); the footer is never touched.
- The footer path is unchanged, so the "above the existing footers" and
  "does not occlude the footers" requirements hold by construction.
- **Remove the dead `dialog_box` helper** (its only callers are two of its
  own tests) and update the module doc-comment boxes; `modal_box` is the
  only dialog renderer.

### Parity table (TUI mode, before → after)

| Key | Today | After |
|---|---|---|
| Ctrl-D, no modal | nothing | **kill switch**: SIGKILL all workers, close any modal, unwind, report, exit 2 |
| Ctrl-D, dialog open | dismiss (`Cancelled`) | kill switch (dismiss via `c`/`cancel`) |
| Ctrl-D, ASK open | stop without answer | kill switch (report + exit 2) |
| Ctrl-C | graceful stop (worker runs to settle; abort inside a dialog) | unchanged |
| stdout not a TTY | line mode | unchanged, byte-for-byte |
| stdin closed | EOF parity for modal; run keeps supervising | unchanged |

## Implementation plan (commit-by-commit)

Each commit is one green unit: implement → `cargo test` →
`cargo fmt --check` → `cargo clippy --all-targets --all-features -- -D warnings`
→ commit with the listed message → stop. Suggested order 1 → 2 → 3 (each
independent; 2 and 3 both touch `compose_frame` but in disjoint spots — if
landed out of order the merge conflicts are trivial).

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: stream text and thinking tokens into flowing styled blocks` | Flowing tokens (fix 2) | `src/tui.rs`: `TuiState.stream` (`StreamLine`), `append_stream` (`\n` split, `\r\n` normalize, kind-change flush, length cap), flush-on-`push_line`/`push_banner`/`set_plan`, `compose_frame` renders `ring + stream`, `trace_lines` emits blank rows for empty logical lines; `src/ui.rs`: `stream_part(delta)` returning raw kind+text — **implement it as the single delta classifier with `apply_delta` delegating to it** (line-mode byte-parity preserved) so the two matchers cannot drift (review F6); `src/main.rs` `worker_tail` TUI branch routes deltas through `append_stream` | unit: append + wrap + kind-flush (thinking block then text block on fresh lines), `\n`/`\r\n` splits, consecutive-newline blank rows, trailing newline, truncation cap, ring eviction with a stream open, `stream_part` for all delta kinds, `compose_frame`/`trace_lines` with open stream at various widths; property (proptest): streamed lines never exceed width |
| 2 | `feat: anchor permission prompts at the bottom above the live trace` | Bottom modal (fix 3) | `src/tui.rs`: `modal_box` bottom-anchored exact-height (top-truncate when taller than viewport), `compose_frame` stacks trace-above / box-below, delete dead `dialog_box` + docs | unit: `modal_box` bottom placement + exact height + height cap, `compose_frame` keeps the trace visible above the box (assert "spawned agent" text survives), footer last row never occluded, narrow/edge widths; update the two `dialog_box` tests (removed) |
| 3 | `feat: add Ctrl-D kill switch that terminates the TUI and all workers` | Kill switch (fix 1) | `src/supervise.rs`: `RunControl.kill_requested`; `src/tui.rs`: `input_task` gains `kill: Arc<AtomicBool>`, Ctrl-D arm sets it + closes modal with `Stop` (no-modal → banner), stdin-closed EOF arm unchanged; `src/main.rs`: `kill_watcher` sets `kill_requested` + `stop_requested` **before** `workers.dispose()` (review F1), spawn wiring, ASK-pause flow consults `kill_requested` to scope the `final_result` fix to the kill only (review F2); **docs sweep (review F3)**: `docs/ARCHITECTURE.md` `Ctrl-D` sentence gains a TUI-mode kill caveat, its Limitations drop the stale "No full-screen TUI" bullet, `README.md` "Deferred features" drops the stale full-screen TUI bullet, `src/tui.rs` module anatomy + `input_task` docstrings updated | unit: pure Ctrl-D helper (flag set once, modal closes with `Stop`, banner without modal), `RunControl::new` shape; integration (FakeWorkerPort): kill mid-stream → stopped outcome + report + exit 2, kill during ASK → report + exit 2 (not `Err`), **Ctrl-C at ASK pause unchanged (Err path — F2 parity)**, no worker spawned after kill, dispose kills every live worker; manual tmux script (below) |

Post-build verification (manual, tmux/ghostty):

- `pi-plan supervise` in tag_tool with a live worker: tokens flow on one
  line (wrap at the right edge), thinking shows as a gray block without
  `⟦thinking: …⟧`, the answer starts on a fresh line, paragraph gaps appear
  where `\n` occurs.
- A permission prompt appears at the bottom above the footer; the newest
  text remains visible directly above it; answering via `1`, `y`/`n`,
  `c`/`cancel` round-trips.
- Ctrl-D mid-stream: TUI unwinds, `pgrep -af "pi --mode rpc"` shows no
  supervise-spawned workers, final report prints, exit code is 2; a
  separate interactive `pi` started in another terminal is still alive.
- Ctrl-D with a dialog open and during an ASK pause: same kill behavior.
- Ctrl-C still behaves as before (graceful stop).
- `pi-plan supervise | cat` and `2>trace.log` unchanged (byte-exact line
  mode; Ctrl-D there remains EOF).

## Risks and notes

- **Race on spawn-vs-kill**: at most one spawn can slip in within a single
  ~50 ms poll window before the kill watcher fires — and with
  `stop_requested` flipped first (review F1), the boundary check blocks
  everything after that; `cmd_supervise`'s teardown `dispose` covers the
  last gap (kills whatever is live).
- **Open-line cap**: the 10 000-char cap + truncation banner bounds memory
  for `\n`-less blob deltas; truncation is a documented cosmetic loss.
- **`\n` presence in deltas is assumed, not verified**: if pi's text deltas
  deliver `\n`-free sentences and paragraphs come from other markers, fix 2
  still delivers the token-flow fix; the `\n` handling is handled by
  construction for when they do appear. Verify against a real worker during
  implementation (a one-line addition to the manual gate above: watch for
  paragraph breaks).
- **Parity change**: TUI-mode `^D → Cancelled` is replaced by the kill
  switch (locked Q&A 1). Commit 3 sweeps the docs (review F3): the
  architecture doc's "`Ctrl-D` at the prompt dismisses the dialog" sentence
  gains a TUI-mode caveat; the architecture doc's Limitations and the
  README's Deferred features drop their now-stale "No full-screen TUI"
  bullets (the TUI shipped in plan-tui-display — that drift is a leftover
  this plan must not carry forward); and the `tui.rs` module anatomy + the
  `input_task` docstrings are updated.
- **Non-goals**: no change to line mode; no change to Ctrl-C; no inline
  tool-call markers (locked Q&A 4); no scrollback/search; no markdown
  styling of streamed content; no new crates (unchanged dependency pins).
- **Branch suggestion**: `agent/tui-fixes` on top of the merged themed-TUI
  work.

## Review trail (2026-09-12, plan review)

Findings F1–F6 from the structured plan review are applied inline (each is
labeled `(review Fn)` where it lands): F1 kill_watcher flag order
(stop → dispose), F2 ASK-pause `final_result` fix scoped to the kill via
`kill_requested` (Ctrl-C keeps its `Err` quirk), F3 docs sweep including
the stale "No full-screen TUI" bullets in `docs/ARCHITECTURE.md` and
`README.md`, F4 race-window wording (one ~50 ms poll, not 250 ms), F5
trailing-`\n\n` double blank row noted as cosmetic, F6 `stream_part` as
the single delta classifier with `apply_delta` delegating.

## Open questions for later

- Should the kill switch leave a `supervisor-state.json` marker
  (`lastOutcome: "killed"`) for the next run's report? (Current plan:
  per-attempt saves already persist; the next `supervise` resumes or
  recomputes from git as today.)
- Should Ctrl-C eventually become a hard abort too (mirroring the kill),
  or stay graceful? (Stays graceful; the kill switch is the hard stop.)
