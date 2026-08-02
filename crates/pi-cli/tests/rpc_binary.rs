//! End-to-end wire contracts for the `rpi-rpc` binary.
//!
//! These tests drive the real binary over stdin/stdout with a temporary HOME
//! and the built-in faux model. Every assertion checks JSON objects on the
//! LF-delimited stdout stream (response envelopes, recovery sequence, and
//! agent lifecycle events) — never source text or implementation details.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn rpc_bin() -> String {
    env!("CARGO_BIN_EXE_rpi-rpc").to_owned()
}

struct RpcSession {
    child: Child,
    stdout: BufReader<ChildStdout>,
    /// Keeps the temp HOME alive for the process lifetime.
    _home: tempfile::TempDir,
}

impl RpcSession {
    fn spawn() -> Self {
        let home = tempfile::tempdir().expect("temporary HOME");
        let mut child = Command::new(rpc_bin())
            .args(["--offline", "--model", "faux/faux-1"])
            .env("HOME", home.path())
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            // Deterministic offline assistant text for prompt-driven contracts.
            .env("PI_FAUX_RESPONSE", "rpc-binary-faux-reply")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn rpi-rpc");
        let stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
        Self {
            child,
            stdout,
            _home: home,
        }
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin pipe");
        stdin
            .write_all(line.as_bytes())
            .expect("write rpc stdin");
        if !line.ends_with('\n') {
            stdin.write_all(b"\n").expect("write rpc LF");
        }
        stdin.flush().expect("flush rpc stdin");
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.child.stdin.as_mut().expect("stdin pipe");
        stdin.write_all(bytes).expect("write rpc stdin bytes");
        stdin.flush().expect("flush rpc stdin bytes");
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Read the next non-empty JSONL record, failing if the deadline elapses.
    fn read_json_deadline(&mut self, deadline: Instant) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            // BufReader::read_line blocks; poll child exit if we already closed stdin.
            // Use a short timed approach via try_wait + blocking read is unavoidable
            // without threads — spawn a helper thread only when needed by callers that
            // already closed stdin. Here we rely on the server always answering.
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("timed out waiting for next JSONL record from rpi-rpc");
            }
            match self.stdout.read_line(&mut line) {
                Ok(0) => {
                    // EOF — collect status for diagnostics.
                    let status = self.child.try_wait().ok().flatten();
                    panic!(
                        "rpi-rpc stdout closed before next JSONL record (status={status:?})"
                    );
                }
                Ok(_) => {
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
                Err(error) => panic!("reading rpi-rpc stdout: {error}"),
            }
        }
    }

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

    fn finish(mut self) -> Output {
        self.close_stdin();
        // Drain remaining stdout so the child is not blocked on a full pipe.
        let mut stdout = Vec::new();
        let _ = self.stdout.read_to_end(&mut stdout);
        let mut stderr = Vec::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_end(&mut stderr);
        }
        let status = self.child.wait().expect("wait rpi-rpc");
        Output {
            status,
            stdout,
            stderr,
        }
    }
}

/// Feed all of `stdin` then wait for process exit (bounded).
fn run_rpc(stdin: &[u8]) -> (Vec<Value>, Output) {
    let mut session = RpcSession::spawn();
    session.write_bytes(stdin);
    session.close_stdin();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut lines = Vec::new();
    let mut raw = Vec::new();
    loop {
        if Instant::now() > deadline {
            let _ = session.child.kill();
            panic!(
                "rpi-rpc exceeded deadline; partial lines={lines:?} stderr={}",
                // best-effort
                String::from_utf8_lossy(
                    &session
                        .child
                        .stderr
                        .as_mut()
                        .map(|s| {
                            let mut buf = Vec::new();
                            let _ = s.read_to_end(&mut buf);
                            buf
                        })
                        .unwrap_or_default()
                )
            );
        }
        let mut line = String::new();
        match session.stdout.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                raw.extend_from_slice(line.as_bytes());
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                assert!(
                    !trimmed.contains('\u{1b}'),
                    "stdout must not contain ANSI escapes: {trimmed:?}"
                );
                assert!(
                    !trimmed.to_ascii_lowercase().starts_with("warning:"),
                    "stdout must not contain plain-text warnings: {trimmed:?}"
                );
                let value = serde_json::from_str::<Value>(trimmed).unwrap_or_else(|error| {
                    panic!("stdout line is not JSON ({error}): {trimmed}")
                });
                assert!(
                    value.is_object(),
                    "stdout line must be a JSON object: {value}"
                );
                lines.push(value);
            }
            Err(error) => panic!("reading rpi-rpc stdout: {error}"),
        }
    }

    let mut stderr = Vec::new();
    if let Some(mut err) = session.child.stderr.take() {
        let _ = err.read_to_end(&mut stderr);
    }
    let status = session.child.wait().expect("wait rpi-rpc");
    // Keep home alive until wait returns.
    drop(session._home);
    (
        lines,
        Output {
            status,
            stdout: raw,
            stderr,
        },
    )
}

