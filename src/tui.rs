//! Pure full-screen TUI frame builders (plan step 3).
//!
//! Repo convention kept: **layout is a pure transform; I/O is thin**. Every
//! function here takes a palette / state / width / height and returns
//! [`StyledLine`]s — no terminal is touched.
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

use crate::rpc::ExtensionUiRequest;
use crate::theme::{Color, Palette};
use crate::ui::{
    FooterStats, LineKind, TuiLine, dialog_lines, format_footer_line, format_header_line,
    truncate_with_ellipsis,
};
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use crate::rpc::UiMethod;
    use crate::theme::default_palette;

    fn palette() -> Palette {
        default_palette()
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
