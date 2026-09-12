# Plan: Inject the `implement-from-plan` skill body into worker prompts & relocate run state out of the project directory

## Status

Follow-up plan to `plan-rust-orchestrator.md` for the `pi-plan` orchestrator.
Two user change requests (2026-09-12):

1. **Skill injection** — inject the installed `implement-from-plan` skill's
   `SKILL.md` body into the first prompt of every worker, framed as
   already-loaded. The skill registry (the `--skill` flag and pi's system
   prompt inventory) and every other part of the prompt stay as they are.
2. **External run state** — stop writing `supervisor-state.json`,
   `.pi-plan/sessions/`, `.pi-plan/worker-stderr.log`, and the
   `.pi-plan-stop` control file into the project directory. Store them under
   `~/.pi-plan/<project-key>/` so the project directory stays clean while the
   orchestrator retrieves its state from there.

All decisions were locked with the user on 2026-09-12 (see Questions &
Answers). Behavior of the worker itself must not change: same argv, same
`--tools` allowlist, same determinism flags, same ASK marker contract.

## The problem being solved (evidence)

- **Progressive disclosure leaves skill loading to chance.** pi lists skills
  in the system prompt as name/description/location only; the model must
  `read` the full `SKILL.md` itself, and pi's docs note "models don't always
  do this; use prompting … to force it". The persona carries that prompting
  today, but the load is still a model-side action that costs a tool round
  trip and can silently not happen. Injecting the body makes the worker's
  instruction set deterministic per run.
- **The project directory accumulates runtime artifacts.** Every supervise
  run leaves `supervisor-state.json`, `.pi-plan/sessions/*` (session JSONL
  transcripts per worker), `.pi-plan/worker-stderr.log`, and a transient
  `.pi-plan-stop` in the repo. None of it belongs in version control, and
  the repo `.gitignore` does not currently exclude a root-level `.pi-plan/`.

## Verified technical facts (pi 0.85.1, this repo)

| Fact | Where verified |
|---|---|
| `--skill <path>` registers the skill directory; the system prompt then lists `<available_skills>` with `<name>`, `<description>`, **and `<location>`** (the absolute path), plus instructions for resolving relative skill paths ("resolve it against the skill directory … and use that absolute path in tool commands"). | pi `docs/skills.md`; `dist/bundle/chunks/chunk-JVUZSMYM.js` `formatSkillsForSystemPrompt` |
| Because `<location>` is visible, the injected body's relative references (`scripts/discover.sh`, `../incremental-development/SKILL.md`, `../rust-dev/SKILL.md`, …) remain resolvable by the worker without extra surgery. | same source; `~/.pi/agent/skills/implement-from-plan/` layout |
| The installed `implement-from-plan/SKILL.md` is 5,759 bytes / 162 lines (~1.5–2K tokens per spawn). Per-spawn overhead is modest; it neither hits RPC frame limits nor interacts with the turn-based stall ceiling (`max_turns` counts `turn_end` events, not prompt tokens). | `wc -c ~/.pi/agent/skills/implement-from-plan/SKILL.md` |
| pi's session repo nests per-cwd directories **inside** the `--session-dir` root: `<root>/--<mangled-cwd>--/<timestamp>_<id>.jsonl`. Moving the `--session-dir` root therefore changes only the outer base; the transcript layout the report shows changes only in its prefix. | `dist/bundle/chunks/chunk-JVUZSMYM.js` `sessionDirectoryName` / `JsonlSessionRepo` |
| `cwd` for all commands comes from `std::env::current_dir()` (already absolute); the repo already uses `sha2::{Digest, Sha256}` (`plan_hash_of`). | `src/main.rs:71`, `src/state.rs` |

## Change 1 — inject the skill body into the worker prompt

### Current behavior

- `src/cli.rs::resolve_skill_path`: `$PI_PLAN_SKILL` wins, else
  `~/.pi/agent/skills/implement-from-plan`, else `None` (flag omitted).
- The orchestrator passes `--skill <path>` (registry); the worker is expected
  to `read` the SKILL.md itself.
- `src/prompt.rs::render_worker_prompt` emits the line
  `Follow the implement-from-plan skill for this step (incremental loop).`
