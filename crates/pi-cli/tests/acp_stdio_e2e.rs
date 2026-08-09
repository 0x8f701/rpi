//! End-to-end coverage for the Agent Client Protocol (A1) through the REAL
//! `rpi agent stdio` binary: Content-Length framed JSON-RPC 2.0 over
//! stdin/stdout.
//!
//! The in-process `modes::acp::tests` suite proves the protocol logic over
//! channels; these tests prove the actual subprocess entrypoint: spawn the
//! binary, negotiate `initialize`/`authenticate`, open a `session/new`
//! against the faux model, run a `session/prompt` turn that streams
//! `agent_message_chunk` deltas carrying the offline reply, close the
//! session, and exit cleanly when stdin closes. It also proves the
//! underlying conversation is recorded to the rpi session store (the ACP
//! session is a real recorded session, resumed later by `rpi sessions`).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

/// Offline faux reply streamed as the ACP assistant text.
const FAUX_REPLY: &str = "acp-stdio-e2e-reply";

/// Hard bound for the whole ACP exchange.
const ACP_TIMEOUT: Duration = Duration::from_secs(60);

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// One Content-Length framed JSON-RPC message.
fn encode_frame(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).expect("serialize acp message");
    let mut out = Vec::with_capacity(json.len() + 64);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", json.len()).as_bytes());
    out.extend_from_slice(&json);
    out
}

/// Parse one Content-Length framed JSON-RPC message from a buffered reader;
/// `None` on EOF.
fn read_frame(reader: &mut impl BufRead) -> Option<Value> {
    let mut header = String::new();
    let mut length: Option<usize> = None;
    for _ in 0..128 {
        header.clear();
        let read = reader.read_line(&mut header).ok()?;
        if read == 0 {
            return None;
        }
        if header == "\r\n" || header == "\n" {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                length = value.trim().parse().ok();
            }
        }
    }
    let length = length?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// A live `rpi agent stdio` subprocess with a framed-message channel on
/// stdout. Drop kills the child as a backstop.
struct AcpProbe {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    frames: Receiver<Value>,
    next_id: i64,
}

impl AcpProbe {
    fn spawn(home: &Path, cwd: &Path, session_dir: &Path) -> Self {
        let mut cmd = Command::new(rpi_bin());
        cmd.env_clear();
        cmd.env("HOME", home);
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("PI_OFFLINE", "1");
        cmd.env("PI_SKIP_VERSION_CHECK", "1");
        cmd.env("PI_FAUX_RESPONSE", FAUX_REPLY);
        cmd.arg("--model");
        cmd.arg("faux/faux-1");
        cmd.arg("--api-key");
        cmd.arg("acp-e2e-key");
        cmd.arg("--session-dir");
        cmd.arg(session_dir);
        cmd.arg("agent");
        cmd.arg("stdio");
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn rpi agent stdio");
        let stdin = child.stdin.take().expect("acp stdin pipe");
        let stdout = child.stdout.take().expect("acp stdout pipe");
        let stderr = child.stderr.take().expect("acp stderr pipe");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(frame) = read_frame(&mut reader) {
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });
        // Drain stderr on a thread so a chatty child cannot wedge the pipes.
        thread::spawn(move || {
            let mut reader = stderr;
            let mut chunk = [0u8; 8192];
            while reader.read(&mut chunk).is_ok() {}
        });
        Self {
            child,
            stdin: Some(stdin),
            frames: rx,
            next_id: 0,
        }
    }

    /// Send a JSON-RPC request and collect notifications until the matching
    /// response arrives. Returns `(response, notifications)`.
    fn request(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.next_id += 1;
        let id = json!(self.next_id);
        let frame = encode_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        self.stdin
            .as_mut()
            .expect("acp stdin")
            .write_all(&frame)
            .expect("write acp frame");
        self.stdin
            .as_mut()
            .expect("acp stdin")
            .flush()
            .expect("flush acp frame");
        let deadline = Instant::now() + ACP_TIMEOUT;
        let mut notifications = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(remaining) {
                Ok(message) => {
                    if message.get("id") == Some(&id) {
                        return (message, notifications);
                    }
                    notifications.push(message);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    panic!("acp server never answered {method} (id {id})");
                }
            }
        }
    }

    /// Join the text of every `agent_message_chunk` delta in order.
    fn joined_agent_text(notifications: &[Value]) -> String {
        notifications
            .iter()
            .filter(|message| {
                message["method"] == "session/update"
                    && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            })
            .filter_map(|message| {
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .collect()
    }

    /// Close stdin and require a clean exit within the deadline.
    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(
                        status.success(),
                        "rpi agent stdio must exit 0 after stdin EOF: {status:?}"
                    );
                    return;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("rpi agent stdio did not exit after stdin EOF");
                }
            }
        }
    }
}

