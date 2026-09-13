# pi-plan

A deterministic, TS-free orchestrator for the TODO.md workflow. `pi-plan` is a
single Rust binary that supervises TODO.md rows by spawning a fresh
`pi --mode rpc` worker process per row, enforcing a per-row run budget,
answering permission dialogs inline over the RPC extension-UI sub-protocol,
and classifying completion git-keyed. No LLM sits in the control loop; the
repository stays the source of truth.

## Install

Requires Rust 1.91.1 (enforced by `rust-toolchain.toml`), `pi` ≥ 0.85.1
on `PATH` (the binary spawns `pi --mode rpc` workers), and the
**`implement-from-plan` skill** installed for pi (default
`~/.pi/agent/skills/implement-from-plan/`, override with `$PI_PLAN_SKILL`).
Supervised runs are hard-required to have it: the frontmatter-stripped
`SKILL.md` body is loaded once at startup and injected into every worker's
first prompt; a missing, unreadable, or unresolvable skill fails fast
before any worker spawns.

The **`clean-worktree` skill** is a **soft** prerequisite: it is resolved
**lazily**, only when the dirty-worktree gate would abort an owner-less
dirty tree (default `~/.pi/agent/skills/clean-worktree/`, override with
`$PI_PLAN_CLEAN_SKILL` — see the Worker contract below). Healthy runs never
touch it; without it, a dirty tree aborts exactly as before, with a
guidance tail in the report.

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
| `pi-plan stop` | Ask a running `supervise` to stop at the next boundary (one-shot `.pi-plan-stop` control file under the external run-state root; a stale file from a killed run is discarded and never inherited). |
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
argv: `--mode rpc --session-dir ~/.pi-plan/<key>/sessions
--name pi-plan-row-<n> --model <model> --thinking high --approve
--tools read,grep,find,ls,bash,edit,write --skill <implement-from-plan>
--no-lsp --no-lens --no-tests --no-autoformat --no-autofix --no-opengrep
--append-system-prompt <persona>`. The persona preamble lives in
`prompts/worker-persona.md`; the row prompt (`src/prompt.rs`) names the
project, plan source, TODO.md path, exact row text, and planned commit
message. The implement-from-plan skill body (frontmatter stripped) is
framed in a bounded section of that first prompt ("…has been loaded for
you automatically"), loaded **once** at `supervise`/`step` startup — the
worker never reads the `SKILL.md` file itself.

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

### The clean-worktree agent

When the dirty-worktree gate sees an **owner-less** stray (`git status
--short` non-empty and no recorded in-progress owner for the row), a
dedicated `pi-plan-clean-<n>` agent gets the tree instead of an immediate
abort: the supervisor spawns it with **both** skills (`--skill
implement-from-plan` and `--skill clean-worktree`) and a prompt that
frames the clean-worktree skill as its **only operating instruction** —
the implement-from-plan body is reference context for navigation, and an
operative-line pin explicitly forbids it from implementing the row (the
next worker owns that). The clean pass is **budget-free and one-shot**: it
runs entirely before the row worker's spawn, never touches the row's
2-run budget, and writes no state.

Success is **git-keyed**, never marker-trusted: the pass counts only if
the agent ends `PI_WORKER_STATUS: COMPLETE`, `git status --short` is empty
afterwards, and no commit matches the row's planned message. On success
the row proceeds as if the tree had been clean. A clean agent's `ASK`
**pauses the run interactively** exactly like a row question — the same
TUI modal / `answer>` line-mode prompt, with `stop`/`restart`/`status`
commands — and the human's answer is folded into a **re-generated** clean
agent that retries the pass with that guidance. The answered continuation
lives for exactly one pass: a second consecutive ASK terminates, and a
carried answer that cannot be consumed (the tree became clean while
you were answering) ends the run with the question and a never-consumable
tail in the report — never a silent drop. Anything else (STUCK, a process
exit, a still-dirty tree, a commit matching the row's planned message)
appends a clean-attempt record (attempt marker `0` — never readable as a
spent budgeted run) and falls back to the existing "working tree not
clean" abort. The skill is resolved lazily; a missing skill falls back to
that abort with a guidance tail (see Troubleshooting).

## Permissions behavior

Workers run with permission `ask` intact — dialogs are never auto-approved.
In `--mode rpc` a permission `ask` surfaces as an `extension_ui_request`
frame on the worker's stdout; `pi-plan` renders it in your terminal and
forwards your reply (`extension_ui_response`) on stdin. This works for
`select`, `confirm`, `input`, and `editor` methods. What pi-plan never does:
hard-deny or auto-allow a permission decision; everything gated by your
global permission system is presented to you verbatim.

**TUI dialogs also support arrow-key selection.** In the full-screen TUI,
↑/↓ move a highlight across the dialog's rows and Enter submits the
highlighted row — an additive alternative to typing, rpiv-style. A dialog
opens with the first row pre-highlighted, so a bare Enter picks the first
row (a select's option 1; on a **confirm** that is **no**, so an
accidental Enter never grants permission). ↑ from the first row wraps to
the last (on a confirm: `no` → `cancel`). Typed replies still win on
Enter — option numbers, `y`/`n`/`c`, `c`/`cancel` — and
`stop`/`restart`/`status` work as before. Line mode (piped stdin,
`--answer`) stays typed-only.

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
| `pi-plan: cannot create the run directory …` | The external run root (`~/.pi-plan/<key>/sessions`, or `$PI_PLAN_STATE_DIR` when set) is not creatable — wrong `HOME`, unwritable base. Fix the base or set `PI_PLAN_STATE_DIR`. |
| Worker spawns but no trace appears | Check `~/.pi-plan/<key>/worker-stderr.log` for the spawned `pi` stderr (credential errors, bad model id). |
| "protocol error: …" / "oversized frame" | The worker's stdout was not clean JSONL (foreign `pi` version or a wrapper on `PATH`). Pin `pi` ≥ 0.85.1; check `~/.pi-plan/<key>/worker-stderr.log`. |
| Bad frames end the worker's stream | The stream is read strictly (LF framing, object frames only, 16 MiB cap). A corrupt peer kills that worker's read loop; the loop classifies and retries. |
| Credentials / model not found | Set the same auth used by your interactive pi (`~/.pi/agent/auth.json`, env). Symptoms land in `~/.pi-plan/<key>/worker-stderr.log`. |
| `Runs were used` unexpectedly after a crash | State recovery: matching `planHash` + row still unmatched in git resumes with `runsUsed` intact. A changed TODO.md recomputes from git. |
| Where are transcripts? | `~/.pi-plan/<key>/sessions/` (per-worker session dir passed via `--session-dir`); `~/.pi-plan/<key>/worker-stderr.log` holds process stderr. |
| Where is run state? | `~/.pi-plan/<key>/` holds `supervisor-state.json`, `sessions/`, `worker-stderr.log`, and the `.pi-plan-stop` control (key = sanitized cwd basename + sha256-8 of the canonical cwd). `$PI_PLAN_STATE_DIR` relocates the base (portable/CI override); every command hard-errors when neither `$HOME` nor the override is available. |
| `supervise` refuses: "working tree not clean" | The dirty-WIP gate: a dirty worktree with no recorded in-progress owner. The supervisor first hands the tree to a `pi-plan-clean-<n>` agent (Worker contract) — the clean-worktree skill ignores expected build artifacts, discards only verified formatting churn, and asks when in doubt. A failing clean pass falls back to the refusal and writes no state. |
| `clean-worktree skill not installed` in the report | The gate fired, the clean-worktree skill is not installed, and `$PI_PLAN_CLEAN_SKILL` is unset: the run aborts as before, no worker spawns, no state is written. Install the skill at `~/.pi/agent/skills/clean-worktree/` or point `$PI_PLAN_CLEAN_SKILL` at a directory holding a `SKILL.md`, then rerun. |
| `pi-plan stop` did nothing | Stop is consumed at the next boundary/terminal event; a fresh `stop` writes a new control file. A stale file from a killed run is discarded at startup. |
| Exit code 2 | Supervise ended with work outstanding (stopped / question / near-miss / budget) — inspect the final report on stderr. |

## Deferred features

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
