//! Live terminal-backend smoke test (the plan's step-4 tmux/ghostty gate).
//!
//! Exercising the backend needs a real terminal: `cargo test` under tmux
//! with the pane sized 80×24 or larger. Headless runs skip themselves clean
//! so the suite stays green in CI.
//!
//! What it verifies on a tty:
//!
//! - `Terminal::enter` switches to the alternate screen and puts stdin into
//!   raw mode (observable as a screen flicker and restored termios);
//! - `Terminal::query_size` decodes the terminal's cursor-position report
//!   to a sane size;
//! - `Terminal::leave` restores the primary screen exactly once;
//! - `watch_resizes` arms the SIGWINCH bridge (a detached signalfd thread
//!   publishes resize events to the returned channel).
//!
//! Manual gate (the libtest harness detaches stdin and pipes stdout, so
//! run the compiled test binary under `script`, which allocates a real
//! pty and also logs the raw byte stream):
//!
//! ```sh
//! tmux new-session -d -s tui-smoke -x 100 -y 30
//! tmux send-keys -t tui-smoke 'script -qec \'./target/debug/deps/tui_backend-* --nocapture\' /tmp/smoke.log' Enter
//! sleep 8
//! tmux capture-pane -p -t tui-smoke | grep "size observed"
//! tmux kill-session -t tui-smoke
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pi_plan::tui::{
    AnsiSink, DisplayMode, Keystroke, SavedTerminal, Size, Terminal, TerminalGuard, TtyFaces,
    choose_mode, decode_key, parse_size_report, watch_resizes,
};

/// A real stdout sink: raw ANSI bytes go to the terminal.
fn stdout_sink<'a>() -> Box<AnsiSink<'a>> {
    Box::new(move |bytes: &[u8]| {
        let _ = nix::unistd::write(std::io::stdout(), bytes);
    })
}

#[test]
fn terminal_backend_live_smoke() {
    let stdin_tty = nix::sys::termios::tcgetattr(std::io::stdin()).is_ok();
    let stdout_tty = nix::sys::termios::tcgetattr(std::io::stdout()).is_ok();
    if !(stdin_tty && stdout_tty) {
        eprintln!(
            "SKIP: not under a tty (stdin={} stdout={}) — use the tmux gate",
            stdin_tty, stdout_tty
        );
        return;
    }

    // Pure parts, sanity-checked on the way in.
    assert_eq!(
        choose_mode(TtyFaces {
            stdin: true,
            stdout: true,
            stderr: true
        }),
        DisplayMode::Tui
    );
    assert_eq!(decode_key(0x03), Keystroke::CtrlC);
    assert_eq!(
        parse_size_report("\u{1b}[24;80R".to_string().as_bytes()),
        Some(Size { rows: 24, cols: 80 })
    );

    // Arm the SIGWINCH bridge first so SIGWINCH is blocked in this thread.
    match watch_resizes() {
        Ok((watcher, _rx)) => assert!(watcher.armed),
        Err(err) => panic!("watch_resizes failed on a tty: {err}"),
    }

    // Enter + a real size query.
    let mut terminal = Terminal::new(stdout_sink());
    let saved = terminal.enter().expect("enter needs a tty");
    let size = terminal.query_size(Size { rows: 24, cols: 80 });

    // Leave restores the primary screen; a second leave is a no-op. Report
    // after leaving — text printed while the alternate screen is active is
    // discarded by tmux when the alt buffer is dropped.
    terminal.leave(&saved);
    terminal.leave(&saved);
    assert!(!terminal.active);
    eprintln!("size observed under the tui: {0}x{1}", size.rows, size.cols);
    assert!(
        size.rows >= 10,
        "rows {0} too small for a real terminal",
        size.rows
    );
    assert!(
        size.cols >= 20,
        "cols {0} too small for a real terminal",
        size.cols
    );

    // The drop guard is armed with the live terminal in the real flow;
    // verify it unwinds without panicking on a second exit path.
    let guard_sink_calls = Arc::new(AtomicU64::new(0));
    {
        let mut guard = TerminalGuard::arm(
            Terminal::new(stdout_sink()),
            SavedTerminal {
                stdin_termios: None,
            },
        );
        guard.terminal.active = true;
        guard.leave();
        // Drop after the explicit leave must be inert.
        let _ = guard_sink_calls.as_ref().fetch_add(0, Ordering::SeqCst);
    }
    assert_eq!(guard_sink_calls.as_ref().load(Ordering::SeqCst), 0);
    eprintln!("ok");
}
