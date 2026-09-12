//! Full-screen TUI for the supervisor display (plan steps 3–4).
//!
//! Repo convention kept: **layout is a pure transform; I/O is thin**. The
//! frame builders take a palette / state / width / height and return
//! [`StyledLine`]s; the terminal backend section ([`Terminal`],
//! [`watch_resizes`]) is the only place that touches the terminal or its
//! syscalls.
//!
//! Screen anatomy (locked 2026-09-12):
//!
//! ```text
//! ┌┤ pi-plan · step 3/12 · Crate skeleton ├───────────  ← header (2 rows)
//! │ source: docs/research/interface-design.md
//! │ ⟦thinking: so the compiler⟧                         │
//! │     ...trace viewport (wrapped to width)...         │ ← trace (scrolls
//! │                                                     │    inside its rows)
//! │ $0.0451 · ctx 61% (59.3k/200k) · turns 4/40 · 1m30s │ ← footer (persistent)
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! The trace scrolls exclusively inside rows `3..H-2` via a scroll region
//! (wired in step 5), so the header/footer never occlude streaming text.
//! [`dialog_box`] overlays a centered modal; [`trace_lines`] returns the
//! bare (unframed) viewport so the render loop owns final placement.

use std::os::unix::io::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
use nix::sys::signalfd::SignalFd;
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::read;
use tokio::sync::broadcast;

use crate::rpc::{ExtensionUiRequest, UiReply};
use crate::theme::{Color, Palette};
use crate::ui::{
    FooterStats, LineCommand, LineKind, TuiLine, dialog_lines, dialog_prompt_label,
    format_footer_line, format_header_line, format_status_line, line_command, reply_from_input,
    truncate_with_ellipsis,
};
use crate::worker::WorkerSnapshot;
/// One fully styled frame line: text plus the palette colors to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledLine {
    pub text: String,
    pub fg: Color,
    pub bg: Option<Color>,
}

/// `n` repetitions of one character (`\u{2500}` box rules, spaces, …).
fn fill_with(ch: char, n: usize) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < n {
        out.push(ch);
        i += 1;
    }
    out
}

/// Pad `text` on the right to exactly `width` characters (truncating with
/// `…` first when it is already too long).
fn pad_line_to(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n >= width {
        return truncate_with_ellipsis(text, width);
    }
    format!("{text}{}", fill_with(' ', width - n))
}

/// Right-pad an owned line to exactly `width` characters with spaces.
fn pad_right(line: String, width: usize) -> String {
    let n = line.chars().count();
    if n >= width {
        return truncate_with_ellipsis(line.as_str(), width);
    }
    format!("{line}{}", fill_with(' ', width - n))
}

// ---------------- header / footer / trace / dialog ----------------

/// The two header rows: the title bar (`┌┤ … ├──┐`) and the `source:`
/// context line (`│ …`). Both are exactly `width` characters.
pub fn header_lines(
    palette: &Palette,
    row: u64,
    total: u64,
    unit: &str,
    source: Option<&str>,
    width: usize,
) -> Vec<StyledLine> {
    // Box art: `┌┤ ` (3) + title + ` ├` (2) + `┐` (1).
    let fixed: usize = 6;
    let title_width = if width > fixed { width - fixed } else { 1 };
    let title = format_header_line(row, total, unit, title_width);
    let fill_cols = if width > fixed + title.chars().count() {
        width - fixed - title.chars().count()
    } else {
        0
    };
    let line1 = truncate_with_ellipsis(
        format!("┌┤ {title} ├{}{}", fill_with('─', fill_cols), "┐").as_str(),
        width,
    );
    let mut line2 = "│".to_string();
    if let Some(source) = source {
        line2 = format!("│ source: {source}");
    }
    vec![
        StyledLine {
            text: line1,
            fg: palette.border_accent,
            bg: None,
        },
        StyledLine {
            text: pad_line_to(line2.as_str(), width),
            fg: palette.muted,
            bg: None,
        },
    ]
}

/// The persistent footer row: the live-stats line plus the `stop / restart
/// / status` hints trailing right when they fit, padded to `width`.
pub fn footer_lines(palette: &Palette, stats: &FooterStats<'_>, width: usize) -> Vec<StyledLine> {
    let content = format_footer_line(stats);
    let hints = " stop / restart / status".to_string();
    let combined = if content.chars().count() + hints.chars().count() <= width {
        format!("{content}{hints}")
    } else {
        truncate_with_ellipsis(content.as_str(), width)
    };
    vec![StyledLine {
        text: pad_line_to(combined.as_str(), width),
        fg: palette.muted,
        bg: None,
    }]
}

/// The palette style for one line kind (fg + optional bg fill).
fn style_for_kind(palette: &Palette, kind: LineKind) -> (Color, Option<Color>) {
    match kind {
        LineKind::Thinking => (palette.thinking_text, None),
        LineKind::Text => (palette.text, None),
        LineKind::Tool => (palette.tool_title, Some(palette.tool_pending_bg)),
        LineKind::Bash => (palette.bash_mode, None),
        LineKind::Turn => (palette.border_muted, None),
        LineKind::Banner => (palette.muted, None),
    }
}

/// The trace viewport: `lines` (oldest first) word-wrapped to `width`,
/// styled by kind, returning the `height` rows ending `offset` rows from
/// the bottom (0 = the newest screen). Wrapped rows never exceed `width`.
pub fn trace_lines(
    palette: &Palette,
    lines: &[TuiLine],
    width: usize,
    height: usize,
    offset: usize,
) -> Vec<StyledLine> {
    let mut wrapped: Vec<StyledLine> = Vec::new();
    for line in lines.iter() {
        let (fg, bg) = style_for_kind(palette, line.kind);
        for chunk in wrap_text(line.text.as_str(), width) {
            wrapped.push(StyledLine {
                text: chunk.to_string(),
                fg,
                bg,
            });
        }
    }
    let mut window: Vec<StyledLine> = Vec::new();
    let len = wrapped.len();
    if len > 0 {
        let end = len.saturating_sub(offset);
        let start = end.saturating_sub(height);
        let mut i = start;
        while i < end {
            window.push(wrapped[i].clone());
            i += 1;
        }
    }
    window
}

/// A centered `extension_ui_request` modal over the trace area: a bordered
/// box (`┌─┐│└┘`, accent frame on the user-message panel fill) holding the
/// dialog's lines. Returns `None` when the width is too narrow to draw;
/// otherwise every returned line is exactly `width` characters and the box
/// is centered both ways in the `height × width` viewport.
pub fn dialog_box(
    palette: &Palette,
    req: &ExtensionUiRequest,
    width: usize,
    height: usize,
) -> Option<Vec<StyledLine>> {
    if width < 5 {
        return None;
    }
    let content = dialog_lines(req);
    let mut inner: usize = 1;
    for line in content.iter() {
        // Two border columns plus the box sides.
        inner = inner.max(line.chars().count() + 2);
    }
    inner = inner.min(width - 2);
    let box_h = content.len() + 2;
    let top = if height > box_h {
        (height - box_h) / 2
    } else {
        0
    };
    let left = (width - inner - 2) / 2;
    let hpad = fill_with(' ', left);
    let frame_fg = palette.border_accent;
    let fill = Some(palette.user_message_bg);

    let mut out: Vec<StyledLine> = Vec::new();
    let mut row = 0;
    while row < top {
        out.push(StyledLine {
            text: fill_with(' ', width),
            fg: Color::Default,
            bg: None,
        });
        row += 1;
    }
    out.push(StyledLine {
        text: pad_right(format!("{hpad}┌{}┐", fill_with('─', inner)), width),
        fg: frame_fg,
        bg: fill,
    });
    for line in content {
        let body = format!("│ {line}");
        let inner_line = pad_line_to(body.as_str(), inner + 1);
        out.push(StyledLine {
            text: pad_right(format!("{hpad}{}│", inner_line), width),
            fg: palette.text,
            bg: fill,
        });
    }
    out.push(StyledLine {
        text: pad_right(format!("{hpad}└{}┘", fill_with('─', inner)), width),
        fg: frame_fg,
        bg: fill,
    });
    while out.len() < height {
        out.push(StyledLine {
            text: fill_with(' ', width),
            fg: Color::Default,
            bg: None,
        });
    }
    Some(out)
}

// ---------------- modal prompts & input (step 6) ----------------

/// One active modal prompt: a permission dialog or an ASK question.
/// Exactly one modal is open at a time; the input task dispatches typed
/// lines against it, and the content that line mode prints (dialog
/// banner, ASK question) is drawn by [`modal_box`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// A blocking permission-dialog `extension_ui_request`.
    Dialog(ExtensionUiRequest),
    /// An ASK pause: the worker's question text.
    Ask(String),
}

/// The input task's verdict on the active modal — what the awaiting flow
/// (dialog round trip / ASK pause) reacts to. The side effects stay in
/// the flows: replies travel `reply_extension_ui`; commands flip the
/// loop's control flags (unchanged from line mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalOutcome {
    /// A dialog reply, already mapped by `reply_from_input` (`^D` and `c`
    /// both produce `Cancelled` — line-mode EOF parity).
    DialogReply(UiReply),
    /// An ASK answer to carry into the fresh worker; `None` means the
    /// operator gave no answer (blank line / `^D` / EOF) — the run stops.
    AskAnswer(Option<String>),
    /// `stop` (or `^C`) — stop the run.
    Stop,
    /// `restart` — restart the run with a fresh worker.
    Restart,
}

