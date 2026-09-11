# pi-plan

A deterministic, TS-free orchestrator for the TODO.md workflow. `pi-plan` is a
single Rust binary that supervises TODO.md rows by spawning a fresh
`pi --mode rpc` worker process per row, enforcing a per-row run budget,
answering permission dialogs inline over the RPC extension-UI sub-protocol,
and classifying completion git-keyed. No LLM sits in the control loop; the
repository stays the source of truth.

## Install

Requires Rust 1.91.1 (enforced by `rust-toolchain.toml`) and `pi` ≥ 0.85.1
on `PATH` (the binary spawns `pi --mode rpc` workers).

```bash
cargo build --release
# binary: target/release/pi-plan — copy it anywhere on PATH, or run in place
```

## Commands

Run every command with the project root (the directory holding `TODO.md`) as
the current working directory.

| Command | What it does |
| --- | --- |
| `pi-plan supervise` | Supervise the plan: spawn a fresh `pi --mode rpc` worker per open row, render live traces, answer permission dialogs inline, advance on git-keyed completion. Stops with a report on budget exhaustion, a question, a near-miss, or `stop`. Exit 0 when all requested rows are done, 2 when work is outstanding. |
| `pi-plan supervise --row N` | Supervise only row N, then stop. |
| `pi-plan supervise --answer "…"` | Pre-answer a row's question; the answer is folded into the first worker's prompt. |
| `pi-plan supervise --config PATH` | Use a config file other than `./supervisor.config.json`. |
| `pi-plan step N [--answer "…"] [--config PATH]` | Supervise exactly one row and exit. |
| `pi-plan status` | Print plan source, per-row git-match tier, persisted state, and worktree cleanliness from a second shell while a run is live. |
| `pi-plan stop` | Ask a running `supervise` to stop at the next boundary (one-shot `.pi-plan-stop` control file; a stale file from a killed run is discarded and never inherited). |
| `pi-plan mark N done` | Adjudicate a near-miss: record row N as done without a matching commit; the next `supervise` skips it. |

### Line commands

During a run, `stop`, `restart` (alias `resume`), and `status` are accepted
at any dialog/answer prompt. `status` reprints the live status line.
`restart` aborts the running worker and spawns a fresh worker for the same
row without spending budget.

## Configuration

Optional `supervisor.config.json` in the project root (or `--config PATH`).
All fields optional; a missing/corrupt file falls back to defaults and never
fails the run.

```jsonc
{
  "maxTurns": 40,                      // global stall ceiling per worker
  "model": "openrouter/deepseek/deepseek-v4-flash",
  "steps": {
    "1": { "maxTurns": 20, "model": "…" },   // per-row override
    "3": { "model": "…" }
  }
}
```

Precedence: `steps.<n>` > config > default (40 turns,
`openrouter/deepseek/deepseek-v4-flash`). Unknown fields are ignored;
wrong types fall back to defaults.

## Worker contract

A worker is a **fresh `pi --mode rpc` process per attempt** with a pinned
argv: `--mode rpc --session-dir <cwd>/.pi-plan/sessions
--name pi-plan-row-<n> --model <model> --thinking high --approve
--tools read,grep,find,ls,bash,edit,write --skill <implement-from-plan>
--no-lsp --no-lens --no-tests --no-autoformat --no-autofix --no-opengrep
--append-system-prompt <persona>`. The persona preamble lives in
`prompts/worker-persona.md`; the row prompt (`src/prompt.rs`) names the
project, plan source, TODO.md path, exact row text, and planned commit
message.

The worker is told to end its final message with
`PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` and, when asking,
`QUESTION: <text>`. The marker is a **hint** — completion is classified
git-keyed (planned commit message vs `git log` subjects) and questions from
the ASK marker, never from the marker alone. A row is done iff an `exact`
or `similar` (ratio ≥ 0.9) commit match lands; a `candidate`
(prefix or ratio ≥ 0.8) near-miss stops for `pi-plan mark N done`.

