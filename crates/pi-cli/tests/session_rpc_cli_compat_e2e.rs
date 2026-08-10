//! Session / RPC / CLI compatibility end-to-end contracts.
//!
//! Drives the real `rpi` binary (the `rpc` subcommand for the JSONL RPC
//! control plane, plus on-disk Pi v3 JSONL) for
//! public boundaries that are easy to regress without a full integration path:
//! session create → file → resume round-trip, sessionDir lifecycle, `-r`,
//! `:max` model suffixes, `@file` expansion, `get_commands` source projection,
//! RPC abort during an active turn, RPC runtime-generation switching, and
//! structured-stdout purity with startup diagnostics on stderr.
//!
//! Every test names the observable contract and the plausible bug it catches.
//! Global faux providers are not registered in-process; binary tests use the
//! built-in offline faux path (`PI_FAUX_RESPONSE` / `faux/faux-1`) only.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Spawn the real `rpi` binary with the `rpc` subcommand first — the
/// successor of the removed `rpi-rpc` companion binary (≡ `--mode rpc`).
fn rpc_cmd() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
    command.arg("rpc");
    command
}

fn write_agent_home(agent: &Path, settings: &str, models: &str) {
    fs::create_dir_all(agent).expect("agent home");
    fs::write(agent.join("settings.json"), settings).expect("settings.json");
    fs::write(agent.join("models.json"), models).expect("models.json");
}

fn offline_env<'a>(command: &'a mut Command, agent: &Path, cwd: &Path) -> &'a mut Command {
    command
        .current_dir(cwd)
        .env("HOME", agent)
        .env("USERPROFILE", agent)
        .env("PI_CODING_AGENT_DIR", agent)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GROQ_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("XAI_API_KEY")
}

/// Hard upper bound for one-shot `rpi` CLI invocations in this file.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(20);

/// Bytes retained from each pipe for diagnostics / return value. Readers still
/// drain to EOF so a chatty child cannot fill the OS pipe; only the prefix is kept.
const PIPE_DIAG_CAP: usize = 64 * 1024;

/// Bounded JSONL queue between the stdout reader thread and the test consumer.
/// Full queue makes the reader exit via try_send so Drop never joins a sender
/// blocked on an unbounded/full channel; the child then blocks on the OS pipe
/// until kill/reap.
const STDOUT_CHANNEL_CAP: usize = 256;

/// Hard caps on records retained by [`RpcSession::read_until`] before failing.
const READ_UNTIL_MAX_SEEN: usize = 512;
const READ_UNTIL_MAX_SEEN_BYTES: usize = 256 * 1024;

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
            }
            Err(_) => break,
        }
    }
    retained
}

/// Run a spawned command with a hard local deadline.
///
/// Drains stdout/stderr on dedicated threads from spawn (retaining only a fixed
/// diagnostic prefix while continuing to EOF), optionally writes and closes
/// stdin, then polls `try_wait` until exit or [`SUBPROCESS_TIMEOUT`]. On expiry
/// or `try_wait` error the child is killed and waited before readers are joined,
/// and the panic includes bounded stdout/stderr diagnostics. Readers are never
/// joined while the child may still hold pipe ends.
fn run_command_bounded(mut command: Command, stdin: Option<&str>) -> Output {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn subprocess");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");

    let stdout_reader = thread::spawn(move || drain_capped(stdout, PIPE_DIAG_CAP));
    let stderr_reader = thread::spawn(move || drain_capped(stderr, PIPE_DIAG_CAP));

    if let Some(input) = stdin {
        let mut sin = child.stdin.take().expect("stdin pipe");
        sin.write_all(input.as_bytes()).expect("write stdin");
        drop(sin); // close so the child sees EOF
    }

    let deadline = Instant::now() + SUBPROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stdout = stdout_reader
                        .join()
                        .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
                    let stderr = stderr_reader
                        .join()
                        .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
                    panic!(
                        "subprocess timed out after {}s\n--- stdout ---\n{}\n--- stderr ---\n{}",
                        SUBPROCESS_TIMEOUT.as_secs(),
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr),
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_reader
                    .join()
                    .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
                let stderr = stderr_reader
                    .join()
                    .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
                panic!(
                    "try_wait subprocess: {error}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                );
            }
        }
    };

    // Child is reaped: safe to join pipe readers (EOF follows close).
    let stdout = stdout_reader
        .join()
        .unwrap_or_else(|_| b"<stdout reader panicked>".to_vec());
    let stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| b"<stderr reader panicked>".to_vec());
    Output {
        status,
        stdout,
        stderr,
    }
}

fn run_rpi(agent: &Path, cwd: &Path, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    let mut command = Command::new(rpi_bin());
    offline_env(&mut command, agent, cwd)
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .env("PI_FAUX_RESPONSE", "compat-e2e-reply");
    let output = run_command_bounded(command, stdin);
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn plant_v3_session(directory: &Path, cwd: &Path, id: &str, user_text: &str) -> PathBuf {
    fs::create_dir_all(directory).expect("session directory");
    let path = directory.join(format!("{id}.jsonl"));
    let cwd = cwd.display().to_string().replace('\\', "\\\\");
    let user_text = user_text.replace('\\', "\\\\").replace('"', "\\\"");
    let body = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n\
         {{\"type\":\"model_change\",\"id\":\"mc1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00.100Z\",\"provider\":\"faux\",\"modelId\":\"faux-1\"}}\n\
         {{\"type\":\"thinking_level_change\",\"id\":\"tl1\",\"parentId\":\"mc1\",\"timestamp\":\"2026-01-01T00:00:00.200Z\",\"thinkingLevel\":\"off\"}}\n\
         {{\"type\":\"message\",\"id\":\"u1\",\"parentId\":\"tl1\",\"timestamp\":\"2026-01-01T00:00:00.300Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{user_text}\"}}],\"timestamp\":0}}}}\n\
         {{\"type\":\"message\",\"id\":\"a1\",\"parentId\":\"u1\",\"timestamp\":\"2026-01-01T00:00:00.400Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"planted reply\"}}],\"api\":\"faux\",\"provider\":\"faux\",\"model\":\"faux-1\",\"stopReason\":\"stop\",\"timestamp\":1}}}}\n"
    );
    fs::write(&path, body).expect("write planted session");
    path
}

fn list_jsonl(directory: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

fn read_jsonl_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read session jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("session line is not JSON ({error}): {line}"))
        })
        .collect()
}