/// A note the modal shows above the input line while it stays open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalNote {
    /// The `status` command: show the live worker status line.
    Status,
    /// The line was not a valid reply for the dialog.
    InvalidReply,
}

/// The input task's verdict on one modal line: close the modal with an
/// outcome, or keep it open and show a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalDecision {
    Close(ModalOutcome),
    Keep(ModalNote),
}

/// Pure: dispatch one submitted modal line exactly like the line mode's
/// prompts do — through the unchanged `line_command` / `reply_from_input`
/// — scoped to the modal kind. `status` keeps the modal open with a
/// status note; a dialog's unrecognized line keeps it open with the
/// invalid-reply hint. For an ASK pause `restart` is a plain answer
/// (line-mode parity: the ASK loop only commands `stop`/`status`).
pub fn dispatch_modal_line(modal: &Modal, input: &str) -> ModalDecision {
    let command: Option<LineCommand> = line_command(input);
    match modal {
        Modal::Dialog(req) => match command {
            Some(LineCommand::Stop) => ModalDecision::Close(ModalOutcome::Stop),
            Some(LineCommand::Restart) => ModalDecision::Close(ModalOutcome::Restart),
            Some(LineCommand::Status) => ModalDecision::Keep(ModalNote::Status),
            None => match reply_from_input(req, input) {
                Some(reply) => ModalDecision::Close(ModalOutcome::DialogReply(reply)),
                None => ModalDecision::Keep(ModalNote::InvalidReply),
            },
        },
        Modal::Ask(_) => match command {
            Some(LineCommand::Stop) => ModalDecision::Close(ModalOutcome::Stop),
            Some(LineCommand::Status) => ModalDecision::Keep(ModalNote::Status),
            Some(LineCommand::Restart) => {
                // Line-mode parity: `restart` at an ASK prompt is the answer.
                ModalDecision::Close(ModalOutcome::AskAnswer(Some(input.to_string())))
            }
            None => {
                let trimmed = input.trim();
                ModalDecision::Close(ModalOutcome::AskAnswer(if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }))
            }
        },
    }
}

/// Pure: the `^D` / EOF outcome for the active modal — line-mode parity
/// (a dialog is dismissed with `Cancelled`; an ASK pause stops without an
/// answer).
pub fn eof_outcome(modal: &Modal) -> ModalOutcome {
    match modal {
        Modal::Dialog(_) => ModalOutcome::DialogReply(UiReply::Cancelled),
        Modal::Ask(_) => ModalOutcome::AskAnswer(None),
    }
}

/// Pure: the `status` command's in-modal note — the live worker status
/// line (line mode prints the same numbers to stderr; the footer shows
/// them continuously).
pub fn status_note_text(row: u64, view: &WorkerView) -> String {
    let agent: Option<&str> = if view.agent_id.is_empty() {
        None
    } else {
        Some(view.agent_id.as_str())
    };
    format!(
        "status: {}",
        format_status_line(
            row.to_string().as_str(),
            agent,
            view.turns,
            view.max_turns,
            view.context_percent,
            view.elapsed_ms,
        )
    )
}

/// The modal overlay drawn over the trace viewport while a prompt is
/// open: a centered bordered box (like [`dialog_box`]) holding the
/// prompt's content, a reserved dim note row (always present so the box
/// never jumps), and the input line (`{label}> {input}▌` — the block
/// marks the typing position). Every returned row is exactly `width`
/// characters (blank outside the box); `None` when `width` is too
/// narrow. The box is centered in the `height × width` viewport and
/// never taller than `height`.
pub fn modal_box(
    palette: &Palette,
    modal: &Modal,
    input: &str,
    note: Option<&str>,
    width: usize,
    height: usize,
) -> Option<Vec<StyledLine>> {
    if width < 5 {
        return None;
    }
    let (content, prompt) = match modal {
        Modal::Dialog(req) => (dialog_lines(req), dialog_prompt_label(req)),
        Modal::Ask(question) => (
            vec![
                "── worker question ──".to_string(),
                question.trim().to_string(),
            ],
            "answer>".to_string(),
        ),
    };
    let mut inner: usize = 1;
    for line in content.iter() {
        inner = inner.max(line.chars().count() + 2);
    }
    inner = inner.min(width - 2);
    // Content + note row + input row + top/bottom borders.
    let box_h = content.len() + 4;
    let top = if height > box_h {
        (height - box_h) / 2
    } else {
        0
    };
    let left = (width - inner - 2) / 2;
    let hpad = fill_with(' ', left);
    let frame_fg = palette.border_accent;
    let fill = Some(palette.user_message_bg);

    let mut out: Vec<StyledLine> = Vec::new();
    let mut row = 0;
    while row < top {
        out.push(StyledLine {
            text: fill_with(' ', width),
            fg: Color::Default,
            bg: None,
        });
        row += 1;
    }
    out.push(StyledLine {
        text: pad_right(format!("{hpad}┌{}┐", fill_with('─', inner)), width),
        fg: frame_fg,
        bg: fill,
    });
    for line in content {
        let body = format!("│ {line}");
        let inner_line = pad_line_to(body.as_str(), inner + 1);
        out.push(StyledLine {
            text: pad_right(format!("{hpad}{}│", inner_line), width),
            fg: palette.text,
            bg: fill,
        });
    }
    // Reserved note row (dim) — always present so the box never jumps.
    let note_text: &str = note.unwrap_or("");
    out.push(StyledLine {
        text: pad_right(
            format!("{hpad}│ {}│", pad_line_to(note_text, inner - 1)),
            width,
        ),
        fg: palette.dim,
        bg: fill,
    });
    // The input line, with a block marking the typing position.
    let input_text = format!("{prompt} {input}▌");
    out.push(StyledLine {
        text: pad_right(
            format!("{hpad}│ {}│", pad_line_to(input_text.as_str(), inner - 1)),
            width,
        ),
        fg: palette.text,
        bg: fill,
    });
    out.push(StyledLine {
        text: pad_right(format!("{hpad}└{}┘", fill_with('─', inner)), width),
        fg: frame_fg,
        bg: fill,
    });
    while out.len() > height {
        out.pop();
    }
    while out.len() < height {
        out.push(StyledLine {
            text: fill_with(' ', width),
            fg: Color::Default,
            bg: None,
        });
    }
    Some(out)
}

// ---------------- text wrapping ----------------

/// A char-vector → `String` restore (handles multibyte characters by never
/// slicing between them).
fn chars_to_string(chars: &[char]) -> String {
    let mut out = String::new();
    for c in chars.iter() {
        out.push(*c);
    }
    out
}

/// Greedy word-wrap of `text` to `width` characters. Words longer than
/// `width` are hard-broken into `width`-sized chunks; every produced line
/// is at most `width` characters (property-tested). A zero/negative width
/// yields no lines.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    // Split into words on ASCII whitespace.
    let mut words: Vec<Vec<char>> = Vec::new();
    let mut current: Vec<char> = Vec::new();
    for c in text.chars() {
        if c == ' ' || c == '\t' || c == '\n' {
            if !current.is_empty() {
                words.push(current);
                current = Vec::new();
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }

    let mut out: Vec<String> = Vec::new();
    let mut line: Vec<char> = Vec::new();
    for w in &words {
        let word: Vec<char> = w.clone();
        if word.len() > width {
            // The current line cannot fit an over-width word: flush it and
            // hard-break the word into width-sized chunks.
            if !line.is_empty() {
                out.push(chars_to_string(&line));
            }
            let mut start: usize = 0;
            while start + width < word.len() {
                out.push(chars_to_string(&word[start..start + width]));
                start += width;
            }
            let mut rest: Vec<char> = Vec::new();
            let mut j = start;
            while j < word.len() {
                rest.push(word[j]);
                j += 1;
            }
            line = rest;
            continue;
        }
        let need = if line.is_empty() {
            word.len()
        } else {
            line.len() + 1 + word.len()
        };
        if need <= width {
            if !line.is_empty() {
                line.push(' ');
            }
            for c in word {
                line.push(c);
            }
        } else {
            out.push(chars_to_string(&line));
            line = word;
        }
    }
    if !line.is_empty() {
        out.push(chars_to_string(&line));
    }
    out
}

// ---------------- terminal backend (thin I/O, step 4) ----------------

/// A terminal size in character cells (rows × cols).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub rows: usize,
    pub cols: usize,
}

/// The size assumed before the first DSR query answers, and the fallback
/// when a terminal never answers within the deadline.
pub const DEFAULT_SIZE: Size = Size { rows: 24, cols: 80 };

/// Display mode decided by the TTY gate (locked 2026-09-12: full-screen
/// TUI on a terminal; the existing byte-exact line mode otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Full-screen alternate-buffer TUI with raw-mode stdin.
    Tui,
    /// The existing plain line mode (traces → stderr, dialogs → stdout).
    Line,
}

/// The TTY-ness of the three standard streams, as observed by the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyFaces {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

/// Pure: the `isatty` gate. The TUI needs a drawable stdout and raw-mode
/// stdin; stderr is carried for completeness — reports print to it after
/// the alternate screen is left and are fine when piped — so a missing
/// stderr tty alone does not fall back to line mode.
pub fn choose_mode(faces: TtyFaces) -> DisplayMode {
    if faces.stdin && faces.stdout {
        DisplayMode::Tui
    } else {
        DisplayMode::Line
    }
}