Per-row budget: **2 runs** (initial + one automatic retry). Question pauses,
stops, and restarts spend nothing; every other terminal event spends one
run. Stall ceiling: 40 turns per worker by default (counted from
`turn_end` events, `maxTurns` in config), plus a wall-clock timeout.

## Permissions behavior

Workers run with permission `ask` intact — dialogs are never auto-approved.
In `--mode rpc` a permission `ask` surfaces as an `extension_ui_request`
frame on the worker's stdout; `pi-plan` renders it in your terminal and
forwards your reply (`extension_ui_response`) on stdin. This works for
`select`, `confirm`, `input`, and `editor` methods. What pi-plan never does:
hard-deny or auto-allow a permission decision; everything gated by your
global permission system is presented to you verbatim.

What differs from guardrails: the `@aliou/pi-guardrails` `pathAccess` gate
(`mode: ask`, allowlist only `/dev/null` on this machine) composes with the
permission system. Work inside the project directory (cwd subtree) is what
the spike exercises; out-of-cwd access is denied unless your permission
config allows it — that is intended. If a worker is denied an op, the
worker receives the denial and adapts; the loop classifies the run from its
outcome.

## Troubleshooting

| Symptom | Likely cause / fix |
| --- | --- |
| `pi-plan: cannot create the run directory …` | The cwd is not writable or not a real directory; run from the project root. |
| Worker spawns but no trace appears | Check `.pi-plan/worker-stderr.log` for the spawned `pi` stderr (credential errors, bad model id). |
| "protocol error: …" / "oversized frame" | The worker's stdout was not clean JSONL (foreign `pi` version or a wrapper on `PATH`). Pin `pi` ≥ 0.85.1; check `worker-stderr.log`. |
| Bad frames end the worker's stream | The stream is read strictly (LF framing, object frames only, 16 MiB cap). A corrupt peer kills that worker's read loop; the loop classifies and retries. |
| Credentials / model not found | Set the same auth used by your interactive pi (`~/.pi/agent/auth.json`, env). Symptoms land in `worker-stderr.log`. |
| `Runs were used` unexpectedly after a crash | State recovery: matching `planHash` + row still unmatched in git resumes with `runsUsed` intact. A changed TODO.md recomputes from git. |
| Where are transcripts? | `<cwd>/.pi-plan/sessions/` (per-worker session dir passed via `--session-dir`); `.pi-plan/worker-stderr.log` holds process stderr. |
| `supervise` refuses: "working tree not clean" | The dirty-WIP gate: a dirty worktree with no recorded in-progress owner refuses and writes no state rather than spawning over strays. Commit/stash the strays, or mark/step after adjudicating. |
| `pi-plan stop` did nothing | Stop is consumed at the next boundary/terminal event; a fresh `stop` writes a new control file. A stale file from a killed run is discarded at startup. |
| Exit code 2 | Supervise ended with work outstanding (stopped / question / near-miss / budget) — inspect the final report on stderr. |

## Deferred features

- Full-screen TUI (ratatui) — plain terminal in v1, keyboard-driven
  split-pane dashboard later.
- Parallel workers — strictly sequential rows in v1.
- Auto mid-step respawn from context% — compaction banner + manual
  `restart` is the v1 bridge.
- `git2`/`gix` binding — shells out to `git` in v1.
- Retiring the TS `/supervise` extension — see `docs/ARCHITECTURE.md`
  (migration).

## Repo layout

```text
pi_plan_workflow/
  Cargo.toml            binary crate: pi-plan (pinned deps)
  rust-toolchain.toml   channel = "1.91.1"
  src/                  main, cli, config, todo, git, prompt, state,
                        worker, rpc, supervise, ui
  test-fixtures/        fake pi RPC peer + manual E2E spike repo
  prompts/              worker persona preamble
  docs/                 architecture + acceptance + research plans
  TODO.md               commit-by-commit implementation plan
```

## Development

```bash
cargo test                                    # all tests pass
cargo fmt --check                             # formatted
cargo clippy --all-targets --all-features -- -D warnings   # zero warnings
```

See `docs/ARCHITECTURE.md` for the system overview and
`docs/acceptance-e2e.md` for the end-to-end gate.
