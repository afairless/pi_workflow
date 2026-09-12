# Implementation Plan: Inject the implement-from-plan skill body into worker prompts & relocate run state out of the project directory

Source: `docs/research/plan-skill-injection-and-external-state.md`

Plan: two locked user change requests (2026-09-12) for the `pi-plan`
orchestrator. (1) **Skill injection** — read the installed
`implement-from-plan/SKILL.md` once at supervise startup and embed the body
(frontmatter stripped) in a framed section of every worker's first prompt,
replacing the terse "Follow the implement-from-plan skill…" line; the
`--skill` registry flag and every other part of the prompt stay
byte-identical, and a missing/unreadable skill or unresolvable frontmatter
fails fast before any worker spawns. (2) **External run state** — stop
writing `supervisor-state.json`, `.pi-plan/sessions/`,
`.pi-plan/worker-stderr.log`, and `.pi-plan-stop` into the project
directory; everything moves under `~/.pi-plan/<project-key>/`
(canonicalized cwd → sanitized basename + sha256-8 hash; `$PI_PLAN_STATE_DIR`
overrides HOME, hard error when HOME is unset), resolved once by a single
`ProjectStorage::resolve` in the new `src/storage.rs` and used by
`supervise`/`status`/`stop`/`mark`. No worker behavior change: same argv,
same `--tools` allowlist, same determinism flags, same ASK marker contract.

Ordering: external state first (larger surface, self-contained), then skill
injection (prompt work), docs last. No auto-migration of an in-project
state file (dropping it only means recompute-from-git — safe, Contract 5).
The plan was reviewed on 2026-09-12 and findings R1–R6 are baked in (see
the plan's "Review trail"). Workflow per step: implement → `cargo test` →
`cargo fmt --check` → `cargo clippy --all-targets --all-features -- -D
warnings` → commit with the message in the table → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
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
