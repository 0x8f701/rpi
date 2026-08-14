//! Agent Client Protocol (ACP) mode — `rpi agent stdio` and `rpi agent serve`.
//!
//! ACP (agentclientprotocol.com) is a JSON-RPC 2.0 protocol that lets
//! ACP-speaking editors embed coding agents. rpi implements the stable ACP v1
//! surface:
//!
//! Client → Agent requests:
//! - `initialize` — negotiate the protocol version, capabilities, and
//!   advertised authentication methods.
//! - `authenticate` — acknowledge rpi's configured-credential auth method.
//! - `session/new` — create a session rooted at a client-supplied `cwd`;
//!   returns a `sessionId` used by all later calls.
//! - `session/prompt` — run a turn; the assistant response streams back as
//!   `session/update` notifications and the request resolves with a
//!   `stopReason` when the turn ends.
//! - `session/cancel` (notification) — abort the active turn; the pending
//!   `session/prompt` resolves with `stopReason: "cancelled"`.
//! - `session/close` — cancel ongoing work and release the session.
//! - `logout` — clear the authenticated state.
//!
//! Agent → Client reverse requests:
//! - `session/request_permission` — the tool-approval gate. When the session's
//!   approval mode (CLI `--approval-mode` or the `approvalMode` setting)
//!   requires confirmation, the agent asks the client for an `allow-once` /
//!   `reject-once` decision before the tool executes. The decision feeds the
//!   same `before_tool_call` approval path every other rpi frontend uses.
//!
//! Agent → Client notifications:
//! - `session/update` — `user_message_chunk`, `agent_message_chunk`,
//!   `agent_thought_chunk`, `tool_call`, `tool_call_update`, and
//!   `usage_update` variants.
//!
//! # Session mapping
//!
//! Each ACP `session/new` builds an independent rpi [`Application`] from the
//! shared [`RunSessionBlueprint`] with the client-supplied working directory.
//! The ACP `sessionId` (`sess_<uuid>`) is a protocol handle; the underlying
//! session is recorded to the normal rpi session store (unless `--no-session`)
//! so resumed rpi runs see the same conversation.
//!
//! # Transport
//!
//! `stdio` reuses the crate's shared Content-Length framing
//! ([`pi_coding::tools::framing`]) so an editor spawns `rpi agent stdio` the
//! same way it spawns an LSP server. `serve` speaks the same JSON-RPC 2.0
//! messages as WebSocket text frames, gated by the shared transport auth
//! policy ([`super::ws_auth`]): the server is **loopback-only** (plaintext
//! WebSocket cannot safely carry the bearer token off the local host; TLS
//! is tracked for a later release), tokenless loopback accepts native
//! clients but rejects browsers (they always send `Origin`), and a
//! configured token is presented via `Authorization: Bearer <token>` or
//! the `rpi-auth.<token>` subprotocol (echoed in the upgrade response).
//! Concurrent connections are capped at [`super::ws_auth::MAX_CONNECTION_TASKS`].
//!
//! # Limitations (documented, minimal viable surface)
//!
//! - `mcpServers` in `session/new` are accepted but not connected yet.
//! - One prompt turn per session at a time; concurrent prompts on the same
//!   session fail with an actionable error. Different sessions run
//!   concurrently.
//! - `session/load`, `session/resume`, `session/list`, and `session/delete`
//!   are not advertised (`loadSession`/`sessionCapabilities` stay off).
//! - `authenticate` acknowledges rpi's configured credentials (auth.json /
//!   provider env keys / faux for tests); it never prints or requests secrets.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use pi_agent::{
    AgentEvent, ApprovalMode, BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult,
    ToolCapability, compose_before_tool_call,
};
use pi_ai::{AssistantMessageEvent, ContentBlock, Message};
use pi_coding::{Application, ApplicationEvent, ExtensionMode, ResourceManagerOptions};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        Message as WsMessage,
        handshake::server::{ErrorResponse, Request, Response},
        protocol::WebSocketConfig,
    },
};
use uuid::Uuid;

use crate::args::Cli;
use crate::session_run_blueprint::{RunSessionBlueprint, RunSessionCandidate};

use super::ws_auth::{
    ListenAddressPolicy, MAX_CONNECTION_TASKS, authorized, load_auth_token,
    websocket_subprotocol,
};

/// The ACP major protocol version rpi speaks (agentclientprotocol.com v1).
pub(crate) const PROTOCOL_VERSION: i64 = 1;
/// Auth method advertised in `initialize` and accepted by `authenticate`.
const AUTH_METHOD_ID: &str = "rpi-auth";
/// How long a `session/request_permission` reverse request waits for the
/// client's decision before the tool is blocked as timed out.
const PERMISSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const OUTBOUND_CHANNEL_CAPACITY: usize = 64;
const INBOUND_CHANNEL_CAPACITY: usize = 64;
/// Cap on client-supplied `resource_link` file contents embedded into prompts.
const MAX_EMBEDDED_RESOURCE_BYTES: usize = 2 * 1024 * 1024;
/// Cap on ACP WebSocket and stdio frames (16 MiB, matching the rpc control
/// plane's generous framing budget).
const MAX_WS_FRAME_BYTES: usize = 16 * 1024 * 1024;

// JSON-RPC 2.0 + ACP error codes (see the ACP schema's `ErrorCode`).
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const AUTH_REQUIRED: i64 = -32000;
const RESOURCE_NOT_FOUND: i64 = -32002;

/// A JSON-RPC error carrying the ACP error code.
#[derive(Debug)]
struct AcpError {
    code: i64,
    message: String,
}

impl AcpError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self { code: INVALID_PARAMS, message: message.into() }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self { code: INTERNAL_ERROR, message: message.into() }
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn rpc_notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// One `session/update` notification carrying an `update` payload.
fn session_update(session_id: &str, update: Value) -> Value {
    rpc_notification("session/update", json!({ "sessionId": session_id, "update": update }))
}

fn session_update_payload(session_update: &str) -> Value {
    json!({ "sessionUpdate": session_update })
}

