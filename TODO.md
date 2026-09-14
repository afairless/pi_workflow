# Implementation Plan: Global supervisor config with XDG layering and auto-create

Source: `docs/research/plan-global-config.md`

The `pi-plan` supervisor has no global default model: `resolve_config`
(`src/cli.rs`) reads only `--config PATH`, then `<cwd>/supervisor.config.json`,
then falls back to the compiled-in rolling alias `DEFAULT_MODEL`
(`src/config.rs:14`, `"openrouter/deepseek/deepseek-v4-flash"`). Home/XDG-level
configuration does not exist, no `PI_PLAN_MODEL`-style env var exists, and no
user-level file is ever consulted. The 2026-09-14 `tag_tool` investigation
showed the model floats because the dated snapshot is attached by OpenRouter
at request time while the code always passes the undated alias; a machine-wide
pinned default is the fix.

This plan adds a **global** config file for configuration (not run-state),
layered under the project file and above the built-in default, and — via
auto-create — makes the supervisor scaffold it on first use. Four commits.
Precedence: `--config` (absolute) > project file > global file > built-in.

The commit messages in the table are **exact** — taken verbatim from the
source plan. Workflow per step: implement → `cargo test` → `cargo fmt --check`
→ `cargo clippy --all-targets --all-features -- -D warnings` → commit with the
table's message → stop.

