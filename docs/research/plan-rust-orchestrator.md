# Plan: `pi-plan` — a Rust Orchestrator for the TODO.md Workflow (Option C)

## Status

A research-to-build plan for the new repository `pi_plan_workflow`
(`/home/tr/Documents/pi_plan_workflow`). The goal is a **deterministic
orchestrator written entirely in Rust** that replaces the parent-agent
delegation role: it launches a fresh `pi --mode rpc` worker process per TODO
row, monitors progress deterministically, and renders worker traces and
permission dialogs **in its own plain terminal** — no LLM sits in the control
loop at all.

This replaces the TypeScript `/supervise` supervisor extension
(`pi_workflow/supervisor/`) once parity is demonstrated; the TS extension stays
usable during the transition (decision D7).

Decisions for this plan were locked with the user on 2026-09-12 (see the
Questions & Answers section). All claims about pi's RPC surface were verified
against the installed pi 0.85.1 package and `~/.pi/agent` configuration.

## The problem being solved (evidence)

- **Parent-agent delegation is unreliable.** In the Sep 11 `gramps_to_zola`
  session, a row-1 worker committed correctly (`df5655d`), a row-2 worker
  aborted mid-edit, supervision stopped, and the interactive session then did
  rows 2–8 itself — 130 bash calls, 31 edits, 9 parent-side commits. The log:
  *"Confirmed stalled — 13+ minutes without activity, no commit, no process.
  Taking over row 2. … I'll complete every remaining row inline."*
- Root cause layer 2: when the deterministic supervisor stops (budget
  exhausted, abort, or failed worker), the remaining work falls back to the
  interactive session's own LLM, which "helps" by doing the engineering
  inline.
- **Fix:** move the orchestrator outside the LLM session entirely. The
  orchestrator is a plain process — a stop is just a stop.

## Verified technical facts (pi 0.85.1)

| Fact | Where verified |
|---|---|
| `pi --mode rpc` = full bidirectional JSONL protocol over stdio; no TUI needed. | `docs/rpc.md` |
| In RPC mode `ctx.hasUI = true`; permission `ask` becomes `extension_ui_request` on stdout; client answers `extension_ui_response` on stdin. | `docs/rpc.md` §Extension UI Protocol, §confirm/select; `extensions.md` §Mode Behavior |
| In `-p` / `--mode json` `hasUI = false` → permission asks hard-deny (`DenyingAuthorizer`). RPC mode is the only headless mode where approvals work. | `extensions.md` §Mode Behavior; `@gotgenes/pi-permission-system` `authorizer.ts` |
| No `--agent` CLI flag; the `plan-implementer` persona is an in-process `@gotgenes/pi-subagents` concept. Persona must be delivered via CLI flags in RPC mode: `--model`, `--thinking`, `--tools`, `--skill`, `--append-system-prompt`. | `pi --help`; `~/.pi/agent/agents/plan-implementer.md` |
| No `--max-turns` CLI flag — the stall ceiling must be enforced orchestrator-side by counting `turn_end` events and sending `abort`. | `pi --help` (no such flag); `docs/rpc.md` `turn_start`/`turn_end`/`abort` |
| `--approve`/`-a` trusts project-local files in headless runs (needed for `.pi` settings in the target project). | `docs/security.md`, `docs/settings.md` |
| Session files persist under `--session-dir`; usable as durable transcripts and for `get_messages`/`get_last_assistant_text` result extraction. | `docs/rpc.md` `get_messages`, `get_last_assistant_text`; `docs/sessions.md` |
| `bash_execution_update`, `tool_execution_update`, `message_update` (text/thinking/toolcall deltas) stream **in real time** — the orchestrator can tail everything, not just dialogs. | `docs/rpc.md` §Events |
| Permission config is global (`settings.json` packages: `@gotgenes/pi-permission-system`, `@aliou/pi-guardrails`, `pi-lens`); loaded identically in RPC workers. | `~/.pi/agent/settings.json` |
| RPC permission asks resolve locally: `selectAuthorizer` gives a `hasUI` session a `LocalUserAuthorizer` when no live relay target exists, so a worker that sets no subagent env vars (`PI_SUBAGENT_*`, `PI_AGENT_ROUTER_*`) receives asks as `extension_ui_request` (method `select`/`input`, **no `timeout` field** → no hidden auto-approve). Forwarding to a parent session activates only when those env vars are set. | `@gotgenes/pi-permission-system` `authorizer.ts` / `authorizer-selection.ts`; `docs/rpc.md` §Extension UI Protocol |
| Toolchain: Rust 1.91.1 (cargo 1.91.1, rustc 1.91.1) installed at `~/.cargo`. | local |

## Questions & Answers (locked 2026-09-12)