/// Replace absolute-path tokens (`/workspace/project/file.rs`, `C:\...`)
/// with a stable `<path>` placeholder so ACP wire errors never leak the
/// host's directory layout. Non-path tokens are preserved verbatim.
fn strip_absolute_paths(message: &str) -> String {
    fn is_absolute_path(token: &str) -> bool {
        // Trim surrounding punctuation (quotes, brackets, trailing colons)
        // but keep a leading `/` so `…:12:5` and trailing colons still count.
        let trimmed = token.trim_matches(|c: char| c.is_ascii_punctuation() && c != '/');
        trimmed.starts_with('/') && trimmed.len() > 1
            || (trimmed.len() >= 3
                && trimmed.as_bytes()[0].is_ascii_alphabetic()
                && trimmed.as_bytes()[1] == b':'
                && matches!(trimmed.as_bytes()[2], b'/' | b'\\'))
    }
    message
        .split_whitespace()
        .map(|token| if is_absolute_path(token) { "<path>".to_owned() } else { token.to_owned() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a stable, path-free wire message for an internal error and log the
/// full `anyhow` chain (with paths and causes) server-side, where it cannot
/// reach the ACP client.
fn wire_internal_error(context: &str, error: &anyhow::Error) -> String {
    eprintln!("ACP {context} failed: {error:#}");
    strip_absolute_paths(&error.to_string())
}

/// A JSON-RPC `INTERNAL_ERROR` response whose message is path-free; the full
/// error chain is logged server-side.
fn internal_error_response(id: &Value, context: &str, error: &anyhow::Error) -> Option<Value> {
    Some(rpc_error(id, INTERNAL_ERROR, wire_internal_error(context, error)))
}

/// A decoded client message: a request, a notification, or a response to one
/// of our reverse requests.
enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: Value, result: Option<Value>, error: Option<Value> },
}

fn parse_incoming(message: &Value) -> std::result::Result<Incoming, AcpError> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(AcpError { code: INVALID_REQUEST, message: "jsonrpc must be \"2.0\"".into() });
    }
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).map(ToOwned::to_owned);
    let has_result = message.get("result").is_some() || message.get("error").is_some();
    match (method, has_result) {
        (Some(method), false) => {
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            if id.is_some() {
                Ok(Incoming::Request { id: id.expect("checked above"), method, params })
            } else {
                Ok(Incoming::Notification { method, params })
            }
        }
        (None, true) => Ok(Incoming::Response {
            id: id.unwrap_or(Value::Null),
            result: message.get("result").cloned(),
            error: message.get("error").cloned(),
        }),
        (Some(_), true) => Err(AcpError {
            code: INVALID_REQUEST,
            message: "message cannot combine method with result/error".into(),
        }),
        (None, false) => Err(AcpError {
            code: INVALID_REQUEST,
            message: "message must have a method or a result/error".into(),
        }),
    }
}

/// One in-flight reverse request awaiting the client's response.
struct PendingRequest {
    session_id: String,
    tx: oneshot::Sender<Value>,
}

/// Outcome of an ACP `session/request_permission` round trip.
enum PermissionDecision {
    Allow,
    Deny(String),
    Cancelled(String),
    Error(String),
}

/// Reverse-request bridge: sends JSON-RPC requests to the ACP client and
/// awaits the response through the shared pending registry.
///
/// The outbound sender, pending registry, and timeout are shared across all
/// sessions of one connection. The `session_id` slot is per-factory:
/// [`AcpServer::handle_session_new`] builds a fresh factory (fresh slot) for
/// every session and binds it before the session builds, so concurrent turns
/// on different sessions never cross permission requests. Clones of one
/// session's factory keep sharing that session's slot.
#[derive(Clone)]
pub(crate) struct AcpApprovalFactory {
    outbound: mpsc::Sender<Value>,
    pending: Arc<StdMutex<HashMap<String, PendingRequest>>>,
    session_id: Arc<StdMutex<Option<String>>>,
    timeout: Duration,
}

impl AcpApprovalFactory {
    fn new(
        outbound: &mpsc::Sender<Value>,
        pending: Arc<StdMutex<HashMap<String, PendingRequest>>>,
        timeout: Duration,
    ) -> Self {
        Self { outbound: outbound.clone(), pending, session_id: Arc::new(StdMutex::new(None)), timeout }
    }

    /// Bind this bridge to one ACP session id (called before the session's
    /// first prompt; the approval hook captures the bridge, not the id).
    fn set_session_id(&self, session_id: String) {
        *self.session_id.lock().expect("acp session id slot") = Some(session_id);
    }

    /// Ask the client to approve `tool_call` and await the decision.
    async fn request_permission(&self, tool_call: &pi_ai::ToolCall) -> PermissionDecision {
        let session_id = match self.session_id.lock().expect("acp session id slot").clone() {
            Some(session_id) => session_id,
            None => {
                return PermissionDecision::Error(
                    "ACP permission request has no active session".into(),
                );
            }
        };
        let request_id = Uuid::now_v7().to_string();
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": tool_call.id,
                    "title": tool_call.name,
                    "kind": tool_kind(&tool_call.name),
                    "status": "pending",
                    "rawInput": tool_call.arguments,
                },
                "options": [
                    { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" },
                ],
            },
        });
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("acp pending registry")
            .insert(request_id.clone(), PendingRequest { session_id: session_id.clone(), tx });
        if self.outbound.send(request).await.is_err() {
            self.pending.lock().expect("acp pending registry").remove(&request_id);
            return PermissionDecision::Error(
                "ACP client disconnected while requesting permission".into(),
            );
        }
        let response = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                return PermissionDecision::Error(
                    "ACP permission response channel closed".into(),
                );
            }
            Err(_) => {
                return PermissionDecision::Error(format!(
                    "ACP permission request timed out after {} seconds",
                    self.timeout.as_secs()
                ));
            }
        };
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("ACP permission request failed");
            return PermissionDecision::Error(message.to_owned());
        }
        let outcome = &response["result"]["outcome"];
        match outcome.get("outcome").and_then(Value::as_str) {
            Some("selected") => match outcome.get("optionId").and_then(Value::as_str) {
                Some("allow-once" | "allow-always") => PermissionDecision::Allow,
                Some("reject-once" | "reject-always") => {
                    PermissionDecision::Deny("tool execution rejected by the client".into())
                }
                Some(other) => {
                    PermissionDecision::Error(format!("unknown permission option selected: {other}"))
                }
                None => PermissionDecision::Error(
                    "ACP permission response is missing optionId".into(),
                ),
            },
            Some("cancelled") => {
                PermissionDecision::Cancelled("tool execution cancelled by the client".into())
            }
            Some(other) => {
                PermissionDecision::Error(format!("unknown permission outcome: {other}"))
            }
            None => {
                PermissionDecision::Error("ACP permission response is missing an outcome".into())
            }
        }
    }
}

