use super::*;

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::cli::outcome_label;
use crate::config::{StepOverride, SupervisorConfig};
use crate::git::{MatchResult, MatchTier};
use crate::rpc::{RpcEvent, UiReply};
use crate::state::{SupervisorState, plan_hash_of};
use crate::todo::{TodoPlan, TodoRow};
use crate::tui::TuiState;
use crate::ui::LineKind;
use crate::worker::{TerminalEvent, Tokens, WorkerError, WorkerSnapshot, WorkerSpawnOpts};
use proptest::prelude::*;

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
    /// Whether a failing spawn surfaces as `WorkerError::Prompt` (the child
    /// started and could have written its own stderr) instead of the
    /// `WorkerError::Spawn` exec-class default (child never wrote a byte).
    spawn_error_prompt: bool,
    interrupt: Option<InterruptKind>,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            terminal: TerminalEvent::ProcessExit,
            text: String::new(),
            transcript: None,
            spawn_error: None,
            spawn_error_prompt: false,
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
    fn with<'a>(scripts: Vec<FakeScript>, control: Option<&'a RunControl>) -> FakeWorkerPort<'a> {
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
            return Err(if script.spawn_error_prompt {
                WorkerError::Prompt(err.clone())
            } else {
                WorkerError::Spawn(err.clone())
            });
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
            pending_tool: None,
            terminal: None,
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
/// lists, one per `subjects()` call, repeating the last entry. The dirty
/// sequence scripts `status --short` per call (last repeats): `true`
/// makes it non-empty — a clean pass needs the gate to see dirt, the
/// post-clean verification to see none, and the next row's gate dirt
/// again, so a plain constant bool is not enough.
#[derive(Debug)]
struct FakeGit {
    staged: Vec<Vec<String>>,
    calls: Cell<usize>,
    dirty_seq: Vec<bool>,
    dirty_calls: Cell<usize>,
}

impl FakeGit {
    fn with(staged: Vec<Vec<String>>, dirty: bool) -> Self {
        Self::with_seq(staged, vec![dirty])
    }

