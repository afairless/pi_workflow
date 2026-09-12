//! pi-plan binary — CLI entry point and the interactive supervise UI loop
//! (plan step 8).
//!
//! Dispatch is clap-derived (`supervise` / `status` / `stop` / `mark` /
//! `step`). `supervise` wires the real `SuperviseServices` (git, RPC
//! workers, state file, report/dialog seams) and runs a live tail task that
//! streams worker events, answers `extension_ui_request` dialogs inline
//! (decision D9), and honors the `stop`/`restart`/`status` line commands.
//! Dialog/ASK output goes to stdout; traces, banners, and reports to stderr.
//!
//! Exit codes: 0 = requested rows completed, 1 = error, 2 = supervise ended
//! with work outstanding (stopped / question / near-miss / budget).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

use pi_plan::cli::{
    Cli, Command, clear_stop_request, format_final_report, format_status_report, mark_done,
    resolve_config, resolve_persona_path, resolve_skill_path, stop_request_present,
    write_stop_request,
};
use pi_plan::config::{SupervisorConfig, resolve_max_turns};
use pi_plan::git::{GitCommands, is_row_done};
use pi_plan::rpc::{ExtensionUiRequest, RpcEvent, UiReply};
use pi_plan::state::{
    SupervisorState, clear_state_file, read_state_file, recover_state, save_state_file,
};
use pi_plan::supervise::{
    ReportKind, RowOutcome, RunControl, RunPlanResult, SuperviseServices, read_todo_file, run_plan,
};
use pi_plan::todo::{TodoPlan, TodoRow, parse_plan};
use pi_plan::ui::{
    LineCommand, TraceRing, apply_delta, ask_lines, dialog_lines, dialog_prompt_label,
    format_status_line, line_command, render_event_line, reply_from_input,
};
use pi_plan::worker::{RpcWorker, WorkerId, WorkerPort, now_epoch_ms};

