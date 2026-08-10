//! End-to-end tests for the `/goal` and `/loop` slash features, driving the
//! real `rpi` binary with an isolated temp HOME and the built-in faux model
//! (`PI_FAUX_RESPONSE`), no network, no credentials.
//!
//! These tests codify the live-verified walkthroughs (T06: goal full
//! lifecycle/budget/persistence via tmux; T05: loop surface):
//!
//! * **REPL harness** — `rpi --mode text` with piped stdin/stdout/stderr.
//!   Slash commands run through the real `repl.rs` dispatch and print their
//!   formatted output lines; errors go to stderr as `rpi: <message>`. This is
//!   the deterministic workhorse for the lifecycle, drop, budget, persistence,
//!   and loop scenarios. Every process runs on the DEFAULT session path under
//!   a fresh temp HOME: this exercises the startup TTL prune against the
//!   just-started recorder's still-empty per-cwd directory (regression: the
//!   prune used to delete it before the first durable goal write, failing
//!   with ENOENT).
//! * **Budget crossing** — the REPL runs with `--listen 127.0.0.1:0`; the test
//!   POSTs `goal_update_usage` over the HTTP control plane (the same
//!   `GoalRuntime::update_usage` pipeline a finished goal turn charges through)
//!   and observes the goal auto-pause on budget exhaustion, resume rejection,
//!   and drop from the exhausted state — exactly the T06 Phase-2 flow.
//! * **Persistence** — restart the binary in the same temp HOME/cwd with
//!   `--continue`; the goal journal is replayed from the session tree, so the
//!   completed goal survives.
//! * **PTY harness** — a real pseudoterminal drives the TUI: bare `/goal`
//!   opens the Create/Show panel, the panel Enter flow creates a goal, and the
//!   footer goal chip renders `🎯 Goal N/M` and transitions `⏸`/`🎯`/`✓` across
//!   pause/resume/complete (T06's footer-verified behavior). Ratatui paints
//!   cells with explicit cursor moves, so assertions run against a small ANSI
//!   screen replay ([`Screen`]) — the reconstructed visible terminal — with
//!   space runs collapsed to match what a user actually sees.
//!
//! Loop coverage mirrors the verified behavior: `/loop <interval> <prompt>`
//! creates and fires (next-fire timestamp advances), `/loops` and `/loop list`
//! list, `/loop cancel <id>` removes (Cancelled), `/loop delete <id>` removes,
//! and missing/unknown ids fail with the canonical usage/not-found errors.
//!
//! Everything is bounded: every wait has a deadline, the faux model replies
//! deterministically, and sleeps between goal turns are fixed and generous.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::Termios;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Deterministic offline assistant text for every turn the faux model runs
/// (goal-work turns, loop turns). Never contains `> ` so the REPL prompt
/// detection stays unambiguous.
const FAUX_RESPONSE: &str = "e2e-faux-reply";

const HIDE_CURSOR: &str = "\x1b[?25l";
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

// ---------------------------------------------------------------------------
// REPL harness: real `rpi` binary, `--mode text`, piped stdio.
// ---------------------------------------------------------------------------

/// A running `rpi --mode text` REPL with piped stdio. stdout/stderr are drained
/// into shared buffers by background threads; `command()` writes one line and
/// returns everything the REPL printed before the next `> ` prompt.
struct ReplProbe {
    child: Child,
    stdin: ChildStdin,
    stdout_buffer: Arc<Mutex<String>>,
    stderr_buffer: Arc<Mutex<String>>,
    /// Absolute stdout offset just past the last consumed `> ` prompt.
    cursor: usize,
}

fn pump<R: Read + Send + 'static>(reader: R, buffer: Arc<Mutex<String>>) {
    thread::spawn(move || {
        let mut reader = reader;
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut guard) = buffer.lock() {
                        guard.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
                Err(_) => break,
            }
        }
    });
}

impl ReplProbe {
    /// Spawn `rpi --mode text` on the default session path.
    ///
    /// Deliberately NO `--session-dir`: the default per-cwd session directory
    /// under a fresh temp HOME is the regression surface — the startup TTL
    /// prune must not delete the just-started recorder's still-empty
    /// directory, and the first durable goal write must succeed.
    fn spawn(home: &Path, cwd: &Path, args: &[&str]) -> Self {
        let mut cmd = Command::new(rpi_bin());
        cmd.env_clear();
        cmd.env("HOME", home);
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("TERM", "xterm-256color");
        cmd.env("PI_OFFLINE", "1");
        cmd.env("PI_SKIP_VERSION_CHECK", "1");
        cmd.env("PI_FAUX_RESPONSE", FAUX_RESPONSE);
        cmd.args(args);
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn rpi REPL");
        let stdin = child.stdin.take().expect("repl stdin pipe");
        let stdout = child.stdout.take().expect("repl stdout pipe");
        let stderr = child.stderr.take().expect("repl stderr pipe");
        let stdout_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        pump(stdout, stdout_buffer.clone());
        pump(stderr, stderr_buffer.clone());
        Self {
            child,
            stdin,
            stdout_buffer,
            stderr_buffer,
            cursor: 0,
        }
    }

    fn stdout_snapshot(&self) -> String {
        self.stdout_buffer.lock().expect("stdout lock").clone()
    }

    fn stderr_snapshot(&self) -> String {
        self.stderr_buffer.lock().expect("stderr lock").clone()
    }

    fn send_line(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .expect("repl stdin write");
        self.stdin.write_all(b"\n").expect("repl stdin newline");
        self.stdin.flush().expect("repl stdin flush");
    }