/// Line delivered from the stdout reader thread (or a terminal error/EOF).
enum StdoutMsg {
    Line(String),
    Eof,
    IoError(String),
}

struct RpcSession {
    child: Option<Child>,
    lines_rx: Receiver<StdoutMsg>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<Vec<u8>>>,
    finished_stdout: Vec<u8>,
    _home: TempDir,
    _cwd: TempDir,
    session_dir: PathBuf,
}

impl RpcSession {
    fn spawn_with(args: &[&str], faux_response: &str) -> Self {
        let home = TempDir::new().expect("rpc home");
        let cwd = TempDir::new().expect("rpc cwd");
        let session_dir = home.path().join("rpc-sessions");
        fs::create_dir_all(&session_dir).expect("rpc session dir");
        write_agent_home(home.path(), "{}", "");
        let mut child = rpc_cmd();
        offline_env(&mut child, home.path(), cwd.path())
            .args(["--offline", "--model", "faux/faux-1", "--session-dir"])
            .arg(&session_dir)
            .args(args)
            .env("PI_FAUX_RESPONSE", faux_response)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = child.spawn().expect("spawn rpi rpc");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let (tx, rx) = mpsc::sync_channel::<StdoutMsg>(STDOUT_CHANNEL_CAP);
        let stdout_reader = thread::spawn(move || stdout_reader_loop(stdout, tx));
        // Drain stderr from spawn so a chatty child cannot fill the pipe and
        // deadlock while tests only consume stdout. Retain only a fixed prefix.
        let stderr_reader = thread::spawn(move || drain_capped(stderr, PIPE_DIAG_CAP));
        Self {
            child: Some(child),
            lines_rx: rx,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            finished_stdout: Vec::new(),
            _home: home,
            _cwd: cwd,
            session_dir,
        }
    }

    fn spawn() -> Self {
        // Long faux text stretches the chunked stream so mid-turn abort has a
        // real window instead of racing an instantaneous completion.
        let long = "compat-abort-".repeat(400);
        Self::spawn_with(&[], &long)
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("rpi rpc child already finished")
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.child_mut().stdin.as_mut().expect("stdin");
        stdin.write_all(line.as_bytes()).expect("write stdin");
        if !line.ends_with('\n') {
            stdin.write_all(b"\n").expect("write lf");
        }
        stdin.flush().expect("flush stdin");
    }

    fn close_stdin(&mut self) {
        if let Some(child) = self.child.as_mut() {
            drop(child.stdin.take());
        }
    }

    fn kill_child(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }

    /// Append one stdout line into the retained diagnostic buffer (prefix only).
    fn retain_stdout_line(&mut self, line: &str) {
        if self.finished_stdout.len() >= PIPE_DIAG_CAP {
            return;
        }
        let room = PIPE_DIAG_CAP - self.finished_stdout.len();
        let bytes = line.as_bytes();
        // Prefer keeping a trailing newline when any room remains.
        let take = bytes.len().min(room.saturating_sub(1));
        if take > 0 {
            self.finished_stdout.extend_from_slice(&bytes[..take]);
        }
        if self.finished_stdout.len() < PIPE_DIAG_CAP {
            self.finished_stdout.push(b'\n');
        }
    }

    /// Read the next non-empty JSONL record, failing if the deadline elapses.
    /// Uses a dedicated reader thread so the Instant deadline can fire even
    /// while the OS read would otherwise block forever.
    fn read_json_deadline(&mut self, deadline: Instant) -> Value {
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.kill_child();
                panic!("timed out waiting for next JSONL record from rpi rpc");
            }
            let remaining = deadline.saturating_duration_since(now);
            match self.lines_rx.recv_timeout(remaining) {
                Ok(StdoutMsg::Line(line)) => {
                    self.retain_stdout_line(&line);
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    assert!(
                        !trimmed.contains('\u{1b}'),
                        "RPC stdout must not contain ANSI: {trimmed:?}"
                    );
                    let value = serde_json::from_str::<Value>(trimmed).unwrap_or_else(|error| {
                        panic!("RPC stdout is not JSON ({error}): {trimmed}")
                    });
                    assert!(value.is_object(), "RPC stdout must be an object: {value}");
                    return value;
                }
                Ok(StdoutMsg::Eof) => {
                    let status = self
                        .child
                        .as_mut()
                        .and_then(|child| child.try_wait().ok().flatten());
                    panic!("rpi rpc stdout closed early (status={status:?})");
                }
                Ok(StdoutMsg::IoError(error)) => {
                    self.kill_child();
                    panic!("reading rpi rpc stdout: {error}");
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.kill_child();
                    panic!("timed out waiting for next JSONL record from rpi rpc");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self
                        .child
                        .as_mut()
                        .and_then(|child| child.try_wait().ok().flatten());
                    panic!("rpi rpc stdout reader disconnected (status={status:?})");
                }
            }
        }
    }

    fn read_json_timeout(&mut self, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines_rx.recv_timeout(remaining) {
                Ok(StdoutMsg::Line(line)) => {
                    self.retain_stdout_line(&line);
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    assert!(!trimmed.contains('\u{1b}'), "RPC stdout must not contain ANSI: {trimmed:?}");
                    let value = serde_json::from_str::<Value>(trimmed)
                        .unwrap_or_else(|error| panic!("RPC stdout is not JSON ({error}): {trimmed}"));
                    assert!(value.is_object(), "RPC stdout must be an object: {value}");
                    return Some(value);
                }
                Ok(StdoutMsg::Eof) | Err(RecvTimeoutError::Timeout) => return None,
                Ok(StdoutMsg::IoError(error)) => panic!("reading rpi rpc stdout: {error}"),
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn read_until(
        &mut self,
        deadline: Instant,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> (Vec<Value>, Value) {
        let mut seen = Vec::new();
        let mut seen_bytes = 0usize;
        loop {
            let value = self.read_json_deadline(deadline);
            if pred(&value) {
                return (seen, value);
            }
            // Approximate retained size without a second full clone of nested trees.
            let encoded_len = value.to_string().len();
            if seen.len() >= READ_UNTIL_MAX_SEEN
                || seen_bytes.saturating_add(encoded_len) > READ_UNTIL_MAX_SEEN_BYTES
            {
                self.kill_child();
                panic!(
                    "read_until retained too much before match \
                     (records={}, bytes≈{}, limits records={} bytes={})",
                    seen.len() + 1,
                    seen_bytes.saturating_add(encoded_len),
                    READ_UNTIL_MAX_SEEN,
                    READ_UNTIL_MAX_SEEN_BYTES,
                );
            }
            seen_bytes += encoded_len;
            seen.push(value);
        }
    }

    /// Bounded shutdown: close stdin, drain stdout while polling exit, kill on
    /// deadline, and only then join reader threads (never join while the child
    /// may still hold pipe ends).
    fn finish(mut self) -> Output {
        self.close_stdin();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = None;

        while Instant::now() < deadline {
            // Prefer non-blocking exit checks; drain any ready stdout between polls.
            if let Some(child) = self.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        status = Some(exit);
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        // Force termination so Drop/join cannot race a live child.
                        if let Some(mut child) = self.child.take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                        panic!("try_wait rpi rpc: {error}");
                    }
                }
            } else {
                break;
            }

            match self.lines_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(StdoutMsg::Line(line)) => {
                    self.retain_stdout_line(&line);
                }
                Ok(StdoutMsg::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    // Stdout closed; keep polling exit until deadline.
                }
                Ok(StdoutMsg::IoError(_)) => {}
                Err(RecvTimeoutError::Timeout) => {}
            }
        }

        if status.is_none() {
            // Deadline elapsed or child still running: force termination.
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                status = Some(child.wait().expect("wait rpi rpc after kill"));
            }
        } else if let Some(mut child) = self.child.take() {
            // Exit already observed via try_wait; reap without blocking forever.
            let _ = child.try_wait();
            // Child is already exited; Drop must not see it.
            drop(child);
        }

        // Child is confirmed dead: safe to join pipe readers (EOF follows close).
        while let Ok(msg) = self.lines_rx.try_recv() {
            if let StdoutMsg::Line(line) = msg {
                self.retain_stdout_line(&line);
            }
        }
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        let stderr = self
            .stderr_reader
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stdout = std::mem::take(&mut self.finished_stdout);
        Output {
            status: status.expect("rpi rpc exit status"),
            stdout,
            stderr,
        }
    }
}

