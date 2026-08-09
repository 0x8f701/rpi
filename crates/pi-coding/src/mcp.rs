//! Model Context Protocol (MCP) client: session-scoped JSON-RPC 2.0 over a
//! stdio child process (Content-Length framing, mirroring the LSP client in
//! `tools/lsp_client.rs` but kept self-contained with MCP-flavored errors).
//!
//! Servers are declared in settings under `mcpServers` (Grok-compatible
//! `[mcp_servers.<name>]` shape): `name`, `transport` (`stdio`|`sse`),
//! `command`/`args`/`env` for stdio, `url` for sse. The client transport in
//! this build is stdio; an sse entry parses and round-trips but `call` /
//! `list_tools` against it report the limitation explicitly.
//!
//! ## Lifecycle: session-scoped, spawn on first use
//!
//! [`McpRegistry`] holds the configured servers plus one live client per
//! server, spawned lazily on the first tool call and killed on drop — the
//! session owns the registry and `Drop` never leaks a child process.
//!
//! **Session cutover:** a logical session replacement in the same working
//! directory reaps every live client (awaited `shutdown` handshake + child
//! wait) via [`McpRegistry::reset_live_sessions`] before the replacement is
//! committed, so the new session never inherits the old session's stdio
//! children, initialized protocol state, or external auth. Configuration
//! survives the reset; the next tool call lazily spawns a fresh server.
//!
//! **Fast-start gate:** the first tool call against a server waits up to
//! [`SPAWN_DEFER`] (250 ms, matching OMP's startup window) while holding the
//! session lock, so sibling calls issued in the same turn batch into a single
//! spawn instead of N sequential spawns.
//!
//! **Reconnects:** transport failures (spawn, framing, io) are retried with
//! capped exponential backoff (100 ms → 1 s) up to [`MAX_CALL_ATTEMPTS`]
//! attempts per call, then surface an actionable error naming the server.
//! JSON-RPC protocol errors and request timeouts are not retried — the
//! wedged session is dropped so the next call respawns a fresh server.
//!
//! **Disabled servers:** entries with `disabled: true` (Cursor-compatible;
//! OMP's canonical shape is the inverse `enabled` flag, Claude Desktop's
//! `disabledMCPServers` list is mapped onto it by the config import) are
//! filtered out at configure time: they never spawn, have no session slot,
//! and never appear in `mcp list_servers`.
//!
//! **Progressive tool discovery:** when a server advertises the `search_tool`
//! extension (`capabilities.tools.search_tool` or
//! `capabilities.experimental.search_tool`), `tools/call` probes the tool
//! with a `tools/search` request and caches the definition instead of
//! requiring a full `tools/list` first. Servers that advertise the extension
//! but reject `tools/search` fall back to the full list automatically.
//!
//! ## Tool surface
//!
//! The `mcp` tool (`list_servers`, `list_tools <server>`,
//! `call <server> <tool> [args]`) renders tools/list as a name+description
//! table and tools/call results as bounded text. Configured `env` values are
//! never echoed into tool output. A server's stderr tail is only surfaced in
//! initialize-failure diagnostics, with secret patterns redacted first (see
//! [`redact_secrets`](crate::redact::redact_secrets)); results are truncated
//! to [`OUTPUT_MAX_BYTES`].

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};

use crate::redact::redact_secrets;
use crate::settings::{McpServerConfig, McpTransport};
use crate::tools::framing::{encode_message, MAX_JUNK_HEADER_LINES};
use crate::tools::{arg_str, check_aborted, s_object, s_string, text_result};
use crate::truncate::truncate_head;

/// Per-request timeout, matching OMP's `DEFAULT_REQUEST_TIMEOUT_MS`.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace for the server to exit after the `exit` notification before kill.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Shutdown-request grace before the client moves on to `exit`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on captured server stderr so a chatty server cannot balloon memory.
const STDERR_CAP: usize = 64 * 1024;
/// Cap on tools collected across tools/list pages.
const MAX_TOOLS: usize = 512;
/// Cap on tools/list pages followed through `nextCursor`.
const MAX_TOOL_PAGES: usize = 32;
/// Cap on a single rendered tool description line.
const TOOL_DESCRIPTION_MAX_CHARS: usize = 200;
/// Output byte budget for rendered tool results (matches github's cap).
const OUTPUT_MAX_BYTES: usize = 32 * 1024;
/// MCP protocol version requested on initialize (the widely supported stdio
/// version); the server's negotiated version is stored on the client.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Fast-start gate: the first tool call against a server waits this long for
/// sibling calls to the same server before spawning, so one spawn serves a
/// whole batch of calls (mirrors OMP's 250 ms startup window).
const SPAWN_DEFER: Duration = Duration::from_millis(250);
/// Base reconnect backoff, doubled per attempt (100 ms → 200 ms → ...).
const RECONNECT_BASE_DELAY_MS: u64 = 100;
/// Cap on the exponential reconnect backoff.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(1);
/// Maximum spawn+call attempts per tool call before an actionable error.
const MAX_CALL_ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Framing (Content-Length, shared with the LSP and DAP clients via
// `tools/framing`, with MCP-flavored error wording)
// ---------------------------------------------------------------------------

/// Marker wrapper for transport-layer failures (spawn, framing, io) that a
/// bounded reconnect may recover from. JSON-RPC protocol errors (server error
/// responses) and request timeouts are deliberately *not* marked — they
/// surface immediately and simply drop the session so the next call respawns.
///
/// The marker rides the `anyhow` error chain as the root cause, so
/// [`anyhow::Error::downcast_ref`] recognizes it through `.context()` layers.
#[derive(Debug)]
struct Retryable(anyhow::Error);

impl std::fmt::Display for Retryable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for Retryable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl Retryable {
    /// Wraps a transport error so [`is_retryable`] recognizes it.
    fn wrap(error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Retryable(error))
    }
}

/// True when the error chain carries a [`Retryable`] transport marker.
fn is_retryable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Retryable>().is_some()
}

/// Reads one `Content-Length` framed JSON-RPC message from `reader` with
/// MCP-flavored error wording. The framing logic itself is shared with the
/// LSP and DAP clients via [`crate::tools::framing`].
async fn read_message(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    crate::tools::framing::read_message(
        "MCP",
        reader,
        crate::tools::framing::DEFAULT_MAX_MESSAGE_BYTES,
    )
    .await
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A per-server MCP session: spawned child + framed stdin/stdout.
pub(crate) struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    initialized: bool,
    /// Protocol version negotiated during initialize.
    protocol_version: String,
    /// Server `capabilities` from the initialize result; used to detect the
    /// `search_tool` extension.
    capabilities: Value,
    /// Tool names advertised by the last full tools/list, used for
    /// client-side validation of tools/call targets. `None` until the first
    /// full list.
    tool_names: Option<Vec<String>>,
    /// Tool definitions discovered so far (from tools/list and/or the
    /// `search_tool` extension's tools/search), keyed by name — the
    /// progressive-discovery cache.
    tool_cache: BTreeMap<String, Value>,
    /// Bounded tail of the server's stderr, surfaced in error messages only.
    stderr_tail: Arc<Mutex<String>>,
}

