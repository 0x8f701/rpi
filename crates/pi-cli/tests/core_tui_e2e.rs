//! Core TUI end-to-end regressions over a real PTY.
//!
//! Surfaces under test (Unix only, faux model, isolated HOME/cwd):
//! 1. Typed settings edit/paste/escape isolation
//! 2. `/todo` overview → detail navigation and close
//! 3. Startup settings diagnostics after TUI launch
//! 4. Direct PTY attach/input/detach with input absent from composer/transcript
//! 5. `/code-review` open/tree selection/collapse/scroll/close + mouse capture restore
//! 6. Session-tree label edit/clear JSONL round-trip + resume display
//! 7. `/code-review <from> <to>` two-revision commit-to-commit diff label + file
//!
//! Assertions target visible terminal behavior and cleanup sequences, not private
//! TUI fields. Child `rpi` is killed on Drop as a hard backstop.
//! PtyProbe retains a hard-capped output tail while still draining the PTY and
//! answering CSI 6n probes; wait markers are absolute stream offsets.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::Termios;
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
use tempfile::TempDir;

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const ESC: u8 = 0x1b;
const CTRL_RIGHT_BRACKET: u8 = 0x1d;
const CTRL_U: u8 = 0x15;

// crossterm EnableMouseCapture / DisableMouseCapture CSI set.
const MOUSE_ENABLE_NORMAL: &str = "\x1b[?1000h";
const MOUSE_ENABLE_BTN: &str = "\x1b[?1002h";
const MOUSE_ENABLE_ANY: &str = "\x1b[?1003h";
const MOUSE_ENABLE_SGR: &str = "\x1b[?1006h";
const MOUSE_DISABLE_SGR: &str = "\x1b[?1006l";
const MOUSE_DISABLE_ANY: &str = "\x1b[?1003l";
const MOUSE_DISABLE_BTN: &str = "\x1b[?1002l";
const MOUSE_DISABLE_NORMAL: &str = "\x1b[?1000l";

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Hard cap on retained PTY output. Large enough for existing full-flow
/// assertions while preventing unbounded growth if a child floods the PTY.
const PTY_OUTPUT_CAP: usize = 512 * 1024;
/// Fixture subprocess deadline (git seed, etc.).
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes retained from each pipe for diagnostics. Readers still drain to EOF.
const PIPE_DIAG_CAP: usize = 64 * 1024;

/// Retained PTY stream window with an absolute byte origin.
///
/// `dropped` is the number of leading bytes discarded from the full stream so
/// markers from [`PtyProbe::len`] remain valid after compaction.
struct PtyOutputBuf {
    tail: String,
    dropped: usize,
}

impl PtyOutputBuf {
    fn new() -> Self {
        Self {
            tail: String::new(),
            dropped: 0,
        }
    }

    fn absolute_len(&self) -> usize {
        self.dropped.saturating_add(self.tail.len())
    }

    fn push_str(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.tail.push_str(chunk);
        self.compact();
    }

    fn compact(&mut self) {
        if self.tail.len() <= PTY_OUTPUT_CAP {
            return;
        }
        let excess = self.tail.len() - PTY_OUTPUT_CAP;
        // Drop whole chars only — never split a UTF-8 codepoint at the front.
        let mut drop_bytes = excess;
        while drop_bytes < self.tail.len() && !self.tail.is_char_boundary(drop_bytes) {
            drop_bytes += 1;
        }
        if drop_bytes == 0 || drop_bytes >= self.tail.len() {
            // Pathological tiny multi-byte chunk larger than remaining room:
            // keep the final char and drop everything before it.
            if let Some((idx, _)) = self.tail.char_indices().next_back() {
                self.dropped = self.dropped.saturating_add(idx);
                self.tail.replace_range(..idx, "");
            }
            return;
        }
        self.dropped = self.dropped.saturating_add(drop_bytes);
        self.tail.replace_range(..drop_bytes, "");
    }

    fn snapshot(&self) -> String {
        self.tail.clone()
    }

    /// Bytes at-or-after an absolute stream marker.
    ///
    /// If the marker fell out of the retained window, the entire tail is
    /// treated as post-marker (all retained bytes were written after it).
    fn since(&self, absolute_marker: usize) -> &str {
        if absolute_marker <= self.dropped {
            self.tail.as_str()
        } else {
            let local = absolute_marker - self.dropped;
            self.tail.get(local..).unwrap_or("")
        }
    }

    fn rfind_absolute(&self, needle: &str) -> Option<usize> {
        self.tail.rfind(needle).map(|local| self.dropped + local)
    }
}

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

struct PtyProbe {
    child: std::process::Child,
    writer: std::fs::File,
    buffer: Arc<Mutex<PtyOutputBuf>>,
    home: TempDir,
    cwd: TempDir,
    rows: usize,
    cols: usize,
}

impl PtyProbe {
    fn spawn(args: &[&str]) -> Self {
        Self::spawn_with(args, 36, 120, |_, _| {})
    }

    fn spawn_seeded(
        args: &[&str],
        rows: u16,
        cols: u16,
        seed: impl FnOnce(&Path, &Path),
    ) -> Self {
        Self::spawn_with(args, rows, cols, seed)
    }

    /// Spawn using caller-owned HOME/cwd directories so a later process can
    /// resume the same on-disk session without host-global config.
    fn spawn_in(args: &[&str], rows: u16, cols: u16, home: TempDir, cwd: TempDir) -> Self {
        Self::spawn_with_dirs(args, rows, cols, home, cwd, |_, _| {})
    }

    fn spawn_with(
        args: &[&str],
        rows: u16,
        cols: u16,
        seed: impl FnOnce(&Path, &Path),
    ) -> Self {
        let home = TempDir::new().expect("temp HOME");
        let cwd = TempDir::new().expect("temp cwd");
        Self::spawn_with_dirs(args, rows, cols, home, cwd, seed)
    }

