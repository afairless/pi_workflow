//! pi-plan binary — CLI entry point and the interactive supervise UI loop
//! (plan step 8).
//!
//! Dispatch is clap-derived (`supervise` / `status` / `stop` / `mark` /
//! `step` / `reset-permissions`). `supervise` wires the real `SuperviseServices` (git, RPC
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

use pi_plan::cli::{
    Cli, Command, KeepResetVerdict, MarkWord, StatusFoot, clear_stop_request, format_final_report,
    format_status_report, keep_reset_verdict, load_clean_skill_body, load_skill_body, mark_done,
    render_keep_reset_prompt, resolve_clean_skill_path, resolve_config,
    resolve_permission_extension, resolve_persona_path, resolve_skill_path, stop_request_present,
    write_stop_request,
};
use pi_plan::config::{SupervisorConfig, resolve_max_turns};
use pi_plan::git::{GitCommands, is_row_done};
use pi_plan::permissions::{
    AutoReply, PermissionEnv, PermissionStore, StoreHealth, auto_approval, clear_permissions,
    load_permissions, record_human_reply, save_permissions,
};
use pi_plan::rpc::{ExtensionUiRequest, RpcEvent, UiReply};
use pi_plan::state::{
    STATE_FILE_NAME, SupervisorState, clear_state_file, read_state_file, recover_state,
    save_state_file,
};
use pi_plan::storage::{ProjectStorage, append_worker_stats};
use pi_plan::supervise::{
    PauseOutcome, QuestionPause, ReportKind, RunControl, RunRecord, SuperviseServices,
    read_todo_file, run_plan_interactive, stop_was_kill, worker_stats_from_run,
};
use pi_plan::theme::{
    Palette, Stylize, ThemeRoots, read_settings_theme, resolve_active_palette, select_source,
};
use pi_plan::todo::{TodoPlan, TodoRow, parse_plan};
use pi_plan::tui::{
    AnsiSink, DEFAULT_SIZE, DisplayMode, Modal, ModalOutcome, SavedTerminal, Size, StyledLine,
    Terminal, TtyFaces, TuiHooks, TuiState, await_modal_outcome, choose_mode, compose_frame,
    input_task, view_from_snapshot, watch_resizes,
};
use pi_plan::ui::{
    LineCommand, LineKind, StreamKind, TraceRing, TuiLine, apply_delta, ask_lines, dialog_lines,
    dialog_prompt_label, format_status_line, line_command, render_event_line, reply_from_input,
    stream_part,
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
            theme,
            ..
        } => {
            cmd_supervise(
                cwd,
                *row,
                answer.as_ref().map(|s| s.as_str()),
                config.as_ref().map(|p| p.as_path()),
                theme.as_ref().map(|p| p.as_path()),
            )
            .await
        }
        Command::Step {
            row,
            answer,
            config,
            theme,
            ..
        } => {
            cmd_supervise(
                cwd,
                Some(*row),
                answer.as_ref().map(|s| s.as_str()),
                config.as_ref().map(|p| p.as_path()),
                theme.as_ref().map(|p| p.as_path()),
            )
            .await
        }
        Command::Status => cmd_status(cwd).await,
        Command::Stop => cmd_stop(cwd).await,
        Command::Mark { row, done } => cmd_mark(cwd, *row, *done).await,
        Command::ResetPermissions { yes } => cmd_reset_permissions(cwd, *yes).await,
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

/// Resolve the per-project run-state root from the environment:
/// `$PI_PLAN_STATE_DIR` (override) > `$HOME/.pi-plan` > hard error. Every
/// command shares this one resolver so `supervise` / `status` / `stop` /
/// `mark` always agree on where state, sessions, logs, and the stop
/// control live — the project directory itself stays clean.
fn resolve_storage(cwd: &Path) -> Result<PathBuf, String> {
    let home = env_string("HOME");
    let state_dir = env_string("PI_PLAN_STATE_DIR");
    ProjectStorage::resolve(home.as_deref(), state_dir.as_deref(), cwd)
}

async fn cmd_status(cwd: &Path) -> Result<u8, String> {
    let root = resolve_storage(cwd)?;
    let content = read_todo_file(cwd);
    let todo = parse_plan(&content);
    if todo.rows.is_empty() {
        return Err(format!("{}/TODO.md has no rows", cwd.to_string_lossy()));
    }
    let git = GitCommands::new(cwd);
    let subjects = git.subjects();
    let state = read_state_file(&root);
    let dirty = git.status_short().len();
    // D8: the status report surfaces how many session grants are on file
    // (the `reset-permissions` command clears them). A missing store reads
    // as a healthy empty one (no stderr noise); a corrupt one warns once.
    let grant_count = load_permissions(&root).grants.len();
    let root_label = root.to_string_lossy().into_owned();
    let lines = format_status_report(
        cwd.to_string_lossy().into_owned().as_str(),
        root_label.as_str(),
        todo.source.as_deref(),
        &todo.rows[..],
        &subjects,
        state.as_ref(),
        StatusFoot {
            dirty_lines: dirty,
            grant_count,
        },
    );
    for line in lines {
        println!("{line}");
    }
    Ok(0)
}

async fn cmd_stop(cwd: &Path) -> Result<u8, String> {
    let root = resolve_storage(cwd)?;
    write_stop_request(&root);
    println!("pi-plan: stop requested — consumed at the supervise loop's next boundary");
    if let Some(state) = read_state_file(&root) {
        println!(
            "  row {} · runs used {} · last outcome {}",
            state.current_row, state.runs_used, state.last_outcome
        );
    } else {
        let state_path = format!("{}/{}", root.to_string_lossy(), STATE_FILE_NAME);
        println!("  no state file (nothing running) — expected at {state_path}");
    }
    Ok(0)
}

