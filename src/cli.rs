//! CLI surface (step 8): clap subcommand parsing, the one-shot `stop`
//! control-file IPC, `mark <n> done` adjudication, the
//! `reset-permissions` store clear, config/persona/skill resolution, and
//! the read-only `status` report builder.
//!
//! Everything interactive (stdin, the supervise UI loop, command dispatch)
//! lives in the binary; this module is parsing + pure helpers so the CLI
//! contract is unit-testable without spawning processes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{SupervisorConfig, read_config_file};
use crate::git::{MatchTier, match_planned};
use crate::prompt::strip_skill_frontmatter;
use crate::state::{
    STATE_FILE_NAME, SupervisorState, plan_hash_of, read_state_file, save_state_file,
};
use crate::supervise::{RowOutcome, RunRecord, read_todo_file, run_outcome_label};
use crate::todo::{TodoPlan, TodoRow, parse_plan};
use crate::ui::{format_cost, format_duration, format_tokens};
use crate::worker::{WorkerSnapshot, now_epoch_ms};

// ---------------- clap surface ----------------

/// pi-plan — deterministic orchestrator for the TODO.md workflow.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The literal command word for `mark <n> done`, as a clap `ValueEnum`:
/// the only accepted value kebab-cases to `done`, so any other word fails
/// at parse time with clap's usage error (exit 2) instead of the runtime
/// `Err` path (exit 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MarkWord {
    /// Mark the row done (adjudicated).
    Done,
}

/// Subcommands of pi-plan.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Supervise the TODO.md plan (or one row) with a fresh pi worker per row.
    Supervise {
        /// Only supervise this row number, then stop (default: next unmatched row).
        #[arg(long)]
        row: Option<u64>,
        /// Pre-answered reply for the row's ASK question (folds into the
        /// first worker prompt).
        #[arg(long)]
        answer: Option<String>,
        /// Path to a supervisor.config.json (default: ./supervisor.config.json).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to a theme JSON (overrides the active pi theme from settings).
        #[arg(long)]
        theme: Option<PathBuf>,
    },
    /// Print a status summary (plan, git matches, persisted state).
    Status,
    /// Ask a running `supervise` to stop at the next boundary.
    Stop,
    /// Mark a row done even without a matching commit: `mark <n> done`.
    Mark {
        /// Row number to mark done.
        row: u64,
        /// The literal command word "done".
        #[arg(value_enum)]
        done: MarkWord,
    },
    /// Clear every stored project permission: `reset-permissions [--yes]`.
    ResetPermissions {
        /// Skip the confirmation prompt (default: ask first).
        #[arg(long)]
        yes: bool,
    },
    /// Supervise exactly one row, then exit.
    Step {
        /// Row number to supervise.
        row: u64,
        /// Pre-answered reply for the row's ASK question.
        #[arg(long)]
        answer: Option<String>,
        /// Path to a supervisor.config.json.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to a theme JSON (overrides the active pi theme from settings).
        #[arg(long)]
        theme: Option<PathBuf>,
    },
}

// ---------------- one-shot stop control file ----------------

/// One-shot stop-request file name (bare, resolved against the run-state
/// root). `supervise` discards a stale file at startup so a killed run's
/// request is never inherited, then watches for it while running and
/// consumes it at the next boundary (plan step 8).
pub const STOP_FILE_NAME: &str = ".pi-plan-stop";

pub fn stop_file_path(root: &Path) -> PathBuf {
    root.join(STOP_FILE_NAME)
}

/// Best-effort write of a stop request; a failed write must not fail `stop`.
pub fn write_stop_request(root: &Path) {
    let _ = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(stop_file_path(root))?;
        f.write_all(b"requested\n")?;
        Ok(())
    })();
}

/// Remove a stop-request file (startup discard / consume-on-read).
pub fn clear_stop_request(root: &Path) {
    let _ = fs::remove_file(stop_file_path(root));
}

/// True when a stop request is pending under `root`.
pub fn stop_request_present(root: &Path) -> bool {
    fs::read_to_string(stop_file_path(root)).ok().is_some()
}

// ---------------- mark <n> done ----------------

/// Merge row `n` into `state.adjudicated` and persist under `root`. Creates
/// a state file when none exists (tied to the current plan hash) so a `mark`
/// before the first `supervise` still takes effect; the run-state root is
/// created on demand by the save. Fails when the plan has no such
/// row. A repeated mark is idempotent.
pub fn mark_done(cwd: &Path, root: &Path, todo: &TodoPlan, row: u64) -> Result<(), String> {
    if !todo.rows.iter().any(|r| r.number == row) {
        return Err(format!("no such row {row} in TODO.md"));
    }
    let mut state = read_state_file(root).unwrap_or_else(|| {
        let content = read_todo_file(cwd);
        SupervisorState {
            plan_hash: plan_hash_of(content.as_str()),
            current_row: row,
            runs_used: 0,
            last_outcome: "adjudicated".to_string(),
            adjudicated: Vec::new(),
            agent_id: None,
            started_at: None,
        }
    });
    if !state.adjudicated.contains(&row) {
        state.adjudicated.push(row);
        state.adjudicated.sort_by_key(|&n| n);
    }
    save_state_file(root, &state);
    Ok(())
}

// ---------------- config / persona / skill ----------------

/// Resolve the supervisor config: `--config PATH` wins, else
/// `<cwd>/supervisor.config.json` when present, else built-in defaults.
/// Never throws (missing/corrupt files fall back to defaults).
pub fn resolve_config(cwd: &Path, config_path: Option<&Path>) -> SupervisorConfig {
    let Some(path) = config_path else {
        return read_config_file(&cwd.join("supervisor.config.json"));
    };
    read_config_file(path)
}

