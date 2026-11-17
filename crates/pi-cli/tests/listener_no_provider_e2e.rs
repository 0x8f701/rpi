//! Real-binary providerless listener acceptance: empty HOME/agent dir with no
//! model — Web `/web` and `/status` answer, a prompt is refused with the typed
//! `model_not_configured` code and zero turn/history/child/provider effect; then
//! (same process) a dummy models.json + set_model makes the real loopback mock
//! serve the parent turn, and task_spawn's child inherits the live model.
#![cfg(unix)]

use std::{fs, io::{Read, Write}, net::TcpStream, path::Path, process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex}, thread, time::{Duration, Instant}};
use serde_json::{Value, json};
use tempfile::TempDir;
use sha2::{Digest, Sha256};
const REPLY: &str = "dummy-provider-reply"; const DEADLINE: Duration = Duration::from_secs(30); const BANNER: &str = "Control plane listening on http://";
#[derive(Default)]
struct MockProvider { requests: Mutex<Vec<String>>, replies: Mutex<Vec<String>> }
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn unique_id(tag: &str) -> String {
    format!("{tag}-{}", NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}
/// Loopback OpenAI-completions mock: captures each request, then records the
/// exact response-write outcome (bytes, SSE newline shape, result, digest).
/// A captured request is NOT proof that a reply was written; `replies` is.
fn spawn_mock_provider() -> (String, Arc<MockProvider>) { spawn_mock_provider_with_gate(None) }
fn spawn_mock_provider_with_gate(gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>) -> (String, Arc<MockProvider>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock provider"); let address = listener.local_addr().expect("mock provider address");
    let mock = Arc::new(MockProvider::default());
    let sink = mock.clone(); let mut gate = gate; thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut bytes = Vec::new(); let mut chunk = [0u8; 8192];
            while !request_complete(&bytes) {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                }
            }
            let request = String::from_utf8_lossy(&bytes).into_owned();
            if let Ok(mut requests) = sink.requests.lock() { requests.push(request); }
            if let Some((entered, release)) = gate.take() { let _ = entered.send(()); let _ = release.recv(); }
            let body = format!("data: {{\"id\":\"chat-1\",\"model\":\"dummy-1\",\"choices\":[{{\"delta\":{{\"content\":\"{REPLY}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n");
            let head = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
            let mut response = head.into_bytes(); response.extend_from_slice(body.as_bytes());
            let write = stream.write_all(&response);
            let flush = stream.flush();
            let trace = format!(
                "response_bytes={} body_bytes={} lf_pairs={} escaped_lf={} sha256={} write_ok={} flush_ok={} write_err={:?} flush_err={:?}",
                response.len(), body.len(), body.matches("\n\n").count(), body.matches("\\n").count(),
                digest(&response), write.is_ok(), flush.is_ok(),
                write.as_ref().err().map(std::io::Error::kind), flush.as_ref().err().map(std::io::Error::kind)
            );
            if let Ok(mut replies) = sink.replies.lock() { replies.push(trace); }
        }
    });
    (format!("http://{address}/v1/"), mock)
}
fn request_complete(bytes: &[u8]) -> bool {
    let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else { return false };
    let length = String::from_utf8_lossy(&bytes[..split]).lines().find_map(|line| {
        line.split_once(':').filter(|entry| entry.0.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
    }).unwrap_or(0);
    bytes.len() >= split + 4 + length
}
fn http(addr: &str, request: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).expect("connect control plane");
    stream.set_read_timeout(Some(DEADLINE)).expect("read timeout"); stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new(); stream.read_to_string(&mut response).expect("read response");
    let (head, body) = response.split_once("\r\n\r\n").expect("http head/body split"); let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("http status");
    (status, head.to_owned(), body.to_owned())
}
fn rpc(addr: &str, command: &Value) -> Value { rpc_status(addr, 200, command) }
fn rpc_status(addr: &str, expected: u16, command: &Value) -> Value {
    let payload = command.to_string();
    let request = format!("POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}", payload.len());
    let (status, _, body) = http(addr, &request);
    assert_eq!(status, expected, "rpc {command} answered {status}, expected {expected}: {body}");
    let response: Value = serde_json::from_str(&body).expect("rpc JSON response");
    assert_eq!(response["success"], json!(expected == 200), "rpc {command} flag mismatch: {response}");
    response
}
fn poll<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(value) = probe() { return value; }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(25));
    }
}
fn request_json(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").expect("request head/body").1).expect("request JSON body")
}
fn request_trace(request: &str) -> String {
    let body = request.split_once("\r\n\r\n").expect("request head/body").1; let mut hasher = std::collections::hash_map::DefaultHasher::new(); std::hash::Hash::hash(body.as_bytes(), &mut hasher);
    format!("model={} bytes={} hash={:016x}", request_json(request)["model"], body.len(), std::hash::Hasher::finish(&hasher))
}
fn job_ids(listener: &Listener) -> Vec<String> {
    let listed = rpc(&listener.address, &json!({"type":"job_list","id":unique_id("jobs-snapshot")}));
    assert_eq!(listed["success"], json!(true), "job_list failed: {listed}");
    listed["data"]["jobs"].as_array().expect("jobs array").iter().filter_map(|job| job["id"].as_str().map(str::to_owned)).collect()
}
struct Listener {
    child: Child,
    address: String,
    stderr: Arc<Mutex<String>>,
}
impl Listener {
    fn spawn(home: &Path, cwd: &Path, agent: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
        command.env_clear(); command.env("HOME", home).env("RPI_HOME", agent).env("TERM", "xterm-256color");
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", path).env("RPI_OFFLINE", "1").env("RPI_SKIP_VERSION_CHECK", "1");
        command.args(["--listen", "127.0.0.1:0", "--listen-plaintext", "--tools", "task"]); command.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn rpi listener");
        let mut pipe = child.stderr.take().expect("listener stderr");
        let stderr = Arc::new(Mutex::new(String::new())); let buffer = stderr.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut text) = buffer.lock() {
                            text.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        }
                    }
                }
            }
        });
        let mut listener = Self { child, address: String::new(), stderr };
        let deadline = Instant::now() + DEADLINE;
        loop {
            let text = listener.stderr.lock().expect("stderr").clone();
            if let Some(start) = text.find(BANNER) {
                let tail = &text[start + BANNER.len()..];
                listener.address = tail[..tail.find(' ').expect("address terminator")].to_owned();
                return listener;
            }
            assert!(Instant::now() < deadline, "listener never announced its control plane; stderr: {text:?}");
            thread::sleep(Duration::from_millis(25));
        }
    }
    fn stop(mut self) {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.child.id() as i32), nix::sys::signal::Signal::SIGTERM)
            .expect("SIGTERM listener");
        let status = poll("listener exit after SIGTERM", || self.child.try_wait().expect("poll listener exit"));
        assert!(status.success(), "listener exit after SIGTERM: {status:?}");
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill(); let _ = self.child.wait();
    }
}
/// Default sandbox policy (no `settings.json` is written, so the product's
/// default `danger-full-access` applies): full providerless → live-model flow.
#[test]
fn providerless_listener_switches_config_in_process_and_child_inherits_live_model() {
    let (home, cwd, agent) = (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap());
    let (base_url, mock) = spawn_mock_provider();
    let mut listener = Listener::spawn(home.path(), cwd.path(), agent.path());
    // Phase 1: providerless boot serves the Web client, runner probe, null model.
    let (status, head, body) = http(&listener.address, &format!("GET /web HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n", listener.address));
    assert_eq!(status, 200, "GET /web must serve the Web client"); assert!(head.to_ascii_lowercase().contains("content-type: text/html"), "{head}");
    assert!(body.to_ascii_lowercase().starts_with("<!doctype html"), "GET /web must return the Web document");
    let (status, _, body) = http(&listener.address, &format!("GET /status HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n", listener.address));
    assert_eq!(status, 200, "GET /status must answer: {body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["tokenRequired"], json!(false), "{body}");
    let state = rpc(&listener.address, &json!({"type":"get_state","id":"state-1"}));
    assert_eq!(state["success"], json!(true), "{state}"); assert!(state["data"]["model"].is_null(), "providerless get_state keeps model null: {state}");
    assert_eq!(state["data"]["messageCount"], json!(0), "{state}");
    let session_id = state["data"]["sessionId"].clone();
    // Phase 2: typed refusal (HTTP 400) before admission — no turn, history, fork, child, provider.
    let refused = rpc_status(&listener.address, 400, &json!({"type":"prompt","id":"prompt-1","message":"ping without a model"}));
    assert_eq!(refused["success"], json!(false), "{refused}");
    assert_eq!(refused["data"]["code"], "model_not_configured", "{refused}");
    assert!(refused["error"].as_str().is_some_and(|e| e.contains("No model is configured for this agent")), "{refused}");
    let after = rpc(&listener.address, &json!({"type":"get_state","id":"state-2"}));
    assert_eq!(after["data"]["messageCount"], json!(0), "{after}");
    assert_eq!(after["data"]["isStreaming"], json!(false), "{after}");
    assert_eq!(after["data"]["sessionId"], session_id, "rejection must not fork a session");
    assert_eq!(rpc(&listener.address, &json!({"type":"get_messages","id":"m1"}))["data"]["messages"], json!([]));
    assert_eq!(rpc(&listener.address, &json!({"type":"job_list","id":"j1"}))["data"]["jobs"], json!([]));
    assert!(mock.requests.lock().unwrap().is_empty(), "a refused prompt reached a provider");
    assert!(listener.child.try_wait().unwrap().is_none(), "the listener exited");
    // Phase 3: same process — publish the dummy provider catalog and select it.
    let models = json!({"providers":{"dummy":{"baseUrl":base_url,"api":"openai-completions","apiKey":"dummy-key",
        "models":[{"id":"dummy-1","name":"Dummy One","contextWindow":32768,"maxTokens":4096}]}}});
    fs::write(agent.path().join("models.json"), models.to_string()).unwrap();
    let selected = rpc(&listener.address, &json!({"type":"set_model","id":"set-1","provider":"dummy","modelId":"dummy-1"}));
    assert_eq!(selected["success"], json!(true), "{selected}"); assert_eq!(selected["data"]["id"], "dummy-1", "{selected}");
    // P1: spawn the child from the still-providerless generation BEFORE the
    // main prompt's deferred refresh. Settle the job first (exact `completed`),
    // then require its dummy-model provider request without waiting on the mock.
    let spawned = rpc(&listener.address, &json!({"type":"task_spawn","id":"task-1","args":{"task":"child ping","agent":"task"}}));
    assert_eq!(spawned["success"], json!(true), "{spawned}");
    let job_id = spawned["data"]["spawns"][0]["jobId"].as_str().expect("child job id").to_owned();
    let job = poll("the child job to settle", || {
        let listed = rpc(&listener.address, &json!({"type":"job_list","id":unique_id("jobs-poll")}));
        assert_eq!(listed["success"], json!(true), "job_list poll failed: {listed}");
        listed["data"]["jobs"].as_array()?.clone().into_iter().find(|job| job["id"] == json!(job_id))
            .filter(|job| matches!(job["status"].as_str(), Some("completed" | "failed" | "cancelled")))
    });
    assert_eq!(job["status"], "completed", "inherited-model child must run: {job}");
    let child_request = mock.requests.lock().unwrap().iter().rev().find(|request| request.contains("child ping")).cloned()
        .expect("a completed child must have issued its provider request");
    assert_eq!(request_json(&child_request)["model"], "dummy-1", "child must inherit the live model");
    let prompted = rpc(&listener.address, &json!({"type":"prompt","id":"prompt-2","message":"ping the live parent"}));
    assert_eq!(prompted["success"], json!(true), "{prompted}");
    assert!(prompted["data"]["turnId"].is_string(), "prompt must admit a real turn: {prompted}");
    let deadline = Instant::now() + DEADLINE;
    loop {
        let state = rpc(&listener.address, &json!({"type":"get_state","id":unique_id("state-poll")}));
        assert_eq!(state["success"], json!(true), "get_state poll failed: {state}");
        let messages = rpc(&listener.address, &json!({"type":"get_messages","id":unique_id("messages-poll")}));
        assert_eq!(messages["success"], json!(true), "get_messages poll failed: {messages}");
        let settled = state["data"]["isStreaming"] == json!(false) && messages["data"]["messages"].to_string().contains(REPLY);
        if settled { break; }
        assert!(Instant::now() < deadline, "parent turn did not settle: state={} requests={:?} replies={:?}", json!({"isStreaming": state["data"]["isStreaming"], "messageCount": state["data"]["messageCount"], "pendingMessageCount": state["data"]["pendingMessageCount"], "activeTurnModel": state["data"]["activeTurnModel"]}), mock.requests.lock().unwrap().iter().map(|r| request_trace(r)).collect::<Vec<_>>(), mock.replies.lock().unwrap().clone());
        thread::sleep(Duration::from_millis(25));
    }
    let messages = rpc(&listener.address, &json!({"type":"get_messages","id":"m2"})); assert!(messages.to_string().contains(REPLY), "live turn persisted no mock reply: {messages}");
    let parent_request = poll("the parent provider request", || {
        mock.requests.lock().unwrap().iter().rev().find(|r| r.contains("ping the live parent")).cloned()
    });
    assert_eq!(request_json(&parent_request)["model"], "dummy-1");
    let reply = poll("the mock response write", || mock.replies.lock().unwrap().last().cloned());
    assert!(reply.contains("write_ok=true") && reply.contains("flush_ok=true"), "mock reply write failed: {reply}");
    assert!(reply.contains("escaped_lf=0") && reply.contains("lf_pairs=3"), "mock SSE must use real LF separators: {reply}");
    let replies = mock.replies.lock().unwrap();
    assert!(replies.len() >= 2, "child and parent provider replies must both be written: {replies:?}");
    assert!(replies.iter().all(|reply| reply.contains("write_ok=true")), "{replies:?}");
    assert_eq!(rpc(&listener.address, &json!({"type":"get_state","id":"state-4"}))["data"]["model"]["id"], "dummy-1");
    listener.stop();
}

/// P1 under explicit confinement: `sandbox.mode: "workspace-write"` (the
/// official confined value; see existing sandbox-mode fixtures) is written
/// before startup, then a child is spawned immediately after `set_model` and
/// before any main prompt. The child must run from the live provider authority
/// (loopback dummy is operator-owned once models.json is selected), so a
/// providerless generation is the failure under test. Asserts stay exact:
/// terminal `completed` and the `dummy-1` provider request.
#[test]
fn workspace_write_confined_generation_serves_live_child_before_prompt() {
    let (home, cwd, agent) = (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap());
    fs::write(agent.path().join("settings.json"), r#"{"sandbox":{"mode":"workspace-write"}}"#).unwrap();
    let (base_url, mock) = spawn_mock_provider();
    let mut listener = Listener::spawn(home.path(), cwd.path(), agent.path());
    let models = json!({"providers":{"dummy":{"baseUrl":base_url,"api":"openai-completions","apiKey":"dummy-key",
        "models":[{"id":"dummy-1","name":"Dummy One","contextWindow":32768,"maxTokens":4096}]}}});
    fs::write(agent.path().join("models.json"), models.to_string()).unwrap();
    let selected = rpc(&listener.address, &json!({"type":"set_model","id":"confined-set-1","provider":"dummy","modelId":"dummy-1"}));
    assert_eq!(selected["success"], json!(true), "{selected}");
    let spawned = rpc(&listener.address, &json!({"type":"task_spawn","id":"confined-task-1","args":{"task":"confined child ping","agent":"task"}}));
    assert_eq!(spawned["success"], json!(true), "{spawned}");
    let job_id = spawned["data"]["spawns"][0]["jobId"].as_str().expect("confined child job id").to_owned();
    let job = poll("the confined child job to settle", || {
        let listed = rpc(&listener.address, &json!({"type":"job_list","id":unique_id("confined-jobs-poll")}));
        assert_eq!(listed["success"], json!(true), "job_list poll failed: {listed}");
        listed["data"]["jobs"].as_array()?.clone().into_iter().find(|job| job["id"] == json!(job_id))
            .filter(|job| matches!(job["status"].as_str(), Some("completed" | "failed" | "cancelled")))
    });
    assert_eq!(job["status"], "completed", "workspace-write confined child must run: {job}");
    let child_request = mock.requests.lock().unwrap().iter().rev().find(|request| request.contains("confined child ping")).cloned()
        .expect("a completed confined child must have issued its provider request");
    assert_eq!(request_json(&child_request)["model"], "dummy-1", "confined child must inherit the live model");
    listener.stop();
}

/// Original active-stable contract: a main turn is streaming from endpoint A
/// (its mock response is gated by channels), the operator selects model B, and
/// `task_spawn` must be rejected with the typed `session_processing` busy code
/// before it can create a job or reach B. Releasing A lets the turn settle;
/// only then may a child spawn and complete against B. The gate signal is the
/// ordering proof (no sleeps), and request bodies are never printed.
#[test]
fn active_turn_cross_model_task_spawn_is_typed_busy_then_after_settle_child_runs() {
    let (home, cwd, agent) = (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap());
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let (url_a, mock_a) = spawn_mock_provider_with_gate(Some((entered_tx, release_rx)));
    let (url_b, mock_b) = spawn_mock_provider();
    let mut listener = Listener::spawn(home.path(), cwd.path(), agent.path());
    let models = json!({"providers":{
        "alpha":{"baseUrl":url_a,"api":"openai-completions","apiKey":"dummy-key",
            "models":[{"id":"a1","name":"Alpha One","contextWindow":32768,"maxTokens":4096}]},
        "beta":{"baseUrl":url_b,"api":"openai-completions","apiKey":"dummy-key",
            "models":[{"id":"b1","name":"Beta One","contextWindow":32768,"maxTokens":4096}]}
    }});
    fs::write(agent.path().join("models.json"), models.to_string()).unwrap();
    let set_a = rpc(&listener.address, &json!({"type":"set_model","id":"active-set-a","provider":"alpha","modelId":"a1"}));
    assert_eq!(set_a["success"], json!(true), "{set_a}");
    let prompted = rpc(&listener.address, &json!({"type":"prompt","id":"active-prompt","message":"active parent A"}));
    assert_eq!(prompted["success"], json!(true), "{prompted}");
    assert!(prompted["data"]["turnId"].is_string(), "prompt must admit a real turn: {prompted}");
    entered_rx.recv_timeout(DEADLINE).expect("mock A must receive the active turn request before continuing");
    let active = rpc(&listener.address, &json!({"type":"get_state","id":unique_id("active-state")}));
    assert_eq!(active["success"], json!(true), "{active}");
    assert_eq!(active["data"]["isStreaming"], json!(true), "A must still be streaming while gated: {active}");
    assert_eq!(active["data"]["activeTurnModel"]["id"], "a1", "{active}");
    assert_eq!(mock_a.requests.lock().unwrap().len(), 1, "exactly one active request A");
    assert_eq!(request_json(&mock_a.requests.lock().unwrap()[0])["model"], "a1");
    let set_b = rpc(&listener.address, &json!({"type":"set_model","id":"active-set-b","provider":"beta","modelId":"b1"}));
    assert_eq!(set_b["success"], json!(true), "{set_b}");
    assert_eq!(set_b["data"]["id"], "b1", "{set_b}");
    let pending = rpc(&listener.address, &json!({"type":"get_state","id":unique_id("pending-state")}));
    assert_eq!(pending["success"], json!(true), "{pending}");
    assert_eq!(pending["data"]["model"]["id"], "b1", "selection must be live while A streams: {pending}");
    assert_eq!(pending["data"]["activeTurnModel"]["id"], "a1", "active turn keeps A authority: {pending}");
    assert_eq!(pending["data"]["isStreaming"], json!(true), "{pending}");
    let jobs_before = job_ids(&listener);
    let guarded = rpc_status(&listener.address, 400, &json!({"type":"task_spawn","id":"guarded-task","args":{"task":"guarded child ping","agent":"task"}}));
    assert_eq!(guarded["success"], json!(false), "{guarded}");
    assert_eq!(guarded["data"]["code"], "session_processing", "{guarded}");
    assert!(guarded["error"].as_str().is_some_and(|error| error.contains("already processing")), "{guarded}");
    assert_eq!(job_ids(&listener), jobs_before, "busy rejection must not create a job");
    assert!(mock_b.requests.lock().unwrap().is_empty(), "busy rejection must not reach endpoint B");
    release_tx.send(()).expect("release the gated A response");
    let deadline = Instant::now() + DEADLINE;
    loop {
        let state = rpc(&listener.address, &json!({"type":"get_state","id":unique_id("settle-state")}));
        assert_eq!(state["success"], json!(true), "{state}");
        let messages = rpc(&listener.address, &json!({"type":"get_messages","id":unique_id("settle-messages")}));
        assert_eq!(messages["success"], json!(true), "{messages}");
        if state["data"]["isStreaming"] == json!(false) && messages["data"]["messages"].to_string().contains(REPLY) { break; }
        assert!(Instant::now() < deadline, "released A turn did not settle: state={} requests={:?}", state.to_string(), mock_a.requests.lock().unwrap().iter().map(|r| request_trace(r)).collect::<Vec<_>>());
        thread::sleep(Duration::from_millis(25));
    }
    let a_reply = mock_a.replies.lock().unwrap().last().cloned().expect("gated A reply must be written");
    assert!(a_reply.contains("write_ok=true"), "gated A reply write failed: {a_reply}");
    let spawned = rpc(&listener.address, &json!({"type":"task_spawn","id":"after-release-task","args":{"task":"after-release child ping","agent":"task"}}));
    assert_eq!(spawned["success"], json!(true), "{spawned}");
    let job_id = spawned["data"]["spawns"][0]["jobId"].as_str().expect("child job id").to_owned();
    let job = poll("the post-settle child job to settle", || {
        let listed = rpc(&listener.address, &json!({"type":"job_list","id":unique_id("after-jobs-poll")}));
        assert_eq!(listed["success"], json!(true), "job_list poll failed: {listed}");
        listed["data"]["jobs"].as_array()?.clone().into_iter().find(|job| job["id"] == json!(job_id))
            .filter(|job| matches!(job["status"].as_str(), Some("completed" | "failed" | "cancelled")))
    });
    assert_eq!(job["status"], "completed", "post-settle child must run on B: {job}");
    let b_request = mock_b.requests.lock().unwrap().iter().rev().find(|request| request.contains("after-release child ping")).cloned()
        .expect("completed post-settle child must have issued its provider request");
    assert_eq!(request_json(&b_request)["model"], "b1", "post-settle child must use the live B model");
    assert!(mock_b.replies.lock().unwrap().last().is_some_and(|reply| reply.contains("write_ok=true")), "B reply write must succeed");
    listener.stop();
}