fn is_response(line: &Value) -> bool {
    line.get("type").and_then(Value::as_str) == Some("response")
}

fn find_response<'a>(lines: &'a [Value], id: &str) -> &'a Value {
    lines
        .iter()
        .find(|line| is_response(line) && line.get("id").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("missing response id={id} in {lines:?}"))
}

fn assert_failure(line: &Value, command: &str, id: Option<&str>, error_substr: &str) {
    assert_eq!(line["type"], "response", "failure envelope type: {line}");
    assert_eq!(
        line["success"], false,
        "failure must set success=false: {line}"
    );
    assert_eq!(line["command"], command, "failure command field: {line}");
    match id {
        Some(expected) => assert_eq!(
            line.get("id").and_then(Value::as_str),
            Some(expected),
            "failure id: {line}"
        ),
        None => assert!(
            line.get("id").is_none() || line.get("id") == Some(&Value::Null),
            "parse failures without a recoverable id must omit id: {line}"
        ),
    }
    let error = line
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("failure must carry error string: {line}"));
    assert!(
        error.contains(error_substr),
        "error {error:?} missing {error_substr:?} in {line}"
    );
    assert!(
        line.get("data").is_none() || line.get("data") == Some(&Value::Null),
        "failure responses must not carry data: {line}"
    );
}

fn assert_success(line: &Value, command: &str, id: &str) {
    assert_eq!(line["type"], "response", "success envelope type: {line}");
    assert_eq!(line["success"], true, "success flag: {line}");
    assert_eq!(line["command"], command, "success command: {line}");
    assert_eq!(line["id"], id, "success id: {line}");
    assert!(
        line.get("error").is_none() || line.get("error") == Some(&Value::Null),
        "success responses must not carry error: {line}"
    );
}

#[test]
fn help_and_version_use_standalone_name() {
    let help = Command::new(rpc_bin())
        .arg("--help")
        .output()
        .expect("rpi-rpc --help");
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout)
            .contains("rpi-rpc - rpi headless RPC server")
    );

    let version = Command::new(rpc_bin())
        .arg("--version")
        .output()
        .expect("rpi-rpc --version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
}