    /// Wait until the first `> ` prompt is painted (the REPL is ready to
    /// accept slash commands) and point the cursor just past it.
    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = self.stdout_snapshot();
            if let Some(pos) = snapshot.find("\n> ") {
                self.cursor = pos + 3;
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "rpi REPL never reached its prompt\nstdout: {snapshot:?}\nstderr: {:?}",
                    self.stderr_snapshot()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Send one slash command and return the stdout the REPL printed for it
    /// (everything up to the next `> ` prompt). Errors print to stderr instead
    /// and yield an empty region; assert on [`ReplProbe::wait_stderr`].
    fn command(&mut self, line: &str) -> String {
        self.send_line(line);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let snapshot = self.stdout_snapshot();
            let from = self.cursor;
            if let Some(rel) = snapshot.get(from..).and_then(|tail| tail.find("\n> ")) {
                let prompt_pos = from + rel;
                let region = snapshot[from..prompt_pos].to_owned();
                self.cursor = prompt_pos + 3;
                return region.trim_end().to_owned();
            }
            if Instant::now() >= deadline {
                panic!(
                    "rpi REPL timed out after {line:?}\nstdout: {snapshot:?}\nstderr: {:?}",
                    self.stderr_snapshot()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_stderr(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.stderr_snapshot().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
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

    /// `/quit` and require a clean exit.
    fn quit(mut self) {
        self.send_line("/quit");
        let status = self
            .wait_exit(Duration::from_secs(20))
            .expect("rpi REPL must exit after /quit");
        assert!(
            status.success(),
            "rpi REPL exit after /quit: {status:?} stderr={:?}",
            self.stderr_snapshot()
        );
    }
}

impl Drop for ReplProbe {
    fn drop(&mut self) {
        // Backstop: never strand a child if a test failed before /quit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// PTY harness: real `rpi` TUI on a pseudoterminal (process_ps_pty pattern).
// ---------------------------------------------------------------------------

struct TuiProbe {
    child: Child,
    writer: std::fs::File,
    buffer: Arc<Mutex<String>>,
    _home: TempDir,
    _cwd: TempDir,
}

impl TuiProbe {
    fn spawn() -> Self {
        let home = TempDir::new().expect("temp HOME");
        let cwd = TempDir::new().expect("temp cwd");
        let winsize = Winsize {
            // Wide enough that the composer footer keeps the full goal chip
            // (`🎯 Goal 0/100`): the chip is the last footer field and the
            // renderer truncates it when the cwd/context fields crowd the row.
            ws_row: 40,
            ws_col: 200,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&winsize), None::<&Termios>).expect("openpty");
        let slave_in = pty.slave.try_clone().expect("clone slave stdin");
        let slave_out = pty.slave.try_clone().expect("clone slave stdout");
        let slave_err = pty.slave;

        // Default session path under the fresh temp HOME, like
        // [`ReplProbe::spawn`]: the startup TTL prune must not remove the
        // just-started recorder's still-empty per-cwd directory.
        let mut cmd = Command::new(rpi_bin());
        cmd.env_clear();
        cmd.env("HOME", home.path());
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("TERM", "xterm-256color");
        cmd.env("PI_OFFLINE", "1");
        cmd.env("PI_SKIP_VERSION_CHECK", "1");
        cmd.env("PI_FAUX_RESPONSE", FAUX_RESPONSE);
        cmd.args(["--model", "faux/faux-1"]);
        cmd.current_dir(cwd.path());
        cmd.stdin(Stdio::from(slave_in));
        cmd.stdout(Stdio::from(slave_out));
        cmd.stderr(Stdio::from(slave_err));

        let child = cmd.spawn().expect("spawn rpi TUI");
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
                        if let Ok(mut guard) = buf.lock() {
                            guard.push_str(&String::from_utf8_lossy(bytes));
                        }
                        // Ratatui inline viewport probes the cursor position
                        // (CSI 6n); a bare PTY has no emulator, so answer.
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

    fn snapshot(&self) -> String {
        self.buffer.lock().expect("buffer lock").clone()
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

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
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

impl Drop for TuiProbe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A tiny ANSI/VT screen replay for the TUI probe.
///
/// Ratatui paints table cells with explicit cursor moves (`\x1b[20;59HShow
/// \x1b[20;69Hdetails`), so multi-word strings are never contiguous in the raw
/// PTY stream even though the terminal *shows* them with spaces. Asserting on
/// the visible screen is therefore the faithful contract. The replay tracks a
/// character grid, absolute/relative cursor moves, line/screen erases, and
/// ignores SGR/mode/DSR sequences. Wide glyphs (🎯, 📁, …) occupy two cells,
/// matching ratatui's `unicode-width` buffer and the terminal, so the
/// unchanged cells that ratatui's diffing backend leaves unwritten land on the
/// correct columns (without this the 🎯→⏸ marker flip misaligns the budget
/// `0/100`); substring matching still works because relative character order
/// is preserved and whitespace runs collapse.
struct Screen {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
        }
    }

    fn feed(&mut self, input: &str) {
        let chars: Vec<char> = input.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            if ch == '\u{1b}' {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                match chars[i] {
                    '[' => {
                        i += 1;
                        let mut params = String::new();
                        while i < chars.len()
                            && (chars[i].is_ascii_digit()
                                || matches!(chars[i], ';' | '?' | ' ' | ':'))
                        {
                            params.push(chars[i]);
                            i += 1;
                        }
                        let final_byte = chars.get(i).copied().unwrap_or(' ');
                        i += 1;
                        match final_byte {
                            'H' | 'f' => {
                                let (r, c) = parse_csi_pos(&params);
                                self.row = r.saturating_sub(1).min(self.rows - 1);
                                self.col = c.saturating_sub(1).min(self.cols - 1);
                            }
                            'A' => {
                                self.row = self.row.saturating_sub(csi_param_int(&params, 1));
                            }
                            'B' => {
                                self.row = (self.row + csi_param_int(&params, 1)).min(self.rows - 1);
                            }
                            'C' => {
                                self.col = (self.col + csi_param_int(&params, 1)).min(self.cols - 1);
                            }
                            'D' => {
                                self.col = self.col.saturating_sub(csi_param_int(&params, 1));
                            }
                            'K' => {
                                let mode = csi_param_int(&params, 0);
                                self.erase_line(mode);
                            }
                            'J' => {
                                let mode = csi_param_int(&params, 0);
                                self.erase_screen(mode);
                            }
                            // SGR, mode sets, DSR reports, and unknown finals
                            // do not affect the character grid.
                            _ => {}
                        }
                    }
                    ']' => {
                        // OSC: skip to BEL or ST.
                        i += 1;
                        while i < chars.len() && chars[i] != '\u{7}' {
                            i += 1;
                        }
                        i += 1;
                    }
                    _ => {
                        // Lone ESC: ignore.
                        i += 1;
                    }
                }
            } else {
                match ch {
                    '\n' => {
                        self.row = (self.row + 1).min(self.rows - 1);
                    }
                    '\r' => {
                        self.col = 0;
                    }
                    _ => {
                        if self.row < self.rows && self.col < self.cols {
                            let width = glyph_width(ch);
                            self.grid[self.row][self.col] = ch;
                            // Wide glyphs occupy a second cell in the terminal;
                            // mark it so the replayed grid matches the
                            // terminal's column layout and later absolute
                            // cursor moves overwrite the whole glyph. Without
                            // this the 🎯(wide)→⏸(narrow) marker flip left the
                            // unchanged budget cells `0/100` misaligned with
                            // the active chip, collapsing to `0/1 0`.
                            for offset in 1..width {
                                if self.col + offset < self.cols {
                                    self.grid[self.row][self.col + offset] = ' ';
                                }
                            }
                            self.col += width;
                            if self.col >= self.cols {
                                self.col = self.cols - 1;
                            }
                        }
                    }
                }
                i += 1;
            }
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let row = self.row;
        match mode {
            1 => {
                for cell in self.grid[row].iter_mut().take(self.col + 1) {
                    *cell = ' ';
                }
            }
            2 => self.grid[row].fill(' '),
            _ => {
                for cell in self.grid[row].iter_mut().skip(self.col) {
                    *cell = ' ';
                }
            }
        }
    }

    fn erase_screen(&mut self, mode: usize) {
        match mode {
            2 | 3 => {
                for row in &mut self.grid {
                    row.fill(' ');
                }
            }
            _ => {
                self.erase_line(0);
                for row in self.grid.iter_mut().skip(self.row + 1) {
                    row.fill(' ');
                }
            }
        }
    }

    /// The visible text: rows joined by newlines, trailing blanks trimmed and
    /// runs of spaces collapsed (paint fragments leave unwritten gap cells, so
    /// the raw-cell row can carry double spaces the terminal renders as one).
    fn text(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `20;59` from a CSI parameter list; missing values default to 1.
fn parse_csi_pos(params: &str) -> (usize, usize) {
    let mut parts = params.split(';');
    let row = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
    let col = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
    (row, col)
}

/// First integer parameter of a CSI sequence, or the provided default.
fn csi_param_int(params: &str, default: usize) -> usize {
    params
        .split(';')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default)
}

/// Terminal column width of a glyph, matching xterm-256color's non-CJK
/// default: emoji pictographs in the Supplementary Multilingual Plane (🎯, 📁,
/// …) are double-width, while the narrow ambiguous-width symbols used in the
/// composer footer (⏸, ▶, ✓, ✗, ─, ◫, ⬢, ◑) stay single-width. The replay
/// grid models wide glyphs as two cells so it aligns with ratatui's buffer
/// (which uses the same `unicode-width` model) and the unchanged cells that
/// ratatui's diffing backend leaves unwritten land on the correct columns.
fn glyph_width(ch: char) -> usize {
    let code = ch as u32;
    if (0x1F300..=0x1FAFF).contains(&code) {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// HTTP control-plane client for the budget-crossing scenario.
// ---------------------------------------------------------------------------

/// POST one JSON-RPC-ish command to the `--listen` control plane and return the
/// `RpcResponse` envelope (JSON body; the server sends `connection: close`).
fn http_rpc(addr: &str, body: &Value) -> Value {
    let mut stream = TcpStream::connect(addr).expect("connect control plane");
    let payload = serde_json::to_string(body).expect("serialize rpc body");
    let request = format!(
        "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("write rpc request");
    stream.flush().expect("flush rpc request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read rpc response");
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .expect("rpc response body");
    serde_json::from_str(body).expect("parse rpc response")
}

/// `127.0.0.1:PORT` parsed from the `Control plane listening on http://…`
/// banner the REPL prints to stderr.
fn control_plane_addr(stderr: &str) -> String {
    const MARKER: &str = "Control plane listening on http://";
    let start = stderr
        .find(MARKER)
        .expect("control plane banner on stderr")
        + MARKER.len();
    let end = stderr[start..]
        .find(' ')
        .expect("control plane address terminator")
        + start;
    stderr[start..end].to_owned()
}

// ---------------------------------------------------------------------------
// RFC3339 helper for loop fire-advance assertions (chrono emits
// `…T12:34:56.123456789+00:00` for `DateTime<Utc>::to_rfc3339`).
// ---------------------------------------------------------------------------

fn rfc3339_millis(value: &str) -> i64 {
    assert!(value.len() >= 20, "malformed rfc3339: {value:?}");
    let bytes = value.as_bytes();
    assert!(
        bytes[4] == b'-' && bytes[7] == b'-' && bytes[10] == b'T'
            && bytes[13] == b':' && bytes[16] == b':',
        "malformed rfc3339: {value:?}"
    );
    let year: i64 = value[0..4].parse().expect("rfc3339 year");
    let month: u32 = value[5..7].parse().expect("rfc3339 month");
    let day: u32 = value[8..10].parse().expect("rfc3339 day");
    let hour: u32 = value[11..13].parse().expect("rfc3339 hour");
    let minute: u32 = value[14..16].parse().expect("rfc3339 minute");
    let second: u32 = value[17..19].parse().expect("rfc3339 second");
    let mut fraction_millis: i64 = 0;
    let mut rest = &value[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        assert!(!digits.is_empty(), "malformed fraction in {value:?}");
        let mut nanos: i64 = 0;
        for (index, ch) in digits.chars().enumerate() {
            if index < 9 {
                nanos = nanos * 10 + i64::from(ch as u8 - b'0');
            }
        }
        for _ in digits.len()..9 {
            nanos *= 10;
        }
        fraction_millis = nanos / 1_000_000;
        rest = &fraction[digits.len()..];
    }
    // Offset: only same-offset values are ever compared (chrono UTC emits
    // "+00:00"; accept "Z" defensively). Validate the shape, ignore magnitude.
    assert!(
        rest.starts_with('Z') || (rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-'))),
        "unexpected rfc3339 offset in {value:?}"
    );
    let days = days_from_civil(year, month, day);
    (days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))
        * 1_000
        + fraction_millis
}

/// Days since 1970-01-01 (Howard Hinnant's civil calendar algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = i64::from((153 * month_prime + 2) / 5 + day - 1);
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + i64::from(day_of_era) - 719_468
}

/// Extract `<id>` from a `scheduled <id> · every … · expires …` create line.
fn loop_id_from_create(output: &str) -> String {
    output
        .split("scheduled ")
        .nth(1)
        .and_then(|rest| rest.split(" · ").next())
        .expect("loop create line must carry an id")
        .trim()
        .to_owned()
}

/// The `next <rfc3339>` fire time of `id` in a `/loop list` / `/loops` line.
fn loop_next_millis(list_output: &str, id: &str) -> i64 {
    let line = list_output
        .lines()
        .find(|line| line.contains(id))
        .unwrap_or_else(|| panic!("loop {id:?} missing from list: {list_output:?}"));
    let parts: Vec<&str> = line.split("  ").collect();
    assert!(parts.len() >= 3, "loop list line: {line:?}");
    let next = parts[2]
        .strip_prefix("next ")
        .expect("loop list line must carry next-fire time");
    rfc3339_millis(next)
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Contract: the full goal lifecycle through the real REPL — bare `/goal`
/// reports no goal; create (with a token budget) activates the goal and starts
/// a goal-work turn; pause, resume (which runs another goal turn), and complete
/// transition the compact state line; every mutation on a completed goal fails
/// with the canonical lifecycle error; parse errors keep their exact messages.
#[test]
fn repl_goal_lifecycle_create_pause_resume_complete_and_invalid_transitions() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let mut repl =
        ReplProbe::spawn(home.path(), cwd.path(), &["--mode", "text", "--model", "faux/faux-1"]);
    repl.ready();

    let bare = repl.command("/goal");
    assert_eq!(bare.trim(), "no goal", "bare /goal without a goal: {bare:?}");

    let created = repl.command("/goal create --tokens 100 ship the widget");
    assert!(
        created.starts_with("Goal work started · active · 0/100 tokens · ship the widget"),
        "goal create output: {created:?}"
    );

    // Let the auto-started goal-work turn settle so the later resume starts a
    // fresh turn deterministically (fixed bounded wait; faux turns are local).
    thread::sleep(Duration::from_secs(2));

    let shown = repl.command("/goal show");
    assert_eq!(shown.trim(), "active · 0/100 tokens · ship the widget");
    let got = repl.command("/goal get");
    assert_eq!(got.trim(), "active · 0/100 tokens · ship the widget");

    // The session owns exactly one goal slot: a second create is rejected.
    repl.command("/goal create another objective");
    assert!(
        repl.wait_stderr("a current goal already exists", Duration::from_secs(5)),
        "duplicate create must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );

    let paused = repl.command("/goal pause");
    assert_eq!(paused.trim(), "paused · 0/100 tokens · ship the widget");

    thread::sleep(Duration::from_secs(1));

    let resumed = repl.command("/goal resume");
    assert!(
        resumed.starts_with("Goal work started · active · 0/100 tokens · ship the widget"),
        "goal resume output: {resumed:?}"
    );

    thread::sleep(Duration::from_secs(1));

    let completed = repl.command("/goal complete");
    assert_eq!(completed.trim(), "completed · 0/100 tokens · ship the widget");

    // Terminal lifecycle rejects every further mutation.
    repl.command("/goal pause");
    assert!(
        repl.wait_stderr(
            "cannot pause a goal in the Completed lifecycle",
            Duration::from_secs(5)
        ),
        "pause on completed must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );
    repl.command("/goal resume");
    assert!(
        repl.wait_stderr(
            "cannot resume a goal in the Completed lifecycle",
            Duration::from_secs(5)
        ),
        "resume on completed must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );
    repl.command("/goal drop");
    assert!(
        repl.wait_stderr(
            "cannot drop a goal in the Completed lifecycle",
            Duration::from_secs(5)
        ),
        "drop on completed must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );

    // Parse-level errors keep their exact messages even with a goal present.
    repl.command("/goal create");
    assert!(
        repl.wait_stderr("goal objective must not be empty", Duration::from_secs(5)),
        "empty create must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );
    repl.command("/goal create --tokens 0 x");
    assert!(
        repl.wait_stderr("--tokens requires a positive integer", Duration::from_secs(5)),
        "zero budget must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );
    repl.command("/goal pause extra");
    assert!(
        repl.wait_stderr(
            "usage: /goal [show|inspect|create [--tokens N] <objective>|pause|resume|complete|drop|pin <text>|pins|unpin <index>]",
            Duration::from_secs(5)
        ),
        "trailing args must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );

    let final_state = repl.command("/goal");
    assert_eq!(final_state.trim(), "completed · 0/100 tokens · ship the widget");

    repl.quit();
}

/// Contract: `/goal drop` moves an active goal to the terminal `dropped` state,
/// further mutations are rejected (complete on dropped), and the one-per-session
/// goal slot stays occupied forever (create is rejected).
#[test]
fn repl_goal_drop_terminates_goal_and_slot_stays_permanent() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let mut repl =
        ReplProbe::spawn(home.path(), cwd.path(), &["--mode", "text", "--model", "faux/faux-1"]);
    repl.ready();

    let created = repl.command("/goal create --tokens 5 fix the lights");
    assert!(
        created.starts_with("Goal work started · active · 0/5 tokens · fix the lights"),
        "goal create output: {created:?}"
    );
    thread::sleep(Duration::from_secs(2));

    let dropped = repl.command("/goal drop");
    assert_eq!(dropped.trim(), "dropped · 0/5 tokens · fix the lights");

    repl.command("/goal complete");
    assert!(
        repl.wait_stderr(
            "cannot complete a goal in the Dropped lifecycle",
            Duration::from_secs(5)
        ),
        "complete on dropped must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );

    repl.command("/goal create replacement");
    assert!(
        repl.wait_stderr("a current goal already exists", Duration::from_secs(5)),
        "create after drop must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );

    let shown = repl.command("/goal show");
    assert_eq!(shown.trim(), "dropped · 0/5 tokens · fix the lights");

    repl.quit();
}

/// Contract: a token budget is enforced through the real binary — create with
/// `--tokens 5`, charge 6 tokens through the control-plane `goal_update_usage`
/// RPC (the same `GoalRuntime::update_usage` pipeline finished goal turns
/// charge through), and the goal auto-pauses as `budget_exhausted`; resume is
/// rejected while the exhausted budget is immutable; drop still works from the
/// exhausted state and complete on dropped is rejected. Wire-level budget
/// validation (`tokenBudget: 0`) is rejected too.
#[test]
fn repl_goal_budget_usage_crossing_pauses_and_blocks_resume() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let mut repl = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        &[
            "--mode",
            "text",
            "--model",
            "faux/faux-1",
            "--listen",
            "127.0.0.1:0",
            "--listen-plaintext",
        ],
    );
    repl.ready();
    // The control-plane banner is printed to stderr before the REPL prompt.
    assert!(
        repl.wait_stderr(
            "Control plane listening on http://",
            Duration::from_secs(30)
        ),
        "control plane must announce its address, stderr: {:?}",
        repl.stderr_snapshot()
    );
    let addr = control_plane_addr(&repl.stderr_snapshot());

    let created = repl.command("/goal create --tokens 5 finish the bridge");
    assert!(
        created.starts_with("Goal work started · active · 0/5 tokens · finish the bridge"),
        "goal create output: {created:?}"
    );

    // Charge past the 5-token budget: active must auto-pause, never complete.
    let charge = http_rpc(
        &addr,
        &json!({"type": "goal_update_usage", "id": "budget-charge", "tokens": 6, "activeTimeSeconds": 1}),
    );
    assert_eq!(charge["success"], true, "charge response: {charge}");
    assert_eq!(charge["command"], "goal_update_usage");
    assert_eq!(charge["id"], "budget-charge");
    assert_eq!(charge["data"]["lifecycle"], "paused");
    assert_eq!(charge["data"]["pauseReason"], "budget_exhausted");
    assert_eq!(charge["data"]["tokenBudget"], 5);
    assert_eq!(charge["data"]["usage"]["tokensUsed"], 6);

    let shown = repl.command("/goal show");
    assert_eq!(shown.trim(), "paused · 6/5 tokens · finish the bridge");

    // Exhausted budgets are immutable: resume is rejected.
    let resume = http_rpc(&addr, &json!({"type": "goal_resume", "id": "budget-resume"}));
    assert_eq!(resume["success"], false, "resume must fail: {resume}");
    assert_eq!(resume["command"], "goal_resume");
    assert!(
        resume["error"]
            .as_str()
            .expect("resume failure error")
            .contains("cannot resume a goal after its token budget is exhausted"),
        "resume failure message: {resume}"
    );

    // Runtime budget validation on the wire path.
    let zero = http_rpc(
        &addr,
        &json!({"type": "goal_create", "id": "budget-zero", "objective": "x", "tokenBudget": 0}),
    );
    assert_eq!(zero["success"], false, "zero budget must fail: {zero}");
    assert!(
        zero["error"]
            .as_str()
            .expect("zero budget failure error")
            .contains("goal token budget must be positive"),
        "zero budget failure message: {zero}"
    );

    let dropped = repl.command("/goal drop");
    assert_eq!(dropped.trim(), "dropped · 6/5 tokens · finish the bridge");

    let complete = http_rpc(
        &addr,
        &json!({"type": "goal_complete", "id": "budget-complete"}),
    );
    assert_eq!(complete["success"], false, "complete must fail: {complete}");
    assert!(
        complete["error"]
            .as_str()
            .expect("complete failure error")
            .contains("cannot complete a goal in the Dropped lifecycle"),
        "complete failure message: {complete}"
    );

    repl.quit();
}

/// Contract: the goal survives a process restart in the same temp HOME/cwd —
/// `--continue` resumes the most recent session, the append-only goal journal
/// is replayed from the session tree, and the terminal `completed` state (with
/// objective and budget) comes back exactly as left.
#[test]
fn repl_goal_state_survives_restart_through_journal_replay() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");

    {
        let mut repl = ReplProbe::spawn(
            home.path(),
            cwd.path(),
            &["--mode", "text", "--model", "faux/faux-1"],
        );
        repl.ready();
        let created = repl.command("/goal create --tokens 100 persist the goal");
        assert!(
            created.starts_with("Goal work started · active · 0/100 tokens · persist the goal"),
            "goal create output: {created:?}"
        );
        // Let the goal-work turn settle before exiting so the journal is idle.
        thread::sleep(Duration::from_secs(2));
        let completed = repl.command("/goal complete");
        assert_eq!(completed.trim(), "completed · 0/100 tokens · persist the goal");
        repl.quit();
    }

    let mut resumed = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        &[
            "--mode",
            "text",
            "--model",
            "faux/faux-1",
            "--continue",
        ],
    );
    resumed.ready();
    assert!(
        resumed.wait_stderr("resumed", Duration::from_secs(10)),
        "restart must resume the recorded session, stderr: {:?}",
        resumed.stderr_snapshot()
    );

    let shown = resumed.command("/goal show");
    assert_eq!(shown.trim(), "completed · 0/100 tokens · persist the goal");
    let bare = resumed.command("/goal");
    assert_eq!(bare.trim(), "completed · 0/100 tokens · persist the goal");

    resumed.quit();
}

/// Contract: an ACTIVE goal with charged usage survives a process restart —
/// `--continue` resumes the same session, the journal replay restores the
/// token budget AND the charged usage (tokens + active time) exactly, the
/// resumed active goal work is safety-paused, and pause/resume/complete all
/// still work on the restored goal.
#[test]
fn repl_goal_active_restart_resumes_session_restores_usage_budget_and_lifecycle() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");

    {
        let mut repl = ReplProbe::spawn(
            home.path(),
            cwd.path(),
            &[
                "--mode",
                "text",
                "--model",
                "faux/faux-1",
                "--listen",
                "127.0.0.1:0",
                "--listen-plaintext",
            ],
        );
        repl.ready();
        assert!(
            repl.wait_stderr(
                "Control plane listening on http://",
                Duration::from_secs(30)
            ),
            "control plane must announce its address, stderr: {:?}",
            repl.stderr_snapshot()
        );
        let addr = control_plane_addr(&repl.stderr_snapshot());

        let created = repl.command("/goal create --tokens 100 deliver the release");
        assert!(
            created.starts_with("Goal work started · active · 0/100 tokens · deliver the release"),
            "goal create output: {created:?}"
        );

        // Charge usage through the same `goal_update_usage` pipeline finished
        // goal turns charge through: 40 tokens + 15s of active time.
        let charge = http_rpc(
            &addr,
            &json!({"type": "goal_update_usage", "id": "restart-charge", "tokens": 40, "activeTimeSeconds": 15}),
        );
        assert_eq!(charge["success"], true, "charge response: {charge}");
        assert_eq!(charge["data"]["lifecycle"], "active");
        assert_eq!(charge["data"]["usage"]["tokensUsed"], 40);
        assert_eq!(charge["data"]["usage"]["activeTimeSeconds"], 15);

        let shown = repl.command("/goal show");
        assert_eq!(shown.trim(), "active · 40/100 tokens · deliver the release");

        // Let the goal-work turn settle before exiting so the journal is idle.
        thread::sleep(Duration::from_secs(2));
        repl.quit();
    }

    let mut resumed = ReplProbe::spawn(
        home.path(),
        cwd.path(),
        &[
            "--mode",
            "text",
            "--model",
            "faux/faux-1",
            "--continue",
            "--listen",
            "127.0.0.1:0",
            "--listen-plaintext",
        ],
    );
    resumed.ready();
    assert!(
        resumed.wait_stderr("resumed", Duration::from_secs(10)),
        "restart must resume the recorded session, stderr: {:?}",
        resumed.stderr_snapshot()
    );
    assert!(
        resumed.wait_stderr(
            "Control plane listening on http://",
            Duration::from_secs(30)
        ),
        "control plane must announce its address, stderr: {:?}",
        resumed.stderr_snapshot()
    );
    let addr = control_plane_addr(&resumed.stderr_snapshot());

    // The journal replay restores the goal with its budget AND charged usage,
    // and the active goal work is safety-paused on resume.
    let state = http_rpc(&addr, &json!({"type": "goal_get", "id": "restart-get"}));
    assert_eq!(state["success"], true, "goal_get response: {state}");
    assert_eq!(state["data"]["current"]["lifecycle"], "paused");
    assert_eq!(state["data"]["current"]["pauseReason"], "resume_safety");
    assert_eq!(state["data"]["current"]["tokenBudget"], 100);
    assert_eq!(state["data"]["current"]["usage"]["tokensUsed"], 40);
    assert_eq!(state["data"]["current"]["usage"]["activeTimeSeconds"], 15);
    assert_eq!(state["data"]["current"]["objective"], "deliver the release");

    let shown = resumed.command("/goal show");
    assert_eq!(shown.trim(), "paused · 40/100 tokens · deliver the release");

    // Lifecycle still works on the restored goal: resume starts goal work
    // again and complete terminates it, preserving the charged usage.
    let resumed_work = resumed.command("/goal resume");
    assert!(
        resumed_work.starts_with("Goal work started · active · 40/100 tokens · deliver the release"),
        "resume output: {resumed_work:?}"
    );
    let completed = resumed.command("/goal complete");
    assert_eq!(completed.trim(), "completed · 40/100 tokens · deliver the release");

    resumed.quit();
}

/// Contract: the goal is scoped to its session — a restart WITHOUT
/// `--continue` starts a fresh session in the same directory and must not
/// see the previous session's goal, even though that journal is still on
/// disk (per-session isolation, T43).
#[test]
fn repl_goal_restart_without_resume_isolates_goal_per_session() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");

    {
        let mut repl =
            ReplProbe::spawn(home.path(), cwd.path(), &["--mode", "text", "--model", "faux/faux-1"]);
        repl.ready();
        let created = repl.command("/goal create --tokens 100 private session goal");
        assert!(
            created.starts_with("Goal work started · active · 0/100 tokens · private session goal"),
            "goal create output: {created:?}"
        );
        // Let the goal-work turn settle before exiting so the journal is idle.
        thread::sleep(Duration::from_secs(2));
        repl.quit();
    }

    // A fresh spawn in the same temp HOME/cwd starts a DIFFERENT session:
    // the per-session goal must not leak into it.
    let mut fresh =
        ReplProbe::spawn(home.path(), cwd.path(), &["--mode", "text", "--model", "faux/faux-1"]);
    fresh.ready();
    let shown = fresh.command("/goal show");
    assert_eq!(shown.trim(), "no goal");
    let bare = fresh.command("/goal");
    assert_eq!(bare.trim(), "no goal");
    fresh.quit();
}

/// Contract: loop lifecycle through the real REPL — `/loop <interval> <prompt>`
/// creates a task (12-char id, human schedule, expiry); `/loops` and
/// `/loop list` list it; the loop actually fires (the next-fire timestamp
/// advances by at least one interval); `/loop cancel <id>` removes it;
/// `/loop delete <id>` removes a second one; and cancel/delete with a missing
/// or unknown id fail with the canonical usage/not-found errors.
#[test]
fn repl_loop_create_list_fire_cancel_delete_and_error_paths() {
    let home = TempDir::new().expect("temp HOME");
    let cwd = TempDir::new().expect("temp cwd");
    let mut repl =
        ReplProbe::spawn(home.path(), cwd.path(), &["--mode", "text", "--model", "faux/faux-1"]);
    repl.ready();

    let created = repl.command("/loop 5s ping the satellite");
    assert!(created.starts_with("scheduled "), "loop create: {created:?}");
    let loop_id = loop_id_from_create(&created);
    assert!(!loop_id.is_empty(), "loop id missing: {created:?}");
    assert!(
        created.contains("every 5 seconds"),
        "loop human schedule: {created:?}"
    );
    assert!(created.contains("expires "), "loop expiry: {created:?}");

    let loops = repl.command("/loops");
    assert!(loops.contains(&loop_id), "/loops must list the loop: {loops:?}");
    assert!(
        loops.contains("every 5 seconds"),
        "/loops schedule: {loops:?}"
    );
    assert!(
        loops.contains("ping the satellite"),
        "/loops prompt: {loops:?}"
    );
    assert!(loops.contains("next "), "/loops next fire: {loops:?}");

    let listed = repl.command("/loop list");
    assert_eq!(
        listed.trim(),
        loops.trim(),
        "/loop list must match /loops output"
    );

    let cancelled = repl.command(&format!("/loop cancel {loop_id}"));
    assert_eq!(cancelled.trim(), format!("cancelled loop {loop_id}"));

    let after_cancel = repl.command("/loops");
    assert_eq!(after_cancel.trim(), "no active loops");

    // A second loop proves firing: the next-fire timestamp advances by at
    // least one interval once the scheduler runs the task.
    let second = repl.command("/loop create 1s firecheck");
    assert!(second.starts_with("scheduled "), "second loop: {second:?}");
    let second_id = loop_id_from_create(&second);
    let next1 = loop_next_millis(&repl.command("/loop list"), &second_id);
    thread::sleep(Duration::from_secs(3));
    let next2 = loop_next_millis(&repl.command("/loop list"), &second_id);
    assert!(
        next2 > next1,
        "loop must fire: next-fire did not advance ({next1} -> {next2})"
    );

    let deleted = repl.command(&format!("/loop delete {second_id}"));
    assert_eq!(deleted.trim(), format!("deleted loop {second_id}"));

    let after_delete = repl.command("/loops");
    assert_eq!(after_delete.trim(), "no active loops");

    // Error paths: missing id and unknown id for both cancel and delete.
    repl.command("/loop cancel");
    assert!(
        repl.wait_stderr("usage: /loop cancel <id>", Duration::from_secs(5)),
        "bare /loop cancel must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );
    repl.command("/loop cancel doesnotexist");
    assert!(
        repl.wait_stderr(
            "no active loop with id doesnotexist",
            Duration::from_secs(5)
        ),
        "unknown cancel must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );
    repl.command("/loop delete doesnotexist");
    assert!(
        repl.wait_stderr(
            "no active loop with id doesnotexist",
            Duration::from_secs(5)
        ),
        "unknown delete must fail, stderr: {:?}",
        repl.stderr_snapshot()
    );

    repl.quit();
}

/// Contract: TUI surface — bare `/goal` opens the Create/Show panel, Enter
/// prefills the composer (`/goal create `) with the objective hint, submitting
/// creates the goal and paints the footer chip `🎯 Goal 0/100`; pause flips the
/// chip to `⏸`, resume back to `🎯` (a goal turn runs), complete flips it to
/// `✓`, and dropping a completed goal is rejected with the lifecycle error.
///
/// All assertions run against the replayed visible screen (ratatui paints
/// table cells and footer fragments with explicit cursor moves, so raw-stream
/// substrings would miss inter-word spaces and gap cells); chip transitions
/// additionally assert the previous chip left the screen, proving the
/// `🎯 → ⏸ → 🎯 → ✓` order.
#[test]
fn pty_tui_goal_panel_and_footer_chip_lifecycle() {
    let mut probe = TuiProbe::spawn();
    assert!(
        probe.wait_for(HIDE_CURSOR, Duration::from_secs(30)),
        "TUI must start: {}",
        probe.snapshot()
    );
    assert!(
        probe.wait_for("faux/faux-1", Duration::from_secs(20)),
        "composer chrome must render: {}",
        probe.snapshot()
    );

    fn visible(probe: &TuiProbe) -> String {
        let mut screen = Screen::new(40, 200);
        screen.feed(&probe.snapshot());
        screen.text()
    }

    fn wait_for_screen(probe: &TuiProbe, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if visible(probe).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    // Bare /goal opens the Create/Show panel when no goal exists.
    probe.send(b"/goal\r");
    assert!(
        wait_for_screen(&probe, "Create goal", Duration::from_secs(10)),
        "goal panel must offer Create goal:\n{}",
        visible(&probe)
    );
    assert!(
        wait_for_screen(&probe, "Show details", Duration::from_secs(10)),
        "goal panel must offer Show details:\n{}",
        visible(&probe)
    );

    // Enter picks "Create goal": the composer is prefilled and the status
    // line explains what to type next.
    probe.send(b"\r");
    assert!(
        wait_for_screen(&probe, "/goal create", Duration::from_secs(10))
            && wait_for_screen(&probe, "Enter the goal objective, then press Enter", Duration::from_secs(10)),
        "create hint must render:\n{}",
        visible(&probe)
    );

    // Complete the prefilled `/goal create ` line with a token budget and
    // objective, then submit.
    probe.send(b"--tokens 100 ship the widget\r");
    assert!(
        wait_for_screen(&probe, "Objective: ship the widget", Duration::from_secs(20)),
        "goal details block must render:\n{}",
        visible(&probe)
    );
    assert!(
        wait_for_screen(&probe, "🎯 Goal 0/100", Duration::from_secs(20)),
        "footer goal chip after create:\n{}",
        visible(&probe)
    );

    // Pause → ⏸ chip (and the 🎯 chip must be gone); resume → 🎯 chip again
    // (a goal turn runs; ⏸ gone); complete → ✓ chip (🎯 gone). Each check
    // waits for the new chip and then asserts the previous one left the
    // visible screen, so the 🎯 → ⏸ → 🎯 → ✓ order is proven.
    probe.send(b"/goal pause\r");
    // Ratatui paints the footer chip with its diffing backend, rewriting only
    // the cells that change between frames; the 🎯(wide)→⏸(narrow) marker
    // flip leaves the unchanged budget cells (`0/100`) unwritten. The
    // wide-glyph-aware Screen models 🎯/📁 as two cells so those unchanged
    // cells land on the correct columns and the chip reads `⏸ Goal 0/100`.
    // Synchronize first on the authoritative composer status line
    // (`paused · 0/100 tokens · ship the widget`, from `state.status`) via the
    // cumulative RAW PTY stream as a best-effort settle barrier — the in-flight
    // goal turn's abort can clear `state.status` before a frame renders, so it
    // is not a reliable pass/fail signal — then assert the footer chip as the
    // authoritative paused-status + `0/100` budget check, and that the active
    // chip left the screen.
    let _ = probe.wait_for("paused · 0/100 tokens · ship the widget", Duration::from_secs(8));
    assert!(
        wait_for_screen(&probe, "⏸ Goal 0/100", Duration::from_secs(20)),
        "footer pause chip after pause:\n{}",
        visible(&probe)
    );
    assert!(
        !visible(&probe).contains("🎯 Goal 0/100"),
        "paused chip must replace the active chip:\n{}",
        visible(&probe)
    );

    probe.send(b"/goal resume\r");
    // Resume re-runs a goal turn, so the composer status line returns to the
    // create-time `Goal work started · active · 0/100 tokens · …` (already
    // present in the raw stream from create, so it cannot distinguish the
    // transition). The footer `🎯 Goal 0/100` active chip — reliably rendered
    // by the wide-glyph-aware Screen — is the authoritative resume signal.
    assert!(
        wait_for_screen(&probe, "🎯 Goal 0/100", Duration::from_secs(20)),
        "footer active chip after resume:\n{}",
        visible(&probe)
    );
    assert!(
        !visible(&probe).contains("⏸ Goal 0/100"),
        "resumed chip must replace the paused chip:\n{}",
        visible(&probe)
    );

    probe.send(b"/goal complete\r");
    // Best-effort synchronization on the authoritative composer status line
    // (`completed · 0/100 tokens · …`); the footer `✓ Goal 0/100` chip is the
    // authoritative completed-status + budget check.
    let _ = probe.wait_for("completed · 0/100 tokens · ship the widget", Duration::from_secs(8));
    assert!(
        wait_for_screen(&probe, "✓ Goal 0/100", Duration::from_secs(20)),
        "footer completed chip after complete:\n{}",
        visible(&probe)
    );
    assert!(
        !visible(&probe).contains("🎯 Goal 0/100"),
        "completed chip must replace the active chip:\n{}",
        visible(&probe)
    );

    // Terminal goals reject drop through the TUI dispatch error surface.
    probe.send(b"/goal drop\r");
    assert!(
        wait_for_screen(&probe, "cannot drop a goal in the Completed lifecycle", Duration::from_secs(10)),
        "drop rejection must render:\n{}",
        visible(&probe)
    );

    // The failed slash command stays in the composer for correction, and
    // Ctrl-D only exits on an empty editor. Ctrl-C clears the composer — but
    // if the resumed goal turn is still streaming, the first Ctrl-C aborts it
    // instead, so retry until the clear status is visible.
    let mut cleared = false;
    for _ in 0..6 {
        probe.send(&[CTRL_C]);
        if wait_for_screen(&probe, "Ctrl+C again to quit", Duration::from_secs(2)) {
            cleared = true;
            break;
        }
    }
    assert!(cleared, "composer must clear before quit:\n{}", visible(&probe));
    // Let the 500ms double-press quit window lapse, then quit cleanly.
    thread::sleep(Duration::from_millis(700));
    probe.send(&[CTRL_D]);
    let status = probe
        .wait_exit(Duration::from_secs(15))
        .expect("rpi TUI must exit after Ctrl+D");
    assert!(status.success(), "TUI exit status: {status:?}");
}
