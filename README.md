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

The **`pi-permission-system` package** is hard-required: workers spawn
bare (`--no-extensions -e <package dir>`) with it as the **only** loaded
extension, so every worker access is gated by its relayed dialogs.
Resolution: `$PI_PLAN_PERMISSION_EXTENSION` first, else
`~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`; an
unresolvable package fails fast before any worker spawns (see
Troubleshooting).

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
| `pi-plan supervise --config PATH` | Use an exact config file (absolute path — used as-is, never resolved against the project). |
| `pi-plan step N [--answer "…"] [--config PATH]` | Supervise exactly one row and exit. |
| `pi-plan status` | Print plan source, per-row git-match tier, persisted state, and worktree cleanliness from a second shell while a run is live. |
| `pi-plan stop` | Ask a running `supervise` to stop at the next boundary (one-shot `.pi-plan-stop` control file under the external run-state root; a stale file from a killed run is discarded and never inherited). |
| `pi-plan mark N done` | Adjudicate a near-miss: record row N as done without a matching commit; the next `supervise` skips it. |
| `pi-plan reset-permissions [--yes]` | Clear every stored session grant (`~/.pi-plan/<key>/permissions.json`). Asks for confirmation unless `--yes`; prints how many grants were removed. |

Usage errors fail at clap parse time, before any command runs: any word
other than `done` in `pi-plan mark N done` (e.g. `mark 4 bogus`) prints
clap's usage output and exits 2 — a parse-time exit distinct from
supervise's exit-2 work-outstanding outcome.

### Line commands

During a run, `stop`, `restart` (alias `resume`), and `status` are accepted
at any dialog/answer prompt. `status` reprints the live status line.
`restart` aborts the running worker and spawns a fresh worker for the same
row without spending budget.

## Configuration

Optional `supervisor.config.json`. Resolved at supervise/step startup
(`status`/`stop`/`mark` never read config) in four tiers, highest first:

| Tier | File | Notes |
|---|---|---|
| `--config PATH` | exactly that file | **Absolute** — used as-is, never resolved against the project; project and global files are ignored |
| Project | `<cwd>/supervisor.config.json` | **Presence-based**: if the file exists it *is* the config, even if corrupt (whole-file replacement — collapses to defaults but still shadows the global tier) |
| Global | `$XDG_CONFIG_HOME` (else `~/.config`) + `pi-plan/supervisor.config.json` | **Auto-created** on first run when absent (scaffold below); a corrupt global file falls back to `DEFAULT_MODEL` |
| Built-in | compiled-in defaults | `maxTurns` 40, model `openrouter/deepseek/deepseek-v4-flash` |

All fields optional; a missing/corrupt file falls back to defaults and
never fails the run.

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

In-file precedence (unchanged): `steps.<n>` > config > default (40 turns,
`openrouter/deepseek/deepseek-v4-flash`). Unknown fields are ignored;
wrong types fall back to defaults.

### Global config and auto-create

The first `pi-plan supervise`/`step` with no `--config` and no project file
creates a global scaffold at `~/.config/pi-plan/supervisor.config.json`
(respecting `$XDG_CONFIG_HOME` when set):

```json
{ "model": "openrouter/deepseek/deepseek-v4-flash-0731" }
```

The scaffold pins the tuned, dated snapshot
`openrouter/deepseek/deepseek-v4-flash-0731` — distinct from the rolling
in-code fallback `openrouter/deepseek/deepseek-v4-flash`
(`DEFAULT_MODEL`), which only applies when no config file resolves. The
global config is **configuration** (XDG), deliberately separate from the
run-state tree under `~/.pi-plan/` (`$PI_PLAN_STATE_DIR`); it is never
overwritten once created (a user edit is kept), and the auto-create is
best-effort — a write failure falls back to built-in defaults and never
fails a run.

Supervise prints the resolved model and config source at startup, e.g.
`model: openrouter/deepseek/deepseek-v4-flash-0731 (global: /home/u/.config/pi-plan/supervisor.config.json)`,
so you can always tell which tier won. Troubleshooting: a corrupt global
file silently falls back to `DEFAULT_MODEL` (the rolling alias), so a bad
edit re-opens the drift problem invisibly — fix or delete the file; a
present-but-corrupt project file shadows the global tier by design
(presence wins), so a broken local edit can hide a good global default.

