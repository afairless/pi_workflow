# Implementation Plan: Live-worker status rotation in the supervise TUI

Source: `docs/research/plan-live-worker-status.md`

The supervise TUI renders header/footer status from a **single**
`TuiState.worker: WorkerView` slot whose `live` flag any worker's terminal
event flips off. While a worker runs, the supervisor's idle line is therefore
visible most of the time and the worker's status only briefly: a finished
worker instantly switches the whole display to the idle line even when another
worker is still running (or a retry is about to spawn), a freshly spawned
worker is invisible until its first `TurnStart`/quiet-cadence refresh (~2.5 s),
and with two running workers the slot ping-pongs last-writer-wins.

This plan replaces the single slot with a **live-worker registry** (one entry
per supervised worker, seeded at spawn, upserted per tail refresh, removed on
every tail exit) plus a **time-sliced rotation cursor** that alternates the
displayed status across live workers (`ROTATION_HOLD_MS = 3000`). The
supervisor idle line renders **iff** zero workers are live. TUI mode only;
line mode is byte-identical. Four commits.

The commit messages in the table are **exact** — taken verbatim from the source
plan. Workflow per step: implement → `cargo test` (409+ tests) → `cargo fmt
--check` → `cargo clippy --all-targets --all-features -- -D warnings` → commit
with the table's message → stop. One step at a time.

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `feat: track every live worker in the TUI display state` | live-worker registry + spawn seeding, idle gating on zero live workers, removal on every tail exit | `src/tui/mod.rs`: `WorkerEntry`, replace `TuiState.worker` with `workers: Vec<WorkerEntry>` + `seed_worker`/`update_worker`/`remove_worker`/`live_workers`; `note_row_terminal` stores `last_terminal` only (drop the slot flip); `compose_frame` gains `displayed: Option<&WorkerEntry>` (header context, footer `FooterStats` from it — `row_id` from the entry, not `state.row`; idle line iff `None`); `render_task` passes a provisional selection (first live entry) until commit 2's rotation lands; `status_note_text` takes the entry (`apply_modal_decision` passes the same provisional selection; empty registry → the degraded note as today); `src/main.rs`: `tail_task` seeds from the spawn notice (`worker_id`, `row_number`, resolved `max_turns`), `worker_tail` upserts via `tui_update_view` and removes its entry on **every** tail exit — stream close, subscribe-fail early return, and the dialog `stop`/`restart`/`^D` break | unit (`src/tui/tests.rs`): registry seed/upsert/remove; seeded entry is live with row + agent id + zeroed stats and renders `row N/agent X` on the first frame; `displayed` ignores a non-live entry; empty registry → idle footer + no header context; **the dialog stop/restart break removes the entry** (no frozen live leftover); **`row_id` follows the entry's `TodoRow.number` (`--row N` and non-contiguous numbering show the plan's own number, not the single-row position `1`)**; port the single-slot assertions (`compose_frame_switches_to_the_idle_footer_after_a_row_terminal`, `note_row_terminal_stores_…`, `compose_frame_bottom_anchor_…`) to the registry API (the `TuiState::new()` footer becomes the idle line, not a fake `row 1/agent —`); integration: terminal hook → idle line only after removal/`view.live=false`, never while a second entry is live; **a clean-worktree agent's spawn seeds a rotating entry like any live worker, removed at the pass's stream close** |
| 2 | `feat: rotate the displayed worker status when multiple workers run` | time-sliced round-robin | `src/tui/mod.rs`: `ROTATION_HOLD_MS`, `RotationCursor`, pure `select_live_worker`; `TuiState.rotation_cursor`; `render_task` (`src/main.rs`) computes selection + write-back cursor inside the existing lock and passes `displayed` into `compose_frame` | unit: one live worker never rotates and always wins; two workers each hold ≥ `HOLD_MS` then alternate in registry order; cursor whose worker vanished advances to the next live worker (wrap-around); non-live entries skipped; `now < since` keeps the hold; `compose_frame` renders the passed entry on both header line 2 and the footer (same worker at any instant); property-based (proptest): under churn the selection never returns a non-live entry and every live worker is eventually selected |
| 3 | `feat: base the modal status note on the displayed worker` | `status` line command | `src/tui/mod.rs`: `apply_modal_decision`'s `ModalNote::Status` swaps commit 1's provisional first-live call for `displayed_worker(&state)` — the same rotation-aware selection the frame renders (a `None` registry → the pre-existing degraded note) — via the commit 1 `status_note_text(entry)` signature | unit: note renders the selected entry's row/agent/turns; with two live workers mid-hold the note follows the frame's current selection; empty registry → no worker numbers (as today) |
| 4 | `docs: document live-worker status rotation in the supervise TUI` | docs | `docs/ARCHITECTURE.md` (~line 478): replace the "last-writer-wins (a valid rotation) … future work" sentence with the registry contract (seed at spawn, upsert per tail, removal on **every** tail exit, time-sliced round-robin at `ROTATION_HOLD_MS`, idle line iff zero live workers, `row_id` = the entry's `TodoRow.number` — note the `--row N`/non-contiguous-numbering footer change); README TUI section if it touches the footer | `cargo fmt --check` (doc-only), suite green |

## Locked decisions (from the source plan)

- **Rotation speed:** time-sliced hold — each worker's status stays visible for
  a fixed window (`ROTATION_HOLD_MS = 3000`), then rotates to the next live
  worker. The 120 ms render loop redraws every frame, so the switch is seamless.
  (Rejected: rotate per stats refresh — uneven timing; per frame tick — 8 Hz
  flicker.)
- **Identity while rotating:** keep today's in-line format — footer ends
  `row N/agent X`, header status starts `row N · agent X · turns …`. No extra
  count/index marker.
- **Fresh spawn counts as running immediately:** the spawn notice seeds a live
  registry entry (row + agent id + resolved max-turns, stats `?`/`0`), so the
  worker's status is visible from the first frame — no idle gap.
- **Scope:** TUI mode only. Line mode has no persistent header/footer; its
  transient per-worker stderr status lines are untouched (byte-exact
  acceptance contract preserved).

## Invariants to preserve

- `WorkerView` shape and `view_from_snapshot` (incl. `live` semantics) are
  unchanged.
- `supervise::report_terminal` and `OnRowTerminalFn` signature are unchanged —
  `note_row_terminal` stores `last_terminal` only; the not-live semantics move
  into the registry (the tail's stream-close removes the entry; an
  `update_worker` that happens to carry a terminal snapshot upserts
  `live: false`).
- Line mode end to end: `render_status`/stderr status lines, stdout dialogs,
  byte-exact prompts. `hooks` is `None` there, so no registry exists. Do not
  touch line mode.
- Header line 1 (`pi-plan · step X/N · unit`) and the trace viewport are
  untouched — they are plan meta / worker output, not supervisor status.
- Actor sequencing: `run_plan` stays strictly sequential (rows one by one);
  this change makes the display correct for N live tails, it does not add
  parallelism.
- `TuiState::new()` initializes the new fields (`workers: Vec::new()`,
  `rotation_cursor: None`); the `Default` impl delegates and `cmd_supervise`
  constructs the state once, so no other construction site needs touching.
- Rotation timing is display-only — every tail keeps updating its own entry
  independently (`update_worker`), so a long-held worker's numbers stay fresh
  when its turn resumes.
- No `unsafe`, no `unwrap()`/`expect()`/`panic!()` in application logic.
- Per commit: `cargo test` green (409+ tests), `cargo fmt --check` clean,
  `cargo clippy --all-targets --all-features -- -D warnings` clean.

## Step notes

### Step 1 (live-worker registry)

- `WorkerEntry { worker_id: u64, row: u64, view: WorkerView }`; `RotationCursor
  { worker_id: u64, shown_since_ms: u64 }` land here or in step 2 as needed by
  the provisional first-live selection.
- `seed_worker(worker_id, row, agent_id, max_turns)` upserts an entry with a
  zeroed stats `WorkerView` (`turns 0`, `context_percent: None`, `cost: None`,
  `elapsed_ms: 0`, `pending_tool: None`, `live: true`) — called from `tail_task`
  on the spawn notice so the row's real turn ceiling shows (`turns 0/40`, not
  `0/0`); `elapsed_ms: 0` shows a `0s` footer until the first refresh (expected).
- `update_worker(worker_id, view)` upserts from `tui_update_view` (replaces
  `set_worker_view`); `remove_worker(worker_id)` deletes an entry.
- `compose_frame` gains `displayed: Option<&WorkerEntry>` and stays pure: header
  line 2 status context, footer `FooterStats` (`row_id` from the entry, not
  `state.row`), and the modal status note all derive from `displayed`. Idle
  footer iff `None`.
- Removal must run on **every** tail exit: the stream-close break
  (`Ok(Err(_)) => break`), the **subscribe-fail early return** (`worker_tail`
  returns without a receiver — without removal the seeded entry is permanently
  "live"), and the dialog `stop`/`restart`/`^D`-kill break (that path aborts the
  worker and exits the loop, so the tail never sees the stream close). Route
  every tail exit through one removal.
- **Clean-worktree agents are registry participants**: the clean pass fires
  `on_spawn` like any row worker, so in TUI mode a clean agent's tail seeds and
  rotates a registry entry and removes it on stream close. No exclusion needed;
  document + test.

### Step 2 (time-sliced rotation)

- Pure `select_live_worker(workers, cursor, now_ms) -> Option<(WorkerEntry,
  RotationCursor)>`: no live worker → `None` (idle line); one live worker →
  always it; multiple → keep the current one while its hold has not elapsed,
  then advance circularly to the next live worker (wrapping); a cursor whose
  worker vanished advances; a non-live entry is never selected; clock skew
  (`now < shown_since`) never elapses the hold. `render_task` computes the
  selection + write-back cursor inside the existing state lock each 120 ms tick
  and passes `displayed` to `compose_frame`.

### Step 3 (modal status note)

- `apply_modal_decision`'s `ModalNote::Status` swaps commit 1's provisional
  first-live call for `displayed_worker(&state)` — the same rotation-aware
  selection the frame renders; empty registry → the pre-existing degraded note.

### Step 4 (docs)

- `docs/ARCHITECTURE.md` (~line 478): replace "last-writer-wins (a valid
  rotation) … future work" with the registry contract; note the `--row N` /
  non-contiguous-numbering footer change (`row_id` = the entry's
  `TodoRow.number`); README TUI section if it touches the footer.

