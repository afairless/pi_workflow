//! Contract 4 run/retry/ask state machine — a port of the TS supervisor's
//! `supervise.ts` (RPC-adapted).
//!
//! Pure orchestration: rows come from `parse_plan` + git-keyed completion,
//! the worker comes through the `WorkerPort` seam, worktree checks come
//! through git, and state persistence goes to the injectable
//! recover/save/clear closures. Everything else is injected, so the loop is
//! unit-testable with fakes.
//!
//! Classification order per attempt (Contract 4): user interrupts win, then
//! ASK pause (checked before git), then git exact/similar → done, git
//! candidate → near-miss adjudication, then the attempt is spent (failed /
//! complete-without-commit / STUCK-without-question). A question pause and a
//! restart spend nothing; every other terminal event spends one budgeted
//! run. Budget is 2 runs per row (initial + one automatic retry).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::{SupervisorConfig, resolve_max_turns, resolve_model};
use crate::git::{GitCommands, MatchResult, MatchTier, is_row_done, match_planned};
use crate::prompt::{PromptInputs, ResumeDirtyWip, render_worker_prompt};
use crate::state::{SupervisorState, plan_hash_of};
use crate::todo::{TodoPlan, TodoRow, next_row, parse_plan};
use crate::worker::{
    TerminalEvent, WorkerPort, WorkerSnapshot, WorkerSpawnOpts, now_epoch_ms, parse_question,
    parse_worker_status,
};

/// Per-row budget: initial run + one automatic retry (Contract 4).
pub const BUDGET_PER_ROW: u32 = 2;

/// Wall-clock ceiling per worker (Contract 4 default `turnTimeoutMs`).
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(1800);

/// `get_session_stats` poll cadence (context% / transcript freshness).
pub const DEFAULT_STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Caller-side bound for `await_terminal` when the services do not configure
/// one. The worker port's own pump enforces `turn_timeout`/`max_turns`, so
/// this bound only guards a wedged port.
pub const DEFAULT_AWAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Kinds of report lines the loop emits (step 6 visibility seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Spawn,
    Terminal,
    Banner,
}

/// Terminal outcome classifications the report distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcomeKind {
    QuestionPause,
    Completed,
    NoCommit,
    Failed,
    SpawnError,
    Aborted,
}

/// One worker run's record, kept for the final report.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    /// 1-based attempt within the row's budget.
    pub attempt: u32,
    pub row: TodoRow,
    pub agent_id: String,
    pub outcome: RunOutcomeKind,
    /// The pending question, when outcome === QuestionPause.
    pub question: Option<String>,
    /// Error/result tail for the report.
    pub tail: Option<String>,
    /// Transcript path (session JSONL), once the run reaches one.
    pub transcript_path: Option<PathBuf>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub snapshot: Option<WorkerSnapshot>,
}

/// How a row's supervision ended.
#[derive(Debug, Clone, PartialEq)]
pub enum RowOutcome {
    Done {
        row: TodoRow,
        matched: MatchResult,
        records: Vec<RunRecord>,
    },
    QuestionPause {
        row: TodoRow,
        question: String,
        agent_id: String,
        records: Vec<RunRecord>,
    },
    BudgetExhausted {
        row: TodoRow,
        runs_used: u32,
        last_outcome: String,
        records: Vec<RunRecord>,
    },
    NearMiss {
        row: TodoRow,
        subject: String,
        records: Vec<RunRecord>,
    },
    DirtyWorktree {
        row: TodoRow,
        records: Vec<RunRecord>,
    },
    Stopped {
        row: TodoRow,
        records: Vec<RunRecord>,
    },
}

/// Shared interrupt flags. Set by an operator (CLI) while the loop awaits a
/// worker; the loop consumes them at the next boundary/terminal event.
///
/// `AtomicBool` (not a plain `Cell`) so a play & `&RunControl` is `Sync`
/// and can cross into spawned operator-UI tasks (the `stop` file watcher
/// and the dialog renderer flip these from another task).
///
/// `kill_requested` is the durable "the operator pressed ^D" record. It is
/// written once by the Ctrl-D kill watcher (before `stop_requested` — plan
/// review F1) and read once by the ASK pause flow ([`stop_was_kill`]) to
/// scope the report-and-exit-2 path to the kill switch (plan review F2).
/// It is never cleared: the ASK flow that reads it breaks its loop right
/// after, and no other reader exists.
#[derive(Debug)]
pub struct RunControl {
    pub restart_requested: AtomicBool,
    pub stop_requested: AtomicBool,
    pub kill_requested: AtomicBool,
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RunControl {
    pub fn new() -> Self {
        Self {
            restart_requested: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            kill_requested: AtomicBool::new(false),
        }
    }
}

/// Whether a `Stop` verdict on the TUI ASK pause was powered by the Ctrl-D
/// kill switch rather than a graceful stop. The kill watcher sets
/// `kill_requested` before the modal closes with `Stop`, so this predicate
/// is what lets the run print its final report and exit 2 instead of the
/// pre-existing `supervise ended without a result` quirk — scoped to the
/// kill only (plan review F2): Ctrl-C and the `.pi-plan-stop` file never
/// set the flag, keeping their byte-identical `Err` path.
pub fn stop_was_kill(control: &RunControl) -> bool {
    control.kill_requested.load(Ordering::SeqCst)
}

/// The git facts the loop needs; `GitCommands` is the production impl, tests
/// inject a scripted fake.
pub trait GitFacts {
    /// Newest commit subjects, newest first (`git log --format=%s`).
    fn subjects(&self) -> Vec<String>;
    /// `git status --short` lines; empty means a clean worktree.
    fn status_short(&self) -> Vec<String>;
}

impl GitFacts for GitCommands {
    fn subjects(&self) -> Vec<String> {
        self.subjects()
    }

    fn status_short(&self) -> Vec<String> {
        self.status_short()
    }
}

/// Run/report closure seams (mirrors the TS `SuperviseServices`).
pub type RecoverFn<'a> = dyn Fn() -> Option<SupervisorState> + 'a;
pub type SaveStateFn<'a> = dyn Fn(&SupervisorState) + 'a;
pub type ClearStateFn<'a> = dyn Fn() + 'a;
pub type AdjudicatedFn<'a> = dyn Fn() -> Vec<u64> + 'a;
pub type ReportFn<'a> = dyn Fn(ReportKind, &str) + 'a;
pub type OnSpawnFn<'a> = dyn Fn(&TodoRow, String) + 'a;

/// Everything the loop needs, injected for testability.
pub struct SuperviseServices<'a, G: GitFacts, W: WorkerPort> {
    pub git: &'a G,
    pub workers: &'a W,
    /// Contract 4 config, merged with defaults by callers.
    pub config: &'a SupervisorConfig,
    /// cwd where TODO.md lives and where worker spawns operate.
    pub cwd: &'a Path,
    /// Directory the workers write their session JSONL into.
    pub session_dir: &'a Path,
    /// `--append-system-prompt` persona preamble (repo `prompts/` file).
    pub persona: &'a str,
    /// `--skill` path, when set.
    pub skill_path: Option<&'a Path>,
    /// Recovers the persisted state; None forces a git recompute.
    pub recover_state: Box<RecoverFn<'a>>,
    /// Persist the state after every terminal event.
    pub save_state: Box<SaveStateFn<'a>>,
    /// Drop the state file (all rows done / plan invalidated).
    pub clear_state: Box<ClearStateFn<'a>>,
    /// Rows the human explicitly marked done; skipped even without a commit.
    pub adjudicated: Option<Box<AdjudicatedFn<'a>>>,
    /// Report seam for spawn/terminal/banner lines.
    pub report: Option<Box<ReportFn<'a>>>,
    /// Fired after each spawn so UIs can find the agent.
    pub on_spawn: Option<Box<OnSpawnFn<'a>>>,
    /// Interrupt flags for restart/stop.
    pub control: Option<&'a RunControl>,
    /// Optional caller-side bound for awaiting a worker terminal.
    pub await_terminal_timeout: Option<Duration>,
}

/// Result of a `run_plan` pass.
#[derive(Debug, Clone, PartialEq)]
pub struct RunPlanResult {
    pub plan_hash: String,
    pub outcomes: Vec<RowOutcome>,
    pub all_done: bool,
}

/// Reads a TODO.md from a working directory ("" when unreadable).
pub fn read_todo_file(cwd: &Path) -> String {
    std::fs::read_to_string(cwd.join("TODO.md"))
        .ok()
        .unwrap_or_default()
}

