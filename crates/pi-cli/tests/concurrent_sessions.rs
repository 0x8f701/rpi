//! Real listen/WebSocket concurrency contracts for the multi-session
//! manager (`SessionRuntimeManager` + `modes::listen`).
//!
//! Drives the actual TCP listener with an in-process faux spawner: every
//! session (primary + children) is an independent `Application`, and turns
//! stream through a test-controlled GATED `StreamFn` (start + one delta,
//! then block until the test releases or aborts). There are no bursts, no
//! wall-clock sleeps for ordering, and no real network/auth: tests wait on
//! the gate's `started` signal, assert concurrent/busy behavior while the
//! turn is provably in flight, then release and wait for the completion
//! event. Each test is a full wire round trip (JSON-RPC over WebSocket),
//! never source text.
//!
//! Covered contracts:
//! - `new_session` builds an independent runtime while the primary is
//!   mid-prompt; child commands progress without waiting on the primary.
//! - Multi-client fan-out: EVERY WebSocket connection receives EVERY
//!   runtime's events, tagged with the owning runtime's top-level
//!   `sessionId`; a dropped WebSocket aborts only that connection — the
//!   session stays loaded and other clients keep streaming.
//! - Top-level `sessionId` routes commands with per-session state isolation;
//!   unknown ids (including lifecycle source ids) fail closed; absent ids
//!   target the primary; `switch_session` dedups by canonical path.
//! - `close_session` rejects missing/unknown ids and every reachable busy
//!   guard with its exact reason WITHOUT cancelling the work.
//! - The concurrent-session cap counts distinct runtimes (primary included),
//!   rejects beyond the limit without evicting, and recovers after an idle
//!   session is closed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use pi_agent::{StreamFn, ThinkingLevel};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Model, StopReason,
    new_assistant_message_event_stream,
};
use pi_cli::extension_ui::ExtensionUiAdapter;
use pi_cli::modes::listen::{ListenConfig, ListenHandle, start};
use pi_cli::modes::session_runtime_manager::{
    MAX_LOADED_SESSIONS, SessionSpawnKind, SessionSpawnRequest, SessionSpawnResult,
    SessionSpawner,
};
use pi_coding::{Application, Session, SessionOptions};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::Message as WsMessage,
};

#[path = "common/mod.rs"]
mod common;
use common::*;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const GATED_REPLY_TEXT: &str = "gated controller reply";

