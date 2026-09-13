# Plan: TUI permission-dialog context, focus styling, and worker-stats reporting

## Status

A research-to-build plan for `pi_plan_workflow`, a follow-up to
`docs/research/plan-tui-arrow-key-selection.md` (implemented, merged). It makes
three independent TUI changes:

1. **Focus styling** — the focused permission-dialog row stops being a
   full-width amber fill; it becomes amber **text** (the theme accent) on the
   normal panel background, bolded so it reads clearly.
2. **Command context** — the permission box finally shows the operator *what
   the agent actually asked to do*. The full command already arrives from the
   permission extension inside the request's multi-line `title`; the TUI
   renders it as one broken row and effectively hides it. We render the whole
   prompt (split lines, wrapped to the box width) and — as a locked
   belt-and-suspenders — surface the pending tool call's arguments from the
   `tool_execution_start` event pi already sends but pi-plan currently drops.
3. **Worker stats** — the header/footer stops showing statistics for
   completed workers; during the between-row window it shows supervisor
   status (last row completed, next row). Every worker's final statistics are
   logged to a durable JSONL under the run-state root *and* printed in the
   supervisor's ending report.

Design decisions were locked with the user on 2026-09-13 via the Q&A below.
No code was changed during this investigation.

## Goal (user requirements, paraphrased)

1. "The bright orange highlight of the user's selection in the permission box
   makes the text hard to read. Instead of highlighting all the space around
   the text, change the color of the text from green to the bright orange
   highlight color. Bold the orange-highlighted text so the selection stands
   out clearly."
2. "I need to see more context for the permission boxes — specifically the
   command the coding agent wants to use. The Pi TUI already does this. Look
   closely at the Pi TUI and determine how it's able to display the requested
   command; we can probably adopt its same methodology."
3. "The header and footer show statistics for the agent workers. When there
   are multiple workers the display alternates among them. It doesn't need to
   show statistics for workers that have already completed their runs... it
   would be good if the statistics for all the workers are logged and
   included in the supervisor's ending report when it terminates."

## Locked decisions (Q&A, 2026-09-13)

