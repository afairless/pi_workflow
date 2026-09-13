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

use crate::rpc::{ExtensionUiRequest, UiMethod, UiReply};
use crate::theme::{Color, Palette};
use crate::ui::{
    DialogRow, FooterStats, LineCommand, LineKind, TuiLine, dialog_item_count, dialog_prompt_label,
    format_footer_line, format_header_line, format_idle_footer_line, format_status_line,
    item_reply, kind_glyph, line_command, modal_dialog_rows, reply_from_input, style_for_kind,
    truncate_with_ellipsis,
};

#[cfg(test)]
use crate::ui::dialog_lines;
use crate::worker::{PendingTool, WorkerSnapshot};
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
mod tests {
    use super::*;
    use proptest::prelude::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use crate::rpc::{UiMethod, UiReply};
    use crate::theme::default_palette;
    use crate::worker::{TerminalEvent, Tokens, WorkerSnapshot};

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
    fn escape_collector_assembles_one_shot_arrow_sequences() {
        // ESC [ A — three feeds, one complete sequence at the final byte.
        let mut c = EscapeCollector::new();
        assert_eq!(c.feed(0x1b), Collect::Pending);
        assert_eq!(c.feed(0x5b), Collect::Pending, "`[` never completes early");
        assert_eq!(c.feed(b'A'), Collect::Sequence(vec![0x1b, 0x5b, b'A']));
        // ESC O B — the SS3 encoding, same shape.
        let mut c2 = EscapeCollector::new();
        assert_eq!(c2.feed(0x1b), Collect::Pending);
        assert_eq!(c2.feed(b'O'), Collect::Pending, "`O` never completes early");
        assert_eq!(c2.feed(b'B'), Collect::Sequence(vec![0x1b, b'O', b'B']));
        // Non-ESC bytes with nothing pending pass straight through.
        let mut c3 = EscapeCollector::new();
        assert_eq!(c3.feed(b'x'), Collect::Plain(b'x'));
        // The collector is empty again after a complete sequence.
        assert_eq!(c.feed(b'y'), Collect::Plain(b'y'));
    }

    #[test]
    fn escape_collector_assembles_sequences_split_across_feeds() {
        let mut c = EscapeCollector::new();
        assert_eq!(c.feed(0x1b), Collect::Pending);
        // A later poll delivers the rest of the sequence.
        assert_eq!(c.feed(0x5b), Collect::Pending);
        assert_eq!(c.feed(b'A'), Collect::Sequence(vec![0x1b, 0x5b, b'A']));
    }

    #[test]
    fn escape_collector_holds_parameters_and_completes_the_whole_dsr_reply() {
        let mut c = EscapeCollector::new();
        assert_eq!(c.feed(0x1b), Collect::Pending);
        assert_eq!(c.feed(b'['), Collect::Pending);
        for byte in "8;40;120".to_string().into_bytes().iter() {
            assert_eq!(c.feed(*byte), Collect::Pending, "digits/`;` are held");
        }
        // The terminal byte completes the whole reply — parse_escape_nav
        // then drops it (a stolen DSR reply never leaks printable bytes).
        assert_eq!(
            c.feed(b't'),
            Collect::Sequence("\u{1b}[8;40;120t".to_string().into_bytes())
        );
    }

    #[test]
    fn escape_collector_caps_pathological_input_at_16_bytes() {
        let mut c = EscapeCollector::new();
        assert_eq!(c.feed(0x1b), Collect::Pending);
        assert_eq!(c.feed(b'['), Collect::Pending);
        // 13 parameter bytes → 15 pending: still collecting.
        let mut n: usize = 0;
        while n < 13 {
            assert_eq!(c.feed(b'1'), Collect::Pending, "below the cap: pending");
            n += 1;
        }
        // The 14th reaches 16 bytes: completed whole, dropped downstream.
        let mut expected: Vec<u8> = vec![0x1b, 0x5b];
        let mut m: usize = 0;
        while m < 14 {
            expected.push(b'1');
            m += 1;
        }
        assert_eq!(c.feed(b'1'), Collect::Sequence(expected));
    }

    #[test]
    fn escape_collector_recovers_from_a_lone_esc_and_an_esc_terminator() {
        // A lone ESC then a control byte: the partial sequence completes
        // and is dropped; the collector is usable for the next byte.
        let mut c = EscapeCollector::new();
        assert_eq!(c.feed(0x1b), Collect::Pending);
        assert_eq!(c.feed(0x03), Collect::Sequence(vec![0x1b]));
        assert_eq!(
            c.feed(b'x'),
            Collect::Plain(b'x'),
            "the collector recovered"
        );
        // A second ESC terminates the first sequence and begins the next
        // buffer: `ESC ESC [ A` decodes to a full Up arrow.
        let mut c2 = EscapeCollector::new();
        assert_eq!(c2.feed(0x1b), Collect::Pending);
        assert_eq!(c2.feed(0x1b), Collect::Sequence(vec![0x1b]));
        assert_eq!(c2.feed(b'['), Collect::Pending);
        assert_eq!(c2.feed(b'A'), Collect::Sequence(vec![0x1b, b'[', b'A']));
    }

