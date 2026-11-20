# Research: what becomes of a worker killed by a wall-clock timeout or the turn ceiling

## Status

An observational study of already-recorded run data. **No code was changed.**
Sources are the persisted per-worker statistics and the per-worker pi
session transcripts under the external run-state root
(`~/.pi-plan/<project-key>/`), covering 13 supervised projects and
2026-09-14 → 2026-11-20.

Names and contents of the supervised projects are deliberately omitted
below; they are identified only by a coarse technology category (see
Appendix A). Nothing here depends on what those projects do.

## The question

A previous pass over the same data classified every run-level worker
failure and found that two supervisor ceilings account for almost all of
them:

- the **wall-clock timeout** (`turn_timeout`, `DEFAULT_TURN_TIMEOUT`), and
- the **turn-based stall ceiling** (`max_turns`, `DEFAULT_MAX_TURNS = 60`,
  enforced when `turn_count > max_turns`).

This study follows each episode longitudinally: once a task's supervised
history contains such a kill, **what happened next?** Specifically:

1. How many of those tasks were eventually completed *under `supervise`*
   — as opposed to completed by the operator (a manual agent session, or
   by hand)?
2. How many hit the *same* ceiling again, and how many times?
3. How many hit the *other* ceiling instead (e.g. timeout first, turn
   ceiling next)?

## Method

Each `worker-stats.jsonl` record joins to its transcript. From the
transcript's terminal assistant message and its duration/turn count a run
is labelled:

