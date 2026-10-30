//! Real TUI/`--resume` path after a revision-2 orchestration sidecar, plus an
//! explicit `workspace-write` TUI/tmux/print development simulation.
//!
//! Unix only. Isolated TempDir HOME/cwd — never the host `~/.rpi/sessions`.
//! Proves `application_from_session_candidate` resumes a bumped sidecar
//! instead of dying with `durable orchestration policy does not match
//! resumed generation`, and that confined workspace-write TUI/tmux/`--print`
//! start, take a faux turn, show `sandbox.mode`, and quit without bwrap panics.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::Termios;
use pi_coding::{DURABLE_STATE_VERSION, DurablePolicyState, DurableState};
use pi_sandbox::{PolicyRevision, PolicySnapshot};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const CTRL_U: u8 = 0x15;
const PTY_OUTPUT_CAP: usize = 512 * 1024;
const MISMATCH: &str = "durable orchestration policy does not match resumed generation";
const FAUX_RESPONSE: &str = "durable-resume-tui-e2e-reply";
const SETTINGS: &str =
    r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":true}}"#;
const WORKSPACE_WRITE_SETTINGS: &str = r#"{"sandbox":{"mode":"workspace-write"}}"#;
const SEED_PROMPT: &str = "durable-resume-seed-turn";

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
}

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

fn write_agent_settings(home: &Path, settings: &str) {
    let agent = home.join(".rpi");
    fs::create_dir_all(&agent).expect("agent dir");
    fs::write(agent.join("settings.json"), settings).expect("seed agent settings");
}

fn write_orchestration_settings(home: &Path) {
    write_agent_settings(home, SETTINGS);
}

fn apply_isolated_env(cmd: &mut Command, home: &Path) {
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
    cmd.env("TERM", "xterm-256color");
    cmd.env("RPI_OFFLINE", "1");
    cmd.env("RPI_SKIP_VERSION_CHECK", "1");
    cmd.env("PI_FAUX_RESPONSE", FAUX_RESPONSE);
}

struct PtyProbe {
    child: std::process::Child,
    writer: std::fs::File,
    buffer: Arc<Mutex<PtyOutputBuf>>,
    home: TempDir,
    cwd: TempDir,
}

impl PtyProbe {
    fn spawn_in(args: &[&str], rows: u16, cols: u16, home: TempDir, cwd: TempDir) -> Self {
        Self::spawn_in_with_settings(args, rows, cols, home, cwd, SETTINGS)
    }

