# Plan: Themed Full-Screen TUI for the `pi-plan` Supervisor Display

## Status

A research-to-build plan for `pi_plan_workflow` (`/home/tr/Documents/pi_plan_workflow`).
The goal is to replace the supervisor's plain, monochrome line output with a
**full-screen TUI** that matches the operator's live Pi terminal appearance —
same active theme (currently **gruvbox-dark**), Pi-style layout anatomy, a
persistent cost/context footer that never occludes the streaming trace, and a
persistent header showing "step X/N — <logical unit>".

Design decisions were locked with the user on 2026-09-12 via the Questions &
Answers section below. All claims about Pi's theme system, RPC stats payload,
and the pinned `nix` crate surface were verified against the installed pi
0.85.1 package, `~/.pi/agent` configuration, and `~/.cargo/registry` sources.

## Goal (user requirements)

1. "Design a TUI for our display" — a real terminal UI, not styled line spam.
2. "As close as possible to the current Pi settings, including the color
   theme" — reuse Pi's active theme file and its visual language exactly.
3. Persistent footer with the **cost / context-window** readouts (the data
   behind the Pi TUI status bar), guaranteed **not to occlude** the streaming
   text.
4. Persistent header line showing **supervisor state**: which plan step is
   executing ("step 3/12") plus a few words describing the step.
5. Plan written to `docs/research/`; no code changed in this investigation.

## Locked decisions (Q&A, 2026-09-12)

| # | Question | Decision |
|---|---|---|
| 1 | Display mode | **Full-screen TUI** (alternate screen buffer, like Pi). Falls back to today's plain line mode whenever stdout is not a TTY (piping, log capture), so non-interactive use is unchanged. |
| 2 | Theme source | **Resolve Pi's active theme at runtime**: read `~/.pi/agent/settings.json` for the theme name, locate its theme JSON, apply its tokens. `--theme <path>` overrides. A bundled palette (gruvbox-dark tokens) is the last-resort fallback. |
| 3 | Permission dialogs | **Pi-style modal boxes** drawn centered inside the TUI; typed replies go through the TUI input line; `stop`/`restart`/`status` still work. |
| 4 | Step descriptor | **Logical unit** column of the TODO.md table (e.g., "step 3/12 — Crate skeleton"). |

## Current state (evidence)

### Renderer today (`src/ui.rs`)

- Pure string transform: `apply_delta` prefixes thinking chunks as
  `⟦thinking: …⟧`; `render_event_line` maps RPC events to plain lines
  (`tool: write (call_…)`, `── turn start`, `agent settled`, `$ bash chunk`);
  `format_status_line` produces `row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s`.
- Header comment: *"no ANSI codes in v1 (ANSI styling is optional/minimal)"* —
  the monochrome look is a stated v1 constraint, now to be lifted.
- Status lines are **printed into the trace** every ~2.5 s of quiet time or on
  `TurnStart` (`main.rs::render_status`), i.e. they spam the log.

### Interactive loop today (`src/main.rs`)

- `worker_tail` (one per worker) subscribes to RPC events:
  - text/thinking deltas → `eprintln` + `TraceRing` (240 lines),
  - `extension_ui_request` dialogs → `dialog_roundtrip` prints text menus to
    **stdout** (`dialog_lines`/`dialog_prompt_label`), reads a line from stdin,
    maps `UiReply`, sends it back through `WorkerPort::reply_extension_ui`,
  - `stop`/`restart`/`status` line commands at any prompt.
- ASK pauses print to stdout and read an answer line in `cmd_supervise`.
- stdout = dialogs/ASK; stderr = traces/banners/reports (`format_final_report`).

### Cost/context data already in-process (`src/worker.rs`)

