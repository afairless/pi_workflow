# Implementation Plan: Supervise TUI fixes — Ctrl-D kill switch, flowing token stream, bottom-anchored permission prompts

Source: `docs/research/plan-supervise-tui-fixes.md`

Plan: fix three operator-visible defects in the full-screen supervise TUI
(`src/tui.rs` + wiring in `src/main.rs`/`src/ui.rs`/`src/supervise.rs`):
(1) a Ctrl-D **kill switch** that SIGKILLs every supervise-spawned `pi
--mode rpc` worker process group (leaving unrelated Pi sessions alone — the
groups are distinct by construction), unwinds the TUI, prints the final
report, and exits 2; (2) a **flowing token stream** — token deltas append to
one open line (no per-token rows, no `⟦thinking: …⟧` wrappers) that closes at
`\n` and at thinking↔text kind changes, with thinking as a separate gray
block; (3) **bottom-anchored permission prompts** — the dialog sits directly
above the persistent footer with the newest generated text still visible
above it. Line mode stays byte-for-byte unchanged (the kill is TUI-only; the
line mode Ctrl-D remains EOF). No new crates. The plan was reviewed on
2026-09-12 and findings F1–F6 are baked in (see the plan's "Review trail":
kill_watcher flag order, kill-only `final_result` fix scoped via
`RunControl.kill_requested`, docs sweep including the stale "No full-screen
TUI" bullets, poll-window wording, trailing-newline blank-row note,
`stream_part` as the single delta classifier).

Workflow per step: implement → `cargo test` → `cargo fmt --check` →
`cargo clippy --all-targets --all-features -- -D warnings` → commit with the
message in the table → stop. Suggested order 1 → 2 → 3 (independent; 2 and 3
both touch `compose_frame` in disjoint spots — landing out of order only
makes the merge conflicts trivial). The post-build manual tmux/ghostty
verification gates live in the plan and are rerun as a whole after Step 3.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: stream text and thinking tokens into flowing styled blocks` | Flowing tokens (fix 2) | `src/tui.rs`: `TuiState.stream` (`StreamLine { kind, text }`), `append_stream` (`\n` split, `\r\n` normalize, kind-change flush, 10 000-char cap + truncation banner), flush-open-line on `push_line`/`push_banner`/`set_plan`, `compose_frame` renders `ring + stream`, `trace_lines` emits blank rows for empty logical lines; `src/ui.rs`: `stream_part(delta)` as the **single** delta classifier with `apply_delta` delegating to it (line-mode bytes preserved); `src/main.rs`: `worker_tail` TUI branch routes `MessageUpdate` deltas through `append_stream` | unit, property |
| 2 | `feat: anchor permission prompts at the bottom above the live trace` | Bottom modal (fix 3) | `src/tui.rs`: `modal_box` bottom-anchored exact-height `box_h` (top-truncate when taller than the viewport, no centering, no padding), `compose_frame` stacks trace-above / box-below (box at the very bottom, above the untouched 1-row footer), delete the dead `dialog_box` helper + update the module anatomy comment | unit |
| 3 | `feat: add Ctrl-D kill switch that terminates the TUI and all workers` | Kill switch (fix 1) | `src/supervise.rs`: `RunControl.kill_requested` (only reader: the ASK pause flow — review F2); `src/tui.rs`: `input_task` gains `kill: Arc<AtomicBool>`, Ctrl-D arm sets it once + closes any modal with `ModalOutcome::Stop` (no modal → banner), stdin-closed EOF arm unchanged; `src/main.rs`: `kill_watcher` flips `kill_requested`+`stop_requested` **before** `workers.dispose()` (review F1), ASK-pause Stop outcome sets `final_result = Some(result)` only when `kill_requested` is set (kill → report + exit 2 instead of the `Err` quirk; Ctrl-C/blank unchanged), plus the docs sweep (review F3): `docs/ARCHITECTURE.md` `Ctrl-D` sentence + stale "No full-screen TUI" Limitations bullet, `README.md` deferred-features bullet, `src/tui.rs` module anatomy + `input_task` docstrings | unit, integration |

Post-build verification (manual, tmux/ghostty — full checklist in the plan):
tokens flow on one line with gray thinking block and fresh-line answer start;
paragraph gaps at `\n`; prompt box pinned above the footer with newest text
visible; Ctrl-D mid-stream/dialog/ASK kills only supervise-spawned workers
(report + exit 2; a second interactive `pi` survives); Ctrl-C behaves as
before; `pi-plan supervise | cat` line mode byte-identical.