impl Drop for RpcSession {
    fn drop(&mut self) {
        // Kill+wait any still-running child BEFORE joining readers so a stuck
        // OS read cannot outlive the test after assertion panics/timeouts.
        // finish() takes the Child first so this is a no-op after a clean exit.
        // The stdout reader uses try_send only, so it never blocks on a full
        // channel; after kill it observes pipe EOF/error and exits, making join safe.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Drop queued messages so the SyncSender side cannot keep memory alive.
        while self.lines_rx.try_recv().is_ok() {}
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

fn stdout_try_send(tx: &SyncSender<StdoutMsg>, msg: StdoutMsg) -> bool {
    match tx.try_send(msg) {
        Ok(()) => true,
        // Full: exit so we never block Drop's join on a saturated queue. The
        // child will eventually block on the OS pipe; consumer deadline/Drop kills.
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
    }
}

fn stdout_reader_loop(mut stdout: impl Read + Send + 'static, tx: SyncSender<StdoutMsg>) {
    // Chunked line split with a hard per-line cap so a missing LF cannot grow an
    // unbounded String. Oversized lines report IoError; the session side kills
    // the child on that path (deadline / Drop also force-terminate). try_send
    // keeps the reader from blocking on a full bounded channel.
    let mut pending = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                if !pending.is_empty() {
                    if pending.len() > PIPE_DIAG_CAP {
                        let _ = stdout_try_send(
                            &tx,
                            StdoutMsg::IoError(format!(
                                "RPC stdout line exceeded {PIPE_DIAG_CAP} bytes"
                            )),
                        );
                    } else {
                        if pending.last() == Some(&b'\r') {
                            pending.pop();
                        }
                        let payload = String::from_utf8_lossy(&pending).into_owned();
                        let _ = stdout_try_send(&tx, StdoutMsg::Line(payload));
                    }
                }
                let _ = stdout_try_send(&tx, StdoutMsg::Eof);
                break;
            }
            Ok(n) => {
                let mut start = 0usize;
                for i in 0..n {
                    if chunk[i] != b'\n' {
                        continue;
                    }
                    pending.extend_from_slice(&chunk[start..i]);
                    start = i + 1;
                    if pending.len() > PIPE_DIAG_CAP {
                        let _ = stdout_try_send(
                            &tx,
                            StdoutMsg::IoError(format!(
                                "RPC stdout line exceeded {PIPE_DIAG_CAP} bytes"
                            )),
                        );
                        return;
                    }
                    if pending.last() == Some(&b'\r') {
                        pending.pop();
                    }
                    let payload = String::from_utf8_lossy(&pending).into_owned();
                    pending.clear();
                    if !stdout_try_send(&tx, StdoutMsg::Line(payload)) {
                        return;
                    }
                }
                if start < n {
                    pending.extend_from_slice(&chunk[start..n]);
                    if pending.len() > PIPE_DIAG_CAP {
                        let _ = stdout_try_send(
                            &tx,
                            StdoutMsg::IoError(format!(
                                "RPC stdout line exceeded {PIPE_DIAG_CAP} bytes"
                            )),
                        );
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = stdout_try_send(&tx, StdoutMsg::IoError(error.to_string()));
                break;
            }
        }
    }
}

fn is_response(line: &Value) -> bool {
    line.get("type").and_then(Value::as_str) == Some("response")
}

fn assert_success(line: &Value, command: &str, id: &str) {
    assert_eq!(line.get("type").and_then(Value::as_str), Some("response"));
    assert_eq!(line.get("command").and_then(Value::as_str), Some(command));
    assert_eq!(line.get("id").and_then(Value::as_str), Some(id));
    assert_eq!(
        line.get("success").and_then(Value::as_bool),
        Some(true),
        "expected success for {command}/{id}: {line}"
    );
}

/// Contract: a print-mode create writes a Pi v3 JSONL session that resumes with
/// the same id, cwd, model_change, and message branch.
///
/// Plausible bug: header version drift, missing model_change on create, or
/// resume loading a different file / dropping branch messages.
#[test]
fn pi_v3_session_jsonl_create_round_trips_through_resume() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("v3-roundtrip");
    write_agent_home(agent.path(), "{}", "");

