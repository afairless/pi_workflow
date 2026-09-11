//! pi RPC client over JSONL (pi `--mode rpc`) — strict framing, commands
//! with id correlation, event streaming, and the extension-UI dialog
//! sub-protocol (per `docs/rpc.md` of the installed pi package).
//!
//! One `RpcClient` owns one spawned `pi --mode rpc` process (or a fake peer
//! in tests) and a background reader task that frames stdout, routes
//! `response` frames to pending requests, and broadcasts events (including
//! `extension_ui_request` dialogs) to subscribers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, broadcast, oneshot};

/// Orchestrator-side cap on one JSONL frame (bytes). The pi protocol
/// documents only LF-split framing; this is our belt-and-braces ceiling.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Default per-command response deadline.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Broadcast channel capacity for events; the worker subscribes immediately
/// after spawn, so dialogs cannot be missed in practice. Surplus copies
/// enable drop-in reconnection logic.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Clone, thiserror::Error)]
pub enum RpcError {
    #[error("failed to spawn pi: {0}")]
    Spawn(String),
    #[error("i/o error on the RPC pipe: {0}")]
    Io(String),
    #[error("peer closed the RPC stream")]
    PeerClosed,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("command {command:?} timed out after {timeout:?}")]
    Timeout { command: String, timeout: Duration },
    #[error("RPC peer unavailable: {0}")]
    Unavailable(&'static str),
}

/// Where a prompt lands when the agent is already streaming (`prompt` frame
/// `streamingBehavior` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingBehavior {
    /// Deliver after the current turn's tool calls, before the next LLM call.
    Steer,
    /// Deliver only when the agent stops.
    FollowUp,
}

/// A command the orchestrator can send (subset of the RPC protocol).
#[derive(Debug, Clone)]
pub enum RpcCommand {
    Prompt {
        message: String,
        streaming_behavior: Option<StreamingBehavior>,
    },
    Abort,
    GetSessionStats,
    GetMessages,
    GetLastAssistantText,
}

/// Decoded `response` frame.
#[derive(Debug, Clone)]
pub struct RpcResponse {
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// A decoded event frame (everything that is not a `response`).
#[derive(Debug, Clone)]
pub enum RpcEvent {
    AgentStart,
    AgentEnd {
        will_retry: bool,
    },
    AgentSettled,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageEnd,
    /// Streaming delta of the assistant message.
    MessageUpdate(MessageDelta),
    BashExecutionUpdate {
        command_id: Option<String>,
        delta: String,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
    QueueUpdate,
    CompactionStart,
    CompactionEnd,
    AutoRetryStart,
    AutoRetryEnd,
    /// A dialog (`select`/`confirm`/`input`/`editor`) must be answered via
    /// `reply_extension_ui`; fire-and-forget methods are surfaced too so the
    /// UI can render notifications/status.
    ExtensionUiRequest(ExtensionUiRequest),
    Unknown {
        name: String,
        raw: serde_json::Value,
    },
}

/// One `assistantMessageEvent` delta inside `message_update`.
#[derive(Debug, Clone)]
pub enum MessageDelta {
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
    },
    ToolCallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
    },
    /// A delta kind the client knows exists but does not model yet.
    Other {
        kind: String,
        raw: serde_json::Value,
    },
}

/// Extension UI dialog or fire-and-forget request.
#[derive(Debug, Clone)]
pub struct ExtensionUiRequest {
    pub id: String,
    pub method: UiMethod,
    pub title: Option<String>,
    pub message: Option<String>,
    pub options: Vec<String>,
    pub placeholder: Option<String>,
    pub prefill: Option<String>,
    pub timeout_ms: Option<u64>,
}