/// One spawn notification on the tail channel: `(worker id, row number)`.
type SpawnNotice = (u64, u64);

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(&cli).await {
        Ok(code) => std::process::exit(code as i32),
        Err(err) => {
            eprintln!("pi-plan: {err}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: &Cli) -> Result<u8, String> {
    let cwd_buf = match std::env::current_dir() {
        Ok(path) => path,
        Err(err) => return Err(format!("cannot resolve the working directory: {err}")),
    };
    let cwd = cwd_buf.as_path();
    match &cli.command {
        Command::Supervise {
            row,
            answer,
            config,
            ..
        } => {
            cmd_supervise(
                cwd,
                *row,
                answer.as_ref().map(|s| s.as_str()),
                config.as_ref().map(|p| p.as_path()),
            )
            .await
        }
        Command::Step {
            row,
            answer,
            config,
            ..
        } => {
            cmd_supervise(
                cwd,
                Some(*row),
                answer.as_ref().map(|s| s.as_str()),
                config.as_ref().map(|p| p.as_path()),
            )
            .await
        }
        Command::Status => cmd_status(cwd).await,
        Command::Stop => cmd_stop(cwd).await,
        Command::Mark { row, done } => cmd_mark(cwd, *row, done.as_str()).await,
    }
}

// ---------------- non-supervise commands ----------------

/// Read a UTF-8 environment variable as a `String`, `None` when unset or
/// when the value is not valid UTF-8 (paths come through `var_os`).
fn env_string(name: &str) -> Option<String> {
    match std::env::var_os(name) {
        Some(os) => os.to_str().map(|s| s.to_string()),
        None => None,
    }
}

async fn cmd_status(cwd: &Path) -> Result<u8, String> {
    let content = read_todo_file(cwd);
    let todo = parse_plan(&content);
    if todo.rows.is_empty() {
        return Err(format!("{}/TODO.md has no rows", cwd.to_string_lossy()));
    }
    let git = GitCommands::new(cwd);
    let subjects = git.subjects();
    let state = read_state_file(cwd);
    let dirty = git.status_short().len();
    let lines = format_status_report(
        cwd.to_string_lossy().into_owned().as_str(),
        todo.source.as_deref(),
        &todo.rows[..],
        &subjects,
        state.as_ref(),
        dirty,
    );
    for line in lines {
        println!("{line}");
    }
    Ok(0)
}

async fn cmd_stop(cwd: &Path) -> Result<u8, String> {
    write_stop_request(cwd);
    println!("pi-plan: stop requested — consumed at the supervise loop's next boundary");
    if let Some(state) = read_state_file(cwd) {
        println!(
            "  row {} · runs used {} · last outcome {}",
            state.current_row, state.runs_used, state.last_outcome
        );
    } else {
        println!("  no supervisor-state.json (nothing running)");
    }
    Ok(0)
}

async fn cmd_mark(cwd: &Path, row: u64, done: &str) -> Result<u8, String> {
    if done != "done" {
        return Err("usage: pi-plan mark <row> done".to_string());
    }
    let content = read_todo_file(cwd);
    let todo = parse_plan(&content);
    mark_done(cwd, &todo, row)?;
    println!("pi-plan: row {row} marked done (adjudicated)");
    Ok(0)
}

// ---------------- supervise ----------------

/// Supervise the whole plan (or exactly one row when `row` is `Some`).
/// `answer` pre-answers the row's ASK question; when absent, an interactive
/// pause prompts on stdin and the answer folds into a fresh worker.
async fn cmd_supervise(
    cwd: &Path,
    row: Option<u64>,
    answer: Option<&str>,
    config_path: Option<&Path>,
) -> Result<u8, String> {
    // A stale stop request from a killed run must not be inherited.
    clear_stop_request(cwd);

    let config = resolve_config(cwd, config_path);
    let todo_content = read_todo_file(cwd);
    let todo = parse_plan(&todo_content);
    if todo.rows.is_empty() {
        return Err(format!(
            "{}/TODO.md has no rows to supervise",
            cwd.to_string_lossy()
        ));
    }
    let plan = match row {
        Some(n) => single_row_plan(&todo, n)?,
        None => todo.clone(),
    };

    let run_dir = cwd.join(".pi-plan");
    let session_dir = run_dir.join("sessions");
    if let Err(err) = (|| -> std::io::Result<()> {
        fs::create_dir_all(&session_dir)?;
        Ok(())
    })() {
        return Err(format!(
            "cannot create the run directory {}: {err}",
            session_dir.to_string_lossy()
        ));
    }
    let stderr_log = run_dir.join("worker-stderr.log");

    let env_persona = env_string("PI_PLAN_PERSONA");
    let env_skill = env_string("PI_PLAN_SKILL");
    let env_home = env_string("HOME");
    let persona = match resolve_persona_path(cwd, env_persona.as_deref()) {
        Some(path) => fs::read_to_string(path).unwrap_or_default(),
        None => String::new(),
    };
    let skill = resolve_skill_path(env_skill.as_deref(), env_home.as_deref());

    let skill_ref: Option<&Path> = skill.as_deref();
    let git = GitCommands::new(cwd);
    let workers = RpcWorker::system(cwd, Some(stderr_log));
    // The control flags are shared with the spawned operator-UI tasks; the
    // command's own borrow (`control.as_ref()`) feeds the supervise loop.
    let control: Arc<RunControl> = Arc::new(RunControl::new());
    let (spawned_tx, spawned_rx): (
        broadcast::Sender<SpawnNotice>,
        broadcast::Receiver<SpawnNotice>,
    ) = broadcast::channel(8);

    // Wiring closures (Contract 4/5 seams). Recovery recomputes from git
    // when the state file is missing, corrupt, plan-hash-mismatched, or
    // stale (the row already in history); adjudication is read fresh per
    // loop so a concurrent `pi-plan mark <n> done` takes effect.
    let is_done_at = |number: u64| {
        let Some(i) = todo.rows.iter().position(|r| r.number == number) else {
            return true;
        };
        is_row_done(&todo.rows[i].commit_message, &git.subjects())
    };
    let services = SuperviseServices {
        git: &git,
        workers: &workers,
        config: &config,
        cwd,
        session_dir: &session_dir,
        persona: persona.as_str(),
        skill_path: skill_ref,
        recover_state: Box::new(move || recover_state(cwd, todo_content.as_str(), is_done_at)),
        save_state: Box::new(move |st: &SupervisorState| save_state_file(cwd, st)),
        clear_state: Box::new(move || clear_state_file(cwd)),
        adjudicated: Some(Box::new(move || {
            read_state_file(cwd)
                .map(|s| s.adjudicated)
                .unwrap_or_default()
        })),
        report: Some(Box::new(move |_kind: ReportKind, line: &str| {
            eprintln!("{line}")
        })),
        on_spawn: Some(Box::new(move |spawned_row: &TodoRow, agent: String| {
            let _ = spawned_tx.send((agent.parse::<u64>().unwrap_or(0), spawned_row.number));
        })),
        control: Some(control.as_ref()),
        await_terminal_timeout: None,
    };

    // Stop-request watcher: `pi-plan stop` from another shell lands here.
    tokio::spawn(stop_watcher(cwd.to_path_buf(), control.clone()));
    // Live tail: worker events → stderr; dialogs → stdout with inline
    // replies. Clones of the port/control/config are Send/`'static`.
    tokio::spawn(tail_task(
        workers.clone(),
        control.clone(),
        spawned_rx,
        config.clone(),
    ));

    // Run rows, folding human answers into fresh workers after a question
    // pause (Contract 4 — the pause itself never spends a run).
    let mut carried: Option<String> = answer.map(|s| s.to_string());
    let mut final_result: Option<RunPlanResult> = None;
    loop {
        let result = run_plan(&services, &plan, carried.as_deref()).await;
        if let Some(question) = last_question(&result.outcomes[..])
            && carried.is_none()
        {
            for line in ask_lines(question.as_str()) {
                println!("{line}");
            }
            let Some(input) = stdin_read_line().await else {
                break; // EOF: no answer — stop here
            };
            match line_command(&input) {
                Some(LineCommand::Stop) => {
                    control
                        .as_ref()
                        .stop_requested
                        .store(true, Ordering::SeqCst);
                    break;
                }
                Some(LineCommand::Status) => {
                    for line in format_status_report(
                        cwd.to_string_lossy().into_owned().as_str(),
                        todo.source.as_deref(),
                        &todo.rows[..],
                        &git.subjects(),
                        read_state_file(cwd).as_ref(),
                        git.status_short().len(),
                    ) {
                        println!("{line}");
                    }
                    continue;
                }
                _ => {}
            }
            if input.trim().is_empty() {
                break; // blank line: stop here
            }
            carried = Some(input);
            continue;
        }
        final_result = Some(result);
        break;
    }

    // Kill every live worker; transcripts survive in --session-dir.
    workers.dispose().await;

    let Some(result) = final_result else {
        return Err("supervise ended without a result".to_string());
    };
    for line in format_final_report(&result.outcomes[..], plan.rows.len()) {
        eprintln!("{line}");
    }
    Ok(if result.all_done { 0 } else { 2 })
}

/// A plan containing exactly one row (for `supervise --row N` / `step N`).
fn single_row_plan(todo: &TodoPlan, row_number: u64) -> Result<TodoPlan, String> {
    let Some(i) = todo.rows.iter().position(|r| r.number == row_number) else {
        return Err(format!("no such row {row_number} in TODO.md"));
    };
    Ok(TodoPlan {
        source: todo.source.clone(),
        prerequisites: todo.prerequisites.to_vec(),
        rows: vec![todo.rows[i].clone()],
        done_marked: todo.done_marked,
    })
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

/// Read one line of interactive stdin (blocking). `None` at EOF.
async fn stdin_read_line() -> Option<String> {
    // A fresh reader per call loses nothing interactive: `read_line` stops
    // at the first newline, and the next line has not been typed yet.
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut buffer = String::new();
    match reader.read_line(&mut buffer).await {
        Ok(0) => None,
        Ok(_) => {
            let line = buffer.strip_suffix('\n').unwrap_or(&buffer);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                None
            } else {
                Some(line.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Polls for the `.pi-plan-stop` request file and flips the loop's flags.
async fn stop_watcher(cwd: PathBuf, control: Arc<RunControl>) {
    loop {
        if stop_request_present(&cwd) {
            control
                .as_ref()
                .stop_requested
                .store(true, Ordering::SeqCst);
            clear_stop_request(&cwd);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------- live tail ----------------

/// Waits for spawn notifications and tails one monitor task per worker.
async fn tail_task(
    workers: RpcWorker,
    control: Arc<RunControl>,
    mut spawned: broadcast::Receiver<(u64, u64)>,
    config: SupervisorConfig,
) {
    loop {
        let (worker_id, row_number) =
            match tokio::time::timeout(Duration::from_hours(24), spawned.recv()).await {
                Ok(Ok(pair)) => pair,
                Err(_) | Ok(Err(_)) => break,
            };
        tokio::spawn(worker_tail(
            workers.clone(),
            control.clone(),
            config.clone(),
            worker_id,
            row_number,
        ));
    }
}

async fn worker_tail(
    workers: RpcWorker,
    control: Arc<RunControl>,
    config: SupervisorConfig,
    worker_id: WorkerId,
    row_number: u64,
) {
    let Some(mut rx) = workers.subscribe(worker_id).await else {
        return;
    };
    let mut ring = TraceRing::new(240);
    let mut text = String::new();
    let mut quiet_ticks: u32 = 0;
    loop {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Err(_) => {
                quiet_ticks += 1;
                if quiet_ticks >= 10 {
                    quiet_ticks = 0;
                    render_status(&workers, worker_id, row_number, &config).await;
                }
            }
            Ok(Err(_)) => break, // worker stream closed
            Ok(Ok(event)) => {
                match &event {
                    RpcEvent::MessageUpdate(delta) => {
                        if let Some(chunk) = apply_delta(&mut text, delta) {
                            eprintln!("{}", chunk.text);
                            ring.push(chunk.text);
                        }
                    }
                    RpcEvent::ExtensionUiRequest(req) => {
                        if req.is_dialog()
                            && dialog_roundtrip(
                                &workers,
                                control.clone(),
                                worker_id,
                                row_number,
                                req,
                                &config,
                            )
                            .await
                        {
                            break; // operator asked to stop/restart the worker
                        }
                    }
                    RpcEvent::TurnStart => {
                        render_status(&workers, worker_id, row_number, &config).await;
                    }
                    _ => {}
                }
                if let Some(line) = render_event_line(&event) {
                    eprintln!("{}", line.text);
                    ring.push(line.text);
                }
            }
        }
    }
}

/// Render one operator status line from the worker's live snapshot.
async fn render_status(
    workers: &RpcWorker,
    worker_id: WorkerId,
    row_number: u64,
    config: &SupervisorConfig,
) {
    let Some(snap) = workers.snapshot(worker_id).await else {
        return;
    };
    let now = now_epoch_ms().unwrap_or(snap.started_at);
    let elapsed = now.max(snap.started_at) - snap.started_at;
    let row_id = row_number.to_string();
    let agent_id = snap.id.to_string();
    let line = format_status_line(
        row_id.as_str(),
        Some(agent_id.as_str()),
        snap.turn_count,
        resolve_max_turns(config, row_number.max(1)),
        snap.context_percent,
        elapsed,
    );
    eprintln!("{line}");
}

/// Answer a dialog `extension_ui_request` inline: render the prompt, read a
/// reply, honor the `stop`/`restart`/`status` line commands, and send the
/// reply through the worker port (permission passthrough, decision D9).
/// Returns `true` when the operator asked to stop/restart the worker.
async fn dialog_roundtrip(
    workers: &RpcWorker,
    control: Arc<RunControl>,
    worker_id: WorkerId,
    row_number: u64,
    req: &ExtensionUiRequest,
    config: &SupervisorConfig,
) -> bool {
    for line in dialog_lines(req) {
        println!("{line}");
    }
    loop {
        let label = dialog_prompt_label(req);
        println!("{label}");
        let Some(input) = stdin_read_line().await else {
            // EOF: dismiss the dialog; the worker's own ceiling decides next.
            let _ = workers
                .reply_extension_ui(worker_id, req.id.as_str(), &UiReply::Cancelled)
                .await;
            return false;
        };
        match line_command(&input) {
            Some(LineCommand::Stop) => {
                control
                    .as_ref()
                    .stop_requested
                    .store(true, Ordering::SeqCst);
                let _ = workers.abort(worker_id).await;
                return true;
            }
            Some(LineCommand::Restart) => {
                control
                    .as_ref()
                    .restart_requested
                    .store(true, Ordering::SeqCst);
                let _ = workers.abort(worker_id).await;
                return true;
            }
            Some(LineCommand::Status) => {
                render_status(workers, worker_id, row_number, config).await;
                continue;
            }
            None => {}
        }
        if let Some(reply) = reply_from_input(req, &input) {
            let _ = workers
                .reply_extension_ui(worker_id, req.id.as_str(), &reply)
                .await;
            return false;
        }
        println!("  invalid reply — try again (or ctrl-d to dismiss)");
    }
}