/// One decoded key from the raw byte stream. Raw mode disables `ICANON`,
/// so the terminal presents a byte stream; the TUI input task maps bytes
/// with [`decode_key`]. `^C`/`^D` map here because `cfmakeraw` clears
/// `ISIG` — they arrive as key bytes, not signals (plan step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keystroke {
    /// A printable/ordinary byte — appended to the input line.
    Char(char),
    /// `\n` / `\r` — submit the current input line.
    Enter,
    /// `\b` or DEL — erase the last character of the input line.
    Backspace,
    /// Tab — no completion in v1; ignored for now.
    Tab,
    /// Escape — escape-sequence parsing is a v1.1 follow-up; ignored.
    Escape,
    /// `^C` — abort and unwind the TUI.
    CtrlC,
    /// `^D` — EOF parity with line mode (`UiReply::Cancelled` dismisses a
    /// dialog; the ASK pause stops the run).
    CtrlD,
    /// Any other control byte (arrows arrive as `ESC [` sequences; v1
    /// ignores them).
    Other,
}

/// Pure: map one raw key byte to a [`Keystroke`].
///
/// ```rust
/// # use pi_plan::tui::{decode_key, Keystroke};
/// assert!(decode_key(0x03) == Keystroke::CtrlC);
/// assert!(decode_key(0x04) == Keystroke::CtrlD);
/// assert!(decode_key(0x0a) == Keystroke::Enter);
/// assert!(decode_key(b'x') == Keystroke::Char('x'));
/// assert!(decode_key(0x7f) == Keystroke::Backspace);
/// ```
pub fn decode_key(byte: u8) -> Keystroke {
    match byte {
        0x03 => Keystroke::CtrlC,
        0x04 => Keystroke::CtrlD,
        0x0a | 0x0d => Keystroke::Enter,
        0x08 | 0x7f => Keystroke::Backspace,
        0x09 => Keystroke::Tab,
        0x1b => Keystroke::Escape,
        b if (0x20..=0x7e).contains(&b) => Keystroke::Char(char::from(b)),
        _ => Keystroke::Other,
    }
}

/// Parse the terminal's size-report reply into a [`Size`].
///
/// Two response shapes are accepted (both emitted by xterm-compatible
/// terminals such as tmux and ghostty):
///
/// - the cursor-position report `ESC [ <rows> ; <cols> R` — the answer to
///   `\e[6n` after planting the cursor at the corner, and
/// - the CSI 18 t form `ESC [ 8 ; <rows> ; <cols> t`.
///
/// Leading/trailing junk is ignored and malformed reports yield `None`, so
/// the DSR query loop ([`Terminal::query_size`]) can call this until it
/// succeeds.
pub fn parse_size_report(report: &[u8]) -> Option<Size> {
    let n = report.len();
    if n < 6 || report[0] != 0x1b || report[1] != b'[' {
        return None;
    }
    let mut i: usize = 2;
    // CSI 18 t answers "8 ; rows ; cols t" — skip the literal "8;".
    if i + 2 < n && report[i] == b'8' && report[i + 1] == b';' {
        i += 2;
    }
    let mut rows: u64 = 0;
    let mut got_rows = false;
    while i < n && report[i] >= b'0' && report[i] <= b'9' {
        rows = rows * 10 + ((report[i] - b'0') as u64);
        got_rows = true;
        i += 1;
    }
    if !got_rows || i >= n || report[i] != b';' {
        return None;
    }
    i += 1;
    let mut cols: u64 = 0;
    let mut got_cols = false;
    while i < n && report[i] >= b'0' && report[i] <= b'9' {
        cols = cols * 10 + ((report[i] - b'0') as u64);
        got_cols = true;
        i += 1;
    }
    if !got_cols || i >= n || (report[i] != b'R' && report[i] != b't') {
        return None;
    }
    if rows == 0 || cols == 0 {
        return None;
    }
    Some(Size {
        rows: rows as usize,
        cols: cols as usize,
    })
}

/// Sink through which the backend emits raw ANSI bytes: the real one
/// writes to stdout; tests count/record (the plan's mocked terminal
/// writer).
///
/// The `Send` bound exists so a [`Terminal`] can cross into the tokio
/// render task (step 5): `tokio::spawn` requires `Send` futures and the
/// backend rides inside its task argument. Every sink in the tree — the
/// stdout writer and the test counters — captures only `Send` values.
pub type AnsiSink<'a> = dyn Fn(&[u8]) + Send + 'a;

/// Terminal state captured by [`Terminal::enter`], restored by
/// [`Terminal::leave`] and the drop guard.
#[derive(Clone)]
pub struct SavedTerminal {
    /// stdin's termios before `cfmakeraw`, when it could be read. `None`
    /// only happens in tests that exercise the escape-only unwind.
    pub stdin_termios: Option<Termios>,
}

/// Enter: alternate screen, clear + home, hide cursor.
pub const ALT_SCREEN_ENTER: &str = "\u{1b}[?1049h\u{1b}[2J\u{1b}[H\u{1b}[?25l";
/// Exit: show cursor, leave the alternate screen.
pub const ALT_SCREEN_LEAVE: &str = "\u{1b}[?25h\u{1b}[?1049l";

/// Thin terminal backend — the only I/O layer of the TUI (the layout
/// stays pure in the frame builders above).
pub struct Terminal<'a> {
    /// Where raw ANSI bytes go.
    pub sink: Box<AnsiSink<'a>>,
    /// Whether the alternate screen + raw stdin are currently owned; makes
    /// `leave` idempotent.
    pub active: bool,
}

impl<'a> Terminal<'a> {
    /// New backend writing through `sink` (the binary passes a stdout
    /// writer; tests pass a counter).
    pub fn new(sink: Box<AnsiSink<'a>>) -> Self {
        Self {
            sink,
            active: false,
        }
    }

    /// Enter the alternate screen, clear it, hide the cursor, and switch
    /// stdin to raw mode (termios save → `cfmakeraw` → `tcsetattr`). The
    /// returned [`SavedTerminal`] must reach `leave` (or the guard) on
    /// every exit path. Fails when stdin is not a terminal.
    pub fn enter(&mut self) -> Result<SavedTerminal, Errno> {
        let saved = tcgetattr(std::io::stdin())?;
        let mut raw = saved.clone();
        cfmakeraw(&mut raw);
        tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw)?;
        (self.sink)(ALT_SCREEN_ENTER.as_bytes());
        self.active = true;
        Ok(SavedTerminal {
            stdin_termios: Some(saved),
        })
    }

    /// Unwind `enter`: show the cursor, leave the alternate screen, then
    /// restore stdin's termios. Idempotent (the `active` flag); safe on
    /// every exit path.
    pub fn leave(&mut self, saved: &SavedTerminal) {
        if !self.active {
            return;
        }
        // Escapes first, while stdin is still raw: the writes are not
        // echoed. Restoring termios is best-effort — it can legitimately
        // fail on a half-closed terminal (or in tests).
        (self.sink)(ALT_SCREEN_LEAVE.as_bytes());
        if let Some(restore) = saved.stdin_termios.as_ref() {
            let _ = tcsetattr(std::io::stdin(), SetArg::TCSANOW, restore);
        }
        self.active = false;
    }

    /// Query the terminal size: park the cursor at the bottom-right corner
    /// (`\e[999;999H`) and request a cursor-position report (`\e[6n`). The
    /// `ESC [ rows ; cols R` answer is decoded by [`parse_size_report`];
    /// terminals that never answer within the deadline (≈200 ms) or decode
    /// to nothing return `fallback`.
    pub fn query_size(&mut self, fallback: Size) -> Size {
        (self.sink)(SIZE_QUERY_REQUEST.as_bytes());
        let mut collected: Vec<u8> = Vec::new();
        let mut chunk: [u8; 64] = [0u8; 64];
        // The PollFd borrows from this handle, so it must outlive the fds.
        let stdin_handle = std::io::stdin();
        let mut fds: Vec<PollFd> = vec![PollFd::new(stdin_handle.as_fd(), PollFlags::POLLIN)];
        let mut budget: u16 = SIZE_QUERY_STEPS;
        while budget > 0 {
            budget -= 1;
            let nready: i32 = poll(&mut fds, SIZE_QUERY_STEP_MS).unwrap_or_default();
            if nready <= 0 {
                continue;
            }
            let got: usize = read(std::io::stdin(), &mut chunk[..]).unwrap_or_default();
            if got == 0 {
                break; // EOF — no terminal answer is coming
            }
            let mut i: usize = 0;
            while i < got {
                collected.push(chunk[i]);
                i += 1;
            }
            if parse_size_report(collected.as_slice()).is_some() {
                break;
            }
        }
        parse_size_report(collected.as_slice()).unwrap_or(fallback)
    }
}

/// Request planted before the DSR size query: jump to the corner, then
/// ask the terminal to report the cursor position (`\e[6n`).
pub const SIZE_QUERY_REQUEST: &str = "\u{1b}[999;999H\u{1b}[6n";

/// Poll step per DSR read (ms).
pub const SIZE_QUERY_STEP_MS: u16 = 20;
/// Max polls per size query → ≈200 ms worst case.
pub const SIZE_QUERY_STEPS: u16 = 10;

/// RAII drop guard around a live TUI: restores the terminal (alt screen,
/// cursor, stdin raw mode) on EVERY exit path — worker failure, `^C`, EOF,
/// or a plain return — per the plan's crash safety. Tests drive it with a
/// counting sink to assert the unwind.
pub struct TerminalGuard<'a> {
    /// The backend whose `leave` the drop runs.
    pub terminal: Terminal<'a>,
    /// Captured by `enter`; restored by the drop (or an explicit leave).
    pub saved: SavedTerminal,
    /// Set once unwound (explicit leave or disarm) so the drop is inert.
    pub finished: bool,
}