| Decision | Chosen | Consequence |
|---|---|---|
| Worker persona | **Built from CLI flags in Rust** — orchestrator constructs `pi --mode rpc` args (`--model`, `--thinking`, `--tools`, `--skill`, `--append-system-prompt`); worker prompt template lives in the repo. | No global agent-file coupling; fully deterministic, unit-testable arg building. |
| Ask / question contract | **Marker + final-message parse** — worker ends its turn with `PI_WORKER_STATUS: ASK` and a `QUESTION: <text>` line; orchestrator parses the last assistant message, prints the question, and folds the human's answer into a fresh worker prompt. | No `ask_parent` dependency (it doesn't exist in vanilla RPC); mirrors the existing contract with a clean marker. |
| Operator interface | **Plain terminal split** — live-tail worker event stream, status line (row/turns/context%), inline y/n & choice dialogs for permission asks and worker questions. tmux-friendly. | Minimal UI code; no ratatui dependency in v1. |
| End-state vs TS `/supervise` | **Replace once Rust is proven** — plan targets full parity; TS extension stays during transition; retiring it is a follow-up decision after E2E parity. | Two implementations exist briefly; `docs/ARCHITECTURE.md` documents both and the migration path (decision D7). |

## Architecture

### System diagram

```
                    YOU
                     │  terminal: live traces + inline dialogs
                     ▼
        ┌──────────────────────────────┐
        │ pi-plan (single Rust binary) │  deterministic TS-free control loop
        │   cli.rs  supervise.rs       │
        │   worker.rs  rpc.rs  ui.rs   │
        └──────┬───────────────────────┘
               │ spawn: pi --mode rpc --session-dir <run>/... (one process per row)
               │        --model --thinking --tools --skill --append-system-prompt
               ▼
        pi worker (fresh process + context, one TODO row only)
               │  event stream: message_update, tool_execution_*, bash_execution_update,
               │  turn_end, agent_settled, extension_ui_request (permission asks)
               │  ├─ permission ask ──► pi-plan renders dialog, you answer ──► extension_ui_response
               │  └─ PI_WORKER_STATUS: ASK ──► pi-plan prints question, you answer ──► fresh worker
               ▼
        repository = source of truth:
          TODO.md · docs/research/*.md · git history · supervisor-state.json
```

### Separation of concerns

| Owner | Owns |
|---|---|
| Repository | durable state: `docs/research/*.md`, `TODO.md`, git history (source of truth, unchanged) |
| Orchestrator (`pi-plan`) | plan progression, worker lifecycle, retry budget, ask surfacing, crash recovery, permission-dialog answering, trace rendering |
| Worker | engineering for exactly one TODO row (a vanilla `pi --mode rpc` session) |
| Human | permission grants, ask answers, near-miss adjudication, mid-step restart |

There is **no parent agent**: the orchestrator is code, never an LLM.

## Contracts (ported with RPC adaptations)

### Contract 1 — TODO.md row contract (unchanged)

Direct port of `pi_workflow/supervisor/src/todo.ts`: parse the `## Steps`
table (`| # | Commit message | Logical unit | Key deliverables | Tests |`),
`Source:` line, `## Prerequisites` block, `## Done` marker. Row-number
extraction regex `(?:^|[^0-9])([0-9]+)` with `UNNUMBERED_ROW_NUMBER =
MAX_SAFE_INTEGER` fallback. Completion is git-keyed (Contract 2); workers
never edit TODO.md.

### Contract 2 — completion detection, git-keyed (unchanged)

Direct port of `git.ts`: `git` facade via `std::process::Command` (mirrors
`spawnSync`; never throws — reports status instead), message normalization,
Levenshtein + `similarityRatio`, match tiers `exact ≥ 0.9 ∋ similar ∋
candidate` and the near-miss adjudication rule. A row is done iff
`exact` or `similar` matches; `candidate` stops for `/pi-plan mark <n> done`.

### Contract 3 — worker prompt contract (RPC-adapted)

`render_worker_prompt` in the repo (ports `prompt.ts`, module `prompt.rs`):

```
You are a worker operating under a supervisor.

Project: <cwd>
Plan source: <path from TODO.md Source: line>
TODO.md: <cwd>/TODO.md

Implement ONLY row <N> of TODO.md:
  <full row text, backtick/pipe-escaped>
Do not start row <N+1>. A supervisor coordinates the rest; stop when row <N> is done.

Follow the implement-from-plan skill for this step (incremental loop).
Commit with exactly the plan's commit message for this row: <message>

If you are blocked and need a decision or information you do not have,
end your final message with:
  PI_WORKER_STATUS: ASK
  QUESTION: <crisp question>
You will not be resumed after a question — the human's answer is folded
into a fresh worker's prompt, and that worker continues the row.
<resumeDirtyWip block when resuming a dirty in-progress row>
End your final message with the line: PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>
```

The marker is a **hint**; classification remains git-keyed (Contract 2) and
ask-classified (Contract 4), never marker-only.

### Contract 3b — worker launch spec (new: persona via CLI flags)

The orchestrator builds the worker's argv (module `worker.rs`):

```text
pi --mode rpc \
   --session-dir <runDir>/sessions \
   --name pi-plan-row-<n> \
   --model <model from config or default openrouter/deepseek/deepseek-v4-flash> \
   --thinking high \
   --approve \
   --tools read,grep,find,ls,bash,edit,write \
   --skill ~/.pi/agent/skills/implement-from-plan \
   --no-extensions -e <permission-system dir> \
   --append-system-prompt <worker persona preamble from repo prompts/worker-persona.md>
```

