//! Wire contracts for `session_move` on the Web control plane.
//!
//! Drives the real `modes::listen` server over WebSocket against a hermetic
//! catalog fixture. An unloaded native saved row moves to a new working
//! directory through the catalog authority; the move response carries the
//! bounded outcome plus the destination catalog row and never spawns or
//! registers a runtime. After a listener RESTART the fresh catalog authority
//! serves only the moved world: the explicit `switch_session` opens the moved
//! file with its preserved identity, while the stale source path fails
//! closed with a bounded error (no local-path echo).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use pi_agent::ThinkingLevel;
use pi_cli::modes::listen::{ListenConfig, ListenHandle, start};
use pi_cli::modes::session_runtime_manager::{
    SessionSpawnKind, SessionSpawnRequest, SessionSpawnResult, SessionSpawner,
};
use pi_coding::{
    Application, ResourceDiscovery, Session, SessionOptions, TodoChildService, ToolSelection,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as WsMessage};

#[path = "common/mod.rs"]
mod common;
use common::*;

use tokio::net::TcpStream;
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Spawner for the wire test: only saved-row opens participate (switch), and
/// the manager's catalog authority is the hermetic fixture catalog.
#[derive(Clone)]
struct WireSpawner {
    spawns: Arc<AtomicUsize>,
    opened: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    catalog: pi_coding::SessionCatalog,
    model: pi_ai::Model,
    _registration: pi_ai::providers::FauxProviderRegistration,
    todo: Arc<TodoChildService>,
}

impl WireSpawner {
    fn new(catalog: pi_coding::SessionCatalog) -> Self {
        let (model, registration) = faux_model("smw-child");
        Self {
            spawns: Arc::new(AtomicUsize::new(0)),
            opened: Arc::new(std::sync::Mutex::new(Vec::new())),
            catalog,
            model,
            _registration: registration,
            todo: TodoChildService::new(),
        }
    }
}

impl SessionSpawner for WireSpawner {
    fn spawn(
        &self,
        request: SessionSpawnRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = std::result::Result<SessionSpawnResult, anyhow::Error>> + Send>> {
        let this = self.clone();
        Box::pin(async move {
            this.spawns.fetch_add(1, Ordering::SeqCst);
            let SessionSpawnKind::Open { prepared } = request.kind else {
                anyhow::bail!("session_move wire test only opens saved rows");
            };
            let resume_path = prepared.path().to_path_buf();
            let session_id = prepared.tree().header.id.clone();
            this.opened
                .lock()
                .expect("opened sessions")
                .push(resume_path.clone());
            let dir = tempfile::tempdir()?;
            let (session, agent_root) = isolated_session(this.model.clone(), dir.path()).await;
            session.set_session_dir(resume_path.parent().expect("session parent").to_path_buf());
            let context = prepared.build_context();
            session
                .resume_recording(prepared.into_recorder()?, context.messages)
                .await?;
            let application = Application::new(session).await;
            let file_identity = application
                .session()
                .session_file_identity()
                .ok_or_else(|| anyhow::anyhow!("opened session has no file identity"))?;
            std::mem::forget(agent_root);
            Ok(SessionSpawnResult::existing(
                session_id,
                resume_path,
                file_identity,
                application,
                pi_cli::extension_ui::ExtensionUiAdapter::default(),
                None,
            ))
        })
    }

    fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
        Some(self.catalog.clone())
    }

    fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
        Some(self.todo.clone())
    }
}

/// Isolated recorded-capable session with a danger-full-access sandbox and
/// no ambient resources (mirrors the concurrent-session wire fixtures).
async fn isolated_session(model: pi_ai::Model, cwd: &Path) -> (Session, TempDir) {
    let session = Session::new_with_todo_and_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("isolated session");
    let agent_root = tempfile::tempdir().expect("agent directory");
    std::fs::write(
        agent_root.path().join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("sandbox settings");
    let mut options = pi_coding::ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_root.path().to_path_buf();
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(
            pi_coding::ResourceManager::new(options).expect("isolated resources"),
        )
        .await
        .expect("attach isolated resources");
    (session, agent_root)
}

struct Listener {
    handle: ListenHandle,
    addr: std::net::SocketAddr,
    spawner: Arc<WireSpawner>,
    _primary: FauxApp,
}

/// Start one listener with a fresh faux primary and a spawner sharing the
/// hermetic catalog authority.
async fn start_listener(catalog: pi_coding::SessionCatalog, label: &str) -> Listener {
    let spawner = Arc::new(WireSpawner::new(catalog));
    let primary = faux_application(label).await;
    let extension_ui = pi_cli::extension_ui::ExtensionUiAdapter::new();
    extension_ui.set_canonical_queries_supported(true);
    let handle = start(
        primary.application.clone(),
        extension_ui,
        ListenConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token_file: None,
            allow_insecure_remote: false,
            advertised_origin: None,
            runner_codename_override: None,
            plaintext: true,
            tls_cert: None,
            tls_key: None,
            session_factory: Some(spawner.clone()),
        },
    )
    .await
    .expect("start listener");
    let addr = handle.local_addr();
    Listener {
        handle,
        addr,
        spawner,
        _primary: primary,
    }
}