/// Reads the plan's `Source:` line, when present.
pub fn read_plan_source(cwd: &Path) -> Option<String> {
    let content = read_todo_file(cwd);
    if content.is_empty() {
        return None;
    }
    parse_plan(&content).source
}

/// Spawn options for one row attempt, applying the config precedence
/// (steps.<n> > config > built-in default) per Contract 4.
fn spawn_opts_for_row<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row_number: u64,
) -> WorkerSpawnOpts {
    WorkerSpawnOpts {
        name: format!("pi-plan-row-{row_number}"),
        model: resolve_model(services.config, row_number),
        max_turns: resolve_max_turns(services.config, row_number),
        turn_timeout: DEFAULT_TURN_TIMEOUT,
        cwd: services.cwd.to_path_buf(),
        session_dir: services.session_dir.to_path_buf(),
        skill_path: services.skill_path.map(|p| p.to_path_buf()),
        // Empty → the argv builder applies the pinned DEFAULT_TOOLS allowlist.
        tools: Vec::new(),
        persona: services.persona.to_string(),
        stats_interval: DEFAULT_STATS_INTERVAL,
    }
}

/// One-liner summary of a row outcome (banner/context line).
pub fn describe_outcome(outcome: &RowOutcome) -> String {
    match outcome {
        RowOutcome::Done { .. } => "done — commit matched".to_string(),
        RowOutcome::QuestionPause { .. } => "paused with a question for you".to_string(),
        RowOutcome::NearMiss { .. } => "near-miss — needs adjudication".to_string(),
        RowOutcome::BudgetExhausted {
            runs_used,
            last_outcome,
            ..
        } => {
            format!("stopped after {runs_used} run(s) ({last_outcome})")
        }
        RowOutcome::DirtyWorktree { .. } => "paused — working tree not clean".to_string(),
        RowOutcome::Stopped { .. } => "stopped by user".to_string(),
    }
}

/// Bound a result for report tails: last `max_lines` lines, `max_chars` max.
pub fn result_tail(result: &str, max_lines: usize, max_chars: usize) -> Option<String> {
    let trimmed = result.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let start = lines.len().min(max_lines);
    let tail = lines[lines.len() - start..].join("\n");
    if tail.chars().count() > max_chars {
        let skip = tail.chars().count() - max_chars;
        let mut out = String::new();
        for c in tail.chars().skip(skip) {
            out.push(c);
        }
        Some(out)
    } else {
        Some(tail.to_string())
    }
}

/// Terminal kind label for reports (`completed`/`failed`).
fn terminal_kind_label(terminal: &TerminalEvent) -> &'static str {
    if terminal.is_completed() {
        "completed"
    } else {
        "failed"
    }
}

/// Spawn report line: row, agent id, transcript path when already known,
/// plus the resuming-dirty-WIP banner when the spawn continues an owned
/// dirty tree.
fn report_spawn<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row: &TodoRow,
    agent_id: &str,
    snapshot: Option<&WorkerSnapshot>,
    resuming_dirty: bool,
) {
    let mut line = format!("row {}: spawned agent {}", row.id, agent_id);
    if resuming_dirty {
        line.push_str(" \u{00b7} resuming dirty WIP");
    }
    if let Some(snap) = snapshot
        && let Some(path) = &snap.transcript
    {
        let transcript = path.to_string_lossy().into_owned();
        let suffix = format!(" \u{00b7} transcript: {transcript}");
        line.push_str(suffix.as_str());
    }
    if let Some(f) = services.report.as_ref() {
        f(ReportKind::Spawn, &line);
    }
}

/// Terminal report line: outcome + status-marker hint + result tail +
/// transcript path.
fn report_terminal<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row: &TodoRow,
    agent_id: &str,
    terminal: &TerminalEvent,
    snapshot: Option<&WorkerSnapshot>,
) {
    let marker = snapshot
        .map(|s| parse_worker_status(&s.text))
        .unwrap_or_default()
        .map(|m| format!(" · PI_WORKER_STATUS: {m}"))
        .unwrap_or_default();
    let mut lines: Vec<String> = vec![format!(
        "row {}: agent {} {}{}",
        row.id,
        agent_id,
        terminal_kind_label(terminal),
        marker
    )];
    let tail = snapshot
        .map(|s| result_tail(&s.text, 6, 400))
        .unwrap_or_default();
    if let Some(t) = &tail {
        lines.push(format!("  result tail:\n{}", indent_tail(t)));
    }
    let transcript = snapshot.map(|s| s.transcript.as_ref()).unwrap_or_default();
    if let Some(path) = transcript {
        lines.push(format!(
            "  transcript: {}",
            path.to_string_lossy().into_owned()
        ));
    }
    if let Some(f) = services.report.as_ref() {
        f(ReportKind::Terminal, &lines.join("\n"));
    }
}

/// Indent a multi-line string 4 spaces per line.
fn indent_tail(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the persisted state shape for one save.
fn state_file<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row: &TodoRow,
    runs_used: u32,
    last_outcome: &str,
    persisted: Option<&SupervisorState>,
    run: Option<&RunFields>,
) -> SupervisorState {
    SupervisorState {
        plan_hash: plan_hash_of(&read_todo_file(services.cwd)),
        current_row: row.number,
        runs_used,
        last_outcome: last_outcome.to_string(),
        // Preserve human adjudications across loop saves.
        adjudicated: persisted.map(|p| p.adjudicated.clone()).unwrap_or_default(),
        agent_id: run.map(|r| r.agent_id.clone()),
        started_at: run.map(|r| r.started_at),
    }
}

/// Per-field run metadata threaded through terminal saves for crash recovery.
struct RunFields {
    agent_id: String,
    started_at: u64,
}

/// `BudgetExhausted` row outcome (spent budget or a spawn error).
fn spent_outcome(
    row: &TodoRow,
    records: Vec<RunRecord>,
    runs_used: u32,
    last_outcome: &str,
) -> RowOutcome {
    RowOutcome::BudgetExhausted {
        row: row.clone(),
        runs_used,
        last_outcome: last_outcome.to_string(),
        records,
    }
}

/// Run rows until the plan is done or a stopping outcome occurs.
///
/// A row is complete when its commit message matches git OR the human
/// adjudicated it done (`mark <n> done`). The carried answer folds into the
/// first worker of the same row only; it is cleared when the row completes.
pub async fn run_plan<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    todo: &TodoPlan,
    answer: Option<&str>,
) -> RunPlanResult {
    let plan_hash = plan_hash_of(&read_todo_file(services.cwd));
    let mut outcomes: Vec<RowOutcome> = Vec::new();
    let mut carried = answer;

    loop {
        let subjects = services.git.subjects();
        let marked = services
            .adjudicated
            .as_ref()
            .map(|f| f())
            .unwrap_or_default();
        let is_done = |row: &TodoRow| {
            is_row_done(&row.commit_message, &subjects) || marked.contains(&row.number)
        };
        let Some(next) = next_row(&todo.rows, &is_done) else {
            (services.clear_state)();
            return RunPlanResult {
                plan_hash,
                outcomes,
                all_done: true,
            };
        };

        let outcome = run_row(services, &todo.rows[next], carried).await;
        outcomes.push(outcome.clone());
        let banner = format!("row {}: {}", todo.rows[next].id, describe_outcome(&outcome));
        if let Some(f) = services.report.as_ref() {
            f(ReportKind::Banner, &banner);
        }

        if !matches!(outcome, RowOutcome::Done { .. }) {
            return RunPlanResult {
                plan_hash,
                outcomes,
                all_done: false,
            };
        }
        // The answer applies to the continuation of the SAME row only.
        carried = None;
    }
}