impl ExtensionUiRequest {
    /// True for dialog methods that block until an `extension_ui_response`.
    pub fn is_dialog(&self) -> bool {
        matches!(
            self.method,
            UiMethod::Select | UiMethod::Confirm | UiMethod::Input | UiMethod::Editor
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMethod {
    Select,
    Confirm,
    Input,
    Editor,
    Notify,
    SetStatus,
    SetWidget,
    SetTitle,
    SetEditorText,
}

/// A client-side reply to a dialog request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiReply {
    /// `select`/`input`/`editor`: the chosen option or entered text.
    Value(String),
    /// `confirm`: yes/no.
    Confirmed(bool),
    /// Any dialog: dismiss (`cancelled: true`).
    Cancelled,
}

/// Spawn parameters for the RPC peer.
pub struct SpawnOptions {
    pub binary: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Where to append the peer's stderr; `None` discards it.
    pub stderr_path: Option<PathBuf>,
}

struct PendingEntry {
    command: String,
    tx: oneshot::Sender<Result<RpcResponse, RpcError>>,
}

/// Owned handle to one RPC peer. `Drop` kills the peer's process group so
/// worker trees cannot survive the orchestrator.
pub struct RpcClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<String, PendingEntry>>>,
    events: Arc<Mutex<Option<broadcast::Sender<RpcEvent>>>>,
    next_id: Arc<AtomicU64>,
    dead: Arc<AtomicBool>,
    peer_pid: Option<u32>,
    command_timeout: Duration,
}

impl RpcClient {
    /// Spawn the peer and start the framing/read loop.
    pub async fn spawn(opts: &SpawnOptions) -> Result<Self, RpcError> {
        let mut cmd = Command::new(&opts.binary);
        cmd.args(&opts.args)
            .current_dir(&opts.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(match &opts.stderr_path {
                Some(path) => Stdio::from(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map_err(|e| RpcError::Io(e.to_string()))?,
                ),
                None => Stdio::null(),
            });
        // New process group so teardown can kill every descendant, not just
        // the direct child.
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| RpcError::Spawn(e.to_string()))?;
        let peer_pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or(RpcError::Unavailable("peer stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(RpcError::Unavailable("peer stdout was not piped"))?;

        let pending = Arc::new(Mutex::new(HashMap::new()));
        let events: Arc<Mutex<Option<broadcast::Sender<RpcEvent>>>> = Arc::new(Mutex::new(None));
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        *events.lock().await = Some(tx.clone());
        let dead = Arc::new(AtomicBool::new(false));
        let next_id = Arc::new(AtomicU64::new(0));

        let client = RpcClient {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: pending.clone(),
            events: events.clone(),
            next_id: next_id.clone(),
            dead: dead.clone(),
            peer_pid,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        };

        tokio::spawn(read_loop(stdout, pending, tx, events, dead));
        Ok(client)
    }

    /// Subscribe to the peer's decoded event stream. Returns `None` when the
    /// read loop has already terminated (the stream is closed).
    pub async fn subscribe(&self) -> Option<broadcast::Receiver<RpcEvent>> {
        let guard = self.events.lock().await;
        Some(guard.as_ref()?.subscribe())
    }

    /// Per-command response deadline (default `DEFAULT_COMMAND_TIMEOUT`).
    pub fn set_command_timeout(&mut self, timeout: Duration) {
        self.command_timeout = timeout;
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Send a command and await its `response` frame, correlated by id.
    pub async fn request(&self, cmd: RpcCommand) -> Result<RpcResponse, RpcError> {
        let id = format!("pi-plan-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let frame = command_frame(&id, &cmd);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                id.clone(),
                PendingEntry {
                    command: command_name(&cmd).to_string(),
                    tx,
                },
            );
        }
        self.write_line(&frame).await?;

        match tokio::time::timeout(self.command_timeout, rx).await {
            Ok(Ok(Ok(resp))) => Ok(resp),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(_)) => Err(RpcError::PeerClosed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout {
                    command: command_name(&cmd).to_string(),
                    timeout: self.command_timeout,
                })
            }
        }
    }

    /// Answer a dialog `extension_ui_request` (permission passthrough).
    pub async fn reply_extension_ui(&self, id: &str, reply: &UiReply) -> Result<(), RpcError> {
        self.write_line(&ui_reply_frame(id, reply)).await
    }

