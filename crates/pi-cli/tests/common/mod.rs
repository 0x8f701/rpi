//! Shared wire-test helpers for the Web/listen control plane tests.
//!
//! Both `listen_control_plane.rs` and `concurrent_sessions.rs` include this
//! module via `#[path = "common/mod.rs"] mod common;` so the helper set has a
//! single owner. Helpers drive the real `modes::listen` server over TCP
//! against a faux `Application`.

use std::collections::BTreeMap;
use std::time::Duration;

use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::Model;
use pi_agent::ThinkingLevel;
use pi_cli::extension_ui::ExtensionUiAdapter;
use pi_cli::modes::listen::{ListenConfig, ListenHandle, MAX_CONNECTION_TASKS, start};
use pi_coding::{Application, ProcessSpawnSpec, Session, SessionOptions};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::client::IntoClientRequest};

pub const DEADLINE: Duration = Duration::from_secs(20);
pub const REMOTE_UI_DISABLED: &str = "remote interactive extension UI is disabled";

pub fn unique(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::now_v7().simple())
}

pub fn spawn_sleep_spec(cwd: &std::path::Path, seconds: u64) -> ProcessSpawnSpec {
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

pub struct FauxApp {
    pub application: Application,
    pub _registration: FauxProviderRegistration,
    pub cwd: TempDir,
}

pub fn faux_model(label: &str) -> (Model, FauxProviderRegistration) {
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

pub async fn faux_application(label: &str) -> FauxApp {
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
    let session_dir = cwd.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let recorder = pi_coding::start_session_in(
        cwd.path(),
        session.model().as_ref(),
        Some("off"),
        Some(&session_dir),
        None,
        None,
    )
    .expect("start listener test recorder");
    session.record(recorder).expect("attach listener test recorder");
    FauxApp {
        application: Application::new(session).await,
        _registration: registration,
        cwd,
    }
}

pub async fn listen(application: Application) -> (ListenHandle, ExtensionUiAdapter) {
    let extension_ui = ExtensionUiAdapter::new();
    extension_ui.set_canonical_queries_supported(true);
    let handle = start(
        application,
        extension_ui.clone(),
        ListenConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token_file: None,
            allow_insecure_remote: false,
            advertised_origin: None,
            session_factory: None,
        },
    )
    .await
    .expect("start listener");
    (handle, extension_ui)
}

pub async fn listen_with_token(
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
            allow_insecure_remote: false,
            advertised_origin: None,
            session_factory: None,
        },
    )
    .await
    .expect("start listener");
    (handle, extension_ui, dir)
}

pub async fn http_post_rpc_with_headers(
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

pub async fn http_post_rpc(
    addr: std::net::SocketAddr,
    body: &[u8],
    token: Option<&str>,
) -> (u16, Vec<u8>) {
    http_post_rpc_with_headers(addr, body, token, &[]).await
}

pub fn parse_status(response: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(response).ok()?;
    let first_line = text.lines().next()?;
    let parts: Vec<&str> = first_line.split(' ').collect();
    parts.get(1)?.parse().ok()
}

pub fn parse_body(response: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(response).ok()?;
    let split = text.find("\r\n\r\n")?;
    Some(response[split + 4..].to_vec())
}

pub async fn http_get(addr: std::net::SocketAddr, path: &str, token: Option<&str>) -> (u16, Vec<u8>) {
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

pub async fn ws_connect(
    addr: std::net::SocketAddr,
    token: Option<&str>,
) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    ws_connect_with_origin(addr, token, None).await
}

pub async fn ws_connect_with_origin(
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

/// Connect to `/ws` offering `Sec-WebSocket-Protocol: <subprotocol>` and an
/// optional Origin, returning the stream plus the server handshake response
/// (whose `Sec-WebSocket-Protocol` header proves the echoed subprotocol).
pub async fn ws_connect_path_with_subprotocol(
    addr: std::net::SocketAddr,
    path: &str,
    subprotocol: Option<&str>,
    origin: Option<&str>,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        http::Response<Option<Vec<u8>>>,
    ),
    String,
> {
    tokio::time::timeout(DEADLINE, async {
        let url = format!("ws://{addr}{path}");
        let mut request = url.into_client_request().map_err(|e| e.to_string())?;
        if let Some(subprotocol) = subprotocol {
            request.headers_mut().insert(
                http::header::SEC_WEBSOCKET_PROTOCOL,
                subprotocol.parse().unwrap(),
            );
        }
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert(http::header::ORIGIN, origin.parse().unwrap());
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(ws, response)| (ws, response))
            .map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|_| Err("ws subprotocol connect timed out".into()))
}

pub async fn ws_connect_with_subprotocol(
    addr: std::net::SocketAddr,
    subprotocol: Option<&str>,
    origin: Option<&str>,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        http::Response<Option<Vec<u8>>>,
    ),
    String,
> {
    ws_connect_path_with_subprotocol(addr, "/ws", subprotocol, origin).await
}

pub async fn try_ws_connect(
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
pub async fn http_raw_exchange(addr: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
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
