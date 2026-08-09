//! OpenAI Codex Responses provider with SSE and WebSocket transports.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use base64::Engine as _;
use futures_util::{FutureExt, SinkExt, StreamExt};
use reqwest::{header::HeaderMap, Response, StatusCode};
use serde_json::{json, Map, Value};
use tokio::{
    net::TcpStream,
    sync::{
        mpsc::{unbounded_channel, UnboundedSender},
        Mutex, OwnedMutexGuard,
    },
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, handshake::client::Response as WebSocketResponse, Message},
    MaybeTlsStream, WebSocketStream,
};

use crate::*;
use super::{common, responses};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
const SESSION_WEBSOCKET_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_WEBSOCKET_MAX_AGE: Duration = Duration::from_secs(55 * 60);
const WEBSOCKET_CONNECTION_LIMIT_REACHED: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND: &str = "previous_response_not_found";
const RETRY_BASE_DELAY_MS: u64 = 1_000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Default)]
pub struct OpenAICodexResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub text_verbosity: Option<String>,
    pub tool_choice: Option<String>,
}

impl From<StreamOptions> for OpenAICodexResponsesOptions {
    fn from(stream: StreamOptions) -> Self { Self { stream, ..Self::default() } }
}

#[derive(Clone)]
struct Continuation {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

struct CachedConnection {
    socket: ClientSocket,
    created_at: Instant,
    last_used: Instant,
    continuation: Option<Continuation>,
    closed: bool,
}

type SharedConnection = Arc<Mutex<CachedConnection>>;

static WEBSOCKET_CACHE: LazyLock<Mutex<HashMap<String, SharedConnection>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SSE_FALLBACK_SESSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug)]
enum CodexError {
    Api { code: Option<String>, message: String },
    Protocol(String),
    Transport(String),
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api { message, .. } | Self::Protocol(message) | Self::Transport(message) => {
                formatter.write_str(message)
            }
        }
    }
}
impl std::error::Error for CodexError {}
impl CodexError {
    fn code(&self) -> Option<&str> {
        match self { Self::Api { code, .. } => code.as_deref(), _ => None }
    }
    fn is_non_transport(&self) -> bool { matches!(self, Self::Api { .. } | Self::Protocol(_)) }
}

struct WebSocketFailure { error: CodexError, started: bool }

enum SocketLease {
    Cached {
        session_id: String,
        shared: SharedConnection,
        guard: OwnedMutexGuard<CachedConnection>,
    },
    OneShot(ClientSocket),
}

impl SocketLease {
    fn socket_mut(&mut self) -> &mut ClientSocket {
        match self { Self::Cached { guard, .. } => &mut guard.socket, Self::OneShot(socket) => socket }
    }
    fn continuation_mut(&mut self) -> Option<&mut Option<Continuation>> {
        match self { Self::Cached { guard, .. } => Some(&mut guard.continuation), Self::OneShot(_) => None }
    }
    async fn finish(mut self, keep: bool, continuation: Option<Continuation>) {
        match &mut self {
            Self::Cached { session_id, shared, guard } => {
                if keep && !guard.closed {
                    guard.last_used = Instant::now();
                    guard.continuation = continuation;
                } else {
                    let _ = guard.socket.close(None).await;
                    guard.closed = true;
                    let session_id = session_id.clone();
                    let shared = Arc::clone(shared);
                    drop(self);
                    remove_cached_connection(&session_id, &shared).await;
                }
            }
            Self::OneShot(socket) => { let _ = socket.close(None).await; }
        }
    }
}

pub fn register_openai_codex_responses() {
    register_api_provider(ApiProvider {
        api: API_OPENAI_CODEX_RESPONSES.into(),
        stream: Arc::new(|model, context, options| async move {
            stream_openai_codex_responses(model, context, OpenAICodexResponsesOptions::from(options))
        }.boxed()),
        stream_simple: Arc::new(|model, context, options| async move {
            stream_simple_openai_codex_responses(model, context, options)
        }.boxed()),
        generate_image: None,
    }, None);
}

pub fn stream_simple_openai_codex_responses(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let requested_max = options.stream.max_tokens.unwrap_or(model.max_tokens);
    let mut stream = options.stream;
    stream.max_tokens = Some(clamp_max_tokens_to_context(&model, &context, requested_max));
    let reasoning_effort = options.reasoning.and_then(|level| {
        let clamped = clamp_thinking_level(&model, thinking_level_name(level));
        (clamped != "off").then(|| clamped.to_owned())
    });
    stream_openai_codex_responses(model, context, OpenAICodexResponsesOptions {
        stream,
        reasoning_effort,
        ..OpenAICodexResponsesOptions::default()
    })
}

