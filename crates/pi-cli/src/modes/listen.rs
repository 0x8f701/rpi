use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use pi_coding::Application;
use serde::Serialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    },
};

use crate::extension_ui::ExtensionUiAdapter;
use super::collab_service::{CollabService, capability_from_protocols};

use super::rpc::{MAX_CONCURRENT_COMMANDS, MAX_RPC_MESSAGE_BYTES, RpcInput, RpcResponse, parse_input};
use super::session_runtime_manager::SessionRuntimeManager;
pub use super::session_runtime_manager::{
    MAX_CONCURRENT_SESSION_COMMANDS, MAX_LOADED_SESSIONS, SessionSpawner,
};

const MAX_HEADER_BYTES: usize = 32 * 1024;
const OUTBOUND_QUEUE_CAPACITY: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WS_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const CONTENT_TYPE_JSON: &str = "application/json";
const REMOTE_EXTENSION_UI_ERROR: &str = "remote interactive extension UI is disabled";

// Transport auth policy shared with the ACP WebSocket server (`ws_auth`);
// `MAX_CONNECTION_TASKS` stays publicly reachable through this module.
pub use super::ws_auth::MAX_CONNECTION_TASKS;
use super::ws_auth::{
    ListenAddressPolicy, authorized, constant_work_eq, load_auth_token, read_token_file,
    websocket_subprotocol,
};

/// Web client assets (vite build output) embedded into the binary by
/// build.rs from `crates/pi-cli/web/dist/`. The page carries no data itself:
/// every command and event flows through the token-gated `/rpc` and `/ws`
/// routes, so the page is served without authentication and everything else
/// keeps the existing auth policy.
///
/// Development override: `RPI_WEB_DEV_DIR` points at a directory containing a
/// built `index.html` (e.g. `crates/pi-cli/web/dist`) served instead of the
/// embedded copy, so frontend iteration does not require rebuilding the
/// binary. `vite dev` is the primary dev loop and needs no override.
const RPI_WEB_DEV_DIR: &str = "RPI_WEB_DEV_DIR";

#[derive(Clone)]
pub struct ListenConfig {
    pub address: SocketAddr,
    pub token_file: Option<PathBuf>,
    /// Permit a non-loopback plaintext bind only when `token_file` is valid.
    pub allow_insecure_remote: bool,
    /// Factory that builds manager-owned session runtimes for the Web
    /// control plane (switch_session / new_session / fork / clone). `None`
    /// disables lifecycle opens with a clear error; tests inject a faux
    /// factory.
    pub session_factory: Option<std::sync::Arc<dyn SessionSpawner>>,
}

pub struct ListenHandle {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
    manager: std::sync::Arc<SessionRuntimeManager>,
    collab: CollabService,
}

impl ListenHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    #[must_use]
    pub fn collab_service(&self) -> CollabService {
        self.collab.clone()
    }
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }


    pub async fn stop(self) -> Result<()> {
        let shutdown_result = self
            .shutdown
            .send(true)
            .map_err(|_| anyhow!("control plane listener stopped before shutdown was signaled"));
        let task_result = match self.task.await {
            Ok(result) => result.context("running control plane listener"),
            Err(error) => Err(anyhow!(error).context("joining control plane listener")),
        };
        // Listener shutdown cleans the manager: abort every fan-in forwarder,
        // then clean manager-owned non-primary runtimes. The primary TUI
        // Application remains owned by lib.rs.
        self.manager.shutdown().await;
        match (shutdown_result, task_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(shutdown_error), Ok(())) => Err(shutdown_error),
            (Ok(()), Err(task_error)) => Err(task_error),
            (Err(shutdown_error), Err(task_error)) => Err(task_error.context(format!(
                "control plane shutdown signaling also failed: {shutdown_error:#}"
            ))),
        }
    }
}

#[derive(Clone)]
struct ServerState {
    manager: std::sync::Arc<SessionRuntimeManager>,
    collab: CollabService,
    token: Option<Arc<[u8]>>,
    base_url: String,
}

pub async fn start(
    application: Application,
    extension_ui: ExtensionUiAdapter,
    config: ListenConfig,
) -> Result<ListenHandle> {
    let policy = if config.allow_insecure_remote {
        ListenAddressPolicy::AllowAuthenticatedPlaintextRemote
    } else {
        ListenAddressPolicy::LoopbackOnly
    };
    let token = load_auth_token(
        config.address.ip(),
        config.token_file.as_deref(),
        "--listen",
        policy,
    )?;
    let listener = TcpListener::bind(config.address)
        .await
        .with_context(|| format!("binding control plane to {}", config.address))?;
    let address = listener
        .local_addr()
        .context("reading control plane listener address")?;
    let manager = SessionRuntimeManager::new(application, extension_ui, config.session_factory).await;
    let collab = CollabService::new(manager.clone());
    let state = ServerState {
        manager: manager.clone(),
        collab: collab.clone(),
        token: token.map(Arc::from),
        base_url: format!("http://{address}"),
    };
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(run_listener(listener, state, shutdown_rx));
    Ok(ListenHandle {
        address,
        shutdown,
        task,
        manager,
        collab,
    })
}