| # | Question | Decision |
|---|---|---|
| 1 | Item 2: where does the command context come from? | **Both.** (a) Render the extension's full multi-line `title` (plus `message` when present) as split, wrapped modal rows — this is pi's methodology, and the extension's title already contains `command : <full command>`. (b) Additionally decode `args` from the `tool_execution_start` event (pi already sends it; pi-plan drops it), track the pending tool call per worker, and render a `tool: … (call_…)` + `$ <command>` context block in the dialog — belt-and-suspenders against sparse third-party dialogs. |
| 2 | Item 3: what does the header/footer show while the last worker is completed and no worker is running? | **Supervisor status with row context.** No worker stats; the footer line switches to show the supervisor is idle, which step was just completed, and which step is next (e.g. `idle · last: row 5 completed · next: row 6 — Unit tests`; during a retry the label is the preceding terminal kind, e.g. `… row 5 failed …`). |
| 3 | Item 3: where do the "logged" worker statistics go? | **Ending report + durable JSONL.** Print one stats line per run attempt inside the final `--pi-plan report` block (and on each worker's terminal report line), and append each terminated worker's stats as a JSONL record under the run-state root (`~/.pi-plan/<key>/worker-stats.jsonl`). |

Scope note: items 1 and 2 change **both modes consistently** (the modal box in
TUI mode and the printed dialog in line mode share `ui.rs` renderers; the
parity tests between them must stay green). This includes item 2B's
pending-tool context block, which renders in **both** modes (review decision
2026-09-13: line mode gets the `tool:` / `$ <command>` rows too, and the
byte-parity proptest covers both renderers over the same context argument).
Item 3 is UI-only for the header/footer (TUI mode) plus report/storage code
that affects both modes.

## Current state (evidence)

All references are to `HEAD` (b7437bf). Line numbers are approximate.

### Item 1 — the focused row is drawn as an amber full-row fill

- `src/tui.rs::modal_box` (~line 442) is the modal renderer. The focused
  content row is styled `fg: palette.user_message_text` +
  `bg: Some(palette.accent)` (lines ~504-510) — a full-row fill of the theme
  accent with the "user message" foreground on top.
- The active theme is gruvbox-dark from `@victor-software-house/
  pi-curated-themes`, resolved at runtime by `src/theme.rs`:
  `accent: #fabd2f` (bright amber/orange), `userMessageText: ""` (terminal
  default → reads light-green on this terminal), `userMessageBg: #2d2d2d`.
  So today: default-colored text on a bright amber band — the low-contrast
  look the user complains about.
- `StyledLine { text, fg, bg }` (`src/tui.rs:65`) has **no bold flag**, and
  `Stylize` (`src/theme.rs:394`) has `fg`/`bg` but no `bold` builder. The
  draw loop in `src/main.rs::render_task` (~line 1165) emits
  `\e[38;2;r;g;bm` (fg), optional `\e[48;2;r;g;bm` (bg), the text, then
  `\e[0m` — a per-row reset already present, so a bold escape needs no new
  reset handling.
- `compose_frame` (`src/tui.rs:1492-1520`) re-wraps trace lines into fresh
  `StyledLine { text, fg, bg }` values in two places; those copies must
  preserve a new `bold` field or the modal's focused bold would be dropped
  when the modal path re-pads trace rows (it doesn't, but the field must be
  threaded correctly for the struct's parity tests).
- Tests pinning the current look: `modal_box_highlights_the_focused_row_
  with_marker_and_accent_fill` (`src/tui.rs:3204`) asserts
  `out[marker_at].bg == Some(accent)`; the composed-frame test at
  `src/tui.rs:3388` asserts the accent fill inside the frame. Both change.

### Item 2 — the full command arrives but is never rendered

**Who asks.** The permission dialog is not produced by pi-plan nor by pi's
core: it comes from the **`@gotgenes/pi-permission-system`** extension, loaded
by the worker from the user-scoped package root
`~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system` (v32.0.2,
installed 2026-09-11 — the nvm-global copy is an outdated v6.0.0 and is not
loaded). Its `authority/permission-dialog.ts:159` prompts with
`ui.select(\`${title}\n${message}\`, decisionOptions)` and
`permission-prompt-component.ts::requestPermissionDecision`renders the ask's
structured payload one fact per line (`renderPromptDialog` in
`presentation/dialog-renderer.ts`) — that is pi's methodology: **the whole
prompt is one multi-line string**, and pi's own TUI shows it verbatim.

**What travels over RPC.** In `--mode rpc`, pi's `select()` serializes
**only** `{ id, method: "select", title, options }`
(`dist/modes/rpc/rpc-mode.js:84`) — there is **no `message` field**, and all
the dialogue content rides inside `title` (the extension concatenates
`title\nmessage` before calling `select`).

**Ground truth (live probe).** I spawned `pi --mode rpc` with the same argv
shape pi-plan uses (`build_worker_args` in `src/worker.rs:178`) and captured
the exact frames pi-plan decodes. A gated bash command produced:

```json
{
  "type": "extension_ui_request",
  "id": "1827d02f-…",
  "method": "select",
  "title": "Permission Required\ntool         : bash\nrule         : *\ncommand      : mkdir -p delete-me-dir\nfull command : mkdir -p delete-me-dir && rm -rf delete-me-dir",
  "options": ["Yes", "Yes, allow bash \"mkdir *\" for this session", "No", "No, provide reason"]
}
```

and a command touching an external path produced:

```json
{
  "type": "extension_ui_request",
  "id": "23bc6d24-…",
  "method": "select",
  "title": "Permission Required\ntool              : bash\nsurface           : external_directory\ncommand           : rm -f /tmp/pi-plan-probe-testfile\nworking directory : /tmp/pi-probe\nexternal path     : /tmp/pi-plan-probe-testfile",
  "options": ["Yes", "Yes, for this session", "No", "No, provide reason"]
}
```

Both carry the **full command with its target path** on the `command :` line
(`rm -f /tmp/pi-plan-probe-testfile`, `mkdir -p delete-me-dir`, and the
`full command :` line for the compound). The same frames were confirmed
against the tag_tool run: the review log
(`~/.pi/agent/extensions/pi-permission-system/logs/*-permission-review.jsonl`)
shows the gated `call_a7ed79…` was `cd /home/tr/Documents/tag_tool && cat >
/tmp/debug_test.rs << 'EOT' …` — information the operator never saw.

**Why pi-plan hides it.** `decode_ui_request` (`src/rpc.rs:718`) puts the whole
string in `ExtensionUiRequest.title`. `modal_dialog_rows` (`src/ui.rs:654`)
emits the title as **one** `DialogRow` (`format!("── {title} ──")`), embedded
newlines and all; `modal_box` counts it as one row (`inner`/`pad_line_to` see
the `\n` as just more characters) and draws it with a literal line break in
the middle of a bordered row. The result is a broken/blank title area — the
operator sees only the trace line `tool: bash (call_…)` (rendered from
`ToolExecutionStart` by `render_event_line`, `src/ui.rs:228`) and the option
list. `message` is always absent on this RPC path, so nothing else renders.

**The args are already on the wire.** The probe also captured:

```json
{"type":"tool_execution_start","toolCallId":"call_723b64d0…","toolName":"bash","args":{"command":"mkdir -p delete-me-dir && rm -rf delete-me-dir"}}
```

arriving **before** the `extension_ui_request` (pi emits `tool_execution_start`
with `args` before the extension's `beforeToolCall` gate runs;
`executeToolCallsParallel` emits, then `prepareToolCall` blocks on the gate).
`src/rpc.rs::decode_event` (~line 645) decodes only `toolCallId`/`toolName`
and drops `args` today.

### Item 3 — completed workers keep driving the stats view; stats are never reported

- The header (`src/tui.rs::header_lines`) and footer
  (`src/ui.rs::format_footer_line`) show **one** worker's stats from
  `TuiState.worker: WorkerView` (`src/tui.rs:1207`), a single slot. Every
  live worker's tail calls `tui_update_view` (`src/main.rs:1086`), which
  blindly overwrites the slot with that worker's snapshot — last writer wins
  (the "alternation" the user describes).
- Completed workers are never removed from `RpcWorker.live`
  (`src/worker.rs:522`; entries are inserted at spawn, `insert` at line 609,
  and only `dispose`/`abort` touch the map), and `WorkerSnapshot` has **no
  liveness field**. `WorkerPort::await_terminal` exists but the terminal
  event (`src/worker.rs` `LiveWorker.terminal` /
  `TerminalEvent::is_completed`) is not exposed on the snapshot, so the TUI
  cannot distinguish a live worker from a finished one. A completed worker's
  last cadence write stays visible until the next worker spawns.
- Per-run statistics **are already captured**: `RunRecord.snapshot:
  Option<WorkerSnapshot>` (`src/supervise.rs:83`, populated from the terminal
  `services.workers.snapshot(worker_num)` read, lines ~631/694/729/752/781).
  The snapshot carries `turn_count`, `context_percent`, `tokens`, `cost`,
  `context_window`, `started_at` (kept fresh by `stats_task`, `src/worker.rs`
  ~440, which polls `get_session_stats`).
- They are never surfaced: `format_final_report` (`src/cli.rs:407`) prints
  per-attempt `question`/`tail`/`transcript` but **no stats**, and nothing is
  persisted — `supervisor-state.json` holds only plan-hash/row/runs/outcome/
  adjudication (`SupervisorState`, `src/supervise.rs`), and the run-state root
  (`~/.pi-plan/<key>/`, `src/storage.rs`) currently contains just `sessions/`,
  `supervisor-state.json`, and `worker-stderr.log`.

## Design

### Item 1 — focused row: amber bold text, no fill

Change the focused dialog row from "amber background band" to "amber text,
bolded". Concretely:

- Add `pub bold: bool` to `StyledLine` (default `false` at every
  construction site — the two `compose_frame` re-pad copies inside the modal
  branch must copy `line.bold` along).
- Add `Stylize::bold() -> String` returning `"\u{1b}[1m"` (empty-string
  nothing needed — SGR bold is unconditional; the per-row `\e[0m` reset that
  `render_task` already emits after each row clears it).
- `render_task` draws `fg` then `bg` then, when `line.bold`, the bold escape,
  then the text (order after the color codes; `\e[1m` may appear before or
  after the color codes — all are SGR parameters, but emitting bold after the
  colors keeps it visually grouped and is what `Stylize::bold` expresses).
- `modal_box`'s focused row becomes `fg: palette.accent`, `bg: fill` (the
  same `user_message_bg` panel fill every other row uses), `bold: true`. The
  `▸` marker stays (it is what marks the row when color is off/impaired).
  The Ask-question modal passes `focus: None` and is untouched.
- Update the two highlight tests to assert `fg == accent`, `bold == true`,
  `bg == fill` (and the composed-frame test likewise).

No new theme token: `accent` is already the bright amber `#fabd2f` in the
active gruvbox-dark theme, and amber-on-panel is the same contrast pi uses
for its accent-colored text.

### Item 2 — render the whole permission prompt + pending-tool context

**Part A — render every line of the request (the pi methodology).**

- New pure helper in `src/ui.rs`: `dialog_title_lines(title: &str) ->
  Vec<String>` splitting `title` on `\n` (empty result → the method label,
  matching today's fallback) and `dialog_message_lines(req) -> Vec<String>`
  for the `message` field when present. Both **strip a trailing `\r`** per
  line — a CRLF title would otherwise shift the aligned `label : value`
  facts by one invisible char and skew the width math (review F7; unit
  tested).
- `dialog_lines(req, tool)` and `modal_dialog_rows(req, focus, tool)` both
  emit: heading row `── {first line} ──`, then the remaining title lines,
  then the `message` lines, then the `tool_context_lines(tool)` rows
  (Part B — empty when there is no pending call), then options. All
  title/message/tool lines are unfocused content rows (they never count
  toward `dialog_item_count`, so option numbering, the focus model, and
  `item_reply` are untouched). Adding the context parameter to `dialog_lines`
  lands in step 3 and touches its call sites: `dialog_roundtrip`
  (`main.rs:1221`), the unit tests (`ui.rs:1056/1065/1119/1144`), the
  modal-height test (`tui.rs:3070`), and the proptest generator
  (`ui.rs:1330`) pass the context they have (`None` in the no-context tests).
- Wrapping: the modal box must **not** ellipsize a long
  `command : rm -rf …` line (`pad_line_to` truncates with `…` today, which
  would cut off exactly the target path the operator needs). In `modal_box`,
  pre-wrap **every** content row (heading, title/message/tool lines, **and
  options**) with the existing greedy `wrap_text` (`src/tui.rs:602`) at the
  **text-cell width `inner − 1`**: each content row is drawn `│` + text
  padded to `inner + 1`, so the text cell is `inner − 1` chars, and the
  focused `│ ▸` prefix occupies the same two cells as each option's
  two-space indent, so a focused row lands on the same width. Wrapping at
  `inner` instead would hand `pad_line_to` chunks one char too wide and
  ellipsize the final character — the exact target-path clipping this
  feature exists to prevent (review F3). The heading is **not**
  special-cased: no rule guarantees a first title line or an option is
  short (a newline-free third-party title, or
  "Yes, allow bash `mkdir *` for this session", wraps to multiple chunks —
  review F4), so every row wraps identically and nothing is ellipsized.
  The `▸` marker belongs to the focused row's **first** chunk only;
  continuation chunks are plain render-only rows. `focus_line` bookkeeping
  must map the focused `DialogRow` to its **first** chunk's box-line index.
- `clip_modal_rows` must start **honoring** `focus_line` (today it takes the
  parameter and ignores it): when the box overflows, keep the focused row's
  first chunk visible whenever the bottom-anchored window can include it —
  the behavior the function's own doc comment already promises. Without it,
  a ≥-height box with a wrapped focused row can scroll the `▸` out of view
  while a bare Enter still submits that row (review F4, review-trail item
  1). The top-drop discipline that pins the note/input/bottom rows is
  unchanged.
- Line mode (`dialog_lines`/`dialog_roundtrip`) improves symmetrically
  (today's `── title\n…──` print is confusing); the byte-parity property test
  between `dialog_lines` and `modal_dialog_rows`
  (`modal_dialog_rows_select_matches_dialog_lines`, `src/ui.rs:1115`, plus
  the proptest at 1330) keeps both renderers aligned over request shapes
  including multi-line titles.

**Part B — pending-tool context from `tool_execution_start` args.**

- `src/rpc.rs::decode_event`: decode `args` (a `serde_json::Value`) on
  `ToolExecutionStart`; the event struct gains `args: Option<serde_json::Value>`.
- `SnapshotAcc` (`src/worker.rs`) gains `pending_tool: Option<PendingTool>`
  where `PendingTool { tool_call_id: String, tool_name: String, args:
  Option<serde_json::Value> }`; `note_event` sets it on `ToolExecutionStart`
  and clears it on `ToolExecutionEnd` (never clears on `ExtensionUiRequest`,
  so the pending call survives the whole gate). `WorkerSnapshot.to_snapshot`
  carries it; `WorkerView` (`src/tui.rs:1135`) gains the same (via
  `view_from_snapshot`).
- When a dialog opens, **both modes** capture the context the same way. The
  gate's `tool_execution_start` has already arrived by then (probe-verified;
  the pump consumes the same FIFO broadcast stream, so a snapshot read at
  dialog time sees the pending call in both modes). TUI: `main.rs::worker_tail`'s
  `ExtensionUiRequest` arm takes `workers.snapshot(worker_id)` first and
  passes its `pending_tool` into `open_modal(modal, tool_context)`; `TuiState`
  stores it in a new `modal_tool` field (cleared by `close_modal`) and
  `compose_frame` threads it into `modal_box` → `modal_dialog_rows`. Line
  mode: `dialog_roundtrip` does the same snapshot read and passes the
  pending tool into `dialog_lines(req, tool)`.
- The row renderer lives in `ui.rs`: `tool_context_lines(tool:
  Option<&PendingTool>) -> Vec<String>` producing `tool: <name>
  (<call_id>)` and — when `tool_name == "bash"` and `args.command` is a
  string — `$ <command>`, else a compact JSON preview of `args` (bounded,
  reusing `truncate` helpers), so non-bash tools (edit/write…) show what they
  will touch. `PendingTool` lives in `worker.rs`; `ui.rs` imports it (no
  dependency cycle — `worker.rs` does not import `ui.rs`).
- Both renderers emit the `tool_context_lines` rows immediately after the
  `message` rows and before the options, in the same dim/content style as the
  message rows; empty context (no pending call) renders nothing, so
  third-party extension dialogs (ASK questions, input prompts) are
  unchanged. With the context in **both** renderers, the byte-parity
  unit/proptest asserts `dialog_lines` vs `modal_dialog_rows` over the
  **same context argument** — the parity guarantee stays meaningful over
  every request × context shape (review F2).

This is what Pi's own TUI effectively shows for a gated call — the full facts
of the ask — and it satisfies "I need to know what file or directory `rm` is
to be applied to" from two independent sources: the extension's own
`command :` fact line (Part A) and pi's wire-level `args` (Part B).

### Item 3 — live-only stats, supervisor idle line, and reported/logged statistics

**Liveness.**

- `WorkerSnapshot` gains `terminal: Option<TerminalEvent>`; `RpcWorker::snapshot`
  reads `LiveWorker.terminal` (in addition to the accumulator).
- `WorkerView` gains `live: bool` (`terminal.is_none()`), threaded through
  `view_from_snapshot`.
- The idle transition is **driven by the row-terminal hook, not by a late
  snapshot** (review F1): `main.rs::worker_tail` has no `AgentSettled` arm,
  and the event channel closes ~250 ms after the terminal — far short of the
  2.5 s quiet cadence — so the slot can never be updated from the completed
  worker's own snapshot. `note_row_terminal` (below) therefore flips the
  displayed view to not-live explicitly.
- `tui_update_view` skips completed workers (a snapshot whose `terminal` is
  set never overwrites the slot): this *protects* the idle marking the hook
  made, guarding against a stale late snapshot from a tail that is still
  draining.
- `compose_frame`: when the displayed worker is not live, the header context
  line and the footer drop the worker stats and render the supervisor status
  instead (see next).

**Supervisor idle line (locked decision 2).**

- `TuiState` gains `plan_units: Vec<(u64, String)>` (row number → logical
  unit) seeded once by `tail_task::set_plan` (it holds the whole `TodoPlan`
  already — store the mapping on the first `set_plan`) and
  `last_terminal: Option<(u64, String)>` (row number → terminal-kind label).
- A structured row-terminal hook: add `on_row_terminal: Option<Box<dyn Fn(u64,
  &str)>>` to `SuperviseServices`; `report_terminal` (`src/supervise.rs:386`)
  calls it next to its existing `report` emit, with (row number,
  `terminal_kind_label(terminal)` → `completed` or `failed`). The label is a
  **terminal kind, not a row outcome**, and the hook fires for **every**
  terminal event, including stalled/failed attempts the loop will retry
  (review F5). The TUI hooks it to `TuiState::note_row_terminal(row, label)`,
  which stores `last_terminal` **and marks the displayed worker view not-live**
  (review F1: without this explicit flip the idle line can never render).
- Idle footer content (new pure formatter in `src/ui.rs`):
  `idle · last: row 5 completed · next: row 6 — Unit tests` (next row's unit
  from `plan_units`; when unknown, `next: row 6` only). During a retry the
  label truthfully shows the preceding attempt, e.g.
  `idle · last: row 5 failed · next: row 5 — …`. The header keeps the step
  banner (row/total/unit). Line mode is untouched (it never had a persistent
  stats line).

**Reporting + durable log (locked decision 3).**

- `format_final_report` (`src/cli.rs`): for each per-attempt record with a
  snapshot, add a stats line:
  `worker: <agent_id> · cost $X.XX · N tokens · ctx P% · T turns · Mm Ss`.
  All values already live on `RunRecord.snapshot`
  (`tokens.total`, `cost`, `context_percent`, `turn_count`,
  `now/stats - started_at`).
- Durable JSONL: new `storage.rs::WorkerStatsRecord` + `append_worker_stats
  (root: &Path, record)` writing one JSON line to
  `<root>/worker-stats.jsonl` (`~/.pi-plan/<key>/worker-stats.jsonl`),
  atomically (write-temp-then-rename, `create_dir_all(root)` first — matching
  `save_state`'s discipline). Each record carries `"v": 1` as an explicit
  schema marker, and a record is written **only when the run's `snapshot`
  is present** — a question pause or abort-before-stats writes nothing, so
  the log has no all-null rows (review F6). The rename makes each record a
  **full-file rewrite, not an incremental append** — at one record per run
  attempt this is trivially cheap, but the README must describe the file as
  an audit log: a reader `tail -f`ing across the rename will miss the newest
  line (review F6).
  Fields: row number, attempt, agent id, outcome kind, cost, tokens,
  context %, context window, turns, started_at, completed_at, transcript
  path. Wired like `save_state`: a `SuperviseServices` closure
  `append_stats: Option<Box<dyn Fn(&RunRecord)>>`, invoked from the run loop
  where each `RunRecord` is finalized; `cmd_supervise` supplies it from
  `ProjectStorage::resolve`. A stats record is written once per run attempt
  (multiple rows → multiple records; the report shows each attempt's stats,
  matching the existing per-attempt format). The terminal report line for
  each worker additionally gains a compact stats suffix
  (`· cost $X · N tokens · T turns`) so line mode logs it too.

**Multiple concurrent workers (future note).** Because every worker's tail
writes the single-slot view and completed workers' writes are now skipped,
"alternation" among concurrently **running** workers remains last-writer-wins
(which is already a valid rotation), the only multi-worker case the user wants
shown. A true round-robin tick over a `Vec<WorkerView>` keyed by live worker
ids can be layered on later without changing this design's liveness rule.

### Tests

- **Item 1:** update `modal_box_highlights_the_focused_row_with_marker_and_
  accent_fill` + the composed-frame test to assert `fg == accent`,
  `bold == true`, `bg == user_message_bg`; the Ask modal still renders
  unfocused; `Stylize::bold` byte test in `theme.rs`.
- **Item 2A:** `dialog_title_lines`/`dialog_message_lines` (empty, single
  line, multi-line, embedded blank lines, **CRLF lines**, no title → method
  label); `dialog_lines`/`modal_dialog_rows` equality over shapes
  **including multi-line titles** (unit + proptest generator extended over
  title/message shapes); `modal_box` wraps a long `command :` line into
  multiple rows without ellipsizing the target (**wrap width `inner − 1` —
  a chunk of exactly `inner − 1` chars is never ellipsized, `inner` chars
  would be**); a long option wraps with the `▸` on its first chunk only;
  options stay numbered from 1; `dialog_item_count` unchanged by extra
  content rows; `clip_modal_rows` keeps options/input/bottom border pinned
  with a tall prompt **and honors `focus_line`** — a wrapped focused row
  stays visible whenever the bottom-anchored window can include it.
- **Item 2B:** `decode_event` extracts `args` from `tool_execution_start`
  (and leaves other events unchanged); pending_tool set on start / cleared on
  end / survives the dialog; `tool_context_lines` for bash (command line),
  non-bash (bounded JSON preview), and absent args; `open_modal`/`close_modal`
  store and clear `modal_tool`; **both** `dialog_lines` and `modal_dialog_rows`
  render the context rows between message and options over the **same context
  argument** (None / bash / non-bash / absent args), and nothing when empty —
  the parity unit tests + proptest generator cover the request × context
  shapes; `dialog_roundtrip` reads the worker snapshot and threads the
  pending tool into `dialog_lines`.
- **Item 3:** `WorkerSnapshot.terminal` surfaced by `snapshot()` (Settled
  case); `WorkerView.live`; `tui_update_view` skips completed; idle footer
  formatter (with and without `plan_units`); `on_row_terminal` wiring from
  `report_terminal`; **the end-to-end idle transition — plan/worker set →
  `note_row_terminal` → `compose_frame` drops the worker stats and the header
  status context and renders the idle footer (review F8)**;
  `format_final_report` prints the per-attempt stats line (snapshot present
  and absent); `append_worker_stats` writes parseable JSONL (with the `v: 1`
  marker, skipping snapshot-less runs) + atomic overwrite behavior; run-loop
  test verifying one record per attempt.
- Ensure the existing `tests/` integration tests (supervisor flows,
  `tests/tui_backend.rs` smoke) stay green.

## Commit-by-commit plan

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit with the
message in the table → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: restyle the focused dialog row as accent-colored bold text` | Focus styling | `src/tui.rs`: `StyledLine.bold` + all construction sites (including the two `compose_frame` re-pad copies), `Stylize::bold` in `src/theme.rs`, `render_task` draw order, focused row in `modal_box` = `fg: accent`, `bg: fill`, `bold: true`; Ask modal untouched | Unit: `Stylize::bold` byte form; focused select row `fg == accent && bold && bg == fill` while siblings keep panel fill; confirm modal `no` highlighted at rest; composed-frame highlight assertion; existing modal height/clip/note/input tests stay green; full suite green |
| 2 | `feat: render the full permission prompt in dialog boxes` | Multi-line title + wrapping | `src/ui.rs`: `dialog_title_lines` (CRLF-stripping), `dialog_message_lines`, use in `dialog_lines` + `modal_dialog_rows`; `src/tui.rs::modal_box` word-wraps **every** content row (via `wrap_text`, width `inner − 1`) instead of truncating; `clip_modal_rows` now **honors `focus_line`** (focused row's first chunk stays visible when the bottom-anchored window can include it), with `focus_line` mapping a focused `DialogRow` to its first wrapped chunk's box line | Unit: title-line splitting (empty/single/multi/blank/CRLF/method-label fallback); `dialog_lines` vs `modal_dialog_rows` parity incl. multi-line titles (unit + proptest generator extended); a long `command :` line wraps, never ellipsizes; a long option wraps with `▸` on its first chunk; options stay numbered; `dialog_item_count` unchanged; tall prompt keeps options/input/bottom pinned; the focused row survives a clipped wrapped box; full suite green |
| 3 | `feat: show the pending tool call and its arguments in permission dialogs` | Pending-tool context (both modes) | `src/rpc.rs`: decode `args` into `ToolExecutionStart`; `src/worker.rs`: `PendingTool` + `SnapshotAcc.pending_tool` maintained by `note_event`, carried by `WorkerSnapshot`; `src/ui.rs::tool_context_lines`; `dialog_lines(req, tool)` + `modal_dialog_rows(req, focus, tool)` emit the context rows (call sites updated: `dialog_roundtrip`, ui.rs tests, `tui.rs:3070`); `src/tui.rs`: `WorkerView.pending_tool`, `TuiState.modal_tool` + `open_modal`/`close_modal` threading, `compose_frame` threads `modal_tool` into `modal_box`; `src/main.rs::worker_tail` passes the pending tool into `open_modal` (from a snapshot read) and `dialog_roundtrip` threads it into `dialog_lines` | Unit: `args` decode; pending set on start / cleared on end / survives the dialog; `tool_context_lines` bash vs non-bash vs absent args; both renderers show the context rows between message and options over the same context arg, none when empty; parity unit tests + proptest generator extended over request × context shapes; full suite green |
| 4 | `feat: drop completed workers from the header and footer stats` | Live-only stats + idle line | `src/worker.rs`: `WorkerSnapshot.terminal`; `src/tui.rs`: `WorkerView.live`, idle footer formatter + `TuiState.plan_units`/`last_terminal`, `note_row_terminal` (stores `last_terminal` **and flips the displayed view not-live** — the hook drives the idle transition, see Step 4 notes); `src/supervise.rs`: `SuperviseServices.on_row_terminal` invoked from `report_terminal` with `terminal_kind_label`; `src/main.rs: tui_update_view` skip + idle-line wiring; `src/ui.rs::format_footer_line` idle variant | Unit: snapshot terminal surfaced; `view.live`; completed snapshot skipped by the view update; idle footer with/without `plan_units`; **end-to-end idle transition — plan/worker set → `note_row_terminal` → `compose_frame` drops the worker stats and the header status context and renders the idle line**; `on_row_terminal` invoked on terminal; full suite green |
| 5 | `feat: log per-worker statistics and report them at the end` | Stats reporting + durable log | `src/cli.rs::format_final_report` per-attempt stats line; `src/storage.rs::append_worker_stats` (+ `WorkerStatsRecord`: `v: 1` schema marker; full-file rewrite per record, atomic; `create_dir_all(root)`; **skips snapshot-less runs**) with atomic write; `src/supervise.rs`: `append_stats` service closure invoked where each `RunRecord` is finalized + compact stats suffix on the terminal report line; `src/main.rs::cmd_supervise` supplies the closure from the resolved run-state root; `tests/` integration for one record per attempt | Unit: report stats line (snapshot present/absent); JSONL record shape (`v: 1`, no-snapshot skip) + atomic overwrite behavior; run-loop wiring test; full suite green |
| 6 | `docs: document permission-dialog context, focus styling, and worker-stats reporting` | User docs | `README.md` "Permissions behavior": the dialog shows the full ask (command/tool args), the focused row is accent bold text, header/footer show only live workers and an idle supervisor line between rows, and every run's statistics are in the ending report and `~/.pi-plan/<key>/worker-stats.jsonl`; `docs/ARCHITECTURE.md` paragraphs; `src/tui.rs` module header note | `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`; docs read cleanly; manual check (real terminal): `pi-plan supervise` — a gated bash call shows `command : …` wrapped in the box plus the `$ …` context line, the focused option is amber bold on panel, the footer drops to the idle line between rows, and the final report lists each attempt's stats |

### Step 2 notes (multi-line titles)

- The extension's title lines use aligned `label : value` facts
  (`tool`, `surface`, `command`, `full command`, `working directory`, …).
  Keeping the raw text verbatim preserves the alignment and the rule
  (`rule : *`) info — no reformatting.
- `inner`/`inner + 1` arithmetic in `modal_box` must switch from "one row per
  DialogRow" to "one row per wrapped chunk" for content rows. The wrap width
  is `inner − 1` (the text cell: the `│` prefix is 2 cells, and the focused
  `│ ▸` prefix replaces each option's two-space indent at the same cell
  width — review F3). Heading and option rows wrap like every other row;
  nothing is special-cased and nothing ellipsizes (review F4). `inner` itself
  is still computed from the **unwrapped** row lengths (capped at
  `width − 2`), so the box width is stable — wrapping changes only the row
  count.
- `dialog_roundtrip` (line mode) prints the split lines unchanged —
  `dialog_lines` returning the extra lines is all it needs; step 3 adds only
  the snapshot read + context argument (Part B); `reply_from_input` and the
  prompt loop are untouched.

### Step 3 notes (pending-tool context)

- The probe proves ordering: `tool_execution_start` (with `args`) is emitted
  **before** the gate's `extension_ui_request`, and the pump consumes the
  same FIFO broadcast stream, so a snapshot read at dialog time sees the
  pending call in **both** modes — `open_modal`'s snapshot read in the TUI
  and `dialog_roundtrip`'s in line mode. Clearing on `ToolExecutionEnd`
  (which can only arrive after the operator answers) means the context block
  never outlives its dialog; an aborted worker's pump exiting leaves the
  slot's last value, which is fine (the modal is closing anyway).
- `serde_json::Value` on `args` keeps the decode lossless; only `tool_name ==
  "bash"` reads `args.command`; every other tool gets a bounded JSON preview
  (reusing the existing `truncate_with_ellipsis`).

### Step 4 notes (idle line)

- `plan_units` is seeded from the same `TodoPlan` `tail_task` already holds;
  `set_plan` gains a "first call stores the map" behavior (or a separate
  `set_plan_units` invoked once at supervise start).
- `on_row_terminal` is the same seam `report(ReportKind::Terminal, …)` uses —
  one structured call site in `report_terminal`, so line mode and TUI mode
  both get it; the TUI wire is a small closure like the existing `report`
  closure in `cmd_supervise`. The closure fires on **every terminal event**
  (stalled/failed attempts included — `report_terminal` sees the terminal
  kind, not the row outcome), so `note_row_terminal` must also be the
  mechanism that flips `WorkerView.live` to false: no other path can deliver
  the terminal to the TUI state (review F1).
- `single_row_plan` mode (`--row N` / `step N`) holds a one-row `TodoPlan`, so
  `plan_units` has a single entry there — the idle footer degrades to
  `next: row N` without a unit; that is the intended behavior.

### Step 5 notes (stats log)

- The terminal snapshot is at most one `stats_interval` stale (the periodic
  poll cadence); that matches the existing `--pi-plan report` transcript
  behavior and needs no extra RPC round-trip. If a fresher cost/token figure
  should appear in the JSONL specifically, a final `get_session_stats` read
  before the reaper can be a follow-up — not required for this change.
- `append_worker_stats` uses the same write-temp-then-rename atomicity as
  `save_state_file` so an interrupted run cannot corrupt the log, and
  `create_dir_all(root)` runs before the first write (a run-state root may
  be fresh). The rename makes each record a **full-file rewrite**, not an
  incremental append — at one record per run attempt this is trivially cheap,
  but the README should describe the file as an audit log: a reader
  `tail -f`ing across the rename will miss the newest line.
- Each record carries `"v": 1` up front so a later field addition is
  detectable by version rather than by guesswork, and records are written
  only for runs whose terminal snapshot exists (a `QuestionPause` or
  abort-before-stats writes nothing).

### Review trail

Reviewed 2026-09-13 with the review-plan pass; a second pass the same day
resolved every finding in place (F1–F8):

- **F1 (idle transition):** the liveness flip lives in `note_row_terminal`, not
  in the snapshot path — `worker_tail` has no `AgentSettled` arm, and the
  event channel closes ~250 ms after the terminal, far short of the 2.5 s
  quiet cadence. Resolved in Item 3.
- **F2 (mode parity for 2B):** the tool-context rows are emitted by **both**
  `dialog_lines` and `modal_dialog_rows` over the same context argument;
  `dialog_roundtrip` threads the worker's pending tool in. Resolved in
  Part B.
- **F3 (wrap width):** content rows word-wrap at the text-cell width
  `inner − 1`, never `inner`, so no chunk is ever ellipsized. Resolved in
  Part A / Step 2 notes.
- **F4 (heading/option wrapping + clip):** every content row wraps like every
  other; the `▸` lives on the focused row's first chunk; `clip_modal_rows`
  is upgraded to honor `focus_line`. Resolved in Part A / Step 2 notes.
- **F5 (hook semantics):** `last_completed` renamed `last_terminal`; the label
  is `terminal_kind_label(terminal)` (`completed`/`failed`) and fires on
  retried attempts too. Resolved in Item 3.
- **F6 (durable JSONL):** records carry `"v": 1`, skip snapshot-less runs,
  and the full-file-rewrite semantics of temp+rename are documented.
  Resolved in Item 3 / Step 5 notes.
- **F7 (CRLF):** `dialog_title_lines`/`dialog_message_lines` strip a trailing
  `\r`; unit-tested. Resolved in Part A.
- **F8 (test gap):** step 4 gains an end-to-end idle-transition test (plan/
  worker set → `note_row_terminal` → idle footer, no worker stats, no header
  status context). Resolved in Tests / commit 4.

Still open at implementation time:

- `serde_json::Value` on `WorkerSnapshot` (serde_json is pinned to
  `=1.0.151` in `Cargo.toml`; its `Value` derives `PartialEq`/`Eq` — verify
  the new field keeps the snapshot's `PartialEq` derive valid at step-3 CI).
- Idle-line width budget: `idle · last: row N completed · next: row M — unit`
  must fit the footer width (truncate with ellipsis via the existing
  `pad_line_to`/`truncate_with_ellipsis` guard in `format_footer_line`).
