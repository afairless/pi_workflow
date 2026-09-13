# Implementation Plan: TUI permission-dialog context, focus styling, and worker-stats reporting

Source: `docs/research/plan-tui-permission-context-worker-stats.md`

A follow-up to the merged `plan-tui-arrow-key-selection.md`. Three independent
TUI changes:

1. **Focus styling** — the focused permission-dialog row stops being a
   full-width amber fill; it becomes amber **text** (`palette.accent`) on the
   normal panel background, bolded so it reads clearly.
2. **Command context** — the permission box shows the operator what the agent
   actually asked to do: (a) the extension's full multi-line `title` (+
   `message`) rendered as split, wrapped modal rows — pi's own methodology —
   and (b) a belt-and-suspenders pending-tool block (`tool: <name>
   (call_…)` + `$ <command>`) decoded from the `tool_execution_start` event
   pi already sends but pi-plan drops.
3. **Worker stats** — header/footer stop showing statistics for completed
   workers; during the between-row window they show supervisor status (last
   row completed, next row). Every worker's final statistics are logged to a
   durable JSONL under the run-state root *and* printed in the supervisor's
   ending report.

Locked decisions (Q&A 2026-09-13): command context comes from **both** the
extension's full `title`/`message` and the decoded `tool_execution_start`
`args`; the idle header/footer line is supervisor status with row context
(`idle · last: row N completed · failed · next: row M — unit`); worker stats
go to the ending report **and** `~/.pi-plan/<key>/worker-stats.jsonl`.

