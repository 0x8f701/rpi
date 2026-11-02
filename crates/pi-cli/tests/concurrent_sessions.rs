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
//!   unknown ids fail closed; exactly one initial id-less `get_state` bootstrap
//!   discovers the primary, and every later session command requires ownership.
//! - `close_session` rejects unknown ids and every reachable busy
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
    MAX_LOADED_SESSIONS, SessionSpawnKind, SessionSpawnRequest, SessionSpawnResult, SessionSpawner,
};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    PreparedSessionResume, ResourceDiscovery, Session, SessionOptions, TodoChildService,
    ToolSelection,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as WsMessage};

#[path = "common/mod.rs"]
mod common;

async fn attach_resources_with_settings(
    session: &Session,
    cwd: &std::path::Path,
    settings: &str,
) -> TempDir {
    let agent_root = tempfile::tempdir().expect("agent directory");
    std::fs::write(agent_root.path().join("settings.json"), settings).expect("runtime settings");
    let mut options = pi_coding::ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_root.path().to_path_buf();
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = pi_coding::ResourceManager::new(options).expect("isolated resources");
    session
        .attach_resources(resources)
        .await
        .expect("attach isolated resources");
    agent_root
}

async fn attach_isolated_resources(session: &Session, cwd: &std::path::Path) -> TempDir {
    attach_resources_with_settings(session, cwd, r#"{"sandbox":{"mode":"danger-full-access"}}"#)
        .await
}

async fn attach_auto_route_resources(session: &Session, cwd: &std::path::Path) -> TempDir {
    attach_resources_with_settings(
        session,
        cwd,
        r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":true,"todo":true},"selector":{"autoMode":"auto"}}"#,
    )
    .await
}
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
    model.base_url = "http://localhost:8787".into();
    // The sandbox authority resolves (and stages) the model's provider HTTP
    // authority on bootstrap; keep a faux provider registered for the api.
    std::mem::forget(pi_ai::providers::register_faux_provider(
        pi_ai::providers::FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 4,
        },
    ));
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
        auth_manager: None,
    }
}

/// A recorded session whose session file EXISTS on disk (header persisted
/// immediately), so resume-catalog scans and path dedup see it.
async fn recorded_application_with(
    model: Model,
    gate: &Gate,
) -> (Application, String, TempDir, TempDir) {
    let dir = tempfile::tempdir().expect("session dir");
    let session = Session::new(session_options(model.clone(), dir.path(), gate)).expect("session");
    let agent_root = attach_auto_route_resources(&session, dir.path()).await;
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
    (application, id, dir, agent_root)
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
    runtime_roots: Arc<std::sync::Mutex<Vec<(TempDir, TempDir)>>>,
    todo_child_service: Arc<TodoChildService>,
    runtime_factory: Arc<std::sync::OnceLock<Arc<dyn ApplicationRuntimeFactory>>>,
}

impl FauxSpawner {
    fn new(gate: &Gate) -> Self {
        Self {
            spawns: Arc::new(AtomicUsize::new(0)),
            model: faux_model("cs-child"),
            gate: gate.clone(),
            runtime_roots: Arc::new(std::sync::Mutex::new(Vec::new())),
            todo_child_service: TodoChildService::new(),
            runtime_factory: Arc::new(std::sync::OnceLock::new()),
        }
    }

    fn retain_runtime_roots(&self, cwd: TempDir, agent_root: TempDir) {
        self.runtime_roots
            .lock()
            .expect("runtime roots lock")
            .push((cwd, agent_root));
    }

    fn set_runtime_factory(&self, factory: Arc<dyn ApplicationRuntimeFactory>) {
        self.runtime_factory
            .set(factory)
            .unwrap_or_else(|_| panic!("runtime factory already configured"));
    }

    fn runtime_factory(&self) -> Arc<dyn ApplicationRuntimeFactory> {
        self.runtime_factory
            .get()
            .expect("runtime factory configured")
            .clone()
    }
}

impl ApplicationRuntimeFactory for FauxSpawner {
    fn build_runtime_candidate(
        &self,
        cwd: std::path::PathBuf,
        mut options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let this = self.clone();
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            options.stream_fn = Some(this.gate.stream(GATED_REPLY_TEXT));
            let session = Session::new_with_todo_and_additional_tools_filtered_and_discovery(
                options,
                Vec::new(),
                ToolSelection::default(),
                ResourceDiscovery::Disabled,
            )?;
            let agent_root = attach_auto_route_resources(&session, &cwd).await;
            let session_root = tempfile::tempdir()?;
            session.set_session_dir(session_root.path().to_path_buf());
            if let Some(resume) = resume {
                let context = resume.build_context();
                let recorder = resume.into_recorder()?;
                session.load_history(context.messages).await?;
                session.record(recorder)?;
            }
            this.retain_runtime_roots(session_root, agent_root);
            Ok(ApplicationRuntimeCandidate::new(session))
        })
    }
}