pub fn stream_openai_codex_responses(
    model: Model,
    context: Context,
    options: OpenAICodexResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let output_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);
        match run_codex(&model, &context, &options, &output_stream, &mut output).await {
            Ok(()) => {
                output_stream.push(AssistantMessageEvent::Done {
                    reason: output.stop_reason,
                    message: output.clone(),
                }).await;
                output_stream.end(Some(output)).await;
            }
            Err(error) => {
                output.stop_reason = if common::is_aborted(&options.stream) {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(if output.stop_reason == StopReason::Aborted {
                    "Request was aborted".into()
                } else {
                    sanitize_error(&error.to_string(), &model, &options.stream)
                });
                output_stream.push(AssistantMessageEvent::Error {
                    reason: output.stop_reason,
                    error: output.clone(),
                }).await;
                output_stream.end(Some(output)).await;
            }
        }
    });
    stream
}

async fn run_codex(
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    stream: &AssistantMessageEventStream,
    output: &mut AssistantMessage,
) -> Result<()> {
    let token = options.stream.api_key.as_deref().filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("No API key for provider: {}", model.provider))?;
    let account_id = extract_account_id(token)?;
    let cache_session_id = if options.stream.cache_retention == CacheRetention::None {
        None
    } else {
        options.stream.session_id.as_deref().filter(|value| !value.is_empty())
            .map(responses::clamp_prompt_cache_key)
    };
    let route_cache_id = cache_session_id.as_deref().map(|session_id| {
        format!("{}\u{1f}{}\u{1f}{}", resolve_codex_websocket_url(&model.base_url), account_id, session_id)
    });
    let compat_model = codex_compat_model(model);
    let grammar_properties = responses::grammar_tool_input_properties(
        &context.tools,
        responses::get_responses_compat(&compat_model).supports_openai_grammar_tools,
    )?;
    let body = build_codex_body(model, context, options, cache_session_id.as_deref())?;
    let body = common::apply_provider_request(body, model, &options.stream).await?;
    if !body.is_object() { return Err(anyhow!("Codex payload hook must return a JSON object")); }

    let request_id = cache_session_id.clone().unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let sse_headers = common::apply_provider_headers(
        build_codex_headers(
            model, &options.stream, token, &account_id, cache_session_id.as_deref(), false,
        )?,
        model,
        &options.stream,
    ).await?;
    let websocket_headers = common::apply_provider_headers(
        build_codex_headers(
            model, &options.stream, token, &account_id, Some(&request_id), true,
        )?,
        model,
        &options.stream,
    ).await?;
    let (event_tx, mut event_rx) = unbounded_channel();
    let drain_stream = stream.clone();
    let drainer = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await { drain_stream.push(event).await; }
    });
    let mut start_emitted = false;
    let transport = options.stream.transport;
    let fallback_active = transport == Transport::Auto && match route_cache_id.as_deref() {
        Some(route_id) => SSE_FALLBACK_SESSIONS.lock().await.contains(route_id),
        None => false,
    };

    let result = if transport != Transport::Sse && !fallback_active {
        let mut retried_connection_limit = false;
        let mut retried_missing_continuation = false;
        loop {
            match process_websocket(
                model,
                options,
                &body,
                &websocket_headers,
                route_cache_id.as_deref(),
                &grammar_properties,
                output,
                &event_tx,
                stream,
                &mut start_emitted,
            ).await {
                Ok(()) => break Ok(()),
                Err(failure) => {
                    if common::is_aborted(&options.stream) { break Err(anyhow!("Request was aborted")); }
                    if failure.error.code() == Some(PREVIOUS_RESPONSE_NOT_FOUND)
                        && !retried_missing_continuation
                    {
                        retried_missing_continuation = true;
                        continue;
                    }
                    let connection_limit = failure.error.code() == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED)
                        && !failure.started;
                    if connection_limit && !retried_connection_limit {
                        retried_connection_limit = true;
                        continue;
                    }
                    if failure.error.is_non_transport() && !connection_limit {
                        break Err(anyhow!(failure.error));
                    }
                    if transport != Transport::Auto || failure.started {
                        break Err(anyhow!(failure.error));
                    }
                    output.diagnostics.push(Diagnostic {
                        code: Some("provider_transport_failure".into()),
                        message: sanitize_error(&failure.error.to_string(), model, &options.stream),
                    });
                    if let Some(route_id) = route_cache_id.as_deref() {
                        SSE_FALLBACK_SESSIONS.lock().await.insert(route_id.to_owned());
                    }
                    debug_assert!(!failure.started);
                    break process_sse(
                        model,
                        options,
                        &body,
                        &sse_headers,
                        &grammar_properties,
                        output,
                        &event_tx,
                        stream,
                        &mut start_emitted,
                    ).await;
                }
            }
        }
    } else {
        process_sse(
            model,
            options,
            &body,
            &sse_headers,
            &grammar_properties,
            output,
            &event_tx,
            stream,
            &mut start_emitted,
        ).await
    };
    drop(event_tx);
    let _ = drainer.await;
    result?;
    if common::is_aborted(&options.stream) { return Err(anyhow!("Request was aborted")); }
    if output.stop_reason == StopReason::Pending {
        return Err(anyhow!("Codex stream ended without a terminal response event"));
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(anyhow!("Codex stream ended with an error status"));
    }
    Ok(())
}

