use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
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

use super::rpc::{
    MAX_RPC_MESSAGE_BYTES, RpcDispatcher, RpcInput, RpcResponse, parse_input,
    project_application_event, project_extension_ui_event,
};

const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_CONNECTION_TASKS: usize = 64;
const OUTBOUND_QUEUE_CAPACITY: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WS_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const CONTENT_TYPE_JSON: &str = "application/json";
const REMOTE_EXTENSION_UI_ERROR: &str = "remote interactive extension UI is disabled";

#[derive(Clone)]
pub struct ListenConfig {
    pub address: SocketAddr,
    pub token_file: Option<PathBuf>,
}

pub struct ListenHandle {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl ListenHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.address
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
    dispatcher: RpcDispatcher,
    extension_ui: ExtensionUiAdapter,
    token: Option<Arc<[u8]>>,
}

pub async fn start(
    application: Application,
    extension_ui: ExtensionUiAdapter,
    config: ListenConfig,
) -> Result<ListenHandle> {
    let token = load_auth_token(config.address.ip(), config.token_file.as_deref())?;
    let listener = TcpListener::bind(config.address)
        .await
        .with_context(|| format!("binding control plane to {}", config.address))?;
    let address = listener
        .local_addr()
        .context("reading control plane listener address")?;
    let state = ServerState {
        dispatcher: RpcDispatcher::new(application),
        extension_ui,
        token: token.map(Arc::from),
    };
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(run_listener(listener, state, shutdown_rx));
    Ok(ListenHandle {
        address,
        shutdown,
        task,
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
        if raw.method != Method::GET || raw.path != "/ws" {
            write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
            return Ok(());
        }
        if !authorized(&raw.headers, state.token.as_deref()) {
            write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized").await?;
            return Ok(());
        }
        return websocket_connection(stream, raw, state).await;
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
        Ok(RpcInput::Command(command)) => state.dispatcher.dispatch(command).await,
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
async fn websocket_connection(stream: TcpStream, raw: RawRequest, state: ServerState) -> Result<()> {
    let mut events = state.dispatcher.application().subscribe();
    let mut ui_events = state.extension_ui.subscribe_non_interactive();
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
        |request: &Request, response: Response| -> std::result::Result<Response, ErrorResponse> {
            if request.uri().path() == "/ws" {
                Ok(response)
            } else {
                let mut error = ErrorResponse::new(Some("not found".into()));
                *error.status_mut() = StatusCode::NOT_FOUND;
                Err(error)
            }
        },
        Some(config),
    )
    .await
    .context("upgrading control plane WebSocket")?;

    let (mut websocket_write, mut websocket_read) = websocket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAPACITY);
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
                    if enqueue_json(&outbound_tx, &project_application_event(event)?).is_err() {
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
                    if let Some(request) = project_extension_ui_event(event)?
                        && enqueue_json(&outbound_tx, &request).is_err()
                    {
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
            incoming = websocket_read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let response = match parse_input(text.as_bytes()) {
                        Ok(RpcInput::Command(command)) => state.dispatcher.dispatch(command).await,
                        Ok(RpcInput::ExtensionUiResponse(_)) => RpcResponse::failure(None, "extension_ui_response", REMOTE_EXTENSION_UI_ERROR),
                        Err(response) => response,
                    };
                    if enqueue_json(&outbound_tx, &response).is_err() {
                        break WebSocketExit::SlowClient;
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

fn authorized(headers: &HeaderMap, token: Option<&[u8]>) -> bool {
    let Some(token) = token else {
        // Loopback is a network boundary, not a browser-origin boundary.
        // Native clients omit Origin; browser HTTP/WebSocket clients send it.
        return !headers.contains_key(http::header::ORIGIN);
    };
    let Some(value) = headers.get(http::header::AUTHORIZATION) else {
        return false;
    };
    let Some(value) = value.as_bytes().strip_prefix(b"Bearer ") else {
        return false;
    };
    !value.is_empty()
        && !value.iter().any(|byte| byte.is_ascii_whitespace())
        && constant_work_eq(value, token)
}

fn constant_work_eq(candidate: &[u8], expected: &[u8]) -> bool {
    let mut different = candidate.len() ^ expected.len();
    let length = candidate.len().max(expected.len());
    for index in 0..length {
        let left = candidate.get(index).copied().unwrap_or(0);
        let right = expected.get(index).copied().unwrap_or(0);
        different |= usize::from(left ^ right);
    }
    different == 0
}

fn load_auth_token(address: IpAddr, path: Option<&Path>) -> Result<Option<Vec<u8>>> {
    if !address.is_loopback() && path.is_none() {
        bail!("--listen on a non-loopback address requires --listen-token-file");
    }
    path.map(read_token_file).transpose()
}

fn read_token_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading control plane token file metadata {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("control plane token path must be a regular file");
    }
    if metadata.len() > 4096 {
        bail!("control plane token file exceeds 4096 bytes");
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading control plane token file {}", path.display()))?;
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    let token = bytes[start..end].to_vec();
    if token.is_empty() {
        bail!("control plane token file must not be empty");
    }
    if token.iter().any(|byte| byte.is_ascii_whitespace()) {
        bail!("control plane token must not contain whitespace");
    }
    Ok(token)
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

    fn headers(input: &[u8]) -> std::result::Result<RawRequest, RequestError> {
        let end = find_header_end(input).expect("header terminator");
        parse_request_headers(&input[..end], input[end + 4..].to_vec())
    }

    #[test]
    fn auth_policy_requires_token_for_non_loopback_and_honors_loopback_token() {
        assert!(load_auth_token("127.0.0.1".parse().unwrap(), None)
            .unwrap()
            .is_none());
        assert!(load_auth_token("::1".parse().unwrap(), None)
            .unwrap()
            .is_none());
        assert!(load_auth_token("0.0.0.0".parse().unwrap(), None).is_err());
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
}