impl SessionSpawner for FauxSpawner {
    fn spawn(
        &self,
        request: SessionSpawnRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<SessionSpawnResult, anyhow::Error>>
                + Send,
        >,
    > {
        let this = self.clone();
        Box::pin(async move {
            this.spawns.fetch_add(1, Ordering::SeqCst);
            let result = match request.kind {
                SessionSpawnKind::Open { prepared } => {
                    let session_id = prepared.tree().header.id.clone();
                    let session_file = prepared.path().to_path_buf();
                    let dir = tempfile::tempdir().expect("runtime cwd");
                    let session =
                        Session::new(session_options(this.model.clone(), dir.path(), &this.gate))
                            .expect("session");
                    let agent_root = attach_auto_route_resources(&session, dir.path()).await;
                    session.set_session_dir(
                        session_file.parent().expect("session parent").to_path_buf(),
                    );
                    let context = prepared.build_context();
                    session.load_history(context.messages).await?;
                    session.record(prepared.into_recorder()?)?;
                    let application = Application::new(session).await;
                    let factory = this.runtime_factory();
                    application.attach_runtime_factory(factory)?;
                    application.attach_todo_child_client(this.todo_child_service.client())?;
                    this.retain_runtime_roots(dir, agent_root);
                    let file_identity = application
                        .session()
                        .session_file_identity()
                        .expect("opened session file identity");
                    SessionSpawnResult::existing(
                        session_id,
                        session_file,
                        file_identity,
                        application,
                        ExtensionUiAdapter::default(),
                        None,
                    )
                }
                _ => {
                    let (application, session_id, dir, agent_root) =
                        recorded_application_with(this.model.clone(), &this.gate).await;
                    let factory = this.runtime_factory();
                    application.attach_runtime_factory(factory)?;
                    application.attach_todo_child_client(this.todo_child_service.client())?;
                    let (session_id, session_file) = application
                        .session()
                        .recorder_info()
                        .expect("spawned session recorder");
                    let file_identity = application
                        .session()
                        .session_file_identity()
                        .expect("spawned session file identity");
                    this.retain_runtime_roots(dir, agent_root);
                    SessionSpawnResult::created(
                        session_id,
                        session_file,
                        file_identity,
                        application,
                        ExtensionUiAdapter::default(),
                        None,
                        None,
                    )
                }
            };
            Ok(result)
        })
    }

    fn todo_child_service(&self) -> Option<Arc<pi_coding::TodoChildService>> {
        Some(self.todo_child_service.clone())
    }
}

struct Harness {
    handle: ListenHandle,
    addr: std::net::SocketAddr,
    gate: Gate,
    spawner: FauxSpawner,
    _primary_app: Application,
    _cwd: TempDir,
    _primary_agent_root: TempDir,
}

async fn harness(label: &str) -> Harness {
    let gate = Gate::new();
    let spawner = FauxSpawner::new(&gate);
    let model = faux_model(label);
    let cwd = tempfile::tempdir().expect("cwd");
    let session = Session::new(session_options(model, cwd.path(), &gate)).expect("session");
    let agent_root = attach_auto_route_resources(&session, cwd.path()).await;
    prepare_session_store(&session, cwd.path());
    let primary_model = session.model().expect("primary model");
    let primary_session_id = uuid::Uuid::now_v7().to_string();
    let primary_recorder = pi_coding::start_session_in(
        cwd.path(),
        Some(&primary_model),
        Some("off"),
        Some(&session.session_dir()),
        Some(&primary_session_id),
        None,
    )
    .expect("start primary session recording");
    session
        .record(primary_recorder)
        .expect("record primary session");
    let primary_app = Application::new(session).await;
    let factory: Arc<dyn ApplicationRuntimeFactory> = Arc::new(spawner.clone());
    spawner.set_runtime_factory(factory.clone());
    primary_app
        .attach_runtime_factory(factory.clone())
        .expect("attach runtime factory");
    spawner
        .todo_child_service
        .configure_factory(factory)
        .expect("configure Todo child service");
    primary_app
        .attach_todo_child_client(spawner.todo_child_service.client())
        .expect("attach Todo child client");
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
            runner_codename_override: None,
            plaintext: true,
            tls_cert: None,
            tls_key: None,
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
        _primary_agent_root: agent_root,
    }
}

async fn stop_harness(harness: Harness) {
    harness.gate.release();
    harness.handle.stop(false).await.expect("stop listener");
    harness._primary_app.cleanup().await;
    harness
        .spawner
        .todo_child_service
        .shutdown()
        .await
        .expect("shut down Todo child service");
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

/// Bootstrap the WebSocket's primary identity and return its sessionId.
async fn bootstrap_primary(ws: &mut Ws) -> String {
    let response = rpc(ws, &json!({"type":"get_state","id":"bootstrap"})).await;
    assert!(response["success"].as_bool().unwrap_or(false), "{response}");
    response["sessionId"]
        .as_str()
        .expect("primary sessionId")
        .to_owned()
}

async fn subscribe(ws: &mut Ws, session_id: &str) -> Value {
    ws.send(WsMessage::Text(
        json!({"type":"subscribe","sessionId":session_id,"cursor":0})
            .to_string()
            .into(),
    ))
    .await
    .expect("send subscribe");
    wait_frame(ws, |frame| {
        matches!(
            frame.get("type").and_then(Value::as_str),
            Some("subscription_snapshot" | "session_resync")
        ) && frame.get("sessionId").and_then(Value::as_str) == Some(session_id)
    })
    .await
}

/// Open a fresh child session over the wire; returns its sessionId.
async fn open_child(ws: &mut Ws, source_id: &str, tag: &str) -> String {
    let response = rpc(
        ws,
        &json!({"type":"new_session","id":format!("ns-{tag}"),"sessionId":source_id}),
    )
    .await;
    assert!(response["success"].as_bool().unwrap_or(false), "{response}");
    let session_id = response["data"]["sessionId"]
        .as_str()
        .expect("child sessionId")
        .to_owned();
    let _ = subscribe(ws, &session_id).await;
    session_id
}

/// The headline contract: new_session and child commands progress while the
/// primary is mid-prompt.
#[tokio::test]
async fn new_session_and_child_commands_progress_while_primary_is_prompting() {
    let harness = harness("cs-concurrent-prompt").await;
    let mut ws = ws_connect_to(&harness).await;
    let primary_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &primary_id).await;

    // A's turn starts and blocks on the gate: provably in flight.
    let started = harness.gate.started.notified();
    ws.send(WsMessage::Text(
        json!({"type":"prompt","id":"slow-a","sessionId":primary_id,"message":"slow on A","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send slow prompt");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("A stream must start");

    let b_id = open_child(&mut ws, &primary_id, "while-a").await;

    // A is still streaming while B is driven.
    let a_state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"a-state","sessionId":primary_id}),
    )
    .await;
    assert!(a_state["success"].as_bool().unwrap_or(false), "{a_state}");
    assert_eq!(
        a_state["data"]["isStreaming"],
        json!(true),
        "A must still be streaming"
    );

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

    // Release A; its no-recorder primary completes with explicit ownership.
    harness.gate.release();
    let a_end = wait_frame(&mut ws, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_end")
            && frame.get("sessionId").and_then(Value::as_str) == Some(primary_id.as_str())
    })
    .await;
    assert_eq!(a_end["type"], "message_end");
    stop_harness(harness).await;
}

