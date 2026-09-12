# Plan: TUI arrow-key selection with row highlighting

## Status

A research-to-build plan for `pi_plan_workflow`, a follow-up to
`docs/research/plan-supervise-tui-fixes.md` (implemented, merged). It adds an
**additional** way to answer a permission dialog in the full-screen supervise
TUI (`src/tui.rs` + the shared dialog text in `src/ui.rs`): ↑/↓ move a row
highlight across the dialog's choices and Enter selects the highlighted row.

Inspiration: the `@juicesharp/rpiv-ask-user-question` pi extension, whose
questionnaire lets the operator highlight options with ↑/↓ and select with
Enter. The typed-reply paths (option numbers, `y`/`n`/`c`, `c`/`cancel`, and
the `stop`/`restart`/`status` line commands) are **retained unchanged** — the
arrows are additive, exactly as the user requested ("we want to add the arrow
keys as an additional way to make a selection").

Design decisions were locked with the user on 2026-09-12 via the Q&A below.
No code was changed during this investigation.

## Goal (user requirements)

1. "The `@juicesharp/rpiv-ask-user-question` extension in Pi has a useful
   feature where, instead of only typing the number of the selection when I'm
   presented with a permission box, I can instead use the up and down arrow
   keys to highlight different options and then hit Enter to select one.
   Currently, the only way to make a selection in our TUI is to type the
   number of the selection. Can we add the arrow keys and highlighting?"
2. "We don't want to replace the typing-the-selection-number option; we want
   to add the arrow keys as an additional way to make a selection."

## Locked decisions (Q&A, 2026-09-12)

| # | Question | Decision |
|---|---|---|
| 1 | Which display modes get arrow-key selection? | **TUI mode only.** The full-screen TUI's modal box (raw-mode `input_task` + `modal_box`) gets ↑/↓ + highlight + Enter. Line mode (the non-TTY/piped fallback: `main.rs::dialog_roundtrip` over cooked `read_line`) keeps typed replies only — its byte-exact stdout and blocking read flow are untouched, and arrows are impossible anyway when stdin is piped. |
| 2 | When a select dialog opens, where does the highlight start? | **First option pre-highlighted** (rpiv-style). The box opens with option 1 highlighted, so a bare Enter selects it immediately. This *changes* today's "Enter on an empty line → invalid reply" for select/confirm dialogs — intentional and locked. |
| 3 | Should confirm (yes/no/cancel) dialogs get arrows too? | **Yes.** In the TUI modal the confirm hint becomes three highlightable rows (row order below); typed `y`/`n`/`c` still works. |
| 4 | How is the focused row highlighted? | **Marker + fill.** The focused row gets a `▸` marker (replacing its two-space item indent) and the theme accent as a background fill across the box row. |
| 5 | On confirm dialogs, which row is pre-highlighted? | **"No" is the first row and is pre-highlighted** (deny by default — an accidental Enter can never grant permission). TUI row order: `(n) no`, `(y) yes`, `(c) cancel`. Line mode's hint `(y) yes / (n) no / (c) cancel` is **unchanged**. |

Scope note: every change is **TUI-mode only**. Line mode's dialog rendering
(`ui::dialog_lines`, `ui::reply_from_input`, `main.rs::dialog_roundtrip`) is
never altered; `tests/tui_backend.rs` (the live terminal smoke) is untouched.

## Current state (evidence)

All references are to `HEAD`. Line numbers are approximate.

### Input: arrows leak their tail byte today (the collector is broken)

- `src/tui.rs::input_task` (~line 1385) is the single stdin owner in TUI
  mode. Single bytes map through `decode_key` (~line 611: `\n`/`\r` →
  `Keystroke::Enter`, `0x1b` → `Keystroke::Escape`, printable → `Char`).
- In the escape-sequence branch (~line 1417) any byte after `ESC` lands in
  `seq`, and the buffer is cleared when a byte in `0x40..=0x7e` arrives
  (or at the 16-byte cap). **`[` (`0x5b`) and `O` (`0x4f`) — the second
  byte of every arrow sequence — are inside that range**, so an arrow is
  never collected as a whole: `ESC [ A` clears at `[` and the trailing `A`
  falls through to `decode_key` → `Char('A')` → appended to the modal
  input line. Arrows are therefore **not swallowed today**: with a dialog
  open they type `A`/`B` into the input (Enter then yields "invalid
  reply"). A DSR size-report reply stolen mid-query leaks its printable
  bytes (`8;40;120t`) the same way. The `Keystroke::Escape` doc's
  "escape-sequence parsing is a v1.1 follow-up; ignored" records the
  intent, not the behavior. This plan **fixes the collector** as a
  prerequisite for arrow decoding (see Design).
- The Enter arm (~line 1455) clones `(modal, input_line)` under the state
  lock and calls `dispatch_modal_line` — there is no focus concept anywhere.

### Modal state and dispatch

- `TuiState` (`src/tui.rs` ~line 954) owns `modal: Option<Modal>` (the single
  open prompt), `modal_input`, `modal_note`, `modal_outcome`. `open_modal`
  (~line 1156) resets input/note/outcome; `close_modal` (~line 1164) clears
  the modal and records the outcome for `await_modal_outcome`.
- `dispatch_modal_line` (~line 272) is the pure typed-line dispatcher: line
  commands (`stop`/`restart`/`status`) win, then `ui::reply_from_input`
  (typed numbers / `y`/`n`/`c` / `c` or `cancel`); anything else keeps the
  modal open with `ModalNote::InvalidReply` (rendered by `apply_modal_decision`
  ~line 1361: "invalid reply — try again (or ^D to dismiss)").

### Rendering

- `src/ui.rs::dialog_lines` (~line 502) builds the dialog text for **both**
  modes: heading, message lines, then for `Select` the numbered options
  (`"  {n}. {option}"`) plus `"  (c) cancel"`, for `Confirm` the single hint
  line, for `Input`/`Editor` placeholder/prefill lines.
- `modal_box` (`src/tui.rs` ~line 366) draws the TUI modal: a bottom-anchored
  bordered box with `dialog_lines` content, a dim note row, and the `{label}>
  {input}▌` input row. Every row is `StyledLine {text, fg, bg}` — styling the
  focused row is a per-row fg/bg change only.
- `compose_frame` (~line 1206) overlays `modal_box` over the trace viewport;
  the render task (`main.rs::render_task` ~line 966) full-frame redraws every
  120 ms from shared state, so **mutating `modal_focus` in `TuiState`
  repaints the highlight automatically** — no dirty flag needed.
- `footer_lines` (`src/tui.rs` ~line 149) shows `stop / restart / status`
  hints; unchanged by this plan (the highlight itself is the affordance).
- The DSR size-report reply (`ESC [ <r> ; <c> R` / `... t`, parsed by
  `parse_size_report`) ends in `R`/`t`, not `A`/`B` — **no collision** with
  arrow decoding; a reply stolen mid-query is dropped whole by the fixed
  collector. (Today its printable bytes leak into the input line, so this
  is a behavior fix, not parity.)

### Which requests become dialogs

- `src/rpc.rs::is_dialog` (~line 186) gates exactly `Select | Confirm | Input
  | Editor`. `worker_tail` (`main.rs` ~line 833) opens `Modal::Dialog` for
  those in TUI mode and awaits the outcome; the ASK pause
  (`main.rs` ~line 428) opens `Modal::Ask`. Only `Select`/`Confirm` get
  focusable rows — `Input`/`Editor` need raw text and `Ask` is free-form.

## Design

### Focusable items

A dialog's *items* are the rows an arrow can highlight. The item count and the
reply each item maps to are pure and index-based (no text parsing):

| Method | Items (TUI modal row order) | `item_reply(req, idx)` |
|---|---|---|
| `Select` | `1. {opt0}`, …, `N. {optN-1}`, `(c) cancel` — count `N+1` | `idx < N` → `UiReply::Value(options[idx])`; `idx == N` → `Cancelled` |
| `Confirm` | `(n) no`, `(y) yes`, `(c) cancel` — count `3` | `0` → `Confirmed(false)`; `1` → `Confirmed(true)`; `2` → `Cancelled` |
| `Input`/`Editor`/`Ask` | no items | — |

Notes:

- Confirm's TUI row order (**no first**, locked Q5) differs from line mode's
  hint order; line mode is untouched. Focus follows the TUI's own rows.
- Initial focus (`open_modal`): `Some(0)` for `Select`/`Confirm` (select →
  option 1; confirm → **no**), `None` otherwise.
- The index-based focus path makes the second of two duplicate option labels
  reachable, whereas the typed path (a pre-existing `reply_from_input` quirk)
  maps both to the first label's number. Accepted as-is (fixing it would
  change line mode); noted for the reviewer.

### Arrow decoding (pure, in `src/tui.rs`)

New `NavKey { Up, Down }` and:

```rust
/// Map one collected escape sequence to an arrow key; `None` → swallow
/// (incl. DSR size-report replies).
pub fn parse_escape_nav(seq: &[u8]) -> Option<NavKey>
```

Recognized (both encodings): `ESC [ A`, `ESC O A` → `Up`; `ESC [ B`,
`ESC O B` → `Down`. Everything else → `None`. The `Keystroke` enum and
`decode_key` are **unchanged** (single-byte map).

**Collector fix (prerequisite — the current collector never assembles an
arrow sequence):** a pure `EscapeCollector` replaces the inline `seq`
buffer in `input_task`:

```rust
/// Verdict on one raw byte fed to the collector.
pub enum Collect {
    /// One COMPLETE escape sequence, ready for `parse_escape_nav` (or drop).
    Sequence(Vec<u8>),
    /// A non-`ESC` byte with nothing pending — handle as today
    /// (`decode_key`).
    Plain(u8),
    /// More bytes needed to complete the sequence.
    Pending,
}

pub struct EscapeCollector { /* pending: Vec<u8> */ }
impl EscapeCollector {
    /// Feed one raw byte; precisely one verdict per byte.
    pub fn feed(&mut self, byte: u8) -> Collect;
}
```

Rules (all pure, unit-tested):

- Nothing pending: a non-`ESC` byte → `Plain(byte)` (the caller runs
  `decode_key` exactly as today); `ESC` (0x1b) starts a pending sequence
  → `Pending`.
- Pending with only `ESC`: `[` (0x5b, CSI) and `O` (0x4f, SS3) push
  without completing → `Pending`; parameter/intermediate bytes
  (`0x20..=0x3f`: digits, `;`, etc.) also push; the first final byte in
  `0x40..=0x7e` completes → `Sequence(pending + byte)`, buffer reset. So
  `ESC [ A` assembles all three bytes and `parse_escape_nav` sees the
  full sequence **exactly once**.
- Any other byte while pending (control bytes, a second `ESC`)
  completes the current sequence (`Sequence`) and restarts: a second
  `ESC` begins the next pending buffer, other bytes are dropped with the
  completed sequence — the buffer can never hang.
- A 16-byte cap (unchanged) completes and returns the sequence, so
  pathological input is dropped whole.
- A sequence split across `read()`s is unaffected (the collector persists
  across poll iterations, like today's `seq` buffer).

`input_task` feeds every raw byte to the collector first: `Sequence`
results go to `parse_escape_nav` (arrow → navigate) or are dropped
(DSR replies, other CSI/SS3 → `None`); `Plain` bytes flow to `decode_key`
as today. Net behavior change vs today: arrows/DSR junk no longer type
`A`/`B`/digits into the input line.

### Focus state and navigation

- `TuiState.modal_focus: Option<usize>` — set by `open_modal` (per above),
  reset by `close_modal`.
- Pure: `navigate_focus(current: Option<usize>, delta: i32, len: usize) ->
  Option<usize>` — wraparound (↓ past the last → 0; ↑ past 0 → last; rpiv
  parity); `len == 0` → `None`; `current == None` with `len > 0` →
  `Some(0)` (defensive — arrows only act on item dialogs, which
  `open_modal` always initializes). Wraparound uses `i64` arithmetic so
  the `usize` underflow on `↑` past `0` is well-defined.
- `input_task`:
  - sequence end (`EscapeCollector` returns `Sequence`):
    `parse_escape_nav(seq)` — `Some(Up|Down)` → lock, and when the open
    modal is a dialog with items, set
    `modal_focus = navigate_focus(modal_focus, delta, item_count)` and clear
    `modal_note` (the status note is stale after navigation); `None` →
    drop the sequence (DSR replies, other CSI/SS3 — no longer leaked as
    typed chars).
  - `Enter` arm: clone `(modal, input_line, focus)` under the lock, then
    dispatch through the new pure `dispatch_modal_submit`:

```rust
/// Typed input wins (line commands, numbers, y/n/c, c/cancel — byte parity
/// with today); an EMPTY line with a focused dialog item submits that item;
/// otherwise the old path (empty select-less dialog → InvalidReply, empty
/// ASK → no answer) applies unchanged.
pub fn dispatch_modal_submit(modal: &Modal, input: &str, focus: Option<usize>) -> ModalDecision
```

Precedence: (1) non-empty input → `dispatch_modal_line(modal, input)` exactly
as today; (2) empty input + dialog with items + `focus = Some(i)` → when
`item_reply(req, i)` is `Some` → `Close(DialogReply(reply))`, and when it
returns `None` (defensive: out-of-range focus) → fall through to case (3),
invalid-reply parity — the modal stays open with the note; (3) empty input
otherwise → `dispatch_modal_line(modal, "")` (ASK blank →
`AskAnswer(None)`, dialogs without items → invalid-reply note; unchanged).

### Rendering the highlight

- New in `src/ui.rs` (the pure renderer, kept string-only):

```rust
pub struct DialogRow { pub text: String, pub focused: bool }
/// TUI modal content: heading, message, then the focusable rows with the
/// focused flag set; `Select` rows are byte-identical to `dialog_lines`
/// (parity-tested); `Confirm` emits the three-row (n/y/c) list; `Input`/
/// `Editor` emit `dialog_lines`-identical rows, all unfocused.
pub fn modal_dialog_rows(req: &ExtensionUiRequest, focus: Option<usize>) -> Vec<DialogRow>
```

- `modal_box(palette, modal, input, note, focus, width, height)`:
  - content from `modal_dialog_rows` (Dialog) / the Ask text (unchanged);
  - the row where `focused` is a **full-row accent fill**: `text` with the
    leading two-space item indent replaced by `▸` (same cell width — `▸` +
    space), `fg: palette.user_message_text`, `bg: Some(palette.accent)`;
    sibling rows keep today's `fg: palette.text, bg: user_message_bg`.
  - `clip_modal_rows(rows, height, focus_line)` — when the box exceeds the
    viewport, keep a window that includes the focused content row when it can
    do so without cutting the input row and bottom border; otherwise fall
    back to today's top-truncation (input row + bottom border always
    survive).
- `compose_frame` passes `state.modal_focus` into `modal_box`.
- The typed input line, prompt label (`select>`), note row, and both line
  commands are unchanged. The invalid-reply note becomes per-dialog-kind
  (`apply_modal_decision` reads the open modal): Select →
  `"invalid reply — type an option number or use ↑/↓ + Enter (^D to
  dismiss)"`; Confirm → `"invalid reply — type y/n/c or use ↑/↓ + Enter
  (^D to dismiss)"`; Input/Editor → unchanged
  `"invalid reply — try again (or ^D to dismiss)"` (they have neither
  option numbers nor focusable rows).

### Explicitly out of scope

- Line mode (typed replies only; byte-exact stdout preserved).
- `Keystroke`/`decode_key` changes, rpc protocol, `dialog_lines`,
  `reply_from_input`, `option_lines`, theme schema (no new palette token).
- Home/End/PgUp/PgDn navigation; scrolling long lists beyond the box clip;
  typed-digit ↔ highlight sync (typing stays an independent path — a typed
  reply wins on Enter); footer hint text.
- Keyboard-driven E2E (needs a pty): the pure functions carry the logic; the
  docs step adds a manual verification.

## Commit-by-commit plan

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

### Review trail

Reviewed 2026-09-12 with the review-plan pass; all findings addressed in
the document:

- **Critical (fixed):** the escape-sequence collector never assembled an
  arrow sequence — `[`/`O` are in `0x40..=0x7e`, so collection cleared at
  the second byte and the trailing `A`/`B` leaked as a typed char into the
  modal input. Step 1 now ships a pure `EscapeCollector`; without it the
  feature cannot work. (Today's "swallowed" premise was wrong, and the
  DSR-stolen-reply claim was likewise corrected — the fixed end state
  genuinely drops them.)
- **Fixed:** `dispatch_modal_submit` with an out-of-range focus now falls
  back to invalid-reply parity instead of a `Close(DialogReply(None))`
  type contradiction; tested.
- **Fixed:** `navigate_focus(None, len > 0)` is defined as `Some(0)`;
  wraparound uses `i64` arithmetic (no `usize` underflow).
- **Fixed:** the invalid-reply note is per-dialog-kind (Select/Confirm
  advertise ↑/↓ + Enter; Input/Editor keep today's text).
- **Fixed:** confirm's ↑-from-`no` → `cancel` wrap is pinned by a test and
  documented in the README step.
- **Testing additions:** `EscapeCollector` unit coverage, a
  `navigate_focus` round-trip property test, and a `modal_dialog_rows` /
  `dialog_lines` parity property test.
- Still open for the Step 2 review (flagged in Design): `clip_modal_rows`
  edge cases (focused row = cancel when the list exceeds the viewport) and
  the duplicate-option asymmetry between the typed and index-based paths.
