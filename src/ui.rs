//! Plain-terminal operator UI (step 8) — a pure renderer.
//!
//! Every piece of layout is a string/struct transform, so the interactive
//! loop in the binary stays thin and everything here is unit-testable
//! (plan's "renderer logic kept pure"). Conventions:
//!
//! - dialogs and ASK questions render to stdout; live traces, banners, and
//!   reports go to stderr (stdout/stderr discipline from the plan);
//! - no ANSI codes in v1 (ANSI styling is optional/minimal);
//! - marker extraction (`PI_WORKER_STATUS` / `QUESTION:`) lives with the
//!   worker port (`worker::parse_worker_status` / `worker::parse_question`);
//!   the renderer consumes already-extracted questions.
//!
//! The interactive loop itself (reading stdin, dispatching replies) is
//! binary-side; ui.rs only turns requests/events/input into text and
//! `UiReply` values.

use crate::rpc::{ExtensionUiRequest, MessageDelta, RpcEvent, UiMethod, UiReply};

/// Line commands the operator can type at any dialog / answer prompt
/// (plan step 8: "line commands `stop`, `restart`, `status` also accepted
/// at any prompt"). `stop`/`restart` flip the loop's `RunControl` flags;
/// `status` prints the current worker status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCommand {
    Stop,
    Restart,
    Status,
}

/// Parse a typed line as a line command; `None` when it is not one.
/// Case-insensitive; `resume` is accepted as an alias of `restart`.
pub fn line_command(input: &str) -> Option<LineCommand> {
    let lower = input.trim().to_lowercase();
    if lower == "stop" {
        return Some(LineCommand::Stop);
    }
    if lower == "restart" || lower == "resume" {
        return Some(LineCommand::Restart);
    }
    if lower == "status" {
        return Some(LineCommand::Status);
    }
    None
}

/// Bounded live tail per worker: keeps the last `capacity` rendered lines so
/// `bash_execution_update` chunks cannot flood the terminal or the process
/// memory (risks-table mitigation). Evicts oldest-first past capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRing {
    /// Maximum number of lines retained.
    pub capacity: usize,
    /// Retained lines, oldest first.
    pub lines: Vec<String>,
}

impl TraceRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            lines: Vec::new(),
        }
    }

    /// Append one rendered line, evicting the oldest past capacity.
    pub fn push(&mut self, line: String) {
        self.lines.push(line);
        while self.lines.len() > self.capacity {
            self.lines.remove(0);
        }
    }

    /// The last `n` lines, newest last. Never more than `capacity`.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let start = self.lines.len().min(n);
        let from = self.lines.len() - start;
        self.lines.iter().skip(from).cloned().collect::<Vec<_>>()
    }
}

/// Prefix thinking chunks distinctly so the assistant's message text stays
/// visually separate in the tail.
pub fn thinking_chunk(delta: &str) -> String {
    format!("⟦thinking: {delta}⟧")
}

/// Apply one `message_update` delta to the worker's live text buffer.
///
/// Text chunks are pushed into `buffer` and returned for the live tail;
/// thinking chunks are rendered (prefixed) but never enter the message
/// text. Structural deltas return `None`.
pub fn apply_delta(buffer: &mut String, delta: &MessageDelta) -> Option<String> {
    match delta {
        MessageDelta::TextDelta { delta, .. } => {
            buffer.push_str(delta.as_str());
            Some(delta.clone())
        }
        MessageDelta::ThinkingDelta { delta, .. } => Some(thinking_chunk(delta.as_str())),
        MessageDelta::TextStart { .. }
        | MessageDelta::TextEnd { .. }
        | MessageDelta::ThinkingStart { .. }
        | MessageDelta::ThinkingEnd { .. }
        | MessageDelta::ToolCallStart { .. }
        | MessageDelta::ToolCallDelta { .. }
        | MessageDelta::ToolCallEnd { .. }
        | MessageDelta::Other { .. } => None,
    }
}