fn build_codex_body(
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    session_id: Option<&str>,
) -> Result<Value> {
    let compat_model = codex_compat_model(model);
    let mut input_context = context.clone();
    input_context.system_prompt.clear();
    let response_options = responses::OpenAIResponsesOptions {
        stream: options.stream.clone(),
        reasoning_effort: options.reasoning_effort.clone(),
        reasoning_summary: options.reasoning_summary.clone().or_else(|| Some("auto".into())),
        service_tier: options.service_tier.clone(),
        tool_choice: options.tool_choice.as_ref().map(|choice| json!(choice)),
        responses_stateful_chain: false,
    };
    let mut body = responses::build_responses_params(&compat_model, &input_context, &response_options)?;
    let object = body.as_object_mut().expect("Responses body is an object");
    object.insert("instructions".into(), json!(if context.system_prompt.is_empty() {
        "You are a helpful assistant."
    } else {
        context.system_prompt.as_str()
    }));
    object.insert("store".into(), json!(false));
    object.insert("stream".into(), json!(true));
    object.insert("text".into(), json!({ "verbosity": options.text_verbosity.as_deref().unwrap_or("low") }));
    object.insert("include".into(), json!(["reasoning.encrypted_content"]));
    object.insert("tool_choice".into(), json!(options.tool_choice.as_deref().unwrap_or("auto")));
    object.insert("parallel_tool_calls".into(), json!(true));
    object.remove("max_output_tokens");
    object.remove("prompt_cache_retention");
    object.remove("prompt_cache_options");
    if options.reasoning_effort.is_none() { object.remove("reasoning"); }
    if let Some(session_id) = session_id {
        object.insert("prompt_cache_key".into(), json!(session_id));
    } else {
        object.remove("prompt_cache_key");
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function")
                && tool.get("strict") == Some(&Value::Bool(false))
            {
                tool.as_object_mut().expect("tool object").insert("strict".into(), Value::Null);
            }
        }
    }
    Ok(body)
}

fn codex_compat_model(model: &Model) -> Model {
    let mut model = model.clone();
    let mut compat = model.compat.take().and_then(|value| value.as_object().cloned()).unwrap_or_default();
    compat.entry("supportsStrictMode").or_insert(Value::Bool(true));
    model.compat = Some(Value::Object(compat));
    model
}

#[allow(clippy::too_many_arguments)]
async fn process_websocket(
    model: &Model, options: &OpenAICodexResponsesOptions, full_body: &Value, headers: &HeaderMap,
    session_id: Option<&str>, grammar_properties: &HashMap<String, String>, output: &mut AssistantMessage,
    event_tx: &UnboundedSender<AssistantMessageEvent>, output_stream: &AssistantMessageEventStream,
    start_emitted: &mut bool,
) -> std::result::Result<(), WebSocketFailure> {
    let mut lease = acquire_websocket(
        &resolve_codex_websocket_url(&model.base_url), headers, session_id, model, &options.stream,
    ).await.map_err(|error| WebSocketFailure { error, started: false })?;
    let use_cached_context = matches!(options.stream.transport, Transport::WebSocketCached | Transport::Auto);
    let request_body = if use_cached_context { cached_request_body(&mut lease, full_body) } else { full_body.clone() };
    let mut envelope = request_body.clone();
    envelope.as_object_mut().expect("Codex body object").insert("type".into(), json!("response.create"));
    if let Err(error) = lease.socket_mut().send(Message::Text(envelope.to_string().into())).await {
        lease.finish(false, None).await;
        return Err(WebSocketFailure { error: CodexError::Transport(format!("WebSocket send failed: {error}")), started: false });
    }
    let mut state = responses::StreamState::with_grammar_input_properties(grammar_properties.clone());
    let mut started = false;
    let result = loop {
        let message = match websocket_next(lease.socket_mut(), &options.stream).await {
            Ok(Some(message)) => message,
            Ok(None) => break Err(CodexError::Transport("WebSocket stream closed before response.completed".into())),
            Err(error) => break Err(error),
        };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text,
                Err(error) => break Err(CodexError::Protocol(format!("Invalid Codex WebSocket UTF-8: {error}"))),
            },
            Message::Ping(payload) => {
                if let Err(error) = lease.socket_mut().send(Message::Pong(payload)).await {
                    break Err(CodexError::Transport(format!("WebSocket pong failed: {error}")));
                }
                continue;
            }
            Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(frame) => {
                let suffix = frame.map_or_else(String::new, |frame| format!(" {} {}", u16::from(frame.code), frame.reason));
                break Err(CodexError::Transport(format!("WebSocket closed{suffix}")));
            }
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => break Err(CodexError::Protocol(format!("Invalid Codex WebSocket JSON: {error}"))),
        };
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(event_type, "error" | "response.failed") {
            started = true;
            if !*start_emitted {
                *start_emitted = true;
                output_stream.push(AssistantMessageEvent::Start { partial: output.clone() }).await;
            }
        }
        match handle_codex_event(value, output, &mut state, model, options.service_tier.as_deref(), event_tx) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(error) => break Err(error),
        }
    };
    match result {
        Ok(()) => {
            state.materialize(output);
            let continuation = if use_cached_context {
                output.response_id.as_deref().map(|response_id| Continuation {
                    last_request_body: full_body.clone(),
                    last_response_id: response_id.to_owned(),
                    last_response_items: response_items(output, grammar_properties),
                })
            } else { None };
            lease.finish(!common::is_aborted(&options.stream), continuation).await;
            Ok(())
        }
        Err(error) => {
            if error.code() == Some(PREVIOUS_RESPONSE_NOT_FOUND) {
                if let Some(continuation) = lease.continuation_mut() { *continuation = None; }
            }
            lease.finish(false, None).await;
            Err(WebSocketFailure { error, started })
        }
    }
}