async fn run_listener(
    listener: TcpListener,
    state: ServerState,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                let _ = changed;
                state.collab.stop_all().await;
                tokio::task::yield_now().await;
                break;
            }
            accepted = listener.accept(), if connections.len() < MAX_CONNECTION_TASKS => {
                let (stream, _) = accepted.context("accepting control plane connection")?;
                let state = state.clone();
                connections.spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                joined
                    .expect("guarded by non-empty connection set")
                    .context("joining control plane connection task")?;
            }
            accepted = listener.accept(), if connections.len() >= MAX_CONNECTION_TASKS => {
                let (stream, _) = accepted.context("accepting saturated control plane connection")?;
                drop(stream);
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn handle_connection(mut stream: TcpStream, state: ServerState) -> Result<()> {
    let raw = match tokio::time::timeout(READ_TIMEOUT, read_http_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            write_plain_response(&mut stream, error.status, error.message).await?;
            return Ok(());
        }
        Err(_) => {
            write_plain_response(&mut stream, StatusCode::REQUEST_TIMEOUT, "request timed out")
                .await?;
            return Ok(());
        }
    };

    if is_websocket_upgrade(&raw.headers) {
        if raw.method != Method::GET {
            write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
            return Ok(());
        }
        if let Some(room_id) = collab_room_id(&raw.path) {
            let Some((protocol, presented)) = capability_from_protocols(&raw.headers) else {
                write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized").await?;
                return Ok(());
            };
            let connection = match state.collab.authenticate(room_id, &presented).await {
                Ok(connection) => connection,
                Err(_) => {
                    write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized").await?;
                    return Ok(());
                }
            };
            return collab_websocket_connection(stream, raw, connection, protocol).await;
        }
        if raw.path != "/ws" {
            write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
            return Ok(());
        }
        let protocol = websocket_subprotocol(&raw.headers, state.token.as_deref());
        if !authorized(&raw.headers, state.token.as_deref()) && protocol.is_none() {
            write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized").await?;
            return Ok(());
        }
        return websocket_connection(stream, raw, state, protocol).await;
    }

    // The static web client page is served without authentication: it carries
    // no data itself, and every command/event flows through authenticated or
    // capability-gated WebSockets. A collaboration join link uses the WS path
    // as its browser document URL; non-upgrade GETs at that exact validated
    // route receive the same embedded client, which reads the secret fragment
    // locally before opening the encrypted WebSocket.
    if raw.method == Method::GET
        && (raw.path == "/web" || collab_room_id(&raw.path).is_some())
    {
        return serve_web_page(&mut stream).await;
    }
    // Named web assets (e.g. hashed JS/CSS bundles) from the embedded table.
    if raw.method == Method::GET && raw.path.starts_with("/assets/") {
        let Some((mime, bytes)) = crate::web::get(&raw.path) else {
            write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
            return Ok(());
        };
        return write_response(&mut stream, StatusCode::OK, mime, bytes).await;
    }

    if raw.method != Method::POST || raw.path != "/rpc" {
        write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
        return Ok(());
    }
    if !authorized(&raw.headers, state.token.as_deref()) {
        write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized").await?;
        return Ok(());
    }
    if !has_json_content_type(&raw.headers) {
        write_plain_response(
            &mut stream,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/json",
        )
        .await?;
        return Ok(());
    }
    let Some(length) = content_length(&raw.headers) else {
        write_plain_response(&mut stream, StatusCode::LENGTH_REQUIRED, "content-length required")
            .await?;
        return Ok(());
    };
    if length > MAX_RPC_MESSAGE_BYTES {
        write_plain_response(&mut stream, StatusCode::PAYLOAD_TOO_LARGE, "request too large")
            .await?;
        return Ok(());
    }
    let body = match tokio::time::timeout(
        READ_TIMEOUT,
        read_body(&mut stream, raw.remainder, length),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(error)) => {
            write_plain_response(&mut stream, error.status, error.message).await?;
            return Ok(());
        }
        Err(_) => {
            write_plain_response(&mut stream, StatusCode::REQUEST_TIMEOUT, "request timed out")
                .await?;
            return Ok(());
        }
    };
    let response = match parse_input(&body) {
        Ok(RpcInput::Command { command, session_id }) => {
            dispatch_http_command(&state, command, session_id).await
        }
        Ok(RpcInput::ExtensionUiResponse(_)) => RpcResponse::failure(
            None,
            "extension_ui_response",
            REMOTE_EXTENSION_UI_ERROR,
        ),
        Err(response) => response,
    };
    let status = if response.success {
        StatusCode::OK
    } else if response
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with("too many concurrent RPC commands"))
    {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::BAD_REQUEST
    };
    write_json_response(&mut stream, status, &response).await
}

