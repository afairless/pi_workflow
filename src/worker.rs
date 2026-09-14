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

use crate::rpc::{
    MessageDelta, PendingTool, RpcClient, RpcCommand, RpcEvent, RpcResponse, SpawnOptions, UiReply,
};

/// Thinking level pinned by Contract 3b (the plan-implementer persona runs
/// in extended-thought mode).
pub const DEFAULT_THINKING_LEVEL: &str = "high";

/// Tool allowlist passed via `--tools` (Contract 3b). The orchestrator does
/// not open the tool surface wider; everything else stays closured.
pub const DEFAULT_TOOLS: &str = "read,grep,find,ls,bash,edit,write";

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
    /// `--skill <path>` entries, in order (implement-from-plan and, for
    /// clean spawns, the clean-worktree skill directory).
    pub skills: Vec<PathBuf>,
    /// Tool allowlist passed as the comma-joined `--tools` value.
    pub tools: Vec<String>,
    /// The resolved `pi-permission-system` package directory — emitted as
    /// `--no-extensions` + `-e <dir>` so the worker runs bare with the
    /// permission system as the ONLY loaded extension (no guardrails).
    pub permission_extension: PathBuf,
    /// Append-only shared worker stderr log (`worker-stderr.log`), opened
    /// by the spawn; only meaningful for spawn errors, where the supervise
    /// layer reads its tail to surface the failing child's own stderr in
    /// the report. `None` never applies (the RPC client then sends the
    /// child's stderr to the void).
    pub stderr_path: Option<PathBuf>,
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

/// Provider-reported token usage from `get_session_stats` (`data.tokens`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
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
    /// Provider-reported cost in USD (`data.cost`); may be 0 or absent.
    pub cost: Option<f64>,
    /// Provider-reported token usage (`data.tokens`), when the full shape
    /// was present.
    pub tokens: Option<Tokens>,
    /// Context-window size in tokens (`data.contextUsage.contextWindow`).
    pub context_window: Option<u64>,
    /// Epoch ms when the worker was spawned.
    pub started_at: u64,
    /// The in-flight tool call gating a permission dialog, when one is
    /// pending (set on `tool_execution_start`, cleared on `tool_execution_end`).
    pub pending_tool: Option<PendingTool>,
    /// The worker's terminal classification, once recorded: `None` while
    /// the worker is live, `Some(..)` after it settled/stalled/timed out
    /// or its stream died. Drives the TUI's live-only footer.
    pub terminal: Option<TerminalEvent>,
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
    #[error("extension UI reply failed: {0}")]
    UiReply(String),
}

/// How to build the spawned process argv.
///
/// `Contract3b` computes the production argv per spawn from the spawn opts
/// (session dir, `--name`, model, skills, persona). `Custom` carries a
/// fixed argv for tests that point the binary at a scripted fake peer.
///
/// This is a plain enum (not a boxed closure) so `RpcWorker` stays
/// Send-capable: worker ports are shared across tokio tasks (the UI tail
/// reaches the port from another task) and `dyn Fn` values are not `Send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgvBuilder {
    /// Contract 3b args built from each spawn's opts.
    Contract3b,
    /// Fixed argv appended after the binary (tests); ignores the opts.
    Custom(Vec<String>),
}

/// Resolve the argv for one spawn (runs after `argv[0]`).
pub fn build_args(builder: &ArgvBuilder, opts: &WorkerSpawnOpts) -> Vec<String> {
    match builder {
        ArgvBuilder::Contract3b => build_worker_args(opts),
        ArgvBuilder::Custom(args) => args.clone(),
    }
}

/// Contract 3b argv — everything after the binary:
///
/// ```text
/// --mode rpc --session-dir <dir> --name pi-plan-row-<n> --model <model>
/// --thinking high --approve --tools read,grep,find,ls,bash,edit,write
/// --skill <path> --no-extensions -e <permission-system dir>
/// --append-system-prompt <persona>
/// ```
///
/// Bare-worker invariant: with `--no-extensions` a worker loads exactly one
/// extension — the permission system — so pi-lens-hosted behaviors (unified
/// LSP, lens, autoformat at `agent_end`, autofix, the write-time test
/// runner, the opengrep auxiliary scanner, the knip/madge/jscpd family)
/// never exist in a worker regardless of flags. The determinism guarantee
/// comes from the extension set, never from flags; if a future pi release
/// moves any of these behaviors into core (or a contract deliberately loads
/// pi-lens), the flags may return **alongside** a `-e <pi-lens>` and only
/// when extensions are loadable.
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
    for skill in &opts.skills {
        args.push("--skill".to_string());
        args.push(skill.to_string_lossy().into_owned());
    }
    // Bare + permission system only: `--no-extensions` disables extension
    // discovery while the explicit `-e` still loads the permission system,
    // so pi-guardrails (and every settings-package extension) never loads
    // in workers — each access is decided solely by the relayed dialogs.
    args.push("--no-extensions".to_string());
    args.push("-e".to_string());
    args.push(opts.permission_extension.to_string_lossy().into_owned());
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
    cost: Option<f64>,
    tokens: Option<Tokens>,
    context_window: Option<u64>,
    started_at: u64,
    max_turns: u32,
    pending_tool: Option<PendingTool>,
}

