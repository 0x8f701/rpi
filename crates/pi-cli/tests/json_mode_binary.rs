//! End-to-end wire contracts for `rpi --mode json`.
//!
//! Drives the real `rpi` binary with a temporary HOME and the built-in faux
//! model. Assertions cover the public LF-delimited JSON event stream only —
//! session header, agent/turn lifecycle, nested text start/delta/end, settled
//! termination, no ANSI on stdout, and stderr separation. No live credentials
//! or network are required (`PI_FAUX_RESPONSE` seeds the offline reply).

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const FAUX_TEXT: &str = "json-mode-binary-reply";

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Spawn `rpi --mode json` with one prompt, isolated HOME, and offline faux.
fn run_json_mode(prompt: &str) -> (Vec<Value>, Output) {
    let home = tempfile::tempdir().expect("temporary HOME");
    let cwd = tempfile::tempdir().expect("temporary cwd");
    let mut child = Command::new(rpi_bin())
        .args([
            "--mode",
            "json",
            "--offline",
            "--model",
            "faux/faux-1",
            "--cwd",
            cwd.path().to_str().expect("cwd utf8"),
            prompt,
        ])
        .env("HOME", home.path())
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_FAUX_RESPONSE", FAUX_TEXT)
        .env_remove("PI_CODING_AGENT_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi --mode json");

    let stdout = child.stdout.take().expect("stdout pipe");
    let mut stderr_pipe = child.stderr.take().expect("stderr pipe");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut lines = Vec::new();
    let mut raw = Vec::new();

    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            let mut stderr = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut stderr);
            panic!(
                "rpi --mode json exceeded deadline; partial lines={lines:?} stderr={}",
                String::from_utf8_lossy(&stderr)
            );
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                raw.extend_from_slice(line.as_bytes());
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                assert!(
                    !trimmed.contains('\u{1b}'),
                    "JSON mode stdout must not contain ANSI escapes: {trimmed:?}"
                );
                assert!(
                    !trimmed.to_ascii_lowercase().starts_with("warning:"),
                    "JSON mode stdout must not contain plain-text warnings: {trimmed:?}"
                );
                let value = serde_json::from_str::<Value>(trimmed).unwrap_or_else(|error| {
                    panic!("JSON mode stdout line is not JSON ({error}): {trimmed}")
                });
                assert!(
                    value.is_object(),
                    "JSON mode stdout line must be a JSON object: {value}"
                );
                assert!(
                    value.get("type").and_then(Value::as_str).is_some(),
                    "every JSON mode record must carry type: {value}"
                );
                lines.push(value);
            }
            Err(error) => panic!("reading rpi --mode json stdout: {error}"),
        }
    }

    let mut stderr = Vec::new();
    let _ = stderr_pipe.read_to_end(&mut stderr);
    let status = child.wait().expect("wait rpi --mode json");
    drop(home);
    drop(cwd);
    (
        lines,
        Output {
            status,
            stdout: raw,
            stderr,
        },
    )
}

fn event_type(line: &Value) -> &str {
    line.get("type")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing type on {line}"))
}

fn nested_assistant_event_type(line: &Value) -> Option<&str> {
    line.get("assistantMessageEvent")
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
}

fn first_index(lines: &[Value], ty: &str) -> usize {
    lines
        .iter()
        .position(|line| event_type(line) == ty)
        .unwrap_or_else(|| panic!("missing {ty} in {lines:?}"))
}