async fn dispatch_http_command(
    state: &ServerState,
    command: super::rpc::RpcCommand,
    session_id: Option<String>,
) -> RpcResponse {
    let id = command.id();
    let name = command.command_name();
    match command {
        super::rpc::RpcCommand::CollabStart { base_url, .. } => {
            let base_url = base_url.as_deref().unwrap_or(&state.base_url);
            match state.collab.start(session_id.as_deref(), base_url).await {
                Ok(started) => RpcResponse::success(id, name, serde_json::to_value(started).ok()),
                Err(error) => RpcResponse::failure(id, name, error.to_string()),
            }
        }
        super::rpc::RpcCommand::CollabStatus { room_id, .. } => RpcResponse::success(
            id,
            name,
            Some(serde_json::json!({
                "rooms": state.collab.status(room_id.as_deref()).await,
            })),
        ),
        super::rpc::RpcCommand::CollabStop { room_id, .. } => {
            match state.collab.stop(&room_id).await {
                Ok(room) => RpcResponse::success(
                    id,
                    name,
                    Some(serde_json::json!({"stopped": true, "room": room})),
                ),
                Err(error) => RpcResponse::failure(id, name, error.to_string()),
            }
        }
        command => state.manager.dispatch(command, session_id).await,
    }
}

fn collab_room_id(path: &str) -> Option<&str> {
    let room_id = path.strip_prefix("/collab/ws/")?;
    (!room_id.is_empty()
        && !room_id.contains('/')
        && room_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(room_id)
}

struct RawRequest {
    method: Method,
    path: String,
    headers: HeaderMap,
    raw_headers: Vec<u8>,
    remainder: Vec<u8>,
}

#[derive(Debug)]
struct RequestError {
    status: StatusCode,
    message: &'static str,
}

async fn read_http_request(
    stream: &mut TcpStream,
) -> std::result::Result<RawRequest, RequestError> {
    let mut bytes = Vec::with_capacity(1024);
    let end = loop {
        let mut chunk = [0_u8; 1024];
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|_| RequestError {
                status: StatusCode::BAD_REQUEST,
                message: "failed to read request",
            })?;
        if count == 0 {
            return Err(RequestError {
                status: StatusCode::BAD_REQUEST,
                message: "incomplete request headers",
            });
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = find_header_end(&bytes) {
            if end + 4 > MAX_HEADER_BYTES {
                return Err(RequestError {
                    status: StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                    message: "request headers too large",
                });
            }
            break end;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(RequestError {
                status: StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                message: "request headers too large",
            });
        }
    };
    let raw_headers = bytes[..end].to_vec();
    let remainder = bytes[end + 4..].to_vec();
    parse_request_headers(&raw_headers, remainder)
}

fn parse_request_headers(
    raw_headers: &[u8],
    remainder: Vec<u8>,
) -> std::result::Result<RawRequest, RequestError> {
    let text = std::str::from_utf8(raw_headers).map_err(|_| RequestError {
        status: StatusCode::BAD_REQUEST,
        message: "malformed request headers",
    })?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(RequestError {
        status: StatusCode::BAD_REQUEST,
        message: "missing request line",
    })?;
    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .and_then(|method| Method::from_bytes(method.as_bytes()).ok())
        .ok_or(RequestError {
            status: StatusCode::BAD_REQUEST,
            message: "malformed request method",
        })?;
    let path = parts.next().ok_or(RequestError {
        status: StatusCode::BAD_REQUEST,
        message: "missing request path",
    })?;
    let version = parts.next();
    if version != Some("HTTP/1.1") || parts.next().is_some() || path.contains('?') {
        return Err(RequestError {
            status: StatusCode::BAD_REQUEST,
            message: "malformed request line",
        });
    }
    let mut headers = HeaderMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(RequestError {
            status: StatusCode::BAD_REQUEST,
            message: "malformed request header",
        })?;
        let name = http::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| RequestError {
                status: StatusCode::BAD_REQUEST,
                message: "malformed request header",
            })?;
        let value = HeaderValue::from_str(value.trim()).map_err(|_| RequestError {
            status: StatusCode::BAD_REQUEST,
            message: "malformed request header",
        })?;
        headers.append(name, value);
    }
    if headers
        .get_all(http::header::TRANSFER_ENCODING)
        .iter()
        .next()
        .is_some()
    {
        return Err(RequestError {
            status: StatusCode::BAD_REQUEST,
            message: "transfer-encoding is not supported",
        });
    }
    Ok(RawRequest {
        method,
        path: path.to_owned(),
        headers,
        raw_headers: [raw_headers, b"\r\n\r\n"].concat(),
        remainder,
    })
}

async fn read_body(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    length: usize,
) -> std::result::Result<Vec<u8>, RequestError> {
    if body.len() > length {
        body.truncate(length);
        return Ok(body);
    }
    body.reserve(length.saturating_sub(body.len()));
    while body.len() < length {
        let mut chunk = [0_u8; 8192];
        let wanted = (length - body.len()).min(chunk.len());
        let count = stream
            .read(&mut chunk[..wanted])
            .await
            .map_err(|_| RequestError {
                status: StatusCode::BAD_REQUEST,
                message: "failed to read request body",
            })?;
        if count == 0 {
            return Err(RequestError {
                status: StatusCode::BAD_REQUEST,
                message: "incomplete request body",
            });
        }
        body.extend_from_slice(&chunk[..count]);
    }
    Ok(body)
}

