//! Binary-level end-to-end coverage for the steering queue surface (D19/D31
//! batch): the user-facing contract that a steered prompt is QUEUED, visible
//! in the pending count, and then DELIVERED into the session transcript at the
//! next turn boundary — driven through the REAL `rpi rpc` subcommand over JSONL
//! stdin/stdout with the faux provider.
//!
//! The in-process suites already prove the queue mechanics at the
//! Application/Session/TUI-state levels (application_runtime.rs
//! `accepts_steering_during_an_active_run`, session.rs
//! `steering_consumption_publishes_queue_update_at_turn_boundaries`, tui.rs
//! `status_line_shows_queued_steering_above_activity`). What those suites
//! cannot see is the full wire path: RPC `steer` command → Application steer →
//! agent queue → turn-boundary consumption → recorded transcript. These tests
//! assert exactly that through the public binary.
//!
//! Deterministic by construction: the faux provider streams a fixed offline
//! response; a mid-stream steer lands because the prompt turn streams a large
//! response while the dispatcher keeps processing stdin. No network, no
//! credentials, no sleeps beyond bounded read deadlines.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Spawn the real `rpi` binary with the `rpc` subcommand first — the
/// successor of the removed `rpi-rpc` companion binary (≡ `--mode rpc`).
fn rpc_cmd() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
    command.arg("rpc");
    command
}

enum RpcLine {
    Line(String),
    Eof,
    Error(std::io::Error),
}

fn pump_stdout(mut stdout: ChildStdout, tx: &mpsc::Sender<RpcLine>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(RpcLine::Eof);
                return;
            }
            Ok(_) => {
                if tx.send(RpcLine::Line(std::mem::take(&mut line))).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(RpcLine::Error(error));
                return;
            }
        }
    }
}

struct RpcSession {
    child: Child,
    lines: mpsc::Receiver<RpcLine>,
    /// Keeps the temp HOME alive for the process lifetime.
    _home: tempfile::TempDir,
}

impl RpcSession {
    fn spawn_with_response(faux_response: &str) -> Self {
        let home = tempfile::tempdir().expect("temporary HOME");
        let mut child = rpc_cmd()
            .args(["--offline", "--model", "faux/faux-1"])
            .env("HOME", home.path())
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("PI_FAUX_RESPONSE", faux_response)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn rpi rpc");
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout pipe");
        std::thread::spawn(move || pump_stdout(stdout, &tx));
        Self {
            child,
            lines: rx,
            _home: home,
        }
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin pipe");
        stdin.write_all(line.as_bytes()).expect("write rpc stdin");
        if !line.ends_with('\n') {
            stdin.write_all(b"\n").expect("write rpc LF");
        }
        stdin.flush().expect("flush rpc stdin");
    }

    fn read_json_deadline(&mut self, deadline: Instant) -> Value {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                panic!("timed out waiting for next JSONL record from rpi rpc");
            }
            match self.lines.recv_timeout(remaining) {
                Ok(RpcLine::Line(line)) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    assert!(
                        !trimmed.contains('\u{1b}'),
                        "stdout must not contain ANSI escapes: {trimmed:?}"
                    );
                    let value = serde_json::from_str::<Value>(trimmed).unwrap_or_else(|error| {
                        panic!("stdout line is not JSON ({error}): {trimmed}")
                    });
                    assert!(
                        value.is_object(),
                        "stdout line must be a JSON object: {value}"
                    );
                    return value;
                }
                Ok(RpcLine::Eof) => {
                    let status = self.child.try_wait().ok().flatten();
                    panic!(
                        "rpi rpc stdout closed before next JSONL record (status={status:?})"
                    );
                }
                Ok(RpcLine::Error(error)) => panic!("reading rpi rpc stdout: {error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("rpi rpc stdout reader thread stopped")
                }
            }
        }
    }

    /// Read until `pred` matches; returns every earlier record plus the match.
    fn read_until(
        &mut self,
        deadline: Instant,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> (Vec<Value>, Value) {
        let mut seen = Vec::new();
        loop {
            let value = self.read_json_deadline(deadline);
            if pred(&value) {
                return (seen, value);
            }
            seen.push(value);
        }
    }

    fn finish(mut self) {
        drop(self.child.stdin.take());
        let drain_deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.child.kill();
                panic!("timed out draining rpi rpc stdout");
            }
            match self.lines.recv_timeout(remaining) {
                Ok(RpcLine::Line(_)) => {}
                Ok(RpcLine::Eof) | Ok(RpcLine::Error(_)) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut stderr = Vec::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_end(&mut stderr);
        }
        let status = self.child.wait().expect("wait rpi rpc");
        assert!(
            status.success(),
            "status: {:?}\nstderr: {}",
            status.code(),
            String::from_utf8_lossy(&stderr)
        );
    }
}

fn is_response(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("response")
}

fn assert_success(value: &Value, command: &str, id: &str) {
    assert_eq!(
        value.get("type").and_then(Value::as_str),
        Some("response"),
        "{command} must be a response: {value}"
    );
    assert_eq!(
        value.get("id").and_then(Value::as_str),
        Some(id),
        "{command} response id: {value}"
    );
    assert_eq!(
        value.get("command").and_then(Value::as_str),
        Some(command),
        "{command} response command: {value}"
    );
    assert_eq!(
        value.get("success").and_then(Value::as_bool),
        Some(true),
        "{command} must succeed: {value}"
    );
}

