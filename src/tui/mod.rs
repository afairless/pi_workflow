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
//! ┌┤ pi-plan · step 3/12 · Crate skeleton ├──────────  ← header (2 rows)
//! │ source: docs/research/interface-design.md
//! │ thinking: so the compiler (gray block)                    │
//! │     ...trace viewport (wrapped to width)...         │ ← trace (scrolls
//! │                                                     │    inside its rows)
//! │ $0.0451 · ctx 61% (59.3k/200k) · turns 4/40 · 1m30s │ ← footer (persistent)
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! The trace scrolls exclusively inside rows `3..H-2` via a scroll region
//! (wired in step 5), so the header/footer never occlude streaming text.
//! [`modal_box`] pins a bottom-anchored prompt above the footer with the
//! newest trace still visible above it; [`trace_lines`] returns the bare
//! (unframed) viewport so the render loop owns final placement.
//!
//! Operator keys: `^C` arms a graceful stop (the worker runs to settle,
//! rows report stopped); `^D` is the **kill switch** — it arms the kill
//! flag ([`apply_ctrl_d`]); the binary's `kill_watcher` SIGKILLs every
//! supervise-spawned worker process group, the TUI unwinds, the final
//! report prints, and the process exits 2. During a modal, `^D` closes it
//! with `Stop` so the awaiting dialog/ASK flow never deadlocks; line mode
//! keeps `^D` as EOF unchanged. During a modal dialog, ↑/↓ move the row
//! highlight and Enter submits the highlighted row (the first row is
//! pre-highlighted on open, so a bare Enter picks it; ↑ from the first
//! row wraps to the last); a typed reply always beats the highlight, and
//! line mode stays typed-only.
//!
//! Permission-dialog rendering lives here and in `ui.rs`. The focused
//! `modal_box` row is drawn as `accent` bold text on the panel fill (not a
//! full-width amber band), and every content row word-wraps to the box
//! width via `wrap_text` instead of truncating, so a long `command : …`
//! fact never ellipsizes its target path. `modal_dialog_rows` accepts the
//! pending tool call (from `TuiState.modal_tool`, threaded through
//! `compose_frame`) so the box shows the pending `tool:`/`$ …` context the
//! same way line mode does. Between rows the header/footer stop showing
//! statistics for completed workers: `WorkerView.live` plus the
//! row-terminal hook flip the displayed view not-live, and
//! `format_footer_line` renders the supervisor idle line with row context
//! instead.

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

use crate::rpc::{ExtensionUiRequest, PendingTool, UiMethod, UiReply};
use crate::theme::{Color, Palette};
use crate::ui::{
    DialogRow, FooterStats, LineCommand, LineKind, TuiLine, dialog_item_count, dialog_prompt_label,
    format_footer_line, format_header_line, format_idle_footer_line, format_status_line,
    item_reply, kind_glyph, line_command, modal_dialog_rows, reply_from_input, style_for_kind,
    truncate_with_ellipsis,
};

#[cfg(test)]
use crate::ui::dialog_lines;
use crate::worker::WorkerSnapshot;
/// One fully styled frame line: text plus the palette colors to apply.
/// `bold` (SGR 1) is drawn after the color codes; the per-row `\e[0m`
/// reset `render_task` emits after every row clears it, so no extra
/// reset handling is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledLine {
    pub text: String,
    pub fg: Color,
    pub bg: Option<Color>,
    pub bold: bool,
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

/// The two header rows: the title bar (`┌┤ … ├──┐`) and the context line
/// (`│ source: …` plus, while a worker is live, its status line). Both
/// are exactly `width` characters.
pub fn header_lines(
    palette: &Palette,
    row: u64,
    total: u64,
    unit: &str,
    source: Option<&str>,
    context: Option<&str>,
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
    // Line 2: the plan source plus the live worker context when there is
    // one (step 7) — the info the status line used to spam to stderr.
    let line2 = match source {
        Some(src) => match context {
            Some(ctx) => format!("│ source: {src} · {ctx}"),
            None => format!("│ source: {src}"),
        },
        None => match context {
            Some(ctx) => format!("│ {ctx}"),
            None => "│".to_string(),
        },
    };
    vec![
        StyledLine {
            text: line1,
            fg: palette.border_accent,
            bg: None,
            bold: false,
        },
        StyledLine {
            text: pad_line_to(line2.as_str(), width),
            fg: palette.muted,
            bg: None,
            bold: false,
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
        bold: false,
    }]
}

/// Wrap one logical line into styled rows, emitting a blank row when the
/// line is empty so `\n` paragraph gaps and trailing newlines show.
fn push_wrapped_line(
    out: &mut Vec<StyledLine>,
    palette: &Palette,
    kind: LineKind,
    text: &str,
    width: usize,
) {
    let (fg, bg) = style_for_kind(palette, kind);
    let glyph = kind_glyph(kind);
    let body = format!("{glyph}{}", text);
    let mut emitted = false;
    for chunk in wrap_text(body.as_str(), width) {
        out.push(StyledLine {
            text: chunk.to_string(),
            fg,
            bg,
            bold: false,
        });
        emitted = true;
    }
    if !emitted {
        out.push(StyledLine {
            text: String::new(),
            fg,
            bg,
            bold: false,
        });
    }
}