struct PrefixedStream {
    prefix: Vec<u8>,
    offset: usize,
    stream: TcpStream,
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.offset < self.prefix.len() {
            let count = buffer
                .remaining()
                .min(self.prefix.len().saturating_sub(self.offset));
            buffer.put_slice(&self.prefix[self.offset..self.offset + count]);
            self.offset += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

struct AbortTask<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortTask<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn handle_mut(&mut self) -> &mut JoinHandle<T> {
        self.handle.as_mut().expect("writer task is present")
    }
}

impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

enum WebSocketExit {
    Graceful(Option<CloseFrame>),
    SlowClient,
}

enum WriterControl {
    Close(Option<CloseFrame>),
}

// Subscribe before accepting the WebSocket upgrade. Once the client observes
// a successful handshake, every subsequent application/UI event must have an
// active receiver rather than racing the server's post-handshake setup.
async fn collab_websocket_connection(
    stream: TcpStream,
    raw: RawRequest,
    mut connection: super::collab_service::CollabConnection,
    protocol: String,
) -> Result<()> {
    let path = raw.path.clone();
    let mut prefix = raw.raw_headers;
    prefix.extend_from_slice(&raw.remainder);
    let config = WebSocketConfig::default()
        .max_message_size(Some(pi_coding::collab::MAX_FRAME_BYTES))
        .max_frame_size(Some(pi_coding::collab::MAX_FRAME_BYTES))
        .max_write_buffer_size(2 * pi_coding::collab::MAX_FRAME_BYTES);
    let websocket = accept_hdr_async_with_config(
        PrefixedStream {
            prefix,
            offset: 0,
            stream,
        },
        move |request: &Request, mut response: Response| -> std::result::Result<Response, ErrorResponse> {
            if request.uri().path() != path {
                let mut error = ErrorResponse::new(Some("not found".into()));
                *error.status_mut() = StatusCode::NOT_FOUND;
                return Err(error);
            }
            response.headers_mut().insert(
                http::header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_str(&protocol).map_err(|_| {
                    let mut error = ErrorResponse::new(Some("invalid subprotocol".into()));
                    *error.status_mut() = StatusCode::BAD_REQUEST;
                    error
                })?,
            );
            Ok(response)
        },
        Some(config),
    )
    .await
    .context("upgrading collaboration WebSocket")?;

    let (mut write, mut read) = websocket.split();
    if *connection.stopped.borrow() {
        let _ = write.send(Message::Close(Some(collab_stopped_close_frame()))).await;
        return Ok(());
    }
    let hello = serde_json::to_string(&connection.hello())
        .context("serializing collaboration hello")?;
    write
        .send(Message::Text(hello.into()))
        .await
        .context("sending collaboration hello")?;
    if *connection.stopped.borrow() {
        let _ = write.send(Message::Close(Some(collab_stopped_close_frame()))).await;
        return Ok(());
    }
    write
        .send(Message::Binary(connection.snapshot_frame()?.into()))
        .await
        .context("sending collaboration snapshot")?;

    loop {
        tokio::select! {
            biased;
            changed = connection.stopped.changed() => {
                if changed.is_err() || *connection.stopped.borrow() {
                    let _ = write.send(Message::Close(Some(collab_stopped_close_frame()))).await;
                    return Ok(());
                }
            }
            event = connection.events.recv() => match event {
                Ok(event) if connection.event_matches_room(&event) => {
                    let frame = connection.event_frame(&event)?;
                    write.send(Message::Binary(frame.into())).await
                        .context("sending collaboration event")?;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = write.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Policy,
                        reason: "collaboration event stream lagged".into(),
                    }))).await;
                    return Ok(());
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            incoming = read.next() => match incoming {
                Some(Ok(Message::Binary(frame))) => {
                    let pending = match connection.prepare_client_frame(&frame) {
                        Ok(pending) => pending,
                        Err(_) => {
                            let _ = write.send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Policy,
                                reason: "invalid collaboration frame".into(),
                            }))).await;
                            return Ok(());
                        }
                    };
                    let response = pending.execute().await;
                    let frame = connection.response_frame(response)?;
                    write.send(Message::Binary(frame.into())).await
                        .context("sending collaboration response")?;
                }
                Some(Ok(Message::Ping(payload))) => {
                    write.send(Message::Pong(payload)).await
                        .context("sending collaboration pong")?;
                }
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(Message::Text(_) | Message::Pong(_) | Message::Frame(_))) => {
                    let _ = write.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Unsupported,
                        reason: "encrypted binary messages required".into(),
                    }))).await;
                    return Ok(());
                }
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                    let _ = write.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Size,
                        reason: "message too large".into(),
                    }))).await;
                    return Ok(());
                }
                Some(Err(_)) => return Ok(()),
            }
        }
    }
}
fn collab_stopped_close_frame() -> CloseFrame {
    CloseFrame {
        code: CloseCode::Away,
        reason: "collaboration room stopped".into(),
    }
}