- `prompts/worker-persona.md` (via `--append-system-prompt`) says
  "You must load the skill `implement-from-plan` before beginning work."

### New behavior

- At `supervise` startup the orchestrator reads `{skill_path}/SKILL.md`
  **once** (a per-run snapshot, same pattern as the persona file). Missing or
  unreadable skill (bad `$PI_PLAN_SKILL`, not installed) is a **hard error**:
  supervised runs require the skill — fail fast before any worker spawns.
- The frontmatter (the leading `---` block: `name`, `description`,
  `compatibility`, `allowed-tools`) is stripped; it is metadata, not
  instructions. A file without resolvable frontmatter — no leading `---`
  block, an unterminated block, leading blank lines/BOM before it — is
  treated like an unreadable skill: **fail fast** with the hard-error
  message below (never guess at where the instructions start).
- `render_worker_prompt` replaces the terse "Follow the …" line with a framed
  section **when a skill body is present**:

  ```text
  The implement-from-plan skill has been loaded for you automatically; its
  instructions are included below in full. Follow them for this step
  (incremental loop). Do not read the skill file again.

  <skill body, verbatim>

  --- (end of the automatically loaded implement-from-plan skill)
  ```

  Transition (commit 3 vs 4): `PromptInputs.skill_body` is optional. Commit 3
  emits the framed section only when a body is present; with `None` it keeps
  today's terse "Follow the implement-from-plan skill for this step
  (incremental loop)." line byte-identical, so no real prompt ever advertises
  an empty injected body. Commit 4 supplies the body from the startup
  snapshot and the framed section takes over.

- The closing marker re-arms the prompt boundary so the model cannot confuse
  the body with the orchestrator's own instructions that follow.
- The injected body makes the implement-from-plan instructions themselves
  deterministic; the skill's own Step 2 still asks the worker to `read` its
  supporting skills (incremental-development, git-workflow, …), and those
  loads remain model-side — unchanged scope, per the change request.