async fn websocket_next(socket: &mut ClientSocket, options: &StreamOptions) -> std::result::Result<Option<Message>, CodexError> {
    let receive = async {
        match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => Err(CodexError::Transport("Request was aborted".into())),
                message = socket.next() => message.transpose().map_err(|error| CodexError::Transport(format!("WebSocket receive failed: {error}"))),
            },
            None => socket.next().await.transpose().map_err(|error| CodexError::Transport(format!("WebSocket receive failed: {error}"))),
        }
    };
    match options.timeout_ms {
        Some(timeout) if timeout > 0 => tokio::time::timeout(Duration::from_millis(timeout), receive).await
            .map_err(|_| CodexError::Transport(format!("WebSocket idle timeout after {timeout}ms")))?,
        _ => receive.await,
    }
}

async fn acquire_websocket(
    url: &str, headers: &HeaderMap, session_id: Option<&str>, model: &Model, options: &StreamOptions,
) -> std::result::Result<SocketLease, CodexError> {
    let Some(session_id) = session_id else {
        let (socket, response) = connect_websocket(url, headers, options).await?;
        notify_websocket_response(options, &response, model).await?;
        return Ok(SocketLease::OneShot(socket));
    };
    let existing = WEBSOCKET_CACHE.lock().await.get(session_id).cloned();
    if let Some(shared) = existing {
        if let Ok(mut guard) = Arc::clone(&shared).try_lock_owned() {
            let now = Instant::now();
            let expired = guard.closed
                || now.duration_since(guard.created_at) >= SESSION_WEBSOCKET_MAX_AGE
                || now.duration_since(guard.last_used) >= SESSION_WEBSOCKET_CACHE_TTL;
            if !expired {
                return Ok(SocketLease::Cached { session_id: session_id.to_owned(), shared, guard });
            }
            let _ = guard.socket.close(None).await;
            guard.closed = true;
            drop(guard);
            remove_cached_connection(session_id, &shared).await;
        } else {
            let (socket, response) = connect_websocket(url, headers, options).await?;
            notify_websocket_response(options, &response, model).await?;
            return Ok(SocketLease::OneShot(socket));
        }
    }
    let (socket, response) = connect_websocket(url, headers, options).await?;
    notify_websocket_response(options, &response, model).await?;
    let now = Instant::now();
    let shared = Arc::new(Mutex::new(CachedConnection { socket, created_at: now, last_used: now, continuation: None, closed: false }));
    WEBSOCKET_CACHE.lock().await.insert(session_id.to_owned(), Arc::clone(&shared));
    let guard = Arc::clone(&shared).lock_owned().await;
    Ok(SocketLease::Cached { session_id: session_id.to_owned(), shared, guard })
}

