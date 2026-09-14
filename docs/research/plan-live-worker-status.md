# Plan: Show only live-worker status in the supervise TUI — multi-worker registry with time-sliced rotation

## Status

A research-to-build plan for `pi_plan_workflow`, a follow-up to
`docs/research/plan-tui-display.md` and `docs/research/plan-supervise-tui-fixes.md`
(both implemented). It fixes one operator-visible defect in the full-screen
supervise TUI (`src/tui/mod.rs` + wiring in `src/main.rs`/`src/ui.rs`):

**While a worker is running, the supervisor's idle status is visible most of
the time and the worker's status only briefly.** The TUI renders the footer
and header status from a **single** `TuiState.worker: WorkerView` slot whose
`live` flag any worker's terminal event flips off, so (a) a finished worker
instantly switches the whole display to the supervisor idle line even when
another worker is still running, (b) a freshly spawned worker's status is
invisible until its first `TurnStart` or quiet-cadence refresh (~2.5 s, and
longer under sustained streaming), and (c) with two running workers the slot
ping-pongs last-writer-wins between the two tails.

Design decisions were locked with the user on 2026-09-14 via the Q&A below.
No code was changed during this investigation.

## Goal (user requirements)

1. "While a worker is running, the worker's status is only briefly and
   intermittently visible in the header and footer status lines. Most of the
   time, the supervisor's status is visible, with something like `idle - last:
   row 14 failed - next: row 14 - Validate op stop / restart /status`."
2. "While a worker is running, the supervisor's status is not important and
   shouldn't be visible. While a worker is running, the only status that
   should be visible is that worker's status."
3. "If 2 workers are running, then the statuses of those 2 workers should
   alternate visibility."
4. "The supervisor status should be visible only if there are no workers
   running."

## Locked decisions (Q&A, 2026-09-14)

| # | Question | Decision |
|---|---|---|
| 1 | When 2+ workers run, how fast should the rotation switch? | **Time-sliced hold** — each worker's status stays visible for a fixed window (`ROTATION_HOLD_MS = 3000`), then rotates to the next live worker. The 120 ms render loop redraws every frame, so the switch is seamless. (Rejected: rotate per stats refresh — uneven timing; rotate per frame tick — 8 Hz flicker.) |
| 2 | How should the UI show *whose* status is displayed while rotating? | **Row/agent in-line** — keep today's format: footer ends `row N/agent X`, header status starts `row N · agent X · turns …`. No extra count/index marker. |
| 3 | When does a freshly spawned worker count as "running" and get displayed? | **Immediately at spawn** — the spawn notice seeds a live registry entry (row + agent id + resolved max-turns, stats still unknown → `?`/`0`), so the worker's status is visible from the first frame and the idle gap disappears entirely. |
| 4 | Scope? | **TUI mode only.** Line mode has no persistent header/footer; its transient per-worker stderr status lines are untouched (byte-exact acceptance contract preserved). |

## Current state (evidence)

All references are to `HEAD` (`e1db4d2`). The baseline suite is green (409
tests, `cargo test`).

### The display is driven by ONE worker slot

- `TuiState.worker: WorkerView` is a single slot (`src/tui/mod.rs` ~line 1276
  struct field; `WorkerView` at ~line 1195). `view_from_snapshot`
  (~line 1226) sets `live: snap.terminal.is_none()`.
- `compose_frame` (`src/tui/mod.rs` ~line 1570) renders:
  - header line 2: `│ source: <src> · <worker status>` **only when**
    `state.worker.agent_id` is non-empty **and** `state.worker.live`, else
    `│ source: <src>` (`status` block, ~line 1584);
  - the footer from `state.worker` **only when** `state.worker.live`, else the
    supervisor idle line (`idle · last: row N <kind> · next: row M — <unit>`
    - `stop / restart / status` hints) via `idle_footer_lines` (~line 1697,
    1732). `format_idle_footer_line` lives in `src/ui.rs` (~line 661).
- `TuiState::set_worker_view` (~line 1372) replaces the slot; the row-terminal
  hook path `TuiState::note_row_terminal` (~line 1393) stores
  `last_terminal: (row, label)` and **flips the slot not-live**.
- The modal `status` command's note also reads the slot:
  `status_note_text(state.row, &state.worker)` (~line 425, called from
  `apply_modal_decision` ~line 1778).

### Why the worker status is "briefly and intermittently" visible

1. **One worker's terminal hides the display for everyone** — the terminal
   hook fires from `supervise::report_terminal` (`src/supervise/mod.rs`
   ~line 530, the single `on_row_terminal` call site; wired in
   `src/main.rs` ~line 574). It flips the **single** slot not-live the moment
   any worker ends. With a retry worker immediately spawning (or a second
   worker already live) the footer shows `idle · last: row 14 failed · next:
   row 14 — Validate …` — exactly the line the user quotes — until the new
   worker's view first refreshes.
2. **No view at spawn** — `tail_task` (`src/main.rs` ~line 947) reacts to the
   spawn notice with `set_plan` + `seed_plan_units` only; `tui_update_view`
   (~line 1229) runs on `TurnStart` or after 10 × 250 ms quiet ticks
   (`worker_tail`, ~line 1029). Under continuous streaming events arrive more
   often than 250 ms, so the quiet cadence never fires and the first refresh
   waits for the next `TurnStart` — the idle gap above.
3. **Last-writer-wins with 2+ workers** — every worker tail calls
   `tui_update_view` and overwrites the same slot; two tails ping-pong it.
   The shipped docs acknowledge this exact limitation:
   `docs/ARCHITECTURE.md` (~line 478): *"Multiple concurrently **running**
   workers remain last-writer-wins (a valid rotation); a round-robin tick over
   live workers is future work."*
4. **The renderer has no notion of "any worker running"** — the footer
   decision is a single boolean (`state.worker.live`) instead of "is the set
   of live workers empty", conflating "the observed worker finished" with "no
   worker is running".

### Coordination seams that already exist (reused, not invented)

- `on_spawn(spawned_row, agent)` → `spawned_tx` → `tail_task` receives
  `(worker_id: u64, row_number: u64)` (`SpawnNotice`, `src/main.rs` ~line 64;
  consumed ~line 951). Delivers the worker's **identity and row before any
  stats exist** — the seed hook.
- `worker_tail` already knows `(worker_id, row_number, hooks)` and already
  breaks on stream close (`Ok(Err(_)) => break`, `src/main.rs` ~line 1042) —
  the removal hook. It also early-returns on subscribe failure
  (~line 1021) — a corner that **must also remove** the entry (see Pitfalls).
- `tui_update_view` already carries `(worker_id, row_number, config)` and the
  resolved max turns (`resolve_max_turns`) — the per-worker upsert.
- `WorkerId = u64` (`src/worker.rs` ~line 34); spawn notices, snapshots, and
  tails all key on the same id.
- The render loop (`render_task`, `src/main.rs` ~line 1240) already owns the
  state lock each 120 ms tick and currently reads `compose_frame` from it —
  the natural place to advance a rotation cursor.

## Design

### 1. A live-worker registry replaces the single slot

`TuiState.worker: WorkerView` becomes a registry plus a rotation cursor:

```rust
/// One registry entry: a supervised worker keyed by the port's worker id.
pub struct WorkerEntry {
    pub worker_id: u64,      // WorkerId (matches tail ids and snapshots)
    pub row: u64,            // 1-based TODO row the worker runs (spawn notice)
    pub view: WorkerView,    // stats; view.live follows the snapshot terminal
}