    fn with_seq(staged: Vec<Vec<String>>, dirty_seq: Vec<bool>) -> Self {
        assert!(
            !staged.is_empty(),
            "the staged sequence needs at least one element (the repeating tail)"
        );
        assert!(
            !dirty_seq.is_empty(),
            "the dirty sequence needs at least one element (the repeating tail)"
        );
        Self {
            staged,
            calls: Cell::new(0),
            dirty_seq,
            dirty_calls: Cell::new(0),
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
        let idx = self.dirty_calls.get().min(self.dirty_seq.len() - 1);
        self.dirty_calls.set(idx + 1);
        if self.dirty_seq[idx] {
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
    terminals: Vec<(u64, String)>,
    stats: Vec<RunRecord>,
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
        spawn_error_prompt: false,
        interrupt: None,
    }
}

fn ask_script(question: &str) -> FakeScript {
    FakeScript {
        terminal: TerminalEvent::Settled,
        text: format!("work done\nPI_WORKER_STATUS: ASK\nQUESTION: {question}"),
        transcript: None,
        spawn_error: None,
        spawn_error_prompt: false,
        interrupt: None,
    }
}

fn settled(text: &str) -> FakeScript {
    script(TerminalEvent::Settled, text)
}

fn failed(text: &str) -> FakeScript {
    script(TerminalEvent::ProcessExit, text)
}

/// A spawn that fails with the given error (Spawn-class, like the fake
/// default); the supervise layer respawns it up to `SPAWN_RETRY_LIMIT`
/// times and spends no budget.
fn broken_script(err: &str) -> FakeScript {
    let mut out = script(TerminalEvent::Settled, "unused");
    out.spawn_error = Some(err.to_string());
    out
}

/// A spawn that fails with a `Prompt`-class error — the child started,
/// parsed, and could have written its own stderr — exactly the class the
/// supervise layer treats as stderr-attributable.
fn prompt_broken_script(err: &str) -> FakeScript {
    let mut out = broken_script(err);
    out.spawn_error_prompt = true;
    out
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

static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Scratch directory for stub files (worker stderr logs); the OS temp dir
/// is used, so a leftover empty dir on test failure is benign.
fn temp_dir() -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("pi-plan-supervise-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write a stub file into a temp dir (worker stderr log content).
fn temp_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write stub");
    path
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
async fn run_row_spends_both_runs_and_asks_at_the_budget() {
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::BudgetChoice {
            runs_used,
            last_outcome,
            records,
            ..
        } => {
            assert_eq!(runs_used, 2);
            assert_eq!(last_outcome, "failed");
            assert_eq!(records.len(), 2);
        }
        other => panic!("expected BudgetChoice, got {other:?}"),
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
    // THREE failing spawns: the retry loop exhausts its bound and the row
    // stops with the distinct spawn-error outcome. A fourth (successful)
    // script beyond the bound is never consumed.
    let port = FakeWorkerPort::with(
        vec![
            broken_script("pi binary missing"),
            broken_script("pi binary missing"),
            broken_script("pi binary missing"),
            settled("unused"),
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::SpawnError {
            row: stopped_row,
            records,
        } => {
            assert_eq!(stopped_row.number, 1);
            assert_eq!(
                records.len(),
                3,
                "one record per actual spawn attempt, never a spent run"
            );
            for record in records {
                assert_eq!(record.outcome, RunOutcomeKind::SpawnError);
                assert_eq!(
                    record.attempt, 1,
                    "respawns share the budget attempt number"
                );
            }
        }
        other => panic!("expected SpawnError, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        3,
        "exactly three spawn attempts, bounded by SPAWN_RETRY_LIMIT"
    );
    let capture = shared_capture(&shared).await;
    assert_eq!(
        capture.saved.len(),
        1,
        "intermediate failures write no state; only the stop persists"
    );
    let saved = capture.saved.last().cloned().expect("saved");
    assert_eq!(saved.last_outcome, "spawn-error");
    assert_eq!(saved.runs_used, 0, "a spawn error spends no run budget");
}

#[tokio::test]
async fn run_row_spawn_error_keeps_a_preexisting_runs_used_unchanged() {
    // Over-grant edge, first half: a row with one REAL spent failure on
    // the books resumes with `runs_used = 1`; exhausted spawn retries then
    // stop WITHOUT spending another run, so the saved state still reads
    // `runs_used = 1` — it merely switches the outcome marker.
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    let persisted = state(1, 1, "failed");
    let port = FakeWorkerPort::with(
        vec![
            broken_script("boom"),
            broken_script("boom"),
            broken_script("boom"),
            settled("unused"),
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
        skill_body: None,
        recover_state: Box::new(move || Some(persisted.clone())),
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::SpawnError { .. } => {}
        other => panic!("expected SpawnError, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        3,
        "the row still got its spawn attempts under the gate"
    );
    let saved = shared_capture(&shared)
        .await
        .saved
        .last()
        .cloned()
        .expect("saved");
    assert_eq!(saved.runs_used, 1, "the real spend is untouched");
    assert_eq!(saved.last_outcome, "spawn-error");
}

#[tokio::test]
async fn run_row_resume_discounts_spawn_error_states_back_to_zero() {
    // Over-grant edge, second half: a legacy/post-fix state whose
    // `runsUsed: 1` came entirely from failed spawns discounts to ZERO on
    // resume, so the row gets its full budget again (the stale state
    // would otherwise hard-block the gate). This self-heals the tag_tool
    // state file and at worst over-grants one run.
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    let persisted = state(1, 1, "spawn-error");
    let port = FakeWorkerPort::with(
        vec![failed("nope"), failed("nope"), settled("unused")],
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
        skill_body: None,
        recover_state: Box::new(move || Some(persisted.clone())),
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::BudgetChoice {
            runs_used,
            last_outcome,
            ..
        } => {
            assert_eq!(runs_used, 2, "the full budget is restored");
            assert_eq!(last_outcome, "failed");
        }
        other => panic!("expected BudgetChoice, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        2,
        "both budgeted runs happened after the discount"
    );
}

#[tokio::test]
async fn read_stderr_tail_bounds_lines_and_chars() {
    let dir = temp_dir();
    // Missing and empty logs read as no tail.
    assert_eq!(
        read_stderr_tail(dir.join("missing.log").as_path(), 6, 400),
        None
    );
    let empty = temp_file(dir.as_path(), "empty.log", "");
    assert_eq!(read_stderr_tail(empty.as_path(), 6, 400), None);
    // Only the last lines survive the line bound.
    let log = temp_file(
        dir.as_path(),
        "log.log",
        "line 0\nline 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7",
    );
    let tail = read_stderr_tail(log.as_path(), 3, 400).expect("tail");
    assert_eq!(tail, "line 5\nline 6\nline 7");
    // The character bound still caps a short tail.
    let capped = read_stderr_tail(log.as_path(), 6, 9).expect("capped");
    assert_eq!(capped.chars().count(), 9, "bounded to max_chars");
    // A big log is read from the tail, never slurped whole: the head of a
    // 20-line log must not appear in a 3-line tail (reads are bounded by
    // the byte cap too, not just the line count).
    let big = temp_file(
        dir.as_path(),
        "big.log",
        (0..400)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            .as_str(),
    );
    let big_tail = read_stderr_tail(big.as_path(), 3, 400).expect("big tail");
    assert_eq!(big_tail, "line 397\nline 398\nline 399");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn run_row_spawn_error_surfaces_a_prompt_class_stderr_tail() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    // The shared worker-stderr log holds the child's own last words.
    let dir = temp_dir();
    let stderr_log = temp_file(
        dir.as_path(),
        "worker-stderr.log",
        "Error: Unknown options: --no-lsp, --no-lens, --no-tests\n",
    );
    // ALL three respawn attempts die a Prompt-class death (the child
    // started and wrote its own stderr: `Unknown options` before exiting).
    let port = FakeWorkerPort::with(
        vec![
            prompt_broken_script("the prompt command was rejected: peer closed the RPC stream"),
            prompt_broken_script("the prompt command was rejected: peer closed the RPC stream"),
            prompt_broken_script("the prompt command was rejected: peer closed the RPC stream"),
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
        stderr_path: Some(stderr_log.as_path()),
        persona: "You are a worker operating under a supervisor.",
        skill_path: None,
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::SpawnError { records, .. } => {
            assert_eq!(records.len(), 3);
            // Only the LAST attempt's record carries the stderr tail: the
            // log tail at failure time belongs to the final spawn.
            assert!(
                records[0]
                    .tail
                    .as_deref()
                    .is_some_and(|t| !t.contains("worker stderr (last lines)")),
                "earlier attempts append no stderr tail"
            );
            let last = records[2].tail.as_deref().expect("last tail");
            assert!(last.contains("worker stderr (last lines):"));
            assert!(
                last.contains("Error: Unknown options: --no-lsp"),
                "the child's own stderr reaches the report"
            );
        }
        other => panic!("expected SpawnError, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn run_row_spawn_error_omits_the_tail_for_exec_class_failures() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    // The log carries leftover lines, but every death is exec-class: the
    // child never wrote a byte, so the tail would be a PREVIOUS attempt's
    // stderr and must not be shown.
    let dir = temp_dir();
    let stderr_log = temp_file(
        dir.as_path(),
        "worker-stderr.log",
        "stale previous attempt\n",
    );
    let port = FakeWorkerPort::with(
        vec![
            broken_script("pi binary missing"),
            broken_script("pi binary missing"),
            broken_script("pi binary missing"),
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
        stderr_path: Some(stderr_log.as_path()),
        persona: "You are a worker operating under a supervisor.",
        skill_path: None,
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::SpawnError { records, .. } => {
            assert_eq!(records.len(), 3);
            for record in records {
                assert!(
                    record
                        .tail
                        .as_deref()
                        .is_some_and(|t| !t.contains("worker stderr")),
                    "exec-class failures never surface the stale log tail"
                );
            }
        }
        other => panic!("expected SpawnError, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, Some("Use the polars API."), None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    // attempt = 2 == budget, but a user-provided answer is a user-driven
    // continuation: it must still get its worker run.
    let outcome = run_row(&services, &row_1, Some("Keep going."), None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::BudgetChoice { runs_used, .. } => {
            assert_eq!(runs_used, 2);
        }
        other => panic!("expected BudgetChoice, got {other:?}"),
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::BudgetChoice {
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
        other => panic!("expected BudgetChoice, got {other:?}"),
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert_eq!(records.len(), 1);
            assert!(
                records[0]
                    .tail
                    .as_deref()
                    .is_some_and(|t| t.contains("not installed"))
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    for (marker, run) in [("dirty", 1), ("spawn-error", 2)] {
        let outcome = run_row(&services, &row_1, None, None).await;
        match outcome {
            RowOutcome::DirtyWorktree { records, .. } => {
                assert_eq!(records.len(), 1);
                assert!(
                    records[0]
                        .tail
                        .as_deref()
                        .is_some_and(|t| t.contains("not installed")),
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

// ------------------------------------------------------------------
// Clean-worktree agent at the dirty gate (Change 4/5)
// ------------------------------------------------------------------

/// A resolved clean-worktree skill pair (directory + body), injected
/// into `SuperviseServices.clean_skill` as if the seam resolved it.
fn clean_skill_installed() -> Box<CleanSkillFn<'static>> {
    Box::new(move || {
        Some((
            Path::new("/skills/clean-worktree").to_path_buf(),
            "## Goal\nMake `git status --short` empty again. Never destroy \
meaningful work; ask instead.\n"
                .to_string(),
        ))
    })
}

/// Services for the owner-less dirty-gate tests: an installed (or
/// absent) clean skill, a report/state capture, and no recovered state
/// (so every gate hit is owner-less).
fn clean_services<'a>(
    git: &'a FakeGit,
    port: &'a FakeWorkerPort<'a>,
    config: &'a SupervisorConfig,
    control: Option<&'a RunControl>,
    clean_skill: Option<Box<CleanSkillFn<'a>>>,
    skill_path: Option<&'a Path>,
    shared: Arc<tokio::sync::Mutex<Capture>>,
) -> SuperviseServices<'a, FakeGit, FakeWorkerPort<'a>> {
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    let report_cap = shared.clone();
    SuperviseServices {
        git,
        workers: port,
        config,
        cwd: Path::new("/repo"),
        session_dir: Path::new("/run/sessions"),
        persona: "You are a worker operating under a supervisor.",
        skill_path,
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control,
        clean_skill,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    }
}

/// `clean_services`-style harness for the resume-gate and budget-driver
/// tests: callers supply the recover/save closures directly, so recovery
/// can return a persisted state (or echo saves like the real state file)
/// instead of always `None`.
fn driver_services<'a>(
    git: &'a FakeGit,
    port: &'a FakeWorkerPort<'a>,
    config: &'a SupervisorConfig,
    control: Option<&'a RunControl>,
    recover_state: Box<RecoverFn<'a>>,
    save_state: Box<SaveStateFn<'a>>,
) -> SuperviseServices<'a, FakeGit, FakeWorkerPort<'a>> {
    SuperviseServices {
        git,
        workers: port,
        config,
        cwd: Path::new("/repo"),
        session_dir: Path::new("/run/sessions"),
        persona: "You are a worker operating under a supervisor.",
        skill_path: None,
        skill_body: None,
        recover_state,
        save_state,
        clear_state: Box::new(move || {}),
        adjudicated: None,
        report: None,
        on_spawn: None,
        on_row_terminal: None,
        append_stats: None,
        control,
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    }
}

#[tokio::test]
async fn clean_success_proceeds_to_the_row_worker_git_keyed() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Gate: dirty; post-clean verification: clean. Tripwire check sees an
    // unrelated history; the row worker's own commit then matches.
    let git = FakeGit::with_seq(
        vec![Vec::new(), vec!["feat: row one".to_string()]],
        vec![true, false],
    );
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![
            settled("cleaned target/\nPI_WORKER_STATUS: COMPLETE"),
            settled("row 1 complete\nPI_WORKER_STATUS: COMPLETE"),
        ],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let impl_path = Path::new("/skills/implement-from-plan");
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        Some(impl_path),
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::Done { records, .. } => {
            assert_eq!(records.len(), 2, "clean record + row record");
            assert_eq!(records[0].attempt, 0, "clean records use marker 0");
            assert_eq!(records[0].tail.as_deref(), Some("worktree cleaned"));
            assert_eq!(records[1].attempt, 1, "the row worker is the budgeted run");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 2, "clean agent, then the row worker");
    assert_eq!(spawned[0].opts.name, "pi-plan-clean-1");
    assert_eq!(spawned[1].opts.name, "pi-plan-row-1");
    assert_eq!(
        spawned[0].opts.skills,
        vec![
            Path::new("/skills/implement-from-plan").to_path_buf(),
            Path::new("/skills/clean-worktree").to_path_buf(),
        ],
        "both skills registered on the clean agent"
    );
    assert_eq!(
        spawned[1].opts.skills,
        vec![Path::new("/skills/implement-from-plan").to_path_buf()],
        "row workers keep the single implement-from-plan skill"
    );
    assert!(
        report_has(
            &shared_capture(&shared).await,
            ReportKind::Terminal,
            "working tree cleaned by 0",
        ),
        "the success report names the clean agent"
    );
    let capture = shared_capture(&shared).await;
    assert_eq!(capture.saved.len(), 1, "no state written by the clean pass");
    assert_eq!(capture.saved[0].last_outcome, "running");
    assert_eq!(capture.saved[0].runs_used, 0);
    assert_eq!(capture.cleared, 1, "done clears the state");
}

#[tokio::test]
async fn clean_complete_but_tree_still_dirty_fails_and_aborts() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Gate dirty and post-clean check still dirty: the marker alone
    // must never pass the git-keyed classifier.
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![settled("cleaned up\nPI_WORKER_STATUS: COMPLETE")],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].attempt, 0);
            assert!(
                records[0]
                    .tail
                    .as_deref()
                    .is_some_and(|t| t.contains("still dirty"))
            );
        }
        other => panic!("expected DirtyWorktree, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        1,
        "only the clean agent spawned — the row worker must not"
    );
    let capture = shared_capture(&shared).await;
    assert!(
        capture.saved.is_empty(),
        "the abort path writes no state (no inflation of runsUsed)"
    );
    assert_eq!(capture.cleared, 0);
}

#[tokio::test]
async fn clean_tripwire_newest_subject_matches_the_rows_planned_message() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // The clean agent committed the row's planned message: the tripwire
    // treats any matching subject as a clean-agent accident.
    let git = FakeGit::with_seq(vec![vec!["feat: row one".to_string()]], vec![true, false]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![settled("committed it all\nPI_WORKER_STATUS: COMPLETE")],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert!(
                records[0]
                    .tail
                    .as_deref()
                    .is_some_and(|t| t.contains("planned message")),
                "the tripwire aborts: got {records:?}"
            );
        }
        other => panic!("expected DirtyWorktree, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        1,
        "the row worker must never spawn after a tripwire"
    );
}

#[tokio::test]
async fn stop_mid_clean_is_a_user_stop_never_a_clean_failure() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![FakeScript {
            terminal: TerminalEvent::Settled,
            text: "half-cleaned".to_string(),
            transcript: None,
            spawn_error: None,
            spawn_error_prompt: false,
            interrupt: Some(InterruptKind::Stop),
        }],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    assert!(
        matches!(outcome, RowOutcome::Stopped { .. }),
        "a stop mid-clean is Stopped, never a clean-failure DirtyWorktree"
    );
    let capture = shared_capture(&shared).await;
    assert_eq!(capture.saved.len(), 1);
    assert_eq!(capture.saved[0].last_outcome, "stopped");
    assert_eq!(capture.cleared, 0);
}

#[tokio::test]
async fn ctrl_d_kill_mid_clean_is_a_stopped_not_a_failure() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![FakeScript {
            terminal: TerminalEvent::ProcessExit,
            text: String::new(),
            transcript: None,
            spawn_error: None,
            spawn_error_prompt: false,
            interrupt: Some(InterruptKind::Kill),
        }],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    assert!(
        matches!(outcome, RowOutcome::Stopped { .. }),
        "a ^D kill mid-clean is Stopped (operator interrupt wins)"
    );
    assert!(
        stop_was_kill(&control),
        "kill_requested is preserved for stop_was_kill"
    );
    let capture = shared_capture(&shared).await;
    assert_eq!(capture.saved.len(), 1);
    assert_eq!(capture.saved[0].last_outcome, "stopped");
}

#[tokio::test]
async fn restart_mid_clean_refires_the_gate_with_nothing_spent() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // gate dirty, re-fired gate still dirty, then the successful clean's
    // verification sees clean; the row worker then commits row one.
    let git = FakeGit::with_seq(
        vec![Vec::new(), vec!["feat: row one".to_string()]],
        vec![true, true, false],
    );
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![
            FakeScript {
                terminal: TerminalEvent::Settled,
                text: "aborting".to_string(),
                transcript: None,
                spawn_error: None,
                spawn_error_prompt: false,
                interrupt: Some(InterruptKind::Restart),
            },
            settled("cleaned\nPI_WORKER_STATUS: COMPLETE"),
            settled("row 1 complete\nPI_WORKER_STATUS: COMPLETE"),
        ],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    assert!(
        matches!(outcome, RowOutcome::Done { .. }),
        "after the restart the clean pass runs again and the row proceeds"
    );
    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 3, "clean, clean-again, then the row worker");
    assert_eq!(spawned[0].opts.name, "pi-plan-clean-1");
    assert_eq!(spawned[1].opts.name, "pi-plan-clean-1");
    assert_eq!(spawned[2].opts.name, "pi-plan-row-1");
    let capture = shared_capture(&shared).await;
    assert_eq!(capture.saved.len(), 1, "restart writes no state");
    assert_eq!(capture.saved[0].last_outcome, "running");
}

#[tokio::test]
async fn clean_ask_returns_a_clean_question_pause_with_the_question() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(vec![ask_script("may I discard target/?")], Some(&control));
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::CleanQuestionPause {
            question,
            agent_id,
            records,
            ..
        } => {
            assert_eq!(question, "may I discard target/?");
            assert_eq!(agent_id, "0");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].attempt, 0);
            assert_eq!(
                records[0].question.as_deref(),
                Some("may I discard target/?")
            );
        }
        other => panic!("expected CleanQuestionPause, got {other:?}"),
    }
    let capture = shared_capture(&shared).await;
    assert!(
        capture.saved.is_empty(),
        "a clean question pause writes no state (unlike a row question)"
    );
    assert_eq!(port.spawned().await.len(), 1);
}

#[tokio::test]
async fn clean_stuck_is_a_failure_abort_not_a_question() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![settled("cannot decide\nPI_WORKER_STATUS: STUCK")],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert!(
                records[0]
                    .tail
                    .as_deref()
                    .is_some_and(|t| t.contains("STUCK"))
            );
        }
        other => panic!("expected DirtyWorktree, got {other:?}"),
    }
}

#[tokio::test]
async fn clean_process_exit_and_missing_marker_fail_and_abort() {
    for (exit_script, tail) in [
        (script(TerminalEvent::ProcessExit, ""), "did not settle"),
        (
            settled("no marker here"),
            "missing a PI_WORKER_STATUS marker",
        ),
    ] {
        let row_1 = row(1, "feat: row one");
        let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
        let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
        let control = RunControl::new();
        let port = FakeWorkerPort::with(vec![exit_script.clone()], Some(&control));
        let config = SupervisorConfig::default();
        let services = clean_services(
            &git,
            &port,
            &config,
            Some(&control),
            Some(clean_skill_installed()),
            None,
            shared.clone(),
        );
        let outcome = run_row(&services, &row_1, None, None).await;
        match outcome {
            RowOutcome::DirtyWorktree { records, .. } => {
                assert!(
                    records[0].tail.as_deref().is_some_and(|t| t.contains(tail)),
                    "{tail}: got {records:?}"
                );
            }
            other => panic!("{tail}: expected DirtyWorktree, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn clean_spawn_error_fails_and_aborts() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    // The clean pass respawns like a row worker: three attempts, then the
    // DirtyWorktree abort with one record per spawn attempt.
    let port = FakeWorkerPort::with(
        vec![
            broken_script("boom"),
            broken_script("boom"),
            broken_script("boom"),
            settled("unused"),
        ],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert_eq!(
                records.len(),
                3,
                "one respawn record per actual spawn attempt"
            );
            for record in records {
                assert_eq!(record.outcome, RunOutcomeKind::SpawnError);
                assert!(
                    record
                        .tail
                        .as_deref()
                        .is_some_and(|t| t.contains("clean spawn failed"))
                );
            }
        }
        other => panic!("expected DirtyWorktree, got {other:?}"),
    }
}

#[tokio::test]
async fn clean_pass_opts_carry_the_stderr_path() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let dir = temp_dir();
    let stderr_log = temp_file(dir.as_path(), "worker-stderr.log", "stub\n");
    // Gate: dirty; post-clean verification: clean; the row worker then
    // completes.
    let git = FakeGit::with_seq(
        vec![Vec::new(), vec!["feat: row one".to_string()]],
        vec![true, false],
    );
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![
            settled("cleaned target/\nPI_WORKER_STATUS: COMPLETE"),
            settled("row 1 complete\nPI_WORKER_STATUS: COMPLETE"),
        ],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let mut services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    // The run's stderr log is threaded into every spawn's opts — clean
    // agents included — so their spawn-error records could surface it.
    services.stderr_path = Some(stderr_log.as_path());
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::Done { .. } => {}
        other => panic!("expected Done, got {other:?}"),
    }
    // The clean agent's OWN spawn opts carry the path, so its spawn-error
    // records could surface the tail exactly like row workers.
    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 2);
    let clean_path = spawned[0]
        .opts
        .stderr_path
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    assert_eq!(clean_path, Some(stderr_log.to_string_lossy().into_owned()));
    assert_eq!(
        spawned[1].opts.stderr_path, spawned[0].opts.stderr_path,
        "row workers AND clean agents share the run's stderr log"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_clean_skill_aborts_with_a_naming_tail() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(Vec::new(), Some(&control));
    let config = SupervisorConfig::default();
    // No clean skill seam at all (the graceful fallback posture).
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        None,
        None,
        shared.clone(),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert_eq!(records.len(), 1);
            assert!(records[0].tail.as_deref().is_some_and(|t| {
                t.contains("clean-worktree skill not installed")
                    && t.contains("PI_PLAN_CLEAN_SKILL")
            }));
        }
        other => panic!("expected DirtyWorktree, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        0,
        "nothing spawns without the skill"
    );
}

#[tokio::test]
async fn carried_clean_answer_is_folded_into_the_regenerated_prompt() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(
        vec![Vec::new(), vec!["feat: row one".to_string()]],
        vec![true, false],
    );
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![
            settled("cleaned with guidance\nPI_WORKER_STATUS: COMPLETE"),
            settled("row 1 complete\nPI_WORKER_STATUS: COMPLETE"),
        ],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let cont = CleanContinuation {
        question: "may I discard target/?".to_string(),
        answer: "yes — add target/ to .gitignore".to_string(),
    };
    let outcome = run_row(&services, &row_1, None, Some(&cont)).await;
    assert!(
        matches!(outcome, RowOutcome::Done { .. }),
        "the answered continuation lets the clean pass succeed"
    );
    let spawned = port.spawned().await;
    assert!(
        spawned[0]
            .prompt
            .contains("The human answered a previous clean-worktree agent's question:"),
        "the re-generated clean prompt folds the answer in"
    );
    assert!(
        spawned[0]
            .prompt
            .contains("> yes — add target/ to .gitignore")
    );
}

#[tokio::test]
async fn carried_clean_answer_with_a_clean_tree_is_an_orphan_abort() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // The tree is already clean (ASK-after-cleaning, or the human
    // cleaned manually while answering) — the gate cannot consume the
    // carried answer, and the run must not silently drop it.
    let git = FakeGit::with_seq(vec![Vec::new()], vec![false]);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(Vec::new(), Some(&control));
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let cont = CleanContinuation {
        question: "may I discard target/?".to_string(),
        answer: "yes".to_string(),
    };
    let outcome = run_row(&services, &row_1, None, Some(&cont)).await;
    match outcome {
        RowOutcome::CleanAnswerOrphaned { question, .. } => {
            assert_eq!(question, "may I discard target/?");
        }
        other => panic!("expected CleanAnswerOrphaned, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        0,
        "the anomaly fires before any spawn"
    );
    let capture = shared_capture(&shared).await;
    assert!(capture.saved.is_empty(), "the anomaly writes no state");
}

#[tokio::test]
async fn carried_clean_answer_never_reaches_a_later_rows_clean_prompt() {
    let two_rows = plan(vec![row(1, "feat: row one"), row(2, "feat: row two")]);
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(
        vec![
            Vec::new(),
            Vec::new(),
            vec!["feat: row one".to_string()],
            vec!["feat: row one".to_string()],
            vec!["feat: row one".to_string()],
            vec!["feat: row one".to_string(), "feat: row two".to_string()],
        ],
        vec![true, false, true, false],
    );
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![
            settled("cleaned row 1 dirt\nPI_WORKER_STATUS: COMPLETE"),
            settled("row 1 complete\nPI_WORKER_STATUS: COMPLETE"),
            settled("cleaned row 2 dirt\nPI_WORKER_STATUS: COMPLETE"),
            settled("row 2 complete\nPI_WORKER_STATUS: COMPLETE"),
        ],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let cont = CleanContinuation {
        question: "may I discard target/?".to_string(),
        answer: "yes — add target/ to .gitignore".to_string(),
    };
    let result = run_plan(&services, &two_rows, None, Some(&cont)).await;
    assert!(
        result.all_done,
        "both rows complete after the answered clean"
    );
    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 4, "clean, row1, clean, row2");
    assert!(
        spawned[0]
            .prompt
            .contains("The human answered a previous clean-worktree agent's question:"),
        "row 1's clean pass folds the carried answer in"
    );
    assert!(
        !spawned[2]
            .prompt
            .contains("The human answered a previous clean-worktree agent's question:"),
        "the answer lives exactly one continuation — row 2's clean prompt is clean"
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let _ = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
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
async fn on_row_terminal_receives_every_terminal_kind_including_retries() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    // First classification sees no commit; the retry's does — so the
    // SAME row fires the hook twice: `failed`, then `completed` (the
    // label is a terminal kind, not a row outcome).
    let git = FakeGit::with(vec![Vec::new(), vec!["feat: row one".to_string()]], false);
    let port = FakeWorkerPort::with(
        vec![failed("worker died mid-edit"), settled("row 1 complete")],
        None,
    );
    let term_cap = shared.clone();
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
        skill_body: None,
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
        on_row_terminal: Some(Box::new(move |row: u64, label: &str| {
            let mut guard = term_cap.try_lock().expect("capture lock");
            guard.terminals.push((row, label.to_string()));
        })),
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::Done { records, .. } => {
            assert_eq!(records.len(), 2, "failed attempt + retry");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let capture = shared_capture(&shared).await;
    assert_eq!(
        capture.terminals,
        vec![(1, "failed".to_string()), (1, "completed".to_string())]
    );
}

#[tokio::test]
async fn append_stats_fires_once_per_attempt_with_the_finalized_record() {
    let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
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
    let stats_cap = shared.clone();
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
        skill_body: None,
        recover_state: Box::new(move || None),
        save_state: Box::new(move |_st: &SupervisorState| {}),
        clear_state: Box::new(move || {}),
        adjudicated: None,
        report: None,
        on_spawn: None,
        on_row_terminal: None,
        append_stats: Some(Box::new(move |record: &RunRecord| {
            let mut guard = stats_cap.try_lock().expect("capture lock");
            guard.stats.push(record.clone());
        })),
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
    assert!(result.all_done);
    let capture = shared_capture(&shared).await;
    assert_eq!(
        capture.stats.len(),
        2,
        "one stats record per run attempt (one per row here)"
    );
    for stats in &capture.stats {
        assert!(
            stats.snapshot.is_some(),
            "the durability seam only ever sees finalized records"
        );
        assert!(!stats.agent_id.is_empty());
    }
    assert_eq!(capture.stats[0].row.number, 1);
    assert_eq!(capture.stats[1].row.number, 2);
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let result = run_plan(&services, &todo, None, None).await;
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
        skill_body: None,
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let done_1 = run_row(&services, &row(1, "feat: row one"), None, None).await;
    assert!(matches!(done_1, RowOutcome::Done { .. }));
    let done_3 = run_row(&services, &row(3, "feat: row three"), None, None).await;
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

fn run_record(snapshot: Option<WorkerSnapshot>) -> RunRecord {
    RunRecord {
        attempt: 1,
        row: TodoRow {
            id: "3".to_string(),
            number: 3,
            commit_message: "feat: x".to_string(),
            logical_unit: "u".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        },
        agent_id: "0".to_string(),
        outcome: RunOutcomeKind::Completed,
        question: None,
        tail: None,
        transcript_path: None,
        started_at: 1_000_000,
        completed_at: Some(1_090_000),
        snapshot,
    }
}

#[test]
fn worker_stats_from_run_skips_snapshot_less_runs() {
    // Spawn-error / clean-pass bookkeeping records carry no snapshot
    // and write nothing — the log has no all-null rows.
    assert_eq!(worker_stats_from_run(&run_record(None)), None);
}

#[test]
fn worker_stats_from_run_maps_the_terminal_snapshot_fields() {
    let snap = WorkerSnapshot {
        id: 0,
        text: "assembled".to_string(),
        tool_uses: 3,
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
    let stats = worker_stats_from_run(&run_record(Some(snap))).expect("stats record");
    assert_eq!(stats.v, 1);
    assert_eq!(stats.row, 3);
    assert_eq!(stats.attempt, 1);
    assert_eq!(stats.agent_id, "0");
    assert_eq!(stats.outcome, "completed");
    assert_eq!(stats.cost, Some(0.0451));
    assert_eq!(stats.tokens, Some(59_300));
    assert_eq!(stats.context_percent, Some(61.5));
    assert_eq!(stats.context_window, Some(200_000));
    assert_eq!(stats.turns, 4);
    assert_eq!(stats.started_at, 1_000_000);
    assert_eq!(stats.completed_at, Some(1_090_000));
    assert_eq!(
        stats.transcript,
        Some("/run/sessions/pi-0/session.jsonl".to_string())
    );
}

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
            RowOutcome::BudgetChoice {
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
            RowOutcome::CleanQuestionPause {
                row: row_1.clone(),
                question: "may I discard target/?".to_string(),
                agent_id: "0".to_string(),
                records: Vec::new(),
            },
            "clean-worktree agent asks a question",
        ),
        (
            RowOutcome::CleanAnswerOrphaned {
                row: row_1.clone(),
                question: "may I discard target/?".to_string(),
                records: Vec::new(),
            },
            "clean answer cannot be consumed",
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

#[tokio::test]
async fn skill_body_is_framed_into_the_spawned_prompt() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], false);
    let port = FakeWorkerPort::with(vec![settled("row 1 complete")], None);
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
        skill_body: Some("## Purpose\n\nImplement one step at a time."),
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::Done { matched, .. } => {
            assert_eq!(matched.tier, MatchTier::Exact);
        }
        other => panic!("expected Done, got {other:?}"),
    }

    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 1);
    assert!(
        spawned[0]
            .prompt
            .contains("has been loaded for you automatically"),
        "the framed skill header reaches the worker prompt"
    );
    assert!(
        spawned[0]
            .prompt
            .contains("## Purpose\n\nImplement one step at a time."),
        "the skill body is embedded verbatim in the worker prompt"
    );
    assert!(
        spawned[0]
            .prompt
            .contains("--- (end of the automatically loaded implement-from-plan skill)"),
        "the closing delimiter closes the framed section"
    );
}