## Worker contract

A worker is a **fresh `pi --mode rpc` process per attempt** with a pinned
argv: `--mode rpc --session-dir ~/.pi-plan/<key>/sessions
--name pi-plan-row-<n> --model <model> --thinking high --approve
--tools read,grep,find,ls,bash,edit,write --skill <implement-from-plan>
--no-extensions -e <permission-system dir>
--append-system-prompt <persona>`. `--no-extensions` disables extension
discovery while the explicit `-e` still loads the permission system, so
`pi-guardrails` (and every settings-package extension) never loads in
workers; each access is decided solely by the permission system's relayed
dialogs (Permissions behavior). **Bare-worker invariant:** with
`--no-extensions` a worker loads exactly one extension — the permission
system — so pi-lens-hosted behaviors (unified LSP, lens, autoformat,
autofix, the write-time test runner, the opengrep scanner, the
knip/madge/jscpd family) never exist in a worker regardless of flags;
determinism comes from the extension set, never from pi-lens flags (the
six `--no-*` pi-lens flags do not exist in a bare worker's parser and
must stay out of the argv). The `<permission-system dir>` resolves
`$PI_PLAN_PERMISSION_EXTENSION` first, else the default install at
`~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`. The persona
preamble lives in `prompts/worker-persona.md`; the row prompt
(`src/prompt.rs`) names the project, plan source, TODO.md path, exact row
text, and planned commit message. The implement-from-plan skill body
(frontmatter stripped) is framed in a bounded section of that first prompt
("…has been loaded for you automatically"), loaded **once** at
`supervise`/`step` startup — the worker never reads the `SKILL.md` file
itself.

The worker is told to end its final message with
`PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` and, when asking,
`QUESTION: <text>`. The marker is a **hint** — completion is classified
git-keyed (planned commit message vs `git log` subjects) and questions from
the ASK marker, never from the marker alone. A row is done iff an `exact`
or `similar` (ratio ≥ 0.9) commit match lands; a `candidate`
(prefix or ratio ≥ 0.8) near-miss stops for `pi-plan mark N done`.

Per-row budget: **2 runs** (initial + one automatic retry). Question pauses,
stops, and restarts spend nothing; a spawn error spends nothing too — the
worker respawns 3 × 2 s, then stops with a distinct "worker spawn failed"
outcome — and every other terminal event spends one run. Stall ceiling: 40
turns per worker by default (counted from
`turn_end` events, `maxTurns` in config), plus a wall-clock timeout.

### Header/footer and worker statistics

In TUI mode the header and footer show statistics for **live workers**
from a live-worker registry, rotating through them when several run at
once: each worker gets an entry at spawn (visible from its first frame),
stats refresh per tail snapshot, the entry is removed when its tail exits
(stream close, subscribe failure, or a dialog `stop`/`restart`/`^D`), and
the display holds each live worker's status for
`ROTATION_HOLD_MS = 3000` ms before rotating to the next — header line 2
and the footer always show the same worker. The footer's `row N` is the
worker's row from the plan (`TodoRow.number`), so `--row N` mode and
non-contiguous numbering agree with the idle line. Only with **no** live
worker does the footer drop to a supervisor-status line with row context
(`idle · last: row 5 completed · next: row 6 — Unit tests`; during a retry
the label is the preceding attempt's terminal kind, `… row 5 failed …`),
and the next row's logical unit comes from the plan (`next: row N` only
when it is unknown, e.g. single-row `--row N` mode). The header keeps the
step banner (row/total/unit). Line mode never had a persistent stats line
and is unchanged.

Every worker's final statistics are **reported and logged**. The ending
`--pi-plan report` block lists a stats line per run attempt
(`worker: <id> · cost $X.XX · N tokens · ctx P% · T turns · duration`)
alongside the existing per-attempt outcome/tail/transcript, and each
attempt's compact stats (`· cost $X · N tokens · T turns`) is also
appended to that worker's terminal report line. A durable audit log is
written under the run-state root at
`~/.pi-plan/<key>/worker-stats.jsonl` — one JSONL record per run attempt
(a `v: 1` schema marker), written atomically (write-temp-then-rename).
Runs without a terminal snapshot (question pauses, aborts before stats)
write nothing, so the log has no all-null rows. Because each record is a
full-file rewrite, treat the file as an audit log, not a live tail: a
reader `tail -f`ing across the rename will miss the newest line.

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