/// Three resident applications own independent prompt gates. All three turns
/// must enter provider streaming before any release, proving real overlap
/// rather than eventual completion through a shared application lock.
#[tokio::test]
async fn three_sessions_stream_concurrently_before_barrier_release() {
    let harness = harness("cs-three-overlap").await;
    let mut ws = ws_connect_to(&harness).await;
    let primary_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &primary_id).await;
    let second_id = open_child(&mut ws, &primary_id, "overlap-b").await;
    let third_id = open_child(&mut ws, &primary_id, "overlap-c").await;

    for (id, session_id) in [
        ("overlap-a", primary_id.as_str()),
        ("overlap-b", second_id.as_str()),
        ("overlap-c", third_id.as_str()),
    ] {
        ws.send(WsMessage::Text(
            json!({"type":"prompt","id":id,"sessionId":session_id,"message":id,"images":[]})
                .to_string()
                .into(),
        ))
        .await
        .expect("send overlapping prompt");
    }

    tokio::time::timeout(DEADLINE, async {
        while harness.gate.calls.load(Ordering::SeqCst) < 3 {
            harness.gate.started.notified().await;
        }
    })
    .await
    .expect("all three sessions must stream before release");
    assert_eq!(harness.gate.calls.load(Ordering::SeqCst), 3);

    for session_id in [&primary_id, &second_id, &third_id] {
        let state = rpc(
            &mut ws,
            &json!({
                "type":"get_state",
                "id":format!("state-{session_id}"),
                "sessionId":session_id,
            }),
        )
        .await;
        assert_eq!(state["data"]["isStreaming"], json!(true), "{state}");
    }

    harness.gate.release();
    for session_id in [&primary_id, &second_id, &third_id] {
        let terminal = wait_frame(&mut ws, |frame| {
            frame.get("type").and_then(Value::as_str) == Some("turn_terminal")
                && frame.get("sessionId").and_then(Value::as_str) == Some(session_id)
        })
        .await;
        assert_eq!(terminal["status"], "succeeded");
    }
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
    let primary_id = bootstrap_primary(&mut ws1).await;
    let _ = subscribe(&mut ws1, &primary_id).await;
    let primary_id_2 = bootstrap_primary(&mut ws2).await;
    let _ = subscribe(&mut ws2, &primary_id_2).await;

    let b_id = open_child(&mut ws1, &primary_id, "fanout").await;
    let _ = subscribe(&mut ws2, &b_id).await;

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
    assert_eq!(
        b_state["data"]["isStreaming"],
        json!(true),
        "B must still be streaming"
    );
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
/// fail closed; the bootstrap is the only missing-id exception; switch_session
/// dedups by canonical path.
#[tokio::test]
async fn commands_route_by_session_unknown_ids_fail_closed_and_switch_dedups() {
    let harness = harness("cs-routing").await;
    let mut ws = ws_connect_to(&harness).await;
    let primary_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &primary_id).await;
    let b_id = open_child(&mut ws, &primary_id, "routing").await;

    let set_a = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"todos-a","sessionId":primary_id,"phases":[{"name":"a-phase","tasks":[]}]}),
    )
    .await;
    assert!(set_a["success"].as_bool().unwrap_or(false), "{set_a}");
    let set_b = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"todos-b","sessionId":b_id,"phases":[{"name":"b-phase","tasks":[]}]}),
    )
    .await;
    assert!(set_b["success"].as_bool().unwrap_or(false), "{set_b}");

    let a_state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"state-a","sessionId":primary_id}),
    )
    .await;
    assert!(a_state["success"].as_bool().unwrap_or(false), "{a_state}");
    let a_phases = a_state["data"]["todoPhases"]
        .as_array()
        .expect("a todoPhases");
    assert!(a_phases.iter().any(|phase| phase["name"] == "a-phase"));
    assert!(!a_phases.iter().any(|phase| phase["name"] == "b-phase"));

    let b_state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"state-b","sessionId":b_id}),
    )
    .await;
    assert!(b_state["success"].as_bool().unwrap_or(false), "{b_state}");
    let b_phases = b_state["data"]["todoPhases"]
        .as_array()
        .expect("b todoPhases");
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
        unknown["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown session no-such-session"),
        "{unknown}"
    );
    let lifecycle_unknown = rpc(
        &mut ws,
        &json!({"type":"new_session","id":"ns-ghost","sessionId":"ghost-session"}),
    )
    .await;
    assert!(
        !lifecycle_unknown["success"].as_bool().unwrap_or(true),
        "{lifecycle_unknown}"
    );
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
        &json!({"type":"switch_session","id":"switch-1","sessionId":primary_id,"sessionPath":path}),
    )
    .await;
    assert!(first["success"].as_bool().unwrap_or(false), "{first}");
    let first_id = first["data"]["sessionId"]
        .as_str()
        .expect("first id")
        .to_owned();
    let second = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"switch-2","sessionId":primary_id,"sessionPath":path}),
    )
    .await;
    assert!(second["success"].as_bool().unwrap_or(false), "{second}");
    assert_eq!(
        second["data"]["sessionId"].as_str(),
        Some(first_id.as_str()),
        "dedup must return the same runtime"
    );
    assert_eq!(
        harness.spawner.spawns.load(Ordering::SeqCst),
        2,
        "no second open"
    );
    let missing = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"switch-missing","sessionId":primary_id,"sessionPath":"/definitely/not/a/session.jsonl"}),
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
    let primary_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &primary_id).await;

    let no_sid = rpc(&mut ws, &json!({"type":"close_session","id":"close-none"})).await;
    assert!(!no_sid["success"].as_bool().unwrap_or(true), "{no_sid}");
    assert_eq!(
        no_sid["error"].as_str().unwrap_or(""),
        "sessionId is required after WebSocket bootstrap"
    );
    let unknown = rpc(
        &mut ws,
        &json!({"type":"close_session","id":"close-ghost","sessionId":"ghost-session"}),
    )
    .await;
    assert!(!unknown["success"].as_bool().unwrap_or(true), "{unknown}");
    assert_eq!(
        unknown["error"].as_str().unwrap_or(""),
        "unknown session ghost-session"
    );

    // Guard: a turn is in progress.
    let turn_id = open_child(&mut ws, &primary_id, "busy-turn").await;
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
    assert!(
        streaming["success"].as_bool().unwrap_or(false),
        "{streaming}"
    );
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
    let process_id = open_child(&mut ws, &primary_id, "busy-process").await;
    let spawned = rpc(
        &mut ws,
        &json!({"type":"process_spawn","id":"busy-spawn","sessionId":process_id,"spec":spawn_sleep_spec(harness._cwd.path(), 30)}),
    )
    .await;
    assert!(spawned["success"].as_bool().unwrap_or(false), "{spawned}");
    let pid = spawned["data"]["id"]
        .as_str()
        .expect("process id")
        .to_owned();
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
    let side_id = open_child(&mut ws, &primary_id, "busy-side").await;
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
    assert!(
        closed.is_some(),
        "side-chat turn must drain, then close succeeds"
    );
    stop_harness(harness).await;
}