/// Test-controlled turn gate shared by the primary AND every spawned child:
/// each turn's stream signals `started`, emits a single delta, then blocks
/// until the test releases it (or the app aborts it). This makes "still in
/// flight" deterministic with zero socket load.
#[derive(Clone)]
struct Gate {
    started: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

impl Gate {
    fn new() -> Self {
        Self {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn stream(&self, text: &str) -> StreamFn {
        gated_reply_stream(
            text.to_owned(),
            self.started.clone(),
            self.release.clone(),
            self.calls.clone(),
        )
    }

    /// Unblock every in-flight gated turn so it finishes normally.
    fn release(&self) {
        self.release.notify_waiters();
    }
}

/// Stream that signals start, emits one text delta, then waits for
/// abort/release (mirrors `side_chat_e2e::gated_reply_stream`).
fn gated_reply_stream(
    text: impl Into<String>,
    started: Arc<Notify>,
    release: Arc<Notify>,
    call_count: Arc<AtomicUsize>,
) -> StreamFn {
    let text = text.into();
    Arc::new(move |model, _context, options| {
        let text = text.clone();
        let started = started.clone();
        let release = release.clone();
        let call_count = call_count.clone();
        Box::pin(async move {
            call_count.fetch_add(1, Ordering::SeqCst);
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text(String::new()));
                producer
                    .push(AssistantMessageEvent::Start {
                        partial: message.clone(),
                    })
                    .await;
                producer
                    .push(AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: message.clone(),
                    })
                    .await;
                producer
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: text.clone(),
                        partial: message.clone(),
                    })
                    .await;
                started.notify_one();

                // Prefer abort signal when present; otherwise wait for test release.
                if let Some(abort) = options.stream.abort_signal.clone() {
                    tokio::select! {
                        _ = abort.cancelled() => {
                            message.stop_reason = StopReason::Aborted;
                            message.error_message = Some("aborted".into());
                            producer
                                .push(AssistantMessageEvent::Error {
                                    reason: StopReason::Aborted,
                                    error: message.clone(),
                                })
                                .await;
                            producer.end(Some(message)).await;
                        }
                        _ = release.notified() => {
                            if let Some(ContentBlock::Text { text: body, .. }) =
                                message.content.get_mut(0)
                            {
                                *body = text.clone();
                            }
                            message.stop_reason = StopReason::Stop;
                            producer
                                .push(AssistantMessageEvent::TextEnd {
                                    content_index: 0,
                                    content: text.clone(),
                                    partial: message.clone(),
                                })
                                .await;
                            producer
                                .push(AssistantMessageEvent::Done {
                                    reason: StopReason::Stop,
                                    message: message.clone(),
                                })
                                .await;
                            producer.end(Some(message)).await;
                        }
                    }
                } else {
                    release.notified().await;
                    if let Some(ContentBlock::Text { text: body, .. }) = message.content.get_mut(0)
                    {
                        *body = text.clone();
                    }
                    message.stop_reason = StopReason::Stop;
                    producer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                    producer.end(Some(message)).await;
                }
            });
            stream
        })
    })
}

fn faux_model(label: &str) -> Model {
    let mut model = Model::default();
    model.id = format!("{label}-model");
    model.name = format!("{label} Model");
    model.api = format!("{}-api", unique(label));
    model.provider = format!("{}-provider", unique(label));
    model.base_url = "http://localhost:0".into();
    model
}

fn session_options(model: Model, cwd: &std::path::Path, gate: &Gate) -> SessionOptions {
    SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(gate.stream(GATED_REPLY_TEXT)),
        auth_resolver: None,
    }
}

/// A recorded session whose session file EXISTS on disk (header persisted
/// immediately), so resume-catalog scans and path dedup see it.
async fn recorded_application_with(model: Model, gate: &Gate) -> (Application, String, TempDir) {
    let dir = tempfile::tempdir().expect("session dir");
    let session = Session::new(session_options(model.clone(), dir.path(), gate)).expect("session");
    session.set_session_dir(dir.path().to_path_buf());
    let recorder = pi_coding::start_session_in(
        dir.path(),
        Some(&model),
        Some("off"),
        Some(dir.path()),
        None,
        None,
    )
    .expect("start session");
    recorder.persist_now().expect("persist header");
    session.record(recorder).expect("record");
    let application = Application::new(session).await;
    let (id, _) = application.session().recorder_info().expect("recorder id");
    (application, id, dir)
}

/// Faux spawner sharing the test-controlled gate: every spawned child is an
/// independent recorded application whose turns stream through the gate. An
/// `Open` child claims the requested resume path as its session file
/// (exactly like the production spawner resuming in place).
#[derive(Clone)]
struct FauxSpawner {
    spawns: Arc<AtomicUsize>,
    model: Model,
    gate: Gate,
}

impl FauxSpawner {
    fn new(gate: &Gate) -> Self {
        Self {
            spawns: Arc::new(AtomicUsize::new(0)),
            model: faux_model("cs-child"),
            gate: gate.clone(),
        }
    }
}