impl SnapshotAcc {
    /// Clone the live accumulator into an owned snapshot (reads go through
    /// the mutex guard; by-value clone methods resolve on owned values).
    fn to_snapshot(&self, id: WorkerId) -> WorkerSnapshot {
        let context_percent = self.context_percent.as_ref().copied();
        let transcript = self.transcript.as_ref().cloned();
        let cost = self.cost.as_ref().copied();
        let tokens = self.tokens.as_ref().copied();
        let context_window = self.context_window.as_ref().copied();
        WorkerSnapshot {
            id,
            text: self.text.clone(),
            tool_uses: self.tool_uses,
            turn_count: self.turn_count,
            compaction_count: self.compaction_count,
            context_percent,
            transcript,
            cost,
            tokens,
            context_window,
            started_at: self.started_at,
            pending_tool: self.pending_tool.as_ref().cloned(),
            terminal: None,
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
        RpcEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            acc.tool_uses += 1;
            acc.pending_tool = Some(PendingTool {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.as_ref().cloned(),
            });
        }
        RpcEvent::ToolExecutionEnd { .. } => {
            // The gate cleared: the pending call is no longer awaiting an
            // answer, so the context block must not outlive its dialog.
            acc.pending_tool = None;
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

/// `data.cost`, when present (provider-reported; may be 0 or absent).
pub fn cost_of(resp: &RpcResponse) -> Option<f64> {
    resp.data.as_ref()?.as_object()?.get("cost")?.as_f64()
}

/// `data.tokens`, when the full five-field shape is present. Any missing
/// or null field makes the report `None` (render `—`); the footer never
/// shows half-consumed token counts.
pub fn tokens_of(resp: &RpcResponse) -> Option<Tokens> {
    let data = resp.data.as_ref()?.as_object()?;
    let tokens = data.get("tokens")?.as_object()?;
    let field = |key: &str| tokens.get(key).and_then(serde_json::Value::as_u64);
    match (
        field("input"),
        field("output"),
        field("cacheRead"),
        field("cacheWrite"),
        field("total"),
    ) {
        (Some(input), Some(output), Some(cache_read), Some(cache_write), Some(total)) => {
            Some(Tokens {
                input,
                output,
                cache_read,
                cache_write,
                total,
            })
        }
        _ => None,
    }
}

/// `data.contextUsage.contextWindow`, when present. The response key is
/// `contextWindow` — a `window` key does not exist and must never be used.
pub fn context_window_of(resp: &RpcResponse) -> Option<u64> {
    let data = resp.data.as_ref()?.as_object()?;
    let usage = data.get("contextUsage")?.as_object()?;
    usage.get("contextWindow")?.as_u64()
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
                guard.cost = cost_of(&resp);
                if let Some(tokens) = tokens_of(&resp) {
                    guard.tokens = Some(tokens);
                }
                if let Some(window) = context_window_of(&resp) {
                    guard.context_window = Some(window);
                }
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
    /// Answer a dialog `extension_ui_request` from a live worker (permission
    /// passthrough, decision D9). The UI renders the request it received
    /// from the event stream and sends the reply here.
    async fn reply_extension_ui(
        &self,
        id: WorkerId,
        ui_id: &str,
        reply: &UiReply,
    ) -> Result<(), WorkerError>;
    /// Release everything the port holds (kills every live worker).
    async fn dispose(&self);
}

/// Real worker port over the pi RPC client.
///
/// `Clone` is cheap (all state is Arc/plain data): the shared `live` map
/// means a clone is a second handle to the SAME workers, which is how the
/// operator-UI tail tasks reach the port while the supervise loop runs
/// (spawned tasks may only own [`Send`] values).
#[derive(Clone)]
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
        let args = build_args(&self.argv, opts);
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
            cost: None,
            tokens: None,
            context_window: None,
            started_at,
            max_turns: opts.max_turns,
            pending_tool: None,
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
        let (acc_arc, terminal_arc) = {
            let live_guard = self.live.lock().await;
            let worker = live_guard.get(&id)?;
            (worker.acc.clone(), worker.terminal.clone())
        };
        let acc_guard = acc_arc.lock().await;
        let acc: &SnapshotAcc = &acc_guard;
        let mut snap = acc.to_snapshot(id);
        snap.terminal = peek_terminal(&terminal_arc).await;
        Some(snap)
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

    async fn reply_extension_ui(
        &self,
        id: WorkerId,
        ui_id: &str,
        reply: &UiReply,
    ) -> Result<(), WorkerError> {
        let client = {
            let live_guard = self.live.lock().await;
            let worker = live_guard.get(&id).ok_or(WorkerError::UnknownWorker(id))?;
            worker.client.clone()
        };
        client
            .reply_extension_ui(ui_id, reply)
            .await
            .map_err(|e| WorkerError::UiReply(format!("{e}")))?;
        Ok(())
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
            ArgvBuilder::Contract3b,
            cwd.to_path_buf(),
            stderr_path,
        )
    }

    /// A worker port over an injected binary and argv (tests point the
    /// binary at the scripted fake-pi peer via [`ArgvBuilder::Custom`]).
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
            skills: vec![Path::new("/skills/implement-from-plan").to_path_buf()],
            tools: vec!["read".to_string(), "bash".to_string()],
            permission_extension: Path::new("/ext/permission-system").to_path_buf(),
            stderr_path: None,
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
            cost: None,
            tokens: None,
            context_window: None,
            started_at: 1_000_000,
            max_turns,
            pending_tool: None,
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
        // Bare + permission system only: the no-extensions pair always sits
        // after the skills; the last element is the persona.
        let no_xt = args
            .iter()
            .position(|a| a == "--no-extensions")
            .expect("--no-extensions present");
        assert!(no_xt > skill, "extension flags come after --skill");
        assert_eq!(args[no_xt + 1], "-e");
        assert_eq!(args[no_xt + 2], "/ext/permission-system");
        // The six pi-lens determinism flags are GONE: they cannot exist in a
        // bare worker's parser (extension discovery is off), so the argv
        // must not carry them (bare-worker invariant).
        for flag in [
            "--no-lsp",
            "--no-lens",
            "--no-tests",
            "--no-autoformat",
            "--no-autofix",
            "--no-opengrep",
        ] {
            assert!(
                !args.contains(&flag.to_string()),
                "pi-lens determinism flag {flag} absent from the worker argv"
            );
        }
        assert_eq!(args.last().cloned(), Some("You are a worker.".to_string()));
    }