    let sessions_arg = sessions.to_str().expect("utf8 sessions");
    let (created, out, err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            sessions_arg,
            "--session-id",
            "v3-roundtrip",
            "--print",
            "remember this branch",
        ],
        None,
    );
    assert!(created, "create failed: stdout={out} stderr={err}");
    assert!(
        out.contains("compat-e2e-reply"),
        "print mode must stream faux text: {out}"
    );

    let path = sessions.join("v3-roundtrip.jsonl");
    assert!(path.is_file(), "explicit session id must land at {path:?}");
    let records = read_jsonl_records(&path);
    assert!(
        records
            .iter()
            .any(|record| record.get("type").and_then(Value::as_str) == Some("session")
                && record.get("version").and_then(Value::as_u64) == Some(3)
                && record.get("id").and_then(Value::as_str) == Some("v3-roundtrip")),
        "session header must be Pi v3 with the explicit id: {records:?}"
    );
    assert!(
        records.iter().any(|record| {
            record.get("type").and_then(Value::as_str) == Some("model_change")
                && record.get("provider").and_then(Value::as_str) == Some("faux")
                && record.get("modelId").and_then(Value::as_str) == Some("faux-1")
        }),
        "create must record model_change: {records:?}"
    );
    let user_seen = records.iter().any(|record| {
        record.get("type").and_then(Value::as_str) == Some("message")
            && record
                .pointer("/message/role")
                .and_then(Value::as_str)
                .is_some_and(|role| role == "user")
            && record
                .pointer("/message/content")
                .map(|content| content.to_string().contains("remember this branch"))
                .unwrap_or(false)
    });
    assert!(user_seen, "user prompt must be on the JSONL branch: {records:?}");

    let path_arg = path.to_str().expect("utf8 path");
    let (resumed, resume_out, resume_err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            sessions_arg,
            "--resume",
            path_arg,
        ],
        Some("/quit\n"),
    );
    assert!(resumed, "resume failed: stdout={resume_out} stderr={resume_err}");
    assert!(
        resume_err.contains("resumed") && resume_err.contains(path_arg),
        "resume must open the created file: {resume_err}"
    );
    assert!(
        resume_out.contains("faux/faux-1"),
        "resume must restore the recorded faux model: {resume_out}"
    );
}

/// Contract: sessionDir precedence is CLI > env > project settings > global
/// settings across create, list, continue, resume-by-id, fork, and new-session
/// placement. Empty env is ignored so settings still win.
///
/// Plausible bug: env/CLI order inversion, fork writing under the source dir,
/// continue scanning the wrong root, or empty env overriding settings.
#[test]
fn session_dir_precedence_covers_create_resume_fork_and_new() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let global = agent.path().join("global-sessions");
    let project = agent.path().join("project-sessions");
    let env_root = agent.path().join("env-sessions");
    let cli_root = agent.path().join("cli-sessions");
    let fork_root = agent.path().join("fork-sessions");

    write_agent_home(
        agent.path(),
        &format!(
            r#"{{"sessionDir":{},"defaultProvider":"faux","defaultModel":"faux-1"}}"#,
            serde_json::to_string(&global).expect("global json")
        ),
        "",
    );
    fs::create_dir_all(cwd.path().join(".pi")).expect("project .pi");
    fs::write(
        cwd.path().join(".pi/settings.json"),
        format!(
            r#"{{"sessionDir":{}}}"#,
            serde_json::to_string(&project).expect("project json")
        ),
    )
    .expect("project settings");

    // Trusted project settings beat global when neither env nor CLI override.
    let (created, _, err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--approve",
            "--model",
            "faux/faux-1",
            "--session-id",
            "settings-owned",
            "--print",
            "from-settings",
        ],
        None,
    );
    assert!(created, "settings create failed: {err}");
    assert!(
        project.join("settings-owned.jsonl").is_file(),
        "project settings must own the session root; project={:?} global={:?} stderr={err}",
        list_jsonl(&project),
        list_jsonl(&global),
    );
    assert!(!global.join("settings-owned.jsonl").exists());

    // Env beats project settings.
    let mut env_cmd = Command::new(rpi_bin());
    offline_env(&mut env_cmd, agent.path(), cwd.path())
        .args([
            "--offline",
            "--approve",
            "--model",
            "faux/faux-1",
            "--session-id",
            "env-owned",
            "--print",
            "from-env",
        ])
        .arg("--cwd")
        .arg(cwd.path())
        .env("PI_CODING_AGENT_SESSION_DIR", &env_root)
        .env("PI_FAUX_RESPONSE", "compat-e2e-reply");
    let env_out = run_command_bounded(env_cmd, None);
    assert!(
        env_out.status.success(),
        "env create failed: {}",
        String::from_utf8_lossy(&env_out.stderr)
    );
    assert!(env_root.join("env-owned.jsonl").is_file());
    assert!(!project.join("env-owned.jsonl").exists());

    // CLI beats env.
    let cli_arg = cli_root.to_str().expect("cli root");
    let mut cli_cmd = Command::new(rpi_bin());
    offline_env(&mut cli_cmd, agent.path(), cwd.path())
        .args([
            "--offline",
            "--approve",
            "--model",
            "faux/faux-1",
            "--session-dir",
            cli_arg,
            "--session-id",
            "cli-owned",
            "--print",
            "from-cli",
        ])
        .arg("--cwd")
        .arg(cwd.path())
        .env("PI_CODING_AGENT_SESSION_DIR", &env_root)
        .env("PI_FAUX_RESPONSE", "compat-e2e-reply");
    let cli_out = run_command_bounded(cli_cmd, None);
    assert!(
        cli_out.status.success(),
        "cli create failed: {}",
        String::from_utf8_lossy(&cli_out.stderr)
    );
    assert!(cli_root.join("cli-owned.jsonl").is_file());
    assert!(!env_root.join("cli-owned.jsonl").exists());

    // Empty env must not override settings.
    let mut empty_env = Command::new(rpi_bin());
    offline_env(&mut empty_env, agent.path(), cwd.path())
        .args([
            "--offline",
            "--approve",
            "--model",
            "faux/faux-1",
            "--session-id",
            "empty-env-owned",
            "--print",
            "empty-env",
        ])
        .arg("--cwd")
        .arg(cwd.path())
        .env("PI_CODING_AGENT_SESSION_DIR", "")
        .env("PI_FAUX_RESPONSE", "compat-e2e-reply");
    let empty_out = run_command_bounded(empty_env, None);
    assert!(
        empty_out.status.success(),
        "empty env create failed: {}",
        String::from_utf8_lossy(&empty_out.stderr)
    );
    assert!(project.join("empty-env-owned.jsonl").is_file());

    // List + continue honor CLI session root.
    let (listed, list_out, list_err) = run_rpi(
        agent.path(),
        cwd.path(),
        &["--offline", "--session-dir", cli_arg, "sessions"],
        None,
    );
    assert!(listed, "list failed: {list_err}");
    assert!(
        list_out.contains("cli-owned"),
        "list must show CLI-root session: {list_out}"
    );
    assert!(
        !list_out.contains("settings-owned"),
        "list must not leak project-root sessions: {list_out}"
    );

    let (continued, _, cont_err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            cli_arg,
            "--continue",
            "--print",
            "continued",
        ],
        None,
    );
    assert!(continued, "continue failed: {cont_err}");
    assert!(
        cont_err.contains("cli-owned.jsonl"),
        "continue must select CLI-root latest: {cont_err}"
    );

    // Resume-by-id resolves against the selected root.
    let (resumed, _, resume_err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            cli_arg,
            "--resume",
            "cli-owned",
            "--print",
            "resumed-id",
        ],
        None,
    );
    assert!(resumed, "resume-by-id failed: {resume_err}");
    assert!(
        resume_err.contains("cli-owned.jsonl"),
        "resume-by-id must open CLI-root file: {resume_err}"
    );

    // Fork of an external path records under the selected sessionDir.
    let external = agent.path().join("external");
    let source = plant_v3_session(&external, cwd.path(), "fork-source", "fork me");
    let source_arg = source.to_str().expect("source");
    let fork_arg = fork_root.to_str().expect("fork root");
    let before_fork = list_jsonl(&fork_root);
    let (forked, _, fork_err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            fork_arg,
            "--fork",
            source_arg,
            "--print",
            "forked-turn",
        ],
        None,
    );
    assert!(forked, "fork failed: {fork_err}");
    let after_fork = list_jsonl(&fork_root);
    assert!(
        after_fork.len() > before_fork.len(),
        "fork must write under --session-dir: before={before_fork:?} after={after_fork:?}"
    );
    assert!(
        !list_jsonl(&external)
            .iter()
            .any(|path| path != &source && path.extension().is_some_and(|ext| ext == "jsonl")),
        "fork must not drop a sibling file next to the source"
    );

    // Interactive /new after a custom root still records under that root once a
    // turn is written (auto-id sessions flush on first message).
    let before_new = list_jsonl(&cli_root);
    let (new_ok, new_out, new_err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            cli_arg,
        ],
        Some("/new\nafter-new turn\n/quit\n"),
    );
    assert!(new_ok, "interactive /new failed: {new_err}");
    assert!(
        new_out.contains("started a new transcript"),
        "new session should acknowledge creation: out={new_out} err={new_err}"
    );
    let after_new = list_jsonl(&cli_root);
    assert!(
        after_new.len() > before_new.len(),
        "/new + turn must create under the effective sessionDir: before={before_new:?} after={after_new:?}"
    );
}