**Baseline caveat:** the working tree currently carries uncommitted drift
(`src/config.rs` 60-turn bump, `src/supervise/mod.rs` `BUDGET_PER_ROW` 2→4)
that step 4's "stale 40 turns" docs work already depends on. Land that baseline
as its own commit before starting this plan.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: layer a global supervisor config under the project file (XDG)` | layering + source tag | `src/config.rs`: `DEFAULT_GLOBAL_MODEL`, `default_global_config_json`; `src/cli.rs`: `ConfigSource`/`ResolvedConfig`, `global_config_path`, `resolve_config` reworked (`--config` > project > global(existing) > builtin) with new signature; `src/main.rs` call site (env reads lifted above the `resolve_config` call), `supervise` `--config` help string | Unit: precedence per tier (explicit beats project beats global beats builtin), presence-based project shadowing, corrupt global → builtin, missing HOME/XDG → builtin, `global_config_path` XDG-over-`~/.config`-over-None (incl. empty-string XDG → `~/.config`) |
| 2 | `feat: auto-create the global supervisor config scaffold when absent` | auto-create side effect | `src/cli.rs`: `ensure_global_config` (mkdir -p, write-only-if-absent, deterministic content), wired into `resolve_config`'s global branch (best-effort: I/O failure falls back to builtin, never fails a run) | Unit: creates file + parent when absent, leaves an existing file untouched, write failure → builtin fallback, scaffold content == `default_global_config_json()` |
| 3 | `feat: print the resolved model and config source at supervise startup` | transparency | `src/cli.rs`: `config_source_label` (pure); `src/main.rs`: emit one startup line in `cmd_supervise` using the `ResolvedConfig` | Unit: label rendering for all four sources; (manual/accepted) startup line shows in line + TUI mode |
| 4 | `docs: document global supervisor config layering and auto-create` | docs | README Configuration section (four-tier precedence, the global path, auto-create, `--config` is absolute), ARCHITECTURE.md / orchestrator-plan Contract 4 notes, `DEFAULT_MODEL` vs pinned scaffold split, stale "40 turns" references in README + `src/config.rs` docstrings | `cargo fmt --check` (doc-only), existing tests stay green |

## Locked decisions (from the source plan)

- Scope is the **config file** only: no new env-var override (`PI_PLAN_CONFIG`
  declined); no change to how workers consume the resolved model (arbitrary
  `--model <resolved>` via `build_worker_args`).
- Precedence is file-only: `--config` (absolute) > `<cwd>/supervisor.config.json`
  (whole-file replacement, presence-based) > `~/.config/pi-plan/supervisor.config.json`
  > built-in `DEFAULT_MODEL`.
- Global path is XDG: `$XDG_CONFIG_HOME` (else `~/.config`) +
  `pi-plan/supervisor.config.json`, **independent** of `$PI_PLAN_STATE_DIR`.
  Config stays out of the run-state tree (`~/.pi-plan/`, `src/storage.rs`).
- The tool **auto-creates** the global config when absent (first
  `supervise`/`step` with no `--config` and no project file), containing the
  pinned model `openrouter/deepseek/deepseek-v4-flash-0731`; it never
  overwrites an existing file and never fails a run on a write error.
- Supervise prints the resolved model + config source at startup.
- Config is only touched by `supervise`/`step` via `resolve_config`, so
  `status`/`stop`/`mark` are unaffected.

## Invariants to preserve

- Fail-safe config: never throws; every tier (missing/corrupt) falls back to a
  safe default; wrong-typed/junk fields ignored via `coerce_config`.
- The in-file model precedence `steps.<n>.model` > `config.model` >
  `DEFAULT_MODEL` is untouched — layering just selects *which file* is config.
- `supervisor-state.json` shape and exit codes unchanged; `SuperviseServices.config`
  (`&SupervisorConfig`) and the `tail_task` clone keep working untouched.
- `status`/`stop`/`mark` never resolve or create config.
- No `unsafe`, no `unwrap()`/`expect()`/`panic!()` in application logic;
  thiserror errors; unit tests next to the code (repo AGENTS.md).

## Step notes

### Step 1 (layering + source tag)

- `src/config.rs`: `DEFAULT_GLOBAL_MODEL = "openrouter/deepseek/deepseek-v4-flash-0731"`
  (the pinned, dated snapshot — distinct from the rolling `DEFAULT_MODEL`
  fallback) and pure builder `default_global_config_json()` for the scaffold
  bytes; doc note that `DEFAULT_MODEL` stays the in-code fallback.
- `src/cli.rs`: `global_config_path(xdg_config_home: Option<&str>, home: Option<&str>
  ) -> Option<PathBuf>` — `$XDG_CONFIG_HOME` wins when set and non-empty, else
  `$HOME/.config`, else `None`; join `pi-plan/supervisor.config.json`.
- `ConfigSource` enum (`Explicit`/`Project`/`Global`/`Builtin` carrying the
  resolved `PathBuf`) + `ResolvedConfig { config, source }`; `resolve_config`
  reworked: `--config` absolute → `Project` (presence via `path.exists()`
  before `read_config_file`, so a present-but-corrupt project file shadows
  global) → `Global` (existing-file branch in step 1; auto-create lands in
  step 2) → `Builtin` (defaults). New signature adds `xdg_config_home` and
  `home` params.
- `src/main.rs`: lift the env reads (`XDG_CONFIG_HOME`, reuse `env_home`) above
  the `resolve_config` call at line 259 (they currently sit at 285–289) and
  pass them in; return value is now `ResolvedConfig`. Drop the
  "(default: ./supervisor.config.json)" suffix from the `supervise` `--config`
  help (matching `step`'s wording).
- Only `cmd_supervise` calls `resolve_config` — blast radius is one call site
  plus new unit tests; `SuperviseServices` literals are unaffected (config
  value shape unchanged).

### Step 2 (auto-create)

- `src/cli.rs`: `ensure_global_config(path: &Path) -> std::io::Result<()>` —
  `mkdir -p` the parent, write the default global config **only when the file
  does not already exist** (never overwrites a user edit). Non-atomic
  exists-then-write is benign under concurrency (identical deterministic
  bytes; a reader catching a partial document falls through to defaults).
- Wired into `resolve_config`'s global branch only (never for `--config`, never
  when a project file exists). Best-effort: an I/O failure falls back to
  built-in defaults, never fails a run.
- Runs in `cmd_supervise` before the empty-`TODO.md` gate — a run that
  immediately errors may still create the scaffold (best-effort, documented).

### Step 3 (transparency)

- `src/cli.rs`: pure `config_source_label(&ResolvedConfig) -> String`,
  e.g. `openrouter/deepseek/deepseek-v4-flash-0731 (global: ~/.config/pi-plan/supervisor.config.json)`,
  `… (project: /repo/supervisor.config.json)`, `… (--config /path)`,
  `openrouter/deepseek/deepseek-v4-flash (built-in default)` — renders full
  resolved paths.
- `src/main.rs`: `cmd_supervise` prints one line to stderr right after
  resolving config, **before** the banner/report seam is constructed
  (`src/main.rs:549`) — a plain `eprintln!` renders correctly in line and TUI
  mode alike (the seam cannot carry it).

### Step 4 (docs)

- README Configuration section: four-tier precedence, the global path,
  auto-create, `--config` is absolute; troubleshooting note that a corrupt
  global file silently falls back to `DEFAULT_MODEL`.
- ARCHITECTURE.md / `docs/research/plan-rust-orchestrator.md` Contract 4 notes.
- Document the `DEFAULT_MODEL` (rolling) vs scaffold pin split.
- Fix stale "40 turns" references in README + `config.rs` docstrings (already
  drifted from the 60-turn bump — the baseline `DEFAULT_MAX_TURNS = 60` must be
  committed first).

## Acceptance criteria (end state)

- `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings` all green after each commit, final commit
  included.
- **Unit:** precedence per tier; explicit `--config` beats both files; project
  file shadows global even when the project file is corrupt; global used only
  when no project file; no HOME/XDG → built-in; `global_config_path` resolves
  XDG-over-`~/.config`-over-None; `ensure_global_config` creates parent+file
  only-when-absent and tolerates a write failure; `default_global_config_json()`
  equals the scaffold bytes; `config_source_label` renders all four sources.
- **Manual (fresh dir):** `pi-plan supervise` in a dir with no
  `supervisor.config.json` → `~/.config/pi-plan/supervisor.config.json` created
  containing `{ "model": "openrouter/deepseek/deepseek-v4-flash-0731" }` and
  startup prints a `model: …0731 (global: …)` line; a second run reads it
  unchanged.
- **Manual (override):** a project `supervisor.config.json` with a different
  model → startup prints `(project: …)` and the worker spawns with that model;
  `--config /path` wins over both.
- **tag_tool:** the existing project file still wins there (its 0731 pin
  matches the new global default); other projects without local config now
  inherit the 0731 global pin instead of the rolling alias.
- **Regression:** existing config and spawn tests stay green; the in-file
  precedence and `supervisor-state.json` shape are unchanged.
