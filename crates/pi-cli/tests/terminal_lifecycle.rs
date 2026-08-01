//! PTY lifecycle tests for the `rpi` TUI terminal guard.
//!
//! Spawns the `rpi` binary on a pseudoterminal and verifies that raw mode and
//! cursor ownership are restored across clean exit, initialization error,
//! panic, SIGTERM, SIGHUP, and suspend/resume. The default TUI must never enter
//! the alternate screen, so ordinary transcript output remains in scrollback.
//!
//! The `rpi` TUI is selected by giving the child a PTY as stdout (`is_terminal`
//! is true) and `--model faux/faux-1` so `build_session` succeeds with the
//! built-in faux provider and no network/auth. The environment is cleared so
//! no inherited provider API key can accidentally satisfy auth gating.

#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::Termios;
use nix::unistd::Pid;
use tempfile::TempDir;

const ENTER_ALT: &str = "\x1b[?1049h";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const CLEAR_AFTER_CURSOR: &str = "\x1b[J";
const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
const DISABLE_LINE_WRAP: &str = "\x1b[?7l";
const ENABLE_LINE_WRAP: &str = "\x1b[?7h";
const CLEAR_CURRENT_LINE: &str = "\x1b[2K";

/// Ctrl+D byte. In raw mode crossterm maps 0x04 to `Char('d') + CONTROL`, which
/// the keybindings resolve to `Action::Quit`; with an empty editor the TUI
/// exits cleanly.
const CTRL_D: u8 = 0x04;
/// Ctrl+C byte. Crossterm maps 0x03 to `Char('c') + CONTROL` → `Action::ClearEditor`.
/// Idle first press arms a 500ms double-press exit; second press exits cleanly.
const CTRL_C: u8 = 0x03;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// An `rpi` subprocess attached to a fresh pseudoterminal, with a background
/// reader draining the master into a shared buffer.
struct PtyProbe {
    child: std::process::Child,
    writer: std::fs::File,
    buffer: Arc<Mutex<String>>,
    _home: TempDir,
    _cwd: TempDir,
}