impl SessionSpawner for FauxSpawner {
    fn spawn(
        &self,
        request: SessionSpawnRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<SessionSpawnResult, anyhow::Error>,
                > + Send,
        >,
    > {
        let this = self.clone();
        Box::pin(async move {
            this.spawns.fetch_add(1, Ordering::SeqCst);
            let resume_path = match &request.kind {
                SessionSpawnKind::Open { resume_path } => Some(resume_path.clone()),
                _ => None,
            };
            let (application, session_id, dir) =
                recorded_application_with(this.model.clone(), &this.gate).await;
            // The session file must outlive the spawn so catalog scans and
            // reopen-by-path can still find it.
            let _kept = dir.keep();
            let session_file = resume_path
                .or_else(|| application.session().recorder_info().map(|(_, path)| path));
            Ok(SessionSpawnResult {
                session_id,
                session_file,
                application,
                extension_ui: ExtensionUiAdapter::default(),
            })
        })
    }
}

struct Harness {
    handle: ListenHandle,
    addr: std::net::SocketAddr,
    gate: Gate,
    spawner: FauxSpawner,
    _primary_app: Application,
    _cwd: TempDir,
}

async fn harness(label: &str) -> Harness {
    let gate = Gate::new();
    let spawner = FauxSpawner::new(&gate);
    let model = faux_model(label);
    let cwd = tempfile::tempdir().expect("cwd");
    let session = Session::new(session_options(model, cwd.path(), &gate)).expect("session");
    let primary_app = Application::new(session).await;
    let kept_app = primary_app.clone();
    let extension_ui = ExtensionUiAdapter::new();
    extension_ui.set_canonical_queries_supported(true);
    let handle = start(
        primary_app,
        extension_ui.clone(),
        ListenConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token_file: None,
            allow_insecure_remote: false,
            advertised_origin: None,
            session_factory: Some(Arc::new(spawner.clone())),
        },
    )
    .await
    .expect("start listener");
    let addr = handle.local_addr();
    Harness {
        handle,
        addr,
        gate,
        spawner,
        _primary_app: kept_app,
        _cwd: cwd,
    }
}

async fn stop_harness(harness: Harness) {
    harness.gate.release();
    harness.handle.stop().await.expect("stop listener");
    harness._primary_app.cleanup().await;
}

async fn ws_connect_to(harness: &Harness) -> Ws {
    ws_connect(harness.addr, None).await
}

/// Send a command and wait for the response carrying the same id.
async fn rpc(ws: &mut Ws, command: &Value) -> Value {
    let id = command
        .get("id")
        .and_then(Value::as_str)
        .expect("command id")
        .to_owned();
    ws.send(WsMessage::Text(command.to_string().into()))
        .await
        .expect("send ws frame");
    wait_response(ws, &id).await
}

/// Wait for a response frame with the given id (deadline-bounded).
async fn wait_response(ws: &mut Ws, id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let frame = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let frame: Value = serde_json::from_str(&text).expect("parse ws frame");
                frame
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(error))) => panic!("WebSocket failed: {error}"),
            Ok(None) => panic!("WebSocket closed before response {id}"),
            Err(_) => panic!("timed out waiting for response {id}"),
        };
        if frame["type"] == "response" && frame.get("id").and_then(Value::as_str) == Some(id) {
            return frame;
        }
    }
}

/// Wait for any frame satisfying the predicate (deadline-bounded).
async fn wait_frame(ws: &mut Ws, mut predicate: impl FnMut(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let frame = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let frame: Value = serde_json::from_str(&text).expect("parse ws frame");
                frame
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(error))) => panic!("WebSocket failed: {error}"),
            Ok(None) => panic!("WebSocket closed before matching frame"),
            Err(_) => panic!("timed out waiting for matching frame"),
        };
        if predicate(&frame) {
            return frame;
        }
    }
}

/// Open a fresh child session over the wire; returns its sessionId.
async fn open_child(ws: &mut Ws, tag: &str) -> String {
    let response = rpc(
        ws,
        &json!({"type":"new_session","id":format!("ns-{tag}")}),
    )
    .await;
    assert!(response["success"].as_bool().unwrap_or(false), "{response}");
    response["data"]["sessionId"]
        .as_str()
        .expect("child sessionId")
        .to_owned()
}