    fn spawn_with_dirs(
        args: &[&str],
        rows: u16,
        cols: u16,
        home: TempDir,
        cwd: TempDir,
        seed: impl FnOnce(&Path, &Path),
    ) -> Self {
        seed(home.path(), cwd.path());

        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
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
        // Keep git from picking up user identity/hooks when code-review shells out.
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env(
            "GIT_CONFIG_GLOBAL",
            cwd.path().join("absent-global-git-config"),
        );
        cmd.args(args);
        cmd.current_dir(cwd.path());
        cmd.stdin(Stdio::from(slave_in));
        cmd.stdout(Stdio::from(slave_out));
        cmd.stderr(Stdio::from(slave_err));

        let child = cmd.spawn().expect("spawn rpi");
        let writer = std::fs::File::from(pty.master.try_clone().expect("clone master writer"));
        let reader = std::fs::File::from(pty.master);
        let buffer = Arc::new(Mutex::new(PtyOutputBuf::new()));
        let buf = buffer.clone();
        thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = &chunk[..n];
                        let s = String::from_utf8_lossy(bytes);
                        if let Ok(mut guard) = buf.lock() {
                            guard.push_str(&s);
                        }
                        // Ratatui inline viewport probes cursor position (CSI 6n).
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
            home,
            cwd,
            rows: usize::from(rows),
            cols: usize::from(cols),
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn cwd_path(&self) -> &Path {
        self.cwd.path()
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    fn type_chars(&mut self, text: &str) {
        for byte in text.bytes() {
            self.send(&[byte]);
            thread::sleep(Duration::from_millis(35));
        }
    }

    fn clear_composer(&mut self) {
        self.send(&[CTRL_U]);
        thread::sleep(Duration::from_millis(80));
    }

    fn bracketed_paste(&mut self, payload: &str) {
        let mut bytes = Vec::with_capacity(
            BRACKETED_PASTE_START.len() + payload.len() + BRACKETED_PASTE_END.len(),
        );
        bytes.extend_from_slice(BRACKETED_PASTE_START.as_bytes());
        bytes.extend_from_slice(payload.as_bytes());
        bytes.extend_from_slice(BRACKETED_PASTE_END.as_bytes());
        self.send(&bytes);
    }

    /// Retained output tail (may omit a dropped prefix of the full stream).
    fn snapshot(&self) -> String {
        self.buffer.lock().expect("buffer lock").snapshot()
    }

    /// Absolute number of bytes observed on the PTY stream (including dropped).
    fn len(&self) -> usize {
        self.buffer.lock().expect("buffer lock").absolute_len()
    }

    /// Retained bytes at-or-after an absolute marker from [`Self::len`].
    fn since(&self, absolute_marker: usize) -> String {
        self.buffer
            .lock()
            .expect("buffer lock")
            .since(absolute_marker)
            .to_owned()
    }

    fn rfind_absolute(&self, needle: &str) -> Option<usize> {
        self.buffer
            .lock()
            .expect("buffer lock")
            .rfind_absolute(needle)
    }

    /// Replay the retained PTY stream into a fixed-size live screen and return
    /// the final visible window (last `rows` lines). Raw PTY output is a paint
    /// history — erased overlay rows and stale status persist in the byte
    /// stream, so only the replayed final screen reflects current UI state.
    fn live_screen(&self) -> String {
        let replayed = replay_terminal_scrollback(&self.snapshot(), self.cols, self.rows);
        replayed[replayed.len().saturating_sub(self.rows)..].join("\n")
    }

    /// Poll the live screen until `predicate` holds over the current visible
    /// window. Use this for assertions about what the user actually sees now.
    fn wait_for_live<F>(&self, timeout: Duration, predicate: F) -> Option<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let live = self.live_screen();
            if predicate(&live) {
                return Some(live);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Poll the live screen only after `marker` bytes have been observed, so
    /// the predicate is evaluated against post-action repaints rather than the
    /// pre-action frame.
    fn wait_for_live_after<F>(
        &self,
        marker: usize,
        timeout: Duration,
        predicate: F,
    ) -> Option<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if self.len() > marker {
                let live = self.live_screen();
                if predicate(&live) {
                    return Some(live);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.snapshot().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
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

    fn quit_cleanly(&mut self) {
        // Close any live overlay before quitting so `/quit` reaches the main
        // composer instead of a hidden panel/editor. Blind Esc/Ctrl-D could
        // leave an overlay open and swallow the quit, so each Esc is verified
        // against the replayed live screen: the overlay marker must actually
        // leave the visible window, not merely be followed by a "Ready" paint
        // that a still-open overlay can also emit.
        for _ in 0..6 {
            if !any_overlay_open(&self.live_screen()) {
                break;
            }
            let close_at = self.len();
            self.send(&[ESC]);
            let _ = self.wait_for_live_after(close_at, Duration::from_secs(6), |live| {
                !any_overlay_open(live)
            });
        }
        self.clear_composer();
        let quit_at = self.len();
        self.bracketed_paste("/quit");
        assert!(
            self.wait_for_live_after(quit_at, Duration::from_secs(8), |live| {
                live.contains("/quit")
            })
            .is_some(),
            "`/quit` must reach the live composer (an overlay may still be open): {}",
            self.live_screen()
        );
        self.send(b"\r");
    }

    /// Kill/reap the child and return the isolated HOME/cwd for a later resume.
    fn shutdown_take_dirs(mut self) -> (TempDir, TempDir) {
        kill_and_reap(&mut self.child, Duration::from_secs(5));
        // Prevent Drop from waiting on an already-reaped child path twice.
        let home = std::mem::replace(&mut self.home, TempDir::new().expect("drop home"));
        let cwd = std::mem::replace(&mut self.cwd, TempDir::new().expect("drop cwd"));
        (home, cwd)
    }
}

impl Drop for PtyProbe {
    fn drop(&mut self) {
        kill_and_reap(&mut self.child, Duration::from_secs(5));
    }
}

/// Best-effort terminate + reap with a hard local deadline.
fn kill_and_reap(child: &mut std::process::Child, timeout: Duration) {
    let _ = child.kill();
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) | Err(_) => {
                // Last-resort blocking wait; kill was already sent.
                let _ = child.wait();
                return;
            }
        }
    }
}

fn await_entered(probe: &PtyProbe) -> bool {
    probe.wait_for(HIDE_CURSOR, Duration::from_secs(30))
        || probe.wait_for("pi (rs)", Duration::from_secs(5))
        || probe.wait_for("π", Duration::from_secs(5))
        || probe.wait_for("Ready", Duration::from_secs(5))
        || probe.wait_for("ready", Duration::from_secs(5))
        || probe.wait_for("faux/faux-1", Duration::from_secs(5))
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
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
                    if next == '\u{1b}' && matches!(chars.peek().copied(), Some('\\')) {
                        let _ = chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                let _ = chars.next();
            }
            None => {}
        }
    }
    out
}