/// Contract: short `-r` is a binary-visible alias for unified resume and keeps
/// selector conflicts fail-closed at the process boundary.
///
/// Plausible bug: clap short alias wired only in unit parse tests, or `-r`
/// bypassing the unified catalog / conflict set.
#[test]
fn resume_short_alias_opens_session_and_rejects_selector_conflicts() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("short-r");
    write_agent_home(agent.path(), "{}", "");
    let path = plant_v3_session(&sessions, cwd.path(), "short-r-id", "hello from -r");
    let path_arg = path.to_str().expect("path");
    let sessions_arg = sessions.to_str().expect("sessions");

    let (ok, out, err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            sessions_arg,
            "-r",
            "short-r-id",
        ],
        Some("/quit\n"),
    );
    assert!(ok, "-r resume failed: stdout={out} stderr={err}");
    assert!(
        err.contains("resumed") && err.contains("short-r-id"),
        "-r must resume the planted session: {err}"
    );
    assert!(
        out.contains("faux/faux-1"),
        "-r must restore planted model: {out}"
    );

    let mut conflict_cmd = Command::new(rpi_bin());
    conflict_cmd
        .args(["-r", path_arg, "--continue"])
        .env("HOME", agent.path())
        .env("PI_CODING_AGENT_DIR", agent.path())
        .env("PI_OFFLINE", "1");
    let conflict = run_command_bounded(conflict_cmd, None);
    assert!(
        !conflict.status.success(),
        "-r must conflict with --continue at the binary boundary"
    );
}

/// Contract: `provider/model:max` is parsed off the model spec, applied as the
/// initial thinking level, and recorded on the Pi v3 branch when the model
/// opts into `max` via thinkingLevelMap.
///
/// Plausible bug: suffix left on the model id, `:max` dropped before session
/// start, or max clamped away despite an explicit map entry.
#[test]
fn model_max_suffix_sets_thinking_level_on_created_session() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("max-sessions");
    let models = r#"{
      "providers": {
        "reason": {
          "baseUrl": "http://localhost:0",
          "api": "faux",
          "apiKey": "offline-not-used",
          "models": [
            {
              "id": "thinker",
              "name": "Thinker",
              "reasoning": true,
              "contextWindow": 32000,
              "maxTokens": 2048,
              "thinkingLevelMap": {
                "off": "off",
                "minimal": "minimal",
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": "xhigh",
                "max": "max"
              }
            }
          ]
        }
      }
    }"#;
    write_agent_home(agent.path(), "{}", models);

    let sessions_arg = sessions.to_str().expect("sessions");
    let (ok, out, err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "reason/thinker:max",
            "--session-dir",
            sessions_arg,
            "--session-id",
            "max-level",
            "--print",
            "use max thinking",
        ],
        None,
    );
    assert!(ok, ":max startup failed: stdout={out} stderr={err}");
    assert!(
        !err.to_ascii_lowercase().contains("not found"),
        "model spec must resolve: {err}"
    );

    let path = sessions.join("max-level.jsonl");
    assert!(path.is_file(), "session file missing: {path:?}");
    let records = read_jsonl_records(&path);
    assert!(
        records.iter().any(|record| {
            record.get("type").and_then(Value::as_str) == Some("model_change")
                && record.get("provider").and_then(Value::as_str) == Some("reason")
                && record.get("modelId").and_then(Value::as_str) == Some("thinker")
        }),
        "model id must not retain the :max suffix: {records:?}"
    );
    assert!(
        records.iter().any(|record| {
            record.get("type").and_then(Value::as_str) == Some("thinking_level_change")
                && record.get("thinkingLevel").and_then(Value::as_str) == Some("max")
        }),
        "thinking_level_change must record max from the model suffix: {records:?}"
    );
}