async fn connect_websocket(
    url: &str, headers: &HeaderMap, options: &StreamOptions,
) -> std::result::Result<(ClientSocket, WebSocketResponse), CodexError> {
    let mut request = url.into_client_request()
        .map_err(|error| CodexError::Transport(format!("Invalid WebSocket URL: {error}")))?;
    for (name, value) in headers {
        request.headers_mut().insert(name, value.clone());
    }
    let connect = connect_async(request);
    let timeout_ms = options.websocket_connect_timeout_ms.unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);
    let connect = async {
        match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => Err(CodexError::Transport("Request was aborted".into())),
                result = connect => result.map_err(|error| CodexError::Transport(format!("WebSocket connect failed: {error}"))),
            },
            None => connect.await.map_err(|error| CodexError::Transport(format!("WebSocket connect failed: {error}"))),
        }
    };
    if timeout_ms == 0 { connect.await } else {
        tokio::time::timeout(Duration::from_millis(timeout_ms), connect).await
            .map_err(|_| CodexError::Transport(format!("WebSocket connect timeout after {timeout_ms}ms")))?
    }
}

async fn notify_websocket_response(
    options: &StreamOptions,
    response: &WebSocketResponse,
    model: &Model,
) -> std::result::Result<(), CodexError> {
    common::notify_response_headers(
        options,
        response.status().as_u16(),
        response.headers(),
        model,
    ).await.map_err(|error| CodexError::Protocol(error.to_string()))
}

async fn remove_cached_connection(session_id: &str, shared: &SharedConnection) {
    let mut cache = WEBSOCKET_CACHE.lock().await;
    if cache.get(session_id).is_some_and(|current| Arc::ptr_eq(current, shared)) { cache.remove(session_id); }
}

fn cached_request_body(lease: &mut SocketLease, body: &Value) -> Value {
    let Some(continuation) = lease.continuation_mut().and_then(|continuation| continuation.as_ref()) else { return body.clone(); };
    let Some(current_input) = body.get("input").and_then(Value::as_array) else { return body.clone(); };
    let Some(last_input) = continuation.last_request_body.get("input").and_then(Value::as_array) else { return body.clone(); };
    if body_without_continuation(body) != body_without_continuation(&continuation.last_request_body) { return body.clone(); }
    let mut baseline = last_input.clone();
    baseline.extend(continuation.last_response_items.clone());
    if current_input.len() < baseline.len() || current_input[..baseline.len()] != baseline { return body.clone(); }
    let mut request = body.clone();
    let object = request.as_object_mut().expect("Codex body object");
    object.insert("previous_response_id".into(), json!(continuation.last_response_id));
    object.insert("input".into(), Value::Array(current_input[baseline.len()..].to_vec()));
    request
}

fn body_without_continuation(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("input");
        object.remove("previous_response_id");
    }
    body
}

fn response_items(output: &AssistantMessage, grammar_properties: &HashMap<String, String>) -> Vec<Value> {
    let mut items = Vec::new();
    for block in &output.content {
        match block {
            ContentBlock::Thinking { thinking_signature: Some(signature), .. } => {
                if let Ok(item) = serde_json::from_str(signature) { items.push(item); }
            }
            ContentBlock::Text { text, text_signature } => {
                let (id, phase) = text_signature.as_deref().map(parse_text_signature)
                    .unwrap_or_else(|| (format!("msg_pi_{}", items.len()), None));
                let mut item = Map::from_iter([
                    ("type".into(), json!("message")),
                    ("role".into(), json!("assistant")),
                    ("status".into(), json!("completed")),
                    ("id".into(), json!(id)),
                    ("content".into(), json!([{ "type": "output_text", "text": text, "annotations": [] }])),
                ]);
                if let Some(phase) = phase { item.insert("phase".into(), json!(phase)); }
                items.push(Value::Object(item));
            }
            ContentBlock::ToolCall(call) => {
                let mut ids = call.id.split('|');
                let call_id = ids.next().unwrap_or("");
                let item_id = ids.next().unwrap_or("");
                let mut item = Map::new();
                if let Some(property) = grammar_properties.get(&call.name) {
                    item.insert("type".into(), json!("custom_tool_call"));
                    item.insert("call_id".into(), json!(call_id));
                    item.insert("name".into(), json!(call.name));
                    item.insert("input".into(), call.arguments.get(property).cloned().unwrap_or(Value::String(String::new())));
                } else {
                    item.insert("type".into(), json!("function_call"));
                    item.insert("call_id".into(), json!(call_id));
                    item.insert("name".into(), json!(call.name));
                    item.insert("arguments".into(), json!(call.arguments.to_string()));
                }
                if !item_id.is_empty() { item.insert("id".into(), json!(item_id)); }
                items.push(Value::Object(item));
            }
            _ => {}
        }
    }
    items
}