async fn websocket_connection(
    stream: TcpStream,
    raw: RawRequest,
    state: ServerState,
    protocol: Option<String>,
) -> Result<()> {
    // Fan-in: the manager merges every session runtime's projected events
    // (tagged with the owning top-level `sessionId`); every connection sees
    // every session's events, and commands route explicitly by sessionId.
    let mut events = state.manager.events();
    // Extension UI events likewise fan in through the manager; host/TUI-owned
    // interactions were already filtered by the runtime forwarders, and
    // remote answering stays rejected below.
    let mut ui_events = state.manager.ui_events();
    let mut prefix = raw.raw_headers;
    prefix.extend_from_slice(&raw.remainder);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RPC_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_RPC_MESSAGE_BYTES))
        .max_write_buffer_size(2 * MAX_RPC_MESSAGE_BYTES);
    let websocket = accept_hdr_async_with_config(
        PrefixedStream {
            prefix,
            offset: 0,
            stream,
        },
        |request: &Request, mut response: Response| -> std::result::Result<Response, ErrorResponse> {
            if request.uri().path() != "/ws" {
                let mut error = ErrorResponse::new(Some("not found".into()));
                *error.status_mut() = StatusCode::NOT_FOUND;
                return Err(error);
            }
            // RFC 6455: the server must select at most one offered subprotocol
            // and echo it, otherwise browsers abort the handshake. Only echo a
            // protocol that already passed the auth check above.
            if let Some(protocol) = protocol.as_deref() {
                response.headers_mut().insert(
                    http::header::SEC_WEBSOCKET_PROTOCOL,
                    HeaderValue::from_str(protocol).map_err(|_| {
                        let mut error = ErrorResponse::new(Some("invalid subprotocol".into()));
                        *error.status_mut() = StatusCode::BAD_REQUEST;
                        error
                    })?,
                );
            }
            Ok(response)
        },
        Some(config),
    )
    .await
    .context("upgrading control plane WebSocket")?;

    let (mut websocket_write, mut websocket_read) = websocket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAPACITY);
    // Non-inline commands run on a per-connection task set so a long command
    // never blocks the read/event select below. The set is bounded at
    // MAX_CONCURRENT_COMMANDS (mirroring the stdio RPC session); dropping it
    // on disconnect aborts whatever is still pending.
    let mut commands = JoinSet::new();
    let (writer_control_tx, mut writer_control_rx) = mpsc::channel::<WriterControl>(1);
    let mut writer = AbortTask::new(tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                biased;
                control = writer_control_rx.recv() => match control {
                    Some(WriterControl::Close(frame)) => {
                        let _ = tokio::time::timeout(
                            WS_CLOSE_TIMEOUT,
                            websocket_write.send(Message::Close(frame)),
                        ).await;
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                message = outbound_rx.recv() => match message {
                    Some(message) => message,
                    None => return Ok(()),
                },
            };
            tokio::select! {
                biased;
                control = writer_control_rx.recv() => match control {
                    Some(WriterControl::Close(frame)) => {
                        let _ = tokio::time::timeout(
                            WS_CLOSE_TIMEOUT,
                            websocket_write.send(Message::Close(frame)),
                        ).await;
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                result = websocket_write.send(message) => {
                    result.context("sending control plane WebSocket message")?;
                }
            }
        }
    }));
    let exit = loop {
        tokio::select! {
            biased;
            writer_result = writer.handle_mut() => {
                let result = writer_result.context("joining control plane WebSocket writer")?;
                return match result {
                    Ok(()) => Err(anyhow!("control plane WebSocket writer exited unexpectedly")),
                    Err(error) => Err(error),
                };
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if enqueue_json(&outbound_tx, &event).is_err() {
                        break WebSocketExit::SlowClient;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    if enqueue_json(&outbound_tx, &RpcResponse::failure(None, "events", format!("application event stream lagged by {count} records"))).is_err() {
                        break WebSocketExit::SlowClient;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break WebSocketExit::Graceful(None);
                }
            },
            event = ui_events.recv() => match event {
                Ok(event) => {
                    // Extension-owned interactions project as read-only
                    // notice cards ("answer in the terminal"); host/TUI-owned
                    // interactions were filtered by the runtime forwarders.
                    if enqueue_json(&outbound_tx, &event).is_err() {
                        break WebSocketExit::SlowClient;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    if enqueue_json(&outbound_tx, &RpcResponse::failure(None, "extension_ui", format!("extension UI event stream lagged by {count} records"))).is_err() {
                        break WebSocketExit::SlowClient;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break WebSocketExit::Graceful(None);
                }
            },
            completed = commands.join_next(), if !commands.is_empty() => match completed {
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(_))) => {
                    // A spawned command could not enqueue its response: the
                    // client stopped reading (outbound queue full) or the
                    // writer stopped. Tear the connection down like any
                    // other outbound failure.
                    break WebSocketExit::SlowClient;
                }
                Some(Err(error)) => {
                    return Err(anyhow!(error).context("joining control plane WebSocket command task"));
                }
                None => {}
            },
            incoming = websocket_read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match parse_input(text.as_bytes()) {
                        Ok(RpcInput::Command { command, session_id }) if command.is_collab_lifecycle() => {
                            let response = dispatch_http_command(&state, command, session_id).await;
                            if enqueue_json(&outbound_tx, &response).is_err() {
                                break WebSocketExit::SlowClient;
                            }
                        }
                        Ok(RpcInput::Command { command, session_id }) if command.runs_inline() => {
                            let response = state.manager.dispatch_inner(command, session_id).await;
                            if enqueue_json(&outbound_tx, &response).is_err() {
                                break WebSocketExit::SlowClient;
                            }
                        }
                        Ok(RpcInput::Command { command, session_id }) if commands.len() >= MAX_CONCURRENT_COMMANDS => {
                            let response = RpcResponse::failure(
                                command.id(),
                                command.command_name(),
                                format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
                            );
                            if enqueue_json(&outbound_tx, &response).is_err() {
                                break WebSocketExit::SlowClient;
                            }
                        }
                        Ok(RpcInput::Command { command, session_id }) => {
                            let manager = state.manager.clone();
                            let outbound_tx = outbound_tx.clone();
                            commands.spawn(async move {
                                let response = manager.dispatch_spawned(command, session_id).await;
                                enqueue_json(&outbound_tx, &response)
                            });
                        }
                        Ok(RpcInput::ExtensionUiResponse(_)) => {
                            if enqueue_json(
                                &outbound_tx,
                                &RpcResponse::failure(None, "extension_ui_response", REMOTE_EXTENSION_UI_ERROR),
                            )
                            .is_err()
                            {
                                break WebSocketExit::SlowClient;
                            }
                        }
                        Err(response) => {
                            if enqueue_json(&outbound_tx, &response).is_err() {
                                break WebSocketExit::SlowClient;
                            }
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    break WebSocketExit::Graceful(Some(CloseFrame {
                        code: CloseCode::Unsupported,
                        reason: "binary messages are not supported".into(),
                    }));
                }
                Some(Ok(Message::Close(_))) | None => break WebSocketExit::Graceful(None),
                Some(Ok(Message::Ping(payload))) => {
                    if enqueue_message(&outbound_tx, Message::Pong(payload)).is_err() {
                        break WebSocketExit::SlowClient;
                    }
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                    break WebSocketExit::Graceful(Some(CloseFrame {
                        code: CloseCode::Size,
                        reason: "message too large".into(),
                    }));
                }
                Some(Err(error)) => {
                    let writer_result = stop_websocket_writer(
                        writer,
                        writer_control_tx,
                        outbound_tx,
                        None,
                    )
                    .await;
                    return match writer_result {
                        Ok(()) => Err(anyhow!(error).context("reading control plane WebSocket")),
                        Err(writer_error) => Err(writer_error.context(format!(
                            "reading control plane WebSocket also failed: {error}"
                        ))),
                    };
                }
            }
        }
    };

    let close_frame = match exit {
        WebSocketExit::Graceful(frame) => frame,
        WebSocketExit::SlowClient => Some(CloseFrame {
            code: CloseCode::Policy,
            reason: "client is not reading messages".into(),
        }),
    };
    stop_websocket_writer(writer, writer_control_tx, outbound_tx, close_frame).await
}

