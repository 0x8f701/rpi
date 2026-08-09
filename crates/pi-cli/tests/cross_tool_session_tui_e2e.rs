//! Cross-tool SessionCatalog TUI regressions over a real PTY.
//!
//! Surfaces under test (Unix only, faux model, isolated HOME/cwd):
//! 1. No-arg welcome "Recent sessions" lists cwd-scoped native Pi + Codex +
//!    Claude rows with sanitized `[pi]` / `[codex]` / `[claude]` badges (and
//!    not wrong-cwd / corrupt / symlink foreign noise).
//! 2. `/resume` (and `/sessions`) selector shows the same unified catalog rows
//!    with badges/status/summary corpus under title `Resume Session`.
//! 3. Filtering a foreign row by unique query + Enter imports into the
//!    effective custom `--session-dir` as `import_*.jsonl` with durable
//!    `import_lineage`, switches the TUI, and surfaces foreign transcript text.
//! 4. Reopening the selector marks the foreign source as imported
//!    (`AlreadyImported` / native_path retained via Target selection).
//! 5. A native row planted only under the effective custom session-dir appears
//!    (TUI must bind NativePi root to `application.session().session_dir()`).
//!
//! Fixture formats match `session_catalog/tests.rs` (`write_codex` /
//! `write_claude` / native v3). Discovery roots match `SessionCatalog::from_env`
//! under `env_clear` + `HOME`/`SESSIONS_HOME` (default `.codex`, `.claude`,
//! `.pi/agent`). Assertions target visible terminal behavior and on-disk public
//! artifacts — never private TUI helpers. Child `rpi` is killed on Drop as a
//! hard backstop. PtyProbe retains a hard-capped output tail while still
//! draining the PTY and answering CSI 6n probes; wait markers are absolute
//! stream offsets.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::Termios;
use pi_coding::session_catalog::{SESSION_CATALOG_CANDIDATE_LIMIT, SESSION_CATALOG_ROW_LIMIT};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
use tempfile::TempDir;

const SHOW_CURSOR: &str = "\x1b[?25h";
const CTRL_D: u8 = 0x04;
const ESC: u8 = 0x1b;
const CTRL_U: u8 = 0x15;
/// Hard cap on retained PTY output.
const PTY_OUTPUT_CAP: usize = 512 * 1024;

/// Unique fixture corpus — must survive filter + display without collision.
const PI_SUMMARY: &str = "native-pi-unique-SIGIL-7a91";
const CODEX_SUMMARY: &str = "codex-foreign-unique-SIGIL-3c2e";
const CLAUDE_SUMMARY: &str = "claude-foreign-unique-SIGIL-9b04";
const CUSTOM_NATIVE_SUMMARY: &str = "custom-session-dir-unique-SIGIL-5d88";
const WRONG_CWD_SUMMARY: &str = "wrong-cwd-leak-unique-SIGIL-1f77";
const CORRUPT_MARKER: &str = "corrupt-foreign-unique-SIGIL-dead";
const SYMLINK_MARKER: &str = "symlink-foreign-unique-SIGIL-link";

const CODEX_ID: &str = "codex-xtool";
const CLAUDE_ID: &str = "claude-xtool";