/// The headline contract: new_session and child commands progress while the
/// primary is mid-prompt.
#[tokio::test]
async fn new_session_and_child_commands_progress_while_primary_is_prompting() {
    let harness = harness("cs-concurrent-prompt").await;
    let mut ws = ws_connect_to(&harness).await;

    // A's turn starts and blocks on the gate: provably in flight.
    let started = harness.gate.started.notified();
    ws.send(WsMessage::Text(
        json!({"type":"prompt","id":"slow-a","message":"slow on A","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send slow prompt");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("A stream must start");

    let b_id = open_child(&mut ws, "while-a").await;

    // A is still streaming while B is driven.
    let a_state = rpc(&mut ws, &json!({"type":"get_state","id":"a-state"})).await;
    assert!(a_state["success"].as_bool().unwrap_or(false), "{a_state}");
    assert_eq!(a_state["data"]["isStreaming"], json!(true), "A must still be streaming");

    let b_state = tokio::time::timeout(
        Duration::from_secs(2),
        rpc(
            &mut ws,
            &json!({"type":"get_state","id":"b-state","sessionId":b_id}),
        ),
    )
    .await
    .expect("B get_state must complete while A is prompting");
    assert!(b_state["success"].as_bool().unwrap_or(false), "{b_state}");
    assert_eq!(b_state["data"]["sessionId"].as_str(), Some(b_id.as_str()));

    // Release A; its no-recorder primary completes with an untagged message_end.
    harness.gate.release();
    let a_end = wait_frame(&mut ws, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_end")
            && frame.get("sessionId").is_none()
    })
    .await;
    assert_eq!(a_end["type"], "message_end");
    stop_harness(harness).await;
}

/// Every WS client receives every runtime's events tagged with the OWNING
/// session; dropping one client mid-stream does not close the session and
/// the other client keeps receiving.
#[tokio::test]
async fn fan_out_tags_events_and_ws_drop_does_not_close_the_session() {
    let harness = harness("cs-fanout").await;
    let mut ws1 = ws_connect_to(&harness).await;
    let mut ws2 = ws_connect_to(&harness).await;

    let b_id = open_child(&mut ws1, "fanout").await;

    // B's turn starts and blocks on the gate: provably in flight.
    let started = harness.gate.started.notified();
    ws1.send(WsMessage::Text(
        json!({"type":"prompt","id":"fan-prompt","sessionId":b_id,"message":"ping","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send prompt");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("B stream must start");

    // Both connections see B's tagged start event (fan-out while streaming).
    let on_ws1 = wait_frame(&mut ws1, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_start")
            && frame.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str())
    })
    .await;
    assert_eq!(on_ws1["sessionId"].as_str(), Some(b_id.as_str()));
    let on_ws2 = wait_frame(&mut ws2, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_start")
            && frame.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str())
    })
    .await;
    assert_eq!(on_ws2["sessionId"].as_str(), Some(b_id.as_str()));

    // B is still streaming when ws2 drops.
    let b_state = rpc(
        &mut ws1,
        &json!({"type":"get_state","id":"mid-stream","sessionId":b_id}),
    )
    .await;
    assert!(b_state["success"].as_bool().unwrap_or(false), "{b_state}");
    assert_eq!(b_state["data"]["isStreaming"], json!(true), "B must still be streaming");
    ws2.close(None).await.expect("close ws2");

    // ws1 keeps receiving B's stream to completion after the release.
    harness.gate.release();
    let end_on_ws1 = wait_frame(&mut ws1, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_end")
            && frame.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str())
    })
    .await;
    assert_eq!(end_on_ws1["sessionId"].as_str(), Some(b_id.as_str()));

    // The session was NOT closed by the drop.
    let alive = rpc(
        &mut ws1,
        &json!({"type":"get_state","id":"still-alive","sessionId":b_id}),
    )
    .await;
    assert!(alive["success"].as_bool().unwrap_or(false), "{alive}");
    let closed = rpc(
        &mut ws1,
        &json!({"type":"close_session","id":"drop-close","sessionId":b_id}),
    )
    .await;
    assert!(closed["success"].as_bool().unwrap_or(false), "{closed}");
    stop_harness(harness).await;
}