/// The `before_tool_call` approval hook for ACP sessions: path-level
/// `permissionRules` first (same evaluation as the interactive host hook),
/// then the capability-wide approval mode, then an ACP reverse-request
/// permission round trip.
#[must_use]
pub(crate) fn acp_approval_before_tool_call(
    mode: ApprovalMode,
    factory: AcpApprovalFactory,
    existing: Option<BeforeToolCallFn>,
    cwd: PathBuf,
    permission_rules: crate::approval::PermissionRulesSource,
) -> BeforeToolCallFn {
    let approval: BeforeToolCallFn = Arc::new(move |context| {
        let factory = factory.clone();
        let permission_rules = permission_rules.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            let verdict = pi_coding::permission_verdict(
                &context.tool_call.name,
                &context.arguments,
                &cwd,
                &permission_rules(),
            );
            let forced_ask = matches!(&verdict, pi_coding::PermissionVerdict::Ask);
            match verdict {
                pi_coding::PermissionVerdict::Deny(reason) => return Ok(blocked(reason)),
                pi_coding::PermissionVerdict::Allow => {
                    return Ok(BeforeToolCallResult::default());
                }
                pi_coding::PermissionVerdict::Ask | pi_coding::PermissionVerdict::NoMatch => {}
            }
            let capability = tool_capability(&context);
            if !forced_ask && !mode.requires_approval(capability) {
                return Ok(BeforeToolCallResult::default());
            }
            match factory.request_permission(&context.tool_call).await {
                PermissionDecision::Allow => Ok(BeforeToolCallResult::default()),
                PermissionDecision::Deny(reason)
                | PermissionDecision::Cancelled(reason)
                | PermissionDecision::Error(reason) => Ok(blocked(reason)),
            }
        })
    });
    compose_before_tool_call(Some(approval), existing).expect("ACP approval hook is always present")
}

fn tool_capability(context: &BeforeToolCallContext) -> ToolCapability {
    context
        .context
        .tools
        .iter()
        .find(|tool| tool.name == context.tool_call.name)
        .map_or_else(ToolCapability::default, |tool| tool.capability)
}

fn blocked(reason: impl Into<String>) -> BeforeToolCallResult {
    BeforeToolCallResult {
        block: true,
        reason: Some(reason.into()),
        arguments: None,
    }
}

/// Map a tool name to an ACP `ToolKind` for client icons.
fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "inspect_image" | "glob" | "ls" | "browser" | "doc_convert" | "mcp"
        | "debug" | "memory" | "github" => "read",
        "edit" | "write" | "imageresize" | "ast_edit" | "lsp" => "edit",
        "bash" | "process" | "web_search" | "eval" | "notebook" => "execute",
        "grep" | "find" | "ast_grep" => "search",
        "ask" => "other",
        _ => "other",
    }
}

/// One ACP session: its rpi application plus the per-session approval bridge.
struct AcpSession {
    application: Application,
    cwd: PathBuf,
}

/// Cancel state for an in-flight prompt turn (keyed by session id).
struct TurnState {
    cancelled: Arc<AtomicBool>,
}

struct AcpServer {
    blueprint: RunSessionBlueprint,
    cli: Cli,
    approval: AcpApprovalFactory,
    sessions: HashMap<String, AcpSession>,
    turns: HashMap<String, TurnState>,
}

impl AcpServer {
    fn new(
        blueprint: RunSessionBlueprint,
        cli: Cli,
        approval: AcpApprovalFactory,
    ) -> Self {
        Self { blueprint, cli, approval, sessions: HashMap::new(), turns: HashMap::new() }
    }

    /// Handle one incoming client message. Returns `Some(response)` when the
    /// client expects a response (the `session/prompt` response is deferred
    /// to the spawned turn task, which sends it directly through `outbound`).
    async fn handle_message(
        &mut self,
        message: &Value,
        outbound: &mpsc::Sender<Value>,
        tasks: &mut JoinSet<String>,
    ) -> Result<Option<Value>> {
        match parse_incoming(message) {
            Ok(Incoming::Request { id, method, params }) => match method.as_str() {
                "initialize" => Ok(Some(rpc_result(&id, initialize_result(&params)))),
                "authenticate" => self.handle_authenticate(&id, &params).await,
                "logout" => {
                    // rpi's credential state is process-wide (auth.json /
                    // provider env keys); `logout` is an acknowledgment that
                    // leaves session activity unaffected, matching the ACP
                    // "no guarantee for running sessions" wording.
                    Ok(Some(rpc_result(&id, json!({}))))
                }
                "session/new" => self.handle_session_new(&id, &params).await,
                "session/prompt" => {
                    self.handle_prompt(&id, &params, outbound, tasks).await
                }
                "session/cancel" => Ok(self.handle_cancel(Some(&id), &params).await),
                "session/close" => self.handle_session_close(&id, &params).await,
                other => Ok(Some(rpc_error(
                    &id,
                    METHOD_NOT_FOUND,
                    format!("method not found: {other}"),
                ))),
            },
            Ok(Incoming::Notification { method, params }) => match method.as_str() {
                "session/cancel" => {
                    let _ = self.handle_cancel(None, &params).await;
                    Ok(None)
                }
                // Unknown notifications are ignored per JSON-RPC 2.0.
                _ => Ok(None),
            },
            Ok(Incoming::Response { id, result, error }) => {
                self.resolve_pending(&id, result, error);
                Ok(None)
            }
            Err(error) => Ok(Some(rpc_error(&Value::Null, error.code, error.message))),
        }
    }

    async fn handle_authenticate(
        &mut self,
        id: &Value,
        params: &Value,
    ) -> Result<Option<Value>> {
        let method_id = params.get("methodId").and_then(Value::as_str);
        let Some(method_id) = method_id else {
            return Ok(Some(rpc_error(
                id,
                INVALID_PARAMS,
                "authenticate requires a methodId",
            )));
        };
        if method_id != AUTH_METHOD_ID {
            return Ok(Some(rpc_error(
                id,
                INVALID_PARAMS,
                format!(
                    "unknown authentication method {method_id:?}; advertised methods: [{AUTH_METHOD_ID}]"
                ),
            )));
        }
        // Acknowledge rpi's configured-credential auth. rpi never collects
        // secrets over ACP: credentials come from auth.json / provider env
        // keys (or the faux provider in tests). Session creation performs the
        // real gate: `session/new` resolves a model with usable credentials
        // and fails with the -32000 auth_required error otherwise.
        Ok(Some(rpc_result(id, json!({}))))
    }