/// Resolve the worker persona file: `PI_PLAN_PERSONA` wins, else
/// `<cwd>/prompts/worker-persona.md` when it exists, else `None` (the
/// worker runs without a persona preamble).
pub fn resolve_persona_path(cwd: &Path, env_pi_plan_persona: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_persona
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    let candidate = cwd.join("prompts").join("worker-persona.md");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Resolve the `--skill` path for worker argv (Contract 3b pins the
/// implement-from-plan skill): `PI_PLAN_SKILL` wins, else
/// `<home>/.pi/agent/skills/implement-from-plan`, else `None` (omit the
/// flag).
pub fn resolve_skill_path(env_pi_plan_skill: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_skill
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    home.map(|h| {
        Path::new(h)
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("implement-from-plan")
    })
    .filter(|p| p.exists())
}

/// Read and strip the implement-from-plan skill once at supervise startup.
/// Any failure — no skill configured, an unreadable file, or unresolvable
/// frontmatter — is a hard error so supervised runs fail fast before any
/// worker spawns. `status`/`stop`/`mark` never load the skill.
pub fn load_skill_body(skill_dir: Option<&Path>) -> Result<String, String> {
    let installation_hint =
        "set PI_PLAN_SKILL or install it to ~/.pi/agent/skills/implement-from-plan";
    let Some(dir) = skill_dir else {
        return Err(format!(
            "cannot read the implement-from-plan skill: not installed — {installation_hint}"
        ));
    };
    let skill_file = dir.join("SKILL.md");
    let skill_label = skill_file.to_string_lossy().into_owned();
    let Some(raw) = fs::read_to_string(skill_file).ok() else {
        return Err(format!(
            "cannot read the implement-from-plan skill: {} — {installation_hint}",
            skill_label,
        ));
    };
    match strip_skill_frontmatter(raw.as_str()) {
        Ok(body) => Ok(body),
        Err(_) => Err(format!(
            "cannot read the implement-from-plan skill: {} — {installation_hint}",
            skill_label,
        )),
    }
}

/// Resolve the clean-worktree skill path for the dirty-gate clean agent —
/// the same precedence shape as `resolve_skill_path`, but for the
/// clean-worktree skill, resolved **lazily** (only when the gate would
/// abort): `PI_PLAN_CLEAN_SKILL` wins, else
/// `<home>/.pi/agent/skills/clean-worktree` when it exists, else `None`
/// (the gate falls back to today's abort).
pub fn resolve_clean_skill_path(
    env_pi_plan_clean_skill: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_clean_skill
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    home.map(|h| {
        Path::new(h)
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("clean-worktree")
    })
    .filter(|p| p.exists())
}

/// Load the clean-worktree skill directory into the `(path, body)` pair the
/// clean spawn needs — the path feeds the agent's `--skill` argv entry, the
/// frontmatter-stripped body feeds its prompt. **Tolerant by design**: any
/// failure — an unreadable `SKILL.md` or unresolvable frontmatter — returns
/// `None`, so a missing or malformed clean-worktree skill can never fail a
/// run, unlike `load_skill_body` which is fail-fast for the hard-required
/// implement-from-plan skill.
pub fn load_clean_skill_body(skill_dir: &Path) -> Option<(PathBuf, String)> {
    let skill_file = skill_dir.join("SKILL.md");
    let raw = fs::read_to_string(skill_file).ok()?;
    let body = strip_skill_frontmatter(raw.as_str()).ok()?;
    Some((skill_dir.to_path_buf(), body))
}

/// Resolve the permission-system package directory for the worker's `-e`
/// flag (Contract 3b `--no-extensions` + `-e <dir>`): the
/// `PI_PLAN_PERMISSION_EXTENSION` env var wins when non-empty, else
/// `<home>/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`.
/// Unlike the skills — optional, omitted from argv when absent — the
/// permission system is HARD-required: workers now spawn bare
/// (`--no-extensions`) and it is the only extension gate left, so an
/// unresolvable extension is a hard error that fails the run before any
/// worker spawns (the same fail-fast shape as the skill prerequisite).
pub fn resolve_permission_extension(
    env_pi_plan_permission_extension: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(p) = env_pi_plan_permission_extension
        && !p.is_empty()
    {
        return Ok(Path::new(p).to_path_buf());
    }
    let Some(h) = home else {
        return Err(
            "cannot resolve the permission-system extension: PI_PLAN_PERMISSION_EXTENSION \
is unset and HOME is unknown — set PI_PLAN_PERMISSION_EXTENSION to the \
pi-permission-system package directory"
                .to_string(),
        );
    };
    let installed = Path::new(h)
        .join(".pi")
        .join("agent")
        .join("npm")
        .join("node_modules")
        .join("@gotgenes")
        .join("pi-permission-system");
    if installed.exists() {
        Ok(installed.to_path_buf())
    } else {
        Err(format!(
            "cannot resolve the permission-system extension: no package at {} — \
set PI_PLAN_PERMISSION_EXTENSION or install pi-permission-system under \
~/.pi/agent/npm/node_modules/@gotgenes",
            installed.to_string_lossy(),
        ))
    }
}

// ---------------- keep/reset permissions prompt ----------------

/// One answer at the start-of-run keep/reset prompt (D6/D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepResetAnswer {
    /// Keep the grants on file (the default).
    Keep,
    /// Clear every project grant.
    Reset,
}

/// The start-of-run keep/reset verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepResetVerdict {
    /// Proceed with the grants on file (keep / restart / EOF — D12).
    Keep,
    /// The operator typed `reset` — clear the store and proceed.
    Reset,
    /// The operator stopped at the prompt (`stop`, Ctrl-C, or ^D) —
    /// cancel the run start and exit 2.
    Stop,
}