fn parse_text_signature(signature: &str) -> (String, Option<String>) {
    if let Ok(value) = serde_json::from_str::<Value>(signature) {
        if value.get("v").and_then(Value::as_i64) == Some(1) {
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                let phase = value.get("phase").and_then(Value::as_str)
                    .filter(|phase| matches!(*phase, "commentary" | "final_answer"))
                    .map(str::to_owned);
                return (id.to_owned(), phase);
            }
        }
    }
    (signature.to_owned(), None)
}

#[allow(clippy::too_many_arguments)]
async fn process_sse(
    model: &Model,
    options: &OpenAICodexResponsesOptions,
    body: &Value,
    headers: &HeaderMap,
    grammar_properties: &HashMap<String, String>,
    output: &mut AssistantMessage,
    event_tx: &UnboundedSender<AssistantMessageEvent>,
    output_stream: &AssistantMessageEventStream,
    start_emitted: &mut bool,
) -> Result<()> {
    let client = common::client(&options.stream)?;
    let response = send_codex_sse_with_retry(
        &client,
        &resolve_codex_url(&model.base_url),
        headers,
        serde_json::to_string(body)?,
        model,
        &options.stream,
    ).await?;
    if !*start_emitted {
        *start_emitted = true;
        output_stream.push(AssistantMessageEvent::Start { partial: output.clone() }).await;
    }
    let mut state = responses::StreamState::with_grammar_input_properties(grammar_properties.clone());
    consume_codex_sse(response, &options.stream, |value| {
        handle_codex_event(value, output, &mut state, model, options.service_tier.as_deref(), event_tx)
    }).await?;
    if !state.saw_terminal { return Err(anyhow!("Codex SSE stream ended before a terminal response event")); }
    state.materialize(output);
    Ok(())
}

async fn send_codex_sse_with_retry(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    body: String,
    model: &Model,
    options: &StreamOptions,
) -> Result<Response> {
    for attempt in 0..=options.max_retries {
        if common::is_aborted(options) { return Err(anyhow!("Request was aborted")); }
        let response = match send_http_request(client.post(url).headers(headers.clone()).body(body.clone()), options).await {
            Ok(response) => response,
            Err(error) if attempt < options.max_retries && !common::is_aborted(options) => {
                abortable_sleep(options, retry_backoff(attempt)).await?;
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        common::notify_response(options, &response, model).await?;
        if response.status().is_success() { return Ok(response); }
        let status = response.status();
        let response_headers = response.headers().clone();
        let text = read_response_text(response, options).await?;
        if attempt < options.max_retries && codex_retryable(status, &text) {
            let delay = retry_after_delay(&response_headers)
                .map(|delay| validate_retry_delay(delay, options)).transpose()?
                .unwrap_or_else(|| retry_backoff(attempt));
            abortable_sleep(options, delay).await?;
            continue;
        }
        return Err(anyhow!(format_codex_http_error(status, &text)));
    }
    Err(anyhow!("Codex retry loop ended unexpectedly"))
}

async fn send_http_request(request: reqwest::RequestBuilder, options: &StreamOptions) -> Result<Response> {
    let send = async {
        match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
                response = request.send() => Ok(response?),
            },
            None => Ok(request.send().await?),
        }
    };
    match options.timeout_ms {
        Some(timeout) if timeout > 0 => tokio::time::timeout(Duration::from_millis(timeout), send).await
            .map_err(|_| anyhow!("Codex SSE response headers timed out after {timeout}ms"))?,
        _ => send.await,
    }
}

async fn read_response_text(response: Response, options: &StreamOptions) -> Result<String> {
    match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
            body = response.text() => Ok(body.unwrap_or_default()),
        },
        None => Ok(response.text().await.unwrap_or_default()),
    }
}

async fn abortable_sleep(options: &StreamOptions, delay: Duration) -> Result<()> {
    match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
            _ = tokio::time::sleep(delay) => Ok(()),
        },
        None => { tokio::time::sleep(delay).await; Ok(()) }
    }
}

fn retry_backoff(attempt: usize) -> Duration {
    Duration::from_millis(RETRY_BASE_DELAY_MS.saturating_mul(1_u64 << attempt.min(20)))
}

fn codex_retryable(status: StatusCode, text: &str) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS && terminal_rate_limit(text) { return false; }
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
        || regex::Regex::new("(?i)rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused")
            .expect("static regex").is_match(text)
}

fn terminal_rate_limit(text: &str) -> bool {
    regex::Regex::new("(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing")
        .expect("static regex").is_match(text)
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    if let Some(milliseconds) = headers.get("retry-after-ms").and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok()).filter(|value| value.is_finite())
    {
        return Some(Duration::from_millis(milliseconds.max(0.0) as u64));
    }
    let retry_after = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = retry_after.parse::<f64>() {
        if seconds.is_finite() { return Some(Duration::from_millis((seconds.max(0.0) * 1_000.0) as u64)); }
    }
    let date = chrono::DateTime::parse_from_rfc2822(retry_after).ok()?.with_timezone(&chrono::Utc);
    Some(Duration::from_millis((date - chrono::Utc::now()).num_milliseconds().max(0) as u64))
}