    async fn handle_session_new(&mut self, id: &Value, params: &Value) -> Result<Option<Value>> {
        let cwd = match params.get("cwd").and_then(Value::as_str) {
            Some(cwd) => PathBuf::from(cwd),
            None => {
                return Ok(Some(rpc_error(id, INVALID_PARAMS, "session/new requires a cwd")));
            }
        };
        if !cwd.is_absolute() {
            return Ok(Some(rpc_error(
                id,
                INVALID_PARAMS,
                "session/new cwd must be an absolute path",
            )));
        }
        if !cwd.is_dir() {
            return Ok(Some(rpc_error(
                id,
                RESOURCE_NOT_FOUND,
                format!("session/new cwd is not a directory: {}", cwd.display()),
            )));
        }
        if let Some(servers) = params.get("mcpServers")
            && !servers.is_array()
        {
            return Ok(Some(rpc_error(
                id,
                INVALID_PARAMS,
                "session/new mcpServers must be an array",
            )));
        }
        // `mcpServers` are accepted but not connected yet (documented
        // limitation of the minimal ACP surface).

        let preview = match self.blueprint.resource_options_for_startup(&cwd) {
            Ok(options) => match pi_coding::ResourceManager::new(options) {
                Ok(resources) => resources,
                Err(error) => {
                    return Ok(internal_error_response(id, "loading settings and resources", &error));
                }
            },
            Err(error) => {
                return Ok(internal_error_response(id, "loading settings and resources", &error));
            }
        };
        let settings = preview.snapshot().settings.clone();
        let (model, api_key, parsed_think) =
            match crate::session_run::resolve_initial_model(&self.cli, &settings, None).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    let message = error.to_string();
                    let code = if message.starts_with("No authenticated models") {
                        AUTH_REQUIRED
                    } else {
                        INTERNAL_ERROR
                    };
                    return Ok(Some(rpc_error(id, code, message)));
                }
            };
        let thinking_level = crate::session_run::resolve_initial_thinking_level(
            self.cli.think.as_deref(),
            &parsed_think,
            None,
            false,
            settings.default_thinking_level.map(crate::output::thinking_level_str),
        );
        let session_dir = match crate::session_run::effective_session_dir(
            &cwd,
            self.cli.session_dir.as_deref(),
            settings.session_dir.as_deref(),
        ) {
            Ok(session_dir) => session_dir,
            Err(error) => {
                return Ok(internal_error_response(id, "resolving the session directory", &error));
            }
        };

        let session_id = format!("sess_{}", Uuid::now_v7().simple());
        // A fresh factory per session: the approval hook clones this factory
        // (sharing its slot) while the shared outbound/pending registry keeps
        // routing decisions to this connection. Cloning the server's shared
        // factory instead would hand every session the same mutable slot and
        // cross-wire permission requests between concurrent sessions.
        let factory = AcpApprovalFactory::new(
            &self.approval.outbound,
            Arc::clone(&self.approval.pending),
            self.approval.timeout,
        );
        factory.set_session_id(session_id.clone());
        let mut blueprint = self.blueprint.clone();
        blueprint.set_session_dir(session_dir.clone());
        blueprint.set_acp_approval(factory);

        let options = pi_coding::SessionOptions {
            model: model.clone(),
            cwd: cwd.clone(),
            system_prompt: String::new(),
            thinking_level,
            api_key,
            compaction: Some(pi_coding::DEFAULT_COMPACTION_SETTINGS),
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        };
        let candidate = match blueprint.build(&cwd, options).await {
            Ok(candidate) => candidate,
            Err(error) => {
                return Ok(internal_error_response(id, "building the session", &error));
            }
        };
        let RunSessionCandidate {
            session,
            extension_runtime: runtime,
            extension_permissions: permissions,
            orchestration,
            goal_tool,
        } = candidate;
        session.set_session_dir(session_dir.clone());
        if !self.cli.no_session {
            match pi_coding::start_session_in(
                &cwd,
                Some(&model),
                Some(crate::output::thinking_level_str(thinking_level)),
                Some(&session_dir),
                None,
                None,
            )
            .and_then(|recorder| session.record(recorder))
            {
                Ok(()) => {}
                Err(error) => {
                    session.abort().await;
                    session.wait_for_idle().await;
                    if let Some(orchestration) = orchestration {
                        orchestration.shutdown().await;
                    }
                    runtime.shutdown().await;
                    return Ok(internal_error_response(id, "recording the session", &error));
                }
            }
        }
        if let Some(name) = self.cli.name.as_deref() {
            let _ = session.set_session_name(name);
        }
        let application = Application::new_with_extensions(session, runtime, permissions).await;
        if let Err(error) = application.attach_runtime_factory(Arc::new(blueprint.clone())) {
            application.cleanup().await;
            return Ok(internal_error_response(id, "attaching the extension runtime", &error));
        }
        if let Some(binding) = goal_tool
            && let Err(error) = application.attach_goal_tool(binding)
        {
            application.cleanup().await;
            return Ok(internal_error_response(id, "attaching the goal tool", &error));
        }
        if let Some(orchestration) = orchestration
            && let Err(error) = application.attach_orchestration(orchestration).await
        {
            application.cleanup().await;
            return Ok(internal_error_response(id, "attaching orchestration", &error));
        }
        let agent_dir = match self.blueprint.resource_options_for_startup(&cwd) {
            Ok(options) => options.agent_dir,
            Err(error) => {
                application.cleanup().await;
                return Ok(internal_error_response(id, "loading agent resources", &error));
            }
        };
        let session_identity = application
            .session()
            .recorder_info()
            .map(|(identity, _)| identity.to_owned())
            .unwrap_or_else(|| crate::session_run::fallback_session_identity().to_owned());
        let (workflow_store_root, workflow_worktree_root) =
            crate::session_run::workflow_storage_roots(&cwd, &agent_dir, &session_identity);
        if let Err(error) = application
            .setup_workflows(cwd.clone(), workflow_store_root, workflow_worktree_root)
            .await
        {
            application.cleanup().await;
            return Ok(internal_error_response(id, "setting up workflows", &error));
        }
        self.sessions.insert(
            session_id.clone(),
            AcpSession { application, cwd },
        );
        Ok(Some(rpc_result(id, json!({ "sessionId": session_id }))))
    }

    /// Start a prompt turn. The turn task streams `session/update`
    /// notifications and resolves the request with a `stopReason` when the
    /// turn settles; `None` is returned so the caller sends no response.
    async fn handle_prompt(
        &mut self,
        id: &Value,
        params: &Value,
        outbound: &mpsc::Sender<Value>,
        tasks: &mut JoinSet<String>,
    ) -> Result<Option<Value>> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            return Ok(Some(rpc_error(
                id,
                INVALID_PARAMS,
                "session/prompt requires a sessionId",
            )));
        };
        let session_id = session_id.to_owned();
        let Some(session) = self.sessions.get(&session_id) else {
            return Ok(Some(rpc_error(
                id,
                RESOURCE_NOT_FOUND,
                format!("unknown session: {session_id}"),
            )));
        };
        if self.turns.contains_key(&session_id) {
            return Ok(Some(rpc_error(
                id,
                INTERNAL_ERROR,
                "session is already processing a prompt",
            )));
        }
        let (text, images) = match parse_prompt_blocks(params) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(Some(rpc_error(id, error.code, error.message))),
        };
        let application = session.application.clone();
        let request_id = id.clone();
        let outbound = outbound.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.turns.insert(
            session_id.clone(),
            TurnState { cancelled: cancelled.clone() },
        );
        tasks.spawn(run_prompt_turn(
            application,
            session_id,
            request_id,
            text,
            images,
            outbound,
            cancelled,
        ));
        Ok(None)
    }

    /// Abort the active turn for a session and resolve its pending permission
    /// requests with the `cancelled` outcome. Returns the JSON-RPC response
    /// when the client sent `session/cancel` as a request (the spec defines
    /// it as a notification, so this is `None` for notifications).
    async fn handle_cancel(&mut self, id: Option<&Value>, params: &Value) -> Option<Value> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            return id.map(|id| {
                rpc_error(id, INVALID_PARAMS, "session/cancel requires a sessionId")
            });
        };
        if let Some(state) = self.turns.get(session_id) {
            state.cancelled.store(true, Ordering::Release);
        }
        if let Some(session) = self.sessions.get(session_id) {
            session.application.abort().await;
        }
        self.resolve_pending_for_session(session_id);
        id.map(|id| rpc_result(id, json!({})))
    }

    async fn handle_session_close(&mut self, id: &Value, params: &Value) -> Result<Option<Value>> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            return Ok(Some(rpc_error(
                id,
                INVALID_PARAMS,
                "session/close requires a sessionId",
            )));
        };
        let Some(mut session) = self.sessions.remove(session_id) else {
            return Ok(Some(rpc_error(
                id,
                RESOURCE_NOT_FOUND,
                format!("unknown session: {session_id}"),
            )));
        };
        if let Some(state) = self.turns.remove(session_id) {
            state.cancelled.store(true, Ordering::Release);
        }
        session.application.abort().await;
        session.application.cleanup().await;
        Ok(Some(rpc_result(id, json!({}))))
    }

    fn resolve_pending(&self, id: &Value, result: Option<Value>, error: Option<Value>) {
        let Some(key) = id.as_str().map(ToOwned::to_owned) else {
            return;
        };
        let entry = self.approval.pending.lock().expect("acp pending registry").remove(&key);
        if let Some(entry) = entry {
            let payload = if let Some(error) = error {
                json!({ "error": error })
            } else {
                json!({ "result": result.unwrap_or(Value::Null) })
            };
            let _ = entry.tx.send(payload);
        }
    }

    /// Resolve every pending reverse request belonging to `session_id` with
    /// the `cancelled` outcome (required when a prompt turn is cancelled).
    fn resolve_pending_for_session(&self, session_id: &str) {
        let mut pending = self.approval.pending.lock().expect("acp pending registry");
        let keys = pending
            .iter()
            .filter(|(_, request)| request.session_id == session_id)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let cancelled = json!({ "result": { "outcome": { "outcome": "cancelled" } } });
        for key in keys {
            if let Some(entry) = pending.remove(&key) {
                let _ = entry.tx.send(cancelled.clone());
            }
        }
    }

    /// Abort all active turns, wait for them to settle, and clean up every
    /// session's application so recorded sessions flush.
    async fn shutdown(&mut self) {
        for state in self.turns.values() {
            state.cancelled.store(true, Ordering::Release);
        }
        for session in self.sessions.values() {
            session.application.abort().await;
        }
        for session in self.sessions.values_mut() {
            session.application.cleanup().await;
        }
        self.sessions.clear();
        self.turns.clear();
    }
}

