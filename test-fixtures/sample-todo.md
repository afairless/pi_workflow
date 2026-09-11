# Implementation Plan: Deterministic `supervise` Extension — Steps 1–9

> **STATUS (2026-09-10):** Stage 0 COMPLETE — S0.1–S0.6 ✅ all pass.

Source: `docs/research/plan-supervisor-extension.md`

## Scope

Step 0 made no code changes. Steps 1–9 below build the deterministic
`supervise` extension: one fresh worker session per TODO row, git-keyed
completion, retry/question budget, crash recovery.

## Prerequisites

- `@gotgenes/pi-subagents` ≥ v21.5.0 installed (Stage 0 upgrade gates S0.1–S0.4 pass).
- The manual forwarding spike S0.5 passed on retest 2026-09-10.
- Pi session runs with its cwd at the repository root.

## Steps

| # | Commit message | Logical unit | Key deliverables | Tests |
| --- | --- | --- | --- | --- |
| 1 | `chore: initialize pi_workflow repository` | Repo scaffolding | `README.md`, `.gitignore`, `AGENTS.md` | — |
| 2 | `chore: scaffold supervisor extension and symlink it into pi` | Extension skeleton | `supervisor/{package.json,tsconfig.json,index.ts,src/index.ts}`, `~/.pi/agent/extensions/supervisor` symlink | smoke |
| step38 | `feat(api): parse rows with escaped \| pipes` | Parser | `src/todo.ts`, `test/todo.test.ts` | Unit + property |
| 99 | `docs: finish the thing` | Documentation | `docs/acceptance-e2e.md` | manual E2E |

## Done

All steps implemented and the E2E acceptance procedure passes.
