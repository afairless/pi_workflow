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
use crate::theme::{Color, Palette};
use crate::worker::PendingTool;

/// The semantic kind of one trace line — the output stage styles by kind
/// instead of pattern-matching text (plan step 3; success/error variants
/// and the SGR mapping finalized in step 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Assistant thinking deltas (`⟦thinking: …⟧`).
    Thinking,
    /// Assistant message text.
    Text,
    /// Tool start/pending rows (`tool: …`).
    Tool,
    /// A successful tool completion (`tool done: …`).
    ToolSuccess,
    /// A failed tool (`tool failed: …`).
    ToolError,
    /// Bash output chunks (`$ …`).
    Bash,
    /// Turn separators (`── turn start/end`).
    Turn,
    /// Agent/banner/settle lines.
    Banner,
}

/// One tagged trace line: the ring element type (kind + unstyled text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiLine {
    pub kind: LineKind,
    pub text: String,
}

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

/// The two stream deltas the open flowing line can hold (plan Q&A 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// A `TextDelta` chunk (the answer's text).
    Text,
    /// A `ThinkingDelta` chunk (the gray reasoning block).
    Thinking,
}

/// Classify one `message_update` delta into the TUI stream pair `(kind,
/// raw text)`: `Some((kind, unwrapped_chunk))` for text/thinking deltas
/// (without the `⟦thinking: …⟧` wrapper), `None` for structural and
/// tool-call deltas. This is the SINGLE delta classifier — [`apply_delta`]
/// delegates to it so the line-mode and TUI routings cannot drift (plan
/// review F6).
pub fn stream_part(delta: &MessageDelta) -> Option<(StreamKind, String)> {
    match delta {
        MessageDelta::TextDelta { delta, .. } => Some((StreamKind::Text, delta.clone())),
        MessageDelta::ThinkingDelta { delta, .. } => Some((StreamKind::Thinking, delta.clone())),
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

/// Apply one `message_update` delta to the worker's live text buffer.
///
/// Text chunks are pushed into `buffer` and returned for the live tail;
/// thinking chunks are rendered (prefixed) but never enter the message
/// text. Structural deltas return `None`. Returned lines are tagged with
/// their [`LineKind`] so the output stage styles by kind. Classification
/// delegates to [`stream_part`].
pub fn apply_delta(buffer: &mut String, delta: &MessageDelta) -> Option<TuiLine> {
    match stream_part(delta) {
        Some((StreamKind::Text, text)) => {
            buffer.push_str(text.as_str());
            Some(TuiLine {
                kind: LineKind::Text,
                text,
            })
        }
        Some((StreamKind::Thinking, text)) => Some(TuiLine {
            kind: LineKind::Thinking,
            text: thinking_chunk(text.as_str()),
        }),
        None => None,
    }
}

/// One rendered trace line for terminal-facing events, tagged with its
/// [`LineKind`] so the output stage styles by kind.
///
/// Returns `None` for events the tail does not surface directly: message
/// deltas (rendered via [`apply_delta`]), structural message events,
/// dialogs (rendered as interactive prompts), and high-frequency updates
/// (tool-call deltas, queue churn).
pub fn render_event_line(event: &RpcEvent) -> Option<TuiLine> {
    match event {
        RpcEvent::AgentStart => Some(TuiLine {
            kind: LineKind::Banner,
            text: "agent started".to_string(),
        }),
        RpcEvent::AgentEnd { will_retry } => {
            let text = if *will_retry {
                "agent end · will retry".to_string()
            } else {
                "agent end".to_string()
            };
            Some(TuiLine {
                kind: LineKind::Banner,
                text,
            })
        }
        RpcEvent::AgentSettled => Some(TuiLine {
            kind: LineKind::Banner,
            text: "agent settled".to_string(),
        }),
        RpcEvent::TurnStart => Some(TuiLine {
            kind: LineKind::Turn,
            text: "── turn start".to_string(),
        }),
        RpcEvent::TurnEnd => Some(TuiLine {
            kind: LineKind::Turn,
            text: "── turn end".to_string(),
        }),
        RpcEvent::MessageStart | RpcEvent::MessageEnd => None,
        RpcEvent::MessageUpdate(_) => None,
        RpcEvent::BashExecutionUpdate { delta, .. } => {
            let chunk = delta.trim();
            if chunk.is_empty() {
                None
            } else {
                Some(TuiLine {
                    kind: LineKind::Bash,
                    text: format!("$ {chunk}"),
                })
            }
        }
        RpcEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            ..
        } => Some(TuiLine {
            kind: LineKind::Tool,
            text: format!("tool: {tool_name} ({tool_call_id})"),
        }),
        RpcEvent::ToolExecutionUpdate { .. } => None,
        RpcEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            ..
        } => {
            let text = if *is_error {
                format!("tool failed: {tool_name}")
            } else {
                format!("tool done: {tool_name}")
            };
            // The text stays byte-identical for line mode; the success /
            // error distinction rides the kind so the TUI styles it.
            let kind = if *is_error {
                LineKind::ToolError
            } else {
                LineKind::ToolSuccess
            };
            Some(TuiLine { kind, text })
        }
        RpcEvent::QueueUpdate => None,
        RpcEvent::CompactionStart => Some(TuiLine {
            kind: LineKind::Banner,
            text: "context compaction — a `restart` may help".to_string(),
        }),
        RpcEvent::CompactionEnd => Some(TuiLine {
            kind: LineKind::Banner,
            text: "compaction done".to_string(),
        }),
        RpcEvent::AutoRetryStart | RpcEvent::AutoRetryEnd => None,
        RpcEvent::ExtensionUiRequest(_) => None,
        RpcEvent::Unknown { .. } => None,
    }
}

// ---------------- kind → style mapping (plan step 7) ----------------