/// Contract: print-mode prompt `@file` expansion embeds workspace text into the
/// recorded user turn (binary path, not only the library helper).
///
/// Plausible bug: print/json modes skip expand_prompt, or expansion happens
/// after the user message is recorded.
#[test]
fn at_file_expansion_embeds_workspace_text_in_print_mode_session() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("at-file-sessions");
    write_agent_home(agent.path(), "{}", "");
    fs::write(cwd.path().join("notes.txt"), "secret-note-body").expect("notes");

    let sessions_arg = sessions.to_str().expect("sessions");
    let (ok, out, err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--model",
            "faux/faux-1",
            "--session-dir",
            sessions_arg,
            "--session-id",
            "at-file",
            "--print",
            "please inspect @notes.txt carefully",
        ],
        None,
    );
    assert!(ok, "@file print failed: stdout={out} stderr={err}");
    assert!(
        out.contains("compat-e2e-reply"),
        "print must still complete the turn: {out}"
    );

    let path = sessions.join("at-file.jsonl");
    let records = read_jsonl_records(&path);
    let user_blob = records
        .iter()
        .filter(|record| record.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|record| {
            record
                .pointer("/message/role")
                .and_then(Value::as_str)
                == Some("user")
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        user_blob.contains("secret-note-body"),
        "expanded file body must appear in the recorded user turn: {user_blob}"
    );
    assert!(
        user_blob.contains("<file") && user_blob.contains("notes.txt"),
        "file wrapper/name must appear in the recorded user turn: {user_blob}"
    );
    assert!(
        !user_blob.contains("@notes.txt"),
        "raw @token should not remain after expansion: {user_blob}"
    );
}