/// `initialize` result: negotiate the protocol version (the client's version
/// when we support it, otherwise ours — the client decides whether to
/// proceed), advertise capabilities, implementation info, and auth methods.
fn initialize_result(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_i64);
    let protocol_version = match requested {
        Some(requested) if requested == PROTOCOL_VERSION => requested,
        _ => PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": protocol_version,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
            "mcpCapabilities": { "http": false, "sse": false },
            "sessionCapabilities": { "close": {} },
            "auth": { "logout": {} },
        },
        "agentInfo": {
            "name": "rpi",
            "title": "rpi — Rust coding agent",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "authMethods": [{
            "id": AUTH_METHOD_ID,
            "name": "rpi configured credentials",
            "description": "Use credentials already configured for rpi (auth.json, provider API-key environment variables).",
        }],
    })
}

/// Convert an ACP `session/prompt` `ContentBlock[]` into the rpi prompt text
/// and image blocks. Baseline support: `text` and `resource_link`; `image`
/// and `resource` are enabled by the advertised prompt capabilities.
fn parse_prompt_blocks(params: &Value) -> std::result::Result<(String, Vec<ContentBlock>), AcpError> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| AcpError::invalid_params("session/prompt requires a prompt array"))?;
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AcpError::invalid_params("text content block is missing text"))?;
                text_parts.push(text.to_owned());
            }
            Some("image") => {
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AcpError::invalid_params("image content block is missing data"))?;
                let mime_type = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_owned();
                images.push(ContentBlock::Image { data: data.to_owned(), mime_type });
            }
            Some("resource") => {
                // Embedded context: prefer the text form; decode image blobs
                // into the image channel; anything else is noted as context.
                let resource = &block["resource"];
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    let uri = resource
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("<embedded resource>");
                    text_parts.push(format!("=== {uri} ===\n{text}\n=== end {uri} ==="));
                } else if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
                    let mime_type = resource
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if mime_type.starts_with("image/") {
                        images.push(ContentBlock::Image {
                            data: blob.to_owned(),
                            mime_type: mime_type.to_owned(),
                        });
                    } else {
                        text_parts.push(format!(
                            "[embedded blob resource {} ({mime_type}) was not decoded]",
                            resource
                                .get("uri")
                                .and_then(Value::as_str)
                                .unwrap_or("<embedded resource>")
                        ));
                    }
                }
            }
            Some("resource_link") => {
                // Baseline support: read `file://` links and embed them.
                let uri = block
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AcpError::invalid_params("resource_link content block is missing uri")
                    })?;
                match file_uri_to_path(uri) {
                    Some(path) => match std::fs::read(&path) {
                        Ok(bytes) if bytes.len() <= MAX_EMBEDDED_RESOURCE_BYTES => {
                            let text = String::from_utf8_lossy(&bytes);
                            text_parts.push(format!("=== {uri} ===\n{text}\n=== end {uri} ==="));
                        }
                        Ok(_) => {
                            text_parts.push(format!(
                                "[resource {uri} exceeds the {MAX_EMBEDDED_RESOURCE_BYTES}-byte embed limit and was skipped]"
                            ));
                        }
                        Err(error) => {
                            text_parts.push(format!(
                                "[resource {uri} could not be read: {error}]"
                            ));
                        }
                    },
                    None => {
                        text_parts.push(format!(
                            "[resource link {uri} is not a local file and was skipped]"
                        ));
                    }
                }
            }
            Some("audio") => {
                return Err(AcpError::invalid_params(
                    "audio prompts are not supported (advertised promptCapabilities.audio is false)",
                ));
            }
            Some(other) => {
                return Err(AcpError::invalid_params(format!(
                    "unsupported prompt content block type: {other}"
                )));
            }
            None => {
                return Err(AcpError::invalid_params(
                    "prompt content block is missing a type",
                ));
            }
        }
    }
    let text = text_parts.join("\n\n");
    if text.is_empty() && images.is_empty() {
        return Err(AcpError::invalid_params("session/prompt contains no usable content"));
    }
    Ok((text, images))
}