    #[test]
    fn parse_escape_nav_decodes_both_arrow_encodings_and_nothing_else() {
        assert_eq!(
            parse_escape_nav(&vec![0x1b, b'[', b'A'][..]),
            Some(NavKey::Up)
        );
        assert_eq!(
            parse_escape_nav(&vec![0x1b, b'[', b'B'][..]),
            Some(NavKey::Down)
        );
        assert_eq!(
            parse_escape_nav(&vec![0x1b, b'O', b'A'][..]),
            Some(NavKey::Up)
        );
        assert_eq!(
            parse_escape_nav(&vec![0x1b, b'O', b'B'][..]),
            Some(NavKey::Down)
        );
        // A DSR size-report reply and other CSI sequences are not arrows
        // (dropped by the caller rather than typed).
        let dsr = "\u{1b}[8;40;120t".to_string().into_bytes();
        assert_eq!(parse_escape_nav(&dsr[..]), None);
        let right = [0x1b, b'[', b'C']; // right arrow: not navigable in v1
        assert_eq!(parse_escape_nav(&right[..]), None);
        let empty: Vec<u8> = Vec::new();
        assert_eq!(parse_escape_nav(&empty[..]), None);
        let lone_esc: Vec<u8> = vec![0x1b];
        assert_eq!(parse_escape_nav(&lone_esc[..]), None);
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

    fn confirm_req() -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: "ui-2".to_string(),
            method: UiMethod::Confirm,
            title: None,
            message: Some("allow this bash?".to_string()),
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

    // ---- header ----

    #[test]
    fn header_lines_render_title_bar_and_source_line_at_exact_width() {
        let lines = header_lines(
            &palette(),
            3,
            12,
            "Crate skeleton",
            Some("docs/research/interface-design.md".to_string().as_str()),
            None,
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
            None,
            40,
        );
        assert!(lines[0].text.contains("step 5/10 · unit"));
    }

    #[test]
    fn header_lines_append_the_live_worker_context_on_line_two() {
        let lines = header_lines(
            &palette(),
            3,
            12,
            "Crate skeleton",
            Some("s.md".to_string().as_str()),
            Some(
                "row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s"
                    .to_string()
                    .as_str(),
            ),
            60,
        );
        assert!(lines[1].text.contains("source: s.md"));
        assert!(
            lines[1]
                .text
                .contains("row 3 · agent 7 · turns 4/40 · ctx 61%")
        );
        // Without a source the live context still lands on line 2.
        let bare = header_lines(
            &palette(),
            1,
            1,
            "unit",
            None,
            Some("row 1 · agent 2".to_string().as_str()),
            40,
        );
        assert!(bare[1].text.contains("row 1 · agent 2"));
    }

    #[test]
    fn header_lines_truncate_a_long_source_path_and_context() {
        let lines = header_lines(
            &palette(),
            3,
            12,
            "unit",
            Some(
                "docs/research/a-very-long-planning-document-name.md"
                    .to_string()
                    .as_str(),
            ),
            Some(
                "row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s"
                    .to_string()
                    .as_str(),
            ),
            40,
        );
        assert_eq!(lines[1].text.chars().count(), 40);
        assert!(
            lines[1].text.contains("…"),
            "the long path/context is ellipsized to the width"
        );
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
        let out = trace_lines(&palette(), &lines[..], None, 20, 20, 0);
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
    fn trace_lines_render_success_and_error_tool_glyphs() {
        let lines: Vec<TuiLine> = vec![
            TuiLine {
                kind: LineKind::ToolSuccess,
                text: "tool done: write".to_string(),
            },
            TuiLine {
                kind: LineKind::ToolError,
                text: "tool failed: bash".to_string(),
            },
        ];
        let out = trace_lines(&palette(), &lines[..], None, 40, 20, 0);
        assert_eq!(out.len(), 2);
        assert!(out[0].text.starts_with("✓ tool done: write"));
        assert!(out[1].text.starts_with("✗ tool failed: bash"));
        // Outcome rows carry their fills + outcome foregrounds.
        assert_eq!(out[0].fg, palette().success);
        assert_eq!(out[0].bg, Some(palette().tool_success_bg));
        assert_eq!(out[1].fg, palette().error);
        assert_eq!(out[1].bg, Some(palette().tool_error_bg));
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
        let bottom = trace_lines(&palette(), &lines[..], None, 80, 2, 0);
        assert_eq!(bottom.len(), 2);
        assert!(bottom[0].text.contains("row 4"));
        assert!(bottom[1].text.contains("row 5"));
        // Offset one reveals earlier rows.
        let scrolled = trace_lines(&palette(), &lines[..], None, 80, 2, 1);
        assert_eq!(scrolled.len(), 2);
        assert!(scrolled[0].text.contains("row 3"));
        assert!(scrolled[1].text.contains("row 4"));
        // A viewport taller than the ring shows everything.
        let tall = trace_lines(&palette(), &lines[..], None, 80, 99, 0);
        assert_eq!(tall.len(), 5);
        assert!(tall[0].text.contains("row 1"));
    }

    #[test]
    fn trace_lines_overflowing_words_are_hard_broken() {
        let lines: Vec<TuiLine> = vec![TuiLine {
            kind: LineKind::Bash,
            text: "abcdefghij".to_string(),
        }];
        let out = trace_lines(&palette(), &lines[..], None, 4, 20, 0);
        assert_eq!(out.len(), 3, "10 chars broken into 4+4+2");
        assert_eq!(out[0].text, "abcd".to_string());
        assert_eq!(out[2].text, "ij".to_string());
        for styled in out.iter() {
            assert!(styled.text.chars().count() <= 4);
        }
    }

    // ---- flowing stream (fix 2) ----

    #[test]
    fn append_stream_flows_across_chunks_on_one_open_line() {
        let mut state = TuiState::new();
        state.append_stream(LineKind::Text, "Hello");
        state.append_stream(LineKind::Text, ", wor");
        state.append_stream(LineKind::Text, "ld!");
        assert_eq!(
            state.ring.len(),
            0,
            "nothing closes until a newline/kind change"
        );
        let open = state.stream.expect("stream must stay open");
        assert_eq!(open.kind, LineKind::Text);
        assert_eq!(open.text, "Hello, world!");
    }

    #[test]
    fn append_stream_flushes_on_kind_change_to_separate_blocks() {
        let mut state = TuiState::new();
        state.append_stream(LineKind::Thinking, "let me think");
        state.append_stream(LineKind::Text, "The answer is 42.");
        assert_eq!(state.ring.len(), 1);
        assert_eq!(state.ring[0].kind, LineKind::Thinking);
        assert_eq!(state.ring[0].text, "let me think");
        // Back to thinking flushes the text block into its own ring line.
        state.append_stream(LineKind::Thinking, "hmm");
        assert_eq!(state.ring.len(), 2);
        assert_eq!(state.ring[1].kind, LineKind::Text);
        assert_eq!(state.ring[1].text, "The answer is 42.");
        let open = state.stream.expect("thinking reopens");
        assert_eq!(open.kind, LineKind::Thinking);
        assert_eq!(open.text, "hmm");
    }

    #[test]
    fn append_stream_closes_lines_on_newlines() {
        let mut state = TuiState::new();
        state.append_stream(LineKind::Text, "line one\nline two\n");
        assert_eq!(state.ring.len(), 2);
        assert_eq!(state.ring[0].text, "line one");
        assert_eq!(state.ring[1].text, "line two");
        // A continuation chunk keeps flowing on the same open line and
        // adds no ring row (the trailing `\n` closed a residual empty
        // line, which now holds the continuation).
        state.append_stream(LineKind::Text, "line three");
        let open = state.stream.expect("open line continues");
        assert_eq!(open.text, "line three");
        assert_eq!(state.ring.len(), 2);
    }

    #[test]
    fn append_stream_preserves_paragraph_gaps_as_blank_rows() {
        let mut state = TuiState::new();
        state.append_stream(LineKind::Text, "para one\n\npara two");
        assert_eq!(state.ring.len(), 2, "the empty segment closes a blank row");
        assert_eq!(state.ring[0].text, "para one");
        assert_eq!(state.ring[1].text, "");
        assert_eq!(state.stream.expect("open").text, "para two");
    }

    #[test]
    fn append_stream_normalizes_crlf_line_endings() {
        let mut state = TuiState::new();
        state.append_stream(LineKind::Text, "a\r\nb");
        assert_eq!(state.ring.len(), 1);
        assert_eq!(state.ring[0].text, "a");
        assert_eq!(state.stream.expect("open").text, "b");
    }

    #[test]
    fn append_stream_caps_oversized_newline_free_blobs() {
        let mut state = TuiState::new();
        let blob = fill_with('x', STREAM_LINE_CAP * 2 + 3);
        state.append_stream(LineKind::Text, blob.as_str());
        // Two full lines + the truncation banners land in the ring; the
        // overflow tail stays open.
        assert_eq!(state.ring.len(), 4);
        assert_eq!(state.ring[0].kind, LineKind::Text);
        assert_eq!(state.ring[0].text.chars().count(), STREAM_LINE_CAP);
        assert_eq!(state.ring[1].kind, LineKind::Banner);
        assert_eq!(state.ring[1].text, "…truncated");
        assert_eq!(state.ring[2].kind, LineKind::Text);
        assert_eq!(state.ring[2].text.chars().count(), STREAM_LINE_CAP);
        assert_eq!(state.ring[3].kind, LineKind::Banner);
        assert_eq!(
            state
                .stream
                .expect("overflow tail stays open")
                .text
                .chars()
                .count(),
            3
        );
    }

    #[test]
    fn push_line_and_push_banner_flush_the_open_stream_line_first() {
        let mut state = TuiState::new();
        state.append_stream(LineKind::Text, "streaming…");
        state.push_line(TuiLine {
            kind: LineKind::Tool,
            text: "tool: write (c1)".to_string(),
        });
        assert!(state.stream.is_none(), "push_line flushes the open stream");
        assert_eq!(state.ring.len(), 2);
        assert_eq!(state.ring[0].kind, LineKind::Text);
        assert_eq!(state.ring[0].text, "streaming…");
        assert_eq!(state.ring[1].kind, LineKind::Tool);

        state.append_stream(LineKind::Thinking, "hmm");
        state.push_banner("row done".to_string());
        assert!(
            state.stream.is_none(),
            "push_banner flushes the open stream"
        );
        assert_eq!(state.ring.len(), 4);
        assert_eq!(state.ring[3].kind, LineKind::Banner);
        assert_eq!(state.ring[3].text, "row done");
    }

    #[test]
    fn set_plan_flushes_the_open_stream_line_at_each_row_boundary() {
        let mut state = TuiState::new();
        state.set_plan(1, 2, "row one".to_string(), None);
        state.append_stream(LineKind::Text, "done");
        state.set_plan(2, 2, "row two".to_string(), Some("plan.md".to_string()));
        assert!(state.stream.is_none(), "set_plan flushes the open stream");
        assert_eq!(state.ring.len(), 1);
        assert_eq!(state.ring[0].text, "done");
        assert_eq!(state.row, 2);
    }

    #[test]
    fn ring_eviction_with_a_stream_open_keeps_the_capacity() {
        let mut state = TuiState::new();
        let mut n: usize = 0;
        while n < TUI_RING_CAPACITY + 5 {
            state.push_banner(format!("line {n}"));
            n += 1;
        }
        state.append_stream(LineKind::Text, "tail");
        state.push_banner("boundary".to_string());
        assert_eq!(
            state.ring.len(),
            TUI_RING_CAPACITY,
            "eviction keeps the capacity"
        );
        assert!(state.stream.is_none());
    }

    #[test]
    fn trace_lines_emits_blank_rows_for_empty_logical_lines() {
        let lines: Vec<TuiLine> = vec![
            TuiLine {
                kind: LineKind::Text,
                text: "gap".to_string(),
            },
            TuiLine {
                kind: LineKind::Text,
                text: String::new(),
            },
            TuiLine {
                kind: LineKind::Text,
                text: "after".to_string(),
            },
        ];
        let out = trace_lines(&palette(), &lines[..], None, 80, 20, 0);
        assert_eq!(out.len(), 3, "the empty line renders as a blank row");
        assert_eq!(
            out[1].text, "",
            "blank row stays empty (padded by the frame)"
        );
        assert_eq!(out[1].fg, palette().text);
    }

    #[test]
    fn trace_lines_renders_the_open_stream_line_as_the_virtual_last_row() {
        let lines: Vec<TuiLine> = vec![TuiLine {
            kind: LineKind::Banner,
            text: "spawned".to_string(),
        }];
        let open = StreamLine {
            kind: LineKind::Text,
            text: "streaming tokens".to_string(),
        };
        let out = trace_lines(&palette(), &lines[..], Some(&open), 40, 20, 0);
        assert_eq!(out.len(), 2);
        assert!(out[1].text.contains("streaming tokens"));
        assert_eq!(out[1].fg, palette().text);
        // An empty open line renders as a trailing blank row (residual).
        let empty = StreamLine {
            kind: LineKind::Text,
            text: String::new(),
        };
        let out2 = trace_lines(&palette(), &lines[..], Some(&empty), 40, 20, 0);
        assert_eq!(out2.len(), 2);
        assert_eq!(out2[1].text, "");
    }

    #[test]
    fn compose_frame_renders_the_open_stream_above_the_footer() {
        let mut state = TuiState::new();
        state.set_plan(1, 1, "unit".to_string(), None);
        state.push_banner("row 1: spawned agent 7".to_string());
        state.append_stream(LineKind::Text, "in-flight answer text");
        let frame = compose_frame(&palette(), &state, 80, 10);
        // 2 header + 2 viewport (banner + stream) + 1 footer; the frame
        // is as tall as its content (trace rows are not height-padded).
        assert_eq!(frame.len(), 5);
        assert!(
            frame[3].text.contains("in-flight answer text"),
            "the open stream is the last viewport row, directly above the footer"
        );
        assert!(frame[4].text.contains("row 1/agent —"), "footer untouched");
    }

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
    fn navigate_focus_wraps_in_both_directions() {
        // Down past the last wraps to the first; up past the first wraps
        // to the last (rpiv parity).
        assert_eq!(navigate_focus(Some(0), 1, 2), Some(1));
        assert_eq!(navigate_focus(Some(1), 1, 2), Some(0));
        assert_eq!(navigate_focus(Some(0), -1, 2), Some(1));
        assert_eq!(navigate_focus(Some(1), -1, 2), Some(0));
        // A single item is a fixed point in both directions.
        assert_eq!(navigate_focus(Some(0), 1, 1), Some(0));
        assert_eq!(navigate_focus(Some(0), -1, 1), Some(0));
        // No items → no focus possible.
        assert_eq!(navigate_focus(Some(0), 1, 0), None);
        assert_eq!(navigate_focus(None, 1, 0), None);
        // None current with items present → the first item (defensive).
        assert_eq!(navigate_focus(None, 1, 3), Some(0));
        assert_eq!(navigate_focus(None, -1, 3), Some(0));
    }

    #[test]
    fn navigate_focus_wraps_confirm_no_to_cancel_on_up() {
        // A confirm's TUI row order is (n) no, (y) yes, (c) cancel; ↑
        // from the pre-highlighted `no` wraps to `cancel` (rpiv parity,
        // pinned here and documented in the README).
        assert_eq!(navigate_focus(Some(0), -1, 3), Some(2));
        assert_eq!(navigate_focus(Some(2), 1, 3), Some(0));
    }

    #[test]
    fn modal_submit_typed_input_beats_focus() {
        let req = select_req();
        // A typed number wins even when a different row is highlighted.
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "2", Some(0)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Value(
                "abort".to_string()
            )))
        );
        // Typed `c` cancels with any focus.
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "c", Some(1)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Cancelled))
        );
        // Line commands win over the focus.
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "stop", Some(0)),
            ModalDecision::Close(ModalOutcome::Stop)
        );
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "status", Some(0)),
            ModalDecision::Keep(ModalNote::Status)
        );
    }

    #[test]
    fn modal_submit_an_empty_line_submits_the_focused_item() {
        let req = select_req(); // options: read file, abort
        // Focus 1 → the second option (the index-based path reaches the
        // second of two duplicates, unlike the typed-number path).
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "", Some(1)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Value(
                "abort".to_string()
            )))
        );
        // The cancel row cancels (a zero-option select's only row).
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "", Some(2)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Cancelled))
        );
        // A confirm's focused row maps in TUI order: no → Confirmed(false),
        // yes → Confirmed(true), cancel → Cancelled.
        let confirm = confirm_req();
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(confirm.clone()), "", Some(0)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Confirmed(false)))
        );
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(confirm.clone()), "", Some(1)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Confirmed(true)))
        );
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(confirm.clone()), "", Some(2)),
            ModalDecision::Close(ModalOutcome::DialogReply(UiReply::Cancelled))
        );
    }

    #[test]
    fn modal_submit_empty_lines_without_a_submittable_item_keep_invalid_parity() {
        let req = select_req();
        // An out-of-range focus (defensive) falls back to the
        // invalid-reply path, exactly like an empty line otherwise.
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "", Some(99)),
            ModalDecision::Keep(ModalNote::InvalidReply)
        );
        // No focus at all on an itemless dialog (Input/Editor): Enter is
        // still the old invalid-reply path.
        let input = input_req();
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(input.clone()), "", None),
            ModalDecision::Keep(ModalNote::InvalidReply)
        );
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(input.clone()), "", Some(0)),
            ModalDecision::Keep(ModalNote::InvalidReply)
        );
        // Whitespace-only input is the typed path: still an invalid
        // reply (byte parity with the typed path).
        assert_eq!(
            dispatch_modal_submit(&Modal::Dialog(req.clone()), "  ", Some(0)),
            ModalDecision::Keep(ModalNote::InvalidReply)
        );
        // An empty ASK line is no answer (unchanged); the focus is
        // irrelevant to an ASK pause.
        let modal = Modal::Ask("go?".to_string());
        assert_eq!(
            dispatch_modal_submit(&modal, "", Some(0)),
            ModalDecision::Close(ModalOutcome::AskAnswer(None))
        );
        assert_eq!(
            dispatch_modal_submit(&modal, "", None),
            ModalDecision::Close(ModalOutcome::AskAnswer(None))
        );
    }

    #[test]
    fn open_modal_initializes_the_focus_only_for_select_and_confirm() {
        let mut state = TuiState::new();
        state.open_modal(Modal::Dialog(select_req()), None);
        assert_eq!(state.modal_focus, Some(0), "select pre-highlights option 1");
        state.close_modal(ModalOutcome::DialogReply(UiReply::Cancelled));
        assert_eq!(state.modal_focus, None, "close resets the focus");

        state.open_modal(Modal::Dialog(confirm_req()), None);
        assert_eq!(state.modal_focus, Some(0), "confirm pre-highlights `no`");
        state.open_modal(Modal::Dialog(input_req()), None);
        assert_eq!(state.modal_focus, None, "input has no focusable rows");
        state.open_modal(Modal::Ask("go?".to_string()), None);
        assert_eq!(state.modal_focus, None, "ask is free-form");
        // A zero-option select still opens on its only row (cancel).
        state.open_modal(
            Modal::Dialog(ExtensionUiRequest {
                id: "ui-0".to_string(),
                method: UiMethod::Select,
                title: None,
                message: None,
                options: Vec::new(),
                placeholder: None,
                prefill: None,
                timeout_ms: None,
            }),
            None,
        );
        assert_eq!(
            state.modal_focus,
            Some(0),
            "zero-option select = cancel row"
        );
    }

    #[test]
    fn open_modal_stores_and_close_modal_clears_the_pending_tool() {
        let mut state = TuiState::new();
        let tool = PendingTool {
            tool_call_id: "call_x".to_string(),
            tool_name: "bash".to_string(),
            args: Some(serde_json::json!({ "command": "ls -la" })),
        };
        state.open_modal(Modal::Dialog(select_req()), Some(&tool));
        assert_eq!(
            state.modal_tool.as_ref().map(|t| t.tool_call_id.clone()),
            Some("call_x".to_string())
        );
        state.close_modal(ModalOutcome::DialogReply(UiReply::Cancelled));
        assert_eq!(state.modal_tool, None, "close clears the captured tool");
        // Ask questions carry no tool.
        state.open_modal(Modal::Ask("q?".to_string()), None);
        assert_eq!(state.modal_tool, None);
    }

    #[test]
    fn modal_box_shows_the_captured_tool_context_rows() {
        let req = select_req();
        let tool = PendingTool {
            tool_call_id: "call_7a".to_string(),
            tool_name: "bash".to_string(),
            args: Some(serde_json::json!({ "command": "mkdir -p delete-me-dir" })),
        };
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: Some(&tool),
            },
            40,
            16,
        )
        .expect("a dialog with tool context fits");
        let mut text = String::new();
        for l in out.iter() {
            text.push_str(l.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("tool: bash (call_7a)"));
        assert!(text.contains("$ mkdir -p delete-me-dir"));
        // No pending call: the box shows no tool rows at all.
        let plain = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            16,
        )
        .expect("a plain dialog fits");
        let mut ptext = String::new();
        for l in plain.iter() {
            ptext.push_str(l.text.as_str());
            ptext.push('\n');
        }
        assert!(!ptext.contains("tool: bash"));
    }

    #[test]
    fn invalid_reply_note_is_per_dialog_kind() {
        let mut state = TuiState::new();
        let invalid = ModalDecision::Keep(ModalNote::InvalidReply);

        state.open_modal(Modal::Dialog(select_req()), None);
        apply_modal_decision(&mut state, invalid.clone());
        assert_eq!(
            state.modal_note.clone().expect("select note set"),
            "invalid reply — type an option number or use ↑/↓ + Enter (^D to dismiss)"
        );

        state.open_modal(Modal::Dialog(confirm_req()), None);
        apply_modal_decision(&mut state, invalid.clone());
        assert_eq!(
            state.modal_note.clone().expect("confirm note set"),
            "invalid reply — type y/n/c or use ↑/↓ + Enter (^D to dismiss)"
        );

        state.open_modal(Modal::Dialog(input_req()), None);
        apply_modal_decision(&mut state, invalid.clone());
        assert_eq!(
            state.modal_note.clone().expect("input note set"),
            "invalid reply — try again (or ^D to dismiss)"
        );

        state.open_modal(Modal::Ask("go?".to_string()), None);
        apply_modal_decision(&mut state, invalid);
        assert_eq!(
            state.modal_note.clone().expect("ask note set"),
            "invalid reply — try again (or ^D to dismiss)"
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
        state.open_modal(Modal::Ask("q?".to_string()), None);
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
        state.open_modal(Modal::Ask("next?".to_string()), None);
        assert_eq!(state.modal_outcome, None);
        assert_eq!(state.modal_input, "");
    }

    // ---- Ctrl-D kill switch (fix 3) ----

    #[test]
    fn ctrl_d_arms_the_kill_flag_once_and_banners_without_a_modal() {
        let mut state = TuiState::new();
        let kill = Arc::new(AtomicBool::new(false));
        apply_ctrl_d(kill.clone(), &mut state);
        assert!(kill.load(Ordering::SeqCst));
        assert_eq!(state.ring.len(), 1);
        assert!(state.ring[0].text.contains("^D"));
        assert!(state.ring[0].text.contains("killing the run"));
        // A second ^D is inert: the flag stays set, no duplicate banner.
        apply_ctrl_d(kill.clone(), &mut state);
        assert!(kill.load(Ordering::SeqCst));
        assert_eq!(state.ring.len(), 1);
    }

    #[test]
    fn ctrl_d_closes_an_open_ask_modal_with_stop() {
        let mut state = TuiState::new();
        state.open_modal(Modal::Ask("continue?".to_string()), None);
        let kill = Arc::new(AtomicBool::new(false));
        apply_ctrl_d(kill.clone(), &mut state);
        assert!(state.modal.is_none());
        assert_eq!(state.modal_outcome, Some(ModalOutcome::Stop));
        assert!(kill.load(Ordering::SeqCst));
        // The close replaces the banner: nothing lands in the ring.
        assert_eq!(state.ring.len(), 0);
    }

    #[test]
    fn ctrl_d_closes_an_open_dialog_modal_with_stop() {
        let mut state = TuiState::new();
        state.open_modal(Modal::Dialog(select_req()), None);
        let kill = Arc::new(AtomicBool::new(false));
        apply_ctrl_d(kill.clone(), &mut state);
        assert!(state.modal.is_none());
        assert_eq!(state.modal_outcome, Some(ModalOutcome::Stop));
        assert!(kill.load(Ordering::SeqCst));
    }

    #[test]
    fn modal_box_frames_a_dialog_with_note_and_input_rows() {
        let req = select_req();
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "2",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            16,
        )
        .expect("a dialog modal fits in 40×16");
        // Box is EXACTLY `content + 4` rows: no centering, no padding.
        let box_h = dialog_lines(&req, None).len() + 4;
        assert_eq!(out.len(), box_h, "the box is exactly its own height");
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
        // Bottom-anchored: the box starts at the first row (no leading
        // padding) and the input row is right above the bottom border.
        assert!(out[0].text.trim_start().starts_with("┌"));
        assert!(out[box_h - 2].text.contains("select> 2▌"));
        assert!(
            out.last()
                .cloned()
                .expect("bottom border")
                .text
                .contains("└")
        );
    }

    #[test]
    fn modal_box_frames_an_ask_question_with_the_answer_input_row() {
        let out = modal_box(
            &palette(),
            &Modal::Ask("continue?".to_string()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: None,
                tool: None,
            },
            30,
            10,
        )
        .expect("an ask modal fits in 30×10");
        assert_eq!(out.len(), 6, "2 content + note + input + 2 borders");
        let mut text = String::new();
        for styled in out.iter() {
            text.push_str(styled.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("── worker question ──"));
        assert!(text.contains("continue?"));
        assert!(text.contains("answer> ▌"));
        // An Ask question has no focusable rows: no highlight anywhere.
        assert!(
            !out.iter().any(|l| l.text.contains("▸")),
            "an Ask modal never carries a row highlight"
        );
    }

    #[test]
    fn modal_box_returns_none_for_very_narrow_viewports() {
        assert_eq!(
            modal_box(
                &palette(),
                &Modal::Ask("q?".to_string()),
                &ModalBoxOpts {
                    input: "",
                    note: None,
                    focus: None,
                    tool: None,
                },
                4,
                10
            ),
            None
        );
    }

    #[test]
    fn modal_box_never_exceeds_the_given_height() {
        let req = select_req();
        // Taller than the viewport: truncated from the top, never taller
        // than `height`, and the input row + bottom border survive.
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            3,
        )
        .expect("a squeezed modal still draws");
        assert_eq!(out.len(), 3);
        assert!(
            out[1].text.contains("select> ▌"),
            "the input row survives the top-truncation"
        );
        assert!(
            out[2].text.contains("└"),
            "the bottom border survives the top-truncation"
        );
    }

    #[test]
    fn modal_box_truncates_from_the_top_never_the_bottom() {
        let req = select_req();
        let full = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "2",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            99,
        )
        .expect("spacious viewport");
        let box_h = full.len();
        assert!(box_h > 3);
        let squeezed = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "2",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            3,
        )
        .expect("squeezed viewport");
        // The last rows of the full box (note + input + bottom border)
        // are the rows that survive — truncation never cuts the bottom.
        assert_eq!(squeezed.len(), 3);
        assert_eq!(squeezed[1].text, full[box_h - 2].text);
        assert_eq!(squeezed[2].text, full[box_h - 1].text);
    }

    #[test]
    fn modal_box_highlights_the_focused_row_with_marker_and_accent_text() {
        let req = select_req();
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(1),
                tool: None,
            },
            40,
            16,
        )
        .expect("a focused select modal fits");
        let marker_at = out
            .iter()
            .position(|l| l.text.contains("▸"))
            .expect("exactly one marker row");
        // The focused row (option 2 — item index 1) carries the `▸`
        // marker replacing its two-space indent, accent-colored bold
        // text on the normal panel fill (no more full-row amber band).
        assert!(out[marker_at].text.contains("▸ 2. abort"));
        assert_eq!(out[marker_at].fg, palette().accent);
        assert!(out[marker_at].bold, "the focused row is bolded");
        assert_eq!(out[marker_at].bg, Some(palette().user_message_bg));
        // Sibling content rows keep today's text fg and the panel fill;
        // the marker row is exactly as wide as its siblings (▸ + space
        // replaces the two-space indent).
        for (i, l) in out.iter().enumerate() {
            assert_eq!(l.text.chars().count(), 40, "every box row is exact width");
            if i >= 1 && i <= out.len() - 4 && i != marker_at {
                assert_eq!(
                    l.bg,
                    Some(palette().user_message_bg),
                    "siblings keep the panel fill"
                );
                assert_eq!(l.fg, palette().text);
                assert!(!l.bold, "siblings are not bold");
            }
        }
        // Only one row is ever highlighted.
        assert_eq!(out.iter().filter(|l| l.text.contains("▸")).count(), 1);
    }

    #[test]
    fn modal_box_wraps_a_long_command_line_without_ellipsizing() {
        let mut req = select_req();
        req.title = Some(
            "Permission Required\ntool : bash\ncommand : mkdir -p delete-me-dir && rm -rf delete-me-dir"
                .to_string()
        );
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            30,
        )
        .expect("a tall prompt fits");
        // The long command line word-wraps into multiple chunks instead
        // of ellipsizing, so the target path survives in full.
        let mut text = String::new();
        for l in out.iter() {
            text.push_str(l.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("command : mkdir -p"), "the fact line starts");
        assert!(text.contains("delete-me-dir"), "the target path is not cut");
        assert!(!text.contains("…"), "no box row ellipsizes");
        assert!(
            out.len() > dialog_lines(&req, None).len() + 4,
            "wrapping grows the box beyond the unwrapped row count"
        );
        for l in out.iter() {
            assert_eq!(l.text.chars().count(), 40);
        }
    }

    #[test]
    fn modal_box_wraps_a_long_option_with_the_marker_on_its_first_chunk() {
        let mut req = select_req();
        req.title = Some(
            "Permission Required\ncommand : a very long compound command that forces every option to wrap"
                .to_string()
        );
        req.options = vec![
            "Yes, allow bash \"mkdir *\" for this session".to_string(),
            "No".to_string(),
        ];
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            30,
        )
        .expect("the prompt fits");
        // The focused long option wraps; the `▸` marker lives on its
        // FIRST chunk only, option numbering stays, and no row
        // ellipsizes.
        assert_eq!(
            out.iter().filter(|l| l.text.contains("▸")).count(),
            1,
            "exactly one chunk carries the marker"
        );
        let marker = out
            .iter()
            .find(|l| l.text.contains("▸"))
            .expect("the marker chunk");
        assert!(
            marker.text.contains("▸ 1. Yes, allow"),
            "number + first words on the marker chunk"
        );
        let mut text = String::new();
        for l in out.iter() {
            text.push_str(l.text.as_str());
            text.push('\n');
        }
        assert!(!text.contains("…"), "no box row ellipsizes");
        assert!(
            !marker.text.contains("session"),
            "the word after the wrap is not on the first chunk"
        );
        assert!(text.contains("session"), "the wrapped tail is still shown");
    }

    #[test]
    fn modal_box_keeps_the_focused_row_in_a_clipped_wrapped_box() {
        let mut req = select_req();
        req.title = Some(
            "Permission Required\ntool : bash\ncommand : rm -rf delete-me-dir test target"
                .to_string(),
        );
        let out = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            8,
        )
        .expect("a clipped tall prompt still draws");
        assert_eq!(out.len(), 8, "never taller than the viewport");
        // The note, input, and bottom border are pinned; the wrapped
        // command chunk sits at the window top.
        assert!(out[6].text.contains("select> ▌"));
        assert!(out[7].text.contains("└"));
        assert!(
            out.iter().any(|l| l.text.contains("▸")),
            "the wrapped focused row stays visible inside the window"
        );
        // A focus above the bottom-anchored window cannot fit together
        // with the pinned input row and bottom border, so the marker may
        // scroll out there (accepted): the tail still survives.
        let tiny = modal_box(
            &palette(),
            &Modal::Dialog(req.clone()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            5,
        )
        .expect("a squeezed tall prompt still draws");
        assert_eq!(tiny.len(), 5);
        assert!(tiny[4].text.contains("└"), "the bottom border survives");
        assert!(tiny[3].text.contains("select> ▌"), "the input row survives");
    }

    #[test]
    fn modal_box_confirm_renders_three_rows_with_no_pre_highlighted() {
        let out = modal_box(
            &palette(),
            &Modal::Dialog(confirm_req()),
            &ModalBoxOpts {
                input: "",
                note: None,
                focus: Some(0),
                tool: None,
            },
            40,
            12,
        )
        .expect("a confirm modal fits");
        let mut text = String::new();
        for l in out.iter() {
            text.push_str(l.text.as_str());
            text.push('\n');
        }
        assert!(text.contains("▸ (n) no"), "`no` is the pre-highlighted row");
        assert!(text.contains("(y) yes"));
        assert!(text.contains("(c) cancel"));
        assert_eq!(
            out.iter().filter(|l| l.text.contains("▸")).count(),
            1,
            "exactly one confirm row is highlighted"
        );
        // The unhovered rows keep the panel fill.
        let yes = out
            .iter()
            .find(|l| l.text.contains("(y) yes"))
            .expect("yes row");
        assert_eq!(yes.bg, Some(palette().user_message_bg));
    }

    #[test]
    fn clip_modal_rows_keeps_the_focused_row_inside_the_kept_tail() {
        let mut rows: Vec<StyledLine> = Vec::new();
        let mut i: usize = 0;
        while i < 12 {
            rows.push(StyledLine {
                text: format!("row {i}"),
                fg: Color::Default,
                bg: None,
                bold: false,
            });
            i += 1;
        }
        // The focused box line sits inside the bottom-anchored window:
        // the box keeps it and never cuts the tail.
        let clipped = clip_modal_rows(rows.clone(), 5, Some(10));
        assert_eq!(clipped.len(), 5);
        assert!(
            clipped.iter().any(|l| l.text == "row 10"),
            "the focused row is kept"
        );
        assert_eq!(clipped[0].text, format!("row 7"));
        assert_eq!(
            clipped.last().cloned().expect("window").text,
            format!("row 11")
        );
    }

    #[test]
    fn clip_modal_rows_falls_back_when_the_focus_is_above_the_window() {
        let mut rows: Vec<StyledLine> = Vec::new();
        let mut i: usize = 0;
        while i < 12 {
            rows.push(StyledLine {
                text: format!("row {i}"),
                fg: Color::Default,
                bg: None,
                bold: false,
            });
            i += 1;
        }
        // The focus (row 2) sits above the bottom-anchored tail: no
        // window can show it with the input row and bottom border, so
        // the same top-truncation applies.
        let clipped = clip_modal_rows(rows.clone(), 5, Some(2));
        assert_eq!(clipped.len(), 5);
        assert!(!clipped.iter().any(|l| l.text == "row 2"));
        assert_eq!(
            clipped.last().cloned().expect("window").text,
            format!("row 11")
        );
        // `None` (Ask / itemless dialogs) is the plain truncation.
        let plain = clip_modal_rows(rows.clone(), 5, None);
        assert_eq!(plain.len(), 5);
        assert_eq!(
            plain.last().cloned().expect("window").text,
            format!("row 11")
        );
        // A box that fits is returned whole, focus or not.
        let fits = clip_modal_rows(rows.clone(), 99, Some(0));
        assert_eq!(fits.len(), 12);
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
        state.open_modal(Modal::Dialog(select_req()), None);
        state.modal_append('1');
        let frame = compose_frame(&palette(), &state, 100, 24);
        assert_eq!(frame.len(), 24);
        for line in frame.iter() {
            assert_eq!(line.text.chars().count(), 100);
        }
        assert!(frame[0].text.contains("step 3/12 · Crate skeleton"));
        // The prompt box hugs the bottom of the viewport (directly above
        // the footer), and the trace stays visible above it.
        let input_row = frame
            .iter()
            .position(|l| l.text.contains("select> 1▌"))
            .expect("the modal input line is drawn");
        assert!(
            input_row > 2,
            "the box is below the header, not overlaid on top of it"
        );
        // The footer is still the last row — a modal never occludes it.
        let footer = frame.last().cloned().expect("footer");
        assert!(footer.text.contains("row 3/agent 7"));
        // The newest trace stays visible directly above the box.
        let trace_row = frame
            .iter()
            .position(|l| l.text.contains("spawned agent 7"))
            .expect("the trace row is rendered");
        assert!(trace_row < input_row, "trace sits above the box");
        // The framework: 100×24 → 2 header + 21 viewport + 1 footer; the
        // 8-row box occupies the bottom of the viewport (indices 15..22)
        // with its input row at 21 and bottom border at 22.
        assert!(
            frame[22].text.contains("└"),
            "bottom border directly above the footer"
        );
        assert_eq!(
            input_row, 21,
            "box input row sits at the very bottom of the viewport"
        );
        // The pre-highlighted first option (focus Some(0)) carries the
        // marker, accent-colored bold text, and the panel fill inside
        // the composed frame.
        let highlight = frame
            .iter()
            .find(|l| l.text.contains("▸ 1. read file"))
            .expect("the focused row is rendered");
        assert_eq!(highlight.fg, palette().accent);
        assert!(highlight.bold);
        assert_eq!(highlight.bg, Some(palette().user_message_bg));
    }

    #[test]
    fn compose_frame_pins_the_modal_box_above_the_footer_and_shows_the_open_stream() {
        let mut state = TuiState::new();
        state.set_plan(1, 1, "unit".to_string(), None);
        state.push_banner("row 1: spawned agent 7".to_string());
        state.append_stream(LineKind::Text, "in-flight answer text");
        state.open_modal(Modal::Ask("keep going?".to_string()), None);
        let frame = compose_frame(&palette(), &state, 80, 12);
        assert_eq!(frame.len(), 12);
        // footer at the very bottom, never occluded.
        assert!(frame[11].text.contains("row 1/agent —"));
        // The box bottom border sits directly above the footer…
        assert!(
            frame[10].text.contains("└"),
            "box bottom border above the footer"
        );
        // …with the newest generated text (the open stream line) still
        // visible above the box (trace rows fill indices 2..3, then
        // blank fill, then the 6-row box at indices 5..10).
        assert!(frame[3].text.contains("in-flight answer text"));
        assert!(frame[2].text.contains("spawned agent 7"));
        // The box input row is present (box row 4 of 6).
        assert!(frame[9].text.contains("answer> ▌"));
        assert!(
            frame[7].text.contains("keep going?"),
            "question text inside the box"
        );
    }

    #[test]
    fn apply_modal_decision_status_and_invalid_keep_the_modal_open() {
        let mut state = TuiState::new();
        state.set_plan(3, 12, "unit".to_string(), None);
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 12, 1_090_000));
        state.open_modal(Modal::Dialog(select_req()), None);

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
            pending_tool: None,
            terminal: None,
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
    fn view_from_snapshot_marks_the_worker_not_live_once_terminated() {
        // `None` terminal (the default, live worker) → live.
        assert!(view_from_snapshot(&worker_snapshot(), 40, 1_090_000).live);
        // A recorded terminal flips the view: the footer/header must drop
        // the worker's stats and show the supervisor idle line.
        let mut snap = worker_snapshot();
        snap.terminal = Some(TerminalEvent::Settled);
        assert!(!view_from_snapshot(&snap, 40, 1_090_000).live);
    }

    #[test]
    fn note_row_terminal_stores_the_terminal_and_flips_the_worker_view() {
        let mut state = TuiState::new();
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
        assert!(state.worker.live);
        state.note_row_terminal(5, "failed");
        assert_eq!(state.last_terminal, Some((5, "failed".to_string())));
        assert!(
            !state.worker.live,
            "the hook flips the displayed view not-live"
        );
    }

    #[test]
    fn idle_footer_lines_take_the_next_unit_from_the_plan_map() {
        let mut state = TuiState::new();
        state.seed_plan_units(vec![(1, "row one".to_string()), (2, "row two".to_string())]);
        state.set_plan(1, 2, "row one".to_string(), None);
        state.note_row_terminal(1, "completed");
        let frame = idle_footer_lines(&palette(), &state, 100);
        let row = &frame[0];
        assert!(
            row.text
                .contains("idle · last: row 1 completed · next: row 2 — row two")
        );

        // No plan map (single-row mode seeds one entry; a fresh state has
        // none): the line degrades to `next: row N` without a unit.
        let mut bare = TuiState::new();
        bare.note_row_terminal(1, "completed");
        let frame = idle_footer_lines(&palette(), &bare, 100);
        let row = &frame[0];
        assert!(
            row.text
                .contains("idle · last: row 1 completed · next: row 2")
        );
        assert!(
            !row.text.contains("—"),
            "no unit suffix when the plan map is empty"
        );

        // A narrow footer truncates the content with `…` (never the
        // dynamic hints tail) so the row still fits the width.
        let mut narrow = TuiState::new();
        narrow.seed_plan_units(vec![(1, "row one".to_string()), (2, "row two".to_string())]);
        narrow.note_row_terminal(1, "completed");
        let frame = idle_footer_lines(&palette(), &narrow, 30);
        let row = &frame[0];
        assert_eq!(
            row.text.chars().count(),
            30,
            "padded to the exact footer width"
        );
        assert!(row.text.ends_with("…"));
    }

    #[test]
    fn compose_frame_switches_to_the_idle_footer_after_a_row_terminal() {
        let mut state = TuiState::new();
        state.seed_plan_units(vec![(1, "row one".to_string()), (2, "row two".to_string())]);
        state.set_plan(1, 2, "row one".to_string(), Some("s.md".to_string()));
        state.set_worker_view(view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
        state.push_banner("row 1: spawned agent 7".to_string());

        // Live worker: the footer shows the worker stats and the header
        // carries the live status context on line two.
        let live = compose_frame(&palette(), &state, 100, 24);
        let live_footer = live.last().cloned().expect("footer");
        assert!(live_footer.text.contains("row 1/agent 7"));
        assert!(
            live[1].text.contains("agent 7"),
            "header carries the live context"
        );

        // Row terminal → the hook flips the displayed view not-live: the
        // worker stats drop from the footer and the header line two, the
        // step banner stays, and the idle line names the completed row
        // and the next row with its unit.
        state.note_row_terminal(1, "completed");
        let idle = compose_frame(&palette(), &state, 100, 24);
        assert!(
            idle.len() == live.len(),
            "same frame budget, content swapped"
        );
        let idle_footer = idle.last().cloned().expect("footer");
        assert!(
            idle_footer
                .text
                .contains("idle · last: row 1 completed · next: row 2 — row two")
        );
        assert!(
            !idle_footer.text.contains("row 1/agent"),
            "no worker stats while the supervisor is idle"
        );
        assert!(
            idle_footer.text.contains(" stop / restart / status"),
            "the hints trail still fits and stays available between rows"
        );
        assert!(
            idle[0].text.contains("step 1/2 · row one"),
            "step banner stays"
        );
        assert!(idle[1].text.contains("source: s.md"));
        assert!(
            !idle[1].text.contains("agent 7"),
            "no worker context while idle"
        );
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
        /// Arrow navigation is a bijection on the row ring: stepping down
        /// then up returns to the same row for any item count > 1.
        #[test]
        fn navigate_focus_steps_round_trip(start in "[0-9]{1,2}", len in "[2-9]{1,2}") {
            let n = len.parse::<usize>().unwrap_or(2);
            let i = start.parse::<usize>().unwrap_or(0) % n;
            let down = navigate_focus(Some(i), 1, n).expect("down stays focused");
            let back = navigate_focus(Some(down), -1, n).expect("up stays focused");
            prop_assert_eq!(back, i);
        }
        /// Greedy wrapping is an upper bound: every wrapped row fits.
        #[test]
        fn wrapped_lines_never_exceed_the_viewport_width(text in "[a-z ]{0,60}", width in "[0-9]{1,2}") {
            let w = width.parse::<usize>().unwrap_or(0);
            for line in wrap_text(text.as_str(), w) {
                prop_assert!(line.chars().count() <= w, "wrapped line exceeds {w}");
            }
        }

        /// The open stream and its closed ring lines render under the
        /// same greedy bound: every streamed row fits the viewport.
        #[test]
        fn streamed_lines_never_exceed_the_viewport_width(
            text in "[a-z ]{0,60}",
            nl in "01",
            flip in "01",
            width in "[2-9]{1,2}",
        ) {
            let w = width.parse::<usize>().unwrap_or(2);
            let mut state = TuiState::new();
            if flip == "0" {
                state.append_stream(LineKind::Text, text.as_str());
            } else {
                // A kind change forces a flush mid-run.
                state.append_stream(LineKind::Thinking, "so ");
                state.append_stream(LineKind::Text, text.as_str());
            }
            state.append_stream(LineKind::Text, text.as_str());
            if nl == "1" {
                state.append_stream(LineKind::Text, "\n");
            }
            for styled in trace_lines(
                &palette(),
                &state.ring[..],
                state.stream.as_ref(),
                w,
                40,
                0,
            ) {
                prop_assert!(
                    styled.text.chars().count() <= w,
                    "streamed row exceeds {w}"
                );
            }
        }
    }
}