/// Replay the ANSI subset emitted by the inline TUI into a fixed-size live
/// screen. Raw PTY output is a paint history, so searching it directly can
/// mistake an erased overlay row, stale status, or closed panel chrome for
/// current UI state. This mirrors `cross_tool_session_tui_e2e`'s replayer so
/// assertions target what the user actually sees on the final visible window.
fn replay_terminal_scrollback(input: &str, width: usize, height: usize) -> Vec<String> {
    let width = width.max(1);
    let height = height.max(1);
    let mut screen = vec![vec![' '; width]; height];
    let mut scrollback = Vec::<String>::new();
    let mut row = 0usize;
    let mut column = 0usize;
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = 0usize;

    let scroll = |screen: &mut Vec<Vec<char>>, scrollback: &mut Vec<String>| {
        scrollback.push(screen[0].iter().collect());
        screen.rotate_left(1);
        screen[height - 1].fill(' ');
    };
    while index < chars.len() {
        match chars[index] {
            '\u{1b}' if chars.get(index + 1) == Some(&'[') => {
                index += 2;
                let start = index;
                while index < chars.len() && !(('@'..='~').contains(&chars[index])) {
                    index += 1;
                }
                if index == chars.len() {
                    break;
                }
                let final_byte = chars[index];
                let params = chars[start..index].iter().collect::<String>();
                let values = params
                    .trim_start_matches('?')
                    .split(';')
                    .map(|part| part.parse::<usize>().unwrap_or(0))
                    .collect::<Vec<_>>();
                match final_byte {
                    'H' | 'f' => {
                        row = values
                            .first()
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                            .saturating_sub(1)
                            .min(height - 1);
                        column = values
                            .get(1)
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                            .saturating_sub(1)
                            .min(width - 1);
                    }
                    'A' => {
                        row = row.saturating_sub(values.first().copied().unwrap_or(1).max(1));
                    }
                    'B' => {
                        row = row
                            .saturating_add(values.first().copied().unwrap_or(1).max(1))
                            .min(height - 1);
                    }
                    'C' => {
                        column = column
                            .saturating_add(values.first().copied().unwrap_or(1).max(1))
                            .min(width - 1);
                    }
                    'D' => {
                        column = column.saturating_sub(values.first().copied().unwrap_or(1).max(1));
                    }
                    'G' => {
                        column = values
                            .first()
                            .copied()
                            .unwrap_or(1)
                            .max(1)
                            .saturating_sub(1)
                            .min(width - 1);
                    }
                    'J' => match values.first().copied().unwrap_or(0) {
                        0 => {
                            screen[row][column..].fill(' ');
                            for line in &mut screen[row + 1..] {
                                line.fill(' ');
                            }
                        }
                        1 => {
                            for line in &mut screen[..row] {
                                line.fill(' ');
                            }
                            screen[row][..=column].fill(' ');
                        }
                        2 | 3 => screen.iter_mut().for_each(|line| line.fill(' ')),
                        _ => {}
                    },
                    'K' => match values.first().copied().unwrap_or(0) {
                        0 => screen[row][column..].fill(' '),
                        1 => screen[row][..=column].fill(' '),
                        2 => screen[row].fill(' '),
                        _ => {}
                    },
                    _ => {}
                }
            }
            '\u{1b}' if chars.get(index + 1) == Some(&']') => {
                index += 2;
                while index < chars.len() {
                    if chars[index] == '\u{7}' {
                        break;
                    }
                    if chars[index] == '\u{1b}' && chars.get(index + 1) == Some(&'\\') {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            '\u{1b}' => index = index.saturating_add(1),
            '\n' => {
                if row + 1 == height {
                    scroll(&mut screen, &mut scrollback);
                } else {
                    row += 1;
                }
            }
            '\r' => column = 0,
            ch if !ch.is_control() => {
                let char_width = ch.width().unwrap_or(0);
                if char_width > 0 {
                    screen[row][column] = ch;
                    column = column.saturating_add(char_width).min(width - 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    scrollback.extend(screen.into_iter().map(|line| line.into_iter().collect()));
    scrollback
}

/// Overlay title/chrome markers that never appear on the bare main composer.
/// Used by [`PtyProbe::quit_cleanly`] to confirm an overlay has actually left
/// the live screen rather than merely being followed by a "Ready" status paint
/// (a hidden overlay can still emit "Ready", so presence-of-"Ready" alone is
/// not proof of closure).
fn any_overlay_open(live: &str) -> bool {
    const OVERLAY_MARKERS: &[&str] = &[
        "Settings",
        "Ctrl-S apply",
        "type to filter",
        "Edit theme",
        "Todo DAG",
        "Code review",
        "focus:tree",
        "focus:diff",
        "Side chat",
        "Ctrl+T edit",
        "Session Tree",
        "Type to search:",
        "Processes",
        "Process detail",
        "Attached to PTY",
        "direct input",
        "Label (empty to remove):",
        "Resume Session",
        "Filter:",
    ];
    OVERLAY_MARKERS.iter().any(|marker| live.contains(marker))
}

fn write_legacy_agent_settings(home: &Path) {
    let agent = home.join(".pi").join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::write(
        agent.join("settings.json"),
        r#"{
  "subagents": {
    "agentOverrides": {
      "reviewer": { "enabled": false, "model": "faux/faux-1" }
    }
  }
}
"#,
    )
    .expect("seed legacy settings");
    let agents = agent.join("agents");
    std::fs::create_dir_all(&agents).expect("agents dir");
    std::fs::write(
        agents.join("reviewer.md"),
        "---\nname: reviewer\ndescription: legacy migration probe\n---\nReview carefully.\n",
    )
    .expect("seed reviewer definition");
}

/// Drain `read` to EOF while retaining at most `cap` bytes (prefix).
fn drain_capped(mut read: impl Read, cap: usize) -> Vec<u8> {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match read.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if retained.len() < cap {
                    let take = (cap - retained.len()).min(n);
                    retained.extend_from_slice(&chunk[..take]);
                }
                // Continue past the cap so the pipe drains to EOF.
            }
            Err(_) => break,
        }
    }
    retained
}

/// Bounded child run: null/piped stdio, concurrent capped drains, `try_wait`
/// deadline, kill+wait cleanup. Readers join only after the child is reaped.
fn run_command_bounded(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn subprocess");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let stdout_reader = thread::spawn(move || drain_capped(stdout, PIPE_DIAG_CAP));
    let stderr_reader = thread::spawn(move || drain_capped(stderr, PIPE_DIAG_CAP));

    let deadline = Instant::now() + SUBPROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stdout = stdout_reader.join().unwrap_or_default();
                    let stderr = stderr_reader.join().unwrap_or_default();
                    panic!(
                        "subprocess exceeded {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                        SUBPROCESS_TIMEOUT,
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr),
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
                panic!(
                    "try_wait subprocess: {error}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                );
            }
        }
    };

    // Child has exited; safe to join pipe readers.
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Output {
        status,
        stdout,
        stderr,
    }
}

fn init_git_repo_with_diff(cwd: &Path) {
    let run = |args: &[&str]| {
        let mut command = Command::new("git");
        // Explicit minimal env: never inherit host GIT_* routing/config/signing/hooks.
        command.env_clear();
        command.env("PATH", std::env::var("PATH").unwrap_or_default());
        command.env("HOME", cwd);
        command.env("USERPROFILE", cwd);
        command
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", cwd.join("absent-global-git-config"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Core Tui")
            .env("GIT_AUTHOR_EMAIL", "core-tui@example.com")
            .env("GIT_COMMITTER_NAME", "Core Tui")
            .env("GIT_COMMITTER_EMAIL", "core-tui@example.com");
        let output = run_command_bounded(command);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    };

    run(&["init"]);
    run(&["config", "user.email", "core-tui@example.com"]);
    run(&["config", "user.name", "Core Tui"]);

    fs::create_dir_all(cwd.join("src").join("nested")).expect("src dirs");
    fs::write(cwd.join("README.md"), "hello\n").expect("readme");
    fs::write(cwd.join("src").join("main.rs"), "fn main() {\n    old();\n}\n").expect("main");
    fs::write(
        cwd.join("src").join("nested").join("deep.rs"),
        "pub fn deep() {}\n",
    )
    .expect("deep");
    run(&["add", "README.md", "src/main.rs", "src/nested/deep.rs"]);
    run(&["commit", "--no-verify", "-m", "initial"]);

    // Two stable branch refs bracketing exactly one committed file change, so
    // `/code-review review-base review-target` exercises a commit-to-commit
    // diff (the added file) that is distinct from the working-tree dirty
    // changes below. `review-base` points at `initial`; `review-target` adds
    // one file on top of it.
    run(&["branch", "review-base"]);
    fs::write(cwd.join("review_committed.md"), "committed-only change\n")
        .expect("committed-only file");
    run(&["add", "review_committed.md"]);
    run(&["commit", "--no-verify", "-m", "add committed-only file"]);
    run(&["branch", "review-target"]);

    // Working tree changes for the bare code-review snapshot (HEAD → WT).
    fs::write(cwd.join("README.md"), "hello\nworld\n").expect("readme dirty");
    fs::write(
        cwd.join("src").join("main.rs"),
        "fn main() {\n    new();\n}\n",
    )
    .expect("main dirty");
    fs::write(
        cwd.join("src").join("nested").join("deep.rs"),
        "pub fn deep() { /* expanded */ }\n",
    )
    .expect("deep dirty");
}

fn mouse_capture_enabled(snap: &str) -> bool {
    snap.contains(MOUSE_ENABLE_NORMAL)
        && snap.contains(MOUSE_ENABLE_BTN)
        && snap.contains(MOUSE_ENABLE_ANY)
        && snap.contains(MOUSE_ENABLE_SGR)
}

fn mouse_capture_disabled_after(probe: &PtyProbe, open_at: usize) -> bool {
    let delta = probe.since(open_at);
    // Disable is emitted in reverse order of enable.
    let sgr = delta.find(MOUSE_DISABLE_SGR);
    let any = delta.find(MOUSE_DISABLE_ANY);
    let btn = delta.find(MOUSE_DISABLE_BTN);
    let normal = delta.find(MOUSE_DISABLE_NORMAL);
    match (sgr, any, btn, normal) {
        (Some(a), Some(b), Some(c), Some(d)) => a < b && b < c && c < d,
        _ => false,
    }
}

/// Plant a multi-branch Pi v3 session under an explicit session-dir.
///
/// Layout (Default filter hides model/thinking metadata):
/// ```text
/// user: branch-root-alpha
/// ├─ assistant: reply-root-alpha
/// │  └─ user: active-leaf-gamma          ← active leaf (selected on open)
/// │     └─ assistant: reply-leaf-gamma
/// └─ user: sibling-branch-beta           ← non-root / non-current target
///    └─ assistant: reply-sibling-beta
/// ```
fn plant_branched_session(session_dir: &Path, cwd: &Path, id: &str) -> PathBuf {
    fs::create_dir_all(session_dir).expect("session dir");
    let path = session_dir.join(format!("{id}.jsonl"));
    let cwd_json = cwd.display().to_string().replace('\\', "\\\\");
    let body = format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd_json}"}}
{{"type":"model_change","id":"mc1","parentId":null,"timestamp":"2026-01-01T00:00:00.100Z","provider":"faux","modelId":"faux-1"}}
{{"type":"thinking_level_change","id":"tl1","parentId":"mc1","timestamp":"2026-01-01T00:00:00.200Z","thinkingLevel":"off"}}
{{"type":"message","id":"u-root","parentId":"tl1","timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"branch-root-alpha"}}],"timestamp":0}}}}
{{"type":"message","id":"a-root","parentId":"u-root","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"reply-root-alpha"}}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":1}}}}
{{"type":"message","id":"u-sibling","parentId":"u-root","timestamp":"2026-01-01T00:00:03.000Z","message":{{"role":"user","content":[{{"type":"text","text":"sibling-branch-beta"}}],"timestamp":2}}}}
{{"type":"message","id":"a-sibling","parentId":"u-sibling","timestamp":"2026-01-01T00:00:04.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"reply-sibling-beta"}}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":3}}}}
{{"type":"message","id":"u-leaf","parentId":"a-root","timestamp":"2026-01-01T00:00:05.000Z","message":{{"role":"user","content":[{{"type":"text","text":"active-leaf-gamma"}}],"timestamp":4}}}}
{{"type":"message","id":"a-leaf","parentId":"u-leaf","timestamp":"2026-01-01T00:00:06.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"reply-leaf-gamma"}}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":5}}}}
"#
    );
    fs::write(&path, body).expect("write branched session");
    path
}

fn read_jsonl_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read session {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("session line is not JSON ({error}): {line}"))
        })
        .collect()
}

fn label_records_for<'a>(records: &'a [Value], target_id: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| {
            record.get("type").and_then(Value::as_str) == Some("label")
                && record.get("targetId").and_then(Value::as_str) == Some(target_id)
        })
        .collect()
}