/// The palette style for one line kind: the foreground color plus an
/// optional background fill. This is the token→style map the TUI applies
/// instead of pattern-matching text — the same mapping drives the
/// [`Stylize`](crate::theme::Stylize) SGR escapes the renderer emits
/// (success/error tool rows carry their outcome fills per the locked
/// gruvbox tokens).
pub fn style_for_kind(palette: &Palette, kind: LineKind) -> (Color, Option<Color>) {
    match kind {
        LineKind::Thinking => (palette.thinking_text, None),
        LineKind::Text => (palette.text, None),
        LineKind::Tool => (palette.tool_title, Some(palette.tool_pending_bg)),
        LineKind::ToolSuccess => (palette.success, Some(palette.tool_success_bg)),
        LineKind::ToolError => (palette.error, Some(palette.tool_error_bg)),
        LineKind::Bash => (palette.bash_mode, None),
        LineKind::Turn => (palette.border_muted, None),
        LineKind::Banner => (palette.muted, None),
    }
}

/// The success/error glyph prepended to tool-completion rows in the TUI
/// (line mode's bytes stay glyph-free). Empty for every other kind.
pub fn kind_glyph(kind: LineKind) -> String {
    match kind {
        LineKind::ToolSuccess => "✓ ".to_string(),
        LineKind::ToolError => "✗ ".to_string(),
        _ => String::new(),
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

// ---------------- TUI header / footer formatters (plan step 3) ----------------

/// Two lowercase decimal digits for a byte (`7` → `"07"`).
fn pad2(n: u64) -> String {
    if n < 10 {
        format!("0{n}")
    } else {
        format!("{n}")
    }
}

/// Four lowercase decimal digits for a byte (`7` → `"0007"`, `451` → `"0451"`).
fn pad4(n: u64) -> String {
    if n < 10 {
        format!("000{n}")
    } else if n < 100 {
        format!("00{n}")
    } else if n < 1000 {
        format!("0{n}")
    } else {
        format!("{n}")
    }
}

/// Format a provider-reported cost à la Pi: `$0.0451` (four decimals under
/// a dollar), `$1.23` at/over a dollar, `0.45¢` under a cent, `—` when
/// absent. Deterministic integer math (micro-dollars) so the exact digits
/// are unit-testable.
pub fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) if c.is_finite() && c >= 0.0 => {
            let micro = (c * 1_000_000.0).round() as u64;
            if c < 0.01 {
                // Cents, two decimals.
                format!("{}.{}¢", micro / 10_000, pad2((micro % 10_000) / 100))
            } else if c < 1.0 {
                // Dollars, four decimals.
                format!("${}.{}", micro / 1_000_000, pad4((micro % 1_000_000) / 100))
            } else {
                // Dollars, two decimals.
                format!(
                    "${}.{}",
                    micro / 1_000_000,
                    pad2((micro % 1_000_000) / 10_000)
                )
            }
        }
        _ => "—".to_string(),
    }
}

/// Abbreviate a token count for the footer: `59323` → `59.3k`, `200000` →
/// `200k`, while sub-thousand counts stay verbatim (`999`).
pub fn format_tokens(n: u64) -> String {
    if n < 1000 {
        format!("{n}")
    } else {
        let whole = n / 1000;
        let tenths = (n % 1000) / 100;
        if tenths == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{tenths}k")
        }
    }
}