then sends the Contract 3 prompt as the first RPC `prompt` command. The
persona preamble is the plan-implementer body text (versioned in this repo)
without the `ask_parent` contract (replaced by the ASK marker section).

**Bare-worker invariant:** with `--no-extensions` a worker loads exactly one
extension — the permission system — so pi-lens-hosted behaviors (unified
LSP, lens, autoformat at `agent_end`, autofix, the write-time test runner,
the opengrep auxiliary scanner, the knip/madge/jscpd family) never exist in a
worker regardless of flags. The determinism guarantee comes from the
extension set, never from flags, and this applies to the row workers and the
clean-worktree agents alike (shared builder). The pi-lens determinism flags
(`--no-lsp --no-lens --no-tests --no-autoformat --no-autofix
--no-opengrep`) MUST NOT appear in the argv: pi 0.85.1 registers them only
inside the pi-lens extension, so with extension discovery disabled they do
not exist in the parser (`Unknown options`). If a future pi release moves
any of these behaviors into core (or a future contract deliberately loads
pi-lens), the flags may return **alongside** a `-e <pi-lens>` and only when
extensions are loadable.

### Contract 4 — run/retry/ask state machine (ported, ask-adapted)

Per row budget **2 runs** (initial + one automatic retry). Per attempt:

```
spawn worker (fresh pi --mode rpc process)
  │  saveState({currentRow, runsUsed, lastOutcome:"running",
  │            agentId, startedAt})          ← spawn-time save
  ▼
await terminal: agent_settled, or stall ceiling (turn_end count > maxTurns)
  │             → send abort + kill process
  ▼
classify (checked in this order):
  ├─ human interrupts (stop/restart) win — restart spends nothing
  ├─ ASK?  last assistant text has PI_WORKER_STATUS: ASK → not spent;
  │        save "question"; aborted worker stays dead; print question,
  │        wait for answer (stdin or `--answer`) → fresh worker with answer folded
  ├─ git match exact/similar → row done; clearState
  ├─ git match candidate → near-miss; save; stop for `/pi-plan mark <n> done`
  ├─ spawn failure → NOT an agent run; respawn the worker (3 attempts
  │        total including the first, 2 s fixed backoff), and a persistent
  │        failure stops with the distinct "worker spawn failed" outcome:
  │        `runsUsed` is left UNTOUCHED, intermediate failures write NO
  │        state, and the report tail surfaces the failing child's own
  │        stderr (`worker-stderr.log`, last 6 lines / 400 chars) for
  │        Prompt-class deaths; resuming a `spawn-error` state restores
  │        the row's full budget
  ├─ dirty worktree at boundary, no recorded owner → refuse; write NO state
  └─ otherwise spent (failed / complete-without-commit / STUCK-no-question):
        save; retry fresh worker if runsUsed < 2, else STOP + full report
```

Spawn-error semantics: a spawn failure spends **no** run-budget (the 2-run
budget counts only real agent runs), so the report never reads "budget
exhausted" for an environment that merely refused to launch a worker. The
respawn is bounded (3 attempts, 2 s fixed backoff) so a deterministic
environment failure cannot hang the loop; each attempt renders one
`run N: spawn-error` line in the final report, and the exit code stays 2
(work outstanding). `supervisor-state.json` is unchanged in shape; only the
resume discount treats `lastOutcome = "spawn-error"` as unspent.

Stall ceiling: **orchestrator-side** — count `turn_end` events per worker;
abort transitively via the `abort` RPC command when `maxTurns` (default 40,
configurable per row) is exceeded. Context-ratio signal comes from periodic
`get_session_stats` (`contextUsage.percent` — `null` right after compaction,
treat as unknown); `compaction_start` raises a
"consider restart" banner. A wall-clock `turnTimeoutMs` (default 1800 s) is an
additional ceiling.

Config resolution is layered (config is configuration, state is
`~/.pi-plan/`): `--config PATH` (absolute) > `<cwd>/supervisor.config.json`
(presence-based; a present-but-corrupt project file still shadows) > global
`~/.config/pi-plan/supervisor.config.json` (XDG; auto-created on first run
when absent with the pinned scaffold model
`openrouter/deepseek/deepseek-v4-flash-0731` — distinct from the rolling
`DEFAULT_MODEL` fallback; best-effort, never overwrites a user edit, never
fails a run) > built-in defaults. `resolve_config` returns the config plus
the `ConfigSource` tier, printed as the startup `model:` line.

### Contract 5 — crash recovery (unchanged shape)

`supervisor-state.json` in the project root (bare filename, gitignored):
`{ planHash, currentRow, runsUsed, lastOutcome, adjudicated, agentId?,
startedAt? }`. `planHash` = SHA-256 of TODO.md content. On start: matching
hash + row still unmatched in git resumes with `runsUsed` intact; anything
else recomputes from git; corrupt/unparseable file → recompute, never throw.
Writes are best-effort (a failed write must not crash the loop) and atomic
(temp file + rename) so a kill mid-write cannot corrupt the recovery input.

## Module layout