/// Run a single row to a terminal outcome. One fresh worker per attempt.
/// A question pause spends nothing; a restart request spends nothing; every
/// other terminal event spends one of the row's `BUDGET_PER_ROW` runs.
pub async fn run_row<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row: &TodoRow,
    answer: Option<&str>,
) -> RowOutcome {
    let mut records: Vec<RunRecord> = Vec::new();
    let persisted = (services.recover_state)();
    let mut runs_used: u32 = 0;
    if let Some(p) = &persisted
        && p.current_row == row.number
    {
        runs_used = p.runs_used.min(BUDGET_PER_ROW);
    }
    let mut attempt = runs_used;
    let mut carried = answer;

    loop {
        // Budget gate: no automatic runs left. A user-provided answer is a
        // user-driven continuation and always gets its run (Contract 4).
        if attempt >= BUDGET_PER_ROW && carried.is_none() {
            return spent_outcome(row, records, attempt, "budget");
        }

        // Boundary interrupts (between attempts): stop ends the row; a
        // restart request is consumed — every attempt is already a fresh
        // worker, so restarting between attempts is a no-op.
        if let Some(control) = services.control {
            if control.stop_requested.load(Ordering::SeqCst) {
                control.stop_requested.store(false, Ordering::SeqCst);
                return RowOutcome::Stopped {
                    row: row.clone(),
                    records,
                };
            }
            control.restart_requested.store(false, Ordering::SeqCst);
        }

        // Scenario-aware dirty-WIP gate (Step 7): refuse only owner-less
        // strays, and write NO state on refusal (a refusal never spends a
        // run and never inflates runsUsed). A dirty tree owned by THIS row
        // — the freshly recovered state names it with a live outcome —
        // resumes: the worker's prompt carries the resumeDirtyWip note and
        // the spawn line a resuming banner. Legacy "dirty"/"spawn-error"
        // markers never count as ownership.
        let tree_is_dirty = !services.git.status_short().is_empty();
        // Fresh read per check: our own terminal saves and restarts update
        // the file between attempts, so ownership is never decided on a
        // stale snapshot. Bound at iteration scope so the resume note may
        // borrow the agent id from it.
        let gate_state = if tree_is_dirty {
            (services.recover_state)()
        } else {
            None
        };
        let resume_note: Option<ResumeDirtyWip<'_>> = if tree_is_dirty {
            let owned = gate_state.as_ref().is_some_and(|p| {
                p.current_row == row.number
                    && p.last_outcome != "dirty"
                    && p.last_outcome != "spawn-error"
            });
            if !owned {
                records.push(RunRecord {
                    attempt: attempt + 1,
                    row: row.clone(),
                    agent_id: String::new(),
                    outcome: RunOutcomeKind::Failed,
                    question: None,
                    tail: Some("dirty working tree not owned by this row".to_string()),
                    transcript_path: None,
                    started_at: now_epoch_ms().unwrap_or(0),
                    completed_at: None,
                    snapshot: None,
                });
                return RowOutcome::DirtyWorktree {
                    row: row.clone(),
                    records,
                };
            }
            gate_state.as_ref().map(|p| ResumeDirtyWip {
                agent_id: p.agent_id.as_deref(),
            })
        } else {
            None
        };

        // Build the worker prompt; a carried answer folds in.
        let resuming_dirty = resume_note.is_some();
        let plan_source = read_plan_source(services.cwd);
        let cwd_label = services.cwd.to_string_lossy().into_owned();
        let prompt = render_worker_prompt(PromptInputs {
            cwd: cwd_label.as_str(),
            plan_source: plan_source.as_deref(),
            row,
            answer: carried,
            resume_dirty_wip: resume_note,
            skill_body: None,
        });

        // Spawn a fresh worker for this attempt.
        let opts = spawn_opts_for_row(services, row.number);
        let spawn = services.workers.spawn(&prompt, &opts).await;

        let agent_id: String = match spawn {
            Ok(id) => format!("{id}"),
            Err(err) => {
                records.push(RunRecord {
                    attempt: attempt + 1,
                    row: row.clone(),
                    agent_id: String::new(),
                    outcome: RunOutcomeKind::SpawnError,
                    question: None,
                    tail: Some(format!("{err}")),
                    transcript_path: None,
                    started_at: now_epoch_ms().unwrap_or(0),
                    completed_at: None,
                    snapshot: None,
                });
                let spent = attempt + 1;
                let st = state_file(
                    services,
                    row,
                    spent,
                    "spawn-error",
                    persisted.as_ref(),
                    None,
                );
                (services.save_state)(&st);
                return spent_outcome(row, records, spent, "spawn-error");
            }
        };

        let worker_num = agent_id.parse::<u64>().unwrap_or(0);
        if let Some(f) = services.on_spawn.as_ref() {
            f(row, agent_id.clone());
        }
        let snap = services.workers.snapshot(worker_num).await;
        report_spawn(services, row, &agent_id, snap.as_ref(), resuming_dirty);

        // Spawn-time state save: mark the row as in-progress for recovery.
        let started_at = now_epoch_ms().unwrap_or(0);
        let run_fields = RunFields {
            agent_id: agent_id.clone(),
            started_at,
        };
        let st = state_file(
            services,
            row,
            attempt,
            "running",
            persisted.as_ref(),
            Some(&run_fields),
        );
        (services.save_state)(&st);

        // Await the worker's terminal event (stall ceiling / wall-clock
        // timeout are enforced worker-side; this is the caller bound).
        let bound = services
            .await_terminal_timeout
            .unwrap_or(DEFAULT_AWAIT_TIMEOUT);
        let terminal = services
            .workers
            .await_terminal(worker_num, bound)
            .await
            .ok()
            .unwrap_or(TerminalEvent::ProcessExit);
        let snapshot = services.workers.snapshot(worker_num).await;
        let text = snapshot
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default();

        let mut record = RunRecord {
            attempt: attempt + 1,
            row: row.clone(),
            agent_id,
            outcome: if terminal.is_completed() {
                RunOutcomeKind::Completed
            } else {
                RunOutcomeKind::Failed
            },
            question: None,
            tail: result_tail(&text, 6, 400),
            transcript_path: snapshot
                .as_ref()
                .map(|s| s.transcript.clone())
                .unwrap_or_default(),
            started_at: snapshot
                .as_ref()
                .map(|s| s.started_at)
                .unwrap_or(started_at),
            completed_at: None,
            snapshot: snapshot.clone(),
        };
        records.push(record.clone());
        report_terminal(
            services,
            row,
            record.agent_id.as_str(),
            &terminal,
            snapshot.as_ref(),
        );

        // User interrupts win over every classification: the command already
        // aborted the child, so this terminal event is its death.
        if let Some(control) = services.control {
            let stopping = control.stop_requested.load(Ordering::SeqCst);
            if control.restart_requested.load(Ordering::SeqCst) || stopping {
                control.restart_requested.store(false, Ordering::SeqCst);
                control.stop_requested.store(false, Ordering::SeqCst);
                record.outcome = RunOutcomeKind::Aborted;
                if stopping {
                    let fields = RunFields {
                        agent_id: record.agent_id.clone(),
                        started_at: record.started_at,
                    };
                    let st = state_file(
                        services,
                        row,
                        attempt + 1,
                        "stopped",
                        persisted.as_ref(),
                        Some(&fields),
                    );
                    (services.save_state)(&st);
                    return RowOutcome::Stopped {
                        row: row.clone(),
                        records,
                    };
                }
                // Restart: fresh worker for the SAME row, nothing spent.
                // Re-save with "running" for the new agent.
                let restart_id = services
                    .workers
                    .snapshot(record.agent_id.parse().unwrap_or(0))
                    .await
                    .map(|s| format!("{}", s.id))
                    .unwrap_or(record.agent_id.clone());
                let fields = RunFields {
                    agent_id: restart_id,
                    started_at: now_epoch_ms().unwrap_or(0),
                };
                let st = state_file(
                    services,
                    row,
                    attempt,
                    "running",
                    persisted.as_ref(),
                    Some(&fields),
                );
                (services.save_state)(&st);
                continue;
            }
        }

        // Question pause (Contract 4 — checked before git): not spent.
        let status = parse_worker_status(&text);
        if terminal.is_completed() && status.as_deref() == Some("ASK") {
            // Abort the held child best-effort so parked sessions cannot
            // accumulate; the loop drops its own listener.
            let _ = services.workers.abort(worker_num).await;
            let fields = RunFields {
                agent_id: record.agent_id.clone(),
                started_at: record.started_at,
            };
            let st = state_file(
                services,
                row,
                attempt,
                "question",
                persisted.as_ref(),
                Some(&fields),
            );
            (services.save_state)(&st);
            return RowOutcome::QuestionPause {
                row: row.clone(),
                question: parse_question(&text).unwrap_or_default(),
                agent_id: record.agent_id,
                records,
            };
        }

        // Completion is git-keyed: a "complete" terminal without a matching
        // commit is a spent run (the marker is a hint, never the classifier).
        let subjects = services.git.subjects();
        let matched = match_planned(&row.commit_message, &subjects);
        if matched.tier == MatchTier::Exact || matched.tier == MatchTier::Similar {
            // Row done. Guard the boundary: strays must not land in the next
            // worker's commit.
            (services.clear_state)();
            return RowOutcome::Done {
                row: row.clone(),
                matched,
                records,
            };
        }
        if matched.tier == MatchTier::Candidate {
            // Near-miss: report both strings, stop for adjudication.
            let spent = attempt + 1;
            let fields = RunFields {
                agent_id: record.agent_id.clone(),
                started_at: record.started_at,
            };
            let st = state_file(
                services,
                row,
                spent,
                "near-miss",
                persisted.as_ref(),
                Some(&fields),
            );
            (services.save_state)(&st);
            return RowOutcome::NearMiss {
                row: row.clone(),
                subject: matched.subject.unwrap_or_default(),
                records,
            };
        }

        // Spent run: failed / complete-without-commit / STUCK-without-question.
        let spent = attempt + 1;
        let spent_kind = if terminal.is_completed() {
            "no-commit"
        } else {
            "failed"
        };
        record.outcome = if terminal.is_completed() {
            RunOutcomeKind::NoCommit
        } else {
            RunOutcomeKind::Failed
        };
        let fields = RunFields {
            agent_id: record.agent_id.clone(),
            started_at: record.started_at,
        };
        let st = state_file(
            services,
            row,
            spent,
            spent_kind,
            persisted.as_ref(),
            Some(&fields),
        );
        (services.save_state)(&st);
        if spent >= BUDGET_PER_ROW {
            return spent_outcome(row, records, spent, spent_kind);
        }
        // Automatic retry with a fresh worker; the answer is not repeated.
        attempt = spent;
        carried = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::collections::{HashMap, VecDeque};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::config::{StepOverride, SupervisorConfig};
    use crate::git::{MatchResult, MatchTier};
    use crate::rpc::{RpcEvent, UiReply};
    use crate::state::{SupervisorState, plan_hash_of};
    use crate::todo::{TodoPlan, TodoRow};
    use crate::tui::TuiState;
    use crate::ui::LineKind;
    use crate::worker::{TerminalEvent, WorkerError, WorkerSnapshot, WorkerSpawnOpts};

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    /// Operator interrupt to inject while the fake awaits a terminal.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InterruptKind {
        Stop,
        Restart,
        /// `^D` kill switch: flips the same pair of flags the real
        /// `main.rs::kill_watcher` does, in the same order (review F1).
        Kill,
    }

    /// One scripted worker attempt: the terminal event and the snapshot
    /// text the port reports for it. `spawn_error` fails the spawn itself;
    /// `interrupt` flips the shared `RunControl` flags when the terminal is
    /// awaited (models an operator interrupt mid-run).
    #[derive(Debug, Clone, PartialEq)]
    struct FakeScript {
        terminal: TerminalEvent,
        text: String,
        transcript: Option<std::path::PathBuf>,
        spawn_error: Option<String>,
        interrupt: Option<InterruptKind>,
    }

    impl Default for FakeScript {
        fn default() -> Self {
            Self {
                terminal: TerminalEvent::ProcessExit,
                text: String::new(),
                transcript: None,
                spawn_error: None,
                interrupt: None,
            }
        }
    }

    /// What the fake recorded about one spawn.
    #[derive(Debug, Clone, PartialEq)]
    struct SpawnRecord {
        id: u64,
        prompt: String,
        opts: WorkerSpawnOpts,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct FakeState {
        /// Queued scripts, consumed one per spawn, front first.
        scripts: VecDeque<FakeScript>,
        /// Every spawn the loop requested, in order.
        spawned: Vec<SpawnRecord>,
        /// Live scripts by worker id (the loop snapshots by id).
        live: HashMap<u64, FakeScript>,
        next_id: u64,
    }

    /// Fake `WorkerPort`: scripted terminals/snapshots plus a way to inject
    /// mid-await operator interrupts. `control` is the same `RunControl`
    /// the loop reads, so flags flipped here are observed there.
    struct FakeWorkerPort<'a> {
        state: Arc<tokio::sync::Mutex<FakeState>>,
        control: Option<&'a RunControl>,
    }

    impl FakeWorkerPort<'_> {
        fn with<'a>(
            scripts: Vec<FakeScript>,
            control: Option<&'a RunControl>,
        ) -> FakeWorkerPort<'a> {
            let mut queue: VecDeque<FakeScript> = VecDeque::new();
            for script in scripts {
                queue.push_back(script);
            }
            FakeWorkerPort {
                state: Arc::new(tokio::sync::Mutex::new(FakeState {
                    scripts: queue,
                    spawned: Vec::new(),
                    live: HashMap::new(),
                    next_id: 0,
                })),
                control,
            }
        }

        /// Snapshot of every spawn request (prompt + spawn opts).
        async fn spawned(&self) -> Vec<SpawnRecord> {
            let state = self.state.lock().await;
            state.spawned.clone()
        }
    }

    impl WorkerPort for FakeWorkerPort<'_> {
        async fn spawn(&self, prompt: &str, opts: &WorkerSpawnOpts) -> Result<u64, WorkerError> {
            let mut state = self.state.lock().await;
            let id = state.next_id;
            state.next_id += 1;
            let script = state.scripts.pop_front().unwrap_or_default();
            state.spawned.push(SpawnRecord {
                id,
                prompt: prompt.to_string(),
                opts: opts.clone(),
            });
            state.live.insert(id, script.clone());
            if let Some(err) = &script.spawn_error {
                return Err(WorkerError::Spawn(err.clone()));
            }
            Ok(id)
        }

        async fn snapshot(&self, id: u64) -> Option<WorkerSnapshot> {
            let state = self.state.lock().await;
            let worker = state.live.get(&id)?;
            Some(WorkerSnapshot {
                id,
                text: worker.text.clone(),
                tool_uses: 0,
                turn_count: 0,
                compaction_count: 0,
                context_percent: None,
                transcript: worker.transcript.as_ref().cloned(),
                cost: None,
                tokens: None,
                context_window: None,
                started_at: 1_000_000,
            })
        }

        async fn abort(&self, id: u64) -> Result<(), WorkerError> {
            let _ = id;
            Ok(())
        }

        async fn await_terminal(
            &self,
            id: u64,
            timeout: Duration,
        ) -> Result<TerminalEvent, WorkerError> {
            let _ = timeout;
            let state = self.state.lock().await;
            let Some(script) = state.live.get(&id) else {
                return Err(WorkerError::UnknownWorker(id));
            };
            if let Some(ctrl) = self.control
                && let Some(kind) = &script.interrupt
            {
                match kind {
                    InterruptKind::Stop => ctrl.stop_requested.store(true, Ordering::SeqCst),
                    InterruptKind::Restart => ctrl.restart_requested.store(true, Ordering::SeqCst),
                    InterruptKind::Kill => {
                        ctrl.kill_requested.store(true, Ordering::SeqCst);
                        ctrl.stop_requested.store(true, Ordering::SeqCst);
                    }
                }
            }
            Ok(script.terminal.clone())
        }

        async fn subscribe(&self, _id: u64) -> Option<tokio::sync::broadcast::Receiver<RpcEvent>> {
            None
        }

        async fn reply_extension_ui(
            &self,
            id: u64,
            _ui_id: &str,
            _reply: &UiReply,
        ) -> Result<(), WorkerError> {
            let state = self.state.lock().await;
            // Dialogs are answered by the operator UI, not the loop; the
            // fake merely acknowledges so UI-side wiring can round-trip.
            if !state.live.contains_key(&id) {
                return Err(WorkerError::UnknownWorker(id));
            }
            Ok(())
        }

        async fn dispose(&self) {}
    }

    /// Fake `GitFacts`: returns a scripted sequence of `git log` subject
    /// lists, one per `subjects()` call, repeating the last entry. `dirty`
    /// makes `status --short` non-empty.
    #[derive(Debug)]
    struct FakeGit {
        staged: Vec<Vec<String>>,
        calls: Cell<usize>,
        dirty: bool,
    }

    impl FakeGit {
        fn with(staged: Vec<Vec<String>>, dirty: bool) -> Self {
            assert!(
                !staged.is_empty(),
                "the staged sequence needs at least one element (the repeating tail)"
            );
            Self {
                staged,
                calls: Cell::new(0),
                dirty,
            }
        }
    }

    impl GitFacts for FakeGit {
        fn subjects(&self) -> Vec<String> {
            let idx = self.calls.get().min(self.staged.len() - 1);
            self.calls.set(idx + 1);
            self.staged[idx].clone()
        }

        fn status_short(&self) -> Vec<String> {
            if self.dirty {
                vec![" M src/stray.rs".to_string()]
            } else {
                Vec::new()
            }
        }
    }

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    /// Captures everything the injected closures record.
    #[derive(Debug, Clone, Default, PartialEq)]
    struct Capture {
        saved: Vec<SupervisorState>,
        cleared: u32,
        reports: Vec<(ReportKind, String)>,
    }

    fn row(number: u64, commit: &str) -> TodoRow {
        TodoRow {
            id: format!("{number}"),
            number,
            commit_message: commit.to_string(),
            logical_unit: "unit".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        }
    }

    fn plan(rows: Vec<TodoRow>) -> TodoPlan {
        TodoPlan {
            source: None,
            prerequisites: Vec::new(),
            rows,
            done_marked: false,
        }
    }

    fn state(runs_used: u32, current_row: u64, last_outcome: &str) -> SupervisorState {
        SupervisorState {
            plan_hash: plan_hash_of(""),
            current_row,
            runs_used,
            last_outcome: last_outcome.to_string(),
            adjudicated: Vec::new(),
            agent_id: None,
            started_at: None,
        }
    }

    fn script(terminal: TerminalEvent, text: &str) -> FakeScript {
        FakeScript {
            terminal,
            text: text.to_string(),
            transcript: None,
            spawn_error: None,
            interrupt: None,
        }
    }

    fn ask_script(question: &str) -> FakeScript {
        FakeScript {
            terminal: TerminalEvent::Settled,
            text: format!("work done\nPI_WORKER_STATUS: ASK\nQUESTION: {question}"),
            transcript: None,
            spawn_error: None,
            interrupt: None,
        }
    }

    fn settled(text: &str) -> FakeScript {
        script(TerminalEvent::Settled, text)
    }

    fn failed(text: &str) -> FakeScript {
        script(TerminalEvent::ProcessExit, text)
    }

    fn with_interrupt(interrupt: InterruptKind, script: FakeScript) -> FakeScript {
        let mut out = script;
        out.interrupt = Some(interrupt);
        out
    }

    /// Clone the capture out of the shared lock (call after the run ends).
    async fn shared_capture(shared: &Arc<tokio::sync::Mutex<Capture>>) -> Capture {
        let guard = shared.lock().await;
        guard.clone()
    }

    fn report_has(capture: &Capture, kind: ReportKind, needle: &str) -> bool {
        capture
            .reports
            .iter()
            .any(|(k, line)| *k == kind && line.contains(needle))
    }

    // ------------------------------------------------------------------
    // run_row: happy path and completions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn run_row_marks_done_on_an_exact_commit_match() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], false);
        let port = FakeWorkerPort::with(vec![settled("row 1 complete")], None);
        let report_cap = shared.clone();
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: Some(Box::new(move |kind: ReportKind, line: &str| {
                let mut guard = report_cap.try_lock().expect("capture lock");
                guard.reports.push((kind, line.to_string()));
            })),
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::Done {
                row,
                matched,
                records,
            } => {
                assert_eq!(row.id, "1");
                assert_eq!(matched.tier, MatchTier::Exact);
                assert_eq!(matched.subject.as_deref(), Some("feat: row one"));
                assert_eq!(records.len(), 1);
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let spawned = port.spawned().await;
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].opts.name, "pi-plan-row-1");
        assert!(
            spawned[0]
                .prompt
                .contains("Implement ONLY row 1 of TODO.md:")
        );

        let mut capture = shared_capture(&shared).await;
        assert_eq!(capture.saved.len(), 1, "spawn-time running save only");
        assert_eq!(capture.saved[0].last_outcome, "running");
        assert_eq!(capture.saved[0].current_row, 1);
        assert_eq!(capture.saved[0].runs_used, 0);
        assert_eq!(capture.cleared, 1, "done clears the state file");
        capture = shared_capture(&shared).await;
        assert!(report_has(
            &capture,
            ReportKind::Spawn,
            "row 1: spawned agent 0"
        ));
        assert!(report_has(
            &capture,
            ReportKind::Terminal,
            "row 1: agent 0 completed"
        ));
    }

    #[tokio::test]
    async fn run_row_retries_once_with_a_fresh_worker_after_a_failed_attempt() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        // First classification sees no commit; the retry's does.
        let git = FakeGit::with(vec![Vec::new(), vec!["feat: row one".to_string()]], false);
        let port = FakeWorkerPort::with(
            vec![failed("worker died mid-edit"), settled("row 1 complete")],
            None,
        );
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::Done {
                matched, records, ..
            } => {
                assert_eq!(matched.tier, MatchTier::Exact);
                assert_eq!(records.len(), 2);
                assert_eq!(records[0].attempt, 1);
                assert_eq!(records[0].outcome, RunOutcomeKind::Failed);
                assert_eq!(records[1].attempt, 2);
                assert_eq!(records[1].outcome, RunOutcomeKind::Completed);
            }
            other => panic!("expected Done, got {other:?}"),
        }

        assert_eq!(
            port.spawned().await.len(),
            2,
            "two fresh workers, one per attempt"
        );
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .iter()
                .map(|s| s.last_outcome.clone())
                .collect::<Vec<String>>(),
            ["running", "failed", "running"],
            "spawn-time saves + the spent 'failed' save"
        );
        assert_eq!(capture.saved[1].runs_used, 1);
        assert_eq!(capture.cleared, 1);
    }

    #[tokio::test]
    async fn run_row_spends_both_runs_and_stops_at_the_budget() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(
            vec![failed("first worker died"), failed("second worker died")],
            None,
        );
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::BudgetExhausted {
                runs_used,
                last_outcome,
                records,
                ..
            } => {
                assert_eq!(runs_used, 2);
                assert_eq!(last_outcome, "failed");
                assert_eq!(records.len(), 2);
            }
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }
        assert_eq!(port.spawned().await.len(), 2);
    }

    #[tokio::test]
    async fn run_row_spawn_error_stops_the_row_immediately() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let mut broken = script(TerminalEvent::Settled, "unused");
        broken.spawn_error = Some("pi binary missing".to_string());
        let port = FakeWorkerPort::with(vec![broken], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::BudgetExhausted {
                runs_used,
                last_outcome,
                records,
                ..
            } => {
                assert_eq!(runs_used, 1, "a spawn error spends one run and stops");
                assert_eq!(last_outcome, "spawn-error");
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].outcome, RunOutcomeKind::SpawnError);
            }
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture.saved.last().cloned().expect("saved").last_outcome,
            "spawn-error"
        );
    }

    // ------------------------------------------------------------------
    // run_row: ASK question pause and answer folding
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn run_row_question_pause_spends_nothing_and_carries_the_question() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(vec![ask_script("polars or pandas?")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::QuestionPause {
                question, agent_id, ..
            } => {
                assert_eq!(question, "polars or pandas?");
                assert_eq!(agent_id, "0");
            }
            other => panic!("expected QuestionPause, got {other:?}"),
        }
        assert_eq!(port.spawned().await.len(), 1);
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("question save")
                .last_outcome,
            "question"
        );
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("question save")
                .runs_used,
            0,
            "a question pause never spends the row's budget"
        );
    }

    #[tokio::test]
    async fn run_row_folds_the_answer_into_the_first_worker_prompt_only() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        // First worker settles without committing (answer run spent); the
        // retry lands the commit with a prompt that must NOT repeat the answer.
        let git = FakeGit::with(vec![Vec::new(), vec!["feat: row one".to_string()]], false);
        let port = FakeWorkerPort::with(
            vec![settled("row 1 complete"), settled("row 1 complete again")],
            None,
        );
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, Some("Use the polars API.")).await;
        match outcome {
            RowOutcome::Done { matched, .. } => {
                assert_eq!(matched.tier, MatchTier::Exact);
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let spawned = port.spawned().await;
        assert_eq!(spawned.len(), 2);
        assert!(
            spawned[0].prompt.contains("Use the polars API."),
            "the answer is folded into the first worker's prompt"
        );
        assert!(
            !spawned[1].prompt.contains("Use the polars API."),
            "the answer is cleared after the first spend"
        );
    }

    #[tokio::test]
    async fn an_answer_after_the_budget_is_exhausted_still_runs() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(vec![ask_script("one more thing?")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        // Persisted state says both budget runs are spent on this row.
        let recovered = state(2, 1, "failed");
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || Some(recovered.clone())),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        // attempt = 2 == budget, but a user-provided answer is a user-driven
        // continuation: it must still get its worker run.
        let outcome = run_row(&services, &row_1, Some("Keep going.")).await;
        match outcome {
            RowOutcome::QuestionPause { question, .. } => {
                assert_eq!(question, "one more thing?");
            }
            other => panic!("expected QuestionPause, got {other:?}"),
        }
        assert_eq!(port.spawned().await.len(), 1);
    }

    // ------------------------------------------------------------------
    // run_row: near-miss, corrupt state, marker precedence, dirty gate
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn run_row_near_miss_stops_for_adjudication() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        // The worker committed the planned message plus unrelated work:
        // candidate tier → report both, stop, never auto-accept.
        let git = FakeGit::with(vec![vec!["feat: row one and more".to_string()]], false);
        let port = FakeWorkerPort::with(vec![settled("committed a superset")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::NearMiss {
                subject, records, ..
            } => {
                assert_eq!(subject, "feat: row one and more");
                assert_eq!(records.len(), 1);
            }
            other => panic!("expected NearMiss, got {other:?}"),
        }
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("near-miss save")
                .last_outcome,
            "near-miss"
        );
        assert_eq!(capture.cleared, 0, "near-miss is not a completion");
    }

    #[tokio::test]
    async fn corrupt_recovered_state_recomputes_and_keeps_the_full_budget() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(vec![failed("died"), failed("died again")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        // recover_state returns None (corrupt/missing file) → the loop must
        // bootstrap from git: zero runs used, both budget runs available.
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::BudgetExhausted { runs_used, .. } => {
                assert_eq!(runs_used, 2);
            }
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }
        assert_eq!(
            port.spawned().await.len(),
            2,
            "corrupt state spends nothing"
        );
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture.saved[0].runs_used, 0,
            "first save starts from a fresh budget"
        );
    }

    #[tokio::test]
    async fn a_complete_marker_without_a_commit_is_a_spent_run() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(
            vec![
                settled("PI_WORKER_STATUS: COMPLETE"),
                settled("PI_WORKER_STATUS: COMPLETE"),
            ],
            None,
        );
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::BudgetExhausted {
                runs_used,
                last_outcome,
                ..
            } => {
                assert_eq!(runs_used, 2);
                assert_eq!(
                    last_outcome, "no-commit",
                    "a COMPLETE marker is a hint; git is the classifier"
                );
            }
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stuck_marker_never_blocks_a_real_git_match() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], false);
        let port = FakeWorkerPort::with(vec![settled("headache\nPI_WORKER_STATUS: STUCK")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::Done { matched, .. } => {
                assert_eq!(matched.tier, MatchTier::Exact);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dirty_tree_without_an_owner_refuses_without_writing_state() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], true);
        let port = FakeWorkerPort::with(Vec::new(), None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::DirtyWorktree { records, .. } => {
                assert_eq!(records.len(), 1);
                assert!(
                    records[0]
                        .tail
                        .as_deref()
                        .is_some_and(|t| t.contains("dirty"))
                );
            }
            other => panic!("expected DirtyWorktree, got {other:?}"),
        }
        assert_eq!(
            port.spawned().await.len(),
            0,
            "no worker spawns on a dirty tree"
        );
        let capture = shared_capture(&shared).await;
        assert!(
            capture.saved.is_empty(),
            "the refusal writes NO state — it cannot inflate runsUsed"
        );
        assert_eq!(capture.cleared, 0);
    }

    #[tokio::test]
    async fn dirty_tree_owned_by_this_row_resumes_with_a_note_and_banner() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let report_cap = shared.clone();
        // The recovered state still names this row with a live outcome: the
        // stray work is owned, so the gate resumes instead of refusing.
        let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], true);
        let port = FakeWorkerPort::with(vec![settled("row 1 complete")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let mut recovered = state(0, 1, "running");
        recovered.agent_id = Some("old-7".to_string());
        recovered.started_at = Some(1_000_000);
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || Some(recovered.clone())),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: Some(Box::new(move |kind: ReportKind, line: &str| {
                let mut guard = report_cap.try_lock().expect("capture lock");
                guard.reports.push((kind, line.to_string()));
            })),
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        assert!(
            matches!(outcome, RowOutcome::Done { .. }),
            "an owned dirty tree resumes and completes, got {outcome:?}"
        );

        let spawned = port.spawned().await;
        assert_eq!(spawned.len(), 1, "ownership grants one spawn");
        assert!(
            spawned[0]
                .prompt
                .contains("The working tree already contains uncommitted changes"),
            "resumeDirtyWip note is present"
        );
        assert!(
            spawned[0].prompt.contains("(agent old-7)"),
            "the note names the prior agent"
        );
        let capture = shared_capture(&shared).await;
        assert!(
            report_has(&capture, ReportKind::Spawn, "resuming dirty WIP"),
            "the spawn line carries the resuming banner"
        );
    }

    #[tokio::test]
    async fn legacy_refusal_markers_do_not_grant_ownership() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], true);
        let port = FakeWorkerPort::with(Vec::new(), None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        // recover_state() is read at row entry AND at each gate check; the
        // queue answers both reads for two invocations with the legacy
        // refusal markers, which must never count as live ownership.
        let mut queue: VecDeque<Option<SupervisorState>> = VecDeque::new();
        queue.push_back(Some(state(0, 1, "dirty")));
        queue.push_back(Some(state(0, 1, "dirty")));
        queue.push_back(Some(state(0, 1, "spawn-error")));
        queue.push_back(Some(state(0, 1, "spawn-error")));
        let queue_cell = Cell::new(queue);
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || {
                let mut rest: VecDeque<Option<SupervisorState>> = queue_cell.take();
                let out = rest.pop_front().unwrap_or_default();
                queue_cell.set(rest);
                out
            }),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        for (marker, run) in [("dirty", 1), ("spawn-error", 2)] {
            let outcome = run_row(&services, &row_1, None).await;
            match outcome {
                RowOutcome::DirtyWorktree { records, .. } => {
                    assert_eq!(records.len(), 1);
                    assert!(
                        records[0]
                            .tail
                            .as_deref()
                            .is_some_and(|t| t.contains("not owned")),
                        "{run}: legacy marker {marker} must refuse"
                    );
                }
                other => panic!("{run}: expected DirtyWorktree, got {other:?}"),
            }
            assert_eq!(
                port.spawned().await.len(),
                0,
                "{run}: a legacy refusal marker spawns nothing"
            );
        }
        assert_eq!(
            shared_capture(&shared).await,
            Capture::default(),
            "no state write on any refusal"
        );
    }

    #[tokio::test]
    async fn terminal_saves_preserve_human_adjudications() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(vec![failed("died")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let mut recovered = state(0, 1, "running");
        recovered.adjudicated = vec![9];
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || Some(recovered.clone())),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let _ = run_row(&services, &row_1, None).await;
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture.saved.last().cloned().expect("save").adjudicated,
            [9],
            "loop saves keep the human's marked-done rows"
        );
    }

    // ------------------------------------------------------------------
    // run_row: operator interrupts win over every classification
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn stop_mid_await_wins_over_ask_and_git() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        // Both would classify as done/ask — the interrupt must win.
        let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], false);
        let control = RunControl::new();
        let port = FakeWorkerPort::with(
            vec![with_interrupt(
                InterruptKind::Stop,
                ask_script("already answered?"),
            )],
            Some(&control),
        );
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::Stopped { records, .. } => {
                assert_eq!(records.len(), 1);
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("stopped save")
                .last_outcome,
            "stopped"
        );
    }

    #[tokio::test]
    async fn restart_mid_await_spends_nothing_and_respawns_fresh() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let control = RunControl::new();
        let port = FakeWorkerPort::with(
            vec![
                with_interrupt(InterruptKind::Restart, settled("mid-work, restarting")),
                ask_script("still need to know X"),
            ],
            Some(&control),
        );
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::QuestionPause {
                question, records, ..
            } => {
                assert_eq!(question, "still need to know X");
                assert_eq!(records.len(), 2);
                // A restart spends nothing: the fresh worker still runs
                // under attempt 1 (the row's budget is untouched).
                assert_eq!(records[0].attempt, 1);
                assert_eq!(records[1].attempt, 1);
            }
            other => panic!("expected QuestionPause, got {other:?}"),
        }
        assert_eq!(
            port.spawned().await.len(),
            2,
            "restart still spawns a fresh worker"
        );
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("question save")
                .runs_used,
            0,
            "a restart spends nothing"
        );
    }

    #[tokio::test]
    async fn stop_at_the_boundary_ends_the_row_before_any_spawn() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(vec![settled("never reached")], None);
        let control = RunControl::new();
        control.stop_requested.store(true, Ordering::SeqCst);
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        assert!(matches!(outcome, RowOutcome::Stopped { .. }));
        assert_eq!(port.spawned().await.len(), 0);
        assert_eq!(
            shared_capture(&shared).await.saved.len(),
            0,
            "no state write for a boundary stop"
        );
    }

    // ------------------------------------------------------------------
    // Ctrl-D kill switch (fix 3)
    // ------------------------------------------------------------------

    #[test]
    fn run_control_new_starts_with_kill_requested_clear() {
        let control = RunControl::new();
        assert!(!control.kill_requested.load(Ordering::SeqCst));
        assert!(!control.stop_requested.load(Ordering::SeqCst));
        assert!(!control.restart_requested.load(Ordering::SeqCst));
    }

    #[test]
    fn stop_was_kill_disambiguates_the_ask_stop_verdict() {
        // F2 parity: only the Ctrl-D kill switch sets `kill_requested`;
        // Ctrl-C and the `.pi-plan-stop` file close the same ASK modal
        // with `Stop` but must keep the pre-existing `Err` path.
        let control = RunControl::new();
        assert!(
            !stop_was_kill(&control),
            "a graceful stop keeps the Err path"
        );
        control.kill_requested.store(true, Ordering::SeqCst);
        assert!(
            stop_was_kill(&control),
            "a ^D stop prints the report and exits 2"
        );
    }

    #[tokio::test]
    async fn kill_mid_await_ends_the_row_stopped_not_failed() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let control = RunControl::new();
        // A SIGKILLed worker dies with a bare ProcessExit; without the
        // kill's `stop_requested` this would classify as a spent failure.
        let port = FakeWorkerPort::with(
            vec![with_interrupt(
                InterruptKind::Kill,
                failed("killed by SIGKILL"),
            )],
            Some(&control),
        );
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::Stopped { records, .. } => {
                assert_eq!(records.len(), 1);
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
        // The kill record survives (never cleared — the ASK flow reads it
        // after the modal closes, review F2) and the row saved "stopped".
        assert!(control.kill_requested.load(Ordering::SeqCst));
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("stopped save")
                .last_outcome,
            "stopped"
        );
    }

    #[tokio::test]
    async fn kill_wins_over_ask_and_git_like_stop_does() {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        // Both would classify as done/ask — the kill interrupt must win.
        let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], false);
        let control = RunControl::new();
        let port = FakeWorkerPort::with(
            vec![with_interrupt(
                InterruptKind::Kill,
                ask_script("already answered?"),
            )],
            Some(&control),
        );
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        match outcome {
            RowOutcome::Stopped { records, .. } => {
                assert_eq!(records.len(), 1);
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
        let capture = shared_capture(&shared).await;
        assert_eq!(
            capture
                .saved
                .last()
                .cloned()
                .expect("stopped save")
                .last_outcome,
            "stopped"
        );
    }

    #[tokio::test]
    async fn kill_at_the_boundary_blocks_any_spawn() {
        // The watcher flips `kill_requested` + `stop_requested` BEFORE
        // disposing (review F1); while a row is starting, the boundary
        // check then ends it without spawning a replacement.
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let control = RunControl::new();
        control.kill_requested.store(true, Ordering::SeqCst);
        control.stop_requested.store(true, Ordering::SeqCst);
        let port = FakeWorkerPort::with(vec![settled("never reached")], Some(&control));
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let outcome = run_row(&services, &row_1, None).await;
        assert!(matches!(outcome, RowOutcome::Stopped { .. }));
        assert_eq!(port.spawned().await.len(), 0);
    }

    #[tokio::test]
    async fn kill_stops_the_plan_with_a_report_and_work_outstanding() {
        let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
        let todo = plan(rows.clone());
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new(), Vec::new()], false);
        let control = RunControl::new();
        let port = FakeWorkerPort::with(
            vec![
                with_interrupt(InterruptKind::Kill, failed("killed by SIGKILL")),
                settled("never spawned"),
            ],
            Some(&control),
        );
        let report_cap = shared.clone();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: Some(Box::new(move |kind: ReportKind, line: &str| {
                let mut guard = report_cap.try_lock().expect("capture lock");
                guard.reports.push((kind, line.to_string()));
            })),
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let result = run_plan(&services, &todo, None).await;
        assert!(
            !result.all_done,
            "work outstanding — the final report exits 2"
        );
        assert_eq!(result.outcomes.len(), 1);
        assert!(matches!(result.outcomes[0], RowOutcome::Stopped { .. }));
        // The kill leaves the run stopped: no further row spawns (F1).
        assert_eq!(port.spawned().await.len(), 1);
        let capture = shared_capture(&shared).await;
        assert!(report_has(
            &capture,
            ReportKind::Banner,
            "row 1: stopped by user"
        ));
    }

    #[tokio::test]
    async fn kill_during_an_ask_pause_keeps_the_result_for_the_final_report() {
        // Row 1 settles into an ASK pause: the TUI modal opens and the run
        // loop holds the QuestionPause result. When the operator presses
        // ^D the kill watcher flips `kill_requested` then `stop_requested`
        // (review F1) before the modal closes with `Stop`; `stop_was_kill`
        // is the exact predicate the ASK handler consults to keep the
        // result (final report + exit 2) instead of the run ending with
        // the pre-existing `Err` quirk (review F2). Ctrl-C / the stop
        // file never set the flag, keeping that `Err` path byte-for-byte.
        let rows = vec![row(1, "feat: row one")];
        let todo = plan(rows.clone());
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let control = RunControl::new();
        let port = FakeWorkerPort::with(vec![ask_script("continue?")], Some(&control));
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let result = run_plan(&services, &todo, None).await;
        assert!(!result.all_done);
        assert!(matches!(
            result.outcomes[0],
            RowOutcome::QuestionPause { .. }
        ));
        // The ^D lands during the pause exactly as the kill watcher orders
        // it: `kill_requested` before `stop_requested` (review F1).
        control.kill_requested.store(true, Ordering::SeqCst);
        control.stop_requested.store(true, Ordering::SeqCst);
        assert!(stop_was_kill(&control));
        assert_eq!(result.outcomes.len(), 1);
        // Same pause, graceful close: the Err path is untouched (F2 parity).
        let graceful = RunControl::new();
        assert!(!stop_was_kill(&graceful));
    }

    // ------------------------------------------------------------------
    // run_plan: the loop across rows
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn run_plan_drives_every_row_to_done_and_reports_all() {
        let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
        let todo = plan(rows.clone());
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        // subjects() is called at each loop top and once per row classification.
        let m1 = "feat: row one".to_string();
        let m2 = "feat: row two".to_string();
        let git = FakeGit::with(
            vec![
                Vec::new(),
                vec![m1.clone()],
                vec![m1.clone()],
                vec![m1.clone(), m2.clone()],
                vec![m1.clone(), m2.clone()],
            ],
            false,
        );
        let port = FakeWorkerPort::with(vec![settled("one done"), settled("two done")], None);
        let report_cap = shared.clone();
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: Some(Box::new(move |kind: ReportKind, line: &str| {
                let mut guard = report_cap.try_lock().expect("capture lock");
                guard.reports.push((kind, line.to_string()));
            })),
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let result = run_plan(&services, &todo, None).await;
        assert!(result.all_done);
        assert_eq!(result.outcomes.len(), 2);
        for outcome in &result.outcomes {
            if !matches!(*outcome, RowOutcome::Done { .. }) {
                panic!("expected every outcome to be Done, got {outcome:?}");
            }
        }

        let spawned = port.spawned().await;
        assert_eq!(spawned.len(), 2);
        assert!(
            spawned[0]
                .prompt
                .contains("Implement ONLY row 1 of TODO.md:")
        );
        assert!(
            spawned[1]
                .prompt
                .contains("Implement ONLY row 2 of TODO.md:")
        );
        let capture = shared_capture(&shared).await;
        assert!(
            capture.cleared >= 2,
            "each done row clears the state; the loop clears once more at the end"
        );
        assert!(report_has(
            &capture,
            ReportKind::Banner,
            "row 1: done — commit matched"
        ));
        assert!(report_has(
            &capture,
            ReportKind::Banner,
            "row 2: done — commit matched"
        ));
    }

    #[tokio::test]
    async fn tui_report_seam_feeds_the_ring_with_line_mode_bytes() {
        let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
        let todo = plan(rows.clone());
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let m1 = "feat: row one".to_string();
        let m2 = "feat: row two".to_string();
        // Commits surface progressively (mirroring the loop's own git
        // reads) so both rows spawn workers and complete.
        let git = FakeGit::with(
            vec![
                Vec::new(),
                vec![m1.clone()],
                vec![m1.clone()],
                vec![m1.clone(), m2.clone()],
                vec![m1.clone(), m2.clone()],
            ],
            false,
        );
        let port = FakeWorkerPort::with(vec![settled("one done"), settled("two done")], None);
        let report_cap = shared.clone();
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let tui = Arc::new(tokio::sync::Mutex::new(TuiState::new()));
        let tui_ring = tui.clone();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: Some(Box::new(move |kind: ReportKind, line: &str| {
                // The TUI seam: the same bytes line mode eprints reach
                // the shared ring, in order, tagged as banners.
                let mut guard = report_cap.try_lock().expect("capture lock");
                guard.reports.push((kind, line.to_string()));
                let mut ring = tui_ring.try_lock().expect("tui lock");
                ring.push_banner(line.to_string());
            })),
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let result = run_plan(&services, &todo, None).await;
        assert!(result.all_done);

        let capture = shared_capture(&shared).await;
        let ring = tui.try_lock().expect("tui lock");
        assert!(
            !capture.reports.is_empty(),
            "the report seam was exercised across both rows"
        );
        // The ring mirrors EVERY report line in order — spawn / terminal /
        // banner content included — so what the TUI shows is byte-equal
        // to what the non-TTY line mode prints for the same lifecycle.
        assert_eq!(ring.ring.len(), capture.reports.len());
        for (i, (_, line)) in capture.reports.iter().enumerate() {
            assert_eq!(ring.ring[i].text.as_str(), line.as_str());
            assert_eq!(ring.ring[i].kind, LineKind::Banner);
        }
        // The full lifecycle transitions landed in the shared state:
        // spawn → terminal for row 1, spawn → terminal for row 2, and the
        // per-row completion banners.
        assert!(report_has(
            &capture,
            ReportKind::Spawn,
            "row 1: spawned agent"
        ));
        assert!(report_has(&capture, ReportKind::Terminal, "row 1: agent"));
        assert!(report_has(
            &capture,
            ReportKind::Spawn,
            "row 2: spawned agent"
        ));
        assert!(report_has(&capture, ReportKind::Terminal, "row 2: agent"));
        assert!(report_has(&capture, ReportKind::Banner, "row 2: done"));
    }

    #[tokio::test]
    async fn run_plan_stops_at_the_first_question_pause() {
        let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
        let todo = plan(rows.clone());
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(vec![ask_script("which parser?")], None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let result = run_plan(&services, &todo, None).await;
        assert!(!result.all_done, "a question stops the loop for the human");
        assert_eq!(result.outcomes.len(), 1);
        let first = result.outcomes[0].clone();
        match first {
            RowOutcome::QuestionPause { question, .. } => {
                assert_eq!(question, "which parser?");
            }
            other => panic!("expected QuestionPause, got {other:?}"),
        }
        assert_eq!(
            port.spawned().await.len(),
            1,
            "row 2 must not spawn after the pause"
        );
    }

    #[tokio::test]
    async fn run_plan_skips_rows_the_human_adjudicated_done() {
        let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
        let todo = plan(rows.clone());
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(vec![Vec::new()], false);
        let port = FakeWorkerPort::with(Vec::new(), None);
        let control = RunControl::new();
        let config = SupervisorConfig::default();
        let marked: Vec<u64> = vec![1, 2];
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: Some(Box::new(move || marked.clone())),
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let result = run_plan(&services, &todo, None).await;
        assert!(result.all_done);
        assert_eq!(result.outcomes.len(), 0);
        assert_eq!(
            port.spawned().await.len(),
            0,
            "adjudicated rows never spawn"
        );
        assert_eq!(shared_capture(&shared).await.cleared, 1);
    }

    // ------------------------------------------------------------------
    // spawn opts: config precedence feeds the worker spawn
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_opts_follow_config_precedence_per_row() {
        let mut config = SupervisorConfig {
            max_turns: Some(20),
            model: Some("config-model".to_string()),
            ..Default::default()
        };
        config.steps.insert(
            1,
            StepOverride {
                model: Some("row-one-model".to_string()),
                max_turns: Some(5),
            },
        );

        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let save_cap = shared.clone();
        let clear_cap = shared.clone();
        let git = FakeGit::with(
            vec![
                vec!["feat: row one".to_string()],
                vec!["feat: row three".to_string()],
            ],
            false,
        );
        let port = FakeWorkerPort::with(vec![settled("one"), settled("three")], None);
        let control = RunControl::new();
        let services = SuperviseServices {
            git: &git,
            workers: &port,
            config: &config,
            cwd: Path::new("/repo"),
            session_dir: Path::new("/run/sessions"),
            persona: "You are a worker operating under a supervisor.",
            skill_path: None,
            recover_state: Box::new(move || None),
            save_state: Box::new(move |st: &SupervisorState| {
                let mut guard = save_cap.try_lock().expect("capture lock");
                guard.saved.push(st.clone());
            }),
            clear_state: Box::new(move || {
                let mut guard = clear_cap.try_lock().expect("capture lock");
                guard.cleared += 1;
            }),
            adjudicated: None,
            report: None,
            on_spawn: None,
            control: Some(&control),
            await_terminal_timeout: Some(Duration::from_secs(30)),
        };

        let done_1 = run_row(&services, &row(1, "feat: row one"), None).await;
        assert!(matches!(done_1, RowOutcome::Done { .. }));
        let done_3 = run_row(&services, &row(3, "feat: row three"), None).await;
        assert!(matches!(done_3, RowOutcome::Done { .. }));

        let spawned = port.spawned().await;
        assert_eq!(spawned.len(), 2);
        assert_eq!(spawned[0].opts.name, "pi-plan-row-1");
        assert_eq!(
            spawned[0].opts.model, "row-one-model",
            "steps.<n> wins over config"
        );
        assert_eq!(spawned[0].opts.max_turns, 5);
        assert_eq!(
            spawned[1].opts.model, "config-model",
            "config wins without a step override"
        );
        assert_eq!(spawned[1].opts.max_turns, 20);
        assert_eq!(spawned[0].opts.cwd.to_string_lossy().into_owned(), "/repo");
        assert_eq!(
            spawned[0].opts.session_dir.to_string_lossy().into_owned(),
            "/run/sessions"
        );
    }

    // ------------------------------------------------------------------
    // pure report helpers
    // ------------------------------------------------------------------

    #[test]
    fn result_tail_bounds_lines_and_chars() {
        assert_eq!(result_tail("", 6, 400), None);
        assert_eq!(result_tail("   \n  \t ", 6, 400), None);

        let three = "line 1\nline 2\nline 3";
        assert_eq!(result_tail(three, 6, 400), Some(three.to_string()));

        assert_eq!(
            result_tail(three, 2, 400).as_deref(),
            Some("line 2\nline 3"),
            "keeps only the last max_lines lines"
        );

        let capped = result_tail(three, 6, 7).expect("non-empty cap");
        assert_eq!(
            capped, "\nline 3",
            "keeps the tail, caps to max_chars chars"
        );
    }

    #[test]
    fn describe_outcome_covers_every_row_outcome() {
        let row_1 = row(1, "feat: row one");
        let matched = MatchResult {
            tier: MatchTier::Exact,
            subject: Some("feat: row one".to_string()),
        };
        let cases: Vec<(RowOutcome, &str)> = vec![
            (
                RowOutcome::Done {
                    row: row_1.clone(),
                    matched,
                    records: Vec::new(),
                },
                "done — commit matched",
            ),
            (
                RowOutcome::QuestionPause {
                    row: row_1.clone(),
                    question: "which one".to_string(),
                    agent_id: "0".to_string(),
                    records: Vec::new(),
                },
                "paused with a question for you",
            ),
            (
                RowOutcome::NearMiss {
                    row: row_1.clone(),
                    subject: "feat: row one and more".to_string(),
                    records: Vec::new(),
                },
                "near-miss — needs adjudication",
            ),
            (
                RowOutcome::BudgetExhausted {
                    row: row_1.clone(),
                    runs_used: 2,
                    last_outcome: "failed".to_string(),
                    records: Vec::new(),
                },
                "stopped after 2 run(s) (failed)",
            ),
            (
                RowOutcome::DirtyWorktree {
                    row: row_1.clone(),
                    records: Vec::new(),
                },
                "not clean",
            ),
            (
                RowOutcome::Stopped {
                    row: row_1.clone(),
                    records: Vec::new(),
                },
                "stopped by user",
            ),
        ];
        for (outcome, needle) in cases {
            let summary = describe_outcome(&outcome);
            assert!(
                summary.contains(needle),
                "summary {summary:?} must mention {needle:?}"
            );
        }
    }
}