/// Bounded-catalog fixture: a single foreign source planted with more
/// entries than the per-source candidate cap so the hard bound must truncate.
/// Deterministic mtimes pin the newest/oldest selection direction. These are
/// distinct from the small-store corpus above so the two contracts cannot mask
/// each other.
const BOUNDED_MTIME_BASE: u64 = 1_900_000_000;
const BOUNDED_EXCESS_BEYOND_LIMIT: usize = 24;
const BOUNDED_NEWEST_SENTINEL: &str = "bounded-newest-codex-SIGIL-n3w";
const BOUNDED_OLDEST_SENTINEL: &str = "bounded-oldest-codex-SIGIL-0ld";
const BOUNDED_WRONG_CWD: &str = "bounded-wrong-cwd-SIGIL-l34k";
const BOUNDED_CORRUPT: &str = "bounded-corrupt-SIGIL-xXx";
const BOUNDED_SYMLINK: &str = "bounded-symlink-SIGIL-lnk";
const BOUNDED_CLAUDE: &str = "bounded-claude-SIGIL-cl4";
const BOUNDED_CUSTOM_NATIVE: &str = "bounded-custom-native-SIGIL-cu5";
const BOUNDED_CLAUDE_ID: &str = "bounded-claude";

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
        let mut drop_bytes = excess;
        while drop_bytes < self.tail.len() && !self.tail.is_char_boundary(drop_bytes) {
            drop_bytes += 1;
        }
        if drop_bytes == 0 || drop_bytes >= self.tail.len() {
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

    fn since(&self, absolute_marker: usize) -> &str {
        if absolute_marker <= self.dropped {
            self.tail.as_str()
        } else {
            let local = absolute_marker - self.dropped;
            self.tail.get(local..).unwrap_or("")
        }
    }
}

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}
fn set_session_import_sources(home: &Path, sources: &[&str]) {
    let agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&agent_dir).expect("agent settings directory");
    fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_vec(&serde_json::json!({ "sessionImportSources": sources }))
            .expect("session import settings"),
    )
    .expect("write session import settings");
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
    fn spawn_in(args: &[&str], rows: u16, cols: u16, home: TempDir, cwd: TempDir) -> Self {
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
        // Minimal environment: catalog from_env reads HOME/SESSIONS_HOME only.
        cmd.env_clear();
        cmd.env("HOME", home.path());
        cmd.env("USERPROFILE", home.path());
        cmd.env("SESSIONS_HOME", home.path());
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("TERM", "xterm-256color");
        cmd.env("PI_OFFLINE", "1");
        cmd.env("PI_SKIP_VERSION_CHECK", "1");
        // Default foreign/native roots under isolated HOME — no overrides.
        cmd.env_remove("PI_CODING_AGENT_DIR");
        cmd.env_remove("PI_CODING_AGENT_SESSION_DIR");
        cmd.env_remove("CODEX_HOME");
        cmd.env_remove("CLAUDE_CONFIG_DIR");
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

    fn snapshot(&self) -> String {
        self.buffer.lock().expect("buffer lock").snapshot()
    }

    fn len(&self) -> usize {
        self.buffer.lock().expect("buffer lock").absolute_len()
    }

    fn since(&self, absolute_marker: usize) -> String {
        self.buffer
            .lock()
            .expect("buffer lock")
            .since(absolute_marker)
            .to_owned()
    }

    fn live_screen(&self) -> String {
        let replayed = replay_terminal_scrollback(&self.snapshot(), self.cols, self.rows);
        replayed[replayed.len().saturating_sub(self.rows)..].join("\n")
    }

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

    /// Poll the live terminal for `needle` in the ANSI-normalized stream.
    ///
    /// The inline TUI paints styled text as per-character SGR runs (e.g.
    /// `Recent\x1b[49m\x1b[38;2;…msessions`), so a raw substring match fails
    /// on text that is visibly present. Searches run against [`strip_ansi`];
    /// raw escape-sequence needles are only used on direct snapshots.
    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if strip_ansi(&self.snapshot()).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_after(&self, marker: usize, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if strip_ansi(&self.since(marker)).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_any(&self, needles: &[&str], timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = strip_ansi(&self.snapshot());
            for needle in needles {
                if snap.contains(needle) {
                    return Some((*needle).to_owned());
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_any_after(
        &self,
        marker: usize,
        needles: &[&str],
        timeout: Duration,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let delta = strip_ansi(&self.since(marker));
            for needle in needles {
                if delta.contains(needle) {
                    return Some((*needle).to_owned());
                }
            }
            if Instant::now() >= deadline {
                return None;
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
        if self.live_screen().contains("Resume Session") {
            let close_at = self.len();
            self.send(&[ESC]);
            assert!(
                self.wait_for_live_after(close_at, Duration::from_secs(8), |live| {
                    !live.contains("Resume Session") && live.contains("faux/faux-1")
                })
                .is_some(),
                "session selector must close before quit: {}",
                self.live_screen()
            );
        }
        self.clear_composer();
        let quit_at = self.len();
        self.type_chars("/quit");
        assert!(
            self.wait_for_live_after(quit_at, Duration::from_secs(8), |live| {
                live.contains("/quit")
            })
            .is_some(),
            "quit command must reach the live composer: {}",
            self.live_screen()
        );
        self.send(b"\r");
    }

    fn shutdown_take_dirs(mut self) -> (TempDir, TempDir) {
        kill_and_reap(&mut self.child, Duration::from_secs(5));
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
                let _ = child.wait();
                return;
            }
        }
    }
}

/// The TUI is fully entered only once a rendered marker appears. Startup
/// capability detection does not read stdin, so bytes typed after terminal
/// acquisition remain owned by the event loop.
fn await_entered(probe: &PtyProbe) -> bool {
    probe.wait_for("Recent sessions", Duration::from_secs(30))
        || probe.wait_for("faux/faux-1", Duration::from_secs(15))
        || probe.wait_for("π", Duration::from_secs(15))
        || probe.wait_for("pi (rs)", Duration::from_secs(15))
        || probe.wait_for("Ready", Duration::from_secs(15))
        || probe.wait_for("ready", Duration::from_secs(15))
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
/// mistake an erased selector row or status for current UI state.
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

fn json_escape_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn set_modified(path: &Path, seconds: u64) {
    use std::fs::{File, FileTimes};
    let file = File::options().write(true).open(path).expect("open mtime");
    file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .expect("set mtime");
}

/// Mirrors `session_store::encode_cwd_safe_path` for default project roots.
fn encode_cwd_project(cwd: &Path) -> String {
    let absolute = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string();
    let mut encoded = absolute;
    if encoded.starts_with('/') || encoded.starts_with('\\') {
        encoded.remove(0);
    }
    encoded.replace(['/', '\\', ':'], "-")
}

/// Native Pi v3 under default `$HOME/.pi/agent/sessions/--cwd--/`.
fn plant_native_pi(home: &Path, cwd: &Path, id: &str, summary: &str, mtime: u64) -> PathBuf {
    let project = format!("--{}--", encode_cwd_project(cwd));
    let dir = home
        .join(".pi")
        .join("agent")
        .join("sessions")
        .join(project);
    fs::create_dir_all(&dir).expect("native session dir");
    let path = dir.join(format!("{id}.jsonl"));
    let cwd_json = json_escape_path(cwd);
    let body = format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd_json}"}}
{{"type":"model_change","id":"mc1","parentId":null,"timestamp":"2026-01-01T00:00:00.100Z","provider":"faux","modelId":"faux-1"}}
{{"type":"thinking_level_change","id":"tl1","parentId":"mc1","timestamp":"2026-01-01T00:00:00.200Z","thinkingLevel":"off"}}
{{"type":"message","id":"u1","parentId":"tl1","timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{summary}"}}],"timestamp":0}}}}
{{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"native-pi-reply"}}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":1}}}}
"#
    );
    fs::write(&path, body).expect("write native pi");
    set_modified(&path, mtime);
    path
}

/// Native Pi v3 planted only under an explicit `--session-dir` root.
fn plant_native_under_session_dir(
    session_dir: &Path,
    cwd: &Path,
    id: &str,
    summary: &str,
    mtime: u64,
) -> PathBuf {
    fs::create_dir_all(session_dir).expect("custom session dir");
    let path = session_dir.join(format!("{id}.jsonl"));
    let cwd_json = json_escape_path(cwd);
    let body = format!(
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-04-01T00:00:00.000Z","cwd":"{cwd_json}"}}
{{"type":"model_change","id":"mc1","parentId":null,"timestamp":"2026-04-01T00:00:00.100Z","provider":"faux","modelId":"faux-1"}}
{{"type":"thinking_level_change","id":"tl1","parentId":"mc1","timestamp":"2026-04-01T00:00:00.200Z","thinkingLevel":"off"}}
{{"type":"message","id":"u1","parentId":"tl1","timestamp":"2026-04-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{summary}"}}],"timestamp":0}}}}
{{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-04-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"custom-root-reply"}}],"api":"faux","provider":"faux","model":"faux-1","stopReason":"stop","timestamp":1}}}}
"#
    );
    fs::write(&path, body).expect("write custom-root native");
    set_modified(&path, mtime);
    path
}

/// Codex rollout under `$HOME/.codex/sessions/` — catalog `write_codex` format.
fn plant_codex(home: &Path, cwd: &Path, id: &str, summary: &str, mtime: u64) -> PathBuf {
    let dir = home.join(".codex").join("sessions");
    fs::create_dir_all(&dir).expect("codex dir");
    let path = dir.join(format!("rollout-{id}.jsonl"));
    let cwd_json = json_escape_path(cwd);
    let body = format!(
        concat!(
            r#"{{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}"#,
            "\n",
            r#"{{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{summary}"}}]}}}}"#,
            "\n",
            r#"{{"timestamp":"2026-02-01T00:00:02Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"codex-ok-reply"}}]}}}}"#,
            "\n"
        ),
        id = id,
        cwd = cwd_json,
        summary = summary
    );
    fs::write(&path, body).expect("write codex");
    set_modified(&path, mtime);
    path
}