async fn cmd_mark(cwd: &Path, row: u64, _done: MarkWord) -> Result<u8, String> {
    let root = resolve_storage(cwd)?;
    let content = read_todo_file(cwd);
    let todo = parse_plan(&content);
    mark_done(cwd, &root, &todo, row)?;
    println!("pi-plan: row {row} marked done (adjudicated)");
    Ok(0)
}

/// Clear every stored project permission (D6): `reset-permissions [--yes]`.
/// Without `--yes` the operator confirms on stdin (an EOF or a non-`yes`
/// answer cancels — the destructive default is deny). The corrupt-store
/// case has no readable grants, but the reset still removes the unreadable
/// file so the next run starts clean. Always exits 0 on a completed (or
/// cancelled) reset; a failed state-root resolution is a hard error.
async fn cmd_reset_permissions(cwd: &Path, yes: bool) -> Result<u8, String> {
    let root = resolve_storage(cwd)?;
    let store = load_permissions(&root);
    if store.health == StoreHealth::Corrupt {
        // A corrupt store is unreadable (its warn line already went to
        // stderr) — reset still clears the file (a repair by removal).
        clear_permissions(&root);
        println!("pi-plan: removed a corrupt permissions store (no readable grants)");
        return Ok(0);
    }
    let count = store.grants.len();
    if count == 0 {
        println!("pi-plan: no stored permission grants to reset");
        return Ok(0);
    }
    if !yes {
        print!("pi-plan: remove {count} stored permission grant(s)? type `yes` to confirm: ");
        let Some(answer) = stdin_read_line().await else {
            println!("pi-plan: reset cancelled (no confirmation)");
            return Ok(0);
        };
        if answer.trim().to_lowercase() != "yes" {
            println!("pi-plan: reset cancelled — nothing removed");
            return Ok(0);
        }
    }
    clear_permissions(&root);
    println!("pi-plan: removed {count} stored permission grant(s)");
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
    theme_path: Option<&Path>,
) -> Result<u8, String> {
    let root = resolve_storage(cwd)?;
    let root_label = root.to_string_lossy().into_owned();
    // A stale stop request from a killed run must not be inherited.
    clear_stop_request(&root);

    let env_home = env_string("HOME");
    let env_xdg_config_home = env_string("XDG_CONFIG_HOME");
    let config = resolve_config(
        cwd,
        config_path,
        env_xdg_config_home.as_deref(),
        env_home.as_deref(),
    );
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

    let session_dir = root.join("sessions");
    if let Err(err) = (|| -> std::io::Result<()> {
        fs::create_dir_all(&session_dir)?;
        Ok(())
    })() {
        return Err(format!(
            "cannot create the run directory {}: {err}",
            session_dir.to_string_lossy()
        ));
    }
    let stderr_log = root.join("worker-stderr.log");

    let env_persona = env_string("PI_PLAN_PERSONA");
    let env_skill = env_string("PI_PLAN_SKILL");
    let env_clean_skill = env_string("PI_PLAN_CLEAN_SKILL");
    let env_permission_ext = env_string("PI_PLAN_PERMISSION_EXTENSION");
    // The control flags are shared with the spawned operator-UI tasks
    // (the input task's ^C watcher, the stop watcher, the tails); the
    // command's own borrow (`control.as_ref()`) feeds the supervise loop.
    let control: Arc<RunControl> = Arc::new(RunControl::new());
    // The Ctrl-D kill-switch flag: armed once by the input task, consumed
    // by `kill_watcher` (spawned once `workers` exists below). Line mode
    // never arms it — Ctrl-D there stays EOF.
    let kill: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // --- TUI session (step 5/6) ---
    //
    // Full-screen alternate-buffer TUI when stdin + stdout are ttys,
    // byte-exact line mode otherwise. The render task OWNS the terminal
    // for the whole run (entered here, restored on every exit path); a
    // failed enter falls back to line mode. The resolved active palette
    // drives the frame builders (`--theme` > settings > bundled).
    let palette = resolve_active_palette(
        &select_source(theme_path, settings_theme(env_home.as_deref()).as_deref()),
        &theme_roots(cwd, env_home.as_deref()),
    );
    let tui_state: Arc<tokio::sync::Mutex<TuiState>> =
        Arc::new(tokio::sync::Mutex::new(TuiState::new()));
    let (banner_tx, banner_rx) = broadcast::channel::<String>(1024);
    let (render_exited_tx, mut render_exited_rx) = broadcast::channel::<()>(4);
    let render_stop = Arc::new(AtomicBool::new(false));
    let mut tui_active = choose_mode(TtyFaces {
        // The termios probe is the repo's own tty test (see the backend
        // smoke test): `tcgetattr` succeeds exactly on a terminal.
        stdin: nix::sys::termios::tcgetattr(std::io::stdin()).is_ok(),
        stdout: nix::sys::termios::tcgetattr(std::io::stdout()).is_ok(),
        stderr: nix::sys::termios::tcgetattr(std::io::stderr()).is_ok(),
    }) == DisplayMode::Tui;
    if tui_active {
        let sink: Box<AnsiSink<'static>> = Box::new(move |bytes: &[u8]| {
            let _ = nix::unistd::write(std::io::stdout(), bytes);
        });
        let mut terminal = Terminal::new(sink);
        match terminal.enter() {
            Err(_) => {
                tui_active = false; // cannot enter: line-mode fallback
            }
            Ok(captured) => {
                let size = terminal.query_size(DEFAULT_SIZE);
                let (_, resize_fallback) = broadcast::channel::<()>(2);
                let resize_rx = match watch_resizes() {
                    Ok((_watcher, rx)) => rx,
                    Err(_) => resize_fallback,
                };
                tokio::spawn(render_task(
                    terminal,
                    captured,
                    size,
                    tui_state.clone(),
                    palette.clone(),
                    RenderFeed {
                        banners: banner_rx,
                        resizes: resize_rx,
                        stop: render_stop.clone(),
                        exited_tx: render_exited_tx.clone(),
                    },
                ));
                // Step 6: ONE stdin owner — the input task reads raw keys
                // and dispatches modal lines; its ^C flag is mirrored into
                // the run control so the TUI unwinds on that path too, and
                // its ^D flag arms the kill switch (the `kill_watcher`
                // spawned below turns it into a SIGKILL + run teardown).
                let ctrl_c = Arc::new(AtomicBool::new(false));
                tokio::spawn(input_task(tui_state.clone(), ctrl_c.clone(), kill.clone()));
                tokio::spawn(ctrl_c_watcher(ctrl_c.clone(), control.clone()));
            }
        }
    }
    let hooks: Option<TuiHooks> = if tui_active {
        Some(TuiHooks {
            state: tui_state.clone(),
        })
    } else {
        None
    };

    let persona = match resolve_persona_path(cwd, env_persona.as_deref()) {
        Some(path) => fs::read_to_string(path).unwrap_or_default(),
        None => String::new(),
    };
    let skill = resolve_skill_path(env_skill.as_deref(), env_home.as_deref());
    // Hard-required: a supervised run fails fast before any worker spawns when
    // the skill is missing, unreadable, or has unresolvable frontmatter.
    let skill_body = load_skill_body(skill.as_deref())?;
    // Hard-required, like the skill: workers spawn bare (`--no-extensions`)
    // with the permission system as the only loaded extension, so an
    // unresolvable extension fails the run before any worker spawns.
    let permission_ext =
        resolve_permission_extension(env_permission_ext.as_deref(), env_home.as_deref())?;

    // Step 4: the shared permission store (loaded once; the step-5
    // keep/reset prompt reuses it) and the always-grant environment the
    // dialog proxy decides against. Every worker tail reads and records
    // through the same Arc-guarded store, so precedents survive across
    // workers and runs.
    let store = Arc::new(tokio::sync::Mutex::new(load_permissions(root.as_path())));
    let perm_env = Arc::new(PermissionEnv::from_env(
        cwd,
        env_home.as_deref(),
        env_skill.as_deref(),
        env_clean_skill.as_deref(),
    ));

    // The interactive driver owns the carried answers and the row-vs-clean
    // routing (plan step 7); the pause seam below is the CLI's only
    // interactive surface — in-TUI modal in TUI mode, the byte-exact
    // stdout prompt in line mode. Hoisted above the keep/reset prompt so
    // the start-of-run question shares the exact same surface (`status` is
    // never offered there — D12 — but Stop/Status reuse the seam logic).
    let git = GitCommands::new(cwd);
    let status_report = || {
        for line in format_status_report(
            cwd.to_string_lossy().into_owned().as_str(),
            root_label.as_str(),
            todo.source.as_deref(),
            &todo.rows[..],
            &git.subjects(),
            read_state_file(&root).as_ref(),
            // Re-read the store's persisted count per call so a mid-run
            // `status` reflects grants recorded since the run started (the
            // dialog proxy persists each human grant immediately).
            StatusFoot {
                dirty_lines: git.status_short().len(),
                grant_count: load_permissions(root.as_path()).grants.len(),
            },
        ) {
            println!("{line}");
        }
    };
    let pause = CliQuestionPause {
        tui_state: tui_state.clone(),
        control: control.clone(),
        tui_active,
        status: &status_report,
    };

    // Step 5: start-of-run keep/reset prompt (D6/D12). Only a HEALTHY
    // store with ≥ 1 grant prompts; a corrupt store skips the prompt and
    // surfaces one warning line in the final report instead; an empty
    // store has nothing to reset.
    let (store_health, grant_count) = {
        let guard = store.lock().await;
        let store_ref: &PermissionStore = &guard;
        (store_ref.health, store_ref.grants.len())
    };
    let store_was_corrupt = store_health == StoreHealth::Corrupt;
    if store_health == StoreHealth::Healthy && grant_count > 0 {
        let question = render_keep_reset_prompt(grant_count);
        let outcome = pause.pause(question.as_str()).await;
        // The line-mode `stop` path surfaces as `NoAnswer` plus the stop
        // flag (the pause only reports the answer); TUI `Stop`/Ctrl-C
        // surface as `Stopped`; TUI `Restart` surfaces as `NoAnswer` plus
        // the restart flag — all decoded here into the pure verdict.
        let mut answered: Option<String> = None;
        let mut stopped = false;
        let mut restarted = false;
        match outcome {
            PauseOutcome::Stopped { .. } => stopped = true,
            PauseOutcome::Answer(answer) => answered = Some(answer),
            PauseOutcome::NoAnswer => {
                stopped = control.as_ref().stop_requested.load(Ordering::SeqCst);
                restarted = control.as_ref().restart_requested.load(Ordering::SeqCst);
            }
        }
        match keep_reset_verdict(answered.as_deref(), stopped, restarted) {
            KeepResetVerdict::Stop => {
                // D12: `stop` cancels the run start — restore the TUI
                // (nothing else started: no workers, no tails) and exit 2.
                if tui_active {
                    render_stop.store(true, Ordering::SeqCst);
                    let _ =
                        tokio::time::timeout(Duration::from_secs(3), render_exited_rx.recv()).await;
                }
                return Ok(2);
            }
            KeepResetVerdict::Reset => {
                let mut guard = store.lock().await;
                let store_mut: &mut PermissionStore = &mut guard;
                let removed = store_mut.grants.len();
                clear_permissions(root.as_path());
                store_mut.grants = Vec::new();
                store_mut.health = StoreHealth::Healthy;
                eprintln!("pi-plan: permissions reset ({removed} grant(s) removed)");
            }
            KeepResetVerdict::Keep => {
                // A TUI `restart` at the startup prompt refers to a worker
                // that does not exist — keep-and-proceed; clear the flag so
                // the run loop does not misread a restart request.
                if restarted {
                    control
                        .as_ref()
                        .restart_requested
                        .store(false, Ordering::SeqCst);
                }
            }
        }
    }

    let skill_ref: Option<&Path> = skill.as_deref();
    let workers = RpcWorker::system(cwd, Some(stderr_log.clone()));
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
    // The closures capture the resolved root by reference (a `&Path`, like
    // the old `cwd`), so every recover/save/clear lands in the external
    // run-state root instead of the project directory.
    let root_path: &Path = root.as_path();
    // The row-terminal hook captures its OWN clone of the shared state
    // (the `move` closure would move the outer one, which the render
    // loop's spawned tasks still use).
    let tui_terminal_state = tui_state.clone();
    let services = SuperviseServices {
        git: &git,
        workers: &workers,
        config: &config.config,
        cwd,
        session_dir: &session_dir,
        stderr_path: Some(stderr_log.as_path()),
        persona: persona.as_str(),
        skill_path: skill_ref,
        permission_extension: permission_ext.as_path(),
        skill_body: Some(skill_body.as_str()),
        // The clean-worktree skill is resolved lazily, only when the dirty
        // gate would abort — constructing this closure cannot fail, and a
        // healthy run never touches it. The closure returns the resolved
        // path and body together so the gate can feed both the `--skill`
        // argv entry and the clean prompt from one call.
        clean_skill: Some(Box::new(move || {
            match resolve_clean_skill_path(env_clean_skill.as_deref(), env_home.as_deref()) {
                Some(dir) => load_clean_skill_body(dir.as_path()),
                None => None,
            }
        })),
        recover_state: Box::new(move || {
            recover_state(root_path, todo_content.as_str(), is_done_at)
        }),
        save_state: Box::new(move |st: &SupervisorState| save_state_file(root_path, st)),
        clear_state: Box::new(move || clear_state_file(root_path)),
        adjudicated: Some(Box::new(move || {
            read_state_file(root_path)
                .map(|s| s.adjudicated)
                .unwrap_or_default()
        })),
        report: if tui_active {
            Some(Box::new(move |_kind: ReportKind, line: &str| {
                // TUI mode: the banner/report seam feeds the shared ring
                // (drained by the render task), so no observable output is
                // dropped and nothing spams stderr mid-run.
                let _ = banner_tx.send(line.to_string());
            }))
        } else {
            Some(Box::new(move |_kind: ReportKind, line: &str| {
                eprintln!("{line}")
            }))
        },
        on_spawn: Some(Box::new(move |spawned_row: &TodoRow, agent: String| {
            let _ = spawned_tx.send((agent.parse::<u64>().unwrap_or(0), spawned_row.number));
        })),
        // The row-terminal hook (step 4): the same seam `report_terminal`
        // uses, so line mode and TUI mode both get it. TUI mode stores
        // the terminal and flips the displayed worker view not-live —
        // the only path that can deliver the terminal to the state (the
        // worker's event channel closes ~250 ms after the terminal, far
        // short of the quiet cadence). Line mode has no persistent
        // stats line and leaves it unset.
        on_row_terminal: if tui_active {
            Some(Box::new(move |row: u64, label: &str| {
                // Sync seam: the hook fires inside the run loop's
                // synchronous report call, so the lock is a non-blocking
                // try — the render task's guards are short-lived, and the
                // report half of the terminal already landed regardless.
                if let Ok(mut guard) = tui_terminal_state.try_lock() {
                    guard.note_row_terminal(row, label)
                }
            }))
        } else {
            None
        },
        // Durability seam: one JSONL stats record per run attempt, written
        // under the resolved run-state root. The builder skips
        // snapshot-less runs, so question pauses and abort-before-stats
        // write nothing (the audit log has no all-null rows).
        append_stats: Some(Box::new(move |record: &RunRecord| {
            if let Some(stats) = worker_stats_from_run(record) {
                append_worker_stats(root_path, &stats);
            }
        })),
        control: Some(control.as_ref()),
        await_terminal_timeout: None,
    };

    // Stop-request watcher: `pi-plan stop` from another shell lands here.
    tokio::spawn(stop_watcher(root.to_path_buf(), control.clone()));
    // Ctrl-D kill watcher (fix 3): on the input task's `^D` flag it
    // records the kill in the run control and SIGKILLs every
    // supervise-spawned worker process group. `kill_requested` is set
    // BEFORE `stop_requested` (review F1): the loop's boundary check then
    // blocks any new spawn before the kill lands, and the interrupt check
    // classifies the killed workers' `ProcessExit` as stopped/aborted
    // rather than an unforced failure. Poll cadence ~50 ms, so at most one
    // spawn can slip in inside that single window; the teardown dispose in
    // `cmd_supervise` closes the last gap.
    tokio::spawn(kill_watcher(kill.clone(), control.clone(), workers.clone()));
    // Live tail: worker events → the TUI ring (or stderr in line mode);
    // dialogs → in-TUI modals (or the byte-exact stdout round trip in
    // line mode). Clones of the port/control/config/plan/hooks are
    // Send/`'static`.
    tokio::spawn(tail_task(
        workers.clone(),
        control.clone(),
        spawned_rx,
        config.config.clone(),
        todo.clone(),
        hooks.clone(),
        DialogProxy {
            store: store.clone(),
            env: perm_env.clone(),
            root: root.to_path_buf(),
        },
    ));

    // The interactive driver owns the carried answers and the row-vs-clean
    // routing (plan step 7); the pause seam (hoisted above the keep/reset
    // prompt) is the CLI's only interactive surface — in-TUI modal in TUI
    // mode, the byte-exact stdout prompt in line mode.
    let final_result = run_plan_interactive(&services, &plan, answer, None, &pause).await;

    // Kill every live worker; transcripts survive in --session-dir.
    workers.dispose().await;

    // Drop the alternate screen + raw stdin BEFORE the final report so it
    // lands on the primary buffer exactly as in line mode (alt-screen
    // output would be discarded when the alternate buffer is dropped).
    // Wait (bounded) for the render task's restore ack.
    if tui_active {
        render_stop.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(Duration::from_secs(3), render_exited_rx.recv()).await;
    }

    match final_result {
        Some(result) => {
            for line in format_final_report(&result.outcomes[..], plan.rows.len()) {
                eprintln!("{line}");
            }
            if store_was_corrupt {
                // D12: a corrupt store skipped the keep/reset prompt — say
                // so once in the final report, so nothing auto-approved
                // silently for the whole run.
                eprintln!(
                    "  warning: permissions.json is corrupt — treated as empty \
(no auto-approvals, no keep/reset prompt)"
                );
            }
            Ok(if result.all_done { 0 } else { 2 })
        }
        None => Err("supervise ended without a result".to_string()),
    }
}