/// Contract: every stdout record is one JSON object terminated by LF; CR is
/// stripped so CRLF frames parse the same as LF frames.
#[test]
fn lf_delimited_framing_accepts_crlf_and_emits_json_objects() {
    // Use CRLF for the first frame and bare LF for the second. Commands run
    // concurrently, so response *order* is not part of the framing contract —
    // both must succeed as distinct correlated JSON objects on an LF stream.
    let input =
        b"{\"type\":\"get_state\",\"id\":\"crlf-1\"}\r\n{\"type\":\"get_state\",\"id\":\"lf-2\"}\n";
    let (lines, output) = run_rpc(input);
    assert!(
        output.status.success(),
        "status {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let raw = String::from_utf8_lossy(&output.stdout);
    assert!(
        raw.ends_with('\n'),
        "protocol writer must terminate the final record with LF: {raw:?}"
    );
    assert!(
        !raw.contains('\r'),
        "host stdout must not emit CR: {raw:?}"
    );
    assert!(
        !raw.contains('\u{1b}'),
        "stdout must not contain ANSI escapes: {raw:?}"
    );

    assert!(
        !lines.is_empty(),
        "expected at least one JSONL record on stdout"
    );
    for line in &lines {
        assert!(line.get("type").is_some(), "every record needs type: {line}");
    }

    let first = find_response(&lines, "crlf-1");
    assert_success(first, "get_state", "crlf-1");
    assert!(
        first["data"].is_object(),
        "get_state returns state object: {first}"
    );
    let second = find_response(&lines, "lf-2");
    assert_success(second, "get_state", "lf-2");
    assert!(
        second["data"].is_object(),
        "get_state returns state object: {second}"
    );
}

/// Contract: a bad JSON line emits a parse failure without id, then the next
/// LF-delimited valid command still succeeds — framing recovery is per-line.
#[test]
fn malformed_then_valid_frames_keep_stdout_jsonl() {
    let (lines, output) =
        run_rpc(b"{bad json}\n{\"type\":\"get_state\",\"id\":\"state-1\"}\n");
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let parse_fail = lines
        .iter()
        .find(|line| is_response(line) && line["command"] == "parse" && line["success"] == false)
        .expect("malformed frame must emit parse failure");
    assert_failure(parse_fail, "parse", None, "Failed to parse command");

    let ok = find_response(&lines, "state-1");
    assert_success(ok, "get_state", "state-1");
    assert!(
        ok.get("data").is_some_and(|data| data.is_object()),
        "get_state success must return state object: {ok}"
    );
    assert!(
        ok["data"].get("model").is_some(),
        "get_state data exposes model: {ok}"
    );

    let fail_idx = lines
        .iter()
        .position(|line| {
            is_response(line) && line["command"] == "parse" && line["success"] == false
        })
        .expect("parse fail index");
    let ok_idx = lines
        .iter()
        .position(|line| line.get("id").and_then(Value::as_str) == Some("state-1"))
        .expect("state-1 index");
    assert!(
        fail_idx < ok_idx,
        "recovery sequence requires parse failure before the recovered success: {lines:?}"
    );
}

/// Contract: unknown `type` and missing `type` produce structured failures that
/// preserve a request id when the JSON object itself is well-formed, and a
/// subsequent valid command still runs.
#[test]
fn unknown_and_missing_command_type_errors_preserve_id_then_recover() {
    let input = concat!(
        r#"{"type":"not_a_real_command","id":"unk-1"}"#,
        "\n",
        r#"{"id":"missing-type-1","message":"no type field"}"#,
        "\n",
        r#"{"type":"get_state","id":"after-errors"}"#,
        "\n",
    );
    let (lines, output) = run_rpc(input.as_bytes());
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let unknown = find_response(&lines, "unk-1");
    assert_failure(
        unknown,
        "not_a_real_command",
        Some("unk-1"),
        "Unknown command",
    );
    assert!(
        unknown["error"]
            .as_str()
            .is_some_and(|error| error.contains("not_a_real_command")),
        "unknown-command error must name the type: {unknown}"
    );

    let missing = find_response(&lines, "missing-type-1");
    assert_failure(
        missing,
        "parse",
        Some("missing-type-1"),
        "missing string field `type`",
    );

    let recovered = find_response(&lines, "after-errors");
    assert_success(recovered, "get_state", "after-errors");

    let unk_idx = lines
        .iter()
        .position(|line| line.get("id").and_then(Value::as_str) == Some("unk-1"))
        .expect("unk-1");
    let miss_idx = lines
        .iter()
        .position(|line| line.get("id").and_then(Value::as_str) == Some("missing-type-1"))
        .expect("missing-type-1");
    let ok_idx = lines
        .iter()
        .position(|line| line.get("id").and_then(Value::as_str) == Some("after-errors"))
        .expect("after-errors");
    assert!(
        unk_idx < miss_idx && miss_idx < ok_idx,
        "error then error then recovery order broken: {lines:?}"
    );
}

/// Contract: invalid fields on a known command fail with the command name and
/// id preserved, without poisoning later frames.
#[test]
fn invalid_known_command_fields_fail_with_command_name() {
    let input = concat!(
        // Missing required modelId on a known command.
        r#"{"type":"set_model","id":"bad-fields","provider":"faux"}"#,
        "\n",
        // Wrong JSON type for a required string field.
        r#"{"type":"bash","id":"bad-type","command":123}"#,
        "\n",
        r#"{"type":"get_state","id":"after-invalid"}"#,
        "\n",
    );
    let (lines, output) = run_rpc(input.as_bytes());
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let missing_field = find_response(&lines, "bad-fields");
    assert_failure(
        missing_field,
        "set_model",
        Some("bad-fields"),
        "Invalid command",
    );
    assert!(
        missing_field["error"]
            .as_str()
            .is_some_and(|error| error.contains("modelId")),
        "missing-field error should name modelId: {missing_field}"
    );

    let bad_type = find_response(&lines, "bad-type");
    assert_failure(bad_type, "bash", Some("bad-type"), "Invalid command");

    let ok = find_response(&lines, "after-invalid");
    assert_success(ok, "get_state", "after-invalid");
}

/// Contract: unterminated final stdin bytes (no trailing LF) surface as a parse
/// failure and do not hang the process.
#[test]
fn unterminated_final_frame_emits_parse_failure() {
    let (lines, output) = run_rpc(br#"{"type":"get_state","id":"no-lf"}"#);
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let parse_fail = lines
        .iter()
        .find(|line| is_response(line) && line["command"] == "parse" && line["success"] == false)
        .expect("unterminated frame must emit parse failure");
    assert_failure(parse_fail, "parse", None, "RPC frame must end with LF");

    // The partial object must not be executed as a successful get_state.
    assert!(
        lines.iter().all(|line| {
            !(is_response(line)
                && line.get("id").and_then(Value::as_str) == Some("no-lf")
                && line["success"] == true)
        }),
        "unterminated frame must not produce a successful get_state: {lines:?}"
    );
}

/// Contract: prompt over faux produces a success response, streams agent
/// events, and eventually emits `agent_settled` on the same JSONL stdout.
///
/// Stdin stays open until settlement so the RPC event loop is not torn down
/// mid-turn by EOF.
#[test]
fn prompt_emits_response_events_and_agent_settled() {
    let mut session = RpcSession::spawn();
    session.write_line(r#"{"type":"prompt","id":"prompt-1","message":"ping from rpc binary test"}"#);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut lines = Vec::new();
    let mut saw_prompt_response = false;
    let mut settled = None;

    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        let is_settled = value.get("type").and_then(Value::as_str) == Some("agent_settled");
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("prompt-1") {
            assert_success(&value, "prompt", "prompt-1");
            assert!(
                value.get("data").is_none() || value.get("data") == Some(&Value::Null),
                "prompt success data must be null/omitted: {value}"
            );
            saw_prompt_response = true;
        }
        if is_settled {
            settled = Some(value);
            break;
        }
        lines.push(value);
    }

    let settled = settled.expect("prompt turn must emit agent_settled");
    assert_eq!(
        settled,
        json!({"type": "agent_settled"}),
        "agent_settled must be the exact wire object"
    );
    assert!(
        saw_prompt_response,
        "prompt response must arrive before or with settlement stream: {lines:?}"
    );

    // At least one agent lifecycle event should appear around the turn.
    let saw_agent_lifecycle = lines.iter().any(|line| {
        matches!(
            line.get("type").and_then(Value::as_str),
            Some("agent_start" | "turn_start" | "message_start" | "message_end" | "agent_end")
        )
    });
    assert!(
        saw_agent_lifecycle,
        "expected agent lifecycle events on the wire before settle: {lines:?}"
    );

    // After settlement, a follow-up get_state must see the recorded exchange.
    session.write_line(r#"{"type":"get_state","id":"after-prompt"}"#);
    let state_deadline = Instant::now() + Duration::from_secs(10);
    let (_ignored, after) = session.read_until(state_deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("after-prompt")
    });
    assert_success(&after, "get_state", "after-prompt");
    let message_count = after["data"]["messageCount"]
        .as_u64()
        .expect("messageCount present after prompt");
    assert!(
        message_count >= 1,
        "prompt must leave messages in session state: {after}"
    );

    let output = session.finish();
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: stdout is exclusively JSONL protocol objects — no ANSI color and
/// no plain diagnostic text mixed into the stream, including after a recovery
/// path that also exercises get_state.
#[test]
fn structured_stdout_is_jsonl_without_ansi_or_plain_warnings() {
    let input = concat!(
        "{not-json}\n",
        r#"{"type":"unknown_cmd","id":"u"}"#,
        "\n",
        r#"{"type":"get_state","id":"clean"}"#,
        "\n",
    );
    let (lines, output) = run_rpc(input.as_bytes());
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "ANSI CSI must not appear on RPC stdout: {stdout:?}"
    );
    for needle in ["Warning:", "warning:", "Error:"] {
        for line in stdout.lines() {
            if line.starts_with(needle) {
                panic!("plain diagnostic line on stdout: {line}");
            }
        }
    }

    assert!(
        lines.iter().all(|line| line.get("type").is_some()),
        "every stdout object must carry type: {lines:?}"
    );
    assert_success(find_response(&lines, "clean"), "get_state", "clean");
    // stderr may carry diagnostics; that is allowed and must not leak to stdout.
    let _ = output.stderr;
}

/// Contract: multiple independent commands on one connection correlate by id
/// and never share response envelopes. Response arrival order is not specified
/// because non-abort commands run concurrently.
#[test]
fn concurrent_ids_correlate_independently() {
    // Stick to cheap local commands so auth/network filters cannot stall the
    // binary under parallel test load.
    let input = concat!(
        r#"{"type":"get_state","id":"a"}"#,
        "\n",
        r#"{"type":"get_commands","id":"b"}"#,
        "\n",
        r#"{"type":"get_session_stats","id":"c"}"#,
        "\n",
    );
    let (lines, output) = run_rpc(input.as_bytes());
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let a = find_response(&lines, "a");
    let b = find_response(&lines, "b");
    let c = find_response(&lines, "c");
    assert_success(a, "get_state", "a");
    assert_success(b, "get_commands", "b");
    assert_success(c, "get_session_stats", "c");
    let command_names = b["data"]["commands"]
        .as_array()
        .expect("get_commands returns an array")
        .iter()
        .map(|command| command["name"].as_str().expect("command name"))
        .collect::<Vec<_>>();
    assert_eq!(
        command_names,
        [
            "settings", "model", "branch", "resume", "fork", "export", "agents",
            "compact", "ps", "loop", "goal", "workflow",
        ],
        "RPC command discovery must match TUI and REPL primary slash surface"
    );

    assert!(a["data"].is_object(), "get_state data: {a}");
    assert!(
        b["data"].is_object() || b["data"].is_array(),
        "get_commands data: {b}"
    );
    assert!(c["data"].is_object(), "get_session_stats data: {c}");

    for (id, command) in [
        ("a", "get_state"),
        ("b", "get_commands"),
        ("c", "get_session_stats"),
    ] {
        let matches = lines
            .iter()
            .filter(|line| {
                is_response(line)
                    && line.get("id").and_then(Value::as_str) == Some(id)
                    && line["command"] == command
            })
            .count();
        assert_eq!(matches, 1, "exactly one {command} response for id={id}");
    }
}

/// Contract: malformed JSON, unknown type, and a valid command on one stream
/// preserve the recovery sequence (parse failure → unknown failure → success)
/// with strict JSON objects and no ANSI on stdout.
#[test]
fn recovery_sequence_malformed_unknown_then_valid() {
    let input = concat!(
        "{bad}\n",
        r#"{"type":"totally_unknown","id":"u2"}"#,
        "\n",
        r#"{"type":"get_state","id":"ok2"}"#,
        "\n",
    );
    let (lines, output) = run_rpc(input.as_bytes());
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let parse_idx = lines
        .iter()
        .position(|line| {
            is_response(line) && line["command"] == "parse" && line["success"] == false
        })
        .expect("parse failure");
    let unknown_idx = lines
        .iter()
        .position(|line| line.get("id").and_then(Value::as_str) == Some("u2"))
        .expect("unknown failure");
    let ok_idx = lines
        .iter()
        .position(|line| line.get("id").and_then(Value::as_str) == Some("ok2"))
        .expect("recovered success");

    assert_failure(&lines[parse_idx], "parse", None, "Failed to parse command");
    assert_failure(
        &lines[unknown_idx],
        "totally_unknown",
        Some("u2"),
        "Unknown command",
    );
    assert_success(&lines[ok_idx], "get_state", "ok2");
    assert!(
        parse_idx < unknown_idx && unknown_idx < ok_idx,
        "recovery sequence broken: {lines:?}"
    );

    let raw = String::from_utf8_lossy(&output.stdout);
    assert!(!raw.contains('\u{1b}'), "no ANSI on recovery path: {raw:?}");
}

/// Contract: loop CRUD commands use the same application lifecycle over RPC,
/// loop events are projected as wire objects, and a malformed loop command does
/// not poison the following valid request.
#[test]
fn loop_crud_events_and_malformed_recovery_share_one_rpc_connection() {
    let mut session = RpcSession::spawn();
    session.write_line(
        r#"{"type":"loop_create","id":"loop-create","interval":"3s","prompt":"rpc scheduled","fireImmediately":true,"durable":false}"#,
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut created = None;
    let mut created_event = None;
    while created.is_none() || created_event.is_none() {
        let line = session.read_json_deadline(deadline);
        if is_response(&line)
            && line.get("id").and_then(Value::as_str) == Some("loop-create")
        {
            created = Some(line);
            continue;
        }
        if line.get("type").and_then(Value::as_str) == Some("loop_created") {
            created_event = Some(line);
        }
    }
    let created = created.expect("loop create response");
    assert_success(&created, "loop_create", "loop-create");
    assert_eq!(created["data"]["intervalSecs"], 3);
    let task_id = created["data"]["id"]
        .as_str()
        .expect("loop_create returns task id")
        .to_owned();
    let created_event = created_event.expect("loop_created event");
    assert_eq!(created_event["task"]["id"], task_id);
    let mut loop_turn_events = vec![created_event];
    if !loop_turn_events.iter().any(|line| {
        matches!(line.get("type").and_then(Value::as_str), Some("loop_finished" | "loop_failed"))
    }) {
        let (events, terminal) = session.read_until(deadline, |line| {
            matches!(line.get("type").and_then(Value::as_str), Some("loop_finished" | "loop_failed"))
        });
        loop_turn_events.extend(events);
        loop_turn_events.push(terminal);
    }
    let scheduled_message = loop_turn_events.iter().find(|line| {
        line.get("type").and_then(Value::as_str) == Some("message_end")
            && line["message"]["role"] == "custom"
    }).expect("public loop message event");
    let serialized = scheduled_message.to_string();
    assert!(serialized.contains("Loop "));
    assert!(serialized.contains("every 3 seconds"));
    assert!(serialized.contains("rpc scheduled"));
    assert!(!serialized.contains("system-reminder"));

    session.write_line(r#"{"type":"get_messages","id":"loop-messages"}"#);
    let (_events, public_messages) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("loop-messages")
    });
    assert_success(&public_messages, "get_messages", "loop-messages");
    let serialized = public_messages["data"]["messages"].to_string();
    assert!(serialized.contains("every 3 seconds"));
    assert!(serialized.contains("rpc scheduled"));
    assert!(!serialized.contains("system-reminder"));

    session.write_line(&format!(
        r#"{{"type":"loop_update","id":"loop-update","taskId":"{task_id}","interval":"2h","prompt":"rpc updated"}}"#
    ));
    let (update_events, updated) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("loop-update")
    });
    assert_success(&updated, "loop_update", "loop-update");
    assert_eq!(updated["data"]["intervalSecs"], 7_200);
    assert_eq!(updated["data"]["prompt"], "rpc updated");
    assert!(update_events.iter().any(|line| {
        line.get("type").and_then(Value::as_str) == Some("loop_updated")
            && line["task"]["id"] == task_id
    }));

    session.write_line(r#"{"type":"loop_list","id":"loop-list"}"#);
    let (_events, listed) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("loop-list")
    });
    assert_success(&listed, "loop_list", "loop-list");
    assert_eq!(listed["data"].as_array().expect("loop list array").len(), 1);

    session.write_line(r#"{"type":"loop_update","id":"bad-loop-update","taskId":7}"#);
    session.write_line(&format!(
        r#"{{"type":"loop_delete","id":"loop-delete","taskId":"{task_id}"}}"#
    ));
    session.write_line(r#"{"type":"loop_list","id":"after-loop-delete"}"#);
    let mut malformed = None;
    let mut deleted = None;
    let mut empty = None;
    while malformed.is_none() || deleted.is_none() || empty.is_none() {
        let line = session.read_json_deadline(deadline);
        match line.get("id").and_then(Value::as_str) {
            Some("bad-loop-update") => malformed = Some(line),
            Some("loop-delete") => deleted = Some(line),
            Some("after-loop-delete") => empty = Some(line),
            _ => {}
        }
    }
    let deleted = deleted.expect("loop delete response");
    assert_success(&deleted, "loop_delete", "loop-delete");
    assert_eq!(deleted["data"], true);
    assert_failure(
        &malformed.expect("malformed loop update response"),
        "loop_update",
        Some("bad-loop-update"),
        "Invalid command",
    );
    let empty = empty.expect("loop list after delete");
    assert_success(&empty, "loop_list", "after-loop-delete");
    assert!(empty["data"].as_array().expect("empty loop list").is_empty());

    let output = session.finish();
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

const FAUX_REPLY: &str = "rpc-binary-faux-reply";

/// Contract: `get_available_models` returns the offline faux catalog entry so
/// CI can discover models without credentials or network.
#[test]
fn get_available_models_includes_offline_faux() {
    let (lines, output) = run_rpc(br#"{"type":"get_available_models","id":"models-1"}
"#);
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let response = find_response(&lines, "models-1");
    assert_success(response, "get_available_models", "models-1");
    let models = response["data"]["models"]
        .as_array()
        .unwrap_or_else(|| panic!("models array missing: {response}"));
    assert!(
        models.iter().any(|model| {
            model.get("provider").and_then(Value::as_str) == Some("faux")
                && model.get("id").and_then(Value::as_str) == Some("faux-1")
        }),
        "available models must include faux/faux-1: {response}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains('\u{1b}'),
        "no ANSI on get_available_models stdout"
    );
}

/// Contract: thinking controls round-trip on the public wire — list levels,
/// set an explicit level, and observe the effective level in the response and
/// subsequent `get_state`.
#[test]
fn thinking_controls_list_and_set_round_trip() {
    let mut session = RpcSession::spawn();
    let deadline = Instant::now() + Duration::from_secs(20);

    session.write_line(r#"{"type":"get_available_thinking_levels","id":"think-levels"}"#);
    let (_events, levels_response) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("think-levels")
    });
    assert_success(
        &levels_response,
        "get_available_thinking_levels",
        "think-levels",
    );
    let levels = levels_response["data"]["levels"]
        .as_array()
        .unwrap_or_else(|| panic!("levels array: {levels_response}"));
    assert!(
        !levels.is_empty(),
        "thinking level list must not be empty: {levels_response}"
    );
    assert!(
        levels.iter().any(|level| level.as_str() == Some("off")),
        "levels must include off: {levels_response}"
    );

    session.write_line(r#"{"type":"set_thinking_level","id":"think-set","level":"high"}"#);
    let (_events, set_response) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("think-set")
    });
    assert_success(&set_response, "set_thinking_level", "think-set");
    assert_eq!(
        set_response["data"]["requested"].as_str(),
        Some("high"),
        "requested level: {set_response}"
    );
    let effective = set_response["data"]["level"]
        .as_str()
        .expect("effective level string");
    assert!(
        !effective.is_empty(),
        "effective thinking level must be present: {set_response}"
    );

    session.write_line(r#"{"type":"get_state","id":"think-state"}"#);
    let (_events, state) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("think-state")
    });
    assert_success(&state, "get_state", "think-state");
    assert_eq!(
        state["data"]["thinkingLevel"].as_str(),
        Some(effective),
        "get_state must reflect set_thinking_level: {state}"
    );

    let output = session.finish();
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: after a faux prompt settles, session stats and last-assistant-text
/// reflect the recorded exchange on the public response envelope.
#[test]
fn session_stats_and_last_assistant_text_after_prompt() {
    let mut session = RpcSession::spawn();
    session.write_line(r#"{"type":"prompt","id":"stats-prompt","message":"stats prompt"}"#);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_prompt_ok = false;
    loop {
        let value = session.read_json_deadline(deadline);
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("stats-prompt")
        {
            assert_success(&value, "prompt", "stats-prompt");
            saw_prompt_ok = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("agent_settled") {
            break;
        }
    }
    assert!(saw_prompt_ok, "prompt must succeed before stats queries");

    session.write_line(r#"{"type":"get_session_stats","id":"stats-1"}"#);
    let (_events, stats) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("stats-1")
    });
    assert_success(&stats, "get_session_stats", "stats-1");
    let data = &stats["data"];
    assert!(
        data["userMessages"].as_u64().unwrap_or(0) >= 1,
        "userMessages after prompt: {stats}"
    );
    assert!(
        data["assistantMessages"].as_u64().unwrap_or(0) >= 1,
        "assistantMessages after prompt: {stats}"
    );
    assert!(
        data["totalMessages"].as_u64().unwrap_or(0) >= 2,
        "totalMessages after prompt: {stats}"
    );
    assert!(
        data.get("tokens").is_some_and(Value::is_object),
        "tokens object required: {stats}"
    );
    assert!(
        data.get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "sessionId present: {stats}"
    );

    session.write_line(r#"{"type":"get_last_assistant_text","id":"last-1"}"#);
    let (_events, last) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("last-1")
    });
    assert_success(&last, "get_last_assistant_text", "last-1");
    assert_eq!(
        last["data"]["text"].as_str(),
        Some(FAUX_REPLY),
        "last assistant text must match faux reply: {last}"
    );

    let output = session.finish();
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: `abort` is accepted on an idle connection and does not poison
/// later commands. A subsequent prompt still settles with agent_settled.
#[test]
fn abort_idle_then_prompt_still_settles() {
    let mut session = RpcSession::spawn();
    let deadline = Instant::now() + Duration::from_secs(20);

    session.write_line(r#"{"type":"abort","id":"abort-idle"}"#);
    let (_events, abort_response) = session.read_until(deadline, |line| {
        is_response(line) && line.get("id").and_then(Value::as_str) == Some("abort-idle")
    });
    assert_success(&abort_response, "abort", "abort-idle");
    assert!(
        abort_response.get("data").is_none() || abort_response.get("data") == Some(&Value::Null),
        "abort success data must be null/omitted: {abort_response}"
    );

    session.write_line(r#"{"type":"prompt","id":"after-abort","message":"still works"}"#);
    let mut saw_prompt = false;
    let mut settled = false;
    while Instant::now() < deadline {
        let value = session.read_json_deadline(deadline);
        if is_response(&value) && value.get("id").and_then(Value::as_str) == Some("after-abort") {
            assert_success(&value, "prompt", "after-abort");
            saw_prompt = true;
        }
        if value.get("type").and_then(Value::as_str) == Some("agent_settled") {
            settled = true;
            break;
        }
    }
    assert!(saw_prompt, "prompt after abort must succeed");
    assert!(settled, "prompt after abort must emit agent_settled");

    let output = session.finish();
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Contract: foreground `bash` runs a deterministic local command, returns
/// exit code and captured output on the response envelope, and keeps stdout
/// free of ANSI / plain diagnostics.
#[test]
fn foreground_bash_returns_output_and_exit_code() {
    let (lines, output) = run_rpc(
        br#"{"type":"bash","id":"bash-1","command":"printf 'hello-rpc-bash'"}
"#,
    );
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let response = find_response(&lines, "bash-1");
    assert_success(response, "bash", "bash-1");
    let data = &response["data"];
    assert_eq!(
        data.get("output").and_then(Value::as_str),
        Some("hello-rpc-bash"),
        "bash output: {response}"
    );
    assert_eq!(
        data.get("exitCode").and_then(Value::as_i64),
        Some(0),
        "bash exitCode: {response}"
    );
    assert_eq!(
        data.get("cancelled").and_then(Value::as_bool),
        Some(false),
        "bash cancelled flag: {response}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains('\u{1b}'),
        "ANSI must not appear on bash RPC stdout: {stdout:?}"
    );
    for line in stdout.lines() {
        assert!(
            !line.starts_with("Warning:") && !line.starts_with("Error:"),
            "plain diagnostic on stdout: {line}"
        );
    }
    // stderr is the diagnostics channel; presence is allowed, leakage is not.
    let _ = output.stderr;
}