```text
pi_plan_workflow/
  Cargo.toml            workspace/package: name pi-plan, pinned deps
  rust-toolchain.toml   channel = "1.91.1"
  README.md             usage + install
  AGENTS.md             repo conventions (cargo test / clippy -D warnings / fmt)
  .gitignore            target/, .pi/, supervisor-state.json (bare),
                        test-fixtures/spike/.git/, test-fixtures/spike/.pi/
  docs/ARCHITECTURE.md  system overview (updated in the same commit as changes)
  docs/acceptance-e2e.md
  docs/research/        this plan + follow-up research
  src/
    main.rs             clap CLI: supervise / status / stop / mark / step
    cli.rs              subcommand parsing + arg validation (+ tests)
    config.rs           supervisor.config.json schema + precedence (+ tests)
    todo.rs             Contract 1 parser (port of todo.ts) (+ tests)
    git.rs              git facade + Contract 2 matcher (port of git.ts) (+ tests)
    prompt.rs           Contract 3 prompt builder (+ tests)
    state.rs            Contract 5 state file (+ tests)
    worker.rs           WorkerPort trait + RPC worker impl + stall ceiling (+ tests)
    rpc.rs              pi RPC client: framing, commands, events, dialog sub-protocol (+ tests)
    supervise.rs        Contract 4 state machine (port of supervise.ts) (+ tests)
    ui.rs               plain-terminal renderer: live tail, status line, dialogs (+ tests)
  test-fixtures/
    rpc-peer/           fake pi test binary (scripted JSONL peer; tests/ integration)
    spike/              small inner git repo with a multi-row TODO.md for manual/E2E runs
  tests/                integration tests (fake-pi peer, end-to-end state machine)
  prompts/worker-persona.md   plan-implementer body text (RPC-adapted)
```

### Dependency plan (`Cargo.toml` pins)

- `tokio` (process, io, time, sync; `default-features = false`, selected
  features only)
- `serde` + `serde_json` (protocol + state)
- `clap` (derive) — CLI
- `thiserror` (library errors) + `anyhow` (binary)
- `sha2` (plan hash)
- dev: `proptest` for property tests of the matcher/parser.
- optional: `libc` or `nix` — only if process-group teardown calls `killpg`
  directly; the no-dep alternative is `setsid` on spawn + `kill(-pgid)` via
  the command shim (matches the existing git-facade pattern).

No TUI framework in v1 (plain terminal; ratatui deferred). Git is shelled to
via `std::process::Command` (mirrors the TS facade and keeps fixtures simple);
`git2`/`gix` are a deferred swap.

## Step ordering rationale

1. **Repo before code** — commit-by-commit like the TS plan; nothing commits
   before Step 1.
2. **Parser + matcher before the loop** — the loop's facts (row, completion)
   must be unit-tested and correct before orchestration builds on them.
3. **State + prompt before RPC** — cheap, pure modules; establish the schema
   and prompt contract first.
4. **RPC client before worker** — the client is the only genuinely new,
   protocol-level piece; it gets its own fake-pi test peer before the worker
   port layers on top.
5. **Minimal loop before polish** — run/retry/ask/recovery proven on unit
   tests with an injected fake worker port, then one manual spike with a real
   pi worker (the permission-dialog path is fatal if wrong, so it is proven
   here, not at the end).
6. **Dirty-gate after the loop** — the scenario-aware dirty gate is a
   refinement of the loop's boundary handling; its unit tests depend on the
   loop's state-save shape.
7. **UI after the loop** — status/status/traces/dialogs need the loop to exist
   first.
8. **E2E + docs last** — acceptance runs the real binary end to end; docs pin
   usage and the migration path.

