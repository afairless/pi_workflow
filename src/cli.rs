//! CLI surface (step 8): clap subcommand parsing, the one-shot `stop`
//! control-file IPC, `mark <n> done` adjudication, config/persona/skill
//! resolution, and the read-only `status` report builder.
//!
//! Everything interactive (stdin, the supervise UI loop, command dispatch)
//! lives in the binary; this module is parsing + pure helpers so the CLI
//! contract is unit-testable without spawning processes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::{SupervisorConfig, read_config_file};
use crate::git::{MatchTier, match_planned};
use crate::state::{SupervisorState, plan_hash_of, read_state_file, save_state_file};
use crate::supervise::{RowOutcome, RunOutcomeKind, RunRecord, read_todo_file};
use crate::todo::{TodoPlan, TodoRow, parse_plan};

// ---------------- clap surface ----------------

/// pi-plan — deterministic orchestrator for the TODO.md workflow.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Command to run.
    #[command(subcommand)]
    pub command: Command,
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
        done: String,
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
    },
}

// ---------------- one-shot stop control file ----------------

/// One-shot stop-request file name (bare, resolved against the project
/// root). `supervise` discards a stale file at startup so a killed run's
/// request is never inherited, then watches for it while running and
/// consumes it at the next boundary (plan step 8).
pub const STOP_FILE_NAME: &str = ".pi-plan-stop";

pub fn stop_file_path(cwd: &Path) -> PathBuf {
    cwd.join(STOP_FILE_NAME)
}

/// Best-effort write of a stop request; a failed write must not fail `stop`.
pub fn write_stop_request(cwd: &Path) {
    let _ = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(stop_file_path(cwd))?;
        f.write_all(b"requested\n")?;
        Ok(())
    })();
}

/// Remove a stop-request file (startup discard / consume-on-read).
pub fn clear_stop_request(cwd: &Path) {
    let _ = fs::remove_file(stop_file_path(cwd));
}

/// True when a stop request is pending in `cwd`.
pub fn stop_request_present(cwd: &Path) -> bool {
    fs::read_to_string(stop_file_path(cwd)).ok().is_some()
}

// ---------------- mark <n> done ----------------

/// Merge row `n` into `state.adjudicated` and persist. Creates a state file
/// when none exists (tied to the current plan hash) so a `mark` before the
/// first `supervise` still takes effect. Fails when the plan has no such
/// row. A repeated mark is idempotent.
pub fn mark_done(cwd: &Path, todo: &TodoPlan, row: u64) -> Result<(), String> {
    if !todo.rows.iter().any(|r| r.number == row) {
        return Err(format!("no such row {row} in TODO.md"));
    }
    let mut state = read_state_file(cwd).unwrap_or_else(|| {
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
    save_state_file(cwd, &state);
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

/// Assemble the `pi-plan status` report lines (pure — the caller does the
/// I/O, this only formats facts).
pub fn format_status_report(
    cwd_label: &str,
    source: Option<&str>,
    rows: &[TodoRow],
    subjects: &[String],
    state: Option<&SupervisorState>,
    dirty_lines: usize,
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
        out.push("state: supervisor-state.json".to_string());
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
        out.push("state: none (no supervisor-state.json)".to_string());
    }
    if dirty_lines > 0 {
        out.push(format!("worktree: DIRTY — {dirty_lines} change(s)"));
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
        | RowOutcome::Stopped { row, .. } => row.number,
    }
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

/// The per-attempt records of an outcome (every variant carries `records`).
fn outcome_records(outcome: &RowOutcome) -> Vec<&RunRecord> {
    match outcome {
        RowOutcome::Done { records, .. }
        | RowOutcome::QuestionPause { records, .. }
        | RowOutcome::NearMiss { records, .. }
        | RowOutcome::BudgetExhausted { records, .. }
        | RowOutcome::DirtyWorktree { records, .. }
        | RowOutcome::Stopped { records, .. } => records.iter().collect::<Vec<&RunRecord>>(),
    }
}

/// Assemble the `--pi-plan report` block (stderr): one line per row outcome,
/// per-attempt details (outcome kind, question, result tail, transcript
/// path), and a done-count summary.
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
        ])
        .expect("parse supervise");
        match cli.command {
            Command::Supervise {
                row,
                answer,
                config,
            } => {
                assert_eq!(row, Some(3));
                assert_eq!(answer, Some("use tag v2".to_string()));
                assert_eq!(
                    config.map(|p| p.to_string_lossy().into_owned()),
                    Some("/tmp/supervisor.config.json".to_string())
                );
            }
            other => panic!("expected Supervise, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_requires_a_row_number() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "step".to_string(),
            "2".to_string(),
        ])
        .expect("parse step");
        match cli.command {
            Command::Step { row, .. } => assert_eq!(row, 2),
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
                assert_eq!(done, "done");
            }
            other => panic!("expected Mark, got {other:?}"),
        }
    }

    #[test]
    fn mark_rejects_any_other_written_argument() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "mark".to_string(),
            "4".to_string(),
            "bogus".to_string(),
        ])
        .expect("parse mark");
        match cli.command {
            Command::Mark { done, .. } => assert!(done != "done"),
            other => panic!("expected Mark, got {other:?}"),
        }
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
        let cwd = temp_cwd();
        fs::write(cwd.join("TODO.md"), TWO_ROWS).expect("write todo");
        let todo = parse_plan(TWO_ROWS);

        // First mark creates a state file tied to the current plan hash.
        mark_done(&cwd, &todo, 2).expect("mark row 2");
        let state = read_state_file(&cwd).expect("state file exists");
        assert_eq!(state.adjudicated, vec![2]);
        assert_eq!(state.plan_hash, plan_hash_of(TWO_ROWS));

        // A second, earlier row sorts in; repeating row 2 is idempotent.
        mark_done(&cwd, &todo, 1).expect("mark row 1");
        mark_done(&cwd, &todo, 2).expect("mark row 2 again");
        let state = read_state_file(&cwd).expect("state file exists");
        assert_eq!(state.adjudicated, vec![1, 2]);
    }

    #[test]
    fn mark_done_rejects_unknown_rows() {
        let cwd = temp_cwd();
        let todo = parse_plan(TWO_ROWS);
        assert!(mark_done(&cwd, &todo, 9).is_err());
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
            Some("docs/research/plan.md".to_string().as_str()),
            &todo.rows[..],
            &subjects,
            state.as_ref(),
            0,
        );
        assert!(lines.contains(&"rows: 1/2 done".to_string()));
        assert!(lines.contains(&"  row 1 (feat: a): done".to_string()));
        assert!(lines.contains(&"  row 2 (feat: implement reader): pending".to_string()));
        assert!(
            lines.contains(&"  current row 2 · runs used 1 · last outcome running".to_string())
        );
        assert!(lines.contains(&"worktree: clean".to_string()));
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
        let lines = format_status_report("/repo", None, &todo.rows[..], &[], state.as_ref(), 2);
        assert!(lines.contains(&"  row 1 (feat: a): adjudicated done".to_string()));
        assert!(lines.contains(&"worktree: DIRTY — 2 change(s)".to_string()));
        assert!(lines.contains(&"state: supervisor-state.json".to_string()));
    }

    #[test]
    fn status_report_without_state_file_says_none() {
        let todo = two_row_plan();
        let lines = format_status_report("/repo", None, &todo.rows[..], &[], None, 0);
        assert!(lines.contains(&"state: none (no supervisor-state.json)".to_string()));
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
}