/// Map a `file://` URI to a local path (`None` for non-file schemes).
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    let path = path.trim_start_matches("/localhost").to_owned();
    let path = percent_decode(path.as_bytes());
    Some(PathBuf::from(path))
}

fn percent_decode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(hex) = hex
                && let Ok(value) = u8::from_str_radix(hex, 16)
            {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Run one prompt turn: echo the user message, stream the assistant's
/// response as `session/update` notifications, and resolve the original
/// `session/prompt` request with a `stopReason` (or an error). Returns the
/// session id so the connection loop can release the session's in-flight
/// turn slot once the turn settles (a second sequential prompt must be
/// accepted after the first completes — success or failure).
async fn run_prompt_turn(
    application: Application,
    session_id: String,
    request_id: Value,
    text: String,
    images: Vec<ContentBlock>,
    outbound: mpsc::Sender<Value>,
    cancelled: Arc<AtomicBool>,
) -> String {
    let mut events = application.subscribe();
    let mut message_id: Option<String> = None;
    let mut failed: Option<String> = None;

    let echo = session_update(
        &session_id,
        json!({
            "sessionUpdate": "user_message_chunk",
            "messageId": Uuid::now_v7().to_string(),
            "content": { "type": "text", "text": text.clone() },
        }),
    );
    if outbound.send(echo).await.is_err() {
        return session_id;
    }

    if let Err(error) = application.prompt(text, images, None).await {
        eprintln!("ACP prompt failed for session {session_id}: {error:#}");
        let _ = outbound
            .send(rpc_error(
                &request_id,
                INTERNAL_ERROR,
                strip_absolute_paths(&error.to_string()),
            ))
            .await;
        return session_id;
    }

    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                failed = Some("application event stream lagged; the turn may be incomplete".into());
                break;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        match event {
            ApplicationEvent::AgentSettled => break,
            ApplicationEvent::RunFailed { message } => {
                failed = Some(strip_absolute_paths(&message));
                break;
            }
            event => {
                for update in project_event(event, &mut message_id) {
                    let notification = session_update(&session_id, update);
                    if outbound.send(notification).await.is_err() {
                        return session_id;
                    }
                }
            }
        }
    }

    // Report the session's context usage after the turn (optional but cheap).
    let stats = application.session_stats();
    let used = stats
        .context_usage
        .as_ref()
        .and_then(|usage| usage.tokens)
        .unwrap_or(stats.tokens.total)
        .max(0);
    let size = stats
        .context_usage
        .as_ref()
        .map(|usage| usage.context_window)
        .filter(|size| *size > 0)
        .unwrap_or(used.max(1));
    let mut usage = session_update_payload("usage_update");
    usage["used"] = json!(used);
    usage["size"] = json!(size);
    if stats.cost > 0.0 {
        usage["cost"] = json!({ "amount": stats.cost, "currency": "USD" });
    }
    let _ = outbound.send(session_update(&session_id, usage)).await;

    // The spec mandates `cancelled` whenever the client sent `session/cancel`,
    // even when the underlying abort surfaces as a run error.
    let response = if cancelled.load(Ordering::Acquire) {
        rpc_result(&request_id, json!({ "stopReason": "cancelled" }))
    } else if let Some(message) = failed {
        rpc_error(&request_id, INTERNAL_ERROR, message)
    } else {
        rpc_result(&request_id, json!({ "stopReason": "end_turn" }))
    };
    let _ = outbound.send(response).await;
    session_id
}

/// Project one [`ApplicationEvent`] into a sequence of `session/update`
/// payloads (the `update` field only; the caller wraps them with the session
/// id). `message_id` tracks the current assistant message across chunks.
fn project_event(event: ApplicationEvent, message_id: &mut Option<String>) -> Vec<Value> {
    match event {
        ApplicationEvent::Agent(AgentEvent::MessageStart { message: Message::Assistant(_) }) => {
            *message_id = Some(Uuid::now_v7().to_string());
            Vec::new()
        }
        ApplicationEvent::Agent(AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        }) => {
            let id = message_id.get_or_insert_with(|| Uuid::now_v7().to_string());
            vec![json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": id,
                "content": { "type": "text", "text": delta },
            })]
        }
        ApplicationEvent::Agent(AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::ThinkingDelta { delta, .. },
            ..
        }) => {
            let id = message_id.get_or_insert_with(|| Uuid::now_v7().to_string());
            vec![json!({
                "sessionUpdate": "agent_thought_chunk",
                "messageId": id,
                "content": { "type": "text", "text": delta },
            })]
        }
        ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            arguments,
        }) => vec![
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "title": tool_name,
                "kind": tool_kind(&tool_name),
                "status": "pending",
                "rawInput": arguments,
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": "in_progress",
            }),
        ],
        ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        }) => {
            let mut update = json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": if is_error { "failed" } else { "completed" },
            });
            let mut text = tool_result_text(&result);
            if is_error && !text.is_empty() {
                text = format!("error: {text}");
            }
            update["content"] = json!([{
                "type": "content",
                "content": { "type": "text", "text": text },
            }]);
            if !result.details.is_object() || !result.details.as_object().is_some_and(|d| d.is_empty()) {
                update["rawOutput"] = result.details.clone();
            }
            vec![update]
        }
        _ => Vec::new(),
    }
}