/// ESC L is the classic meta encoding for Alt+L; uppercase L also sets SHIFT in
/// crossterm (`parse_event(b"\x1bL")` → Char('L') + ALT|SHIFT → chord alt+shift+l).
fn send_tree_edit_label(probe: &mut PtyProbe) {
    probe.send(&[ESC, b'L']);
    thread::sleep(Duration::from_millis(120));
}

fn open_session_tree(probe: &mut PtyProbe) -> usize {
    let open_at = probe.len();
    probe.clear_composer();
    probe.type_chars("/tree");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(8), |live| {
                live.contains("/tree")
            })
            .is_some(),
        "composer must show /tree on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(20), |live| {
                live.contains("Session Tree") && live.contains("Type to search:")
            })
            .is_some(),
        "session tree panel must open on the live screen: {}",
        probe.live_screen()
    );
    open_at
}

/// Move selection from the active leaf onto the sibling branch entry.
///
/// Default open selects the active path's leaf (`a-leaf`). The sibling branch
/// is a non-root, non-current entry with unique text `sibling-branch-beta`.
fn select_sibling_branch_entry(probe: &mut PtyProbe) {
    // Prefer search so selection is deterministic even if sort order shifts.
    let search_at = probe.len();
    probe.type_chars("sibling-branch-beta");
    // Per-key typing interleaves cursor/separator cells in the raw stream, so
    // assert against the replayed live screen where the typed query lands as a
    // contiguous row, not against the raw paint history.
    assert!(
        probe
            .wait_for_live_after(search_at, Duration::from_secs(8), |live| {
                live.contains("Session Tree") && live.contains("sibling-branch-beta")
            })
            .is_some(),
        "tree search must surface sibling branch on the live screen: {}",
        probe.live_screen()
    );
    // With a unique query the sibling row is the only match and stays selected
    // (or is the sole visible entry). Nudge with Home/Up then Down once so a
    // multi-match fallback still lands on the sibling text.
    probe.send(b"\x1b[H"); // Home
    thread::sleep(Duration::from_millis(60));
    for _ in 0..6 {
        let live = probe.live_screen();
        // Selected row uses the › cursor glyph from render_tree_panel.
        if live.lines().any(|line| {
            line.contains('›') && line.contains("sibling-branch-beta")
        }) {
            return;
        }
        probe.send(b"\x1b[B"); // Down
        thread::sleep(Duration::from_millis(80));
    }
    let live = probe.live_screen();
    assert!(
        live.lines().any(|line| {
            line.contains('›') && line.contains("sibling-branch-beta")
        }) || live.contains("sibling-branch-beta"),
        "must select non-root sibling branch entry before label edit: {live}"
    );
}

/// Contract: `/settings` opens the schema panel; Enter opens a typed value
/// editor; bracketed paste lands only in the setting value; Escape cancels the
/// value editor without closing the panel; second Escape dismisses settings and
/// restores the composer without retaining paste text in the composer draft.
#[test]
fn pty_settings_edit_paste_escape_isolation() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"]);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(400));

    let before_settings = probe.len();
    probe.type_chars("/settings");
    assert!(
        probe
            .wait_for_live_after(before_settings, Duration::from_secs(8), |live| {
                live.contains("/settings")
            })
            .is_some(),
        "composer must show /settings on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(before_settings, Duration::from_secs(20), |live| {
                live.contains("Settings")
                    && (live.contains("Ctrl-S apply")
                        || live.contains("type to filter")
                        || live.contains("Enter edit/toggle"))
            })
            .is_some(),
        "settings panel must open on the live screen: {}",
        probe.live_screen()
    );

    // Filter to a string setting that opens the typed value editor.
    let before_filter = probe.len();
    probe.type_chars("theme");
    assert!(
        probe
            .wait_for_live_after(before_filter, Duration::from_secs(8), |live| {
                live.contains("Settings") && live.contains("theme")
            })
            .is_some(),
        "settings search must show theme on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r"); // open edit for selected row
    assert!(
        probe
            .wait_for_live_after(before_filter, Duration::from_secs(10), |live| {
                live.contains("Edit theme")
            })
            .is_some(),
        "typed settings editor must open on the live screen: {}",
        probe.live_screen()
    );

    let paste_token = "settings-paste-ISOLATION-token-7f3a";
    let before_paste = probe.len();
    // Bracketed paste into the value editor — must not reach the main composer.
    probe.bracketed_paste(paste_token);
    assert!(
        probe
            .wait_for_live_after(before_paste, Duration::from_secs(8), |live| {
                live.contains("Edit theme") && live.contains(paste_token)
            })
            .is_some(),
        "paste must appear in the settings value editor on the live screen: {}",
        probe.live_screen()
    );

    // Escape cancels value input but keeps the settings panel. The live screen
    // must drop the "Edit theme" editor chrome while still showing the panel —
    // a raw-history search would match the just-erased editor frame.
    let esc_value_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(esc_value_at, Duration::from_secs(10), |live| {
                !live.contains("Edit theme")
                    && (live.contains("Settings")
                        || live.contains("Ctrl-S apply")
                        || live.contains("type to filter")
                        || live.contains("Enter edit/toggle"))
            })
            .is_some(),
        "Escape on value editor must keep settings panel open on the live screen: {}",
        probe.live_screen()
    );

    // Second Escape dismisses the panel entirely. Require the panel marker to
    // actually leave the live screen — a hidden panel can still emit "Ready",
    // so presence-of-"Ready" alone is not proof of closure.
    let before_close = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(before_close, Duration::from_secs(10), |live| {
                !live.contains("Settings")
                    && !live.contains("Ctrl-S apply")
                    && !live.contains("type to filter")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Escape must dismiss settings on the live screen: {}",
        probe.live_screen()
    );

    // The dismissed settings value must not linger as a composer draft.
    assert!(
        !probe.live_screen().contains(paste_token),
        "paste token must not remain on the live screen after settings dismiss: {}",
        probe.live_screen()
    );

    // Composer must accept new input (bracketed paste → one contiguous render
    // on the live screen; per-key typing interleaves cursor cells in the raw
    // stream and would not satisfy a contiguous raw assertion).
    probe.clear_composer();
    let before_focus = probe.len();
    let focus_marker = "composer-after-settings";
    probe.bracketed_paste(focus_marker);
    assert!(
        probe
            .wait_for_live_after(before_focus, Duration::from_secs(8), |live| {
                live.contains(focus_marker)
            })
            .is_some(),
        "composer must accept input after settings dismiss: {}",
        probe.live_screen()
    );
    probe.clear_composer();

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit after settings flow: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership"
    );
}