/// The CLI's `QuestionPause` seam (plan step 7): opens the in-TUI modal
/// (TUI mode) or the byte-exact stdout prompt (line mode) and maps the
/// operator's verdict onto [`PauseOutcome`]. `status` re-prints the live
/// status report and re-prompts in line mode (never surfaced to the
/// driver). The `^D` kill-switch nuance is read through [`stop_was_kill`]
/// at `Stop` time — the kill watcher records `kill_requested` before the
/// modal closes with `Stop`, so the flag is set exactly when the caller
/// used to observe it.
struct CliQuestionPause<'a> {
    tui_state: Arc<tokio::sync::Mutex<TuiState>>,
    control: Arc<RunControl>,
    tui_active: bool,
    status: &'a dyn Fn(),
}

impl QuestionPause for CliQuestionPause<'_> {
    async fn pause(&self, question: &str) -> PauseOutcome {
        if self.tui_active {
            {
                let mut guard = self.tui_state.lock().await;
                let state_mut: &mut TuiState = &mut guard;
                state_mut.open_modal(Modal::Ask(question.to_string()), None);
            }
            match await_modal_outcome(self.tui_state.clone()).await {
                Some(ModalOutcome::AskAnswer(Some(answer))) => PauseOutcome::Answer(answer),
                Some(ModalOutcome::AskAnswer(None)) => PauseOutcome::NoAnswer,
                Some(ModalOutcome::Stop) => {
                    self.control.stop_requested.store(true, Ordering::SeqCst);
                    PauseOutcome::Stopped {
                        kill: stop_was_kill(self.control.as_ref()),
                    }
                }
                Some(ModalOutcome::Restart) => {
                    self.control.restart_requested.store(true, Ordering::SeqCst);
                    PauseOutcome::NoAnswer
                }
                _ => PauseOutcome::NoAnswer,
            }
        } else {
            let mut done = false;
            let mut verdict: PauseOutcome = PauseOutcome::NoAnswer;
            while !done {
                for line in ask_lines(question) {
                    println!("{line}");
                }
                let Some(input) = stdin_read_line().await else {
                    break; // EOF: no answer — stop here
                };
                match line_command(&input) {
                    Some(LineCommand::Stop) => {
                        self.control.stop_requested.store(true, Ordering::SeqCst);
                        done = true;
                    }
                    Some(LineCommand::Status) => {
                        (self.status)();
                    }
                    _ => {
                        if input.trim().is_empty() {
                            done = true; // blank line: stop here
                        } else {
                            verdict = PauseOutcome::Answer(input);
                            done = true;
                        }
                    }
                }
            }
            verdict
        }
    }
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

