# Plan: global supervisor config with XDG layering and auto-create

## Status

Feature plan for the `pi-plan` orchestrator, derived from the 2026-09-14
conversation about the `tag_tool` supervise run. The supervisor's model was
believed to be pinned (`deepseek/deepseek-v4-flash-0731`) but actually
floated with the OpenRouter alias `openrouter/deepseek/deepseek-v4-flash`
(the code's `DEFAULT_MODEL`), so dated-snapshot drift was visible "elsewhere"
while pi-plan itself always passes the undated alias to every worker
(`src/worker.rs::build_worker_args` unconditionally appends
`--model <resolved>`; `DEFAULT_MODEL` at `src/config.rs:14`).

That investigation established three facts the current build has no global
default: `resolve_config` (`src/cli.rs:165`) checks only `--config PATH` then
`<cwd>/supervisor.config.json`, then the compiled-in `DEFAULT_MODEL`; there is
no home/XDG-level config, no `PI_PLAN_MODEL`-style env var, and no user-level
file is ever consulted. This plan adds a **global** config file for
configuration (not run-state), layered under the project file and above the
built-in default, and — via the auto-create decision — makes the supervisor
scaffold it on first use so a machine-wide pinned default always exists.

Decisions locked with the user on 2026-09-14 (Q&A): (1) **surface** the
resolved model and its config source at supervise startup; (2) **auto-create**
the global config file when absent; (3) **file-only** layering — no new
environment-variable override; precedence is `--config` > project file >
global file > built-in default. Global config lives at
`~/.config/pi-plan/supervisor.config.json` (XDG), deliberately **separate**
from the run-state root `~/.pi-plan/` / `$PI_PLAN_STATE_DIR`
(`src/storage.rs`).

## The problem being solved (evidence)

- **No global default exists.** `DEFAULT_MODEL` is a compile-time constant
  (`src/config.rs:14`, `"openrouter/deepseek/deepseek-v4-flash"`). `resolve_config`
  reads at most one file (`--config` or `<cwd>/supervisor.config.json`) and
  else returns `SupervisorConfig::default()`; `resolve_model` then yields
  `DEFAULT_MODEL`. There is no way to set a default model once for every
  project short of a per-project `supervisor.config.json` or a per-invocation
  `--config` (`src/cli.rs:165-172`).
- **The model actually floats.** Every worker spawn passes the undated alias
  (`src/worker.rs:207-210`); the dated snapshot is attached by OpenRouter at
  request time. 28 tag_tool worker transcripts all record
  `"modelId":"deepseek/deepseek-v4-flash"`. `-0731`/`-0423` appear nowhere in
  this repo (`rg 0731|0423` → 0 hits). A global pin is the fix.
- **State and config are already separated by convention.** The external root
  is run-state (`~/.pi-plan/<project-key>/`: sessions, stats, permissions,
  supervisor-state, worker-stderr) and is redirected by `$PI_PLAN_STATE_DIR`
  (`src/storage.rs`). User configuration has no home; the XDG convention
  (`$XDG_CONFIG_HOME`, else `~/.config`) is the natural one and is already the
  pattern for `pi` itself (`~/.pi/agent/…`). A config file under `~/.config/pi-plan/`
  cannot be mistaken for, or moved with, state.
- **Fail-safe config posture.** `read_config_file` never throws — a missing or
  corrupt file falls back to `SupervisorConfig::default()` with wrong-typed/
  junk fields ignored (`src/config.rs` `coerce_config`). The new layering must
  preserve this at every tier.

## Verified technical facts

| Fact | Where verified |
|---|---|
| `--config` is declared on both `supervise` and `step`, with help "Path to a supervisor.config.json (default: ./supervisor.config.json)". | `src/cli.rs` (`Command::Supervise` / `Command::Step`). |
| `resolve_config(cwd, config_path)` is the single consumer point; its only caller is `cmd_supervise` (`let config = resolve_config(cwd, config_path);`), shared by `supervise` and `step`. | `src/main.rs` `cmd_supervise`. |
| `SuperviseServices.config` is `&SupervisorConfig`; `tail_task` clones a `SupervisorConfig`. Config value shape is unchanged by this plan. | `src/supervise/mod.rs`, `src/main.rs`. |
| HOME and other env vars are already read into `cmd_supervise` via `env_string("HOME")` / `env_string("PI_PLAN_*")`; the pattern for "env wins, else `<home>/…`" resolution exists (`resolve_skill_path`, `resolve_permission_extension`). | `src/main.rs`, `src/cli.rs`. |
| Startup banners/traces go to stderr in line mode and through the `report` banner channel in TUI mode; `state:` lines already surface at startup. | `src/main.rs` (`report` seam, `banner_tx`). |
| `read_config_file` cannot distinguish "missing" from "corrupt" (both → default) — presence-detection is needed at the project/global tiers. | `src/config.rs`. |
| Run-state root resolution honors `$PI_PLAN_STATE_DIR` then `$HOME/.pi-plan` (hard error if neither). | `src/storage.rs`. |

## Design

### 1. Global config path (XDG)

A new pure resolver in `src/cli.rs`, matching the shape of the other
home-based resolvers (which take the env/home values as arguments so they stay
unit-testable):

```rust
/// $XDG_CONFIG_HOME (non-empty) else $HOME/.config, joined with
/// pi-plan/supervisor.config.json. None when neither is known.
pub fn global_config_path(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf>
```

(`Option<&str>` matches `resolve_skill_path` / `resolve_permission_extension`,
which take the env/home values as `Option<&str>` from `env_string(...).as_deref()`
so the call site needs no `Path::new` conversion.)

Rules:

- `$XDG_CONFIG_HOME` wins when set and non-empty (used as-is); else
  `$HOME/.config`; else `None` (no global config possible — fall through to
  built-in).
- Append `pi-plan/supervisor.config.json`.
- Deliberately **independent** of `$PI_PLAN_STATE_DIR`: configuration follows
  the config convention, state follows the state convention. Documented.

### 2. Layering with a source tag

`resolve_config` becomes layered and reports *where* the winning config came
from so startup can surface it (decision 1). Precedence, highest first:

| Tier | Trigger | Result |
|---|---|---|
| **Explicit `--config PATH`** | flag passed | Read exactly that file; missing/corrupt → defaults. **Absolute precedence** — project and global are ignored (decision 3 / preserved behavior). |
| **Project** `<cwd>/supervisor.config.json` | file `exists()` | Whole-file replacement: this file *is* the config, even if corrupt → defaults (presence-based shadowing). |
| **Global** `~/.config/pi-plan/supervisor.config.json` | directory resolvable | Auto-create the file when absent (decision 2), then read it; a successful create yields the pinned scaffold model; create-failure → defaults; corrupt → defaults (both fall to `DEFAULT_MODEL` for model resolution). |
| **Built-in** | nothing above | `SupervisorConfig::default()`; model resolves to `DEFAULT_MODEL`. |

Signature sharpened so the source rides along:

```rust
pub enum ConfigSource {
    Explicit(PathBuf),
    Project(PathBuf),
    Global(PathBuf),
    Builtin,
}

pub struct ResolvedConfig {
    pub config: SupervisorConfig,
    pub source: ConfigSource,
}

pub fn resolve_config(
    cwd: &Path,
    config_path: Option<&Path>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> ResolvedConfig
```

- `--config` and project tiers need only read + presence checks (no config-dir
  inputs).
- Presence is checked with `path.exists()` *before* `read_config_file`, so the
  project tier shadows global even when it collapses to defaults on a parse
  failure — matching the user's "a configuration file in a project repo would
  override the global configuration" (decision 2, presence-based).
- The config value inside is unchanged, so `SuperviseServices.config` and the
  `tail_task` clone keep working untouched.

### 3. Auto-create the global scaffold (decision 2)

In the global branch only (never when `--config` is given, never when a
project file exists; config is only touched by `supervise`/`step` via
`resolve_config`, so `status`/`stop`/`mark` are unaffected):

```rust
/// Best-effort scaffold: mkdir -p the parent, then write the default
/// global config only when the file does not already exist. Any I/O
/// failure returns Err but MUST NOT fail the run (caller falls back to
/// built-in defaults).
pub fn ensure_global_config(path: &Path) -> std::io::Result<()>
```

The scaffold content is the pinned recommended model:

```json
{ "model": "openrouter/deepseek/deepseek-v4-flash-0731" }
```

- Produced by a pure builder in `src/config.rs` so it is unit-testable:

  ```rust
  /// Model written to a freshly-created global config scaffold — the tuned,
  /// dated snapshot, distinct from the rolling `DEFAULT_MODEL` fallback.
  pub const DEFAULT_GLOBAL_MODEL: &str = "openrouter/deepseek/deepseek-v4-flash-0731";

  /// Serialized default global config (scaffold file contents).
  pub fn default_global_config_json() -> String
  ```

- The scaffold is written **only when absent** (never overwrites a user edit);
  once created it is ordinary user-editable config. The `exists()`-then-write
  is a TOCTOU pair (non-atomic), so concurrent `supervise` runs can both
  create the file; that is benign because a reader catching the file
  mid-write sees a partial document and falls through `read_config_file` to
  defaults — exactly the sanctioned fail-safe — and determinism guarantees
  the bytes are identical whichever writer wins.
- `DEFAULT_MODEL` (undated alias) stays the in-code fallback for when no
  global file can be created (HOME/XDG unknown, or a write failure) — a clear,
  documented split: *code* follows the rolling alias, *scaffolded global
  config* pins the trusted snapshot.

### 4. Surface the resolved model and source at startup (decision 1)

A pure, tested label formatter in `src/cli.rs`:

```rust
pub fn config_source_label(resolved: &ResolvedConfig) -> String
```

e.g. `openrouter/deepseek/deepseek-v4-flash-0731 (global: ~/.config/pi-plan/supervisor.config.json)`,
`… (project: /repo/supervisor.config.json)`, `… (--config /path)`,
`openrouter/deepseek/deepseek-v4-flash (built-in default)`.

`cmd_supervise` prints one line to stderr right after resolving config. The
`state:`-class startup facts print via a plain `eprintln!` here — the
banner/report seam is constructed later in the function (`src/main.rs:549`),
so it cannot carry this line in either mode; a plain stderr line emitted
before the TUI enters renders correctly in line and TUI mode alike. The label
renders full resolved paths (the `~` in the examples is shorthand for the
resolved home). This is the exact nibble that would have diagnosed the
tag_tool confusion ("why is it pinned to 0731?" / "why is it on the alias?").

## Code map (integration points)

| File | Change |
|---|---|
| `src/config.rs` | `DEFAULT_GLOBAL_MODEL` constant; `default_global_config_json()` pure builder (+ tests); doc note that `DEFAULT_MODEL` is the rolling in-code fallback. |
| `src/cli.rs` | `global_config_path(...)`; `ConfigSource` + `ResolvedConfig`; `resolve_config` reworked to layered precedence returning `ResolvedConfig` with the new params; `ensure_global_config(...)`; `config_source_label(...)` (pure, tested). Update the `supervise` `--config` help string (drop the "(default: ./supervisor.config.json)" suffix — `step`'s help already reads "Path to a supervisor.config.json." and keeps that wording). |
| `src/main.rs` | `cmd_supervise`: read `XDG_CONFIG_HOME` (+ reuse `env_home`) — **the env reads must move above the `resolve_config` call at line 259** (they currently sit at lines 285–289, after it) — pass into `resolve_config`, print the model/source startup line. |
| `docs/research/plan-rust-orchestrator.md`, `docs/ARCHITECTURE.md`, `README.md` | Document the global config file, the four-tier precedence, auto-create, and the `--config` semantics (absolute). |

Unchanged: `SuperviseServices.config` (`&SupervisorConfig`), `tail_task`'s
`SupervisorConfig` clone, worker argv building, state file shape, exit codes.

## Step table (commit-by-commit)

Quality gates after every commit: `cargo test` → `cargo fmt --check` →
`cargo clippy --all-targets --all-features -- -D warnings` (repo AGENTS.md);
each commit leaves the suite green.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: layer a global supervisor config under the project file (XDG)` | layering + source tag | `config.rs`: `DEFAULT_GLOBAL_MODEL`, `default_global_config_json`; `cli.rs`: `ConfigSource`/`ResolvedConfig`, `global_config_path`, `resolve_config` reworked (`--config` > project > global(existing) > builtin), new signature + `main.rs` call site (env reads lifted above the call), `supervise` `--config` help string | Unit: precedence per tier (explicit beats project beats global beats builtin), presence-based project shadowing, corrupt global → builtin, missing HOME/XDG → builtin, `global_config_path` XDG-over-`~/.config`-over-None (including an empty-string XDG falling back to `~/.config`) |
| 2 | `feat: auto-create the global supervisor config scaffold when absent` | auto-create side effect | `cli.rs`: `ensure_global_config` (mkdir -p, write-only-if-absent, deterministic content), wired into `resolve_config`'s global branch (best-effort: I/O failure falls back to builtin, never fails a run) | Unit: creates file + parent when absent, leaves an existing file untouched, write failure → builtin fallback, scaffold content == `default_global_config_json()` |
| 3 | `feat: print the resolved model and config source at supervise startup` | transparency | `cli.rs`: `config_source_label` (pure); `main.rs`: emit one startup line in `cmd_supervise` using the `ResolvedConfig` | Unit: label rendering for all four sources; (manual/accepted) startup line shows in line + TUI mode |
| 4 | `docs: document global supervisor config layering and auto-create` | docs | README Configuration section (four-tier precedence, the global path, auto-create, `--config` is absolute), ARCHITECTURE.md / orchestrator-plan Contract 4 notes, `DEFAULT_MODEL` vs pinned scaffold split, stale "40 turns" references in README + `config.rs` docstrings (already drifted from the 60-turn bump) | `cargo fmt --check` (doc-only), existing tests stay green |

Steps 1–3 are independently mergable and green; step 1 is the precedence
foundation, step 2 the scaffold, step 3 the observability. Step 4 is doc-only.

## Locked decisions

- Scope is the **config file** only: no new env-var override
  (`PI_PLAN_CONFIG` declined); no code change to how workers consume the
  resolved model (arbitrary `--model <resolved>`).
- Precedence is file-only: `--config` (absolute) > `<cwd>/supervisor.config.json`
  (whole-file replacement, presence-based) > `~/.config/pi-plan/supervisor.config.json`
  > built-in `DEFAULT_MODEL`.
- Global path is XDG: `$XDG_CONFIG_HOME` (else `~/.config`) + `pi-plan/supervisor.config.json`,
  independent of `$PI_PLAN_STATE_DIR`. Config stays out of the run-state tree.
- The tool **auto-creates** the global config when absent (first `supervise`/
  `step` with no `--config` and no project file), containing the pinned model
  `openrouter/deepseek/deepseek-v4-flash-0731`; it never overwrites an existing
  file and never fails a run on a write error.
- Supervise prints the resolved model + config source at startup.
- The auto-created global pin also covers every future project that has no
  local config — a deliberate, user-visible default change (the point of the
  feature), matching the `tag_tool`-level pin the user already set.

## Invariants to preserve

- Fail-safe config: never throws; every tier (missing/corrupt) falls back to
  a saf default; wrong-typed/junk fields ignored via `coerce_config`.
- The in-file model precedence `steps.<n>.model` > `config.model` >
  `DEFAULT_MODEL` is untouched — layering just selects *which file* is the
  config.
- `supervisor-state.json` shape and exit codes are unchanged.
- `status`/`stop`/`mark` never resolve or create config (they do not call
  `resolve_config`).
- No `unsafe`, no `unwrap()`/`expect()`/`panic!()` in application logic;
  thiserror errors; unit tests next to the code (repo AGENTS.md).

## Risks / pitfalls

- **Side effect at first run.** Auto-creating `~/.config/pi-plan/…` is the first
  home-directory write this tool does. Mitigated by best-effort semantics (a
  write failure falls back to built-in, never errors) and by scoping creation
  to the no-`--config`/no-project path. Documented so it isn't a surprise.
- **Two model constants.** `DEFAULT_MODEL` (rolling alias, in-code fallback)
  vs `DEFAULT_GLOBAL_MODEL` (pinned snapshot, scaffold content) could drift or
  confuse. Mitigated by the explicit doc split and by the fact that the durable
  global file (not `DEFAULT_GLOBAL_MODEL`) is what users see once created.
- **Corrupt global file → alias.** A user who edits the global file into an
  invalid shape silently gets `DEFAULT_MODEL` (rolling) for model resolution —
  the same fail-safe as today, but worth the README note so a bad edit doesn't
  re-open the drift problem invisibly.
- **Presence vs corrupt at the project tier.** A present-but-corrupt project
  file shadows the global file (collapses to built-in). This is the intended
  "presence wins" semantics, but it means a bad local edit can hide a good
  global default — documented in the README so it reads as a feature, not a
  loss.
- **Test churn.** The `resolve_config` signature changes and gains params;
  only `cmd_supervise` calls it, so the blast radius is one call site plus new
  unit tests. `SuperviseServices` literals in tests are unaffected (the config
  value shape is unchanged).
- **Scaffold before validation.** `resolve_config` runs in `cmd_supervise`
  before the empty-`TODO.md` gate, so a run that immediately errors (no rows,
  bad row number) still creates the global scaffold. Best-effort and benign,
  but documented so the ordering is not a surprise.

## Acceptance

1. **Unit:** precedence per tier; explicit `--config` beats both files; project
   file (when present) shadows global even when the project file is corrupt;
   global used only when no project file; no HOME/XDG → built-in;
   `global_config_path` resolves XDG-over-`~/.config`-over-None;
   `ensure_global_config` creates parent+file only-when-absent and tolerates a
   write failure (built-in fallback); `default_global_config_json()` equals the
   scaffold bytes; `config_source_label` renders all four sources.
2. **Manual (fresh dir):** run `pi-plan supervise` in a directory with no
   `supervisor.config.json` → `~/.config/pi-plan/supervisor.config.json` is
   created containing `{ "model": "openrouter/deepseek/deepseek-v4-flash-0731" }`
   and startup prints a `model: …0731 (global: …)` line; a second run reads it
   unchanged.
3. **Manual (override):** add a `supervisor.config.json` with a different model
   in a project dir → startup prints `(project: …)` and the worker spawns with
   that model; with `--config /path` → `(--config …)` wins over both.
4. **tag_tool:** the existing project file still wins there (its 0731 pin
   matches the new global default); other projects without local config now
   inherit the 0731 global pin instead of the rolling alias.
5. **Regression:** `cargo test`, `cargo fmt --check`, `cargo clippy
   --all-targets --all-features -- -D warnings` all pass; existing config and
   spawn tests stay green.