/// The trace viewport: `lines` (oldest first) word-wrapped to `width`,
/// styled by kind, returning the `height` rows ending `offset` rows from
/// the bottom (0 = the newest screen). Success/error tool rows carry
/// their outcome glyphs. Wrapped rows never exceed `width`. The open
/// stream line renders as a virtual last logical line; empty logical
/// lines emit a blank row so paragraph gaps and trailing newlines show.
pub fn trace_lines(
    palette: &Palette,
    lines: &[TuiLine],
    trailing: Option<&StreamLine>,
    width: usize,
    height: usize,
    offset: usize,
) -> Vec<StyledLine> {
    let mut wrapped: Vec<StyledLine> = Vec::new();
    for line in lines.iter() {
        push_wrapped_line(&mut wrapped, palette, line.kind, line.text.as_str(), width);
    }
    if let Some(open) = trailing {
        push_wrapped_line(&mut wrapped, palette, open.kind, open.text.as_str(), width);
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
    /// A dialog reply, already mapped by `reply_from_input` (`c`/`cancel`
    /// produces `Cancelled`; in the TUI `^D` is the kill switch, so this
    /// keeps line-mode EOF parity via the closed-stdin arm only).
    DialogReply(UiReply),
    /// An ASK answer to carry into the fresh worker; `None` means the
    /// operator gave no answer (blank line / EOF) — the run stops.
    AskAnswer(Option<String>),
    /// `stop`, `^C`, or the `^D` kill switch — stop the run.
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

/// Move the modal highlight by `delta` rows (`+1` down, `−1` up) with
/// wraparound — down past the last wraps to the first, up past the first
/// wraps to the last (rpiv parity). `len == 0` → `None`; a `None`
/// current with items present → `Some(0)` (defensive — `open_modal`
/// always initializes a select/confirm focus). `i64` arithmetic keeps
/// the `usize` underflow on `↑` past 0 well-defined.
pub fn navigate_focus(current: Option<usize>, delta: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(pos) = current else {
        // Defensive: arrows only act on dialogs with items, which
        // `open_modal` always initializes — with items present a `None`
        // focus falls back to the first row in either direction.
        return Some(0);
    };
    if len == 1 {
        return Some(0);
    }
    let mut next = (pos as i64 + (delta as i64)) % (len as i64);
    if next < 0 {
        next += len as i64;
    }
    Some(next as usize)
}

/// The TUI modal's Enter verdict. Precedence: (1) typed input wins —
/// byte parity with [`dispatch_modal_line`], including whitespace-only
/// lines; (2) an EMPTY line with a focused dialog item submits that item
/// (a `None` reply — defensive, an out-of-range focus — falls through);
/// (3) otherwise the old path applies unchanged (an empty line on a
/// dialog without a focused item → invalid reply, an empty ASK line →
/// no answer). `focus` is strictly a TUI-mode concept:
/// [`dispatch_modal_line`] never reads it, so line mode cannot observe
/// it.
pub fn dispatch_modal_submit(modal: &Modal, input: &str, focus: Option<usize>) -> ModalDecision {
    if !input.is_empty() {
        return dispatch_modal_line(modal, input);
    }
    if let Modal::Dialog(req) = modal
        && let Some(idx) = focus
        && let Some(reply) = item_reply(req, idx)
    {
        return ModalDecision::Close(ModalOutcome::DialogReply(reply));
    }
    dispatch_modal_line(modal, "")
}

/// The focus a modal opens with: `Some(0)` when the dialog has
/// focusable rows (select → option 1, confirm → **no** — deny by
/// default, so an accidental bare Enter can never grant), `None` for
/// input/editor/ask (nothing to highlight).
fn modal_focus_for(modal: &Modal) -> Option<usize> {
    match modal {
        Modal::Dialog(req) => {
            if req.method == UiMethod::Select || req.method == UiMethod::Confirm {
                Some(0)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Pure: the EOF (stdin closed / terminal gone) outcome for the active
/// modal — line-mode parity: a dialog is dismissed with `Cancelled`, an
/// ASK pause stops without an answer. In the TUI `^D` is the kill switch
/// ([`apply_ctrl_d`]), so this maps the closed-stdin arm only.
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
/// The modal prompt's bottom-anchored box: a bordered frame
/// (`┌─┐│└┘`, accent frame on the user-message panel fill) holding the
/// dialog/ASK content, the reserved dim note row, and the input row.
/// The box is EXACTLY `content.len() + 4` rows tall and pinned to the
/// bottom — no vertical centering, no viewport padding. When it would
/// exceed `height` it is truncated from the top, so the input row and
/// bottom border always survive and the box never exceeds the viewport.
/// Returns `None` when the width is too narrow to draw; every returned
/// row is exactly `width` characters.
/// The focused dialog row (step 8) is drawn with the `▸` marker
/// replacing its two-space item indent (same cell width) and the theme
/// accent as a full-row fill; an Ask question passes `focus: None`, so
/// it never carries a highlight.
/// The render options for [`modal_box`]: the operator's input line, the
/// dim note, the focused dialog row, and the pending-tool context
/// captured when the modal opened (step 3). Bundled so `modal_box`
/// stays under clippy's argument ceiling.
pub struct ModalBoxOpts<'a> {
    pub input: &'a str,
    pub note: Option<&'a str>,
    pub focus: Option<usize>,
    pub tool: Option<&'a PendingTool>,
}

/// Render the modal prompt box: a bordered panel with inline note and
/// input rows, bottom-anchored and clipped into the viewport.
pub fn modal_box(
    palette: &Palette,
    modal: &Modal,
    opts: &ModalBoxOpts<'_>,
    width: usize,
    height: usize,
) -> Option<Vec<StyledLine>> {
    if width < 5 {
        return None;
    }
    let (content, prompt): (Vec<DialogRow>, String) = match modal {
        Modal::Dialog(req) => (
            modal_dialog_rows(req, opts.focus, opts.tool),
            dialog_prompt_label(req),
        ),
        Modal::Ask(question) => (
            vec![
                DialogRow {
                    text: "── worker question ──".to_string(),
                    focused: false,
                },
                DialogRow {
                    text: question.trim().to_string(),
                    focused: false,
                },
            ],
            "answer>".to_string(),
        ),
    };
    let mut inner: usize = 1;
    for row in content.iter() {
        inner = inner.max(row.text.chars().count() + 2);
    }
    inner = inner.min(width - 2);
    // Content + note row + input row + top/bottom borders. The box is
    // exactly this tall and bottom-anchored, never padded to `height`.
    let left = (width - inner - 2) / 2;
    let hpad = fill_with(' ', left);
    let frame_fg = palette.border_accent;
    let fill = Some(palette.user_message_bg);

    let mut out: Vec<StyledLine> = Vec::new();
    out.push(StyledLine {
        text: pad_right(format!("{hpad}┌{}┐", fill_with('─', inner)), width),
        fg: frame_fg,
        bg: fill,
        bold: false,
    });
    // The focused content row's first wrapped chunk's box line (top
    // border is line 0), for the overflow clip below. `box_line` counts
    // one line per wrapped chunk.
    let mut focus_line: Option<usize> = None;
    let mut box_line: usize = 1; // below the top border (line 0)
    for row in content.iter() {
        // Every content row is word-wrapped at the text-cell width
        // `inner - 1` (the `│ ` prefix and the `│` suffix take the other
        // three cells of `inner + 2`), so nothing ellipsizes: a long
        // `command : rm -rf …` line never loses its target path, and a
        // long option wraps with the `▸` marker on its first chunk only.
        // Continuation chunks are plain render-only rows.
        let body = if row.focused {
            format!("▸ {}", row.text.trim_start())
        } else {
            row.text.to_string()
        };
        let mut chunks = wrap_text(body.as_str(), inner - 1);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        for (ci, chunk) in chunks.iter().enumerate() {
            out.push(StyledLine {
                text: pad_right(
                    format!("{hpad}│ {}│", pad_line_to(chunk.as_str(), inner - 1)),
                    width,
                ),
                fg: if row.focused {
                    palette.accent
                } else {
                    palette.text
                },
                bg: fill,
                bold: row.focused && ci == 0,
            });
        }
        if row.focused {
            focus_line = Some(box_line);
        }
        box_line += chunks.len();
    }
    // Reserved note row (dim) — always present so the box never jumps.
    let note_text: &str = opts.note.unwrap_or("");
    out.push(StyledLine {
        text: pad_right(
            format!("{hpad}│ {}│", pad_line_to(note_text, inner - 1)),
            width,
        ),
        fg: palette.dim,
        bg: fill,
        bold: false,
    });
    // The input line, with a block marking the typing position.
    let input_text = format!("{prompt} {}▌", opts.input);
    out.push(StyledLine {
        text: pad_right(
            format!("{hpad}│ {}│", pad_line_to(input_text.as_str(), inner - 1)),
            width,
        ),
        fg: palette.text,
        bg: fill,
        bold: false,
    });
    out.push(StyledLine {
        text: pad_right(format!("{hpad}└{}┘", fill_with('─', inner)), width),
        fg: frame_fg,
        bg: fill,
        bold: false,
    });
    Some(clip_modal_rows(out, height, focus_line))
}

fn clip_modal_rows(
    rows: Vec<StyledLine>,
    height: usize,
    focus_line: Option<usize>,
) -> Vec<StyledLine> {
    if rows.len() <= height {
        return rows;
    }
    let mut drop = rows.len() - height;
    // The box may only cut top rows, so the note row, the input row, and
    // the bottom border always survive (the step-6 truncation,
    // unchanged). The focused content row — `focus_line`, its first
    // wrapped chunk's box line as mapped by `modal_box` — is kept
    // whenever the bottom-anchored window can include it, i.e. whenever
    // its line lands inside the kept tail (`rows.len() - focus` fits in
    // `height`); sliding the window up to the focus is then a no-op.
    // A focus above that window cannot fit together with the pinned
    // tail, so the same truncation applies there (flagged and accepted
    // as-is by the plan review). `None` (an Ask question or an itemless
    // dialog) is the plain truncation.
    if let Some(focus) = focus_line
        && rows.len() - focus <= height
        && focus < drop
    {
        drop = focus;
    }
    rows.iter().skip(drop).cloned().collect::<Vec<_>>()
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

/// The verdict on one raw byte fed to [`EscapeCollector::feed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collect {
    /// One COMPLETE escape sequence, ready for [`parse_escape_nav`] (or
    /// to be dropped whole).
    Sequence(Vec<u8>),
    /// A non-`ESC` byte with nothing pending — handle as today
    /// ([`decode_key`]).
    Plain(u8),
    /// More bytes are needed to complete the pending sequence.
    Pending,
}

/// Assembles `ESC`-introduced byte sequences a whole at a time — the
/// fix for the old inline collector, which cleared at ANY byte in
/// `0x40..=0x7e` and so never assembled an arrow: `[`/`O` are in that
/// range, so `ESC [ A` cleared at `[` and the trailing `A` leaked into
/// the input line as a typed char. Pure and unit-tested; the input task
/// feeds every raw byte here first. The collector persists across
/// `read()`s, so a sequence split across polls still assembles; the
/// 16-byte cap (unchanged) completes and returns a pathological
/// sequence whole, so junk is dropped by [`parse_escape_nav`] rather
/// than typed. A pending sequence can never hang: a second `ESC`
/// completes the current one and begins the next buffer; any other
/// control byte completes it and is dropped with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapeCollector {
    /// The sequence currently being assembled (`ESC` plus what follows).
    pending: Vec<u8>,
}

impl EscapeCollector {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Feed one raw byte; exactly one verdict per byte.
    pub fn feed(&mut self, byte: u8) -> Collect {
        if self.pending.is_empty() {
            if byte == 0x1b {
                self.pending.push(byte);
                return Collect::Pending;
            }
            return Collect::Plain(byte);
        }
        if byte == 0x1b {
            // A second `ESC` terminates the partial sequence (returned
            // so it can be dropped whole) and begins the next one.
            let completed = self.pending.clone();
            self.pending = vec![byte];
            return Collect::Sequence(completed);
        }
        if self.pending.len() == 1 && (byte == 0x5b || byte == 0x4f) {
            // `ESC [ …` / `ESC O …`: the introducer byte must NEVER
            // complete early (the old collector's bug — `[`/`O` are in
            // the final-byte range) — hold it and keep collecting.
            self.pending.push(byte);
            return self.capped();
        }
        if (0x40..=0x7e).contains(&byte) {
            // First final byte: the sequence is complete.
            self.pending.push(byte);
            let completed = self.pending.clone();
            self.pending = Vec::new();
            return Collect::Sequence(completed);
        }
        if (0x20..=0x3f).contains(&byte) {
            // Parameter/intermediate bytes (digits, `;`, …): held.
            self.pending.push(byte);
            return self.capped();
        }
        // Any other byte while pending (control bytes): the malformed
        // sequence completes and is dropped whole; the offending byte is
        // dropped with it — the buffer can never hang.
        let completed = self.pending.clone();
        self.pending = Vec::new();
        Collect::Sequence(completed)
    }

    /// Complete the pending sequence when it reached the 16-byte cap, so
    /// pathological input is dropped whole rather than growing forever.
    fn capped(&mut self) -> Collect {
        if self.pending.len() >= 16 {
            let completed = self.pending.clone();
            self.pending = Vec::new();
            Collect::Sequence(completed)
        } else {
            Collect::Pending
        }
    }
}

impl Default for EscapeCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// One navigable arrow key decoded from a complete escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    Up,
    Down,
}

/// Pure: map one complete escape sequence to an arrow key. Both standard
/// encodings are accepted: `ESC [ A` / `ESC O A` → up, `ESC [ B` /
/// `ESC O B` → down. Everything else → `None`, so the caller drops it
/// whole (DSR size-report replies, other CSI/SS3 sequences — nothing
/// leaks into the input line).
pub fn parse_escape_nav(seq: &[u8]) -> Option<NavKey> {
    if seq.len() == 3 && seq[0] == 0x1b && (seq[1] == 0x5b || seq[1] == 0x4f) {
        match seq[2] {
            0x41 => Some(NavKey::Up),
            0x42 => Some(NavKey::Down),
            _ => None,
        }
    } else {
        None
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
    /// The in-flight tool call gating the worker's permission dialog,
    /// when one is pending (`None` outside the gate or in line mode).
    pub pending_tool: Option<PendingTool>,
    /// False once the worker terminated: the footer and the header's
    /// status context drop the worker's stats and show the supervisor
    /// idle line instead (step 4).
    pub live: bool,
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
        pending_tool: snap.pending_tool.as_ref().cloned(),
        live: snap.terminal.is_none(),
    }
}

/// The tagged trace ring capacity in the TUI: raised from line mode's 240
/// to ~1000 with the full-frame re-render cost in mind (plan risks).
pub const TUI_RING_CAPACITY: usize = 1000;

/// Maximum characters one open stream line may hold before it is flushed
/// truncated with a banner note (plan risks: bounds ring memory against
/// `\n`-less blob deltas).
pub const STREAM_LINE_CAP: usize = 10_000;

/// One open (unfinished) stream line: token deltas of one [`LineKind`]
/// append to `text` until a `\n` or a kind change (thinking ↔ text)
/// flushes it into the ring. The stream is the flowing token tail; the
/// frame renders it as the virtual last trace line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    pub kind: LineKind,
    pub text: String,
}

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
    /// The open flowing-text line (fix 2): deltas append here until a
    /// `\n` or kind change closes it into the ring. `None` while no
    /// message stream is in flight.
    pub stream: Option<StreamLine>,
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
    /// The focused dialog row (step 8): the index into the open dialog's
    /// focusable rows that a bare Enter submits — the ↑/↓ highlight.
    /// Strictly TUI-mode: `open_modal` initializes it (`Some(0)` for
    /// Select/Confirm, `None` otherwise), `close_modal` resets it, and
    /// the typed-reply paths never read it (line mode cannot observe
    /// it).
    pub modal_focus: Option<usize>,
    /// The pending tool call captured when the modal opened (step 3):
    /// [`compose_frame`] threads it into the dialog's context rows.
    /// Cleared by [`TuiState::close_modal`]; Ask questions never set it.
    pub modal_tool: Option<PendingTool>,
    /// Row number → logical unit for every row of the FULL plan (seeded
    /// once from the same `TodoPlan` the tail holds), so the idle footer
    /// can name the next row's unit.
    pub plan_units: Vec<(u64, String)>,
    /// The last row terminal observed by the row-terminal hook: row
    /// number → terminal-kind label (`completed`/`failed`). Drives the
    /// idle footer; set by [`TuiState::note_row_terminal`].
    pub last_terminal: Option<(u64, String)>,
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
                pending_tool: None,
                live: true,
            },
            ring: Vec::new(),
            stream: None,
            scroll_offset: 0,
            modal: None,
            modal_input: String::new(),
            modal_note: None,
            modal_focus: None,
            modal_tool: None,
            plan_units: Vec::new(),
            last_terminal: None,
            modal_outcome: None,
        }
    }

    /// Set the header's plan meta — called once per row by the supervise
    /// loop (via the spawn seam). `row` is the 1-based position in the
    /// FULL table and `total` its length, so `--row n` runs still show
    /// where they sit in the whole plan.
    pub fn set_plan(&mut self, row: u64, total: u64, unit: String, source: Option<String>) {
        self.flush_stream();
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

    /// Seed the row → logical-unit map once (first call wins); the tail
    /// supplies the whole `TodoPlan`, so the idle footer can name the
    /// next row's unit.
    pub fn seed_plan_units(&mut self, units: Vec<(u64, String)>) {
        if self.plan_units.is_empty() && !units.is_empty() {
            self.plan_units = units;
        }
    }

    /// Record a row terminal (the `on_row_terminal` hook): store the
    /// row → terminal-kind label and flip the displayed worker view
    /// not-live. The flip lives HERE, not in the snapshot path: the
    /// worker's event channel closes ~250 ms after the terminal, far
    /// short of the quiet cadence, so no snapshot can ever carry it (and
    /// `tui_update_view` skips terminal snapshots anyway).
    pub fn note_row_terminal(&mut self, row: u64, label: &str) {
        self.last_terminal = Some((row, label.to_string()));
        self.worker.live = false;
    }

    /// Append one tagged line, evicting the oldest past the capacity (the
    /// same eviction contract as the line mode's [`TraceRing`]-analog).
    /// The open stream line is flushed first so tool rows, turn
    /// separators, and banners never interleave mid-stream.
    pub fn push_line(&mut self, line: TuiLine) {
        self.flush_stream();
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

    /// Characters currently held by the open stream line (`0` when none
    /// is open).
    fn stream_open_len(&self) -> usize {
        if let Some(open) = &self.stream {
            open.text.chars().count()
        } else {
            0
        }
    }

    /// Close the open stream line into the ring (no-op when none open).
    fn flush_stream(&mut self) {
        let (kind, text) = {
            let Some(open) = &self.stream else {
                return;
            };
            (open.kind, open.text.clone())
        };
        self.stream = None;
        self.ring.push(TuiLine { kind, text });
        while self.ring.len() > TUI_RING_CAPACITY {
            self.ring.remove(0);
        }
    }

    /// Open a fresh empty stream line of `kind`.
    fn open_stream(&mut self, kind: LineKind) {
        self.stream = Some(StreamLine {
            kind,
            text: String::new(),
        });
    }

    /// Append one character to the open stream line.
    fn stream_append_char(&mut self, c: char) {
        if let Some(open) = &mut self.stream {
            open.text.push(c);
        }
    }

    /// Append one kind-tagged delta chunk to the flowing text stream
    /// (fix 2). A kind change (thinking ↔ text) closes the current line
    /// first so the two never interleave — the message text always starts
    /// on a fresh line after a thinking block. Embedded `\n` characters
    /// close lines; an empty segment closes an empty line so paragraph
    /// gaps render as blank rows. The final segment stays open for the
    /// next chunk. The open line is capped at [`STREAM_LINE_CAP`] — past
    /// it the line is flushed truncated with a banner note and the stream
    /// reopens empty.
    pub fn append_stream(&mut self, kind: LineKind, text: &str) {
        // A kind change closes the current line before anything else.
        let mut flipped = false;
        if let Some(open) = &self.stream {
            flipped = open.kind != kind;
        }
        if flipped {
            self.flush_stream();
        }
        if self.stream.is_none() {
            self.open_stream(kind);
        }
        let normalized = text.replace("\r\n", "\n");
        let segments: Vec<&str> = normalized.split('\n').collect::<Vec<_>>();
        let mut i: usize = 0;
        while i < segments.len() {
            let is_last = i + 1 == segments.len();
            let mut chars: Vec<char> = Vec::new();
            for c in segments[i].chars() {
                chars.push(c);
            }
            // Append in bounded passes: once the open line reaches the
            // cap, close it truncated, note the loss, and reopen empty so
            // the remainder keeps flowing.
            let mut room = STREAM_LINE_CAP.saturating_sub(self.stream_open_len());
            let mut j: usize = 0;
            while j < chars.len() {
                if room == 0 {
                    self.flush_stream();
                    self.push_banner("…truncated".to_string());
                    self.open_stream(kind);
                    room = STREAM_LINE_CAP;
                }
                self.stream_append_char(chars[j]);
                room -= 1;
                j += 1;
            }
            if !is_last {
                // `\n` closes the line; reopen so the next segment has
                // an open line to fill.
                self.flush_stream();
                self.open_stream(kind);
            }
            i += 1;
        }
    }

    /// Open a modal prompt, discarding any stale input/note/outcome.
    /// `tool` is the pending tool call captured at dialog time (the
    /// gate's `tool_execution_start` has already arrived by then), or
    /// `None` for Ask questions and itemless dialogs.
    pub fn open_modal(&mut self, modal: Modal, tool: Option<&PendingTool>) {
        self.modal_focus = modal_focus_for(&modal);
        self.modal = Some(modal);
        self.modal_input = String::new();
        self.modal_note = None;
        self.modal_outcome = None;
        self.modal_tool = tool.cloned();
    }

    /// Close the modal with a verdict for the awaiting flow.
    pub fn close_modal(&mut self, outcome: ModalOutcome) {
        self.modal = None;
        self.modal_input = String::new();
        self.modal_note = None;
        self.modal_focus = None;
        self.modal_tool = None;
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
    // Header line 2's live context: the worker status line (step 7) —
    // the numbers the status line used to spam to stderr, now part of
    // the persistent header while a worker is live. A completed worker
    // drops the stats too: during the between-row window the header
    // keeps only the step banner.
    let status: Option<String> = if state.worker.agent_id.is_empty() || !state.worker.live {
        None
    } else {
        let agent = Some(state.worker.agent_id.as_str());
        Some(format_status_line(
            state.row.to_string().as_str(),
            agent,
            state.worker.turns,
            state.worker.max_turns,
            state.worker.context_percent,
            state.worker.elapsed_ms,
        ))
    };
    let mut out: Vec<StyledLine> = Vec::new();
    for line in header_lines(
        palette,
        state.row,
        state.total,
        state.unit.as_str(),
        state.source.as_deref(),
        status.as_deref(),
        width,
    ) {
        out.push(line);
    }
    // The viewport: the trace ring (with the open stream line as its
    // tail), or — while a prompt is open — the newest trace rows stacked
    // directly above the prompt's bottom-anchored box (the footer below
    // stays, so the persistent readout is never occluded). Wrap yields
    // ≤ width rows; pad so a shorter row erases the previous frame's
    // content (exact-width redraw).
    let viewport: Vec<StyledLine> = match &state.modal {
        Some(modal) => {
            let vh = viewport_height_for(height);
            let box_rows = modal_box(
                palette,
                modal,
                &ModalBoxOpts {
                    input: state.modal_input.as_str(),
                    note: state.modal_note.as_deref(),
                    focus: state.modal_focus,
                    tool: state.modal_tool.as_ref(),
                },
                width,
                vh,
            )
            .unwrap_or_default();
            let box_h = box_rows.len();
            let trace_h = vh.saturating_sub(box_h);
            let mut lines: Vec<StyledLine> = Vec::new();
            // The newest trace (including the open stream line) stays
            // visible directly above the prompt box.
            for line in trace_lines(
                palette,
                &state.ring[..],
                state.stream.as_ref(),
                width,
                trace_h,
                state.scroll_offset,
            ) {
                lines.push(StyledLine {
                    text: pad_line_to(line.text.as_str(), width),
                    fg: line.fg,
                    bg: line.bg,
                    bold: line.bold,
                });
            }
            // Blank-fill the trace allotment so the box stays pinned to
            // the very bottom of the viewport (directly above the
            // footer) even when the trace is short.
            while lines.len() < trace_h {
                lines.push(StyledLine {
                    text: fill_with(' ', width),
                    fg: Color::Default,
                    bg: None,
                    bold: false,
                });
            }
            for line in box_rows.iter() {
                lines.push(line.clone());
            }
            lines
        }
        None => {
            let mut lines: Vec<StyledLine> = Vec::new();
            for line in trace_lines(
                palette,
                &state.ring[..],
                state.stream.as_ref(),
                width,
                viewport_height_for(height),
                state.scroll_offset,
            ) {
                lines.push(StyledLine {
                    text: pad_line_to(line.text.as_str(), width),
                    fg: line.fg,
                    bg: line.bg,
                    bold: line.bold,
                });
            }
            lines
        }
    };
    for line in viewport {
        out.push(line);
    }
    // The footer borrows its stats from the state; `format_footer_line`
    // runs inside this expression so the borrows end here. A completed
    // worker (or no worker yet) shows the supervisor idle line instead:
    // which row just terminated, and which row is next — with its unit
    // from the seeded plan map when known. The label is a terminal kind
    // (completed/failed), so a retried row truthfully shows the
    // preceding attempt and `next` points at the same row again.
    let footer: Vec<StyledLine> = if state.worker.live {
        let agent: Option<&str> = if state.worker.agent_id.is_empty() {
            None
        } else {
            Some(state.worker.agent_id.as_str())
        };
        footer_lines(
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
        )
    } else {
        idle_footer_lines(palette, state, width)
    };
    for line in footer {
        out.push(line);
    }
    out
}

/// The supervisor idle footer (one row): `idle · last: row N <kind> ·
/// next: row M — <unit>`, truncated to the footer width with the same
/// hints tail as the live footer. `next` advances past a completed row
/// and stays on a failed/stalled one (the retry); its unit comes from
/// the seeded plan map when known.
fn idle_footer_lines(palette: &Palette, state: &TuiState, width: usize) -> Vec<StyledLine> {
    let next_row: u64 = match &state.last_terminal {
        Some((row, label)) => {
            if *label == "completed" {
                *row + 1
            } else {
                *row
            }
        }
        None => state.row,
    };
    let next_unit: Option<&str> = state
        .plan_units
        .iter()
        .find(|(nr, _)| *nr == next_row)
        .map(|(_, unit)| unit.as_str());
    let content =
        format_idle_footer_line(state.last_terminal.as_ref().cloned(), next_row, next_unit);
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
        bold: false,
    }]
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
                state.modal_note = Some(invalid_reply_note(&state.modal));
            }
        },
    }
}