#[tokio::test]
async fn the_startup_skill_snapshot_serves_every_attempt_in_a_run() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let clear_cap = shared.clone();
    // First classification sees no commit; the retry's does.
    let git = FakeGit::with(vec![Vec::new(), vec!["feat: row one".to_string()]], false);
    // Two attempts: the first fails, the second settles.
    let port = FakeWorkerPort::with(vec![failed("died"), settled("row 1 complete")], None);
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
        skill_body: Some("## Purpose\n\nOne snapshot for the whole run."),
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
        on_row_terminal: None,
        append_stats: None,
        control: Some(&control),
        clean_skill: None,
        permission_extension: Path::new("/ext/permission-system"),
        stderr_path: None,
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::Done {
            matched, records, ..
        } => {
            assert_eq!(matched.tier, MatchTier::Exact);
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].outcome, RunOutcomeKind::Failed);
            assert_eq!(records[1].outcome, RunOutcomeKind::Completed);
        }
        other => panic!("expected Done, got {other:?}"),
    }

    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 2);
    assert!(
        spawned[0]
            .prompt
            .contains("One snapshot for the whole run.")
            && spawned[1]
                .prompt
                .contains("One snapshot for the whole run."),
        "the startup snapshot serves every attempt in the run"
    );
}

// ------------------------------------------------------------------
// Interactive driver (plan step 7): run_plan_interactive + the moved
// last_question / last_clean_question / keep_clean_continuation helpers
// ------------------------------------------------------------------