async fn stop_websocket_writer(
    mut writer: AbortTask<Result<()>>,
    writer_control_tx: mpsc::Sender<WriterControl>,
    outbound_tx: mpsc::Sender<Message>,
    frame: Option<CloseFrame>,
) -> Result<()> {
    let _ = writer_control_tx.try_send(WriterControl::Close(frame));
    drop(writer_control_tx);
    drop(outbound_tx);
    match tokio::time::timeout(WS_CLOSE_TIMEOUT, writer.handle_mut()).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow!(error).context("joining control plane WebSocket writer")),
        Err(_) => {
            let handle = writer.handle.take().expect("writer task is present");
            handle.abort();
            match handle.await {
                Err(error) if error.is_cancelled() => {
                    Err(anyhow!("control plane WebSocket writer did not stop promptly"))
                }
                Ok(result) => result,
                Err(error) => Err(anyhow!(error)
                    .context("joining aborted control plane WebSocket writer")),
            }
        }
    }
}

fn enqueue_message(sender: &mpsc::Sender<Message>, message: Message) -> Result<()> {
    sender.try_send(message).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => anyhow!("control plane outbound queue is full"),
        mpsc::error::TrySendError::Closed(_) => anyhow!("control plane outbound writer stopped"),
    })
}

fn enqueue_json<T: Serialize>(sender: &mpsc::Sender<Message>, value: &T) -> Result<()> {
    let text = serde_json::to_string(value).context("serializing control plane message")?;
    enqueue_message(sender, Message::Text(text.into()))
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && headers
            .get(http::header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(CONTENT_TYPE_JSON))
}

fn content_length(headers: &HeaderMap) -> Option<usize> {
    let mut values = headers.get_all(http::header::CONTENT_LENGTH).iter();
    let first = values.next()?.to_str().ok()?.trim().parse::<usize>().ok()?;
    if values.any(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            != Some(first)
    }) {
        return None;
    }
    Some(first)
}