/// Contract: seed a multi-phase Todo via `/todo` markdown (uniquely named
/// phases, decomposed tasks, mixed pending/in-progress markers), open the DAG
/// overview (main row + counts projected), Enter into detail and assert phase
/// names, task texts, status markers (○/●), status text, counts, and zero
/// linked child jobs (faux/offline: none spawned). Esc returns to overview,
/// second Esc closes and restores composer focus. No dependency syntax is
/// asserted from markdown; readiness/edge coverage stays in typed/RPC tests.
#[test]
fn pty_todo_overview_detail_navigation() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"]);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(400));

    // Seed todos via bracketed paste so newlines stay in the composer draft
    // (raw LF can be ambiguous with Enter on a bare PTY).
    let seed_start = probe.len();
    // Multi-phase markdown: uniquely named phases and decomposed tasks with
    // mixed pending/in-progress markers. No dependency syntax is asserted here
    // (markdown carries none); readiness/edge coverage stays in typed/RPC tests.
    let seed = "/todo # Survey\n- [ ] map parser surface\n- [ ] map renderer surface\n# Construct\n- [/] repair composer repaint\n- [/] bound Todo projection\n- [ ] verify Todo flow";
    probe.bracketed_paste(seed);
    assert!(
        probe
            .wait_for_live_after(seed_start, Duration::from_secs(8), |live| {
                live.contains("repair composer repaint")
            })
            .is_some(),
        "todo seed must land in the live composer: {}",
        probe.live_screen()
    );
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(seed_start, Duration::from_secs(20), |live| {
                live.contains("updated todo")
                    || live.contains("Todo:")
                    || live.contains("Remaining items")
            })
            .is_some(),
        "todo seed must report success on the live screen: {}",
        probe.live_screen()
    );
    thread::sleep(Duration::from_millis(250));

    let open_start = probe.len();
    probe.clear_composer();
    probe.type_chars("/todo");
    assert!(
        probe
            .wait_for_live_after(open_start, Duration::from_secs(8), |live| {
                live.contains("/todo")
            })
            .is_some(),
        "composer must show /todo on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(open_start, Duration::from_secs(15), |live| {
                live.contains("Todo DAGs")
                    && (live.contains("Enter details") || live.contains("Main session"))
            })
            .is_some(),
        "todo overview must open on the live screen: {}",
        probe.live_screen()
    );
    assert!(
        probe
            .wait_for_live_after(open_start, Duration::from_secs(10), |live| {
                live.contains("Main session")
                    && live.contains("[main]")
                    && live.contains("open 5 active 2 blocked 0")
            })
            .is_some(),
        "todo overview must project the main DAG row and counts: {}",
        probe.live_screen()
    );

    let detail_start = probe.len();
    probe.send(b"\r"); // Enter → detail
    assert!(
        probe
            .wait_for_live_after(detail_start, Duration::from_secs(15), |live| {
                live.contains("Todo DAG detail")
                    && live.contains("Survey")
                    && live.contains("Construct")
                    && live.contains("map parser surface")
                    && live.contains("repair composer repaint")
                    && live.contains("verify Todo flow")
                    && live.contains("\u{25cb}")
                    && live.contains("\u{25cf}")
                    && live.contains("in progress")
                    && live.contains("0 completed")
                    && live.contains("5 open")
                    && live.contains("2 active")
                    && live.contains("0 blocked")
                    && !live.contains("job:")
            })
            .is_some(),
        "todo detail must render phases, tasks, status markers, counts, and zero linked jobs: {}",
        probe.live_screen()
    );

    // Esc steps back to overview (not full close). The detail-specific "Todo DAG
    // detail" chrome must leave the live screen while the overview "Todo DAGs"
    // returns — a raw search would still see the just-erased detail frame.
    let back_start = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(back_start, Duration::from_secs(10), |live| {
                !live.contains("Todo DAG detail")
                    && live.contains("Todo DAGs")
                    && (live.contains("Enter details") || live.contains("Main session"))
            })
            .is_some(),
        "Esc from detail must return to overview on the live screen: {}",
        probe.live_screen()
    );

    // Second Esc closes the panel. Require the panel marker to actually leave
    // the live screen — a hidden panel can still emit "Ready".
    let close_start = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(close_start, Duration::from_secs(10), |live| {
                !live.contains("Todo DAG")
                    && !live.contains("Enter details")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Esc from overview must close the panel on the live screen: {}",
        probe.live_screen()
    );

    // Composer focus restored (bracketed paste → one contiguous live render).
    let focus_start = probe.len();
    probe.bracketed_paste("todo-focus-ok");
    assert!(
        probe
            .wait_for_live_after(focus_start, Duration::from_secs(8), |live| {
                live.contains("todo-focus-ok")
            })
            .is_some(),
        "composer must accept input after todo close: {}",
        probe.live_screen()
    );
    probe.clear_composer();

    probe.quit_cleanly();
    assert!(
        probe.wait_exit(Duration::from_secs(20)).is_some(),
        "TUI must exit after todo flow: {}",
        probe.snapshot()
    );
}

/// Contract: `subagents.agentOverrides` migration is silent — the TUI starts
/// without a deprecation warning toast, and the composer is immediately
/// usable. (Previously the warning was shown and dismissed with Esc; now the
/// migration runs silently per the silenced-deprecated-warnings change.)
#[test]
fn pty_silent_legacy_migration_keeps_override_and_composer_usable() {
    let mut probe = PtyProbe::spawn_seeded(&["--model", "faux/faux-1"], 28, 100, |home, _cwd| {
        write_legacy_agent_settings(home);
    });
    assert!(
        await_entered(&probe),
        "TUI must start with seeded settings: {}",
        probe.snapshot()
    );

    // Silence covers the complete PTY stream, not only the final screen after
    // ratatui has repainted over any pre-TUI stderr output.
    let snapshot = probe.snapshot();
    assert!(
        !snapshot.contains("deprecated subagents.agentOverrides")
            && !snapshot.contains("subagents.agentOverrides"),
        "agentOverrides migration is silent on the complete PTY stream: {snapshot}"
    );

    // The migrated override still takes effect: the Agents panel resolves the
    // reviewer row through the configured faux model instead of merely hiding
    // the old warning while dropping the legacy data.
    let agents_start = probe.len();
    probe.type_chars("/agents");
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(agents_start, Duration::from_secs(15), |live| {
                live.contains("Agents")
                    && live.lines().any(|line| {
                        line.contains("OFF")
                            && line.contains("reviewer")
                            && line.contains("model=faux/faux-1")
                    })
            })
            .is_some(),
        "silent migration must feed the seeded reviewer definition: {}",
        probe.live_screen()
    );
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(agents_start, Duration::from_secs(8), |live| {
                !live.contains("Global settings · Enter toggle")
            })
            .is_some(),
        "Agents panel must close before composer assertion: {}",
        probe.live_screen()
    );

    // Composer is immediately usable (no warning toast to dismiss).
    let focus_start = probe.len();
    probe.bracketed_paste("after-warning");
    assert!(
        probe
            .wait_for_live_after(focus_start, Duration::from_secs(8), |live| {
                live.contains("after-warning")
            })
            .is_some(),
        "composer must remain usable after startup warning: {}",
        probe.live_screen()
    );
    probe.clear_composer();

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(status.is_some(), "TUI must exit: {}", probe.snapshot());
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR) || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore cursor"
    );
}

/// Contract: attach to a supervised PTY, send a unique secret as direct input,
/// observe child echo in the attachment overlay, detach, and prove the secret
/// does not become composer draft or a user transcript turn.
#[test]
fn pty_direct_attach_input_secrecy_and_detach() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"]);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(400));

    let secret = "PTY_DIRECT_INPUT_e2e_9c4b";
    let start = probe.len();
    probe.send(
        format!("/process start --tty sh -c \"read line; echo CHILD:$line; sleep 30\"\r")
            .as_bytes(),
    );
    assert!(
        probe
            .wait_for_live_after(start, Duration::from_secs(20), |live| {
                live.contains("running")
                    || live.contains("Running")
                    || live.contains("sh -c")
            })
            .is_some(),
        "PTY process must start on the live screen: {}",
        probe.live_screen()
    );
    thread::sleep(Duration::from_millis(350));

    let panel_at = probe.len();
    probe.clear_composer();
    probe.type_chars("/ps");
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(panel_at, Duration::from_secs(15), |live| {
                live.contains("Processes")
                    && (live.contains("a attach PTY") || live.contains("Esc close"))
            })
            .is_some(),
        "process panel must open on the live screen: {}",
        probe.live_screen()
    );

    let attach_at = probe.len();
    probe.send(b"a");
    assert!(
        probe
            .wait_for_live_after(attach_at, Duration::from_secs(12), |live| {
                live.contains("Attached to PTY")
                    || live.contains("direct input")
                    || live.contains("Input goes directly to the child PTY")
            })
            .is_some(),
        "attach overlay must open on the live screen: {}",
        probe.live_screen()
    );

    let input_at = probe.len();
    probe.send(secret.as_bytes());
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(input_at, Duration::from_secs(12), |live| {
                live.contains(&format!("CHILD:{secret}"))
            })
            .is_some(),
        "child must echo secret into the attachment overlay on the live screen: {}",
        probe.live_screen()
    );

    // Detach while child still sleeps. The attach overlay must actually leave
    // the live screen — a raw "Detached from PTY" search could match a toast
    // paint while the overlay is still open.
    let detach_at = probe.len();
    probe.send(&[CTRL_RIGHT_BRACKET]);
    assert!(
        probe
            .wait_for_live_after(detach_at, Duration::from_secs(10), |live| {
                !live.contains("Attached to PTY")
                    && !live.contains("direct input")
                    && !live.contains("Input goes directly to the child PTY")
            })
            .is_some(),
        "Ctrl+] must detach and close the attach overlay on the live screen: {}",
        probe.live_screen()
    );
    thread::sleep(Duration::from_millis(200));

    // Composer must be empty of the secret. Paste a distinct marker (bracketed
    // paste → one contiguous live render; per-key typing interleaves cursor
    // cells in the raw stream) and prove the secret is absent from the entire
    // live screen — it never reached the composer draft nor the transcript.
    probe.clear_composer();
    let focus_at = probe.len();
    let marker = "composer-marker-only";
    probe.bracketed_paste(marker);
    assert!(
        probe
            .wait_for_live_after(focus_at, Duration::from_secs(8), |live| {
                live.contains(marker)
            })
            .is_some(),
        "composer must accept input after detach: {}",
        probe.live_screen()
    );
    {
        let live = probe.live_screen();
        assert!(
            !live.contains(secret),
            "secret must not remain on the live screen (composer draft or transcript) after detach: {live}"
        );
        // No user transcript turn with the secret may be created. A submitted
        // user turn would be the most recent transcript line and thus visible.
        assert!(
            !live.contains(&format!("You{secret}"))
                && !live.contains(&format!("You\n{secret}"))
                && !live.contains(&format!("\n{secret}")),
            "secret must not become a user transcript turn after detach: {live}"
        );
    }
    probe.clear_composer();

    // Best-effort stop of the still-running sleep child via /ps.
    let stop_at = probe.len();
    probe.type_chars("/ps");
    probe.send(b"\r");
    let _ = probe.wait_for_live_after(stop_at, Duration::from_secs(10), |live| {
        live.contains("Processes") || live.contains("sleep") || live.contains("sh -c")
    });
    probe.send(b"\r"); // detail
    thread::sleep(Duration::from_millis(200));
    probe.send(b"x");
    thread::sleep(Duration::from_millis(150));
    let live = probe.live_screen();
    if live.contains("Stop process") || live.contains("SIGTERM") {
        probe.send(b"y");
        thread::sleep(Duration::from_millis(300));
    }

    probe.quit_cleanly();
    assert!(
        probe.wait_exit(Duration::from_secs(20)).is_some(),
        "TUI must exit after PTY secrecy flow: {}",
        probe.snapshot()
    );
}