/// Scripted `QuestionPause` seam: a queue of `PauseOutcome` verdicts plus
/// a counter so a test can assert the driver pauses exactly when it
/// should (the second consecutive clean ASK must terminate without
/// pausing again).
struct FakePauseState {
    outcomes: VecDeque<PauseOutcome>,
    calls: u32,
}

struct FakePause {
    state: Arc<tokio::sync::Mutex<FakePauseState>>,
}

impl FakePause {
    fn with(outcomes: Vec<PauseOutcome>) -> Self {
        let mut queue: VecDeque<PauseOutcome> = VecDeque::new();
        for outcome in outcomes {
            queue.push_back(outcome);
        }
        Self {
            state: Arc::new(tokio::sync::Mutex::new(FakePauseState {
                outcomes: queue,
                calls: 0,
            })),
        }
    }

    async fn calls(&self) -> u32 {
        let state = self.state.lock().await;
        state.calls
    }
}

impl QuestionPause for FakePause {
    async fn pause(&self, _question: &str) -> PauseOutcome {
        let mut state = self.state.lock().await;
        state.calls += 1;
        state.outcomes.pop_front().unwrap_or(PauseOutcome::NoAnswer)
    }
}

/// Scripted `BudgetPrompt` seam (budget reset): a queue of verdicts plus
/// a counter so a test can assert the driver asks exactly when it should
/// (only on a `BudgetChoice` pass). An exhausted queue declines — the safe
/// default that ends the run with today's report + exit 2 bytes.
struct FakeBudgetPromptState {
    outcomes: VecDeque<BudgetDecision>,
    calls: u32,
}