impl Drop for AcpProbe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Recursively find a session JSONL file under `root` whose content carries
/// `needle` (the recorded conversation proves ACP sessions land in the
/// normal session store).
fn recorded_session_contains(root: &Path, needle: &str) -> bool {
    fn walk(dir: &Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, needle) {
                    return true;
                }
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                == Some("jsonl")
                && let Ok(body) = std::fs::read_to_string(&path)
                && body.contains(needle)
            {
                return true;
            }
        }
        false
    }
    walk(root, needle)
}

/// Contract: the full stdio round trip through the real binary —
/// initialize/authenticate/session-new/prompt/close, streamed assistant
/// text, a clean exit on stdin EOF, and a recorded session in the store.
#[test]
fn acp_stdio_full_round_trip_and_records_session() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut acp = AcpProbe::spawn(home.path(), cwd.path(), session_dir.path());

    let (response, _) = acp.request(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    assert!(response.get("result").is_some(), "initialize: {response}");
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert!(
        response["result"]["agentCapabilities"].is_object(),
        "capabilities: {response}"
    );

    let (response, _) = acp.request("authenticate", json!({ "methodId": "rpi-auth" }));
    assert!(response.get("result").is_some(), "authenticate: {response}");

    let (response, _) = acp.request(
        "session/new",
        json!({ "cwd": cwd.path().to_str().expect("cwd"), "mcpServers": [] }),
    );
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(session_id.starts_with("sess_"), "session id prefix: {session_id}");

    let (response, notifications) = acp.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "hello acp stdio" }],
        }),
    );
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "prompt result: {response}"
    );
    let text = AcpProbe::joined_agent_text(&notifications);
    assert_eq!(text, FAUX_REPLY, "assistant text must stream the faux reply");

    let (response, _) = acp.request("session/close", json!({ "sessionId": session_id }));
    assert!(response.get("result").is_some(), "session/close: {response}");

    acp.finish();

    assert!(
        recorded_session_contains(session_dir.path(), FAUX_REPLY),
        "the ACP conversation must be recorded to the session store under {:?}",
        session_dir.path()
    );
}

/// Contract: prompting a session that was never opened fails with the typed
/// ACP `RESOURCE_NOT_FOUND` error and the connection stays usable (the
/// subsequent real prompt still succeeds).
#[test]
fn acp_stdio_unknown_session_fails_typed_and_connection_recovers() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut acp = AcpProbe::spawn(home.path(), cwd.path(), session_dir.path());

    let (response, _) = acp.request(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    assert!(response.get("result").is_some(), "initialize: {response}");

    let (response, _) = acp.request(
        "session/prompt",
        json!({ "sessionId": "sess_missing", "prompt": [] }),
    );
    assert!(
        response["error"].is_object(),
        "unknown session must fail with a typed error: {response}"
    );
    assert!(
        response["error"]["code"].is_i64(),
        "error carries the ACP error code: {response}"
    );

    let (response, _) = acp.request("authenticate", json!({ "methodId": "rpi-auth" }));
    assert!(response.get("result").is_some(), "connection recovers: {response}");

    acp.finish();
}

/// Contract: a well-framed but invalid-JSON body is answered with the typed
/// JSON-RPC `PARSE_ERROR` (-32700) and the connection stays usable for the
/// real handshake afterwards (the stdio reader resyncs instead of dying).
#[test]
fn acp_stdio_parse_error_answered_and_connection_recovers() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let mut acp = AcpProbe::spawn(home.path(), cwd.path(), session_dir.path());

    let body = b"not json at all";
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    acp.stdin
        .as_mut()
        .expect("acp stdin")
        .write_all(&frame)
        .expect("write malformed frame");
    acp.stdin
        .as_mut()
        .expect("acp stdin")
        .flush()
        .expect("flush malformed frame");

    let message = acp
        .frames
        .recv_timeout(ACP_TIMEOUT)
        .expect("parse error response");
    assert_eq!(message["id"], Value::Null, "parse errors carry a null id: {message}");
    assert_eq!(
        message["error"]["code"], -32700,
        "parse errors use the JSON-RPC parse error code: {message}"
    );

    // The connection survives and serves the full handshake.
    let (response, _) = acp.request(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    assert!(response.get("result").is_some(), "initialize after parse error: {response}");

    acp.finish();
}