Workers spawn **bare**: `--no-extensions -e <permission-system>` removes
`pi-guardrails` (and every other extension) from worker runs, so every
worker access is gated solely by the permission system's relayed dialogs.
In `--mode rpc` a permission `ask` surfaces as an `extension_ui_request`
frame on the worker's stdout; `pi-plan` renders it in your terminal and
forwards your reply (`extension_ui_response`) on stdin. This works for
`select`, `confirm`, `input`, and `editor` methods. Nothing is hard-denied
or auto-allowed on the supervisor's own judgment: every decision below is
either a rule you can reason about or a dialog presented to you verbatim.

**The supervisor is a permission proxy with memory.** When you approve a
select dialog's **"…for this session"** option, pi-plan records that grant
durably — permission-shaped `(surface-family, direction, pattern, width)`,
never a command string — at `~/.pi-plan/<key>/permissions.json` (deduped,
whole-file atomic rewrite; `$PI_PLAN_STATE_DIR` relocates the base). A
later ask in the same project whose suggested pattern is **contained by**
a stored grant is auto-approved with the same session option, so the
worker stops re-asking mid-run. Plain one-time `Yes` replies are **never**
recorded; verb-less surfaces record narrowly (a `bash` pattern needs ≥ 1
concrete command token before a trailing `*` — `git status *` never covers
`git push`; `skill` only exact names); `mcp` and any bare `*` pattern are
never recorded. The supervisor's own auto-approvals never create
precedents: the store contains only approvals you actually chose (the
`status` grant count stays operator-meaningful) and denials are never
recorded or auto-made.

Three **always-grants** run before stored grants; each requires the ask to
carry ≥ 1 concrete flagged path (a bash ask with no file access is never
auto-approved):

1. **Within the project root** (the `cwd` subtree) — any direction.
2. **Within the derived skills root** (`$HOME/.pi/agent/skills`, with
   `$PI_PLAN_SKILL` / `$PI_PLAN_CLEAN_SKILL` overrides) — **read**
   direction only.
3. **A bash command targeting `<skills root>/**/scripts/**`** — approved
   once with a plain `Yes` (never recorded).

Anything uncovered — a multi-path ask, a direction a rule did not adopt,
a write into the skills tree, an unparseable label — renders and prompts
exactly as before.

**Reset UX.** At `supervise`/`step` startup, when `permissions.json` holds
≥ 1 grant you get a keep/reset prompt (default **keep**; `reset` clears
the project grants; `stop` cancels the run start and exits 2; `restart`
is keep-and-proceed; EOF keeps). `pi-plan reset-permissions [--yes]`
clears them from a shell — it asks for confirmation unless `--yes`, then
prints how many grants were removed. A corrupt store skips the prompt and
surfaces one warning line in the final report.

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

The **focused** row is drawn as amber (`accent`) **bold text** on the
normal panel background (not a full-row amber fill), with a `▸` marker
retained so the highlight reads even when color is off. The highlight it
replaces — a full-width amber band behind default-colored text — was
low-contrast and hard to read.

Each permission dialog shows the **full ask**, not just the option list.
Both TUI and line mode render the whole multi-line `title` (the aligned
`tool`/`rule`/`command`/`full command`/`working directory` facts the
extension sends, split and word-wrapped to the box width) plus any
`message`, and — when a tool call is pending — a context block with the
tool name and call id and, for a `bash` call, the `$ <command>` it will
run (other tools show a compact JSON preview of their args). Long
`command : …` lines that would hide the target path are wrapped, never
ellipsized. Dialogs with no pending call (third-party ASK / input
prompts) are unchanged.

What differs from guardrails: the `@aliou/pi-guardrails` `pathAccess` gate
no longer loads in workers, so out-of-cwd access is **not silently
blocked any more** — it prompts (unless covered by a stored session grant
or an always-grant), and the permission system's own `mode: ask` still
gates everything else. If a worker is denied an op, the worker receives
the denial and adapts; the loop classifies the run from its outcome.