/// The invalid-reply hint for the open modal, per dialog kind: dialogs
/// with focusable rows advertise the ↑/↓ + Enter path (select also
/// lists its typed option numbers; confirm its y/n/c); input/editor
/// (and ASK) have neither option numbers nor focusable rows and keep
/// the original text.
fn invalid_reply_note(modal: &Option<Modal>) -> String {
    match modal {
        Some(Modal::Dialog(req)) => match req.method {
            UiMethod::Select => {
                "invalid reply — type an option number or use ↑/↓ + Enter (^D to dismiss)"
                    .to_string()
            }
            UiMethod::Confirm => {
                "invalid reply — type y/n/c or use ↑/↓ + Enter (^D to dismiss)".to_string()
            }
            _ => "invalid reply — try again (or ^D to dismiss)".to_string(),
        },
        _ => "invalid reply — try again (or ^D to dismiss)".to_string(),
    }
}

/// Pure: the operator's `^D` (kill switch, plan fix 3). Arms the kill
/// flag once per run — later `^D`s are inert — then, with a modal open,
/// closes it with `Stop` so the awaiting flow (dialog round trip / ASK
/// pause) unwinds on the existing stop path without deadlocking; with no
/// modal open it pushes the kill banner into the ring. The binary's
/// `kill_watcher` reads the flag, flips `RunControl.kill_requested` +
/// `stop_requested`, and disposes every supervise-spawned worker.
pub fn apply_ctrl_d(kill: Arc<AtomicBool>, state: &mut TuiState) {
    if !kill.load(Ordering::SeqCst) {
        kill.store(true, Ordering::SeqCst);
        if state.modal.is_some() {
            apply_modal_decision(state, ModalDecision::Close(ModalOutcome::Stop));
        } else {
            state.push_banner("^D — killing the run…".to_string());
        }
    }
}