/// Contract: `/code-review` opens the fullscreen browser against a temp git
/// dirty tree, enables mouse capture, supports tree selection/collapse and
/// scroll, closes on Esc, disables mouse capture, and restores composer focus.
#[test]
fn pty_code_review_tree_mouse_lifecycle() {
    let mut probe = PtyProbe::spawn_seeded(&["--model", "faux/faux-1"], 40, 140, |_home, cwd| {
        init_git_repo_with_diff(cwd);
    });
    assert!(
        await_entered(&probe),
        "TUI must start in git cwd {}: {}",
        probe.cwd_path().display(),
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(500));

    // Baseline: mouse capture should not be active before code-review.
    let pre = probe.snapshot();
    let mouse_before = mouse_capture_enabled(&pre);

    let open_at = probe.len();
    probe.clear_composer();
    probe.type_chars("/code-review");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(8), |live| {
                live.contains("/code-review")
            })
            .is_some(),
        "composer must show /code-review on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r");

    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(20), |live| {
                live.contains("Code review")
                    && (live.contains("focus:tree")
                        || live.contains("Esc/q close")
                        || live.contains("click fold/open")
                        || live.contains("No tracked changes"))
            })
            .is_some(),
        "code-review page must open on the live screen: {}",
        probe.live_screen()
    );

    // Dirty tree fixtures should surface file names on the live screen.
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(8), |live| {
                ["README.md", "main.rs", "deep.rs", "src", "file"]
                    .iter()
                    .any(|needle| live.contains(needle))
            })
            .is_some(),
        "code-review should list dirty files on the live screen: {}",
        probe.live_screen()
    );

    // Mouse capture must be enabled while the page is open. Mouse capture is a
    // monotonic terminal control sequence, so the raw stream is the right place
    // to confirm the enable set was emitted.
    assert!(
        probe.wait_for(MOUSE_ENABLE_SGR, Duration::from_secs(10))
            && {
                let snap = probe.snapshot();
                mouse_capture_enabled(&snap) || snap.contains(MOUSE_ENABLE_NORMAL)
            },
        "code-review must enable mouse capture (was_enabled_before={mouse_before}): {}",
        probe
            .snapshot()
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
    );
    let mouse_open_at = probe
        .rfind_absolute(MOUSE_ENABLE_SGR)
        .or_else(|| probe.rfind_absolute(MOUSE_ENABLE_NORMAL))
        .unwrap_or(open_at);

    // Keyboard tree navigation: move and collapse/expand.
    probe.send(b"j"); // down
    thread::sleep(Duration::from_millis(120));
    probe.send(b"k"); // up
    thread::sleep(Duration::from_millis(120));
    probe.send(b"h"); // collapse / parent
    thread::sleep(Duration::from_millis(150));
    probe.send(b"l"); // expand / focus diff
    thread::sleep(Duration::from_millis(150));
    // Space toggles directory collapse when on a dir row.
    probe.send(b" ");
    thread::sleep(Duration::from_millis(120));
    probe.send(b" ");
    thread::sleep(Duration::from_millis(120));

    // Scroll keys on tree / after Tab on diff.
    probe.send(b"\t"); // focus diff
    thread::sleep(Duration::from_millis(100));
    let _ = probe.wait_for_live_after(open_at, Duration::from_secs(5), |live| {
        live.contains("focus:diff")
    });
    probe.send(b"\x1b[B"); // Down
    thread::sleep(Duration::from_millis(80));
    probe.send(b"\x1b[6~"); // PageDown
    thread::sleep(Duration::from_millis(80));
    probe.send(b"\x1b[5~"); // PageUp
    thread::sleep(Duration::from_millis(80));
    probe.send(b"\t"); // back to tree
    thread::sleep(Duration::from_millis(80));

    // SGR mouse: wheel and click inside the approximate tree pane (left side).
    // Layout places the tree on the left ~32% of the body; col 5,row 5 is inside
    // the fullscreen page for our 40x140 geometry.
    probe.send(b"\x1b[<64;5;5M"); // wheel up
    thread::sleep(Duration::from_millis(60));
    probe.send(b"\x1b[<65;5;8M"); // wheel down
    thread::sleep(Duration::from_millis(60));
    probe.send(b"\x1b[<0;5;6M"); // press
    probe.send(b"\x1b[<0;5;6m"); // release
    thread::sleep(Duration::from_millis(120));

    // Page must still be open after mouse/keyboard noise — assert against the
    // live screen, not the raw paint history which retains erased frames.
    let mid = probe.live_screen();
    assert!(
        mid.contains("Code review")
            || mid.contains("focus:tree")
            || mid.contains("focus:diff")
            || mid.contains("Esc/q close")
            || mid.contains("click fold/open"),
        "code-review must remain open after nav/mouse: {mid}"
    );

    // Close and require every page marker to leave the live screen.
    let close_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(close_at, Duration::from_secs(10), |live| {
                !live.contains("Code review")
                    && !live.contains("focus:tree")
                    && !live.contains("focus:diff")
                    && !live.contains("click fold/open")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Esc must close code-review on the live screen: {}",
        probe.live_screen()
    );

    // Mouse capture restore is a monotonic control sequence: confirm the
    // disable set was emitted after the enable (raw stream, persistent).
    let after_close = probe.snapshot();
    assert!(
        mouse_capture_disabled_after(&probe, mouse_open_at)
            || (after_close.contains(MOUSE_DISABLE_SGR)
                && after_close.contains(MOUSE_DISABLE_NORMAL)),
        "closing code-review must disable mouse capture: tail={}",
        after_close.chars().rev().take(500).collect::<String>()
    );

    // Composer focus restored (bracketed paste → one contiguous live render;
    // per-key typing interleaves cursor/separator cells in the raw stream).
    thread::sleep(Duration::from_millis(200));
    let focus_at = probe.len();
    probe.bracketed_paste("post-review");
    assert!(
        probe
            .wait_for_live_after(focus_at, Duration::from_secs(8), |live| {
                live.contains("post-review")
            })
            .is_some(),
        "composer must accept input after code-review close: {}",
        probe.live_screen()
    );
    probe.clear_composer();

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit after code-review flow: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership after code-review"
    );
    // Final cleanup should not leave mouse capture enabled without a matching disable.
    let last_enable = snap.rfind(MOUSE_ENABLE_NORMAL);
    let last_disable = snap.rfind(MOUSE_DISABLE_NORMAL);
    if let (Some(en), Some(dis)) = (last_enable, last_disable) {
        assert!(
            dis > en,
            "last mouse disable must follow last enable on clean exit"
        );
    }
}