/// Contract: JSON mode emits LF-delimited objects starting with the session
/// header, then agent/turn/message lifecycle, nested text_start → text_delta →
/// text_end inside message_update, and terminates the turn on agent_settled.
/// Stdout stays pure JSONL (no ANSI); diagnostics stay on stderr.
#[test]
fn json_mode_lf_event_order_text_lifecycle_and_stdout_purity() {
    let (lines, output) = run_json_mode("ping from json mode binary test");
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let raw = String::from_utf8_lossy(&output.stdout);
    assert!(
        !raw.contains('\u{1b}'),
        "ANSI CSI must not appear on JSON mode stdout: {raw:?}"
    );
    assert!(
        raw.ends_with('\n') || lines.is_empty(),
        "JSONL stream must be LF-delimited: {raw:?}"
    );
    // Stderr may carry diagnostics; they must not leak onto the JSONL channel.
    let _ = output.stderr;

    assert!(
        !lines.is_empty(),
        "JSON mode must emit at least the session header"
    );

    // First record is the session header (SessionStartedEvent wire shape).
    let header = &lines[0];
    assert_eq!(
        event_type(header),
        "session",
        "first record must be session header: {header}"
    );
    assert_eq!(
        header.get("version").and_then(Value::as_u64),
        Some(3),
        "session header version: {header}"
    );
    assert!(
        header
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "session header id: {header}"
    );
    assert!(
        header
            .get("cwd")
            .and_then(Value::as_str)
            .is_some_and(|cwd| !cwd.is_empty()),
        "session header cwd: {header}"
    );
    assert!(
        header
            .get("timestamp")
            .and_then(Value::as_str)
            .is_some_and(|ts| !ts.is_empty()),
        "session header timestamp: {header}"
    );

    let agent_start = first_index(&lines, "agent_start");
    let turn_start = first_index(&lines, "turn_start");
    let message_start = first_index(&lines, "message_start");
    let message_end = first_index(&lines, "message_end");
    let turn_end = first_index(&lines, "turn_end");
    let agent_end = first_index(&lines, "agent_end");
    let settled = first_index(&lines, "agent_settled");

    assert!(
        agent_start < turn_start,
        "agent_start before turn_start: {lines:?}"
    );
    assert!(
        turn_start < message_start,
        "turn_start before message_start: {lines:?}"
    );
    assert!(
        message_start < message_end,
        "message_start before message_end: {lines:?}"
    );
    assert!(
        message_end < turn_end,
        "message_end before turn_end: {lines:?}"
    );
    assert!(
        turn_end < agent_end,
        "turn_end before agent_end: {lines:?}"
    );
    assert!(
        agent_end < settled,
        "agent_end before agent_settled: {lines:?}"
    );
    assert_eq!(
        lines[settled],
        serde_json::json!({"type": "agent_settled"}),
        "agent_settled must be the exact wire object"
    );
    // Turn completes at agent_settled; nothing protocol-bearing follows it.
    assert_eq!(
        settled,
        lines.len() - 1,
        "agent_settled must be the final JSONL record for a single prompt: {lines:?}"
    );

    // Nested text lifecycle lives on message_update.assistantMessageEvent.
    let mut saw_text_start = false;
    let mut saw_text_delta = false;
    let mut saw_text_end = false;
    let mut assembled = String::new();
    let mut last_text_kind: Option<&str> = None;
    for line in &lines {
        if event_type(line) != "message_update" {
            continue;
        }
        match nested_assistant_event_type(line) {
            Some("text_start") => {
                assert!(
                    !saw_text_end,
                    "text_start must precede text_end: {lines:?}"
                );
                saw_text_start = true;
                last_text_kind = Some("text_start");
                let event = &line["assistantMessageEvent"];
                assert_eq!(
                    event.get("content_index").and_then(Value::as_u64),
                    Some(0),
                    "text_start content_index: {line}"
                );
            }
            Some("text_delta") => {
                assert!(
                    saw_text_start,
                    "text_delta requires prior text_start: {lines:?}"
                );
                assert!(
                    !saw_text_end,
                    "text_delta must precede text_end: {lines:?}"
                );
                saw_text_delta = true;
                last_text_kind = Some("text_delta");
                let delta = line["assistantMessageEvent"]["delta"]
                    .as_str()
                    .expect("text_delta.delta string");
                assert!(!delta.is_empty(), "text_delta must carry content: {line}");
                assembled.push_str(delta);
            }
            Some("text_end") => {
                assert!(
                    saw_text_start,
                    "text_end requires prior text_start: {lines:?}"
                );
                assert!(
                    matches!(last_text_kind, Some("text_start" | "text_delta")),
                    "text_end must follow text_start/delta, not {last_text_kind:?}: {lines:?}"
                );
                saw_text_end = true;
                last_text_kind = Some("text_end");
                let content = line["assistantMessageEvent"]["content"]
                    .as_str()
                    .expect("text_end.content string");
                assert_eq!(
                    content, FAUX_TEXT,
                    "text_end content must match faux reply: {line}"
                );
                if !assembled.is_empty() {
                    assert_eq!(
                        assembled, FAUX_TEXT,
                        "concatenated text_delta must equal text_end content"
                    );
                }
            }
            _ => {}
        }
    }

    assert!(
        saw_text_start,
        "expected text_start on message_update: {lines:?}"
    );
    assert!(
        saw_text_delta,
        "expected text_delta on message_update: {lines:?}"
    );
    assert!(
        saw_text_end,
        "expected text_end on message_update: {lines:?}"
    );

    // Final assistant message_end must surface the faux text on the public wire.
    let assistant_end = lines.iter().rev().find(|line| {
        event_type(line) == "message_end" && line["message"]["role"] == "assistant"
    });
    let assistant_end = assistant_end.expect("assistant message_end on wire");
    let end_text = assistant_end["message"]["content"]
        .as_array()
        .expect("assistant content array")
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<String>();
    assert_eq!(
        end_text, FAUX_TEXT,
        "assistant message_end text: {assistant_end}"
    );
}