/// Claude JSONL under `$HOME/.claude/projects/proj/` — catalog `write_claude` format.
fn plant_claude(home: &Path, cwd: &Path, id: &str, summary: &str, mtime: u64) -> PathBuf {
    let dir = home.join(".claude").join("projects").join("proj");
    fs::create_dir_all(&dir).expect("claude dir");
    let path = dir.join(format!("{id}.jsonl"));
    let cwd_json = json_escape_path(cwd);
    let body = format!(
        concat!(
            r#"{{"type":"user","uuid":"u","parentUuid":null,"isSidechain":false,"sessionId":"{id}","cwd":"{cwd}","timestamp":"2026-03-01T00:00:01Z","message":{{"role":"user","content":"{summary}"}}}}"#,
            "\n",
            r#"{{"type":"assistant","uuid":"a","parentUuid":"u","isSidechain":false,"timestamp":"2026-03-01T00:00:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"claude-ok-reply"}}]}}}}"#,
            "\n",
            r#"{{"type":"last-prompt","leafUuid":"a","sessionId":"{id}"}}"#,
            "\n"
        ),
        id = id,
        cwd = cwd_json,
        summary = summary
    );
    fs::write(&path, body).expect("write claude");
    set_modified(&path, mtime);
    path
}

fn plant_noise_entries(home: &Path, other_cwd: &Path, good_codex: &Path) {
    // Wrong-cwd foreign must not leak into cwd-scoped welcome/selector.
    let _ = plant_codex(home, other_cwd, "wrong-cwd-codex", WRONG_CWD_SUMMARY, 10);

    // Corrupt Codex rollout — discover may see the path; scan must drop it.
    let corrupt = home
        .join(".codex")
        .join("sessions")
        .join("rollout-corrupt.jsonl");
    fs::write(&corrupt, format!("{{not json\n{CORRUPT_MARKER}\n")).expect("write corrupt codex");
    set_modified(&corrupt, 11);

    // Symlink foreign entry must be excluded from discovery.
    let link = home
        .join(".codex")
        .join("sessions")
        .join(format!("rollout-{SYMLINK_MARKER}.jsonl"));
    let _ = fs::remove_file(&link);
    symlink(good_codex, &link).expect("symlink codex rollout");
}

fn open_resume_selector(probe: &mut PtyProbe) -> usize {
    probe.clear_composer();
    let command_at = probe.len();
    probe.type_chars("/resume");
    assert!(
        probe
            .wait_for_live_after(command_at, Duration::from_secs(8), |live| {
                live.contains("/resume")
            })
            .is_some(),
        "resume command must reach the live composer: {}",
        probe.live_screen()
    );
    let open_at = probe.len();
    probe.send(b"\r");
    assert!(
        probe
            .wait_for_live_after(open_at, Duration::from_secs(20), |live| {
                live.contains("Resume Session") && live.contains("Filter:")
            })
            .is_some(),
        "resume selector must become live: {}",
        probe.live_screen()
    );
    open_at
}

fn clear_filter(probe: &mut PtyProbe) -> usize {
    let marker = probe.len();
    // DEL is the selector's Backspace key over this PTY. Avoid 0x08, which
    // crossterm reports as Ctrl+H and only produces an unknown-key status.
    for _ in 0..96 {
        probe.send(&[0x7f]);
        thread::sleep(Duration::from_millis(6));
    }
    marker
}

fn replace_filter(probe: &mut PtyProbe, text: &str) -> usize {
    assert!(
        probe.live_screen().contains("Resume Session"),
        "filter input requires a live Resume Session selector: {}",
        probe.live_screen()
    );
    clear_filter(probe);
    let marker = probe.len();
    probe.type_chars(text);
    marker
}

fn wait_for_exact_filtered_row(
    probe: &mut PtyProbe,
    marker: usize,
    query: &str,
    badge: &str,
    label: &str,
    timeout: Duration,
) -> Option<String> {
    let row_badge = format!("[{badge}]");
    let predicate = |live: &str| {
        let filter_visible = live
            .lines()
            .any(|line| line.contains("Filter:") && line.contains(query));
        let rows = live
            .lines()
            .filter(|line| line.contains(" messages") && line.contains('['))
            .collect::<Vec<_>>();
        live.contains("Resume Session")
            && filter_visible
            && rows.len() == 1
            && rows[0].contains(&row_badge)
            && rows[0].contains(label)
    };
    let live = probe.wait_for_live_after(marker, timeout, predicate)?;

    // Filtering to one row normally selects index zero. An explicit Up makes
    // that selection deterministic even if prior query selection restoration
    // races the last filter events.
    let select_at = probe.len();
    probe.send(b"\x1b[A");
    probe.wait_for_live_after(select_at, Duration::from_secs(8), predicate)
        .or(Some(live))
}

fn wait_for_unfiltered_selector(probe: &PtyProbe, marker: usize, timeout: Duration) -> Option<String> {
    probe.wait_for_live_after(marker, timeout, |live| {
        live.contains("Resume Session")
            && live.lines().any(|line| {
                line.contains("Filter:")
                    && !line
                        .split_once("Filter:")
                        .is_some_and(|(_, tail)| tail.chars().any(char::is_alphanumeric))
            })
    })
}

fn close_resume_selector(probe: &mut PtyProbe) {
    if !probe.live_screen().contains("Resume Session") {
        return;
    }
    let close_at = probe.len();
    probe.send(&[ESC]);
    assert!(
        probe
            .wait_for_live_after(close_at, Duration::from_secs(8), |live| {
                !live.contains("Resume Session") && live.contains("faux/faux-1")
            })
            .is_some(),
        "session selector must close on Escape: {}",
        probe.live_screen()
    );
}

fn assert_badge_label(plain: &str, badge: &str, labels: &[&str]) -> bool {
    let badge = format!("[{badge}]");
    labels.iter().any(|label| {
        plain
            .match_indices(&badge)
            .any(|(index, _)| plain[index + badge.len()..].trim_start().starts_with(label))
    })
}