    /// Kill the peer's process group and reap the direct child.
    pub async fn kill(&self) -> Result<(), RpcError> {
        if let Some(pid) = self.peer_pid {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        Ok(())
    }

    async fn write_line(&self, frame: &serde_json::Value) -> Result<(), RpcError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(RpcError::PeerClosed);
        }
        let line = serde_json::to_string(frame)
            .map_err(|e| RpcError::Protocol(format!("cannot serialize frame: {e}")))?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| RpcError::Io(e.to_string()))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| RpcError::Io(e.to_string()))?;
        stdin.flush().await.map_err(|e| RpcError::Io(e.to_string()))
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        // Synchronous process-group kill so orphans cannot outlive the client
        // even when dropped from a non-async context.
        if let Some(pid) = self.peer_pid {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
}

/// The `extension_ui_response` frame for a dialog reply.
fn ui_reply_frame(id: &str, reply: &UiReply) -> serde_json::Value {
    let (value, confirmed, cancelled) = match reply {
        UiReply::Value(v) => (Some(v.as_str()), None, false),
        UiReply::Confirmed(c) => (None, Some(*c), false),
        UiReply::Cancelled => (None, None, true),
    };
    let mut frame = serde_json::json!({
        "type": "extension_ui_response",
        "id": id,
    });
    if let Some(v) = value {
        frame["value"] = serde_json::Value::String(v.to_string());
    }
    if let Some(c) = confirmed {
        frame["confirmed"] = serde_json::Value::Bool(c);
    }
    if cancelled {
        frame["cancelled"] = serde_json::Value::Bool(true);
    }
    frame
}

fn command_name(cmd: &RpcCommand) -> &'static str {
    match cmd {
        RpcCommand::Prompt { .. } => "prompt",
        RpcCommand::Abort => "abort",
        RpcCommand::GetSessionStats => "get_session_stats",
        RpcCommand::GetMessages => "get_messages",
        RpcCommand::GetLastAssistantText => "get_last_assistant_text",
    }
}

fn command_frame(id: &str, cmd: &RpcCommand) -> serde_json::Value {
    match cmd {
        RpcCommand::Prompt {
            message,
            streaming_behavior,
        } => {
            let mut frame = serde_json::json!({
                "id": id,
                "type": "prompt",
                "message": message,
            });
            match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    frame["streamingBehavior"] = serde_json::Value::String("steer".to_string());
                }
                Some(StreamingBehavior::FollowUp) => {
                    frame["streamingBehavior"] = serde_json::Value::String("followUp".to_string());
                }
                None => {}
            }
            frame
        }
        RpcCommand::Abort => serde_json::json!({ "id": id, "type": "abort" }),
        RpcCommand::GetSessionStats => serde_json::json!({ "id": id, "type": "get_session_stats" }),
        RpcCommand::GetMessages => serde_json::json!({ "id": id, "type": "get_messages" }),
        RpcCommand::GetLastAssistantText => {
            serde_json::json!({ "id": id, "type": "get_last_assistant_text" })
        }
    }
}

/// Framing loop: strict JSONL (split on `\n` only, tolerate a trailing `\r`),
/// object-only frames, oversized frames rejected as `Protocol` errors.
/// On any protocol violation or EOF the loop fails every pending request,
/// publishes no further events, and the client becomes permanently dead.
async fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<String, PendingEntry>>>,
    events: broadcast::Sender<RpcEvent>,
    events_handle: Arc<Mutex<Option<broadcast::Sender<RpcEvent>>>>,
    dead: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let outcome: Result<(), RpcError> = loop {
        buf.clear();
        match read_line_capped(&mut reader, MAX_FRAME_BYTES, &mut buf).await {
            Ok(0) => break Ok(()), // clean EOF
            Ok(_) => {}
            Err(err) => break Err(err),
        }
        let line = match String::from_utf8(buf.clone()) {
            Ok(line) => line,
            Err(_) => break Err(RpcError::Protocol("frame is not UTF-8".to_string())),
        };
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let frame: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => break Err(RpcError::Protocol(format!("frame is not JSON: {e}"))),
        };
        if !frame.is_object() {
            break Err(RpcError::Protocol("frame is not a JSON object".to_string()));
        }
        let frame_type = frame.get("type").and_then(serde_json::Value::as_str);
        match frame_type {
            Some("response") => route_response(frame, &pending).await,
            Some("extension_ui_request") => {
                if let Some(req) = decode_ui_request(&frame) {
                    let _ = events.send(RpcEvent::ExtensionUiRequest(req));
                }
            }
            Some(other) => {
                if let Some(event) = decode_event(other, &frame) {
                    let _ = events.send(event);
                }
            }
            None => {
                break Err(RpcError::Protocol("frame lacks a type field".to_string()));
            }
        }
    };

    if let Err(err) = &outcome {
        eprintln!("[pi-plan rpc] read loop ended with error: {err}");
    }
    dead.store(true, Ordering::SeqCst);
    *events_handle.lock().await = None;
    drop(events);

    // Fail every pending request so awaiting callers unblock with an error.
    let reason = match outcome {
        Ok(()) => RpcError::PeerClosed,
        Err(err) => err,
    };
    let mut pending = pending.lock().await;
    for (_, entry) in pending.drain() {
        let _ = entry.tx.send(Err(reason.clone()));
    }
}