/// Rotation cursor: the worker currently displayed and when it took over.
pub struct RotationCursor {
    pub worker_id: u64,
    pub shown_since_ms: u64,
}

// In TuiState:
pub workers: Vec<WorkerEntry>,
pub rotation_cursor: Option<RotationCursor>,
```

New state methods (all pure state transforms on `TuiState`):

- `seed_worker(worker_id, row, agent_id, max_turns)` — **upsert**: insert an
  entry with a zeroed stats `WorkerView` (`turns 0`, `context_percent: None`,
  `cost: None`, `elapsed_ms: 0`, `pending_tool: None`, `live: true`).
  Called from the spawn notice so the status is visible on the first frame.
- `update_worker(worker_id, view)` — upsert from `tui_update_view`
  (replaces `set_worker_view`).
- `remove_worker(worker_id)` — delete the entry (tail stream close, and the
  subscribe-fail early return).
- `live_workers() -> Vec<&WorkerEntry>` / `has_live_worker()` — derived
  helpers used by the selection and by the idle gating.
- `note_row_terminal(row, label)` **now only stores `last_terminal`** — the
  not-live semantics move into the registry: the tail's stream-close removes
  the entry promptly after the terminal (the channel is the worker's RPC
  stream and closes at exit), and an `update_worker` that *happens* to carry
  a terminal snapshot (a `TurnStart`-timed poll around the boundary) upserts
  `live: false` (`view_from_snapshot` untouched), so selection skips it. The
  skip-non-live rule is a **defense, not the primary bound**: under
  sustained streaming no quiet-cadence poll can fire inside the
  terminal→close window, so a just-finished worker's last-known stats may
  linger one or two render ticks until its tail removes it. No
  hook-signature change (`OnRowTerminalFn` stays `(row, label)`), which
  keeps the seam stable for line mode and the supervise tests.

Liveness definition: an entry is live iff `entry.view.live`. The **set of
running workers** = `{ e ∈ workers : e.view.live }`.

### 2. Seed at spawn closes the idle gap

In `tail_task` (`src/main.rs` ~line 951), after `set_plan`/`seed_plan_units`,
the TUI hook now also calls `seed_worker`:

```rust
state_mut.seed_worker(
    worker_id,
    row_number,
    agent_id: worker_id.to_string(),
    max_turns: resolve_max_turns(&config, row_number.max(1)), // tail_task has config
);
```

`tail_task` already holds `config` and the full plan, so the seeded view can
carry the row's real turn ceiling (`turns 0/40`, not `0/0`). The first real
`tui_update_view` replaces the zeroed stats on `TurnStart`/quiet cadence as
before. The seeded view carries `elapsed_ms: 0`, so the footer shows a `0s`
duration until the first snapshot refresh — expected, not a bug. Result:
from the worker's first frame the header/footer show its status; the
supervisor idle line cannot appear mid-run from the spawn gap.

### 3. Time-sliced rotation is a pure selection

A pure function (next to `WorkerView`), the renderer's only new logic:

```rust
/// ROTATION_HOLD_MS = 3000  // tunable const in tui/mod.rs