/// Contract: `/code-review <from> <to>` resolves two stable branch refs to a
/// commit-to-commit diff, renders the `<from> → <to>` comparison label and the
/// single committed-only file (no working-tree dirty files), closes on Esc,
/// and exits cleanly. Bare `/code-review` lifecycle is covered by
/// [`pty_code_review_tree_mouse_lifecycle`].
#[test]
fn pty_code_review_two_revision_range() {
    let mut probe = PtyProbe::spawn_seeded(&["--model", "faux/faux-1"], 40, 140, |_home, cwd| {
        init_git_repo_with_diff(cwd);
    });
    assert!(
        await_entered(&probe),
        "TUI must start in git cwd {}: {}",
        probe.cwd_path().display(),
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(500));

    // Type the two-revision form; the composer must echo the full slash line
    // before submission, proving argument plumbing reaches the panel dispatch.
    let open_at = probe.len();
    probe.clear_composer();
    probe.type_chars("/code-review review-base review-target");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(8), |live| {
                live.contains("/code-review review-base review-target")
            })
            .is_some(),
        "composer must show the two-revision command on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r");

    // The panel opens and the title bar renders the comparison label with the
    // user's verbatim ref tokens joined by `→` (U+2192).
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(20), |live| {
                live.contains("Code review") && live.contains("review-base → review-target")
            })
            .is_some(),
        "code-review title must show the comparison label `review-base → review-target`: {}",
        probe.live_screen()
    );

    // Commit-to-commit diff: only the file committed between the two branches
    // appears. The dirty working-tree files (README.md / main.rs / deep.rs) are
    // not part of either commit. The dirty deep.rs body token `expanded` only
    // exists in the working tree, so its absence proves the panel is showing
    // the commit-to-commit diff rather than the HEAD→working-tree snapshot.
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(12), |live| {
                live.contains("review_committed")
            })
            .is_some(),
        "code-review must list the committed-only file `review_committed.md` on the live screen: {}",
        probe.live_screen()
    );

    // Settle before the absence check so we are not racing an earlier bare-diff
    // paint rather than the settled commit-to-commit render.
    thread::sleep(Duration::from_millis(400));
    let live = probe.live_screen();
    assert!(
        !live.contains("expanded"),
        "commit-to-commit diff must not include working-tree dirty content (`expanded`): {}",
        live
    );

    // Close the overlay and require every page marker to leave the live screen.
    let close_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(close_at, Duration::from_secs(10), |live| {
                !live.contains("Code review")
                    && !live.contains("review-base → review-target")
                    && !live.contains("focus:tree")
                    && !live.contains("focus:diff")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Esc must close the two-revision code-review page on the live screen: {}",
        probe.live_screen()
    );

    // Composer focus restored after close.
    thread::sleep(Duration::from_millis(200));
    let focus_at = probe.len();
    probe.bracketed_paste("post-range");
    assert!(
        probe
            .wait_for_live_after(focus_at, Duration::from_secs(8), |live| {
                live.contains("post-range")
            })
            .is_some(),
        "composer must accept input after two-revision code-review close: {}",
        probe.live_screen()
    );
    probe.clear_composer();

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit after two-revision code-review flow: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership after two-revision code-review"
    );
}


// Failure-exit cleanup after `/btw` (commit/draw/input error once the side
// controller is live) is NOT covered here. The only TUI test-only fault seam
// is `PI_TEST_PANIC_AFTER_ENTER` in `tui.rs` (fires immediately after
// `TerminalGuard::enter`, before the event loop), so it cannot open `/btw`
// first. Missing seams that would enable a PTY regression:
// - `PI_TEST_PANIC_AFTER_SIDE_CHAT` (or equivalent) once `side_chat.is_some()`
// - `PI_TEST_FAIL_COMMIT_SETTLED` / `PI_TEST_FAIL_DRAW` on the next loop tick
// - `PI_TEST_FAIL_NEXT_INPUT` after the first EventStream read
// Until one of those exists, controller `shutdown()` E2E
// (`side_chat_e2e.rs`) plus reviewer path-trace cover exit cleanup; panic
// terminal restore remains in `terminal_lifecycle.rs` (pre-side-chat only).
/// Contract: `/btw` opens the side-chat overlay with read-only chrome; a typed
/// or pasted side draft stays in the side editor; Esc closes the idle overlay
/// while keeping the controller; reopen shows the persisted draft; quit restores
/// terminal ownership.
#[test]
fn pty_btw_side_chat_open_paste_esc_reopen_persist() {
    let mut probe = PtyProbe::spawn(&["--model", "faux/faux-1"]);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        probe.snapshot()
    );
    thread::sleep(Duration::from_millis(400));

    let open_at = probe.len();
    probe.clear_composer();
    probe.type_chars("/btw");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(8), |live| {
                live.contains("/btw")
            })
            .is_some(),
        "composer must show /btw on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(20), |live| {
                live.contains("Side chat")
                    && (live.contains("read-only")
                        || live.contains("read only")
                        || live.contains("Ctrl+T")
                        || live.contains("Esc close"))
            })
            .is_some(),
        "side-chat overlay must open with read-only chrome on the live screen: {}",
        probe.live_screen()
    );
    let live_open = probe.live_screen();
    assert!(
        live_open.contains("Side chat")
            && (live_open.contains("read-only")
                || live_open.contains("read only")
                || live_open.contains("Ctrl+T")),
        "read-only side-chat chrome must be visible on the live screen: {live_open}"
    );

    // Draft into the side editor (not main composer). Prefer bracketed paste so
    // multi-byte tokens land as one Event::Paste into handle_side_chat paste
    // path and paint as a contiguous live row.
    let draft = "btw-side-draft-persist-e2e";
    let draft_at = probe.len();
    probe.bracketed_paste(draft);
    let draft_landed = probe
        .wait_for_live_after(draft_at, Duration::from_secs(8), |live| {
            live.contains("Side chat") && live.contains(draft)
        })
        .is_some();
    if !draft_landed {
        // Fallback: per-key type into the side editor, then confirm on the live
        // screen (per-key typing interleaves cursor cells in the raw stream).
        probe.type_chars(draft);
    }
    assert!(
        probe
            .wait_for_live_after(draft_at, Duration::from_secs(8), |live| {
                live.contains("Side chat") && live.contains(draft)
            })
            .is_some(),
        "side draft must appear in the overlay editor on the live screen: {}",
        probe.live_screen()
    );

    // Esc closes the idle overlay while retaining its controller.
    let close_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(close_at, Duration::from_secs(12), |live| {
                !live.contains("Side chat")
                    && !live.contains("Ctrl+T edit")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Esc must close the idle side-chat overlay on the live screen: {}",
        probe.live_screen()
    );
    thread::sleep(Duration::from_millis(200));

    // Main composer accepts input while side session remains in memory
    // (bracketed paste → one contiguous live render).
    let main_at = probe.len();
    probe.bracketed_paste("main-after-btw");
    assert!(
        probe
            .wait_for_live_after(main_at, Duration::from_secs(8), |live| {
                live.contains("main-after-btw")
            })
            .is_some(),
        "main composer must work after side-chat Esc: {}",
        probe.live_screen()
    );
    probe.clear_composer();

    // Reopen /btw — controller reuse must restore the side draft.
    let reopen_at = probe.len();
    probe.type_chars("/btw");
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(reopen_at, Duration::from_secs(15), |live| {
                live.contains("Side chat")
                    && (live.contains("Ctrl+T edit") || live.contains("Esc close"))
            })
            .is_some(),
        "reopen must show side-chat overlay on the live screen: {}",
        probe.live_screen()
    );
    assert!(
        probe
            .wait_for_live_after(reopen_at, Duration::from_secs(12), |live| {
                live.contains("Side chat") && live.contains(draft)
            })
            .is_some(),
        "reopened side chat must retain the prior side draft ({draft}) on the live screen: {}",
        probe.live_screen()
    );

    // Close overlay again (live-verified), then quit cleanly.
    let reclose_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(reclose_at, Duration::from_secs(8), |live| {
                !live.contains("Side chat") && !any_overlay_open(live)
            })
            .is_some(),
        "Esc must close the reopened side-chat overlay on the live screen: {}",
        probe.live_screen()
    );

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit after /btw flow: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership after /btw"
    );
}