/// One line-mode answer at the keep/reset prompt: exactly `reset`
/// (trimmed, case-insensitive) resets; everything else — `keep`,
/// `restart` (keep-and-proceed, D12), a blank line, or garbage — keeps.
pub fn parse_keep_reset_answer(input: &str) -> KeepResetAnswer {
    if input.trim().to_lowercase() == "reset" {
        KeepResetAnswer::Reset
    } else {
        KeepResetAnswer::Keep
    }
}

/// The keep/reset verdict decided purely from the pause outcome's
/// observable pieces (D12): `stopped` (`PauseOutcome::Stopped`, or a
/// line-mode `stop` — which surfaces as `NoAnswer` plus the stop flag)
/// cancels the run; `restarted` (a TUI `Restart` with no worker to
/// restart) keeps; a parsed `reset` answer resets; everything else —
/// `keep`, line-mode `restart`, EOF — keeps. The caller reads the flags
/// from the shared run control right after the pause returns.
pub fn keep_reset_verdict(
    answered: Option<&str>,
    stopped: bool,
    restarted: bool,
) -> KeepResetVerdict {
    if stopped {
        KeepResetVerdict::Stop
    } else if restarted {
        KeepResetVerdict::Keep
    } else if let Some(answer) = answered {
        if parse_keep_reset_answer(answer) == KeepResetAnswer::Reset {
            KeepResetVerdict::Reset
        } else {
            KeepResetVerdict::Keep
        }
    } else {
        KeepResetVerdict::Keep
    }
}

/// The question text the start-of-run keep/reset prompt renders (line
/// mode and the TUI modal both go through the `QuestionPause` seam,
/// D6/D12). The default is keep; `reset` clears the project grants;
/// `stop` cancels the run start (exit 2); `restart` is keep-and-proceed.
pub fn render_keep_reset_prompt(grant_count: usize) -> String {
    format!(
        "keep or reset the stored project permissions? ({grant_count} grant(s) on file)\n\
   keep — continue with the grants on file (default)\n\
   reset — clear every project grant\n\
   stop — cancel this run (exit 2)"
    )
}

// ---------------- status report ----------------

/// Row status label for the `status` report: git match tier first, then
/// human adjudication (a marked row is done even without a commit).
fn row_status(row: &TodoRow, subjects: &[String], state: Option<&SupervisorState>) -> String {
    if let Some(state) = state
        && state.adjudicated.contains(&row.number)
    {
        return "adjudicated done".to_string();
    }
    match match_planned(&row.commit_message, subjects).tier {
        MatchTier::Exact | MatchTier::Similar => "done".to_string(),
        MatchTier::Candidate => "candidate (needs review)".to_string(),
        MatchTier::None => "pending".to_string(),
    }
}

/// The numeric footer facts for the status report: worktree dirt and the
/// stored permission-grant count (D8). Bundled so `format_status_report`
/// stays within clippy's argument budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFoot {
    pub dirty_lines: usize,
    pub grant_count: usize,
}

/// Assemble the `pi-plan status` report lines (pure — the caller does the
/// I/O, this only formats facts). `root_label` is the resolved external
/// run-state root (`~/.pi-plan/<key>`), printed in the `state:` lines so a
/// user grepping the project root is never misled by a cwd-relative name.
pub fn format_status_report(
    cwd_label: &str,
    root_label: &str,
    source: Option<&str>,
    rows: &[TodoRow],
    subjects: &[String],
    state: Option<&SupervisorState>,
    foot: StatusFoot,
) -> Vec<String> {
    let mut out: Vec<String> = vec![format!("pi-plan status — {cwd_label}")];
    if let Some(source) = source {
        out.push(format!("plan source: {source}"));
    }
    let done = rows
        .iter()
        .filter(|r| row_status(r, subjects, state) == "done")
        .count();

    let done_str = done.to_string();
    out.push(format!("rows: {done_str}/{} done", rows.len()));
    for row in rows.iter() {
        out.push(format!(
            "  row {} ({}): {}",
            row.id,
            row.commit_message,
            row_status(row, subjects, state)
        ));
    }
    if let Some(state) = state {
        out.push(format!("state: {root_label}/{}", STATE_FILE_NAME));
        out.push(format!(
            "  current row {} · runs used {} · last outcome {}",
            state.current_row, state.runs_used, state.last_outcome
        ));
        let agent = state.agent_id.clone().unwrap_or_else(|| "—".to_string());
        let started = state
            .started_at
            .map(|t| format!(" · started {}", crate::ui::format_duration(t)))
            .unwrap_or_default();
        out.push(format!("  agent {agent}{started}"));
    } else {
        out.push("state: none (no state file (nothing running))".to_string());
    }
    if foot.grant_count > 0 {
        out.push(format!("permissions: {} grant(s) stored", foot.grant_count));
    } else {
        out.push("permissions: none stored".to_string());
    }
    if foot.dirty_lines > 0 {
        out.push(format!("worktree: DIRTY — {} change(s)", foot.dirty_lines));
    } else {
        out.push("worktree: clean".to_string());
    }
    out
}

/// Parse + readless helper binding `parse_plan` for callers that already
/// have the TODO text (kept here so binary wiring stays thin).
pub fn parse_todo(content: &str) -> TodoPlan {
    parse_plan(content)
}

// ---------------- final report ----------------