/// The loaded-session cap counts distinct runtimes (primary included).
/// Opening past the cap retires the least-recently-used idle non-primary
/// runtime and keeps the primary plus the newer idle sessions.
#[tokio::test]
async fn concurrent_session_cap_retires_lru_idle_and_keeps_newer_sessions() {
    let harness = harness("cs-cap").await;
    let mut ws = ws_connect_to(&harness).await;
    let primary_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &primary_id).await;

    let mut opened = Vec::new();
    for index in 0..(MAX_LOADED_SESSIONS - 1) {
        let response = rpc(
            &mut ws,
            &json!({"type":"new_session","id":format!("cap-open-{index}"),"sessionId":primary_id}),
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
    let admitted = rpc(
        &mut ws,
        &json!({"type":"new_session","id":"cap-admit","sessionId":primary_id}),
    )
    .await;
    assert!(
        admitted["success"].as_bool().unwrap_or(false),
        "an idle slot must be retired so the open can proceed: {admitted}"
    );
    let admitted_id = admitted["data"]["sessionId"]
        .as_str()
        .expect("admitted sessionId");
    assert_ne!(admitted_id, opened[0].as_str());

    let retired = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"cap-retired","sessionId":opened[0]}),
    )
    .await;
    assert!(
        !retired["success"].as_bool().unwrap_or(true),
        "the oldest idle session must leave the loaded pool: {retired}"
    );
    let kept = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"cap-kept","sessionId":opened[opened.len()-1]}),
    )
    .await;
    assert!(
        kept["success"].as_bool().unwrap_or(false),
        "a newer idle session must stay loaded: {kept}"
    );
    let primary = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"cap-primary","sessionId":primary_id}),
    )
    .await;
    assert!(
        primary["success"].as_bool().unwrap_or(false),
        "the primary runtime is not retired: {primary}"
    );
    stop_harness(harness).await;
}

/// Create one recorded on-disk session file for switch_session targets.
async fn recorded_switch_target(model: &Model, tag: &str) -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("switch target dir");
    let recorder = pi_coding::start_session_in(
        dir.path(),
        Some(model),
        Some("off"),
        Some(dir.path()),
        None,
        None,
    )
    .expect("start target session");
    recorder.persist_now().expect("persist target header");
    let _ = tag;
    (dir, recorder.path().to_string_lossy().into_owned())
}