## Pitfalls (from the source plan)

- **Subscribe-fail early return must remove the entry** — otherwise the seeded
  entry stays permanently "live" and the rotation shows a dead worker's frozen
  status forever.
- **Dialog stop/restart/`^D` break must remove the entry too** — that path never
  sees the stream close; without removal the dead entry masks the idle line.
- **Cursor stability under churn** — the cursor stores a worker id, not an
  index, so removal never shifts it; the selection advances to the next live
  worker after the vanished id (or wraps). Test explicitly.
- **`now_ms` may go backwards** (clock skew / test clock) — `now_epoch_ms()`
  returns `Option`; the render loop passes `0`, so treat non-positive deltas as
  "hold not elapsed".
- **`row_id` must come from the entry** — `state.row` would label every worker
  with the step-banner row; `--row N`/non-contiguous output intentionally
  changes to the plan's own `TodoRow.number` (the idle footer always used it).
- **Terminal→removal window is cosmetic** — between the terminal hook and the
  tail's `break` the entry still carries `live: true`, so it may render one or
  two ticks; self-bounding, no latch needed. The guarantee that matters — no
  idle line while a worker runs — is unaffected.

## Acceptance criteria (end state)

1. A freshly spawned worker's status (header line 2 + footer) is visible from
   the **first frame** — no `idle · …` line appears while it runs, including at
   the start of a row and of a retry.