fn tool_result_text(result: &pi_agent::AgentToolResult) -> String {
    let mut parts = Vec::new();
    for block in &result.content {
        if let ContentBlock::Text { text, .. } = block {
            parts.push(text.clone());
        }
    }
    parts.join("\n")
}

/// Core connection loop shared by the stdio and WebSocket transports. Reads
/// parsed JSON-RPC messages from `inbound`, responds through `outbound`, and
/// spawns prompt turns. Returns when the client closes `inbound` (EOF / WS
/// close), after aborting and cleaning up every session.
pub(crate) async fn serve_connection(
    blueprint: RunSessionBlueprint,
    cli: Cli,
    mut inbound: mpsc::Receiver<Value>,
    outbound: mpsc::Sender<Value>,
) -> Result<()> {
    let pending = Arc::new(StdMutex::new(HashMap::new()));
    let approval = AcpApprovalFactory::new(&outbound, pending, PERMISSION_REQUEST_TIMEOUT);
    let mut server = AcpServer::new(blueprint, cli, approval);
    let mut tasks: JoinSet<String> = JoinSet::new();
    loop {
        tokio::select! {
            incoming = inbound.recv() => match incoming {
                Some(message) => {
                    if let Some(response) = server.handle_message(&message, &outbound, &mut tasks).await? {
                        outbound.send(response).await.context("sending ACP response")?;
                    }
                }
                None => break,
            },
            completed = tasks.join_next(), if !tasks.is_empty() => {
                match completed {
                    Some(Ok(session_id)) => {
                        // The turn settled (success, failure, or cancel): its
                        // response was already sent, so release the session's
                        // in-flight slot and accept the next prompt.
                        server.turns.remove(&session_id);
                    }
                    Some(Err(error)) => {
                        return Err(anyhow!(error).context("ACP prompt task failed"));
                    }
                    None => {}
                }
            }
        }
    }
    server.shutdown().await;
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Build the shared session blueprint for the ACP mode: catalogs loaded,
/// headless resource options, no interactive extension-UI adapter, and the
/// Json extension mode (ACP clients see extension UI as unavailable).
async fn acp_blueprint(cli: &Cli) -> Result<RunSessionBlueprint> {
    crate::session_run::load_startup_catalogs(cli).await?;
    let (_, resource_options) = crate::session_run::startup_resource_options(cli, true)?;
    let mut blueprint = RunSessionBlueprint::from_cli(cli, resource_options, None);
    blueprint.set_extension_mode(ExtensionMode::Json);
    Ok(blueprint)
}

/// `rpi agent stdio`: ACP over Content-Length framed JSON-RPC on stdin/stdout.
pub async fn run_stdio(cli: Cli) -> Result<()> {
    let blueprint = acp_blueprint(&cli).await?;
    let (outbound, outbound_rx) = mpsc::channel::<Value>(OUTBOUND_CHANNEL_CAPACITY);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Value>(INBOUND_CHANNEL_CAPACITY);
    let reader = tokio::spawn(read_stdio_frames(
        tokio::io::stdin(),
        inbound_tx,
        outbound.clone(),
    ));
    let server = tokio::spawn(serve_connection(blueprint, cli, inbound_rx, outbound));
    let pump = tokio::spawn(pump_stdout(outbound_rx, tokio::io::stdout()));
    // A client closing stdin ends the connection; the reader's framing error
    // (peer EOF) is the expected shutdown signal and is not an error here.
    let _ = reader.await;
    match server.await {
        Ok(result) => result?,
        Err(error) => return Err(anyhow!(error).context("joining ACP stdio server")),
    }
    match pump.await {
        Ok(result) => result?,
        Err(error) => return Err(anyhow!(error).context("joining ACP stdio writer")),
    }
    Ok(())
}

/// Read Content-Length framed JSON-RPC messages from the client. Parse errors
/// (well-framed but invalid JSON) are answered with a JSON-RPC parse error via
/// `outbound` and the connection continues; framing failures (truncated
/// bodies, missing headers, EOF) end the reader, closing `tx` and signaling
/// the server.
async fn read_stdio_frames<R>(
    input: R,
    tx: mpsc::Sender<Value>,
    outbound: mpsc::Sender<Value>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
{
    let mut reader = BufReader::new(input);
    loop {
        match pi_coding::tools::framing::read_message("ACP", &mut reader, MAX_WS_FRAME_BYTES)
            .await
        {
            Ok(message) => {
                if tx.send(message).await.is_err() {
                    return Ok(());
                }
            }
            Err(error) => {
                if error
                    .root_cause()
                    .downcast_ref::<serde_json::Error>()
                    .is_some()
                {
                    let response = rpc_error(
                        &Value::Null,
                        PARSE_ERROR,
                        format!("invalid JSON in ACP message: {error}"),
                    );
                    if outbound.send(response).await.is_err() {
                        return Ok(());
                    }
                    continue;
                }
                // Framing/EOF failure: the connection is unusable.
                return Ok(());
            }
        }
    }
}

/// Serialize outbound ACP messages to stdout with Content-Length framing.
async fn pump_stdout<R>(mut outbound: mpsc::Receiver<Value>, mut stdout: R) -> Result<()>
where
    R: AsyncWrite + Unpin + Send,
{
    while let Some(message) = outbound.recv().await {
        let frame = pi_coding::tools::framing::encode_message(&message)?;
        stdout.write_all(&frame).await.context("writing ACP frame to stdout")?;
        stdout.flush().await.context("flushing ACP stdout")?;
    }
    Ok(())
}

/// `rpi agent serve`: ACP over a local WebSocket server. Each connection is an
/// independent ACP conversation (its own session registry).
///
/// Transport security mirrors the control-plane listener ([`super::ws_auth`]):
/// the server is **loopback-only** (plaintext WebSocket cannot safely carry
/// the bearer token off the local host; TLS is tracked for a later release),
/// a tokenless loopback accepts only native clients (browser `Origin` is
/// rejected), and a configured token is presented via
/// `Authorization: Bearer <token>` or the `rpi-auth.<token>` subprotocol.
/// Concurrent connection tasks are capped at [`MAX_CONNECTION_TASKS`];
/// connections beyond the cap are dropped.
pub async fn run_serve(
    cli: Cli,
    address: std::net::SocketAddr,
    token_file: Option<PathBuf>,
) -> Result<()> {
    // ACP always keeps the strict loopback-only policy even though the
    // separate `rpi --listen` surface supports an explicit remote opt-in.
    let token = load_auth_token(
        address.ip(),
        token_file.as_deref(),
        "agent serve",
        ListenAddressPolicy::LoopbackOnly,
    )?;
    let blueprint = acp_blueprint(&cli).await?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("binding ACP WebSocket server to {address}"))?;
    let address = listener.local_addr().context("reading ACP WebSocket server address")?;
    eprintln!("ACP WebSocket server listening on ws://{address}");
    if token.is_some() {
        eprintln!("ACP WebSocket connections require the token from {token_file:?} (rpi-auth.<token> subprotocol)");
    } else {
        eprintln!("ACP WebSocket accepts only loopback native clients (no token configured; browsers are rejected)");
    }
    serve_ws(listener, blueprint, cli, token.map(Arc::from)).await
}

/// Accept loop for `rpi agent serve`: spawns one task per WebSocket
/// connection, reaps finished tasks, and caps concurrent connection tasks at
/// [`MAX_CONNECTION_TASKS`] (mirroring the control-plane listener) so a
/// flood of clients cannot exhaust the runtime.
async fn serve_ws(
    listener: TcpListener,
    blueprint: RunSessionBlueprint,
    cli: Cli,
    token: Option<Arc<[u8]>>,
) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if connections.len() < MAX_CONNECTION_TASKS => {
                let (stream, _) = accepted.context("accepting ACP WebSocket connection")?;
                let blueprint = blueprint.clone();
                let cli = cli.clone();
                let token = token.clone();
                connections.spawn(async move {
                    if let Err(error) = handle_ws_connection(stream, blueprint, cli, token).await {
                        eprintln!("ACP WebSocket connection error: {error:#}");
                    }
                });
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                joined
                    .expect("guarded by non-empty connection set")
                    .context("joining ACP WebSocket connection task")?;
            }
            accepted = listener.accept(), if connections.len() >= MAX_CONNECTION_TASKS => {
                let (stream, _) =
                    accepted.context("accepting saturated ACP WebSocket connection")?;
                drop(stream);
            }
        }
    }
}