/// Collect the recorded user texts from a `get_messages` response. The RPC
/// exposes the recorded `Message` values; user entries carry their submitted
/// text in text content blocks, so the steered prompt and the assignment both
/// surface here.
fn user_texts(messages: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(list) = messages["messages"].as_array() {
        for message in list {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(blocks) = message["content"].as_array() {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            out.push(text.to_owned());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Contract: a `steer` RPC while idle is queued (visible in the pending
/// message count), and the next `prompt` run consumes it at the first turn
/// boundary — both the assignment and the steered prompt end up recorded in
/// the session transcript and the queue drains to zero.
#[test]
fn idle_steer_is_queued_then_delivered_by_next_prompt() {
    let mut session = RpcSession::spawn_with_response("rpc-steering-faux-reply");

    // 1) Steer while idle: accepted and queued.
    session.write_line(r#"{"type":"steer","id":"steer-1","message":"course correction alpha"}"#);
    let deadline = Instant::now() + Duration::from_secs(20);
    let (_seen, steer_resp) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("steer-1")
    });
    assert_success(&steer_resp, "steer", "steer-1");

    // 2) The queue is user-visible: pendingMessageCount reports it.
    session.write_line(r#"{"type":"get_state","id":"state-1"}"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (_seen, state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("state-1")
    });
    assert_success(&state, "get_state", "state-1");
    assert_eq!(
        state["data"]["pendingMessageCount"].as_u64(),
        Some(1),
        "the queued steering must be visible in the pending count: {state}"
    );

    // 3) The next prompt consumes the queued steering before settling.
    session.write_line(r#"{"type":"prompt","id":"prompt-1","message":"main assignment alpha"}"#);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_prompt_response = false;
    let mut settled = None;
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("prompt-1") {
            assert_success(&value, "prompt", "prompt-1");
            saw_prompt_response = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("agent_settled") {
            settled = Some(value);
            break;
        }
        collected.push(value);
    }
    let settled = settled.expect("prompt turn must emit agent_settled");
    assert_eq!(settled, json!({"type": "agent_settled"}));
    assert!(saw_prompt_response, "prompt response missing: {collected:?}");

    // 4) The queue drained and both user prompts are recorded.
    session.write_line(r#"{"type":"get_state","id":"state-2"}"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (_seen, state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("state-2")
    });
    assert_success(&state, "get_state", "state-2");
    assert_eq!(
        state["data"]["pendingMessageCount"].as_u64(),
        Some(0),
        "the queue must drain once the run consumes the steering: {state}"
    );
    assert!(
        state["data"]["messageCount"].as_u64().unwrap_or(0) >= 2,
        "both user prompts must be recorded: {state}"
    );

    session.write_line(r#"{"type":"get_messages","id":"messages-1"}"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (_seen, messages) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("messages-1")
    });
    assert_success(&messages, "get_messages", "messages-1");
    let texts = user_texts(&messages["data"]);
    assert!(
        texts.iter().any(|text| text.contains("course correction alpha")),
        "steered prompt must be recorded in the transcript: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("main assignment alpha")),
        "assignment must be recorded in the transcript: {texts:?}"
    );

    session.finish();
}

/// Contract: a `steer` RPC that lands while a prompt is STREAMING is still
/// delivered — the run does not settle until the steered prompt has been
/// consumed at the next turn boundary and the queue is empty.
#[test]
fn mid_stream_steer_is_delivered_before_settle() {
    // A large offline response keeps the first turn streaming while the steer
    // command (written immediately after prompt) is processed by the
    // concurrent dispatcher — deterministic mid-turn delivery.
    let big_reply = format!("streamed reply {}", "x".repeat(100_000));
    let mut session = RpcSession::spawn_with_response(&big_reply);

    session.write_line(r#"{"type":"prompt","id":"prompt-mid","message":"long assignment beta"}"#);
    session.write_line(r#"{"type":"steer","id":"steer-mid","message":"mid-turn correction beta"}"#);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut saw_prompt_response = false;
    let mut saw_steer_response = false;
    let mut saw_stream_event = false;
    let mut settled = None;
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        if is_response(&value) {
            match value.get("id").and_then(Value::as_str) {
                Some("prompt-mid") => {
                    assert_success(&value, "prompt", "prompt-mid");
                    saw_prompt_response = true;
                }
                Some("steer-mid") => {
                    assert_success(&value, "steer", "steer-mid");
                    saw_steer_response = true;
                }
                _ => {}
            }
        }
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("message_start" | "message_delta" | "agent_start" | "turn_start")
        ) {
            saw_stream_event = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("agent_settled") {
            settled = Some(value);
            break;
        }
        collected.push(value);
    }
    let settled = settled.expect("the run must settle with the steered turn consumed");
    assert_eq!(settled, json!({"type": "agent_settled"}));
    assert!(saw_prompt_response, "prompt response missing: {collected:?}");
    assert!(saw_steer_response, "steer response missing: {collected:?}");
    assert!(saw_stream_event, "expected streaming events before settle: {collected:?}");

    // The queue drained and the steered prompt reached the transcript.
    session.write_line(r#"{"type":"get_state","id":"state-mid"}"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (_seen, state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("state-mid")
    });
    assert_success(&state, "get_state", "state-mid");
    assert_eq!(
        state["data"]["pendingMessageCount"].as_u64(),
        Some(0),
        "the mid-stream steering must be consumed before settle: {state}"
    );
    assert!(
        state["data"]["messageCount"].as_u64().unwrap_or(0) >= 2,
        "the steered prompt must be recorded: {state}"
    );

    session.write_line(r#"{"type":"get_messages","id":"messages-mid"}"#);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (_seen, messages) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("messages-mid")
    });
    assert_success(&messages, "get_messages", "messages-mid");
    let texts = user_texts(&messages["data"]);
    assert!(
        texts.iter().any(|text| text.contains("mid-turn correction beta")),
        "mid-stream steered prompt must be recorded: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("long assignment beta")),
        "assignment must be recorded: {texts:?}"
    );

    session.finish();
}