struct FakeBudgetPrompt {
    state: Arc<tokio::sync::Mutex<FakeBudgetPromptState>>,
}

impl FakeBudgetPrompt {
    fn with(outcomes: Vec<BudgetDecision>) -> Self {
        let mut queue: VecDeque<BudgetDecision> = VecDeque::new();
        for outcome in outcomes {
            queue.push_back(outcome);
        }
        Self {
            state: Arc::new(tokio::sync::Mutex::new(FakeBudgetPromptState {
                outcomes: queue,
                calls: 0,
            })),
        }
    }

    async fn calls(&self) -> u32 {
        let state = self.state.lock().await;
        state.calls
    }
}

impl BudgetPrompt for FakeBudgetPrompt {
    async fn prompt(
        &self,
        _row_number: u64,
        _runs_used: u32,
        _last_outcome: &str,
    ) -> BudgetDecision {
        let mut state = self.state.lock().await;
        state.calls += 1;
        state
            .outcomes
            .pop_front()
            .unwrap_or(BudgetDecision::Decline)
    }
}

fn clean_pause(question: &str) -> RowOutcome {
    RowOutcome::CleanQuestionPause {
        row: row(1, "feat: row one"),
        question: question.to_string(),
        agent_id: "0".to_string(),
        records: Vec::new(),
    }
}

#[test]
fn last_clean_question_finds_the_last_clean_pause() {
    let outcomes: Vec<RowOutcome> = vec![
        clean_pause("may I discard target/?"),
        clean_pause("may I also remove node_modules/?"),
    ];
    assert_eq!(
        last_clean_question(&outcomes[..]).as_deref(),
        Some("may I also remove node_modules/?")
    );
}