- `stats_task` calls the RPC `get_session_stats` on every worker every
  `stats_interval` (5 s default) — the **same data source as the Pi TUI
  footer** (rpc.md § Session: `contextUsage` is documented as "the actual
  current context-window estimate used for compaction and **footer display**").
- Response shape (verified, rpc.md:554–595):

```json
{ "type": "response", "command": "get_session_stats", "success": true,
  "data": {
    "sessionFile": "…", "sessionId": "…",
    "userMessages": 5, "assistantMessages": 5,
    "toolCalls": 12, "toolResults": 12, "totalMessages": 22,
    "tokens": { "input": 50000, "output": 10000,
                "cacheRead": 40000, "cacheWrite": 5000, "total": 105000 },
    "cost": 0.45,
    "contextUsage": { "tokens": 60000, "contextWindow": 200000, "percent": 30 }
  } }
```

- Today only `contextUsage.percent` and `sessionFile` are consumed
  (`context_percent_of` / `session_file_of`). **`tokens` and `cost` arrive in
  the very same responses and are currently discarded** — no new channel or
  protocol work is needed, only parsing.

### Step-descriptor data (`src/todo.rs`, `src/supervise.rs`)

- `TodoRow { id, number, commit_message, logical_unit, deliverables, tests }`;
  `TodoPlan { source, prerequisites, rows, done_marked }`. The header's
  "few words" come from `logical_unit` (e.g. tag_tool TODO.md: "Crate
  skeleton", "JSON contract types", "Driver inspect").
- The supervise loop already knows the live row (`on_spawn` fires
  `(worker_id, row_number)`; `run_plan` iterates `todo.rows`), and the
  banner reports carry row transition facts.

### Theme system (verified)

- Pi loads themes from: built-ins (`dark`, `light` at
  `<pi>/dist/modes/interactive/theme/*.json`), global
  `~/.pi/agent/themes/*.json`, project `.pi/themes/*.json`, packages
  (`themes/` dirs or `pi.themes` entries in `package.json`), the settings
  `themes` array, and `--theme <path>`.
- This machine: `~/.pi/agent/settings.json` → `"theme": "gruvbox-dark"`, whose
  JSON lives at
  `~/.pi/agent/npm/node_modules/@victor-software-house/pi-curated-themes/themes/gruvbox-dark.json`
  (package declares `"pi": {"themes": ["./themes"]}`).
- Theme JSON format: `vars` (alias map) + `colors` (51 required tokens
  per the installed theme-schema.json; gruvbox-dark defines exactly 51)
  referencing vars or hex. `""` means "terminal default" (e.g. `text`).
  Optional tokens fall back (`thinkingMax`→`thinkingXhigh`,
  `searchMatchBg`→`selectedBg`, `searchMatchText`→`text`).
- gruvbox-dark palette (vars, after resolution) relevant to this design:

| Token | Color | Token | Color |
|---|---|---|---|
| `accent` | `#fabd2f` | `border` | `#928374` |
| `borderAccent` | `#fabd2f` | `borderMuted` | `#373737` |
| `success` | `#8ec07c` | `error` | `#fb4934` |
| `warning` | `#fabd2f` | `muted` | `#928374` |
| `dim` | `#928374` | `text` | default (`""`) |
| `thinkingText` | `#928374` | `userMessageBg` | `#2d2d2d` |
| `userMessageText` | default | `mdHeading` | `#ebdbb2` |
| `toolTitle` | `#ebdbb2` | `toolPendingBg` | `#303030` |
| `toolSuccessBg` | `#2f302f` | `toolErrorBg` | `#382f2e` |
| `toolOutput` | `#ebdbb2` | `bashMode` | `#fabd2f` |
| `thinkingHigh` | `#fabd2f` | `mdCode` | `#fabd2f` |
| `mdCodeBlockBorder` | `#c88b00` | `plain fg` | `#ebdbb2` |

### Terminal primitives available with zero new crates

All verified in `nix =0.31.3`. Two small **Cargo.toml feature additions** are
required — empty feature flags, no new transitive dependencies: `term`
(gates the entire `nix::sys::termios` module) and `poll` (gates the
`nix::poll` module). `signal`+`process` are already enabled and stay:

- `nix::unistd::isatty(fd: AsFd) -> Result<bool>` — TTY detection (ungated).
- `nix::sys::termios::{tcgetattr, tcsetattr}` + `cfmakeraw()` — raw-mode input
  (character-by-character, no echo; constants `VMIN`/`VTIME`, `ICANON`,
  `ECHO` available) — **requires adding the `term` feature**.
- `nix::poll::poll` — readiness check on the stdin fd — **requires adding
  the `poll` feature**.
- `nix::unistd::read(fd, buf)` — key bytes (ungated).
- `nix::sys::signal::SIGWINCH` / `signalfd` — terminal resize events (gated
  by the already-enabled `signal` feature; SIGWINCH must be blocked with
  `sigprocmask` for the signalfd to receive it, bridged to the tokio render
  loop via a std thread + notify channel — see "Input & signal ownership").

`tokio::signal::Signal` would require tokio's `signal` feature (pulls
mio/os-poll etc.) and is deliberately **not** used. This keeps AGENTS.md
dependency discipline (all deps exact-pinned, unused default features
disabled) — **no crossterm/ratatui and no new crates for v1** (see
Alternatives and risks).

## Design

### Screen anatomy (full-screen, alternate buffer)

```
┌┤ pi-plan · step 3/12 · Crate skeleton ├───────────  ← header (accent border, accent/border colors)
│ source: docs/research/interface-design.md           ← header line 2 (muted)
│ ⟦thinking: so the compiler⟧                         │
│ doesn't complain about empty modules.               │  ← trace viewport (scrollback ring,
│                                                     │     word-wrapped to width; thinking = gray
│ [tool: write (call_23b9…)] #1ebd2b-ish success      │     thinkingText, tools = toolTitle,
│                                                     │     errors = error, turns = borderMuted)
│                                                     │
├─────────────────────────────────────────────────────┤
│ $0.0451 · ctx 61% (59.3k/200k) · turns 4/40 · 1m30s │  ← persistent footer row (last line of
│ stop / restart / status                             │     the screen; never overlapped)
└─────────────────────────────────────────────────────┘
```

- Three fixed regions: **header** (top 2 rows), **trace** (the middle;
  a viewport over the bounded scrollback ring), **footer** (bottom 1–2 rows,
  drawn last, always visible). The trace scrolls exclusively inside rows
  `3..H-2` via a scroll region, so by construction **the footer and header
  never occlude streaming text** — this is the direct answer to requirement 3.
- **Modal dialogs** (locked decision 3) draw a bordered box centered over the
  trace (accent border, `userMessageBg`-style panel fill, numbered options)
  and pause trace scrolling until answered or cancelled. The input line
  appears inside the modal; `stop`/`restart`/`status` are still accepted there.
- **Alternate screen** (`\e[?1049h`/`\e[?1049l`), cursor hidden during
  redraws, shown before input prompts. On exit (including crash paths — see
  Risks) we restore the primary buffer and print the existing
  `format_final_report` to stderr exactly as today.

### Header (supervisor state)

- Line 1: `pi-plan · step {X}/{N} · {logical unit}` — accent for the step
  numbers, `text` for the unit, surrounded by a Pi-style border drawn in
  `border`/`borderAccent`.
  - `X` = 1-based position of the current row in the **full** TODO.md table;
    `N` = total rows. For `supervise --row n` / `step n`, X/N still track the
    overall table (find `n` in `todo.rows`), so the header answers
    "where are we in the whole plan" even for single-row runs.
  - Truncate with `…` at the header width; the unit is short by construction.
- Line 2 feeds from plan context: `source: {plan source}` (muted) plus, when a
  worker is live, the current context line (below).

### Firebase of the footer (live stats)

The footer is rendered from the latest `get_session_stats` data plus the
worker snapshot (all already polled):

```
${cost} · ctx {pct}% ({tokens}/{window}) · turns {t}/{max} · {elapsed} · row {id}/agent {agent}
```

- `cost`: from `data.cost` formatted à la Pi (`$0.0451`, or `4.51¢` under a
  dollar-equivalent threshold). `success` color when ≥ some threshold,
  `muted` otherwise (cosmetic, v1 default muted).
- `ctx {pct}`: `contextUsage.percent` (already rendered today as `ctx 61%`;
  `?` while null after compaction — keep that contract).
- `({tokens}/{window})`: `contextUsage.tokens` / `contextUsage.contextWindow`
  (the response key is `contextWindow` — verified rpc.md:586; a draft said
  `window`, which does not exist and must not be used) — new fields parsed
  from the same response. `tokens`/`percent` are both null immediately after
  compaction (rpc.md:595): render `?` for each, same contract as `ctx`.
- `turns {t}/{max}` / `{elapsed}` / agent id: from `WorkerSnapshot` +
  `resolve_max_turns` (already computed in `render_status`).
- Key hints (`stop / restart / status`) on a second footer row or trailing in
  dim — v1 puts them in `dim` on the same row right-aligned.
- The footer redraws on stats arrival (5 s cadence) and on every frame tick
  otherwise; it is a dedicated row, so "persistent but non-occluding" holds.

### Theme pipeline (`src/theme.rs`, new)

1. **Selection** (precedence): `--theme <path>` (new clap flag on
   `supervise`/`step`) → `~/.pi/agent/settings.json` `theme` value (string
   name) → bundled fallback palette (gruvbox-dark tokens embedded).
2. **Resolution** (named lookup in Pi's exact documented order — themes.md:
   built-ins → global → **project → packages** → settings array): global
   `~/.pi/agent/themes/<name>.json` → pi install built-ins
   (`<pi>/dist/modes/interactive/theme/<name>.json`, pi path derived from
   `which pi`/node root; optional) → project `.pi/themes/<name>.json`
   (pi only applies project themes after the project is trusted; v1 keeps
   global/built-in ahead so an untrusted project cannot shadow them) →
   installed packages: scan `~/.pi/agent/npm/node_modules/*/*/` for a
   `package.json` with a `pi.themes` array and match `<dir>/<name>.json`.
   The settings `themes` array is a v1 non-goal (active-theme resolution
   covers the locked decision). Missing name → fallback palette (never fail
   hard).
3. **Parse/resolve**: `serde_json::Value` → `Theme { vars, colors }` →
   resolve var aliases recursively → `Palette` struct keyed on the ~20 tokens
   this UI uses (any color: (r,g,b); `""` → `Default`).
4. **ANSI mapping** (pure): `Stylize::fg(color)` emits truecolor
   `\e[38;2;r;g;bm` (Default → no escape), `bg()` similarly. Group-level
   styling: `style.line` carries one fg + optional bg for tool rows.

All of 2–4 are pure and unit-testable without a terminal.

### Renderer (`src/tui.rs`, new)

- Kept to the repo convention: **layout is a pure transform; I/O is thin**.
  Pure functions take `(theme, state, width, height)` and return
  `Vec<StyledLine>`:
  - `header_lines(theme, plan_context, width)`,
  - `footer_lines(theme, stats_view, width)`,
  - `trace_lines(lines, viewport_width, viewport_height, scroll_offset)` —
    word-wrap + truncate, `…` on overflow,
  - `dialog_box(theme, req, width, height) -> Option<Vec<StyledLine>>` —
    centered box for `extension_ui_request`.
- Thin backend: alternate-screen enter/leave, cursor hide/show, raw mode
  (termios save/restore via `tcgetattr`/`tcsetattr` + `cfmakeraw`), a write
  path `\e[{line};{col}H` + SGR + text, and a `SIGWINCH` slot refreshed into
  the frame state.
- A **render loop task** (120 ms tick, or woken by a change notifier) reads
  the shared `TuiState` + trace ring, rebuilds and redraws. Full-frame redraw
  per tick is acceptable at these sizes (Pi's own TUI does `fullRender`).

### Shared state & wiring (`src/main.rs`, `src/worker.rs`, `src/ui.rs`)

- New `TuiState` (`Arc<tokio::sync::Mutex<…>>`), written by:
  - the supervise loop: plan meta (`step X/N`, `logical_unit`, `source`),
    loop status (`banner` lines feed a status text), set once per row,
  - `worker_tail`: the live per-worker view (agent id, turns, max, elapsed,
    ctx%, cost, tokens — read from the **extended `WorkerSnapshot`**),
  - dialog/ASK handlers: modal visibility + pending prompt,
  - the render loop: reads it.
- The shared **trace ring** lives inside `TuiState`; its element type becomes
  `TuiLine { kind, text }` (kind ∈ thinking / text / tool / bash / turn /
  banner / …) — the unstyled form of the renderer's `StyledLine`, which is
  `TuiLine` + resolved SGR from the token→style map. `render_event_line` and `apply_delta` keep their output but
  tag each line with its kind, so the output stage styles by kind instead of
  pattern-matching text. Banner/report lines (the `report:` seam) also enter
  the ring in TUI mode so no observable output is dropped.
- `WorkerSnapshot` gains `cost: Option<f64>`, `tokens: Option<Tokens>`
  (`Tokens { input, output, cacheRead, cacheWrite, total }`), and
  `context_window: Option<u64>`; `stats_task` parses the already-received
  response (helpers `cost_of`, `tokens_of`, `context_window_of` beside the
  existing `context_percent_of`/`session_file_of`). Pure parsers → unit-test.
- `ui.rs` gains pure `format_footer_line(...)`, `format_header_line(...)`, and
  token→style mapping; the existing pure `format_status_line` output becomes
  the header's second line while a worker is live (no more log spam).
- `main.rs`: decide TUI vs line mode with `nix::unistd::isatty(stdout)`
  (+ stderr being attached). In TUI mode `dialog_roundtrip` and the ASK
  prompt become modal+input-line flows; replies still travel
  `reply_extension_ui`/carried answer exactly as today.
- Non-TTY: the existing line path is preserved byte-for-byte (traces→stderr,
  dialogs→stdout).

### Input & signal ownership (review-locked 2026-09-12)

- **One stdin owner.** A single stdin reader task owns the raw key stream in
  TUI mode — raw mode disables `ICANON`, so the line-mode
  `BufReader::read_line` paths cannot assemble text anymore. Keys append to
  one shared input line; `\n` dispatches through the unchanged
  `line_command`/`reply_from_input` against the current modal/ASK target;
  `^D` maps to the line-mode EOF semantics (`UiReply::Cancelled` parity);
  `^C` maps to abort + TUI unwind (see Risks). `dialog_roundtrip` and the
  ASK loop stop reading stdin directly — they set modal state and await the
  input task, and their stdout prints are mode-gated (byte-equivalent in
  non-TTY mode).
- **SIGWINCH.** Block the signal (`sigprocmask`), read the signalfd in a
  dedicated std thread, and notify the render loop via a
  `broadcast::channel<()>`; the 120 ms tick refreshes the frame size from
  the shared state. No tokio io/signal feature is required (locked).

### Content styling (match Pi's look)

| Content | Style (gruvbox-dark tokens) |
|---|---|
| Thinking deltas | `thinkingText` (gray), still `⟦…⟧`-wrapped |
| Assistant text | default `text` |
| `tool: <name>` lines | `toolTitle` (`#ebdbb2`) with `toolPendingBg` fill; success/error rows use `toolSuccessBg`/`toolErrorBg` + `success`/`error` glyphs |
| Bash chunks (`$ …`) | `bashMode` (`accent`, `#fabd2f`) |
| Turn separators | `borderMuted` (`#373737`) |
| Dialog boxes | `borderAccent` border, `userMessageBg` fill, numbered options in `text`, cancel hint in `dim` |
| Header/footer frames | `border`/`borderAccent`; step numbers `accent`; hints `dim` |
| Banner/settle lines | `success` / `error` / `warning` by outcome |

v1 does **not** do markdown/syntax highlighting of tool output (see
Non-goals) — content stays monospace, exactly as Pi shows raw tool output.

## Implementation plan (commit-by-commit)

Each commit below is one green unit: implement → `cargo test` →
`cargo fmt --check` → `cargo clippy --all-targets --all-features -- -D warnings`
→ commit with the listed message.

| # | Commit message (conventional) | Scope | Tests |
|---|---|---|---|
| 1 | `feat: add Pi theme loader with runtime resolution and fallback palette` | `src/theme.rs`; clap `--theme` on `supervise`/`step` | unit: var resolution, alias cycles, `""`→default, lookup order (global/built-in/project/package via injected roots — pi's order), fallback on missing name/path, ANSI escape mapping; property (proptest): resolution is idempotent and never loops on any var aliasing |
| 2 | `feat: report cost, tokens and context window from get_session_stats` | `src/worker.rs` (`WorkerSnapshot`, `stats_task`, parsers) | unit: parse present/absent/null cost, tokens, contextWindow; snapshot shape |
| 3 | `feat: add pure TUI frame builders (header, trace, footer, dialog)` | `src/tui.rs` layout pure functions; `TuiLine { kind, text }` ring element type; `src/ui.rs` token→style + `format_footer_line` | unit: header `step 3/12 — unit` at various widths + the `--row n` numbering corner (`step 5/10` for row 5 of 10); footer formatting (cost `$0.0451`, `4.51¢`, `?` ctx, `?` tokens when null, truncation); trace wrap/truncate/scroll with `TuiLine` kinds; dialog box centering at H×W; width edge cases; property (proptest): wrapped lines never exceed the viewport width |
| 4 | `feat: add thin terminal backend (alt screen, raw mode, resize)` | `Cargo.toml` (nix gains `poll`+`term`); `src/tui.rs` backend (nix termios/poll/unistd/signalfd, sigprocmask SIGWINCH block, resize thread→channel bridge) | unit: pure parts only (`isatty` gate, keybyte→command mapping incl. `^C`→abort, `^D`→cancel), drop-guard unwind against a mocked terminal writer; manual keyboard script in tmux for backend |
| 5 | `feat: wire TUI render loop with shared TuiState and line-mode fallback` | `src/main.rs` (TuiState, render task, isatty dispatch), `worker_tail` writes state (ring holds `TuiLine`s), banners feed the ring in TUI mode | integration (FakeWorkerPort): TuiState transitions on spawn/banner/terminal; trace ring→viewport; fallback path byte-equivalent under non-TTY (incl. banner/dialog bytes) |
| 6 | `feat: render permission dialogs and ASK questions as TUI modals` | `src/main.rs` `dialog_roundtrip` + ASK pause modal path; `src/tui.rs` input handling — single stdin reader task owning raw keys, editing the current modal/ASK input line, dispatching via unchanged `reply_from_input`/`line_command` | unit: reply mapping (unchanged `reply_from_input`), modal layout, EOF→`UiReply::Cancelled` parity with line mode, `^C`→abort; manual tmux script for `stop`/`restart`/`status` inside a modal |
| 7 | `polish: apply theme styling to streaming content and finalize footer` | `src/ui.rs` style mapping for thinking/tool/bash/turn lines; header line 2 context | unit: styled-line mapping test (token→SGR string), truncation of long units/<paths>; full-suite gates |

Suggested branch/order: commits 1→2 (data layer) then 3→4 (renderer) then
5→6→7 (wiring/polish); 1–4 are independent and could be reordered safely.

Post-build verification (manual, tmux/ghostty):

- `pi-plan supervise` in tag_tool with a live worker → header shows
  `step 3/17 · Crate skeleton`-style line, footer shows `$… · ctx …%` and
  updates on the 5 s stats cadence, thinking renders gray, a permission
  prompt appears as a modal and answers round-trip.
- Resize the terminal → SIGWINCH re-flow within one frame.
- `pi-plan supervise | cat` → identical to today's line mode.
- `pi-plan status`/`stop`/`mark` unchanged.

## Risks, non-goals, alternatives

- **Crash safety**: alt-screen + raw mode must be unwound on every exit path
  (worker failure, Ctrl+C, EOF). Mitigation: a `guard`-style drop guard
  around TUI enter (restore termios + `\e[?1049l` in its drop). Note:
  `cfmakeraw` clears ISIG, so Ctrl+C is **not** SIGINT in TUI mode — the
  input task maps the `^C` keybyte to abort + unwind ("Input & signal
  ownership"). A hard kill (SIGKILL, crash) cannot run the guard and leaves
  the terminal raw; documented remediation is `reset`, and a `--no-tui`
  escape hatch is a v1.1 candidate. Non-TTY path unaffected.
- **Cost fidelity**: `cost`/tokens are provider-reported; may be 0 for some
  providers or while streaming (rpc.md). Footer renders `—` when absent.
  This is a display of pi's own numbers — same caveats as Pi's footer.
- **No new dependencies (v1)**: hand-rolled ANSI on `nix` (raw mode, SIGWINCH,
  poll) keeps the pinned-dep discipline; two nix feature flags (`poll`,
  `term`) are added — empty flags, no new transitive deps, still
  exact-pinned. Alternative: `crossterm` (mature, cross-platform) adds
  `=pinned` deps and a broader toolchain surface; kept as a fallback if
  terminal edge cases multiply.
- **Wide characters**: wrap/truncate width math must count East-Asian-wide
  cells so header/footer borders align with the styled trace (v1 targets
  ASCII-dominant traces; CJK/emoji degrade gracefully, tested at wrap
  boundaries).
- **Non-goals (v1)**: markdown/code syntax highlighting of assistant text or
  tool output; mouse support; search/scrollback navigation UI; multi-pane
  layout. The trace stays a bounded ring (cap raised from 240 to ~1000 with
  the re-render cost in mind).
- **Concurrency**: multiple workers (retry) share one footer/trace; the last
  spawn owns the footer view while both tails append to the ring. Spawn of a
  new worker re-seeds the header context line via `on_spawn`.

## Open questions for later

- Should tool-output diffs (`toolDiffAdded`/`toolDiffRemoved`/`toolDiffContext`
  tokens) get colored in v1.1?
- Should the trace be scrollable/pausable in the TUI (Pi has scrollback
  search) — likely a follow-up.
- Should the final report (`format_final_report`) also be themed, or stay
  plain on the primary buffer? (Current plan: stays plain; it is not the
  "display".)