Scope note: items 1 and 2 change **both modes consistently** (the modal box in
TUI mode and the printed dialog in line mode share `ui.rs` renderers; the
byte-parity tests between them must stay green). Item 3 is UI-only for the
header/footer (TUI mode) plus report/storage code affecting both modes.

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit with the message
in the table → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: restyle the focused dialog row as accent-colored bold text` | Focus styling | `src/tui.rs`: `StyledLine.bold` + all construction sites (including the two `compose_frame` re-pad copies), `Stylize::bold` in `src/theme.rs`, `render_task` draw order, focused row in `modal_box` = `fg: accent`, `bg: fill`, `bold: true`; Ask modal untouched | Unit: `Stylize::bold` byte form; focused select row `fg == accent && bold && bg == fill` while siblings keep panel fill; confirm modal `no` highlighted at rest; composed-frame highlight assertion; existing modal height/clip/note/input tests stay green; full suite green |
| 2 | `feat: render the full permission prompt in dialog boxes` | Multi-line title + wrapping | `src/ui.rs`: `dialog_title_lines` (CRLF-stripping), `dialog_message_lines`, use in `dialog_lines` + `modal_dialog_rows`; `src/tui.rs::modal_box` word-wraps **every** content row (via `wrap_text`, width `inner − 1`) instead of truncating; `clip_modal_rows` now **honors `focus_line`** (focused row's first chunk stays visible when the bottom-anchored window can include it), with `focus_line` mapping a focused `DialogRow` to its first wrapped chunk's box line | Unit: title-line splitting (empty/single/multi/blank/CRLF/method-label fallback); `dialog_lines` vs `modal_dialog_rows` parity incl. multi-line titles (unit + proptest generator extended); a long `command :` line wraps, never ellipsizes; a long option wraps with `▸` on its first chunk; options stay numbered; `dialog_item_count` unchanged; tall prompt keeps options/input/bottom pinned; the focused row survives a clipped wrapped box; full suite green |
| 3 | `feat: show the pending tool call and its arguments in permission dialogs` | Pending-tool context (both modes) | `src/rpc.rs`: decode `args` into `ToolExecutionStart`; `src/worker.rs`: `PendingTool` + `SnapshotAcc.pending_tool` maintained by `note_event`, carried by `WorkerSnapshot`; `src/ui.rs::tool_context_lines`; `dialog_lines(req, tool)` + `modal_dialog_rows(req, focus, tool)` emit the context rows (call sites updated: `dialog_roundtrip`, ui.rs tests, `tui.rs:3070`); `src/tui.rs`: `WorkerView.pending_tool`, `TuiState.modal_tool` + `open_modal`/`close_modal` threading, `compose_frame` threads `modal_tool` into `modal_box`; `src/main.rs::worker_tail` passes the pending tool into `open_modal` (from a snapshot read) and `dialog_roundtrip` threads it into `dialog_lines` | Unit: `args` decode; pending set on start / cleared on end / survives the dialog; `tool_context_lines` bash vs non-bash vs absent args; both renderers show the context rows between message and options over the same context arg, none when empty; parity unit tests + proptest generator extended over request × context shapes; full suite green |
| 4 | `feat: drop completed workers from the header and footer stats` | Live-only stats + idle line | `src/worker.rs`: `WorkerSnapshot.terminal`; `src/tui.rs`: `WorkerView.live`, idle footer formatter + `TuiState.plan_units`/`last_terminal`, `note_row_terminal` (stores `last_terminal` **and flips the displayed view not-live** — the hook drives the idle transition, see Step 4 notes); `src/supervise.rs`: `SuperviseServices.on_row_terminal` invoked from `report_terminal` with `terminal_kind_label`; `src/main.rs: tui_update_view` skip + idle-line wiring; `src/ui.rs::format_footer_line` idle variant | Unit: snapshot terminal surfaced; `view.live`; completed snapshot skipped by the view update; idle footer with/without `plan_units`; **end-to-end idle transition — plan/worker set → `note_row_terminal` → `compose_frame` drops the worker stats and the header status context and renders the idle line**; `on_row_terminal` invoked on terminal; full suite green |
| 5 | `feat: log per-worker statistics and report them at the end` | Stats reporting + durable log | `src/cli.rs::format_final_report` per-attempt stats line; `src/storage.rs::append_worker_stats` (+ `WorkerStatsRecord`: `v: 1` schema marker; full-file rewrite per record, atomic; `create_dir_all(root)`; **skips snapshot-less runs**); `src/supervise.rs`: `append_stats` service closure invoked where each `RunRecord` is finalized + compact stats suffix on the terminal report line; `src/main.rs::cmd_supervise` supplies the closure from the resolved run-state root; `tests/` integration for one record per attempt | Unit: report stats line (snapshot present/absent); JSONL record shape (`v: 1`, no-snapshot skip) + atomic overwrite behavior; run-loop wiring test; full suite green |
| 6 | `docs: document permission-dialog context, focus styling, and worker-stats reporting` | User docs | `README.md` "Permissions behavior": the dialog shows the full ask (command/tool args), the focused row is accent bold text, header/footer show only live workers and an idle supervisor line between rows, and every run's statistics are in the ending report and `~/.pi-plan/<key>/worker-stats.jsonl`; `docs/ARCHITECTURE.md` paragraphs; `src/tui.rs` module header note | `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`; docs read cleanly; manual check (real terminal): `pi-plan supervise` — a gated bash call shows `command : …` wrapped in the box plus the `$ …` context line, the focused option is amber bold on panel, the footer drops to the idle line between rows, and the final report lists each attempt's stats |

### Step 1 notes (focus styling)

- `StyledLine { text, fg, bg }` gains `pub bold: bool` (default `false` at
  every construction site); the two `compose_frame` re-pad copies must copy
  `line.bold` along. `Stylize::bold()` returns `"\u{1b}[1m"` — SGR bold is
  unconditional and the per-row `\e[0m` reset that `render_task` already
  emits after each row clears it.
- `render_task` draws `fg` then `bg` then, when `line.bold`, the bold escape,
  then the text. The `▸` marker stays (it marks the row when color is
  off/impaired). The Ask-question modal passes `focus: None` and is untouched.
- No new theme token: `accent` is already the bright amber `#fabd2f` in the
  active gruvbox-dark theme.

### Step 2 notes (multi-line titles)

- The extension's title lines use aligned `label : value` facts
  (`tool`, `surface`, `command`, `full command`, `working directory`, …).
  Keeping the raw text verbatim preserves the alignment and the rule
  (`rule : *`) info — no reformatting.
- `dialog_title_lines`/`dialog_message_lines` strip a trailing `\r` per line
  (a CRLF title would skew the aligned `label : value` width math).
- `inner`/`inner + 1` arithmetic in `modal_box` must switch from "one row per
  DialogRow" to "one row per wrapped chunk" for content rows. The wrap width
  is `inner − 1` (the text cell: the `│` prefix is 2 cells, and the focused
  `│ ▸` prefix replaces each option's two-space indent at the same cell
  width — review F3). Heading and option rows wrap like every other row;
  nothing is special-cased and nothing ellipsizes (review F4). `inner` itself
  is still computed from the **unwrapped** row lengths (capped at
  `width − 2`), so the box width is stable — wrapping changes only the row
  count.
- All title/message/tool lines are unfocused content rows — they never count
  toward `dialog_item_count`, so option numbering, the focus model, and
  `item_reply` are untouched.
- `dialog_roundtrip` (line mode) prints the split lines unchanged —
  `dialog_lines` returning the extra lines is all it needs; `reply_from_input`
  and the prompt loop are untouched.
- `clip_modal_rows` must start **honoring** `focus_line` (today it takes the
  parameter and ignores it): keep the focused row's first chunk visible
  whenever the bottom-anchored window can include it — the behavior the
  function's own doc comment already promises. The top-drop discipline that
  pins the note/input/bottom rows is unchanged.

### Step 3 notes (pending-tool context)

- The probe proves ordering: `tool_execution_start` (with `args`) is emitted
  **before** the gate's `extension_ui_request`, and the pump consumes the
  same FIFO broadcast stream, so a snapshot read at dialog time sees the
  pending call in **both** modes — `open_modal`'s snapshot read in the TUI
  and `dialog_roundtrip`'s in line mode. Clearing on `ToolExecutionEnd`
  (which can only arrive after the operator answers) means the context block
  never outlives its dialog; an aborted worker's pump exiting leaves the
  slot's last value, which is fine (the modal is closing anyway).
- Empty context (no pending call) renders nothing, so third-party extension
  dialogs (ASK questions, input prompts) are unchanged.
- `PendingTool` lives in `worker.rs`; `ui.rs` imports it (no dependency cycle
  — `worker.rs` does not import `ui.rs`).
- `serde_json::Value` on `args` keeps the decode lossless; only `tool_name ==
  "bash"` reads `args.command` (a string); every other tool gets a bounded
  compact JSON preview (reusing the existing `truncate_with_ellipsis`), so
  non-bash tools (edit/write…) show what they will touch.

### Step 4 notes (idle line)

- The idle transition is **driven by the row-terminal hook, not by a late
  snapshot** (review F1): `worker_tail` has no `AgentSettled` arm, and the
  event channel closes ~250 ms after the terminal — far short of the 2.5 s
  quiet cadence — so the slot can never be updated from the completed
  worker's own snapshot.
- `tui_update_view` skips completed workers (a snapshot whose `terminal` is
  set never overwrites the slot): this *protects* the idle marking the hook
  made, guarding against a stale late snapshot from a tail that is still
  draining.
- `on_row_terminal` is the same seam `report(ReportKind::Terminal, …)` uses —
  one structured call site in `report_terminal`, so line mode and TUI mode
  both get it; the TUI wire is a small closure like the existing `report`
  closure in `cmd_supervise`. The closure fires on **every terminal event**
  (stalled/failed attempts included — `report_terminal` sees the terminal
  kind, not the row outcome), so `note_row_terminal` must also be the
  mechanism that flips `WorkerView.live` to false: no other path can deliver
  the terminal to the TUI state (review F1).
- The label is a **terminal kind, not a row outcome**: `terminal_kind_label`
  → `completed` or `failed`, and `last_terminal: Option<(u64, String)>`
  (row number → label). The label truthfully shows the preceding attempt
  during a retry (e.g. `idle · last: row 5 failed · next: row 5 — …`).
- `plan_units: Vec<(u64, String)>` (row number → logical unit) is seeded from
  the same `TodoPlan` `tail_task` already holds — `set_plan` gains a "first
  call stores the map" behavior (or a separate `set_plan_units` invoked once
  at supervise start). When the next row's unit is unknown, `next: row N`
  only. `single_row_plan` mode (`--row N` / `step N`) holds a one-row plan,
  so the idle footer degrades to `next: row N` without a unit — intended.