/// Polls for the stop-request file under the run-state `root` and flips the
/// loop's flags.
async fn stop_watcher(root: PathBuf, control: Arc<RunControl>) {
    loop {
        if stop_request_present(&root) {
            control
                .as_ref()
                .stop_requested
                .store(true, Ordering::SeqCst);
            clear_stop_request(&root);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Mirrors the TUI input task's `^D` kill flag into the run control and
/// then SIGKILLs every supervise-spawned worker process group: `dispose`
/// → `RpcClient::kill` → `killpg` only on the groups `RpcClient::spawn`
/// created (`process_group(0)`), so unrelated Pi sessions are untouched by
/// construction. Flag order matters (plan review F1): `kill_requested`
/// (the ^D disambiguation record) is written first, then `stop_requested`
/// — the loop's boundary check blocks any new spawn before the kill
/// lands, and the interrupt check classifies the killed workers'
/// `ProcessExit` as stopped/aborted instead of an unforced failure that
/// would spend budget and mislabel the report row. The supervise process
/// itself does not die here: the run unwinds on the normal stop path,
/// prints the final report, and exits 2.
async fn kill_watcher(kill: Arc<AtomicBool>, control: Arc<RunControl>, workers: RpcWorker) {
    loop {
        if kill.load(Ordering::SeqCst) {
            control
                .as_ref()
                .kill_requested
                .store(true, Ordering::SeqCst);
            control
                .as_ref()
                .stop_requested
                .store(true, Ordering::SeqCst);
            workers.dispose().await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Mirrors the TUI input task's `^C` flag into the run control: in raw
/// mode `^C` is a key byte (`cfmakeraw` cleared `ISIG`), so the input
/// task flags it and this watcher turns it into a graceful stop — the
/// run unwinds on the normal stop path (terminal restored, final report
/// printed) instead of dying with the terminal left raw.
async fn ctrl_c_watcher(ctrl_c: Arc<AtomicBool>, control: Arc<RunControl>) {
    loop {
        if ctrl_c.load(Ordering::SeqCst) {
            control
                .as_ref()
                .stop_requested
                .store(true, Ordering::SeqCst);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------- theme resolution (step 1 pipeline, live filesystem) ----------------

/// The Pi settings file's `theme` value (`~/.pi/agent/settings.json`),
/// best-effort: `None` when HOME is unknown or the file is unreadable,
/// unparseable, or unset.
fn settings_theme(home: Option<&str>) -> Option<String> {
    match home {
        Some(h) => {
            let path = Path::new(h).join(".pi").join("agent").join("settings.json");
            read_settings_theme(&path)
        }
        None => None,
    }
}

/// The npm-style package names (`@scope/name`) from the settings
/// `packages` array — the installed theme package dirs are probed from
/// these. Best-effort: a missing/malformed settings file yields none.
fn settings_packages(home: Option<&str>) -> Vec<String> {
    let Some(h) = home else {
        return Vec::new();
    };
    let path = Path::new(h).join(".pi").join("agent").join("settings.json");
    let raw = fs::read_to_string(&path).unwrap_or_default();
    let parsed: Option<serde_json::Value> = serde_json::from_str(raw.as_str()).ok();
    let Some(value) = parsed else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let Some(packages) = object.get("packages") else {
        return Vec::new();
    };
    let Some(entries) = packages.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        let Some(rest) = entry.as_str().and_then(|s| s.strip_prefix("npm:")) else {
            continue;
        };
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }
    out
}

/// Inject the real theme roots (step 1's named resolution on the live
/// filesystem): the global `~/.pi/agent/themes` dir, the project's
/// `.pi/themes` when present, and the installed theme packages named in
/// the settings `packages` array (probed read-only — the std fs surface
/// has no directory enumeration, so a missing dir is simply skipped).
/// Built-ins (`<pi>/dist/modes/interactive/theme`) are a v1.1 follow-up;
/// the bundled gruvbox-dark fallback covers an unsearchable name.
fn theme_roots(cwd: &Path, home: Option<&str>) -> ThemeRoots {
    let Some(h) = home else {
        return ThemeRoots {
            global_dir: Path::new("").to_path_buf(),
            builtin_dirs: Vec::new(),
            project_dir: None,
            package_dirs: Vec::new(),
        };
    };
    let agent_dir = Path::new(h).join(".pi").join("agent");
    let mut package_dirs: Vec<PathBuf> = Vec::new();
    for package in settings_packages(home) {
        let dir = agent_dir
            .join("npm")
            .join("node_modules")
            .join(package.as_str())
            .join("themes");
        if dir.exists() {
            package_dirs.push(dir.to_path_buf());
        }
    }
    let project = cwd.join(".pi").join("themes");
    ThemeRoots {
        global_dir: agent_dir.join("themes").to_path_buf(),
        builtin_dirs: Vec::new(),
        project_dir: if project.exists() {
            Some(project.to_path_buf())
        } else {
            None
        },
        package_dirs,
    }
}

// ---------------- live tail ----------------

/// The shared permission context every worker tail's dialog proxy needs:
/// the Arc-guarded store (read for pre-arm, recorded on human grants), the
/// always-grant environment, and the run-state root for persistence.
struct DialogProxy {
    store: Arc<tokio::sync::Mutex<PermissionStore>>,
    env: Arc<PermissionEnv>,
    root: PathBuf,
}

/// Waits for spawn notifications and tails one monitor task per worker.
/// In TUI mode each spawn also seeds the header's plan meta from the FULL
/// plan (once per row) before the tail starts. The shared permission proxy
/// rides along so every tail can auto-approve/record the dialogs it relays
/// (steps 4-5).
async fn tail_task(
    workers: RpcWorker,
    control: Arc<RunControl>,
    mut spawned: broadcast::Receiver<(u64, u64)>,
    config: SupervisorConfig,
    todo: TodoPlan,
    hooks: Option<TuiHooks>,
    proxy: DialogProxy,
) {
    loop {
        let (worker_id, row_number) =
            match tokio::time::timeout(Duration::from_hours(24), spawned.recv()).await {
                Ok(Ok(pair)) => pair,
                Err(_) | Ok(Err(_)) => break,
            };
        if let Some(h) = hooks.as_ref()
            && let Some(index) = todo.rows.iter().position(|r| r.number == row_number)
        {
            let mut guard = h.state.lock().await;
            let state_mut: &mut TuiState = &mut guard;
            state_mut.set_plan(
                (index + 1) as u64,
                todo.rows.len() as u64,
                todo.rows[index].logical_unit.clone(),
                todo.source.clone(),
            );
        }
        // Seed the idle footer's row → logical-unit map once, from the
        // FULL plan this task already holds (single-row plan modes hold
        // a one-row plan, so the map degrades to a single entry — the
        // intended `next: row N` without a unit).
        if let Some(h) = hooks.as_ref() {
            let mut guard = h.state.lock().await;
            let state_mut: &mut TuiState = &mut guard;
            state_mut.seed_plan_units(
                todo.rows
                    .iter()
                    .map(|r| (r.number, r.logical_unit.clone()))
                    .collect::<Vec<(u64, String)>>(),
            );
        }
        tokio::spawn(worker_tail(
            workers.clone(),
            control.clone(),
            config.clone(),
            worker_id,
            row_number,
            hooks.clone(),
            DialogProxy {
                store: proxy.store.clone(),
                env: proxy.env.clone(),
                root: proxy.root.clone(),
            },
        ));
    }
}

async fn worker_tail(
    workers: RpcWorker,
    control: Arc<RunControl>,
    config: SupervisorConfig,
    worker_id: WorkerId,
    row_number: u64,
    hooks: Option<TuiHooks>,
    proxy: DialogProxy,
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
                    match hooks.as_ref() {
                        Some(h) => {
                            tui_update_view(h, &workers, worker_id, row_number, &config).await;
                        }
                        None => render_status(&workers, worker_id, row_number, &config).await,
                    }
                }
            }
            Ok(Err(_)) => break, // worker stream closed
            Ok(Ok(event)) => {
                match &event {
                    RpcEvent::MessageUpdate(delta) => {
                        if let Some(chunk) = apply_delta(&mut text, delta) {
                            match hooks.as_ref() {
                                Some(h) => {
                                    let mut guard = h.state.lock().await;
                                    let state_mut: &mut TuiState = &mut guard;
                                    // Flowing stream: raw kind + text via
                                    // the single classifier (F6); line
                                    // mode's per-chunk bytes untouched.
                                    match stream_part(delta) {
                                        Some((StreamKind::Text, raw)) => {
                                            state_mut.append_stream(LineKind::Text, raw.as_str())
                                        }
                                        Some((StreamKind::Thinking, raw)) => state_mut
                                            .append_stream(LineKind::Thinking, raw.as_str()),
                                        None => {}
                                    }
                                }
                                None => {
                                    eprintln!("{}", chunk.text);
                                    ring.push(chunk.text);
                                }
                            }
                        }
                    }
                    RpcEvent::ExtensionUiRequest(req) => {
                        if req.is_dialog() {
                            // Step 4 pre-arm: an always-grant or a stored
                            // precedent covers the ask → the supervisor
                            // auto-approves with a machine-generated reply
                            // (D10: never recorded), logs one visible line
                            // (D8), and skips the operator prompt. Never
                            // fires for non-permission asks — `auto_approval`
                            // only returns a reply when a session-grant
                            // label parses AND an always-grant/stored grant
                            // covers it.
                            let auto = {
                                let guard = proxy.store.lock().await;
                                let store_ref: &PermissionStore = &guard;
                                auto_approval(store_ref, proxy.env.as_ref(), req)
                            };
                            if let Some(reply) = auto {
                                let reply_text = match reply {
                                    AutoReply::PlainYes => "Yes".to_string(),
                                    AutoReply::SessionOption(opt) => opt,
                                };
                                let _ = workers
                                    .reply_extension_ui(
                                        worker_id,
                                        req.id.as_str(),
                                        &UiReply::Value(reply_text.clone()),
                                    )
                                    .await;
                                let line = format!("permission auto-approved: {reply_text}");
                                match hooks.as_ref() {
                                    Some(h) => {
                                        let mut guard = h.state.lock().await;
                                        let state_mut: &mut TuiState = &mut guard;
                                        state_mut.push_line(TuiLine {
                                            kind: LineKind::Banner,
                                            text: line,
                                        });
                                    }
                                    None => {
                                        eprintln!("{line}");
                                        ring.push(line.clone());
                                    }
                                }
                                continue;
                            }
                            // Step 6: in TUI mode the dialog is a modal —
                            // the render loop draws the box and the input
                            // task owns raw keys (dispatching through the
                            // unchanged `reply_from_input`/`line_command`);
                            // replies travel exactly as the line-mode
                            // round trip. Line mode keeps its byte-exact
                            // stdout prompt.
                            let (reply, stop) = match hooks.as_ref() {
                                Some(h) => {
                                    // Capture the pending tool call before
                                    // the dialog opens: the gate's
                                    // `tool_execution_start` (with `args`)
                                    // has already arrived by now, so a
                                    // snapshot read sees it in both modes.
                                    let pending = workers
                                        .snapshot(worker_id)
                                        .await
                                        .and_then(|s| s.pending_tool.as_ref().cloned());
                                    {
                                        let mut guard = h.state.lock().await;
                                        let state_mut: &mut TuiState = &mut guard;
                                        state_mut.open_modal(
                                            Modal::Dialog(req.clone()),
                                            pending.as_ref(),
                                        );
                                    }
                                    match await_modal_outcome(h.state.clone()).await {
                                        Some(ModalOutcome::DialogReply(reply)) => {
                                            let _ = workers
                                                .reply_extension_ui(
                                                    worker_id,
                                                    req.id.as_str(),
                                                    &reply,
                                                )
                                                .await;
                                            (Some(reply), false)
                                        }
                                        Some(ModalOutcome::Stop) => {
                                            control
                                                .as_ref()
                                                .stop_requested
                                                .store(true, Ordering::SeqCst);
                                            let _ = workers.abort(worker_id).await;
                                            (None, true)
                                        }
                                        Some(ModalOutcome::Restart) => {
                                            control
                                                .as_ref()
                                                .restart_requested
                                                .store(true, Ordering::SeqCst);
                                            let _ = workers.abort(worker_id).await;
                                            (None, true)
                                        }
                                        _ => (None, false),
                                    }
                                }
                                None => {
                                    dialog_roundtrip(
                                        &workers,
                                        control.clone(),
                                        worker_id,
                                        row_number,
                                        req,
                                        &config,
                                    )
                                    .await
                                }
                            };
                            // Step 4 recorder (D10): a HUMAN reply is
                            // recorded only when the operator chose a
                            // session-grant option on the select — the
                            // auto pre-arm never routes here, so machine
                            // approvals can never accrue precedents. Both
                            // TUI (modal outcome) and line mode (round
                            // trip) converge on this single spot.
                            if let Some(UiReply::Value(text)) = reply {
                                let mut guard = proxy.store.lock().await;
                                let store_mut: &mut PermissionStore = &mut guard;
                                let worker_label = format!("pi-plan-worker-{worker_id}");
                                if record_human_reply(
                                    store_mut,
                                    req,
                                    text.as_str(),
                                    worker_label.as_str(),
                                    now_epoch_ms().unwrap_or(0),
                                ) {
                                    save_permissions(proxy.root.as_path(), store_mut);
                                }
                            }
                            if stop {
                                break; // operator asked to stop/restart the worker
                            }
                        }
                    }
                    RpcEvent::TurnStart => match hooks.as_ref() {
                        Some(h) => {
                            tui_update_view(h, &workers, worker_id, row_number, &config).await;
                        }
                        None => render_status(&workers, worker_id, row_number, &config).await,
                    },
                    _ => {}
                }
                if let Some(line) = render_event_line(&event) {
                    match hooks.as_ref() {
                        Some(h) => {
                            let mut guard = h.state.lock().await;
                            let state_mut: &mut TuiState = &mut guard;
                            state_mut.push_line(line);
                        }
                        None => {
                            eprintln!("{}", line.text);
                            ring.push(line.text);
                        }
                    }
                }
            }
        }
    }
}

/// Refresh the shared TUI footer view from the worker's live snapshot —
/// the TUI-mode replacement for the status line (called on the quiet
/// cadence and at `turn start`, exactly where line mode prints).
async fn tui_update_view(
    hooks: &TuiHooks,
    workers: &RpcWorker,
    worker_id: WorkerId,
    row_number: u64,
    config: &SupervisorConfig,
) {
    let Some(snap) = workers.snapshot(worker_id).await else {
        return;
    };
    let now = now_epoch_ms().unwrap_or(snap.started_at);
    let mut guard = hooks.state.lock().await;
    let state_mut: &mut TuiState = &mut guard;
    state_mut.set_worker_view(view_from_snapshot(
        &snap,
        resolve_max_turns(config, row_number.max(1)),
        now,
    ));
}

/// The render loop's control set — everything it watches besides the
/// frame inputs: the banner seam, resize signals, the dialog gate, the
/// stop flag, and the exit ack. Bundled so the task signature stays small.
struct RenderFeed {
    /// The report seam lines that enter the ring in TUI mode.
    banners: broadcast::Receiver<String>,
    /// SIGWINCH notifications (from the signalfd bridge).
    resizes: broadcast::Receiver<()>,
    /// Set by `cmd_supervise` when the run is over.
    stop: Arc<AtomicBool>,
    /// Acked once the terminal is restored (the final report waits).
    exited_tx: broadcast::Sender<()>,
}

/// The 120 ms render loop: drains the banner seam into the ring,
/// refreshes the size on resize signals, and full-frame redraws from the
/// shared state (the modal overlay included). It OWNS the terminal
/// backend (entered by `cmd_supervise`), so it leaves the alternate
/// screen + raw stdin exactly once when the run ends and acks on
/// `exited_tx` so the final report waits for the restore.
async fn render_task(
    mut terminal: Terminal<'static>,
    saved: SavedTerminal,
    mut size: Size,
    state: Arc<tokio::sync::Mutex<TuiState>>,
    palette: Palette,
    mut feed: RenderFeed,
) {
    let active = true;
    loop {
        if feed.stop.load(Ordering::SeqCst) {
            break;
        }
        // The banner seam (report lines) feeds the ring in TUI mode, so
        // no observable output is dropped.
        while let Ok(Ok(line)) = tokio::time::timeout(Duration::ZERO, feed.banners.recv()).await {
            let mut guard = state.lock().await;
            let state_mut: &mut TuiState = &mut guard;
            state_mut.push_banner(line);
        }
        // Resize signals re-query the size — only while the TUI is live (a
        // DSR query would steal a dialog's keystrokes).
        if active && let Ok(Ok(_)) = tokio::time::timeout(Duration::ZERO, feed.resizes.recv()).await
        {
            size = terminal.query_size(size)
        }
        if active {
            let frame: Vec<StyledLine> = {
                let guard = state.lock().await;
                let state_view: &TuiState = &guard;
                compose_frame(&palette, state_view, size.cols, size.rows)
            };
            if !frame.is_empty() {
                // Full-frame redraw per tick (acceptable at these sizes,
                // like Pi's own fullRender). Rows beyond the drawn content
                // stay as the enter-time clear.
                let mut buf = String::new();
                let mut row: usize = 1;
                for line in frame.iter() {
                    buf.push_str(format!("\u{1b}[{row};1H{}", Stylize::fg(&line.fg)).as_str());
                    if let Some(bg) = &line.bg {
                        buf.push_str(Stylize::bg(bg).as_str());
                    }
                    if line.bold {
                        buf.push_str(Stylize::bold().as_str());
                    }
                    buf.push_str(line.text.as_str());
                    buf.push_str("\u{1b}[0m");
                    row += 1;
                }
                (terminal.sink)(buf.as_bytes());
            }
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    if active {
        terminal.leave(&saved);
    }
    let _ = feed.exited_tx.send(());
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
/// Returns `(reply, stop)`: the VALUE reply sent (when one was — the step-4
/// recorder persists human session grants from it; `None` when the dialog
/// was dismissed) and whether the operator asked to stop/restart the worker.
async fn dialog_roundtrip(
    workers: &RpcWorker,
    control: Arc<RunControl>,
    worker_id: WorkerId,
    row_number: u64,
    req: &ExtensionUiRequest,
    config: &SupervisorConfig,
) -> (Option<UiReply>, bool) {
    // Same snapshot read as the TUI modal path: the pending tool call
    // (its `tool_execution_start` already arrived) renders as the
    // `tool:` / `$ …` context rows in line mode too.
    let pending = workers
        .snapshot(worker_id)
        .await
        .and_then(|s| s.pending_tool.as_ref().cloned());
    for line in dialog_lines(req, pending.as_ref()) {
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
            return (None, false);
        };
        match line_command(&input) {
            Some(LineCommand::Stop) => {
                control
                    .as_ref()
                    .stop_requested
                    .store(true, Ordering::SeqCst);
                let _ = workers.abort(worker_id).await;
                return (None, true);
            }
            Some(LineCommand::Restart) => {
                control
                    .as_ref()
                    .restart_requested
                    .store(true, Ordering::SeqCst);
                let _ = workers.abort(worker_id).await;
                return (None, true);
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
            return (Some(reply), false);
        }
        println!("  invalid reply — try again (or ctrl-d to dismiss)");
    }
}