/// Commands route by sessionId with per-session state isolation; unknown ids
/// (commands AND lifecycle source ids) fail closed; absent ids target the
/// primary; switch_session dedups by canonical path.
#[tokio::test]
async fn commands_route_by_session_unknown_ids_fail_closed_and_switch_dedups() {
    let harness = harness("cs-routing").await;
    let mut ws = ws_connect_to(&harness).await;
    let b_id = open_child(&mut ws, "routing").await;

    let set_a = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"todos-a","phases":[{"name":"a-phase","tasks":[]}]}),
    )
    .await;
    assert!(set_a["success"].as_bool().unwrap_or(false), "{set_a}");
    let set_b = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"todos-b","sessionId":b_id,"phases":[{"name":"b-phase","tasks":[]}]}),
    )
    .await;
    assert!(set_b["success"].as_bool().unwrap_or(false), "{set_b}");

    let a_state = rpc(&mut ws, &json!({"type":"get_state","id":"state-a"})).await;
    assert!(a_state["success"].as_bool().unwrap_or(false), "{a_state}");
    let a_phases = a_state["data"]["todoPhases"].as_array().expect("a todoPhases");
    assert!(a_phases.iter().any(|phase| phase["name"] == "a-phase"));
    assert!(!a_phases.iter().any(|phase| phase["name"] == "b-phase"));

    let b_state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"state-b","sessionId":b_id}),
    )
    .await;
    assert!(b_state["success"].as_bool().unwrap_or(false), "{b_state}");
    let b_phases = b_state["data"]["todoPhases"].as_array().expect("b todoPhases");
    assert!(b_phases.iter().any(|phase| phase["name"] == "b-phase"));
    assert!(!b_phases.iter().any(|phase| phase["name"] == "a-phase"));

    // Unknown ids fail closed, for commands and lifecycle source ids alike.
    let unknown = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"unknown","sessionId":"no-such-session"}),
    )
    .await;
    assert!(!unknown["success"].as_bool().unwrap_or(true), "{unknown}");
    assert!(
        unknown["error"].as_str().unwrap_or("").contains("unknown session no-such-session"),
        "{unknown}"
    );
    let lifecycle_unknown = rpc(
        &mut ws,
        &json!({"type":"new_session","id":"ns-ghost","sessionId":"ghost-session"}),
    )
    .await;
    assert!(!lifecycle_unknown["success"].as_bool().unwrap_or(true), "{lifecycle_unknown}");
    assert!(
        lifecycle_unknown["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown session ghost-session"),
        "{lifecycle_unknown}"
    );

    // switch_session dedups by canonical path.
    let dir = tempfile::tempdir().expect("session dir");
    let recorder = pi_coding::start_session_in(
        dir.path(),
        Some(&faux_model("cs-dedup-file")),
        Some("off"),
        Some(dir.path()),
        None,
        None,
    )
    .expect("start session");
    recorder.persist_now().expect("persist");
    let path = recorder.path().to_string_lossy().into_owned();
    let first = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"switch-1","sessionPath":path}),
    )
    .await;
    assert!(first["success"].as_bool().unwrap_or(false), "{first}");
    let first_id = first["data"]["sessionId"].as_str().expect("first id").to_owned();
    let second = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"switch-2","sessionPath":path}),
    )
    .await;
    assert!(second["success"].as_bool().unwrap_or(false), "{second}");
    assert_eq!(
        second["data"]["sessionId"].as_str(),
        Some(first_id.as_str()),
        "dedup must return the same runtime"
    );
    assert_eq!(harness.spawner.spawns.load(Ordering::SeqCst), 2, "no second open");
    let missing = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"switch-missing","sessionPath":"/definitely/not/a/session.jsonl"}),
    )
    .await;
    assert!(!missing["success"].as_bool().unwrap_or(true), "{missing}");
    stop_harness(harness).await;
}