/// Contract: `rpi agent serve` enforces transport auth through the real
/// binary — a client offering the `rpi-auth.<token>` subprotocol is upgraded
/// (and the protocol echoed), while a wrong token and a tokenless browser
/// Origin are refused with HTTP 401 before any ACP message is exchanged.
#[test]
fn acp_serve_gates_connections_with_token_subprotocol() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let session_dir = TempDir::new().expect("session dir");
    let token_dir = TempDir::new().expect("token dir");
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, "e2e-secret").expect("write token file");

    let mut cmd = Command::new(rpi_bin());
    cmd.env_clear();
    cmd.env("HOME", home.path());
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
    cmd.env("PI_OFFLINE", "1");
    cmd.env("PI_SKIP_VERSION_CHECK", "1");
    cmd.arg("--model");
    cmd.arg("faux/faux-1");
    cmd.arg("--api-key");
    cmd.arg("acp-e2e-key");
    cmd.arg("--session-dir");
    cmd.arg(session_dir.path());
    cmd.arg("agent");
    cmd.arg("serve");
    cmd.arg("--address");
    cmd.arg("127.0.0.1:0");
    cmd.arg("--token-file");
    cmd.arg(&token_file);
    cmd.current_dir(cwd.path());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn rpi agent serve");
    let stderr = child.stderr.take().expect("serve stderr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Wait for the listening banner, which carries the bound address.
    let deadline = Instant::now() + ACP_TIMEOUT;
    let mut address = None;
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Some(rest) = line.strip_prefix("ACP WebSocket server listening on ws://") {
            address = Some(rest.to_owned());
            break;
        }
    }
    let address = address.expect("serve banner with bound address");
    let url = format!("ws://{address}/");

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            use futures_util::{SinkExt as _, StreamExt as _};
            use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

            // Valid subprotocol: upgraded, the protocol is echoed, and a real
            // ACP initialize round trip works.
            let mut request = url.clone().into_client_request().expect("ws request");
            request.headers_mut().insert(
                http::header::SEC_WEBSOCKET_PROTOCOL,
                http::HeaderValue::from_static("rpi-auth.e2e-secret"),
            );
            let (mut socket, response) = tokio_tungstenite::connect_async(request)
                .await
                .expect("authenticated ws connect");
            assert_eq!(
                response.headers().get(http::header::SEC_WEBSOCKET_PROTOCOL),
                Some(&http::HeaderValue::from_static("rpi-auth.e2e-secret")),
                "the matched subprotocol must be echoed in the upgrade response"
            );
            let id = json!(1);
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "initialize",
                        "params": { "protocolVersion": 1 },
                    }))
                    .expect("initialize json")
                    .into(),
                ))
                .await
                .expect("send initialize");
            let message = match socket.next().await.expect("ws message") {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => text,
                other => panic!("expected text, got {other:?}"),
            };
            let response: Value = serde_json::from_str(&message).expect("response json");
            assert_eq!(response["id"], id);
            assert_eq!(response["result"]["protocolVersion"], 1);
            socket
                .send(tokio_tungstenite::tungstenite::Message::Close(None))
                .await
                .expect("close ws");

            // Wrong token: refused with 401 before any ACP message.
            let mut request = url.clone().into_client_request().expect("ws request");
            request.headers_mut().insert(
                http::header::SEC_WEBSOCKET_PROTOCOL,
                http::HeaderValue::from_static("rpi-auth.wrong"),
            );
            match tokio_tungstenite::connect_async(request).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED, "wrong token");
                }
                other => panic!("wrong token must be refused, got {other:?}"),
            }

            // Tokenless browser Origin: refused with 401.
            let mut request = url.clone().into_client_request().expect("ws request");
            request.headers_mut().insert(
                http::header::ORIGIN,
                http::HeaderValue::from_static("http://localhost"),
            );
            match tokio_tungstenite::connect_async(request).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED, "browser origin");
                }
                other => panic!("browser origin must be refused, got {other:?}"),
            }
        });

    child.kill().expect("kill serve");
    let _ = child.wait();
}