impl<'a> TerminalGuard<'a> {
    /// Arm the guard around an already-entered terminal.
    pub fn arm(terminal: Terminal<'a>, saved: SavedTerminal) -> Self {
        Self {
            terminal,
            saved,
            finished: false,
        }
    }

    /// Explicitly unwind now; the later drop is a no-op.
    pub fn leave(&mut self) {
        if !self.finished {
            self.terminal.leave(&self.saved);
            self.finished = true;
        }
    }

    /// Detach the guard without unwinding (the caller restored state).
    pub fn disarm(&mut self) {
        self.finished = true;
    }
}

impl<'a> Drop for TerminalGuard<'a> {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Broadcast capacity for resize notifications (unit events).
pub const RESIZE_CHANNEL_CAPACITY: usize = 4;

/// Handle returned by [`watch_resizes`]: keeps the signalfd thread alive.
/// It holds no resources of its own once armed — the thread drains the
/// signalfd until the process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeWatcher {
    pub armed: bool,
}

/// Drain the signalfd, publishing a unit per SIGWINCH until the fd errors
/// (process teardown). Runs on a detached std thread with SIGWINCH blocked
/// (inherited from the arming thread).
fn drain_resizes(sfd: SignalFd, tx: broadcast::Sender<()>) {
    loop {
        match sfd.read_signal() {
            Ok(Some(info)) if info.ssi_signo == Signal::SIGWINCH as u32 => {
                let _ = tx.send(());
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(_) => break,
        }
    }
}

/// Arm the SIGWINCH bridge: block SIGWINCH in the calling thread (a
/// signalfd only sees signals that are blocked) and spawn a std thread
/// that reads the signalfd, publishing a unit on the returned channel for
/// every window-size change. The render loop (step 5) awaits the channel
/// next to its 120 ms tick and re-queries the size on a short cadence as a
/// safety net, because a process-directed SIGWINCH can be consumed by a
/// thread that did not inherit the block.
pub fn watch_resizes() -> Result<(ResizeWatcher, broadcast::Receiver<()>), Errno> {
    let (tx, rx) = broadcast::channel::<()>(RESIZE_CHANNEL_CAPACITY);
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGWINCH);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)?;
    let sfd = SignalFd::new(&mask)?;
    // The thread inherits the blocked mask from this one.
    let _ = std::thread::spawn::<_, ()>(move || drain_resizes(sfd, tx));
    Ok((ResizeWatcher { armed: true }, rx))
}

// ---------------- shared state & frame composition (step 5) ----------------

/// The live per-worker view the footer renders, assembled from the
/// extended [`WorkerSnapshot`] by the tail task (plan step 5). The stats
/// arrive on the same `get_session_stats` poll that today only feeds
/// `context%`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerView {
    /// The worker id (`agent <id>` in the footer).
    pub agent_id: String,
    /// `turn_end` events observed so far.
    pub turns: u32,
    /// The row's turn ceiling (`resolve_max_turns`).
    pub max_turns: u32,
    /// Latest `contextUsage.percent` from the stats poll; `None` right
    /// after compaction (rendered `?`).
    pub context_percent: Option<f64>,
    /// The session token count (`data.tokens.total`) backing the footer's
    /// `(tokens/window)` readout. `None` before the first stats response.
    pub context_tokens: Option<u64>,
    /// `data.contextUsage.contextWindow` from the same poll.
    pub context_window: Option<u64>,
    /// Provider-reported cost in USD (may be 0 or absent; rendered `—`).
    pub cost: Option<f64>,
    /// Wall-clock ms since the worker spawned.
    pub elapsed_ms: u64,
}

/// Assemble a [`WorkerView`] from a worker snapshot + row budget (pure).
/// `now` is the current epoch-ms clock reading; elapsed is clamped to
/// non-negative under clock skew.
pub fn view_from_snapshot(snap: &WorkerSnapshot, max_turns: u32, now: u64) -> WorkerView {
    WorkerView {
        agent_id: snap.id.to_string(),
        turns: snap.turn_count,
        max_turns,
        context_percent: snap.context_percent,
        context_tokens: snap.tokens.map(|t| t.total),
        context_window: snap.context_window,
        cost: snap.cost,
        elapsed_ms: now.max(snap.started_at).saturating_sub(snap.started_at),
    }
}

/// The tagged trace ring capacity in the TUI: raised from line mode's 240
/// to ~1000 with the full-frame re-render cost in mind (plan risks).
pub const TUI_RING_CAPACITY: usize = 1000;

/// The shared TUI state: the plan meta the header renders, the live worker
/// view the footer renders, and the tagged trace ring the viewport
/// renders. Guarded by a tokio mutex in the binary
/// (`Arc<tokio::sync::Mutex<TuiState>>`). The render loop reads it; the
/// tail/banner writers mutate it.
#[derive(Debug, Clone, PartialEq)]
pub struct TuiState {
    /// 1-based position of the current row in the FULL TODO.md table.
    pub row: u64,
    /// Total rows in that table.
    pub total: u64,
    /// The row's logical unit (the header's "few words").
    pub unit: String,
    /// The plan source path (header line 2, muted).
    pub source: Option<String>,
    /// The live worker view (footer).
    pub worker: WorkerView,
    /// The tagged trace ring, oldest first (the viewport, bottom-anchored).
    pub ring: Vec<TuiLine>,
    /// Rows scrolled back from the bottom. v1 keeps 0; scrollback
    /// navigation is a v1.1 follow-up.
    pub scroll_offset: usize,
    /// The active modal prompt, if any (step 6): the input task edits and
    /// dispatches against it; [`compose_frame`] overlays [`modal_box`].
    pub modal: Option<Modal>,
    /// The operator's raw input line for the active modal.
    pub modal_input: String,
    /// A dim note row shown above the modal input line (the `status`
    /// command / the invalid-reply hint); cleared on the next keystroke.
    pub modal_note: Option<String>,
    /// The last dispatch verdict, consumed by the awaiting modal flow
    /// ([`await_modal_outcome`]); `Some` only while a modal just closed.
    pub modal_outcome: Option<ModalOutcome>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiState {
    /// A blank state: no plan meta, an idle footer, an empty ring.
    pub fn new() -> Self {
        Self {
            row: 0,
            total: 0,
            unit: String::new(),
            source: None,
            worker: WorkerView {
                agent_id: String::new(),
                turns: 0,
                max_turns: 0,
                context_percent: None,
                context_tokens: None,
                context_window: None,
                cost: None,
                elapsed_ms: 0,
            },
            ring: Vec::new(),
            scroll_offset: 0,
            modal: None,
            modal_input: String::new(),
            modal_note: None,
            modal_outcome: None,
        }
    }

    /// Set the header's plan meta — called once per row by the supervise
    /// loop (via the spawn seam). `row` is the 1-based position in the
    /// FULL table and `total` its length, so `--row n` runs still show
    /// where they sit in the whole plan.
    pub fn set_plan(&mut self, row: u64, total: u64, unit: String, source: Option<String>) {
        self.row = row;
        self.total = total;
        self.unit = unit;
        self.source = source;
    }

    /// Replace the live worker view (footer) — called by the tail on the
    /// status cadence and at `turn start`.
    pub fn set_worker_view(&mut self, view: WorkerView) {
        self.worker = view;
    }

    /// Append one tagged line, evicting the oldest past the capacity (the
    /// same eviction contract as the line mode's [`TraceRing`]-analog).
    pub fn push_line(&mut self, line: TuiLine) {
        self.ring.push(line);
        while self.ring.len() > TUI_RING_CAPACITY {
            self.ring.remove(0);
        }
    }

    /// Append a banner line (spawn / terminal / banner report lines).
    pub fn push_banner(&mut self, text: String) {
        self.push_line(TuiLine {
            kind: LineKind::Banner,
            text,
        });
    }

    /// Open a modal prompt, discarding any stale input/note/outcome.
    pub fn open_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
        self.modal_input = String::new();
        self.modal_note = None;
        self.modal_outcome = None;
    }

    /// Close the modal with a verdict for the awaiting flow.
    pub fn close_modal(&mut self, outcome: ModalOutcome) {
        self.modal = None;
        self.modal_input = String::new();
        self.modal_note = None;
        self.modal_outcome = Some(outcome);
    }

    /// Append one printable key to the modal input line (drop the note).
    pub fn modal_append(&mut self, c: char) {
        self.modal_note = None;
        self.modal_input.push(c);
    }

    /// Erase the last character of the modal input line, if any.
    pub fn modal_backspace(&mut self) {
        self.modal_note = None;
        let mut chars: Vec<char> = Vec::new();
        for c in self.modal_input.chars() {
            chars.push(c);
        }
        if chars.is_empty() {
            return;
        }
        chars.pop();
        self.modal_input = chars_to_string(&chars);
    }
}

/// The trace viewport height for a screen of `height` rows: everything
/// below the 2-row header and above the 1-row footer (rows 3..H-1).
/// `0` when the fixed regions cannot fit.
pub fn viewport_height_for(height: usize) -> usize {
    if height >= 4 { height - 3 } else { 0 }
}