/// Read one LF-terminated record, capped at `cap` bytes. Returns the record
/// length, or `0` at a clean EOF (nothing buffered). A peer that dies
/// mid-record is a protocol error.
async fn read_line_capped<R>(
    reader: &mut R,
    cap: usize,
    out: &mut Vec<u8>,
) -> Result<usize, RpcError>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let avail = reader
            .fill_buf()
            .await
            .map_err(|e| RpcError::Io(e.to_string()))?;
        if avail.is_empty() {
            if out.is_empty() {
                return Ok(0);
            }
            return Err(RpcError::Protocol(
                "peer closed the stream mid-frame".to_string(),
            ));
        }
        if let Some(idx) = avail.iter().position(|b| *b == b'\n') {
            out.extend_from_slice(&avail[..=idx]);
            reader.consume(idx + 1);
            return Ok(out.len());
        }
        out.extend_from_slice(avail);
        if out.len() >= cap {
            return Err(RpcError::Protocol(format!(
                "frame exceeds the {cap}-byte cap"
            )));
        }
        let consumed = avail.len();
        reader.consume(consumed);
    }
}

/// Resolve a `response` frame to its pending request. Id correlation wins;
/// a response without an id falls back to the (single) pending request of
/// the same command name.
async fn route_response(
    frame: serde_json::Value,
    pending: &Arc<Mutex<HashMap<String, PendingEntry>>>,
) {
    let command = frame
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let response = RpcResponse {
        id: frame
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        command,
        success: frame
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        data: frame.get("data").cloned(),
        error: frame
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
    };

    let mut pending = pending.lock().await;
    if let Some(id) = &response.id
        && let Some(entry) = pending.remove(id)
    {
        let _ = entry.tx.send(Ok(response));
        return;
    }
    // No id (or no pending request with that id): route by command name when
    // unambiguous — the peer responded without echoing our id.
    let matches: Vec<String> = pending
        .iter()
        .filter(|(_, entry)| entry.command == response.command)
        .map(|(id, _)| id.clone())
        .collect();
    if matches.len() == 1
        && let Some(entry) = pending.remove(&matches[0])
    {
        let _ = entry.tx.send(Ok(response));
    }
}