#[test]
fn last_clean_question_ignores_row_questions_and_other_outcomes() {
    let mut outcomes: Vec<RowOutcome> = vec![
        RowOutcome::QuestionPause {
            row: row(1, "feat: row one"),
            question: "polars or pandas?".to_string(),
            agent_id: "0".to_string(),
            records: Vec::new(),
        },
        RowOutcome::DirtyWorktree {
            row: row(1, "feat: row one"),
            records: Vec::new(),
        },
    ];
    assert_eq!(last_clean_question(&outcomes[..]), None);
    outcomes.push(clean_pause("may I discard target/?"));
    assert_eq!(
        last_clean_question(&outcomes[..]).as_deref(),
        Some("may I discard target/?")
    );
}

#[test]
fn a_carried_clean_answer_lives_exactly_one_clean_question_pass() {
    let cont = CleanContinuation {
        question: "may I discard target/?".to_string(),
        answer: "yes — add target/ to .gitignore".to_string(),
    };
    // A pass ending in the clean question pause keeps the channel.
    let paused: Vec<RowOutcome> = vec![clean_pause("may I discard target/?")];
    assert_eq!(
        keep_clean_continuation(&paused[..], Some(cont.clone())),
        Some(cont.clone())
    );
    // Every other ending abandons it — a row question and an orphaned
    // answer clear the channel.
    for (outcome, label) in [
        (
            RowOutcome::QuestionPause {
                row: row(1, "feat: row one"),
                question: "polars or pandas?".to_string(),
                agent_id: "0".to_string(),
                records: Vec::new(),
            },
            "row question",
        ),
        (
            RowOutcome::CleanAnswerOrphaned {
                row: row(1, "feat: row one"),
                question: "may I discard target/?".to_string(),
                records: Vec::new(),
            },
            "orphaned answer",
        ),
        (
            RowOutcome::DirtyWorktree {
                row: row(1, "feat: row one"),
                records: Vec::new(),
            },
            "dirty-tree abort",
        ),
    ] {
        let outcomes: Vec<RowOutcome> = vec![outcome];
        assert_eq!(
            keep_clean_continuation(&outcomes[..], Some(cont.clone())),
            None,
            "{label}"
        );
    }
    // A second consecutive ASK keeps the channel: the loop's terminator
    // reads it and ends the run instead of re-asking.
    let second_ask: Vec<RowOutcome> = vec![clean_pause("still unclear?")];
    assert_eq!(
        keep_clean_continuation(&second_ask[..], Some(cont.clone())),
        Some(cont.clone())
    );
    // A completed pass (no stopping outcome) also clears the channel.
    assert_eq!(
        keep_clean_continuation(&Vec::new(), Some(cont.clone())),
        None
    );
}

#[tokio::test]
async fn run_plan_interactive_folds_an_answered_row_question_into_the_next_pass() {
    let rows = vec![row(1, "feat: row one")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Pass 1 sees no commits (row pending, worker asks); pass 2 spawns the
    // row worker whose commit the completion check then matches.
    let git = FakeGit::with_seq(
        vec![Vec::new(), Vec::new(), vec!["feat: row one".to_string()]],
        vec![false],
    );
    let port = FakeWorkerPort::with(
        vec![
            ask_script("which parser?"),
            settled("row 1 complete\nPI_WORKER_STATUS: COMPLETE"),
        ],
        None,
    );
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::Answer("polars".to_string())]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("the answered pause must fold into the next pass");
    };
    assert!(result.all_done, "the folded answer completed the plan");
    assert_eq!(
        pause.calls().await,
        1,
        "exactly one pause for the one row ASK"
    );
    assert_eq!(
        port.spawned().await.len(),
        2,
        "ask worker + completing worker"
    );
}

