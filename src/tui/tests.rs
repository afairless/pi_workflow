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
    let frame = compose_frame(&palette(), &state, 80, 10, None);
    // 2 header + 2 viewport (banner + stream) + 1 footer; the frame
    // is as tall as its content (trace rows are not height-padded).
    assert_eq!(frame.len(), 5);
    assert!(
        frame[3].text.contains("in-flight answer text"),
        "the open stream is the last viewport row, directly above the footer"
    );
    assert!(
        frame[4].text.contains("idle"),
        "footer is the idle line with no worker"
    );
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
    apply_modal_decision(&mut state, invalid.clone(), 0);
    assert_eq!(
        state.modal_note.clone().expect("select note set"),
        "invalid reply — type an option number or use ↑/↓ + Enter (^D to dismiss)"
    );

    state.open_modal(Modal::Dialog(confirm_req()), None);
    apply_modal_decision(&mut state, invalid.clone(), 0);
    assert_eq!(
        state.modal_note.clone().expect("confirm note set"),
        "invalid reply — type y/n/c or use ↑/↓ + Enter (^D to dismiss)"
    );

    state.open_modal(Modal::Dialog(input_req()), None);
    apply_modal_decision(&mut state, invalid.clone(), 0);
    assert_eq!(
        state.modal_note.clone().expect("input note set"),
        "invalid reply — try again (or ^D to dismiss)"
    );

    state.open_modal(Modal::Ask("go?".to_string()), None);
    apply_modal_decision(&mut state, invalid, 0);
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
    let entry = WorkerEntry {
        worker_id: 7,
        row: 3,
        view: view_from_snapshot(&worker_snapshot(), 40, 1_090_000),
    };
    assert_eq!(
        status_note_text(Some(&entry)),
        "status: row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s".to_string()
    );
    // No live worker: the degraded note carries no worker numbers.
    assert_eq!(status_note_text(None), "status: no worker running");
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
        "Permission Required\ntool : bash\ncommand : rm -rf delete-me-dir test target".to_string(),
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
    state.seed_worker(7, 3, "7".to_string(), 40);
    state.update_worker(7, view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
    state.push_banner("row 3: spawned agent 7".to_string());
    state.open_modal(Modal::Dialog(select_req()), None);
    state.modal_append('1');
    let displayed = state.live_workers().first().copied();
    let frame = compose_frame(&palette(), &state, 100, 24, displayed);
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
    let frame = compose_frame(&palette(), &state, 80, 12, None);
    assert_eq!(frame.len(), 12);
    // footer at the very bottom, never occluded — with no live worker it
    // is the supervisor idle line, not the old fake `row 1/agent —`.
    assert!(frame[11].text.contains("idle"));
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
    state.seed_worker(7, 3, "7".to_string(), 12);
    state.update_worker(7, view_from_snapshot(&worker_snapshot(), 12, 1_090_000));
    state.open_modal(Modal::Dialog(select_req()), None);

    apply_modal_decision(&mut state, ModalDecision::Keep(ModalNote::Status), 0);
    assert!(state.modal.is_some(), "status keeps the modal open");
    let note = state.modal_note.clone().expect("a status note was set");
    assert_eq!(
        note,
        "status: row 3 · agent 7 · turns 4/12 · ctx 61% · 1m30s"
    );
    // A later plain key clears the note (modal_append drops it).
    state.modal_append('x');
    assert_eq!(state.modal_note, None);

    apply_modal_decision(&mut state, ModalDecision::Keep(ModalNote::InvalidReply), 0);
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
        0,
    );
    assert!(state.modal.is_none());
    assert_eq!(state.modal_note, None);
    assert_eq!(
        state.modal_outcome,
        Some(ModalOutcome::DialogReply(UiReply::Cancelled)),
    );
}

// ---- modal status note follows the displayed worker (step 3) ----

