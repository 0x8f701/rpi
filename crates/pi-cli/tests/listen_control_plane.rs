//! End-to-end wire contracts for the `--listen` control plane.
//!
//! Drives the real `modes::listen` server over TCP against a faux
//! `Application`, asserting exact JSON-RPC responses, WebSocket frames, auth
//! / Origin enforcement, bounded overload, shared Application identity,
//! canonical TUI extension queries, and remote interactive UI isolation —
//! never source text.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::Model;
use pi_agent::ThinkingLevel;
use pi_cli::extension_ui::ExtensionUiAdapter;
use pi_cli::modes::listen::{ListenConfig, ListenHandle, MAX_CONNECTION_TASKS, start};
use pi_coding::{
    Application, ExtensionCancellation, ExtensionInstanceId, ExtensionMode,
    ExtensionThemeDescriptor, ExtensionUiContext, ExtensionUiHost, ExtensionUiRequest,
    ExtensionUiResponse, ProcessSpawnSpec, Session, SessionOptions, TodoPhase,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};

const DEADLINE: Duration = Duration::from_secs(20);
const REMOTE_UI_DISABLED: &str = "remote interactive extension UI is disabled";

fn unique(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::now_v7().simple())
}

fn spawn_sleep_spec(cwd: &std::path::Path, seconds: u64) -> ProcessSpawnSpec {
    ProcessSpawnSpec {
        argv: vec!["sleep".into(), seconds.to_string()],
        cwd: cwd.to_path_buf(),
        env: BTreeMap::new(),
        tty: false,
        terminal_size: None,
        label: None,
        timeout_ms: None,
        output_bytes: None,
    }
}

struct FauxApp {
    application: Application,
    _registration: FauxProviderRegistration,
    cwd: TempDir,
}

fn faux_model(label: &str) -> (Model, FauxProviderRegistration) {
    let suffix = unique(label);
    let mut model = Model::default();
    model.id = format!("{label}-model");
    model.name = format!("{label} Model");
    model.api = format!("{suffix}-api");
    model.provider = format!("{suffix}-provider");
    model.base_url = "http://localhost:0".into();
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 8,
    });
    registration.set_responses(vec![FauxResponse::text("listen-faux-reply")]);
    (model, registration)
}

async fn faux_application(label: &str) -> FauxApp {
    let (model, registration) = faux_model(label);
    let cwd = tempfile::tempdir().expect("cwd");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
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
    })
    .expect("session");
    FauxApp {
        application: Application::new(session).await,
        _registration: registration,
        cwd,
    }
}

async fn listen(application: Application) -> (ListenHandle, ExtensionUiAdapter) {
    let extension_ui = ExtensionUiAdapter::new();
    extension_ui.set_canonical_queries_supported(true);
    let handle = start(
        application,
        extension_ui.clone(),
        ListenConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token_file: None,
        },
    )
    .await
    .expect("start listener");
    (handle, extension_ui)
}

async fn listen_with_token(
    application: Application,
    token: &str,
) -> (ListenHandle, ExtensionUiAdapter, TempDir) {
    let dir = tempfile::tempdir().expect("token dir");
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, token).expect("write token");
    let extension_ui = ExtensionUiAdapter::new();
    extension_ui.set_canonical_queries_supported(true);
    let handle = start(
        application,
        extension_ui.clone(),
        ListenConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token_file: Some(token_path),
        },
    )
    .await
    .expect("start listener");
    (handle, extension_ui, dir)
}

async fn http_post_rpc_with_headers(
    addr: std::net::SocketAddr,
    body: &[u8],
    token: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> (u16, Vec<u8>) {
    tokio::time::timeout(DEADLINE, async {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let mut request = format!(
            "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        if let Some(token) = token {
            request.push_str(&format!("authorization: Bearer {token}\r\n"));
        }
        for (name, value) in extra_headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        stream.write_all(body).await.expect("write body");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        (
            parse_status(&response).unwrap_or(0),
            parse_body(&response).unwrap_or_default(),
        )
    })
    .await
    .expect("http POST /rpc timed out")
}

async fn http_post_rpc(
    addr: std::net::SocketAddr,
    body: &[u8],
    token: Option<&str>,
) -> (u16, Vec<u8>) {
    http_post_rpc_with_headers(addr, body, token, &[]).await
}

fn parse_status(response: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(response).ok()?;
    let first_line = text.lines().next()?;
    let parts: Vec<&str> = first_line.split(' ').collect();
    parts.get(1)?.parse().ok()
}

fn parse_body(response: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(response).ok()?;
    let split = text.find("\r\n\r\n")?;
    Some(response[split + 4..].to_vec())
}

async fn http_get(addr: std::net::SocketAddr, path: &str, token: Option<&str>) -> (u16, Vec<u8>) {
    tokio::time::timeout(DEADLINE, async {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let mut request = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n");
        if let Some(token) = token {
            request.push_str(&format!("authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        (
            parse_status(&response).unwrap_or(0),
            parse_body(&response).unwrap_or_default(),
        )
    })
    .await
    .expect("http GET timed out")
}

async fn ws_connect(
    addr: std::net::SocketAddr,
    token: Option<&str>,
) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    ws_connect_with_origin(addr, token, None).await
}

async fn ws_connect_with_origin(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    origin: Option<&str>,
) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    tokio::time::timeout(DEADLINE, async {
        let url = format!("ws://{addr}/ws");
        let mut request = url.into_client_request().expect("build ws request");
        if let Some(token) = token {
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
        }
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert(http::header::ORIGIN, origin.parse().unwrap());
        }
        tokio_tungstenite::connect_async(request)
            .await
            .expect("ws connect")
            .0
    })
    .await
    .expect("ws connect timed out")
}

