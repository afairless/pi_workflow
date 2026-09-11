//! Worker port over the pi RPC client (Contract 3b + lifecycle).
//!
//! `WorkerPort` is the fakeable seam the supervise loop depends on (ported
//! from the TS `workers.ts`): it spawns one fresh `pi --mode rpc` process per
//! attempt, assembles a live snapshot from the JSONL event stream, enforces
//! the orchestrator-side stall ceiling (turn count + wall clock), and
//! classifies the terminal event (`agent_settled`, stall-ceiling abort,
//! wall-clock timeout, or process exit).
//!
//! The real implementation (`RpcWorker`) owns one `RpcClient` per worker. A
//! background pump task consumes the broadcast event receiver so the
//! snapshot stays fresh while `await_terminal` waits; exactly one terminal
//! event is ever recorded per worker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::rpc::{MessageDelta, RpcClient, RpcCommand, RpcEvent, RpcResponse, SpawnOptions};

/// Thinking level pinned by Contract 3b (the plan-implementer persona runs
/// in extended-thought mode).
pub const DEFAULT_THINKING_LEVEL: &str = "high";

/// Tool allowlist passed via `--tools` (Contract 3b). The orchestrator does
/// not open the tool surface wider; everything else stays closured.
pub const DEFAULT_TOOLS: &str = "read,grep,find,ls,bash,edit,write";

/// Determinism flags pinned by Contract 3b: they stop autoformat/autofix/
/// test-runner-on-write and the opengrep auxiliary scanner from mutating
/// the worker's tree or doing unplanned network work.
const DETERMINISM_FLAGS: [&str; 6] = [
    "--no-lsp",
    "--no-lens",
    "--no-tests",
    "--no-autoformat",
    "--no-autofix",
    "--no-opengrep",
];

/// Opaque id for one spawned worker attempt.
pub type WorkerId = u64;

/// Everything the port needs to spawn one worker (per row, per attempt).
///
/// Built by the supervise loop from the row's config precedence; the argv
/// builder reads it to emit the Contract 3b command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpawnOpts {
    /// `pi-plan-row-<n>` — passed as `--name`.
    pub name: String,
    /// `--model <model>` (already resolved by config precedence).
    pub model: String,
    /// Turn-based stall ceiling for this worker (`--maxTurns` analog).
    pub max_turns: u32,
    /// Wall-clock ceiling for this worker (abort + classify failed).
    pub turn_timeout: Duration,
    /// Directory the worker operates in (project root; spawn `cwd`).
    pub cwd: PathBuf,
    /// `--session-dir` (where the peer keeps its session JSONL).
    pub session_dir: PathBuf,
    /// `--skill <path>` when set (implement-from-plan skill directory).
    pub skill_path: Option<PathBuf>,
    /// Tool allowlist passed as the comma-joined `--tools` value.
    pub tools: Vec<String>,
    /// Persona preamble passed via `--append-system-prompt`.
    pub persona: String,
    /// Cadence of the periodic `get_session_stats` poll. `Duration::ZERO`
    /// disables stats polling (no context% / transcript from stats).
    pub stats_interval: Duration,
}

/// The worker's terminal classification (ported `TerminalEvent`).
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    /// `agent_settled` observed — the worker finished deliberately.
    Settled,
    /// `turn_end` count exceeded the per-worker ceiling; the port aborted
    /// the worker (RPC `abort` + process-group kill).
    StallCeiling { max_turns: u32 },
    /// The wall-clock ceiling expired; the port aborted the worker.
    WallTimeout { timeout: Duration },
    /// The peer process exited / the RPC stream died before settling.
    ProcessExit,
}

impl TerminalEvent {
    /// True when the worker reached a deliberate terminal state.
    pub fn is_completed(&self) -> bool {
        matches!(*self, TerminalEvent::Settled)
    }
}

