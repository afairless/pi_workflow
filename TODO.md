# Implementation Plan: TUI arrow-key selection with row highlighting

Source: `docs/research/plan-tui-arrow-key-selection.md`

Adds an **additional** way to answer a permission dialog in the full-screen
supervise TUI (`src/tui.rs` + shared dialog text in `src/ui.rs`): ↑/↓ move a
row highlight across the dialog's choices and Enter selects the highlighted
row (rpiv `ask-user-question` style). Typed replies (option numbers,
`y`/`n`/`c`, `c`/`cancel`, `stop`/`restart`/`status` line commands) are
**retained unchanged** — arrows are strictly additive. Every change is
**TUI-mode only**; line mode (typed replies, byte-exact stdout) is never
touched.

Locked decisions (Q&A 2026-09-12): TUI mode only (Q1); first option
pre-highlighted on open, bare Enter submits it (Q2); confirm dialogs get
arrows too (Q3); focused row = `▸` marker + accent background fill (Q4);
confirm pre-highlights **no** — TUI row order `(n) no`, `(y) yes`,
`(c) cancel` (Q5). Prerequisite fix: today's inline escape collector never
assembles an arrow sequence (`[`/`O` land in `0x40..=0x7e` and clear the
buffer, leaking the trailing `A`/`B` as typed chars) — step 1 ships a pure
`EscapeCollector` so sequences are seen whole at all. The plan was reviewed
on 2026-09-12; all review findings (collector behavior, out-of-range focus,
wraparound arithmetic, per-kind invalid-reply notes, confirm-wrap pin) are
baked in.

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit with the
message in the table → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: navigate focusable dialog rows with arrow keys in the TUI modal` | Collector fix + arrow decode + focus state + submit | `src/ui.rs`: `DialogRow`, `modal_dialog_rows`, `dialog_item_count`, `item_reply`; `src/tui.rs`: `EscapeCollector` (pure sequence assembler — the collector fix), `NavKey`, `parse_escape_nav`, `navigate_focus`, `dispatch_modal_submit`, `TuiState.modal_focus` (initialized by `open_modal`: `Some(0)` for Select/Confirm, `None` otherwise; reset by `close_modal`), `input_task` feeds the collector, sequence-complete ↑/↓ arms + Enter arm cloned with focus, per-kind invalid-reply note updated | Unit: `EscapeCollector` (one-shot `ESC [ A` → one complete sequence; sequence split across feeds; `ESC O B` → complete; `[`/`O` never complete early; digits/`;` held; 16-byte cap drops whole; lone `ESC` and terminator `ESC` recover; non-arrow CSI/DSR completes → `parse_escape_nav` → `None`); `parse_escape_nav` (both encodings × both directions; DSR report and other CSI sequences → `None`; empty seq → `None`); `navigate_focus` (wrap both ways — incl. confirm's `(n)` ↑ → `(c)` cancel wrap —, single item, `len 0` → `None`, `None` current + `len > 0` → `Some(0)`); property: round-trip `navigate_focus(navigate_focus(x, +1, len), -1, len) == x` for `len > 1`; `item_reply` (select option, select cancel, confirm `no`/`yes`/`cancel`, out-of-range → `None`); `dispatch_modal_submit` (typed number beats focus; typed `c` cancels with any focus; empty + focus submits the focused item; empty + out-of-range focus → invalid-reply parity; empty without items → invalid-reply parity; empty ASK → `AskAnswer(None)`; `stop`/`status` with focus → command wins); `open_modal` focus init per method; `apply_modal_decision` per-kind note texts; `modal_dialog_rows` select parity with `dialog_lines` (unit + property over request shapes), confirm renders `(n)/(y)/(c)` order; full suite green |
| 2 | `feat: highlight the focused dialog row in the TUI modal box` | Render the focus | `src/tui.rs::modal_box` gains `focus: Option<usize>`; content via `modal_dialog_rows`; focused row = `▸` marker (replaces the two-space indent) + `fg: user_message_text` + `bg: accent`; `clip_modal_rows` keeps the focused content row when the box overflows (note/input/bottom border survive; else bottom-anchored fallback); `compose_frame` passes `state.modal_focus` | Unit: focused select row carries the marker + accent bg while siblings stay `user_message_bg`; confirm modal renders 3 rows with `no` highlighted at rest; `clip_modal_rows` keeps the focused row when possible and falls back otherwise; existing `modal_box` height-cap / top-truncation / note+input-row tests stay green; `compose_frame` modal overlay test extended to assert the highlight row's fg/bg; full suite green |
| 3 | `docs: document TUI arrow-key selection and highlighting` | User docs | `README.md` "Permissions behavior": TUI dialogs support ↑/↓ to move the highlight and Enter to submit, bare Enter picks the first row (select: option 1; confirm: **no**), ↑ from the first row wraps to the last (confirm: no → cancel), typed numbers/`y`/`n`/`c`/`c`ancel and `stop`/`restart`/`status` unchanged, line mode typed-only; `src/tui.rs` module header operator-keys note; `docs/ARCHITECTURE.md` one paragraph on the modal focus model | `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`; docs read cleanly; manual check (real terminal): `pi-plan supervise` in a fake-RPC spike — select dialog responds to ↑/↓ with the highlight, Enter lands the highlighted option, typed numbers still work, confirm defaults to no |

### Step 1 notes (focus semantics)

- `modal_focus` is strictly a TUI-mode concept; `dispatch_modal_line` and
  `reply_from_input` are untouched, so line mode's code path cannot observe
  it. The Enter arm's clone tuple changes from `(modal, line)` to
  `(modal, line, focus)`. `input_task` also swaps the inline `seq` buffer
  for the pure `EscapeCollector` (the **fix** that makes arrow sequences
  assemblable at all) and routes complete sequences through
  `parse_escape_nav` — without it, no arrow sequence is ever seen whole
  and the feature cannot work.
- Opening a `Select`/`Confirm` dialog sets `modal_focus = Some(0)`; pressing
  ↑ before any ↓ wraps to the last item (rpiv parity) — on a Confirm that
  means ↑ from the pre-highlighted `(n) no` lands on `(c) cancel` (pin with
  a test; state it in the README). Arrows with no modal / an `Ask` / an
  `Input`/`Editor` dialog are no-ops, dropped by the collector — a strict
  improvement over today, where they typed `A`/`B` into the input line.
- A `Select` dialog with zero options has one item (`(c) cancel`), so a bare
  Enter cancels — sensible, and the invalid-reply path for empty lines
  disappears only for dialogs that have items.

### Step 2 notes (rendering)

- Existing `modal_box` callers: only `compose_frame`; the signature change is
  confined to `tui.rs`. The Ask modal passes `focus: None`.
- `▸` is single-cell wide, so replacing `"  "` keeps the focused row exactly
  the same text width as its siblings; box rows are padded to exact width
  regardless (`pad_line_to`).
- The bundled gruvbox palette resolves `accent` to `rgb(250,189,47)` amber
  and `user_message_text` to terminal default — amber fill with the default
  foreground is readable on dark terminals (the fzf-style selection look) and
  needs no new theme token.

### Step 3 notes (docs)

- Keyboard-driven E2E is out of scope (needs a pty); the docs step carries
  the real-terminal manual verification instead: `pi-plan supervise` in a
  fake-RPC spike — select dialog responds to ↑/↓ with the highlight, Enter
  lands the highlighted option, typed numbers still work, confirm defaults
  to no.

### Open items carried from the plan review

- `clip_modal_rows` edge cases (focused row = cancel when the list exceeds
  the viewport) and the duplicate-option asymmetry between the typed and
  index-based paths are flagged for the Step 2 review; accepted as-is for
  now (fixing the typed-path quirk would change line mode).