/// One rendered trace line for terminal-facing events.
///
/// Returns `None` for events the tail does not surface directly: message
/// deltas (rendered via [`apply_delta`]), structural message events,
/// dialogs (rendered as interactive prompts), and high-frequency updates
/// (tool-call deltas, queue churn).
pub fn render_event_line(event: &RpcEvent) -> Option<String> {
    match event {
        RpcEvent::AgentStart => Some("agent started".to_string()),
        RpcEvent::AgentEnd { will_retry } => {
            if *will_retry {
                Some("agent end · will retry".to_string())
            } else {
                Some("agent end".to_string())
            }
        }
        RpcEvent::AgentSettled => Some("agent settled".to_string()),
        RpcEvent::TurnStart => Some("── turn start".to_string()),
        RpcEvent::TurnEnd => Some("── turn end".to_string()),
        RpcEvent::MessageStart | RpcEvent::MessageEnd => None,
        RpcEvent::MessageUpdate(_) => None,
        RpcEvent::BashExecutionUpdate { delta, .. } => {
            let chunk = delta.trim();
            if chunk.is_empty() {
                None
            } else {
                Some(format!("$ {chunk}"))
            }
        }
        RpcEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            ..
        } => Some(format!("tool: {tool_name} ({tool_call_id})")),
        RpcEvent::ToolExecutionUpdate { .. } => None,
        RpcEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            ..
        } => {
            if *is_error {
                Some(format!("tool failed: {tool_name}"))
            } else {
                Some(format!("tool done: {tool_name}"))
            }
        }
        RpcEvent::QueueUpdate => None,
        RpcEvent::CompactionStart => Some("context compaction — a `restart` may help".to_string()),
        RpcEvent::CompactionEnd => Some("compaction done".to_string()),
        RpcEvent::AutoRetryStart | RpcEvent::AutoRetryEnd => None,
        RpcEvent::ExtensionUiRequest(_) => None,
        RpcEvent::Unknown { .. } => None,
    }
}

/// Human-friendly elapsed duration (`412ms`, `9s`, `1m05s`).
pub fn format_duration(elapsed_ms: u64) -> String {
    if elapsed_ms < 1000 {
        format!("{elapsed_ms}ms")
    } else if elapsed_ms < 60_000 {
        let secs = elapsed_ms / 1000;
        format!("{secs}s")
    } else {
        let minutes = elapsed_ms / 60_000;
        let seconds = (elapsed_ms % 60_000) / 1000;
        if seconds < 10 {
            format!("{minutes}m0{seconds}s")
        } else {
            format!("{minutes}m{seconds}s")
        }
    }
}

/// One-line operator status: `row <id> · agent <id> · turns <t>/<max> ·
/// ctx <pct|?> · <elapsed>`.
///
/// `context_percent` is `None` right after a compaction (unknown → `?`),
/// mirroring the snapshot's contract.
pub fn format_status_line(
    row_id: &str,
    agent_id: Option<&str>,
    turns: u32,
    max_turns: u32,
    context_percent: Option<f64>,
    elapsed_ms: u64,
) -> String {
    let agent = agent_id.unwrap_or("—");
    let ctx = match context_percent {
        Some(p) => {
            let pct = p as u64;
            format!("{pct}%")
        }
        None => "?".to_string(),
    };
    format!(
        "row {row_id} · agent {agent} · turns {turns}/{max_turns} · ctx {ctx} · {}",
        format_duration(elapsed_ms)
    )
}

/// The method label used as the dialog heading when no title is set.
pub fn method_label(method: &UiMethod) -> String {
    match method {
        UiMethod::Select => "select".to_string(),
        UiMethod::Confirm => "confirm".to_string(),
        UiMethod::Input => "input".to_string(),
        UiMethod::Editor => "editor".to_string(),
        UiMethod::Notify => "notification".to_string(),
        UiMethod::SetStatus => "status".to_string(),
        UiMethod::SetWidget => "widget".to_string(),
        UiMethod::SetTitle => "title".to_string(),
        UiMethod::SetEditorText => "editor text".to_string(),
    }
}