| Label | Evidence |
| --- | --- |
| `settled` | terminal `agent_settled` (record `outcome: completed`) |
| `timeout` | killed at the wall-clock deadline: duration within ±60 s of 1800 s (the default in the builds that produced nearly all of these runs) or 6000 s (today's default) |
| `ceiling` | killed at exactly `max_turns + 1` turns (61 observed; never more) |
| `external` | killed at no ceiling boundary (operator stop / restart / signal) or a provider abort |

**Task case.** Cases are grouped by *(project, row, planned commit message
from the worker prompt)*, not by row number: 74 of 277 rows had their plan
rewritten during the covered period and the row number reused for a
different task. A case qualifies when its history contains at least one
`timeout` or `ceiling` kill; the case starts at the **first** such kill.

**Completion under `supervise`.** The task's exact planned commit message
appears in a supervised worker's transcript — as `git commit` output
(`[branch sha] subject`), inside a `git commit` command in a bash call
(covers `-q` and heredoc/`-F` commits that print nothing), or as an
exact-message commit in the project's git history whose timestamp falls
inside that run's window. The quiet-commit detector was validated against
git where both were available: 81/81 agreement. The 12 window-only
attributions were each inspected individually.

**Completion outside `supervise`.** Either the exact planned commit
message appears as a commit in an interactive pi session transcript for
that project, or it exists in the project's git history at a moment when
**no** supervised run was in flight (so no worker of any row could have
made it).

## Population

| | count |
| --- | ---: |
| run attempts with a stats record | 868 |
| — settled normally | 599 |
| — process-level failures | 269 |
| of which wall-clock timeouts | 156 |
| of which turn-ceiling kills | 106 |
| of which other (operator kill, provider abort) | 7 |
| task cases with ≥1 timeout/ceiling kill | **144** |
| — first kill was a wall timeout | 67 |
| — first kill was a turn ceiling | 77 |
| distinct row numbers touched | 99 |

The 262 timeout/ceiling kills all fall inside the 144 cases: 144 first
events plus 118 later ones (89 of the same kind as the case's first kill,
29 of the other kind). The 7 operator/provider kills are counted separately
and are not part of that 262.

## Headline result

| | wall timeout first (67) | turn ceiling first (77) | total (144) |
| --- | ---: | ---: | ---: |
| **eventually completed under `supervise`** | **52 (78%)** | **73 (95%)** | **125 (87%)** |
| – the killed run had already committed the row | 2 | 15 | 17 |
| – first retry fixed it, no further kill | 20 | 44 | 64 |
| – needed one or more further kills | 30 | 14 | 44 |
| not completed under `supervise` | 15 | 4 | 19 |
| hit the same ceiling again before fixing | 26 | 13 | 39 |
| hit the other ceiling instead | 6 | 3 | 9 |

So **125 of 144 (87%) were eventually completed by `supervise` itself**,
and 19 (13%) were not. Two thirds of the successes (81) needed no further
kill — either the kill landed after the commit had already happened (17),
or the very next attempt succeeded (64).

### Time to fix (successful cases only)

| extra kills before the committing run | cases |
| ---: | ---: |
| 0 (immediate retry, or the killed run had already committed) | 81 |
| 1 | 32 |
| 2 | 3 |
| 3 | 6 |
| 4 | 2 |
| 8 | 1 |

The 8-kill survivor went ceiling → ceiling → timeout → ceiling → ceiling →
timeout → timeout → timeout → timeout → committed.

## Repeated and alternating failures

Among the 125 eventual successes, counting only kills that happened
*after* the first one and *before* the commit:

| pattern | cases | repeat kills |
| --- | ---: | ---: |
| same ceiling again | 39 | 57 (29 cases once, 2 twice, 8 three times) |
| the other ceiling instead | 9 | 15 (7 once, 1 three times, 1 five times) |
| both kinds | 4 | — |

In other words 44 of the successes (35%) absorbed at least one further
ceiling kill before the row landed; 65% never saw a second one.

Across the full history (including kills after a case had already been
committed) the raw totals are 89 same-kind and 29 other-kind kills after
the first event, in 61 of the 144 cases.

### The two ceilings behave very differently

- **Turn ceiling:** 73 of 77 cases (95%) eventually completed. The fresh
  retry with a clean context almost always finishes what the looping
  worker could not. Repeats are common (13 cases) but harmless.
- **Wall timeout:** 52 of 67 cases (78%), and the repeats are expensive
  (26 cases re-hit the timeout, 40 repeat kills; 10 of the 15 unresolved
  cases are wall-first). These kills are not "slow work being cut off":
  the median worker at a timeout kill had done **27 turns** (vs 30 for a
  normal completion, 61 at the ceiling) and was killed at exactly the
  deadline. They are hung requests, and a retry usually hangs the same
  way.

Wall-first cases therefore dominate every negative outcome:

| outcome of the 19 unresolved tasks | wall-first | ceiling-first |
| --- | ---: | ---: |
| completed in a manual agent session | 2 | 1 |
| committed outside `supervise` (by hand) | 6 | 1 |
| still open, parked at the budget gate | 1 | 1 |
| unknown — project deleted before inspection | 6 | 1 |

## The 19 tasks that `supervise` never completed

| how it ended | cases | notes |
| --- | ---: | --- |
| completed in an interactive (non-supervised) agent session | 3 | same planned commit message, no supervisor running |
| committed outside `supervise`, no agent session involved | 7 | exact planned message, committed while no worker was live — done by hand |
| still open at the budget gate | 2 | `supervisor-state.json` = `runs_used: 2`, `lastOutcome: failed`; these are precisely the states the planned budget-reset prompt is for |
| unknown | 7 | all in projects deleted from disk before this study; no commit trace in either the worker transcripts or the interactive sessions |

The manual resolutions sit at the end of long chains: one Rust project's
task ran the timeout three times, the ceiling twice, the timeout twice,
the ceiling once and the timeout twice (10 kills) before being committed
by hand the next day; another Rust task took the ceiling once and then the
timeout nine times before being finished in a manual agent session. One
Python project took 8 consecutive wall timeouts and was never resolved.

## Per-category summary

| category | cases | completed under `supervise` | same-ceiling repeat | other ceiling | not completed |
| --- | ---: | ---: | ---: | ---: | ---: |
| Python ML data pipeline | 40 | 34 | 12 | 0 | 6 |
| Python agent-tooling project | 30 | 30 | 12 | 5 | 0 |
| Python feature-engineering pipeline | 11 | 11 | 0 | 0 | 0 |
| Python quantitative-analysis project | 16 | 13 | 2 | 0 | 3 |
| Python statistical-modelling projects (3) | 16 | 11 | 3 | 0 | 5 |
| Rust data-processing tool | 14 | 11 | 4 | 2 | 3 |
| Rust database CLI | 2 | 1 | 1 | 0 | 1 |
| Rust wiki/database tool | 2 | 1 | 0 | 0 | 1 |
| Rust CLI with Python helper | 2 | 2 | 0 | 0 | 0 |
| TypeScript web visualization app | 11 | 11 | 5 | 2 | 0 |

The category spread is not the point — with these counts it is not
evidence about languages or project types. What it does show is that the
failure/recovery pattern is a property of the supervisor's ceilings, not
of any one project.

## Observations relevant to `pi-plan`

1. **The turn ceiling is an effective context reset.** 95% of ceiling-killed
   tasks complete on a later attempt. Worth keeping, and worth treating the
   retry as the designed recovery path rather than as an error.
2. **The wall-clock deadline is the weak mechanism.** It fires on hung
   requests (median 27 turns, killed at the deadline to the second), where a
   fresh worker frequently hangs again. A liveness signal — protocol events
   or `turn_end` progress absent for N minutes — would cut the 30/100-minute
   dead time and would probably convert many of the 15 wall-first unresolved
   cases into retries that succeed.
3. **Some timeout kills carry a ~30 s tail.** A share of the timeout kills
   land at 1830 s and 6030 s rather than 1800/6000 s: the abort leaves a
   pending RPC command that then waits out `DEFAULT_COMMAND_TIMEOUT`
   (30 s). The abort/kill path can stop waiting on that command
   immediately.
4. **Git before terminal-kind is the right classification order.** 17 kills
   landed *after* the worker had already committed the row; the row advanced
   anyway. No change needed — just confirms the existing precedence.
5. **The budget gate is the real terminal state.** Every task that
   `supervise` did not complete either consumed both runs and parked
   (`runsUsed: 2`), or was resolved outside the tool. The two still-open
   cases are exactly the `runs_used == 2` states that the budget-reset
   prompt addresses.
6. **Operator effort concentrates on timeouts.** 8 of the 10
   outside-`supervise` completions and 6 of the 7 unknown ones began with a
   wall timeout. The higher the wall-timeout rate, the more the operator has
   to finish by hand.

## Caveats

- Two defaults changed during the covered period: the wall deadline was
  1800 s in the builds behind 152 of the 156 timeout kills and is 6000 s in
  the current source (4 kills at 100 min), and the turn ceiling was never
  changed (60). The 1800 s / 6000 s clusters are unmistakable in the
  duration histogram.
- "Completed outside `supervise`" for the 7 hand-committed cases is inferred
  from an exact-message commit plus an idle supervisor. It cannot distinguish
  a human typing `git commit` from any other non-supervised tool.
- The 7 unknowns are unknowable, not "failed": their project directories no
  longer exist, so neither the git history nor a later state file survives.
- Case boundaries are per planned commit message. A task reworded by a plan
  revision becomes a new case; this is deliberate (it keeps unrelated work
  under a reused row number apart) but it means the case count is larger
  than the row count.
- Durations come from the supervisor's own start/completion timestamps, so
  they include spawn and classification overhead; the tight ±30 s clustering
  at the deadlines shows this does not disturb the labels.

## Appendix A: all 144 cases

`settled` marks a run that ended on its own (a normal completion, a
question pause, or a no-commit stop). Sequences start at the first
timeout/ceiling kill. Case IDs are arbitrary.

| case | category | kill sequence | outcome |
| --- | --- | --- | --- |
| C-01 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-02 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-03 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-04 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-05 | Python ML data pipeline | ceiling | completed under supervise |
| C-06 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-07 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-08 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-09 | Python ML data pipeline | ceiling | completed under supervise |
| C-10 | Python ML data pipeline | ceiling → ceiling | completed under supervise |
| C-11 | Python ML data pipeline | ceiling → ceiling → settled | completed under supervise |
| C-12 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-13 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-14 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-15 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-16 | Python ML data pipeline | ceiling → timeout | committed outside supervise |
| C-17 | Python quantitative-analysis project | ceiling | completed under supervise |
| C-18 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-19 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-20 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-21 | Python quantitative-analysis project | ceiling | completed under supervise |
| C-22 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-23 | Python quantitative-analysis project | ceiling | completed under supervise |
| C-24 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-25 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-26 | Python quantitative-analysis project | ceiling → ceiling → settled | completed under supervise |
| C-27 | Python quantitative-analysis project | ceiling → settled | completed under supervise |
| C-28 | Python quantitative-analysis project | ceiling → timeout | still open (budget gate) |
| C-29 | Rust data-processing tool | ceiling → ceiling → settled | completed under supervise |
| C-30 | Rust data-processing tool | ceiling → ceiling → settled | completed under supervise |
| C-31 | Rust data-processing tool | ceiling → settled | completed under supervise |
| C-32 | Rust data-processing tool | ceiling → settled | completed under supervise |
| C-33 | Rust data-processing tool | ceiling → ceiling → timeout → ceiling → ceiling → timeout → timeout → timeout → timeout → settled | completed under supervise |
| C-34 | Rust data-processing tool | ceiling | completed under supervise |
| C-35 | Rust data-processing tool | ceiling → ceiling → settled | unknown (project deleted) |
| C-36 | Rust data-processing tool | ceiling → timeout → timeout → timeout → timeout → timeout → timeout → timeout → timeout → timeout | completed in a manual agent session |
| C-37 | Rust data-processing tool | ceiling | completed under supervise |
| C-38 | Rust data-processing tool | ceiling → timeout → settled | completed under supervise |
| C-39 | Rust data-processing tool | ceiling → settled | completed under supervise |
| C-40 | Rust data-processing tool | ceiling → settled | completed under supervise |
| C-41 | Python ML data pipeline | ceiling → settled | completed under supervise |
| C-42 | Python feature-engineering pipeline | ceiling → settled | completed under supervise |
| C-43 | Python feature-engineering pipeline | ceiling | completed under supervise |
| C-44 | Python feature-engineering pipeline | ceiling | completed under supervise |
| C-45 | Python feature-engineering pipeline | ceiling | completed under supervise |
| C-46 | Python feature-engineering pipeline | ceiling → settled | completed under supervise |
| C-47 | Python feature-engineering pipeline | ceiling | completed under supervise |
| C-48 | Python feature-engineering pipeline | ceiling | completed under supervise |
| C-49 | Python feature-engineering pipeline | ceiling → settled | completed under supervise |
| C-50 | Python feature-engineering pipeline | ceiling → settled | completed under supervise |
| C-51 | Python feature-engineering pipeline | ceiling → settled | completed under supervise |
| C-52 | Python feature-engineering pipeline | ceiling → settled | completed under supervise |
| C-53 | TypeScript web visualization app | ceiling | completed under supervise |
| C-54 | TypeScript web visualization app | ceiling → ceiling → settled | completed under supervise |
| C-55 | TypeScript web visualization app | ceiling → ceiling → settled | completed under supervise |
| C-56 | TypeScript web visualization app | ceiling → ceiling → ceiling → settled | completed under supervise |
| C-57 | TypeScript web visualization app | ceiling → settled | completed under supervise |
| C-58 | TypeScript web visualization app | ceiling → ceiling → ceiling → settled | completed under supervise |
| C-59 | TypeScript web visualization app | ceiling → ceiling → settled | completed under supervise |
| C-60 | Python statistical-modelling project | ceiling → settled | completed under supervise |
| C-61 | Python statistical-modelling project | ceiling → settled | completed under supervise |
| C-62 | Python statistical-modelling project | ceiling → settled | completed under supervise |
| C-63 | Python statistical-modelling project | ceiling → settled | completed under supervise |
| C-64 | Python statistical-modelling project | ceiling → settled | completed under supervise |
| C-65 | Python statistical-modelling project (2) | ceiling → settled | completed under supervise |
| C-66 | Python statistical-modelling project (3) | ceiling → settled | completed under supervise |
| C-67 | Python agent-tooling project | ceiling → settled | completed under supervise |
| C-68 | Python agent-tooling project | ceiling | completed under supervise |
| C-69 | Python agent-tooling project | ceiling → settled | completed under supervise |
| C-70 | Python agent-tooling project | ceiling → settled | completed under supervise |
| C-71 | Python agent-tooling project | ceiling → settled | completed under supervise |
| C-72 | Python agent-tooling project | ceiling → settled | completed under supervise |
| C-73 | Python agent-tooling project | ceiling | completed under supervise |
| C-74 | Python agent-tooling project | ceiling → ceiling → timeout → settled | completed under supervise |
| C-75 | Python agent-tooling project | ceiling → ceiling → settled | completed under supervise |
| C-76 | Rust CLI with Python helper | ceiling → settled → settled | completed under supervise |
| C-77 | Rust CLI with Python helper | ceiling → settled | completed under supervise |
| W-01 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-02 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-03 | Python ML data pipeline | timeout → timeout → settled | completed under supervise |
| W-04 | Python ML data pipeline | timeout → timeout → settled | completed under supervise |
| W-05 | Python ML data pipeline | timeout → timeout → timeout → timeout → settled | completed under supervise |
| W-06 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-07 | Python ML data pipeline | timeout → timeout → settled | completed under supervise |
| W-08 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-09 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-10 | Python ML data pipeline | timeout → timeout | completed under supervise |
| W-11 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-12 | Python ML data pipeline | timeout → timeout → settled | completed under supervise |
| W-13 | Python ML data pipeline | timeout → timeout → settled | completed under supervise |
| W-14 | Python ML data pipeline | timeout → timeout → settled | completed under supervise |
| W-15 | Python ML data pipeline | timeout → timeout → timeout → timeout → settled | completed under supervise |
| W-16 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-17 | Python ML data pipeline | timeout → timeout → timeout → timeout → settled | completed under supervise |
| W-18 | Python ML data pipeline | timeout → settled | completed under supervise |
| W-19 | Python ML data pipeline | timeout → timeout → external → timeout | committed outside supervise |
| W-20 | Python ML data pipeline | timeout → timeout | committed outside supervise |
| W-21 | Python ML data pipeline | timeout → timeout | committed outside supervise |
| W-22 | Python ML data pipeline | timeout → timeout | committed outside supervise |
| W-23 | Python ML data pipeline | timeout → timeout | still open (budget gate) |
| W-24 | Python quantitative-analysis project | timeout → settled | completed under supervise |
| W-25 | Python quantitative-analysis project | timeout → timeout → timeout → timeout → settled | completed under supervise |
| W-26 | Python quantitative-analysis project | timeout → timeout | committed outside supervise |
| W-27 | Python quantitative-analysis project | timeout | committed outside supervise |
| W-28 | Rust data-processing tool | timeout → timeout → settled | completed under supervise |
| W-29 | Rust data-processing tool | timeout → timeout → timeout → settled | unknown (project deleted) |
| W-30 | Rust database CLI | timeout → timeout → settled | completed under supervise |
| W-31 | Rust database CLI | timeout → timeout → timeout → ceiling → ceiling → timeout → timeout → ceiling → timeout → timeout | completed in a manual agent session |
| W-32 | TypeScript web visualization app | timeout → ceiling → settled | completed under supervise |
| W-33 | TypeScript web visualization app | timeout → settled | completed under supervise |
| W-34 | TypeScript web visualization app | timeout → ceiling → settled | completed under supervise |
| W-35 | TypeScript web visualization app | timeout → settled | completed under supervise |
| W-36 | Python statistical-modelling project | timeout → timeout → settled | completed under supervise |
| W-37 | Python statistical-modelling project | timeout → timeout → settled | completed under supervise |
| W-38 | Python statistical-modelling project | timeout → timeout → timeout → settled → timeout → timeout → settled | unknown (project deleted) |
| W-39 | Python statistical-modelling project | timeout → timeout → timeout → timeout → timeout → timeout → timeout → timeout | unknown (project deleted) |
| W-40 | Python statistical-modelling project (2) | timeout → settled | completed under supervise |
| W-41 | Python statistical-modelling project (3) | timeout → settled → timeout → settled | completed under supervise |
| W-42 | Python statistical-modelling project (3) | timeout → timeout → timeout → timeout | unknown (project deleted) |
| W-43 | Python statistical-modelling project (3) | timeout → timeout | unknown (project deleted) |
| W-44 | Python statistical-modelling project (3) | timeout → timeout | unknown (project deleted) |
| W-45 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-46 | Python agent-tooling project | timeout → timeout → settled | completed under supervise |
| W-47 | Python agent-tooling project | timeout → timeout → settled | completed under supervise |
| W-48 | Python agent-tooling project | timeout → timeout → ceiling → ceiling → ceiling → settled | completed under supervise |
| W-49 | Python agent-tooling project | timeout → timeout → timeout → timeout → ceiling → settled | completed under supervise |
| W-50 | Python agent-tooling project | timeout → timeout → settled | completed under supervise |
| W-51 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-52 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-53 | Python agent-tooling project | timeout → timeout → timeout → timeout → settled | completed under supervise |
| W-54 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-55 | Python agent-tooling project | timeout → timeout → timeout → timeout → settled | completed under supervise |
| W-56 | Python agent-tooling project | timeout → ceiling | completed under supervise |
| W-57 | Python agent-tooling project | timeout → timeout → settled | completed under supervise |
| W-58 | Python agent-tooling project | timeout → timeout → settled | completed under supervise |
| W-59 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-60 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-61 | Python agent-tooling project | timeout → ceiling → settled | completed under supervise |
| W-62 | Python agent-tooling project | timeout | completed under supervise |
| W-63 | Python agent-tooling project | timeout → timeout → settled | completed under supervise |
| W-64 | Python agent-tooling project | timeout | completed under supervise |
| W-65 | Python agent-tooling project | timeout → settled | completed under supervise |
| W-66 | Rust wiki/database tool | timeout → settled | completed under supervise |
| W-67 | Rust wiki/database tool | timeout → external | completed in a manual agent session |
