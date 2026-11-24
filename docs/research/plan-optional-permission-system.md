# Plan: Let `supervise` run when the permission system is not installed

## Status

Research-and-build plan for `pi_plan_workflow` (the Rust `pi-plan`
orchestrator). Triggered by a real failure on 2026-11-24 in
`~/Documents/candle_model`:

```text
pi-plan: cannot resolve the permission-system extension: no package at
/home/pk/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system — set
PI_PLAN_PERMISSION_EXTENSION or install pi-permission-system under
~/.pi/agent/npm/node_modules/@gotgenes
```

`supervise` exited 1 before any worker spawned. Nothing about that machine
is broken: `@gotgenes/*` is simply not installed there (its
`~/.pi/agent/npm/node_modules` holds `pi-lens`, `pi-hermes-memory`,
`@juicesharp/*`, `@victor-software-house/*`; `~/.pi/agent/settings.json`
lists only those packages, and `~/.pi/agent/extensions/` holds two unrelated
`.ts` files). The permission system became **hard-required** by
`docs/research/plan-supervisor-permission-memory.md` step 3, because workers
are spawned bare (`--no-extensions`) with it as the **only** loaded
extension. A hard requirement on an external optional package makes the tool
unusable on every machine that does not install that package.

This plan makes the permission system **optional**: when it cannot be
resolved the run continues with a loud warning, workers spawn with no
extension at all, and an explicit strict mode (env var or config key)
restores today's fail-fast for whoever wants it.

No code was changed during this investigation. Reviewed 2026-11-24: the
findings and their resolutions are folded in below (derived decisions
D10–D13), so this document is the plan of record.

## Goal (user requirement)

> "We should fix this so that supervise can run even if one (or more)
> permission systems that we usually use are not installed for Pi."

## Locked decisions (Q&A, 2026-11-24)

| # | Question | Decision |
| --- | --- | --- |
| 1 | Default behavior when the package cannot be resolved | **Degrade with a loud warning.** The run proceeds, workers are ungated, no flag is needed to start. |
| 2 | Strict override restoring fail-fast | **Yes — env var and config key**: `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=1` and `"requirePermissionExtension": true`. Degrade stays the default; strict is opt-in. |
| 3 | Worker argv when the package is missing | **`--no-extensions` alone** (no `-e`). Workers stay fully bare — never fall back to the ambient extension set (guardrails/lens/hermes) and never `-e builtin:mcp`. |
| 4 | Surfacing the degraded posture | **One startup stderr warning line only.** No TUI footer badge, no final-report line, no change to the start-of-run keep/reset prompt. |

Derived decisions (flagged for review; not asked as questions):