impl PtyProbe {
    /// Spawn `rpi` with `args` on a PTY. `extra_env` augments a minimal,
    /// sanitized environment (HOME/PATH/TERM plus PI_OFFLINE and
    /// PI_SKIP_VERSION_CHECK to avoid any network).
    fn spawn(args: &[&str], extra_env: &[(&str, &str)]) -> Self {
        let home = TempDir::new().expect("temp HOME");
        let cwd = TempDir::new().expect("temp cwd");
        let winsize = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&winsize), None::<&Termios>).expect("openpty");
        let slave_in = pty.slave.try_clone().expect("clone slave stdin");
        let slave_out = pty.slave.try_clone().expect("clone slave stdout");
        let slave_err = pty.slave;

        let mut cmd = Command::new(rpi_bin());
        cmd.env_clear();
        cmd.env("HOME", home.path());
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("TERM", "xterm-256color");
        cmd.env("PI_OFFLINE", "1");
        cmd.env("PI_SKIP_VERSION_CHECK", "1");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.args(args);
        cmd.current_dir(cwd.path());
        cmd.stdin(Stdio::from(slave_in));
        cmd.stdout(Stdio::from(slave_out));
        cmd.stderr(Stdio::from(slave_err));

        let child = cmd.spawn().expect("spawn rpi");
        let writer = std::fs::File::from(pty.master.try_clone().expect("clone master writer"));
        let reader = std::fs::File::from(pty.master);
        let buffer = Arc::new(Mutex::new(String::new()));
        let buf = buffer.clone();
        thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = &chunk[..n];
                        let s = String::from_utf8_lossy(bytes).into_owned();
                        if let Ok(mut guard) = buf.lock() {
                            guard.push_str(&s);
                        }
                        // Ratatui's inline viewport asks the terminal for its
                        // cursor position (CSI 6n). A bare PTY has no emulator,
                        // so answer like a terminal at row 1, column 1.
                        if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = reader.write_all(b"\x1b[1;1R");
                            let _ = reader.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            writer,
            buffer,
            _home: home,
            _cwd: cwd,
        }
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    fn signal(&mut self, signal: Signal) {
        let pid = Pid::from_raw(self.child.id() as i32);
        kill(pid, signal).expect("send signal to child");
    }

    fn count(&self, needle: &str) -> usize {
        self.buffer
            .lock()
            .expect("buffer lock")
            .matches(needle)
            .count()
    }

    /// Wait until `needle` appears at least `want` times in the captured
    /// output, or `timeout` elapses.
    fn wait_for_count(&self, needle: &str, want: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count(needle) >= want {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn snapshot(&self) -> String {
        self.buffer.lock().expect("buffer lock").clone()
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for PtyProbe {
    fn drop(&mut self) {
        // Best effort: don't strand a child if a test failed before exit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait for the inline TUI to acquire the cursor, with a generous timeout
/// covering debug-build session setup.
fn await_entered(probe: &PtyProbe) -> bool {
    probe.wait_for_count(HIDE_CURSOR, 1, Duration::from_secs(30))
}

#[test]
fn pty_clean_exit_restores_terminal() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(await_entered(&probe), "TUI must acquire the inline viewport: {}", probe.snapshot());
    assert!(!probe.snapshot().contains(ENTER_ALT), "default TUI must not enter alternate screen");
    assert!(
        probe.snapshot().contains(ENABLE_BRACKETED_PASTE),
        "TUI must enable bracketed paste while active"
    );
    assert!(
        probe.snapshot().contains(DISABLE_LINE_WRAP),
        "TUI must disable terminal autowrap while active"
    );
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(DISABLE_BRACKETED_PASTE, 1, Duration::from_secs(15)),
        "clean exit must disable bracketed paste: {}",
        probe.snapshot()
    );
    assert!(
        probe.snapshot().contains(ENABLE_LINE_WRAP),
        "clean exit must restore terminal autowrap"
    );
    assert!(
        probe.snapshot().contains(SHOW_CURSOR),
        "clean exit must restore the cursor without erasing normal output: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "clean exit must report success: {status:?}"
    );
    assert!(
        probe.snapshot().contains(CLEAR_AFTER_CURSOR),
        "clean exit must clear only the live inline viewport"
    );
    let output = probe.snapshot();
    let after_clear = output.rsplit_once(CLEAR_AFTER_CURSOR).map_or("", |(_, tail)| tail);
    assert!(!after_clear.contains("faux/faux-1"));
    assert!(!after_clear.contains("ready"));
}

#[test]
fn pty_settings_overlay_escape_does_not_retain_settings_in_scrollback() {
    // Open /settings on the normal-screen inline TUI, dismiss with Escape, keep
    // interacting, then quit. The PTY capture is the native scrollback surface:
    // settings chrome must not remain after dismiss while ordinary conversation
    // markers stay available.
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(
        !probe.snapshot().contains(ENTER_ALT),
        "settings path must stay on the normal screen"
    );
    // Wait until the idle composer is painted so slash input is accepted.
    assert!(
        probe.wait_for_count("faux/faux-1", 1, Duration::from_secs(20)),
        "composer model chrome must render before /settings: {}",
        probe.snapshot()
    );

    probe.send(b"/settings\r");
    assert!(
        probe.wait_for_count("Settings", 1, Duration::from_secs(20)),
        "settings overlay must render: {}",
        probe.snapshot()
    );
    // Distinct settings chrome only present while the page is open.
    assert!(
        probe.wait_for_count("Ctrl-S apply", 1, Duration::from_secs(5))
            || probe.snapshot().contains("type to filter")
            || probe.snapshot().contains("Ctrl-G"),
        "settings help chrome must be visible while open: {}",
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(150));

    // Escape dismisses the page overlay; clear-on-dismiss erases live pixels.
    probe.send(b"\x1b");
    thread::sleep(Duration::from_millis(200));

    // Continue without submitting a model turn (avoid faux prompt races). Type
    // into the composer then wipe with Ctrl-C so any dirty overlay rows would
    // still have had a chance to be committed if the dismiss clear were missing.
    probe.send(b"hello after settings");
    thread::sleep(Duration::from_millis(150));
    probe.send(&[CTRL_C]);
    thread::sleep(Duration::from_millis(150));

    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "clean exit after settings must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "exit after settings dismiss must succeed: {status:?}\nsnapshot:\n{}",
        probe.snapshot()
    );

    let output = probe.snapshot();
    assert!(
        !output.contains(ENTER_ALT),
        "settings flow must never enter the alternate screen"
    );

    // The full PTY write log still contains the open-frame paint (expected).
    // Retained scrollback is the content that survives after the dismiss clear
    // of the live inline viewport. Take the suffix after the first clear that
    // follows the open settings chrome — that is the post-Escape surface.
    let settings_open_at = output
        .find("Ctrl-S apply")
        .or_else(|| output.find("type to filter"))
        .expect("open settings chrome must have been painted");
    let after_open = &output[settings_open_at..];
    let post_dismiss = after_open
        .find(CLEAR_AFTER_CURSOR)
        .map(|idx| &after_open[idx + CLEAR_AFTER_CURSOR.len()..])
        .expect("Escape must clear the live inline viewport after settings");
    let row_clear_at = after_open
        .find(CLEAR_CURRENT_LINE)
        .expect("Escape must clear overlay rows before clearing the viewport");
    let viewport_clear_at = after_open
        .find(CLEAR_AFTER_CURSOR)
        .expect("Escape must reset the live viewport after row clears");
    assert!(
        row_clear_at < viewport_clear_at,
        "row-wise clearing must precede ED so tmux does not retain overlay rows"
    );
    let plain_after = strip_ansi(post_dismiss);
    let settings_needles = [
        "Ctrl-S apply",
        "Ctrl-G/Ctrl-P",
        "type to filter",
        "↑/↓ select · Enter edit/toggle",
        "Enter edit/toggle · Del reset",
        "defaultProvider",
        "thinkingBudgets",
    ];
    for needle in settings_needles {
        assert!(
            !plain_after.contains(needle),
            "settings overlay needle {needle:?} must be absent from the post-dismiss live/scroll surface.\npost_dismiss plain:\n{plain_after}\nfull plain:\n{}",
            strip_ansi(&output)
        );
    }
    // Durable conversation chrome remains reachable in the overall capture
    // (and typically is repainted after dismiss into the live region).
    let plain_all = strip_ansi(&output);
    assert!(
        plain_all.contains("faux/faux-1"),
        "durable conversation chrome must remain accessible after settings dismiss.\nplain:\n{plain_all}"
    );
    assert!(
        plain_after.contains("faux/faux-1") || plain_after.contains("hello after settings"),
        "post-dismiss surface must still show conversation chrome or continued input.\npost_dismiss plain:\n{plain_after}"
    );
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() || next == '@' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC ... BEL or ST
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    let _ = chars.next();
                }
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[test]
fn pty_initialization_error_leaves_terminal_clean() {
    // No --model and no auth (cleared env) => build_session fails before the
    // TUI is ever entered, so no terminal-acquisition escape is emitted.
    let mut probe = PtyProbe::spawn(&[], &[]);
    assert!(
        probe.wait_for_count(
            "No authenticated models available",
            1,
            Duration::from_secs(30)
        ),
        "init error must report missing auth: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit on initialization error");
    assert!(
        !status.success(),
        "init error must exit non-zero: {status:?}"
    );
    let out = probe.snapshot();
    assert!(
        !out.contains(ENTER_ALT),
        "init error must not enter the alternate screen: {out}"
    );
}

#[test]
fn pty_panic_restores_terminal() {
    let mut probe = PtyProbe::spawn(
        &["--model", "faux/faux-1"],
        &[("PI_TEST_PANIC_AFTER_ENTER", "1")],
    );
    assert!(await_entered(&probe), "TUI must acquire the cursor before panicking: {}", probe.snapshot());
    assert!(!probe.snapshot().contains(ENTER_ALT));
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "panic hook must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after panic");
    assert_eq!(
        status.code(),
        Some(101),
        "uncaught panic must exit with code 101: {status:?}"
    );
}

#[test]
fn pty_sigterm_restores_terminal_and_exits_cleanly() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(await_entered(&probe), "TUI must acquire inline viewport: {}", probe.snapshot());
    assert!(!probe.snapshot().contains(ENTER_ALT));
    thread::sleep(Duration::from_millis(200));
    probe.signal(Signal::SIGTERM);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "SIGTERM must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after SIGTERM");
    assert!(
        status.success(),
        "SIGTERM is handled gracefully (exit 0): {status:?}"
    );
}