    #[test]
    fn build_worker_args_emits_bare_extension_flags() {
        // The extension dir is ONE argv element (never shell-split) and the
        // pair is always emitted regardless of the persona quoting.
        let args = build_worker_args(&opts(Some(&|o| {
            o.permission_extension = Path::new("/ext/dir with spaces").to_path_buf();
            o.persona = "line one\nline two with spaces".to_string();
        })));
        let no_xt = args
            .iter()
            .position(|a| a == "--no-extensions")
            .expect("--no-extensions present");
        assert_eq!(args[no_xt + 1], "-e");
        assert_eq!(args[no_xt + 2], "/ext/dir with spaces");
        assert_eq!(
            args.last().cloned(),
            Some("line one\nline two with spaces".to_string())
        );
        assert!(
            !args.iter().any(|a| a.contains("guardrails")),
            "guardrails is never referenced in the worker argv"
        );
    }

    #[test]
    fn build_worker_args_omits_skill_and_quotes_the_persona() {
        let args = build_worker_args(&opts(Some(&|o| {
            o.skills = Vec::new();
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
    fn build_worker_args_emits_one_skill_flag_per_entry_in_order() {
        let args = build_worker_args(&opts(Some(&|o| {
            o.skills = vec![
                Path::new("/skills/implement-from-plan").to_path_buf(),
                Path::new("/skills/clean-worktree").to_path_buf(),
            ];
        })));
        let flags = args
            .iter()
            .enumerate()
            .filter(|(_i, a)| *a == "--skill")
            .map(|(i, _)| args[i + 1].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            flags,
            vec![
                "/skills/implement-from-plan".to_string(),
                "/skills/clean-worktree".to_string(),
            ]
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
                    args: None,
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

    #[test]
    fn note_event_tracks_the_pending_tool_until_the_gate_clears() {
        let mut a = acc(40);
        assert_eq!(a.pending_tool, None);
        // A start sets the pending call with lossless args…
        assert_eq!(
            note_event(
                &mut a,
                &RpcEvent::ToolExecutionStart {
                    tool_call_id: "c1".to_string(),
                    tool_name: "bash".to_string(),
                    args: Some(serde_json::json!({ "command": "mkdir -p x" })),
                }
            ),
            None
        );
        let pending = a.pending_tool.as_ref().cloned().expect("pending set");
        assert_eq!(pending.tool_call_id, "c1");
        assert_eq!(pending.tool_name, "bash");
        assert_eq!(
            pending
                .args
                .as_ref()
                .and_then(|a| a.get("command"))
                .and_then(serde_json::Value::as_str)
                .as_ref()
                .map(|s| s.to_string()),
            Some("mkdir -p x".to_string())
        );
        // …it survives an unrelated event (the whole gate window)…
        assert_eq!(note_event(&mut a, &RpcEvent::TurnStart), None);
        assert!(a.pending_tool.is_some());
        // …and an end clears it: only the terminal can arrive after the
        // operator answered, so the context never outlives its dialog.
        assert_eq!(
            note_event(
                &mut a,
                &RpcEvent::ToolExecutionEnd {
                    tool_call_id: "c1".to_string(),
                    tool_name: "bash".to_string(),
                    is_error: false,
                }
            ),
            None
        );
        assert_eq!(a.pending_tool, None);
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

    #[test]
    fn cost_of_reads_present_zero_absent_and_null_cost() {
        assert_eq!(cost_of(&stats_response(r#"{"cost": 0.45}"#)), Some(0.45));
        assert_eq!(
            cost_of(&stats_response(r#"{"cost": 0.0}"#)),
            Some(0.0),
            "zero cost is a real value, not absent"
        );
        assert_eq!(cost_of(&stats_response(r#"{"cost": null}"#)), None);
        assert_eq!(cost_of(&stats_response(r#"{}"#)), None, "missing cost");
    }

    #[test]
    fn tokens_of_reads_the_full_shape_and_rejects_partial_reports() {
        assert_eq!(
            tokens_of(&stats_response(
                r#"{"tokens": {"input": 50000, "output": 10000, "cacheRead": 40000, "cacheWrite": 5000, "total": 105000}}"#
            )),
            Some(Tokens {
                input: 50000,
                output: 10000,
                cache_read: 40000,
                cache_write: 5000,
                total: 105000,
            })
        );
        assert_eq!(tokens_of(&stats_response(r#"{}"#)), None);
        assert_eq!(
            tokens_of(&stats_response(r#"{"tokens": {"total": 1}}"#)),
            None,
            "a partial shape is absent, never half-consumed"
        );
    }

    #[test]
    fn context_window_of_reads_the_context_window_key_only() {
        assert_eq!(
            context_window_of(&stats_response(
                r#"{"contextUsage": {"contextWindow": 200000}}"#
            )),
            Some(200000)
        );
        assert_eq!(
            context_window_of(&stats_response(r#"{"contextUsage": {"window": 200000}}"#)),
            None,
            "`window` does not exist in the response"
        );
        assert_eq!(context_window_of(&stats_response(r#"{}"#)), None);
    }

    #[test]
    fn snapshot_carries_cost_tokens_and_context_window() {
        let mut a = acc(40);
        a.cost = Some(0.45);
        a.tokens = Some(Tokens {
            input: 50000,
            output: 10000,
            cache_read: 40000,
            cache_write: 5000,
            total: 105000,
        });
        a.context_window = Some(200000);
        let snap = a.to_snapshot(7);
        assert_eq!(snap.cost, Some(0.45));
        assert_eq!(
            snap.tokens,
            Some(Tokens {
                input: 50000,
                output: 10000,
                cache_read: 40000,
                cache_write: 5000,
                total: 105000,
            })
        );
        assert_eq!(snap.context_window, Some(200000));
    }

    #[test]
    fn to_snapshot_starts_with_no_terminal() {
        // The accumulator's own view is always live; the port wrapper
        // (`RpcWorker::snapshot`) stamps the recorded terminal on top.
        let a = acc(40);
        assert_eq!(a.to_snapshot(7).terminal, None);
        assert!(a.to_snapshot(7).terminal.is_none());
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