/// Compose one full frame from the shared state: the 2-row header (plan
/// meta), the trace viewport (ring, bottom-anchored, wrapped to `width`
/// — or the modal prompt's [`modal_box`] overlay while one is open), and
/// the 1-row footer (live stats + hints). Every returned line is exactly
/// `width` characters; the output has `2 + viewport + 1` rows at most
/// `height`. Returns an empty vec when the screen cannot fit the fixed
/// regions. Pure (the render loop draws the result).
pub fn compose_frame(
    palette: &Palette,
    state: &TuiState,
    width: usize,
    height: usize,
) -> Vec<StyledLine> {
    if height < 4 || width < 3 {
        return Vec::new();
    }
    let mut out: Vec<StyledLine> = Vec::new();
    for line in header_lines(
        palette,
        state.row,
        state.total,
        state.unit.as_str(),
        state.source.as_deref(),
        width,
    ) {
        out.push(line);
    }
    // The viewport: the trace ring, or the modal overlay when a prompt is
    // open (the footer below stays — the persistent readout is never
    // occluded by a modal). Wrap yields ≤ width rows; pad so a shorter
    // row erases the previous frame's content (exact-width redraw).
    let viewport: Vec<StyledLine> = match &state.modal {
        Some(modal) => modal_box(
            palette,
            modal,
            state.modal_input.as_str(),
            state.modal_note.as_deref(),
            width,
            viewport_height_for(height),
        )
        .unwrap_or_default(),
        None => {
            let mut lines: Vec<StyledLine> = Vec::new();
            for line in trace_lines(
                palette,
                &state.ring[..],
                width,
                viewport_height_for(height),
                state.scroll_offset,
            ) {
                lines.push(StyledLine {
                    text: pad_line_to(line.text.as_str(), width),
                    fg: line.fg,
                    bg: line.bg,
                });
            }
            lines
        }
    };
    for line in viewport {
        out.push(line);
    }
    // The footer borrows its stats from the state; `format_footer_line`
    // runs inside this expression so the borrows end here.
    let agent: Option<&str> = if state.worker.agent_id.is_empty() {
        None
    } else {
        Some(state.worker.agent_id.as_str())
    };
    for line in footer_lines(
        palette,
        &FooterStats {
            row_id: state.row.to_string().as_str(),
            agent_id: agent,
            turns: state.worker.turns,
            max_turns: state.worker.max_turns,
            context_percent: state.worker.context_percent,
            context_tokens: state.worker.context_tokens,
            context_window: state.worker.context_window,
            cost: state.worker.cost,
            elapsed_ms: state.worker.elapsed_ms,
        },
        width,
    ) {
        out.push(line);
    }
    out
}

// ---------------- stdin input task & modal flow (step 6) ----------------

/// Poll step for raw stdin (ms) — bounded wakeups keep the task
/// responsive to modal opens/closes between reads.
pub const INPUT_POLL_MS: u16 = 500;

/// Apply the input task's verdict to the shared state: `Close` records
/// the outcome for the awaiting flow; `Keep` sets the modal's note row.
/// Side effects are confined to [`TuiState`].
pub fn apply_modal_decision(state: &mut TuiState, decision: ModalDecision) {
    match decision {
        ModalDecision::Close(outcome) => state.close_modal(outcome),
        ModalDecision::Keep(note) => match note {
            ModalNote::Status => {
                state.modal_note = Some(status_note_text(state.row, &state.worker));
            }
            ModalNote::InvalidReply => {
                state.modal_note = Some("invalid reply — try again (or ^D to dismiss)".to_string());
            }
        },
    }
}

/// THE single stdin owner in TUI mode (plan step 6): `cfmakeraw`
/// disabled `ICANON`, so no line-mode `read_line` can assemble text
/// anymore — this task reads every raw key, edits the active modal's
/// input line, and dispatches submits through [`dispatch_modal_line`]
/// (the unchanged `line_command` / `reply_from_input`). `^D` closes with
/// EOF parity ([`eof_outcome`]); `^C` flips `ctrl_c` and closes any open
/// modal with `Stop`, so the binary's watcher unwinds the TUI on that
/// path. Escape sequences are swallowed whole (v1 ignores arrows; a DSR
/// size-report reply stolen mid-query is dropped rather than typed).
/// Keys with no modal open are dropped — raw mode does not echo, which
/// matches line mode where nothing reads stdin between prompts.
pub async fn input_task(state: Arc<tokio::sync::Mutex<TuiState>>, ctrl_c: Arc<AtomicBool>) {
    // The PollFd borrows from this handle, so it must outlive the fds.
    let stdin_handle = std::io::stdin();
    let mut fds: Vec<PollFd> = vec![PollFd::new(stdin_handle.as_fd(), PollFlags::POLLIN)];
    let mut bytes: [u8; 64] = [0u8; 64];
    let mut seq: Vec<u8> = Vec::new();
    loop {
        let nready: i32 = poll(&mut fds, INPUT_POLL_MS).unwrap_or_default();
        if nready <= 0 {
            continue;
        }
        let got: usize = read(std::io::stdin(), &mut bytes[..]).unwrap_or_default();
        if got == 0 {
            // stdin closed (the terminal went away): EOF parity for any
            // active modal; the run keeps supervising.
            let mut guard = state.lock().await;
            let outcome = guard.modal.as_ref().map(eof_outcome);
            if let Some(outcome) = outcome {
                apply_modal_decision(&mut guard, ModalDecision::Close(outcome));
            }
            continue;
        }
        let mut i: usize = 0;
        while i < got {
            let byte = bytes[i];
            i += 1;
            if !seq.is_empty() {
                // Inside an escape sequence: swallow up to the final byte
                // (0x40..=0x7e) or a sane cap, then drop the whole thing.
                seq.push(byte);
                if (0x40..=0x7e).contains(&byte) || seq.len() >= 16 {
                    seq.clear();
                }
                continue;
            }
            match decode_key(byte) {
                Keystroke::Escape => seq.push(byte),
                Keystroke::Char(c) => {
                    let mut guard = state.lock().await;
                    if guard.modal.is_some() {
                        guard.modal_append(c);
                    }
                }
                Keystroke::Backspace => {
                    let mut guard = state.lock().await;
                    if guard.modal.is_some() {
                        guard.modal_backspace();
                    }
                }
                Keystroke::Enter => {
                    // Read + clear the line under one lock, clone the
                    // modal, then apply the verdict under a second lock
                    // (the decision is plain data).
                    let (modal, line): (Option<Modal>, String) = {
                        let mut guard = state.lock().await;
                        let line = guard.modal_input.clone();
                        guard.modal_input = String::new();
                        (guard.modal.clone(), line)
                    };
                    if let Some(modal) = modal {
                        let decision = dispatch_modal_line(&modal, line.as_str());
                        let mut guard = state.lock().await;
                        if guard.modal.is_some() {
                            apply_modal_decision(&mut guard, decision);
                        }
                    }
                }
                Keystroke::CtrlD => {
                    let mut guard = state.lock().await;
                    let outcome = guard.modal.as_ref().map(eof_outcome);
                    if let Some(outcome) = outcome {
                        apply_modal_decision(&mut guard, ModalDecision::Close(outcome));
                    }
                }
                Keystroke::CtrlC => {
                    if !ctrl_c.load(Ordering::SeqCst) {
                        ctrl_c.store(true, Ordering::SeqCst);
                        let mut guard = state.lock().await;
                        if guard.modal.is_some() {
                            apply_modal_decision(
                                &mut guard,
                                ModalDecision::Close(ModalOutcome::Stop),
                            );
                        } else {
                            guard.push_banner("^C — stopping the run…".to_string());
                        }
                    }
                }
                Keystroke::Tab | Keystroke::Other => {}
            }
        }
    }
}