fn validate_retry_delay(delay: Duration, options: &StreamOptions) -> Result<Duration> {
    let limit = options.max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if limit > 0 && delay > Duration::from_millis(limit) {
        return Err(anyhow!("Server requested {}s retry delay (max: {}s)",
            delay.as_millis().div_ceil(1_000), limit.div_ceil(1_000)));
    }
    Ok(delay)
}

fn format_codex_http_error(status: StatusCode, text: &str) -> String {
    let parsed = serde_json::from_str::<Value>(text).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let code = error.and_then(|value| value.get("code").or_else(|| value.get("type")))
        .and_then(Value::as_str).unwrap_or("");
    if status == StatusCode::TOO_MANY_REQUESTS
        || matches!(code, "usage_limit_reached" | "usage_not_included" | "rate_limit_exceeded")
    {
        let plan = error.and_then(|value| value.get("plan_type")).and_then(Value::as_str)
            .map(|plan| format!(" ({} plan)", plan.to_lowercase())).unwrap_or_default();
        return format!("You have hit your ChatGPT usage limit{plan}.");
    }
    error.and_then(|value| value.get("message")).and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| if text.trim().is_empty() { "Request failed" } else { text.trim() })
        .to_owned()
}

async fn consume_codex_sse<F>(response: Response, options: &StreamOptions, mut handle: F) -> Result<()>
where
    F: FnMut(Value) -> std::result::Result<bool, CodexError>,
{
    let mut bytes = response.bytes_stream();
    let mut pending = String::new();
    loop {
        let next = match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => return Err(anyhow!("Request was aborted")),
                chunk = bytes.next() => chunk,
            },
            None => bytes.next().await,
        };
        let Some(chunk) = next else { break };
        pending.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some((position, separator_len)) = sse_separator(&pending) {
            let event = pending[..position].to_owned();
            pending.drain(..position + separator_len);
            if let Some(data) = sse_data(&event) {
                if data == "[DONE]" { continue; }
                let value = serde_json::from_str::<Value>(&data)
                    .map_err(|error| anyhow!(CodexError::Protocol(format!("Invalid Codex SSE JSON: {error}"))))?;
                if handle(value).map_err(anyhow::Error::new)? { return Ok(()); }
            }
        }
    }
    Ok(())
}

fn sse_separator(input: &str) -> Option<(usize, usize)> {
    input.find("\r\n\r\n").map(|position| (position, 4))
        .or_else(|| input.find("\n\n").map(|position| (position, 2)))
}

fn sse_data(event: &str) -> Option<String> {
    let lines: Vec<&str> = event.lines()
        .filter_map(|line| line.strip_suffix('\r').unwrap_or(line).strip_prefix("data:"))
        .map(str::trim_start)
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n").trim().to_owned())
}

fn handle_codex_event(
    mut event: Value,
    output: &mut AssistantMessage,
    state: &mut responses::StreamState,
    model: &Model,
    requested_service_tier: Option<&str>,
    event_tx: &UnboundedSender<AssistantMessageEvent>,
) -> std::result::Result<bool, CodexError> {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("").to_owned();
    if event_type == "error" {
        let nested = event.get("error").and_then(Value::as_object);
        let code = event.get("code").and_then(Value::as_str)
            .or_else(|| nested.and_then(|value| value.get("code")).and_then(Value::as_str))
            .map(str::to_owned);
        let message = event.get("message").and_then(Value::as_str)
            .or_else(|| nested.and_then(|value| value.get("message")).and_then(Value::as_str))
            .map(str::to_owned).or_else(|| code.clone()).unwrap_or_else(|| "Unknown Codex error".into());
        return Err(CodexError::Api { code, message: format!("Codex error: {message}") });
    }
    if event_type == "response.failed" {
        let error = event.pointer("/response/error");
        let code = error.and_then(|value| value.get("code")).and_then(Value::as_str).map(str::to_owned);
        let message = error.and_then(|value| value.get("message")).and_then(Value::as_str)
            .unwrap_or("Codex response failed").to_owned();
        return Err(CodexError::Api { code, message });
    }
    let terminal = matches!(event_type.as_str(), "response.done" | "response.completed" | "response.incomplete");
    if terminal {
        if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
            if !response.contains_key("status") && event_type == "response.incomplete" {
                response.insert("status".into(), json!("incomplete"));
            }
            if response.get("service_tier").and_then(Value::as_str) == Some("default")
                && matches!(requested_service_tier, Some("flex" | "priority"))
            {
                response.insert("service_tier".into(), json!(requested_service_tier));
            }
        }
        event.as_object_mut().expect("event object").insert("type".into(), json!("response.completed"));
    }
    responses::handle_event(None, &event.to_string(), output, state, model, requested_service_tier, event_tx)
        .map_err(|error| CodexError::Protocol(error.to_string()))?;
    Ok(terminal)
}