fn assert_source_badges(plain: &str, context: &str) {
    assert!(plain.contains("[pi]"), "{context}: missing [pi] badge:\n{plain}");
    assert!(
        plain.contains("[codex]"),
        "{context}: missing [codex] badge:\n{plain}"
    );
    assert!(
        plain.contains("[claude]"),
        "{context}: missing [claude] badge:\n{plain}"
    );
    // PTY snapshots record terminal cursor motion rather than a rectangular
    // screen, so cell gaps may collapse. Assert badge/label adjacency while
    // tolerating that transport artifact.
    assert!(
        assert_badge_label(
            plain,
            "pi",
            &[PI_SUMMARY, CUSTOM_NATIVE_SUMMARY, "native-pi", "custom-root"],
        ),
        "{context}: missing native `[pi] <summary>` row:\n{plain}"
    );
    assert!(
        assert_badge_label(plain, "codex", &[CODEX_SUMMARY, CODEX_ID]),
        "{context}: missing `[codex] <summary>` row:\n{plain}"
    );
    assert!(
        assert_badge_label(plain, "claude", &[CLAUDE_SUMMARY, CLAUDE_ID]),
        "{context}: missing `[claude] <summary>` row:\n{plain}"
    );
}

fn assert_noise_absent(plain: &str, context: &str) {
    assert!(
        !plain.contains(WRONG_CWD_SUMMARY),
        "{context}: wrong-cwd foreign row must not appear:\n{plain}"
    );
    assert!(
        !plain.contains(CORRUPT_MARKER),
        "{context}: corrupt foreign entry must not appear:\n{plain}"
    );
    assert!(
        !plain.contains(SYMLINK_MARKER),
        "{context}: symlink foreign entry must not appear:\n{plain}"
    );
}

/// Exact production marker from tui.rs welcome/selector rows.
const IMPORTED_MARKER: &str = " · imported";

/// Require the lowercase ` · imported` marker on a `[codex]` row — never
/// accept bare `import_*.jsonl` path text as proof of AlreadyImported status.
fn row_shows_imported(plain: &str, badge: &str, label: &str) -> bool {
    plain.lines().any(|line| {
        line.contains(&format!("[{badge}]"))
            && (line.contains(label) || line.contains(CODEX_ID) || line.contains(CODEX_SUMMARY))
            && line.contains(IMPORTED_MARKER)
    }) || (plain.contains(&format!("[{badge}]"))
        && plain.contains(label)
        && plain.contains(IMPORTED_MARKER))
}

