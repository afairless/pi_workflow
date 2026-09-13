# Plan: supervisor memory of permission approvals (auto-approval with precedent)

## Status

New feature plan for the `pi-plan` orchestrator, prompted by user change
requests (2026-09-13):

1. **Precedent approvals** — when the operator grants a worker a
   "for this session" permission (e.g. *"Yes, allow reads to `"/home/tr/*"`
   for this session"*), `pi-plan` records a durable, project-scoped copy of
   that grant. When a later worker asks for the same permission, `pi-plan`
   approves it automatically, without the operator.
2. **Always-grants** — workers may (1) always read and write anywhere in the
   project directory, (2) always read skills, (3) always execute scripts
   that accompany skills.
3. **Reset control** — permissions do not expire by default; the operator can
   reset them per project, via a keep/reset prompt at the start of a new
   supervisor session and via a dedicated CLI command.

All decisions below were locked with the user on 2026-09-13 (see Locked
decisions). Scope is strictly the `pi-plan` supervisor controlling its own
workers. The `plan-master` parent-agent/subagent path is out of scope. No
configuration of `pi-permission-system`, `pi-guardrails`, or any other Pi
extension is modified; the supervisor's **only** control mechanism is
answering the permission dialogs it relays.

## The problem being solved (evidence)

- **Worker sessions are fresh `pi --mode rpc` processes.** Each row attempt
  spawns a new worker with a new session. Approvals the operator grants to
  worker 1 are session-scoped: `@gotgenes/pi-permission-system` keeps them
  in an in-memory `SessionRules` store that is cleared at `session_shutdown`
  (`src/session/session-rules.ts` in the package). Worker 2 asks the same
  question again.
- **The permission generalization already exists on the dialogs.** When an
  `ask` resolves, the permission system offers *"Yes, allow
  `<reads|writes|reads and writes>` to `"<pattern>"` for this session"*,
  where the pattern is computed from the **accessed paths** (parent-directory
  globs for path surfaces, arity-based command patterns for the bash
  surface). The suggested pattern is what the operator actually approves.
- **Command-string matching cannot be the record.** `cat ~/text_file.txt`
  and `cd ~; cat text_file.txt` are different strings but the same access.
  The permission system already resolves this: its bash pipeline parses the
  command with tree-sitter, folds in-shell `cd` chains to compute an
  effective working directory, and extracts proven read/write path effects
  (`src/access-intent/bash/*`) — so both spellings gate on the same path and
  suggest the same pattern. A **path-and-direction record** therefore
  generalizes across command spellings for free; a command-string record
  cannot.
- **The review log cannot serve as the memory.** The dropped review log
  records decisions, but the granted session pattern serializes as
  `decidedBy.pattern: null` in the installed version. The record must be
  captured **at the dialog layer**, where `pi-plan` already sees the option
  strings and forwards the chosen option text back.
- **`pi-guardrails` hard-blocks out-of-cwd access in workers, and there is
  no dialog to answer.** Its `pathAccess` gate prompts via `ctx.ui.custom`,
  which is a documented no-op in `--mode rpc` (returns `undefined` →
  guardrails falls through to `block: "User denied access outside working
  directory"` — verified in `extensions/path-access/index.ts` and pi's
  `dist/modes/rpc/rpc-mode.js`). Its only automatic exceptions are inside
  `cwd`, `/dev/null`, pi's own docs, and the **active** skill's file+base
  dir (`--skill …`). Consequently, before this plan: in-project access is
  relayed as dialogs (approvable), but reading non-active skills and
  executing their scripts can never be approved at all, and the operator's
  out-of-cwd grants (`~/text_file.txt`) cannot be exercised.
- **The worker's extension set can be pinned.** `pi --no-extensions --extension <path>`
  disables extension discovery while explicit `-e` paths still load
  (`pi --help`; `docs/usage.md`). This lets the supervisor launch workers
  **without** `pi-guardrails`, so its hard-block disappears and every worker
  access is decided by the permission system's relayed dialogs — which the
  supervisor can now auto-answer.

## Verified technical facts (pi 0.85.1, this repo, installed extensions)

| Fact | Where verified |
|---|---|
| Permission select dialogs in RPC mode are plain `ctx.ui.select` prompts: options `["Yes", <session option>, (<width option>), "No", "No, provide reason"]`; the session option embeds the suggested pattern/direction, e.g. `Yes, allow reads to "/home/tr/*" for this session`, and the width option `Yes, allow reads and writes to "…" for this session`. | `@gotgenes/pi-permission-system` `src/authority/permission-dialog.ts`, `permission-prompt-component.ts`, `local-user-authorizer.ts`, `presentation/pattern-suggest.ts` |
| Multi-path / direction-disagreeing asks carry no single pattern in the label (`… to N paths for this session`, or the plain `Yes, for this session`). | `presentation/pattern-suggest.ts::describeGrantTarget` |
| Verb-less surfaces (`bash`, `mcp`, `skill`, tool catch-all) offer session options with **no direction verb**: `Yes, allow "<pattern>" for this session`; bash suggestions are arity-based command prefixes (`git status *`), skill suggestions are exact names (`librarian`), tool catch-all is `*`; these surfaces never offer the width (`reads and writes`) option. | docs `session-approvals.md` "Grant Direction" / "Suggested Patterns"; package `src/access-intent/bash/bash-arity.ts`, `src/session/session-rules.ts` |
| Session grants are ephemeral in-memory rules, cleared at `session_shutdown`; pattern surfaces: `path`/`path_read`/`path_write`, `external_directory`/`external_directory_read`/`external_directory_write`, per-tool (`read`/`write`/`edit`/…), `bash`, `skill`, `mcp`; a bare family expands to both directional members; family-width grant = both directions. | package `src/session/session-rules.ts`, `src/access-intent/path-surfaces.ts`, docs `session-approvals.md` |
| Pattern language: `*` is greedy and crosses `/`; `?` is one character; trailing `~/a_directory/*` covers the subtree. Path patterns match referenced and symlink-resolved forms. | docs `configuration.md`, README |
| `--no-extensions, -ne` disables extension discovery; explicit `--extension, -e <path>` still loads (repeatable). Settings-package extensions (incl. guardrails, lens, rpiv, subagents) then do not load. Built-in tools are unaffected (`--tools` still applies). | `pi --help`; `docs/usage.md` |
| `ctx.ui.custom` is a no-op in RPC mode (`return undefined`); guardrails treats an unanswered pathAccess ask as a block. | pi `dist/modes/rpc/rpc-mode.js`; guardrails `extensions/path-access/index.ts`, `prompt.ts` |
| Current worker argv (contract 3b) contains no extension flags; `--approve` is pi's project-trust flag, unrelated to dialogs. | `src/worker.rs::build_worker_args` |
| `pi-plan` sees the full dialog (`title` facts, `options: Vec<String>`) and forwards the **chosen option string** back as `UiReply::Value` in both TUI (modal rows) and line mode (`reply_from_input`). | `src/rpc.rs::ExtensionUiRequest`; `src/ui.rs::dialog_lines`/`reply_from_input`; `src/main.rs::worker_tail`, `dialog_roundtrip` |
| Run-state root and per-project key: `$PI_PLAN_STATE_DIR > $HOME/.pi-plan`, key = sanitized cwd basename + `sha256-8` — already home of `supervisor-state.json`, `worker-stats.jsonl`, `sessions/`. | `src/storage.rs::ProjectStorage::resolve`; README "Where is run state?" |

## Design

The supervisor is a **permission proxy**: it decides, for every relayed
permission dialog, whether to (a) auto-approve because an always-grant or a
stored precedent covers it, or (b) show the dialog as today. When the
operator picks a "…for this session" option, the supervisor records the
grant durably. No extension config is touched.

### 1. The grant record (the form)

A grant is a **permission-shaped, command-agnostic** record:

```json
{
  "v": 1,
  "grants": [
    {
      "id": "3fa8…",
      "surface": "external_directory",     // family name only
      "direction": "read",                  // read | write | both | null
      "pattern": "/home/tr/*",              // the approved suggested pattern
      "width": "proven",                    // proven | family | null
      "worker": "pi-plan-row-3",            // who earned it (audit)
      "createdAt": "2026-09-13T12:00:00Z"
    },
    {
      "id": "7c1d…",
      "surface": "bash",                    // verb-less family (D11)
      "direction": null,                    // no capability axis on this surface
      "pattern": "git status *",            // token-prefix suggestion, verbatim
      "width": null,
      "worker": "pi-plan-row-9",
      "createdAt": "2026-09-13T14:00:00Z"
    }
  ]
}
```

- `direction` = the verb in the approved label (`reads` → `read`, `writes`
  → `write`, `reads and writes` → `both`/family width). A `family`-width
  grant's direction is `both`. Verb-less labels (`bash`, `skill`, `mcp`,
  tool catch-all — D11) carry no verb: `direction` is `null`, `width` is
  `null` (no axis to split), and matching skips the direction/width check
  for them.
- `pattern` = the quoted pattern from the approved label, verbatim. For
  path surfaces the suggestion is derived from the *accessed paths*, so the
  same record matches later, differently-spelled commands that access the
  same paths (the `cat ~/text_file.txt` vs `cd ~; cat text_file.txt` case
  falls out naturally). For verb-less surfaces the pattern is the surface's
  own suggestion grammar (bash token-prefix, skill exact name).
- **Never record** (D10): plain `Yes` (one-time — never a precedent, by
  decision D4), labels without a single quoted pattern (multi-path or
  direction-disagreeing asks), any `No`/denial, the tool catch-all `*` and
  any other bare-`*` pattern (D11), **and every reply the supervisor
  generated itself** — only operator-chosen options from the human dialog
  path reach the store.
- A **matching check** applies a stored grant to a new ask when surface
  family matches, direction is compatible when both sides carry one (family
  width covers both; verb-less grants skip the direction/width check), and
  the stored pattern **covers** the new ask's suggested pattern.
  Containment is per-surface grammar: **path globs** use glob containment
  (`*`/`?`/literal language; stored ⊇ new); **bash** uses token-segment
  prefix containment (`git status *` ⊇ `git status --short`; `git status *`
  does **not** cover `git push`); **skill** uses exact equality. The new
  ask's suggestion is read from its own session-option label — the
  extension already did the proof/path resolution, so `pi-plan` never
  parses shell.

### 2. Always-grant rules (supervisor-side, evaluated before stored grants)

Built-in rules, data in code, not persisted:

| # | Rule | Decision |
|---|---|---|
| 1 | The ask has **≥ 1 flagged path** and every flagged path lies within the project root (`cwd`) | auto-approve, any direction |
| 2 | The ask has **≥ 1 flagged path**, every flagged path lies under the derived skills root(s) (`$HOME/.pi/agent/skills`, the resolved `--skill` dirs), **and** the ask is read-direction (label verb `reads`, or bash read-proven) | auto-approve |
| 3 | Bash ask whose command text references a script under `<skills root>/**/scripts/**` | auto-approve once (`Yes`) |

"Flagged path" comes from the ask's own facts (`path : …` core fact; the
`external path` evidence lines for bash-external asks; the quoted glob in
the session option otherwise; a bash command-prefix pattern such as
`git status *` is **not** a flagged path). The empty-set guard on rules
# 1/#2 is deliberate: a bash ask with no file access (e.g. `git push`,
`curl …`) has zero flagged paths and must **not** vacuously satisfy rule
# 1 or #2 — it prompts exactly as today. Rules #2/#3 derive the skills root
from the same resolution the repo already uses for skill flags
(`$HOME/.pi/agent/skills`, `$PI_PLAN_SKILL` / `$PI_PLAN_CLEAN_SKILL`
overrides), so a relocated skill keeps its read auto-approval. Anything not
covered by an always-grant or a stored grant is prompted exactly as today —
including all denials, writes into the skills tree, in-cwd *commands* with
no file access (the bash surface), and `skill`-surface fetches (no path
facts; those are covered only by a stored exact-name skill grant, D11).

Auto-reply payload: when the dialog offers a session option (pattern-bearing
label), reply with that option string when covered — the worker records the
grant in its **own session rules**, so it stops re-asking mid-run; when the
ask is path-covered by an always-grant but the label is pattern-less, reply
plain `Yes`; rule #3 (skill scripts) always replies plain `Yes` (one-time).
The reply string is the supervisor's own, and by D10 it is **never** written
to `permissions.json` — only operator-chosen replies record. Uncovered,
write-into-skills, and unknown asks never auto-approve: they render and
prompt exactly as today.

### 3. Durable store

- Location: `<run-state root>/permissions.json`, i.e. `~/.pi-plan/<key>/permissions.json`
  (`$PI_PLAN_STATE_DIR`-aware), resolved by the existing storage resolver —
  so the store is per-project, survives workers **and** supervisor sessions,
  and a moved/copied repo starts fresh like the rest of the run state.
- Write discipline: whole-file atomic rewrite (temp file + rename), same
  mechanism as `worker-stats.jsonl`; dedupe on identical
  (family, direction, pattern — `null` direction coalesces for verb-less
  surfaces) — idempotent re-grants update nothing. Only operator-chosen
  replies ever write (D10); auto-generated approvals never touch the store.
- No expiry by default (D5). Pruning only via reset.
- Read failure: warn on stderr and treat as **empty** (auto-approval
  disabled, run continues, no reset prompt) — fail-closed on the permit
  side, never blocks the run.

### 4. Worker extension set (the guardrails answer)

The worker spawn changes from "load settings packages" to **bare + the
permission system only**:

```
pi --mode rpc --session-dir … --name … --model … --thinking high --approve
   --tools read,grep,find,ls,bash,edit,write --skill <path>
   --no-extensions -e <permission-system dir>
   --no-lsp --no-lens --no-tests --no-autoformat --no-autofix --no-opengrep
   --append-system-prompt <persona>
```

- `<permission-system dir>` resolves `$PI_PLAN_PERMISSION_EXTENSION` first,
  else `~/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`
  (mirrors `$PI_PLAN_SKILL`); unresolvable → fail fast before any worker
  spawns, exactly like the skill prerequisite.
- Effect: `pi-guardrails` (and lens/rpiv/subagents/hermes packages) no longer
  load in workers; the pathAccess hard-block is gone; every worker access is
  decided by the permission system's gates, surfaced as the select dialogs
  the supervisor relays — so out-of-cwd approvals (skills, `~/…`) become
  exercisable for the first time.
- No tool-set regression: the `--tools` allowlist is unchanged; lens tools
  were already disabled via `--no-lens`; rpiv/subagent tools are not in the
  allowlist; the injected `implement-from-plan` body no longer needs the
  guardrails active-skill auto-allow (it never relied on reading its file).
- Behavior change to document: out-of-cwd access in workers goes from
  *silently blocked by guardrails* to *prompted (or auto-approved when
  covered)*.
- **The clean-worktree agent inherits the same spawn.** It shares
  `build_worker_args` and the dialog tail, so `-ne -e <permission-system>`
  and the always-grants/proxy apply to clean passes too; guardrails'
  active-skill exception disappears there as well, replaced by rules #2/#3
  against the derived skills root (see Risks).
- **Loading proof.** `-e <package-dir>` resolves the package manifest
  (`package.json` → `pi.extensions` → `<dir>/src/index.ts`); step 3 lands a
  one-command spike (`pi -ne -e <permission-system dir> --help` shows the
  permission system's flags), and the e2e out-of-cwd-ask item is the
  load-bearing proof that worker gating survived (see Acceptance).

### 5. Reset UX

- **Start-of-run prompt:** on `supervise`/`step`, when `permissions.json`
  exists with ≥ 1 grant, prompt (same in-TUI modal / line-mode machinery as
  the existing `QuestionPause` seam): *"Keep existing permissions"* (default)
  / *"Reset permissions for this project"*. `stop` cancels the run start
  and exits **2** (stopped semantics); `restart` at the prompt is treated as
  keep-and-proceed (there is no worker to restart); EOF in line mode → keep
  (one informational line). `status`/`mark` never prompt. A corrupt store
  (read failure) skips the prompt; the warning is repeated as a line in the
  final report (D8).
- **CLI command:** `pi-plan reset-permissions [--yes]` clears the store,
  prints how many grants it removed; without `--yes` it asks for
  confirmation first (mirrors `mark` ergonomics).
- **Visibility:** `pi-plan status` reports the stored grant count; the final
  supervise report gains a line (auto-approvals this run + grants on file).

## Code map (integration points)

| Site | Change |
|---|---|
| `src/permissions.rs` (new leaf) | `Grant` model (direction/width nullable for verb-less surfaces); load/save (atomic, dedupe, clear); per-surface containment matcher (path glob ⊇, bash token-prefix, skill exact); option-label parser (direction verb + quoted glob for path surfaces; verb-less `bash`/`skill` shapes; pattern-less detection); recording predicate (select-only + reply must be one of `req.options`, D10); ask-view builder from `ExtensionUiRequest`; always-grant predicates (project root, skills root, skill scripts — each requiring ≥ 1 flagged path); parse failures degrade to `prompt`. |
| `src/worker.rs::build_worker_args` (+ skill-path resolver pattern) | `--no-extensions`, `-e <permission-system dir>`; `$PI_PLAN_PERMISSION_EXTENSION` override; hard-fail when unresolvable; **same commit updates the in-code Contract 3b argv doc comment and the hardcoded-shape test**; manual spike `pi -ne -e <permission-system dir> --help`. |
| `src/main.rs::cmd_supervise` (startup) | Load store; keep/reset prompt via the pause seam (`stop` → exit 2, `restart` → keep-and-proceed); pass the store into the tail task; final report counts + corrupt-store warn line (D12). |
| `src/main.rs::worker_tail` (dialog arm) | Before opening the modal/roundtrip: match ask → auto-reply via `reply_extension_ui` (+ one log line), **tagged auto so the recorder skips it (D10)**. After a **human** modal/roundtrip reply: if `UiReply::Value` is a session-grant label with a parseable pattern **and** the ask is a select whose options contain the reply → record + persist. Both TUI and line paths converge on these two spots. |
| `src/cli.rs` | `reset-permissions [--yes]` command; `status` gains the grant count. |
| Docs | README ("Permissions behavior", "Worker contract", Troubleshooting), `docs/ARCHITECTURE.md` (module map), `docs/acceptance-e2e.md` (new checks). |

## Step table (commit-by-commit)

Workflow per step: implement → `cargo test` → `cargo fmt --check` → `cargo
clippy --all-targets --all-features -- -D warnings` → commit → stop.

| # | Commit message | Logical unit | Key deliverables | Tests |
|---|---|---|---|---|
| 1 | `feat: add the project permissions store (load, save, dedupe, clear)` | Storage leaf | `src/permissions.rs`: `Grant` (`surface`/`direction`/`pattern`/`width`/`worker`/`createdAt`), `v:1` serialization, atomic rewrite at `permissions_path(root)`, dedupe on (family, direction, pattern), `clear`, corrupt → `Err`-less empty-with-warning contract; `src/storage.rs` resolver hook | Unit: round-trip, dedupe, clear, corrupt-file handling, path resolution under `$PI_PLAN_STATE_DIR` |
| 2 | `feat: match stored grants and always-grants against dialogs` | Matching core | Option-label parser (direction verb + quoted glob for path surfaces; verb-less `bash` token-prefix and `skill` exact-name shapes; pattern-less detection); ask-view builder from `ExtensionUiRequest`; per-surface containment (path glob ⊇, bash token-prefix, skill exact); always-grant predicates (#1 cwd paths, #2 skills-root read, #3 skill-script commands — each requiring ≥ 1 flagged path); auto-reply option selection | Unit: `cat ~/text_file.txt` vs `cd ~; cat text_file.txt` converge; read/write/both widths; `git *` ⊇ `git status *` and `git status *` ⊇ `git status --short` but NOT `git push`; skill exact-only; containment edges (`/home/tr/*` ⊇ `/home/tr/x/*`, `*` crossings); **bash ask with no file access NOT auto-approved (non-vacuous guard)**; catch-all `*` / verb-less-with-`*` never match; unparseable labels → prompt; #1/#2/#3 boundaries (write into skills NOT covered); proptest: containment reflexivity + transitivity over generated globs |
| 3 | `feat: spawn workers without pi-guardrails (-ne + -e permission-system)` | Worker argv | `build_worker_args` emits `--no-extensions` and `-e <resolved>`; `$PI_PLAN_PERMISSION_EXTENSION` override; hard-fail when unresolvable; existing no-* flags kept; **same commit updates the in-code Contract 3b argv doc comment and the hardcoded-shape test**; manual spike: `pi -ne -e <permission-system dir> --help` lists the permission system's flags (proves `-e <package-dir>` resolves the manifest) | Unit: argv shape/quoting; `resolve_permission_extension` precedence + fail-fast; existing suite green |
| 4 | `feat: auto-approve covered permission dialogs and record session grants` | Dialog proxy | `worker_tail` pre-arm: always-grant/stored match → `reply_extension_ui` with the session/`Yes` option + log line, **tagged auto so the recorder skips it (D10)**; post-reply: only a **human** reply that is a select option ∈ `req.options` and parses as a session-grant label → store.add + persist; both TUI and line paths | Unit: matcher wiring; record-on-grant for **operator** replies; **auto-approval replies never recorded (store unchanged)**; no-pattern / non-select / non-grant replies ignored; verb-less bash + exact skill labels parse and record; helper tests for reply selection; manual acceptance in both modes |
| 5 | `feat: prompt to keep or reset project permissions at supervise start` | Reset prompt | Startup check (exists & non-empty) → modal/stdout prompt via the existing `QuestionPause` machinery; keep (default) / reset / `stop` (→ exit 2) / `restart` (→ keep-and-proceed); EOF → keep; corrupt store skips the prompt + warn line in the final report; `step` shares it | Unit: prompt decision logic (present/absent, empty, corrupt, stop→exit 2, restart→keep, EOF); line-mode round trip asserted |
| 6 | `feat: add pi-plan reset-permissions command` | CLI | `src/cli.rs` `reset-permissions [--yes]`; confirmation without `--yes`; prints removed count; `status` gains grant count; usage errors at clap parse time | Unit: parse (`--yes`/bogus), storage clear, status line; full suite |
| 7 | `docs: document supervisor permission memory and the bare worker extension set` | Docs + e2e | README "Permissions behavior" (proxy, always-grants, reset, out-of-cwd no-longer-blocked), "Worker contract" (argv), Troubleshooting table (`-ne`/`-e`, `$PI_PLAN_PERMISSION_EXTENSION`, permissions.json); ARCHITECTURE module map + prose; `docs/acceptance-e2e.md` **revised A-section in-cwd items** (no dialog — auto-approved, per always-grant #1) + worker-pinning note in Prerequisites + new checks | fmt/test/clippy green; docs read cleanly; e2e checks below |

## Locked decisions

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
  and never written to `permissions.json`, so auto-approvals cannot accrue
  precedents the operator never made (the `status` count and the keep/reset
  prompt stay operator-meaningful). Auto-replies still answer with the
  session option so the worker stops re-asking mid-run; recording simply
  skips them. Recording additionally requires a select dialog whose option
  set byte-contains the replied label.
- **D11** Verb-less surfaces record securely and narrowly: `bash` patterns
  with ≥ 1 concrete command token before a trailing `*` (matched by
  token-segment prefix containment — `git status *` never covers
  `git push`); `skill` patterns only when exact names (no `*`/`?`; exact
  match); `mcp` and any bare catch-all `*` pattern are **never** recorded
  (workers carry no MCP tools; a catch-all would silently auto-approve a
  whole surface). Verb-less grants store `direction: null` / `width: null`;
  matching skips the direction/width check for them. Unparseable or
  unsupported labels degrade to prompt (never auto-approve on a parse
  failure).
- **D12** Start-of-run prompt semantics: `stop` cancels the run start and
  exits **2** (stopped semantics); `restart` at the prompt is keep-and-
  proceed; EOF in line mode keeps; `status`/`mark` never prompt; a corrupt
  store skips the prompt and surfaces a warn line in the final report.

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

## Risks / pitfalls

- **Label parsing skew.** The option labels are the extension's vocabulary;
  a future extension version can reword them. Parser is isolated in
  `src/permissions.rs` with its own tests; any unrecognized label degrades
  to "prompt" (never auto-approve on a parse failure).
- **Broad patterns.** A stored path glob like `*` covers its whole surface —
  that is exactly the width the operator approved (the label shows it);
  documented, not special-cased. The tool catch-all `*` and other bare-`*`
  patterns are **never recorded** (D11), so no verb-less surface can ever
  auto-approve wholesale from a durable grant.
- **Guardrails removal.** Out-of-cwd worker access flips from silently
  blocked to prompted-unless-covered; the task is to surface this in README
  and acceptance, not restore the block.
- **Definition of "read" for #2.** Direction comes from the ask's label
  verb; a direction-unproven ask on a skill path (e.g. a non-script program
  under skills) is NOT auto-approved (#3 covers only `scripts/**` command
  targets). Conservative by construction.
- **Store hygiene.** Grants accrue until reset; count is visible in
  `status`. Cross-machine sync is out of scope (like all run state).
- **Auto-reply ≠ operator approval.** The pre-arm's session-option replies
  are the supervisor's own and are excluded from durable recording (D10);
  a session-option reply is recorded only on the human dialog path, so
  auto-approvals never create precedents and the store never feeds its own
  grants.
- **Verb-less recording is bounded by the extension's proofs.** A durable
  bash grant only ever covers a later ask whose *suggested* pattern is
  token-prefix-contained; `pi-plan` never parses shell, so the generality
  ceiling is exactly what the extension's tree-sitter proof pipeline
  proposed and the operator saw in the label.
- **Skills root is derived, not literal.** Rules #2/#3 resolve the skills
  root with the same precedence as the repo's skill flags
  (`$HOME/.pi/agent/skills`, `$PI_PLAN_SKILL` / `$PI_PLAN_CLEAN_SKILL`), so
  relocated skills keep read auto-approval. The `skill` surface (no path
  facts) is not covered by rule #2 — it prompts unless a stored exact-name
  skill grant (D11) covers it.
- **Clean agent inherits the change.** The clean-worktree pass shares
  `build_worker_args` and the dialog tail; its active-skill guardrails
  exception is replaced by rules #2/#3 against the derived root, and its
  dialogs are auto-approved/prompted by the same proxy as row workers.

## Acceptance (additions to `docs/acceptance-e2e.md`)

- Worker argv contains `--no-extensions` and `-e <permission-system>` and
  never references guardrails; the first e2e out-of-cwd ask **prompting** is
  the proof the permission system still loads and gates workers (step 3's
  spike covers the flag surface).
- Out-of-cwd read (`cat ~/text_file.txt` then `cd ~; cat text_file.txt` in
  a later worker): first ask prompts; a "for this session" grant is
  recorded; the second worker's ask is auto-approved (no prompt) and the
  command succeeds; `permissions.json` contains one deduped grant — and the
  auto-approval **does not** add a second record (D10).
- In-cwd write (always-grant #1): auto-approved, no prompt, both TUI and
  line mode; `permissions.json` is unchanged (no operator grant accrued).
- In-cwd write (always-grant #1) alongside a bash no-file-access sibling
  (`git status`): the path ask auto-approves; the bash ask still prompts
  (non-vacuous guard).
- Non-active skill read (#2): auto-approved when read-direction; a write into
  the skills tree still prompts.
- Skill script execution (#3): auto-approved once; a stray script outside
  the skills tree prompts.
- Verb-less round trip (`git status --short` then, in a later worker,
  `git status`): the first grant records pattern `git status *`, the later
  ask auto-approves, and a `git push` under the same session still does not
  auto-approve from that grant (D11).
- Novel uncovered ask still prompts; `stop`/`restart`/`status` work at
  dialogs and at the startup keep/reset prompt; `stop` at the startup prompt
  exits 2 (D12).
- `pi-plan reset-permissions` removes all grants (counted); a fresh
  `supervise` no longer offers the keep/reset prompt.
- E2E checklist A: in-cwd dialog items (currently "approve the dialog")
  are revised to expect **no dialog** + an auto-approval log line.