impl McpClient {
    /// Spawns the configured stdio server as a child process.
    pub(crate) async fn spawn(config: &McpServerConfig) -> Result<Self> {
        let command_name = config
            .command
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if command_name.is_empty() {
            bail!("MCP server `{}` has no command (stdio transport)", config.name);
        }
        let mut command = Command::new(&command_name);
        if let Some(args) = &config.args {
            command.args(args);
        }
        if let Some(env) = &config.env {
            for (key, value) in env {
                command.env(key, value);
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        let mut child = command.spawn().with_context(|| {
            format!(
                "spawning MCP server `{}` (command `{command_name}`)",
                config.name
            )
        })?;
        let stdin = child.stdin.take().context("MCP server stdin unavailable")?;
        let stdout = child.stdout.take().context("MCP server stdout unavailable")?;
        let stderr = child.stderr.take().context("MCP server stderr unavailable")?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut reader = BufReader::new(stderr);
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut guard = tail.lock();
                            guard.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if guard.len() > STDERR_CAP {
                                let overflow = guard.len() - STDERR_CAP;
                                guard.drain(..overflow);
                            }
                        }
                    }
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            initialized: false,
            protocol_version: String::new(),
            capabilities: Value::Null,
            tool_names: None,
            tool_cache: BTreeMap::new(),
            stderr_tail,
        })
    }

    /// Sends a JSON-RPC notification (no response expected).
    pub(crate) async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.stdin
            .write_all(&encode_message(&message)?)
            .await
            .map_err(|error| Retryable::wrap(error.into()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| Retryable::wrap(error.into()))?;
        Ok(())
    }

    /// Sends a JSON-RPC request and waits for the matching response.
    ///
    /// Notifications pushed while waiting (e.g. `notifications/message`) are
    /// dropped; a response whose id does not match is skipped. Transport
    /// failures are wrapped in [`Retryable`] so the caller can reconnect.
    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.stdin
            .write_all(&encode_message(&message)?)
            .await
            .map_err(|error| Retryable::wrap(error.into()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| Retryable::wrap(error.into()))?;
        self.read_response(id).await
    }

    /// [`Self::request`] bounded by `timeout`.
    pub(crate) async fn request_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        match tokio::time::timeout(timeout, self.request(method, params)).await {
            Ok(result) => result,
            Err(_) => bail!(
                "MCP request `{method}` timed out after {}s",
                timeout.as_secs()
            ),
        }
    }

    /// Reads messages until the response with `id` arrives.
    async fn read_response(&mut self, id: i64) -> Result<Value> {
        loop {
            let message = read_message(&mut self.stdout)
                .await
                .map_err(Retryable::wrap)?;
            if message.get("id").is_none() {
                continue; // notification — nothing waits on it
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // response to a request this client never sent
            }
            if let Some(error) = message.get("error") {
                bail!("MCP request failed: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Runs the MCP `initialize` handshake and sends `notifications/initialized`.
    pub(crate) async fn initialize(&mut self) -> Result<Value> {
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "rpi",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let result = self
            .request_timeout("initialize", params, REQUEST_TIMEOUT)
            .await?;
        if let Some(version) = result.get("protocolVersion").and_then(Value::as_str) {
            self.protocol_version = version.to_owned();
        }
        self.capabilities = result
            .get("capabilities")
            .cloned()
            .unwrap_or(Value::Null);
        self.notify("notifications/initialized", json!({})).await?;
        self.initialized = true;
        Ok(result)
    }

    /// The protocol version negotiated during initialize (empty before).
    pub(crate) fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// Calls `tools/list`, following `nextCursor` pages, and caches the
    /// advertised tool names for [`Self::call_tool`] validation.
    pub(crate) async fn list_tools(&mut self) -> Result<Vec<Value>> {
        let mut tools: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self
                .request_timeout("tools/list", params, REQUEST_TIMEOUT)
                .await?;
            if let Some(page) = result.get("tools").and_then(Value::as_array) {
                tools.extend(page.iter().cloned());
            }
            if tools.len() > MAX_TOOLS {
                tools.truncate(MAX_TOOLS);
                break;
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        self.tool_names = Some(
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(String::from))
                .collect(),
        );
        for tool in &tools {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                self.tool_cache.insert(name.to_owned(), tool.clone());
            }
        }
        Ok(tools)
    }

    /// True when the server advertises the `search_tool` extension: a
    /// `tools/search` request that returns the definition for one tool name.
    /// Accepted under `capabilities.tools.search_tool` or the older
    /// `capabilities.experimental.search_tool` location, either as a boolean
    /// or an object.
    pub(crate) fn search_tool_supported(&self) -> bool {
        fn enabled(value: Option<&Value>) -> bool {
            matches!(value, Some(Value::Bool(true)) | Some(Value::Object(_)))
        }
        let capabilities = &self.capabilities;
        enabled(capabilities.pointer("/tools/search_tool"))
            || enabled(capabilities.pointer("/experimental/search_tool"))
    }

    /// Progressive discovery: resolves `name` to a tool definition without
    /// requiring a full `tools/list` first.
    ///
    /// When the server advertises the `search_tool` extension the definition
    /// is fetched lazily with a `tools/search` round-trip and cached; a
    /// server that advertises the extension but rejects the request (e.g. an
    /// older build without the method) falls back to a full, cached
    /// `tools/list`. Returns `Ok(false)` when the server authoritatively has
    /// no such tool.
    pub(crate) async fn resolve_tool(&mut self, name: &str) -> Result<bool> {
        if self.tool_cache.contains_key(name) {
            return Ok(true);
        }
        if self.search_tool_supported() {
            let params = json!({ "name": name });
            match self
                .request_timeout("tools/search", params, REQUEST_TIMEOUT)
                .await
            {
                Ok(result) => {
                    let matches = result
                        .get("tools")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(tool) = matches.into_iter().find(|tool| {
                        tool.get("name").and_then(Value::as_str) == Some(name)
                    }) {
                        self.tool_cache.insert(name.to_owned(), tool);
                        return Ok(true);
                    }
                    // Authoritative empty result: the server searched and
                    // found nothing.
                    return Ok(false);
                }
                Err(_) => {
                    // The advertised extension is not actually implemented:
                    // fall back to the full tools/list (also cached).
                    let tools = self.list_tools().await?;
                    return Ok(tools.iter().any(|tool| {
                        tool.get("name").and_then(Value::as_str) == Some(name)
                    }));
                }
            }
        }
        let tools = self.list_tools().await?;
        Ok(tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name)))
    }

    /// Calls `tools/call` with `name` and optional JSON `arguments`.
    ///
    /// When the advertised tool list is known, an unknown tool is rejected
    /// client-side with the available names. For servers advertising the
    /// `search_tool` extension, an unknown tool is first resolved lazily via
    /// `tools/search` (see [`Self::resolve_tool`]) and rejected from that
    /// result without loading the full list. Otherwise — no cached list, no
    /// search extension — the server's JSON-RPC error is surfaced verbatim.
    pub(crate) async fn call_tool(&mut self, name: &str, arguments: Option<Value>) -> Result<Value> {
        if let Some(names) = &self.tool_names {
            if !names.iter().any(|known| known == name) {
                bail!(
                    "MCP server has no tool `{name}` (available: {})",
                    names.join(", ")
                );
            }
        } else if self.search_tool_supported() {
            let resolved = self.resolve_tool(name).await?;
            if !resolved {
                bail!(
                    "MCP server has no tool `{name}` (searched via the server's `tools/search` \
                     extension; run `mcp list_tools <server>` for the full list)"
                );
            }
        }
        let params = match arguments {
            Some(arguments) => json!({ "name": name, "arguments": arguments }),
            None => json!({ "name": name }),
        };
        self.request_timeout("tools/call", params, REQUEST_TIMEOUT)
            .await
    }

    /// Best-effort MCP shutdown handshake, then reaps (and if necessary
    /// kills) the child. Never fails the caller.
    pub(crate) async fn shutdown(&mut self) {
        if self.initialized {
            let _ = tokio::time::timeout(
                SHUTDOWN_TIMEOUT,
                self.request("shutdown", json!(null)),
            )
            .await;
            let _ = self.notify("notifications/exit", json!(null)).await;
        }
        match tokio::time::timeout(EXIT_TIMEOUT, self.child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
        }
    }

    /// Bounded tail of the server's stderr (for diagnostics in error paths).
    pub(crate) fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().clone()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Never leak a server process, even on panic/abort paths.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Registry (session-scoped servers)
// ---------------------------------------------------------------------------

struct McpRegistryState {
    /// Configured (enabled) servers by name (from `Settings.mcp_servers`).
    /// Entries with `disabled: true` are filtered out here: they never spawn,
    /// have no session slot, and never appear in `list_servers`.
    servers: BTreeMap<String, McpServerConfig>,
    /// Live per-server sessions; `None` until the first tool call spawns one.
    sessions: BTreeMap<String, Arc<AsyncMutex<Option<McpClient>>>>,
    /// Fast-start window: how long the first tool call against a server waits
    /// for sibling calls before spawning (see [`SPAWN_DEFER`]).
    spawn_defer: Duration,
}

impl Default for McpRegistryState {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            spawn_defer: SPAWN_DEFER,
        }
    }
}

/// Session-scoped MCP server registry: spawn on first use, kill on drop.
///
/// Cloning shares the same state. A session builds one registry, threads it
/// into the `mcp` tool, and calls [`McpRegistry::configure`] on resource
/// reloads so settings changes take effect without losing live sessions whose
/// configuration did not change. Logical same-CWD session replacements call
/// [`McpRegistry::reset_live_sessions`] first so the new session starts with
/// fresh (uninitialized) MCP clients.
#[derive(Clone, Default)]
pub(crate) struct McpRegistry {
    inner: Arc<RwLock<McpRegistryState>>,
}