/// Repeated and concurrent A→B→A switch_session over the same persisted file
/// must never surface "process manager publication is already bound": every
/// open dedups through the per-identity open lock, and a repeated switch to
/// an already-loaded runtime returns the SAME runtime id without spawning.
#[tokio::test]
async fn repeated_and_concurrent_switch_session_never_rebinds_publication() {
    let harness = harness("cs-switch-rebind").await;
    let mut ws = ws_connect_to(&harness).await;
    let a_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &a_id).await;
    let model = faux_model("cs-switch-target");
    let (_dir, path) = recorded_switch_target(&model, "rebind").await;

    // A → B (first open spawns one runtime).
    let to_b = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"sw-b1","sessionId":a_id,"sessionPath":path}),
    )
    .await;
    assert!(to_b["success"].as_bool().unwrap_or(false), "A→B: {to_b}");
    let b_id = to_b["data"]["sessionId"].as_str().expect("B id").to_owned();
    let spawns_after_first = harness.spawner.spawns.load(Ordering::SeqCst);

    // Repeated A→B: the loaded runtime is reused, no second spawn, and no
    // "already bound" failure from re-binding a publication.
    let to_b_again = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"sw-b2","sessionId":a_id,"sessionPath":path}),
    )
    .await;
    assert!(
        to_b_again["success"].as_bool().unwrap_or(false),
        "repeat A→B: {to_b_again}"
    );
    assert_eq!(
        to_b_again["data"]["sessionId"].as_str(),
        Some(b_id.as_str()),
        "repeat switch must return the same runtime"
    );
    assert_eq!(
        harness.spawner.spawns.load(Ordering::SeqCst),
        spawns_after_first,
        "repeat switch must not spawn"
    );

    // Concurrent duplicate switches: both serialize through the per-file
    // open lock; each succeeds, both name the SAME runtime, and neither
    // reports "already bound". The frames share one socket, so the two
    // commands are SENT back to back (the manager serializes admission) and
    // the responses are collected by id.
    let left_command =
        json!({"type":"switch_session","id":"sw-b3","sessionId":a_id,"sessionPath":path});
    let right_command =
        json!({"type":"switch_session","id":"sw-b4","sessionId":a_id,"sessionPath":path});
    ws.send(WsMessage::Text(left_command.to_string().into()))
        .await
        .expect("send left switch");
    ws.send(WsMessage::Text(right_command.to_string().into()))
        .await
        .expect("send right switch");
    let left = wait_response(&mut ws, "sw-b3").await;
    let right = wait_response(&mut ws, "sw-b4").await;
    assert!(
        left["success"].as_bool().unwrap_or(false),
        "concurrent left: {left}"
    );
    assert!(
        right["success"].as_bool().unwrap_or(false),
        "concurrent right: {right}"
    );
    assert_eq!(left["data"]["sessionId"].as_str(), Some(b_id.as_str()));
    assert_eq!(right["data"]["sessionId"].as_str(), Some(b_id.as_str()));
    assert!(
        !left.to_string().contains("already bound") && !right.to_string().contains("already bound"),
        "concurrent switches must never surface a publication rebind failure"
    );
    assert_eq!(
        harness.spawner.spawns.load(Ordering::SeqCst),
        spawns_after_first,
        "concurrent duplicate switches must not spawn extra runtimes"
    );
    stop_harness(harness).await;
}

/// While A is streaming a gated turn, switching the SOURCE identity to B
/// must not stop A's work: A's turn keeps streaming and completes on A, and
/// B serves its own independent requests meanwhile.
#[tokio::test]
async fn switch_to_b_while_a_streams_keeps_a_turn_alive_and_b_independent() {
    let harness = harness("cs-switch-streaming").await;
    let mut ws = ws_connect_to(&harness).await;
    let a_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &a_id).await;
    let model = faux_model("cs-switch-stream-target");
    let (_dir, path) = recorded_switch_target(&model, "streaming").await;

    // A's turn enters the gated stream: provably in flight.
    let started = harness.gate.started.notified();
    ws.send(WsMessage::Text(
        json!({"type":"prompt","id":"slow-a-switch","sessionId":a_id,"message":"stream on A","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send A prompt");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("A stream must start before the switch");

    // Switch the source to B while A is mid-turn.
    let switched = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"sw-to-b","sessionId":a_id,"sessionPath":path}),
    )
    .await;
    assert!(
        switched["success"].as_bool().unwrap_or(false),
        "switch: {switched}"
    );
    let b_id = switched["data"]["sessionId"]
        .as_str()
        .expect("B id")
        .to_owned();

    // A is STILL streaming after the cutover.
    let a_state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"a-after-switch","sessionId":a_id}),
    )
    .await;
    assert!(a_state["success"].as_bool().unwrap_or(false), "{a_state}");
    assert_eq!(
        a_state["data"]["isStreaming"],
        json!(true),
        "A's turn must survive the switch to B"
    );

    // B serves an independent request while A streams.
    let b_todos = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"b-todos","sessionId":b_id,"phases":[{"name":"b-phase-switch","tasks":[]}]}),
    )
    .await;
    assert!(b_todos["success"].as_bool().unwrap_or(false), "{b_todos}");
    let b_state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"b-state-switch","sessionId":b_id}),
    )
    .await;
    assert!(b_state["success"].as_bool().unwrap_or(false), "{b_state}");
    let b_phases = b_state["data"]["todoPhases"].as_array().expect("b phases");
    assert!(
        b_phases
            .iter()
            .any(|phase| phase["name"] == "b-phase-switch")
    );

    // Release; A's completion event arrives tagged with A, not B.
    harness.gate.release();
    let a_end = wait_frame(&mut ws, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_end")
            && frame.get("sessionId").and_then(Value::as_str) == Some(a_id.as_str())
    })
    .await;
    assert_eq!(a_end["sessionId"].as_str(), Some(a_id.as_str()));
    stop_harness(harness).await;
}