    fn spawn_in_with_settings(
        args: &[&str],
        rows: u16,
        cols: u16,
        home: TempDir,
        cwd: TempDir,
        settings: &str,
    ) -> Self {
        write_agent_settings(home.path(), settings);

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
        apply_isolated_env(&mut cmd, home.path());
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
        }
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    fn snapshot(&self) -> String {
        self.buffer.lock().expect("buffer lock").snapshot()
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.snapshot();
            if snap.contains(needle) || strip_ansi(&snap).contains(needle) {
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
        self.send(&[CTRL_U]);
        thread::sleep(Duration::from_millis(80));
        self.send(b"/quit\r");
    }

    fn seed_session_turn(&mut self) {
        self.send(&[CTRL_U]);
        thread::sleep(Duration::from_millis(80));
        self.send(SEED_PROMPT.as_bytes());
        self.send(b"\r");
        assert!(
            self.wait_for(FAUX_RESPONSE, Duration::from_secs(20)),
            "seed turn must flush a faux assistant reply so the session jsonl exists: {}",
            strip_ansi(&self.snapshot())
        );
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

fn sessions_root(home: &Path) -> PathBuf {
    home.join(".rpi").join("sessions")
}

fn dump_tree(root: &Path) -> String {
    let mut lines = Vec::new();
    fn walk(dir: &Path, root: &Path, lines: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            lines.push(format!("<unreadable {}>", dir.display()));
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if path.is_dir() {
                lines.push(format!("{}/", rel.display()));
                walk(&path, root, lines);
            } else {
                lines.push(rel.display().to_string());
            }
        }
    }
    if !root.exists() {
        return format!("{} (missing)", root.display());
    }
    walk(root, root, &mut lines);
    if lines.is_empty() {
        format!("{} (empty)", root.display())
    } else {
        lines.join("\n")
    }
}

fn collect_native_jsonl(sessions: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, under_children: bool, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_children = entry.file_name() == "children";
            if path.is_dir() {
                walk(&path, under_children || is_children, out);
            } else if !under_children && path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    if sessions.is_dir() {
        walk(sessions, false, &mut out);
    }
    out
}

fn session_id_from_header(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let first = text.lines().next()?;
    let value: Value = serde_json::from_str(first).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

struct LocatedSession {
    id: String,
    path: PathBuf,
    sidecar: PathBuf,
}

fn locate_session_and_sidecar(home: &Path) -> LocatedSession {
    let sessions = sessions_root(home);
    let jsonl = collect_native_jsonl(&sessions);
    let tree = dump_tree(&sessions);
    assert!(
        !jsonl.is_empty(),
        "first launch must write a native session jsonl under {}; tree:\n{tree}",
        sessions.display()
    );

    let mut found = Vec::new();
    for path in &jsonl {
        let Some(id) = session_id_from_header(path) else {
            continue;
        };
        let candidates = [
            path.parent()
                .unwrap_or(path)
                .join("children")
                .join(&id)
                .join("orchestration-state.json"),
            sessions
                .join("children")
                .join(&id)
                .join("orchestration-state.json"),
        ];
        if let Some(sidecar) = candidates.into_iter().find(|candidate| candidate.is_file()) {
            found.push(LocatedSession {
                id,
                path: path.clone(),
                sidecar,
            });
        }
    }

    assert!(
        !found.is_empty(),
        "orchestration.tasks=true must write children/<id>/orchestration-state.json; \
         native jsonl={jsonl:?}; tree:\n{tree}"
    );
    found.remove(0)
}

fn bump_sidecar_revision(sidecar: &Path) {
    let mut state: DurableState =
        serde_json::from_slice(&fs::read(sidecar).expect("read sidecar")).expect("decode sidecar");
    let captured = state.durable_policy.clone();
    state.version = DURABLE_STATE_VERSION;
    state.durable_policy = DurablePolicyState::new(
        PolicySnapshot::new(
            PolicyRevision::new(2).expect("revision 2"),
            captured.snapshot().created_on(),
            captured.snapshot().policy().clone(),
        )
        .expect("revision-2 snapshot"),
        captured.credential_grants().clone(),
    )
    .expect("revision-2 durable policy");
    fs::write(
        sidecar,
        serde_json::to_vec_pretty(&state).expect("serialize sidecar"),
    )
    .expect("rewrite sidecar");

    let rewritten: DurableState =
        serde_json::from_slice(&fs::read(sidecar).expect("reread sidecar"))
            .expect("redecode sidecar");
    assert_eq!(
        rewritten.durable_policy.snapshot().revision().get(),
        2,
        "sidecar digest rewrite must persist revision 2"
    );
}

fn assert_no_mismatch(label: &str, snapshot: &str) {
    let plain = strip_ansi(snapshot);
    assert!(
        !snapshot.contains(MISMATCH) && !plain.contains(MISMATCH),
        "{label} must not surface the revision-mismatch bail: {plain}"
    );
}

fn assert_no_sandbox_bootstrap_panic(label: &str, snapshot: &str) {
    let plain = strip_ansi(snapshot);
    const MARKERS: &[&str] = &[
        "validated Application bootstrap",
        "thread 'main' panicked",
        "panicked at",
        "sandbox bootstrap",
        "Can't find source path /proc/self/fd",
        "binding orchestration to application session",
    ];
    for marker in MARKERS {
        assert!(
            !snapshot.contains(marker) && !plain.contains(marker),
            "{label} must not contain a sandbox bootstrap panic ({marker}): {plain}"
        );
    }
}

fn assert_clean_quit(status: Option<std::process::ExitStatus>, snapshot: &str) {
    let status = status.expect("TUI must exit after /quit");
    assert_no_mismatch("quit snapshot", snapshot);
    assert!(
        status.success() || status.code() != Some(1) || snapshot.contains(SHOW_CURSOR),
        "resume /quit must not crash-exit from a policy mismatch: status={status:?} snapshot={}",
        strip_ansi(snapshot)
    );
}

#[test]
fn pty_resume_after_revision_2_sidecar() {
    let home = TempDir::new().expect("isolated HOME");
    let cwd = TempDir::new().expect("isolated cwd");
    eprintln!(
        "pty launch: {} --model faux/faux-1 (HOME={}, cwd={})",
        rpi_bin(),
        home.path().display(),
        cwd.path().display()
    );

    let mut probe = PtyProbe::spawn_in(&["--model", "faux/faux-1"], 36, 120, home, cwd);
    assert!(
        await_entered(&probe),
        "TUI must start: {}",
        strip_ansi(&probe.snapshot())
    );
    probe.seed_session_turn();
    probe.quit_cleanly();
    let first_status = probe.wait_exit(Duration::from_secs(20));
    assert!(
        first_status.is_some(),
        "first TUI must exit after /quit: {}",
        strip_ansi(&probe.snapshot())
    );
    let (home, cwd) = probe.shutdown_take_dirs();

    let located = locate_session_and_sidecar(home.path());
    eprintln!(
        "sidecar found: id={} session={} sidecar={}",
        located.id,
        located.path.display(),
        located.sidecar.display()
    );
    bump_sidecar_revision(&located.sidecar);

    let resume_id = located.id.clone();
    let mut probe = PtyProbe::spawn_in(
        &["--model", "faux/faux-1", "--resume", resume_id.as_str()],
        36,
        120,
        home,
        cwd,
    );
    let snap = probe.snapshot();
    assert_no_mismatch("resume PTY snapshot", &snap);
    assert!(
        await_entered(&probe),
        "TUI must resume after revision-2 sidecar: {}",
        strip_ansi(&probe.snapshot())
    );
    assert_no_mismatch("entered resume snapshot", &probe.snapshot());

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    assert_clean_quit(status, &probe.snapshot());
}

#[test]
fn pty_workspace_write_tui_starts_and_quits() {
    let home = TempDir::new().expect("isolated HOME");
    let cwd = TempDir::new().expect("isolated cwd");
    eprintln!(
        "pty workspace-write launch: {} --model faux/faux-1 (HOME={}, cwd={})",
        rpi_bin(),
        home.path().display(),
        cwd.path().display()
    );

    let mut probe = PtyProbe::spawn_in_with_settings(
        &["--model", "faux/faux-1"],
        36,
        120,
        home,
        cwd,
        WORKSPACE_WRITE_SETTINGS,
    );
    let entered = await_entered(&probe);
    let snap = probe.snapshot();
    assert_no_mismatch("workspace-write start snapshot", &snap);
    assert_no_sandbox_bootstrap_panic("workspace-write start snapshot", &snap);
    assert!(
        entered,
        "workspace-write TUI must start (do not skip on confined bootstrap/bwrap failure): {}",
        strip_ansi(&snap)
    );
    eprintln!("workspace-write TUI entered");

    probe.seed_session_turn();
    assert_no_sandbox_bootstrap_panic("workspace-write seed snapshot", &probe.snapshot());

    probe.send(&[CTRL_U]);
    thread::sleep(Duration::from_millis(80));
    probe.send(b"/settings\r");
    assert!(
        probe.wait_for("Settings", Duration::from_secs(20)),
        "workspace-write /settings must open: {}",
        strip_ansi(&probe.snapshot())
    );
    probe.send(b"sandbox.mode");
    assert!(
        probe.wait_for("workspace-write", Duration::from_secs(10))
            || strip_ansi(&probe.snapshot()).contains("workspace-write"),
        "settings must show sandbox.mode=workspace-write: {}",
        strip_ansi(&probe.snapshot())
    );
    probe.send(b"\x1b");
    thread::sleep(Duration::from_millis(120));

    probe.quit_cleanly();
    let status = probe.wait_exit(Duration::from_secs(20));
    let quit_snap = probe.snapshot();
    assert_no_sandbox_bootstrap_panic("workspace-write quit snapshot", &quit_snap);
    assert_clean_quit(status, &quit_snap);
    eprintln!("workspace-write TUI quit cleanly after seed turn + settings");
}

#[test]
fn print_workspace_write_turn_does_not_panic() {
    let home = TempDir::new().expect("isolated HOME");
    let cwd = TempDir::new().expect("isolated cwd");
    write_agent_settings(home.path(), WORKSPACE_WRITE_SETTINGS);
    let mut cmd = Command::new(rpi_bin());
    apply_isolated_env(&mut cmd, home.path());
    cmd.args([
        "--model",
        "faux/faux-1",
        "--print",
        "workspace-write-print-turn",
    ]);
    cmd.current_dir(cwd.path());
    cmd.stdin(Stdio::null());
    let output = cmd.output().expect("spawn rpi --print");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    assert_no_mismatch("print workspace-write", &combined);
    assert_no_sandbox_bootstrap_panic("print workspace-write", &combined);
    assert!(
        output.status.success(),
        "confined --print must succeed: status={:?} stdout={stdout} stderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains(FAUX_RESPONSE) || stderr.contains(FAUX_RESPONSE),
        "confined --print must stream the faux reply: stdout={stdout} stderr={stderr}"
    );
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

struct TmuxSession {
    name: String,
}

impl TmuxSession {
    fn start(home: &Path, cwd: &Path, extra_args: &[&str]) -> Self {
        let name = format!("rpi-dur-resume-{}", Uuid::new_v4().simple());
        let mut command = format!(
            "env -i HOME={} PATH={} TERM=xterm-256color RPI_OFFLINE=1 RPI_SKIP_VERSION_CHECK=1 PI_FAUX_RESPONSE={} {} --model faux/faux-1",
            sh_quote(&home.display().to_string()),
            sh_quote(&std::env::var("PATH").unwrap_or_default()),
            sh_quote(FAUX_RESPONSE),
            sh_quote(&rpi_bin()),
        );
        for arg in extra_args {
            command.push(' ');
            command.push_str(&sh_quote(arg));
        }
        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "120",
                "-y",
                "36",
                "-c",
            ])
            .arg(cwd)
            .arg(&command)
            .status()
            .expect("tmux new-session");
        assert!(status.success(), "tmux new-session {name} failed: {status}");
        Self { name }
    }

    fn capture(&self) -> String {
        let output = Command::new("tmux")
            .args([
                "capture-pane",
                "-p",
                "-S",
                "-2000",
                "-t",
                &format!("{}:0", self.name),
            ])
            .output()
            .expect("tmux capture-pane");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn has_session(&self) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn wait_entered(&self, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let capture = self.capture();
            if capture_shows_ready(&capture) {
                return capture;
            }
            if Instant::now() >= deadline {
                panic!(
                    "tmux session {} never showed a TUI ready marker: {}",
                    self.name, capture
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let capture = self.capture();
            if capture.contains(needle) {
                return capture;
            }
            if Instant::now() >= deadline {
                panic!(
                    "tmux session {} never showed {needle:?}: {capture}",
                    self.name
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn send_key(&self, key: &str) {
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", self.name), key])
            .status();
    }

    fn send_literal(&self, text: &str) {
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", self.name), "-l", text])
            .status();
    }

    fn seed_session_turn(&self) {
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", self.name), "C-u"])
            .status();
        thread::sleep(Duration::from_millis(80));
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &format!("{}:0", self.name),
                "-l",
                SEED_PROMPT,
            ])
            .status();
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", self.name), "Enter"])
            .status();
        let capture = self.wait_for(FAUX_RESPONSE, Duration::from_secs(20));
        assert_no_mismatch("tmux seed-turn capture", &capture);
    }

    fn quit_and_wait(&self) {
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", self.name), "C-u"])
            .status();
        thread::sleep(Duration::from_millis(80));
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &format!("{}:0", self.name),
                "-l",
                "/quit",
            ])
            .status();
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", self.name), "Enter"])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.has_session() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

fn capture_shows_ready(capture: &str) -> bool {
    capture.contains("pi (rs)")
        || capture.contains('π')
        || capture.contains("Ready")
        || capture.contains("ready")
        || capture.contains("faux/faux-1")
}

#[test]
fn tmux_resume_after_revision_2_sidecar() {
    if !tmux_available() {
        eprintln!("skip: tmux not installed");
        return;
    }

    let home = TempDir::new().expect("isolated HOME");
    let cwd = TempDir::new().expect("isolated cwd");
    write_orchestration_settings(home.path());
    eprintln!(
        "tmux launch: {} --model faux/faux-1 (HOME={}, cwd={})",
        rpi_bin(),
        home.path().display(),
        cwd.path().display()
    );

    let first = TmuxSession::start(home.path(), cwd.path(), &[]);
    let first_capture = first.wait_entered(Duration::from_secs(40));
    assert_no_mismatch("tmux first capture", &first_capture);
    first.seed_session_turn();
    first.quit_and_wait();
    drop(first);

    let located = locate_session_and_sidecar(home.path());
    eprintln!(
        "tmux sidecar found: id={} session={} sidecar={}",
        located.id,
        located.path.display(),
        located.sidecar.display()
    );
    bump_sidecar_revision(&located.sidecar);

    let resume = TmuxSession::start(home.path(), cwd.path(), &["--resume", located.id.as_str()]);
    let resume_capture = resume.wait_entered(Duration::from_secs(40));
    assert_no_mismatch("tmux resume capture", &resume_capture);
    assert!(
        capture_shows_ready(&resume_capture),
        "tmux resume must show TUI ready: {resume_capture}"
    );
    resume.quit_and_wait();
    eprintln!("tmux ran: session resumed after revision-2 sidecar");
}

#[test]
fn tmux_workspace_write_development_loop() {
    if !tmux_available() {
        panic!("tmux is required for workspace-write development simulation");
    }

    let home = TempDir::new().expect("isolated HOME");
    let cwd = TempDir::new().expect("isolated cwd");
    write_agent_settings(home.path(), WORKSPACE_WRITE_SETTINGS);
    eprintln!(
        "tmux workspace-write launch: {} --model faux/faux-1 (HOME={}, cwd={})",
        rpi_bin(),
        home.path().display(),
        cwd.path().display()
    );

    let session = TmuxSession::start(home.path(), cwd.path(), &[]);
    let entered = session.wait_entered(Duration::from_secs(40));
    assert_no_mismatch("tmux workspace-write start", &entered);
    assert_no_sandbox_bootstrap_panic("tmux workspace-write start", &entered);
    assert!(
        capture_shows_ready(&entered),
        "tmux workspace-write must show TUI ready: {entered}"
    );

    session.seed_session_turn();
    let after_turn = session.capture();
    assert_no_mismatch("tmux workspace-write seed", &after_turn);
    assert_no_sandbox_bootstrap_panic("tmux workspace-write seed", &after_turn);
    assert!(
        after_turn.contains(FAUX_RESPONSE),
        "tmux workspace-write seed turn must render the faux reply: {after_turn}"
    );
    session.send_key("C-u");
    thread::sleep(Duration::from_millis(80));
    session.send_literal("/settings");
    session.send_key("Enter");
    let settings = session.wait_for("Settings", Duration::from_secs(20));
    assert_no_sandbox_bootstrap_panic("tmux workspace-write settings", &settings);
    session.send_literal("sandbox.mode");
    let filtered = session.wait_for("workspace-write", Duration::from_secs(10));
    assert!(
        filtered.contains("workspace-write"),
        "tmux settings must show sandbox.mode workspace-write: {filtered}"
    );

    session.quit_and_wait();
    eprintln!("tmux workspace-write development loop entered, turned, opened settings, quit");
}