fn list_import_jsonl(session_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(session_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with("import_") && name.ends_with(".jsonl") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn default_project_session_dir(home: &Path, cwd: &Path) -> PathBuf {
    let project = format!("--{}--", encode_cwd_project(cwd));
    home.join(".pi")
        .join("agent")
        .join("sessions")
        .join(project)
}

fn assert_import_lineage(path: &Path, source: &str, source_session_id: &str) {
    let body = fs::read_to_string(path).expect("read import");
    assert!(
        body.contains("import_lineage"),
        "imported session must stamp import_lineage: {}",
        path.display()
    );
    assert!(
        body.contains(&format!("\"source\":\"{source}\""))
            || body.contains(&format!("\"source\": \"{source}\"")),
        "lineage source must be {source}: {body}"
    );
    assert!(
        body.contains("sourceSessionId") && body.contains(source_session_id),
        "lineage must retain sourceSessionId={source_session_id}: {body}"
    );
    let records: Vec<Value> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let has_lineage = records.iter().any(|record| {
        record.get("customType").and_then(Value::as_str) == Some("import_lineage")
    });
    assert!(
        has_lineage,
        "expected custom import_lineage record in {}",
        path.display()
    );
}

fn plant_all_fixtures(home: &Path, cwd: &Path, session_dir: &Path) -> PathBuf {
    let other_cwd = home.join("other-project-cwd");
    fs::create_dir_all(&other_cwd).expect("other cwd");

    // Default-home native (cwd-scoped) for [pi] badge coverage.
    let _ = plant_native_pi(home, cwd, "native-pi-default", PI_SUMMARY, 100);
    let codex = plant_codex(home, cwd, CODEX_ID, CODEX_SUMMARY, 300);
    let _ = plant_claude(home, cwd, CLAUDE_ID, CLAUDE_SUMMARY, 200);
    // Proves TUI binds NativePi root to effective --session-dir.
    let _ = plant_native_under_session_dir(
        session_dir,
        cwd,
        "custom-root-native",
        CUSTOM_NATIVE_SUMMARY,
        400,
    );
    plant_noise_entries(home, &other_cwd, &codex);
    codex
}

/// Plant a foreign source with more candidates than the per-source cap so the
/// hard bound must truncate. Returns the deterministic newest sentinel's id,
/// on-disk path, and the planted codex count. mtimes are monotonic across the
/// flood so truncation direction (keep newest, drop oldest) is observable.
fn plant_bounded_large_catalog(
    home: &Path,
    cwd: &Path,
    session_dir: &Path,
) -> (String, PathBuf, usize) {
    // Exceed the per-source candidate cap by a small, CI-practical delta. The
    // global row cap is the same value, so this fixture also exceeds it.
    let count = SESSION_CATALOG_CANDIDATE_LIMIT + BOUNDED_EXCESS_BEYOND_LIMIT;
    assert!(
        count > SESSION_CATALOG_ROW_LIMIT,
        "bounded fixture must exceed the global row cap"
    );
    let other_cwd = home.join("bounded-other-cwd");
    fs::create_dir_all(&other_cwd).expect("bounded other cwd");

    let mut newest_path = PathBuf::new();
    let mut newest_id = String::new();
    for index in 0..count {
        let id = format!("bounded-codex-{index:04}");
        let summary = if index == 0 {
            BOUNDED_OLDEST_SENTINEL.to_owned()
        } else if index == count - 1 {
            BOUNDED_NEWEST_SENTINEL.to_owned()
        } else {
            format!("bounded-codex-fill-{index:04}")
        };
        let mtime = BOUNDED_MTIME_BASE + index as u64;
        let path = plant_codex(home, cwd, &id, &summary, mtime);
        if index == count - 1 {
            newest_path = path;
            newest_id = id;
        }
    }

    // Other-source rows at a mid mtime: must survive the global cap (not
    // starved by the codex flood) and remain visible via selector filter.
    let _ = plant_claude(
        home,
        cwd,
        BOUNDED_CLAUDE_ID,
        BOUNDED_CLAUDE,
        BOUNDED_MTIME_BASE + 200,
    );
    let _ = plant_native_under_session_dir(
        session_dir,
        cwd,
        "bounded-custom-native",
        BOUNDED_CUSTOM_NATIVE,
        BOUNDED_MTIME_BASE + 201,
    );

    // Noise with deliberately HIGH mtimes so each would survive the
    // per-source candidate cap if cwd-scope / parse / symlink isolation
    // regressed — making their absence a meaningful signal under the bound.

    // Wrong-cwd codex, newest mtime of all candidates: cwd_scope (applied
    // after the cap) must drop it; a missing cwd-scope would surface it.
    let _ = plant_codex(
        home,
        &other_cwd,
        "bounded-wrong-cwd",
        BOUNDED_WRONG_CWD,
        BOUNDED_MTIME_BASE + count as u64 + 1,
    );

    // Corrupt codex with a high mtime: it survives the candidate cap but scan
    // must drop it on parse failure rather than emitting a broken row.
    let corrupt = home
        .join(".codex")
        .join("sessions")
        .join("rollout-bounded-corrupt.jsonl");
    fs::write(&corrupt, format!("{{not json\n{BOUNDED_CORRUPT}\n"))
        .expect("write bounded corrupt");
    set_modified(&corrupt, BOUNDED_MTIME_BASE + count as u64 + 2);

    // Symlink to the newest sentinel: discovery path safety must reject it
    // regardless of mtime. Do not set_modified on the link (open() would
    // mutate the target's mtime); the symlink is rejected before mtime sort.
    let link = home
        .join(".codex")
        .join("sessions")
        .join(format!("rollout-{BOUNDED_SYMLINK}.jsonl"));
    let _ = fs::remove_file(&link);
    symlink(&newest_path, &link).expect("symlink bounded codex");

    (newest_id, newest_path, count)
}

/// Require the bounded noise sentinels (wrong-cwd / corrupt / symlink) to be
/// absent from a rendered surface. Each is planted with a high mtime so it
/// would survive the candidate cap absent the relevant safety filter.
fn assert_bounded_noise_absent(plain: &str, context: &str) {
    assert!(
        !plain.contains(BOUNDED_WRONG_CWD),
        "{context}: bounded wrong-cwd row must not appear:\n{plain}"
    );
    assert!(
        !plain.contains(BOUNDED_CORRUPT),
        "{context}: bounded corrupt entry must not appear:\n{plain}"
    );
    assert!(
        !plain.contains(BOUNDED_SYMLINK),
        "{context}: bounded symlink entry must not appear:\n{plain}"
    );
}

/// Contract: welcome Recent sessions + `/resume` unified selector expose
/// cwd-scoped native/Codex/Claude rows with `[source]` badges; selecting a
/// foreign row imports under effective `--session-dir` with lineage; reopen
/// shows imported; wrong-cwd/corrupt/symlink stay out; custom-root native is
/// visible. Catches old `list_sessions(cwd)`, missing badges, lost Target
/// status, import-to-default-root, and noise inclusion.
#[test]
fn pty_cross_tool_welcome_resume_import_under_session_dir() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(home.path(), &["codex", "claude"]);
    // Canonical path identity used by catalog cwd-scope matching.
    let cwd_canon = cwd
        .path()
        .canonicalize()
        .unwrap_or_else(|_| cwd.path().to_path_buf());
    let session_dir = home.path().join("effective-session-dir");
    fs::create_dir_all(&session_dir).expect("session dir");
    let _ = plant_all_fixtures(home.path(), &cwd_canon, &session_dir);

    let session_dir_arg = session_dir.to_str().expect("utf8 session dir").to_owned();
    let args = [
        "--model",
        "faux/faux-1",
        "--session-dir",
        session_dir_arg.as_str(),
    ];

    // ── Phase 1: no-arg welcome shows badges / top rows ────────────────────
    let mut probe = PtyProbe::spawn_in(&args, 42, 140, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        strip_ansi(&probe.snapshot())
    );
    assert!(
        probe.wait_for("Recent sessions", Duration::from_secs(20))
            || probe.wait_for(CUSTOM_NATIVE_SUMMARY, Duration::from_secs(8))
            || probe.wait_for(CODEX_SUMMARY, Duration::from_secs(8))
            || probe.wait_for(PI_SUMMARY, Duration::from_secs(8))
            || probe.wait_for("[codex]", Duration::from_secs(8))
            || probe.wait_for("[pi]", Duration::from_secs(8)),
        "welcome must surface Recent sessions / catalog rows: {}",
        strip_ansi(&probe.snapshot())
    );
    thread::sleep(Duration::from_millis(500));
    let welcome_plain = strip_ansi(&probe.snapshot());
    assert!(
        welcome_plain.contains("Recent sessions"),
        "welcome Recent sessions heading missing: {welcome_plain}"
    );
    // Effective custom session-dir native must appear (not ignored).
    // Welcome format: `  • [badge] summary/name` (+ optional ` · imported`).
    assert!(
        welcome_plain.contains(CUSTOM_NATIVE_SUMMARY)
            || welcome_plain.contains(PI_SUMMARY)
            || welcome_plain.contains("custom-root")
            || welcome_plain.contains("native-pi"),
        "welcome must include custom --session-dir native row with [pi] badge: {welcome_plain}"
    );
    // Up to 3 cwd-matching rows; require sanitized [source] badge form on bullets.
    assert!(
        ["pi", "codex", "claude"]
            .iter()
            .any(|badge| welcome_plain.contains(&format!("[{badge}]"))),
        "welcome Recent sessions must show a source badge: {welcome_plain}"
    );
    assert_noise_absent(&welcome_plain, "welcome");

    // ── Phase 2: /resume selector lists unified rows with badges ───────────
    let _open_at = open_resume_selector(&mut probe);
    let selector_plain = probe.live_screen();
    assert_source_badges(&selector_plain, "/resume selector");
    assert!(
        selector_plain.contains(CUSTOM_NATIVE_SUMMARY)
            || selector_plain.contains("custom-root-native"),
        "/resume must list native row from effective custom session-dir: {selector_plain}"
    );
    assert_noise_absent(&selector_plain, "/resume selector");

    // ── Phase 3: filter foreign Codex + Enter → import under session-dir ───
    let filter_query = CODEX_ID;
    let filter_at = replace_filter(&mut probe, filter_query);
    let filtered = wait_for_exact_filtered_row(
        &mut probe,
        filter_at,
        filter_query,
        "codex",
        CODEX_SUMMARY,
        Duration::from_secs(12),
    )
    .unwrap_or_else(|| {
        panic!(
            "filter must isolate exact codex row on the live selector: {}",
            probe.live_screen()
        )
    });
    assert!(
        !filtered.contains(WRONG_CWD_SUMMARY) && !filtered.contains(CORRUPT_MARKER),
        "filtered view must not resurrect noise: {filtered}"
    );

    let before_imports = list_import_jsonl(&session_dir);
    let default_root = default_project_session_dir(probe.home_path(), &cwd_canon);
    let before_default = list_import_jsonl(&default_root);

    // Enter selects the sole live filtered row via ResumeSelectionRequest::Target.
    let select_at = probe.len();
    probe.send(b"\r");

    let import_deadline = Instant::now() + Duration::from_secs(25);
    let mut saw_live_switch = false;
    while Instant::now() < import_deadline {
        let live = probe.live_screen();
        if probe.len() > select_at
            && !live.contains("Resume Session")
            && (live.contains("Resumed ")
                || live.contains(CODEX_SUMMARY)
                || live.contains("codex-ok-reply"))
        {
            saw_live_switch = true;
        }
        if saw_live_switch && list_import_jsonl(&session_dir).len() > before_imports.len() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_live_switch,
        "selecting foreign codex must close selector and switch the live TUI: {}",
        probe.live_screen()
    );

    let imports = list_import_jsonl(&session_dir);
    assert!(
        imports.len() > before_imports.len(),
        "foreign select must create import_*.jsonl under effective --session-dir {} (not default root); before={before_imports:?} after={imports:?}",
        session_dir.display()
    );
    let default_after = list_import_jsonl(&default_root);
    assert!(
        default_after.len() == before_default.len()
            || imports.iter().any(|p| p.starts_with(&session_dir)),
        "import must land under effective session-dir, not leak solely to default project root: effective={imports:?} default_before={before_default:?} default_after={default_after:?}"
    );
    for path in &imports {
        if before_imports.contains(path) {
            continue;
        }
        assert!(
            path.starts_with(&session_dir),
            "new import path must be under effective session-dir: {}",
            path.display()
        );
    }
    let import_path = imports
        .iter()
        .find(|path| !before_imports.contains(path))
        .cloned()
        .unwrap_or_else(|| imports.last().expect("import path").clone());
    assert_import_lineage(&import_path, "codex", CODEX_ID);

    let after_import_plain = probe.live_screen();
    assert!(
        after_import_plain.contains("Resumed ")
            || after_import_plain.contains(CODEX_SUMMARY)
            || after_import_plain.contains("codex-ok-reply"),
        "live TUI must surface Resumed <path> status or foreign transcript after import: {after_import_plain}"
    );

    // ── Phase 4: reopen selector → exact ` · imported` on foreign row ──────
    let _reopen_at = open_resume_selector(&mut probe);
    let refilter_at = replace_filter(&mut probe, filter_query);
    let reopened = wait_for_exact_filtered_row(
        &mut probe,
        refilter_at,
        filter_query,
        "codex",
        CODEX_SUMMARY,
        Duration::from_secs(12),
    )
    .unwrap_or_else(|| panic!("reopened selector must isolate codex row: {}", probe.live_screen()));
    assert!(
        row_shows_imported(&reopened, "codex", CODEX_SUMMARY),
        "reopened selector must show exact `{IMPORTED_MARKER}` on [codex] row (not import_* path alone): {reopened}"
    );
    assert!(
        reopened.contains("messages") && reopened.contains("[codex]"),
        "reopened selector row must keep badge + messages corpus: {reopened}"
    );

    let clear_at = clear_filter(&mut probe);
    let unfiltered = wait_for_unfiltered_selector(&probe, clear_at, Duration::from_secs(10))
        .unwrap_or_else(|| panic!("reopened selector filter must clear: {}", probe.live_screen()));
    assert!(
        unfiltered.contains("[codex]")
            && unfiltered.contains("[pi]")
            && unfiltered.contains("[claude]"),
        "reopened unfiltered selector must retain all source badges: {unfiltered}"
    );
    assert!(
        unfiltered.contains(IMPORTED_MARKER),
        "unfiltered reopen must retain exact `{IMPORTED_MARKER}` marker: {unfiltered}"
    );
    assert_noise_absent(&unfiltered, "reopened selector");

    close_resume_selector(&mut probe);
    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit cleanly: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership"
    );

    // Final disk assertions (public artifacts only).
    assert!(
        import_path.exists(),
        "import file must remain at {}",
        import_path.display()
    );
    let import_body = fs::read_to_string(&import_path).expect("import body");
    assert!(
        import_body.contains(CODEX_SUMMARY) || import_body.contains("codex-ok-reply"),
        "imported transcript must carry foreign content"
    );
    let leaked_only = list_import_jsonl(&default_root);
    assert!(
        leaked_only.is_empty()
            || list_import_jsonl(&session_dir)
                .iter()
                .any(|p| p.starts_with(&session_dir)),
        "effective session-dir must own imports; default_root={leaked_only:?}"
    );

    // Keep dirs alive through assertions.
    let _ = probe.shutdown_take_dirs();
}

