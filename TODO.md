# Implementation Plan: Let `supervise` run when the permission system is not installed

Source: `docs/research/plan-optional-permission-system.md`

Makes the `pi-permission-system` extension **optional**. Resolution becomes
infallible (a four-candidate search, every candidate `exists()`-checked), a
missing package degrades the run with a loud two-line startup warning instead
of `exit 1`, workers then spawn with `--no-extensions` and no `-e` at all, and
an opt-in strict mode (`$PI_PLAN_REQUIRE_PERMISSION_EXTENSION` or the new
`requirePermissionExtension` config key) restores today's fail-fast. The
startup posture line (`permissions: …`) prints on every run, gated or
degraded.

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit with the table's
message → stop. One step at a time.

**Sequence note.** The source plan's four-commit table is kept in its original
order. Step 2 is a single commit by necessity, not by choice: changing
`WorkerSpawnOpts.permission_extension` to `Option<PathBuf>` breaks the three
spawn-opt builders and 40 test literals at once, so the compiler forces the
whole ripple into one step; it cannot be split without leaving the tree red.
The loud warning is bundled with the degrade in that same step (decision 1's
accepted fail-open trade needs its mitigation at the moment it appears), while
strict mode stays out of it and lands in step 3.

## Steps

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | refactor: resolve the permission-system package from candidate installs | Candidate resolution | `src/cli.rs::resolve_permission_extension`: four-candidate search in order (env override, then `<home>/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`, `<home>/.pi/agent/npm/node_modules/pi-permission-system`, `<home>/.pi/agent/extensions/pi-permission-system`), every candidate `Path::exists()`-checked including the env override (D6, D7), paths used verbatim (never absolutized or canonicalized, `builtin:` deliberately rejected), `Err` text lists every searched path and keeps the `$HOME`-unknown case; signature stays `Result<PathBuf, String>` so no call site moves; rewrite the pinned test `permission_extension_resolution_env_wins_and_default_requires_an_install` (`src/cli.rs:1951`) into fixture-dir cases (env wins against an existing dir, env set but absent is an `Err`) | unit |
| 2 | feat: spawn ungated bare workers when the permission system is missing | Degrade path | `src/cli.rs`: `PermissionExtension { dir: Option<PathBuf>, detail: String }`, the resolver made infallible with the documented `detail` table including the D11 combined detail; `src/worker.rs`: `permission_extension: Option<PathBuf>`, conditional `-e` (the `--no-extensions` flag is never conditional), rewritten Contract 3b doc comment; `src/supervise/mod.rs`: `Option<&'a Path>` plus the three `.map(..)` conversions at `:734`, `:762`, `:1353`; `src/supervise/tests.rs` 38 literals and the two `opts()` defaults in `src/worker.rs` wrapped in `Some(..)`; `src/main.rs`: resolution (and its env read) moved up to just below the `model:` line, services field from `dir.as_deref()`; new pure `permission_status_lines(&PermissionExtension)` printed at that seam | unit |
| 3 | feat: add strict mode for runs without a permission system | Strict policy + config key | `src/config.rs`: `require_permission_extension: bool` on `SupervisorConfig` plus `coerce_config` coercion from `requirePermissionExtension` (absent or wrong type falls back to `false`, global scaffold untouched); `src/cli.rs`: `StrictMode { required, ignored_env }`, `resolve_require_permission_extension(env, config)` (env > config > default, junk value reported in `ignored_env`), pure `enforce_permission_requirement(&PermissionExtension, bool) -> Result<Option<PathBuf>, String>` keeping today's message prefix and appending the escape hatch; `permission_status_lines` gains the `; strict` marker and the unrecognized-env line (D10); `src/main.rs` calls the gate before `read_todo_file` and any session or worker work | unit |
| 4 | docs: document the optional permission system and ungated workers | Documentation + e2e | `README.md`: Prerequisites (soft prerequisite, strict opt-in, CI/unattended recommendation), Configuration JSONC block and precedence prose (`requirePermissionExtension`), Commands `supervise` row, Worker contract (conditional `-e`, candidate list), Permissions behavior (grants inert while ungated), Troubleshooting (hard-error row becomes a degraded-mode row); `docs/ARCHITECTURE.md`: Contract 3b, "at most one extension" and the extension-UI permission sub-protocol intro; `docs/acceptance-e2e.md`: intro, Scope bullet, Pass criteria, Prerequisites and new section K items K.1-K.8 | — |

## Constraints carried from the source plan

- **Degrade is the default, strict is opt-in** (locked decision 1/2). No run
  outcome may hinge on a package that is not on disk; `exit 1` happens only
  when `$PI_PLAN_REQUIRE_PERMISSION_EXTENSION` or
  `"requirePermissionExtension"` asks for it.
- **Workers stay fully bare when ungated** (locked decision 3):
  `--no-extensions` alone, never `-e` of an ambient extension set, never
  `builtin:mcp`. A worker loads *at most* one extension.
- **One startup stderr warning line only** (locked decision 4). Degraded runs
  print two lines total; no TUI footer badge, no final-report line, no change
  to the start-of-run keep/reset prompt.
- **Existence is the only test** (D7, D8). A wrong candidate costs a
  diagnosable `pi` load error on the worker's stderr and an ungated worker,
  never a dead run — the resolved path is always printed.
- **Paths verbatim.** A relative env value resolves against the supervisor's
  cwd, which is also the worker's spawn cwd, so existence and `-e` agree.
- **Untouched:** the dialog proxy, the grants store, the always-grants, the
  keep/reset prompt (inert but present while ungated), `pi-plan status`, the
  final report and every exit code. No auto-install, no fallback to
  `pi-guardrails` or any ambient extension, no changes to
  `pi-permission-system` itself.

## Acceptance criteria (from the source plan)

1. Absent package, no strict mode: `model:` then `permissions: DISABLED — …`
   plus the indented ungated line naming `bash/edit/write`; the run proceeds,
   no permission dialog ever appears (K.1).
2. A bad env override is named on the posture line, not silently trusted
   (K.2), and when discovery still resolves the ignored override is named too
   (K.2b, D11).
3. Strict mode with the package missing: the old message prefix on stderr,
   exit 1, no worker, no session directory, no `permissions:` line (K.3).
4. Strict precedence: env wins over the config key in both directions; a
   gated strict run reads `…; strict` (K.4); an unrecognized env value is
   named on an extra line (K.7, D10).
5. With the package installed nothing regresses: the gated line prints and an
   out-of-cwd ask still relays a dialog (K.5); `status`/`stop`/`mark`/
   `reset-permissions`/`step N`/`supervise --row N` are unaffected (K.6).
6. While ungated, stored grants are inert and the keep/reset prompt still
   appears when grants exist (K.8).
7. `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets
   --all-features -- -D warnings` green at every step, and the manual gate
   recorded in `docs/acceptance-e2e.md` §K.