async fn serve_web_page(stream: &mut TcpStream) -> Result<()> {
    if let Ok(dir) = std::env::var(RPI_WEB_DEV_DIR)
        && !dir.trim().is_empty()
    {
        let path = Path::new(dir.trim()).join("index.html");
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return write_response(stream, StatusCode::OK, "text/html; charset=utf-8", &bytes).await;
        }
    }
    let (mime, bytes) = crate::web::index().context("embedded web client assets are missing")?;
    write_response(stream, StatusCode::OK, mime, bytes).await
}

async fn write_plain_response(
    stream: &mut TcpStream,
    status: StatusCode,
    message: &str,
) -> Result<()> {
    write_response(stream, status, "text/plain; charset=utf-8", message.as_bytes()).await
}

async fn write_json_response<T: Serialize>(
    stream: &mut TcpStream,
    status: StatusCode,
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value).context("serializing HTTP RPC response")?;
    write_response(stream, status, CONTENT_TYPE_JSON, &body).await
}

async fn write_response(
    stream: &mut TcpStream,
    status: StatusCode,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = status.canonical_reason().unwrap_or("Error");
    let header = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        status.as_u16(),
        reason,
        content_type,
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::sync::broadcast;
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream,
        tungstenite::client::IntoClientRequest,
    };

    use super::super::{
        collab_service::{CollabConnection, CollabRuntime},
        rpc::RpcCommand,
    };

    struct CollabTestRuntime {
        events: broadcast::Sender<Value>,
    }

    impl CollabTestRuntime {
        fn new() -> Arc<Self> {
            let (events, _) = broadcast::channel(8);
            Arc::new(Self { events })
        }
    }

    #[async_trait::async_trait]
    impl CollabRuntime for CollabTestRuntime {
        fn events(&self) -> broadcast::Receiver<Value> {
            self.events.subscribe()
        }

        async fn snapshot(
            &self,
            _session_id: Option<&str>,
            _max_entries: usize,
            _max_bytes: usize,
        ) -> Result<(String, Value)> {
            Ok((
                "session-1".to_owned(),
                json!({"sessionId":"session-1","truncated":false,"entries":[]}),
            ))
        }

        async fn dispatch(&self, command: RpcCommand, _session_id: String) -> RpcResponse {
            RpcResponse::success(None, command.command_name(), None)
        }
    }

    async fn open_collab_socket(
        connection: CollabConnection,
    ) -> (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        JoinHandle<Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let protocol = "rpi-collab.test";
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream).await.expect("read upgrade request");
            collab_websocket_connection(stream, raw, connection, protocol.to_owned()).await
        });
        let mut request = format!("ws://{address}/collab/ws/test-room")
            .into_client_request()
            .expect("client request");
        request.headers_mut().insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(protocol),
        );
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect WebSocket");
        (socket, server)
    }

    async fn test_collab_connection() -> (CollabService, String, CollabConnection) {
        let service = CollabService::with_runtime(CollabTestRuntime::new());
        let started = service
            .start(None, "http://127.0.0.1:4321")
            .await
            .expect("start room");
        let parsed = pi_coding::collab::parse_link(&started.view_link).expect("parse view link");
        let capability = pi_coding::collab::capability(&parsed.secret.key);
        let connection = service
            .authenticate(&started.room_id, &capability)
            .await
            .expect("authenticate");
        (service, started.room_id, connection)
    }

    async fn next_collab_message(
        socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Message {
        tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("WebSocket message timeout")
            .expect("WebSocket closed without a message")
            .expect("read WebSocket message")
    }

    fn assert_away_close(message: Message) {
        let Message::Close(Some(frame)) = message else {
            panic!("expected Away close, received {message:?}");
        };
        assert_eq!(frame.code, CloseCode::Away);
        assert_eq!(frame.reason, "collaboration room stopped");
    }


    fn headers(input: &[u8]) -> std::result::Result<RawRequest, RequestError> {
        let end = find_header_end(input).expect("header terminator");
        parse_request_headers(&input[..end], input[end + 4..].to_vec())
    }

    #[test]
    fn auth_policy_keeps_default_strict_and_allows_authenticated_remote_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token-file");
        std::fs::write(&token, b"fixture-value").unwrap();
        for address in ["127.0.0.1", "::1"] {
            let address = address.parse().unwrap();
            assert!(
                load_auth_token(address, None, "--listen", ListenAddressPolicy::LoopbackOnly)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::LoopbackOnly,
                )
                .unwrap(),
                Some(b"fixture-value".to_vec())
            );
        }
        for address in ["0.0.0.0", "::", "198.51.100.7", "8.8.8.8"] {
            let address = address.parse().unwrap();
            assert!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::LoopbackOnly,
                )
                .is_err()
            );
            assert!(
                load_auth_token(
                    address,
                    None,
                    "--listen",
                    ListenAddressPolicy::AllowAuthenticatedPlaintextRemote,
                )
                .is_err()
            );
            assert_eq!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::AllowAuthenticatedPlaintextRemote,
                )
                .unwrap(),
                Some(b"fixture-value".to_vec())
            );
        }
    }

    #[test]
    fn token_file_must_be_regular_bounded_trimmed_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_token_file(dir.path()).is_err());
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b" \n\t ").unwrap();
        assert!(read_token_file(&empty).is_err());
        let large = dir.path().join("large");
        std::fs::write(&large, vec![b'x'; 4097]).unwrap();
        assert!(read_token_file(&large).is_err());
        let valid = dir.path().join("valid");
        std::fs::write(&valid, b"  secret-value\n").unwrap();
        assert_eq!(read_token_file(&valid).unwrap(), b"secret-value");
    }

    #[test]
    fn constant_work_comparison_handles_lengths_and_bytes() {
        assert!(constant_work_eq(b"token", b"token"));
        assert!(!constant_work_eq(b"token", b"tokeN"));
        assert!(!constant_work_eq(b"token", b"token-long"));
        assert!(!constant_work_eq(b"", b"token"));
    }

    #[test]
    fn bounded_http_parser_rejects_chunking_queries_and_bad_lengths() {
        assert!(headers(b"POST /rpc?token=x HTTP/1.1\r\nhost: x\r\n\r\n").is_err());
        assert!(headers(b"POST /rpc HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n").is_err());
        let parsed = headers(b"POST /rpc HTTP/1.1\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\n")
            .unwrap();
        assert_eq!(content_length(&parsed.headers), None);
    }

    #[test]
    fn authorization_accepts_exact_bearer_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert!(authorized(&headers, Some(b"secret")));
        assert!(!authorized(&headers, Some(b"wrong")));
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic secret"),
        );
        assert!(!authorized(&headers, Some(b"secret")));
    }

    #[test]
    fn unauthenticated_loopback_rejects_browser_origin() {
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, None));
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer ignored-without-token-policy"),
        );
        assert!(authorized(&headers, None));
        headers.insert(http::header::ORIGIN, HeaderValue::from_static("https://example.test"));
        assert!(!authorized(&headers, None));
    }

    fn protocol_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_str(value).expect("header value"),
        );
        headers
    }

    #[test]
    fn ws_subprotocol_accepts_exact_token_and_echoes_spelling() {
        let headers = protocol_headers("rpi-auth.secret");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")).as_deref(),
            Some("rpi-auth.secret")
        );
        // The exact offered spelling is preserved so the server echoes it.
        let headers = protocol_headers("rpi-auth.secret, something-else");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")).as_deref(),
            Some("rpi-auth.secret")
        );
        let headers = protocol_headers("chat, rpi-auth.secret");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")).as_deref(),
            Some("rpi-auth.secret")
        );
    }

    #[test]
    fn ws_subprotocol_rejects_wrong_empty_and_missing_token() {
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.wrong"), Some(b"secret")),
            None,
            "wrong token must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth."), Some(b"secret")),
            None,
            "empty candidate must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.sec ret"), Some(b"secret")),
            None,
            "whitespace candidate must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.secret"), None),
            None,
            "no configured token must not grant subprotocol auth"
        );
        let mut headers = HeaderMap::new();
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")),
            None,
            "missing header must not authenticate"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("not-an-auth-protocol"),
        );
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")),
            None,
            "unrelated subprotocol must not authenticate"
        );
    }

    #[test]
    fn ws_subprotocol_is_constant_time_and_case_sensitive() {
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.Secret"), Some(b"secret")),
            None,
            "token compare must be case-sensitive"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.secret-long"), Some(b"secret")),
            None,
            "prefix match on a longer token must fail"
        );
    }

    #[tokio::test]
    async fn already_stopped_collaboration_connection_closes_before_hello_or_snapshot() {
        let (service, room_id, connection) = test_collab_connection().await;
        service.stop(&room_id).await.expect("stop room");

        let (mut socket, server) = open_collab_socket(connection).await;

        assert_away_close(next_collab_message(&mut socket).await);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn established_collaboration_connection_closes_away_on_future_stop() {
        let (service, room_id, connection) = test_collab_connection().await;
        let (mut socket, server) = open_collab_socket(connection).await;

        assert!(matches!(next_collab_message(&mut socket).await, Message::Text(_)));
        assert!(matches!(next_collab_message(&mut socket).await, Message::Binary(_)));
        service.stop(&room_id).await.expect("stop room");
        assert_away_close(next_collab_message(&mut socket).await);
        server.await.expect("server task").expect("server result");
    }

    #[test]
    fn collaboration_browser_path_accepts_only_valid_room_ids() {
        assert_eq!(collab_room_id("/collab/ws/room-123_abc"), Some("room-123_abc"));
        assert_eq!(collab_room_id("/collab/ws/"), None);
        assert_eq!(collab_room_id("/collab/ws/room/child"), None);
        assert_eq!(collab_room_id("/collab/ws/room?secret=x"), None);
        assert_eq!(collab_room_id("/collab/ws/room%23fragment"), None);
    }
}