/// Contract: `get_commands` projects each executable command with a wire
/// `source` from the closed set {builtin,prompt,skill,extension}, and primary
/// builtins are labeled `builtin`.
///
/// Plausible bug: source field omitted/renamed, or every command forced to one
/// label so hosts cannot distinguish builtins from package commands.
#[test]
fn get_commands_projects_builtin_source_on_rpc_wire() {
    let mut session = RpcSession::spawn_with(&[], "get-commands-reply");
    let deadline = Instant::now() + Duration::from_secs(20);
    session.write_line(r#"{"type":"get_commands","id":"cmds-1"}"#);
    let (_events, response) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("cmds-1")
    });
    assert_success(&response, "get_commands", "cmds-1");

    let commands = response["data"]["commands"]
        .as_array()
        .expect("commands array on data");
    assert!(
        !commands.is_empty(),
        "get_commands must return at least the primary builtins: {response}"
    );

    let mut saw_settings = false;
    let mut saw_model = false;
    for command in commands {
        let name = command
            .get("name")
            .and_then(Value::as_str)
            .expect("command name");
        let source = command
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("command {name} missing source: {command}"));
        assert!(
            matches!(source, "builtin" | "prompt" | "skill" | "extension"),
            "unexpected source {source:?} on {name}"
        );
        assert!(
            command.get("description").and_then(Value::as_str).is_some(),
            "command {name} missing description"
        );
        if name == "settings" {
            assert_eq!(source, "builtin");
            saw_settings = true;
        }
        if name == "model" {
            assert_eq!(source, "builtin");
            saw_model = true;
        }
    }
    assert!(saw_settings, "settings builtin missing: {commands:?}");
    assert!(saw_model, "model builtin missing: {commands:?}");

    let output = session.finish();
    assert!(
        output.status.success(),
        "status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: abort during an active prompt turn settles the agent without
/// poisoning the RPC connection; a follow-up prompt still completes.
///
/// Plausible bug: abort only works when idle, leaves is_streaming stuck, or
/// drops the event loop so later commands hang.
#[test]
fn rpc_abort_during_active_prompt_settles_and_allows_follow_up() {
    let mut session = RpcSession::spawn();
    let deadline = Instant::now() + Duration::from_secs(30);

    // Prompt is dispatched on a worker task; abort runs inline. Issue both
    // back-to-back so abort lands while the faux stream is still chunking the
    // long PI_FAUX_RESPONSE rather than only after the turn is idle.
    session.write_line(r#"{"type":"prompt","id":"prompt-abort","message":"stream a long answer"}"#);
    session.write_line(r#"{"type":"abort","id":"abort-live"}"#);

    let mut saw_abort = false;
    let mut settled = false;
    let mut saw_prompt_lifecycle = false;
    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(
            ty,
            "agent_start"
                | "turn_start"
                | "message_start"
                | "message_update"
                | "message_end"
                | "agent_end"
        ) {
            saw_prompt_lifecycle = true;
        }
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("abort-live") {
            assert_success(&value, "abort", "abort-live");
            saw_abort = true;
        }
        if ty == "agent_settled" {
            settled = true;
            if saw_abort {
                break;
            }
        }
        if settled && saw_abort {
            break;
        }
    }
    assert!(saw_abort, "abort response missing");
    assert!(settled, "active-turn abort must still emit agent_settled");
    assert!(
        saw_prompt_lifecycle,
        "expected some agent lifecycle traffic around the aborted prompt"
    );

    // Connection remains usable for a fresh prompt after abort settlement.
    session.write_line(r#"{"type":"prompt","id":"prompt-after","message":"after abort"}"#);
    let mut after_settled = false;
    let mut after_prompt_ok = false;
    let follow_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < follow_deadline {
        let value = session.read_json_deadline(follow_deadline);
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("prompt-after") {
            assert_success(&value, "prompt", "prompt-after");
            after_prompt_ok = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("agent_settled") {
            after_settled = true;
            break;
        }
    }
    assert!(after_prompt_ok, "follow-up prompt response missing");
    assert!(after_settled, "follow-up prompt must settle after abort");

    let output = session.finish();
    assert!(
        output.status.success(),
        "status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}
/// Contract: abort during a loop-owned active turn crosses the public RPC/JSON
/// boundary as an expected cancellation: the agent settles, no `loop_failed`
/// or `loop_finished` terminal loop event is emitted for the cancelled
/// iteration, stdout remains JSON/ANSI-free, and the loop task stays scheduled.
///
/// Plausible bug: `Application::abort` aborted the provider session before the
/// loop iteration token, so the provider's "Request was aborted" error won the
/// race and the scheduler emitted `loop_failed` for an expected user abort.
#[test]
fn rpc_abort_during_loop_iteration_settles_without_loop_failed() {
    let mut session = RpcSession::spawn();
    let deadline = Instant::now() + Duration::from_secs(30);
    session.write_line(
        r#"{"type":"loop_create","id":"loop-create-abort","interval":"30m","prompt":"rpc loop abort turn","fireImmediately":true,"durable":false}"#,
    );

    let mut task_id = None;
    let mut saw_fired = false;
    let mut saw_streaming = false;
    while task_id.is_none() || !saw_fired || !saw_streaming {
        let value = session.read_json_deadline(deadline);
        if is_response(&value)
            && value.get("id").and_then(Value::as_str) == Some("loop-create-abort")
        {
            assert_success(&value, "loop_create", "loop-create-abort");
            task_id = Some(
                value["data"]["id"]
                    .as_str()
                    .expect("loop_create returns task id")
                    .to_owned(),
            );
        }
        let ty = value.get("type").and_then(Value::as_str);
        if ty == Some("loop_fired") {
            saw_fired = true;
        }
        if ty == Some("message_update") {
            saw_streaming = true;
        }
    }
    let task_id = task_id.expect("loop task id");
    assert!(saw_fired, "loop iteration must fire");
    assert!(saw_streaming, "loop iteration must be actively streaming");

    session.write_line(r#"{"type":"abort","id":"abort-loop-live"}"#);
    let mut saw_abort = false;
    let mut saw_settled = false;
    let mut saw_loop_failed = false;
    let mut saw_loop_finished = false;
    while !saw_abort || !saw_settled {
        let value = session.read_json_deadline(deadline);
        if is_response(&value)
            && value.get("id").and_then(Value::as_str) == Some("abort-loop-live")
        {
            assert_success(&value, "abort", "abort-loop-live");
            saw_abort = true;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("agent_settled") => saw_settled = true,
            Some("loop_failed") => saw_loop_failed = true,
            Some("loop_finished") => saw_loop_finished = true,
            _ => {}
        }
    }
    assert!(saw_abort, "abort response missing");
    assert!(saw_settled, "loop iteration abort must emit agent_settled");
    assert!(!saw_loop_failed, "user abort must not emit loop_failed");
    assert!(!saw_loop_finished, "cancelled iteration must not emit loop_finished");

    // A public loop_list request proves the RPC connection is still usable and
    // the task remains scheduled. Inspect every intervening record for a late
    // terminal event from the cancelled iteration.
    session.write_line(r#"{"type":"loop_list","id":"loop-after-abort"}"#);
    let (intervening, listed) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("loop-after-abort")
    });
    assert_success(&listed, "loop_list", "loop-after-abort");
    assert!(
        !intervening.iter().any(|line| {
            matches!(
                line.get("type").and_then(Value::as_str),
                Some("loop_failed" | "loop_finished")
            )
        }),
        "cancelled iteration emitted a late terminal loop event: {intervening:?}"
    );
    let tasks = listed["data"].as_array().expect("loop_list array");
    assert!(
        tasks.iter().any(|task| task["id"].as_str() == Some(task_id.as_str())),
        "loop task must remain scheduled after iteration abort: {tasks:?}"
    );

    let output = session.finish();
    assert!(
        output.status.success(),
        "status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "structured RPC stdout must remain ANSI-free: {stdout:?}"
    );
    assert!(
        !stdout.contains("loop_failed"),
        "structured RPC stdout must not report loop_failed for user abort: {stdout:?}"
    );
}

/// Contract: abort_bash cancels an in-flight foreground bash and returns
/// cancelled=true on the bash response envelope without ANSI on stdout.
///
/// Plausible bug: abort_bash is a no-op while bash runs, or cancellation is not
/// reflected on the public response.
#[test]
fn rpc_abort_bash_cancels_in_flight_command() {
    let mut session = RpcSession::spawn_with(&[], "bash-abort-unused");
    let deadline = Instant::now() + Duration::from_secs(30);

    session.write_line(
        r#"{"type":"bash","id":"bash-slow","command":"sleep 8; printf done"}
"#,
    );
    // Give the child a moment to enter sleep, then cancel.
    std::thread::sleep(Duration::from_millis(200));
    session.write_line(r#"{"type":"abort_bash","id":"abort-bash-1"}"#);

    let mut saw_abort = false;
    let mut bash_response = None;
    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("abort-bash-1") {
            assert_success(&value, "abort_bash", "abort-bash-1");
            saw_abort = true;
        }
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("bash-slow") {
            bash_response = Some(value);
            break;
        }
    }
    assert!(saw_abort, "abort_bash response missing");
    let bash = bash_response.expect("bash response missing");
    assert_success(&bash, "bash", "bash-slow");
    assert_eq!(
        bash["data"].get("cancelled").and_then(Value::as_bool),
        Some(true),
        "in-flight bash must report cancelled=true: {bash}"
    );

    let output = session.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "ANSI leaked onto RPC stdout: {stdout:?}"
    );
    assert!(
        output.status.success(),
        "status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: switch_session advances the application runtime generation on the
/// public wire (`runtime_changed`) and subsequent get_state / get_messages
/// observe the replacement session only. new_session clears the live transcript
/// onto a distinct session file (in-place reset; no epoch bump required).
///
/// Plausible bug: switch swaps files without bumping epoch, emits no
/// runtime_changed, or leaves get_state/messages pinned to the previous runtime;
/// new_session keeps the prior transcript or reuses the old file path.
#[test]
fn rpc_switch_emits_runtime_changed_and_new_clears_transcript() {
    let mut session = RpcSession::spawn_with(&[], "runtime-gen-reply");
    let deadline = Instant::now() + Duration::from_secs(30);

    // Establish a first recorded turn so the source session is non-empty.
    session.write_line(r#"{"type":"prompt","id":"seed","message":"source-only marker"}"#);
    let mut seed_settled = false;
    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        if value.get("type").and_then(Value::as_str) == Some("agent_settled") {
            seed_settled = true;
            break;
        }
    }
    assert!(seed_settled, "seed prompt must settle");

    session.write_line(r#"{"type":"get_state","id":"state-source"}"#);
    let (_events, source_state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("state-source")
    });
    assert_success(&source_state, "get_state", "state-source");
    let source_file = source_state["data"]["sessionFile"]
        .as_str()
        .expect("source sessionFile")
        .to_owned();
    let source_count = source_state["data"]["messageCount"]
        .as_u64()
        .expect("source messageCount");
    assert!(source_count >= 1, "source session should have messages: {source_state}");

    // Plant a distinct target session under the same RPC sessionDir but with a
    // different recorded cwd so this exercises runtime-generation cutover.
    // Same-cwd switching intentionally replaces the Session in place and does
    // not emit RuntimeChanged.
    let target_cwd = TempDir::new().expect("runtime target cwd");
    let target = plant_v3_session(
        &session.session_dir,
        target_cwd.path(),
        "runtime-target",
        "target-only marker",
    );
    let target_arg = target.to_str().expect("target path");
    let switch_cmd = json!({
        "type": "switch_session",
        "id": "switch-1",
        "sessionPath": target_arg,
    });
    session.write_line(&switch_cmd.to_string());
    let switch_deadline = Instant::now() + Duration::from_secs(30);

    let mut saw_runtime_changed = false;
    let mut switch_response = None;
    let mut runtime_epoch = None;
    while Instant::now() < switch_deadline {
        let value = session.read_json_deadline(switch_deadline);
        if value.get("type").and_then(Value::as_str) == Some("runtime_changed") {
            saw_runtime_changed = true;
            runtime_epoch = value.get("epoch").and_then(Value::as_u64);
        }
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("switch-1") {
            switch_response = Some(value);
        }
        if switch_response.is_some() && saw_runtime_changed {
            break;
        }
    }
    if switch_response.is_some() && !saw_runtime_changed {
        let drain_until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < drain_until {
            let Some(value) = session.read_json_timeout(Duration::from_millis(200)) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) == Some("runtime_changed") {
                saw_runtime_changed = true;
                runtime_epoch = value.get("epoch").and_then(Value::as_u64);
                break;
            }
        }
    }
    let switch = switch_response.expect("switch_session response");
    assert_success(&switch, "switch_session", "switch-1");
    assert_eq!(
        switch["data"].get("cancelled").and_then(Value::as_bool),
        Some(false),
        "switch must not report cancelled: {switch}"
    );
    assert!(
        saw_runtime_changed,
        "switch_session must emit runtime_changed on the RPC wire"
    );
    assert!(
        runtime_epoch.is_some_and(|epoch| epoch >= 1),
        "runtime_changed epoch must be present: {runtime_epoch:?}"
    );

    session.write_line(r#"{"type":"get_state","id":"state-target"}"#);
    let (_events, target_state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("state-target")
    });
    assert_success(&target_state, "get_state", "state-target");
    let target_file = target_state["data"]["sessionFile"]
        .as_str()
        .expect("target sessionFile");
    assert_ne!(
        target_file, source_file,
        "get_state must point at the switched session file"
    );
    assert!(
        target_file.contains("runtime-target"),
        "switched sessionFile unexpected: {target_state}"
    );

    session.write_line(r#"{"type":"get_messages","id":"msgs-target"}"#);
    let (_events, messages) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("msgs-target")
    });
    assert_success(&messages, "get_messages", "msgs-target");
    let encoded = messages["data"]["messages"].to_string();
    assert!(
        encoded.contains("target-only marker"),
        "switched runtime messages must include target transcript: {messages}"
    );
    assert!(
        !encoded.contains("source-only marker"),
        "switched runtime must not retain source-only messages: {messages}"
    );

    // new_session clears the live transcript onto a fresh session file. Unlike
    // switch_session it is an in-place reset (no RuntimeChanged epoch bump).
    session.write_line(r#"{"type":"new_session","id":"new-1"}"#);
    let (_events, new_session) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("new-1")
    });
    assert_success(&new_session, "new_session", "new-1");
    assert_eq!(
        new_session["data"].get("cancelled").and_then(Value::as_bool),
        Some(false),
        "new_session cancelled flag: {new_session}"
    );

    session.write_line(r#"{"type":"get_state","id":"state-new"}"#);
    let (_events, fresh_state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("state-new")
    });
    assert_success(&fresh_state, "get_state", "state-new");
    assert_eq!(
        fresh_state["data"]["messageCount"].as_u64(),
        Some(0),
        "new_session must clear live messages: {fresh_state}"
    );
    let fresh_file = fresh_state["data"]["sessionFile"].as_str();
    assert!(
        fresh_file.is_some_and(|path| path != target_file && path != source_file),
        "new_session must open a distinct session file: {fresh_state}"
    );

    let output = session.finish();
    assert!(
        output.status.success(),
        "status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: startup diagnostics (deprecated settings keys) stay on stderr while
/// JSON-mode stdout remains pure LF-delimited objects with no ANSI / Warning:
/// prefixes — even when sessionDir resolution and model startup both run.
///
/// Plausible bug: settings/sessionDir warnings printed on stdout break JSON
/// consumers, or structured modes swallow diagnostics entirely.
#[test]
fn startup_warning_stays_on_stderr_and_json_stdout_stays_pure() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    let sessions = agent.path().join("warn-sessions");
    write_agent_home(
        agent.path(),
        &format!(
            r#"{{"sessionDir":{},"subagents":{{"agentOverrides":{{"reviewer":{{"enabled":false}}}}}}}}"#,
            serde_json::to_string(&sessions).expect("sessions json")
        ),
        "",
    );

    let (ok, out, err) = run_rpi(
        agent.path(),
        cwd.path(),
        &[
            "--offline",
            "--mode",
            "json",
            "--model",
            "faux/faux-1",
            "json purity check",
        ],
        None,
    );
    assert!(ok, "json mode failed: stderr={err}");
    assert!(
        !err.contains("deprecated subagents.agentOverrides"),
        "agentOverrides migration is silent: no stderr warning: {err}"
    );
    assert!(
        !out.contains("deprecated subagents.agentOverrides"),
        "startup warning must not pollute JSON stdout: {out}"
    );
    assert!(
        !out.contains('\u{1b}'),
        "JSON stdout must not contain ANSI: {out:?}"
    );

    let mut saw_object = false;
    for line in out.lines().filter(|line| !line.trim().is_empty()) {
        assert!(
            !line.starts_with("Warning:") && !line.starts_with("Error:"),
            "plain diagnostic on JSON stdout: {line}"
        );
        let value = serde_json::from_str::<Value>(line)
            .unwrap_or_else(|error| panic!("JSON stdout polluted ({error}): {line}"));
        assert!(value.is_object(), "JSON stdout line must be an object: {line}");
        saw_object = true;
    }
    assert!(saw_object, "JSON mode must emit at least one object on stdout");
    assert!(
        sessions
            .read_dir()
            .ok()
            .map(|entries| entries.filter_map(Result::ok).any(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "jsonl")
            }))
            .unwrap_or(false),
        "settings sessionDir must still control create while warnings fire"
    );
}