/// One-line label for a row outcome in the final report.
pub fn outcome_label(outcome: &RowOutcome) -> String {
    match outcome {
        RowOutcome::Done { .. } => "done — commit matched".to_string(),
        RowOutcome::QuestionPause { .. } => "paused with a question for you".to_string(),
        RowOutcome::NearMiss { .. } => "near-miss — needs adjudication".to_string(),
        RowOutcome::BudgetExhausted { .. } => "stopped — budget exhausted".to_string(),
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

/// The existing `describe_outcome` label is the loop's banner one-liner; the
/// final report reuses it so both surfaces agree.
pub fn banner_label(outcome: &RowOutcome) -> String {
    crate::supervise::describe_outcome(outcome)
}

/// Row number of an outcome (all variants carry their row).
fn outcome_row_number(outcome: &RowOutcome) -> u64 {
    match outcome {
        RowOutcome::Done { row, .. }
        | RowOutcome::QuestionPause { row, .. }
        | RowOutcome::NearMiss { row, .. }
        | RowOutcome::BudgetExhausted { row, .. }
        | RowOutcome::DirtyWorktree { row, .. }
        | RowOutcome::CleanQuestionPause { row, .. }
        | RowOutcome::CleanAnswerOrphaned { row, .. }
        | RowOutcome::Stopped { row, .. } => row.number,
    }
}

/// The per-attempt records of an outcome (every variant carries `records`).
fn outcome_records(outcome: &RowOutcome) -> Vec<&RunRecord> {
    match outcome {
        RowOutcome::Done { records, .. }
        | RowOutcome::QuestionPause { records, .. }
        | RowOutcome::NearMiss { records, .. }
        | RowOutcome::BudgetExhausted { records, .. }
        | RowOutcome::DirtyWorktree { records, .. }
        | RowOutcome::CleanQuestionPause { records, .. }
        | RowOutcome::CleanAnswerOrphaned { records, .. }
        | RowOutcome::Stopped { records, .. } => records.iter().collect::<Vec<&RunRecord>>(),
    }
}

/// Assemble the `--pi-plan report` block (stderr): one line per row outcome,
/// per-attempt details (outcome kind, question, result tail, transcript
/// path), and a done-count summary.
/// One attempt's stats line for the final report (step 5): worker id,
/// cost, tokens, context %, turns, and wall-clock run time — every value
/// already lives on the terminal snapshot (its `started_at` is kept fresh
/// by the stats poll cadence).
fn format_run_stats_line(record: &RunRecord, snap: &WorkerSnapshot) -> String {
    let now = now_epoch_ms().unwrap_or(snap.started_at);
    let ctx = match snap.context_percent {
        Some(p) => {
            let pct = p as u64;
            format!("{pct}%")
        }
        None => "?".to_string(),
    };
    format!(
        "worker: {} · cost {} · {} tokens · ctx {} · {} turns · {}",
        record.agent_id,
        format_cost(snap.cost),
        format_tokens(snap.tokens.map(|t| t.total).unwrap_or(0)),
        ctx,
        snap.turn_count,
        format_duration(now.saturating_sub(snap.started_at))
    )
}

pub fn format_final_report(outcomes: &[RowOutcome], plan_rows: usize) -> Vec<String> {
    let mut out: Vec<String> = vec!["── pi-plan report ──".to_string()];
    let mut done: usize = 0;
    for outcome in outcomes.iter() {
        let number = outcome_row_number(outcome);
        if matches!(*outcome, RowOutcome::Done { .. }) {
            done += 1;
        }
        out.push(format!("  row {number}: {}", outcome_label(outcome)));
        for record in outcome_records(outcome) {
            out.push(format!(
                "    run {}: {}",
                record.attempt,
                run_outcome_label(record.outcome)
            ));
            if let Some(snap) = record.snapshot.as_ref() {
                out.push(format!("      {}", format_run_stats_line(record, snap)));
            }
            if let Some(question) = &record.question
                && !question.is_empty()
            {
                out.push(format!("      question: {question}"));
            }
            if let Some(tail) = &record.tail {
                for line in tail.lines() {
                    out.push(format!("      {line}"));
                }
            }
            if let Some(path) = record.transcript_path.as_ref() {
                out.push(format!(
                    "      transcript: {}",
                    path.to_string_lossy().into_owned()
                ));
            }
        }
    }
    let done_str = done.to_string();
    out.push(format!("done: {done_str}/{} rows", plan_rows));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::git::MatchResult;
    use crate::supervise::RunOutcomeKind;
    use crate::ui::ask_lines;
    use crate::worker::Tokens;

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cwd() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pi-plan-cli-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    const TWO_ROWS: &str = "## Steps\n\
| # | Commit message | Logical unit | Key deliverables | Tests |\n\
| --- | --- | --- | --- | --- |\n\
| 1 | `feat: a` | u | d | t |\n\
| 2 | `feat: implement reader` | u | d | t |\n";

    // ---- clap surface ----

    #[test]
    fn parse_supervise_flags_and_config() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "supervise".to_string(),
            "--row".to_string(),
            "3".to_string(),
            "--answer".to_string(),
            "use tag v2".to_string(),
            "--config".to_string(),
            "/tmp/supervisor.config.json".to_string(),
            "--theme".to_string(),
            "/tmp/my-theme.json".to_string(),
        ])
        .expect("parse supervise");
        match cli.command {
            Command::Supervise {
                row,
                answer,
                config,
                theme,
            } => {
                assert_eq!(row, Some(3));
                assert_eq!(answer, Some("use tag v2".to_string()));
                assert_eq!(
                    config.map(|p| p.to_string_lossy().into_owned()),
                    Some("/tmp/supervisor.config.json".to_string())
                );
                assert_eq!(
                    theme.map(|p| p.to_string_lossy().into_owned()),
                    Some("/tmp/my-theme.json".to_string())
                );
            }
            other => panic!("expected Supervise, got {other:?}"),
        }
    }

    #[test]
    fn supervise_parses_without_a_theme_flag() {
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "supervise".to_string()])
            .expect("parse supervise");
        match cli.command {
            Command::Supervise { theme, .. } => assert_eq!(theme, None),
            other => panic!("expected Supervise, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_requires_a_row_number_and_accepts_theme() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "step".to_string(),
            "2".to_string(),
            "--theme".to_string(),
            "light.json".to_string(),
        ])
        .expect("parse step");
        match cli.command {
            Command::Step { row, theme, .. } => {
                assert_eq!(row, 2);
                assert_eq!(
                    theme.map(|p| p.to_string_lossy().into_owned()),
                    Some("light.json".to_string())
                );
            }
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn parse_mark_requires_the_done_word() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "mark".to_string(),
            "4".to_string(),
            "done".to_string(),
        ])
        .expect("parse mark");
        match cli.command {
            Command::Mark { row, done } => {
                assert_eq!(row, 4);
                assert_eq!(done, MarkWord::Done);
            }
            other => panic!("expected Mark, got {other:?}"),
        }
    }

    #[test]
    fn mark_rejects_any_other_written_argument() {
        let result = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "mark".to_string(),
            "4".to_string(),
            "bogus".to_string(),
        ]);
        assert!(
            result.is_err(),
            "an invalid mark word must fail at clap parse time (exit 2)"
        );
    }

    #[test]
    fn bare_status_and_stop_parse_without_arguments() {
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "status".to_string()])
            .expect("parse status");
        assert!(matches!(cli.command, Command::Status));
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "stop".to_string()])
            .expect("parse stop");
        assert!(matches!(cli.command, Command::Stop));
    }

    // ---- stop control file ----

    #[test]
    fn stop_request_write_present_and_clear() {
        let cwd = temp_cwd();
        assert!(!stop_request_present(&cwd));
        write_stop_request(&cwd);
        assert!(stop_request_present(&cwd));
        clear_stop_request(&cwd);
        assert!(!stop_request_present(&cwd));
    }

    // ---- mark done ----

    #[test]
    fn mark_done_creates_state_and_merges_idempotently() {
        let root = temp_cwd();
        fs::write(root.join("TODO.md"), TWO_ROWS).expect("write todo");
        let todo = parse_plan(TWO_ROWS);

        // First mark creates a state file tied to the current plan hash.
        mark_done(&root, &root, &todo, 2).expect("mark row 2");
        let state = read_state_file(&root).expect("state file exists");
        assert_eq!(state.adjudicated, vec![2]);
        assert_eq!(state.plan_hash, plan_hash_of(TWO_ROWS));

        // A second, earlier row sorts in; repeating row 2 is idempotent.
        mark_done(&root, &root, &todo, 1).expect("mark row 1");
        mark_done(&root, &root, &todo, 2).expect("mark row 2 again");
        let state = read_state_file(&root).expect("state file exists");
        assert_eq!(state.adjudicated, vec![1, 2]);
    }

    #[test]
    fn mark_done_creates_the_storage_root_directory() {
        let cwd = temp_cwd();
        fs::write(cwd.join("TODO.md"), TWO_ROWS).expect("write todo");
        let todo = parse_plan(TWO_ROWS);
        let root = cwd.join("fresh-root");
        assert!(!root.exists(), "root starts missing");
        mark_done(&cwd, &root, &todo, 1).expect("mark row 1");
        assert!(root.exists(), "mark creates the storage root");
        assert!(
            root.join("supervisor-state.json").exists(),
            "state file lives under the created root"
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn mark_done_rejects_unknown_rows() {
        let root = temp_cwd();
        let todo = parse_plan(TWO_ROWS);
        assert!(mark_done(&root, &root, &todo, 9).is_err());
    }

    // ---- status report ----

    fn two_row_plan() -> TodoPlan {
        parse_plan(TWO_ROWS)
    }

    #[test]
    fn status_report_shows_git_tiers_and_state() {
        let todo = two_row_plan();
        let state = Some(SupervisorState {
            plan_hash: "abc".to_string(),
            current_row: 2,
            runs_used: 1,
            last_outcome: "running".to_string(),
            adjudicated: vec![],
            agent_id: Some("7".to_string()),
            started_at: Some(1_000_000),
        });
        // Row 1's commit exists exactly in git; row 2 has no match yet.
        let subjects = vec!["feat: a".to_string()];
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            Some("docs/research/plan.md".to_string().as_str()),
            &todo.rows[..],
            &subjects,
            state.as_ref(),
            StatusFoot {
                dirty_lines: 0,
                grant_count: 3,
            },
        );
        assert!(lines.contains(&"rows: 1/2 done".to_string()));
        assert!(lines.contains(&"  row 1 (feat: a): done".to_string()));
        assert!(lines.contains(&"  row 2 (feat: implement reader): pending".to_string()));
        assert!(
            lines.contains(&"  current row 2 · runs used 1 · last outcome running".to_string())
        );
        assert!(lines.contains(
            &"state: /home/u/.pi-plan/my-repo-a1b2c3d4/supervisor-state.json".to_string()
        ));
        assert!(lines.contains(&"worktree: clean".to_string()));
        assert!(lines.contains(&"permissions: 3 grant(s) stored".to_string()));
    }

    #[test]
    fn status_report_lists_worktree_dirt_and_marked_rows() {
        let todo = two_row_plan();
        let state = Some(SupervisorState {
            plan_hash: "abc".to_string(),
            current_row: 1,
            runs_used: 0,
            last_outcome: "adjudicated".to_string(),
            adjudicated: vec![1],
            agent_id: None,
            started_at: None,
        });
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            state.as_ref(),
            StatusFoot {
                dirty_lines: 2,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"  row 1 (feat: a): adjudicated done".to_string()));
        assert!(lines.contains(&"worktree: DIRTY — 2 change(s)".to_string()));
        assert!(lines.contains(
            &"state: /home/u/.pi-plan/my-repo-a1b2c3d4/supervisor-state.json".to_string()
        ));
        assert!(lines.contains(&"permissions: none stored".to_string()));
    }

    #[test]
    fn status_report_without_state_file_says_none() {
        let todo = two_row_plan();
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            None,
            StatusFoot {
                dirty_lines: 0,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"state: none (no state file (nothing running))".to_string()));
    }

    // ---- reset-permissions ----

    #[test]
    fn reset_permissions_parses_without_yes_and_with_flag() {
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "reset-permissions".to_string()])
            .expect("parse reset-permissions");
        match cli.command {
            Command::ResetPermissions { yes } => assert!(!yes, "no --yes → ask for confirmation"),
            other => panic!("expected ResetPermissions, got {other:?}"),
        }
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "reset-permissions".to_string(),
            "--yes".to_string(),
        ])
        .expect("parse reset-permissions --yes");
        match cli.command {
            Command::ResetPermissions { yes } => assert!(yes, "--yes skips the confirmation"),
            other => panic!("expected ResetPermissions, got {other:?}"),
        }
    }

    #[test]
    fn reset_permissions_rejects_extra_arguments() {
        let result = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "reset-permissions".to_string(),
            "bogus".to_string(),
        ]);
        assert!(
            result.is_err(),
            "a stray positional must fail at clap parse time (exit 2), not at runtime"
        );
    }

    #[test]
    fn status_report_counts_stored_grants() {
        let todo = two_row_plan();
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            None,
            StatusFoot {
                dirty_lines: 0,
                grant_count: 2,
            },
        );
        assert!(lines.contains(&"permissions: 2 grant(s) stored".to_string()));
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            None,
            StatusFoot {
                dirty_lines: 0,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"permissions: none stored".to_string()));
    }

    // ---- persona / skill resolution ----

    #[test]
    fn persona_resolution_prefers_env_over_cwd_file() {
        let cwd = temp_cwd();
        fs::create_dir_all(cwd.join("prompts")).expect("create prompts dir");
        fs::write(
            cwd.join("prompts").join("worker-persona.md"),
            "you are a worker",
        )
        .expect("write persona");
        assert_eq!(
            resolve_persona_path(&cwd, Some("/env/persona.md".to_string().as_str())),
            Some(Path::new("/env/persona.md").to_path_buf())
        );
        let from_cwd = resolve_persona_path(&cwd, None);
        assert_eq!(
            from_cwd.map(|p| p.to_string_lossy().into_owned()),
            Some(
                cwd.join("prompts")
                    .join("worker-persona.md")
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    #[test]
    fn persona_resolution_falls_back_to_none() {
        let cwd = temp_cwd();
        assert_eq!(resolve_persona_path(&cwd, None), None);
    }

    #[test]
    fn skill_resolution_defaults_from_home_and_env_wins() {
        // No skill installed in the fake home → None.
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        assert_eq!(resolve_skill_path(None, Some(home_str.as_str())), None);
        // Env override always wins, even over a real home install.
        assert_eq!(
            resolve_skill_path(
                Some("/skills/mine".to_string().as_str()),
                Some(home_str.as_str())
            ),
            Some(Path::new("/skills/mine").to_path_buf())
        );
        // An installed skill under the fake home resolves.
        let installed = home
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("implement-from-plan");
        fs::create_dir_all(&installed).expect("create skill dir");
        fs::write(installed.join("SKILL.md"), "# skill").expect("write skill");
        assert_eq!(
            resolve_skill_path(None, Some(home_str.as_str())),
            Some(installed)
        );
    }

    #[test]
    fn load_skill_body_fails_fast_when_no_skill_is_configured() {
        match load_skill_body(None) {
            Ok(_) => panic!("expected a hard error for a missing skill"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
    }

    #[test]
    fn load_skill_body_fails_fast_on_unreadable_skill_paths() {
        // A directory that does not exist.
        let missing = temp_cwd().join("nope");
        match load_skill_body(Some(missing.as_path())) {
            Ok(_) => panic!("expected a hard error for a missing dir"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
        // A directory without a SKILL.md.
        let empty = temp_cwd();
        match load_skill_body(Some(empty.as_path())) {
            Ok(_) => panic!("expected a hard error for a missing SKILL.md"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
    }

    #[test]
    fn load_skill_body_strips_frontmatter_from_a_valid_skill_file() {
        let dir = temp_cwd();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: implement-from-plan\ndescription: x\n---\n\n# Implement from Plan\n\n## Purpose\n",
        )
        .expect("write skill");
        let body = load_skill_body(Some(dir.as_path())).expect("skill body loads");
        assert_eq!(body, "# Implement from Plan\n\n## Purpose\n");
        assert!(!body.contains("name: implement-from-plan"));
    }

    #[test]
    fn load_skill_body_fails_fast_when_frontmatter_cannot_be_resolved() {
        // A BOM before the opening delimiter makes the frontmatter
        // unresolvable; the loader must not guess where instructions start.
        let dir = temp_cwd();
        fs::write(dir.join("SKILL.md"), "\u{feff}---\nname: x\n---\n# Body\n")
            .expect("write skill");
        match load_skill_body(Some(dir.as_path())) {
            Ok(_) => panic!("expected a hard error for BOM-leading frontmatter"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
    }

    #[test]
    fn keep_reset_verdict_maps_all_pause_outcomes() {
        // EOF (no answer, no flags) → keep.
        assert_eq!(
            keep_reset_verdict(None, false, false),
            KeepResetVerdict::Keep
        );
        // `keep` / line-mode `restart` / garbage → keep.
        assert_eq!(
            keep_reset_verdict(Some("keep".to_string().as_str()), false, false),
            KeepResetVerdict::Keep
        );
        assert_eq!(
            keep_reset_verdict(Some("restart".to_string().as_str()), false, false),
            KeepResetVerdict::Keep
        );
        assert_eq!(
            keep_reset_verdict(Some("mumble".to_string().as_str()), false, false),
            KeepResetVerdict::Keep
        );
        // Exactly `reset` (case/whitespace-insensitive) resets.
        assert_eq!(
            keep_reset_verdict(Some("reset".to_string().as_str()), false, false),
            KeepResetVerdict::Reset
        );
        assert_eq!(
            keep_reset_verdict(Some(" RESET ".to_string().as_str()), false, false),
            KeepResetVerdict::Reset
        );
        // Stop wins over any answer text (stop → exit 2, D12).
        assert_eq!(
            keep_reset_verdict(Some("reset".to_string().as_str()), true, false),
            KeepResetVerdict::Stop
        );
        // TUI restart (no answer, restart flag) → keep-and-proceed.
        assert_eq!(
            keep_reset_verdict(None, false, true),
            KeepResetVerdict::Keep
        );
    }

    #[test]
    fn keep_reset_prompt_renders_choices_and_round_trips_in_line_mode() {
        let question = render_keep_reset_prompt(3);
        let lines = ask_lines(question.as_str());
        // The rendered prompt names every choice, and each answer parses
        // back to its verdict — the line-mode round trip (the TUI modal
        // offers the same options through the same seam).
        assert!(lines.iter().any(|l| l.contains("keep")));
        assert!(lines.iter().any(|l| l.contains("reset")));
        assert!(lines.iter().any(|l| l.contains("stop")));
        assert!(lines.iter().any(|l| l.contains("3 grant(s) on file")));
        assert_eq!(
            parse_keep_reset_answer("reset".to_string().as_str()),
            KeepResetAnswer::Reset
        );
        assert_eq!(
            parse_keep_reset_answer("keep".to_string().as_str()),
            KeepResetAnswer::Keep
        );
        assert_eq!(
            parse_keep_reset_answer("RESTART".to_string().as_str()),
            KeepResetAnswer::Keep
        );
    }

    #[test]
    fn permission_extension_resolution_env_wins_and_default_requires_an_install() {
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        // Env override always wins, even over a real home install.
        assert_eq!(
            resolve_permission_extension(
                Some("/ext/mine".to_string().as_str()),
                Some(home_str.as_str())
            ),
            Ok(Path::new("/ext/mine").to_path_buf())
        );
        // No install under the fake home → hard error (never run bare).
        match resolve_permission_extension(None, Some(home_str.as_str())) {
            Ok(_) => panic!("an unresolvable extension must fail fast"),
            Err(msg) => assert!(msg.contains("cannot resolve the permission-system extension")),
        }
        // An empty env value falls through to the default install (it is
        // set but blank — not a resolution).
        match resolve_permission_extension(Some("".to_string().as_str()), Some(home_str.as_str())) {
            Ok(_) => panic!("an empty env value must not win"),
            Err(msg) => assert!(msg.contains("cannot resolve the permission-system extension")),
        }
        // An installed package under the fake home resolves the default.
        let installed = home
            .join(".pi")
            .join("agent")
            .join("npm")
            .join("node_modules")
            .join("@gotgenes")
            .join("pi-permission-system");
        fs::create_dir_all(&installed).expect("create package dir");
        assert_eq!(
            resolve_permission_extension(None, Some(home_str.as_str())),
            Ok(installed.clone())
        );
        // …and the empty-env case now resolves to that same default.
        assert_eq!(
            resolve_permission_extension(Some("".to_string().as_str()), Some(home_str.as_str())),
            Ok(installed.clone())
        );
        // No HOME at all → hard error.
        match resolve_permission_extension(None, None) {
            Ok(_) => panic!("no HOME and no env var must fail fast"),
            Err(msg) => assert!(msg.contains("cannot resolve the permission-system extension")),
        }
    }

    #[test]
    fn clean_skill_resolution_defaults_from_home_and_env_wins() {
        // No clean-worktree skill installed in the fake home → None.
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        assert_eq!(
            resolve_clean_skill_path(None, Some(home_str.as_str())),
            None
        );
        // Env override always wins, even over a real home install.
        assert_eq!(
            resolve_clean_skill_path(
                Some("/clean/skills/mine".to_string().as_str()),
                Some(home_str.as_str())
            ),
            Some(Path::new("/clean/skills/mine").to_path_buf())
        );
        // An installed skill under the fake home resolves.
        let installed = home
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("clean-worktree");
        fs::create_dir_all(&installed).expect("create skill dir");
        fs::write(installed.join("SKILL.md"), "# skill").expect("write skill");
        assert_eq!(
            resolve_clean_skill_path(None, Some(home_str.as_str())),
            Some(installed)
        );
    }

    #[test]
    fn clean_skill_body_loading_is_tolerant_and_returns_the_path_body_pair() {
        // A missing directory is never a startup error → None.
        assert_eq!(
            load_clean_skill_body(temp_cwd().join("nope").as_path()),
            None
        );
        // A directory without a SKILL.md → None.
        assert_eq!(load_clean_skill_body(temp_cwd().as_path()), None);
        // Malformed frontmatter (BOM before the delimiter) → None, never a
        // hard error — the opposite posture of the fail-fast row skill.
        let bad = temp_cwd();
        fs::write(bad.join("SKILL.md"), "\u{feff}---\nname: x\n---\n# Body\n")
            .expect("write skill");
        assert_eq!(load_clean_skill_body(bad.as_path()), None);

        // A valid skill resolves to the (path, body) pair — the path feeds
        // the clean spawn's `--skill` argv, the stripped body its prompt.
        let dir = temp_cwd();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: clean-worktree\ndescription: x\n---\n\n# Clean Worktree\n\n## Goal\n",
        )
        .expect("write skill");
        let Some((path, body)) = load_clean_skill_body(dir.as_path()) else {
            panic!("expected the clean skill to load");
        };
        assert_eq!(path, dir);
        assert_eq!(body, "# Clean Worktree\n\n## Goal\n");
        assert!(!body.contains("name: clean-worktree"));
        // The loader is a pure function: every call re-resolves and returns
        // the same pair, so the gate can call the seam once per fire.
        let Some((path2, body2)) = load_clean_skill_body(dir.as_path()) else {
            panic!("expected the clean skill to load on a second call");
        };
        assert_eq!(path2, dir);
        assert_eq!(body2, body);
    }

    #[test]
    fn final_report_carries_clean_questions_and_the_orphan_tail() {
        // Step 6 report surface: a clean question pause's record and the
        // orphan's never-consumable tail must both show up in the final
        // report — the exit-2 path prints them verbatim.
        let row = TodoRow {
            id: "1".to_string(),
            number: 1,
            commit_message: "feat: row one".to_string(),
            logical_unit: "u".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        };
        let outcomes: Vec<RowOutcome> = vec![
            RowOutcome::CleanQuestionPause {
                row: row.clone(),
                question: "may I discard target/?".to_string(),
                agent_id: "0".to_string(),
                records: vec![RunRecord {
                    attempt: 0,
                    row: row.clone(),
                    agent_id: "0".to_string(),
                    outcome: RunOutcomeKind::Completed,
                    question: Some("may I discard target/?".to_string()),
                    tail: Some("asks before destroying".to_string()),
                    transcript_path: None,
                    started_at: 1,
                    completed_at: None,
                    snapshot: None,
                }],
            },
            RowOutcome::CleanAnswerOrphaned {
                row: row.clone(),
                question: "may I discard target/?".to_string(),
                records: vec![RunRecord {
                    attempt: 0,
                    row: row.clone(),
                    agent_id: String::new(),
                    outcome: RunOutcomeKind::Failed,
                    question: Some("may I discard target/?".to_string()),
                    tail: Some(
                        "clean answer never consumable — the worktree is already clean".to_string(),
                    ),
                    transcript_path: None,
                    started_at: 1,
                    completed_at: None,
                    snapshot: None,
                }],
            },
        ];
        let report = format_final_report(&outcomes[..], 1).join("\n");
        assert!(report.contains("paused — clean-worktree agent asks a question"));
        assert!(report.contains("paused — clean answer cannot be consumed"));
        assert!(
            report.contains("question: may I discard target/?"),
            "the clean question reaches the report text"
        );
        assert!(
            report.contains("never consumable"),
            "the anomaly's never-consumable tail reaches the report text"
        );
    }

    #[test]
    fn final_report_lists_each_attempts_stats_from_the_terminal_snapshot() {
        let row = TodoRow {
            id: "1".to_string(),
            number: 1,
            commit_message: "feat: row one".to_string(),
            logical_unit: "u".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        };
        let snap = WorkerSnapshot {
            id: 0,
            text: "assembled".to_string(),
            tool_uses: 38,
            turn_count: 4,
            compaction_count: 0,
            context_percent: Some(61.5),
            transcript: Some(Path::new("/run/sessions/pi-0/session.jsonl").to_path_buf()),
            cost: Some(0.0451),
            tokens: Some(Tokens {
                input: 50_000,
                output: 9_300,
                cache_read: 40_000,
                cache_write: 5_000,
                total: 59_300,
            }),
            context_window: Some(200_000),
            started_at: 1_000_000,
            pending_tool: None,
            terminal: None,
        };
        let outcomes: Vec<RowOutcome> = vec![
            RowOutcome::Done {
                row: row.clone(),
                matched: MatchResult {
                    tier: MatchTier::Exact,
                    subject: Some("feat: row one".to_string()),
                },
                records: vec![RunRecord {
                    attempt: 1,
                    row: row.clone(),
                    agent_id: "0".to_string(),
                    outcome: RunOutcomeKind::Completed,
                    question: None,
                    tail: Some("one done".to_string()),
                    transcript_path: None,
                    started_at: 1_000_000,
                    completed_at: None,
                    snapshot: Some(snap),
                }],
            },
            RowOutcome::BudgetExhausted {
                row: row.clone(),
                runs_used: 1,
                last_outcome: "failed".to_string(),
                records: vec![RunRecord {
                    attempt: 1,
                    row: row.clone(),
                    agent_id: "0".to_string(),
                    outcome: RunOutcomeKind::Failed,
                    question: None,
                    tail: Some("nope".to_string()),
                    transcript_path: None,
                    started_at: 1_000_000,
                    completed_at: None,
                    snapshot: None,
                }],
            },
        ];
        let report = format_final_report(&outcomes[..], 2);
        let text = report.join("\n");
        // The snapshot-backed attempt lists its stats; the duration suffix
        // is wall-clock, so the assertion pins the deterministic prefix.
        assert!(
            text.contains("      worker: 0 · cost $0.0451 · 59.3k tokens · ctx 61% · 4 turns · ")
        );
        // The snapshot-less record shows a run/outcome/tail but NO stats
        // line — exactly one stats line across the whole report.
        let stats_lines = report
            .iter()
            .filter(|l| l.starts_with("      worker: "))
            .collect::<Vec<_>>();
        assert_eq!(stats_lines.len(), 1);
        // The budget-exhausted attempt's run line is present without stats.
        assert!(text.contains("    run 1: failed"));
    }
}
