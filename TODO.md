# Implementation Plan: Supervisor memory of permission approvals (auto-approval with precedent)

Source: `docs/research/plan-supervisor-permission-memory.md`

The `pi-plan` supervisor becomes a permission proxy over the dialogs it
relays: it records the operator's "…for this session" grants durably
(`~/.pi-plan/<key>/permissions.json`), auto-approves later asks covered by
an always-grant rule or a stored precedent, spawns workers without
`pi-guardrails` (`--no-extensions -e <permission-system>`), and gains a
keep/reset prompt plus a `reset-permissions` command. Only the supervisor's
own workers are in scope; no extension configuration is modified. Seven
commits.

The commit messages in the table below are **exact** — taken verbatim from
the source plan. Workflow per step: implement → `cargo test` → `cargo fmt
--check` → `cargo clippy --all-targets --all-features -- -D warnings` →
commit with the table's message → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: add the project permissions store (load, save, dedupe, clear)` | Storage leaf | `src/permissions.rs`: `Grant` (`surface`/`direction`/`pattern`/`width`/`worker`/`createdAt`), `v:1` serialization, atomic rewrite at `permissions_path(root)`, dedupe on (family, direction, pattern), `clear`, corrupt → `Err`-less empty-with-warning contract; `src/lib.rs` gains `pub mod permissions;`; `src/storage.rs` resolver hook | Unit: round-trip, dedupe, clear, corrupt-file handling, path resolution under `$PI_PLAN_STATE_DIR` |
| 2 | `feat: match stored grants and always-grants against dialogs` | Matching core | Option-label parser (direction verb + quoted glob for path surfaces; verb-less `bash` token-prefix and `skill` exact-name shapes; pattern-less detection); ask-view builder from `ExtensionUiRequest`; per-surface containment (path glob ⊇, bash token-prefix, skill exact); always-grant predicates (#1 cwd paths, #2 skills-root read, #3 skill-script commands — each requiring ≥ 1 flagged path); auto-reply option selection | Unit: `cat ~/text_file.txt` vs `cd ~; cat text_file.txt` converge; read/write/both widths; `git *` ⊇ `git status *` and `git status *` ⊇ `git status --short` but NOT `git push`; skill exact-only; containment edges (`/home/tr/*` ⊇ `/home/tr/x/*`, `*` crossings); bash ask with no file access NOT auto-approved (non-vacuous guard); catch-all `*` / verb-less-with-`*` never match; unparseable labels → prompt; #1/#2/#3 boundaries (write into skills NOT covered); proptest: containment reflexivity + transitivity over generated globs |
| 3 | `feat: spawn workers without pi-guardrails (-ne + -e permission-system)` | Worker argv | `build_worker_args` emits `--no-extensions` and `-e <resolved>`; `$PI_PLAN_PERMISSION_EXTENSION` override; hard-fail when unresolvable; existing no-* flags kept; **same commit updates the in-code Contract 3b argv doc comment and the hardcoded-shape test**; manual spike: `pi -ne -e <permission-system dir> --help` lists the permission system's flags (proves `-e <package-dir>` resolves the manifest) | Unit: argv shape/quoting; `resolve_permission_extension` precedence + fail-fast; existing suite green |
| 4 | `feat: auto-approve covered permission dialogs and record session grants` | Dialog proxy | `worker_tail` pre-arm: always-grant/stored match → `reply_extension_ui` with the session/`Yes` option + log line, tagged auto so the recorder skips it (D10); post-reply: only a **human** reply that is a select option ∈ `req.options` and parses as a session-grant label → store.add + persist; both TUI and line paths | Unit: matcher wiring; record-on-grant for **operator** replies; **auto-approval replies never recorded (store unchanged)**; no-pattern / non-select / non-grant replies ignored; verb-less bash + exact skill labels parse and record; helper tests for reply selection; manual acceptance in both modes |
| 5 | `feat: prompt to keep or reset project permissions at supervise start` | Reset prompt | Startup check (exists & non-empty) → modal/stdout prompt via the existing `QuestionPause` machinery; keep (default) / reset / `stop` (→ exit 2) / `restart` (→ keep-and-proceed); EOF → keep; corrupt store skips the prompt + warn line in the final report; `step` shares it | Unit: prompt decision logic (present/absent, empty, corrupt, stop→exit 2, restart→keep, EOF); line-mode round trip asserted |
| 6 | `feat: add pi-plan reset-permissions command` | CLI | `src/cli.rs` `reset-permissions [--yes]`; confirmation without `--yes`; prints removed count; `status` gains grant count; usage errors at clap parse time | Unit: parse (`--yes`/bogus), storage clear, status line; full suite |
| 7 | `docs: document supervisor permission memory and the bare worker extension set` | Docs + e2e | README "Permissions behavior" (proxy, always-grants, reset, out-of-cwd no-longer-blocked), "Worker contract" (argv), Troubleshooting table (`-ne`/`-e`, `$PI_PLAN_PERMISSION_EXTENSION`, permissions.json); ARCHITECTURE module map + prose; `docs/acceptance-e2e.md` revised A-section in-cwd items (no dialog — auto-approved, per always-grant #1) + worker-pinning note in Prerequisites + new checks | fmt/test/clippy green; docs read cleanly; e2e checks below |

## Locked decisions (from the source plan)

- **D1** Scope is `pi-plan`'s supervisor answering worker dialogs. The
  `plan-master`/subagent path is out of scope.
- **D2** No changes to `pi-permission-system`, `pi-guardrails`, or any
  extension configuration. `pi-guardrails` is removed from workers via
  `--no-extensions` + explicit `-e <permission-system>`, so every worker
  access is gated solely by the permission system's relayed dialogs.
- **D3** The permission record is permission-shaped: `(surface-family,
  direction, pattern, width)`, never a command string.
- **D4** Only "…for this session" approvals become precedents; plain one-time
  `Yes` never does.
- **D5** No expiry by default; store at `~/.pi-plan/<key>/permissions.json`;
  dedupe; read failures → empty + warn (fail-closed on permits, never
  blocks the run).
- **D6** Reset via start-of-run keep/reset prompt (EOF→keep) **and**
  `pi-plan reset-permissions [--yes]`.
- **D7** Always-grants #1–3 are supervisor-side rules evaluated before
  stored grants; each requires **≥ 1 flagged path** (no vacuous coverage of
  path-less bash asks); anything uncovered is prompted as today; denials are
  never recorded or auto-made.
- **D8** Auto-approvals are visible: one log line per event + final-report
  counts + `status` grant count.
- **D9** Multi-path / direction-disagreeing asks (no single pattern in the
  label) are never recorded; they may still be auto-approved only by the
  path-based always-grants. Containment is per-surface grammar (glob,
  token-prefix, exact) — see D11.
- **D10** Durable recording captures only **operator-chosen** session-grant
  options from the human dialog path. The supervisor's own auto-approval
  replies (always-grants and stored-grant matches) are tagged as generated
  and never written to `permissions.json`; auto-replies still answer with
  the session option so the worker stops re-asking mid-run; recording
  additionally requires a select dialog whose option set byte-contains the
  replied label.
- **D11** Verb-less surfaces record securely and narrowly: `bash` patterns
  with ≥ 1 concrete command token before a trailing `*` (token-segment
  prefix containment — `git status *` never covers `git push`); `skill`
  patterns only when exact names; `mcp` and any bare catch-all `*` pattern
  are **never** recorded. Verb-less grants store `direction: null` /
  `width: null`; matching skips the direction/width check for them.
  Unparseable or unsupported labels degrade to prompt.
- **D12** Start-of-run prompt semantics: `stop` cancels the run start and
  exits **2**; `restart` at the prompt is keep-and-proceed; EOF in line mode
  keeps; `status`/`mark` never prompt; a corrupt store skips the prompt and
  surfaces a warn line in the final report.

## Invariants to preserve

- Worker determinism flags, `--approve`, `--tools` allowlist, session dir,
  persona, and the `PI_WORKER_STATUS` contract are unchanged (argv gains
  only `--no-extensions` + `-e`).
- Every permission decision still goes through byte-exact dialog relay;
  novel or unparseable asks render and prompt exactly as today in both TUI
  and line mode (undo: store empty → old behavior, minus guardrails).
- The durable store never contains a grant the operator did not choose:
  auto-approval replies are excluded from recording (D10), and verb-less
  recording is bounded by D11's grammar constraints.
- `stop`/`restart`/`status` line commands behave identically at dialogs and
  at the new startup prompt.
- The project directory stays clean (all state under the run-state root).

## Step notes

### Step 1 (permissions store)

- New leaf `src/permissions.rs`; register `pub mod permissions;` in
  `src/lib.rs` (alphabetical position after `prompt`).
- `Grant` fields: `id` (uuid-ish string per grant), `surface` (family name
  only — e.g. `external_directory`, `bash`, `skill`), `direction`
  (`read`/`write`/`both`/null), `pattern`, `width` (`proven`/`family`/null),
  `worker`, `createdAt` (RFC3339). Serialize `{ "v": 1, "grants": [...] }`.
- Dedupe key = (family, direction, pattern); `null` direction coalesces for
  verb-less surfaces so re-grants are idempotent.
- Persist via whole-file atomic rewrite (temp file + rename), same mechanism
  as `worker-stats.jsonl` in `src/storage.rs`.
- Corrupt read → warn on stderr, treat as empty (never block the run).
- Resolve the path with the existing `ProjectStorage::resolve` machinery so
  `$PI_PLAN_STATE_DIR` is honored, mirroring `supervisor-state.json` /
  `worker-stats.jsonl` (`src/storage.rs:42`).

### Step 2 (matching core)

- Option-label parser reads the extension's own session-option strings
  (e.g. `Yes, allow reads to "/home/tr/*" for this session`; verb-less
  `Yes, allow "git status *" for this session`; exact `Yes, allow
  "librarian" for this session`); `pi-plan` never parses shell — the
  extension's tree-sitter pipeline already produced the suggested pattern.
- Ask-view builder: flagged paths come from the ask's own facts (`path : …`
  core fact, `external path` evidence lines, the quoted glob in the session
  option). A bash command-prefix pattern (`git status *`) is **not** a
  flagged path.
- Containment per surface: path glob containment (`*` crosses `/`, `?` one
  char, trailing `~/a/*` covers the subtree); bash token-segment prefix
  containment; skill exact equality.
- Always-grants run before stored grants, each gated on ≥ 1 flagged path:
  #1 all flagged paths within project root (any direction); #2 all within
  the derived skills root (`$HOME/.pi/agent/skills`, `$PI_PLAN_SKILL` /
  `$PI_PLAN_CLEAN_SKILL` overrides) **and** read-direction; #3 bash command
  referencing `<skills root>/**/scripts/**` (plain `Yes`, one-time).
- Parse failures, missing patterns, bare `*` patterns → `prompt` (never
  auto-approve on confusion).

### Step 3 (worker argv)

- `build_worker_args` (`src/worker.rs:195`) gains `--no-extensions` and
  `-e <resolved path>`; the 6 existing `DETERMINISM_FLAGS` stay.
- `<permission-system dir>` resolves `$PI_PLAN_PERMISSION_EXTENSION` first,
  else `~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`
  (mirror the `$PI_PLAN_SKILL` resolver pattern in `src/cli.rs`); when
  unresolvable, fail fast before any worker spawns — exactly like the skill
  prerequisite.
- Same commit: update the in-code Contract 3b doc comment
  (`src/worker.rs:186-194`) **and** the hardcoded-shape test
  `build_worker_args_matches_contract_3b_shape` (`src/worker.rs:860`), which
  asserts positional argv.

### Step 4 (dialog proxy)

- Pre-arm in `worker_tail` (`src/main.rs:819`), before the modal/roundtrip:
  build the ask view → match always-grants then stored grants → on a match,
  `reply_extension_ui(worker_id, req.id, UiReply::Value(session_option))`
  (or plain `Yes` when path-covered but pattern-less), log one line, and
  **tag the reply as machine-generated** so the recorder skips it.
- Post-reply: only when the reply is a **human** `UiReply::Value` (TUI
  `dialog_lines` path and line-mode `reply_from_input` path both converge
  here) AND the reply string ∈ `req.options` AND it parses as a session-grant
  label → `store.add(...)` + persist.
- Never record: plain `Yes`, pattern-less labels, denials, non-select asks,
  the tool catch-all `*`, any bare-`*` pattern, `mcp`.

### Step 5 (reset prompt)

- In `cmd_supervise` startup (`src/main.rs:196`): if `permissions.json`
  exists with ≥ 1 grant, open the keep/reset question through the existing
  `QuestionPause` seam (`src/supervise/mod.rs:61`, `LineCommand`/TUI modal
  machinery).
- `stop` → cancel run start, exit 2; `restart` → keep-and-proceed; EOF in
  line mode → keep, one informational line; corrupt store → skip the prompt,
  warn line in the final report.
- `step` shares the startup path (it dispatches through the same
  `cmd_supervise` flow). `status`/`mark` never prompt.

### Step 6 (reset-permissions CLI)

- New `Command::ResetPermissions` variant with an optional `--yes` flag;
  without it, ask for confirmation (mirror `mark` ergonomics in
  `src/cli.rs` + `src/main.rs::cmd_mark`); print how many grants were
  removed; usage errors at clap parse time (exit 2).
- `cmd_status` (`src/main.rs:139`) gains the stored grant count.

### Step 7 (docs + e2e)

- README: "Permissions behavior" (proxy, always-grants #1–3, reset UX,
  out-of-cwd flips from silently blocked to prompted-unless-covered),
  "Worker contract" (argv with `-ne -e`), Troubleshooting (`$PI_PLAN_*`
  vars, `permissions.json`).
- `docs/ARCHITECTURE.md`: module map gains `permissions.rs`; prose for the
  proxy, D3 record shape, D10 exclusion rule.
- `docs/acceptance-e2e.md`: revise A.2/A.5 in-cwd dialog items to expect
  **no dialog** (always-grant #1 auto-approval + log line); Prerequisites
  gains the worker-pinning note; add the new checks listed under Acceptance
  in the source plan (out-of-cwd round trip, in-cwd write, non-vacuous bash
  sibling, skills-root read / write-into-skills, skill-script once, verb-less
  `git status` round trip vs `git push`, reset-permissions, startup-prompt
  stop/restart/status).

## Acceptance criteria (end state)

- `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings` all green after the final commit.
- Worker argv contains `--no-extensions` and `-e <permission-system>` and
  never references guardrails.
- Out-of-cwd read round trip: first ask prompts, "for this session" grant is
  recorded, later worker's ask auto-approves, `permissions.json` holds one
  deduped grant and the auto-approval does **not** add a second record.
- In-cwd write: auto-approved with no prompt and no grant accrued; a `git
  status` sibling ask still prompts.
- `git status *` grant covers a later `git status` (any spelling) but never
  `git push`; skill read auto-approves, a write into the skills tree still
  prompts; skill-script execution approves once, a stray script prompts.
- `pi-plan reset-permissions` removes all grants (counted); a fresh
  `supervise` then shows no keep/reset prompt; `stop` at the startup prompt
  exits 2.