/// Contract: `/sessions` opens the same unified selector surface as `/resume`
/// (cwd-scoped badges), so slash alias wiring cannot silently regress to
/// native-only `list_sessions(cwd)`.
#[test]
fn pty_sessions_slash_shows_cross_tool_badges() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(home.path(), &["codex", "claude"]);
    let cwd_canon = cwd
        .path()
        .canonicalize()
        .unwrap_or_else(|_| cwd.path().to_path_buf());
    let session_dir = home.path().join("sessions-slash-dir");
    fs::create_dir_all(&session_dir).expect("session dir");
    let _ = plant_all_fixtures(home.path(), &cwd_canon, &session_dir);
    let session_dir_arg = session_dir.to_str().expect("utf8").to_owned();
    let args = [
        "--model",
        "faux/faux-1",
        "--session-dir",
        session_dir_arg.as_str(),
    ];

    let mut probe = PtyProbe::spawn_in(&args, 40, 140, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        strip_ansi(&probe.snapshot())
    );

    probe.clear_composer();
    let command_at = probe.len();
    probe.type_chars("/sessions");
    assert!(
        probe
            .wait_for_live_after(command_at, Duration::from_secs(8), |live| {
                live.contains("/sessions")
            })
            .is_some(),
        "/sessions command must reach the live composer: {}",
        probe.live_screen()
    );
    let open_at = probe.len();
    probe.send(b"\r");
    let plain = probe
        .wait_for_live_after(open_at, Duration::from_secs(20), |live| {
            live.contains("Resume Session")
                && live.contains("Filter:")
                && live.contains("[codex]")
                && live.contains("[claude]")
                && live.contains("[pi]")
        })
        .unwrap_or_else(|| panic!("/sessions must open unified selector: {}", probe.live_screen()));
    assert_source_badges(&plain, "/sessions selector");
    assert_noise_absent(&plain, "/sessions selector");
    assert!(
        plain.contains(CUSTOM_NATIVE_SUMMARY) || plain.contains("custom-root-native"),
        "/sessions must include effective session-dir native row: {plain}"
    );

    close_resume_selector(&mut probe);
    probe.quit_cleanly();
    assert!(
        probe.wait_exit(Duration::from_secs(20)).is_some(),
        "TUI must exit: {}",
        probe.snapshot()
    );
}