fn decode_event(name: &str, frame: &serde_json::Value) -> Option<RpcEvent> {
    match name {
        "agent_start" => Some(RpcEvent::AgentStart),
        "agent_end" => Some(RpcEvent::AgentEnd {
            will_retry: frame
                .get("willRetry")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }),
        "agent_settled" => Some(RpcEvent::AgentSettled),
        "turn_start" => Some(RpcEvent::TurnStart),
        "turn_end" => Some(RpcEvent::TurnEnd),
        "message_start" => Some(RpcEvent::MessageStart),
        "message_end" => Some(RpcEvent::MessageEnd),
        "message_update" => frame
            .get("assistantMessageEvent")
            .and_then(decode_message_delta)
            .map(RpcEvent::MessageUpdate),
        "bash_execution_update" => Some(RpcEvent::BashExecutionUpdate {
            command_id: frame
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            delta: frame
                .get("delta")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "tool_execution_start" => Some(RpcEvent::ToolExecutionStart {
            tool_call_id: required_str(frame, "toolCallId")?,
            tool_name: required_str(frame, "toolName")?,
        }),
        "tool_execution_update" => Some(RpcEvent::ToolExecutionUpdate {
            tool_call_id: required_str(frame, "toolCallId")?,
            tool_name: required_str(frame, "toolName")?,
        }),
        "tool_execution_end" => Some(RpcEvent::ToolExecutionEnd {
            tool_call_id: required_str(frame, "toolCallId")?,
            tool_name: required_str(frame, "toolName")?,
            is_error: frame
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }),
        "queue_update" => Some(RpcEvent::QueueUpdate),
        "compaction_start" => Some(RpcEvent::CompactionStart),
        "compaction_end" => Some(RpcEvent::CompactionEnd),
        "auto_retry_start" => Some(RpcEvent::AutoRetryStart),
        "auto_retry_end" => Some(RpcEvent::AutoRetryEnd),
        other => Some(RpcEvent::Unknown {
            name: other.to_string(),
            raw: frame.clone(),
        }),
    }
}

fn required_str(frame: &serde_json::Value, key: &str) -> Option<String> {
    Some(frame.get(key)?.as_str()?.to_string())
}

fn decode_message_delta(value: &serde_json::Value) -> Option<MessageDelta> {
    let kind = value.get("type")?.as_str()?;
    let content_index = value
        .get("contentIndex")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let delta = value
        .get("delta")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    match kind {
        "text_start" => Some(MessageDelta::TextStart { content_index }),
        "text_delta" => Some(MessageDelta::TextDelta {
            content_index,
            delta,
        }),
        "text_end" => Some(MessageDelta::TextEnd { content_index }),
        "thinking_start" => Some(MessageDelta::ThinkingStart { content_index }),
        "thinking_delta" => Some(MessageDelta::ThinkingDelta {
            content_index,
            delta,
        }),
        "thinking_end" => Some(MessageDelta::ThinkingEnd { content_index }),
        "toolcall_start" => Some(MessageDelta::ToolCallStart {
            content_index,
            id: value.get("id")?.as_str()?.to_string(),
            tool_name: value.get("toolName")?.as_str()?.to_string(),
        }),
        "toolcall_delta" => Some(MessageDelta::ToolCallDelta {
            content_index,
            delta,
        }),
        "toolcall_end" => Some(MessageDelta::ToolCallEnd { content_index }),
        other => Some(MessageDelta::Other {
            kind: other.to_string(),
            raw: value.clone(),
        }),
    }
}

fn decode_ui_request(frame: &serde_json::Value) -> Option<ExtensionUiRequest> {
    let id = frame.get("id")?.as_str()?.to_string();
    let method = match frame.get("method")?.as_str()? {
        "select" => UiMethod::Select,
        "confirm" => UiMethod::Confirm,
        "input" => UiMethod::Input,
        "editor" => UiMethod::Editor,
        "notify" => UiMethod::Notify,
        "setStatus" => UiMethod::SetStatus,
        "setWidget" => UiMethod::SetWidget,
        "setTitle" => UiMethod::SetTitle,
        "set_editor_text" => UiMethod::SetEditorText,
        _ => return None,
    };
    let options = frame
        .get("options")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Some(ExtensionUiRequest {
        id,
        method,
        title: str_field(frame, "title"),
        message: str_field(frame, "message"),
        options,
        placeholder: str_field(frame, "placeholder"),
        prefill: str_field(frame, "prefill"),
        timeout_ms: frame.get("timeout").and_then(serde_json::Value::as_u64),
    })
}

fn str_field(frame: &serde_json::Value, key: &str) -> Option<String> {
    frame
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_frame_shapes_match_the_protocol() {
        let f = command_frame(
            "req-1",
            &RpcCommand::Prompt {
                message: "hello".to_string(),
                streaming_behavior: None,
            },
        );
        assert_eq!(f["id"], "req-1");
        assert_eq!(f["type"], "prompt");
        assert_eq!(f["message"], "hello");
        assert!(f.get("streamingBehavior").is_none());

        let f = command_frame(
            "req-2",
            &RpcCommand::Prompt {
                message: "steer".to_string(),
                streaming_behavior: Some(StreamingBehavior::Steer),
            },
        );
        assert_eq!(f["streamingBehavior"], "steer");

        let f = command_frame("req-3", &RpcCommand::Abort);
        assert_eq!(f["type"], "abort");

        let f = command_frame("req-4", &RpcCommand::GetSessionStats);
        assert_eq!(f["type"], "get_session_stats");
    }

    #[test]
    fn ui_reply_frames_match_the_protocol() {
        let frame = ui_reply_frame("u-1", &UiReply::Value("Allow".to_string()));
        assert_eq!(frame["type"], "extension_ui_response");
        assert_eq!(frame["id"], "u-1");
        assert_eq!(frame["value"], "Allow");
        assert!(frame.get("confirmed").is_none());
        let frame = ui_reply_frame("u-2", &UiReply::Confirmed(false));
        assert_eq!(frame["confirmed"], false);
        let frame = ui_reply_frame("u-3", &UiReply::Cancelled);
        assert_eq!(frame["cancelled"], true);
    }

    #[test]
    fn decoder_handles_the_known_event_shapes() {
        let frame: serde_json::Value = serde_json::from_str(
            r#"{"type":"message_update","usage":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hi"}}"#,
        )
        .expect("json");
        match decode_event("message_update", &frame) {
            Some(RpcEvent::MessageUpdate(MessageDelta::TextDelta {
                content_index: 0,
                delta,
            })) => assert_eq!(delta, "Hi"),
            other => panic!("unexpected decode: {other:?}"),
        }

        let frame: serde_json::Value = serde_json::from_str(
            r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{}}"#,
        )
        .expect("json");
        match decode_event("tool_execution_start", &frame) {
            Some(RpcEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
            }) => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(tool_name, "bash");
            }
            other => panic!("unexpected decode: {other:?}"),
        }

        let frame: serde_json::Value =
            serde_json::from_str(r#"{"type":"unknown_event_xyz","foo":1}"#).expect("json");
        match decode_event("unknown_event_xyz", &frame) {
            Some(RpcEvent::Unknown { name, .. }) => assert_eq!(name, "unknown_event_xyz"),
            other => panic!("unexpected decode: {other:?}"),
        }
    }

    #[test]
    fn ui_request_decoding_covers_dialogs_and_fire_and_forget() {
        let frame: serde_json::Value = serde_json::from_str(
            r#"{"type":"extension_ui_request","id":"u-1","method":"select","title":"Allow?","options":["Allow","Block"],"timeout":10000}"#,
        )
        .expect("json");
        let req = decode_ui_request(&frame).expect("decoded");
        assert_eq!(req.id, "u-1");
        assert_eq!(req.method, UiMethod::Select);
        assert_eq!(req.options, ["Allow", "Block"]);
        assert_eq!(req.timeout_ms, Some(10_000));
        assert!(req.is_dialog());

        let frame: serde_json::Value = serde_json::from_str(
            r#"{"type":"extension_ui_request","id":"u-2","method":"notify","message":"hi"}"#,
        )
        .expect("json");
        let req = decode_ui_request(&frame).expect("decoded");
        assert_eq!(req.method, UiMethod::Notify);
        assert!(!req.is_dialog());
    }

    #[test]
    fn read_line_capped_rejects_oversized_frames_and_honors_crlf() {
        // Exercise the framing helper over an in-memory reader.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut out = Vec::new();
            let mut reader = BufReader::new(&b"{\"type\":\"x\"}\r\n"[..]);
            let n = read_line_capped(&mut reader, MAX_FRAME_BYTES, &mut out)
                .await
                .expect("read");
            assert_eq!(n, 14); // 12 JSON chars + \r + \n
            assert_eq!(&out[..], b"{\"type\":\"x\"}\r\n");

            // Oversized: a line longer than the cap must error, not buffer.
            let mut out = Vec::new();
            let bytes = [b'a'; 100];
            let mut reader = BufReader::new(&bytes[..]);
            let err = read_line_capped(&mut reader, 64, &mut out).await;
            assert!(matches!(err, Err(RpcError::Protocol(_))));
        });
    }
}
