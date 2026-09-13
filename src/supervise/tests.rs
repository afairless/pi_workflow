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
use crate::worker::{TerminalEvent, Tokens, WorkerError, WorkerSnapshot, WorkerSpawnOpts};

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
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
        await_terminal_timeout: Some(Duration::from_secs(30)),
    };

    let outcome = run_row(&services, &row_1, None, None).await;
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
    let port = FakeWorkerPort::with(
        vec![FakeScript {
            terminal: TerminalEvent::ProcessExit,
            text: String::new(),
            transcript: None,
            spawn_error: Some("boom".to_string()),
            interrupt: None,
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
    match outcome {
        RowOutcome::DirtyWorktree { records, .. } => {
            assert_eq!(records[0].outcome, RunOutcomeKind::SpawnError);
            assert!(
                records[0]
                    .tail
                    .as_deref()
                    .is_some_and(|t| t.contains("clean spawn failed"))
            );
        }
        other => panic!("expected DirtyWorktree, got {other:?}"),
    }
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
    let pause = FakePause::with(vec![PauseOutcome::Answer("polars".to_string())]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
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
    let pause = FakePause::with(vec![PauseOutcome::NoAnswer]);
    let result = run_plan_interactive(&services, &todo, None, None, &pause).await;
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
    let pause = FakePause::with(vec![PauseOutcome::Stopped { kill: false }]);
    let result = run_plan_interactive(&services, &todo, None, None, &pause).await;
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
    let pause = FakePause::with(vec![PauseOutcome::Stopped { kill: true }]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
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
    let pause = FakePause::with(vec![PauseOutcome::Answer("yes".to_string())]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
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
    let pause = FakePause::with(vec![PauseOutcome::NoAnswer]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
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
    let pause = FakePause::with(vec![PauseOutcome::Stopped { kill: true }]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
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
    let pause = FakePause::with(vec![PauseOutcome::Answer("yes".to_string())]);
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
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
    let pause = FakePause::with(Vec::new());
    let Some(result) = run_plan_interactive(&services, &todo, None, None, &pause).await else {
        panic!("a done plan reports normally");
    };
    assert!(result.all_done);
    assert_eq!(pause.calls().await, 0, "nothing to ask with every row done");
    assert_eq!(port.spawned().await.len(), 0);
}