- Everything else in the prompt stays byte-identical: project/plan source/
  TODO.md lines, the row block, `Implement ONLY row <N>` + `Do not start row
  <N+1>` (these pins keep precedence over the skill's whole-plan procedure),
  the commit-message line, the ASK/QUESTION contract, the optional
  answered-question block, the `resumeDirtyWip` block, and the final
  `PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>` line.
- `prompts/worker-persona.md` is rewritten so the "must load" imperative
  becomes a "pre-loaded" statement (still delivered via
  `--append-system-prompt`): the skill has already been loaded
  automatically, its instructions are in the first message, and the worker
  should not read the skill file again. The ASK/QUESTION paragraph is
  unchanged.
- The `--skill` flag stays in the worker argv (this is the "registry … as
  is" requirement), so the inventory entry remains for the worker to resolve
  relative skill paths and supporting skill documents.

### Why this is safe

- The body's relative references keep working (verified: `<location>` is in
  the system-prompt inventory).
- The `allowed-tools` frontmatter is stripped, so no unused tool surface is
  implied; the worker's `--tools` allowlist is unchanged
  (`read,grep,find,ls,bash,edit,write`), and the ASK contract still replaces
  the ask tool the skill's frontmatter would otherwise mention.
- Deterministic per run: the snapshot is whatever is installed at startup, so
  a run's prompts never change mid-flight.

## Change 2 — external per-project run state under `~/.pi-plan/`

### Target layout

```text
~/.pi-plan/<project-key>/
  supervisor-state.json      ← Contract 5 crash recovery + adjudication
  .pi-plan-stop              ← one-shot stop control (moved too)
  sessions/                  ← per-worker session JSONL (--session-dir)
  worker-stderr.log          ← spawned pi stderr
```

The project directory is left completely clean: no `supervisor-state.json`,
no `.pi-plan/`, no `.pi-plan-stop` ever appear in the repo.

### Project key

- Canonicalize the cwd first (`fs::canonicalize`, fall back to the absolute
  `current_dir()` on failure) so symlinked views of one project share a key.
- `key = sanitize(basename(canonical_cwd)) + "-" + sha256(canonical_cwd)[:8]`,
  where `sanitize` maps every char outside `[A-Za-z0-9._-]` to `_`.
- Example: `~/.pi-plan/my-repo-a3f9c1d2/`. `basename == ""` (filesystem root)
  falls back to `root-<hash>`.
- Base directory: `$PI_PLAN_STATE_DIR` when set (test/portability override),
  else `$HOME/.pi-plan`. When HOME is unset and no override exists, fail with
  a clear error — **no** silent fallback into the project directory.

### Who resolves it (all consumers must agree)

One resolver, `ProjectStorage::resolve(home, cwd) -> Result<PathBuf, String>`
in a new `src/storage.rs`, used by every entry point in `src/main.rs`:

| Consumer | Today | After |
|---|---|---|
| `supervise` | creates `cwd/.pi-plan/sessions`, passes stderr log `cwd/.pi-plan/worker-stderr.log`, discards stale `cwd/.pi-plan-stop`, polls it, `state_file_path(cwd)` for save/clear | all under `~/.pi-plan/<key>/` |
| `status` | `read_state_file(cwd)` | `read_state_file(root)` |
| `stop` | `write_stop_request(cwd)` + `read_state_file(cwd)` | both under `root` |
| `mark <n> done` | `mark_done(cwd)`, `save_state_file(cwd)` (creates state when absent) | under `root`; creates `root` on demand |

### Plumbing

- `src/storage.rs` (new): `ProjectStorage::resolve`, key derivation helpers,
  root layout constants. Pure part (sanitize + hash + key) is unit-testable
  without the filesystem.
- `src/state.rs`: `state_file_path`, `read_state_file`, `save_state_file`,
  `clear_state_file`, `recover_state` change their `cwd: &Path` parameter to
  an explicit storage `root: &Path`. `save_state_file` additionally
  `create_dir_all(root)` before the tmp+rename write (atomicity unchanged).
- `src/cli.rs`: `stop_file_path`, `write_stop_request`, `clear_stop_request`,
  `stop_request_present`, `mark_done` change to take the storage root.
  `mark_done` creates the root before its first save.
- `src/main.rs`: `cmd_supervise` / `cmd_status` / `cmd_stop` / `cmd_mark`
  resolve `ProjectStorage::resolve(HOME, cwd)` once and pass `root` to all of
  the above; the supervise loop's recover/save/clear closures and the stop
  watcher use `root`; `--session-dir` and the stderr log path are
  `root/sessions` and `root/worker-stderr.log`.
- `src/supervise.rs`: **no loop changes** — the loop depends on
  `SuperviseServices` closures (`recover_state`, `save_state`, `clear_state`)
  and the `session_dir` already present; only `main.rs` wiring changes where
  those point. `spawn_opts_for_row` continues to use `services.session_dir`.
- `src/worker.rs`: no change (`--session-dir` and stderr path arrive through
  spawn opts).

### Recovery semantics (unchanged, relocated)

`recover_state` still validates against `plan_hash`, matches
`current_row` in git, and resumes with `runs_used` intact — the rules are
identical, the file just lives in `root`. A corrupt/missing file still
recomputes from git.

### Non-goals / notes

- **No auto-migration** of an in-project `supervisor-state.json` left by the
  old layout. The state is best-effort bookkeeping; dropping it only means
  recompute-from-git (safe, Contract 5). Noted as a possible follow-up.
- **Every command now requires HOME (or `$PI_PLAN_STATE_DIR`)** —
  `status`/`stop`/`mark` previously ran with HOME unset; after this change
  `ProjectStorage::resolve` hard-errors with the documented message. Accepted
  per the locked "no silent fallback" rule; recorded here so the regression
  is a conscious decision (a HOME-less cron/container must set
  `PI_PLAN_STATE_DIR`).
- **`$PI_PLAN_STATE_DIR` is a public knob** — precedence and fallback are
  documented in the README in step 5.
- **No cleanup policy** for `~/.pi-plan/<key>/`: sessions and logs persist
  after a plan completes (`clear_state_file` drops only
  `supervisor-state.json`); per-project dirs serve as forensics. A follow-up
  can add pruning.
- The root repo `.gitignore` keeps its entries (the bare-name
  `supervisor-state.json` and the spike ignores become vestigial but
  harmless); `test-fixtures/spike/.gitignore` stays as is.
- Concurrency behavior (two supervises of the same project) is unchanged —
  they shared `cwd/supervisor-state.json` before and now share the keyed
  root.

## Questions & Answers (locked 2026-09-12)

| # | Question | Locked answer |
|---|---|---|
| 1 | The persona says "You must load the skill implement-from-plan before beginning work." Once the body is auto-injected, that instruction is contradictory. Handle? | **Rewrite the persona to say the skill is pre-loaded** ("has already been loaded automatically; its instructions are in your first message; do not read the skill file again"), preventing a redundant read and a contradiction. |
| 2 | How to name `~/ .pi-plan/`'s per-project subdirectory? | **Basename + short hash** (e.g. `my-repo-a3f9c1d2/`), computed over the canonical(ized) cwd: readable, collision-safe, compact. |
| 3 | Move `.pi-plan-stop` under the external root too? | **Move it.** The project directory stays 100% clean; `stop` and `supervise` already agree on the key. |
| 4 | Skill body unreadable at supervise time? | **Fail fast with an error** (no fallback to prompt-only loading): supervised runs require the skill. |

## Implementation plan

Ordering: external state first (larger surface, self-contained), then skill
injection (prompt work), docs last. Each commit passes
`cargo test`, `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`.

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `feat: add external per-project storage root and root-parameterized state paths` | Storage resolver | `src/storage.rs` (new): `ProjectStorage::resolve` — canonical cwd, sanitized basename + sha256-8 key, `$PI_PLAN_STATE_DIR` > `$HOME/.pi-plan`, error when HOME unset; `state.rs`/`cli.rs` path helpers take an explicit storage root; `save_state_file`/`mark_done` create the root; `main.rs` still passes the project cwd as root (legacy behavior, suite stays green) | Unit: key derivation (sanitize, hash, root fallback, basename-empty), override/HOME precedence, state round-trip + recovery via root, tmp-file cleanup, stop-file fns via root, `mark_done` dir creation |
| 2 | `feat: relocate run state, sessions, logs, and stop control under ~/.pi-plan/<key>` | Wire external root | `main.rs`: `supervise`/`status`/`stop`/`mark` resolve `ProjectStorage::resolve`; `--session-dir` = `root/sessions`, stderr log = `root/worker-stderr.log`; stale-stop discard + stop watcher poll `root/.pi-plan-stop`; recover/save/clear closures point at `root`; report transcript paths now print the external prefix; user-visible state names follow — `cmd_stop`'s "no supervisor-state.json" line and `format_status_report`'s `state:` lines print the resolved root path (or "no state file (nothing running)"); `docs/ARCHITECTURE.md` layout/sourcing paragraphs updated | Unit: closure wiring (recover/save/clear), spawn opts carry the external session dir, status/stop strings carry the root label; docs read cleanly |
| 3 | `feat: render the implement-from-plan skill body inside the worker prompt` | Prompt builder (pure) | `src/prompt.rs`: `strip_skill_frontmatter` helper (leading `---` block removed; no frontmatter / unterminated block / BOM-leading file → `Err` so the caller can fail fast), `PromptInputs.skill_body: Option<&str>`, framed section emitted **only when the body is present** (missing body keeps today's terse "Follow the implement-from-plan skill…" line byte-identical), closing delimiter; `src/supervise.rs` `run_row` passes `skill_body: None` (one line, behavior-neutral); all other prompt content byte-identical | Unit: framing text, frontmatter stripped, malformed-frontmatter → error, `None` → terse-line fallback, body embedded verbatim, no-fence/no-escape corruption, row/ASK/dirty-WIP blocks unchanged |
| 4 | `feat: load the implement-from-plan skill body at supervise startup` | Wiring + persona | `main.rs`: resolve `{skill_path}/SKILL.md` once at `supervise`/`step` start — no skill configured (`None`), unreadable file, or unresolvable frontmatter all fail fast with the step-4 error before any worker spawns; `SuperviseServices.skill_body` threaded into `run_row` → `render_worker_prompt`; `prompts/worker-persona.md` rewritten to "skill pre-loaded" (ASK paragraph unchanged). Mechanical: adding the field touches all 28 `SuperviseServices` constructions in `supervise.rs` tests | Unit: missing-skill (`None`) and unreadable-skill both fail supervise before spawn, the startup snapshot serves every attempt in a run, body flows into every attempt's prompt; persona grep; full-suite green |
| 5 | `docs: document skill injection and external run state` | User docs + e2e | `README.md`: "Install" gains the implement-from-plan skill prerequisite (hard-required from step 4); "Worker contract": skill body injected; "Troubleshooting": `~/.pi-plan/<key>/worker-stderr.log` and `$PI_PLAN_STATE_DIR` documented; `docs/acceptance-e2e.md`: Prerequisites gain the skill install, all paths rerouted to external roots, new "project dir stays clean" checks, and a new check that the first worker's transcript contains a distinctive framed-section phrase (e.g. `has been loaded for you automatically`) — optionally that no skill-file `read` follows; stale-reference sweep scoped to `docs/ARCHITECTURE.md` / `README.md` / `docs/acceptance-e2e.md` (historical research docs, incl. `plan-rust-orchestrator.md`, are left as-is) | `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`, docs read cleanly |

### Step 2 notes (external root wiring details)

- `cmd_supervise` resolves `root = ProjectStorage::resolve(home, cwd)`,
  `create_dir_all(root/sessions)`, opens `root/worker-stderr.log` for the
  spawned `pi` stderr, clears a stale `root/.pi-plan-stop` at startup, and
  wires `recover_state = || read_state_file(root)` /
  `save_state = || save_state_file(root, _)` /
  `clear_state = || clear_state_file(root)` into `SuperviseServices`.
- `cmd_stop` writes `root/.pi-plan-stop` and prints the summary from
  `read_state_file(root)`; `cmd_status` and `cmd_mark` read/write `root`.
- The user-visible state-file names move with it: `cmd_stop`'s
  "no supervisor-state.json (nothing running)" line and
  `format_status_report`'s `state:` lines print the resolved root path
  (`~/.pi-plan/<key>/supervisor-state.json`, or "no state file (nothing
  running)" when absent) — a bare cwd-relative filename would mislead a
  user grepping the project root.
- The supervise loop body in `src/supervise.rs` does **not** change in this
  commit — verify with `git diff` that only `main.rs` + path helpers differ.

### Step 4 notes (fail-fast wording)

Suggested error: `cannot read the implement-from-plan skill: <path> —
set PI_PLAN_SKILL or install it to ~/.pi/agent/skills/implement-from-plan`.
Fires for all three failure shapes — no skill configured
(`resolve_skill_path` → `None`), unreadable file, and unresolvable
frontmatter (no `---` block / unterminated / BOM-leading) — before any
worker spawns, keeping the "fail fast" decision deterministic and the tool
surface untouched. `supervise` and `step` share this path (`step` routes
through `cmd_supervise`); `status`/`stop`/`mark` never load the skill.

## Review trail (2026-09-12 second review)

Findings R1–R6 from the review of this revision are baked in above:

- **R1 (step ordering)** — `PromptInputs.skill_body` is `Option`; commit 3
  keeps the terse line byte-identical when the body is absent, so no interim
  prompt ever advertises an empty injected skill. Commit 3's one-line
  `skill_body: None` change in `run_row` is behavior-neutral.
- **R2 (e2e gap)** — step 5's acceptance section now checks the first
  worker's transcript actually contains the framed-section phrase, proving
  the injection reached the worker (the core change-request guarantee).
- **R3 (edge cases)** — malformed frontmatter (none / unterminated / BOM)
  fails fast; the missing-skill (`None`) case and the same-snapshot-across-
  attempts property are explicit step-4 tests.
- **R4 (user-visible strings)** — `cmd_stop` and `format_status_report`
  print the resolved root path instead of a bare cwd-relative filename.
- **R5 (behavior change)** — every command now hard-errors without HOME or
  `$PI_PLAN_STATE_DIR`; recorded in Non-goals as an accepted regression.
- **R6 (docs)** — skill-install prerequisite and `$PI_PLAN_STATE_DIR` are
  documented; the stale-reference sweep is scoped to
  ARCHITECTURE/README/acceptance-e2e (historical research docs untouched).

Locked with the user 2026-09-12: apply all fixes; commit-3 transition keeps
 the terse line on `None`; status/stop strings print the resolved root path.