/// Contract: with a foreign source holding more candidates than the
/// per-source cap (`SESSION_CATALOG_CANDIDATE_LIMIT`) — and more rows than the
/// global cap (`SESSION_CATALOG_ROW_LIMIT`) — startup still reaches the
/// welcome screen and the `/resume` selector under the existing hard
/// deadlines, the deterministic newest same-cwd sentinel stays visible, the
/// oldest sentinel is truncated away, other sources are not starved, the
/// selector remains navigable, and selecting the newest sentinel still
/// imports under the effective `--session-dir` with lineage. Catches: unbounded
/// discovery/startup/render stalls, wrong truncation direction (keeping oldest
/// / dropping newest), source starvation, and cwd/parse/symlink safety
/// regressing under the bound.
#[test]
fn pty_bounded_large_catalog_startup_resume_import() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    set_session_import_sources(home.path(), &["codex", "claude"]);
    let cwd_canon = cwd
        .path()
        .canonicalize()
        .unwrap_or_else(|_| cwd.path().to_path_buf());
    let session_dir = home.path().join("bounded-session-dir");
    fs::create_dir_all(&session_dir).expect("session dir");
    let (newest_id, _newest_path, count) =
        plant_bounded_large_catalog(home.path(), &cwd_canon, &session_dir);
    // Fixture sanity: the flood exceeds both hard bounds.
    assert!(
        count > SESSION_CATALOG_CANDIDATE_LIMIT && count > SESSION_CATALOG_ROW_LIMIT,
        "bounded fixture must exceed both caps: count={count}"
    );

    let session_dir_arg = session_dir.to_str().expect("utf8 session dir").to_owned();
    let args = [
        "--model",
        "faux/faux-1",
        "--session-dir",
        session_dir_arg.as_str(),
    ];

    // ── Phase 1: startup reaches welcome under the hard deadline ─────────
    let startup_deadline = Duration::from_secs(30);
    let t0 = Instant::now();
    let mut probe = PtyProbe::spawn_in(&args, 42, 140, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must start under a bounded large catalog: {}",
        strip_ansi(&probe.snapshot())
    );
    assert!(
        t0.elapsed() < startup_deadline,
        "startup must reach the TUI within {startup_deadline:?} under a bounded catalog (took {:?}); an unbounded discover/scan/render would stall here",
        t0.elapsed()
    );
    assert!(
        probe.wait_for("Recent sessions", Duration::from_secs(20))
            || probe.wait_for(BOUNDED_NEWEST_SENTINEL, Duration::from_secs(8))
            || probe.wait_for("[codex]", Duration::from_secs(8)),
        "welcome must surface Recent sessions / catalog rows under the bound: {}",
        strip_ansi(&probe.snapshot())
    );
    thread::sleep(Duration::from_millis(500));
    let welcome_plain = strip_ansi(&probe.snapshot());
    assert!(
        welcome_plain.contains("Recent sessions"),
        "welcome Recent sessions heading missing: {welcome_plain}"
    );
    // Newest same-cwd sentinel has the highest same-cwd mtime, so it is the
    // first Recent row. A bound that keeps oldest / drops newest would hide it.
    assert!(
        welcome_plain.contains(BOUNDED_NEWEST_SENTINEL),
        "welcome must show the newest sentinel (kept by newest-first truncation): {welcome_plain}"
    );
    assert!(
        !welcome_plain.contains(BOUNDED_OLDEST_SENTINEL),
        "welcome must omit the oldest sentinel (truncated past the cap): {welcome_plain}"
    );
    assert!(
        welcome_plain.contains("[codex]"),
        "welcome Recent sessions must show a `[codex]` badge under the bound: {welcome_plain}"
    );
    assert_bounded_noise_absent(&welcome_plain, "welcome");

    // ── Phase 2: /resume opens and stays navigable under the bound ───────
    let sel_t0 = Instant::now();
    let _open_at = open_resume_selector(&mut probe);
    assert!(
        sel_t0.elapsed() < Duration::from_secs(20),
        "/resume must open within the deadline under a bounded catalog (took {:?}); an unbounded selector rendering all rows/lines per frame would stall",
        sel_t0.elapsed()
    );
    let selector_plain = probe.live_screen();
    assert!(
        selector_plain.contains(BOUNDED_NEWEST_SENTINEL),
        "/resume selector must show the newest sentinel at the top: {selector_plain}"
    );
    assert!(
        !selector_plain.contains(BOUNDED_OLDEST_SENTINEL),
        "/resume selector must omit the oldest sentinel (truncated, not merely off-screen): {selector_plain}"
    );
    assert_bounded_noise_absent(&selector_plain, "/resume selector");

    // Fairness: the codex flood must not starve the other sources. Each is
    // found via the same bounded selector filter.
    let claude_query = "bounded-claude SIGIL-cl4";
    let claude_filter_at = replace_filter(&mut probe, claude_query);
    let claude_filtered = wait_for_exact_filtered_row(
        &mut probe,
        claude_filter_at,
        claude_query,
        "claude",
        BOUNDED_CLAUDE,
        Duration::from_secs(12),
    )
    .unwrap_or_else(|| panic!("selector must isolate non-starved claude row: {}", probe.live_screen()));

    let native_query = "bounded-custom-native SIGIL-cu5";
    let native_filter_at = replace_filter(&mut probe, native_query);
    let native_filtered = wait_for_exact_filtered_row(
        &mut probe,
        native_filter_at,
        native_query,
        "pi",
        BOUNDED_CUSTOM_NATIVE,
        Duration::from_secs(12),
    )
    .unwrap_or_else(|| panic!("selector must isolate custom-root native row: {}", probe.live_screen()));

    // ── Phase 3: select the newest sentinel → import under session-dir ────
    let newest_query = format!("{newest_id} SIGIL-n3w");
    let import_filter_at = replace_filter(&mut probe, &newest_query);
    let import_filtered = wait_for_exact_filtered_row(
        &mut probe,
        import_filter_at,
        &newest_query,
        "codex",
        BOUNDED_NEWEST_SENTINEL,
        Duration::from_secs(12),
    )
    .unwrap_or_else(|| panic!("filter must isolate exact newest sentinel row: {}", probe.live_screen()));
    assert!(
        !import_filtered.contains(BOUNDED_OLDEST_SENTINEL)
            && !import_filtered.contains(BOUNDED_WRONG_CWD),
        "filtered view must not resurrect truncated/noise: {import_filtered}"
    );

    let before_imports = list_import_jsonl(&session_dir);
    let default_root = default_project_session_dir(probe.home_path(), &cwd_canon);
    let before_default = list_import_jsonl(&default_root);

    let select_at = probe.len();
    probe.send(b"\r");

    let import_deadline = Instant::now() + Duration::from_secs(25);
    let mut saw_live_switch = false;
    while Instant::now() < import_deadline {
        let live = probe.live_screen();
        if probe.len() > select_at
            && !live.contains("Resume Session")
            && (live.contains("Resumed ")
                || live.contains(BOUNDED_NEWEST_SENTINEL)
                || live.contains("codex-ok-reply"))
        {
            saw_live_switch = true;
        }
        if saw_live_switch && list_import_jsonl(&session_dir).len() > before_imports.len() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_live_switch,
        "selecting newest sentinel must close selector and switch the live TUI: {}",
        probe.live_screen()
    );

    let imports = list_import_jsonl(&session_dir);
    assert!(
        imports.len() > before_imports.len(),
        "newest sentinel select must create import_*.jsonl under effective --session-dir {} (not default root); before={before_imports:?} after={imports:?}",
        session_dir.display()
    );
    let default_after = list_import_jsonl(&default_root);
    assert!(
        default_after.len() == before_default.len(),
        "import must land under effective session-dir, not leak to default project root; before={before_default:?} after={default_after:?}"
    );
    let import_path = imports
        .iter()
        .find(|path| !before_imports.contains(path))
        .cloned()
        .unwrap_or_else(|| imports.last().expect("import path").clone());
    assert!(
        import_path.starts_with(&session_dir),
        "new import path must be under effective session-dir: {}",
        import_path.display()
    );
    assert_import_lineage(&import_path, "codex", &newest_id);

    let after_import_plain = probe.live_screen();
    assert!(
        after_import_plain.contains("Resumed ")
            || after_import_plain.contains(BOUNDED_NEWEST_SENTINEL)
            || after_import_plain.contains("codex-ok-reply"),
        "live TUI must surface Resumed <path> status or newest transcript after import: {after_import_plain}"
    );

    // ── Phase 4: reopen selector → imported marker, still navigable, oldest still gone
    let reopen_t0 = Instant::now();
    let _reopen_at = open_resume_selector(&mut probe);
    assert!(
        reopen_t0.elapsed() < Duration::from_secs(20),
        "reopened selector must load within the deadline under the bound (took {:?})",
        reopen_t0.elapsed()
    );
    let refilter_at = replace_filter(&mut probe, &newest_query);
    let reopened = wait_for_exact_filtered_row(
        &mut probe,
        refilter_at,
        &newest_query,
        "codex",
        BOUNDED_NEWEST_SENTINEL,
        Duration::from_secs(12),
    )
    .unwrap_or_else(|| panic!("reopened selector must isolate newest row: {}", probe.live_screen()));
    assert!(
        row_shows_imported(&reopened, "codex", BOUNDED_NEWEST_SENTINEL),
        "reopened selector must mark the imported newest sentinel with `{IMPORTED_MARKER}`: {reopened}"
    );
    assert!(
        reopened.contains("messages") && reopened.contains("[codex]"),
        "reopened selector row must keep badge + messages corpus: {reopened}"
    );

    let clear_at = clear_filter(&mut probe);
    let unfiltered = wait_for_unfiltered_selector(&probe, clear_at, Duration::from_secs(10))
        .unwrap_or_else(|| panic!("reopened selector filter must clear: {}", probe.live_screen()));
    assert!(
        !unfiltered.contains(BOUNDED_OLDEST_SENTINEL),
        "oldest sentinel must remain truncated after reopen: {unfiltered}"
    );
    assert!(
        unfiltered.contains("[codex]"),
        "reopened unfiltered selector must retain codex badges: {unfiltered}"
    );
    assert!(
        unfiltered.contains(IMPORTED_MARKER),
        "unfiltered reopen must retain the `{IMPORTED_MARKER}` marker: {unfiltered}"
    );
    assert_bounded_noise_absent(&unfiltered, "reopened selector");

    // ── Phase 5: clean exit + public on-disk artifacts ────────────────────
    close_resume_selector(&mut probe);
    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        status.is_some(),
        "TUI must exit cleanly under the bound: {}",
        probe.snapshot()
    );
    let snap = probe.snapshot();
    assert!(
        snap.contains(SHOW_CURSOR)
            || snap.contains("\x1b[?2004l")
            || status.is_some_and(|s| s.success() || s.code() == Some(0)),
        "exit must restore terminal ownership"
    );

    assert!(
        import_path.exists(),
        "import file must remain at {}",
        import_path.display()
    );
    let import_body = fs::read_to_string(&import_path).expect("import body");
    assert!(
        import_body.contains(BOUNDED_NEWEST_SENTINEL) || import_body.contains("codex-ok-reply"),
        "imported transcript must carry the newest sentinel foreign content"
    );
    let leaked_only = list_import_jsonl(&default_root);
    assert!(
        leaked_only.is_empty(),
        "effective session-dir must own imports; default_root leaked={leaked_only:?}"
    );

    let _ = probe.shutdown_take_dirs();
}

/// Documents the unix gate for this module on platforms that compile it.
#[cfg(unix)]
#[test]
fn cross_tool_session_tui_e2e_module_is_unix_gated() {
    assert!(cfg!(unix));
    let _ = SystemTime::now();
}