/// close_session rejects missing/unknown ids verbatim and every reachable
/// busy guard with its exact reason, WITHOUT cancelling the work.
#[tokio::test]
async fn close_session_rejects_busy_guards_without_cancelling_work() {
    let harness = harness("cs-busy").await;
    let mut ws = ws_connect_to(&harness).await;

    let no_sid = rpc(&mut ws, &json!({"type":"close_session","id":"close-none"})).await;
    assert!(!no_sid["success"].as_bool().unwrap_or(true), "{no_sid}");
    assert_eq!(
        no_sid["error"].as_str().unwrap_or(""),
        "close_session requires a sessionId"
    );
    let unknown = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"close-ghost","sessionId":"ghost-session"}),
    )
    .await;
    assert!(!unknown["success"].as_bool().unwrap_or(true), "{unknown}");
    assert_eq!(unknown["error"].as_str().unwrap_or(""), "unknown session ghost-session");

    // Guard: a turn is in progress.
    let turn_id = open_child(&mut ws, "busy-turn").await;
    let started = harness.gate.started.notified();
    ws.send(WsMessage::Text(
        json!({"type":"prompt","id":"busy-turn-prompt","sessionId":turn_id,"message":"busy","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send busy prompt");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("busy turn stream must start");
    let rejected = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"close-turn","sessionId":turn_id}),
    )
    .await;
    assert!(!rejected["success"].as_bool().unwrap_or(true), "{rejected}");
    assert_eq!(
        rejected["error"].as_str().unwrap_or(""),
        "session is busy: a turn is in progress"
    );
    // Work alive: the turn is STILL streaming after the rejected close.
    let streaming = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"turn-still-streaming","sessionId":turn_id}),
    )
    .await;
    assert!(streaming["success"].as_bool().unwrap_or(false), "{streaming}");
    assert_eq!(
        streaming["data"]["isStreaming"],
        json!(true),
        "rejected close must not cancel the turn"
    );
    // Release; the turn completes normally and the idle close succeeds.
    harness.gate.release();
    let turn_end = wait_frame(&mut ws, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_end")
            && frame.get("sessionId").and_then(Value::as_str) == Some(turn_id.as_str())
    })
    .await;
    assert_eq!(turn_end["sessionId"].as_str(), Some(turn_id.as_str()));
    let closed = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"close-turn-drained","sessionId":turn_id}),
    )
    .await;
    assert!(closed["success"].as_bool().unwrap_or(false), "{closed}");

    // Guard: supervised processes are running.
    let process_id = open_child(&mut ws, "busy-process").await;
    let spawned = rpc(
        &mut ws,
        &json!({"type":"process_spawn","id":"busy-spawn","sessionId":process_id,"spec":spawn_sleep_spec(harness._cwd.path(), 30)}),
    )
    .await;
    assert!(spawned["success"].as_bool().unwrap_or(false), "{spawned}");
    let pid = spawned["data"]["id"].as_str().expect("process id").to_owned();
    let rejected = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"close-process","sessionId":process_id}),
    )
    .await;
    assert!(!rejected["success"].as_bool().unwrap_or(true), "{rejected}");
    assert_eq!(
        rejected["error"].as_str().unwrap_or(""),
        "session is busy: supervised processes are running"
    );
    // Work alive: the process is still supervised.
    let listed = rpc(
        &mut ws,
        &json!({"type":"process_list","id":"busy-processes","sessionId":process_id}),
    )
    .await;
    assert!(listed["success"].as_bool().unwrap_or(false), "{listed}");
    assert!(
        listed["data"]
            .as_array()
            .map(|rows| rows.iter().any(|row| row["id"] == pid))
            .unwrap_or(false),
        "process must still be alive: {listed}"
    );
    let stopped = rpc(
        &mut ws,
        &json!({"type":"process_stop","id":"busy-stop","sessionId":process_id,"processId":pid}),
    )
    .await;
    assert!(stopped["success"].as_bool().unwrap_or(false), "{stopped}");

    // Guard: a side-chat turn is in progress.
    let side_id = open_child(&mut ws, "busy-side").await;
    let chat = rpc(
        &mut ws,
        &json!({"type":"side_chat_new","id":"busy-chat-new","sessionId":side_id,"name":"guard"}),
    )
    .await;
    assert!(chat["success"].as_bool().unwrap_or(false), "{chat}");
    let started = harness.gate.started.notified();
    let prompted = rpc(
        &mut ws,
        &json!({"type":"side_chat_prompt","id":"busy-chat-prompt","sessionId":side_id,"message":"side busy"}),
    )
    .await;
    assert!(prompted["success"].as_bool().unwrap_or(false), "{prompted}");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("side-chat stream must start");
    let rejected = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"close-side","sessionId":side_id}),
    )
    .await;
    assert!(!rejected["success"].as_bool().unwrap_or(true), "{rejected}");
    assert_eq!(
        rejected["error"].as_str().unwrap_or(""),
        "session is busy: a side-chat turn is in progress"
    );
    // Work alive: once released, the side-chat turn drains and close succeeds.
    harness.gate.release();
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut closed = None;
    while tokio::time::Instant::now() < deadline {
        let attempt = rpc(
            &mut ws,
            &json!({"type":"close_session","id":"close-side-drain","sessionId":side_id}),
        )
        .await;
        if attempt["success"].as_bool().unwrap_or(false) {
            closed = Some(attempt);
            break;
        }
    }
    assert!(closed.is_some(), "side-chat turn must drain, then close succeeds");
    stop_harness(harness).await;
}