async fn stop_listener(listener: Listener) {
    listener.handle.stop(false).await.expect("stop listener");
    listener._primary.application.cleanup().await;
}

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

async fn wait_response(ws: &mut Ws, id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let frame: Value = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                serde_json::from_str(&text).expect("parse ws frame")
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

/// Bootstrap the WebSocket's primary protocol identity.
async fn bootstrap_primary(ws: &mut Ws) -> String {
    let response = rpc(ws, &json!({"type":"get_state","id":"bootstrap"})).await;
    assert!(
        response["success"].as_bool().unwrap_or(false),
        "{response}"
    );
    response["sessionId"]
        .as_str()
        .expect("primary sessionId")
        .to_owned()
}

fn move_command(id: &str, session_path: &Path, target_cwd: &Path) -> Value {
    json!({
        "type": "session_move",
        "id": id,
        "sessionPath": session_path.to_string_lossy(),
        "targetCwd": target_cwd.to_string_lossy(),
    })
}

/// Headline wire contract: move an unloaded native saved row over the wire,
/// replay it idempotently, restart the listener, and open the moved session
/// through an explicit switch while the stale source path fails closed.
#[tokio::test]
async fn session_move_wire_moves_row_replays_and_switches_after_restart() {
    let (_home, native, catalog) = {
        let home = tempfile::tempdir().expect("catalog home");
        let native = tempfile::tempdir().expect("native root");
        let catalog = pi_coding::SessionCatalog::new(home.path())
            .with_native_session_root(native.path());
        (home, native, catalog)
    };
    let project = tempfile::tempdir().expect("source project");
    let source_id = "smw-moved-row";
    let recorder = pi_coding::start_session_in(
        project.path(),
        None,
        None,
        Some(native.path()),
        Some(source_id),
        None,
    )
    .expect("source recorder");
    recorder
        .record_message(&pi_ai::Message::user_text("prompt before move", 0))
        .expect("source message");
    let source_path = recorder.path();
    drop(recorder);

    let target = tempfile::tempdir().expect("move target");
    let listener = start_listener(catalog.clone(), "smw-primary").await;
    let mut ws = ws_connect(listener.addr, None).await;
    let primary_id = bootstrap_primary(&mut ws).await;

    // Move the unloaded row: bounded outcome plus destination row, no spawn.
    let moved = rpc(&mut ws, &move_command("move-1", &source_path, target.path())).await;
    assert_eq!(moved["command"], "session_move");
    assert!(moved["success"].as_bool().unwrap_or(false), "{moved}");
    let data = moved["data"].as_object().expect("move data");
    assert_eq!(data["moved"], json!(true));
    assert_eq!(data["sessionId"], json!(source_id));
    let destination = PathBuf::from(data["to"].as_str().expect("to"));
    assert_eq!(data["from"], json!(source_path.to_string_lossy()));
    assert_eq!(
        data["session"]["path"],
        json!(destination.to_string_lossy()),
        "move response carries the destination catalog row"
    );
    assert!(!source_path.exists(), "source row removed");
    assert!(destination.exists(), "destination published");
    assert_eq!(
        destination.parent(),
        Some(pi_coding::default_session_dir(target.path()).as_path()),
        "destination publishes into the target's native session store"
    );
    assert_eq!(
        listener.spawner.spawns.load(Ordering::SeqCst),
        0,
        "move never spawns or registers a runtime"
    );

    // Idempotent retry over the wire: identical request replays the outcome.
    let replay = rpc(&mut ws, &move_command("move-2", &source_path, target.path())).await;
    assert!(replay["success"].as_bool().unwrap_or(false), "{replay}");
    assert_eq!(replay["data"]["to"], json!(destination.to_string_lossy()));
    drop(ws);

    // Restart: a fresh listener over the same catalog authority serves only
    // the moved world. The explicit switch opens the moved file; the stale
    // source path fails closed.
    stop_listener(listener).await;
    let restarted = start_listener(catalog.clone(), "smw-primary-restarted").await;
    let mut ws = ws_connect(restarted.addr, None).await;
    let _ = bootstrap_primary(&mut ws).await;

    let stale = rpc(&mut ws, &move_command("stale-check", &source_path, target.path())).await;
    assert!(
        !stale["success"].as_bool().unwrap_or(true),
        "stale retry must fail after restart: {stale}"
    );
    let stale_error = stale["error"].as_str().expect("bounded error");
    assert!(
        !stale_error.contains(source_path.to_string_lossy().as_ref())
            && !stale_error.contains(target.path().to_string_lossy().as_ref()),
        "no local-path echo: {stale_error}"
    );

    let switched = rpc(
        &mut ws,
        &json!({
            "type": "switch_session",
            "id": "switch-moved",
            "sessionId": primary_id,
            "sessionPath": destination.to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(switched["command"], "switch_session");
    assert!(switched["success"].as_bool().unwrap_or(false), "{switched}");
    assert_eq!(
        switched["data"]["sessionId"],
        json!(source_id),
        "the moved session keeps its identity across restart and switch"
    );
    let opened = restarted.spawner.opened.lock().expect("opened sessions");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0], destination);
    drop(opened);

    let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
    let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(project.path()));
    stop_listener(restarted).await;
}