async fn try_ws_connect(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    origin: Option<&str>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    match tokio::time::timeout(DEADLINE, async {
        let url = format!("ws://{addr}/ws");
        let mut request = url.into_client_request().map_err(|e| e.to_string())?;
        if let Some(token) = token {
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
        }
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert(http::header::ORIGIN, origin.parse().unwrap());
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(ws, _)| ws)
            .map_err(|e| e.to_string())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("ws connect timed out".into()),
    }
}

/// Connect, write headers/body, and read the full HTTP response under [`DEADLINE`].
async fn http_raw_exchange(addr: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    tokio::time::timeout(DEADLINE, async {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request).await.expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        response
    })
    .await
    .expect("http raw exchange timed out")
}

#[tokio::test]
async fn http_get_state_returns_exact_rpc_response() {
    let app = faux_application("listen-get-state").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"state-1"}).to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(
        status,
        200,
        "status {status} body {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse response");
    assert_eq!(value["type"], "response");
    assert_eq!(value["command"], "get_state");
    assert_eq!(value["id"], "state-1");
    assert!(value["success"].as_bool().unwrap_or(false), "response: {value}");
    assert!(value["data"].is_object(), "data: {:?}", value["data"]);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn http_rejects_wrong_path_and_method() {
    let app = faux_application("listen-paths").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let (status, _) = http_get(addr, "/unknown", None).await;
    assert_eq!(status, 404);
    let body = json!({"type":"get_state"}).to_string();
    let (status, _) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(status, 200);
    let (status, _) = http_get(addr, "/rpc", None).await;
    assert_eq!(status, 404);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn http_rejects_missing_content_length() {
    let app = faux_application("listen-no-cl").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let request =
        b"POST /rpc HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n";
    let response = http_raw_exchange(addr, request).await;
    assert_eq!(parse_status(&response).unwrap_or(0), 411);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn http_rejects_oversized_body() {
    let app = faux_application("listen-oversized").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let request = format!(
        "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        5 * 1024 * 1024
    );
    let response = http_raw_exchange(addr, request.as_bytes()).await;
    assert_eq!(parse_status(&response), Some(413));
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}


#[tokio::test]
async fn ws_get_state_returns_response_and_application_events() {
    let app = faux_application("listen-ws").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"ws-1"}).to_string().into(),
    ))
    .await
    .expect("send command");

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut got_response = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse ws frame");
                if value["type"] == "response" && value["id"] == "ws-1" {
                    assert_eq!(value["command"], "get_state");
                    assert!(value["success"].as_bool().unwrap_or(false));
                    got_response = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(got_response, "ws did not return get_state response");

    app.application
        .set_todos(vec![TodoPhase {
            name: "listen-event".into(),
            tasks: vec![],
        }])
        .expect("set todos publishes TodoUpdated");

    let mut saw_todo_event = false;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse event");
                if value["type"] == "todo_updated" {
                    assert_eq!(value["phases"][0]["name"], "listen-event");
                    saw_todo_event = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(saw_todo_event, "ws never projected application TodoUpdated");
    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn ws_rejects_binary_messages() {
    let app = faux_application("listen-ws-binary").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;
    ws.send(WsMessage::Binary(vec![1, 2, 3].into()))
        .await
        .expect("send binary");
    let closed = tokio::time::timeout(DEADLINE, ws.next()).await;
    assert!(
        matches!(
            closed,
            Ok(Some(Ok(WsMessage::Close(_)))) | Ok(Some(Err(_))) | Ok(None)
        ),
        "ws did not close on binary"
    );
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn auth_rejects_missing_and_wrong_token_and_accepts_correct() {
    let app = faux_application("listen-auth").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "secret-token").await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"auth-1"}).to_string();
    assert_eq!(http_post_rpc(addr, body.as_bytes(), None).await.0, 401);
    assert_eq!(
        http_post_rpc(addr, body.as_bytes(), Some("wrong")).await.0,
        401
    );
    let (status, response) = http_post_rpc(addr, body.as_bytes(), Some("secret-token")).await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_slice(&response).expect("parse response");
    assert!(value["success"].as_bool().unwrap_or(false));
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn auth_rejects_ws_without_token() {
    let app = faux_application("listen-ws-auth").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "ws-secret").await;
    let addr = handle.local_addr();
    let result = tokio::time::timeout(
        DEADLINE,
        tokio_tungstenite::connect_async(
            format!("ws://{addr}/ws")
                .into_client_request()
                .unwrap(),
        ),
    )
    .await;
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "ws without token should fail"
    );
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn tokenless_loopback_rejects_browser_origin_over_http_and_ws() {
    let app = faux_application("listen-origin").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"origin-1"}).to_string();

    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&response));

    let (status, _) = http_post_rpc_with_headers(
        addr,
        body.as_bytes(),
        None,
        &[("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(status, 401, "browser origin without token must be 401");
    assert!(
        try_ws_connect(addr, None, Some("https://evil.example"))
            .await
            .is_err(),
        "ws browser origin without token must fail"
    );

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn token_authenticated_browser_origin_is_accepted() {
    let app = faux_application("listen-origin-token").await;
    let (handle, _extension_ui, _token_dir) =
        listen_with_token(app.application.clone(), "browser-ok").await;
    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"origin-ok"}).to_string();
    let (status, response) = http_post_rpc_with_headers(
        addr,
        body.as_bytes(),
        Some("browser-ok"),
        &[("origin", "https://app.example")],
    )
    .await;
    assert_eq!(
        status,
        200,
        "token+origin http: {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert!(value["success"].as_bool().unwrap_or(false));

    let mut ws =
        ws_connect_with_origin(addr, Some("browser-ok"), Some("https://app.example")).await;
    ws.send(WsMessage::Text(
        json!({"type":"get_state","id":"ws-origin"}).to_string().into(),
    ))
    .await
    .expect("ws send");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut ok = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["id"] == "ws-origin" {
                    assert!(value["success"].as_bool().unwrap_or(false));
                    ok = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(ok, "token+origin ws get_state missing");
    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn non_loopback_listen_requires_token_file() {
    let app = faux_application("listen-non-loopback").await;
    let extension_ui = ExtensionUiAdapter::new();
    let err = match start(
        app.application.clone(),
        extension_ui,
        ListenConfig {
            address: "0.0.0.0:0".parse().unwrap(),
            token_file: None,
        },
    )
    .await
    {
        Ok(handle) => {
            handle.stop().await.expect("stop unexpected listener");
            panic!("non-loopback without token must fail");
        }
        Err(error) => error,
    };
    let message = format!("{err:#}");
    assert!(
        message.contains("listen-token-file")
            || message.contains("non-loopback")
            || message.contains("token"),
        "unexpected error: {message}"
    );
    app.application.cleanup().await;
}

#[tokio::test]
async fn stop_handle_closes_listener_and_cleanup_still_runs() {
    let app = faux_application("listen-stop").await;
    let process = app
        .application
        .process_spawn(spawn_sleep_spec(app.cwd.path(), 30))
        .await
        .expect("spawn process for cleanup observability");
    assert!(
        app.application
            .process_list()
            .iter()
            .any(|info| info.id == process.id)
    );

    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    handle.stop().await.expect("stop");
    let result = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await;
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "listener should be closed after stop"
    );

    app.application.cleanup().await;
    assert!(
        app.application.process_list().is_empty(),
        "cleanup must shut down owned processes"
    );
}

#[tokio::test]
async fn overload_produces_429_when_concurrency_exceeded() {
    let app = faux_application("listen-overload").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let spawn = json!({
        "type": "process_spawn",
        "id": "overload-process",
        "spec": {
            "argv": ["sleep", "30"],
            "cwd": app.cwd.path(),
            "env": {},
            "tty": false
        }
    })
    .to_string();
    let (spawn_status, spawn_response) = http_post_rpc(addr, spawn.as_bytes(), None).await;
    assert_eq!(
        spawn_status,
        200,
        "spawn response: {}",
        String::from_utf8_lossy(&spawn_response)
    );
    let process: pi_coding::ProcessInfo = serde_json::from_value(
        serde_json::from_slice::<Value>(&spawn_response).expect("parse spawn response")["data"]
            .clone(),
    )
    .expect("parse process info");
    let process_id = process.id.as_str().to_owned();

    let mut wait_streams = Vec::new();
    for index in 0..16 {
        let body = json!({
            "type": "process_wait",
            "id": format!("wait-{index}"),
            "processId": process_id,
            "timeoutMs": 30_000
        })
        .to_string();
        let stream = tokio::time::timeout(DEADLINE, async {
            let mut stream = TcpStream::connect(addr).await.expect("connect wait");
            let request = format!(
                "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(request.as_bytes())
                .await
                .expect("write wait request");
            stream
        })
        .await
        .expect("process_wait connect/write timed out");
        wait_streams.push(stream);
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut overload_response = None;
    for attempt in 0..32 {
        let overflow = json!({
            "type": "process_wait",
            "id": format!("wait-overflow-{attempt}"),
            "processId": process_id,
            "timeoutMs": 1
        })
        .to_string();
        let response = http_post_rpc(addr, overflow.as_bytes(), None).await;
        if response.0 == 429 {
            overload_response = Some(response);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (overflow_status, overflow_body) = overload_response.expect("expected overload rejection");
    assert_eq!(overflow_status, 429);
    let overflow_json: Value =
        serde_json::from_slice(&overflow_body).expect("parse overload body");
    assert!(
        overflow_json["error"]
            .as_str()
            .is_some_and(|error| error.contains("too many concurrent RPC commands")),
        "overload body: {overflow_json}"
    );

    // Recovery must go through the public HTTP boundary, not Application APIs.
    // process_stop is runs_inline and must bypass the saturated work slots.
    let stop = json!({
        "type": "process_stop",
        "id": "recover",
        "processId": process_id
    })
    .to_string();
    let (stop_status, stop_body) = http_post_rpc(addr, stop.as_bytes(), None).await;
    assert_ne!(
        stop_status, 429,
        "process_stop must bypass saturation: {}",
        String::from_utf8_lossy(&stop_body)
    );
    assert_eq!(
        stop_status, 200,
        "process_stop over HTTP while saturated: {}",
        String::from_utf8_lossy(&stop_body)
    );
    let stop_json: Value = serde_json::from_slice(&stop_body).expect("parse stop");
    assert!(
        stop_json["success"].as_bool().unwrap_or(false),
        "process_stop body: {stop_json}"
    );

    // Waiters must settle after the HTTP stop (no hang / deadlock).
    for mut stream in wait_streams {
        let mut response = Vec::new();
        let read = tokio::time::timeout(DEADLINE, stream.read_to_end(&mut response)).await;
        assert!(
            read.is_ok(),
            "saturated process_wait must drain after HTTP process_stop"
        );
    }
    handle.stop().await.expect("stop listener");
    app.application.cleanup().await;
}

#[tokio::test]
async fn listener_shares_live_application_identity() {
    let app = faux_application("listen-shared-app").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();

    let before = app.application.state().await;
    let body = json!({"type":"get_state","id":"shared-1"}).to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert_eq!(status, 200);
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert!(value["success"].as_bool().unwrap_or(false));
    if let Some(session_id) = before.session_id.as_deref() {
        assert_eq!(
            value["data"]["sessionId"].as_str(),
            Some(session_id),
            "listener must expose the same session id"
        );
    }

    let set_todos = json!({
        "type": "set_todos",
        "id": "shared-todos",
        "phases": [{"name":"from-rpc","tasks":[]}]
    })
    .to_string();
    let (status, response) = http_post_rpc(addr, set_todos.as_bytes(), None).await;
    assert_eq!(
        status,
        200,
        "set_todos: {}",
        String::from_utf8_lossy(&response)
    );
    let todos = app.application.session().todo_state();
    assert!(
        todos.phases.iter().any(|phase| phase.name == "from-rpc"),
        "RPC set_todos must mutate the shared Application todos: {todos:?}"
    );

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn listener_preserves_canonical_tui_extension_queries() {
    let app = faux_application("listen-canonical").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    extension_ui.set_host_editor_text("canonical-editor-buffer");
    extension_ui.set_host_tools_expanded(true);
    extension_ui.set_themes(vec![ExtensionThemeDescriptor {
        name: "listen-theme".into(),
        path: None,
    }]);
    extension_ui.set_active_theme(Some("listen-theme".into()));

    let context = ExtensionUiContext {
        instance: ExtensionInstanceId {
            extension_id: "canonical-owner".into(),
            generation: 1,
        },
        mode: ExtensionMode::Tui,
    };
    let editor = extension_ui
        .request(
            context.clone(),
            ExtensionUiRequest::GetEditorText,
            ExtensionCancellation::new(),
        )
        .await
        .expect("GetEditorText");
    assert!(
        matches!(
            editor,
            ExtensionUiResponse::EditorText { ref value } if value == "canonical-editor-buffer"
        ),
        "editor: {editor:?}"
    );
    let themes = extension_ui
        .request(
            context.clone(),
            ExtensionUiRequest::GetAllThemes,
            ExtensionCancellation::new(),
        )
        .await
        .expect("GetAllThemes");
    match themes {
        ExtensionUiResponse::Themes { themes } => {
            assert!(
                themes.iter().any(|theme| theme.name == "listen-theme"),
                "themes: {themes:?}"
            );
        }
        other => panic!("expected Themes response, got {other:?}"),
    }
    let expanded = extension_ui
        .request(
            context,
            ExtensionUiRequest::GetToolsExpanded,
            ExtensionCancellation::new(),
        )
        .await
        .expect("GetToolsExpanded");
    assert!(
        matches!(
            expanded,
            ExtensionUiResponse::ToolsExpanded { expanded: true }
        ),
        "expanded: {expanded:?}"
    );

    let addr = handle.local_addr();
    let body = json!({"type":"get_state","id":"canonical-rpc"}).to_string();
    assert_eq!(http_post_rpc(addr, body.as_bytes(), None).await.0, 200);

    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn tui_interaction_ids_are_not_ws_respondable() {
    let app = faux_application("listen-tui-interaction").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    // The listen server observes non-interactive UI state only. A live TUI is
    // the exclusive owner required to keep interactive requests pending.
    let _tui_events = extension_ui.subscribe();
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;
    let _ = tokio::time::timeout(Duration::from_millis(50), ws.next()).await;

    let adapter = extension_ui.clone();
    let pending = tokio::spawn(async move {
        adapter
            .request(
                ExtensionUiContext {
                    instance: ExtensionInstanceId {
                        extension_id: "tui-owner".into(),
                        generation: 1,
                    },
                    mode: ExtensionMode::Tui,
                },
                ExtensionUiRequest::Confirm {
                    title: "Approve?".into(),
                    message: "tui-only".into(),
                },
                ExtensionCancellation::new(),
            )
            .await
    });

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let interaction_id = loop {
        if let Some(interaction) = extension_ui.pending_interactions().first() {
            break interaction.id.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("TUI interaction never became pending");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let scan_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < scan_deadline {
        match tokio::time::timeout(Duration::from_millis(50), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap_or(json!({}));
                if value["type"] == "extension_ui_request" {
                    assert_ne!(
                        value["id"].as_str(),
                        Some(interaction_id.as_str()),
                        "WS must not receive TUI InteractionRequested id"
                    );
                    assert_ne!(
                        value["method"], "confirm",
                        "WS received confirm interaction: {value}"
                    );
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }

    ws.send(WsMessage::Text(
        json!({
            "type": "extension_ui_response",
            "id": interaction_id,
            "confirmed": true
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send remote response");

    let mut saw_failure = false;
    let fail_deadline = tokio::time::Instant::now() + DEADLINE;
    while tokio::time::Instant::now() < fail_deadline {
        match tokio::time::timeout_at(fail_deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["type"] == "response"
                    && value["command"] == "extension_ui_response"
                    && value["success"] == false
                {
                    assert_eq!(
                        value["error"].as_str(),
                        Some(REMOTE_UI_DISABLED),
                        "exact disable error required, got {value}"
                    );
                    saw_failure = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(
        saw_failure,
        "WS must emit explicit extension_ui_response failure"
    );
    assert!(
        extension_ui
            .pending_interactions()
            .iter()
            .any(|interaction| interaction.id == interaction_id),
        "remote WS response must not consume TUI interaction"
    );

    extension_ui
        .respond_confirmed(&interaction_id, false)
        .expect("local TUI deny");
    let decision = tokio::time::timeout(DEADLINE, pending)
        .await
        .expect("pending join")
        .expect("pending task")
        .expect("pending result");
    assert!(matches!(
        decision,
        ExtensionUiResponse::Confirmed { confirmed: false }
    ));

    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn multiple_ws_clients_cannot_resolve_tui_interaction() {
    let app = faux_application("listen-multi-ws").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    // The listen server observes non-interactive UI state only. A live TUI is
    // the exclusive owner required to keep interactive requests pending.
    let _tui_events = extension_ui.subscribe();
    let addr = handle.local_addr();
    let mut ws_a = ws_connect(addr, None).await;
    let mut ws_b = ws_connect(addr, None).await;

    let adapter = extension_ui.clone();
    let pending = tokio::spawn(async move {
        adapter
            .request(
                ExtensionUiContext {
                    instance: ExtensionInstanceId {
                        extension_id: "race-owner".into(),
                        generation: 1,
                    },
                    mode: ExtensionMode::Tui,
                },
                ExtensionUiRequest::Confirm {
                    title: "Race?".into(),
                    message: "only tui".into(),
                },
                ExtensionCancellation::new(),
            )
            .await
    });

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let interaction_id = loop {
        if let Some(interaction) = extension_ui.pending_interactions().first() {
            break interaction.id.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("interaction missing");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    for ws in [&mut ws_a, &mut ws_b] {
        ws.send(WsMessage::Text(
            json!({
                "type": "extension_ui_response",
                "id": interaction_id,
                "confirmed": true
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send race response");
    }

    // Both clients should get the same hard failure; neither consumes the pending id.
    for (label, ws) in [("a", &mut ws_a), ("b", &mut ws_b)] {
        let mut saw_failure = false;
        let fail_deadline = tokio::time::Instant::now() + DEADLINE;
        while tokio::time::Instant::now() < fail_deadline {
            match tokio::time::timeout_at(fail_deadline, ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse");
                    if value["type"] == "response"
                        && value["command"] == "extension_ui_response"
                        && value["success"] == false
                    {
                        assert_eq!(
                            value["error"].as_str(),
                            Some(REMOTE_UI_DISABLED),
                            "{label} failure text"
                        );
                        saw_failure = true;
                        break;
                    }
                }
                Ok(Some(Ok(_))) => continue,
                _ => break,
            }
        }
        assert!(saw_failure, "{label} must get remote UI disabled failure");
    }

    assert!(
        extension_ui
            .pending_interactions()
            .iter()
            .any(|interaction| interaction.id == interaction_id),
        "neither WS client may consume the TUI interaction"
    );

    extension_ui
        .respond_confirmed(&interaction_id, true)
        .expect("tui owner allow");
    let decision = tokio::time::timeout(DEADLINE, pending)
        .await
        .expect("join")
        .expect("task")
        .expect("result");
    assert!(matches!(
        decision,
        ExtensionUiResponse::Confirmed { confirmed: true }
    ));

    ws_a.close(None).await.ok();
    ws_b.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

/// Pre-auth slow-header connections are hard-capped at [`MAX_CONNECTION_TASKS`].
/// Connection N+1 must be dropped promptly (never hang for the 10s header timeout).
#[tokio::test]
async fn preauth_slow_header_connections_are_capped() {
    let app = faux_application("listen-preauth-cap").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    assert_eq!(MAX_CONNECTION_TASKS, 64);

    let mut slow = Vec::with_capacity(MAX_CONNECTION_TASKS);
    for _ in 0..MAX_CONNECTION_TASKS {
        let stream = tokio::time::timeout(DEADLINE, async {
            let mut stream = TcpStream::connect(addr).await.expect("slow connect");
            stream
                .write_all(b"POST /rpc HTTP/1.1\r\nhost: x\r\n")
                .await
                .expect("partial headers");
            stream
        })
        .await
        .expect("slow-header connect/write timed out");
        slow.push(stream);
    }

    // Let the accept loop observe the saturated JoinSet.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connection 65: production accepts then immediately drops with no header task.
    // A full RPC write+read must fail promptly; a 2s hang is a contract failure.
    let overflow = tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = TcpStream::connect(addr).await.expect("overflow connect");
        let body = br#"{"type":"get_state","id":"cap-overflow"}"#;
        let request = format!(
            "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(request.as_bytes()).await.is_err() {
            return None;
        }
        if stream.write_all(body).await.is_err() {
            return None;
        }
        let mut response = Vec::new();
        match stream.read_to_end(&mut response).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(response),
        }
    })
    .await
    .expect("over-cap path must settle within 2s; timeout means the cap is not enforced");

    if let Some(response) = overflow {
        if let Some(status) = parse_status(&response) {
            assert_ne!(status, 200, "over-cap must not serve successful RPC");
        }
        if let Ok(value) =
            serde_json::from_slice::<Value>(&parse_body(&response).unwrap_or_default())
        {
            assert_ne!(value["success"], true, "over-cap body: {value}");
        }
    }

    // Release capacity and prove a normal request recovers.
    drop(slow.pop());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let body = json!({"type":"get_state","id":"after-cap"}).to_string();
    let mut recovered = None;
    for _ in 0..32 {
        let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
        if status == 200 {
            recovered = Some(response);
            break;
        }
        let _ = slow.pop();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let response = recovered.expect("normal request should succeed after releasing capacity");
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert!(value["success"].as_bool().unwrap_or(false));

    drop(slow);
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn listener_stop_error_path_still_allows_application_cleanup() {
    let app = faux_application("listen-stop-error").await;
    let process = app
        .application
        .process_spawn(spawn_sleep_spec(app.cwd.path(), 60))
        .await
        .expect("spawn");
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let stop_result = handle.stop().await;
    if let Err(error) = &stop_result {
        assert!(!format!("{error:#}").is_empty());
    }
    // Cleanup always runs after stop returns (Ok or Err), matching main_run.
    app.application.cleanup().await;
    assert!(
        app.application
            .process_list()
            .iter()
            .all(|info| info.id != process.id)
            || app.application.process_list().is_empty(),
        "cleanup must reclaim processes after listener stop"
    );
}

#[tokio::test]
async fn http_rejects_extension_ui_response_command() {
    let app = faux_application("listen-http-ui-response").await;
    let (handle, _extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let body = json!({
        "type": "extension_ui_response",
        "id": "nope",
        "confirmed": true
    })
    .to_string();
    let (status, response) = http_post_rpc(addr, body.as_bytes(), None).await;
    assert!(
        status == 400 || status == 200,
        "status {status} {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value = serde_json::from_slice(&response).expect("parse");
    assert_eq!(value["success"], false);
    assert_eq!(
        value["error"].as_str(),
        Some(REMOTE_UI_DISABLED),
        "http body: {value}"
    );
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

#[tokio::test]
async fn ws_projects_noninteractive_ui_events_but_not_confirms() {
    let app = faux_application("listen-ui-notify").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let mut ws = ws_connect(addr, None).await;

    extension_ui
        .request(
            ExtensionUiContext {
                instance: ExtensionInstanceId {
                    extension_id: "notify-owner".into(),
                    generation: 1,
                },
                mode: ExtensionMode::Tui,
            },
            ExtensionUiRequest::Notify {
                message: "hello-from-host".into(),
                level: pi_coding::UiNotificationLevel::Info,
            },
            ExtensionCancellation::new(),
        )
        .await
        .expect("notify");

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut saw_notify = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).expect("parse");
                if value["type"] == "extension_ui_request"
                    && value["method"] == "notify"
                {
                    assert_eq!(value["message"], "hello-from-host");
                    saw_notify = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(saw_notify, "non-interactive notify should project over WS");
    ws.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

/// Cap retained child diagnostics while still draining pipes to EOF.
const PIPE_DIAG_CAP: usize = 64 * 1024;

/// Drain a pipe to EOF on a background thread, retaining at most [`PIPE_DIAG_CAP`] bytes.
fn spawn_pipe_drain(
    pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut retained = Vec::new();
        let mut reader = pipe;
        let mut chunk = [0u8; 8 * 1024];
        loop {
            match std::io::Read::read(&mut reader, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(PIPE_DIAG_CAP.saturating_sub(retained.len()));
                    if take > 0 {
                        retained.extend_from_slice(&chunk[..take]);
                    }
                    // Continue reading past the cap so the pipe drains to EOF.
                }
                Err(_) => break,
            }
        }
        retained
    })
}

fn join_pipe_text(handle: std::thread::JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&handle.join().unwrap_or_default()).into_owned()
}

/// Kill (best-effort), wait, join both readers, then panic with captured streams.
fn kill_wait_join_panic(
    mut child: std::process::Child,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
    message: String,
) -> ! {
    let _ = child.kill();
    let _ = child.wait();
    let stdout = join_pipe_text(stdout);
    let stderr = join_pipe_text(stderr);
    panic!("{message}\nstdout={stdout}\nstderr={stderr}");
}

/// Bounded child run: concurrent stdout/stderr drains, poll exit, kill+wait on
/// deadline, then join readers (never join while the child may hold pipe ends).
fn finish_child_bounded(
    mut child: std::process::Child,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
    deadline: std::time::Duration,
    label: &str,
) -> (i32, String, String) {
    use std::time::{Duration, Instant};

    let end = Instant::now() + deadline;
    let mut status = None;
    while Instant::now() < end {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                kill_wait_join_panic(
                    child,
                    stdout,
                    stderr,
                    format!("try_wait {label}: {error}"),
                );
            }
        }
    }
    if status.is_none() {
        kill_wait_join_panic(
            child,
            stdout,
            stderr,
            format!("{label} exceeded {deadline:?}; killed child"),
        );
    }
    // Exit observed via try_wait; Drop reaps without blocking forever.
    drop(child);

    let stdout = join_pipe_text(stdout);
    let stderr = join_pipe_text(stderr);
    (
        status.expect("child exit status").code().unwrap_or(1),
        stdout,
        stderr,
    )
}


fn run_rpi_binary(args: &[&str]) -> (i32, String, String) {
    use std::process::{Command, Stdio};

    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    let mut child = Command::new(rpi_bin())
        .args(args)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PI_CODING_AGENT_DIR", home.path().join(".pi"))
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_OFFLINE", "1")
        .env("PI_FAUX_RESPONSE", "listen-cli-should-not-run")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi binary");
    let stdout = spawn_pipe_drain(child.stdout.take().expect("stdout pipe"));
    let stderr = spawn_pipe_drain(child.stderr.take().expect("stderr pipe"));
    finish_child_bounded(child, stdout, stderr, DEADLINE, "rpi binary")
}

/// Public binary: `--listen` + `--list-models` exits nonzero promptly.
#[test]
fn binary_listen_rejects_list_models_combination() {
    let (code, stdout, stderr) = run_rpi_binary(&[
        "--listen",
        "127.0.0.1:0",
        "--list-models",
        "--model",
        "faux/faux-1",
    ]);
    assert_ne!(code, 0, "stdout={stdout} stderr={stderr}");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("--listen") && combined.contains("--list-models"),
        "error must mention both flags: {combined}"
    );
    assert!(
        !combined.contains("listen-cli-should-not-run"),
        "must not emit faux prompt/model output: {combined}"
    );
    assert!(
        !combined.contains("Control plane listening"),
        "must not start the listener: {combined}"
    );
}

/// Public binary: non-TTY `--listen addr prompt` stays on the line REPL path
/// (not silent print mode). Hold stdin open, prove the listener answers RPC,
/// then close stdin and assert cleanup.
#[test]
fn binary_listen_with_prompt_serves_rpc_on_nontty_repl() {
    use std::io::Write;
    use std::net::TcpStream as StdTcpStream;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().expect("home");
    let cwd = tempfile::tempdir().expect("cwd");
    // Bind a concrete loopback port so the child and parent share the address.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe port");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);

    let mut child = Command::new(rpi_bin())
        .args([
            "--listen",
            &addr.to_string(),
            "--model",
            "faux/faux-1",
            "--api-key",
            "faux",
            "hello from non-tty prompt",
        ])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PI_CODING_AGENT_DIR", home.path().join(".pi"))
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_OFFLINE", "1")
        .env("PI_FAUX_RESPONSE", "listen-cli-should-not-run")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi");
    let stdout_reader = spawn_pipe_drain(child.stdout.take().expect("stdout pipe"));
    let stderr_reader = spawn_pipe_drain(child.stderr.take().expect("stderr pipe"));

    let body = br#"{"type":"get_state","id":"binary-listen-1"}"#;
    let request = format!(
        "POST /rpc HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rpc_ok = false;
    while Instant::now() < deadline {
        match StdTcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .ok();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .ok();
                if stream.write_all(request.as_bytes()).is_err() {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                if stream.write_all(body).is_err() {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                let mut response = Vec::new();
                let _ = std::io::Read::read_to_end(&mut stream, &mut response);
                if let Ok(text) = std::str::from_utf8(&response) {
                    if text.contains("\"success\":true") && text.contains("binary-listen-1") {
                        rpc_ok = true;
                        break;
                    }
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_pipe_text(stdout_reader);
                let stderr = join_pipe_text(stderr_reader);
                panic!(
                    "rpi exited before listener served RPC: status={status:?} stdout={stdout} stderr={stderr}"
                );
            }
            Ok(None) => {}
            Err(error) => {
                kill_wait_join_panic(
                    child,
                    stdout_reader,
                    stderr_reader,
                    format!("try_wait rpi during RPC probe: {error}"),
                );
            }
        }
    }
    if !rpc_ok {
        kill_wait_join_panic(
            child,
            stdout_reader,
            stderr_reader,
            "non-TTY --listen with prompt must serve control-plane RPC (not silent print mode)"
                .into(),
        );
    }

    drop(child.stdin.take());
    let exit_deadline = Instant::now() + Duration::from_secs(20);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                kill_wait_join_panic(
                    child,
                    stdout_reader,
                    stderr_reader,
                    format!("try_wait rpi during exit poll: {error}"),
                );
            }
        }
    }
    if !exited {
        kill_wait_join_panic(
            child,
            stdout_reader,
            stderr_reader,
            "rpi did not exit within 20s after REPL EOF".into(),
        );
    }
    // Child is dead: safe to join pipe readers.
    let stdout = join_pipe_text(stdout_reader);
    let stderr = join_pipe_text(stderr_reader);
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("Control plane listening") || rpc_ok,
        "listener should have started: {combined}"
    );

    // After exit the port must be free again (listener stopped + cleanup).
    let closed = Instant::now() + Duration::from_secs(5);
    let mut port_free = false;
    while Instant::now() < closed {
        match StdTcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            Err(_) => {
                port_free = true;
                break;
            }
            Ok(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    assert!(port_free, "listener port should close after REPL EOF/cleanup");
}

#[tokio::test]
async fn ws_evicts_slow_reader_without_blocking_fresh_clients() {
    let app = faux_application("listen-ws-slow-reader").await;
    let (handle, extension_ui) = listen(app.application.clone()).await;
    let addr = handle.local_addr();
    let socket = tokio::net::TcpSocket::new_v4().expect("create slow-reader TCP socket");
    socket
        .set_recv_buffer_size(1024)
        .expect("shrink slow-reader TCP receive buffer");
    let stream = tokio::time::timeout(DEADLINE, socket.connect(addr))
        .await
        .expect("slow-reader TCP connect timed out")
        .expect("connect slow-reader TCP socket");
    let request = format!("ws://{addr}/ws")
        .into_client_request()
        .expect("build slow-reader WS request");
    let slow = tokio::time::timeout(DEADLINE, tokio_tungstenite::client_async(request, stream))
        .await
        .expect("slow-reader WS handshake timed out")
        .expect("complete slow-reader WS handshake")
        .0;
    let mut slow = slow.into_inner();

    let payload = "x".repeat(128 * 1024);
    tokio::time::timeout(DEADLINE, async {
        for _ in 0..512 {
            extension_ui
                .request(
                    ExtensionUiContext {
                        instance: ExtensionInstanceId {
                            extension_id: "slow-reader-flood".into(),
                            generation: 1,
                        },
                        mode: ExtensionMode::Tui,
                    },
                    ExtensionUiRequest::Status {
                        key: "bounded-flood".into(),
                        text: Some(payload.clone()),
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .expect("publish public status event");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded public event flood timed out");

    tokio::time::timeout(DEADLINE, async {
        let mut buffer = [0_u8; 8192];
        loop {
            match slow.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await
    .expect("slow WebSocket TCP socket was not closed within the deadline");

    let mut fresh = ws_connect(addr, None).await;
    fresh
        .send(WsMessage::Text(
            json!({"type":"get_state","id":"after-slow-reader"})
                .to_string()
                .into(),
        ))
        .await
        .expect("send get_state after slow-reader eviction");
    let response = tokio::time::timeout(DEADLINE, async {
        loop {
            match fresh.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("parse get_state response");
                    if value["type"] == "response" && value["id"] == "after-slow-reader" {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("fresh WebSocket failed: {error}"),
                None => panic!("fresh WebSocket closed before get_state response"),
            }
        }
    })
    .await
    .expect("fresh WebSocket get_state timed out");
    assert_eq!(response["command"], "get_state");
    assert_eq!(response["success"], true);

    fresh.close(None).await.ok();
    handle.stop().await.expect("stop");
    app.application.cleanup().await;
}