## Implementation plan

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `chore: initialize pi_plan_workflow repository` | Repo scaffolding | `README.md`, `.gitignore`, `AGENTS.md`, `rust-toolchain.toml` (1.91.1), `Cargo.toml` (pinned deps), `src/main.rs` placeholder, `docs/research/` plan docs committed; also `docs/ARCHITECTURE.md` initial skeleton | `cargo fmt --check`, `cargo test` (empty), `cargo clippy -- -D warnings` |
| 2 | `feat: parse TODO.md steps and match rows against git history` | Parser + matcher | `src/todo.rs`, `src/git.rs` (ports of `todo.ts`, `git.ts`) | Unit + property (`proptest`): canonical table, header/separator, backticks, `stepNN` ids, CRLF, matcher tier exclusivity + near-miss thresholds |
| 3 | `feat: add worker prompt builder with ASK contract and durable state` | Prompt + state + config | `src/prompt.rs` (ASK/QUESTION markers, escape, answer fold, dirty-WIP block), `src/state.rs` (SupervisorState, `planHashOf`, `recoverState`, corrupt-file tolerance), `src/config.rs` (steps-`n` > config > default precedence) | Unit: prompt variants, answer/escape, state round-trip, recovery rule matrix, config precedence |
| 4 | `feat: add pi RPC client over JSONL with extension-UI dialog answering` | RPC client | `src/rpc.rs`: process spawn, strict JSONL framing (split on `\n` only; reject oversized/non-object frames), commands (`prompt`, `abort`, `get_session_stats`, `get_messages`, `get_last_assistant_text`), event decoding (message/tool/bash/turn/agent/compaction/extension_ui), `extension_ui_response` writer; `test-fixtures/rpc-peer/` fake-pi JSONL peer | Unit + integration (`tests/rpc_fake_pi.rs`): framing edge cases, out-of-order responses, dialog round-trip, timeout |
| 5 | `feat: add worker port over the pi RPC client` | Worker adapter | `src/worker.rs`: `WorkerPort` trait (spawn/snapshot/abort/awaitTerminal/on), real impl over RPC client: argv builder (Contract 3b), snapshot assembly (text from deltas, toolUses, compactionCount, context%), turn-based stall ceiling (count `turn_end` → `abort`), transcript path from `--session-dir` | Unit (fake RPC peer): argv shape, snapshot assembly, stall-ceiling abort, terminal classification, timeout |
| 6 | `feat: implement supervise loop with retries, question pause, and crash recovery` | Orchestration loop | `src/supervise.rs`: `BUDGET_PER_ROW = 2`, `run_row` state machine (spawn → await terminal → classify ask/git/candidate/spent), spawn-time state save, question pause (not spent) + answer fold, near-miss stop, full report seam | Unit with injected `FakeWorkerPort` + fake git (ported from `supervise.test.ts`), incl.: happy path, fail-once-then-retry, budget stop, ask–pause–answer, **answering after budget exhausted still works**, near-miss, corrupt state recompute, marker-vs-classifier precedence, dirty gate v1 (clean-tree enforcement) |
| 7 | `feat: gate spawns behind scenario-aware dirty-worktree check and persist in-progress state` | Dirty-WIP gate + in-progress state | `state.rs` gains `agentId`/`startedAt` (backward compatible); spawn-time "running" save; gate refuses only owner-less strays, **writes no state on refusal** (no budget inflation); `resumeDirty` flag → prompt note + spawn-report banner; **`docs/ARCHITECTURE.md` updated in this commit** | Unit: refuse-without-owner, resume-on-owned-dirty, no-spend refusal, legacy-marker tolerance, banner |
| 8 | `feat: surface worker traces and inline permission dialogs in a terminal UI` | Operator UI + CLI | `src/ui.rs` (live tail of worker events, status line: row/turns/context%, inline y/n & choice dialogs for `extension_ui_request` and ASK, `stop`/`restart` line commands), `src/main.rs` + `src/cli.rs` (clap: `supervise [--row N] [--answer "…"] [--config PATH]`, `status`, `stop`, `mark <n> done`, `step N [--answer …]`), `/home/pi-plan` report formatting (result tail, transcript path) | Unit (pure renderer functions: dialog option mapping, status-line formatting, marker extraction) + manual smoke |
| 9 | `test: add end-to-end acceptance procedure and fixture` | E2E gate | `docs/acceptance-e2e.md` scripted checklist on `test-fixtures/spike/` (3 rows): real `pi --mode rpc` worker per row, forwarded dialog answered inline, ASK question answered, commits land per row, loop advances; force one spent failure (kill worker mid-row) → retry+report; `/pi-plan stop` + restart; crash recovery (kill binary mid-row, restart, resume) | manual E2E |
| 10 | `docs: document orchestrator usage, config, and migration from /supervise` | User docs + migration | `README.md` (install, commands, config, worker contract, permissions behavior, troubleshooting), `docs/ARCHITECTURE.md` refresh, migration section: TS extension stays until E2E parity is demonstrated; retire decision is a follow-up | `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`, docs read cleanly |

### Step 1 — `chore: initialize pi_plan_workflow repository`

- `git init` (default branch `main`); `README.md` (purpose, layout, quick
  start); `.gitignore` — `target/`, `.pi/`, `supervisor-state.json` (bare
  name matches at any depth), `test-fixtures/spike/.git/`,
  `test-fixtures/spike/.pi/`.
- `AGENTS.md` — repo conventions: `cargo test`, `cargo clippy --all-targets
  --all-features -- -D warnings`, `cargo fmt --check`, commit style
  (conventional, one logical unit per commit), plan workflow
  (`docs/research/` → `TODO.md` → incremental loop).
- `rust-toolchain.toml` — `channel = "1.91.1"`.
- `Cargo.toml` — binary crate `pi-plan`; pinned deps per the Dependency plan;
  `[profile.dev]` defaults; no default-features bloat.