/// One WebSocket ACP connection: authenticate and upgrade, then run the
/// shared connection loop with WS text frames replacing Content-Length
/// framing.
async fn handle_ws_connection(
    stream: TcpStream,
    blueprint: RunSessionBlueprint,
    cli: Cli,
    token: Option<Arc<[u8]>>,
) -> Result<()> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_FRAME_BYTES))
        .max_frame_size(Some(MAX_WS_FRAME_BYTES));
    let websocket = accept_hdr_async_with_config(
        stream,
        move |request: &Request, mut response: Response| -> std::result::Result<Response, ErrorResponse> {
            if request.uri().path() != "/" {
                let mut error = ErrorResponse::new(Some("not found".into()));
                *error.status_mut() = http::StatusCode::NOT_FOUND;
                return Err(error);
            }
            // Same transport auth policy as the control-plane listener
            // (`ws_auth`), except ACP keeps the strict tokenless stance:
            // tokenless loopback accepts native clients and rejects browsers
            // (they always send Origin); a configured token must be
            // presented as `Authorization: Bearer <token>` or the
            // `rpi-auth.<token>` subprotocol, which is echoed verbatim in
            // the upgrade response so the browser accepts the handshake.
            let headers = request.headers();
            let protocol = websocket_subprotocol(headers, token.as_deref());
            if !authorized(headers, token.as_deref(), false) && protocol.is_none() {
                let mut error = ErrorResponse::new(Some("unauthorized".into()));
                *error.status_mut() = http::StatusCode::UNAUTHORIZED;
                return Err(error);
            }
            if let Some(protocol) = protocol {
                response.headers_mut().insert(
                    http::header::SEC_WEBSOCKET_PROTOCOL,
                    http::HeaderValue::from_str(&protocol)
                        .expect("matched subprotocol is a valid header value"),
                );
            }
            Ok(response)
        },
        Some(config),
    )
    .await
    .context("upgrading ACP WebSocket connection")?;
    let (mut write, mut read) = websocket.split();
    let (outbound, mut outbound_rx) = mpsc::channel::<Value>(OUTBOUND_CHANNEL_CAPACITY);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Value>(INBOUND_CHANNEL_CAPACITY);
    let server = tokio::spawn(serve_connection(blueprint, cli, inbound_rx, outbound));
    let reader = tokio::spawn(async move {
        loop {
            tokio::select! {
                incoming = read.next() => match incoming {
                    Some(Ok(WsMessage::Text(text))) => {
                        match serde_json::from_str::<Value>(&text) {
                            Ok(message) => {
                                if inbound_tx.send(message).await.is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                let response = rpc_error(
                                    &Value::Null,
                                    PARSE_ERROR,
                                    format!("invalid JSON in ACP message: {error}"),
                                );
                                if write
                                    .send(WsMessage::Text(
                                        serde_json::to_string(&response)?.into(),
                                    ))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(payload))) => {
                        if write.send(WsMessage::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Ok(WsMessage::Binary(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        eprintln!("ACP WebSocket read error: {error}");
                        break;
                    }
                },
                outgoing = outbound_rx.recv() => match outgoing {
                    Some(message) => {
                        let text: tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes =
                            serde_json::to_string(&message)?.into();
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        // The reader owns the write half: close the WebSocket cleanly once
        // the connection loop (or the server) is done.
        let _ = write.close().await;
        Ok::<(), anyhow::Error>(())
    });
    match reader.await {
        Ok(result) => result?,
        Err(error) => return Err(anyhow!(error).context("joining ACP WebSocket reader")),
    }
    match server.await {
        Ok(result) => result?,
        Err(error) => return Err(anyhow!(error).context("joining ACP WebSocket server")),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