#[tokio::test]
async fn run_plan_interactive_a_no_answer_row_question_returns_none() {
    let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with(vec![Vec::new()], false);
    let port = FakeWorkerPort::with(vec![ask_script("which parser?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::NoAnswer]);
    let result = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await;
    assert_eq!(
        result, None,
        "blank/^D at a row question keeps the exit-1 Err path"
    );
    assert_eq!(pause.calls().await, 1);
    assert_eq!(
        port.spawned().await.len(),
        1,
        "row 2 must not spawn after the pause"
    );
}

#[tokio::test]
async fn run_plan_interactive_a_graceful_stop_at_a_row_question_returns_none() {
    let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with(vec![Vec::new()], false);
    let port = FakeWorkerPort::with(vec![ask_script("which parser?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::Stopped { kill: false }]);
    let result = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await;
    assert_eq!(
        result, None,
        "a graceful stop at a row question keeps the Err path"
    );
}

#[tokio::test]
async fn run_plan_interactive_a_kill_stop_at_a_row_question_returns_the_result() {
    let rows = vec![row(1, "feat: row one"), row(2, "feat: row two")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with(vec![Vec::new()], false);
    let port = FakeWorkerPort::with(vec![ask_script("which parser?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::Stopped { kill: true }]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("the ^D kill must keep the final-report path");
    };
    assert!(!result.all_done);
    assert_eq!(result.outcomes.len(), 1);
    assert!(matches!(
        result.outcomes[0],
        RowOutcome::QuestionPause { .. }
    ));
    assert_eq!(port.spawned().await.len(), 1, "no second row may spawn");
}

#[tokio::test]
async fn run_plan_interactive_a_clean_answer_terminates_on_the_second_consecutive_ask() {
    let rows = vec![row(1, "feat: row one")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // The tree stays dirty, so every pass's gate re-fires a clean agent.
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let port = FakeWorkerPort::with(
        vec![
            ask_script("may I discard target/?"),
            ask_script("still unclear?"),
        ],
        None,
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        None,
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::Answer("yes".to_string())]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a clean answer must end with a reportable result");
    };
    assert!(!result.all_done);
    assert_eq!(
        result.outcomes.len(),
        1,
        "the terminating pass holds the second ASK"
    );
    match result.outcomes[0].clone() {
        RowOutcome::CleanQuestionPause { question, .. } => {
            assert_eq!(question, "still unclear?");
        }
        other => panic!("expected the second clean question, got {other:?}"),
    }
    assert_eq!(
        pause.calls().await,
        1,
        "the second consecutive ASK must not pause again"
    );
    assert_eq!(port.spawned().await.len(), 2, "one clean agent per pass");
}

#[tokio::test]
async fn run_plan_interactive_a_no_answer_clean_question_returns_the_result() {
    let rows = vec![row(1, "feat: row one")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let port = FakeWorkerPort::with(vec![ask_script("may I discard target/?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        None,
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::NoAnswer]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a no-answer clean question still reports (exit 2)");
    };
    assert!(!result.all_done);
    assert_eq!(pause.calls().await, 1);
}

#[tokio::test]
async fn run_plan_interactive_a_stop_at_a_clean_question_returns_the_result() {
    let rows = vec![row(1, "feat: row one")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true]);
    let port = FakeWorkerPort::with(vec![ask_script("may I discard target/?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        None,
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::Stopped { kill: true }]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a stop at a clean question still reports (exit 2)");
    };
    assert!(!result.all_done);
    assert_eq!(pause.calls().await, 1);
}

#[tokio::test]
async fn run_plan_interactive_an_orphaned_clean_answer_returns_the_result() {
    let rows = vec![row(1, "feat: row one")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Pass 1: dirty gate fires a clean agent that asks. Pass 2: the tree
    // is already clean when the carried answer arrives — orphan abort.
    let git = FakeGit::with_seq(vec![Vec::new()], vec![true, false]);
    let port = FakeWorkerPort::with(vec![ask_script("may I discard target/?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        None,
        Some(clean_skill_installed()),
        None,
        shared.clone(),
    );
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(vec![PauseOutcome::Answer("yes".to_string())]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("an orphaned answer still ends with a reportable result");
    };
    assert!(!result.all_done);
    assert_eq!(result.outcomes.len(), 1);
    assert!(matches!(
        result.outcomes[0],
        RowOutcome::CleanAnswerOrphaned { .. }
    ));
    assert_eq!(
        port.spawned().await.len(),
        1,
        "no worker spawns for the orphan"
    );
    assert_eq!(pause.calls().await, 1);
}

#[tokio::test]
async fn run_plan_interactive_a_done_plan_returns_the_result_without_pausing() {
    let rows = vec![row(1, "feat: row one")];
    let todo = plan(rows.clone());
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // The commit is already in git: no row is pending on pass 1.
    let git = FakeGit::with(vec![vec!["feat: row one".to_string()]], false);
    let port = FakeWorkerPort::with(Vec::new(), None);
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let pause = FakePause::with(Vec::new());
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a done plan reports normally");
    };
    assert!(result.all_done);
    assert_eq!(pause.calls().await, 0, "nothing to ask with every row done");
    assert_eq!(port.spawned().await.len(), 0);
}

// ------------------------------------------------------------------
// Budget-reset seam (commit: ask-the-operator budget reset)
// ------------------------------------------------------------------

#[tokio::test]
async fn run_row_on_a_spent_resume_returns_a_budget_choice_without_spawning_or_saving() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    // The recovered state names this row with a fully spent budget: the
    // gate must fire on the FIRST iteration — zero spawns and zero extra
    // state writes (the spent data is already on disk from last session).
    let persisted = state(2, 1, "failed");
    let port = FakeWorkerPort::with(Vec::new(), None);
    let config = SupervisorConfig::default();
    let services = driver_services(
        &git,
        &port,
        &config,
        None,
        Box::new(move || Some(persisted.clone())),
        Box::new(move |st: &SupervisorState| {
            let mut guard = save_cap.try_lock().expect("capture lock");
            guard.saved.push(st.clone());
        }),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    match outcome {
        RowOutcome::BudgetChoice {
            runs_used,
            last_outcome,
            records,
            ..
        } => {
            assert_eq!(runs_used, 2);
            assert_eq!(last_outcome, "failed");
            assert!(records.is_empty(), "the gate has no report rows");
        }
        other => panic!("expected BudgetChoice, got {other:?}"),
    }
    assert_eq!(
        port.spawned().await.len(),
        0,
        "the resume-blocked gate spawns nothing"
    );
    let capture = shared_capture(&shared).await;
    assert!(capture.saved.is_empty(), "the gate writes no state");
}

#[tokio::test]
async fn run_row_gate_surfaces_the_persisted_last_outcome() {
    for (marker, expected) in [("near-miss", "near-miss"), ("stopped", "stopped")] {
        let row_1 = row(1, "feat: row one");
        let git = FakeGit::with(vec![Vec::new()], false);
        // Persisted `last_outcome` provenance: any spent marker with
        // `runs_used = 2` surfaces verbatim in the gate's `BudgetChoice`.
        let persisted = state(2, 1, marker);
        let port = FakeWorkerPort::with(Vec::new(), None);
        let config = SupervisorConfig::default();
        let services = driver_services(
            &git,
            &port,
            &config,
            None,
            Box::new(move || Some(persisted.clone())),
            Box::new(move |_: &SupervisorState| {}),
        );
        let outcome = run_row(&services, &row_1, None, None).await;
        assert!(
            matches!(&outcome, RowOutcome::BudgetChoice { last_outcome, .. } if last_outcome == expected),
            "persisted {marker:?} must surface verbatim, got {outcome:?}"
        );
    }
}

#[tokio::test]
async fn a_budget_reset_marker_owns_a_dirty_tree_on_resume() {
    let row_1 = row(1, "feat: row one");
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let save_cap = shared.clone();
    // Dirty tree + a recovered `budget-reset` marker naming this row: the
    // dirty-WIP gate must treat the row as OWNED (resume note) — the
    // marker is an owner marker, deliberately NOT `dirty`/`spawn-error`,
    // so the row never routes to the clean-worktree agent.
    let git = FakeGit::with(vec![Vec::new()], true);
    let port = FakeWorkerPort::with(
        vec![failed("first worker died"), failed("second worker died")],
        None,
    );
    let config = SupervisorConfig::default();
    let persisted = state(0, 1, "budget-reset");
    let services = driver_services(
        &git,
        &port,
        &config,
        None,
        Box::new(move || Some(persisted.clone())),
        Box::new(move |st: &SupervisorState| {
            let mut guard = save_cap.try_lock().expect("capture lock");
            guard.saved.push(st.clone());
        }),
    );
    let outcome = run_row(&services, &row_1, None, None).await;
    assert!(
        matches!(&outcome, RowOutcome::BudgetChoice { runs_used: 2, .. }),
        "the owned row runs its full post-reset budget, got {outcome:?}"
    );
    let spawned = port.spawned().await;
    assert_eq!(
        spawned.len(),
        2,
        "the owned resuming row spawns both budgeted attempts"
    );
    assert!(
        spawned[0]
            .prompt
            .contains("uncommitted changes from a previous"),
        "the worker's prompt carries the resumeDirtyWip note"
    );
    let _ = shared_capture(&shared).await;
}

#[tokio::test]
async fn run_plan_interactive_budget_reset_writes_runs_used_zero_and_re_runs_the_pass() {
    let row_1 = row(1, "feat: row one");
    let todo = plan(vec![row_1.clone()]);
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Seed the "state file" with a fully spent row (2/2 runs, adjudicated
    // preserved); the save closure echoes writes back so recovery reads
    // the latest state exactly like the real state file.
    let initial = SupervisorState {
        plan_hash: plan_hash_of(""),
        current_row: 1,
        runs_used: 2,
        last_outcome: "failed".to_string(),
        adjudicated: vec![7],
        agent_id: None,
        started_at: None,
    };
    {
        let mut guard = shared.try_lock().expect("capture lock");
        guard.saved.push(initial);
    }
    let recover_cap = shared.clone();
    let save_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    // Pass 2 (after the reset) runs both budgeted attempts to exhaustion.
    let port = FakeWorkerPort::with(
        vec![failed("first worker died"), failed("second worker died")],
        None,
    );
    let config = SupervisorConfig::default();
    let services = driver_services(
        &git,
        &port,
        &config,
        None,
        Box::new(move || {
            let guard = recover_cap.try_lock().expect("capture lock");
            guard.saved.last().cloned()
        }),
        Box::new(move |st: &SupervisorState| {
            let mut guard = save_cap.try_lock().expect("capture lock");
            guard.saved.push(st.clone());
        }),
    );
    let pause = FakePause::with(Vec::new());
    // Pass 1 answers Reset; pass 2's exhaustion declines (queue empty) so
    // the run ends with a reportable result.
    let budget = FakeBudgetPrompt::with(vec![BudgetDecision::Reset]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("the reset must re-run the pass and end with a reportable result");
    };
    assert_eq!(
        budget.calls().await,
        2,
        "one blocking prompt per exhaustion"
    );
    assert_eq!(
        port.spawned().await.len(),
        2,
        "the post-reset pass runs the full budget again"
    );
    // The reset write: runs_used = 0, the budget-reset marker, the row
    // intact, adjudicated preserved, and no live-worker fields.
    let capture = shared_capture(&shared).await;
    let resets: Vec<&SupervisorState> = capture
        .saved
        .iter()
        .filter(|st| st.last_outcome == "budget-reset")
        .collect();
    assert_eq!(resets.len(), 1, "exactly one reset write");
    let reset = resets[0];
    assert_eq!(reset.runs_used, 0, "the full budget is restored");
    assert_eq!(reset.current_row, 1);
    assert_eq!(
        reset.adjudicated,
        vec![7],
        "adjudication survives the reset"
    );
    assert_eq!(reset.agent_id, None, "no live worker after a reset");
    assert_eq!(reset.started_at, None);
    // The decline path still reports the BudgetChoice byte-identically.
    let last = result.outcomes.last().expect("a final outcome");
    assert!(matches!(
        last,
        RowOutcome::BudgetChoice { runs_used: 2, .. }
    ));
    assert_eq!(outcome_label(last), "stopped — budget exhausted");
}

#[tokio::test]
async fn run_plan_interactive_budget_decline_returns_the_budget_choice_result() {
    let row_1 = row(1, "feat: row one");
    let todo = plan(vec![row_1.clone()]);
    let git = FakeGit::with(vec![Vec::new()], false);
    let persisted = state(2, 1, "failed");
    let port = FakeWorkerPort::with(Vec::new(), None);
    let config = SupervisorConfig::default();
    let services = driver_services(
        &git,
        &port,
        &config,
        None,
        Box::new(move || Some(persisted.clone())),
        Box::new(move |_: &SupervisorState| {}),
    );
    let pause = FakePause::with(Vec::new());
    let budget = FakeBudgetPrompt::with(vec![BudgetDecision::Decline]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a decline still ends with the reportable result");
    };
    assert_eq!(budget.calls().await, 1, "exactly one prompt");
    assert_eq!(port.spawned().await.len(), 0, "a decline spawns nothing");
    let last = result.outcomes.last().expect("a final outcome");
    assert!(matches!(
        last,
        RowOutcome::BudgetChoice {
            runs_used: 2,
            last_outcome,
            ..
        } if last_outcome == "failed"
    ));
    assert_eq!(outcome_label(last), "stopped — budget exhausted");
}

#[tokio::test]
async fn run_plan_interactive_never_asks_the_budget_prompt_for_a_question_pass() {
    let row_1 = row(1, "feat: row one");
    let todo = plan(vec![row_1]);
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with(vec![Vec::new()], false);
    let port = FakeWorkerPort::with(vec![ask_script("which parser?")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let pause = FakePause::with(vec![PauseOutcome::NoAnswer]);
    let budget = FakeBudgetPrompt::with(Vec::new());
    let result = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await;
    assert_eq!(result, None, "the no-answer question keeps the exit-1 path");
    assert_eq!(budget.calls().await, 0, "only the ASK seam fires");
    assert_eq!(pause.calls().await, 1);
}

#[tokio::test]
async fn run_plan_interactive_never_asks_the_budget_prompt_for_a_near_miss() {
    let row_1 = row(1, "feat: row one");
    let todo = plan(vec![row_1]);
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Candidate tier: the row stops for adjudication — no budget prompt.
    let git = FakeGit::with(vec![vec!["feat: row one and more".to_string()]], false);
    let port = FakeWorkerPort::with(vec![settled("committed a superset")], None);
    let config = SupervisorConfig::default();
    let services = clean_services(&git, &port, &config, None, None, None, shared.clone());
    let pause = FakePause::with(Vec::new());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a near-miss still reports");
    };
    assert!(matches!(result.outcomes[0], RowOutcome::NearMiss { .. }));
    assert_eq!(budget.calls().await, 0, "a near-miss never asks");
}

#[tokio::test]
async fn run_plan_interactive_never_asks_the_budget_prompt_for_a_stopped_row() {
    let row_1 = row(1, "feat: row one");
    let todo = plan(vec![row_1]);
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    let git = FakeGit::with(vec![Vec::new()], false);
    let control = RunControl::new();
    let port = FakeWorkerPort::with(
        vec![with_interrupt(InterruptKind::Stop, failed("death"))],
        Some(&control),
    );
    let config = SupervisorConfig::default();
    let services = clean_services(
        &git,
        &port,
        &config,
        Some(&control),
        None,
        None,
        shared.clone(),
    );
    let pause = FakePause::with(Vec::new());
    let budget = FakeBudgetPrompt::with(Vec::new());
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause, &budget).await
    else {
        panic!("a stopped row still reports");
    };
    assert!(matches!(result.outcomes[0], RowOutcome::Stopped { .. }));
    assert_eq!(budget.calls().await, 0, "a stopped row never asks");
}

#[tokio::test]
async fn run_plan_interactive_a_carried_answer_re_folds_into_the_post_reset_pass() {
    let row_1 = row(1, "feat: row one");
    let todo = plan(vec![row_1.clone()]);
    let shared = Arc::new(tokio::sync::Mutex::new(Capture::default()));
    // Pass 1: the answer run on the exhausted row lands on the post-spent
    // check; Reset re-folds the SAME carried answer into every post-reset
    // attempt (question-pause carry parity), then the automatic retry
    // clears it.
    {
        let mut guard = shared.try_lock().expect("capture lock");
        guard.saved.push(state(2, 1, "failed"));
    }
    let recover_cap = shared.clone();
    let save_cap = shared.clone();
    let git = FakeGit::with(vec![Vec::new()], false);
    let port = FakeWorkerPort::with(
        vec![
            failed("answer run spent"),
            failed("post-reset attempt 1"),
            failed("post-reset attempt 2"),
        ],
        None,
    );
    let config = SupervisorConfig::default();
    let services = driver_services(
        &git,
        &port,
        &config,
        None,
        Box::new(move || {
            let guard = recover_cap.try_lock().expect("capture lock");
            guard.saved.last().cloned()
        }),
        Box::new(move |st: &SupervisorState| {
            let mut guard = save_cap.try_lock().expect("capture lock");
            guard.saved.push(st.clone());
        }),
    );
    let pause = FakePause::with(Vec::new());
    // Pass 1 answers Reset; pass 2 declines (queue empty) and the run
    // ends with a reportable result.
    let budget = FakeBudgetPrompt::with(vec![BudgetDecision::Reset]);
    let Some(result) =
        run_plan_interactive(&services, &todo, Some("Keep going."), None, &pause, &budget).await
    else {
        panic!("the run must end with a reportable result");
    };
    assert!(
        result.outcomes.len() == 1,
        "one stopped pass per exhaustion"
    );
    assert_eq!(budget.calls().await, 2, "one prompt per exhaustion");
    let spawned = port.spawned().await;
    assert_eq!(spawned.len(), 3, "1 answer run + 2 post-reset runs");
    assert!(
        spawned[0].prompt.contains("Keep going."),
        "the answer run carries the answer"
    );
    assert!(
        spawned[1].prompt.contains("Keep going."),
        "the reset re-folds the carried answer into the first post-reset attempt"
    );
    assert!(
        !spawned[2].prompt.contains("Keep going."),
        "the answer is cleared after the first post-reset spend"
    );
}

proptest! {
    /// State-machine invariant (budget): for any valid recovered state,
    /// `run_row` fires the budget gate — returns `BudgetChoice` without
    /// spawning — iff the restored attempt count already reached the
    /// budget AND no carried answer bypasses it. A non-gate budget stop
    /// always comes from the post-spent check after a genuine run, so it
    /// carries the true spent kind and an over-budget count.
    #[test]
    fn budget_gate_fires_iff_the_restored_attempt_reached_the_budget_without_an_answer(
        runs_used in 0u32..=2,
        last_outcome in "near-miss|stopped|failed|no-commit|budget-reset",
        has_answer in prop::bool::ANY,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime");
        let _ = rt.block_on(async move {
            let row_1 = row(1, "feat: row one");
            let persisted = state(runs_used, 1, &last_outcome);
            let git = FakeGit::with(vec![Vec::new()], false);
            // Zero scripts: a gate hit must not spawn; a bypassed gate
            // spawns the default failed script and lands on the
            // post-spent check.
            let port = FakeWorkerPort::with(Vec::new(), None);
            let config = SupervisorConfig::default();
            let services = driver_services(
                &git,
                &port,
                &config,
                None,
                Box::new(move || Some(persisted.clone())),
                Box::new(move |_: &SupervisorState| {}),
            );
            let answer: Option<&str> = has_answer.then_some("carried answer");
            let outcome = run_row(&services, &row_1, answer, None).await;
            let restored = runs_used.min(BUDGET_PER_ROW);
            let gate_hit = restored >= BUDGET_PER_ROW && answer.is_none();
            match outcome {
                RowOutcome::BudgetChoice {
                    runs_used: used,
                    last_outcome: lo,
                    records,
                    ..
                } => {
                    if gate_hit {
                        prop_assert_eq!(used, 2, "the gate reports the restored budget");
                        prop_assert_eq!(
                            &lo,
                            &last_outcome,
                            "the gate surfaces the persisted marker"
                        );
                        prop_assert!(records.is_empty(), "the gate has no report rows");
                    } else {
                        prop_assert!(used >= 2, "a post-spent stop is at/after the budget");
                        prop_assert!(
                            lo == "failed" || lo == "no-commit",
                            "post-spent carries the true spent kind"
                        );
                    }
                }
                other => prop_assert!(false, "expected a BudgetChoice, got {other:?}"),
            }
            Ok(())
        });
    }
}