- Idle footer must fit the footer width (truncate with ellipsis via the
  existing `pad_line_to`/`truncate_with_ellipsis` guard in
  `format_footer_line`). During idle, the header context line drops the
  worker stats too; the header keeps the step banner (row/total/unit).
- Line mode is untouched (it never had a persistent stats line).

### Step 5 notes (stats log)

- The terminal snapshot is at most one `stats_interval` stale (the periodic
  poll cadence); that matches the existing `--pi-plan report` transcript
  behavior and needs no extra RPC round-trip. A final `get_session_stats`
  read before the reaper can be a follow-up — not required for this change.
- `append_worker_stats` uses the same write-temp-then-rename atomicity as
  `save_state_file` so an interrupted run cannot corrupt the log, and
  `create_dir_all(root)` runs before the first write (a run-state root may
  be fresh). The rename makes each record a **full-file rewrite**, not an
  incremental append — at one record per run attempt this is trivially cheap,
  but the README must describe the file as an audit log: a reader
  `tail -f`ing across the rename will miss the newest line.
- Records carry `"v": 1` up front so a later field addition is detectable by
  version rather than by guesswork (review F6), and a record is written
  **only when the run's terminal snapshot is present** — a `QuestionPause` or
  abort-before-stats writes nothing, so the log has no all-null rows.
- Fields: row number, attempt, agent id, outcome kind, cost, tokens, context
  %, context window, turns, started_at, completed_at, transcript path. A
  stats record is written once per run attempt (multiple rows → multiple
  records; the report shows each attempt's stats, matching the existing
  per-attempt format). The terminal report line for each worker additionally
  gains a compact stats suffix (`· cost $X · N tokens · T turns`) so line
  mode logs it too.

### Review trail

The full review trail (findings F1–F8, all resolved in place on 2026-09-13)
lives in the source document's "Review trail" section.

### Open items carried from the plan review

- `serde_json::Value` on `WorkerSnapshot` (serde_json is pinned to
  `=1.0.151` in `Cargo.toml`; its `Value` derives `PartialEq`/`Eq` — verify
  the new field keeps the snapshot's `PartialEq` derive valid at step-3 CI).
- Idle-line width budget: `idle · last: row N completed · next: row M — unit`
  must fit the footer width (truncate via `pad_line_to`/
  `truncate_with_ellipsis` in `format_footer_line`).
- Multiple concurrent workers remain last-writer-wins among **running**
  workers (already a valid rotation); a true round-robin tick over a
  `Vec<WorkerView>` keyed by live worker ids can be layered on later without
  changing this design's liveness rule — explicitly out of scope for v1.