/// Truncate `text` to at most `width` characters, replacing the dropped
/// tail with `…` (terminal-style). Shorter text is returned unchanged.
/// `width == 0` degrades to just the ellipsis.
pub fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let cut = width.max(1) - 1;
    let mut out = String::new();
    for (seen, c) in text.chars().enumerate() {
        if seen >= cut {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// The header's title line: `pi-plan · step {row}/{total} · {unit}`,
/// truncated with `…` at the header width. `row`/`total` track the FULL
/// TODO.md table, so `--row n` / `step n` runs still show where in the
/// whole plan they are (`step 5/10` for row 5 of 10).
pub fn format_header_line(row: u64, total: u64, unit: &str, width: usize) -> String {
    let base = format!("pi-plan · step {row}/{total} · {unit}");
    truncate_with_ellipsis(base.as_str(), width)
}

/// The live stats backing the persistent footer row (plan step 3): cost,
/// context usage, turns, elapsed time, and the row/agent ids. A borrowed
/// view assembled from the worker snapshot + plan context each frame by
/// the supervise loop, then handed to [`format_footer_line`].
pub struct FooterStats<'a> {
    pub row_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub turns: u32,
    pub max_turns: u32,
    pub context_percent: Option<f64>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost: Option<f64>,
    pub elapsed_ms: u64,
}

/// The footer's live-stats line: cost · context (pct + tokens/window) ·
/// turns · elapsed · row/agent. `context_percent`/`context_tokens`/
/// `context_window` are `None` right after compaction (unknown → `?`),
/// mirroring the snapshot's contract.
pub fn format_footer_line(stats: &FooterStats<'_>) -> String {
    let ctx = match stats.context_percent {
        Some(p) => {
            let pct = p as u64;
            format!("{pct}%")
        }
        None => "?".to_string(),
    };
    let tokens = stats
        .context_tokens
        .map(format_tokens)
        .unwrap_or("?".to_string());
    let window = stats
        .context_window
        .map(format_tokens)
        .unwrap_or("?".to_string());
    let agent = stats.agent_id.unwrap_or("—");
    let turns = stats.turns;
    let max_turns = stats.max_turns;
    let row_id = stats.row_id;
    format!(
        "{} · ctx {ctx} ({tokens}/{window}) · turns {turns}/{max_turns} · {} · row {row_id}/agent {agent}",
        format_cost(stats.cost),
        format_duration(stats.elapsed_ms)
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

/// Bound for a non-bash tool-args JSON preview: `edit`/`write` and
/// friends show what they touch without flooding the dialog (the modal
/// box wraps rows anyway; this caps the line-mode stdout row).
const TOOL_ARGS_PREVIEW_CAP: usize = 96;

/// Compact single-line JSON preview of a tool's args, bounded.
fn preview_json(args: &serde_json::Value) -> String {
    match serde_json::to_string(args) {
        Ok(compact) => truncate_with_ellipsis(compact.as_str(), TOOL_ARGS_PREVIEW_CAP),
        Err(_) => "…".to_string(),
    }
}

/// The pending-tool context rows for a permission dialog: a `tool:
/// <name> (<call_id>)` fact plus, for bash, `$ <command>` from the
/// decoded args; any other tool gets a bounded compact JSON preview of
/// its args. Empty context (no pending call) renders nothing, so
/// third-party extension dialogs (ASK questions, input prompts) are
/// unchanged. `worker::PendingTool` lives outside ui.rs to avoid a
/// dependency cycle (worker.rs never imports ui.rs).
pub fn tool_context_lines(tool: Option<&PendingTool>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(pending) = tool {
        out.push(format!(
            "tool: {} ({})",
            pending.tool_name, pending.tool_call_id
        ));
        if let Some(args) = &pending.args {
            if pending.tool_name == "bash"
                && let Some(command) = args.get("command").and_then(serde_json::Value::as_str)
            {
                out.push(format!("$ {command}"));
            } else {
                out.push(format!("args: {}", preview_json(args)));
            }
        }
    }
    out
}

/// Split prompt text into display lines: one per `\n`, a trailing `\r`
/// stripped per line, embedded blank lines kept verbatim, and a
/// trailing run of empty lines dropped (a trailing `\n` must not tack
/// stray blank rows onto the dialog's options).
fn split_display_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        out.push(strip_trailing_cr(line));
    }
    while !out.is_empty() && out[out.len() - 1].is_empty() {
        out.pop();
    }
    out
}

/// Remove a single trailing `\r` from a line (CRLF); everything else is
/// kept verbatim so the aligned `label : value` facts stay byte-exact.
fn strip_trailing_cr(line: &str) -> String {
    let mut cs = line.chars().collect::<Vec<char>>();
    if !cs.is_empty() {
        let last_idx = cs.len() - 1;
        if cs[last_idx] == '\r' {
            cs.pop();
        }
    }
    let mut out = String::new();
    for c in cs.iter() {
        out.push(*c);
    }
    out
}

/// The display lines for a dialog title: one per `\n` line with a
/// trailing `\r` stripped (a CRLF title would otherwise shift the
/// aligned `label : value` facts by one invisible char). An empty or
/// newline-only title yields no lines — callers fall back to the
/// method label, matching the pre-split behavior.
pub fn dialog_title_lines(title: &str) -> Vec<String> {
    split_display_lines(title)
}

/// The message display lines for a dialog request: the `message` field
/// split like [`dialog_title_lines`], or no lines when absent/empty.
pub fn dialog_message_lines(req: &ExtensionUiRequest) -> Vec<String> {
    if let Some(message) = &req.message
        && !message.is_empty()
    {
        split_display_lines(message.as_str())
    } else {
        Vec::new()
    }
}

/// The dialog banner lines for an `extension_ui_request` (stdout):
/// a heading with the title's first line (or method), the remaining
/// title lines, the message lines, then the hints.
pub fn dialog_lines(req: &ExtensionUiRequest, tool: Option<&PendingTool>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut title_lines = dialog_title_lines(req.title.as_deref().unwrap_or(""));
    if title_lines.is_empty() {
        title_lines.push(method_label(&req.method));
    }
    out.push(format!("── {} ──", title_lines[0]));
    let mut i: usize = 1;
    while i < title_lines.len() {
        out.push(title_lines[i].clone());
        i += 1;
    }
    for line in dialog_message_lines(req) {
        out.push(line);
    }
    for line in tool_context_lines(tool) {
        out.push(line);
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

/// The idle footer text shown between rows (supervisor status): no
/// worker stats — `idle`, which row just terminated and with which
/// terminal kind, and which row is next (with its unit when known, e.g.
/// `idle · last: row 5 completed · next: row 6 — Unit tests`; during a
/// retry the label is the preceding kind, `… row 5 failed …`). Callers
/// truncate to the footer width.
pub fn format_idle_footer_line(
    last: Option<(u64, String)>,
    next_row: u64,
    next_unit: Option<&str>,
) -> String {
    let mut line = "idle".to_string();
    if let Some((row, label)) = &last {
        line.push_str(format!(" · last: row {} {}", row, label).as_str());
    }
    line.push_str(format!(" · next: row {next_row}").as_str());
    if let Some(unit) = next_unit {
        line.push_str(format!(" — {unit}").as_str());
    }
    line
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

/// One TUI modal content row: the row text plus whether this row is the
/// focused item (the `▸`-highlighted one a bare Enter submits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogRow {
    pub text: String,
    pub focused: bool,
}

/// The number of highlightable rows for a dialog (the rows ↑/↓ can move
/// across and a bare Enter can submit): `Select` has one row per option
/// plus `(c) cancel` (`N + 1`, so a zero-option select still has
/// cancel), `Confirm` has three (no/yes/cancel in TUI row order), and
/// `Input`/`Editor`/others have none.
pub fn dialog_item_count(req: &ExtensionUiRequest) -> usize {
    match req.method {
        UiMethod::Select => req.options.len() + 1,
        UiMethod::Confirm => 3,
        UiMethod::Input | UiMethod::Editor => 0,
        _ => 0,
    }
}

/// The reply a focused dialog row submits. `Select`: row `idx` is option
/// `idx` and row `options.len()` is cancel; `Confirm`: `no`/`yes`/
/// `cancel` by row (TUI order); anything else (itemless dialogs and
/// out-of-range indices) yields `None`.
pub fn item_reply(req: &ExtensionUiRequest, idx: usize) -> Option<UiReply> {
    match req.method {
        UiMethod::Select => {
            for (i, option) in req.options.iter().enumerate() {
                if idx == i {
                    return Some(UiReply::Value(option.clone()));
                }
            }
            if idx == req.options.len() {
                Some(UiReply::Cancelled)
            } else {
                None
            }
        }
        UiMethod::Confirm => match idx {
            0 => Some(UiReply::Confirmed(false)),
            1 => Some(UiReply::Confirmed(true)),
            2 => Some(UiReply::Cancelled),
            _ => None,
        },
        _ => None,
    }
}

/// The TUI modal's dialog content: heading + remaining title lines +
/// message rows (identical to [`dialog_lines`]), then the focusable rows
/// with the `focused` flag on the focused index. `Select` rows are
/// byte-identical to `dialog_lines`' (parity-tested over request
/// shapes); `Confirm` renders its own three-row `(n) no` / `(y) yes` /
/// `(c) cancel` order (line mode's single hint line is unchanged);
/// `Input`/`Editor` emit `dialog_lines`-identical rows, all unfocused.
pub fn modal_dialog_rows(
    req: &ExtensionUiRequest,
    focus: Option<usize>,
    tool: Option<&PendingTool>,
) -> Vec<DialogRow> {
    let mut out: Vec<DialogRow> = Vec::new();
    let mut title_lines = dialog_title_lines(req.title.as_deref().unwrap_or(""));
    if title_lines.is_empty() {
        title_lines.push(method_label(&req.method));
    }
    out.push(DialogRow {
        text: format!("── {} ──", title_lines[0]),
        focused: false,
    });
    let mut i: usize = 1;
    while i < title_lines.len() {
        out.push(DialogRow {
            text: title_lines[i].clone(),
            focused: false,
        });
        i += 1;
    }
    for line in dialog_message_lines(req) {
        out.push(DialogRow {
            text: line,
            focused: false,
        });
    }
    for line in tool_context_lines(tool) {
        out.push(DialogRow {
            text: line,
            focused: false,
        });
    }
    match req.method {
        UiMethod::Select => {
            for (i, option) in req.options.iter().enumerate() {
                let n = i + 1;
                out.push(DialogRow {
                    text: format!("  {n}. {option}"),
                    focused: focus == Some(i),
                });
            }
            out.push(DialogRow {
                text: "  (c) cancel".to_string(),
                focused: focus == Some(req.options.len()),
            });
        }
        UiMethod::Confirm => {
            out.push(DialogRow {
                text: "  (n) no".to_string(),
                focused: focus == Some(0),
            });
            out.push(DialogRow {
                text: "  (y) yes".to_string(),
                focused: focus == Some(1),
            });
            out.push(DialogRow {
                text: "  (c) cancel".to_string(),
                focused: focus == Some(2),
            });
        }
        UiMethod::Input | UiMethod::Editor => {
            if let Some(ph) = &req.placeholder
                && !ph.is_empty()
                && req.prefill.is_none()
            {
                out.push(DialogRow {
                    text: format!("  placeholder: {ph}"),
                    focused: false,
                });
            }
            if let Some(prefill) = &req.prefill
                && !prefill.is_empty()
            {
                out.push(DialogRow {
                    text: format!("  prefill: {prefill}"),
                    focused: false,
                });
            }
            out.push(DialogRow {
                text: "  (c) cancel".to_string(),
                focused: false,
            });
        }
        _ => {}
    }
    out
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
    use proptest::prelude::*;

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
            Some(TuiLine {
                kind: LineKind::Text,
                text: "hello ".to_string()
            })
        );
        assert_eq!(
            apply_delta(
                &mut buf,
                &MessageDelta::ThinkingDelta {
                    content_index: 0,
                    delta: "hmm".to_string()
                },
            ),
            Some(TuiLine {
                kind: LineKind::Thinking,
                text: "⟦thinking: hmm⟧".to_string(),
            })
        );
        assert_eq!(
            apply_delta(
                &mut buf,
                &MessageDelta::TextDelta {
                    content_index: 0,
                    delta: "world".to_string()
                },
            ),
            Some(TuiLine {
                kind: LineKind::Text,
                text: "world".to_string()
            })
        );
        assert_eq!(
            apply_delta(&mut buf, &MessageDelta::TextEnd { content_index: 0 }),
            None
        );
        // Thinking never enters the message text.
        assert_eq!(buf, "hello world");
    }

    #[test]
    fn stream_part_classifies_only_text_and_thinking_deltas() {
        let text = MessageDelta::TextDelta {
            content_index: 0,
            delta: "partial".to_string(),
        };
        let (kind, raw) = stream_part(&text).expect("text delta is a stream part");
        assert_eq!(kind, StreamKind::Text);
        assert_eq!(raw, "partial");

        let thinking = MessageDelta::ThinkingDelta {
            content_index: 0,
            delta: "reason".to_string(),
        };
        let (kind2, raw2) = stream_part(&thinking).expect("thinking delta is a stream part");
        assert_eq!(kind2, StreamKind::Thinking);
        assert_eq!(raw2, "reason");

        for delta in [
            MessageDelta::TextStart { content_index: 0 },
            MessageDelta::TextEnd { content_index: 0 },
            MessageDelta::ThinkingStart { content_index: 0 },
            MessageDelta::ThinkingEnd { content_index: 0 },
            MessageDelta::ToolCallStart {
                content_index: 0,
                id: "c1".to_string(),
                tool_name: "bash".to_string(),
            },
            MessageDelta::ToolCallDelta {
                content_index: 0,
                delta: "x".to_string(),
            },
            MessageDelta::ToolCallEnd { content_index: 0 },
            MessageDelta::Other {
                kind: "queue".to_string(),
                raw: serde_json::Value::Null,
            },
        ] {
            assert_eq!(stream_part(&delta), None);
        }
    }

    #[test]
    fn event_lines_cover_terminal_and_tool_facts_only() {
        assert_eq!(
            render_event_line(&RpcEvent::AgentSettled),
            Some(TuiLine {
                kind: LineKind::Banner,
                text: "agent settled".to_string(),
            })
        );
        assert_eq!(
            render_event_line(&RpcEvent::TurnEnd),
            Some(TuiLine {
                kind: LineKind::Turn,
                text: "── turn end".to_string()
            })
        );
        assert_eq!(
            render_event_line(&RpcEvent::AgentEnd { will_retry: true }),
            Some(TuiLine {
                kind: LineKind::Banner,
                text: "agent end · will retry".to_string(),
            })
        );
        assert_eq!(
            render_event_line(&RpcEvent::ToolExecutionStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "bash".to_string(),
                args: None,
            }),
            Some(TuiLine {
                kind: LineKind::Tool,
                text: "tool: bash (tc-1)".to_string(),
            })
        );
        assert_eq!(
            render_event_line(&RpcEvent::ToolExecutionEnd {
                tool_call_id: "tc-1".to_string(),
                tool_name: "bash".to_string(),
                is_error: true,
            }),
            Some(TuiLine {
                kind: LineKind::ToolError,
                text: "tool failed: bash".to_string(),
            })
        );
        assert_eq!(
            render_event_line(&RpcEvent::ToolExecutionEnd {
                tool_call_id: "tc-1".to_string(),
                tool_name: "write".to_string(),
                is_error: false,
            }),
            Some(TuiLine {
                kind: LineKind::ToolSuccess,
                text: "tool done: write".to_string(),
            })
        );
        assert_eq!(
            render_event_line(&RpcEvent::BashExecutionUpdate {
                command_id: None,
                delta: "  chunk\n".to_string(),
            }),
            Some(TuiLine {
                kind: LineKind::Bash,
                text: "$ chunk".to_string()
            })
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
        let lines = dialog_lines(&req, None);
        assert_eq!(lines[0], "── pick ──");
        assert!(lines.contains(&"choose one".to_string()));
        assert!(lines.contains(&"  1. a".to_string()));
        assert!(lines.contains(&"  2. b".to_string()));
    }

    #[test]
    fn confirm_dialog_heading_falls_back_to_method_label() {
        let lines = dialog_lines(&confirm_req(), None);
        assert_eq!(lines[0], "── confirm ──");
        assert!(lines.contains(&"  (y) yes / (n) no / (c) cancel".to_string()));
    }

    #[test]
    fn dialog_title_lines_split_lines_and_strip_crlf() {
        // Empty and newline-only titles have no display lines — callers
        // fall back to the method label.
        assert!(dialog_title_lines("").is_empty());
        assert!(dialog_title_lines("\n\n").is_empty());
        // A single line passes through untouched.
        assert_eq!(dialog_title_lines("pick").join("\n"), "pick".to_string());
        // Multi-line titles keep every line and the aligned facts.
        assert_eq!(
            dialog_title_lines("Permission Required\ntool : bash\ncommand : rm -f /tmp/x")
                .join("\n"),
            "Permission Required\ntool : bash\ncommand : rm -f /tmp/x".to_string()
        );
        // Embedded blank lines are kept verbatim.
        assert_eq!(
            dialog_title_lines("a\n\nb").join("\n"),
            "a\n\nb".to_string()
        );
        // CRLF lines lose their `\r`; a trailing newline adds no stray
        // blank row.
        assert_eq!(
            dialog_title_lines("a\r\nb\r\n").join("\n"),
            "a\nb".to_string()
        );
    }

    #[test]
    fn dialog_message_lines_follow_the_title_line_splitting() {
        let req = select_req(vec!["a".to_string()]);
        assert_eq!(
            dialog_message_lines(&req).join("\n"),
            "choose one".to_string()
        );
        let crlf = ExtensionUiRequest {
            id: "ui-m".to_string(),
            method: UiMethod::Select,
            title: Some("t".to_string()),
            message: Some("line one\r\nline two\r\n".to_string()),
            options: vec!["a".to_string()],
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        };
        assert_eq!(
            dialog_message_lines(&crlf).join("\n"),
            "line one\nline two".to_string()
        );
        let no_message = ExtensionUiRequest {
            id: "ui-n".to_string(),
            method: UiMethod::Select,
            title: Some("t".to_string()),
            message: None,
            options: vec!["a".to_string()],
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        };
        assert!(dialog_message_lines(&no_message).is_empty());
    }

    #[test]
    fn tool_context_lines_render_bash_non_bash_and_absent_args() {
        // No pending call → nothing (third-party dialogs unchanged).
        assert!(tool_context_lines(None).is_empty());
        // bash with a command string → the tool fact + `$ command`.
        let bash = PendingTool {
            tool_call_id: "call_9b7d".to_string(),
            tool_name: "bash".to_string(),
            args: Some(serde_json::json!({ "command": "mkdir -p x && rm -rf x" })),
        };
        assert_eq!(
            tool_context_lines(Some(&bash)).join("\n"),
            "tool: bash (call_9b7d)\n$ mkdir -p x && rm -rf x".to_string()
        );
        // A non-bash tool previews its args as bounded compact JSON.
        let edit = PendingTool {
            tool_call_id: "call_1a2b".to_string(),
            tool_name: "edit".to_string(),
            args: Some(serde_json::json!({ "file_path": "/repo/src/main.rs", "content": "..." })),
        };
        let edit_lines = tool_context_lines(Some(&edit));
        assert_eq!(edit_lines[0], "tool: edit (call_1a2b)");
        assert!(edit_lines[1].starts_with("args: {"), "compact JSON preview");
        assert!(edit_lines[1].contains("file_path"));
        // bash without a command string falls back to the same preview.
        let bare = PendingTool {
            tool_call_id: "call_3c".to_string(),
            tool_name: "bash".to_string(),
            args: Some(serde_json::json!({ "timeout": 30_000 })),
        };
        let bare_lines = tool_context_lines(Some(&bare));
        assert_eq!(bare_lines.len(), 2);
        assert!(bare_lines[1].starts_with("args: {"));
        assert!(bare_lines[1].contains("30000"));
        // Missing args → the tool fact only.
        let no_args = PendingTool {
            tool_call_id: "call_42".to_string(),
            tool_name: "bash".to_string(),
            args: None,
        };
        assert_eq!(
            tool_context_lines(Some(&no_args)).join("\n"),
            "tool: bash (call_42)".to_string()
        );
    }

    #[test]
    fn both_renderers_show_the_tool_context_between_message_and_options() {
        let req = select_req(vec!["read file".to_string(), "abort".to_string()]);
        let tool = PendingTool {
            tool_call_id: "call_ab12".to_string(),
            tool_name: "bash".to_string(),
            args: Some(serde_json::json!({ "command": "rm -f /tmp/plan-probe-testfile" })),
        };
        let lines = dialog_lines(&req, Some(&tool));
        let rows = modal_dialog_rows(&req, Some(0), Some(&tool));
        assert_eq!(rows.len(), lines.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.text, lines[i], "row {i} is shared between modes");
        }
        // The context rows sit between the message and the options: the
        // tool fact, then the `$ command` line, then the option list.
        assert_eq!(lines[2], "tool: bash (call_ab12)");
        assert_eq!(lines[3], "$ rm -f /tmp/plan-probe-testfile");
        assert!(lines[4].contains("1. read file"));
        assert!(!rows[2].focused, "context rows are never focusable");
        assert!(!rows[3].focused);
        // Empty context renders nothing over the same request.
        let plain = dialog_lines(&req, None);
        assert_eq!(plain.len(), lines.len() - 2);
        assert!(!plain.iter().any(|l| l.starts_with("tool: bash")));
    }

    #[test]
    fn dialog_lines_and_modal_rows_share_multiline_title_shapes() {
        let mut req = select_req(vec!["a".to_string(), "b".to_string()]);
        req.title = Some(
            "Permission Required\ntool : bash\ncommand : mkdir -p delete-me-dir\r".to_string(),
        );
        let rows = modal_dialog_rows(&req, Some(0), None);
        let lines = dialog_lines(&req, None);
        assert_eq!(rows.len(), lines.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.text, lines[i], "row {i} is shared between modes");
        }
        // The heading takes the FIRST title line only; the rest render
        // as their own unfocused content rows (option numbering and the
        // focus model are untouched by extra content rows).
        assert_eq!(lines[0], "── Permission Required ──");
        assert_eq!(lines[1], "tool : bash");
        assert_eq!(lines[2], "command : mkdir -p delete-me-dir");
        assert_eq!(lines[3], "choose one");
        assert_eq!(dialog_item_count(&req), 3, "two options + cancel");
        assert!(rows[4].focused, "option 1 stays the focused row");
        assert!(rows[4].text.contains("1. a"), "options keep numbering");
    }

    // ---- modal focusable rows (plan step 8) ----

    #[test]
    fn dialog_item_count_covers_select_confirm_and_itemless_dialogs() {
        let req = select_req(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(dialog_item_count(&req), 3, "two options + cancel");
        assert_eq!(
            dialog_item_count(&select_req(vec![])),
            1,
            "zero options leaves cancel"
        );
        assert_eq!(dialog_item_count(&confirm_req()), 3, "no / yes / cancel");
        assert_eq!(dialog_item_count(&input_req()), 0);
    }

    #[test]
    fn item_reply_maps_select_indices_including_cancel() {
        let req = select_req(vec!["read file".to_string(), "abort".to_string()]);
        assert_eq!(
            item_reply(&req, 0),
            Some(UiReply::Value("read file".to_string()))
        );
        assert_eq!(
            item_reply(&req, 1),
            Some(UiReply::Value("abort".to_string()))
        );
        assert_eq!(item_reply(&req, 2), Some(UiReply::Cancelled));
        assert_eq!(item_reply(&req, 3), None, "out of range");
    }

    #[test]
    fn item_reply_maps_confirm_in_tui_row_order_no_yes_cancel() {
        let req = confirm_req();
        assert_eq!(item_reply(&req, 0), Some(UiReply::Confirmed(false)));
        assert_eq!(item_reply(&req, 1), Some(UiReply::Confirmed(true)));
        assert_eq!(item_reply(&req, 2), Some(UiReply::Cancelled));
        assert_eq!(item_reply(&req, 3), None, "out of range");
        assert_eq!(
            item_reply(&input_req(), 0),
            None,
            "input has no focusable rows"
        );
    }

    #[test]
    fn modal_dialog_rows_select_matches_dialog_lines_with_focus_flags() {
        let req = select_req(vec!["a".to_string(), "b".to_string()]);
        let rows = modal_dialog_rows(&req, Some(1), None);
        assert_eq!(rows.len(), 5, "heading + message + 2 options + cancel");
        let lines = dialog_lines(&req, None);
        assert_eq!(rows.len(), lines.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.text, lines[i], "row {i} is byte-identical to line mode");
        }
        // The focused row is option 2 (item index 1); siblings are not.
        assert!(rows[3].focused, "focused row is option 2");
        assert!(!rows[2].focused);
        assert!(!rows[4].focused, "cancel is not focused");
    }

    #[test]
    fn modal_dialog_rows_confirm_renders_three_rows_in_no_yes_cancel_order() {
        let rows = modal_dialog_rows(&confirm_req(), Some(0), None);
        assert_eq!(rows[2].text, "  (n) no");
        assert_eq!(rows[3].text, "  (y) yes");
        assert_eq!(rows[4].text, "  (c) cancel");
        assert!(rows[2].focused, "confirm pre-highlights `no`");
        assert!(!rows[3].focused);
        assert!(!rows[4].focused);
    }

    #[test]
    fn modal_dialog_rows_input_keeps_dialog_lines_rows_all_unfocused() {
        let rows = modal_dialog_rows(&input_req(), Some(0), None);
        let lines = dialog_lines(&input_req(), None);
        assert_eq!(rows.len(), lines.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.text, lines[i]);
            assert!(!row.focused, "input rows are never focused");
        }
    }

    // ---- ask ----

    #[test]
    fn ask_lines_render_question_and_answer_prompt() {
        let lines = ask_lines("  Which tag?  ");
        assert_eq!(lines[0], "── worker question ──");
        assert_eq!(lines[1], "Which tag?");
        assert_eq!(lines[2], "answer>");
    }

    // ---- kind → style mapping (plan step 7) ----

    use crate::theme::{Stylize, default_palette};

    #[test]
    fn style_for_kind_maps_every_kind_to_palette_tokens() {
        let p = default_palette();
        let (fg, bg) = style_for_kind(&p, LineKind::Thinking);
        assert_eq!(fg, p.thinking_text);
        assert_eq!(bg, None);
        let (fg, bg) = style_for_kind(&p, LineKind::Text);
        assert_eq!(fg, p.text);
        assert_eq!(bg, None);
        let (fg, bg) = style_for_kind(&p, LineKind::Tool);
        assert_eq!(fg, p.tool_title);
        assert_eq!(bg, Some(p.tool_pending_bg));
        let (fg, bg) = style_for_kind(&p, LineKind::ToolSuccess);
        assert_eq!(fg, p.success);
        assert_eq!(bg, Some(p.tool_success_bg));
        let (fg, bg) = style_for_kind(&p, LineKind::ToolError);
        assert_eq!(fg, p.error);
        assert_eq!(bg, Some(p.tool_error_bg));
        let (fg, _) = style_for_kind(&p, LineKind::Bash);
        assert_eq!(fg, p.bash_mode);
        let (fg, _) = style_for_kind(&p, LineKind::Turn);
        assert_eq!(fg, p.border_muted);
        let (fg, _) = style_for_kind(&p, LineKind::Banner);
        assert_eq!(fg, p.muted);
    }

    #[test]
    fn tool_outcome_styles_render_to_token_sgr_escapes() {
        // The token→SGR end-to-end string: kind → palette → escape.
        let p = default_palette();
        let (fg, bg) = style_for_kind(&p, LineKind::ToolSuccess);
        assert_eq!(Stylize::fg(&fg), "\u{1b}[38;2;142;192;124m");
        assert_eq!(
            Stylize::bg(&bg.expect("success rows carry a fill")),
            "\u{1b}[48;2;47;48;47m"
        );
        let (failed_fg, failed_bg) = style_for_kind(&p, LineKind::ToolError);
        assert_eq!(Stylize::fg(&failed_fg), "\u{1b}[38;2;251;73;52m");
        assert_eq!(
            Stylize::bg(&failed_bg.expect("error rows carry a fill")),
            "\u{1b}[48;2;56;47;46m"
        );
        // Thinking is gray (thinkingText) per the locked palette; the
        // default text color renders no escape at all.
        let (thinking_fg, _) = style_for_kind(&p, LineKind::Thinking);
        assert_eq!(Stylize::fg(&thinking_fg), "\u{1b}[38;2;146;131;116m");
        let (text_fg, _) = style_for_kind(&p, LineKind::Text);
        assert_eq!(Stylize::fg(&text_fg), "");
    }

    #[test]
    fn kind_glyphs_mark_tool_outcomes_only() {
        assert_eq!(kind_glyph(LineKind::ToolSuccess), "✓ ");
        assert_eq!(kind_glyph(LineKind::ToolError), "✗ ");
        assert_eq!(kind_glyph(LineKind::Tool), "");
        assert_eq!(kind_glyph(LineKind::Bash), "");
        assert_eq!(kind_glyph(LineKind::Banner), "");
    }

    // ---- TUI formatters (plan step 3) ----

    #[test]
    fn header_line_shows_step_position_and_truncates_at_width() {
        assert_eq!(
            format_header_line(3, 12, "Crate skeleton", 60),
            "pi-plan · step 3/12 · Crate skeleton"
        );
        // The `--row n` corner: row 5 of a 10-row table still says 5/10.
        assert_eq!(
            format_header_line(5, 10, "Parser", 60),
            "pi-plan · step 5/10 · Parser"
        );
        // Truncated with an ellipsis at the header width.
        let cut = format_header_line(3, 12, "Crate skeleton", 14);
        assert_eq!(cut.chars().count(), 14);
        assert!(cut.ends_with("…"));
    }

    #[test]
    fn truncate_with_ellipsis_keeps_short_text_and_marks_the_cut() {
        assert_eq!(truncate_with_ellipsis("short", 20), "short");
        assert_eq!(truncate_with_ellipsis("abcdef", 6), "abcdef");
        assert_eq!(truncate_with_ellipsis("abcdef", 5), "abcd…");
        assert_eq!(truncate_with_ellipsis("abcdef", 1), "…");
        assert_eq!(
            truncate_with_ellipsis("abcdef", 0),
            "…",
            "width 0 degrades to the ellipsis"
        );
    }

    #[test]
    fn cost_formatting_matches_the_locked_order_of_magnitude_rule() {
        assert_eq!(
            format_cost(Some(0.0451)),
            "$0.0451",
            "sub-dollar, four decimals"
        );
        assert_eq!(format_cost(Some(0.451)), "$0.4510");
        assert_eq!(
            format_cost(Some(0.0)),
            "0.00¢",
            "zero is present, not absent"
        );
        assert_eq!(
            format_cost(Some(1.5)),
            "$1.50",
            "at/over a dollar, two decimals"
        );
        assert_eq!(format_cost(Some(12.0)), "$12.00");
        assert_eq!(format_cost(Some(0.00451)), "0.45¢", "under a cent, cents");
        assert_eq!(format_cost(Some(0.00999)), "0.99¢");
        assert_eq!(format_cost(None), "—");
    }

    #[test]
    fn token_counts_abbreviate_with_k_only_when_round() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(59_300), "59.3k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(105_000), "105k");
    }

    #[test]
    fn footer_line_renders_stats_and_question_marks_for_unknowns() {
        assert_eq!(
            format_footer_line(&FooterStats {
                row_id: "3",
                agent_id: Some("7"),
                turns: 4,
                max_turns: 40,
                context_percent: Some(61.5),
                context_tokens: Some(59_300),
                context_window: Some(200_000),
                cost: Some(0.0451),
                elapsed_ms: 90_000,
            }),
            "$0.0451 · ctx 61% (59.3k/200k) · turns 4/40 · 1m30s · row 3/agent 7"
        );
        // Unknown context right after compaction renders `?`, missing cost
        // `—`, and a missing agent `—`.
        assert_eq!(
            format_footer_line(&FooterStats {
                row_id: "3",
                agent_id: None,
                turns: 0,
                max_turns: 40,
                context_percent: None,
                context_tokens: None,
                context_window: None,
                cost: None,
                elapsed_ms: 250,
            }),
            "— · ctx ? (?/?) · turns 0/40 · 250ms · row 3/agent —"
        );
    }

    #[test]
    fn idle_footer_line_renders_the_supervisor_status() {
        // No terminal yet: just the idle marker and the next row.
        assert_eq!(format_idle_footer_line(None, 1, None), "idle · next: row 1");
        // A completed row advances `next` past it; the caller resolves
        // the unit from the plan map.
        assert_eq!(
            format_idle_footer_line(Some((5, "completed".to_string())), 6, Some("Unit tests")),
            "idle · last: row 5 completed · next: row 6 — Unit tests"
        );
        // A failed row keeps `next` on the same row (the retry) and
        // shows the preceding terminal kind truthfully.
        assert_eq!(
            format_idle_footer_line(Some((5, "failed".to_string())), 5, None),
            "idle · last: row 5 failed · next: row 5"
        );
    }

    // ---- property-based ----

    proptest! {
        /// The TUI select rows are byte-identical to line mode's dialog
        /// lines for every option list, regardless of the focus.
        #[test]
        fn modal_dialog_rows_select_matches_dialog_lines(count in "[0-2]") {
            let n = count.parse::<usize>().unwrap_or(0);
            let mut options: Vec<String> = Vec::new();
            let mut i: usize = 0;
            while i < n {
                options.push(format!("opt {i}"));
                i += 1;
            }
            let req = select_req(options);
            let rows = modal_dialog_rows(&req, Some(0), None);
            let lines = dialog_lines(&req, None);
            prop_assert_eq!(rows.len(), lines.len());
            for (i, row) in rows.iter().enumerate() {
                prop_assert_eq!(&row.text, &lines[i]);
            }
        }

        /// The two renderers stay byte-identical over arbitrary title
        /// and message shapes too (both split through the same line
        /// helpers, so multi-line and CRLF prompts stay aligned).
        #[test]
        fn modal_dialog_rows_matches_dialog_lines_over_title_shapes(
            title in "[a-z :]{0,40}",
            message in "[a-z :]{0,40}",
        ) {
            let req = ExtensionUiRequest {
                id: "ui-p".to_string(),
                method: UiMethod::Select,
                title: Some(title),
                message: Some(message),
                options: vec!["read file".to_string(), "abort".to_string()],
                placeholder: None,
                prefill: None,
                timeout_ms: None,
            };
            let rows = modal_dialog_rows(&req, Some(0), None);
            let lines = dialog_lines(&req, None);
            prop_assert_eq!(rows.len(), lines.len());
            for (i, row) in rows.iter().enumerate() {
                prop_assert_eq!(&row.text, &lines[i]);
            }
        }

        /// The same byte-parity holds over the pending-tool context
        /// shapes: both renderers emit the identical `tool:`/`$ …` (or
        /// JSON-preview) rows over the same context argument.
        #[test]
        fn modal_dialog_rows_matches_dialog_lines_over_context_shapes(
            title in "[a-z :]{0,20}",
            kind in "[0-2]",
        ) {
            let tool: Option<PendingTool> = if kind == "0" {
                None
            } else if kind == "1" {
                Some(PendingTool {
                    tool_call_id: "c1".to_string(),
                    tool_name: "bash".to_string(),
                    args: Some(serde_json::json!({ "command": "mkdir -p delete-me-dir" })),
                })
            } else {
                Some(PendingTool {
                    tool_call_id: "c2".to_string(),
                    tool_name: "edit".to_string(),
                    args: Some(serde_json::json!({ "file": "src/main.rs" })),
                })
            };
            let req = ExtensionUiRequest {
                id: "ui-q".to_string(),
                method: UiMethod::Select,
                title: Some(title),
                message: Some("allow this?".to_string()),
                options: vec!["yes".to_string(), "no".to_string()],
                placeholder: None,
                prefill: None,
                timeout_ms: None,
            };
            let rows = modal_dialog_rows(&req, Some(0), tool.as_ref());
            let lines = dialog_lines(&req, tool.as_ref());
            prop_assert_eq!(rows.len(), lines.len());
            for (i, row) in rows.iter().enumerate() {
                prop_assert_eq!(&row.text, &lines[i]);
            }
        }
    }
}
