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
use crate::prompt::{
    CleanContinuation, CleanPromptInputs, PromptInputs, ResumeDirtyWip, render_clean_prompt,
    render_worker_prompt,
};
use crate::state::{SupervisorState, plan_hash_of};
use crate::storage::WorkerStatsRecord;
use crate::todo::{TodoPlan, TodoRow, next_row, parse_plan};
use crate::ui::{format_cost, format_tokens};
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

/// Short label for one run's outcome kind.
pub fn run_outcome_label(kind: RunOutcomeKind) -> String {
    match kind {
        RunOutcomeKind::QuestionPause => "question".to_string(),
        RunOutcomeKind::Completed => "completed".to_string(),
        RunOutcomeKind::NoCommit => "no-commit".to_string(),
        RunOutcomeKind::Failed => "failed".to_string(),
        RunOutcomeKind::SpawnError => "spawn-error".to_string(),
        RunOutcomeKind::Aborted => "aborted".to_string(),
    }
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

/// Build a stats record from a finalized run record. `None` when the run
/// has no terminal snapshot (spawn-error / clean-pass / question-only
/// bookkeeping) — such runs write nothing, so the log has no all-null
/// rows.
pub fn worker_stats_from_run(record: &RunRecord) -> Option<WorkerStatsRecord> {
    let snap = record.snapshot.as_ref()?;
    Some(WorkerStatsRecord {
        v: 1,
        row: record.row.number,
        attempt: record.attempt,
        agent_id: record.agent_id.clone(),
        outcome: run_outcome_label(record.outcome),
        cost: snap.cost,
        tokens: snap.tokens.map(|t| t.total),
        context_percent: snap.context_percent,
        context_window: snap.context_window,
        turns: snap.turn_count,
        started_at: record.started_at,
        completed_at: record.completed_at,
        transcript: snap
            .transcript
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    })
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
    /// A clean-worktree agent ended ASK: the run pauses so a human can
    /// answer, and the re-generated clean agent completes the pass with the
    /// answer folded in (distinct from `QuestionPause`, which routes to a
    /// row worker).
    CleanQuestionPause {
        row: TodoRow,
        question: String,
        agent_id: String,
        records: Vec<RunRecord>,
    },
    /// A carried clean answer could not be consumed — the tree is already
    /// clean (ASK-after-cleaning, or the human cleaned manually while
    /// answering). Never a silent success or a silent drop (review F2).
    CleanAnswerOrphaned {
        row: TodoRow,
        question: String,
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
/// Row-terminal hook: fired once per terminal event with the row number
/// and the terminal-kind label (`completed`/`failed`) — the same seam
/// `report_terminal` uses, so line mode and TUI mode both get it.
pub type OnRowTerminalFn<'a> = dyn Fn(u64, &str) + 'a;
/// Durability hook: fired with each finalized run record so a caller can
/// persist its statistics. Invoked once per run attempt, next to the
/// `report_terminal` emit; the builder skips snapshot-less records.
pub type AppendStatsFn<'a> = dyn Fn(&RunRecord) + 'a;
/// Lazy clean-worktree skill resolution: the resolved skill directory
/// **path plus its frontmatter-stripped body**, fetched only when the
/// dirty gate would abort. `None` → fall back to today's abort.
pub type CleanSkillFn<'a> = dyn Fn() -> Option<(PathBuf, String)> + 'a;

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
    /// The implement-from-plan skill body (frontmatter stripped), loaded
    /// once at supervise startup; workers get it framed in their prompts.
    pub skill_body: Option<&'a str>,
    /// Lazy clean-worktree skill seam (path + body pair), resolved only
    /// when the dirty gate would abort. `None` → the gate falls back to
    /// today's `DirtyWorktree` refusal.
    pub clean_skill: Option<Box<CleanSkillFn<'a>>>,
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
    /// Fired on every row terminal (stalled/failed attempts included):
    /// row number + terminal-kind label. TUI mode flips the displayed
    /// worker view not-live here — no snapshot path can deliver it.
    pub on_row_terminal: Option<Box<OnRowTerminalFn<'a>>>,
    /// Fired once per run attempt, next to the terminal report emit, with
    /// the finalized record — the caller persists its statistics (the
    /// branch writes nothing itself; snapshot-less records are skipped by
    /// the builder).
    pub append_stats: Option<Box<AppendStatsFn<'a>>>,
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
        skills: services
            .skill_path
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default(),
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
        RowOutcome::CleanQuestionPause { .. } => {
            "paused — clean-worktree agent asks a question".to_string()
        }
        RowOutcome::CleanAnswerOrphaned { .. } => {
            "paused — clean answer cannot be consumed".to_string()
        }
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

/// Compact stats suffix for the terminal report line
/// (` · cost $X · N tokens · T turns`) so line mode logs each attempt's
/// figures too. No suffix when the snapshot carries neither cost nor
/// tokens — bookkeeping records have no stats to show.
fn terminal_stats_suffix(snap: &WorkerSnapshot) -> String {
    let cost = snap.cost;
    let tokens = snap.tokens.map(|t| t.total);
    if cost.is_none() && tokens.is_none() {
        return String::new();
    }
    format!(
        " · cost {} · {} tokens · {} turns",
        format_cost(cost),
        format_tokens(tokens.unwrap_or(0)),
        snap.turn_count
    )
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
    let suffix = snapshot.map(terminal_stats_suffix).unwrap_or_default();
    let mut lines: Vec<String> = vec![format!(
        "row {}: agent {} {}{}{}",
        row.id,
        agent_id,
        terminal_kind_label(terminal),
        marker,
        suffix
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
    if let Some(f) = services.on_row_terminal.as_ref() {
        // One structured call site, next to the report emit: the label
        // is a terminal KIND, not a row outcome, so stalled/failed
        // attempts the loop will retry fire it too.
        f(row.number, terminal_kind_label(terminal));
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

/// One owner-less dirty-tree clean pass's verdict (Change 4/5): whether to
/// restart (re-fire the gate) or stop with an outcome. A verdict with no
/// restart and no outcome means the tree was cleaned and git-verified, so
/// the row proceeds.
#[derive(Debug, Clone)]
struct CleanVerdict {
    /// Operator restart: abort the pass, re-fire the gate (nothing spent).
    restart: bool,
    /// A stopping outcome when the pass ended the row (Stopped /
    /// CleanQuestionPause / DirtyWorktree).
    outcome: Option<RowOutcome>,
}

impl CleanVerdict {
    fn proceed() -> Self {
        Self {
            restart: false,
            outcome: None,
        }
    }

    fn restart() -> Self {
        Self {
            restart: true,
            outcome: None,
        }
    }

    fn outcome(stop: RowOutcome) -> Self {
        Self {
            restart: false,
            outcome: Some(stop),
        }
    }
}

/// Run one owner-less dirty-tree clean pass (Change 4/5). Runs entirely
/// before the row worker's prompt build: resolves the clean-worktree skill
/// lazily, renders the clean prompt (a carried continuation folds the human's
/// answer in exactly once), spawns a fresh `pi-plan-clean-<row>` agent with
/// both skills, awaits the terminal, and classifies -- **operator interrupts
/// first** (a stop/^D-kill mid-clean is a stop, never a clean failure; a
/// restart re-fires the gate), then git-keyed: COMPLETE **and** an empty
/// `status_short()` **and** no accidental commit matching the row's planned
/// message => `Proceed`; ASK => `CleanQuestionPause`; anything else => a clean
/// record (attempt marker `0`) plus the existing `DirtyWorktree` abort. The
/// pass never touches `attempt`/`runs_used` and writes no state, so it can
/// never be read as a spent budgeted run.
async fn run_row_clean_pass<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row: &TodoRow,
    records: &mut Vec<RunRecord>,
    persisted: Option<&SupervisorState>,
    attempt: u32,
    continuation: Option<&CleanContinuation>,
) -> CleanVerdict {
    let mut clean_abort = |tail: String| {
        records.push(RunRecord {
            attempt: 0,
            row: row.clone(),
            agent_id: String::new(),
            outcome: RunOutcomeKind::Failed,
            question: None,
            tail: Some(tail),
            transcript_path: None,
            started_at: now_epoch_ms().unwrap_or(0),
            completed_at: None,
            snapshot: None,
        });
        CleanVerdict::outcome(RowOutcome::DirtyWorktree {
            row: row.clone(),
            records: records.clone(),
        })
    };

    // Lazy resolve (Change 2): a missing or malformed clean-worktree skill
    // falls back to today's abort with a naming tail -- never a run error.
    let clean_pair: Option<(PathBuf, String)> = match services.clean_skill.as_ref() {
        Some(f) => f(),
        None => None,
    };
    let Some((clean_dir, clean_body)) = clean_pair else {
        return clean_abort(
            "clean-worktree skill not installed -- set PI_PLAN_CLEAN_SKILL or \
install it to ~/.pi/agent/skills/clean-worktree"
                .to_string(),
        );
    };

    // The clean prompt: operative clean-worktree body + implement-from-plan
    // reference context, the next-row note, and the carried answer block
    // when a human already answered a previous clean agent.
    let cwd_label = services.cwd.to_string_lossy().into_owned();
    let plan_source = read_plan_source(services.cwd);
    let prompt = render_clean_prompt(CleanPromptInputs {
        cwd: cwd_label.as_str(),
        plan_source: plan_source.as_deref(),
        row,
        clean_body: Some(clean_body.as_str()),
        impl_body: services.skill_body,
        continuation,
    });
    // Both skills on one agent: implement-from-plan (reference) first, then
    // the clean-worktree dir from the lazily resolved pair.
    let mut clean_skills: Vec<PathBuf> = Vec::new();
    if let Some(path) = services.skill_path {
        clean_skills.push(path.to_path_buf());
    }
    clean_skills.push(clean_dir);
    let opts = WorkerSpawnOpts {
        name: format!("pi-plan-clean-{}", row.number),
        model: resolve_model(services.config, row.number),
        max_turns: resolve_max_turns(services.config, row.number),
        turn_timeout: DEFAULT_TURN_TIMEOUT,
        cwd: services.cwd.to_path_buf(),
        session_dir: services.session_dir.to_path_buf(),
        skills: clean_skills,
        tools: Vec::new(),
        persona: services.persona.to_string(),
        stats_interval: DEFAULT_STATS_INTERVAL,
    };
    let clean_agent_id: String = match services.workers.spawn(&prompt, &opts).await {
        Ok(id) => format!("{id}"),
        Err(err) => {
            records.push(RunRecord {
                attempt: 0,
                row: row.clone(),
                agent_id: String::new(),
                outcome: RunOutcomeKind::SpawnError,
                question: None,
                tail: Some(format!("clean spawn failed: {err}")),
                transcript_path: None,
                started_at: now_epoch_ms().unwrap_or(0),
                completed_at: None,
                snapshot: None,
            });
            return CleanVerdict::outcome(RowOutcome::DirtyWorktree {
                row: row.clone(),
                records: records.clone(),
            });
        }
    };
    // Fire on_spawn so the trace/UI surfaces the clean session.
    if let Some(f) = services.on_spawn.as_ref() {
        f(row, clean_agent_id.clone());
    }
    let worker_num = clean_agent_id.parse::<u64>().unwrap_or(0);

    // Await the clean agent (same caller bound as row workers).
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
    let started_at = snapshot
        .as_ref()
        .map(|s| s.started_at)
        .unwrap_or(now_epoch_ms().unwrap_or(0));
    let transcript_path = snapshot
        .as_ref()
        .map(|s| s.transcript.clone())
        .unwrap_or_default();

    // Operator interrupts win over the clean classification (review F1):
    // consume the control flags exactly like the row flow before any
    // Success/Question/Failure mapping.
    if let Some(control) = services.control {
        let stopping = control.stop_requested.load(Ordering::SeqCst);
        if control.restart_requested.load(Ordering::SeqCst) || stopping {
            control.restart_requested.store(false, Ordering::SeqCst);
            control.stop_requested.store(false, Ordering::SeqCst);
            if stopping {
                // Save the stopped state exactly as the row flow does;
                // `kill_requested` is preserved for `stop_was_kill`.
                let fields = RunFields {
                    agent_id: clean_agent_id,
                    started_at,
                };
                let st = state_file(
                    services,
                    row,
                    attempt + 1,
                    "stopped",
                    persisted,
                    Some(&fields),
                );
                (services.save_state)(&st);
                return CleanVerdict::outcome(RowOutcome::Stopped {
                    row: row.clone(),
                    records: records.clone(),
                });
            }
            // Restart: re-fire the gate; the pass spends nothing and writes
            // no state.
            return CleanVerdict::restart();
        }
    }

    let status = parse_worker_status(&text);
    // Question pause first (a question wins over a clean tree -- review F2).
    if terminal.is_completed() && status.as_deref() == Some("ASK") {
        let question = parse_question(&text).unwrap_or_default();
        records.push(RunRecord {
            attempt: 0,
            row: row.clone(),
            agent_id: clean_agent_id.clone(),
            outcome: RunOutcomeKind::Completed,
            question: Some(question.clone()),
            tail: result_tail(&text, 6, 400),
            transcript_path,
            started_at,
            completed_at: None,
            snapshot: snapshot.clone(),
        });
        return CleanVerdict::outcome(RowOutcome::CleanQuestionPause {
            row: row.clone(),
            question,
            agent_id: clean_agent_id,
            records: records.clone(),
        });
    }
    if terminal.is_completed() && status.as_deref() == Some("COMPLETE") {
        // Success is git-keyed, never marker-trusted: the agent must have
        // left `status_short()` empty and must not have committed the row's
        // planned message (the tripwire -- the next worker owns that commit).
        let subjects = services.git.subjects();
        let tripwire_hit = is_row_done(&row.commit_message, &subjects);
        let tree_clean = services.git.status_short().is_empty();
        if tree_clean && !tripwire_hit {
            if let Some(report) = services.report.as_ref() {
                let clean_line = format!(
                    "row {}: working tree cleaned by {}",
                    row.id,
                    clean_agent_id.as_str(),
                );
                report(ReportKind::Terminal, clean_line.as_str());
            }
            records.push(RunRecord {
                attempt: 0,
                row: row.clone(),
                agent_id: clean_agent_id,
                outcome: RunOutcomeKind::Completed,
                question: None,
                tail: Some("worktree cleaned".to_string()),
                transcript_path,
                started_at,
                completed_at: None,
                snapshot: snapshot.clone(),
            });
            return CleanVerdict::proceed();
        }
        // Failure reasons: the tree is still dirty or the tripwire hit.
        let reason = if tripwire_hit {
            "clean agent committed the row's planned message".to_string()
        } else {
            "working tree still dirty after the clean pass".to_string()
        };
        records.push(RunRecord {
            attempt: 0,
            row: row.clone(),
            agent_id: clean_agent_id,
            outcome: RunOutcomeKind::Failed,
            question: None,
            tail: Some(format!(
                "{reason}; last result: {}",
                result_tail(&text, 4, 200).unwrap_or("(no output)".to_string()),
            )),
            transcript_path,
            started_at,
            completed_at: None,
            snapshot: snapshot.clone(),
        });
        return CleanVerdict::outcome(RowOutcome::DirtyWorktree {
            row: row.clone(),
            records: records.clone(),
        });
    }
    // Anything else -- STUCK, a missing/unknown marker, a process exit, or a
    // stall-ceiling abort -- is a clean failure: record it and abort.
    let reason = if !terminal.is_completed() {
        "clean agent did not settle (terminal event indicates failure)".to_string()
    } else if status.as_deref() == Some("STUCK") {
        "clean agent reported STUCK".to_string()
    } else {
        "clean agent missing a PI_WORKER_STATUS marker".to_string()
    };
    records.push(RunRecord {
        attempt: 0,
        row: row.clone(),
        agent_id: clean_agent_id,
        outcome: RunOutcomeKind::Failed,
        question: None,
        tail: Some(format!(
            "{reason}; last result: {}",
            result_tail(&text, 4, 200).unwrap_or("(no output)".to_string()),
        )),
        transcript_path,
        started_at,
        completed_at: None,
        snapshot: snapshot.clone(),
    });
    CleanVerdict::outcome(RowOutcome::DirtyWorktree {
        row: row.clone(),
        records: records.clone(),
    })
}

/// Verdict of one interactive ASK-pause round trip (plan step 7). The CLI
/// seam maps the TUI modal / line-mode stdin outcomes onto this
/// exhaustive set; the driver routes it per the row-vs-clean semantics in
/// [`run_plan_interactive`].
#[derive(Debug, Clone, PartialEq)]
pub enum PauseOutcome {
    /// The operator answered; the answer folds into the next pass.
    Answer(String),
    /// No answer: blank line / EOF, a TUI `Restart`, or a line-mode `stop`
    /// — the run ends on the caller's existing no-answer path.
    NoAnswer,
    /// TUI `Stop` (Ctrl-C / stop file). `kill` records whether the ^D kill
    /// switch powered the stop (final report + exit 2, via [`stop_was_kill`])
    /// or it was graceful (the exit-1 `Err` path).
    Stopped { kill: bool },
}

/// The interactive ASK-pause seam (plan step 7): the binary implements it
/// over the TUI modal / line-mode stdin and tests script it. The driver
/// pauses only when a row or clean-worktree agent ended ASK, and never sees
/// `status` reprints — those live inside the seam.
///
/// The `async_fn_in_trait` lint is suppressed deliberately: this seam is
/// only ever used through [`run_plan_interactive`], never through dynamic
/// dispatch, so auto trait bounds on the returned futures are irrelevant
/// (same rationale as [`WorkerPort`]).
#[allow(async_fn_in_trait)]
pub trait QuestionPause {
    async fn pause(&self, question: &str) -> PauseOutcome;
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
    clean_continuation: Option<&CleanContinuation>,
) -> RunPlanResult {
    let plan_hash = plan_hash_of(&read_todo_file(services.cwd));
    let mut outcomes: Vec<RowOutcome> = Vec::new();
    let mut carried = answer;
    let mut carried_clean = clean_continuation;

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

        let outcome = run_row(services, &todo.rows[next], carried, carried_clean).await;
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
        // The answers apply to the continuation of the SAME row only.
        carried = None;
        carried_clean = None;
    }
}

/// The interactive supervise loop (plan step 7): runs `run_plan` passes,
/// pauses for the human on ASK, and folds answers into fresh workers. Owns
/// the carried row answer and the clean-answer channel exactly as the
/// binary loop did.
///
/// Returns `Some(result)` when the run ended with a result to report (the
/// binary's final-report exit-2/0 path) and `None` when it ended without
/// one (the binary's `supervise ended without a result` exit-1 path).
pub async fn run_plan_interactive<'a, G: GitFacts, W: WorkerPort, P: QuestionPause>(
    services: &SuperviseServices<'a, G, W>,
    plan: &TodoPlan,
    answer: Option<&str>,
    clean_continuation: Option<&CleanContinuation>,
    pause: &P,
) -> Option<RunPlanResult> {
    let mut carried: Option<String> = answer.map(|s| s.to_string());
    let mut carried_clean: Option<CleanContinuation> = clean_continuation.cloned();
    loop {
        let result = run_plan(services, plan, carried.as_deref(), carried_clean.as_ref()).await;
        // One-continuation lifetime (review F2): a carried clean answer
        // survives only a pass whose LAST outcome is the clean question
        // pause that produced it. Any other ending abandons the channel —
        // otherwise a stale answer could fold into an unrelated later
        // row's clean pass.
        carried_clean = keep_clean_continuation(&result.outcomes[..], carried_clean);
        if let Some(question) = last_question(&result.outcomes[..])
            && carried.is_none()
        {
            // Answer folds into the next pass; no answer (stop / restart /
            // blank / EOF) ends the run here. A kill-powered stop still
            // prints the final report (exit 2); Ctrl-C and the stop file
            // keep the `Err` path byte-for-byte.
            match pause.pause(question.as_str()).await {
                PauseOutcome::Answer(answer) => {
                    carried = Some(answer);
                    continue;
                }
                PauseOutcome::NoAnswer => {
                    return None;
                }
                PauseOutcome::Stopped { kill } => {
                    if kill {
                        return Some(result);
                    }
                    return None;
                }
            }
        }
        // A clean-worktree agent's ASK pauses the run exactly like a row
        // question (same seam), and the answer is carried into a
        // RE-generated clean agent: the gate re-fires and folds the answer
        // into the fresh clean prompt. A second consecutive ASK after an
        // answered continuation terminates (one answered continuation per
        // pause chain — parity with rows); stop/blank/^D/EOF ends the run
        // (exit 2) with the question in the final report.
        if let Some(question) = last_clean_question(&result.outcomes[..]) {
            if carried_clean.is_some() {
                // No second re-ask: terminate with the new question in the
                // report.
                return Some(result);
            }
            match pause.pause(question.as_str()).await {
                PauseOutcome::Answer(answer) => {
                    carried_clean = Some(CleanContinuation {
                        question: question.clone(),
                        answer,
                    });
                    continue;
                }
                _ => {
                    // No answer (stop / restart / blank / EOF): the clean
                    // question ends the run (exit 2) with the question in
                    // the final report.
                    return Some(result);
                }
            }
        }
        return Some(result);
    }
}

/// The last question a run paused on, when any outcome is a question pause.
fn last_question(outcomes: &[RowOutcome]) -> Option<String> {
    let mut found: Option<String> = None;
    for outcome in outcomes.iter() {
        if let RowOutcome::QuestionPause { question, .. } = outcome {
            found = Some(question.clone());
        }
    }
    found
}

/// The last clean-worktree question a run paused on, when any outcome is a
/// clean question pause. Deliberately distinct from [`last_question`]: a
/// clean answer routes to a re-generated clean agent, never to a row
/// worker.
fn last_clean_question(outcomes: &[RowOutcome]) -> Option<String> {
    let mut found: Option<String> = None;
    for outcome in outcomes.iter() {
        if let RowOutcome::CleanQuestionPause { question, .. } = outcome {
            found = Some(question.clone());
        }
    }
    found
}

/// The one-continuation lifetime filter for the clean-answer channel
/// (review F2): the carried continuation is kept only when the pass's last
/// outcome is a clean question pause (the pause that produced it). Every
/// other ending — a row question, an orphaned answer, an abort, or a
/// completed plan — abandons the channel, so a stale answer can never fold
/// into an unrelated later row's clean pass.
fn keep_clean_continuation(
    outcomes: &[RowOutcome],
    carried_clean: Option<CleanContinuation>,
) -> Option<CleanContinuation> {
    if last_clean_question(outcomes).is_some() {
        carried_clean
    } else {
        None
    }
}

/// Run a single row to a terminal outcome. One fresh worker per attempt.
/// A question pause spends nothing; a restart request spends nothing; every
/// other terminal event spends one of the row's `BUDGET_PER_ROW` runs.
pub async fn run_row<'a, G: GitFacts, W: WorkerPort>(
    services: &SuperviseServices<'a, G, W>,
    row: &TodoRow,
    answer: Option<&str>,
    clean_continuation: Option<&CleanContinuation>,
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
    let mut carried_clean = clean_continuation;

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
        // A carried clean answer with a clean tree is unconsumable — the
        // gate cannot fire (ASK-after-cleaning, or the human cleaned
        // manually while answering): anomaly abort, never a silent drop
        // (review F2).
        if !tree_is_dirty && carried_clean.is_some() {
            // The final report must carry the question and the
            // never-consumable tail: the run ends exit 2 with both visible,
            // never a silent drop (review F2).
            let question = carried_clean
                .map(|c| c.question.clone())
                .unwrap_or_default();
            records.push(RunRecord {
                attempt: 0,
                row: row.clone(),
                agent_id: String::new(),
                outcome: RunOutcomeKind::Failed,
                question: Some(question.clone()),
                tail: Some(
                    "clean answer never consumable — the worktree is already clean".to_string(),
                ),
                transcript_path: None,
                started_at: now_epoch_ms().unwrap_or(0),
                completed_at: None,
                snapshot: None,
            });
            return RowOutcome::CleanAnswerOrphaned {
                row: row.clone(),
                question,
                records,
            };
        }
        let resume_note: Option<ResumeDirtyWip<'_>> = if tree_is_dirty {
            let owned = gate_state.as_ref().is_some_and(|p| {
                p.current_row == row.number
                    && p.last_outcome != "dirty"
                    && p.last_outcome != "spawn-error"
            });
            if !owned {
                // Owner-less dirt (the only abort path): hand the tree to a
                // clean-worktree agent instead of aborting on sight. On
                // success the tree is clean and the row proceeds; the
                // continuation, if any, was folded into the clean agent and
                // is consumed for good.
                let verdict = run_row_clean_pass(
                    services,
                    row,
                    &mut records,
                    persisted.as_ref(),
                    attempt,
                    carried_clean,
                )
                .await;
                if verdict.restart {
                    continue;
                }
                if let Some(stop) = verdict.outcome {
                    return stop;
                }
                // Proceed: the tree is clean and git-verified.
                carried_clean = None;
                None
            } else {
                gate_state.as_ref().map(|p| ResumeDirtyWip {
                    agent_id: p.agent_id.as_deref(),
                })
            }
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
            skill_body: services.skill_body,
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
        // Finalize the record: one terminal timestamp, then it is pushed
        // (the report's view) and handed to the durability seam.
        record.completed_at = Some(now_epoch_ms().unwrap_or(0));
        records.push(record.clone());
        report_terminal(
            services,
            row,
            record.agent_id.as_str(),
            &terminal,
            snapshot.as_ref(),
        );
        if let Some(f) = services.append_stats.as_ref() {
            // One stats record per run attempt — the builder skips
            // snapshot-less records, so question pauses and run attempts
            // that never reached a terminal write nothing.
            f(&record);
        }

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
mod tests;