/// Normalized worker snapshot — the live view the loop renders and classifies
/// from (text assembled from deltas, counters, context%, transcript).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerSnapshot {
    pub id: WorkerId,
    /// Assistant text assembled from `message_update` deltas.
    pub text: String,
    /// `tool_execution_start` events observed.
    pub tool_uses: u32,
    /// `turn_end` events observed.
    pub turn_count: u32,
    /// `compaction_start` events observed.
    pub compaction_count: u32,
    /// Latest `contextUsage.percent` from `get_session_stats`; `None` right
    /// after compaction (unknown / rendered as `?`).
    pub context_percent: Option<f64>,
    /// Session file path (`data.sessionFile`), when reported.
    pub transcript: Option<PathBuf>,
    /// Epoch ms when the worker was spawned.
    pub started_at: u64,
}

/// Port-level error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum WorkerError {
    #[error("failed to spawn the worker process: {0}")]
    Spawn(String),
    #[error("the prompt command was rejected: {0}")]
    Prompt(String),
    #[error("no live worker with id {0}")]
    UnknownWorker(WorkerId),
    #[error("the worker's RPC event stream is unavailable: {0}")]
    NoStream(String),
}

/// How to build the spawned process argv.
///
/// Production uses `build_worker_args` (Contract 3b); tests inject a builder
/// that points the binary at the scripted fake-pi peer.
pub type ArgvBuilder = Box<dyn Fn(&WorkerSpawnOpts) -> Vec<String>>;

/// Contract 3b argv — everything after the binary:
///
/// ```text
/// --mode rpc --session-dir <dir> --name pi-plan-row-<n> --model <model>
/// --thinking high --approve --tools read,grep,find,ls,bash,edit,write
/// --skill <path> --no-lsp --no-lens --no-tests --no-autoformat
/// --no-autofix --no-opengrep --append-system-prompt <persona>
/// ```
///
/// Pure (no I/O) so the shape and quoting are unit-testable.
pub fn build_worker_args(opts: &WorkerSpawnOpts) -> Vec<String> {
    let mut args: Vec<String> = vec!["--mode".to_string(), "rpc".to_string()];
    args.push("--session-dir".to_string());
    args.push(opts.session_dir.to_string_lossy().into_owned());
    args.push("--name".to_string());
    args.push(opts.name.clone());
    args.push("--model".to_string());
    args.push(opts.model.clone());
    args.push("--thinking".to_string());
    args.push(DEFAULT_THINKING_LEVEL.to_string());
    args.push("--approve".to_string());
    args.push("--tools".to_string());
    args.push(tools_csv(opts));
    if let Some(skill) = &opts.skill_path {
        args.push("--skill".to_string());
        args.push(skill.to_string_lossy().into_owned());
    }
    for flag in DETERMINISM_FLAGS {
        args.push(flag.to_string());
    }
    args.push("--append-system-prompt".to_string());
    args.push(opts.persona.clone());
    args
}

fn tools_csv(opts: &WorkerSpawnOpts) -> String {
    let list = if opts.tools.is_empty() {
        DEFAULT_TOOLS.split(',').collect::<Vec<_>>()
    } else {
        opts.tools.iter().map(|t| t.as_str()).collect::<Vec<_>>()
    };
    list.join(",")
}

/// The production argv builder: `build_worker_args` unchanged.
pub fn system_argv_builder(_opts: &WorkerSpawnOpts) -> Vec<String> {
    build_worker_args(_opts)
}

/// Mutable accumulator behind a worker's `WorkerSnapshot`. Guarded by the
/// pump task; `snapshot` clones it out.
#[derive(Debug, Clone)]
struct SnapshotAcc {
    text: String,
    tool_uses: u32,
    turn_count: u32,
    compaction_count: u32,
    context_percent: Option<f64>,
    transcript: Option<PathBuf>,
    started_at: u64,
    max_turns: u32,
}

impl SnapshotAcc {
    /// Clone the live accumulator into an owned snapshot (reads go through
    /// the mutex guard; by-value clone methods resolve on owned values).
    fn to_snapshot(&self, id: WorkerId) -> WorkerSnapshot {
        let context_percent = self.context_percent.as_ref().copied();
        let transcript = self.transcript.as_ref().cloned();
        WorkerSnapshot {
            id,
            text: self.text.clone(),
            tool_uses: self.tool_uses,
            turn_count: self.turn_count,
            compaction_count: self.compaction_count,
            context_percent,
            transcript,
            started_at: self.started_at,
        }
    }
}