/// THE single stdin owner in TUI mode (plan step 6): `cfmakeraw`
/// disabled `ICANON`, so no line-mode `read_line` can assemble text
/// anymore — this task reads every raw key, edits the active modal's
/// input line, and dispatches submits through
/// [`dispatch_modal_submit`] (which keeps the typed path byte-parity
/// via [`dispatch_modal_line`] and submits a focused dialog row on a
/// bare Enter, step 8). Every raw byte first goes through
/// [`EscapeCollector`]: a complete escape sequence is decoded by
/// [`parse_escape_nav`] — ↑/↓ move the open dialog's row highlight
/// ([`navigate_focus`] wraps) and everything else (DSR size-report
/// replies, other CSI/SS3) is dropped whole rather than typed. `^D`
/// arms the **kill switch** ([`apply_ctrl_d`]): it closes any open
/// modal with `Stop` (so the awaiting dialog/ASK flow unwinds on the
/// stop path and never deadlocks) or pushes the kill banner, and the
/// binary's `kill_watcher` SIGKILLs every supervise-spawned worker.
/// `^C` flips `ctrl_c` and closes any open modal with `Stop`, so the
/// binary's watcher unwinds the TUI on that path. A closed stdin (the
/// terminal went away) keeps the pre-kill EOF parity
/// ([`eof_outcome`]); the run keeps supervising. Keys with no modal
/// open are dropped — raw mode does not echo, which matches line mode
/// where nothing reads stdin between prompts.
pub async fn input_task(
    state: Arc<tokio::sync::Mutex<TuiState>>,
    ctrl_c: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
) {
    // The PollFd borrows from this handle, so it must outlive the fds.
    let stdin_handle = std::io::stdin();
    let mut fds: Vec<PollFd> = vec![PollFd::new(stdin_handle.as_fd(), PollFlags::POLLIN)];
    let mut bytes: [u8; 64] = [0u8; 64];
    // The collector persists across poll iterations, so a sequence split
    // across `read()`s still assembles (step 8's collector fix).
    let mut collector = EscapeCollector::new();
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
            match collector.feed(byte) {
                Collect::Plain(byte) => match decode_key(byte) {
                    Keystroke::Escape => {
                        // Unreachable here: a raw `ESC` byte is consumed
                        // by the collector, never a `Plain` verdict.
                    }
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
                        // modal, then apply the verdict under a second
                        // lock (the decision is plain data).
                        let (modal, line, focus): (Option<Modal>, String, Option<usize>) = {
                            let mut guard = state.lock().await;
                            let line = guard.modal_input.clone();
                            guard.modal_input = String::new();
                            (guard.modal.clone(), line, guard.modal_focus)
                        };
                        if let Some(modal) = modal {
                            let decision = dispatch_modal_submit(&modal, line.as_str(), focus);
                            let mut guard = state.lock().await;
                            if guard.modal.is_some() {
                                apply_modal_decision(&mut guard, decision);
                            }
                        }
                    }
                    Keystroke::CtrlD => {
                        let mut guard = state.lock().await;
                        // Clone the handle: the loop outlives any one ^D and
                        // the shared `AtomicBool` underneath stays the same.
                        apply_ctrl_d(kill.clone(), &mut guard);
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
                },
                Collect::Sequence(seq) => {
                    // A whole escape sequence: arrows navigate the open
                    // dialog's focus; everything else (DSR size-report
                    // replies, other CSI/SS3) is dropped whole — nothing
                    // leaks into the input line.
                    if let Some(key) = parse_escape_nav(&seq[..]) {
                        let delta: i32 = match key {
                            NavKey::Up => -1,
                            NavKey::Down => 1,
                        };
                        let mut guard = state.lock().await;
                        if let Some(Modal::Dialog(req)) = &guard.modal {
                            let len = dialog_item_count(req);
                            if len > 0 {
                                guard.modal_focus = navigate_focus(guard.modal_focus, delta, len);
                                guard.modal_note = None;
                            }
                        }
                    }
                }
                Collect::Pending => {}
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
mod tests;