| # | Decision | Rationale |
| --- | --- | --- |
| D5 | Resolution returns a `PermissionExtension { dir: Option<PathBuf>, detail: String }` and **never fails**; the strict-mode refusal is a separate pure policy function. | The warning line and the strict error need the same "why" text; one resolver keeps the two consistent and both unit-testable. |
| D6 | An **explicitly set but nonexistent** `$PI_PLAN_PERMISSION_EXTENSION` is treated as *missing* (degraded, warning names the env var), not a hard error. | Consistent with decision 1: no run outcome may hinge on a package that is not on disk. Strict mode still refuses. |
| D7 | Resolution checks `Path::exists()` on **every** candidate, including the env override. | Measured (see evidence): `pi` prints a load error for a nonexistent `-e` path and **keeps running ungated`, exit 0. Today a typo'd env var silently disables gating while `pi-plan` believes it is gated. |
| D8 | A small **candidate list** is searched before declaring the package missing (today exactly one path is tried). | Install variants differ (scoped npm dir, plain extensions dir); a false "missing" is as bad as a false "gated". The resolved candidate is always printed. |
| D9 | Startup prints a `permissions:` line on **every** run (gated or degraded), directly below the existing `model:` line. | The posture must be unambiguous on every run, not only on degraded ones — the same transparency precedent as the step-3 `model:` line. |
| D10 | Strict-mode resolution returns `StrictMode { required, ignored_env }`, not a bare `bool`. | A mistyped `$PI_PLAN_REQUIRE_PERMISSION_EXTENSION` must be **named**, never swapped for the config value in silence: the posture line alone cannot express strictness, so a typo would select the insecure posture invisibly (review finding A1). |
| D11 | When the env override is set but absent **and** discovery resolves, `detail` names both. | Otherwise the operator's typo stays invisible on an otherwise-gated run (review finding A2). |
| D12 | The degraded warning names the ungated tools (`bash`/`edit`/`write`) and the docs recommend strict mode for CI/unattended runs. | Fail-open is the accepted trade; its two mitigations (warning, strict) must be legible exactly where they matter (review finding E1). |
| D13 | E2E absence is simulated with a temp `$HOME` **plus** an explicit `$PI_PLAN_SKILL`. | K.1 must be reproducible on a machine that *has* the package without uninstalling it, and a bare temp `HOME` would fail the hard-required skill prerequisite first (review finding D1). |

D10–D13 were confirmed at the 2026-11-24 plan review.

## Current state (evidence, HEAD `261e484`)

### The hard requirement, end to end

- `src/cli.rs:470` — `resolve_permission_extension(env, home) -> Result<PathBuf, String>`:
  a non-empty `$PI_PLAN_PERMISSION_EXTENSION` wins **without an existence
  check**; otherwise `<home>/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`;
  `path.exists() == false` → `Err` with the message the operator saw.
- `src/main.rs:398` (inside `cmd_supervise`, shared by `supervise`,
  `supervise --row N`, and `step`) — `let permission_ext =
  resolve_permission_extension(..)?;`. The `?` aborts in `run()`
  (`src/main.rs:71-77`) → `pi-plan: <msg>` on stderr, exit 1.
- The resolved path is carried as `SuperviseServices.permission_extension:
  &'a Path` (`src/supervise/mod.rs:649`) and copied into three spawn-opt
  builders: `spawn_opts_for_row` (`:734`), `verifier_spawn_opts` (`:762`),
  and the clean-pass opts (`:1353`), as `WorkerSpawnOpts.permission_extension:
  PathBuf` (`src/worker.rs:62`).
- `build_worker_args` (`src/worker.rs:202-228`) always emits
  `--no-extensions` + `-e <dir>`; the pair is unconditional, so making the
  field optional is compiler-enforced at exactly these sites.
- Tests pinning today's behavior:
  `permission_extension_resolution_env_wins_and_default_requires_an_install`
  (`src/cli.rs:1951`, asserts the hard error), and the two argv tests
  `build_worker_args_matches_contract_3b_shape` (`src/worker.rs:871`) and
  `build_worker_args_emits_bare_extension_flags` (`:930`). `src/supervise/tests.rs`
  has 38 `SuperviseServices { … permission_extension: Path::new("/ext/permission-system") … }`
  literals.

### Nothing else needs the package at startup