## Troubleshooting

| Symptom | Likely cause / fix |
| --- | --- |
| `pi-plan: cannot create the run directory …` | The external run root (`~/.pi-plan/<key>/sessions`, or `$PI_PLAN_STATE_DIR` when set) is not creatable — wrong `HOME`, unwritable base. Fix the base or set `PI_PLAN_STATE_DIR`. |
| Worker spawns but no trace appears | Check `~/.pi-plan/<key>/worker-stderr.log` for the spawned `pi` stderr (credential errors, bad model id). |
| "protocol error: …" / "oversized frame" | The worker's stdout was not clean JSONL (foreign `pi` version or a wrapper on `PATH`). Pin `pi` ≥ 0.85.1; check `~/.pi-plan/<key>/worker-stderr.log`. |
| Bad frames end the worker's stream | The stream is read strictly (LF framing, object frames only, 16 MiB cap). A corrupt peer kills that worker's read loop; the loop classifies and retries. |
| Credentials / model not found | Set the same auth used by your interactive pi (`~/.pi/agent/auth.json`, env). Symptoms land in `~/.pi-plan/<key>/worker-stderr.log`. |
| `stopped — worker spawn failed` (or `run N: spawn-error` lines) | The worker never started: every spawn failed after 3 attempts with 2 s backoff. The report tail carries the failing child's own stderr — `Error: Unknown options: --no-…` means the worker argv carries a pi-lens-only flag that cannot exist in a bare worker (see the Worker contract's bare-worker invariant); `peer closed the RPC stream` / credential errors point at `~/.pi-plan/<key>/worker-stderr.log`. A spawn error spends **no** run budget, so the next `supervise` resumes the row with its full budget. |
| `Runs were used` unexpectedly after a crash | State recovery: matching `planHash` + row still unmatched in git resumes with `runsUsed` intact. A changed TODO.md recomputes from git. |
| Where are transcripts? | `~/.pi-plan/<key>/sessions/` (per-worker session dir passed via `--session-dir`); `~/.pi-plan/<key>/worker-stderr.log` holds process stderr. |
| Where is run state? | `~/.pi-plan/<key>/` holds `supervisor-state.json`, `permissions.json`, `sessions/`, `worker-stderr.log`, and the `.pi-plan-stop` control (key = sanitized cwd basename + sha256-8 of the canonical cwd). `$PI_PLAN_STATE_DIR` relocates the base (portable/CI override); every command hard-errors when neither `$HOME` nor the override is available. |
| `supervise` refuses: "working tree not clean" | The dirty-WIP gate: a dirty worktree with no recorded in-progress owner. The supervisor first hands the tree to a `pi-plan-clean-<n>` agent (Worker contract) — the clean-worktree skill ignores expected build artifacts, discards only verified formatting churn, and asks when in doubt. A failing clean pass falls back to the refusal and writes no state. |
| `clean-worktree skill not installed` in the report | The gate fired, the clean-worktree skill is not installed, and `$PI_PLAN_CLEAN_SKILL` is unset: the run aborts as before, no worker spawns, no state is written. Install the skill at `~/.pi/agent/skills/clean-worktree/` or point `$PI_PLAN_CLEAN_SKILL` at a directory holding a `SKILL.md`, then rerun. |
| `pi-plan: cannot resolve the permission-system extension: no package at …` | Workers spawn bare, so the permission system must resolve (`$PI_PLAN_PERMISSION_EXTENSION`, else `~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`). Install the package or point the env var at a directory holding its manifest; the run fails fast before any worker spawns. |
| Where is `permissions.json`? | `~/.pi-plan/<key>/permissions.json` (or `$PI_PLAN_STATE_DIR/<key>/` when set) holds the session grants the proxy auto-approves from. `pi-plan status` shows the grant count; `pi-plan reset-permissions [--yes]` clears it; the start-of-run keep/reset prompt offers the same reset interactively. A corrupt file is read as empty with a stderr warning and repaired on the next save. |
| A `stop` at the start-of-run keep/reset prompt | `stop` cancels the run start and exits 2 (stopped semantics); `restart` there is keep-and-proceed. |
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
                        worker, rpc, supervise/ (+ tests), tui/ (+ tests),
                        ui, theme
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
