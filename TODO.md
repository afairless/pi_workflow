# Implementation Plan: Themed Full-Screen TUI for the `pi-plan` Supervisor Display

Source: `docs/research/plan-tui-display.md`

Plan: replace the supervisor's plain monochrome line output (`src/ui.rs`) with
a full-screen TUI that matches the operator's live Pi terminal — same active
theme (resolved at runtime from `~/.pi/agent`, currently gruvbox-dark),
Pi-style layout anatomy (header / trace viewport / persistent footer), a
persistent cost–context footer that **never occludes** the streaming trace
(dedicated scroll-region rows), and a persistent header showing
"step X/N — <logical unit>". Permission dialogs and ASK questions become
Pi-style centered modals answered through the TUI input line. When stdout is
not a TTY, the existing line mode is preserved byte-for-byte. No new crates:
hand-rolled ANSI on the already-pinned `nix 0.31.3` with two added feature
flags (`poll`, `term`). Design decisions locked 2026-09-12 (see plan Q&A).

Workflow per step: implement → `cargo test` → `cargo fmt --check` →
`cargo clippy --all-targets --all-features -- -D warnings` → commit with the
message in the table → stop. Integration tests (FakeWorkerPort) land in
Step 5; the manual tmux/ghostty verification gates are listed in Steps 4 and 6
and rerun as a whole after Step 7 (post-build verification in the plan).

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: add Pi theme loader with runtime resolution and fallback palette` | Theme loader | `src/theme.rs` (new): selection precedence (`--theme` > `~/.pi/agent/settings.json` > bundled gruvbox-dark palette), named resolution in Pi's order (global → built-in → project → packages, injected roots), var-alias resolution, token→`Palette` mapping; `src/cli.rs`: clap `--theme <path>` on `supervise`/`step` | unit, property |
| 2 | `feat: report cost, tokens and context window from get_session_stats` | Stats parsing | `src/worker.rs`: `WorkerSnapshot` gains `cost/tokens/context_window`; `stats_task` parses already-received responses via `cost_of`/`tokens_of`/`context_window_of` beside `context_percent_of`/`session_file_of` | unit |
| 3 | `feat: add pure TUI frame builders (header, trace, footer, dialog)` | Frame builders | `src/tui.rs` (new): pure `header_lines`/`footer_lines`/`trace_lines`/`dialog_box` + `TuiLine { kind, text }` ring element type; `src/ui.rs`: token→style map, `format_footer_line`, `format_header_line`; `render_event_line`/`apply_delta` tag lines by kind | unit, property |
| 4 | `feat: add thin terminal backend (alt screen, raw mode, resize)` | Terminal backend | `Cargo.toml`: `nix` gains `poll`+`term` features; `src/tui.rs`: alternate-screen enter/leave, termios raw-mode save/restore (`cfmakeraw`, `tcgetattr`/`tcsetattr`), `isatty` gate, `poll` stdin readiness, `sigprocmask` SIGWINCH block + `signalfd` thread→channel resize bridge | unit, manual (tmux) |
| 5 | `feat: wire TUI render loop with shared TuiState and line-mode fallback` | Render-loop wiring | `src/main.rs`: `TuiState` (Arc<Mutex>), 120 ms render task with full-frame redraw, TUI-vs-line dispatch on `isatty(stdout)`; `src/worker.rs` `worker_tail` writes live view (extended `WorkerSnapshot`) into `TuiState`; banners feed the ring in TUI mode; non-TTY path byte-identical | integration (FakeWorkerPort) |
| 6 | `feat: render permission dialogs and ASK questions as TUI modals` | Modal dialogs & input | `src/main.rs`: `dialog_roundtrip` + ASK pause become modal flows on the TUI path; `src/tui.rs`: single stdin reader task owning raw keys, modal input line, dispatch via unchanged `reply_from_input`/`line_command`; `^D`→`UiReply::Cancelled` parity, `^C`→abort+unwind | unit, manual (tmux) |
| 7 | `polish: apply theme styling to streaming content and finalize footer` | Content styling | `src/ui.rs`: token→SGR styling for thinking/tool/bash/turn lines and dialog boxes; header line 2 (source + live context); truncation of long units and paths | unit |