#[test]
fn pty_sighup_restores_terminal_and_exits_cleanly() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(await_entered(&probe), "TUI must acquire inline viewport: {}", probe.snapshot());
    assert!(!probe.snapshot().contains(ENTER_ALT));
    thread::sleep(Duration::from_millis(200));
    probe.signal(Signal::SIGHUP);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "SIGHUP must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after SIGHUP");
    assert!(
        status.success(),
        "SIGHUP is handled gracefully (exit 0): {status:?}"
    );
}

#[test]
fn pty_suspend_resume_yields_and_reacquires_terminal() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(await_entered(&probe), "TUI must acquire inline viewport: {}", probe.snapshot());
    assert!(!probe.snapshot().contains(ENTER_ALT));
    probe.send(b"/logout anthropic\r");
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "suspend must restore the cursor: {}",
        probe.snapshot()
    );
    assert!(
        probe.wait_for_count(HIDE_CURSOR, 2, Duration::from_secs(15)),
        "resume must reacquire the cursor: {}",
        probe.snapshot()
    );
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 2, Duration::from_secs(15)),
        "exit after resume must restore the cursor again: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after resume + Ctrl+D");
    assert!(
        status.success(),
        "suspend/resume then quit must exit 0: {status:?}"
    );
}

#[test]
fn pty_double_ctrl_c_exits_and_restores_terminal() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );
    assert!(!probe.snapshot().contains(ENTER_ALT));

    // First idle Ctrl-C arms the exit ladder only.
    probe.send(&[CTRL_C]);
    thread::sleep(Duration::from_millis(80));
    assert!(
        probe.child.try_wait().ok().flatten().is_none(),
        "single Ctrl-C must not exit: {}",
        probe.snapshot()
    );

    // Second press within 500ms exits through the normal return path.
    probe.send(&[CTRL_C]);
    assert!(
        probe.wait_for_count(DISABLE_BRACKETED_PASTE, 1, Duration::from_secs(15)),
        "double Ctrl-C must disable bracketed paste: {}",
        probe.snapshot()
    );
    assert!(
        probe.snapshot().contains(SHOW_CURSOR),
        "double Ctrl-C exit must restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after double Ctrl-C");
    assert!(
        status.success(),
        "double Ctrl-C clean exit must report success: {status:?}"
    );
    assert!(
        probe.snapshot().contains(CLEAR_AFTER_CURSOR),
        "double Ctrl-C exit must clear only the live inline viewport"
    );
}

#[test]
fn pty_single_ctrl_c_does_not_exit() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"], &[]);
    assert!(
        await_entered(&probe),
        "TUI must acquire the inline viewport: {}",
        probe.snapshot()
    );

    probe.send(&[CTRL_C]);
    // Wait past the 500ms double-press window.
    thread::sleep(Duration::from_millis(700));
    assert!(
        probe.child.try_wait().ok().flatten().is_none(),
        "single Ctrl-C must leave the TUI running: {}",
        probe.snapshot()
    );

    // Ctrl-D remains the direct empty-editor exit.
    probe.send(&[CTRL_D]);
    assert!(
        probe.wait_for_count(SHOW_CURSOR, 1, Duration::from_secs(15)),
        "Ctrl+D after single Ctrl-C must still restore the cursor: {}",
        probe.snapshot()
    );
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi must exit after Ctrl+D");
    assert!(
        status.success(),
        "Ctrl+D exit after single Ctrl-C must succeed: {status:?}"
    );
}