/// Apply one RPC event to the accumulator; pure so snapshot assembly is
/// unit-testable from scripted events. Returns the terminal this event
/// implies, when any (`agent_settled`, turn-based stall ceiling overflow).
fn note_event(acc: &mut SnapshotAcc, event: &RpcEvent) -> Option<TerminalEvent> {
    match event {
        RpcEvent::MessageUpdate(MessageDelta::TextDelta { delta, .. })
        | RpcEvent::MessageUpdate(MessageDelta::ThinkingDelta { delta, .. })
        | RpcEvent::MessageUpdate(MessageDelta::ToolCallDelta { delta, .. }) => {
            acc.text.push_str(delta);
        }
        RpcEvent::ToolExecutionStart { .. } => {
            acc.tool_uses += 1;
        }
        RpcEvent::TurnEnd => {
            acc.turn_count += 1;
            if acc.turn_count > acc.max_turns {
                return Some(TerminalEvent::StallCeiling {
                    max_turns: acc.max_turns,
                });
            }
        }
        RpcEvent::CompactionStart => {
            acc.compaction_count += 1;
            // Context estimate is null right after compaction — treat as unknown.
            acc.context_percent = None;
        }
        RpcEvent::AgentSettled => return Some(TerminalEvent::Settled),
        _ => {}
    }
    None
}

/// `data.contextUsage.percent`, when present and not null.
pub fn context_percent_of(resp: &RpcResponse) -> Option<f64> {
    let data = resp.data.as_ref()?.as_object()?;
    let usage = data.get("contextUsage")?.as_object()?;
    usage.get("percent")?.as_f64()
}