impl McpRegistry {
    /// An empty registry (no servers configured).
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A registry with a custom fast-start window. Tests use a shorter (or
    /// zero) window to keep the suite fast; production uses [`SPAWN_DEFER`].
    pub(crate) fn with_spawn_defer(defer: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(McpRegistryState {
                spawn_defer: defer,
                ..McpRegistryState::default()
            })),
        }
    }

    /// The fast-start window for this registry.
    fn spawn_defer(&self) -> Duration {
        self.inner.read().spawn_defer
    }

    /// Replaces the configured servers.
    ///
    /// Disabled entries are dropped outright — they never spawn and have no
    /// session slot. Live sessions whose configuration changed, that no
    /// longer exist, or that became disabled are dropped (killing their
    /// child); unchanged sessions survive.
    pub(crate) fn configure(&self, servers: Vec<McpServerConfig>) {
        let mut state = self.inner.write();
        let next: BTreeMap<String, McpServerConfig> = servers
            .into_iter()
            .filter(|server| !server.disabled)
            .map(|server| (server.name.clone(), server))
            .collect();
        let previous = state.servers.clone();
        state.sessions.retain(|name, _| {
            matches!((previous.get(name), next.get(name)), (Some(old), Some(new)) if old == new)
        });
        state.servers = next;
    }

    /// The configured servers, in name order.
    pub(crate) fn list_servers(&self) -> Vec<McpServerConfig> {
        self.inner.read().servers.values().cloned().collect()
    }

    /// The configuration for `name`, if configured.
    pub(crate) fn config(&self, name: &str) -> Option<McpServerConfig> {
        self.inner.read().servers.get(name).cloned()
    }

    /// The live-session slot for `name`, creating it on first use. `None`
    /// when no such server is configured.
    pub(crate) fn session(&self, name: &str) -> Option<Arc<AsyncMutex<Option<McpClient>>>> {
        let mut state = self.inner.write();
        if !state.servers.contains_key(name) {
            return None;
        }
        Some(
            state
                .sessions
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(None)))
                .clone(),
        )
    }

    /// Names of servers with a live session: the session slot exists and is
    /// either currently spawning (lock held) or holding a spawned client.
    /// Used by `mcp list_servers` to mark live servers and by tests.
    pub(crate) fn live_servers(&self) -> Vec<String> {
        let state = self.inner.read();
        state
            .sessions
            .iter()
            .filter(|(_, session)| {
                session
                    .try_lock()
                    .map(|guard| guard.is_some())
                    .unwrap_or(true)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Number of live (spawned) sessions — used by tests and diagnostics.
    #[cfg(test)]
    fn live_session_count(&self) -> usize {
        self.live_servers().len()
    }

    /// Reaps every live MCP client (awaited `shutdown` handshake + child
    /// wait) while preserving the configured servers. Session slots are
    /// reset to the empty state, so the next tool call lazily spawns a fresh
    /// server process.
    ///
    /// The registry map lock is held only while draining the slots (bounded,
    /// non-async); the actual shutdowns run outside the lock so a slow or
    /// wedged server can never block registry operations. Called on every
    /// logical same-CWD session cutover (new/fresh/resume/switch/fork/clone)
    /// before the replacement is committed: without it the new session would
    /// inherit the old session's stdio children, initialized protocol state,
    /// and external auth.
    ///
    /// Fails when a slot is still held by an in-flight tool call or a client
    /// refuses to die even after the kill fallback — the caller must not
    /// proceed, or the old server process leaks across the boundary.
    pub(crate) async fn reset_live_sessions(&self) -> Result<()> {
        // Drain under the write lock (bounded): replace the session map with
        // fresh empty slots and take the old ones out to reap them outside
        // the lock. Configuration (`servers`) is untouched.
        let drained: Vec<(String, Arc<AsyncMutex<Option<McpClient>>>)> = {
            let mut state = self.inner.write();
            std::mem::take(&mut state.sessions).into_iter().collect()
        };
        let mut failures: Vec<String> = Vec::new();
        for (name, slot) in drained {
            let mut guard = match slot.try_lock() {
                Ok(guard) => guard,
                Err(_) => {
                    // The cutover only runs on an idle session, so a held
                    // slot means a tool call is genuinely in flight — fail
                    // with context rather than leak the child.
                    failures.push(format!(
                        "MCP server `{name}` has a tool call in flight and could not be shut down"
                    ));
                    continue;
                }
            };
            let Some(mut client) = guard.take() else {
                continue; // empty slot (touched but never spawned) — nothing live
            };
            drop(guard);
            client.shutdown().await;
            if client.child.try_wait().ok().flatten().is_none() {
                // `shutdown` is best-effort; escalate to kill, then verify
                // the child is actually gone before allowing the cutover.
                let _ = client.child.kill().await;
                match tokio::time::timeout(EXIT_TIMEOUT, client.child.wait()).await {
                    Ok(Ok(_)) => {}
                    _ => failures.push(format!(
                        "MCP server `{name}` child process did not exit after shutdown"
                    )),
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!("failed to shut down live MCP sessions: {}", failures.join("; "))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool surface
// ---------------------------------------------------------------------------

/// The `mcp` tool: MCP server discovery and tool calls.
pub(crate) fn mcp_tool(registry: McpRegistry) -> AgentTool {
    let description = format!(
        "Call Model Context Protocol (MCP) servers configured under settings `mcpServers` \
         (stdio child processes). Actions: list_servers — show configured servers (live \
         sessions marked); list_tools <server> — list a server's tools with descriptions; \
         call <server> <tool> [args] — invoke a server tool with an optional JSON object \
         (or JSON string) of arguments. Servers spawn on first use (batched within a 250 ms \
         window) and shut down with the session; transport failures reconnect with bounded \
         backoff. Servers with `disabled: true` in settings are never spawned and are \
         omitted here. Results are bounded text and configured env values are never echoed."
    );
    let params = s_object(
        vec![
            (
                "action",
                s_string("MCP action to run: list_servers, list_tools, call"),
            ),
            (
                "server",
                s_string("Configured MCP server name (required for list_tools and call)"),
            ),
            (
                "tool",
                s_string("Tool name to invoke on the server (required for call)"),
            ),
            (
                "args",
                s_string(
                    "JSON arguments object for the tool call, or a JSON string that parses to \
                     an object (optional for call)",
                ),
            ),
        ],
        vec!["action"],
    );
    AgentTool::new("mcp", description, params, move |ctx: ToolCallContext| {
        let registry = registry.clone();
        async move { run_mcp(&registry, ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Start with `mcp list_servers` to see configured servers, then `mcp list_tools <server>` to discover what each exposes.".to_string(),
        "MCP servers may mutate external state (files, repositories, chat apps) — review call arguments before invoking.".to_string(),
        "Pass `args` as a JSON object (or JSON string); non-JSON values are rejected.".to_string(),
    ])
}

/// Entry point: validates the action and dispatches.
pub(crate) async fn run_mcp(
    registry: &McpRegistry,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action = arg_str(&args, "action");
    let action = action.trim();
    if action.is_empty() {
        bail!("mcp action is required (one of: list_servers, list_tools, call)");
    }
    match action {
        "list_servers" => Ok(text_result(list_servers_text(registry))),
        "list_tools" => {
            let server = required_server(&args)?;
            let render_server = server.clone();
            run_with_server(registry, &server, &abort, move |client| {
                let render_server = render_server.clone();
                async move {
                    let tools = client.list_tools().await?;
                    Ok(text_result(render_tools(&render_server, &tools)))
                }
                .boxed()
            })
            .await
        }
        "call" => {
            let server = required_server(&args)?;
            let render_server = server.clone();
            let tool = arg_str(&args, "tool");
            let tool = tool.trim().to_owned();
            if tool.is_empty() {
                bail!("mcp call requires a `tool` name");
            }
            let arguments = parse_call_args(&args)?;
            run_with_server(registry, &server, &abort, move |client| {
                let tool = tool.clone();
                let arguments = arguments.clone();
                let render_server = render_server.clone();
                async move {
                    let result = client.call_tool(&tool, arguments).await?;
                    Ok(text_result(render_call_result(&render_server, &tool, &result)))
                }
                .boxed()
            })
            .await
        }
        other => bail!(
            "unknown mcp action `{other}` (expected one of: list_servers, list_tools, call)"
        ),
    }
}

/// Reconnect backoff for the next spawn attempt (1-based attempt number of
/// the upcoming spawn): 100 ms after the first failure, then doubled, capped
/// at [`RECONNECT_MAX_DELAY`].
fn reconnect_delay(next_attempt: usize) -> Duration {
    let exponent = (next_attempt as u32).saturating_sub(2).min(10);
    let ms = RECONNECT_BASE_DELAY_MS
        .checked_shl(exponent)
        .unwrap_or(u64::MAX);
    Duration::from_millis(
        ms.min(RECONNECT_MAX_DELAY.as_millis() as u64),
    )
}

/// Spawns and initializes the configured stdio server, attaching the
/// (redacted, bounded) stderr tail to initialize failures. Transport-level
/// failures keep their [`Retryable`] marker so the reconnect loop recognizes
/// them; JSON-RPC initialize errors do not.
async fn spawn_initialized(
    config: &McpServerConfig,
    server: &str,
) -> Result<McpClient> {
    let mut client = McpClient::spawn(config)
        .await
        .map_err(Retryable::wrap)
        .with_context(|| format!("spawning MCP server `{server}`"))?;
    if let Err(error) = client.initialize().await {
        let retryable = is_retryable(&error);
        let stderr = redact_secrets(&client.stderr_tail());
        let mut message = format!("MCP server `{server}` failed to initialize: {error:#}");
        if !stderr.trim().is_empty() {
            message.push_str("\n--- server stderr ---\n");
            message.push_str(&stderr);
        }
        // The initialize error text is server-controlled; run the whole
        // message through the redactor, not just the tail.
        let final_error = anyhow!(redact_secrets(&message));
        return if retryable {
            Err(Retryable::wrap(final_error))
        } else {
            Err(final_error)
        };
    }
    Ok(client)
}

/// Runs `f` against the live session for `server`, spawning and initializing
/// the child on first use.
///
/// **Fast-start gate:** the first attempt waits up to the registry's
/// [`SPAWN_DEFER`] window (holding the session lock) so sibling tool calls to
/// the same server batch into one spawn.
///
/// **Reconnect:** transport failures (spawn, framing, io) retry with capped
/// exponential backoff up to [`MAX_CALL_ATTEMPTS`] attempts, then surface an
/// actionable error naming the server. JSON-RPC protocol errors and request
/// timeouts are not retried: the session is dropped so the next call
/// respawns a fresh server instead of reusing a wedged one.
///
/// `f` must be a plain `Fn` (not `FnOnce`): a retried call re-runs it against
/// the respawned client.
async fn run_with_server<T, F>(
    registry: &McpRegistry,
    server: &str,
    abort: &AbortSignal,
    f: F,
) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> Fn(&'a mut McpClient) -> BoxFuture<'a, Result<T>> + Send + 'static,
{
    let session = registry.session(server).ok_or_else(|| {
        let configured = registry
            .list_servers()
            .iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>();
        if configured.is_empty() {
            anyhow!(
                "no MCP servers configured (settings `mcpServers` is empty or all entries are \
                 disabled); add a stdio server entry and reload, then `mcp list_tools <server>`"
            )
        } else {
            anyhow!(
                "no MCP server configured with name `{server}`; configured: {}",
                configured.join(", ")
            )
        }
    })?;
    let config = registry
        .config(server)
        .expect("session entry implies configured server");
    if config.transport == McpTransport::Sse {
        bail!(
            "MCP server `{server}` uses the sse transport, which is not implemented in this \
             build (stdio servers only); configure it with transport \"stdio\" and a command"
        );
    }
    let mut guard = session.lock().await;
    let spawn_defer = registry.spawn_defer();
    let mut last_error: Option<anyhow::Error> = None;
    let mut stderr_tail = String::new();
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        if guard.is_none() {
            let delay = if attempt == 1 {
                // Fast-start gate: hold the session lock while waiting so any
                // sibling calls arriving in this window queue behind us and
                // share the single spawn.
                spawn_defer
            } else {
                reconnect_delay(attempt)
            };
            if !delay.is_zero() {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = abort.cancelled() => bail!("Operation aborted"),
                }
            }
            match spawn_initialized(&config, server).await {
                Ok(client) => *guard = Some(client),
                Err(error) => {
                    stderr_tail.clear();
                    // Only transport-level spawn/initialize failures are worth
                    // a bounded retry; a JSON-RPC refusal (e.g. a server that
                    // rejects initialize) will not succeed on retry.
                    let retryable = is_retryable(&error);
                    last_error = Some(error);
                    if !retryable || attempt >= MAX_CALL_ATTEMPTS {
                        break;
                    }
                    continue;
                }
            }
        }
        let outcome = {
            let client = guard.as_mut().expect("spawned above");
            tokio::select! {
                result = f(client) => result,
                _ = abort.cancelled() => Err(anyhow!("Operation aborted")),
            }
        };
        match outcome {
            Ok(value) => return Ok(value),
            Err(error) if abort.is_aborted() => {
                // Clean abort: drop the session like any error, but surface
                // the abort itself rather than a retry summary.
                *guard = None;
                return Err(error);
            }
            Err(error) => {
                stderr_tail = guard
                    .as_ref()
                    .map(|client| client.stderr_tail())
                    .unwrap_or_default();
                // Drop the wedged client; a retry (or the next call) respawns.
                *guard = None;
                last_error = Some(error);
                if !is_retryable(last_error.as_ref().expect("set above"))
                    || attempt >= MAX_CALL_ATTEMPTS
                {
                    break;
                }
            }
        }
    }
    let last = last_error.unwrap_or_else(|| anyhow!("MCP request to `{server}` failed"));
    let attempts_label = if attempt == 1 { "attempt" } else { "attempts" };
    let mut message = format!(
        "MCP server `{server}` failed after {attempt} {attempts_label}: {last:#}"
    );
    let stderr = redact_secrets(&stderr_tail);
    if !stderr.trim().is_empty() && !message.contains("--- server stderr ---") {
        message.push_str("\n--- server stderr ---\n");
        message.push_str(&stderr);
    }
    bail!(redact_secrets(&message))
}

fn required_server(args: &Value) -> Result<String> {
    let server = arg_str(args, "server");
    let server = server.trim();
    if server.is_empty() {
        bail!("mcp `server` is required for this action");
    }
    Ok(server.to_owned())
}

/// Parses the optional `args` argument: a JSON object, a JSON string that
/// parses to an object, or absent/null.
fn parse_call_args(args: &Value) -> Result<Option<Value>> {
    let Some(raw) = args.get("args") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    if let Some(object) = raw.as_object() {
        return Ok(Some(Value::Object(object.clone())));
    }
    if let Some(text) = raw.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let parsed: Value = serde_json::from_str(trimmed)
            .with_context(|| "mcp call `args` must be a JSON object or a JSON string")?;
        if !parsed.is_object() {
            bail!("mcp call `args` must parse to a JSON object");
        }
        return Ok(Some(parsed));
    }
    bail!("mcp call `args` must be a JSON object or a JSON string")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn list_servers_text(registry: &McpRegistry) -> String {
    let servers = registry.list_servers();
    if servers.is_empty() {
        return "No MCP servers configured. Add entries under settings `mcpServers` (e.g. \
                { name, transport: \"stdio\", command, args?, env?, disabled? }) and reload, \
                then `mcp list_tools <server>`. Entries with `disabled: true` are never \
                spawned and do not appear here."
            .to_string();
    }
    let live = registry.live_servers();
    let mut out = format!("MCP servers ({} configured):", servers.len());
    for server in &servers {
        let target = match server.transport {
            McpTransport::Stdio => {
                let mut command = server.command.clone().unwrap_or_default();
                if let Some(args) = &server.args {
                    command.push(' ');
                    command.push_str(&args.join(" "));
                }
                format!("stdio: {command}")
            }
            McpTransport::Sse => {
                format!("sse: {}", server.url.clone().unwrap_or_default())
            }
        };
        // `env` is deliberately not rendered: configured values may hold secrets.
        let live_marker = if live.contains(&server.name) {
            " (live)"
        } else {
            ""
        };
        out.push_str(&format!("\n- {}{} ({target})", server.name, live_marker));
    }
    out
}

fn render_tools(server: &str, tools: &[Value]) -> String {
    if tools.is_empty() {
        return format!("MCP server `{server}` advertises no tools.");
    }
    let mut out = format!("Tools from MCP server `{server}` ({}):", tools.len());
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        out.push_str(&format!("\n- {name}"));
        if let Some(description) = tool.get("description").and_then(Value::as_str) {
            let description = description.trim();
            if !description.is_empty() {
                let mut line = description.replace(['\n', '\r'], " ");
                if line.chars().count() > TOOL_DESCRIPTION_MAX_CHARS {
                    line = line
                        .chars()
                        .take(TOOL_DESCRIPTION_MAX_CHARS)
                        .collect::<String>();
                    line.push('…');
                }
                out.push_str(&format!(" — {line}"));
            }
        }
    }
    out
}

fn render_call_result(server: &str, tool: &str, result: &Value) -> String {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    for block in &content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text_block) = block.get("text").and_then(Value::as_str) {
                    text.push_str(text_block);
                    if !text_block.ends_with('\n') {
                        text.push('\n');
                    }
                }
            }
            Some("image") => text.push_str("[image result omitted — binary data not rendered]\n"),
            Some("resource") => {
                let uri = block
                    .pointer("/resource/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("");
                text.push_str(&format!("[resource {uri} ({mime}) omitted]\n"));
            }
            other => text.push_str(&format!("[unexpected content block {other:?}]\n")),
        }
    }
    let rendered = if text.trim().is_empty() {
        // No renderable content blocks: fall back to the raw result JSON.
        serde_json::to_string_pretty(result).unwrap_or_else(|_| format!("{result}"))
    } else {
        text
    };
    let bounded = truncate_head(&rendered, 0, OUTPUT_MAX_BYTES).content;
    let mut out = format!("MCP server `{server}` tool `{tool}`");
    if is_error {
        out.push_str(" reported an error");
    }
    out.push_str(":\n");
    out.push_str(&bounded);
    if rendered.len() > OUTPUT_MAX_BYTES {
        out.push_str("\n[output truncated]");
    }
    out
}

#[cfg(test)]
pub(crate) fn fake_server_exe() -> std::path::PathBuf {
    use std::sync::LazyLock;
    static COPY: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
        let exe = std::env::current_exe().expect("test binary path");
        let copy = std::env::temp_dir().join(format!(
            "pi-fake-mcp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::copy(&exe, &copy).expect("copy test binary for the fake MCP server");
        copy
    });
    COPY.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};

    use serde_json::json;

    use crate::settings::{McpServerConfig, McpTransport};

    fn stdio_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: Some("fake".to_owned()),
            args: None,
            url: None,
            env: None,
            extra: Default::default(),
        }
    }

    /// A registry with a minimal fast-start window so spawn tests stay fast;
    /// the fast-start batching behavior itself is covered by dedicated tests.
    fn test_registry() -> McpRegistry {
        McpRegistry::with_spawn_defer(Duration::from_millis(5))
    }

    // -----------------------------------------------------------------------
    // Framing
    // -----------------------------------------------------------------------

    #[test]
    fn encode_message_uses_content_length_framing() {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
        let bytes = encode_message(&body).unwrap();
        let serialized = serde_json::to_vec(&body).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
        assert!(
            bytes.starts_with(header.as_bytes()),
            "expected header prefix {header:?} in {bytes:?}"
        );
        assert_eq!(&bytes[header.len()..], &serialized[..]);
    }

    #[test]
    fn encode_message_counts_bytes_not_chars() {
        let body = json!({ "text": "héllo 世界 — ünïcode" });
        let bytes = encode_message(&body).unwrap();
        let serialized = serde_json::to_vec(&body).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
        assert!(bytes.starts_with(header.as_bytes()));
        let text_chars = body["text"].as_str().unwrap().chars().count();
        assert!(serialized.len() > text_chars, "bytes {} vs chars {text_chars}", serialized.len());
        assert_eq!(&bytes[header.len()..], &serialized[..]);
    }

    #[tokio::test]
    async fn read_message_decodes_multiple_framed_messages() {
        let mut payload = Vec::new();
        payload.extend(encode_message(&json!({"jsonrpc":"2.0","id":1,"result":true})).unwrap());
        payload.extend(encode_message(&json!({"jsonrpc":"2.0","id":2,"result":"two"})).unwrap());
        let mut reader = BufReader::new(&payload[..]);
        let first = read_message(&mut reader).await.unwrap();
        let second = read_message(&mut reader).await.unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["result"], true);
        assert_eq!(second["id"], 2);
        assert_eq!(second["result"], "two");
    }

    #[tokio::test]
    async fn read_message_accepts_any_header_case_and_order() {
        let body = r#"{"jsonrpc":"2.0","id":9,"result":"ok"}"#;
        let payload = format!(
            "x-custom: ignored\r\ncontent-length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message(&mut reader).await.unwrap();
        assert_eq!(message["id"], 9);
        assert_eq!(message["result"], "ok");
    }

    #[tokio::test]
    async fn read_message_rejects_missing_content_length() {
        let payload = format!("{}\r\n\r\n{{}}", "\n".repeat(MAX_JUNK_HEADER_LINES + 1));
        let mut reader = BufReader::new(payload.as_bytes());
        let err = read_message(&mut reader).await.unwrap_err().to_string();
        assert!(err.to_string().contains("Content-Length"), "{err}");
    }

    #[tokio::test]
    async fn read_message_skips_junk_lines_before_headers() {
        let body = r#"{"jsonrpc":"2.0","id":7,"result":true}"#;
        let payload = format!(
            "\n\nrunning 1 test\n\ntest mcp::x ... ok\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message(&mut reader).await.unwrap();
        assert_eq!(message["id"], 7);
        assert_eq!(message["result"], true);
    }

    #[tokio::test]
    async fn read_message_reports_eof() {
        let mut reader = BufReader::new(&b""[..]);
        let err = read_message(&mut reader).await.unwrap_err().to_string();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Fake MCP server
    // -----------------------------------------------------------------------
    //
    // The client tests below spawn a fake MCP server by re-executing this test
    // binary with `--exact mcp::tests::fake_mcp_server_process` (substring
    // filter) and `PI_FAKE_MCP_SERVER=1`. The test then acts as a minimal
    // server speaking initialize/tools/list/tools/call/shutdown/exit over
    // Content-Length framing (implemented independently of the client's
    // framing so a framing asymmetry fails the test).

    /// Runs the fake server loop when invoked via the env-var re-exec trick;
    /// a silent no-op when the test suite runs it directly.
    ///
    /// Modes (all optional):
    /// - `PI_FAKE_MCP_SERVER_BOOM=1`: log credential-shaped text to stderr,
    ///   then refuse initialize — exercises the stderr-tail redaction.
    /// - `PI_FAKE_MCP_TRACE=<path>`: append `spawn`, `list`, `search:<name>`
    ///   and `call:<name>` lines to the file so tests can count spawns and
    ///   observe the discovery path taken.
    /// - `PI_FAKE_MCP_PID_FILE=<path>`: append this process's pid on spawn so
    ///   tests can tell server generations apart across session cutovers
    ///   (each logical session must get a fresh client process).
    /// - `PI_FAKE_MCP_SEARCH=1`: advertise the `search_tool` extension and
    ///   answer `tools/search` from the same tool set.
    /// - `PI_FAKE_MCP_SEARCH_FALLBACK=1`: advertise `search_tool` but answer
    ///   `tools/search` with method-not-found, forcing the client's full-list
    ///   fallback.
    /// - `PI_FAKE_MCP_CRASH_MARK=<path>`: the first spawned process creates
    ///   the marker and exits on the first `tools/call`; later processes serve
    ///   normally — simulates a one-off transport failure.
    /// - `PI_FAKE_MCP_CRASH_ALWAYS=1`: every `tools/call` exits, simulating a
    ///   persistently broken server.
    #[test]
    fn fake_mcp_server_process() {
        if std::env::var_os("PI_FAKE_MCP_SERVER").is_none() {
            return;
        }
        // Boom mode (PI_FAKE_MCP_SERVER_BOOM=1): log credential-shaped text
        // to stderr, then refuse initialize — exercises the stderr-tail
        // redaction in the initialize-failure error path.
        let boom = std::env::var_os("PI_FAKE_MCP_SERVER_BOOM").is_some();
        if boom {
            let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123456789"].concat();
            let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
            eprintln!("fake mcp server: token={ghp} {sk}");
            // Let the client's stderr reader drain the line before the
            // initialize error is answered, so the tail is captured.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Trace: append one line per lifecycle event (spawn / list / search /
        // call) so tests can count spawns and inspect the discovery path.
        let trace_path = std::env::var_os("PI_FAKE_MCP_TRACE").map(std::path::PathBuf::from);
        fn trace(trace_path: &Option<std::path::PathBuf>, line: &str) {
            if let Some(path) = trace_path {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(file, "{line}");
                }
            }
        }
        trace(&trace_path, "spawn");
        // PID identity: append this process's pid once per spawn so tests can
        // distinguish server generations across resets/session cutovers.
        if let Some(pid_file) = std::env::var_os("PI_FAKE_MCP_PID_FILE") {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(pid_file)
            {
                let _ = writeln!(file, "{}", std::process::id());
            }
        }
        // Crash modes: a one-off transport failure (marker file) or a
        // persistently broken server.
        let crash_mark = std::env::var_os("PI_FAKE_MCP_CRASH_MARK").map(std::path::PathBuf::from);
        let crash_on_first_call = match &crash_mark {
            Some(path) if !path.exists() => {
                let _ = std::fs::write(path, b"crashed");
                true
            }
            _ => false,
        };
        let crash_always = std::env::var_os("PI_FAKE_MCP_CRASH_ALWAYS").is_some();
        let search_mode = std::env::var_os("PI_FAKE_MCP_SEARCH").is_some();
        let search_fallback = std::env::var_os("PI_FAKE_MCP_SEARCH_FALLBACK").is_some();
        let search_experimental = std::env::var_os("PI_FAKE_MCP_SEARCH_EXPERIMENTAL").is_some();
        let advertise_search = search_mode || search_fallback || search_experimental;
        // Paging mode: tools/list answers in two pages via nextCursor.
        let page_mode = std::env::var_os("PI_FAKE_MCP_PAGE").is_some();
        // Notification mode: push an id-less notifications/message before
        // every tools/call answer, so the client must skip it.
        let notify_mode = std::env::var_os("PI_FAKE_MCP_NOTIFY").is_some();
        // The tool definitions served by tools/list and tools/search.
        let tool_defs = || {
            json!([
                {
                    "name": "echo",
                    "description": "Echo the given message back",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "message": { "type": "string" } }
                    }
                },
                {
                    "name": "add",
                    "description": "Add two integers and return the sum"
                },
                {
                    "name": "large",
                    "description": "Return a very large text payload"
                }
            ])
        };
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut writer = std::io::BufWriter::new(stdout.lock());

        fn send(writer: &mut std::io::BufWriter<std::io::StdoutLock<'_>>, body: &Value) {
            let bytes = sync_encode(body).expect("fake server encode");
            writer.write_all(&bytes).expect("fake server write");
            writer.flush().expect("fake server flush");
        }

        loop {
            let message = sync_read_message(&mut reader).expect("fake server read");
            let method = message.get("method").and_then(Value::as_str);
            let id = message.get("id").cloned();
            match (method, id) {
                (Some("initialize"), Some(id)) if boom => send(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32000, "message": "initialize refused by fake server" }
                    }),
                ),
                (Some("initialize"), Some(id)) => {
                    let capabilities = if search_experimental {
                        // The older `experimental.search_tool` location, in
                        // object form rather than a plain boolean.
                        json!({ "experimental": { "search_tool": { "version": 1 } } })
                    } else if advertise_search {
                        json!({ "tools": { "search_tool": true } })
                    } else {
                        json!({ "tools": {} })
                    };
                    send(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": capabilities,
                                "serverInfo": { "name": "fake-mcp", "version": "1.0.0" }
                            }
                        }),
                    );
                }
                (Some("notifications/initialized"), _) => {}
                (Some("tools/search"), Some(id)) => {
                    trace(&trace_path, &format!("search:{}", message.pointer("/params/name").and_then(Value::as_str).unwrap_or("")));
                    if search_fallback {
                        send(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": "Method not found: tools/search" }
                            }),
                        );
                    } else {
                        let wanted = message
                            .pointer("/params/name")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let tools = tool_defs()
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|tool| tool.get("name").and_then(Value::as_str) == Some(wanted))
                            .collect::<Vec<_>>();
                        send(
                            &mut writer,
                            &json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } }),
                        );
                    }
                }
                (Some("tools/list"), Some(id)) => {
                    if page_mode {
                        let cursor = message
                            .pointer("/params/cursor")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        trace(&trace_path, &format!("list:{cursor}"));
                        let defs = tool_defs();
                        let defs = defs.as_array().cloned().unwrap_or_default();
                        match cursor {
                            "" => send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "tools": [defs[0].clone(), defs[1].clone()],
                                        "nextCursor": "page2",
                                    }
                                }),
                            ),
                            "page2" => send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": { "tools": [defs[2].clone()] }
                                }),
                            ),
                            other => send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": {
                                        "code": -32602,
                                        "message": format!("Unknown cursor: {other}")
                                    }
                                }),
                            ),
                        }
                    } else {
                        trace(&trace_path, "list");
                        send(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "tools": tool_defs() }
                            }),
                        );
                    }
                }
                (Some("tools/call"), Some(id)) => {
                    if notify_mode {
                        // Push an id-less notification the client must skip
                        // while waiting for its response.
                        send(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/message",
                                "params": { "level": "info", "data": "server says hi" }
                            }),
                        );
                    }
                    let tool_name = message
                        .pointer("/params/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    trace(&trace_path, &format!("call:{tool_name}"));
                    if crash_on_first_call || crash_always {
                        // Simulate a transport failure: die without answering.
                        std::process::exit(1);
                    }
                    match tool_name {
                        "echo" => {
                            let message_text = message
                                .pointer("/params/arguments/message")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{ "type": "text", "text": message_text }]
                                    }
                                }),
                            );
                        }
                        "add" => {
                            let a = message
                                .pointer("/params/arguments/a")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                            let b = message
                                .pointer("/params/arguments/b")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                            send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{
                                            "type": "text",
                                            "text": format!("{}", a + b)
                                        }]
                                    }
                                }),
                            );
                        }
                        "large" => {
                            let big = "x".repeat(100_000);
                            send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{ "type": "text", "text": big }]
                                    }
                                }),
                            );
                        }
                        "boom" => {
                            send(
                                &mut writer,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "isError": true,
                                        "content": [{
                                            "type": "text",
                                            "text": "boom happened"
                                        }]
                                    }
                                }),
                            );
                        }
                        other => send(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32602,
                                    "message": format!("Unknown tool: {other}")
                                }
                            }),
                        ),
                    }
                }
                (Some("shutdown"), Some(id)) => send(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                ),
                (Some("notifications/exit"), _) => return,
                _ => {}
            }
        }
    }

    /// Independent sync framing writer for the fake server, kept separate from
    /// the async client writer so a framing asymmetry fails the tests.
    fn sync_encode(body: &Value) -> std::io::Result<Vec<u8>> {
        let json = serde_json::to_vec(body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
        out.extend_from_slice(&json);
        Ok(out)
    }

    /// Independent sync framing reader for the fake server.
    fn sync_read_message(reader: &mut impl std::io::BufRead) -> std::io::Result<Value> {
        let mut header = String::new();
        let mut content_length = None;
        loop {
            header.clear();
            if reader.read_line(&mut header)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "fake server stdin closed",
                ));
            }
            if header == "\r\n" || header == "\n" {
                break;
            }
            if let Some((name, value)) = header.trim_end().split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    content_length = Some(value.trim().parse().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                    })?);
                }
            }
        }
        let length = content_length.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
        })?;
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        serde_json::from_slice(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Path to this test binary for the re-exec fake server (copied once per
    /// test process to a private temp path — see the top-level helper).
    fn fake_server_exe() -> std::path::PathBuf {
        crate::mcp::fake_server_exe()
    }

    async fn spawn_fake_client_with(extra_env: &[(&str, &str)]) -> McpClient {
        let mut config = fake_server_config();
        {
            let env = config.env.as_mut().expect("fake env");
            for (key, value) in extra_env {
                env.insert((*key).to_owned(), (*value).to_owned());
            }
        }
        let mut client = McpClient::spawn(&config)
            .await
            .expect("fake server spawn");
        client
            .initialize()
            .await
            .expect("fake server initializes");
        client
    }

    async fn spawn_fake_client() -> McpClient {
        spawn_fake_client_with(&[]).await
    }

    #[tokio::test]
    async fn client_initializes_and_lists_tools() {
        let mut client = spawn_fake_client().await;
        assert_eq!(client.protocol_version(), "2024-11-05");
        let tools = client.list_tools().await.expect("tools/list");
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["echo", "add", "large"]);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn client_calls_tool_with_object_arguments() {
        let mut client = spawn_fake_client().await;
        let _ = client.list_tools().await.expect("tools/list");
        let result = client
            .call_tool("add", Some(json!({ "a": 20, "b": 22 })))
            .await
            .expect("tools/call");
        assert_eq!(result["content"][0]["text"], "42");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn client_rejects_unknown_tool_against_cached_list() {
        let mut client = spawn_fake_client().await;
        let _ = client.list_tools().await.expect("tools/list");
        let err = client
            .call_tool("does_not_exist", None)
            .await
            .expect_err("unknown tool must fail");
        assert!(
            err.to_string().contains("no tool `does_not_exist`"),
            "{err}"
        );
        assert!(err.to_string().contains("echo"), "{err}");
    }

    #[tokio::test]
    async fn client_surfaces_server_error_for_unknown_tool_without_cache() {
        // A fresh session with no cached tools/list lets the server answer.
        let mut client = spawn_fake_client().await;
        let err = client
            .call_tool("does_not_exist", None)
            .await
            .expect_err("server error must surface");
        let text = err.to_string();
        assert!(text.contains("Unknown tool: does_not_exist"), "{text}");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn client_renders_is_error_results() {
        let mut client = spawn_fake_client().await;
        let result = client.call_tool("boom", None).await.expect("tools/call");
        assert_eq!(result["isError"], true);
        let rendered = render_call_result("fake", "boom", &result);
        assert!(rendered.contains("reported an error"), "{rendered}");
        assert!(rendered.contains("boom happened"), "{rendered}");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn client_shutdown_handshake_completes() {
        let mut client = spawn_fake_client().await;
        client.shutdown().await;
        // The child must have exited; nothing more to assert without a race,
        // but a wedged shutdown would hang the test binary's exit wait.
    }

    #[tokio::test]
    async fn spawn_requires_a_command() {
        // A stdio entry without a command must fail before any spawn attempt
        // with an actionable error naming the server.
        let config = McpServerConfig {
            name: "hollow".to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: None,
            args: None,
            url: None,
            env: None,
            extra: Default::default(),
        };
        let err = match McpClient::spawn(&config).await {
            Ok(_) => panic!("spawn without a command must fail"),
            Err(error) => error,
        };
        assert!(
            err.to_string().contains("`hollow` has no command"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn client_follows_tools_list_paging() {
        // The server answers tools/list in two pages via nextCursor; the
        // client must follow the cursor, aggregate both pages, and cache the
        // union for client-side call validation.
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let mut client = spawn_fake_client_with(&[
            ("PI_FAKE_MCP_PAGE", "1"),
            ("PI_FAKE_MCP_TRACE", trace_file.to_str().expect("trace path")),
        ])
        .await;

        let tools = client.list_tools().await.expect("paged tools/list");
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["echo", "add", "large"], "pages must be aggregated");

        let lines = read_trace_lines(&trace_file);
        assert!(
            lines.iter().any(|line| line == "list:"),
            "first page requested without a cursor: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "list:page2"),
            "second page must follow nextCursor: {lines:?}"
        );

        // The paged union is cached: a tool from the second page resolves,
        // and an unknown tool is rejected client-side.
        let result = client.call_tool("large", None).await.expect("tool from second page");
        assert_eq!(result["content"][0]["text"], "x".repeat(100_000));
        let err = client
            .call_tool("missing", None)
            .await
            .expect_err("unknown tool must be rejected");
        assert!(err.to_string().contains("no tool `missing`"), "{err}");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn client_skips_interleaved_notifications_while_waiting() {
        // The server pushes an id-less notifications/message before its
        // tools/call answer; the client must skip notifications and still
        // pair the response with its own request id.
        let mut client = spawn_fake_client_with(&[("PI_FAKE_MCP_NOTIFY", "1")]).await;
        let result = client
            .call_tool("echo", Some(json!({ "message": "hi" })))
            .await
            .expect("call with interleaved notification");
        assert_eq!(result["content"][0]["text"], "hi");
        // A second call still matches its own (incremented) request id.
        let result = client
            .call_tool("add", Some(json!({ "a": 1, "b": 2 })))
            .await
            .expect("second call");
        assert_eq!(result["content"][0]["text"], "3");
        client.shutdown().await;
    }

    // -----------------------------------------------------------------------
    // Tool surface
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_tool_list_servers_with_empty_registry() {
        let registry = McpRegistry::new();
        let tool = mcp_tool(registry);
        let context = tool_context(json!({ "action": "list_servers" }));
        let result = (tool.execute)(context).await.expect("list_servers");
        let text = result_text(&result);
        assert!(text.contains("No MCP servers configured"), "{text}");
    }

    /// Concatenates the text blocks of a tool result.
    fn result_text(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn tool_context(arguments: Value) -> pi_agent::ToolCallContext {
        pi_agent::ToolCallContext {
            tool_call_id: "1".to_owned(),
            arguments,
            on_update: Arc::new(|_| {}),
            abort: AbortSignal::none(),
            model: None,
        }
    }

    async fn tool_text(tool: &AgentTool, arguments: Value) -> Result<String> {
        let result = (tool.execute)(tool_context(arguments)).await?;
        Ok(result_text(&result))
    }

    fn registry_with_fake_server() -> McpRegistry {
        let registry = test_registry();
        registry.configure(vec![stdio_config("fake")]);
        registry
    }

    fn fake_server_config() -> McpServerConfig {
        let exe = fake_server_exe();
        McpServerConfig {
            name: "fake".to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: Some(exe.to_string_lossy().into_owned()),
            args: Some(vec![
                "mcp::tests::fake_mcp_server_process".to_owned(),
                "--nocapture".to_owned(),
            ]),
            url: None,
            env: Some(BTreeMap::from([(
                "PI_FAKE_MCP_SERVER".to_owned(),
                "1".to_owned(),
            )])),
            extra: Default::default(),
        }
    }

    #[tokio::test]
    async fn mcp_tool_lists_configured_servers_without_echoing_env() {
        let registry = McpRegistry::new();
        registry.configure(vec![McpServerConfig {
            name: "github".to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: Some("npx".to_owned()),
            args: Some(vec!["-y".to_owned(), "@modelcontextprotocol/server-github".to_owned()]),
            url: None,
            env: Some(BTreeMap::from([(
                "GITHUB_TOKEN".to_owned(),
                "super-secret-token".to_owned(),
            )])),
            extra: Default::default(),
        }]);
        let tool = mcp_tool(registry);
        let text = tool_text(&tool, json!({ "action": "list_servers" }))
            .await
            .expect("list_servers");
        assert!(text.contains("github"), "{text}");
        assert!(text.contains("stdio: npx -y @modelcontextprotocol/server-github"), "{text}");
        assert!(!text.contains("super-secret-token"), "env must never be echoed: {text}");
    }

    #[tokio::test]
    async fn initialize_failure_redacts_secrets_from_server_stderr() {
        // A fake server that logs credential-shaped text to stderr and then
        // refuses initialize: the tool error must embed the stderr tail but
        // never the secret values.
        let registry = test_registry();
        let mut config = fake_server_config();
        config
            .env
            .as_mut()
            .expect("fake config env")
            .insert("PI_FAKE_MCP_SERVER_BOOM".to_owned(), "1".to_owned());
        registry.configure(vec![config]);
        let tool = mcp_tool(registry);
        let err = tool_text(&tool, json!({ "action": "list_tools", "server": "fake" }))
            .await
            .expect_err("initialize must fail");
        let text = err.to_string();
        assert!(text.contains("failed to initialize"), "{text}");
        assert!(text.contains("--- server stderr ---"), "{text}");
        let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123456789"].concat();
        let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
        for secret in [ghp.as_str(), sk.as_str()] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(text.contains("[REDACTED]"), "redaction marker missing: {text}");
    }

    #[tokio::test]
    async fn mcp_tool_list_tools_against_fake_server() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry);
        let text = tool_text(
            &tool,
            json!({ "action": "list_tools", "server": "fake" }),
        )
        .await
        .expect("list_tools");
        assert!(text.contains("Tools from MCP server `fake` (3):"), "{text}");
        assert!(text.contains("- echo — Echo the given message back"), "{text}");
        assert!(text.contains("- add"), "{text}");
    }

    #[tokio::test]
    async fn mcp_tool_call_with_json_string_arguments() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry);
        let text = tool_text(
            &tool,
            json!({
                "action": "call",
                "server": "fake",
                "tool": "add",
                "args": r#"{"a": 20, "b": 22}"#
            }),
        )
        .await
        .expect("call");
        assert!(text.contains("tool `add`"), "{text}");
        assert!(text.contains("42"), "{text}");
    }

    #[tokio::test]
    async fn mcp_tool_missing_server_error() {
        let registry = McpRegistry::new();
        registry.configure(vec![stdio_config("alpha")]);
        let tool = mcp_tool(registry);
        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "beta", "tool": "echo" }),
        )
        .await
        .expect_err("unknown server must fail");
        assert!(err.to_string().contains("no MCP server configured with name `beta`"), "{err}");
        assert!(err.to_string().contains("alpha"), "{err}");
    }

    #[tokio::test]
    async fn mcp_tool_no_servers_error_lists_remedy() {
        let tool = mcp_tool(McpRegistry::new());
        let err = tool_text(
            &tool,
            json!({ "action": "list_tools", "server": "anything" }),
        )
        .await
        .expect_err("no servers must fail");
        assert!(err.to_string().contains("no MCP servers configured"), "{err}");
    }

    #[tokio::test]
    async fn mcp_tool_call_requires_tool_name() {
        let tool = mcp_tool(McpRegistry::new());
        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake" }),
        )
        .await
        .expect_err("missing tool must fail");
        assert!(err.to_string().contains("`tool`"), "{err}");
    }

    #[tokio::test]
    async fn mcp_tool_rejects_invalid_args_json() {
        let registry = McpRegistry::new();
        registry.configure(vec![stdio_config("fake")]);
        let tool = mcp_tool(registry);
        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "echo", "args": "not json" }),
        )
        .await
        .expect_err("invalid args JSON must fail");
        assert!(err.to_string().contains("args"), "{err}");
    }

    #[tokio::test]
    async fn mcp_tool_rejects_unknown_action() {
        let tool = mcp_tool(McpRegistry::new());
        let err = tool_text(&tool, json!({ "action": "fly" }))
            .await
            .expect_err("unknown action must fail");
        assert!(err.to_string().contains("unknown mcp action `fly`"), "{err}");
    }

    #[tokio::test]
    async fn mcp_tool_sse_server_reports_transport_limitation() {
        let registry = McpRegistry::new();
        registry.configure(vec![McpServerConfig {
            name: "remote".to_owned(),
            disabled: false,
            transport: McpTransport::Sse,
            command: None,
            args: None,
            url: Some("https://example.com/mcp".to_owned()),
            env: None,
            extra: Default::default(),
        }]);
        let tool = mcp_tool(registry);
        let text = tool_text(&tool, json!({ "action": "list_servers" }))
            .await
            .expect("list_servers");
        assert!(text.contains("sse: https://example.com/mcp"), "{text}");
        // Calling an sse server must fail fast with the transport limitation.
        let err = tool_text(
            &tool,
            json!({ "action": "list_tools", "server": "remote" }),
        )
        .await
        .expect_err("sse transport is not implemented");
        assert!(err.to_string().contains("sse"), "{err}");
    }

    #[tokio::test]
    async fn mcp_tool_large_output_is_bounded() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry);
        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "large" }),
        )
        .await
        .expect("call large");
        assert!(text.len() < OUTPUT_MAX_BYTES + 512, "bounded: {}", text.len());
        assert!(text.contains("[output truncated]"), "truncation marker");
    }

    #[tokio::test]
    async fn mcp_tool_abort_cancels_in_flight_call() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry);
        let (controller, abort) = pi_agent::AbortController::new();
        let context = pi_agent::ToolCallContext {
            tool_call_id: "1".to_owned(),
            arguments: json!({ "action": "call", "server": "fake", "tool": "echo" }),
            on_update: Arc::new(|_| {}),
            abort: abort.clone(),
            model: None,
        };
        controller.abort();
        let err = (tool.execute)(context)
            .await
            .expect_err("aborted call must fail");
        assert!(err.to_string().contains("aborted"), "{err}");
    }

    #[tokio::test]
    async fn registry_configure_drops_changed_sessions_and_keeps_unchanged() {
        let registry = McpRegistry::new();
        let alpha = stdio_config("alpha");
        let beta = stdio_config("beta");
        registry.configure(vec![alpha.clone(), beta.clone()]);
        // Force session slots for both servers.
        assert!(registry.session("alpha").is_some());
        assert!(registry.session("beta").is_some());
        assert_eq!(registry.live_session_count(), 0);

        // Reconfigure with alpha unchanged and beta changed: beta's slot must
        // be dropped, alpha's retained.
        let beta_changed = McpServerConfig {
            command: Some("different-binary".to_owned()),
            ..beta.clone()
        };
        registry.configure(vec![alpha.clone(), beta_changed]);
        assert!(registry.session("alpha").is_some(), "unchanged server keeps session");
        assert!(registry.session("beta").is_some(), "changed server gets a fresh slot");
        assert_eq!(registry.config("alpha"), Some(alpha.clone()));
        assert_eq!(
            registry.config("beta"),
            Some(McpServerConfig {
                command: Some("different-binary".to_owned()),
                ..beta
            })
        );
        // Removing a server removes its session slot entirely.
        registry.configure(vec![alpha]);
        assert!(registry.session("beta").is_none(), "removed server has no session");
    }

    #[tokio::test]
    async fn failed_call_drops_session_for_respawn() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry.clone());
        // Unknown tool without a cached list: the server answers with an
        // error, and the session must be dropped so the next call respawns.
        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "missing_tool" }),
        )
        .await
        .expect_err("unknown tool fails");
        assert!(err.to_string().contains("Unknown tool: missing_tool"), "{err}");
        assert_eq!(registry.live_session_count(), 0, "session dropped after error");
        // A subsequent healthy call respawns and succeeds.
        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "alive" } }),
        )
        .await
        .expect("respawned call");
        assert!(text.contains("alive"), "{text}");
        assert_eq!(registry.live_session_count(), 1, "session respawned");
    }

    #[tokio::test]
    async fn call_after_list_tools_validates_against_cached_names() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry);
        let _ = tool_text(&tool, json!({ "action": "list_tools", "server": "fake" }))
            .await
            .expect("list_tools");
        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "missing_tool" }),
        )
        .await
        .expect_err("cached list rejects unknown tool");
        assert!(err.to_string().contains("no tool `missing_tool`"), "{err}");
        assert!(err.to_string().contains("echo"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Disabled servers
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn disabled_server_is_excluded_and_never_spawns() {
        let registry = McpRegistry::new();
        let mut disabled = fake_server_config();
        disabled.name = "off".to_owned();
        disabled.disabled = true;
        registry.configure(vec![fake_server_config(), disabled]);

        // list_servers excludes the disabled entry entirely.
        let names = registry
            .list_servers()
            .iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["fake"], "disabled servers are filtered out");
        assert!(
            registry.session("off").is_none(),
            "disabled servers get no session slot"
        );
        assert_eq!(registry.live_session_count(), 0);

        // Calls against it are rejected before any spawn can happen.
        let tool = mcp_tool(registry.clone());
        let err = tool_text(
            &tool,
            json!({ "action": "list_tools", "server": "off" }),
        )
        .await
        .expect_err("disabled server must be rejected");
        assert!(
            err.to_string().contains("no MCP server configured with name `off`"),
            "{err}"
        );
        assert_eq!(
            registry.live_session_count(),
            0,
            "a disabled server must never spawn"
        );
    }

    #[tokio::test]
    async fn configure_drops_session_when_server_becomes_disabled() {
        let registry = test_registry();
        registry.configure(vec![fake_server_config()]);
        let tool = mcp_tool(registry.clone());
        let _ = tool_text(&tool, json!({ "action": "list_tools", "server": "fake" }))
            .await
            .expect("spawn");
        assert_eq!(registry.live_session_count(), 1);

        // Reconfigure with the same server disabled: the live session must be
        // torn down and the slot removed.
        let mut disabled = fake_server_config();
        disabled.disabled = true;
        registry.configure(vec![disabled]);
        assert_eq!(
            registry.live_session_count(),
            0,
            "session dropped when its server becomes disabled"
        );
        assert!(registry.session("fake").is_none());
    }

    // -----------------------------------------------------------------------
    // Session cutover reset (reset_live_sessions)
    // -----------------------------------------------------------------------

    /// Pids recorded by the fake server (one line per spawn).
    fn read_pid_lines(path: &std::path::Path) -> Vec<u32> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    /// True when a process with `pid` exists (Linux procfs).
    fn process_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// A registry configured with the fake server that appends its pid per
    /// spawn to `pid_file`, plus a ready `mcp` tool.
    fn registry_with_pid_tracking(pid_file: &std::path::Path) -> (McpRegistry, AgentTool) {
        let registry = test_registry();
        let mut config = fake_server_config();
        config
            .env
            .as_mut()
            .expect("fake env")
            .insert(
                "PI_FAKE_MCP_PID_FILE".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            );
        registry.configure(vec![config]);
        let tool = mcp_tool(registry.clone());
        (registry, tool)
    }

    #[tokio::test]
    async fn reset_live_sessions_reaps_client_keeps_config_and_respawns_fresh() {
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let pid_file = trace_dir.path().join("pids.txt");
        let (registry, tool) = registry_with_pid_tracking(&pid_file);

        // First call spawns a live client; record its process id.
        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "first" } }),
        )
        .await
        .expect("first call");
        assert!(text.contains("first"), "{text}");
        assert_eq!(registry.live_session_count(), 1);
        let pids = read_pid_lines(&pid_file);
        assert_eq!(pids.len(), 1, "exactly one spawn so far: {pids:?}");
        let old_pid = pids[0];
        assert!(process_alive(old_pid), "client must be running before the reset");

        // Reset: the client must be dead before the call returns, the slot
        // drained, and the server configuration preserved.
        registry.reset_live_sessions().await.expect("reset");
        assert!(
            !process_alive(old_pid),
            "old MCP client must be reaped before the reset returns"
        );
        assert_eq!(registry.live_session_count(), 0, "no live sessions after reset");
        assert!(
            registry.list_servers().iter().any(|server| server.name == "fake"),
            "server configuration must survive the reset"
        );

        // The next call lazily spawns a fresh server: a new process id, and
        // the configuration is still listed.
        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "second" } }),
        )
        .await
        .expect("second call");
        assert!(text.contains("second"), "{text}");
        let pids = read_pid_lines(&pid_file);
        assert_eq!(pids.len(), 2, "reset must force a fresh spawn: {pids:?}");
        assert_ne!(pids[1], old_pid, "fresh client must be a new process");
        assert!(process_alive(pids[1]), "fresh client must be running");
        assert_eq!(registry.live_session_count(), 1);
    }

    #[tokio::test]
    async fn reset_live_sessions_ignores_empty_slots_and_preserves_config() {
        let registry = test_registry();
        registry.configure(vec![stdio_config("fake")]);
        // A slot created (e.g. by a failed spawn or a touched session) with
        // no live client must be a silent no-op, not an error.
        assert!(registry.session("fake").is_some(), "slot created");
        assert_eq!(registry.live_session_count(), 0);
        registry
            .reset_live_sessions()
            .await
            .expect("reset with no live client must succeed");
        assert_eq!(registry.live_session_count(), 0);
        assert!(
            registry.list_servers().iter().any(|server| server.name == "fake"),
            "configuration must survive"
        );
    }

    #[tokio::test]
    async fn reset_live_sessions_fails_with_context_when_slot_is_in_flight() {
        let registry = test_registry();
        registry.configure(vec![stdio_config("fake")]);
        // Simulate a tool call in flight: hold the slot lock.
        let slot = registry.session("fake").expect("configured slot");
        let (held, release) = tokio::sync::oneshot::channel::<()>();
        let task = {
            let slot = slot.clone();
            tokio::spawn(async move {
                let _guard = slot.lock().await;
                let _ = held.send(());
                std::future::pending::<()>().await;
            })
        };
        let _ = release.await.expect("slot lock acquired");

        let error = registry
            .reset_live_sessions()
            .await
            .expect_err("an in-flight slot must fail the reset");
        let text = error.to_string();
        assert!(text.contains("fake"), "error must name the server: {text}");
        assert!(text.contains("in flight"), "error must explain the cause: {text}");

        // Unblock the holder so teardown drops cleanly.
        task.abort();
    }

    // -----------------------------------------------------------------------
    // Fast-start gate (spawn deferral)
    // -----------------------------------------------------------------------

    /// Reads the fake server's trace file (one line per lifecycle event).
    fn read_trace_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Runs the `mcp` action directly against the registry (bypasses the
    /// AgentTool wrapper so concurrent calls share one registry borrow).
    async fn run_mcp_text(registry: &McpRegistry, args: Value) -> Result<String> {
        let result = run_mcp(registry, args, AbortSignal::none()).await?;
        Ok(result_text(&result))
    }

    #[tokio::test]
    async fn fast_start_batches_two_concurrent_calls_into_one_spawn() {
        // Two calls to the same server issued back-to-back must share a single
        // spawn: the first waits the fast-start window holding the session
        // lock, the second queues behind it and reuses the spawned client.
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let registry = McpRegistry::with_spawn_defer(Duration::from_millis(300));
        let mut config = fake_server_config();
        config
            .env
            .as_mut()
            .expect("fake env")
            .insert(
                "PI_FAKE_MCP_TRACE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            );
        registry.configure(vec![config]);

        let started = std::time::Instant::now();
        let (first, second) = tokio::join!(
            run_mcp_text(
                &registry,
                json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "one" } }),
            ),
            run_mcp_text(
                &registry,
                json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "two" } }),
            ),
        );
        let elapsed = started.elapsed();
        let first = first.expect("first call succeeds");
        let second = second.expect("second call succeeds");
        assert!(first.contains("one") && second.contains("two"), "{first} / {second}");

        let lines = read_trace_lines(&trace_file);
        let spawns = lines.iter().filter(|line| line.as_str() == "spawn").count();
        assert_eq!(spawns, 1, "one spawn must serve both calls: {lines:?}");
        // The gate actively waited: a lone spawn would finish well under the
        // 300 ms window.
        assert!(
            elapsed >= Duration::from_millis(290),
            "first call must wait the fast-start gate: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn fast_start_window_is_skippable_with_zero_defer() {
        // A zero-window registry spawns immediately; used by the reconnect
        // tests and available to sessions that want no batching delay.
        let registry = McpRegistry::with_spawn_defer(Duration::ZERO);
        registry.configure(vec![fake_server_config()]);
        let started = std::time::Instant::now();
        let text = run_mcp_text(
            &registry,
            json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "fast" } }),
        )
        .await
        .expect("call");
        assert!(text.contains("fast"), "{text}");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "zero defer must not sleep: {:?}",
            started.elapsed()
        );
    }

    // -----------------------------------------------------------------------
    // Reconnects (bounded backoff)
    // -----------------------------------------------------------------------

    #[test]
    fn reconnect_backoff_is_capped_exponential() {
        // Attempt 1 is the fast-start window (the defer, never the backoff);
        // from attempt 2 on the base delay doubles each time, capped at 1 s.
        assert_eq!(reconnect_delay(2), Duration::from_millis(100));
        assert_eq!(reconnect_delay(3), Duration::from_millis(200));
        assert_eq!(reconnect_delay(4), Duration::from_millis(400));
        assert_eq!(reconnect_delay(5), Duration::from_millis(800));
        assert_eq!(reconnect_delay(6), Duration::from_millis(1000));
        assert_eq!(reconnect_delay(20), Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn reconnect_after_transport_failure_with_backoff() {
        // The first spawned process crashes on its first tools/call (marker
        // file arms the crash); the client must detect the transport failure,
        // back off, respawn, and complete the call on the second attempt.
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let crash_mark = trace_dir.path().join("crash.mark");
        let registry = McpRegistry::with_spawn_defer(Duration::ZERO);
        let mut config = fake_server_config();
        {
            let env = config.env.as_mut().expect("fake env");
            env.insert(
                "PI_FAKE_MCP_TRACE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            );
            env.insert(
                "PI_FAKE_MCP_CRASH_MARK".to_owned(),
                crash_mark.to_string_lossy().into_owned(),
            );
        }
        registry.configure(vec![config]);
        let tool = mcp_tool(registry);

        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "echo", "args": { "message": "back" } }),
        )
        .await
        .expect("call survives the transport failure");
        assert!(text.contains("back"), "{text}");

        let lines = read_trace_lines(&trace_file);
        let spawns = lines.iter().filter(|line| line.as_str() == "spawn").count();
        assert_eq!(
            spawns, 2,
            "exactly one respawn after the crash: {lines:?}"
        );
        let calls = lines
            .iter()
            .filter(|line| line.starts_with("call:echo"))
            .count();
        assert_eq!(calls, 2, "the call is re-issued against the respawned server: {lines:?}");
    }

    #[tokio::test]
    async fn initialize_refusal_is_a_protocol_error_and_is_not_retried() {
        // A server that answers initialize with a JSON-RPC error (the boom
        // mode) is a protocol refusal, not a transport failure: the client
        // must surface it after exactly one spawn instead of burning the
        // reconnect budget on a server that will refuse again.
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let registry = McpRegistry::with_spawn_defer(Duration::ZERO);
        let mut config = fake_server_config();
        {
            let env = config.env.as_mut().expect("fake env");
            env.insert("PI_FAKE_MCP_SERVER_BOOM".to_owned(), "1".to_owned());
            env.insert(
                "PI_FAKE_MCP_TRACE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            );
        }
        registry.configure(vec![config]);
        let tool = mcp_tool(registry.clone());

        let err = tool_text(
            &tool,
            json!({ "action": "list_tools", "server": "fake" }),
        )
        .await
        .expect_err("initialize refusal must fail");
        assert!(err.to_string().contains("failed to initialize"), "{err}");

        let lines = read_trace_lines(&trace_file);
        let spawns = lines.iter().filter(|line| line.as_str() == "spawn").count();
        assert_eq!(
            spawns, 1,
            "a JSON-RPC initialize refusal must not be retried: {lines:?}"
        );
        assert_eq!(
            registry.live_session_count(),
            0,
            "no session survives the refused initialize"
        );
    }

    #[tokio::test]
    async fn exhausted_reconnects_report_actionable_error() {
        // A persistently crashing server: after the bounded attempts the error
        // must name the server and how many attempts were made, and the
        // session must be dropped (so a later call tries again fresh).
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let registry = McpRegistry::with_spawn_defer(Duration::ZERO);
        let mut config = fake_server_config();
        {
            let env = config.env.as_mut().expect("fake env");
            env.insert(
                "PI_FAKE_MCP_TRACE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            );
            env.insert("PI_FAKE_MCP_CRASH_ALWAYS".to_owned(), "1".to_owned());
        }
        registry.configure(vec![config]);
        let tool = mcp_tool(registry.clone());

        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "echo" }),
        )
        .await
        .expect_err("persistent crash must fail after bounded attempts");
        let text = err.to_string();
        assert!(text.contains("failed after 3 attempts"), "{text}");
        assert!(text.contains("`fake`"), "server named in the error: {text}");
        assert_eq!(
            registry.live_session_count(),
            0,
            "session dropped after exhausting attempts"
        );
        let lines = read_trace_lines(&trace_file);
        let spawns = lines.iter().filter(|line| line.as_str() == "spawn").count();
        assert_eq!(spawns, 3, "exactly the bounded attempt count: {lines:?}");
    }

    // -----------------------------------------------------------------------
    // Progressive search_tool discovery
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn search_tool_discovery_uses_tools_search_without_full_list() {
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let registry = McpRegistry::with_spawn_defer(Duration::ZERO);
        let mut config = fake_server_config();
        {
            let env = config.env.as_mut().expect("fake env");
            env.insert(
                "PI_FAKE_MCP_TRACE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            );
            env.insert("PI_FAKE_MCP_SEARCH".to_owned(), "1".to_owned());
        }
        registry.configure(vec![config]);
        let tool = mcp_tool(registry);

        // A known tool resolves through tools/search alone.
        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "add", "args": { "a": 2, "b": 3 } }),
        )
        .await
        .expect("call via search");
        assert!(text.contains("5"), "{text}");
        let mut lines = read_trace_lines(&trace_file);
        assert!(
            lines.iter().any(|line| line == "search:add"),
            "progressive path must use tools/search: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "list"),
            "progressive path must not load the full tools/list: {lines:?}"
        );

        // An unknown tool is rejected from the search result, still without a
        // full list.
        let err = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "missing" }),
        )
        .await
        .expect_err("unknown tool must be rejected");
        assert!(err.to_string().contains("no tool `missing`"), "{err}");
        lines = read_trace_lines(&trace_file);
        assert!(
            lines.iter().any(|line| line == "search:missing"),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "list"),
            "still no full list: {lines:?}"
        );
    }

    #[tokio::test]
    async fn search_tool_gating_accepts_experimental_object_form() {
        // The extension can be advertised under `capabilities.experimental
        // .search_tool` as an object rather than a boolean under
        // `capabilities.tools.search_tool`; both shapes must enable the
        // progressive tools/search path.
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let mut client = spawn_fake_client_with(&[
            ("PI_FAKE_MCP_SEARCH_EXPERIMENTAL", "1"),
            ("PI_FAKE_MCP_TRACE", trace_file.to_str().expect("trace path")),
        ])
        .await;
        assert!(
            client.search_tool_supported(),
            "experimental object-form search_tool must be detected"
        );
        assert!(
            client.resolve_tool("echo").await.expect("tools/search resolves"),
            "echo must resolve through tools/search"
        );
        let lines = read_trace_lines(&trace_file);
        assert!(
            lines.iter().any(|line| line == "search:echo"),
            "the search probe must be used: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line == "list"),
            "no full tools/list on the progressive path: {lines:?}"
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn search_tool_falls_back_to_full_list_when_unsupported() {
        // The server advertises search_tool but answers tools/search with
        // method-not-found: the client must fall back to the full tools/list
        // and complete the call.
        let trace_dir = tempfile::tempdir().expect("trace dir");
        let trace_file = trace_dir.path().join("trace.txt");
        let registry = McpRegistry::with_spawn_defer(Duration::ZERO);
        let mut config = fake_server_config();
        {
            let env = config.env.as_mut().expect("fake env");
            env.insert(
                "PI_FAKE_MCP_TRACE".to_owned(),
                trace_file.to_string_lossy().into_owned(),
            );
            env.insert("PI_FAKE_MCP_SEARCH_FALLBACK".to_owned(), "1".to_owned());
        }
        registry.configure(vec![config]);
        let tool = mcp_tool(registry);

        let text = tool_text(
            &tool,
            json!({ "action": "call", "server": "fake", "tool": "add", "args": { "a": 4, "b": 5 } }),
        )
        .await
        .expect("call via full-list fallback");
        assert!(text.contains("9"), "{text}");
        let lines = read_trace_lines(&trace_file);
        assert!(
            lines.iter().any(|line| line == "search:add"),
            "the search probe is attempted first: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "list"),
            "then the full list fallback: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "call:add"),
            "and the call completes: {lines:?}"
        );
    }
}