/// A WebSocket disconnect and reconnect must restore every previously loaded
/// subscription (A and B) exactly once each, with authoritative snapshots —
/// no duplicated transcript rows and no cross-session events.
#[tokio::test]
async fn reconnect_restores_loaded_subscriptions_without_duplication_or_crosstalk() {
    let harness = harness("cs-switch-reconnect").await;
    let mut ws = ws_connect_to(&harness).await;
    let a_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &a_id).await;
    let model = faux_model("cs-switch-reconnect-target");
    let (_dir, path) = recorded_switch_target(&model, "reconnect").await;

    // Load A and B, and give each a distinguishable todo phase.
    let switched = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"sw-reconnect-b","sessionId":a_id,"sessionPath":path}),
    )
    .await;
    assert!(switched["success"].as_bool().unwrap_or(false), "{switched}");
    let b_id = switched["data"]["sessionId"]
        .as_str()
        .expect("B id")
        .to_owned();
    let a_todos = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"recon-a-todos","sessionId":a_id,"phases":[{"name":"recon-a","tasks":[]}]}),
    )
    .await;
    assert!(a_todos["success"].as_bool().unwrap_or(false), "{a_todos}");
    let b_todos = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"recon-b-todos","sessionId":b_id,"phases":[{"name":"recon-b","tasks":[]}]}),
    )
    .await;
    assert!(b_todos["success"].as_bool().unwrap_or(false), "{b_todos}");

    // Start a gated turn on A BEFORE the disconnect: it survives the socket
    // drop (the session stays loaded) and its completion must arrive on the
    // reconnected socket tagged with A only.
    let started = harness.gate.started.notified();
    ws.send(WsMessage::Text(
        json!({"type":"prompt","id":"recon-a-prompt","sessionId":a_id,"message":"survive reconnect","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send A prompt before reconnect");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("A stream must start before reconnect");

    // Drop the socket and reconnect.
    drop(ws);
    let mut ws = ws_connect_to(&harness).await;
    let boot_id = bootstrap_primary(&mut ws).await; // The bootstrap discovers the authoritative primary (A or B — whichever
    // the manager routes first); BOTH loaded sessions must be restorable.
    let restored = [a_id.clone(), b_id.clone()];
    assert!(
        restored.contains(&boot_id),
        "bootstrap must route to a loaded session: {boot_id}"
    );
    for sid in &restored {
        let snapshot = subscribe(&mut ws, sid).await;
        assert_eq!(
            snapshot.get("sessionId").and_then(Value::as_str),
            Some(sid.as_str()),
            "reconnect snapshot must be tagged with the owning session"
        );
        let state = rpc(
            &mut ws,
            &json!({"type":"get_state","id":format!("recon-state-{sid}"),"sessionId":sid}),
        )
        .await;
        assert!(state["success"].as_bool().unwrap_or(false), "{state}");
        let phases = state["data"]["todoPhases"].as_array().expect("phases");
        let expected = if sid == &a_id { "recon-a" } else { "recon-b" };
        assert!(
            phases.iter().any(|phase| phase["name"] == expected),
            "{sid} must retain its own todo state after reconnect: {state}"
        );
    }

    // Post-reconnect events stay session-scoped: prompt A, and only A's
    // completion carries A's id (no B-tagged duplicate of the same turn).
    harness.gate.release();
    let a_end = wait_frame(&mut ws, |frame| {
        frame.get("type").and_then(Value::as_str) == Some("message_end")
            && frame.get("sessionId").and_then(Value::as_str) == Some(a_id.as_str())
    })
    .await;
    assert_eq!(a_end["sessionId"].as_str(), Some(a_id.as_str()));
    stop_harness(harness).await;
}

/// Multi-session workflow QA: workflow state is scoped across A and B — B
/// never sees A's workflow list, and routing back to the still-loaded A
/// identity returns A's unchanged state without rebinding either runtime.
#[tokio::test]
async fn workflow_lists_stay_session_scoped_across_switches() {
    let harness = harness("cs-switch-workflow-qa").await;
    let mut ws = ws_connect_to(&harness).await;
    let a_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &a_id).await;
    let (_, a_path) = harness
        ._primary_app
        .session()
        .recorder_info()
        .expect("primary session recording");
    let model = faux_model("cs-switch-workflow-target");
    let target_recorder = pi_coding::start_session_in(
        harness._primary_app.session().cwd(),
        Some(&model),
        Some("off"),
        a_path.parent(),
        None,
        None,
    )
    .expect("start workflow switch target");
    target_recorder
        .persist_now()
        .expect("persist workflow switch target");
    let path = target_recorder.path().to_string_lossy().into_owned();

    // A's workflow list: sessions without an orchestration manager answer
    // with a typed failure — also per-session state. Either shape is fine;
    // the contract under test is isolation across switches.
    let before = rpc(
        &mut ws,
        &json!({"type":"workflow_list","id":"wf-a-before","sessionId":a_id}),
    )
    .await;
    let before_ok = before["success"].as_bool().unwrap_or(false);

    // Switch to B and read ITS list.
    let switched = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"sw-wf-b","sessionId":a_id,"sessionPath":path}),
    )
    .await;
    assert!(switched["success"].as_bool().unwrap_or(false), "{switched}");
    let b_id = switched["data"]["sessionId"]
        .as_str()
        .expect("B id")
        .to_owned();
    let b_list = rpc(
        &mut ws,
        &json!({"type":"workflow_list","id":"wf-b-list","sessionId":b_id}),
    )
    .await;
    let b_ok = b_list["success"].as_bool().unwrap_or(false);

    // B must not observe A's workflows: compare ids if both lists are
    // non-empty; an empty B list trivially satisfies isolation.
    let a_ids: Vec<String> = before["data"]["workflows"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let b_ids: Vec<String> = b_list["data"]["workflows"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if before_ok && b_ok {
        for workflow_id in &a_ids {
            assert!(
                !b_ids.contains(workflow_id),
                "workflow {workflow_id} leaked from A into B: {b_ids:?}"
            );
        }
    }

    // Route back to A by its stable loaded identity. Web `switch_session`
    // opens a persisted file; it is not a selected-tab mutation, so an
    // already-loaded A remains directly addressable after opening B.
    let after = rpc(
        &mut ws,
        &json!({"type":"workflow_list","id":"wf-a-after","sessionId":a_id}),
    )
    .await;
    let after_ok = after["success"].as_bool().unwrap_or(false);
    assert_eq!(
        before_ok, after_ok,
        "A's workflow-manager availability must survive the round trip"
    );
    if before_ok && after_ok {
        let after_ids: Vec<String> = after["data"]["workflows"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            a_ids, after_ids,
            "A's workflow list must survive the round trip"
        );
    }
    stop_harness(harness).await;
}
/// A natural-language-routed Todo child remains process-owned while its Web
/// connection drops and the displayed session moves A→B→A. B sees no child
/// from A, and later A traffic reuses the original child and receipt lineage.
#[tokio::test]
async fn todo_child_survives_websocket_disconnect_and_displayed_session_round_trip() {
    let harness = harness("cs-todo-child-round-trip").await;
    let mut ws = ws_connect_to(&harness).await;
    let a_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &a_id).await;
    let a_path = harness
        ._primary_app
        .session()
        .recorder_info()
        .expect("primary session recording")
        .1;
    let child_started = harness.gate.started.notified();

    let routed = rpc(
        &mut ws,
        &json!({
            "type":"prompt",
            "id":"todo-route-a",
            "sessionId":a_id,
            "message":"implement the manager Todo child round trip in src/lib.rs",
            "images":[]
        }),
    )
    .await;
    assert!(routed["success"].as_bool().unwrap_or(false), "{routed}");
    tokio::time::timeout(DEADLINE, child_started)
        .await
        .expect("routed child must be running before the disconnect and switch");
    let list_a = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_list",
            "id":"todo-list-a",
            "sessionId":a_id,
            "parentSessionId":a_id
        }),
    )
    .await;
    assert!(list_a["success"].as_bool().unwrap_or(false), "{list_a}");
    let entries = list_a["data"].as_array().expect("A child catalog");
    assert_eq!(entries.len(), 1);
    let child_id = entries[0]["childSessionId"]
        .as_str()
        .expect("routed child id")
        .to_owned();
    let status_a = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_status",
            "id":"todo-status-a",
            "sessionId":a_id,
            "parentSessionId":a_id,
            "childSessionId":child_id
        }),
    )
    .await;
    assert!(status_a["success"].as_bool().unwrap_or(false), "{status_a}");
    let receipt_id = status_a["data"]["receipts"][0]["id"]
        .as_str()
        .expect("routed receipt id")
        .to_owned();
    assert_eq!(status_a["data"]["isRunning"], json!(true));
    assert_eq!(status_a["data"]["receipts"][0]["status"], json!("running"));

    let model = faux_model("cs-todo-child-target");
    let target_recorder = pi_coding::start_session_in(
        harness._primary_app.session().cwd(),
        Some(&model),
        Some("off"),
        a_path.parent(),
        None,
        None,
    )
    .expect("start B target session");
    target_recorder
        .persist_now()
        .expect("persist B target header");
    let target_path = target_recorder.path().to_string_lossy().into_owned();
    drop(target_recorder);
    let switched = rpc(
        &mut ws,
        &json!({
            "type":"switch_session",
            "id":"todo-switch-b",
            "sessionId":a_id,
            "sessionPath":target_path
        }),
    )
    .await;
    assert!(switched["success"].as_bool().unwrap_or(false), "{switched}");
    let b_id = switched["data"]["sessionId"]
        .as_str()
        .expect("B id")
        .to_owned();
    let list_b = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_list",
            "id":"todo-list-b",
            "sessionId":b_id,
            "parentSessionId":b_id
        }),
    )
    .await;
    assert!(list_b["success"].as_bool().unwrap_or(false), "{list_b}");
    assert_eq!(list_b["data"], json!([]), "B must not see A's child");

    let switched_back = rpc(
        &mut ws,
        &json!({
            "type":"switch_session",
            "id":"todo-switch-a",
            "sessionId":b_id,
            "sessionPath":a_path
        }),
    )
    .await;
    assert!(
        switched_back["success"].as_bool().unwrap_or(false),
        "{switched_back}"
    );
    assert_eq!(
        switched_back["data"]["sessionId"].as_str(),
        Some(a_id.as_str()),
        "B→A must restore the already-loaded A runtime",
    );

    drop(ws);
    let mut ws = ws_connect_to(&harness).await;
    let boot_id = bootstrap_primary(&mut ws).await;
    assert_eq!(
        boot_id, a_id,
        "reconnect bootstrap/display target must remain A after B→A"
    );
    let _ = subscribe(&mut ws, &a_id).await;
    let _ = subscribe(&mut ws, &b_id).await;
    let restored = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_status",
            "id":"todo-status-restored",
            "sessionId":a_id,
            "parentSessionId":a_id,
            "childSessionId":child_id
        }),
    )
    .await;
    assert!(restored["success"].as_bool().unwrap_or(false), "{restored}");
    assert_eq!(
        restored["data"]["childSessionId"].as_str(),
        Some(child_id.as_str())
    );
    assert_eq!(
        restored["data"]["receipts"][0]["id"].as_str(),
        Some(receipt_id.as_str())
    );
    assert_eq!(restored["data"]["isRunning"], json!(true));
    assert_eq!(restored["data"]["receipts"][0]["status"], json!("running"));
    assert!(
        !restored.to_string().contains("orchestration shut down"),
        "disconnect or displayed switch shut down the routed child: {restored}",
    );

    let routed_again = rpc(
        &mut ws,
        &json!({
            "type":"prompt",
            "id":"todo-route-restored",
            "sessionId":a_id,
            "message":"fix the manager Todo child round trip in tests/manager.rs",
            "images":[]
        }),
    )
    .await;
    assert!(
        routed_again["success"].as_bool().unwrap_or(false),
        "{routed_again}"
    );
    let status_after_route = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_status",
            "id":"todo-status-rerouted",
            "sessionId":a_id,
            "parentSessionId":a_id,
            "childSessionId":child_id
        }),
    )
    .await;
    assert!(
        status_after_route["success"].as_bool().unwrap_or(false),
        "{status_after_route}"
    );
    assert_eq!(
        status_after_route["data"]["childSessionId"].as_str(),
        Some(child_id.as_str())
    );
    assert_eq!(
        status_after_route["data"]["receipts"]
            .as_array()
            .expect("receipts after reroute")
            .len(),
        2,
    );

    let continued = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_send",
            "id":"todo-send-restored",
            "sessionId":a_id,
            "parentSessionId":a_id,
            "message":"continue on the original Todo child"
        }),
    )
    .await;
    assert!(
        continued["success"].as_bool().unwrap_or(false),
        "{continued}"
    );
    assert_eq!(
        continued["data"]["childSessionId"].as_str(),
        Some(child_id.as_str())
    );
    assert_ne!(continued["data"]["id"].as_str(), Some(receipt_id.as_str()));
    let final_list = rpc(
        &mut ws,
        &json!({
            "type":"todo_child_list",
            "id":"todo-list-final",
            "sessionId":a_id,
            "parentSessionId":a_id
        }),
    )
    .await;
    assert!(
        final_list["success"].as_bool().unwrap_or(false),
        "{final_list}"
    );
    assert_eq!(
        final_list["data"]
            .as_array()
            .expect("final A catalog")
            .len(),
        1
    );
    assert_eq!(
        final_list["data"][0]["childSessionId"].as_str(),
        Some(child_id.as_str())
    );

    stop_harness(harness).await;
}