2. When the retried worker of a failed row comes up, the header/footer show
   that worker immediately; the `idle · last: row N failed · next: row N` line
   appears only in the brief 0-worker window between terminal and spawn (and at
   the 0-worker boundaries of the run). A just-finished worker's last-known
   stats may linger one or two render ticks past its terminal — never past its
   tail's exit.
3. With two live workers the two statuses **alternate**, each held for
   `ROTATION_HOLD_MS`; identity stays readable via `row N/agent X` (footer) and
   `row N · agent X · turns …` (header), and header/footer show the **same**
   worker at every instant.
4. `stop`/`restart`/`^D` at a permission dialog leaves **no frozen registry
   entry**: after respawn the restarted row shows only the new worker, the dead
   worker never rotates back in, and once every tail has exited the idle line
   cannot be masked by a leftover entry.
5. The footer's `row N` is the worker's row from the plan (`TodoRow.number`) —
   in `--row N` mode and with non-contiguous numbering the live footer and the
   idle footer agree (both use the plan's own number, e.g. `row 38` for a
   `step38` row).
6. With zero workers the supervisor line is exactly today's
   `idle · last: … · next: … — <unit>` + `stop / restart / status` hints.
7. The modal `status` command reports the currently displayed worker (the
   frame's current selection while rotating).
8. Line mode output is byte-identical.
9. `docs/ARCHITECTURE.md` no longer lists round-robin as future work.
10. Per commit: `cargo test` green (409+), `cargo fmt --check` clean,
    `cargo clippy --all-targets --all-features -- -D warnings` clean.

**Manual verification** (mirrors `docs/acceptance-e2e.md` style): run
`pi-plan supervise` in TUI mode against a multi-row TODO; during a row with a
live worker confirm the footer never shows `idle ·`; trigger a failure + retry
and confirm the worker's status appears instantly at respawn; on a long
single-turn row confirm the status stays visible the whole time (not only at
turn starts); answer a permission dialog with `restart` and confirm the dead
worker never reappears in the rotation afterwards; run `pi-plan supervise
--row N` once and confirm the footer reads `row N/agent X` (the plan's own
number), not `row 1`.