#[test]
fn apply_modal_decision_status_note_follows_the_displayed_worker() {
    // Two live workers; the rotation cursor holds worker 1. The status
    // note reports the frame's current selection — the same rotation-aware
    // pick `select_live_worker` makes — not a fixed "first live" entry.
    let mut state = TuiState::new();
    state.seed_worker(1, 3, "1".to_string(), 40);
    state.update_worker(1, view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
    state.seed_worker(2, 4, "2".to_string(), 40);
    state.update_worker(2, view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
    state.open_modal(Modal::Dialog(select_req()), None);

    // Mid-hold on worker 1 (row 3): the note names worker 1.
    state.rotation_cursor = Some(RotationCursor {
        worker_id: 1,
        shown_since_ms: 1_000,
    });
    apply_modal_decision(&mut state, ModalDecision::Keep(ModalNote::Status), 1_500);
    assert_eq!(
        state.modal_note.clone().expect("a status note was set"),
        "status: row 3 · agent 7 · turns 4/40 · ctx 61% · 1m30s"
    );

    // Hold elapsed: the note advances to the next live worker (row 4).
    apply_modal_decision(
        &mut state,
        ModalDecision::Keep(ModalNote::Status),
        1_000 + ROTATION_HOLD_MS,
    );
    assert_eq!(
        state.modal_note.clone().expect("a status note was set"),
        "status: row 4 · agent 7 · turns 4/40 · ctx 61% · 1m30s"
    );

    // displayed_worker is exactly the selection the frame renders: a
    // cursor whose hold elapsed wraps around to the first live worker.
    state.rotation_cursor = Some(RotationCursor {
        worker_id: 2,
        shown_since_ms: 5_000,
    });
    assert_eq!(
        displayed_worker(&state, 5_000 + ROTATION_HOLD_MS + 1).map(|e| e.worker_id),
        Some(1),
        "wrap-around to the first live worker after the hold"
    );
}

#[test]
fn apply_modal_decision_status_note_degrades_with_an_empty_registry() {
    let mut state = TuiState::new();
    state.open_modal(Modal::Dialog(select_req()), None);
    apply_modal_decision(&mut state, ModalDecision::Keep(ModalNote::Status), 0);
    assert_eq!(
        state.modal_note.clone().expect("a status note was set"),
        "status: no worker running"
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
    state.seed_worker(7, 3, "7".to_string(), 40);
    state.update_worker(7, view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
    assert_eq!(state.row, 5);
    assert_eq!(state.total, 10);
    assert_eq!(state.unit, "Parser");
    assert_eq!(
        state.source,
        Some("docs/research/interface-design.md".to_string())
    );
    // Footer view bytes: agent, turns, context, cost, elapsed, and the
    // entry's own worker id + row (not the step-banner row).
    assert_eq!(state.workers.len(), 1);
    assert_eq!(state.workers[0].worker_id, 7);
    assert_eq!(state.workers[0].row, 3);
    let view = &state.workers[0].view;
    assert_eq!(view.agent_id, "7");
    assert_eq!(view.turns, 4);
    assert_eq!(view.max_turns, 40);
    assert_eq!(view.context_percent, Some(61.5));
    assert_eq!(view.context_tokens, Some(59_300));
    assert_eq!(view.context_window, Some(200_000));
    assert_eq!(view.cost, Some(0.0451));
    assert_eq!(view.elapsed_ms, 90_000);
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
    state.seed_worker(7, 3, "7".to_string(), 40);
    state.update_worker(7, view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
    state.push_banner("row 3: spawned agent 7".to_string());
    state.push_line(TuiLine {
        kind: LineKind::Thinking,
        text: "so the compiler…".to_string(),
    });
    state.push_line(TuiLine {
        kind: LineKind::Tool,
        text: "tool: write".to_string(),
    });

    let displayed = state.live_workers().first().copied();
    let frame = compose_frame(&palette(), &state, 100, 24, displayed);
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
fn note_row_terminal_stores_the_terminal_without_removing_the_entry() {
    // The hook now only records the terminal; the not-live semantics live
    // in the registry, where removal happens on the tail's exit — never in
    // `note_row_terminal` (a second live worker must stay on screen).
    let mut state = TuiState::new();
    state.seed_worker(7, 5, "7".to_string(), 40);
    assert!(state.has_live_worker());
    state.note_row_terminal(5, "failed");
    assert_eq!(state.last_terminal, Some((5, "failed".to_string())));
    assert!(
        state.has_live_worker(),
        "the hook only stores the label; removal happens on tail exit"
    );
    // Once the tail removes the entry the idle line is unmasked.
    state.remove_worker(7);
    assert!(!state.has_live_worker());
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
fn compose_frame_switches_to_the_idle_footer_only_after_the_entry_is_removed() {
    let mut state = TuiState::new();
    state.seed_plan_units(vec![(1, "row one".to_string()), (2, "row two".to_string())]);
    state.set_plan(1, 2, "row one".to_string(), Some("s.md".to_string()));
    state.seed_worker(1, 1, "7".to_string(), 40);
    state.seed_worker(2, 2, "9".to_string(), 40);
    state.push_banner("row 1: spawned agent 7".to_string());

    // Live worker: the footer shows the worker stats and the header
    // carries the live status context on line two.
    let live = {
        let displayed = state.live_workers().first().copied();
        compose_frame(&palette(), &state, 100, 24, displayed)
    };
    let live_footer = live.last().cloned().expect("footer");
    assert!(live_footer.text.contains("row 1/agent 7"));
    assert!(
        live[1].text.contains("agent 7"),
        "header carries the live context"
    );

    // A row terminal does NOT idle the display while a second entry is
    // still live — the hook only records the label; the tail's exit
    // removes the entry.
    state.note_row_terminal(1, "completed");
    let still_second = {
        let displayed = state.live_workers().first().copied();
        compose_frame(&palette(), &state, 100, 24, displayed)
    };
    assert!(
        !still_second.last().unwrap().text.contains("idle"),
        "no idle line while another worker is live"
    );

    // Every tail exits -> the registry empties -> the idle line appears,
    // naming the completed row and the next row with its unit.
    state.remove_worker(1);
    state.remove_worker(2);
    let idle = compose_frame(&palette(), &state, 100, 24, None);
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
fn registry_seed_upserts_and_removes_entries() {
    let mut state = TuiState::new();
    assert!(!state.has_live_worker());
    assert!(state.live_workers().is_empty());

    state.seed_worker(7, 3, "7".to_string(), 40);
    assert_eq!(state.workers.len(), 1);
    let e = &state.workers[0];
    assert_eq!(e.worker_id, 7);
    assert_eq!(e.row, 3);
    assert_eq!(e.view.agent_id, "7");
    assert!(e.view.live, "a seeded entry is live from the first frame");
    assert_eq!(e.view.turns, 0, "zeroed stats until the first refresh");
    assert_eq!(e.view.max_turns, 40, "seeded with the resolved ceiling");
    assert!(state.has_live_worker());

    // Seeding the same id upserts (row + view), never duplicates.
    state.seed_worker(7, 9, "7".to_string(), 99);
    assert_eq!(state.workers.len(), 1);
    assert_eq!(state.workers[0].row, 9);
    assert_eq!(state.workers[0].view.max_turns, 99);

    // update_worker replaces the stats, keeping the entry's row.
    state.update_worker(7, view_from_snapshot(&worker_snapshot(), 99, 1_090_000));
    assert_eq!(state.workers.len(), 1);
    assert_eq!(state.workers[0].row, 9);
    assert_eq!(state.workers[0].view.turns, 4);
    assert_eq!(state.workers[0].view.max_turns, 99);

    // A second worker is an independent entry.
    state.seed_worker(9, 4, "9".to_string(), 40);
    assert_eq!(state.workers.len(), 2);

    // remove_worker deletes by id only.
    state.remove_worker(7);
    assert_eq!(state.workers.len(), 1);
    assert_eq!(state.workers[0].worker_id, 9);
    state.remove_worker(9);
    assert!(!state.has_live_worker());
}

#[test]
fn seeded_worker_renders_the_plan_row_number_on_the_first_frame() {
    // `--row N` / non-contiguous numbering: the footer's row comes from
    // the entry's TodoRow.number, not the single-row plan position 1.
    let mut state = TuiState::new();
    state.set_plan(1, 1, "step38".to_string(), Some("s.md".to_string()));
    state.seed_worker(7, 38, "5".to_string(), 40);
    let displayed = state.live_workers().first().copied();
    let frame = compose_frame(&palette(), &state, 100, 24, displayed);
    let footer = frame.last().cloned().expect("footer");
    assert!(
        footer.text.contains("row 38/agent 5"),
        "the footer uses the plan's own TodoRow.number (38), not position 1"
    );
    assert!(
        frame[1].text.contains("row 38 · agent 5"),
        "the header context uses the entry's row too"
    );
}

#[test]
fn compose_frame_ignores_a_non_live_displayed_entry() {
    let mut state = TuiState::new();
    state.set_plan(2, 4, "unit".to_string(), Some("s.md".to_string()));
    // An update carrying a terminal snapshot makes the entry not live.
    let mut snap = worker_snapshot();
    snap.terminal = Some(TerminalEvent::Settled);
    state.update_worker(7, view_from_snapshot(&snap, 40, 1_090_000));
    assert!(!state.has_live_worker());

    // Passing the non-live entry renders as if absent (the idle line).
    let dead = state.workers.iter().find(|e| e.worker_id == 7);
    let frame = compose_frame(&palette(), &state, 100, 24, dead);
    let footer = frame.last().cloned().expect("footer");
    assert!(footer.text.contains("idle"), "non-live entry is ignored");
    assert!(
        !footer.text.contains("/agent"),
        "no worker stats for a non-live entry"
    );
    assert!(
        !frame[1].text.contains("agent"),
        "no header worker context for a non-live entry"
    );
}

#[test]
fn empty_registry_renders_the_idle_footer_and_no_header_context() {
    let mut state = TuiState::new();
    state.set_plan(2, 4, "unit".to_string(), Some("s.md".to_string()));
    let frame = compose_frame(&palette(), &state, 100, 24, None);
    let footer = frame.last().cloned().expect("footer");
    assert!(
        footer.text.contains("idle"),
        "zero live workers -> the supervisor idle line"
    );
    assert!(!footer.text.contains("/agent"));
    assert!(
        !frame[1].text.contains("agent"),
        "no header worker context with no live workers"
    );
    assert!(frame[1].text.contains("source: s.md"));
}

// ---- live-worker rotation (step 2) ----

/// A bare registry entry for rotation tests: id, row, live flag.
fn entry(worker_id: u64, row: u64, live: bool) -> WorkerEntry {
    WorkerEntry {
        worker_id,
        row,
        view: WorkerView {
            agent_id: worker_id.to_string(),
            turns: 0,
            max_turns: 0,
            context_percent: None,
            context_tokens: None,
            context_window: None,
            cost: None,
            elapsed_ms: 0,
            pending_tool: None,
            live,
        },
    }
}

#[test]
fn select_live_worker_with_one_live_worker_never_rotates() {
    let workers = vec![entry(1, 1, true)];
    // No cursor: the first live worker, `since` = now.
    let (sel, cur) = select_live_worker(&workers, None, 1_000).expect("one live worker selects");
    assert_eq!(sel.worker_id, 1);
    assert_eq!(cur.shown_since_ms, 1_000);
    // Far past the hold with a cursor on it: still the one worker.
    let cursor = RotationCursor {
        worker_id: 1,
        shown_since_ms: 1_000,
    };
    let (sel2, _) = select_live_worker(&workers, Some(&cursor), 1_000 + 2 * ROTATION_HOLD_MS)
        .expect("single live worker still selects");
    assert_eq!(sel2.worker_id, 1, "single live worker always wins");
    // No live workers -> None (the caller renders the idle line).
    assert!(select_live_worker(&[entry(1, 1, false)], None, 1_000).is_none());
}

#[test]
fn select_live_worker_alternates_two_workers_each_holding_the_slice() {
    let workers = vec![entry(1, 1, true), entry(2, 2, true)];
    let t0 = 50_000u64;
    let (sel0, cur0) = select_live_worker(&workers, None, t0).expect("two live workers select");
    assert_eq!(sel0.worker_id, 1, "starts at the first live worker");

    // Within the first hold: worker 1 stays, `since` untouched.
    let mid = t0 + ROTATION_HOLD_MS / 2;
    let (sel1, cur1) = select_live_worker(&workers, Some(&cur0), mid).expect("holds worker 1");
    assert_eq!(sel1.worker_id, 1, "hold not elapsed -> keeps worker 1");
    assert_eq!(cur1.shown_since_ms, t0, "since unchanged within the hold");

    // At the hold boundary: advance to the next live worker.
    let at_end = t0 + ROTATION_HOLD_MS;
    let (sel2, cur2) = select_live_worker(&workers, Some(&cur0), at_end).expect("rotates");
    assert_eq!(sel2.worker_id, 2, "rotates to the next live worker");
    assert_eq!(cur2.shown_since_ms, at_end, "since refreshed on rotation");

    // Worker 2 holds its own full slice, then wraps back to worker 1.
    let (sel3, _) =
        select_live_worker(&workers, Some(&cur2), at_end + ROTATION_HOLD_MS / 2).expect("holds 2");
    assert_eq!(sel3.worker_id, 2);
    let (sel4, _) =
        select_live_worker(&workers, Some(&cur2), at_end + ROTATION_HOLD_MS).expect("wraps");
    assert_eq!(sel4.worker_id, 1, "wrap-around to the first live worker");
}

#[test]
fn select_live_worker_advances_past_a_vanished_cursor() {
    // Cursor pointed at worker 2, which is no longer live; worker 1 remains.
    let workers = vec![entry(1, 1, true), entry(2, 2, false)];
    let cursor = RotationCursor {
        worker_id: 2,
        shown_since_ms: 1_000,
    };
    let (sel, cur) = select_live_worker(&workers, Some(&cursor), 5_000).expect("advances");
    assert_eq!(sel.worker_id, 1, "advances to the next live worker");
    assert_eq!(cur.shown_since_ms, 5_000);

    // A cursor id removed from the vec entirely: still a live pick.
    let workers_after_removal = vec![entry(3, 3, true), entry(4, 4, true)];
    let cursor = RotationCursor {
        worker_id: 99,
        shown_since_ms: 1_000,
    };
    let (sel2, _) =
        select_live_worker(&workers_after_removal, Some(&cursor), 9_000).expect("still selects");
    assert_eq!(
        sel2.worker_id, 3,
        "first live entry when the cursor id is gone"
    );
}

#[test]
fn select_live_worker_skips_non_live_entries_and_handles_clock_skew() {
    let workers = vec![entry(1, 1, false), entry(2, 2, true), entry(3, 3, false)];
    let (sel, _) = select_live_worker(&workers, None, 1_000).expect("skips non-live to a live");
    assert_eq!(sel.worker_id, 2, "non-live entries are never selected");

    // now < since (clock skew) must NOT elapse the hold.
    let cursor = RotationCursor {
        worker_id: 2,
        shown_since_ms: 5_000,
    };
    let (sel2, cur2) =
        select_live_worker(&workers, Some(&cursor), 1_000).expect("skew keeps the hold");
    assert_eq!(sel2.worker_id, 2, "clock skew keeps the current hold");
    assert_eq!(cur2.shown_since_ms, 5_000, "since never moves backwards");
}

#[test]
fn compose_frame_renders_the_passed_entry_on_header_and_footer_alike() {
    let mut state = TuiState::new();
    state.set_plan(2, 4, "unit".to_string(), Some("s.md".to_string()));
    state.seed_worker(7, 2, "5".to_string(), 40);
    state.seed_worker(9, 3, "6".to_string(), 40);
    state.update_worker(7, view_from_snapshot(&worker_snapshot(), 40, 1_090_000));
    // The displayed selection (mid-rotation) is worker 9.
    let displayed = state.workers.iter().find(|e| e.worker_id == 9);
    let frame = compose_frame(&palette(), &state, 100, 24, displayed);
    let footer = frame.last().expect("footer").text.clone();
    assert!(
        footer.contains("row 3/agent 6"),
        "the footer shows the displayed worker"
    );
    assert!(
        frame[1].text.contains("row 3 · agent 6"),
        "the header shows the same displayed worker"
    );
    assert!(
        !frame[1].text.contains("agent 5"),
        "the non-displayed worker is absent from the header context"
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
    let frame = compose_frame(&palette(), &state, 80, 10, None);
    // 2 header + 7 viewport + 1 footer; the viewport holds the newest
    // seven lines (rows 23..29), so the first viewport row is 23.
    assert_eq!(frame.len(), 10);
    assert!(frame[2].text.contains("row 23"));
    assert!(frame[8].text.contains("row 29"));
    // No live worker: the last row is the supervisor idle line.
    assert!(frame[9].text.contains("idle"));
}

#[test]
fn compose_frame_returns_empty_when_the_fixed_regions_cannot_fit() {
    let state = TuiState::new();
    assert_eq!(
        compose_frame(&palette(), &state, 80, 3, None).len(),
        0,
        "needs at least 4 rows"
    );
    assert_eq!(
        compose_frame(&palette(), &state, 2, 24, None).len(),
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

    /// Under live-worker churn, the rotation never returns a non-live
    /// entry, and every live worker is eventually selected.
    #[test]
    fn select_live_worker_never_picks_a_non_live_entry_and_rotates_through_all(
        live_flags in proptest::collection::vec(any::<bool>(), 6),
    ) {
        let workers: Vec<WorkerEntry> = live_flags
            .iter()
            .enumerate()
            .map(|(i, &live)| entry(i as u64, (i as u64) + 1, live))
            .collect();
        if !workers.iter().any(|e| e.view.live) {
            // Nothing live -> selection is always None.
            prop_assert!(select_live_worker(&workers, None, 1_000).is_none());
        } else {
            let mut cursor = None;
            let mut selected: Vec<u64> = Vec::new();
            let mut now = 1_000u64;
            for _ in 0..200 {
                let (sel, next) = select_live_worker(&workers, cursor.as_ref(), now)
                    .expect("a live worker exists");
                let e = workers
                    .iter()
                    .find(|e| e.worker_id == sel.worker_id)
                    .expect("known id");
                prop_assert!(e.view.live, "selection never returns a non-live entry");
                selected.push(sel.worker_id);
                cursor = Some(next);
                now += ROTATION_HOLD_MS; // advance one full hold per step
            }
            // Every live worker is eventually selected.
            for (i, &live) in live_flags.iter().enumerate() {
                if live {
                    prop_assert!(
                        selected.contains(&(i as u64)),
                        "live worker {i} eventually selected"
                    );
                }
            }
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