/// The dialog banner lines for an `extension_ui_request` (stdout):
/// a heading with the title (or method), the message, then the hints.
pub fn dialog_lines(req: &ExtensionUiRequest) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let title = req
        .title
        .as_ref()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| method_label(&req.method));
    out.push(format!("── {title} ──"));
    if let Some(message) = &req.message
        && !message.is_empty()
    {
        for line in message.lines() {
            out.push(line.to_string());
        }
    }
    option_lines(req, &mut out);
    out
}

/// Append the input hints to a dialog's lines: a numbered list for
/// `select`, `(y)/(n)/(c)` hints for `confirm`, and a placeholder line for
/// `input`.
fn option_lines(req: &ExtensionUiRequest, out: &mut Vec<String>) {
    match req.method {
        UiMethod::Select => {
            for (i, option) in req.options.iter().enumerate() {
                let n = i + 1;
                out.push(format!("  {n}. {option}"));
            }
            out.push("  (c) cancel".to_string());
        }
        UiMethod::Confirm => {
            out.push("  (y) yes / (n) no / (c) cancel".to_string());
        }
        UiMethod::Input | UiMethod::Editor => {
            if let Some(ph) = &req.placeholder
                && !ph.is_empty()
                && req.prefill.is_none()
            {
                out.push(format!("  placeholder: {ph}"));
            }
            if let Some(prefill) = &req.prefill
                && !prefill.is_empty()
            {
                out.push(format!("  prefill: {prefill}"));
            }
            out.push("  (c) cancel".to_string());
        }
        _ => {}
    }
}

/// The input prompt label for a dialog (`select>`, `confirm>`, `input>`).
pub fn dialog_prompt_label(req: &ExtensionUiRequest) -> String {
    let label = method_label(&req.method);
    format!("{label}>")
}

/// Map the operator's typed reply to a `UiReply` for a dialog request.
///
/// `None` means the input is not a valid reply (the loop re-prompts).
/// `c`/`cancel` cancels any dialog. Select takes a 1-based option number;
/// confirm takes `y`/`yes`/`n`/`no`; input/editor take the raw text.
pub fn reply_from_input(req: &ExtensionUiRequest, input: &str) -> Option<UiReply> {
    let line = input.trim();
    if line.is_empty() {
        return None;
    }
    let lower = line.to_lowercase();
    if lower == "c" || lower == "cancel" {
        return Some(UiReply::Cancelled);
    }
    match req.method {
        UiMethod::Select => {
            for option in req.options.iter() {
                let n = req.options.iter().position(|o| o == option).unwrap_or(0) + 1;
                if lower == format!("{n}") {
                    return Some(UiReply::Value(option.clone()));
                }
            }
            None
        }
        UiMethod::Confirm => {
            if lower == "y" || lower == "yes" {
                Some(UiReply::Confirmed(true))
            } else if lower == "n" || lower == "no" {
                Some(UiReply::Confirmed(false))
            } else {
                None
            }
        }
        UiMethod::Input | UiMethod::Editor => Some(UiReply::Value(line.to_string())),
        _ => None,
    }
}