/// The concurrent-session cap counts distinct runtimes (primary included),
/// rejects beyond the limit without evicting, and recovers after an idle
/// session is closed.
#[tokio::test]
async fn concurrent_session_cap_rejects_without_evicting_and_recovers_after_drain() {
    let harness = harness("cs-cap").await;
    let mut ws = ws_connect_to(&harness).await;

    let mut opened = Vec::new();
    for index in 0..(MAX_LOADED_SESSIONS - 1) {
        let response = rpc(
            &mut ws,
            &json!({"type":"new_session","id":format!("cap-open-{index}")}),
        )
        .await;
        assert!(response["success"].as_bool().unwrap_or(false), "{response}");
        opened.push(
            response["data"]["sessionId"]
                .as_str()
                .expect("sessionId")
                .to_owned(),
        );
    }
    let rejected = rpc(
        &mut ws,
        &json!({"type":"new_session","id":"cap-reject"}),
    )
    .await;
    assert!(!rejected["success"].as_bool().unwrap_or(true), "{rejected}");
    assert_eq!(
        rejected["error"].as_str().unwrap_or(""),
        "too many concurrent sessions (limit 8); close an idle session first"
    );

    // No eviction: the first child still routes.
    let existing = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"cap-existing","sessionId":opened[0]}),
    )
    .await;
    assert!(existing["success"].as_bool().unwrap_or(false), "{existing}");

    // Draining an idle session frees a slot.
    let closed = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"cap-close","sessionId":opened[0]}),
    )
    .await;
    assert!(closed["success"].as_bool().unwrap_or(false), "{closed}");
    let reopened = rpc(
        &mut ws,
        &json!({"type":"new_session","id":"cap-reopen"}),
    )
    .await;
    assert!(reopened["success"].as_bool().unwrap_or(false), "{reopened}");
    stop_harness(harness).await;
}