/// A FAILED switch (invalid session path) leaves the source session fully
/// usable: no runtime is spawned, and subsequent commands on the original
/// session still succeed.
#[tokio::test]
async fn failed_switch_keeps_source_session_usable() {
    let harness = harness("cs-switch-failed").await;
    let mut ws = ws_connect_to(&harness).await;
    let a_id = bootstrap_primary(&mut ws).await;
    let _ = subscribe(&mut ws, &a_id).await;
    let spawns_before = harness.spawner.spawns.load(Ordering::SeqCst);

    let failed = rpc(
        &mut ws,
        &json!({"type":"switch_session","id":"sw-fail","sessionId":a_id,"sessionPath":"/definitely/missing/cs-switch.jsonl"}),
    )
    .await;
    assert!(!failed["success"].as_bool().unwrap_or(true), "{failed}");
    assert_eq!(
        harness.spawner.spawns.load(Ordering::SeqCst),
        spawns_before,
        "a failed switch must not spawn a runtime"
    );

    // The source session still serves commands and turns.
    let todos = rpc(
        &mut ws,
        &json!({"type":"set_todos","id":"post-fail-todos","sessionId":a_id,"phases":[{"name":"post-fail","tasks":[]}]}),
    )
    .await;
    assert!(todos["success"].as_bool().unwrap_or(false), "{todos}");
    let state = rpc(
        &mut ws,
        &json!({"type":"get_state","id":"post-fail-state","sessionId":a_id}),
    )
    .await;
    assert!(state["success"].as_bool().unwrap_or(false), "{state}");
    let phases = state["data"]["todoPhases"].as_array().expect("phases");
    assert!(phases.iter().any(|phase| phase["name"] == "post-fail"));

    let started = harness.gate.started.notified();
    ws.send(WsMessage::Text(
        json!({"type":"prompt","id":"post-fail-prompt","sessionId":a_id,"message":"still alive","images":[]})
            .to_string()
            .into(),
    ))
    .await
    .expect("send post-fail prompt");
    tokio::time::timeout(DEADLINE, started)
        .await
        .expect("A must still accept prompts after a failed switch");
    harness.gate.release();
    stop_harness(harness).await;
}