/// The banner lines for an ASK question pause (stdout): heading, the
/// worker's question, and the answer prompt.
pub fn ask_lines(question: &str) -> Vec<String> {
    vec![
        "── worker question ──".to_string(),
        question.trim().to_string(),
        "answer>".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select_req(options: Vec<String>) -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: "ui-1".to_string(),
            method: UiMethod::Select,
            title: Some("pick".to_string()),
            message: Some("choose one".to_string()),
            options,
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        }
    }

    fn confirm_req() -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: "ui-2".to_string(),
            method: UiMethod::Confirm,
            title: None,
            message: Some("allow this bash? ".to_string()),
            options: Vec::new(),
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        }
    }

    fn input_req() -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: "ui-3".to_string(),
            method: UiMethod::Input,
            title: None,
            message: Some("target commit?".to_string()),
            options: Vec::new(),
            placeholder: Some("abc123".to_string()),
            prefill: None,
            timeout_ms: None,
        }
    }

    // ---- line commands ----

    #[test]
    fn line_commands_parse_case_insensitively_with_resume_alias() {
        assert_eq!(line_command("stop"), Some(LineCommand::Stop));
        assert_eq!(line_command("  STOP "), Some(LineCommand::Stop));
        assert_eq!(line_command("status"), Some(LineCommand::Status));
        assert_eq!(line_command("Restart"), Some(LineCommand::Restart));
        assert_eq!(line_command("resume"), Some(LineCommand::Restart));
        assert_eq!(line_command("1"), None);
        assert_eq!(line_command(""), None);
    }

    // ---- trace ring ----

    #[test]
    fn trace_ring_evicts_oldest_past_capacity() {
        let mut ring = TraceRing::new(3);
        ring.push("a".to_string());
        ring.push("b".to_string());
        ring.push("c".to_string());
        ring.push("d".to_string());
        assert_eq!(ring.lines, vec!["b", "c", "d"]);
        assert_eq!(ring.tail(2), vec!["c", "d"]);
        assert_eq!(ring.tail(10), vec!["b", "c", "d"]);
    }

    #[test]
    fn trace_ring_capacity_is_at_least_one() {
        assert_eq!(TraceRing::new(0).capacity, 1);
        assert_eq!(TraceRing::new(0).lines.len(), 0);
    }

    // ---- deltas & event lines ----

    #[test]
    fn text_deltas_accumulate_and_render_but_thinking_does_not_pollute() {
        let mut buf = String::new();
        assert_eq!(
            apply_delta(
                &mut buf,
                &MessageDelta::TextDelta {
                    content_index: 0,
                    delta: "hello ".to_string(),
                }
            ),
            Some("hello ".to_string())
        );
        assert_eq!(
            apply_delta(
                &mut buf,
                &MessageDelta::ThinkingDelta {
                    content_index: 0,
                    delta: "hmm".to_string()
                },
            ),
            Some("⟦thinking: hmm⟧".to_string())
        );
        assert_eq!(
            apply_delta(
                &mut buf,
                &MessageDelta::TextDelta {
                    content_index: 0,
                    delta: "world".to_string()
                },
            ),
            Some("world".to_string())
        );
        assert_eq!(
            apply_delta(&mut buf, &MessageDelta::TextEnd { content_index: 0 }),
            None
        );
        // Thinking never enters the message text.
        assert_eq!(buf, "hello world");
    }

    #[test]
    fn event_lines_cover_terminal_and_tool_facts_only() {
        assert_eq!(
            render_event_line(&RpcEvent::AgentSettled),
            Some("agent settled".to_string())
        );
        assert_eq!(
            render_event_line(&RpcEvent::TurnEnd),
            Some("── turn end".to_string())
        );
        assert_eq!(
            render_event_line(&RpcEvent::AgentEnd { will_retry: true }),
            Some("agent end · will retry".to_string())
        );
        assert_eq!(
            render_event_line(&RpcEvent::ToolExecutionStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "bash".to_string(),
            }),
            Some("tool: bash (tc-1)".to_string())
        );
        assert_eq!(
            render_event_line(&RpcEvent::ToolExecutionEnd {
                tool_call_id: "tc-1".to_string(),
                tool_name: "bash".to_string(),
                is_error: true,
            }),
            Some("tool failed: bash".to_string())
        );
        assert_eq!(
            render_event_line(&RpcEvent::BashExecutionUpdate {
                command_id: None,
                delta: "  chunk\n".to_string(),
            }),
            Some("$ chunk".to_string())
        );
        // Dialogs and structural/message events never render as tail lines.
        assert_eq!(
            render_event_line(&RpcEvent::ExtensionUiRequest(select_req(vec![]))),
            None
        );
        assert_eq!(
            render_event_line(&RpcEvent::MessageUpdate(MessageDelta::TextStart {
                content_index: 0,
            })),
            None
        );
        assert_eq!(render_event_line(&RpcEvent::QueueUpdate), None);
    }

    // ---- status line ----

    #[test]
    fn status_line_renders_agent_turns_ctx_and_duration() {
        assert_eq!(
            format_status_line("3", Some("7"), 4, 40, Some(61.5), 90_000),
            "row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s"
        );
        // Unknown context right after compaction renders as `?`; no agent as `—`.
        assert_eq!(
            format_status_line("3", None, 0, 40, None, 250),
            "row 3 · agent — · turns 0/40 · ctx ? · 250ms"
        );
    }

    #[test]
    fn duration_formatting_covers_ms_s_and_minutes() {
        assert_eq!(format_duration(412), "412ms");
        assert_eq!(format_duration(9000), "9s");
        assert_eq!(format_duration(65_000), "1m05s");
        assert_eq!(format_duration(3_600_000), "60m00s");
    }

    // ---- dialogs ----

    #[test]
    fn select_dialog_lists_options_and_maps_numbers() {
        let req = select_req(vec!["read file".to_string(), "abort".to_string()]);
        assert_eq!(dialog_prompt_label(&req), "select>");
        assert_eq!(
            reply_from_input(&req, "2"),
            Some(UiReply::Value("abort".to_string()))
        );
        assert_eq!(reply_from_input(&req, "c"), Some(UiReply::Cancelled));
        assert_eq!(reply_from_input(&req, "3"), None);
        assert_eq!(reply_from_input(&req, ""), None);
    }

    #[test]
    fn confirm_dialog_maps_yes_no_and_cancel() {
        let req = confirm_req();
        assert_eq!(reply_from_input(&req, "y"), Some(UiReply::Confirmed(true)));
        assert_eq!(
            reply_from_input(&req, "NO"),
            Some(UiReply::Confirmed(false))
        );
        assert_eq!(reply_from_input(&req, "cancel"), Some(UiReply::Cancelled));
        assert_eq!(reply_from_input(&req, "maybe"), None);
    }

    #[test]
    fn input_dialog_takes_raw_text_and_cancel() {
        let req = input_req();
        assert_eq!(
            reply_from_input(&req, "abc123"),
            Some(UiReply::Value("abc123".to_string()))
        );
        assert_eq!(reply_from_input(&req, "c"), Some(UiReply::Cancelled));
        assert_eq!(reply_from_input(&req, "   "), None);
    }

    #[test]
    fn dialog_lines_include_message_and_numbered_options() {
        let req = select_req(vec!["a".to_string(), "b".to_string()]);
        let lines = dialog_lines(&req);
        assert_eq!(lines[0], "── pick ──");
        assert!(lines.contains(&"choose one".to_string()));
        assert!(lines.contains(&"  1. a".to_string()));
        assert!(lines.contains(&"  2. b".to_string()));
    }

    #[test]
    fn confirm_dialog_heading_falls_back_to_method_label() {
        let lines = dialog_lines(&confirm_req());
        assert_eq!(lines[0], "── confirm ──");
        assert!(lines.contains(&"  (y) yes / (n) no / (c) cancel".to_string()));
    }

    // ---- ask ----

    #[test]
    fn ask_lines_render_question_and_answer_prompt() {
        let lines = ask_lines("  Which tag?  ");
        assert_eq!(lines[0], "── worker question ──");
        assert_eq!(lines[1], "Which tag?");
        assert_eq!(lines[2], "answer>");
    }
}