/// `data.sessionFile`, when present.
pub fn session_file_of(resp: &RpcResponse) -> Option<PathBuf> {
    resp.data
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|d| d.get("sessionFile"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
}

/// Record the terminal event once; later callers see the first outcome.
async fn set_terminal_once(
    slot: &Arc<tokio::sync::Mutex<Option<TerminalEvent>>>,
    event: TerminalEvent,
) {
    let mut guard = slot.lock().await;
    if guard.is_none() {
        *guard = Some(event);
    }
}

/// Read the recorded terminal, if any.
async fn peek_terminal(
    slot: &Arc<tokio::sync::Mutex<Option<TerminalEvent>>>,
) -> Option<TerminalEvent> {
    let guard = slot.lock().await;
    guard.as_ref().cloned()
}

/// Pump task: consume the worker's event stream, keep the snapshot fresh,
/// and record the terminal event exactly once.
///
/// Terminal conditions, first wins:
/// - `agent_settled` → `Settled`
/// - `turn_end` count exceeding `max_turns` → abort RPC + kill → `StallCeiling`
/// - wall-clock `turn_timeout` elapsed → abort RPC + kill → `WallTimeout`
/// - event stream closed (peer exit / read-loop death) → `ProcessExit`
async fn pump_task(
    client: Arc<RpcClient>,
    mut events: tokio::sync::broadcast::Receiver<RpcEvent>,
    acc: Arc<tokio::sync::Mutex<SnapshotAcc>>,
    terminal: Arc<tokio::sync::Mutex<Option<TerminalEvent>>>,
    stop: Arc<AtomicBool>,
    turn_timeout: Duration,
) {
    let started = tokio::time::Instant::now();
    let wall_ms = turn_timeout.as_millis() as i64;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if wall_ms > 0 && started.elapsed().as_millis() as i64 >= wall_ms {
            let _ = client.request(RpcCommand::Abort).await;
            let _ = client.kill().await;
            set_terminal_once(
                &terminal,
                TerminalEvent::WallTimeout {
                    timeout: turn_timeout,
                },
            )
            .await;
            break;
        }
        match tokio::time::timeout(Duration::from_millis(250), events.recv()).await {
            Err(_) => continue, // quiet window; re-check wall clock + stop
            Ok(Err(_)) => {
                // Event channel closed: the read loop ended (peer exit or a
                // fatal protocol failure) before the worker settled.
                set_terminal_once(&terminal, TerminalEvent::ProcessExit).await;
                break;
            }
            Ok(Ok(event)) => {
                let mut acc_guard = acc.lock().await;
                let acc_mut: &mut SnapshotAcc = &mut acc_guard;
                let pending = note_event(acc_mut, &event);
                if let Some(term) = pending {
                    if let TerminalEvent::StallCeiling { .. } = term {
                        let _ = client.request(RpcCommand::Abort).await;
                        let _ = client.kill().await;
                    }
                    set_terminal_once(&terminal, term).await;
                    break;
                }
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
}

/// Periodic `get_session_stats` poller: keeps `context_percent` and
/// `transcript` fresh. First poll runs immediately at spawn so the spawn
/// report can show a transcript when the peer reports one early.
async fn stats_task(
    client: Arc<RpcClient>,
    acc: Arc<tokio::sync::Mutex<SnapshotAcc>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match client.request(RpcCommand::GetSessionStats).await {
            Ok(resp) if resp.success => {
                let mut guard = acc.lock().await;
                guard.context_percent = context_percent_of(&resp);
                if let Some(path) = session_file_of(&resp) {
                    guard.transcript = Some(path);
                }
            }
            _ => {} // best-effort: keep the previous values
        }
        tokio::time::sleep(interval).await;
    }
}

/// The fakeable worker seam the supervise loop depends on (ported
/// `WorkerPort`). `RpcWorker` is the real implementation; tests inject a
/// scripted fake implementing the same trait.
///
/// The `async_fn_in_trait` lint is suppressed deliberately: this trait is
/// only ever used by our own control loop (`supervise.rs`), never through
/// dynamic dispatch, so auto trait bounds on the returned futures are
/// irrelevant. (Same rationale as `polars.io`'s `ByteSource`.)
#[allow(async_fn_in_trait)]
pub trait WorkerPort {
    /// Spawn a worker for one row attempt; returns the id immediately.
    async fn spawn(&self, prompt: &str, opts: &WorkerSpawnOpts) -> Result<WorkerId, WorkerError>;
    /// Latest normalized snapshot for a worker id.
    async fn snapshot(&self, id: WorkerId) -> Option<WorkerSnapshot>;
    /// Abort a running worker (RPC abort + process-group kill).
    async fn abort(&self, id: WorkerId) -> Result<(), WorkerError>;
    /// Resolve the worker's terminal event. The worker is spawned before
    /// await, so a terminal already recorded is returned immediately.
    ///
    /// `timeout` is a caller-side bound; when it expires the port aborts the
    /// worker and classifies `WallTimeout` (mirrors the TS
    /// `awaitTerminal(id, timeoutMs)` contract).
    async fn await_terminal(
        &self,
        id: WorkerId,
        timeout: Duration,
    ) -> Result<TerminalEvent, WorkerError>;
    /// Subscribe to the worker's live RPC event stream (UI tail / dialogs).
    /// Returns `None` when the read loop has already terminated.
    async fn subscribe(&self, id: WorkerId) -> Option<tokio::sync::broadcast::Receiver<RpcEvent>>;
    /// Release everything the port holds (kills every live worker).
    async fn dispose(&self);
}

/// Real worker port over the pi RPC client.
pub struct RpcWorker {
    binary: String,
    argv: ArgvBuilder,
    cwd: PathBuf,
    stderr_path: Option<PathBuf>,
    next_id: Arc<AtomicU64>,
    live: Arc<tokio::sync::Mutex<HashMap<WorkerId, LiveWorker>>>,
}

struct LiveWorker {
    client: Arc<RpcClient>,
    acc: Arc<tokio::sync::Mutex<SnapshotAcc>>,
    terminal: Arc<tokio::sync::Mutex<Option<TerminalEvent>>>,
    stop: Arc<AtomicBool>,
}

impl WorkerPort for RpcWorker {
    async fn spawn(&self, prompt: &str, opts: &WorkerSpawnOpts) -> Result<WorkerId, WorkerError> {
        let args = (self.argv)(opts);
        let spawn_opts = SpawnOptions {
            binary: self.binary.clone(),
            args,
            cwd: self.cwd.clone(),
            stderr_path: self.stderr_path.clone(),
        };
        let client = Arc::new(
            RpcClient::spawn(&spawn_opts)
                .await
                .map_err(|e| WorkerError::Spawn(format!("{e}")))?,
        );
        let events = client
            .subscribe()
            .await
            .ok_or(WorkerError::NoStream("subscribe after spawn".to_string()))?;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let started_at = now_epoch_ms().unwrap_or(0);
        let acc = Arc::new(tokio::sync::Mutex::new(SnapshotAcc {
            text: String::new(),
            tool_uses: 0,
            turn_count: 0,
            compaction_count: 0,
            context_percent: None,
            transcript: None,
            started_at,
            max_turns: opts.max_turns,
        }));
        let terminal: Arc<tokio::sync::Mutex<Option<TerminalEvent>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));

        tokio::spawn(pump_task(
            client.clone(),
            events,
            acc.clone(),
            terminal.clone(),
            stop.clone(),
            opts.turn_timeout,
        ));
        if opts.stats_interval.as_millis() > 0 {
            tokio::spawn(stats_task(
                client.clone(),
                acc.clone(),
                stop.clone(),
                opts.stats_interval,
            ));
        }

        let resp = client
            .request(RpcCommand::Prompt {
                message: prompt.to_string(),
                streaming_behavior: None,
            })
            .await
            .map_err(|e| WorkerError::Prompt(format!("{e}")))?;
        if !resp.success {
            stop.store(true, Ordering::SeqCst);
            let _ = client.kill().await;
            return Err(WorkerError::Prompt(
                resp.error
                    .unwrap_or("prompt failed".to_string())
                    .to_string(),
            ));
        }

        self.live.lock().await.insert(
            id,
            LiveWorker {
                client,
                acc,
                terminal,
                stop,
            },
        );
        Ok(id)
    }

    async fn snapshot(&self, id: WorkerId) -> Option<WorkerSnapshot> {
        let acc_arc = {
            let live_guard = self.live.lock().await;
            let worker = live_guard.get(&id)?;
            worker.acc.clone()
        };
        let acc_guard = acc_arc.lock().await;
        let acc: &SnapshotAcc = &acc_guard;
        Some(acc.to_snapshot(id))
    }

    async fn abort(&self, id: WorkerId) -> Result<(), WorkerError> {
        let (client, terminal, stop) = {
            let live_guard = self.live.lock().await;
            let Some(worker) = live_guard.get(&id) else {
                return Err(WorkerError::UnknownWorker(id));
            };
            (
                worker.client.clone(),
                worker.terminal.clone(),
                worker.stop.clone(),
            )
        };
        stop.store(true, Ordering::SeqCst);
        let _ = client.request(RpcCommand::Abort).await;
        let _ = client.kill().await;
        set_terminal_once(&terminal, TerminalEvent::ProcessExit).await;
        Ok(())
    }

    async fn await_terminal(
        &self,
        id: WorkerId,
        timeout: Duration,
    ) -> Result<TerminalEvent, WorkerError> {
        let (client, terminal) = {
            let live_guard = self.live.lock().await;
            let Some(worker) = live_guard.get(&id) else {
                return Err(WorkerError::UnknownWorker(id));
            };
            (worker.client.clone(), worker.terminal.clone())
        };
        let deadline = tokio::time::Instant::now();
        loop {
            if let Some(term) = peek_terminal(&terminal).await {
                return Ok(term);
            }
            if deadline.elapsed() >= timeout {
                // Caller-side bound: abort the worker and classify the wall
                // clock as the terminal event.
                let _ = client.request(RpcCommand::Abort).await;
                let _ = client.kill().await;
                let term = TerminalEvent::WallTimeout { timeout };
                set_terminal_once(&terminal, term.clone()).await;
                return Ok(term);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn subscribe(&self, id: WorkerId) -> Option<tokio::sync::broadcast::Receiver<RpcEvent>> {
        let client = {
            let live_guard = self.live.lock().await;
            let worker = live_guard.get(&id)?;
            worker.client.clone()
        };
        client.subscribe().await
    }

    async fn dispose(&self) {
        let clients: Vec<Arc<RpcClient>> = {
            let mut live = self.live.lock().await;
            let out = live
                .values()
                .map(|w| w.client.clone())
                .collect::<Vec<Arc<RpcClient>>>();
            live.clear();
            out
        };
        for client in clients {
            let _ = client.kill().await;
        }
    }
}

impl RpcWorker {
    /// A worker port that spawns `pi` with the Contract 3b argv.
    pub fn system(cwd: &Path, stderr_path: Option<PathBuf>) -> Self {
        Self::with(
            "pi".to_string(),
            Box::new(move |opts: &WorkerSpawnOpts| system_argv_builder(opts)),
            cwd.to_path_buf(),
            stderr_path,
        )
    }

    /// A worker port over an injected binary/argv builder (tests point the
    /// binary at the scripted fake-pi peer).
    pub fn with(
        binary: String,
        argv: ArgvBuilder,
        cwd: PathBuf,
        stderr_path: Option<PathBuf>,
    ) -> Self {
        RpcWorker {
            binary,
            argv,
            cwd,
            stderr_path,
            next_id: Arc::new(AtomicU64::new(0)),
            live: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}

/// Current epoch time in milliseconds (best-effort; a clock read failure
/// yields `None` so callers decide a fallback).
pub fn now_epoch_ms() -> Option<u64> {
    let now = std::time::SystemTime::now();
    let since = now.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_millis() as u64)
}

/// Extract the `PI_WORKER_STATUS: <value>` marker from a worker's final
/// text. A hint only — classification never relies on it (ported from the
/// TS supervisor's `parseWorkerStatus`).
pub fn parse_worker_status(result: &str) -> Option<String> {
    let index = result.find("PI_WORKER_STATUS:")?;
    let rest = &result[index + "PI_WORKER_STATUS:".len()..];
    let trimmed = rest.trim_start();
    let Some(end) = trimmed.find(|c: char| !c.is_ascii_alphabetic() && c != '_') else {
        return Some(trimmed.to_string());
    };
    Some(trimmed[..end].to_string())
}

/// Extract the `QUESTION: <text>` line from a worker's final text (the ASK
/// contract's second marker line).
pub fn parse_question(result: &str) -> Option<String> {
    for line in result.lines() {
        let trimmed = line.trim();
        if let Some(q) = trimmed.strip_prefix("QUESTION:") {
            let question = q.trim();
            if !question.is_empty() {
                return Some(question.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(overrides: Option<&dyn Fn(&mut WorkerSpawnOpts)>) -> WorkerSpawnOpts {
        let mut o = WorkerSpawnOpts {
            name: "pi-plan-row-3".to_string(),
            model: "openrouter/deepseek/deepseek-v4-flash".to_string(),
            max_turns: 40,
            turn_timeout: Duration::from_secs(1800),
            cwd: Path::new("/repo").to_path_buf(),
            session_dir: Path::new("/run/sessions").to_path_buf(),
            skill_path: Some(Path::new("/skills/implement-from-plan").to_path_buf()),
            tools: vec!["read".to_string(), "bash".to_string()],
            persona: "You are a worker.".to_string(),
            stats_interval: Duration::from_secs(5),
        };
        if let Some(f) = overrides {
            f(&mut o);
        }
        o
    }

    fn acc(max_turns: u32) -> SnapshotAcc {
        SnapshotAcc {
            text: String::new(),
            tool_uses: 0,
            turn_count: 0,
            compaction_count: 0,
            context_percent: None,
            transcript: None,
            started_at: 1_000_000,
            max_turns,
        }
    }

    // ---- argv shape & quoting (Contract 3b) ----

    #[test]
    fn build_worker_args_matches_contract_3b_shape() {
        let args = build_worker_args(&opts(None));
        assert_eq!(args[0], "--mode");
        assert_eq!(args[1], "rpc");
        assert_eq!(args[2], "--session-dir");
        assert_eq!(args[3], "/run/sessions");
        assert_eq!(args[4], "--name");
        assert_eq!(args[5], "pi-plan-row-3");
        assert_eq!(args[6], "--model");
        assert_eq!(args[7], "openrouter/deepseek/deepseek-v4-flash");
        assert_eq!(args[8], "--thinking");
        assert_eq!(args[9], "high");
        let approve = args
            .iter()
            .position(|a| a == "--approve")
            .expect("--approve present");
        assert!(approve < args.len() - 1);
        let tools = args
            .iter()
            .position(|a| a == "--tools")
            .expect("--tools present");
        assert_eq!(args[tools + 1], "read,bash");
        let skill = args
            .iter()
            .position(|a| a == "--skill")
            .expect("--skill present");
        assert_eq!(args[skill + 1], "/skills/implement-from-plan");
        for flag in DETERMINISM_FLAGS {
            assert!(
                args.contains(&flag.to_string()),
                "determinism flag {flag} present"
            );
        }
        assert_eq!(args.last().cloned(), Some("You are a worker.".to_string()));
    }

    #[test]
    fn build_worker_args_omits_skill_and_quotes_the_persona() {
        let args = build_worker_args(&opts(Some(&|o| {
            o.skill_path = None;
            o.persona = "line one\nline two with spaces".to_string();
        })));
        assert!(
            !args.iter().any(|a| a == "--skill"),
            "no --skill without a skill path"
        );
        // The persona is ONE argv element, never shell-split.
        assert_eq!(
            args.last().cloned(),
            Some("line one\nline two with spaces".to_string())
        );
    }

    #[test]
    fn build_worker_args_defaults_tools_when_empty() {
        let args = build_worker_args(&opts(Some(&|o| o.tools = Vec::new())));
        let tools = args.iter().position(|a| a == "--tools").expect("--tools");
        assert_eq!(args[tools + 1], DEFAULT_TOOLS);
    }

    // ---- snapshot assembly (pure note_event) ----

    fn text_delta(t: &str) -> RpcEvent {
        RpcEvent::MessageUpdate(MessageDelta::TextDelta {
            content_index: 0,
            delta: t.to_string(),
        })
    }

    #[test]
    fn note_event_assembles_text_and_counts_tools_and_turns() {
        let mut a = acc(40);
        assert_eq!(note_event(&mut a, &text_delta("Work")), None);
        assert_eq!(note_event(&mut a, &text_delta("ing")), None);
        assert_eq!(
            note_event(
                &mut a,
                &RpcEvent::ToolExecutionStart {
                    tool_call_id: "c1".to_string(),
                    tool_name: "bash".to_string(),
                }
            ),
            None
        );
        assert_eq!(note_event(&mut a, &RpcEvent::TurnEnd), None);
        assert_eq!(note_event(&mut a, &RpcEvent::TurnEnd), None);
        assert_eq!(a.text, "Working");
        assert_eq!(a.tool_uses, 1);
        assert_eq!(a.turn_count, 2);
    }

    #[test]
    fn note_event_tracks_compaction_and_clears_context() {
        let mut a = acc(40);
        a.context_percent = Some(30.0);
        assert_eq!(note_event(&mut a, &RpcEvent::CompactionStart), None);
        assert_eq!(a.compaction_count, 1);
        assert_eq!(
            a.context_percent, None,
            "context unknown right after compaction"
        );
    }

    #[test]
    fn note_event_returns_settled_on_agent_settled() {
        let mut a = acc(40);
        assert_eq!(
            note_event(&mut a, &RpcEvent::AgentSettled),
            Some(TerminalEvent::Settled)
        );
    }

    #[test]
    fn note_event_stalls_when_turn_count_exceeds_max() {
        let mut a = acc(2);
        assert_eq!(note_event(&mut a, &RpcEvent::TurnEnd), None);
        assert_eq!(note_event(&mut a, &RpcEvent::TurnEnd), None);
        assert_eq!(
            note_event(&mut a, &RpcEvent::TurnEnd),
            Some(TerminalEvent::StallCeiling { max_turns: 2 })
        );
    }

    #[test]
    fn note_event_ignores_unknown_and_bookkeeping_events() {
        let mut a = acc(40);
        assert_eq!(
            note_event(
                &mut a,
                &RpcEvent::Unknown {
                    name: "something_new".to_string(),
                    raw: serde_json::json!({"x": 1}),
                }
            ),
            None
        );
        assert_eq!(note_event(&mut a, &RpcEvent::TurnStart), None);
        assert_eq!(note_event(&mut a, &RpcEvent::CompactionEnd), None);
        assert_eq!(a.turn_count, 0);
    }

    // ---- terminal classification ----

    #[test]
    fn terminal_is_completed_only_for_settled() {
        assert!(TerminalEvent::Settled.is_completed());
        assert!(!TerminalEvent::StallCeiling { max_turns: 2 }.is_completed());
        assert!(
            !TerminalEvent::WallTimeout {
                timeout: Duration::from_secs(1)
            }
            .is_completed()
        );
        assert!(!TerminalEvent::ProcessExit.is_completed());
    }

    // ---- marker extraction ----

    #[test]
    fn parse_worker_status_extracts_the_marker() {
        assert_eq!(
            parse_worker_status("done\nPI_WORKER_STATUS: ASK\nQUESTION: what?").as_deref(),
            Some("ASK")
        );
        assert_eq!(
            parse_worker_status("PI_WORKER_STATUS:  COMPLETE").as_deref(),
            Some("COMPLETE")
        );
        assert_eq!(parse_worker_status("no marker here"), None);
    }

    #[test]
    fn parse_question_extracts_the_question_line() {
        assert_eq!(
            parse_question("PI_WORKER_STATUS: ASK\nQUESTION: use polars not pandas").as_deref(),
            Some("use polars not pandas")
        );
        assert_eq!(parse_question("no question"), None);
    }

    // ---- stats response parsing ----

    /// Build a `get_session_stats` response whose `data` is the given JSON
    /// object literal. `RpcResponse` is assembled by hand (matching
    /// `route_response`) rather than deserialized — `RpcResponse` is not a
    /// serde type.
    fn stats_response(data: &str) -> RpcResponse {
        let parsed: serde_json::Value = serde_json::from_str(data).expect("json");
        RpcResponse {
            id: None,
            command: "get_session_stats".to_string(),
            success: true,
            data: Some(parsed),
            error: None,
        }
    }

    #[test]
    fn context_percent_of_handles_absent_and_null_percent() {
        assert_eq!(
            context_percent_of(&stats_response(r#"{"contextUsage":{"percent": 30}}"#)),
            Some(30.0)
        );
        assert_eq!(
            context_percent_of(&stats_response(r#"{"contextUsage":{"percent": null}}"#)),
            None,
            "null after compaction = unknown"
        );
        assert_eq!(context_percent_of(&stats_response(r#"{}"#)), None);
    }

    #[test]
    fn session_file_of_reads_the_session_path() {
        assert_eq!(
            session_file_of(&stats_response(r#"{"sessionFile":"/tmp/s/s.jsonl"}"#))
                .map(|p| p.to_string_lossy().into_owned()),
            Some("/tmp/s/s.jsonl".to_string())
        );
        assert_eq!(session_file_of(&stats_response(r#"{}"#)), None);
    }

    // ---- worker id / port surface (non-async parts) ----

    #[test]
    fn now_epoch_ms_is_recent_and_monotonic() {
        let a = now_epoch_ms().expect("clock");
        let b = now_epoch_ms().expect("clock");
        assert!(a > 1_700_000_000_000, "epoch-ms scale");
        assert!(b >= a);
    }
}