/// Await the input task's verdict for the modal this flow opened. Polls
/// the shared state on a short cadence (the operator may take arbitrarily
/// long — exactly like line mode's blocking `read_line`). Returns `None`
/// only when the modal was closed without a verdict (the run itself is
/// ending); the caller treats that as a stop.
pub async fn await_modal_outcome(state: Arc<tokio::sync::Mutex<TuiState>>) -> Option<ModalOutcome> {
    loop {
        {
            let mut guard = state.lock().await;
            if guard.modal.is_none() {
                let outcome = guard.modal_outcome.clone();
                guard.modal_outcome = None;
                return outcome;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The TUI-mode hooks the tails and the supervise loop need: the shared
/// state (ring / plan meta / footer view / active modal). An `Option` in
/// the wiring — `None` keeps the line-mode tail byte-identical.
#[derive(Clone)]
pub struct TuiHooks {
    pub state: Arc<tokio::sync::Mutex<TuiState>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::rpc::{UiMethod, UiReply};
    use crate::theme::default_palette;
    use crate::worker::{Tokens, WorkerSnapshot};

    fn palette() -> Palette {
        default_palette()
    }

    /// A sink that counts writes; the plan's mocked terminal writer.
    fn counting_sink<'a>(calls: Arc<AtomicU64>) -> Box<AnsiSink<'a>> {
        Box::new(move |_: &[u8]| {
            calls.as_ref().fetch_add(1, Ordering::SeqCst);
        })
    }

    // ---- terminal backend (step 4) ----

    #[test]
    fn choose_mode_requires_stdin_and_stdout_ttys() {
        assert!(
            choose_mode(TtyFaces {
                stdin: true,
                stdout: true,
                stderr: true
            }) == DisplayMode::Tui
        );
        assert!(
            choose_mode(TtyFaces {
                stdin: false,
                stdout: true,
                stderr: true
            }) == DisplayMode::Line
        );
        assert!(
            choose_mode(TtyFaces {
                stdin: true,
                stdout: false,
                stderr: true
            }) == DisplayMode::Line
        );
        // stderr may be piped (`2>log`); the draw + raw input gates decide.
        assert!(
            choose_mode(TtyFaces {
                stdin: true,
                stdout: true,
                stderr: false
            }) == DisplayMode::Tui
        );
    }

    #[test]
    fn decode_key_maps_control_bytes_and_printables() {
        assert!(decode_key(0x03) == Keystroke::CtrlC);
        assert!(decode_key(0x04) == Keystroke::CtrlD);
        assert!(decode_key(b'\n') == Keystroke::Enter);
        assert!(decode_key(b'\r') == Keystroke::Enter);
        assert!(decode_key(0x08) == Keystroke::Backspace);
        assert!(decode_key(0x7f) == Keystroke::Backspace);
        assert!(decode_key(b'\t') == Keystroke::Tab);
        assert!(decode_key(0x1b) == Keystroke::Escape);
        assert!(decode_key(b'x') == Keystroke::Char('x'));
        assert!(decode_key(b' ') == Keystroke::Char(' '));
        assert!(decode_key(0x00) == Keystroke::Other);
    }

    #[test]
    fn parse_size_report_accepts_cpr_and_csi_18_t_forms() {
        let cpr: Vec<u8> = "\u{1b}[24;80R".to_string().into_bytes();
        let got = parse_size_report(cpr.as_slice());
        assert_eq!(got, Some(Size { rows: 24, cols: 80 }));

        let t18: Vec<u8> = "\u{1b}[8;40;120t".to_string().into_bytes();
        let got18 = parse_size_report(t18.as_slice());
        assert_eq!(
            got18,
            Some(Size {
                rows: 40,
                cols: 120
            })
        );

        // Trailing junk past the terminator is ignored.
        let padded: Vec<u8> = "\u{1b}[12;60R\u{1b}".to_string().into_bytes();
        assert_eq!(
            parse_size_report(padded.as_slice()),
            Some(Size { rows: 12, cols: 60 })
        );
    }

    #[test]
    fn parse_size_report_rejects_malformed_reports() {
        assert!(parse_size_report("plain".to_string().as_bytes()).is_none());
        assert!(parse_size_report("".to_string().as_bytes()).is_none());
        // Truncated before the terminating letter.
        assert!(parse_size_report("\u{1b}[24;80".to_string().as_bytes()).is_none());
        // Zero dimensions are not a size.
        assert!(parse_size_report("\u{1b}[0;80R".to_string().as_bytes()).is_none());
        assert!(parse_size_report("\u{1b}[24;0R".to_string().as_bytes()).is_none());
        // Colon instead of the required semicolon.
        assert!(parse_size_report("\u{1b}[24:80R".to_string().as_bytes()).is_none());
        // Missing prefix.
        assert!(parse_size_report("24;80R".to_string().as_bytes()).is_none());
    }

    #[test]
    fn alt_screen_escape_constants_match_expected_sequences() {
        assert_eq!(
            ALT_SCREEN_ENTER,
            "\u{1b}[?1049h\u{1b}[2J\u{1b}[H\u{1b}[?25l"
        );
        assert_eq!(ALT_SCREEN_LEAVE, "\u{1b}[?25h\u{1b}[?1049l");
        assert_eq!(SIZE_QUERY_REQUEST, "\u{1b}[999;999H\u{1b}[6n");
    }

    #[test]
    fn query_size_falls_back_without_a_terminal() {
        // In CI stdin is not a terminal; a live answer is covered by the
        // tmux smoke test (tests/tui_backend.rs). Skip when a tty answers.
        if tcgetattr(std::io::stdin()).is_ok() {
            eprintln!("skip: stdin is a tty — live sizes are for the smoke test");
            return;
        }
        let mut term = Terminal::new(counting_sink(Arc::new(AtomicU64::new(0))));
        let fallback = Size { rows: 7, cols: 11 };
        assert_eq!(term.query_size(fallback), fallback);
    }

    #[test]
    fn leave_emits_leave_escapes_once_and_is_idempotent() {
        let calls = Arc::new(AtomicU64::new(0));
        let mut term = Terminal::new(counting_sink(calls.clone()));
        // No tty in CI: prime the `active` flag as `enter` would.
        term.active = true;
        let saved = SavedTerminal {
            stdin_termios: None,
        };
        term.leave(&saved);
        term.leave(&saved);
        assert_eq!(calls.as_ref().load(Ordering::SeqCst), 1);
    }

    #[test]
    fn drop_guard_restores_on_every_exit_path_but_not_after_disarm() {
        // Guarded scope: the drop must restore exactly once.
        let guarded = Arc::new(AtomicU64::new(0));
        {
            let mut guard = TerminalGuard::arm(
                Terminal::new(counting_sink(guarded.clone())),
                SavedTerminal {
                    stdin_termios: None,
                },
            );
            guard.terminal.active = true; // entered, in CI without a tty
        }
        assert_eq!(guarded.as_ref().load(Ordering::SeqCst), 1);

        // Explicit leave + drop ⇒ still exactly one restore.
        let explicit = Arc::new(AtomicU64::new(0));
        {
            let mut guard = TerminalGuard::arm(
                Terminal::new(counting_sink(explicit.clone())),
                SavedTerminal {
                    stdin_termios: None,
                },
            );
            guard.terminal.active = true;
            guard.leave();
        }
        assert_eq!(explicit.as_ref().load(Ordering::SeqCst), 1);

        // Disarmed guard ⇒ no restore.
        let disarmed = Arc::new(AtomicU64::new(0));
        {
            let mut guard = TerminalGuard::arm(
                Terminal::new(counting_sink(disarmed.clone())),
                SavedTerminal {
                    stdin_termios: None,
                },
            );
            guard.terminal.active = true;
            guard.disarm();
        }
        assert_eq!(disarmed.as_ref().load(Ordering::SeqCst), 0);

        // Dormant guard (never entered) ⇒ nothing emitted.
        let dormant = Arc::new(AtomicU64::new(0));
        {
            let _ = TerminalGuard::arm(
                Terminal::new(counting_sink(dormant.clone())),
                SavedTerminal {
                    stdin_termios: None,
                },
            );
        }
        assert_eq!(dormant.as_ref().load(Ordering::SeqCst), 0);
    }

    fn select_req() -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: "ui-1".to_string(),
            method: UiMethod::Select,
            title: Some("pick".to_string()),
            message: Some("choose one".to_string()),
            options: vec!["read file".to_string(), "abort".to_string()],
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        }
    }

    // ---- header ----

    #[test]
    fn header_lines_render_title_bar_and_source_line_at_exact_width() {
        let lines = header_lines(
            &palette(),
            3,
            12,
            "Crate skeleton",
            Some("docs/research/interface-design.md".to_string().as_str()),
            60,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text.chars().count(), 60);
        assert!(lines[0].text.contains("step 3/12 · Crate skeleton"));
        assert!(lines[0].text.starts_with("┌┤ "));
        assert_eq!(lines[1].text.chars().count(), 60);
        assert!(
            lines[1]
                .text
                .contains("source: docs/research/interface-design.md")
        );
        assert!(lines[1].text.starts_with("│ "));
    }

    #[test]
    fn header_lines_truncate_a_long_unit_with_ellipsis() {
        let lines = header_lines(
            &palette(),
            3,
            12,
            "an extremely long logical unit that will never fit",
            None,
            24,
        );
        assert_eq!(lines[0].text.chars().count(), 24);
        assert!(
            lines[0].text.contains("…"),
            "the unit is ellipsized inside the bar"
        );
        assert_eq!(lines[1].text.chars().count(), 24);
        assert!(lines[1].text.starts_with("│"));
    }

    #[test]
    fn header_lines_track_the_full_plan_position_for_single_row_runs() {
        let lines = header_lines(
            &palette(),
            5,
            10,
            "unit",
            Some("s".to_string().as_str()),
            40,
        );
        assert!(lines[0].text.contains("step 5/10 · unit"));
    }

    // ---- footer ----

    #[test]
    fn footer_lines_join_stats_and_hints_at_exact_width() {
        let lines = footer_lines(
            &palette(),
            &FooterStats {
                row_id: "3",
                agent_id: Some("7"),
                turns: 4,
                max_turns: 40,
                context_percent: Some(61.5),
                context_tokens: Some(59_300),
                context_window: Some(200_000),
                cost: Some(0.0451),
                elapsed_ms: 90_000,
            },
            100,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text.chars().count(), 100);
        assert!(
            lines[0]
                .text
                .contains("$0.0451 · ctx 61% (59.3k/200k) · turns 4/40")
        );
        assert!(
            lines[0]
                .text
                .trim_end()
                .ends_with(" stop / restart / status")
        );
    }

    #[test]
    fn footer_lines_fall_back_to_truncation_on_narrow_widths() {
        let lines = footer_lines(
            &palette(),
            &FooterStats {
                row_id: "3",
                agent_id: None,
                turns: 0,
                max_turns: 40,
                context_percent: None,
                context_tokens: None,
                context_window: None,
                cost: None,
                elapsed_ms: 250,
            },
            16,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text.chars().count(), 16);
        assert!(lines[0].text.ends_with("…"));
    }

    // ---- trace ----

    #[test]
    fn trace_lines_wrap_style_and_window_by_kind() {
        let lines: Vec<TuiLine> = vec![
            TuiLine {
                kind: LineKind::Thinking,
                text: "so the compiler does not complain".to_string(),
            },
            TuiLine {
                kind: LineKind::Tool,
                text: "tool: write (call_1)".to_string(),
            },
        ];
        let out = trace_lines(&palette(), &lines[..], 20, 20, 0);
        // The thinking line wraps into multiple rows; none exceeds 20.
        assert!(out.len() >= 3);
        for styled in out.iter() {
            assert!(styled.text.chars().count() <= 20);
        }
        // Kinds carry their palette styles.
        assert_eq!(out[0].fg, palette().thinking_text);
        assert_eq!(out[1].fg, palette().thinking_text);
        assert_eq!(
            out.last().cloned().expect("tool row").fg,
            palette().tool_title
        );
        assert_eq!(
            out.last().cloned().expect("tool row").bg,
            Some(palette().tool_pending_bg)
        );
    }

    #[test]
    fn trace_lines_viewport_height_and_offset_select_the_bottom_window() {
        let lines: Vec<TuiLine> = vec![
            TuiLine {
                kind: LineKind::Banner,
                text: "row 1: done".to_string(),
            },
            TuiLine {
                kind: LineKind::Banner,
                text: "row 2: done".to_string(),
            },
            TuiLine {
                kind: LineKind::Banner,
                text: "row 3: done".to_string(),
            },
            TuiLine {
                kind: LineKind::Banner,
                text: "row 4: done".to_string(),
            },
            TuiLine {
                kind: LineKind::Banner,
                text: "row 5: done".to_string(),
            },
        ];
        // Bottom two rows by default.
        let bottom = trace_lines(&palette(), &lines[..], 80, 2, 0);
        assert_eq!(bottom.len(), 2);
        assert!(bottom[0].text.contains("row 4"));
        assert!(bottom[1].text.contains("row 5"));
        // Offset one reveals earlier rows.
        let scrolled = trace_lines(&palette(), &lines[..], 80, 2, 1);
        assert_eq!(scrolled.len(), 2);
        assert!(scrolled[0].text.contains("row 3"));
        assert!(scrolled[1].text.contains("row 4"));
        // A viewport taller than the ring shows everything.
        let tall = trace_lines(&palette(), &lines[..], 80, 99, 0);
        assert_eq!(tall.len(), 5);
        assert!(tall[0].text.contains("row 1"));
    }

    #[test]
    fn trace_lines_overflowing_words_are_hard_broken() {
        let lines: Vec<TuiLine> = vec![TuiLine {
            kind: LineKind::Bash,
            text: "abcdefghij".to_string(),
        }];
        let out = trace_lines(&palette(), &lines[..], 4, 20, 0);
        assert_eq!(out.len(), 3, "10 chars broken into 4+4+2");
        assert_eq!(out[0].text, "abcd".to_string());
        assert_eq!(out[2].text, "ij".to_string());
        for styled in out.iter() {
            assert!(styled.text.chars().count() <= 4);
        }
    }

    // ---- dialog ----

    #[test]
    fn dialog_box_is_centered_framed_and_exactly_frame_wide() {
        let req = select_req();
        let out = dialog_box(&palette(), &req, 40, 12).expect("a box fits in 40×12");
        assert_eq!(out.len(), 12, "modal covers the full viewport");
        for styled in &out {
            assert_eq!(styled.text.chars().count(), 40);
        }
        // Vertically centered: (12 - box_h) / 2 blank rows before the frame.
        let box_h = dialog_lines(&req).len() + 2;
        let top = (12 - box_h) / 2;
        assert_eq!(
            out.iter()
                .position(|s| s.text.trim_start().starts_with("┌")),
            Some(top)
        );
        // Horizontally centered: the top border has margin on both sides.
        let bar = out[top].text.clone();
        assert!(bar.trim_start().starts_with("┌"));
        assert!(bar.trim_end().ends_with("┐"));
        assert!(out[top + box_h - 1].text.trim_start().starts_with("└"));
    }

    #[test]
    fn dialog_box_returns_none_for_very_narrow_viewports() {
        let req = select_req();
        assert_eq!(dialog_box(&palette(), &req, 4, 10), None);
    }

    // ---- modal prompts & input (step 6) ----

    #[test]
    fn modal_dispatch_maps_dialog_replies_and_cancel() {
        // select: 1-based option numbers map through reply_from_input.
        let req = select_req();
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "1"),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Value(
                "read file".to_string()
            )))
        );
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "2"),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Value(
                "abort".to_string()
            )))
        );
        // cancel: `c` and `cancel` dismiss with Cancelled.
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "c"),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Cancelled))
        );
        // Not a command and not a reply: keep the modal open.
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "9"),
            ModalDecision::Keep(ModalNote::InvalidReply)
        );
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "  "),
            ModalDecision::Keep(ModalNote::InvalidReply)
        );
    }

    #[test]
    fn modal_dispatch_maps_dialog_commands() {
        let req = select_req();
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "stop"),
            ModalDecision::Close(ModalOutcome::Stop)
        );
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "restart"),
            ModalDecision::Close(ModalOutcome::Restart)
        );
        // status is accepted: it keeps the modal open with a status note.
        assert_eq!(
            dispatch_modal_line(&Modal::Dialog(req.clone()), "status"),
            ModalDecision::Keep(ModalNote::Status)
        );
    }

    #[test]
    fn modal_dispatch_maps_ask_answers_blank_and_commands() {
        let modal = Modal::Ask("continue?".to_string());
        assert_eq!(
            dispatch_modal_line(&modal, "yes, carry on"),
            ModalDecision::Close(ModalOutcome::AskAnswer(Some("yes, carry on".to_string())))
        );
        // blank / whitespace only: no answer — the run stops.
        assert_eq!(
            dispatch_modal_line(&modal, "  "),
            ModalDecision::Close(ModalOutcome::AskAnswer(None))
        );
        assert_eq!(
            dispatch_modal_line(&modal, "stop"),
            ModalDecision::Close(ModalOutcome::Stop)
        );
        assert_eq!(
            dispatch_modal_line(&modal, "status"),
            ModalDecision::Keep(ModalNote::Status)
        );
        // Line-mode parity: at an ASK prompt `restart` is the answer, not
        // a command (the ASK loop only commands stop/status).
        assert_eq!(
            dispatch_modal_line(&modal, "restart"),
            ModalDecision::Close(ModalOutcome::AskAnswer(Some("restart".to_string())))
        );
    }

    #[test]
    fn eof_outcome_cancels_a_dialog_and_stops_an_ask() {
        assert_eq!(
            eof_outcome(&Modal::Dialog(select_req())),
            ModalOutcome::DialogReply(UiReply::Cancelled)
        );
        assert_eq!(
            eof_outcome(&Modal::Ask("why?".to_string())),
            ModalOutcome::AskAnswer(None)
        );
    }

    #[test]
    fn status_note_text_renders_the_live_worker_status() {
        let view = view_from_snapshot(&worker_snapshot(), 40, 1_090_000);
        assert_eq!(
            status_note_text(3, &view),
            "status: row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s".to_string()
        );
    }

    #[test]
    fn tui_state_modal_lifecycle_edits_notes_and_verdicts() {
        let mut state = TuiState::new();
        assert!(state.modal.is_none());
        state.open_modal(Modal::Ask("q?".to_string()));
        assert_eq!(state.modal, Some(Modal::Ask("q?".to_string())));
        state.modal_append('y');
        state.modal_append('e');
        state.modal_append('s');
        assert_eq!(state.modal_input, "yes");
        state.modal_backspace();
        assert_eq!(state.modal_input, "ye");
        // Editing clears a stale note.
        state.modal_note = Some("status: …".to_string());
        state.modal_append('x');
        assert_eq!(state.modal_note, None);
        // Backspace on an empty line is a no-op.
        state.modal_input = String::new();
        state.modal_backspace();
        assert_eq!(state.modal_input, "");
        // Closing records the verdict and clears input/note.
        state.close_modal(ModalOutcome::AskAnswer(Some("yex".to_string())));
        assert!(state.modal.is_none());
        assert_eq!(state.modal_input, "");
        assert_eq!(state.modal_note, None);
        assert_eq!(
            state.modal_outcome,
            Some(ModalOutcome::AskAnswer(Some("yex".to_string())))
        );
        // A fresh open discards the stale verdict.
        state.open_modal(Modal::Ask("next?".to_string()));
        assert_eq!(state.modal_outcome, None);
        assert_eq!(state.modal_input, "");
    }

    #[test]
    fn modal_box_frames_a_dialog_with_note_and_input_rows() {
        let req = select_req();
        let out = modal_box(&palette(), &Modal::Dialog(req.clone()), "2", None, 40, 16)
            .expect("a dialog modal fits in 40×16");
        assert_eq!(out.len(), 16, "the modal covers the viewport");
        for styled in &out {
            assert_eq!(styled.text.chars().count(), 40);
        }
        let mut text = String::new();
        for styled in out.iter() {
            text.push_str(styled.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("── pick ──"));
        assert!(text.contains("1. read file"));
        assert!(text.contains("select> 2▌"));
        // The disabled note row stays empty (dim) until a note is set.
        assert!(
            out.iter().any(|l| l.fg == palette().dim),
            "a dim note row is reserved"
        );
    }

    #[test]
    fn modal_box_frames_an_ask_question_with_the_answer_input_row() {
        let out = modal_box(
            &palette(),
            &Modal::Ask("continue?".to_string()),
            "",
            None,
            30,
            10,
        )
        .expect("an ask modal fits in 30×10");
        assert_eq!(out.len(), 10);
        let mut text = String::new();
        for styled in out.iter() {
            text.push_str(styled.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("── worker question ──"));
        assert!(text.contains("continue?"));
        assert!(text.contains("answer> ▌"));
    }

    #[test]
    fn modal_box_returns_none_for_very_narrow_viewports() {
        assert_eq!(
            modal_box(&palette(), &Modal::Ask("q?".to_string()), "", None, 4, 10),
            None
        );
    }

    #[test]
    fn modal_box_never_exceeds_the_given_height() {
        let req = select_req();
        let out = modal_box(&palette(), &Modal::Dialog(req.clone()), "", None, 40, 3)
            .expect("a squeezed modal still draws");
        assert!(out.len() <= 3);
    }

    #[test]
    fn compose_frame_overlays_the_modal_and_keeps_the_footer() {
        let mut state = TuiState::new();
        state.set_plan(
            3,
            12,
            "Crate skeleton".to_string(),
            Some("s.md".to_string()),
        );
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
        state.push_banner("row 3: spawned agent 7".to_string());
        state.open_modal(Modal::Dialog(select_req()));
        state.modal_append('1');
        let frame = compose_frame(&palette(), &state, 100, 24);
        assert_eq!(frame.len(), 24);
        for line in frame.iter() {
            assert_eq!(line.text.chars().count(), 100);
        }
        assert!(frame[0].text.contains("step 3/12 · Crate skeleton"));
        let mut text = String::new();
        for line in frame.iter() {
            text.push_str(line.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("select> 1▌"), "the modal input line is drawn");
        // The footer is still the last row — a modal never occludes it.
        let footer = frame.last().cloned().expect("footer");
        assert!(footer.text.contains("row 3/agent 7"));
        // The trace content is covered by the overlay while the modal is
        // open (blank rows around the centered box).
        assert!(!text.contains("spawned agent 7"));
    }

    #[test]
    fn apply_modal_decision_status_and_invalid_keep_the_modal_open() {
        let mut state = TuiState::new();
        state.set_plan(3, 12, "unit".to_string(), None);
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 12, 1_090_000));
        state.open_modal(Modal::Dialog(select_req()));

        apply_modal_decision(&mut state, ModalDecision::Keep(ModalNote::Status));
        assert!(state.modal.is_some(), "status keeps the modal open");
        let note = state.modal_note.clone().expect("a status note was set");
        assert_eq!(
            note,
            "status: row 3 · agent 7 · turns 4/12 · ctx 61% · 1m30s"
        );
        // A later plain key clears the note (modal_append drops it).
        state.modal_append('x');
        assert_eq!(state.modal_note, None);

        apply_modal_decision(&mut state, ModalDecision::Keep(ModalNote::InvalidReply));
        assert!(
            state.modal.is_some(),
            "an invalid reply keeps the modal open"
        );
        let note = state
            .modal_note
            .clone()
            .expect("an invalid-reply hint was set");
        assert!(note.contains("invalid reply"));

        // A verdict still closes: it records the outcome and clears the
        // note (the same `c`-typed-in-line-mode result, Cancelled).
        apply_modal_decision(
            &mut state,
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Cancelled)),
        );
        assert!(state.modal.is_none());
        assert_eq!(state.modal_note, None);
        assert_eq!(
            state.modal_outcome,
            Some(ModalOutcome::DialogReply(UiReply::Cancelled)),
        );
    }

    // ---- shared state & frame composition (step 5) ----

    /// A snapshot with every step-2 stat populated.
    fn worker_snapshot() -> WorkerSnapshot {
        WorkerSnapshot {
            id: 7,
            text: "assembled".to_string(),
            tool_uses: 3,
            turn_count: 4,
            compaction_count: 0,
            context_percent: Some(61.5),
            transcript: None,
            cost: Some(0.0451),
            tokens: Some(Tokens {
                input: 50000,
                output: 10000,
                cache_read: 40000,
                cache_write: 5000,
                total: 59_300,
            }),
            context_window: Some(200_000),
            started_at: 1_000_000,
        }
    }

    #[test]
    fn tui_state_ring_evicts_oldest_past_capacity_and_keeps_kinds() {
        let mut state = TuiState::new();
        let mut n: usize = 0;
        while n < TUI_RING_CAPACITY + 5 {
            state.push_line(TuiLine {
                kind: LineKind::Banner,
                text: format!("line {n}"),
            });
            n += 1;
        }
        assert_eq!(state.ring.len(), TUI_RING_CAPACITY);
        assert_eq!(state.ring[0].text, "line 5", "5 evicted, 6 kept");
        assert_eq!(
            state.ring.last().cloned().expect("ring not empty").text,
            format!("line {}", TUI_RING_CAPACITY + 4)
        );
        assert_eq!(state.ring[0].kind, LineKind::Banner);
    }

    #[test]
    fn tui_state_tracks_plan_meta_and_worker_view() {
        let mut state = TuiState::new();
        state.set_plan(
            5,
            10,
            "Parser".to_string(),
            Some("docs/research/interface-design.md".to_string()),
        );
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
        assert_eq!(state.row, 5);
        assert_eq!(state.total, 10);
        assert_eq!(state.unit, "Parser");
        assert_eq!(
            state.source,
            Some("docs/research/interface-design.md".to_string())
        );
        // Footer view bytes: agent, turns, context, cost, elapsed.
        assert_eq!(state.worker.agent_id, "7");
        assert_eq!(state.worker.turns, 4);
        assert_eq!(state.worker.max_turns, 40);
        assert_eq!(state.worker.context_percent, Some(61.5));
        assert_eq!(state.worker.context_tokens, Some(59_300));
        assert_eq!(state.worker.context_window, Some(200_000));
        assert_eq!(state.worker.cost, Some(0.0451));
        assert_eq!(state.worker.elapsed_ms, 90_000);
    }

    #[test]
    fn view_from_snapshot_handles_absent_stats_and_clock_skew() {
        let mut snap = worker_snapshot();
        snap.cost = None;
        snap.tokens = None;
        snap.context_window = None;
        snap.context_percent = None;
        // The clock reads before spawn (skew): elapsed clamps to 0.
        let view = view_from_snapshot(&snap, 40, 999_999);
        assert_eq!(view.cost, None);
        assert_eq!(view.context_tokens, None);
        assert_eq!(view.context_window, None);
        assert_eq!(view.context_percent, None);
        assert_eq!(view.elapsed_ms, 0);
    }

    #[test]
    fn compose_frame_builds_header_viewport_and_footer() {
        let mut state = TuiState::new();
        state.set_plan(
            3,
            12,
            "Crate skeleton".to_string(),
            Some("s.md".to_string()),
        );
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
        state.push_banner("row 3: spawned agent 7".to_string());
        state.push_line(TuiLine {
            kind: LineKind::Thinking,
            text: "so the compiler…".to_string(),
        });
        state.push_line(TuiLine {
            kind: LineKind::Tool,
            text: "tool: write".to_string(),
        });

        let frame = compose_frame(&palette(), &state, 100, 24);
        for line in frame.iter() {
            assert_eq!(
                line.text.chars().count(),
                100,
                "every frame row is exact width"
            );
        }
        // Header rows 1-2: the title bar + the source line.
        assert!(frame[0].text.starts_with("┌┤ "));
        assert!(frame[0].text.contains("step 3/12 · Crate skeleton"));
        assert!(frame[1].text.contains("source: s.md"));
        // The whole frame is 2 header + wrapped trace + 1 footer rows.
        assert!(frame.len() >= 5);
        // The footer is the last drawn row, with the live stats + hints.
        let footer = frame.last().cloned().expect("footer");
        assert!(footer.text.contains("row 3/agent 7"));
        assert!(footer.text.trim_end().ends_with(" stop / restart / status"));
        // The newest tagged line is bottom-anchored and keeps its style.
        let tool = frame
            .iter()
            .find(|l| l.text.contains("tool: write"))
            .expect("tool row");
        assert_eq!(tool.fg, palette().tool_title);
        assert_eq!(tool.bg, Some(palette().tool_pending_bg));
    }

    #[test]
    fn compose_frame_bottom_anchor_shows_the_newest_viewport_lines() {
        let mut state = TuiState::new();
        state.set_plan(1, 1, "unit".to_string(), None);
        let mut n: usize = 0;
        while n < 30 {
            state.push_banner(format!("row {n}"));
            n += 1;
        }
        let frame = compose_frame(&palette(), &state, 80, 10);
        // 2 header + 7 viewport + 1 footer; the viewport holds the newest
        // seven lines (rows 23..29), so the first viewport row is 23.
        assert_eq!(frame.len(), 10);
        assert!(frame[2].text.contains("row 23"));
        assert!(frame[8].text.contains("row 29"));
        assert!(frame[9].text.contains("row 1/agent —"));
    }

    #[test]
    fn compose_frame_returns_empty_when_the_fixed_regions_cannot_fit() {
        let state = TuiState::new();
        assert_eq!(
            compose_frame(&palette(), &state, 80, 3).len(),
            0,
            "needs at least 4 rows"
        );
        assert_eq!(
            compose_frame(&palette(), &state, 2, 24).len(),
            0,
            "needs at least 3 columns"
        );
    }

    #[test]
    fn tui_state_push_banner_tags_report_lines() {
        let mut state = TuiState::new();
        state.push_banner("row 3: spawned agent 7".to_string());
        assert_eq!(state.ring.len(), 1);
        assert_eq!(state.ring[0].kind, LineKind::Banner);
        assert_eq!(state.ring[0].text, "row 3: spawned agent 7");
    }

    // ---- property-based ----

    proptest! {
        /// Greedy wrapping is an upper bound: every wrapped row fits.
        #[test]
        fn wrapped_lines_never_exceed_the_viewport_width(text in "[a-z ]{0,60}", width in "[0-9]{1,2}") {
            let w = width.parse::<usize>().unwrap_or(0);
            for line in wrap_text(text.as_str(), w) {
                prop_assert!(line.chars().count() <= w, "wrapped line exceeds {w}");
            }
        }
    }
}