The permission system only matters at worker runtime, where an `ask`
surfaces as an `extension_ui_request` on the RPC stream (README "Permissions
behavior"). With no extension loaded no ask is ever emitted, so the dialog
proxy in `worker_tail`, the grants store (`src/permissions.rs`), the
always-grants, and the keep/reset prompt stay correct — they simply never
fire. The only startup dependency is the resolution itself.

### Measured `pi` behavior that motivates D7

```text
$ pi --mode rpc -ne -e /tmp/definitely-not-here   # stdin: shutdown command
Error: Failed to load extension "/tmp/definitely-not-here": Extension path does not exist: /tmp/definitely-not-here
Hint: Start without extensions using "pi -ne".
$ echo $?
0
```

(pi 1.6.0, verified during this investigation.) The peer starts, runs with no
extension, and exits 0 — the supervisor has no way to learn that gating is
gone. That failure mode is silent today and is what an existence check plus
the startup line remove.

A directory that exists but is **not** a loadable extension behaves the same
way (measured during the plan review, pi 1.6.0):

```text
$ mkdir /tmp/empty-ext && echo -n '' | pi --mode rpc -ne -e /tmp/empty-ext
Error: Failed to load extension "/tmp/empty-ext": Failed to load extension: Cannot find module '/tmp/empty-ext'
Require stack: …
Hint: Start without extensions using "pi -ne".
$ echo $?
0
```

So a false candidate (D8) costs a diagnosable load error and an ungated
worker, never a dead run — the Risks section relies on exactly this. Candidate
4 (`~/.pi/agent/extensions/pi-permission-system`) therefore only works in the
layouts pi accepts for a subdirectory of the extensions dir: `index.ts`/
`index.js`, or `package.json` with a `pi.extensions` field. A directory with
an `index.ts` loads cleanly through `-e` (also measured); a directory with
neither does not.

### Doc claims this plan invalidates

`README.md:36-43` (Prerequisites — "hard-required"),
`README.md:73-77` (Commands table `supervise` row — "answer permission dialogs
inline"),
`README.md:98-110` (the Configuration JSONC schema block + the precedence
prose, which must list `requirePermissionExtension`),
`README.md:160-172` (Worker contract argv + the single-path resolution
sentence),
`README.md:398` ("Workers spawn **bare** … every worker access is gated
solely by the permission system"),
`README.md:499` (Troubleshooting row for the hard error),
`docs/ARCHITECTURE.md:100-118` (Contract 3b prose, the single-path resolution
sentence, and "a worker loads exactly one extension"),
`docs/ARCHITECTURE.md:602-607` (the extension-UI permission sub-protocol intro
— "one permission system — and nothing else — gates every worker access"),
and `docs/acceptance-e2e.md:3-4` (intro — "real permission forwarding
answered inline"), `:20-21` (Scope bullet — the fixed bare-worker argv),
`:51-57` (Prerequisites), and `:310-313` (Pass criteria — "real
forwarding").

Wherever a doc enumerates the resolution path (README, ARCHITECTURE,
acceptance Prerequisites), the updated text names the **candidate list**
(D8), not the single default install.

## Proposed design

### 1. Resolution becomes infallible and explains itself (D5, D6, D7, D8, D11)

```rust
/// Where the permission-system package lives, or why it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionExtension {
    /// `Some(dir)` → workers get `-e <dir>`; `None` → ungated bare workers.
    pub dir: Option<PathBuf>,
    /// Always non-empty: the resolved package dir, or the reason it is missing.
    pub detail: String,
}

pub fn resolve_permission_extension(
    env_pi_plan_permission_extension: Option<&str>,
    home: Option<&str>,
) -> PermissionExtension
```

Candidate order (first that **exists** wins):

1. `$PI_PLAN_PERMISSION_EXTENSION` (non-empty, trimmed; not required to be
   under `$HOME`) — D6/D7: a nonexistent value is *missing*, never accepted
   blindly.
2. `<home>/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`
   (today's only default).
3. `<home>/.pi/agent/npm/node_modules/pi-permission-system` (unscoped
   install).
4. `<home>/.pi/agent/extensions/pi-permission-system` (a subdirectory of pi's
   global extensions directory; `-e <dir>` loads it only in a layout pi
   accepts — `index.ts`/`index.js`, or `package.json` with a `pi.extensions`
   field; both measured, see "Measured `pi` behavior").

`detail` strings (exactly these shapes; they reach the operator):

| Case | `detail` |
| --- | --- |
| env override resolves | `the pi-permission-system package at <path> (from $PI_PLAN_PERMISSION_EXTENSION)` |
| env override set, path absent, discovery also fails | `$PI_PLAN_PERMISSION_EXTENSION points at <path>, which does not exist` |
| discovery resolves, env override unset/blank | `the pi-permission-system package at <path>` |
| discovery resolves, env override set but absent (D11) | `the pi-permission-system package at <path>; $PI_PLAN_PERMISSION_EXTENSION points at <bad>, which does not exist and was ignored` |
| nothing found | `no pi-permission-system package found (searched <p2>, <p3>, <p4>)` |
| `$HOME` unknown (env unset, blank, or already reported absent) | `no pi-permission-system package found and $HOME is unknown` |

Existence is the only test; a directory that exists but is not a permission
system still wins (pi reports the load failure on the worker's stderr and the
resolved path is printed, so the misconfiguration stays diagnosable — see
Risks and the measured empty-dir case above).

**Path handling.** Paths are used **verbatim**: never absolutized, never
canonicalized. A relative env value is therefore resolved against the
supervisor's cwd, which is also the worker's spawn cwd, so existence and `-e`
agree; `detail` prints the value exactly as supplied. A literal
`builtin:<name>` value is deliberately **not** accepted — `exists()` rejects
it, and decision 3 forbids loading any builtin extension in a worker anyway. No
builtin permission system exists, so this costs nothing today; the deliberate
rejection is recorded here so a future reader does not "fix" it.

### 2. The strict-mode refusal is a separate pure policy gate (decision 2)

```rust
pub fn enforce_permission_requirement(
    resolution: &PermissionExtension,
    required: bool,
) -> Result<Option<PathBuf>, String>
```

- `required == false` → `Ok(resolution.dir.clone())` — degrade, never fails.
- `required == true` and `dir.is_some()` → `Ok(dir)`.
- `required == true` and `dir.is_none()` → `Err` whose message starts with
  today's **prefix** (so scripts grepping the old message still match; note the
  tail after the prefix changes, because the `detail` table above replaces
  today's `no package at <path> — …` sentence) plus the new escape hatch:

  ```text
  cannot resolve the permission-system extension: <detail> — set
  PI_PLAN_PERMISSION_EXTENSION, install pi-permission-system under
  ~/.pi/agent/npm/node_modules/@gotgenes, or turn strict mode off
  (PI_PLAN_REQUIRE_PERMISSION_EXTENSION=0, "requirePermissionExtension": false)
  to run ungated
  ```

### 3. Strict-mode flag: env > config > default `false` (D10)

New pure resolver in `src/cli.rs`:

```rust
/// The resolved strict-mode posture plus any env value that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictMode {
    /// Refuse to run when the permission system cannot be resolved.
    pub required: bool,
    /// `Some(raw)` when `$PI_PLAN_REQUIRE_PERMISSION_EXTENSION` held a value
    /// outside the truthy/falsy tables — reported, never silently swallowed.
    pub ignored_env: Option<String>,
}

pub fn resolve_require_permission_extension(
    env: Option<&str>,
    config: &SupervisorConfig,
) -> StrictMode
```

| `$PI_PLAN_REQUIRE_PERMISSION_EXTENSION` | Result |
| --- | --- |
| unset / empty / whitespace | `required = config.require_permission_extension` (default `false`), `ignored_env = None` |
| `1`, `true`, `yes`, `on` (trimmed, case-insensitive) | `required = true`, `ignored_env = None` |
| `0`, `false`, `no`, `off` | `required = false` — overrides a `true` config (env wins, matching the `$PI_PLAN_*` convention), `ignored_env = None` |
| anything else | `required = config` value **and** `ignored_env = Some(trimmed raw)` — the run still proceeds, but §4 prints an extra line naming the value (D10) |

Config schema (`src/config.rs`): add to `SupervisorConfig`
(`src/config.rs:80`, camelCase derive, all fields optional):

```rust
/// Refuse to run when the permission-system package is missing.
#[serde(default)]
pub require_permission_extension: bool,
```

and coerce it in `coerce_config` (`src/config.rs:109`) from
`object.get("requirePermissionExtension").and_then(Value::as_bool).unwrap_or(false)`
(a wrong type falls back to `false`, like every other field). The global
scaffold (`default_global_config_json`, `src/config.rs:32`) is **not**
changed — the key stays opt-in and appears only when a user writes it.

### 4. Startup line (D9, D10, decision 4)

Emitted in `cmd_supervise` immediately after the existing
`eprintln!("model: …")` (`src/main.rs:272`) — i.e. **before** the TUI enters
the alternate buffer, so line mode and TUI mode both see it in the terminal's
own scrollback (the same seam and reasoning as the `model:` line). That means
the resolution moves *up* from `src/main.rs:398` to just below the `model:`
line (moving the `$PI_PLAN_PERMISSION_EXTENSION` env read at
`:302` with it; the config the strict gate needs is already resolved at
`:267`), so it runs before `read_todo_file` and the `sessions/` creation and
strict mode also fails faster than today.

| Posture | Output |
| --- | --- |
| gated | `permissions: gated (<detail>)` |
| gated, strict mode on | `permissions: gated (<detail>; strict)` |
| degraded | `permissions: DISABLED — <detail>` followed by an indented second line: `  workers spawn with no extensions: every tool call is allowed without a dialog — bash/edit/write run ungated; install pi-permission-system or set PI_PLAN_PERMISSION_EXTENSION to restore gating` |
| any posture, unrecognized strict env value (D10) | one extra line: `permissions: ignoring unrecognized $PI_PLAN_REQUIRE_PERMISSION_EXTENSION=<value> (strict mode follows the config: <on\|off>)` |

All strings are produced by one pure helper
(`permission_status_lines(&PermissionExtension, &StrictMode) -> Vec<String>`) in
`src/cli.rs`, so the wording is unit-tested rather than assembled inline. A
gated non-strict run prints exactly one line; a gated strict run also prints one
(strictness is inline). A degraded run prints two; a degraded **strict** run
prints none — the gate refuses first (§2). A run with an unrecognized strict
env value prints one extra line on top of whichever posture applies, so a typo
is visible on gated and degraded runs alike (D10).

### 5. Worker argv: `--no-extensions` always, `-e` only when resolved (decision 3)

```rust
if let Some(dir) = &opts.permission_extension {
    args.push("-e".to_string());
    args.push(dir.to_string_lossy().into_owned());
}
```

The in-code Contract 3b doc comment (`src/worker.rs:180-200`) is rewritten in
the same commit: the **bare-worker invariant** becomes *"a worker loads **at
most** one extension — the permission system when it resolves, none when it
does not; determinism comes from the extension set, never from flags"*. The
`--no-extensions` flag is never conditional: an ungated worker must not
inherit the ambient extension set (lens/hermes/rpiv/curated-themes, which
would otherwise inject their own `extension_ui_request` frames into the RPC
stream the proxy reads).

### 6. Type ripples (compiler-enforced)

| Site | Change |
| --- | --- |
| `src/worker.rs:62` | `pub permission_extension: Option<PathBuf>` (+ doc comment) |
| `src/supervise/mod.rs:649` | `pub permission_extension: Option<&'a Path>` |
| `src/supervise/mod.rs:734`, `:762`, `:1353` | `.to_path_buf()` → `.map(\|p\| p.to_path_buf())` |
| `src/supervise/tests.rs` (38 literals) | `permission_extension: Path::new(..)` → `Some(Path::new(..))` |
| `src/main.rs:398-399`, `:544`, `:302` | resolution (and its env read) → `PermissionExtension` + `StrictMode`; services field → `permission_ext.dir.as_deref()` |
| `src/worker.rs:843`, `:934` | test `opts()` default `Some(...)`; the spaces test stays as-is |

### 7. Deliberately untouched

- **Dialog proxy / always-grants / grants store / keep-reset prompt**
  (decision 4): with no extension no dialog arrives; the prompt still appears
  when grants are on file. Docs gain one sentence: while ungated, stored
  grants and the always-grants are *inert*, and resetting them changes
  nothing until the package is installed. The prompt appearing right after a
  `permissions: DISABLED` line is an accepted confusion (see Risks), not a
  reason to change the prompt.
- `pi-plan status`, the final report, and exit codes: unchanged. A degraded
  run still exits 0/2 purely by work state, never by posture.
- No auto-install, no fallback to `pi-guardrails` or any ambient extension
  (decision 3), no `pi list` probing, no changes to
  `pi-permission-system` or any extension configuration.

### 8. What an ungated run means (for the docs)

| Behavior | Gated | Degraded |
| --- | --- | --- |
| Extensions loaded in a worker | exactly the permission system | none |
| Tool-call gating | each access decided by relayed dialogs | **no gating** — every tool call is allowed (pi's own model: no approval before every call) |
| `extension_ui_request` dialogs | relayed to the operator | never emitted |
| `permissions.json` grants / always-grants | auto-approve covered asks | inert (nothing to approve) |
| Worker tools allowlist, persona, skills, budget, git-keyed completion | unchanged | unchanged |

## Code map (integration points)

| Site | Change |
| --- | --- |
| `src/cli.rs::resolve_permission_extension` (`:470`) | Infallible; returns `PermissionExtension` with the 4-candidate search and the `detail` table above |
| `src/cli.rs` (new) | `enforce_permission_requirement`, `resolve_require_permission_extension` (→ `StrictMode`), `permission_status_lines(&PermissionExtension, &StrictMode)` (all pure) |
| `src/config.rs` (`:80`, `:109`) | `require_permission_extension: bool` field + coercion; scaffold untouched |
| `src/main.rs::cmd_supervise` (`:265-310`, `:398`, `:544`) | Resolve + strict gate right after the `model:` line (moving the `:302` env read up); print `permissions:`; pass `Option<&Path>` into services |
| `src/worker.rs` (`:57-62`, `:180-228`) | `Option<PathBuf>`; conditional `-e`; rewritten Contract 3b doc comment |
| `src/supervise/mod.rs` (`:643-650`, `:734`, `:762`, `:1353`) | `Option<&'a Path>` + three `.map(..)` conversions |
| `src/supervise/tests.rs` | 38 literal sites → `Some(..)`; no behavioral test changes |
| Docs | `README.md` Prerequisites (soft prerequisite + strict opt-in + CI/unattended guidance) / **Configuration JSONC block + precedence prose (`requirePermissionExtension`)** / Commands `supervise` row / Worker contract (argv + candidate list) / Permissions behavior (inert grants) / Troubleshooting (degraded-mode row, strict guidance); `docs/ARCHITECTURE.md` Contract 3b + "at most one extension" + **the extension-UI permission sub-protocol intro (`:602-607`)**; `docs/acceptance-e2e.md` **intro, Scope bullet, Pass criteria** + Prerequisites + new section K |

## Step table (commit-by-commit)

Workflow per step: implement → `cargo test` → `cargo fmt --check` →
`cargo clippy --all-targets --all-features -- -D warnings` → commit → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `refactor: resolve the permission-system package from candidate installs` | Resolution only (`src/cli.rs`) | Candidate list (2–4, each `exists()`-checked incl. the env override); error text names what was searched; signature still `Result<PathBuf, String>` so nothing else moves | Unit: env wins (against an existing dir); env set but absent → `Err` (stricter than today, becomes degraded in step 2); each discovery candidate resolves; nothing found lists all searched paths; no `$HOME` case; whitespace-only env falls through. **Rewrite `permission_extension_resolution_env_wins_and_default_requires_an_install` (`src/cli.rs:1951`)**: its `/ext/mine`-without-existence assertion dies here — split it into env-wins/fixture-dir cases |
| 2 | `feat: spawn ungated bare workers when the permission system is missing` | The degrade path | `PermissionExtension` + infallible resolver (incl. the D11 combined detail); `Option` ripples (`WorkerSpawnOpts`, `SuperviseServices`, 3 conversions, 38 test literals); conditional `-e` in `build_worker_args`; `permission_status_lines` + its print site next to `model:`; Contract 3b doc comment rewritten | Unit: argv with `Some` still emits `--no-extensions` + `-e <dir>` in order; argv with `None` emits `--no-extensions` and **no** `-e`, persona still last, no lens/guardrails tokens; status lines (gated: 1 line, degraded: 2 lines incl. exact indentation, ungated tools named, detail embedded); env-absent+discovery-resolves detail names both (D11); existing suite green |
| 3 | `feat: add strict mode for runs without a permission system` | Policy + config | `require_permission_extension` field + coercion; `StrictMode` + `resolve_require_permission_extension`; `enforce_permission_requirement`; `permission_status_lines` gains the strict marker + the unrecognized-env line (D10); main.rs calls the gate before any worker/TODO work (exit 1, message keeps the old prefix) | Unit: env parse table (truthy incl. padded/uppercase, falsy, junk → `ignored_env`, empty); config coercion (`true`/`false`/wrong type/absent); precedence env > config; strict + missing → `Err` containing both the old prefix and the escape hatch; strict + present → `Ok(Some(dir))`; non-strict + missing → `Ok(None)`; status lines: `; strict` on the gated line, the extra ignored-env line, and none printed on a strict refusal |
| 4 | `docs: document the optional permission system and ungated workers` | Docs + e2e | README: Prerequisites (soft prerequisite, strict opt-in, CI/unattended recommendation), **Configuration JSONC block + precedence prose**, Commands `supervise` row, Worker contract argv (`-e` conditional + candidate list), Permissions behavior (inert grants while ungated), Troubleshooting (hard-error row → degraded-mode row with install/env/CI-strict instructions); ARCHITECTURE Contract 3b + "at most one extension" + the D9 sub-protocol intro; acceptance-e2e intro + Scope bullet + Pass criteria + Prerequisites + section K | fmt/test/clippy green; docs read cleanly; K items below |

## Acceptance (e2e additions, `docs/acceptance-e2e.md`)

New section **K. Working without a permission system**, run in
`test-fixtures/spike/` with `PI_PLAN_STATE_DIR` isolated.

**Creating the absent-package condition (D13).** On a machine that *does* have
the package, simulate absence with a throwaway home — a bare temp `$HOME`
still needs the hard-required skill, so point `$PI_PLAN_SKILL` at the real one:

```bash
FakeHome=$(mktemp -d)
RealSkill="$HOME/.pi/agent/skills/implement-from-plan"   # expand BEFORE overriding HOME
HOME="$FakeHome" \
  PI_PLAN_SKILL="$RealSkill" \
  PI_PLAN_STATE_DIR=/tmp/pi-plan-e2e \
  pi-plan supervise
```

Capture `$RealSkill` in a preceding command, **not** inline as
`PI_PLAN_SKILL="$HOME/…"`: bash applies prefix assignments left to right, so
an inline `$HOME` there already expands to `FakeHome` (measured) and the run
would fail the hard-required skill prerequisite before reaching K.1. Every K
item below states which condition it uses: **absent-home** = the command
above, **bad-env** = the real home with
`PI_PLAN_PERMISSION_EXTENSION=/tmp/nope`.

1. **K.1** *(absent-home, `PI_PLAN_PERMISSION_EXTENSION` unset)* `pi-plan
   supervise` prints `model: …` then
   `permissions: DISABLED — no pi-permission-system package found (searched …)`
   plus the indented ungated line (naming `bash/edit/write`), and **proceeds**
   (no exit 1). No permission dialog ever appears; rows complete on their
   commits.
2. **K.2** *(bad-env)* → `permissions: DISABLED —
   $PI_PLAN_PERMISSION_EXTENSION points at /tmp/nope, which does not exist`,
   run proceeds.
3. **K.2b** *(real home, package installed, `PI_PLAN_PERMISSION_EXTENSION=/tmp/nope`)*
   → `permissions: gated (the pi-permission-system package at …; …
   points at /tmp/nope, which does not exist and was ignored)` — the ignored
   env override is named (D11).
4. **K.3** *(absent-home, `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=1`)* →
   `pi-plan: cannot resolve the permission-system extension: …` on stderr,
   exit 1, **no** worker spawned, no session/file created, **no**
   `permissions:` line printed.
5. **K.4** Strict precedence: a project `supervisor.config.json` with
   `{"requirePermissionExtension": true}` + `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=0`
   → degrades (env wins); the same config alone → refuses; with the package
   installed the gated line reads `…; strict` when the config key is `true`.
6. **K.5** Regression with the package installed: `permissions: gated (the
   pi-permission-system package at …)` and an out-of-cwd ask still relays a
   dialog (the existing I.1 item).
7. **K.6** `pi-plan status` / `stop` / `mark` / `reset-permissions` are
   unaffected in degraded mode (no new output, no new failure); `step N` and
   `supervise --row N` (same `cmd_supervise`) print the same `permissions:`
   line as `supervise`.
8. **K.7** *(absent-home, `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=ture`)* → the
   run degrades **and** prints an extra line naming the value:
   `permissions: ignoring unrecognized $PI_PLAN_REQUIRE_PERMISSION_EXTENSION=ture
   (strict mode follows the config: off)` (D10).
9. **K.8** *(absent-home with ≥ 1 grant on file from an earlier gated run)*
   the startup keep/reset prompt still appears; `restart` proceeds and the
   grants are inert (no dialog, no auto-approval); a later gated run still
   honours them.

## Risks (all accepted at the 2026-11-24 review)

- **Ungated workers are unrestricted.** pi's own security doc is explicit that
  there is no approval before every tool call; with zero extensions the
  worker's only limits are the `--tools` allowlist, the persona, and the OS
  account. Mitigations are exactly the chosen ones: the loud two-line startup
  warning (naming `bash/edit/write` as ungated), strict mode, and docs — with
  the README recommending `PI_PLAN_REQUIRE_PERMISSION_EXTENSION=1` for
  CI/unattended runs, where an ungated autonomous worker is worst and no one
  is watching stderr (D12). This is the accepted trade for "supervise runs
  anywhere".
- **The TUI hides stderr scrollback** once the alternate buffer takes over, so
  a long run can outlive the operator's memory of the warning. Decision 4
  accepted that (confirmed at the plan review); a permanent footer badge is a
  self-contained follow-up (one `TuiState` field + footer formatting) if it
  turns out to be missed. The CI recommendation above is the mitigation for
  the case where nobody sees the line at all.
- **A candidate can be a directory that is not a permission system.** `-e`
  then fails to load, pi logs it on the worker's stderr
  (`~/.pi-plan/<key>/worker-stderr.log`) and the worker runs ungated —
  measured for an empty directory during the review (see "Measured `pi`
  behavior"). The resolved path is printed on the startup line, so the
  misconfiguration is visible. Accepted; verifying a package manifest
  (`package.json` → `pi.extensions`) is a possible later tightening.
- **A typoed strict env value still permits a degraded run** when the config
  value is `false` (D10 falls back, it does not fail). The extra
  `permissions: ignoring unrecognized …` line is what keeps it from being
  silent; a user who wants a typo to be fatal can set the config key instead.
- **Strict mode fails before `TODO.md` is parsed** (the resolution moves
  above `read_todo_file`). Acceptable: prerequisite failures precede plan
  failures, exactly as the implement-from-plan skill prerequisite already
  does.
- **Grants are inert while ungated** and the keep/reset prompt still appears
  when grants exist (decision 4), immediately after the `permissions:
  DISABLED` line. Accepted confusion, named explicitly here, mitigated by one
  docs sentence and e2e K.8; skipping the prompt while ungated is explicitly
  out of scope.
- **Docs land in the last commit** (step 4) while README/ARCHITECTURE become
  false in step 2. This matches the repo's established final `docs:` commit
  precedent (e.g. `261e484`), so steps 2–3 are knowingly inconsistent with
  the docs until step 4 lands; do not interleave halves of a step to "fix" it.
- **`SupervisorConfig` gains a field on a `Serialize`-deriving struct.** Only
  `read_config_file` / `coerce_config` are used for reading today and the
  scaffold is a hand-formatted string, so no output changes.