/// Contract: public session-tree label edit → Pi v3 JSONL label record on the
/// selected non-root/current entry → resume shows `[label]` → clear removes it.
///
/// Drives real rpi over PTY only (no Application/session_store/private TUI
/// handlers). Catches wrong target, dropped persistence, ignored clear, and
/// resume display loss. Every wait is hard-bounded; child is killed/reaped on
/// Drop and between resume phases.
#[test]
fn pty_session_tree_label_edit_clear_jsonl_round_trip() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = home.path().join("tui-label-sessions");
    let session_id = "tui-label-roundtrip";
    let session_path = plant_branched_session(&session_dir, cwd.path(), session_id);
    let session_dir_arg = session_dir.to_str().expect("session dir utf8").to_owned();
    let label = "e2e-label-SIGIL-7f3a";
    let target_id = "u-sibling";

    let args = [
        "--model",
        "faux/faux-1",
        "--session-dir",
        session_dir_arg.as_str(),
        "--resume",
        session_id,
    ];

    // ── Phase 1: open tree, label the sibling branch, persist, quit ─────────
    let mut probe = PtyProbe::spawn_in(&args, 40, 140, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must resume branched session: {}",
        strip_ansi(&probe.snapshot())
    );
    // Transcript should surface the active leaf on the live screen (proves
    // resume loaded history); a raw search could match the pre-TUI build log.
    let _ = probe.wait_for_live(Duration::from_secs(10), |live| {
        live.contains("active-leaf-gamma")
            || live.contains("reply-leaf-gamma")
            || live.contains("branch-root-alpha")
    });
    thread::sleep(Duration::from_millis(400));

    let open_at = open_session_tree(&mut probe);
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(10), |live| {
                live.contains("Session Tree")
                    && (live.contains("branch-root-alpha")
                        || live.contains("sibling-branch-beta")
                        || live.contains("active-leaf-gamma")
                        || live.contains("reply-root-alpha"))
            })
            .is_some(),
        "session tree must list branched entries on the live screen: {}",
        probe.live_screen()
    );

    select_sibling_branch_entry(&mut probe);

    let edit_at = probe.len();
    send_tree_edit_label(&mut probe);
    assert!(
        probe
            .wait_for_live_after(edit_at, Duration::from_secs(10), |live| {
                live.contains("Label (empty to remove):")
            })
            .is_some(),
        "alt+shift+l must open label editor on the selected entry: {}",
        probe.live_screen()
    );

    // Editor starts empty for unlabeled nodes; type the distinctive label.
    // Per-key typing interleaves cursor cells in the raw stream, so assert
    // against the replayed live screen where the label paints contiguously.
    let type_at = probe.len();
    probe.type_chars(label);
    assert!(
        probe
            .wait_for_live_after(type_at, Duration::from_secs(8), |live| {
                live.contains("Label (empty to remove):") && live.contains(label)
            })
            .is_some(),
        "label text must appear in the editor on the live screen: {}",
        probe.live_screen()
    );
    probe.send(b"\r"); // commit

    // After save the panel reloads; label renders as `[label]` beside the entry.
    assert!(
        probe
            .wait_for_live_after(edit_at, Duration::from_secs(12), |live| {
                live.contains("Session Tree")
                    && live.contains(&format!("[{label}]"))
                    && live.contains("sibling-branch-beta")
            })
            .is_some(),
        "saved label must decorate the sibling branch entry on the live screen: {}",
        probe.live_screen()
    );
    // Dismiss tree (live-verified) and quit so the recorder flushes cleanly.
    let dismiss_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(dismiss_at, Duration::from_secs(10), |live| {
                !live.contains("Session Tree")
                    && !live.contains("Type to search:")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Esc must close the session tree on the live screen: {}",
        probe.live_screen()
    );
    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit after label save: {}",
        probe.snapshot()
    );

    // On-disk JSONL must carry a label record targeting the sibling entry.
    let records_after_set = read_jsonl_records(&session_path);
    let set_labels = label_records_for(&records_after_set, target_id);
    assert!(
        !set_labels.is_empty(),
        "JSONL must contain a label record for {target_id}: {records_after_set:?}"
    );
    let last_set = set_labels
        .last()
        .expect("non-empty label records for target");
    assert_eq!(
        last_set.get("label").and_then(Value::as_str),
        Some(label),
        "label record must store the committed text for {target_id}: {last_set}"
    );
    // Guard against wrong-target bugs: no other entry should carry this label.
    let wrong_target = records_after_set.iter().any(|record| {
        record.get("type").and_then(Value::as_str) == Some("label")
            && record.get("label").and_then(Value::as_str) == Some(label)
            && record.get("targetId").and_then(Value::as_str) != Some(target_id)
    });
    assert!(
        !wrong_target,
        "label must not be written against a non-selected target: {records_after_set:?}"
    );

    let (home, cwd) = probe.shutdown_take_dirs();

    // ── Phase 2: resume and assert label is still visible on the tree ───────
    let mut probe = PtyProbe::spawn_in(&args, 40, 140, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must resume after label save: {}",
        strip_ansi(&probe.snapshot())
    );
    thread::sleep(Duration::from_millis(400));

    let reopen_at = open_session_tree(&mut probe);
    assert!(
        probe
            .wait_for_live_after(reopen_at, Duration::from_secs(12), |live| {
                live.contains("Session Tree")
                    && live.contains(&format!("[{label}]"))
                    && live.contains("sibling-branch-beta")
            })
            .is_some(),
        "resumed session tree must show persisted label [{label}] on the live screen: {}",
        probe.live_screen()
    );

    // ── Phase 3: clear the label through the same public editor ─────────────
    select_sibling_branch_entry(&mut probe);
    let clear_at = probe.len();
    send_tree_edit_label(&mut probe);
    assert!(
        probe
            .wait_for_live_after(clear_at, Duration::from_secs(10), |live| {
                live.contains("Label (empty to remove):")
            })
            .is_some(),
        "label editor must reopen for clear on the live screen: {}",
        probe.live_screen()
    );
    // Existing label is prefilled — backspace it away (empty commits a clear).
    for _ in 0..(label.len() + 4) {
        probe.send(&[0x7f]); // DEL/Backspace
        thread::sleep(Duration::from_millis(25));
    }
    // Also try ASCII BS in case the terminal maps differently.
    for _ in 0..(label.len() + 2) {
        probe.send(&[0x08]);
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(100));
    // Marker taken at commit time — editor/backspace frames can still repaint
    // `[label]` in the raw history, so the cleared check must use the live
    // screen (which only reflects the final post-commit repaint).
    let clear_commit_at = probe.len();
    probe.send(b"\r"); // commit empty → clear

    // Reloaded tree must stop showing the old bracket label on post-commit frames.
    assert!(
        probe
            .wait_for_live_after(clear_commit_at, Duration::from_secs(12), |live| {
                (live.contains("Session Tree") || live.contains("Type to search:"))
                    && !live.contains(&format!("[{label}]"))
            })
            .is_some(),
        "clearing the label must remove [{label}] from the live session-tree surface: {}",
        probe.live_screen()
    );
    let dismiss_clear_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(dismiss_clear_at, Duration::from_secs(10), |live| {
                !live.contains("Session Tree")
                    && !live.contains("Type to search:")
                    && !any_overlay_open(live)
            })
            .is_some(),
        "Esc must close the session tree after clear on the live screen: {}",
        probe.live_screen()
    );
    probe.quit_cleanly();
    assert!(
        probe.wait_exit(Duration::from_secs(20)).is_some(),
        "TUI must exit after label clear: {}",
        probe.snapshot()
    );

    let records_after_clear = read_jsonl_records(&session_path);
    let clear_labels = label_records_for(&records_after_clear, target_id);
    assert!(
        clear_labels.len() >= 2,
        "JSONL must append a clear record after the set record for {target_id}: {records_after_clear:?}"
    );
    let last_clear = clear_labels
        .last()
        .expect("clear label records");
    assert!(
        last_clear.get("label").is_none()
            || last_clear
                .get("label")
                .and_then(Value::as_str)
                .is_some_and(str::is_empty),
        "clear record must omit/empty the label field: {last_clear}"
    );

    let (home, cwd) = probe.shutdown_take_dirs();

    // ── Phase 4: resume again and prove the label stays gone ────────────────
    let mut probe = PtyProbe::spawn_in(&args, 40, 140, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must resume after label clear: {}",
        strip_ansi(&probe.snapshot())
    );
    thread::sleep(Duration::from_millis(400));

    let final_at = open_session_tree(&mut probe);
    assert!(
        probe
            .wait_for_live_after(final_at, Duration::from_secs(12), |live| {
                live.contains("Session Tree")
                    && (live.contains("sibling-branch-beta")
                        || live.contains("branch-root-alpha"))
            })
            .is_some(),
        "final resume must open session tree on the live screen: {}",
        probe.live_screen()
    );
    let final_live = probe.live_screen();
    assert!(
        !final_live.contains(&format!("[{label}]")) && !final_live.contains(label),
        "cleared label must stay absent from the live screen after reopen: {final_live}"
    );
    assert!(
        final_live.contains("sibling-branch-beta"),
        "sibling branch entry must still be listed without a label: {final_live}"
    );

    let final_dismiss_at = probe.len();
    probe.send(&[ESC]);
    let _ = probe.wait_for_live_after(final_dismiss_at, Duration::from_secs(6), |live| {
        !live.contains("Session Tree")
    });
    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit after final resume: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership after label round-trip"
    );
}

/// Documents the unix gate for this module on platforms that compile it.
#[cfg(unix)]
#[test]
fn core_tui_e2e_module_is_unix_gated() {
    assert!(cfg!(unix));
}