fn build_codex_headers(
    model: &Model,
    options: &StreamOptions,
    token: &str,
    account_id: &str,
    request_id: Option<&str>,
    websocket: bool,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(model_headers) = &model.headers { common::insert_header_map(&mut headers, model_headers)?; }
    common::insert_header_map(&mut headers, &options.headers)?;
    common::insert_header(&mut headers, "authorization", &format!("Bearer {token}"))?;
    common::insert_header(&mut headers, "chatgpt-account-id", account_id)?;
    common::insert_header(&mut headers, "originator", "rpi")?;
    common::insert_header(&mut headers, "user-agent", &format!("rpi ({}; {})", std::env::consts::OS, std::env::consts::ARCH))?;
    if websocket {
        headers.remove("accept");
        headers.remove("content-type");
        headers.remove("openai-beta");
        common::insert_header(&mut headers, "openai-beta", "responses_websockets=2026-02-06")?;
    } else {
        common::insert_header(&mut headers, "openai-beta", "responses=experimental")?;
        common::insert_header(&mut headers, "accept", "text/event-stream")?;
        common::insert_header(&mut headers, "content-type", "application/json")?;
    }
    if let Some(request_id) = request_id {
        common::insert_header(&mut headers, "session-id", request_id)?;
        common::insert_header(&mut headers, "x-client-request-id", request_id)?;
    }
    Ok(headers)
}

fn extract_account_id(token: &str) -> Result<String> {
    let payload = token.split('.').nth(1).ok_or_else(|| anyhow!("Failed to extract accountId from token"))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .map_err(|_| anyhow!("Failed to extract accountId from token"))?;
    let value: Value = serde_json::from_slice(&decoded)
        .map_err(|_| anyhow!("Failed to extract accountId from token"))?;
    value.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_owned)
        .ok_or_else(|| anyhow!("Failed to extract accountId from token"))
}

fn resolve_codex_url(base_url: &str) -> String {
    let normalized = if base_url.trim().is_empty() { DEFAULT_CODEX_BASE_URL } else { base_url.trim() }
        .trim_end_matches('/');
    if normalized.ends_with("/codex/responses") { normalized.to_owned() }
    else if normalized.ends_with("/codex") { format!("{normalized}/responses") }
    else { format!("{normalized}/codex/responses") }
}

fn resolve_codex_websocket_url(base_url: &str) -> String {
    let url = resolve_codex_url(base_url);
    if let Some(rest) = url.strip_prefix("https://") { format!("wss://{rest}") }
    else if let Some(rest) = url.strip_prefix("http://") { format!("ws://{rest}") }
    else { url }
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn sanitize_error(message: &str, model: &Model, options: &StreamOptions) -> String {
    let mut sanitized = message.to_owned();
    let mut secrets = Vec::new();
    if let Some(token) = options.api_key.as_deref() { secrets.push(token); }
    if let Some(headers) = &model.headers { secrets.extend(headers.values().map(String::as_str)); }
    secrets.extend(options.headers.values().map(String::as_str));
    for secret in secrets {
        if !secret.is_empty() { sanitized = sanitized.replace(secret, "[REDACTED]"); }
    }
    sanitized
}

pub async fn close_openai_codex_websocket_sessions(session_id: Option<&str>) {
    let entries = {
        let mut cache = WEBSOCKET_CACHE.lock().await;
        if let Some(session_id) = session_id {
            let suffix = format!("\u{1f}{session_id}");
            let keys: Vec<String> = cache.keys().filter(|key| key.ends_with(&suffix)).cloned().collect();
            keys.into_iter().filter_map(|key| cache.remove(&key)).collect::<Vec<_>>()
        } else {
            cache.drain().map(|(_, value)| value).collect::<Vec<_>>()
        }
    };
    for entry in entries {
        if let Ok(mut guard) = entry.try_lock_owned() { let _ = guard.socket.close(None).await; }
    }
}

pub async fn reset_openai_codex_transport_state(session_id: Option<&str>) {
    close_openai_codex_websocket_sessions(session_id).await;
    let mut fallback = SSE_FALLBACK_SESSIONS.lock().await;
    if let Some(session_id) = session_id {
        let suffix = format!("\u{1f}{session_id}");
        fallback.retain(|key| !key.ends_with(&suffix));
    } else {
        fallback.clear();
    }
}