- `src/main.rs` — minimal placeholder (`fn main()`: version/usage stub). A
  package with no targets fails `cargo build`/`cargo test` ("no targets
  specified in the manifest"), so the skeleton must compile from Step 1; the
  real clap CLI lands in Step 8.
- Commit this plan + research docs (this file is untracked today).
- Verify: `cargo build`, `cargo test` (empty suite passes), `cargo fmt
  --check`, `cargo clippy -- -D warnings` all clean.

### Step 2 — `feat: parse TODO.md steps and match rows against git history`

- `todo.rs` — port `splitRow` (escaped pipes), `isSeparatorRow`,
  `isHeaderRow`, `parseRow`, `extractRowNumber`, `parsePlan`, `nextRow`,
  `allDone` with the exact TS semantics (including `## Steps` trailing-space
  tolerance and the header/separator skip).
- `git.rs` — port `normalizeMessage` (backticks, whitespace collapse,
  trailing punctuation, lowercase), `levenshtein`, `similarityRatio`,
  `matchTier` (thresholds 0.9/0.8), `matchPlanned`, `isRowDone`; git facade =
  `std::process::Command` wrappers (`rev-parse HEAD`, `log --format=%s
  --no-decorate`, `status --short`, `status --porcelain`), runner returns
  `(status, stdout, stderr)` and never panics.
- Tests: port `todo.test.ts`/`git.test.ts` cases; add `proptest` properties:
  tiers are an **exclusive partition** over (planned, subject) pairs — a pair
  matches exactly one tier, and `similar` is NOT a subset of `candidate`
  (candidate requires `startsWith(planned)` or ratio ∈ [0.8, 0.9));
  `similarityRatio` ∈ [0, 1] with 1.0 exactly on identical normalized strings;
  `isRowDone ⇔ tier ∈ {exact, similar}`.
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 3 — `feat: add worker prompt builder with ASK contract and durable state`

- `prompt.rs` — `render_worker_prompt` per Contract 3; `escape_row_text`,
  `format_row_block`, answer fold (`>` quoted + escaped), `resumeDirtyWip`
  block, `PI_WORKER_STATUS` trailer including the new `ASK` value.
- `state.rs` — `SupervisorState` with the v1 fields; `plan_hash_of` (sha2);
  `read_state_file`/`coerce_state` (any shape failure → None, never throw);
  `save_state`/`clear_state` best-effort + atomic (temp + rename);
  `recover_state` (hash match + row not done → resume).
- `config.rs` — `SupervisorConfig` (`max_turns`, `model`, `steps` map);
  `resolve_max_turns`/`resolve_model` precedence
  (`steps.<n>` > config > default 40 / default model).
- Tests: unit as tabled in the summary (prompt variants, state round-trip +
  corrupt tolerance + recovery matrix, config precedence).
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 4 — `feat: add pi RPC client over JSONL with extension-UI dialog answering`

- `rpc.rs` —
  - spawn `pi --mode rpc` via `tokio::process::Command` (stdin write, stdout
    read, stderr → log file);
  - **framing:** strict JSONL — split on `\n` only, tolerate `\r\n`, reject
    non-object/non-JSON frames and oversize frames as `ProtocolError`
    (orchestrator-side limit, e.g. > 16 MiB; `rpc.md` §Framing documents only
    split semantics: LF-only, `\r\n` tolerance, no Unicode separators);
  - commands: `prompt` (+ `streamingBehavior` rules), `abort`,
    `get_session_stats`, `get_messages`, `get_last_assistant_text` — each
    with request `id` correlation and per-command response handling;
  - events: `agent_start/end/settled`, `turn_start/end`,
    `message_update` (text/thinking/toolcall deltas — assemble live message),
    `tool_execution_start/update/end`, `bash_execution_update`,
    `compaction_start/end`, `auto_retry_start/end`, `queue_update`,
    `extension_ui_request` (select/confirm/input/editor);
  - dialog responder: `reply_extension_ui(id, value|confirmed)` on stdin —
    the **permission passthrough** (this is what infinity-harness lacked);
  - timeout/deadline handling; owned process teardown (kill process group on
    drop — `setsid` the worker pre-exec and `kill(-pgid)` via the command
    shim, or add `libc`/`nix` for a direct `killpg`; see Dependency plan);
- `test-fixtures/rpc-peer/` — a fake `pi` executable (small Rust script or
  shell wrapper) that prints scripted JSONL events from a fixture file and
  asserts expected stdin commands — the offline protocol peer (pattern from
  twaldin's `rpc_agent.py`).
- Tests: integration `tests/rpc_fake_pi.rs` — framing edge cases, dialog
  round-trip (ask → inline response → resume) incl. a `select` cancel
  (`cancelled: true`) and a method-`input` exchange, out-of-order response
  correlation, abort, deadline expiry, bad-frame handling.
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 5 — `feat: add worker port over the pi RPC client`

- `worker.rs` —
  - `WorkerPort` trait (ported seam): `spawn(prompt, opts) -> WorkerId`,
    `snapshot(id) -> Option<Snapshot>`, `abort(id)`, `await_terminal(id,
    timeout) -> TerminalEvent`, `subscribe(channel)`,
    `dispose()` — keeps `supervise.rs` fully fakeable;
  - `RpcWorker` impl: argv builder per Contract 3b; sends Contract 3
    `prompt`; assembles `Snapshot` from live events (text via deltas,
    toolUses from `tool_execution_start` count, compactionCount from
    `compaction_start`, context% via periodic `get_session_stats` —
    `contextUsage.percent` is `null` right after compaction, treat as
    unknown/`?`); terminal
    = `agent_settled` (or stall-ceiling abort / process exit);
  - stall ceiling: `max_turns` per worker counted from `turn_end`; on exceed
    → `abort` + close and classify `failed`; `turn_timeout_ms` wall clock;
  - transcript path from the spawned session file under `--session-dir`;
  - marker extraction helper `parse_worker_status` / `parse_question`.
- Tests: unit against fake RPC peer — argv shape & quoting, snapshot
  assembly from scripted events, stall-ceiling abort at N turns, terminal
  classification incl. process-death, timeout paths.
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 6 — `feat: implement supervise loop with retries, question pause, and crash recovery`

- `supervise.rs` — port of the TS state machine with the ASK adaptation:
  - `run_plan(todo, answer)` → loop rows via `next_row`; done when no
    unmatched row or `## Done`;
  - `run_row(row, answer)` per Contract 4: budget gate, boundary interrupt
    flags, clean-tree gate (v1: refuse on dirty; refined in Step 7),
    spawn + spawn-time state save, await terminal, classify in order
    (interrupts → ASK → git exact/similar → candidate → spent), spend budget,
    retry fresh worker (answer cleared after first spend), stop with full
    report otherwise;
  - ASK classification: `terminal.completed` + last assistant text contains
    `PI_WORKER_STATUS: ASK` → extract `QUESTION: …` line → save
    `lastOutcome:"question"` → print question → wait for answer (stdin prompt
    or `--answer`) → fresh worker with answer folded (user-driven, works even
    after budget exhaustion);
  - restart/stop flags honored mid-await; abort child on question-pause so no
    parked sessions accumulate;
  - `read_todo_file`, `read_plan_source`, `describe_outcome`,
    `parse_worker_status` ports.
- Tests: unit with injected `FakeWorkerPort` + fake git — the full matrix
  from the summary table (ported from `supervise.test.ts`), including an
  explicit `stop`/`restart`-mid-await row (interrupts win over every
  classification).
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 7 — `feat: gate spawns behind scenario-aware dirty-worktree check and persist in-progress state`

- `state.rs` — `SupervisorState` gains optional `agentId`/`startedAt`
  (backward compatible `coerce`), spawn-time `"running"` save, terminal saves
  thread the run fields.
- `supervise.rs` — scenario-aware gate at the top of the attempt loop: fresh
  `recover_state()` per check; `rowInProgress` = state names this row and
  `lastOutcome` not in (`dirty`,`spawn-error`) legacy markers; refuse only
  when dirty **and** not in-progress; refusal writes **no state**; `resumeDirty`
  flag → prompt note (Contract 3) + spawn-report banner
  (`· resuming dirty WIP`).
- **`docs/ARCHITECTURE.md` updated in this commit** (state schema + gate
  walkthrough, per AGENTS.md).
- Tests: port of the dirty-gate suite (refuse-without-owner, resume-on-owned,
  no-spend, legacy tolerance, banner).
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 8 — `feat: surface worker traces and inline permission dialogs in a terminal UI`

- `ui.rs` — plain-terminal renderer:
  - live tail: `message_update` text/thinking deltas, tool-call lines,
    `bash_execution_update` chunks (bounded ring buffer per worker); ANSI
    styling optional and minimal;
  - status line: `row n/m · agent <id> · turns t/max · ctx % · tokens · dur`;
  - dialogs: `extension_ui_request` select/confirm/input → inline `y/n` or
    numbered choice; ASK questions → `answer>` prompt; line commands
    `stop`, `restart`, `status` also accepted at any prompt;
  - renderer logic kept pure (string/struct transforms) so it's unit-testable;
    stdout/stderr discipline (dialogs to stdout, logs to stderr or file).
- `cli.rs`/`main.rs` — clap: `pi-plan supervise [--row N] [--answer "…"]
  [--config PATH]`, `pi-plan status`, `pi-plan stop` (writes a one-shot
  control file — consumed at the next boundary/startup, a stale file from a
  killed run is discarded on read and never inherited by a later `supervise`;
  also reads state), `pi-plan mark <n> done`,
  `pi-plan step <n> [--answer "…"]`;
  config file loading; report formatting (result tail, transcript path).
- Tests: unit for renderer pure functions (dialog option mapping, status
  line, marker/QUESTION extraction); manual smoke: run against spike row 1,
  approve a gated `bash`, watch live tail.
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

### Step 9 — `test: add end-to-end acceptance procedure and fixture`

- `test-fixtures/spike/` — inner git repo (3 rows; row 1's work includes a
  **proven-gated op** — e.g. `write` or `mkdir`, per the permission config's
  `write: ask` / un-allowlisted `bash` — so the dialog path cannot silently
  pass without being exercised).
- `docs/acceptance-e2e.md` — scripted checklist: start `pi-plan supervise`,
  watch live trace, approve forwarded permission dialog inline, answer an ASK
  question, observe per-row commits, force one spent failure — kill the worker
  process mid-row so the row's commit never lands → verify automatic retry +
  full report, `pi-plan stop` mid-row → restart → resume,
  kill the binary mid-run → restart → recover from `supervisor-state.json`.
- Acceptance = checklist passes end-to-end with a **real** pi worker, real
  permission forwarding over the extension-UI sub-protocol, real commits.
- Verify: run the procedure; record date + result in the doc.

### Step 10 — `docs: document orchestrator usage, config, and migration from /supervise`

- `README.md` — install (cargo build/release), commands table, config schema
  - precedence, worker contract, permissions behavior (what forwards, what
  guardrails denies and why), troubleshooting (bad frames, credential
  failures, state recovery, transcript location), deferred-features list.
- `docs/ARCHITECTURE.md` — module map, data flow, the permission-dialog
  sub-protocol, migration section: TS `/supervise` remains until E2E parity is
  demonstrated; retiring it is a tracked follow-up decision; the two
  implementations' shared conventions (TODO contract, git-keyed completion,
  state file) are documented so the transition is mechanical.
- Verify: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`,
  docs read cleanly top to bottom.

## Design decisions

| # | Decision | Chosen | Rationale |
|---|---|---|---|
| D1 | Worker recycle model | Step-granular; fresh `pi --mode rpc` process per attempt; manual mid-step restart via `restart` command | Same as TS supervisor (D1); OS-process isolation is a bonus |
| D2 | Progression source | Git history vs planned commit messages | Unchanged; crash-proof; worker never edits TODO.md |
| D3 | Ask contract | `PI_WORKER_STATUS: ASK` + `QUESTION:` marker parsed from final assistant text; answer folded into fresh worker | `ask_parent` is gotgenes-internal; marker works in vanilla RPC; no next-turn resume dependency |
| D4 | Trace visibility | Full live event stream in the orchestrator's terminal + transcript path | `pi --mode rpc` streams everything; the orchestrator renders, not just dialogs |
| D5 | Retry budget | 2 runs/row, then stop | Unchanged (bounded cost; human adjudication after) |
| D6 | Language | Rust 1.91.1, single binary, pinned deps, no unsafe; clippy `-D warnings`, fmt enforced | rust-dev conventions; crash-robust daemon; portability |
| D7 | Code home + migration | New repo `pi_plan_workflow`; TS `/supervise` stays until E2E parity, then retire (follow-up) | Versioned, testable, reversible |
| D8 | Worker persona | Repo-versioned argv + `prompts/worker-persona.md`; no global agent-file coupling | Fully deterministic; the plan-implementer behavior becomes reproducible input |
| D9 | Permission answering | Orchestrator answers `extension_ui_request` itself (Option C) — no `PI_SUBAGENT_PARENT_SESSION`, no live root session required | Standalone; deterministic; audit log of every ask |
| D10 | Stall ceiling | Orchestrator-side: `turn_end` count + wall clock + context% | pi CLI has no max-turns flag |
| D11 | Worktree gate | Scenario-aware (owner-less strays refuse; owned dirty resumes), no-spend refusal | Port of `plan-resume-dirty-worktree` fixes, baked into v1 |

## Deferred / out of scope

- **ratatui full-screen TUI** — plain terminal in v1; keyboard-driven
  split-pane dashboard is a future UI step.
- **Parallel workers** — strictly sequential rows in v1 (matches TS
  supervisor).
- **Auto mid-step respawn from context%** — compacted-banner + manual
  `restart` is the v1 bridge (same as TS supervisor).
- **`git2`/`gix`** — shelling to `git` in v1; native binding swap is a
  follow-up.
- **In-process SDK embedding** (`createAgentSession`) — Node-only; the RPC
  subprocess path is the Rust-native choice and is sufficient.
- **Web/remote dashboard** and **`man`-style long-form help**.
- **Retiring the TS `/supervise` extension** — tracked as the post-parity
  follow-up decision (D7).

## Risks and mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Permission dialog never renders inline while the loop awaits a worker | Low–med | Step 4 fake-pi peer tests the dialog round-trip; Step 9 E2E gates it with a real worker and a proven-gated op. Fallback = pause-and-instruct (same shape as the ASK pause). |
| RPC protocol drift between pi versions | Med | Pin pi version in README; protocol conformance tests in `tests/rpc_fake_pi.rs`; a version-check on startup prints a warning on mismatch. |
| Worker quality stalls a row (model ceiling) | Med | 2-run budget + manual restart; per-row config override; wall-clock timeout. |
| Near-miss commit matching rejects a legit commit | Med | Similarity tier + `mark <n> done` adjudication; report shows both strings (ported behavior). |
| `bash_execution_update` floods the terminal | Low | Bounded ring buffer per worker + tail only; full output stays in the session log. |
| Orphaned worker processes on binary kill | Med | Process-group kill on drop/teardown; `--session-dir` transcripts survive; state file enables resume. |
| Capturing stdin for dialogs conflicts with worker stdin | Design-eliminated | Orchestrator never forwards its stdin to workers — workers are RPC subprocesses with piped stdio; all dialogs are orchestrator-rendered. |
| Path-guardrails deny in-cwd paths (mis-config) | Low | Guardrails allow cwd subtree; out-of-cwd denial is intended; troubleshooting note in README. |

## Verification (every step)

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Step 9 additionally runs the E2E acceptance checklist against
`test-fixtures/spike/` with a real `pi` worker.

## Next steps after this document

1. Review this plan (`review-plan` skill / second opinion) and apply fixes.
2. `write-todo-from-plan` → `TODO.md` in `pi_plan_workflow` (commit-by-commit
   table matching the steps above).
3. Execute Steps 1–10 in order — one commit per step, each matching the
   commit message in the `TODO.md` table.