/// Pick the worker the header/footer rotate through, and the next cursor.
/// - No live worker            -> None (caller renders the idle line)
/// - One live worker           -> always it (cursor stays, `since` refreshed)
/// - Multiple live workers     -> keep the current one while its hold has not
///                                elapsed; then advance circularly to the next
///                                live worker after it (wrapping). A cursor
///                                whose worker vanished advances to the next.
///                                A non-live entry is never selected.
/// - now_ms < shown_since (clock skew) -> hold does not elapse.
pub fn select_live_worker(
    workers: &[WorkerEntry],
    cursor: Option<&RotationCursor>,
    now_ms: u64,
) -> Option<(WorkerEntry, RotationCursor)>
```

The render loop mutates only the cursor (not the layout):

```rust
// render_task (src/main.rs), inside the existing state lock:
let now = now_epoch_ms().unwrap_or(0);
let (displayed, next_cursor) = select_live_worker(
    &guard.workers, guard.rotation_cursor.as_ref(), now,
);
guard.rotation_cursor = next_cursor;          // write-back stays in the loop
let frame = compose_frame(&palette, &guard, width, height, displayed.as_ref());
```

`compose_frame` gains one parameter — `displayed: Option<&WorkerEntry>` — and
stays pure (it draws *from* the entry instead of computing the slot itself).
The header status context, footer `FooterStats`, and the modal status note all
derive from `displayed`:

- header line 2: `│ source: … · <format_status_line(displayed.row, displayed.view…)>`
  when `displayed` is `Some`, else `│ source: …` (today's fallback).
- footer: `FooterStats { row_id: displayed.row.to_string(), agent_id: …,
  turns/max/ctx/tokens/window/cost/elapsed from displayed.view }` + the
  unchanged `stop / restart / status` hints; **`row_id` comes from the entry,
  not `state.row`**, so a worker on a different row shows its own row.
- `status_note_text` (`ModalNote::Status`) takes the entry (commit 1 changes
  the signature and the `apply_modal_decision` call site to a provisional
  first-live selection; commit 3 swaps in the rotation-aware selection).

### 4. Idle line only when nothing runs

`compose_frame` picks the idle footer **iff `displayed` is `None`** (which per
selection means the registry holds no live worker). `idle_footer_lines`,
`last_terminal`, and the plan-unit lookup are unchanged — with zero workers
the supervisor line (`idle · last: … · next: … — <unit>` + hints) is still
correct and unchanged. Worker/entry removal paths:

- primary: **every tail exit removes its entry** — the stream-close break
  (`Ok(Err(_)) => break` in `worker_tail`), the **subscribe-fail early
  return** (see Pitfalls), and the dialog `stop`/`restart`/`^D`-kill break:
  that path aborts the worker and exits the loop, so this tail never sees
  the stream close — the removal must run before the `break`.
- defensive: selection never returns a non-live entry, so an entry cannot
  display or mask the idle line once its `live` flag drops. The only stale
  display is the brief terminal→removal window, in which a just-finished
  worker's last-known stats still carry `live: true` (self-bounding, see
  Pitfalls).

**Clean-worktree agents are registry participants.** The clean pass fires
`on_spawn` (`src/supervise/mod.rs` ~line 829) like any row worker, so in TUI
mode a clean agent's tail seeds and rotates a registry entry — a live worker
*is* running and shows its status, and its terminal removes the entry on
stream close exactly like a row worker. No exclusion is needed; document the
behavior and cover it with a test.

### 5. Not changing

- `WorkerView` shape and `view_from_snapshot` (incl. `live` semantics).
- `supervise::report_terminal` and `OnRowTerminalFn` signature.
- Line mode end to end: `render_status`/stderr status lines, stdout dialogs,
  byte-exact prompts. `hooks` is `None` there, so no registry exists.
- Header line 1 (`pi-plan · step X/N · unit`) and the trace viewport: they are
  plan meta / worker output, not supervisor status.
- Actor sequencing: `run_plan` stays strictly sequential (rows one by one);
  this change makes the display **correct for N live tails**, it does not add
  parallelism. Two live workers can already arise today at the
  terminal/retry boundary; the registry is also what a future parallel mode
  would lean on.

## Commit plan

Every commit leaves `cargo test` (409+ tests), `cargo fmt --check`, and
`cargo clippy --all-targets --all-features -- -D warnings` green. Commit
messages are exact. One step at a time: implement → verify → commit → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: track every live worker in the TUI display state` | live-worker registry + spawn seeding, idle gating on zero live workers, removal on every tail exit | `src/tui/mod.rs`: `WorkerEntry`, replace `TuiState.worker` with `workers: Vec<WorkerEntry>` + `seed_worker`/`update_worker`/`remove_worker`/`live_workers`; `note_row_terminal` stores `last_terminal` only (drop the slot flip); `compose_frame` gains `displayed: Option<&WorkerEntry>` (header context, footer `FooterStats` from it — `row_id` from the entry, not `state.row`; idle line iff `None`); `render_task` passes a provisional selection (first live entry) until commit 2's rotation lands; `status_note_text` takes the entry (`apply_modal_decision` passes the same provisional selection; empty registry → the degraded note as today); `src/main.rs`: `tail_task` seeds from the spawn notice (`worker_id`, `row_number`, resolved `max_turns`), `worker_tail` upserts via `tui_update_view` and removes its entry on **every** tail exit — stream close, subscribe-fail early return, and the dialog `stop`/`restart`/`^D` break | unit (`src/tui/tests.rs`): registry seed/upsert/remove; seeded entry is live with row + agent id + zeroed stats and renders `row N/agent X` on the first frame; `displayed` ignores a non-live entry; empty registry → idle footer + no header context; **the dialog stop/restart break removes the entry** (no frozen live leftover); **`row_id` follows the entry's `TodoRow.number` (`--row N` and non-contiguous numbering show the plan's own number, not the single-row position `1`)**; port the single-slot assertions (`compose_frame_switches_to_the_idle_footer_after_a_row_terminal`, `note_row_terminal_stores_…`, `compose_frame_bottom_anchor_…`) to the registry API (the `TuiState::new()` footer becomes the idle line, not a fake `row 1/agent —`); integration: terminal hook → idle line only after removal/`view.live=false`, never while a second entry is live; **a clean-worktree agent's spawn seeds a rotating entry like any live worker, removed at the pass's stream close** |
| 2 | `feat: rotate the displayed worker status when multiple workers run` | time-sliced round-robin | `src/tui/mod.rs`: `ROTATION_HOLD_MS`, `RotationCursor`, pure `select_live_worker`; `TuiState.rotation_cursor`; `render_task` (`src/main.rs`) computes selection + write-back cursor inside the existing lock and passes `displayed` into `compose_frame` | unit: one live worker never rotates and always wins; two workers each hold ≥ `HOLD_MS` then alternate in registry order; cursor whose worker vanished advances to the next live worker (wrap-around); non-live entries skipped; `now < since` keeps the hold; `compose_frame` renders the passed entry on both header line 2 and the footer (same worker at any instant); property-based (proptest): under churn the selection never returns a non-live entry and every live worker is eventually selected |
| 3 | `feat: base the modal status note on the displayed worker` | `status` line command | `src/tui/mod.rs`: `apply_modal_decision`'s `ModalNote::Status` swaps commit 1's provisional first-live call for `displayed_worker(&state)` — the same rotation-aware selection the frame renders (a `None` registry → the pre-existing degraded note) — via the commit 1 `status_note_text(entry)` signature | unit: note renders the selected entry's row/agent/turns; with two live workers mid-hold the note follows the frame's current selection; empty registry → no worker numbers (as today) |
| 4 | `docs: document live-worker status rotation in the supervise TUI` | docs | `docs/ARCHITECTURE.md` (~line 478): replace the "last-writer-wins (a valid rotation) … future work" sentence with the registry contract (seed at spawn, upsert per tail, removal on **every** tail exit, time-sliced round-robin at `ROTATION_HOLD_MS`, idle line iff zero live workers, `row_id` = the entry's `TodoRow.number` — note the `--row N`/non-contiguous-numbering footer change); README TUI section if it touches the footer | `cargo fmt --check` (doc-only), suite green |

## Pitfalls

- **Subscribe-fail early return must remove the entry.** `worker_tail` returns
  without a receiver when `workers.subscribe(worker_id)` fails; if the seeded
  entry is left behind it is permanently "live" and the rotation shows a dead
  worker's frozen status forever. `remove_worker` must run on that path too.
- **The dialog stop/restart/`^D` break must remove the entry too.** That path
  aborts the worker and `break`s out of the loop, so this tail never sees the
  stream close; without `remove_worker` the restarted row's dead entry stays
  `live: true` forever — it rotates in as a frozen participant and masks the
  idle line (the same failure as the subscribe-fail path). Route every tail
  exit through one removal.
- **Cursor stability under churn.** A `RotationCursor` stores a worker id, not
  an index, so removal never shifts it: the selection advances to the next
  live worker after the vanished id (or wraps). Test this explicitly.
- **`now_ms` may go backwards** (clock skew / test clock). The hold must not
  elapse when `now < shown_since`; `now_epoch_ms()` returns `Option`, so the
  render loop passes `0` → treat non-positive deltas as "hold not elapsed".
- **`row_id` must come from the entry.** Using `state.row` in a
  multi-worker world would label every worker's stats with the step-banner
  row; the header status and the footer must both use `displayed.row`. This
  also changes `--row N`/non-contiguous output: the footer's `row N` becomes
  the plan's own `TodoRow.number` (e.g. `38` for a `step38` row) where the
  old slot rendered the single-row plan's `1` — an intended consistency fix
  (the idle footer always used `TodoRow.number`), covered by commit 1's
  tests and called out in commit 4's docs.
- **The terminal→removal window displays the just-finished worker's
  last-known stats.** Stream-close removal is prompt (the channel is the
  worker's RPC stream and closes at exit), but between the terminal hook and
  the tail's `break` the entry still carries `live: true`, so selection can
  render it for a tick or two. The skip-non-live rule helps only when an
  `update_worker` *happens* to carry the terminal (a `TurnStart`-timed poll
  around the boundary — under sustained streaming no quiet-cadence poll
  fires at all). Cosmetic and self-bounding; no new latch is needed in
  `note_row_terminal`, and the guarantee that matters — no idle line while a
  worker runs — is unaffected.
- **Do not touch line mode.** `hooks` is `None`, so no registry is created;
  `render_status` and the stdout dialogs stay byte-identical (acceptance
  contract in `docs/ARCHITECTURE.md` "Line mode" notes).
- **`TuiState::new()` and its construction site must initialize the new
  fields.** `workers: Vec::new()` and `rotation_cursor: None` belong in the
  struct literal inside `TuiState::new()` (`src/tui/mod.rs`); the `Default`
  impl delegates to `new()` and `cmd_supervise` constructs the state once
  (`src/main.rs` ~line 321), so no other site needs touching.
- **Rotation timing is display-only.** It must not gate stats collection:
  every tail keeps updating its own entry independently (`update_worker`),
  so a long-held worker's numbers stay fresh when its turn resumes.

## Acceptance criteria

1. A freshly spawned worker's status (header line 2 + footer) is visible from
   the **first frame** — no `idle · …` line appears while it runs, including
   at the start of a row and of a retry.
2. When the retried worker of a failed row comes up, the header/footer show
   that worker immediately; the `idle · last: row N failed · next: row N`
   line appears only in the brief 0-worker window between terminal and spawn
   (and at the 0-worker boundaries of the run). The just-finished worker's
   last-known stats may linger one or two render ticks past its terminal
   (the removal window) — never past its tail's exit.
3. With two live workers the two statuses **alternate**, each held for
   `ROTATION_HOLD_MS`; their identity stays readable via `row N/agent X`
   (footer) and `row N · agent X · turns …` (header), and header/footer show
   the **same** worker at every instant.
4. `stop`/`restart`/`^D` at a permission dialog leaves **no frozen registry
   entry**: after the respawn the restarted row shows only the new worker,
   the dead worker never rotates back in, and once every tail has exited the
   idle line cannot be masked by a leftover entry.
5. The footer's `row N` is the worker's row from the plan
   (`TodoRow.number`) — in `--row N` mode and with non-contiguous numbering
   the live footer and the idle footer agree (both use the plan's own
   number, e.g. `row 38` for a `step38` row).
6. With zero workers the supervisor line is exactly today's
   `idle · last: … · next: … — <unit>` + `stop / restart / status` hints.
7. The modal `status` command reports the currently displayed worker (the
   frame's current selection while rotating).
8. Line mode output is byte-identical.
9. `docs/ARCHITECTURE.md` no longer lists round-robin as future work.
10. Per commit: `cargo test` green, `cargo fmt --check` clean, `cargo clippy
   --all-targets --all-features -- -D warnings` clean.

Manual verification (mirrors `docs/acceptance-e2e.md` style): run
`pi-plan supervise` in TUI mode against a multi-row TODO; during a row with a
live worker confirm the footer never shows `idle ·`; trigger a failure + retry
and confirm the worker's status appears instantly at respawn; on a long
single-turn row confirm the status stays visible the whole time (not only at
turn starts); answer a permission dialog with `restart` and confirm the dead
worker never reappears in the rotation afterwards (footer/header always show
the live respawn); run `pi-plan supervise --row N` once and confirm the
footer reads `row N/agent X` (the plan's own number), not `row 1`.
