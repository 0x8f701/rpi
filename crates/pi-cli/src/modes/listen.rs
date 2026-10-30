use std::{
    collections::VecDeque,
    io,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{FutureExt, SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use pi_coding::{Application, TranscriptUnavailable};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use serde::Serialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    },
};

use super::collab_service::{CollabService, capability_from_protocols};
use crate::extension_ui::ExtensionUiAdapter;

use super::rpc::{
    MAX_CONCURRENT_COMMANDS, MAX_RPC_MESSAGE_BYTES, MAX_WS_MESSAGE_BYTES,
    MAX_WS_OUTBOUND_MESSAGE_BYTES, ResyncRequiredReason, RpcInput, RpcResponse, RuntimeIdentity,
    estimate_value_bytes, parse_input,
};
use super::session_runtime_manager::{
    RuntimeUnavailable, SessionRuntimeManager, SnapshotInProgress, StaleRuntime,
};
pub use super::session_runtime_manager::{
    MAX_CONCURRENT_SESSION_COMMANDS, MAX_LOADED_SESSIONS, SessionSpawner,
};

const MAX_HEADER_BYTES: usize = 32 * 1024;
const SESSION_DOWNLOAD_PREFIX: &str = "/download/session/";
const OUTBOUND_QUEUE_CAPACITY: usize = 64;
const OUTBOUND_QUEUE_BYTE_BUDGET: usize = MAX_WS_OUTBOUND_MESSAGE_BYTES;
/// Tungstenite `Frame::len` is payload plus header; the write buffer must exceed the payload budget.
const OUTBOUND_FRAME_HEADER_SLACK: usize = 16;
/// Strict cap on inbound frames drained around outbound waits: liveness
/// credit stays proportional to real reads and the pending queue is bounded.
const MAX_PENDING_INBOUND: usize = 16;
const SNAPSHOT_WINDOW_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const MAX_PENDING_SUBSCRIPTIONS_PER_CONNECTION: usize = MAX_LOADED_SESSIONS;
/// Subscribe identities are session/incarnation UUIDs; anything longer is
/// rejected at parse so pending keys and subscription state stay byte-bounded.
const MAX_SUBSCRIBE_IDENTITY_CHARS: usize = 128;
/// How long an outbound enqueue may wait for one slot in the bounded queue
/// before the client is declared a slow reader and closed with 1008
/// ("client is not reading messages"). A browser main-thread long task (e.g.
/// a Mermaid render) can stall reading for hundreds of milliseconds while
/// event bursts keep arriving; the grace absorbs that transient losslessly,
/// and only a sustained no-read (a genuinely slow client) is evicted.
const SLOW_CLIENT_GRACE: Duration = Duration::from_secs(5);
/// Per-connection ordinary command capacity. One additional spawned task is
/// reserved for safety recovery (notably code-review abort/close), while the
/// total task set remains hard-bounded even under bypass-command flooding.
const RESERVED_RECOVERY_COMMANDS: usize = 1;
const MAX_SPAWNED_COMMANDS: usize = MAX_CONCURRENT_COMMANDS + RESERVED_RECOVERY_COMMANDS;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time an accepted TLS connection may occupy a connection task
/// without completing its server-side handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WS_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Server-side ping cadence for every established WebSocket client. Browsers
/// answer protocol pings automatically; a healthy connection therefore
/// refreshes its per-client liveness state without application traffic.
const WS_PING_INTERVAL: Duration = Duration::from_secs(5);
/// Any WebSocket client that produces no inbound frame (including Pong)
/// within this window is evicted so a dead transport cannot retain a
/// connection-task slot and strand replacement reconnect attempts.
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CONTENT_TYPE_JSON: &str = "application/json";
const REMOTE_EXTENSION_UI_ERROR: &str = "remote interactive extension UI is disabled";

// Transport auth policy shared with the ACP WebSocket server (`ws_auth`);
// `MAX_CONNECTION_TASKS` stays publicly reachable through this module.
pub use super::ws_auth::MAX_CONNECTION_TASKS;
use super::ws_auth::{
    ListenAddressPolicy, authorized, authorized_cross_origin, constant_work_eq, load_auth_token,
    normalize_origin, read_token_file, websocket_subprotocol,
};

/// Bounds for authenticated WebSocket handshakes and idle collaboration
/// guests. Production uses [`DEFAULT_WS_TIMEOUTS`]; tests shrink the windows
/// so stalled-handshake and silent-guest capacity reclamation is provable
/// without waiting out the real durations.
#[derive(Clone, Copy)]
struct WebSocketTimeouts {
    /// Ceiling for an upgrade handshake once the request headers are in hand.
    /// A client that sends headers and then stalls must not pin the
    /// connection task — or, for collaboration, the participant slot its
    /// already-issued lease holds — forever.
    handshake: Duration,
    /// Cadence for server pings to established WebSocket clients.
    ping_interval: Duration,
    /// A WebSocket client sending no frame within this window is closed.
    idle: Duration,
    /// How long a /ws outbound enqueue waits for one slot in the bounded
    /// queue before the client is closed with 1008. Tests shrink the window
    /// so slow-client eviction is provable without waiting out the real
    /// duration.
    slow_client_grace: Duration,
}
#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SubscribeFrame {
    session_id: String,
    #[serde(default)]
    runtime_incarnation: Option<String>,
    #[serde(default)]
    cursor: u64,
}

fn parse_subscribe_frame(text: &str) -> Option<std::result::Result<SubscribeFrame, String>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    (value.get("type").and_then(serde_json::Value::as_str) == Some("subscribe")).then(|| {
        serde_json::from_value(value)
            .map_err(|error| format!("invalid subscribe frame: {error}"))
            .and_then(|frame: SubscribeFrame| {
                let oversized = frame.session_id.len() > MAX_SUBSCRIBE_IDENTITY_CHARS
                    || frame
                        .runtime_incarnation
                        .as_ref()
                        .is_some_and(|incarnation| incarnation.len() > MAX_SUBSCRIBE_IDENTITY_CHARS);
                if oversized {
                    Err(format!(
                        "subscribe identity exceeds {MAX_SUBSCRIBE_IDENTITY_CHARS} chars"
                    ))
                } else {
                    Ok(frame)
                }
            })
    })
}

struct SnapshotTaskResult {
    identity: RuntimeIdentity,
    generation: u64,
    snapshot: serde_json::Value,
}
type SnapshotTaskId = tokio::task::Id;
fn spawn_subscription_snapshot(
    snapshots: &mut JoinSet<SnapshotTaskResult>,
    active: &mut Option<ActiveSnapshot>,
    manager: Arc<SessionRuntimeManager>,
    identity: RuntimeIdentity,
    cursor: u64,
    generation: u64,
    cancel: CancellationToken,
    exact_incarnation: bool,
) {
    let active_identity = identity.clone();
    let active_cancel = cancel.clone();
    let handle = snapshots.spawn(async move {
        let attempt = async {
            manager
                .subscription_snapshot_for_identity(
                    identity.clone(),
                    cursor,
                    cancel,
                    exact_incarnation,
                )
                .await
        };
        let snapshot = match FutureExt::catch_unwind(std::panic::AssertUnwindSafe(attempt)).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                let mut response = serde_json::json!({
                    "type": "subscription_error",
                    "sessionId": identity.session_id,
                    "error": error.to_string(),
                });
                if !identity.runtime_incarnation.is_empty() {
                    response["runtimeIncarnation"] = json!(identity.runtime_incarnation);
                }
                if let Some(unavailable) = error.downcast_ref::<RuntimeUnavailable>() {
                    response["code"] = json!("runtime_unavailable");
                    response["cause"] = json!(unavailable.cause.wire());
                    response["retryable"] = json!(unavailable.cause.retryable());
                } else if error.downcast_ref::<TranscriptUnavailable>().is_some() {
                    response["code"] = json!("transcript_unavailable");
                } else if error.downcast_ref::<StaleRuntime>().is_some() {
                    response["code"] = json!("stale_runtime");
                } else if error.downcast_ref::<SnapshotInProgress>().is_some() {
                    response["code"] = json!("snapshot_in_progress");
                    response["retryable"] = json!(true);
                }
                response
            }
            Err(panic) => serde_json::json!({
                "type": "subscription_error",
                "sessionId": identity.session_id,
                "runtimeIncarnation": identity.runtime_incarnation,
                "error": format!("internal error: subscription snapshot task failed: {}", panic_message(&panic)),
            }),
        };
        SnapshotTaskResult {
            identity,
            generation,
            snapshot,
        }
    });
    *active = Some(ActiveSnapshot {
        identity: active_identity,
        generation,
        requested_cursor: cursor,
        cancel: active_cancel,
        task_id: Some(handle.id()),
    });
}
fn queue_pending_subscription(
    pending: &mut std::collections::HashMap<RuntimeIdentity, (u64, bool)>,
    identity: RuntimeIdentity,
    cursor: u64,
    exact_incarnation: bool,
) -> bool {
    if pending.len() >= MAX_PENDING_SUBSCRIPTIONS_PER_CONNECTION
        && !pending.contains_key(&identity)
    {
        return false;
    }
    pending
        .entry(identity)
        .and_modify(|queued| {
            queued.0 = queued.0.min(cursor);
            queued.1 |= exact_incarnation;
        })
        .or_insert((cursor, exact_incarnation));
    true
}
fn pending_subscription_limit_error(identity: &RuntimeIdentity) -> serde_json::Value {
    json!({
        "type": "subscription_error",
        "sessionId": identity.session_id,
        "runtimeIncarnation": identity.runtime_incarnation,
        "error": format!("too many pending subscriptions (limit {MAX_PENDING_SUBSCRIPTIONS_PER_CONNECTION})"),
    })
}
fn start_next_pending_snapshot(
    pending: &mut std::collections::HashMap<RuntimeIdentity, (u64, bool)>,
    snapshots: &mut JoinSet<SnapshotTaskResult>,
    active: &mut Option<ActiveSnapshot>,
    windows: &mut std::collections::HashMap<RuntimeIdentity, SnapshotWindow>,
    subscriptions: &mut ConnectionSubscriptions,
    generation: &mut u64,
    manager: Arc<SessionRuntimeManager>,
) {
    if active.is_some() {
        return;
    }
    let Some((identity, (cursor, exact_incarnation))) = pending
        .iter()
        .min_by_key(|(_, (cursor, _))| *cursor)
        .map(|(identity, request)| (identity.clone(), *request))
    else {
        return;
    };
    pending.remove(&identity);
    *generation = generation.saturating_add(1);
    let cancel = CancellationToken::new();
    subscriptions.states.insert(
        identity.clone(),
        SubscriptionState::Snapshotting {
            generation: *generation,
            requested_cursor: cursor,
        },
    );
    ConnectionSubscriptions::start_snapshot(windows, &identity, cursor);
    spawn_subscription_snapshot(
        snapshots,
        active,
        manager,
        identity,
        cursor,
        *generation,
        cancel,
        exact_incarnation,
    );
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&'static str>() {
        (*text).to_owned()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

struct ActiveSnapshot {
    identity: RuntimeIdentity,
    generation: u64,
    requested_cursor: u64,
    cancel: CancellationToken,
    task_id: Option<SnapshotTaskId>,
}
fn take_matching_active_snapshot(
    active: &mut Option<ActiveSnapshot>,
    completed: &SnapshotTaskResult,
) -> Option<ActiveSnapshot> {
    let current = active.as_ref()?;
    if current.identity != completed.identity || current.generation != completed.generation {
        return None;
    }
    active.take()
}
fn take_failed_active_snapshot(
    active: &mut Option<ActiveSnapshot>,
    failed_task: SnapshotTaskId,
) -> Option<ActiveSnapshot> {
    active
        .as_ref()
        .is_some_and(|snapshot| snapshot.task_id == Some(failed_task))
        .then(|| active.take())
        .flatten()
}

#[derive(Default)]
struct ConnectionSubscriptions {
    states: std::collections::HashMap<RuntimeIdentity, SubscriptionState>,
}

enum SubscriptionState {
    Ready { cursor: u64 },
    Snapshotting { generation: u64, requested_cursor: u64 },
    ResyncRequired { reason: ResyncRequiredReason, last_cursor: u64 },
}

struct OutboundMessage {
    message: Message,
    _bytes: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Clone)]
struct OutboundSender {
    messages: mpsc::Sender<OutboundMessage>,
    bytes: Arc<Semaphore>,
    /// Serializes at most one JSON frame at a time. The owner keeps this
    /// permit from before serialization until the encoded frame has acquired
    /// its byte reservation and transferred that reservation to the queue.
    encoder: Arc<Semaphore>,
}
fn outbound_message_bytes(message: &Message) -> usize {
    message.len()
}

fn outbound_write_timeout(bytes: usize, grace: Duration) -> Duration {
    let extra_secs = u64::try_from(bytes / (1024 * 1024)).unwrap_or(u64::MAX);
    grace.saturating_add(Duration::from_secs(extra_secs))
}

const SUBSCRIPTION_WINDOW_EVENT_CAPACITY: usize = 2048;

#[derive(Default)]
struct SnapshotWindow {
    cursor: u64,
    events: VecDeque<serde_json::Value>,
    bytes: usize,
    is_overflowed: bool,
}

#[cfg(test)]
#[derive(Default)]
struct SnapshotOverflowObserver {
    observed: Notify,
}

#[cfg(test)]
impl SnapshotOverflowObserver {
    fn observe(&self) {
        self.observed.notify_one();
    }

    async fn wait(&self) {
        self.observed.notified().await;
    }
}

#[derive(Debug)]
enum SubscriptionDecision {
    Forward,
    Skip,
    Gap { identity: RuntimeIdentity, cursor: u64 },
}

impl ConnectionSubscriptions {
    fn install(&mut self, identity: RuntimeIdentity, watermark: u64) {
        self.states
            .insert(identity, SubscriptionState::Ready { cursor: watermark });
    }

    fn mark_resync(&mut self, identity: RuntimeIdentity, reason: ResyncRequiredReason, last_cursor: u64) {
        self.states.insert(
            identity,
            SubscriptionState::ResyncRequired { reason, last_cursor },
        );
    }

    fn drop_identity(&mut self, identity: &RuntimeIdentity) {
        self.states.remove(identity);
    }

    fn identity_from_event(&self, value: &serde_json::Value) -> Option<RuntimeIdentity> {
        let session_id = value.get("sessionId").and_then(serde_json::Value::as_str)?;
        if let Some(incarnation) = value
            .get("runtimeIncarnation")
            .and_then(serde_json::Value::as_str)
        {
            return Some(RuntimeIdentity::new(session_id, incarnation));
        }
        let matches: Vec<_> = self
            .states
            .keys()
            .filter(|identity| identity.session_id == session_id)
            .cloned()
            .collect();
        (matches.len() == 1).then(|| matches.into_iter().next().unwrap())
    }

    fn decide(&mut self, value: &serde_json::Value) -> SubscriptionDecision {
        let Some(identity) = self.identity_from_event(value) else {
            if value.get("sequence").is_none() && self.states.is_empty() {
                return SubscriptionDecision::Forward;
            }
            return SubscriptionDecision::Skip;
        };
        match self.states.get_mut(&identity) {
            Some(SubscriptionState::Ready { cursor }) => {
                let Some(sequence) = value.get("sequence").and_then(serde_json::Value::as_u64) else {
                    return SubscriptionDecision::Forward;
                };
                if sequence <= *cursor {
                    return SubscriptionDecision::Skip;
                }
                if sequence != cursor.saturating_add(1) {
                    return SubscriptionDecision::Gap {
                        identity,
                        cursor: *cursor,
                    };
                }
                *cursor = sequence;
                SubscriptionDecision::Forward
            }
            Some(SubscriptionState::ResyncRequired { .. }) => SubscriptionDecision::Skip,
            Some(SubscriptionState::Snapshotting { .. }) | None => SubscriptionDecision::Skip,
        }
    }

    fn buffer_snapshot_event(
        windows: &mut std::collections::HashMap<RuntimeIdentity, SnapshotWindow>,
        value: &serde_json::Value,
        #[cfg(test)] overflow_observer: Option<&SnapshotOverflowObserver>,
    ) -> bool {
        let Some(session_id) = value.get("sessionId").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(window) = windows.iter_mut().find_map(|(identity, window)| {
            (identity.session_id == session_id
                && value
                    .get("runtimeIncarnation")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|incarnation| incarnation == identity.runtime_incarnation))
            .then_some(window)
        }) else {
            return false;
        };
        let Some(sequence) = value.get("sequence").and_then(serde_json::Value::as_u64) else {
            return false;
        };
        if sequence <= window.cursor {
            return true;
        }
        let event_bytes = estimate_value_bytes(value);
        if window.events.len() == SUBSCRIPTION_WINDOW_EVENT_CAPACITY
            || window.bytes.saturating_add(event_bytes) > SNAPSHOT_WINDOW_BYTE_BUDGET
            || event_bytes > SNAPSHOT_WINDOW_BYTE_BUDGET
        {
            if !window.is_overflowed {
                window.is_overflowed = true;
                #[cfg(test)]
                if let Some(observer) = overflow_observer {
                    observer.observe();
                }
            }
            return true;
        }
        window.bytes = window.bytes.saturating_add(event_bytes);
        window.events.push_back(value.clone());
        true
    }

    fn start_snapshot(
        windows: &mut std::collections::HashMap<RuntimeIdentity, SnapshotWindow>,
        identity: &RuntimeIdentity,
        _cursor: u64,
    ) {
        windows.insert(
            identity.clone(),
            SnapshotWindow {
                cursor: 0,
                events: VecDeque::new(),
                bytes: 0,
                is_overflowed: false,
            },
        );
    }

    fn contains(&self, session_id: Option<&str>, runtime_incarnation: Option<&str>) -> bool {
        if self.states.is_empty() {
            return true;
        }
        let Some(session_id) = session_id else {
            return false;
        };
        if let Some(runtime_incarnation) = runtime_incarnation {
            return self
                .states
                .contains_key(&RuntimeIdentity::new(session_id, runtime_incarnation));
        }
        self.states
            .keys()
            .filter(|identity| identity.session_id == session_id)
            .count()
            == 1
    }

    fn identities(&self) -> Vec<(RuntimeIdentity, u64)> {
        self.states
            .iter()
            .map(|(identity, state)| {
                let cursor = match state {
                    SubscriptionState::Ready { cursor }
                    | SubscriptionState::Snapshotting {
                        requested_cursor: cursor,
                        ..
                    }
                    | SubscriptionState::ResyncRequired {
                        last_cursor: cursor,
                        ..
                    } => *cursor,
                };
                (identity.clone(), cursor)
            })
            .collect()
    }

    fn cursor(&self, identity: &RuntimeIdentity) -> Option<u64> {
        match self.states.get(identity) {
            Some(SubscriptionState::Ready { cursor }) => Some(*cursor),
            Some(SubscriptionState::Snapshotting { requested_cursor, .. }) => Some(*requested_cursor),
            Some(SubscriptionState::ResyncRequired { last_cursor, .. }) => Some(*last_cursor),
            None => None,
        }
    }

    /// Remove the subscription for exactly `identity`. A stale unload for an
    /// old incarnation of the same session must never delete the current
    /// incarnation's subscription, so removal is keyed on the full
    /// [`RuntimeIdentity`] (session + incarnation), never on the session id
    /// alone.
    fn unload_identity(&mut self, identity: &RuntimeIdentity) {
        self.states.remove(identity);
    }
}

fn resync_required_frame(
    identity: &RuntimeIdentity,
    sequence: Option<u64>,
    reason: ResyncRequiredReason,
) -> serde_json::Value {
    let mut value = json!({
        "type": "session_resync_required",
        "sessionId": identity.session_id,
        "runtimeIncarnation": identity.runtime_incarnation,
        "reason": reason,
    });
    if let Some(sequence) = sequence {
        value["sequence"] = json!(sequence);
    }
    value
}

fn json_is_control_frame(value: &serde_json::Value) -> bool {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("session_unloaded" | "session_resync_required" | "subscription_error" | "pong") => true,
        Some("response") => value
            .get("data")
            .and_then(serde_json::Value::as_object)
            .is_none_or(|data| !data.contains_key("messages")),
        _ => false,
    }
}

const DEFAULT_WS_TIMEOUTS: WebSocketTimeouts = WebSocketTimeouts {
    handshake: READ_TIMEOUT,
    ping_interval: WS_PING_INTERVAL,
    idle: WS_IDLE_TIMEOUT,
    slow_client_grace: SLOW_CLIENT_GRACE,
};

/// Web client assets (vite build output) embedded into the binary by
/// build.rs from `crates/pi-cli/web/dist/`. The page carries no data itself:
/// every command and event flows through the `/rpc` and `/ws` routes, which
/// are token-gated when a token is configured and tokenless otherwise, so
/// the page is served without authentication and everything else keeps the
/// existing auth policy.
///
/// Development override: `RPI_WEB_DEV_DIR` points at a directory containing a
/// built `index.html` (e.g. `crates/pi-cli/web/dist`) served instead of the
/// embedded copy, so frontend iteration does not require rebuilding the
/// binary. `vite dev` is the primary dev loop and needs no override.
const RPI_WEB_DEV_DIR: &str = "RPI_WEB_DEV_DIR";

/// Cache file names for the auto-generated self-signed listener certificate,
/// stored under the standard rpi home directory (`~/.rpi/`, honoring
/// `RPI_HOME`) so the same certificate persists across restarts: a browser's
/// one-time acceptance of the self-signed certificate keeps working instead
/// of warning on every fresh pair.
const LISTEN_CERT_CACHE_NAME: &str = "listen-cert.pem";
const LISTEN_KEY_CACHE_NAME: &str = "listen-key.pem";
const MAX_TLS_PEM_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct ListenConfig {
    pub address: SocketAddr,
    pub token_file: Option<PathBuf>,
    /// Explicit insecure-remote opt-in (`--listen-allow-insecure-remote`).
    /// `start` gives this flag priority over the plaintext/TLS branch, so a
    /// non-loopback bind may omit a token regardless of transport (the one
    /// tokenless-remote escape hatch). If `token_file` is supplied it remains
    /// mandatory. Without the flag, non-loopback plaintext is refused and
    /// non-loopback TLS requires `token_file`.
    pub allow_insecure_remote: bool,
    /// Explicitly advertised HTTP(S) origin used to build collaboration
    /// links when `address` is a wildcard bind (0.0.0.0/::). Validated and
    /// normalized by `parse_advertised_origin` at CLI parse time; loopback
    /// and other specific binds synthesize from the bound address and never
    /// need this.
    pub advertised_origin: Option<String>,
    /// Serve plaintext HTTP/WebSocket instead of terminating TLS. When
    /// `false` (the default) the listener uses HTTPS: either the certificate
    /// pair in `tls_cert`/`tls_key` or an auto-generated self-signed
    /// certificate cached under the standard rpi home directory.
    pub plaintext: bool,
    /// TLS certificate file (PEM) for HTTPS, paired with `tls_key`.
    pub tls_cert: Option<PathBuf>,
    /// TLS private key file (PEM) for HTTPS, paired with `tls_cert`.
    pub tls_key: Option<PathBuf>,
    /// Factory that builds manager-owned session runtimes for the Web
    /// control plane (switch_session / new_session / fork / clone). `None`
    /// disables lifecycle opens with a clear error; tests inject a faux
    /// factory.
    pub session_factory: Option<std::sync::Arc<dyn SessionSpawner>>,
    /// Explicit `--listen-codename` override for this runner's D2 display
    /// identity. Resolved through `runner_identity::resolve` after bind;
    /// `None` reuses or generates the stored codename. The effective
    /// codename is forwarded to mDNS advertise and the id-less `get_state`
    /// overlay.
    pub runner_codename_override: Option<String>,
}

pub struct ListenHandle {
    address: SocketAddr,
    advertised_origin: Option<String>,
    /// Whether the listener terminates TLS (https) or serves plaintext
    /// (http). Drives the scheme of synthesized link URLs.
    tls: bool,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
    manager: std::sync::Arc<SessionRuntimeManager>,
    collab: CollabService,
    /// Keeps the mDNS daemon alive for the listener lifetime.
    _discovery: Option<crate::team::TeamDiscovery>,
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
    /// Effective advertised origin used to build collaboration links.
    ///
    /// Loopback and other specific binds synthesize the scheme-appropriate
    /// `https://<bound address>` (or `http://` with `--listen-plaintext`)
    /// automatically; a wildcard bind (0.0.0.0/::) yields `None` unless an
    /// explicit advertised origin was configured, so `/collab` fails closed
    /// instead of printing links synthesized from an unreachable wildcard.
    #[must_use]
    pub fn base_url(&self) -> Option<String> {
        advertised_base_url(self.address, self.advertised_origin.as_deref(), self.tls)
    }
    /// Directly-openable Web UI origin for the banner.
    ///
    /// Like [`base_url`](Self::base_url), an explicit advertised origin
    /// wins and a concrete bind synthesizes from the bound address; unlike
    /// it, a wildcard bind performs a best-effort [`discover_lan_ip`] so
    /// the banner can still offer a reachable URL. Returns `None` only
    /// when no usable LAN address can be discovered, in which case the
    /// caller prints a textual fallback. Collaboration link generation
    /// keeps using [`base_url`](Self::base_url), which stays fail-closed
    /// for wildcard binds — this method is for human-facing display only.
    #[must_use]
    pub fn display_web_url(&self) -> Option<String> {
        let discovered = discover_lan_ip(self.address);
        display_web_url(
            self.address,
            self.advertised_origin.as_deref(),
            self.tls,
            discovered,
        )
    }

    pub async fn stop(self, is_restart: bool) -> Result<()> {
        let shutdown_result = self
            .shutdown
            .send(true)
            .map_err(|_| anyhow!("control plane listener stopped before shutdown was signaled"));
        let task_result = match self.task.await {
            Ok(result) => result.context("running control plane listener"),
            Err(error) => Err(anyhow!(error).context("joining control plane listener")),
        };
        // Process-host listener shutdown aborts fan-in forwarding and hands
        // every manager-owned application to the manager's single lifecycle
        // authority. `lib.rs` owns only terminal Todo/primary cleanup.
        let manager_result = if is_restart {
            self.manager.shutdown_for_restart().await
        } else {
            self.manager.shutdown().await
        };
        match (shutdown_result, task_result, manager_result) {
            (Ok(()), Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(()), Ok(())) => Err(error),
            (Ok(()), Err(error), Ok(())) | (Ok(()), Ok(()), Err(error)) => Err(error),
            (Err(signal_error), Err(error), Ok(())) => Err(error.context(format!(
                "control plane shutdown signaling also failed: {signal_error:#}"
            ))),
            (_, Err(error), Err(manager_error)) => Err(error.context(format!(
                "session manager shutdown also failed: {manager_error:#}"
            ))),
            (Err(signal_error), Ok(()), Err(manager_error)) => Err(manager_error.context(format!(
                "control plane shutdown signaling also failed: {signal_error:#}"
            ))),
        }
    }
}

#[derive(Clone)]
struct ServerState {
    manager: std::sync::Arc<SessionRuntimeManager>,
    collab: CollabService,
    token: Option<Arc<[u8]>>,
    allow_insecure_remote: bool,
    /// D2 display codename resolved from the runner identity at listen
    /// start; `None` when identity resolution failed or no codename is
    /// configured. Exposed (unauthenticated) by `GET /status` so the Web
    /// client can label the runner before connecting.
    runner_codename: Option<String>,
    /// Resolved advertised origin used to build collaboration links and to
    /// default `collab_start` requests that omit an explicit `baseUrl`.
    /// `None` for wildcard binds without an explicit advertised origin:
    /// link generation fails closed. Browser auth does not consult this —
    /// tokenless browsers are accepted only when same-origin against the
    /// request's own `Host` ([`authorized`]).
    base_url: Option<String>,
    /// LAN mDNS candidate list from the background `_rpi._tcp` browser.
    /// `None` only when the mDNS daemon failed to start.
    discovery: Option<crate::team::DiscoveryView>,
    /// Fail-fast concurrency bound for video uploads: each in-flight
    /// preprocessing run holds up to 64 MiB of upload bytes plus an ffmpeg
    /// subprocess, so a burst of uploads must not stack unboundedly. A full
    /// semaphore answers new uploads with 429 before any body is read.
    video_upload_permits: std::sync::Arc<tokio::sync::Semaphore>,
    /// Fail-fast concurrency bound for PDF uploads: each in-flight
    /// extraction holds up to 32 MiB of upload bytes plus a pdftotext
    /// subprocess. A full semaphore answers new uploads with 429 before
    /// any body is read.
    pdf_upload_permits: std::sync::Arc<tokio::sync::Semaphore>,
    tmux_terminals: Arc<crate::modes::tmux::TmuxTerminalRegistry>,

    #[cfg(test)]
    snapshot_overflow_observer: Option<Arc<SnapshotOverflowObserver>>,
    #[cfg(test)]
    command_task_gate: Option<Arc<CommandTaskGate>>,
}

impl ServerState {
    fn browser_authorized(&self, headers: &http::HeaderMap) -> bool {
        authorized_cross_origin(
            headers,
            self.token.as_deref(),
            true,
            self.allow_insecure_remote,
        )
    }

    fn tokenless_origin_authorized(&self, headers: &http::HeaderMap) -> bool {
        authorized_cross_origin(headers, None, true, self.allow_insecure_remote)
    }
}
#[cfg(test)]
struct CommandTaskGate {
    started: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl Default for CommandTaskGate {
    fn default() -> Self {
        Self {
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

/// Maximum concurrent `POST /upload/video` preprocessing runs.
const MAX_CONCURRENT_VIDEO_UPLOADS: usize = 4;
/// Maximum concurrent `POST /upload/pdf` preprocessing runs.
const MAX_CONCURRENT_PDF_UPLOADS: usize = 4;

/// Resolve this listener's D2 display codename from the persisted runner
/// identity, honoring the [`ListenConfig`] override. Must run after bind
/// resolved `bound_port` (the default codename disambiguates co-hosted
/// listeners by port). Failures are non-fatal: the runner then advertises
/// and reports without a codename, exactly like pre-identity listeners.
fn resolve_runner_codename(
    agent_dir: &Path,
    override_codename: Option<&str>,
    bound_port: u16,
) -> Option<String> {
    match crate::runner_identity::resolve(agent_dir, override_codename, bound_port) {
        Ok(identity) => Some(identity.codename),
        Err(error) => {
            eprintln!("warning: runner identity resolve failed: {error:#}");
            None
        }
    }
}

pub async fn start(
    application: Application,
    extension_ui: ExtensionUiAdapter,
    config: ListenConfig,
) -> Result<ListenHandle> {
    let policy = if config.allow_insecure_remote {
        ListenAddressPolicy::AllowInsecureRemote
    } else if !config.plaintext {
        // TLS is the default transport; a non-loopback HTTPS listener is not
        // plaintext, so it needs no insecure-remote opt-in, but it still
        // requires a token file — enforced by `load_auth_token` under
        // `AllowTlsRemote` (a tokenless non-loopback TLS bind is rejected
        // pre-bind). `allow_insecure_remote` above takes priority and is the
        // one tokenless-remote escape hatch.
        ListenAddressPolicy::AllowTlsRemote
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
    // TLS is the default transport: `--listen-plaintext` opts out; an
    // explicit --listen-cert/--listen-key pair loads a real certificate;
    // otherwise a self-signed certificate is generated and cached. Build the
    // acceptor before allocating the session manager so certificate problems
    // fail fast as startup errors.
    let tls = if config.plaintext {
        None
    } else {
        Some(
            make_tls_acceptor(
                config.tls_cert.as_deref(),
                config.tls_key.as_deref(),
                address,
            )
            .await?,
        )
    };
    let tls_active = tls.is_some();
    let runner_codename = resolve_runner_codename(
        &pi_coding::agent_dir_path(),
        config.runner_codename_override.as_deref(),
        address.port(),
    );
    let manager = SessionRuntimeManager::new(
        application,
        extension_ui,
        config.session_factory,
        runner_codename.clone(),
    )
    .await
    .context("creating session runtime manager")?;
    let collab = CollabService::new(manager.clone());
    // Browse `_rpi._tcp` for the listen lifetime. Advertise only a
    // LAN-reachable host so a loopback bind never publishes loopback.
    let discovery_handle = match crate::team::TeamDiscovery::start() {
        Ok(discovery) => {
            if let Some(host) = mdns_advertise_host(address) {
                let scheme = if tls_active { "wss" } else { "ws" };
                if let Err(error) = discovery.advertise(
                    &host,
                    address.port(),
                    scheme,
                    runner_codename.as_deref(),
                ) {
                    eprintln!("LAN discovery advertise failed: {error:#}");
                }
            }
            Some(discovery)
        }
        Err(error) => {
            eprintln!("LAN discovery disabled: {error:#}");
            None
        }
    };
    let state = ServerState {
        manager: manager.clone(),
        collab: collab.clone(),
        token: token.map(Arc::from),
        allow_insecure_remote: config.allow_insecure_remote,
        runner_codename,
        base_url: advertised_base_url(address, config.advertised_origin.as_deref(), tls_active),
        discovery: discovery_handle.as_ref().map(|discovery| discovery.view()),
        video_upload_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(
            MAX_CONCURRENT_VIDEO_UPLOADS,
        )),
        pdf_upload_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(
            MAX_CONCURRENT_PDF_UPLOADS,
        )),
        tmux_terminals: crate::modes::tmux::TmuxTerminalRegistry::new(),

        #[cfg(test)]
        snapshot_overflow_observer: None,
        #[cfg(test)]
        command_task_gate: None,
    };
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(run_listener(listener, tls, state, shutdown_rx));
    Ok(ListenHandle {
        address,
        advertised_origin: config.advertised_origin,
        tls: tls_active,
        shutdown,
        task,
        manager,
        collab,
        _discovery: discovery_handle,
    })
}

/// Parse and normalize the `--listen-advertised-origin` value.
///
/// Accepts a strict HTTP(S) origin: an `http://` or `https://` scheme, a
/// non-empty host (optionally with a numeric port in 1..=65535), and nothing
/// else — credentials, non-root paths, query strings, and fragments are
/// rejected. A single trailing `/` (the root path) is allowed and normalized
/// away. Runs at CLI validation time so bad values fail before the listener
/// starts; the normalized value is what collaboration links are built from.
pub(crate) fn parse_advertised_origin(input: &str) -> Result<String> {
    let Some(rest) = input
        .strip_prefix("http://")
        .or_else(|| input.strip_prefix("https://"))
    else {
        bail!(
            "--listen-advertised-origin must be an http:// or https:// origin (no credentials, path, query, or fragment)"
        );
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if tail.starts_with('?') {
        bail!("--listen-advertised-origin must not contain a query string");
    }
    if tail.starts_with('#') {
        bail!("--listen-advertised-origin must not contain a fragment");
    }
    if !tail.is_empty() && tail != "/" {
        bail!("--listen-advertised-origin must not contain a path");
    }
    if authority.is_empty() {
        bail!("--listen-advertised-origin must include a host");
    }
    if authority.contains('@') {
        bail!("--listen-advertised-origin must not contain credentials");
    }
    if authority.chars().any(char::is_whitespace) {
        bail!("--listen-advertised-origin must not contain whitespace");
    }
    let (host, port, bracketed) = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| anyhow!("--listen-advertised-origin has an invalid IPv6 host"))?;
        let host = &authority[1..end];
        let after = &authority[end + 1..];
        let port = match after {
            "" => None,
            rest => Some(
                rest.strip_prefix(':')
                    .ok_or_else(|| anyhow!("--listen-advertised-origin has an invalid host"))?,
            ),
        };
        (host, port, true)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => (host, Some(port), false),
            _ => (authority, None, false),
        }
    };
    if host.is_empty()
        || host
            .bytes()
            .any(|byte| !byte.is_ascii_graphic() || (!bracketed && byte == b':'))
    {
        bail!("--listen-advertised-origin has an invalid host");
    }
    if bracketed && host.parse::<std::net::Ipv6Addr>().is_err() {
        bail!("--listen-advertised-origin has an invalid IPv6 host");
    }
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("--listen-advertised-origin has an invalid port");
        }
        let port_value: u16 = port
            .parse()
            .map_err(|_| anyhow!("--listen-advertised-origin has an invalid port"))?;
        if port_value == 0 {
            bail!("--listen-advertised-origin has an invalid port");
        }
    }
    Ok(input.trim_end_matches('/').to_owned())
}

/// Resolve the effective advertised origin used to build collaboration links.
///
/// An explicitly configured `--listen-advertised-origin` wins. Loopback and
/// other specific bind addresses synthesize `<scheme>://<bound address>`
/// automatically, using `https` when the listener terminates TLS and `http`
/// for `--listen-plaintext`. A wildcard bind (0.0.0.0/::) without an explicit
/// origin yields `None` so `/collab` and `collab_start` fail closed instead
/// of printing unreachable links.
fn advertised_base_url(bind: SocketAddr, advertised: Option<&str>, tls: bool) -> Option<String> {
    advertised.map(str::to_owned).or_else(|| {
        (!bind.ip().is_unspecified()).then(|| {
            let scheme = if tls { "https" } else { "http" };
            format!("{scheme}://{bind}")
        })
    })
}

/// Select a directly-openable Web UI origin for the banner.
///
/// Unlike [`advertised_base_url`] (which drives collaboration link
/// generation and stays fail-closed for wildcard binds), this helper
/// aims to always produce a *displayable* URL. Precedence:
/// 1. an explicit advertised origin wins, used verbatim;
/// 2. a concrete (non-wildcard) bind synthesizes `<scheme>://<bind>`;
/// 3. a wildcard bind uses the best-effort `discovered_lan_ip` when it is
///    a usable LAN address, formatted with the bound port (IPv6 gets
///    brackets via [`SocketAddr`] display).
///
/// Returns `None` only for a wildcard bind with no usable discovered IP,
/// in which case the caller prints a textual fallback instead of an
/// unreachable `0.0.0.0`/`::` URL. `discovered_lan_ip` is validated here
/// (not just in [`discover_lan_ip`]) so a stale or malformed injection
/// can never yield a bogus URL. The scheme follows `tls`: `https` for the
/// default TLS listener, `http` for `--listen-plaintext`.
fn display_web_url(
    bind: SocketAddr,
    advertised: Option<&str>,
    tls: bool,
    discovered_lan_ip: Option<IpAddr>,
) -> Option<String> {
    if let Some(origin) = advertised {
        return Some(origin.to_owned());
    }
    let scheme = if tls { "https" } else { "http" };
    let ip = bind.ip();
    if !ip.is_unspecified() {
        return Some(format!("{scheme}://{bind}"));
    }
    // Wildcard bind: fall back to a discovered LAN address, preserving the
    // bound port. SocketAddr::new + Display formats IPv6 with brackets.
    let lan = discovered_lan_ip.filter(|ip| is_usable_lan_ip(ip))?;
    let lan_addr = SocketAddr::new(lan, bind.port());
    Some(format!("{scheme}://{lan_addr}"))
}

/// Whether `ip` is a plausible LAN address to print in the banner.
///
/// Rejects the three classes that would produce a misleading or
/// unreachable URL: unspecified (0.0.0.0/::, the wildcard itself),
/// loopback (127.0.0.0/8, ::1), and multicast (224.0.0.0/4, ff00::/8).
/// Matches the contract exactly; link-local addresses are not excluded
/// because the contract does not require it.
fn is_usable_lan_ip(ip: &IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()
}

fn mdns_advertise_host(bind: SocketAddr) -> Option<String> {
    if is_usable_lan_ip(&bind.ip()) {
        return Some(bind.ip().to_string());
    }
    if !bind.ip().is_unspecified() {
        return None;
    }
    discover_lan_ip(bind)
        .filter(|ip| is_usable_lan_ip(ip))
        .map(|ip| ip.to_string())
}

/// Best-effort discovery of this machine's LAN IP, with no external data
/// transfer.
///
/// Binds a [`UdpSocket`] to the wildcard address and `connect`s it to a
/// documentation-reserved endpoint so the OS performs a route lookup and
/// stamps the socket's local address with the egress interface — without
/// sending any datagram (UDP `connect` only sets the default destination).
/// The chosen destination is in documentation space (RFC 5737 TEST-NET-3
/// for IPv4, RFC 3849 `2001:db8::/32` for IPv6) so even a leaked packet
/// would not reach a real host. The result is validated by
/// [`is_usable_lan_ip`]; unusable addresses yield `None`.
///
/// A wildcard IPv4 bind probes IPv4 only; a wildcard IPv6 bind probes
/// IPv6 first and falls back to an IPv4 probe (the bound port is
/// preserved by [`display_web_url`], not by the probe). `None` means no
/// route was found and the banner must print its textual fallback.
fn discover_lan_ip(bind: SocketAddr) -> Option<IpAddr> {
    match bind {
        SocketAddr::V4(_) => probe_lan_ip_v4(),
        SocketAddr::V6(_) => probe_lan_ip_v6().or_else(probe_lan_ip_v4),
    }
}

/// Probe the LAN IPv4 address by asking the OS for a route to TEST-NET-3.
fn probe_lan_ip_v4() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("203.0.113.123:9").ok()?;
    let local = socket.local_addr().ok()?;
    let ip = local.ip();
    is_usable_lan_ip(&ip).then_some(ip)
}

/// Probe the LAN IPv6 address by asking the OS for a route to the
/// documentation prefix `2001:db8::/32`.
fn probe_lan_ip_v6() -> Option<IpAddr> {
    let socket = UdpSocket::bind("[::]:0").ok()?;
    socket.connect("[2001:db8::123]:9").ok()?;
    let local = socket.local_addr().ok()?;
    let ip = local.ip();
    is_usable_lan_ip(&ip).then_some(ip)
}

/// Cache directory for the auto-generated self-signed certificate: the
/// standard rpi home directory (`~/.rpi/`, honoring `RPI_HOME`).
fn listen_cert_cache_dir() -> Result<PathBuf> {
    Ok(pi_coding::agent_dir_path())
}

/// Generate (once) and cache the self-signed listener certificate, returning
/// the cert/key PEM paths. A complete existing pair is reused so the
/// certificate stays stable across restarts — a browser's one-time
/// acceptance of the self-signed certificate keeps working. `bind` only
/// influences the generated certificate's subject alternative names.
async fn self_signed_cert_paths(bind: SocketAddr) -> Result<(PathBuf, PathBuf)> {
    let dir = listen_cert_cache_dir()?;
    let cert_path = dir.join(LISTEN_CERT_CACHE_NAME);
    let key_path = dir.join(LISTEN_KEY_CACHE_NAME);
    // Probe both cache files through exact OperatorAuth handles. Missing or
    // unreadable partial pairs are regenerated together; a display path is
    // never reopened as authority.
    if pi_coding::read_operator_auth_file(&cert_path, MAX_TLS_PEM_BYTES).is_ok()
        && pi_coding::read_operator_auth_file(&key_path, MAX_TLS_PEM_BYTES).is_ok()
    {
        return Ok((cert_path, key_path));
    }
    // Modern browsers require the requested host/IP to appear in the
    // subject alternative names before they even show the self-signed
    // interstitial, so the bound address (plus localhost) is included.
    let mut subject_alt_names = vec!["localhost".to_owned()];
    if !bind.ip().is_unspecified() {
        subject_alt_names.push(bind.ip().to_string());
    }
    let mut params = CertificateParams::new(subject_alt_names)
        .context("building self-signed certificate parameters")?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "rpi-listener");
    let signing_key = KeyPair::generate().context("generating self-signed key pair")?;
    let cert = params
        .self_signed(&signing_key)
        .context("generating self-signed certificate")?;
    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();
    pi_coding::replace_private_operator_auth_file(&cert_path, cert_pem.into_bytes())
        .with_context(|| format!("writing self-signed certificate to {}", cert_path.display()))?;
    pi_coding::replace_private_operator_auth_file(&key_path, key_pem.into_bytes())
        .with_context(|| format!("writing self-signed private key to {}", key_path.display()))?;
    Ok((cert_path, key_path))
}

/// Build the TLS acceptor for the control plane listener.
///
/// With an explicit `--listen-cert`/`--listen-key` pair (enforced at CLI
/// parse time) the PEM files are loaded as-is, e.g. a Let's Encrypt
/// fullchain and private key. Without them a self-signed certificate is
/// generated and cached under the standard rpi home directory so it remains
/// stable across restarts. The handshake and HTTP/WebSocket processing share
/// the same stream type, so TLS termination slots in before the request parser
/// without changing anything downstream.
async fn make_tls_acceptor(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    bind: SocketAddr,
) -> Result<TlsAcceptor> {
    // rustls 0.23 requires an explicit CryptoProvider; install ring (already
    // in the dependency tree via rcgen/reqwest) before building ServerConfig.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let (cert_path, key_path) = match (cert_path, key_path) {
        (Some(cert_path), Some(key_path)) => (cert_path.to_path_buf(), key_path.to_path_buf()),
        (None, None) => self_signed_cert_paths(bind).await?,
        _ => bail!("--listen-cert and --listen-key must be provided together"),
    };
    let cert_pem = pi_coding::read_operator_auth_file(&cert_path, MAX_TLS_PEM_BYTES)
        .with_context(|| format!("reading TLS certificate {}", cert_path.display()))?;
    let key_pem = pi_coding::read_operator_auth_file(&key_path, MAX_TLS_PEM_BYTES)
        .with_context(|| format!("reading TLS private key {}", key_path.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing TLS certificate {}", cert_path.display()))?;
    if certs.is_empty() {
        bail!("no certificates found in {}", cert_path.display());
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .with_context(|| format!("parsing TLS private key {}", key_path.display()))?
        .ok_or_else(|| anyhow!("no private key found in {}", key_path.display()))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS server configuration from certificate and key")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn run_listener(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
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
                let (tcp_stream, _) = accepted.context("accepting control plane connection")?;
                let tls = tls.clone();
                let state = state.clone();
                connections.spawn(async move {
                    if let Some(acceptor) = tls {
                        let Ok(Ok(tls_stream)) = tokio::time::timeout(
                            TLS_HANDSHAKE_TIMEOUT,
                            acceptor.accept(tcp_stream),
                        )
                        .await
                        else {
                            return;
                        };
                        let _ = handle_connection(tls_stream, state).await;
                    } else {
                        let _ = handle_connection(tcp_stream, state).await;
                    }
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
    // Close the listening socket while this task still runs, at loop exit —
    // before the (potentially slow) connection abort/drain and before the task
    // completes. `ListenHandle::stop` awaits this task, so a resolved stop is
    // a true barrier: the accept window ends here and no new TCP connection
    // can be established once the listener task has completed, independent of
    // JoinHandle/dealloc timing.
    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

/// Handle one accepted control-plane connection. `S` is the transport: a
/// plain `TcpStream` for `--listen-plaintext` or a
/// `tokio_rustls::server::TlsStream<TcpStream>` after the TLS handshake.
/// Both implement [`AsyncRead`] + [`AsyncWrite`] + `Unpin` + `Send`, so HTTP
/// parsing and the WebSocket upgrade run unchanged on either.
async fn handle_connection<S>(mut stream: S, state: ServerState) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let raw = match tokio::time::timeout(READ_TIMEOUT, read_http_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            write_plain_response(&mut stream, error.status, error.message).await?;
            return Ok(());
        }
        Err(_) => {
            write_plain_response(
                &mut stream,
                StatusCode::REQUEST_TIMEOUT,
                "request timed out",
            )
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
                    write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized")
                        .await?;
                    return Ok(());
                }
            };
            return collab_websocket_connection(
                stream,
                raw,
                connection,
                protocol,
                DEFAULT_WS_TIMEOUTS,
            )
            .await;
        }
        if raw.path != "/ws" {
            write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
            return Ok(());
        }
        let protocol = websocket_subprotocol(&raw.headers, state.token.as_deref());
        // Tokenless browsers are accepted only when the request is
        // same-origin: a single `http://` `Origin` whose authority equals
        // the request's `Host`. Tokened clients authenticate via bearer or
        // subprotocol regardless of Origin.
        if !state.browser_authorized(&raw.headers) && protocol.is_none() {
            write_plain_response(&mut stream, StatusCode::UNAUTHORIZED, "unauthorized").await?;
            return Ok(());
        }
        return websocket_connection(stream, raw, state, protocol, DEFAULT_WS_TIMEOUTS).await;
    }

    // The static web client page is served without authentication: it carries
    // no data itself, and every command/event flows through authenticated or
    // capability-gated WebSockets. A collaboration join link uses the WS path
    // as its browser document URL; non-upgrade GETs at that exact validated
    // route receive the same embedded client, which reads the secret fragment
    // locally before opening the encrypted WebSocket.
    if raw.method == Method::GET
        && (raw.path == "/web" || raw.path == "/chat" || collab_room_id(&raw.path).is_some())
    {
        let if_none_match = raw
            .headers
            .get(http::header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        return serve_web_page(&mut stream, if_none_match.as_deref()).await;
    }
    // Named web assets (e.g. hashed JS/CSS bundles) from the embedded table.
    if raw.method == Method::GET && raw.path.starts_with("/assets/") {
        let Some((mime, bytes)) = crate::web::get(&raw.path) else {
            write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
            return Ok(());
        };
        return write_response(&mut stream, StatusCode::OK, mime, bytes).await;
    }

    if raw.method == Method::GET && raw.path.starts_with(SESSION_DOWNLOAD_PREFIX) {
        return handle_session_download(&mut stream, raw, &state).await;
    }

    if raw.method == Method::OPTIONS && raw.path == "/upload/video" {
        return handle_video_upload_preflight(&mut stream, raw, &state).await;
    }
    if raw.method == Method::POST && raw.path == "/upload/video" {
        return handle_video_upload(&mut stream, raw, &state).await;
    }
    if raw.method == Method::OPTIONS && raw.path == "/upload/pdf" {
        return handle_pdf_upload_preflight(&mut stream, raw, &state).await;
    }
    if raw.method == Method::POST && raw.path == "/upload/pdf" {
        return handle_pdf_upload(&mut stream, raw, &state).await;
    }
    if raw.method == Method::OPTIONS && raw.path == "/upload/tmux-file" {
        return handle_tmux_file_preflight(&mut stream, raw, &state).await;
    }
    if raw.method == Method::POST && raw.path == "/upload/tmux-file" {
        return handle_tmux_file_upload(&mut stream, raw, &state).await;
    }


    // Unauthenticated liveness/identity probe for the Web client's
    // preflight: `tokenRequired` tells the client whether the listener is
    // token-gated, `version`/`codename` identify the runner. The response
    // carries no session, path, or token data. A validated browser Origin
    // is reflected (same policy as the video upload) so a cross-authority
    // Web UI can read the probe; OPTIONS is answered for the CORS preflight
    // a cross-origin fetch requires.
    if raw.method == Method::GET && raw.path == "/status" {
        return handle_status(&mut stream, raw, &state).await;
    }
    if raw.method == Method::OPTIONS && raw.path == "/status" {
        return handle_status_preflight(&mut stream, raw, &state).await;
    }

    if raw.method != Method::POST || raw.path != "/rpc" {
        write_plain_response(&mut stream, StatusCode::NOT_FOUND, "not found").await?;
        return Ok(());
    }
    if !state.browser_authorized(&raw.headers) {
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
        write_plain_response(
            &mut stream,
            StatusCode::LENGTH_REQUIRED,
            "content-length required",
        )
        .await?;
        return Ok(());
    };
    if length > MAX_RPC_MESSAGE_BYTES {
        write_plain_response(
            &mut stream,
            StatusCode::PAYLOAD_TOO_LARGE,
            "request too large",
        )
        .await?;
        return Ok(());
    }
    let body =
        match tokio::time::timeout(READ_TIMEOUT, read_body(&mut stream, raw.remainder, length))
            .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => {
                write_plain_response(&mut stream, error.status, error.message).await?;
                return Ok(());
            }
            Err(_) => {
                write_plain_response(
                    &mut stream,
                    StatusCode::REQUEST_TIMEOUT,
                    "request timed out",
                )
                .await?;
                return Ok(());
            }
        };
    let response = match parse_input(&body) {
        Ok(RpcInput::Command {
            command,
            session_id,
        }) => dispatch_http_command(&state, command, session_id, None).await,
        Ok(RpcInput::ExtensionUiResponse(_)) => {
            RpcResponse::failure(None, "extension_ui_response", REMOTE_EXTENSION_UI_ERROR)
        }
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

struct TmuxWs {
    owner: uuid::Uuid,
}

struct TmuxTerminalForwarder {
    terminal_id: String,
    owner: uuid::Uuid,
    events: mpsc::Receiver<crate::modes::tmux::TmuxTerminalEvent>,
    registry: Arc<crate::modes::tmux::TmuxTerminalRegistry>,
}

async fn run_tmux_terminal_forwarder(
    mut forwarder: TmuxTerminalForwarder,
    outbound: OutboundSender,
    slow_client_grace: Duration,
) -> Result<(), ()> {
    while let Some(event) = forwarder.events.recv().await {
        if enqueue_json(&outbound, &event, slow_client_grace)
            .await
            .is_err()
        {
            let _ = forwarder
                .registry
                .detach(forwarder.owner, &forwarder.terminal_id);
            return Err(());
        }
    }
    Ok(())
}

fn tag_requested_session(response: RpcResponse, session_id: Option<&str>) -> RpcResponse {
    match session_id {
        Some(session_id) => response.with_session_id(session_id),
        None => response,
    }
}

async fn dispatch_http_command(
    state: &ServerState,
    command: super::rpc::RpcCommand,
    session_id: Option<String>,
    tmux_ws: Option<TmuxWs>,
) -> RpcResponse {
    let id = command.id();
    let name = command.command_name();
    match command {
        super::rpc::RpcCommand::CollabStart { base_url, .. } => {
            let base_url = match base_url {
                Some(requested) => requested,
                None => match &state.base_url {
                    Some(advertised) => advertised.clone(),
                    None => {
                        return tag_requested_session(
                            RpcResponse::failure(
                                id,
                                name,
                                "collaboration link generation requires an advertised origin: pass --listen-advertised-origin <URL> for wildcard binds (or an explicit baseUrl in the request)"
                                    .to_owned(),
                            ),
                            session_id.as_deref(),
                        );
                    }
                },
            };
            match state.collab.start(session_id.as_deref(), &base_url).await {
                Ok(started) => {
                    let owner = started.session_id.clone();
                    RpcResponse::success(id, name, serde_json::to_value(started).ok())
                        .with_session_id(owner)
                }
                Err(error) => tag_requested_session(
                    RpcResponse::failure(id, name, error.to_string()),
                    session_id.as_deref(),
                ),
            }
        }
        super::rpc::RpcCommand::CollabStatus { room_id, .. } => tag_requested_session(
            RpcResponse::success(
                id,
                name,
                Some(serde_json::json!({
                    "rooms": state.collab.status(room_id.as_deref()).await,
                })),
            ),
            session_id.as_deref(),
        ),
        super::rpc::RpcCommand::CollabStop { room_id, .. } => {
            match state.collab.stop(&room_id).await {
                Ok(room) => {
                    let owner = room.session_id.clone();
                    RpcResponse::success(
                        id,
                        name,
                        Some(serde_json::json!({"stopped": true, "room": room})),
                    )
                    .with_session_id(owner)
                }
                Err(error) => tag_requested_session(
                    RpcResponse::failure(id, name, error.to_string()),
                    session_id.as_deref(),
                ),
            }
        }
        super::rpc::RpcCommand::GetDiscoveredRunners { .. } => {
            let runners = state.discovery.as_ref().map_or_else(Vec::new, |view| {
                view.snapshot()
                    .into_iter()
                    .map(|runner| {
                        serde_json::json!({
                            "origin": runner.http_origin(),
                            "host": runner.host,
                            "port": runner.port,
                            "label": runner.label,
                            "codename": runner.codename,
                            "version": runner.version,
                            "scheme": runner.scheme,
                        })
                    })
                    .collect::<Vec<_>>()
            });
            tag_requested_session(
                RpcResponse::success(id, name, Some(serde_json::json!({ "runners": runners }))),
                session_id.as_deref(),
            )
        }
        command @ (super::rpc::RpcCommand::TmuxListSessions { .. }
            | super::rpc::RpcCommand::TmuxPaneSnapshot { .. }
            | super::rpc::RpcCommand::TmuxSendKeys { .. }) => tag_requested_session(
            super::rpc::handle_tmux_command(command).await,
            session_id.as_deref(),
        ),
        command @ (super::rpc::RpcCommand::TmuxTerminalAttach { .. }
            | super::rpc::RpcCommand::TmuxTerminalInput { .. }
            | super::rpc::RpcCommand::TmuxTerminalResize { .. }
            | super::rpc::RpcCommand::TmuxTerminalDetach { .. }) => {
            let (response, _) =
                handle_tmux_terminal_command(state, command, tmux_ws).await;
            tag_requested_session(response, session_id.as_deref())
        }
        command => state.manager.dispatch(command, session_id).await,
    }
}

async fn handle_tmux_terminal_command(
    state: &ServerState,
    command: super::rpc::RpcCommand,
    tmux_ws: Option<TmuxWs>,
) -> (RpcResponse, Option<TmuxTerminalForwarder>) {
    let id = command.id();
    let name = command.command_name();
    let Some(tmux_ws) = tmux_ws else {
        return (
            RpcResponse::failure(
                id,
                name,
                "tmux terminal commands require a WebSocket connection",
            ),
            None,
        );
    };
    match command {
        super::rpc::RpcCommand::TmuxTerminalAttach {
            session, cols, rows, full_history, cached_history_key, ..
        } => {
            match state
                .tmux_terminals
                .attach(
                    tmux_ws.owner,
                    &session,
                    cols,
                    rows,
                    full_history,
                    cached_history_key.as_deref(),
                )
                .await
            {
                Ok((attached, events)) => match serde_json::to_value(&attached) {
                    Ok(data) => (
                        RpcResponse::success(id, name, Some(data)),
                        Some(TmuxTerminalForwarder {
                            terminal_id: attached.terminal_id,
                            owner: tmux_ws.owner,
                            events,
                            registry: state.tmux_terminals.clone(),
                        }),
                    ),
                    Err(error) => {
                        let _ = state
                            .tmux_terminals
                            .detach(tmux_ws.owner, &attached.terminal_id);
                        (
                            RpcResponse::failure(
                                id,
                                name,
                                format!("tmux terminal attach serialize failed: {error}"),
                            ),
                            None,
                        )
                    }
                },
                Err(error) => (RpcResponse::failure(id, name, error), None),
            }
        }
        super::rpc::RpcCommand::TmuxTerminalInput {
            terminal_id, data, ..
        } => match state
            .tmux_terminals
            .input(tmux_ws.owner, &terminal_id, &data)
            .await
        {
            Ok(()) => (RpcResponse::success(id, name, None), None),
            Err(error) => (RpcResponse::failure(id, name, error), None),
        },
        super::rpc::RpcCommand::TmuxTerminalResize {
            terminal_id,
            cols,
            rows,
            fit_window,
            ..
        } => match state
            .tmux_terminals
            .resize(tmux_ws.owner, &terminal_id, cols, rows, fit_window)
            .await
        {
            Ok(()) => (RpcResponse::success(id, name, None), None),
            Err(error) => (RpcResponse::failure(id, name, error), None),
        },
        super::rpc::RpcCommand::TmuxTerminalDetach { terminal_id, .. } => {
            match state.tmux_terminals.detach(tmux_ws.owner, &terminal_id) {
                Ok(()) => (RpcResponse::success(id, name, None), None),
                Err(error) => (RpcResponse::failure(id, name, error), None),
            }
        }
        _ => (
            RpcResponse::failure(id, name, "tmux terminal command routing mismatch"),
            None,
        ),
    }
}

fn is_tmux_terminal_lifecycle(command: &super::rpc::RpcCommand) -> bool {
    matches!(
        command,
        super::rpc::RpcCommand::TmuxTerminalInput { .. }
            | super::rpc::RpcCommand::TmuxTerminalResize { .. }
            | super::rpc::RpcCommand::TmuxTerminalDetach { .. }
    )
}

async fn admit_tmux_terminal_lifecycle(
    state: &ServerState,
    command: super::rpc::RpcCommand,
    session_id: Option<&str>,
    tmux_owner: uuid::Uuid,
    outbound: &OutboundSender,
    slow_client_grace: Duration,
) -> Result<(), ()> {
    let (response, forwarder) =
        handle_tmux_terminal_command(state, command, Some(TmuxWs { owner: tmux_owner })).await;
    let _ = forwarder;
    let response = tag_requested_session(response, session_id);
    enqueue_json(outbound, &response, slow_client_grace)
        .await
        .map_err(|_| ())
}

fn session_download_id(path: &str) -> Option<&str> {
    let download_id = path.strip_prefix(SESSION_DOWNLOAD_PREFIX)?;
    if download_id.is_empty()
        || download_id.contains('/')
        || uuid::Uuid::parse_str(download_id).is_err()
    {
        return None;
    }
    Some(download_id)
}

async fn handle_session_download<S>(
    stream: &mut S,
    raw: RawRequest,
    state: &ServerState,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let cors = state
        .tokenless_origin_authorized(&raw.headers)
        .then(|| {
            raw.headers
                .get(http::header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                .and_then(validated_cors_origin)
        })
        .flatten();
    let Some(download_id) = session_download_id(&raw.path) else {
        return write_plain_response_cors(
            stream,
            StatusCode::NOT_FOUND,
            "not found",
            cors.as_deref(),
        )
        .await;
    };
    let Some(download) = state.manager.consume_session_download(download_id).await else {
        return write_plain_response_cors(
            stream,
            StatusCode::NOT_FOUND,
            "not found",
            cors.as_deref(),
        )
        .await;
    };
    write_session_download_response(
        stream,
        download,
        cors.as_deref(),
        SESSION_RESPONSE_WRITE_TIMEOUT,
    )
    .await
}

/// Deadline for a session-download response write (header + body +
/// shutdown): a client that stops reading must not pin the connection task
/// while the selected-session HTML payload is written. Reuses the request
/// read bound, matching the video-upload response policy.
const SESSION_RESPONSE_WRITE_TIMEOUT: Duration = READ_TIMEOUT;

async fn write_session_download_response<S>(
    stream: &mut S,
    download: super::session_runtime_manager::StagedSessionDownload,
    allow_origin: Option<&str>,
    timeout: Duration,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    // Bound the *entire* response — HTTP header, body, and shutdown — so a
    // non-reading (or stalled) client cannot pin this connection task past
    // the deadline. One-shot consumption already happened in the caller, so
    // a timed-out write only loses the transport, never the download entry.
    match tokio::time::timeout(timeout, async {
        let mut header = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncontent-disposition: attachment; filename=\"{}\"\r\n",
            download.bytes.len(),
            download.file_name,
        );
        if let Some(origin) = allow_origin {
            header.push_str("access-control-allow-origin: ");
            header.push_str(origin);
            header.push_str("\r\nvary: origin\r\n");
        }
        header.push_str("cache-control: no-store\r\nconnection: close\r\n\r\n");
        stream.write_all(header.as_bytes()).await?;
        stream.write_all(&download.bytes).await?;
        stream.shutdown().await?;
        Ok(())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => bail!("timed out writing the session download response"),
    }
}

/// `X-Video-Name` header carrying the user-visible file name for
/// `POST /upload/video` (the request body is the raw video bytes).
const VIDEO_NAME_HEADER: &str = "x-video-name";

/// Byte cap for the percent-DECODED `X-Video-Name` value. The raw header is
/// already bounded by [`MAX_HEADER_BYTES`]; the decode cap keeps a hostile
/// expansion bounded before sanitization.
const MAX_VIDEO_NAME_DECODED_BYTES: usize = 1024;

/// Percent-decode an `encodeURIComponent`-style header value (the Web client
/// always sends the video name percent-encoded so raw Unicode never hits the
/// ByteString header limit). Decoded bytes are interpreted as UTF-8 (lossy);
/// invalid percent sequences are kept literal; output is capped at `cap`
/// bytes.
fn percent_decode_video_name(value: &str, cap: usize) -> String {
    let input = value.as_bytes();
    let mut bytes = Vec::with_capacity(input.len().min(cap));
    let mut index = 0;
    while index < input.len() && bytes.len() < cap {
        let byte = input[index];
        if byte == b'%' && index + 2 < input.len() {
            let hi = (input[index + 1] as char).to_digit(16);
            let lo = (input[index + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                bytes.push((hi * 16 + lo) as u8);
                index += 3;
                continue;
            }
        }
        bytes.push(byte);
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Authenticated, bounded video upload endpoint for the Web client.
///
/// The request body is the raw video; the `X-Video-Name` header carries the
/// user-visible file name. The response is a bounded JSON attachment —
/// `attachmentId`, sanitized name/container/duration, the extracted
/// chronological JPEG frames, and a ready-made instruction string — that the
/// Web client renders as a video-attachment marker and feeds into
/// `prompt`/`steer` through the existing image `ContentBlock` path. Raw
/// video bytes never ride the prompt WebSocket JSON, and nothing is stored
/// server-side: the frames exist only in this response and the temporary
/// work directory is removed before the handler returns.
///
/// Auth mirrors `/rpc` exactly ([`authorized`] with the same tokenless
/// same-origin browser policy). Failures map to bounded JSON
/// `{"error": "..."}` bodies; the ffmpeg-missing case is an actionable 503.
async fn handle_video_upload<S>(stream: &mut S, raw: RawRequest, state: &ServerState) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The actual upload is gated by the normal bearer/same-origin auth; a
    // refused request gets no CORS headers (the browser blocks reading it).
    if !state.browser_authorized(&raw.headers) {
        return write_video_plain_response_bounded(
            stream,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            None,
            VIDEO_RESPONSE_WRITE_TIMEOUT,
        )
        .await;
    }
    // CORS: reflect a validated browser Origin on every post-auth response
    // so a cross-authority Web UI (e.g. loaded from 127.0.0.1, talking to
    // the LAN address) can read the result. Malformed/null/file origins are
    // never reflected; native clients without an Origin get no CORS headers.
    let cors = raw
        .headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(validated_cors_origin);
    let cors = cors.as_deref();
    let write_timeout = VIDEO_RESPONSE_WRITE_TIMEOUT;
    let Some(name_value) = raw.headers.get(VIDEO_NAME_HEADER) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "missing X-Video-Name header"}),
            cors,
            write_timeout,
        )
        .await;
    };
    // The Web client sends the name `encodeURIComponent`-encoded (raw
    // Unicode would exceed the ByteString header limit); decode it bounded
    // before sanitizing. Invalid percent sequences stay literal.
    let raw_name = percent_decode_video_name(
        &String::from_utf8_lossy(name_value.as_bytes()),
        MAX_VIDEO_NAME_DECODED_BYTES,
    );
    // Reject an unsupported name before reading a single body byte. The
    // message is generic: the raw name may carry a client-side local path
    // and must never be echoed.
    if crate::video_extract::sanitize_video_name(&raw_name).is_none() {
        return write_video_json_response_bounded(
            stream,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &serde_json::json!({"error": format!(
                "unsupported video file — supported containers: mkv, mp4, webm, mov, avi, ogg"
            )}),
            cors,
            write_timeout,
        )
        .await;
    }
    let Some(length) = content_length(&raw.headers) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::LENGTH_REQUIRED,
            &serde_json::json!({"error": "content-length is required"}),
            cors,
            write_timeout,
        )
        .await;
    };
    if length > crate::video_extract::MAX_VIDEO_UPLOAD_BYTES {
        return write_video_json_response_bounded(
            stream,
            StatusCode::PAYLOAD_TOO_LARGE,
            &serde_json::json!({"error": format!(
                "upload exceeds the {} MiB video limit",
                crate::video_extract::MAX_VIDEO_UPLOAD_BYTES / 1024 / 1024
            )}),
            cors,
            write_timeout,
        )
        .await;
    }
    // Fail-fast concurrency bound: each in-flight upload holds up to 64 MiB
    // plus an ffmpeg subprocess, so a burst must not stack. The permit is
    // held for the whole handler and released on return.
    let _permit = match state.video_upload_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return write_video_json_response_bounded(
                stream,
                StatusCode::TOO_MANY_REQUESTS,
                &serde_json::json!({"error": "too many concurrent video uploads — try again shortly"}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let body =
        match tokio::time::timeout(READ_TIMEOUT, read_body(stream, raw.remainder, length)).await {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => {
                write_video_plain_response_bounded(
                    stream,
                    error.status,
                    error.message,
                    cors,
                    write_timeout,
                )
                .await?;
                return Ok(());
            }
            Err(_) => {
                write_video_plain_response_bounded(
                    stream,
                    StatusCode::REQUEST_TIMEOUT,
                    "request timed out",
                    cors,
                    write_timeout,
                )
                .await?;
                return Ok(());
            }
        };
    if let Err(error) = crate::video_extract::reject_container_mismatch(&raw_name, &body) {
        return write_video_json_response_bounded(
            stream,
            error.status(),
            &serde_json::json!({"error": error.message}),
            cors,
            write_timeout,
        )
        .await;
    }
    let process = state.manager.governed_process_context();
    let program = match crate::video_extract::governed_ffmpeg_program(&process) {
        Ok(program) => program,
        Err(error) => {
            let error = crate::video_extract::ffmpeg_program_error(&error);
            return write_video_json_response_bounded(
                stream,
                error.status(),
                &serde_json::json!({"error": error.message}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let limits = crate::video_extract::VideoLimits::default();
    let result = tokio::task::spawn_blocking(move || {
        crate::video_extract::extract_video(&process, &program, limits, body, &raw_name)
    })
    .await
    .context("video preprocessing task panicked")?;
    match result {
        Ok(video) => {
            let response = serde_json::json!({
                "attachmentId": uuid::Uuid::new_v4().to_string(),
                "name": video.name,
                "container": video.container,
                "mimeType": video.mime_type,
                "sizeBytes": video.size_bytes,
                "durationSeconds": video.duration_seconds,
                "frameCount": video.frames.len(),
                "framesBase64Bytes": video.frames_base64_bytes(),
                "frames": video.frames,
                "instruction": video.instruction,
            });
            write_video_json_response_bounded(
                stream,
                StatusCode::OK,
                &response,
                cors,
                write_timeout,
            )
            .await
        }
        Err(error) => {
            write_video_json_response_bounded(
                stream,
                error.status(),
                &serde_json::json!({"error": error.message}),
                cors,
                write_timeout,
            )
            .await
        }
    }
}

/// Headers the upload preflight allows the actual request to carry.
const VIDEO_PREFLIGHT_ALLOWED_HEADERS: &[&str] = &["authorization", "x-video-name", "content-type"];

/// CORS preflight for `POST /upload/video`.
///
/// Browsers do NOT attach the bearer token to the preflight — they only
/// DECLARE it in `Access-Control-Request-Headers` — so the preflight is
/// authorized by origin policy alone: the `Origin` must be a valid
/// `http(s)://host[:port]` (no credentials, path, query, fragment, null,
/// file, or wildcard), the requested method must be `POST`, and every
/// requested header must be on the allowlist. The validated origin is then
/// reflected; the actual POST is still gated by the normal bearer /
/// same-origin auth.
async fn handle_video_upload_preflight<S>(
    stream: &mut S,
    raw: RawRequest,
    state: &ServerState,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let Some(origin_value) = raw.headers.get(http::header::ORIGIN) else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "origin required").await;
    };
    let Ok(origin_text) = origin_value.to_str() else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    let Some(origin) = validated_cors_origin(origin_text) else {
        // Never reflect a malformed/null/file/wildcard origin.
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    // Preflights cannot carry the bearer token (browsers only DECLARE it in
    // Access-Control-Request-Headers), so the gate is the same policy as
    // `authorized()` minus the token: with a token configured any valid
    // origin passes (the real POST is bearer-gated); tokenless listeners
    // reflect only the same-origin browser (Origin authority == Host).
    if state.token.is_none() && !state.tokenless_origin_authorized(&raw.headers) {
        return write_plain_response(stream, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }
    if !matches!(
        raw.headers
            .get("access-control-request-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    ) {
        return write_plain_response(
            stream,
            StatusCode::BAD_REQUEST,
            "access-control-request-method must be POST",
        )
        .await;
    }
    // Every header the actual request intends to send must be allowlisted.
    if let Some(requested) = raw.headers.get("access-control-request-headers") {
        let requested = requested.to_str().unwrap_or("");
        let requested = requested
            .split(',')
            .map(str::trim)
            .filter(|header| !header.is_empty())
            .collect::<Vec<_>>();
        if requested
            .iter()
            .any(|header| !VIDEO_PREFLIGHT_ALLOWED_HEADERS.contains(header))
        {
            return write_plain_response(
                stream,
                StatusCode::BAD_REQUEST,
                "requested headers are not allowed",
            )
            .await;
        }
    }
    let header = format!(
        "HTTP/1.1 204 No Content\r\naccess-control-allow-origin: {origin}\r\n\
         access-control-allow-methods: POST, OPTIONS\r\n\
         access-control-allow-headers: authorization, x-video-name, content-type\r\n\
         access-control-max-age: 600\r\n\
         vary: origin, access-control-request-method, access-control-request-headers\r\n\
         connection: close\r\n\r\n"
    );
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// `X-Pdf-Name` header carrying the user-visible file name for
/// `POST /upload/pdf` (the request body is the raw PDF bytes).
const PDF_NAME_HEADER: &str = "x-pdf-name";

/// Headers the PDF upload preflight allows the actual request to carry.
const PDF_PREFLIGHT_ALLOWED_HEADERS: &[&str] = &["authorization", "x-pdf-name", "content-type"];

/// Authenticated, bounded PDF upload endpoint for the Web client.
///
/// The request body is the raw PDF; the `X-Pdf-Name` header carries the
/// user-visible file name. The response is a bounded JSON attachment —
/// `attachmentId`, sanitized name, extracted UTF-8 text, and a ready-made
/// instruction string — that the Web client prepends to the prompt
/// `message`. Raw PDF bytes never ride the prompt WebSocket JSON, and
/// nothing is stored server-side: the text exists only in this response
/// and the temporary work directory is removed before the handler returns.
///
/// Auth mirrors `/upload/video` exactly. Failures map to bounded JSON
/// `{"error": "..."}` bodies; the pdftotext-missing case is an actionable
/// 503.
async fn handle_pdf_upload<S>(stream: &mut S, raw: RawRequest, state: &ServerState) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !state.browser_authorized(&raw.headers) {
        return write_video_plain_response_bounded(
            stream,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            None,
            VIDEO_RESPONSE_WRITE_TIMEOUT,
        )
        .await;
    }
    let cors = raw
        .headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(validated_cors_origin);
    let cors = cors.as_deref();
    let write_timeout = VIDEO_RESPONSE_WRITE_TIMEOUT;
    let Some(name_value) = raw.headers.get(PDF_NAME_HEADER) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "missing X-Pdf-Name header"}),
            cors,
            write_timeout,
        )
        .await;
    };
    let raw_name = percent_decode_video_name(
        &String::from_utf8_lossy(name_value.as_bytes()),
        MAX_VIDEO_NAME_DECODED_BYTES,
    );
    if crate::pdf_extract::sanitize_pdf_name(&raw_name).is_none() {
        return write_video_json_response_bounded(
            stream,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &serde_json::json!({"error": "unsupported PDF file — attach a .pdf document"}),
            cors,
            write_timeout,
        )
        .await;
    }
    let Some(length) = content_length(&raw.headers) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::LENGTH_REQUIRED,
            &serde_json::json!({"error": "content-length is required"}),
            cors,
            write_timeout,
        )
        .await;
    };
    if length > crate::pdf_extract::MAX_PDF_UPLOAD_BYTES {
        return write_video_json_response_bounded(
            stream,
            StatusCode::PAYLOAD_TOO_LARGE,
            &serde_json::json!({"error": format!(
                "upload exceeds the {} MiB PDF limit",
                crate::pdf_extract::MAX_PDF_UPLOAD_BYTES / 1024 / 1024
            )}),
            cors,
            write_timeout,
        )
        .await;
    }
    let _permit = match state.pdf_upload_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return write_video_json_response_bounded(
                stream,
                StatusCode::TOO_MANY_REQUESTS,
                &serde_json::json!({"error": "too many concurrent PDF uploads — try again shortly"}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let body =
        match tokio::time::timeout(READ_TIMEOUT, read_body(stream, raw.remainder, length)).await {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => {
                write_video_plain_response_bounded(
                    stream,
                    error.status,
                    error.message,
                    cors,
                    write_timeout,
                )
                .await?;
                return Ok(());
            }
            Err(_) => {
                write_video_plain_response_bounded(
                    stream,
                    StatusCode::REQUEST_TIMEOUT,
                    "request timed out",
                    cors,
                    write_timeout,
                )
                .await?;
                return Ok(());
            }
        };
    let process = state.manager.governed_process_context();
    let program = match crate::pdf_extract::governed_pdftotext_program(&process) {
        Ok(program) => program,
        Err(error) => {
            let error = crate::pdf_extract::pdftotext_program_error(&error);
            return write_video_json_response_bounded(
                stream,
                error.status(),
                &serde_json::json!({"error": error.message}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let limits = crate::pdf_extract::PdfLimits::default();
    let result = tokio::task::spawn_blocking(move || {
        crate::pdf_extract::extract_pdf(&process, &program, limits, body, &raw_name)
    })
    .await
    .context("PDF preprocessing task panicked")?;
    match result {
        Ok(pdf) => {
            let response = serde_json::json!({
                "attachmentId": uuid::Uuid::new_v4().to_string(),
                "name": pdf.name,
                "mimeType": "application/pdf",
                "sizeBytes": pdf.size_bytes,
                "textBytes": pdf.text.len(),
                "text": pdf.text,
                "instruction": pdf.instruction,
            });
            write_video_json_response_bounded(
                stream,
                StatusCode::OK,
                &response,
                cors,
                write_timeout,
            )
            .await
        }
        Err(error) => {
            write_video_json_response_bounded(
                stream,
                error.status(),
                &serde_json::json!({"error": error.message}),
                cors,
                write_timeout,
            )
            .await
        }
    }
}

/// CORS preflight for `POST /upload/pdf`. Same origin policy as the video
/// upload preflight; the actual POST is still bearer / same-origin gated.
async fn handle_pdf_upload_preflight<S>(
    stream: &mut S,
    raw: RawRequest,
    state: &ServerState,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let Some(origin_value) = raw.headers.get(http::header::ORIGIN) else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "origin required").await;
    };
    let Ok(origin_text) = origin_value.to_str() else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    let Some(origin) = validated_cors_origin(origin_text) else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    if state.token.is_none() && !state.tokenless_origin_authorized(&raw.headers) {
        return write_plain_response(stream, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }
    if !matches!(
        raw.headers
            .get("access-control-request-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    ) {
        return write_plain_response(
            stream,
            StatusCode::BAD_REQUEST,
            "access-control-request-method must be POST",
        )
        .await;
    }
    if let Some(requested) = raw.headers.get("access-control-request-headers") {
        let requested = requested.to_str().unwrap_or("");
        let requested = requested
            .split(',')
            .map(str::trim)
            .filter(|header| !header.is_empty())
            .collect::<Vec<_>>();
        if requested
            .iter()
            .any(|header| !PDF_PREFLIGHT_ALLOWED_HEADERS.contains(header))
        {
            return write_plain_response(
                stream,
                StatusCode::BAD_REQUEST,
                "requested headers are not allowed",
            )
            .await;
        }
    }
    let header = format!(
        "HTTP/1.1 204 No Content\r\naccess-control-allow-origin: {origin}\r\n\
         access-control-allow-methods: POST, OPTIONS\r\n\
         access-control-allow-headers: authorization, x-pdf-name, content-type\r\n\
         access-control-max-age: 600\r\n\
         vary: origin, access-control-request-method, access-control-request-headers\r\n\
         connection: close\r\n\r\n"
    );
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

const TMUX_FILE_NAME_HEADER: &str = "x-file-name";
const TMUX_SESSION_HEADER: &str = "x-tmux-session";
const TMUX_FILE_PREFLIGHT_ALLOWED_HEADERS: &[&str] =
    &["authorization", "x-file-name", "x-tmux-session", "content-type"];

async fn handle_tmux_file_upload<S>(stream: &mut S, raw: RawRequest, state: &ServerState) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !state.browser_authorized(&raw.headers) {
        return write_video_plain_response_bounded(
            stream,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            None,
            VIDEO_RESPONSE_WRITE_TIMEOUT,
        )
        .await;
    }
    let cors = raw
        .headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(validated_cors_origin);
    let cors = cors.as_deref();
    let write_timeout = VIDEO_RESPONSE_WRITE_TIMEOUT;
    let Some(name_value) = raw.headers.get(TMUX_FILE_NAME_HEADER) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "missing X-File-Name header"}),
            cors,
            write_timeout,
        )
        .await;
    };
    let Some(session_value) = raw.headers.get(TMUX_SESSION_HEADER) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "missing X-Tmux-Session header"}),
            cors,
            write_timeout,
        )
        .await;
    };
    let raw_name = percent_decode_video_name(
        &String::from_utf8_lossy(name_value.as_bytes()),
        MAX_VIDEO_NAME_DECODED_BYTES,
    );
    let session_raw = percent_decode_video_name(
        &String::from_utf8_lossy(session_value.as_bytes()),
        MAX_VIDEO_NAME_DECODED_BYTES,
    );
    let session = match super::tmux::validate_session_name(&session_raw) {
        Ok(session) => session.to_owned(),
        Err(error) => {
            return write_video_json_response_bounded(
                stream,
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": error}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let Some(length) = content_length(&raw.headers) else {
        return write_video_json_response_bounded(
            stream,
            StatusCode::LENGTH_REQUIRED,
            &serde_json::json!({"error": "content-length is required"}),
            cors,
            write_timeout,
        )
        .await;
    };
    if length > crate::tmux_file::MAX_TMUX_FILE_BYTES {
        return write_video_json_response_bounded(
            stream,
            StatusCode::PAYLOAD_TOO_LARGE,
            &serde_json::json!({"error": crate::tmux_file::TmuxFileError::TooLarge.message()}),
            cors,
            write_timeout,
        )
        .await;
    }
    let body =
        match tokio::time::timeout(READ_TIMEOUT, read_body(stream, raw.remainder, length)).await {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => {
                write_video_plain_response_bounded(
                    stream,
                    error.status,
                    error.message,
                    cors,
                    write_timeout,
                )
                .await?;
                return Ok(());
            }
            Err(_) => {
                write_video_plain_response_bounded(
                    stream,
                    StatusCode::REQUEST_TIMEOUT,
                    "request timed out",
                    cors,
                    write_timeout,
                )
                .await?;
                return Ok(());
            }
        };
    let accepted = match crate::tmux_file::accept_tmux_file(&raw_name, &body) {
        Ok(accepted) => accepted,
        Err(error) => {
            return write_video_json_response_bounded(
                stream,
                error.status(),
                &serde_json::json!({"error": error.message()}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let path = match crate::tmux_file::write_tmux_file(&accepted, &body) {
        Ok(path) => path,
        Err(error) => {
            return write_video_json_response_bounded(
                stream,
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({"error": error}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    let path_text = match path.to_str() {
        Some(text) => text.to_owned(),
        None => {
            let error = match cleanup_tmux_upload_file(path) {
                Ok(()) => "tmux file path is not UTF-8".to_owned(),
                Err(cleanup) => format!("tmux file path is not UTF-8; {cleanup}"),
            };
            return write_video_json_response_bounded(
                stream,
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({"error": error}),
                cors,
                write_timeout,
            )
            .await;
        }
    };
    match super::tmux::paste_text(&session, &path_text) {
        Ok(()) => {
            crate::tmux_file::schedule_tmux_file_delete(
                path,
                crate::tmux_file::TMUX_FILE_DELETE_AFTER,
            );
            write_video_json_response_bounded(
                stream,
                StatusCode::OK,
                &serde_json::json!({"path": path_text}),
                cors,
                write_timeout,
            )
            .await
        }
        Err(error) => {
            let error = match cleanup_tmux_upload_file(path) {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; {cleanup}"),
            };
            write_video_json_response_bounded(
                stream,
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({"error": error}),
                cors,
                write_timeout,
            )
            .await
        }
    }
}

fn cleanup_tmux_upload_file(path: std::path::PathBuf) -> Result<(), String> {
    match crate::tmux_file::remove_tmux_file_or_retry(path) {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("tmux-file cleanup failed: {error}");
            Err(error)
        }
    }
}

async fn handle_tmux_file_preflight<S>(
    stream: &mut S,
    raw: RawRequest,
    state: &ServerState,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let Some(origin_value) = raw.headers.get(http::header::ORIGIN) else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "origin required").await;
    };
    let Ok(origin_text) = origin_value.to_str() else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    let Some(origin) = validated_cors_origin(origin_text) else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    if state.token.is_none() && !state.tokenless_origin_authorized(&raw.headers) {
        return write_plain_response(stream, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }
    if !matches!(
        raw.headers
            .get("access-control-request-method")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    ) {
        return write_plain_response(
            stream,
            StatusCode::BAD_REQUEST,
            "access-control-request-method must be POST",
        )
        .await;
    }
    if let Some(requested) = raw.headers.get("access-control-request-headers") {
        let requested = requested.to_str().unwrap_or("");
        let requested = requested
            .split(',')
            .map(str::trim)
            .filter(|header| !header.is_empty())
            .collect::<Vec<_>>();
        if requested
            .iter()
            .any(|header| !TMUX_FILE_PREFLIGHT_ALLOWED_HEADERS.contains(header))
        {
            return write_plain_response(
                stream,
                StatusCode::BAD_REQUEST,
                "requested headers are not allowed",
            )
            .await;
        }
    }
    let header = format!(
        "HTTP/1.1 204 No Content\r\naccess-control-allow-origin: {origin}\r\n\
         access-control-allow-methods: POST, OPTIONS\r\n\
         access-control-allow-headers: authorization, x-file-name, x-tmux-session, content-type\r\n\
         access-control-max-age: 600\r\n\
         vary: origin, access-control-request-method, access-control-request-headers\r\n\
         connection: close\r\n\r\n"
    );
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}


/// Unauthenticated `GET /status` probe for the Web client's runner
/// preflight. The response is `{ tokenRequired, version, codename }` and
/// nothing else: no session, path, or token data ever leaves the listener
/// through this route. `tokenRequired` is whether a token file is
/// configured; `version` is the compile-time package version; `codename` is
/// the resolved runner identity codename, or `null` when identity
/// resolution failed or none is configured. A validated browser `Origin`
/// (same policy as the video upload: strict `http(s)://host[:port]`, never
/// `null`/`file:`/wildcard) is reflected so a cross-authority Web UI can
/// read the probe; native clients without an `Origin` get no CORS headers.
async fn handle_status<S>(stream: &mut S, raw: RawRequest, state: &ServerState) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let cors = raw
        .headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(validated_cors_origin);
    let body = serde_json::json!({
        "tokenRequired": state.token.is_some(),
        "version": env!("CARGO_PKG_VERSION"),
        "codename": state.runner_codename,
    });
    write_json_response_cors(stream, StatusCode::OK, &body, cors.as_deref()).await
}

/// CORS preflight for `GET /status`, so a cross-origin Web client can fetch
/// the probe. Browsers do not attach the bearer token to preflights, and
/// the probe itself is unauthenticated, so the gate is origin policy alone:
/// the `Origin` must be a valid `http(s)://host[:port]` (no credentials,
/// path, query, fragment, null, file, or wildcard) and the requested method
/// must be `GET`. The validated origin is reflected; no wildcard is ever
/// emitted.
async fn handle_status_preflight<S>(stream: &mut S, raw: RawRequest, state: &ServerState) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let Some(origin_value) = raw.headers.get(http::header::ORIGIN) else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "origin required").await;
    };
    let Ok(origin_text) = origin_value.to_str() else {
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    let Some(origin) = validated_cors_origin(origin_text) else {
        // Never reflect a malformed/null/file/wildcard origin.
        return write_plain_response(stream, StatusCode::BAD_REQUEST, "malformed origin").await;
    };
    if !matches!(
        raw.headers
            .get("access-control-request-method")
            .and_then(|value| value.to_str().ok()),
        Some("GET")
    ) {
        return write_plain_response(
            stream,
            StatusCode::BAD_REQUEST,
            "access-control-request-method must be GET",
        )
        .await;
    }
    let header = format!(
        "HTTP/1.1 204 No Content\r\naccess-control-allow-origin: {origin}\r\n\
         access-control-allow-methods: GET, OPTIONS\r\n\
         access-control-max-age: 600\r\n\
         vary: origin, access-control-request-method\r\n\
         connection: close\r\n\r\n"
    );
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Validate a CORS `Origin` for reflection: exactly `http(s)://host[:port]`
/// with a non-empty host, no credentials/path/query/fragment/whitespace, and
/// a numeric port when one is present. Returns the canonical lowercased
/// form. `null` (opaque/sandboxed), `file:`, and wildcard origins never
/// pass — there is nothing safe to reflect.
fn validated_cors_origin(value: &str) -> Option<String> {
    let origin = normalize_origin(value)?;
    let authority = origin.split_once("://")?.1;
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[::1]` or `[::1]:8765`.
        let (host, after) = rest.split_once(']')?;
        match after {
            "" => (host, None),
            port if port.starts_with(':') => (host, Some(&port[1..])),
            _ => return None,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
    }
    Some(origin)
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

async fn read_http_request<S>(stream: &mut S) -> std::result::Result<RawRequest, RequestError>
where
    S: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(1024);
    let end = loop {
        let mut chunk = [0_u8; 1024];
        let count = stream.read(&mut chunk).await.map_err(|_| RequestError {
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
        let name =
            http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| RequestError {
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

async fn read_body<S>(
    stream: &mut S,
    mut body: Vec<u8>,
    length: usize,
) -> std::result::Result<Vec<u8>, RequestError>
where
    S: AsyncRead + Unpin,
{
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

struct PrefixedStream<S> {
    prefix: Vec<u8>,
    offset: usize,
    stream: S,
}

impl<S> AsyncRead for PrefixedStream<S>
where
    S: AsyncRead + Unpin,
{
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

impl<S> AsyncWrite for PrefixedStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
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
    /// Exit still needs the writer to send a close frame we produce here.
    Graceful(Option<CloseFrame>),
    /// Exit observed the peer's own close frame: tungstenite auto-queued the
    /// echo reply, so the writer must flush it (see [`WriterControl`]).
    PeerInitiatedClose,
    SlowClient,
}

enum WriterControl {
    /// Server-initiated close: the writer still owns the close frame and
    /// sends it itself.
    Close(Option<CloseFrame>),
    /// Peer-initiated close: the read half received `Message::Close`, so
    /// tungstenite already switched to `ClosedByPeer` and auto-queued the
    /// echoed close reply in the shared context. Sending another Close is a
    /// no-op (`write` rejects with `SendAfterClosing` before reaching the
    /// close path), so the writer must drain the sink instead: `flush` has
    /// no active-state gate and transmits exactly the queued echo reply,
    /// completing the RFC 6455 close handshake.
    FlushPeerClose,
}

// Subscribe before accepting the WebSocket upgrade. Once the client observes
// a successful handshake, every subsequent application/UI event must have an
// active receiver rather than racing the server's post-handshake setup.
async fn collab_websocket_connection<S>(
    stream: S,
    raw: RawRequest,
    mut connection: super::collab_service::CollabConnection,
    protocol: String,
    timeouts: WebSocketTimeouts,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let path = raw.path.clone();
    let mut prefix = raw.raw_headers;
    prefix.extend_from_slice(&raw.remainder);
    let config = WebSocketConfig::default()
        .max_message_size(Some(pi_coding::collab::MAX_FRAME_BYTES))
        .max_frame_size(Some(pi_coding::collab::MAX_FRAME_BYTES))
        .max_write_buffer_size(2 * pi_coding::collab::MAX_FRAME_BYTES);
    // Bound the upgrade itself. The lease for this guest's participant slot
    // was issued during authentication, before the handshake, so a client
    // that sends request headers and then stalls must not hold the slot (or
    // the connection task) forever: the timeout cancels the accept and drops
    // the connection, releasing the lease with it.
    let websocket = match tokio::time::timeout(
        timeouts.handshake,
        accept_hdr_async_with_config(
            PrefixedStream {
                prefix,
                offset: 0,
                stream,
            },
            move |request: &Request,
                  mut response: Response|
                  -> std::result::Result<Response, ErrorResponse> {
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
        ),
    )
    .await
    {
        Ok(result) => result.context("upgrading collaboration WebSocket")?,
        Err(_) => bail!("collaboration WebSocket handshake timed out"),
    };

    let (mut write, mut read) = websocket.split();
    // Every direct write on this connection shares the slow-client grace as
    // its write-progress bound (see bounded_ws_send).
    let write_timeout = timeouts.slow_client_grace;
    if *connection.stopped.borrow() {
        let _ = bounded_ws_send(
            &mut write,
            Message::Close(Some(collab_stopped_close_frame())),
            write_timeout,
            "sending collaboration stopped close",
        )
        .await;
        return Ok(());
    }
    let hello =
        serde_json::to_string(&connection.hello()).context("serializing collaboration hello")?;
    bounded_ws_send(
        &mut write,
        Message::Text(hello.into()),
        write_timeout,
        "sending collaboration hello",
    )
    .await?;
    if *connection.stopped.borrow() {
        let _ = bounded_ws_send(
            &mut write,
            Message::Close(Some(collab_stopped_close_frame())),
            write_timeout,
            "sending collaboration stopped close",
        )
        .await;
        return Ok(());
    }
    bounded_ws_send(
        &mut write,
        Message::Binary(connection.snapshot_frame()?.into()),
        write_timeout,
        "sending collaboration snapshot",
    )
    .await?;

    // Idle watchdog: the server pings the guest on a fixed cadence; browsers
    // answer automatically with pongs at the protocol level (RFC 6455
    // 5.5.2), so every live guest keeps producing incoming frames. A guest
    // that sends nothing — not even a pong — within `idle` is closed
    // cleanly and its participant slot released instead of being pinned
    // indefinitely.
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + timeouts.ping_interval,
        timeouts.ping_interval,
    );
    // A prompt dispatch can outlast several intervals; catch up with a single
    // ping on the next aligned tick instead of bursting a backlog.
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle = tokio::time::sleep(timeouts.idle);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            biased;
            changed = connection.stopped.changed() => {
                if changed.is_err() || *connection.stopped.borrow() {
                    let _ = bounded_ws_send(
                        &mut write,
                        Message::Close(Some(collab_stopped_close_frame())),
                        write_timeout,
                        "sending collaboration stopped close",
                    )
                    .await;
                    return Ok(());
                }
            }
            event = connection.events.recv() => match event {
                Ok(event) if connection.event_matches_room(&event) => {
                    let frame = connection.event_frame(&event)?;
                    bounded_ws_send(
                        &mut write,
                        Message::Binary(frame.into()),
                        write_timeout,
                        "sending collaboration event",
                    )
                    .await?;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = bounded_ws_send(
                        &mut write,
                        Message::Close(Some(CloseFrame {
                            code: CloseCode::Policy,
                            reason: "collaboration event stream lagged".into(),
                        })),
                        write_timeout,
                        "sending collaboration lag close",
                    )
                    .await;
                    return Ok(());
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = ping.tick() => {
                if bounded_ws_send(
                    &mut write,
                    Message::Ping(Vec::new().into()),
                    write_timeout,
                    "sending collaboration ping",
                )
                .await
                .is_err()
                {
                    return Ok(());
                }
            }
            _ = &mut idle => {
                let _ = bounded_ws_send(
                    &mut write,
                    Message::Close(Some(CloseFrame {
                        code: CloseCode::Policy,
                        reason: "collaboration connection idle".into(),
                    })),
                    write_timeout,
                    "sending collaboration idle close",
                )
                .await;
                return Ok(());
            }
            incoming = read.next() => match incoming {
                Some(Ok(frame)) => {
                    match frame {
                        Message::Binary(frame) => {
                            let pending = match connection.prepare_client_frame(&frame) {
                                Ok(pending) => pending,
                                Err(_) => {
                                    let _ = bounded_ws_send(
                                        &mut write,
                                        Message::Close(Some(CloseFrame {
                                            code: CloseCode::Policy,
                                            reason: "invalid collaboration frame".into(),
                                        })),
                                        write_timeout,
                                        "sending collaboration invalid-frame close",
                                    )
                                    .await;
                                    return Ok(());
                                }
                            };
                            let response = pending.execute().await;
                            let frame = connection.response_frame(response)?;
                            bounded_ws_send(
                                &mut write,
                                Message::Binary(frame.into()),
                                write_timeout,
                                "sending collaboration response",
                            )
                            .await?;
                        }
                        Message::Ping(payload) => {
                            bounded_ws_send(
                                &mut write,
                                Message::Pong(payload),
                                write_timeout,
                                "sending collaboration pong",
                            )
                            .await?;
                        }
                        // Pongs are protocol-level replies to our pings (and
                        // the browser's automatic pong), never application
                        // data; a healthy guest may send them at any time.
                        Message::Pong(_) => {}
                        Message::Close(_) => return Ok(()),
                        Message::Text(_) | Message::Frame(_) => {
                            let _ = bounded_ws_send(
                                &mut write,
                                Message::Close(Some(CloseFrame {
                                    code: CloseCode::Unsupported,
                                    reason: "encrypted binary messages required".into(),
                                })),
                                write_timeout,
                                "sending collaboration unsupported close",
                            )
                            .await;
                            return Ok(());
                        }
                    }
                    // Any inbound frame (a command, a ping, or the browser's
                    // automatic pong) proves the guest is alive; restart the
                    // idle window only after it is handled, so a long-running
                    // prompt dispatch never counts against a guest that is
                    // legitimately awaiting the response.
                    idle.as_mut().reset(tokio::time::Instant::now() + timeouts.idle);
                }
                None => return Ok(()),
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                    let _ = bounded_ws_send(
                        &mut write,
                        Message::Close(Some(CloseFrame {
                            code: CloseCode::Size,
                            reason: "message too large".into(),
                        })),
                        write_timeout,
                        "sending collaboration size close",
                    )
                    .await;
                    return Ok(());
                }
                Some(Err(_)) => return Ok(()),
            }
        }
    }
}

/// The 1008 close frame for a client that stopped reading — used by the
/// enqueue-slow-client exit and by the writer's send-progress timeout, so
/// both eviction paths carry the identical close policy.
fn slow_client_close_frame() -> CloseFrame {
    CloseFrame {
        code: CloseCode::Policy,
        reason: "client is not reading messages".into(),
    }
}

/// Marker error returned by the outbound writer when a dequeued frame made
/// no socket write progress within the slow-client grace (peer stopped
/// reading, Send-Q wedged). The connection loop downcasts for it and takes
/// the same slow-client exit as a full enqueue queue instead of surfacing a
/// transport error, keeping the eviction policy in one place.
#[derive(Debug)]
struct WriteStalledError;

impl std::fmt::Display for WriteStalledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "control plane WebSocket write made no progress; client is not reading messages",
        )
    }
}

impl std::error::Error for WriteStalledError {}

fn collab_stopped_close_frame() -> CloseFrame {
    CloseFrame {
        code: CloseCode::Away,
        reason: "collaboration room stopped".into(),
    }
}

/// One bounded outbound WebSocket write. A client that stops reading wedges
/// the kernel Send-Q long before any enqueue-side bound can observe it, so
/// every direct `write.send` must itself be bounded: on timeout the send is
/// abandoned and the caller exits the connection instead of pinning the task
/// (and, for collaboration, the participant slot) forever. Control-plane
/// `/ws` frames go through the writer task, which applies the same bound to
/// each dequeued frame; the handshake keeps its own separate
/// [`WebSocketTimeouts::handshake`] bound.
async fn bounded_ws_send<W>(
    write: &mut W,
    message: Message,
    timeout: Duration,
    context: &'static str,
) -> Result<()>
where
    W: futures_util::Sink<Message> + Unpin,
    W::Error: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(timeout, write.send(message)).await {
        Ok(result) => result.map_err(anyhow::Error::from).context(context),
        Err(_) => bail!("WebSocket write made no progress within {timeout:?}: {context}"),
    }
}

async fn websocket_connection<S>(
    stream: S,
    raw: RawRequest,
    state: ServerState,
    protocol: Option<String>,
    timeouts: WebSocketTimeouts,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Fan-in: the manager merges every session runtime's projected events
    // (tagged with the owning top-level `sessionId`); every connection sees
    // every session's events, and commands route explicitly by sessionId.
    let mut events = state.manager.events();
    // Extension UI events likewise fan in through the manager; host/TUI-owned
    // interactions were already filtered by the runtime forwarders, and
    // remote answering stays rejected below.
    let mut ui_events = state.manager.ui_events();
    let mut discovery_events = state.discovery.as_ref().map(|view| view.subscribe());
    let mut prefix = raw.raw_headers;
    prefix.extend_from_slice(&raw.remainder);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WS_MESSAGE_BYTES))
        .max_write_buffer_size(OUTBOUND_QUEUE_BYTE_BUDGET.saturating_add(OUTBOUND_FRAME_HEADER_SLACK));
    // Bound the upgrade itself so a client that sends request headers and
    // then stalls cannot pin one of the listener's connection tasks forever.
    let websocket = match tokio::time::timeout(
        timeouts.handshake,
        accept_hdr_async_with_config(
            PrefixedStream {
                prefix,
                offset: 0,
                stream,
            },
            |request: &Request,
             mut response: Response|
             -> std::result::Result<Response, ErrorResponse> {
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
        ),
    )
    .await
    {
        Ok(result) => result.context("upgrading control plane WebSocket")?,
        Err(_) => bail!("control plane WebSocket handshake timed out"),
    };

    let (mut websocket_write, mut websocket_read) = websocket.split();
    // Inbound frames/results drained (consumed) around outbound enqueue
    // waits. The bool marks frames whose idle credit was already granted at
    // drain time, so processing them in the read branch cannot credit twice.
    // One entry is staged per iteration OUTSIDE the select (never popped
    // inside a droppable select future) and re-offered until handled.
    let mut pending_inbound: VecDeque<(
        Result<Message, tokio_tungstenite::tungstenite::Error>,
        bool,
    )> = VecDeque::new();
    let mut inbound_front: Option<(
        Result<Message, tokio_tungstenite::tungstenite::Error>,
        bool,
    )> = None;
    // Connection state owns transport/bootstrap only. It never owns an active
    // session: one missing-id get_state may discover the primary, then every
    // session-scoped command must carry explicit ownership.
    let primary_session_id = state.manager.primary_session_id();
    let mut bootstrap_available = true;
    let mut subscriptions = ConnectionSubscriptions::default();
    let (outbound_messages, mut outbound_rx) =
        mpsc::channel::<OutboundMessage>(OUTBOUND_QUEUE_CAPACITY);
    let outbound_tx = OutboundSender {
        messages: outbound_messages,
        bytes: Arc::new(Semaphore::new(OUTBOUND_QUEUE_BYTE_BUDGET)),
        encoder: Arc::new(Semaphore::new(1)),
    };
    let tmux_owner = crate::modes::tmux::TmuxOwnerGuard::new(
        state.tmux_terminals.clone(),
        uuid::Uuid::now_v7(),
    );
    let tmux_owner_id = tmux_owner.owner();
    // A full queue is a slow-client signal, not a failure: wait up to the
    // grace window for one slot (see enqueue_message) so transient writer
    // stalls never drop events or close the connection.
    let slow_client_grace = timeouts.slow_client_grace;
    // Non-inline commands run on a per-connection task set so a long command
    // never blocks the read/event select below. Ordinary work retains
    // MAX_CONCURRENT_COMMANDS slots, plus one reserved recovery task; the
    // JoinSet itself is hard-bounded at MAX_SPAWNED_COMMANDS so bypass-command
    // flooding cannot grow the per-connection task set without bound.
    let mut commands = JoinSet::new();
    let mut tmux_forwarders = JoinSet::new();
    let (tmux_fwd_tx, mut tmux_fwd_rx) = mpsc::channel::<TmuxTerminalForwarder>(16);
    let mut ordinary_commands = 0usize;
    #[cfg(test)]
    let snapshot_overflow_observer = state.snapshot_overflow_observer.clone();
    let mut active_snapshot: Option<ActiveSnapshot> = None;
    let mut pending_snapshots = std::collections::HashMap::<RuntimeIdentity, (u64, bool)>::new();
    let mut snapshot_windows = std::collections::HashMap::<RuntimeIdentity, SnapshotWindow>::new();
    let mut snapshots = JoinSet::<SnapshotTaskResult>::new();
    let mut next_snapshot_generation = 0u64;
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
                    Some(WriterControl::FlushPeerClose) => {
                        // The peer's close arrived on the read half, so the
                        // echo reply is auto-queued in the shared context.
                        // Draining the sink transmits it; sending another
                        // Close would be rejected before any flush.
                        let _ = tokio::time::timeout(WS_CLOSE_TIMEOUT, websocket_write.flush()).await;
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
                    Some(WriterControl::FlushPeerClose) => {
                        let _ = tokio::time::timeout(WS_CLOSE_TIMEOUT, websocket_write.flush()).await;
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                send = tokio::time::timeout(
                    outbound_write_timeout(
                        outbound_message_bytes(&message.message),
                        slow_client_grace,
                    ),
                    websocket_write.send(message.message),
                ) => match send {
                    Ok(result) => result.context("sending control plane WebSocket message")?,
                    Err(_) => {
                        // The frame was dequeued, but the socket made no
                        // write progress within the slow-client grace: the
                        // peer stopped reading and the kernel Send-Q is
                        // wedged. Enqueue bounds cannot observe this; the
                        // write itself is the backpressure frontier. Exit
                        // immediately (a close frame could not be delivered
                        // on a socket that cannot drain anyway) with the
                        // stall marker, so the connection loop takes the
                        // same slow-client exit as a full queue and stops
                        // every further enqueue, serialization, and snapshot
                        // for this connection instead of pinning the writer
                        // (and one tokio worker) forever.
                        return Err(anyhow::Error::new(WriteStalledError));
                    }
                }
            }
        }
    }));
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + timeouts.ping_interval,
        timeouts.ping_interval,
    );
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle = tokio::time::sleep(timeouts.idle);
    tokio::pin!(idle);
    // Inbound item processing shared by the staged and socket read arms: one
    // idle credit per frame (drained entries arrive pre-credited), identical
    // handling for both paths.
    macro_rules! process_inbound {
        ($result:expr, $credited:expr) => {
            match $result {
                Ok(message) => {
                    if !$credited {
                        idle.as_mut().reset(tokio::time::Instant::now() + timeouts.idle);
                    }
                    match message {
                        Message::Text(text) => {
                            if let Some(subscription) = parse_subscribe_frame(&text) {
                                match subscription {
                                    Ok(subscription) => {
                                        let exact_incarnation = subscription.runtime_incarnation.is_some();
                                        let identity = if let Some(incarnation) = subscription.runtime_incarnation {
                                            RuntimeIdentity::new(subscription.session_id, incarnation)
                                        } else {
                                            match state.manager.current_runtime_identity(&subscription.session_id).await {
                                                Ok(identity) => identity,
                                                Err(error) => {
                                                    let response = json!({
                                                        "type": "subscription_error",
                                                        "sessionId": subscription.session_id,
                                                        "error": error.to_string(),
                                                    });
                                                    if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                                        break WebSocketExit::SlowClient;
                                                    }
                                                    continue;
                                                }
                                            }
                                        };
                                        if let Some(active) = active_snapshot.as_ref()
                                            && active.identity == identity
                                        {
                                            if subscription.cursor >= active.requested_cursor {
                                                continue;
                                            }
                                            if !queue_pending_subscription(
                                                &mut pending_snapshots,
                                                identity.clone(),
                                                subscription.cursor,
                                                exact_incarnation,
                                            ) {
                                                let response = pending_subscription_limit_error(&identity);
                                                if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                                    break WebSocketExit::SlowClient;
                                                }
                                            }
                                            continue;
                                        }
                                        next_snapshot_generation = next_snapshot_generation.saturating_add(1);
                                        if active_snapshot.is_some() {
                                            if !queue_pending_subscription(
                                                &mut pending_snapshots,
                                                identity.clone(),
                                                subscription.cursor,
                                                exact_incarnation,
                                            ) {
                                                let response = pending_subscription_limit_error(&identity);
                                                if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                                    break WebSocketExit::SlowClient;
                                                }
                                                continue;
                                            }
                                            continue;
                                        }
                                        let cancel = CancellationToken::new();
                                        subscriptions.states.insert(
                                            identity.clone(),
                                            SubscriptionState::Snapshotting {
                                                generation: next_snapshot_generation,
                                                requested_cursor: subscription.cursor,
                                            },
                                        );
                                        ConnectionSubscriptions::start_snapshot(
                                            &mut snapshot_windows,
                                            &identity,
                                            subscription.cursor,
                                        );
                                        spawn_subscription_snapshot(
                                            &mut snapshots,
                                            &mut active_snapshot,
                                            state.manager.clone(),
                                            identity,
                                            subscription.cursor,
                                            next_snapshot_generation,
                                            cancel,
                                            exact_incarnation,
                                        );
                                    }
                                    Err(error) => {
                                        let response = serde_json::json!({ "type": "subscription_error", "error": error });
                                        if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                            break WebSocketExit::SlowClient;
                                        }
                                    }
                                }
                                continue;
                            }
                            if text == "{\"type\":\"ping\"}" {
                                if enqueue_json(&outbound_tx, &serde_json::json!({ "type": "pong" }), slow_client_grace).await.is_err() {
                                    break WebSocketExit::SlowClient;
                                }
                                continue;
                            }
                            match parse_input(text.as_bytes()) {
                                Ok(RpcInput::Command { command, session_id }) => {
                                    let session_id = if let Some(session_id) = session_id {
                                        Some(session_id)
                                    } else if bootstrap_available && matches!(command, super::rpc::RpcCommand::GetState { .. }) {
                                        bootstrap_available = false;
                                        Some(primary_session_id.clone())
                                    } else if command.is_listen_local() {
                                        None
                                    } else {
                                        let response = RpcResponse::failure(
                                            command.id(),
                                            command.command_name(),
                                            "sessionId is required after WebSocket bootstrap",
                                        );
                                        if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                            break WebSocketExit::SlowClient;
                                        }
                                        continue;
                                    };
                                    if is_tmux_terminal_lifecycle(&command) {
                                        if admit_tmux_terminal_lifecycle(
                                            &state,
                                            command,
                                            session_id.as_deref(),
                                            tmux_owner_id,
                                            &outbound_tx,
                                            slow_client_grace,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            break WebSocketExit::SlowClient;
                                        }
                                        continue;
                                    }
                                    let bypasses_slots = command.bypasses_command_slots();
                                    let saturated = commands.len() >= MAX_SPAWNED_COMMANDS
                                        || (!bypasses_slots && ordinary_commands >= MAX_CONCURRENT_COMMANDS);
                                    if saturated {
                                        let limit = if bypasses_slots {
                                            MAX_SPAWNED_COMMANDS
                                        } else {
                                            MAX_CONCURRENT_COMMANDS
                                        };
                                        let response = tag_requested_session(
                                            RpcResponse::slot_overload(
                                                command.id(),
                                                command.command_name(),
                                                format!("too many concurrent RPC commands (limit {limit})"),
                                            ),
                                            session_id.as_deref(),
                                        );
                                        if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                            break WebSocketExit::SlowClient;
                                        }
                                    } else {
                                        let was_ordinary = !bypasses_slots;
                                        if was_ordinary {
                                            ordinary_commands += 1;
                                        }
                                        let command_state = state.clone();
                                        let outbound_tx = outbound_tx.clone();
                                        let tmux_fwd_tx = tmux_fwd_tx.clone();
                                        #[cfg(test)]
                                        let command_task_gate = state.command_task_gate.clone();
                                        // Command identity is captured before the
                                        // dispatch move so a panicking handler can
                                        // still answer its own id.
                                        let command_id = command.id();
                                        let command_name = command.command_name();
                                        commands.spawn(async move {
                                            let task = async {
                                                #[cfg(test)]
                                                if let Some(gate) = command_task_gate {
                                                    gate.started.add_permits(1);
                                                    gate.release.acquire().await.expect("test command release").forget();
                                                }
                                                let tmux_ws = Some(TmuxWs {
                                                    owner: tmux_owner_id,
                                                });
                                                let (response, tmux_forwarder) = if matches!(
                                                    command,
                                                    super::rpc::RpcCommand::TmuxTerminalAttach { .. }
                                                ) {
                                                    let (response, forwarder) =
                                                        handle_tmux_terminal_command(
                                                            &command_state,
                                                            command,
                                                            tmux_ws,
                                                        )
                                                        .await;
                                                    (
                                                        tag_requested_session(
                                                            response,
                                                            session_id.as_deref(),
                                                        ),
                                                        forwarder,
                                                    )
                                                } else if command.is_listen_local() {
                                                    (
                                                        dispatch_http_command(
                                                            &command_state,
                                                            command,
                                                            session_id,
                                                            tmux_ws,
                                                        )
                                                        .await,
                                                        None,
                                                    )
                                                } else {
                                                    (
                                                        command_state
                                                            .manager
                                                            .admit_web(command, session_id, false)
                                                            .await
                                                            .wait()
                                                            .await,
                                                        None,
                                                    )
                                                };
                                                let enqueued = enqueue_json(
                                                    &outbound_tx,
                                                    &response,
                                                    slow_client_grace,
                                                )
                                                .await;
                                                if let Some(forwarder) = tmux_forwarder {
                                                    match enqueued {
                                                        Ok(()) => {
                                                            let registry = forwarder.registry.clone();
                                                            let owner = forwarder.owner;
                                                            let terminal_id =
                                                                forwarder.terminal_id.clone();
                                                            if tmux_fwd_tx.send(forwarder).await.is_err()
                                                            {
                                                                let _ = registry
                                                                    .detach(owner, &terminal_id);
                                                            }
                                                        }
                                                        Err(_) => {
                                                            let _ = forwarder.registry.detach(
                                                                forwarder.owner,
                                                                &forwarder.terminal_id,
                                                            );
                                                        }
                                                    }
                                                }
                                                enqueued
                                            };
                                            // A panic in one command handler is
                                            // command-scoped: it becomes a bounded
                                            // RpcResponse failure on the SAME
                                            // transport. Letting it escape would
                                            // surface as a JoinError in the select
                                            // loop and close /ws for every panel.
                                            let enqueued = match FutureExt::catch_unwind(std::panic::AssertUnwindSafe(task)).await {
                                                Ok(enqueued) => enqueued,
                                                Err(panic) => {
                                                    let response = RpcResponse::failure(
                                                        command_id,
                                                        command_name,
                                                        format!("internal error: command task failed: {}", panic_message(&panic)),
                                                    );
                                                    enqueue_json(&outbound_tx, &response, slow_client_grace).await
                                                }
                                            };
                                            (was_ordinary, enqueued)
                                        });
                                    }
                                }
                                Ok(RpcInput::ExtensionUiResponse(_)) => {
                                    if enqueue_json(
                                        &outbound_tx,
                                        &RpcResponse::failure(None, "extension_ui_response", REMOTE_EXTENSION_UI_ERROR),
                                        slow_client_grace,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break WebSocketExit::SlowClient;
                                    }
                                }
                                Err(response) => {
                                    if enqueue_json(&outbound_tx, &response, slow_client_grace).await.is_err() {
                                        break WebSocketExit::SlowClient;
                                    }
                                }
                            }
                        }
                        Message::Binary(_) => {
                            break WebSocketExit::Graceful(Some(CloseFrame {
                                code: CloseCode::Unsupported,
                                reason: "binary messages are not supported".into(),
                            }));
                        }
                        Message::Close(_) => break WebSocketExit::PeerInitiatedClose,
                        Message::Ping(payload) => {
                            if enqueue_message(&outbound_tx, Message::Pong(payload), slow_client_grace).await.is_err() {
                                break WebSocketExit::SlowClient;
                            }
                        }
                        Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
                Err(tokio_tungstenite::tungstenite::Error::Capacity(_)) => {
                    break WebSocketExit::Graceful(Some(CloseFrame {
                        code: CloseCode::Size,
                        reason: "message too large".into(),
                    }));
                }
                Err(error) => {
                    let writer_result = stop_websocket_writer(
                        writer,
                        writer_control_tx,
                        outbound_tx,
                        WriterControl::Close(None),
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
        };
    }

    let exit = loop {
        if inbound_front.is_none()
            && let Some(entry) = pending_inbound.pop_front()
        {
            inbound_front = Some(entry);
        }
        tokio::select! {
            // Fair branch polling (NO `biased`): with `biased` the application
            // event branches starve the inbound-read branch whenever the
            // events channel is continuously ready. Inbound safety and
            // fast-ack commands must still be read while event delivery is
            // backpressured by a slow-draining client.
            writer_result = writer.handle_mut() => {
                let result = writer_result.context("joining control plane WebSocket writer")?;
                match result {
                    Ok(()) => {
                        return Err(anyhow!("control plane WebSocket writer exited unexpectedly"));
                    }
                    Err(error) => {
                        if error.downcast_ref::<WriteStalledError>().is_some() {
                            // The writer's own send-progress timeout evicted
                            // the client and the writer already exited: this
                            // poll consumed the JoinHandle output, so
                            // stop_websocket_writer must not join it again.
                            // Release the outbound path exactly as it does
                            // (the 1008 close frame cannot be delivered on a
                            // socket that just proved non-writable) and take
                            // the slow-client eviction.
                            writer.handle.take();
                            outbound_tx.encoder.close();
                            outbound_tx.bytes.close();
                            return Err(error);
                        }
                        return Err(error);
                    }
                }
            }
            _ = ping.tick() => {
                if enqueue_message(&outbound_tx, Message::Ping(Vec::new().into()), slow_client_grace).await.is_err() {
                    break WebSocketExit::SlowClient;
                }
                drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
            }
            _ = &mut idle, if inbound_front.is_none() && pending_inbound.is_empty() => {
                break WebSocketExit::Graceful(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "control plane connection idle".into(),
                }));
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if ConnectionSubscriptions::buffer_snapshot_event(
                        &mut snapshot_windows,
                        &event,
                        #[cfg(test)]
                        snapshot_overflow_observer.as_deref(),
                    ) {
                        continue;
                    }
                    if event.get("type").and_then(serde_json::Value::as_str) == Some("session_unloaded") {
                        let identity = subscriptions.identity_from_event(&event);
                        if enqueue_json(&outbound_tx, &event, slow_client_grace).await.is_err() {
                            break WebSocketExit::SlowClient;
                        }
                        drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                        if let Some(identity) = identity {
                            subscriptions.unload_identity(&identity);
                            snapshot_windows.remove(&identity);
                            pending_snapshots.remove(&identity);
                            if let Some(active) = active_snapshot.as_ref()
                                && active.identity == identity
                            {
                                active.cancel.cancel();
                                active_snapshot.take();
                                start_next_pending_snapshot(
                                    &mut pending_snapshots,
                                    &mut snapshots,
                                    &mut active_snapshot,
                                    &mut snapshot_windows,
                                    &mut subscriptions,
                                    &mut next_snapshot_generation,
                                    state.manager.clone(),
                                );
                            }
                        }
                        continue;
                    }
                    match subscriptions.decide(&event) {
                        SubscriptionDecision::Forward => {
                            if enqueue_json(&outbound_tx, &event, slow_client_grace).await.is_err() {
                                break WebSocketExit::SlowClient;
                            }
                            drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                        }
                        SubscriptionDecision::Skip => {}
                        SubscriptionDecision::Gap { identity, cursor } => {
                            subscriptions.mark_resync(
                                identity.clone(),
                                ResyncRequiredReason::Gap,
                                cursor,
                            );
                            if enqueue_json(
                                &outbound_tx,
                                &resync_required_frame(
                                    &identity,
                                    Some(cursor),
                                    ResyncRequiredReason::Gap,
                                ),
                                slow_client_grace,
                            )
                            .await
                            .is_err()
                            {
                                break WebSocketExit::SlowClient;
                            }
                            drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                        }
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let mut is_slow_client = false;
                    for (identity, cursor) in subscriptions.identities() {
                        if matches!(subscriptions.states.get(&identity), Some(SubscriptionState::ResyncRequired { .. })) {
                            continue;
                        }
                        subscriptions.mark_resync(
                            identity.clone(),
                            ResyncRequiredReason::Lag,
                            cursor,
                        );
                        if enqueue_json(
                            &outbound_tx,
                            &resync_required_frame(
                                &identity,
                                Some(cursor),
                                ResyncRequiredReason::Lag,
                            ),
                            slow_client_grace,
                        )
                        .await
                        .is_err()
                        {
                            is_slow_client = true;
                            break;
                        }
                        drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                    }
                    if let Some(active) = active_snapshot.take() {
                        active.cancel.cancel();
                        snapshot_windows.remove(&active.identity);
                    }
                    pending_snapshots.clear();
                    if is_slow_client {
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
                    if !subscriptions.contains(
                        event.session_id.as_deref(),
                        event.runtime_incarnation.as_deref(),
                    ) {
                        continue;
                    }
                    if enqueue_json(&outbound_tx, &event, slow_client_grace).await.is_err() {
                        break WebSocketExit::SlowClient;
                    }
                    drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    if enqueue_json(&outbound_tx, &RpcResponse::failure(None, "extension_ui", format!("extension UI event stream lagged by {count} records")), slow_client_grace).await.is_err() {
                        break WebSocketExit::SlowClient;
                    }
                    drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break WebSocketExit::Graceful(None);
                }
            },
            event = async {
                match discovery_events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match event {
                Ok(event) => {
                    if enqueue_json(&outbound_tx, &discovery_event_json(&event), slow_client_grace).await.is_err() {
                        break WebSocketExit::SlowClient;
                    }
                    drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
            },
            completed = snapshots.join_next_with_id(), if !snapshots.is_empty() => match completed {
                Some(Ok((_task_id, completed))) => {
                    let Some(active) = take_matching_active_snapshot(&mut active_snapshot, &completed) else {
                        continue;
                    };
                    let identity = completed.identity;
                    let snapshot = completed.snapshot;
                    let watermark = snapshot.get("watermark").and_then(serde_json::Value::as_u64);
                    let window = snapshot_windows.remove(&identity).unwrap_or_default();
                    let is_error = snapshot.get("type").and_then(serde_json::Value::as_str)
                        == Some("subscription_error");
                    if window.is_overflowed {
                        subscriptions.mark_resync(
                            identity.clone(),
                            ResyncRequiredReason::Overflow,
                            window.cursor,
                        );
                        if enqueue_json(
                            &outbound_tx,
                            &resync_required_frame(
                                &identity,
                                Some(window.cursor),
                                ResyncRequiredReason::Overflow,
                            ),
                            slow_client_grace,
                        )
                        .await
                        .is_err()
                        {
                            break WebSocketExit::SlowClient;
                        }
                        drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                    } else {
                        if let Some(watermark) = watermark {
                            subscriptions.install(identity.clone(), watermark);
                        } else if is_error {
                            subscriptions.drop_identity(&identity);
                        }
                        if enqueue_json(&outbound_tx, &snapshot, slow_client_grace).await.is_err() {
                            break WebSocketExit::SlowClient;
                        }
                        drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                        if let Some(watermark) = watermark {
                            let mut replay_failed = false;
                            for event in window.events {
                                let Some(sequence) = event.get("sequence").and_then(serde_json::Value::as_u64) else { continue };
                                if sequence <= watermark { continue; }
                                match subscriptions.decide(&event) {
                                    SubscriptionDecision::Forward => {
                                        if enqueue_json(&outbound_tx, &event, slow_client_grace).await.is_err() {
                                            replay_failed = true;
                                            break;
                                        }
                                        drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                                    }
                                    SubscriptionDecision::Skip => {}
                                    SubscriptionDecision::Gap { identity, cursor } => {
                                        subscriptions.mark_resync(identity.clone(), ResyncRequiredReason::Gap, cursor);
                                        if enqueue_json(
                                            &outbound_tx,
                                            &resync_required_frame(&identity, Some(cursor), ResyncRequiredReason::Gap),
                                            slow_client_grace,
                                        ).await.is_err() {
                                            replay_failed = true;
                                        }
                                        drain_buffered_inbound(&mut websocket_read, idle.as_mut(), timeouts.idle, &mut pending_inbound);
                                        break;
                                    }
                                }
                            }
                            if replay_failed {
                                break WebSocketExit::SlowClient;
                            }
                        }
                    }
                    start_next_pending_snapshot(
                        &mut pending_snapshots,
                        &mut snapshots,
                        &mut active_snapshot,
                        &mut snapshot_windows,
                        &mut subscriptions,
                        &mut next_snapshot_generation,
                        state.manager.clone(),
                    );
                }
                Some(Err(error)) => {
                    if let Some(active) = take_failed_active_snapshot(&mut active_snapshot, error.id()) {
                        snapshot_windows.remove(&active.identity);
                        subscriptions.drop_identity(&active.identity);
                        start_next_pending_snapshot(
                            &mut pending_snapshots,
                            &mut snapshots,
                            &mut active_snapshot,
                            &mut snapshot_windows,
                            &mut subscriptions,
                            &mut next_snapshot_generation,
                            state.manager.clone(),
                        );
                    }
                }
                None => {}
            },
            Some(forwarder) = tmux_fwd_rx.recv() => {
                let outbound = outbound_tx.clone();
                tmux_forwarders.spawn(run_tmux_terminal_forwarder(
                    forwarder,
                    outbound,
                    slow_client_grace,
                ));
            }
            completed = tmux_forwarders.join_next(), if !tmux_forwarders.is_empty() => match completed {
                Some(Ok(Err(()))) | Some(Err(_)) => {
                    break WebSocketExit::SlowClient;
                }
                Some(Ok(Ok(()))) | None => {}
            },
            completed = commands.join_next(), if !commands.is_empty() => match completed {
                Some(Ok((was_ordinary, Ok(())))) => {
                    if was_ordinary {
                        ordinary_commands -= 1;
                    }
                }
                Some(Ok((was_ordinary, Err(_)))) => {
                    if was_ordinary {
                        ordinary_commands -= 1;
                    }
                    // A spawned command could not enqueue its response: the
                    // client stopped reading (outbound queue full) or the
                    // writer stopped. Tear the connection down like any
                    // other outbound failure.
                    break WebSocketExit::SlowClient;
                }
                Some(Err(_)) => {
                    // Unreachable in production: the command task converts
                    // its own panics into a command-scoped RpcResponse
                    // failure (see the spawn below). Kept as a
                    // transport-preserving no-op so a latent JoinError can
                    // never again tear down the shared socket (the old code
                    // returned Err here and closed /ws).
                }
                None => {}
            },
            staged = std::future::ready(()), if inbound_front.is_some() => {
                let (result, credited) = inbound_front.take().expect("staged inbound entry");
                process_inbound!(result, credited);
            }
            incoming = websocket_read.next(), if inbound_front.is_none() => match incoming {
                Some(result) => process_inbound!(result, false),
                None => break WebSocketExit::Graceful(None),
            }
        }
    };

    let close = match exit {
        WebSocketExit::Graceful(frame) => WriterControl::Close(frame),
        WebSocketExit::PeerInitiatedClose => WriterControl::FlushPeerClose,
        WebSocketExit::SlowClient => WriterControl::Close(Some(slow_client_close_frame())),
    };
    let result = stop_websocket_writer(writer, writer_control_tx, outbound_tx, close).await;
    drop(tmux_owner);
    result
}

async fn stop_websocket_writer(
    mut writer: AbortTask<Result<()>>,
    writer_control_tx: mpsc::Sender<WriterControl>,
    outbound_tx: OutboundSender,
    close: WriterControl,
) -> Result<()> {
    let _ = writer_control_tx.try_send(close);
    drop(writer_control_tx);
    // Wake command tasks waiting for encoder/data ownership immediately;
    // close control uses its dedicated channel and never queues behind them.
    outbound_tx.encoder.close();
    outbound_tx.bytes.close();
    drop(outbound_tx);
    match tokio::time::timeout(WS_CLOSE_TIMEOUT, writer.handle_mut()).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow!(error).context("joining control plane WebSocket writer")),
        Err(_) => {
            let handle = writer.handle.take().expect("writer task is present");
            handle.abort();
            match tokio::time::timeout(WS_CLOSE_TIMEOUT, handle).await {
                Ok(Err(error)) if error.is_cancelled() => Err(anyhow!(
                    "control plane WebSocket writer did not stop promptly"
                )),
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    Err(anyhow!(error).context("joining aborted control plane WebSocket writer"))
                }
                Err(_) => Err(anyhow!(
                    "control plane WebSocket writer did not stop promptly"
                )),
            }
        }
    }
}
/// Consume immediately-readable inbound frames after an outbound wait, up to
/// the strict `MAX_PENDING_INBOUND` cap: liveness credit stays proportional
/// to real reads and the pending queue can never grow without bound.
fn drain_buffered_inbound<S>(
    read: &mut S,
    mut idle: Pin<&mut tokio::time::Sleep>,
    idle_window: Duration,
    pending: &mut VecDeque<(
        Result<Message, tokio_tungstenite::tungstenite::Error>,
        bool,
    )>,
) where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        if pending.len() >= MAX_PENDING_INBOUND {
            return;
        }
        match read.next().now_or_never() {
            Some(Some(Ok(message))) => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_window);
                pending.push_back((Ok(message), true));
            }
            Some(Some(Err(error))) => {
                pending.push_back((Err(error), false));
                return;
            }
            _ => return,
        }
    }
}

async fn enqueue_message(
    sender: &OutboundSender,
    message: Message,
    slow_client_grace: Duration,
) -> Result<()> {
    let bytes = outbound_message_bytes(&message);
    if bytes > OUTBOUND_QUEUE_BYTE_BUDGET {
        bail!("control plane outbound message exceeds byte budget");
    }
    let bytes = u32::try_from(bytes).context("control plane outbound message size")?;
    let permit = tokio::time::timeout(
        slow_client_grace,
        sender.bytes.clone().acquire_many_owned(bytes),
    )
    .await
    .map_err(|_| anyhow!("control plane outbound byte budget is exhausted"))?
    .map_err(|_| anyhow!("control plane outbound writer stopped"))?;
    enqueue_reserved(sender, message, permit, slow_client_grace).await
}

async fn enqueue_reserved(
    sender: &OutboundSender,
    message: Message,
    permit: tokio::sync::OwnedSemaphorePermit,
    slow_client_grace: Duration,
) -> Result<()> {
    let outbound = OutboundMessage {
        message,
        _bytes: permit,
    };
    tokio::time::timeout(slow_client_grace, sender.messages.send(outbound))
        .await
        .map_err(|_| anyhow!("control plane outbound queue is full"))?
        .map_err(|_| anyhow!("control plane outbound writer stopped"))
}

async fn enqueue_json<T: Serialize>(
    sender: &OutboundSender,
    value: &T,
    slow_client_grace: Duration,
) -> Result<()> {
    if sender.messages.is_closed() {
        bail!("control plane outbound writer stopped");
    }
    // Only one connection task may approach serialization. Reserving the
    // connection's complete byte budget before serde runs means the encoded
    // String, and every already queued String, can never coexist above the
    // budget. Once its exact size is known, only that portion is transferred
    // to OutboundMessage and the unused reservation is released.
    let reservation_timeout =
        outbound_write_timeout(OUTBOUND_QUEUE_BYTE_BUDGET, slow_client_grace);
    let encoder = tokio::time::timeout(
        reservation_timeout,
        sender.encoder.clone().acquire_owned(),
    )
    .await
    .map_err(|_| anyhow!("control plane outbound encoder is busy"))?
    .map_err(|_| anyhow!("control plane outbound writer stopped"))?;
    let mut reservation = tokio::time::timeout(
        reservation_timeout,
        sender
            .bytes
            .clone()
            .acquire_many_owned(OUTBOUND_QUEUE_BYTE_BUDGET as u32),
    )
    .await
    .map_err(|_| anyhow!("control plane outbound byte budget is exhausted"))?
    .map_err(|_| anyhow!("control plane outbound writer stopped"))?;
    let text = serde_json::to_string(value).context("serializing control plane message")?;
    let bytes = text.len();
    if bytes > OUTBOUND_QUEUE_BYTE_BUDGET {
        bail!("control plane outbound message exceeds byte budget");
    }
    let permit = reservation
        .split(bytes)
        .expect("encoded frame fits the complete outbound reservation");
    drop(reservation);
    let result = enqueue_reserved(sender, Message::Text(text.into()), permit, slow_client_grace).await;
    drop(encoder);
    result
}

fn discovery_event_json(event: &crate::team::DiscoveryEvent) -> serde_json::Value {
    match event {
        crate::team::DiscoveryEvent::RunnerDiscovered {
            origin,
            host,
            port,
            label,
            version,
            scheme,
            codename,
        } => {
            let http_scheme = if scheme == "wss" { "https" } else { "http" };
            serde_json::json!({
                "type": "runner_discovered",
                "origin": format!("{http_scheme}://{host}:{port}"),
                "wsOrigin": origin,
                "host": host,
                "port": port,
                "label": label,
                "codename": codename,
                "version": version,
                "scheme": scheme,
            })
        }
        crate::team::DiscoveryEvent::RunnerGone { origin } => {
            serde_json::json!({
                "type": "runner_gone",
                "origin": origin,
            })
        }
    }
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

async fn serve_web_page<S>(stream: &mut S, if_none_match: Option<&str>) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let (mime, bytes) = if let Ok(dir) = std::env::var(RPI_WEB_DEV_DIR)
        && !dir.trim().is_empty()
    {
        let file = pi_coding::read_operator_file(
            &Path::new(dir.trim()).join("index.html"),
            64 * 1024 * 1024,
        )
        .with_context(|| format!("RPI_WEB_DEV_DIR {dir:?} index.html is unreadable"))?;
        ("text/html; charset=utf-8".to_owned(), file.bytes)
    } else {
        let (mime, bytes) = crate::web::index().context("embedded web client assets are missing")?;
        (mime.to_owned(), bytes.to_vec())
    };
    // The web client is a moving bundle: it must always revalidate, but an
    // unchanged 6+ MB document must not re-download on every phone wake. The
    // ETag turns a warm page load into one small 304 round-trip.
    let etag = format!("\"{}\"", web_bundle_etag(&bytes));
    if if_none_match.is_some_and(|candidate| candidate.trim() == etag) {
        let header = format!(
            "HTTP/1.1 304 Not Modified\r\netag: {etag}\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
        );
        stream.write_all(header.as_bytes()).await?;
        stream.shutdown().await?;
        return Ok(());
    }
    let reason = StatusCode::OK.canonical_reason().unwrap_or("OK");
    let header = format!(
        "HTTP/1.1 200 {reason}\r\ncontent-type: {mime}\r\ncontent-length: {}\r\netag: {etag}\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
        bytes.len(),
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
    Ok(())
}

fn web_bundle_etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The web client is a moving bundle: every deploy must reach browsers on the
/// next load, so the document is revalidated instead of heuristically cached
/// (phones otherwise run stale JS for days and resurface fixed bugs).
async fn write_response_no_cache<S>(
    stream: &mut S,
    status: StatusCode,
    content_type: &str,
    body: &[u8],
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let reason = status.canonical_reason().unwrap_or("Error");
    let header = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
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

async fn write_plain_response<S>(stream: &mut S, status: StatusCode, message: &str) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response(
        stream,
        status,
        "text/plain; charset=utf-8",
        message.as_bytes(),
    )
    .await
}

async fn write_json_response<T: Serialize, S>(
    stream: &mut S,
    status: StatusCode,
    value: &T,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_json_response_cors(stream, status, value, None).await
}

/// Like [`write_json_response`] with an optional reflected CORS origin.
async fn write_json_response_cors<T: Serialize, S>(
    stream: &mut S,
    status: StatusCode,
    value: &T,
    allow_origin: Option<&str>,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value).context("serializing HTTP RPC response")?;
    write_response_with_cors(stream, status, CONTENT_TYPE_JSON, &body, allow_origin).await
}

/// Like [`write_plain_response`] with an optional reflected CORS origin.
async fn write_plain_response_cors<S>(
    stream: &mut S,
    status: StatusCode,
    message: &str,
    allow_origin: Option<&str>,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response_with_cors(
        stream,
        status,
        "text/plain; charset=utf-8",
        message.as_bytes(),
        allow_origin,
    )
    .await
}

/// Deadline for a video-upload response write: a client that stops reading
/// must not pin the connection task while the (multi-MiB) frame payload is
/// written. Reuses the request read bound.
const VIDEO_RESPONSE_WRITE_TIMEOUT: Duration = READ_TIMEOUT;

/// Write a video-upload JSON response under a bounded deadline (slow-reader
/// protection). The timeout is a parameter so tests can shrink it.
async fn write_video_json_response_bounded<T: Serialize, S>(
    stream: &mut S,
    status: StatusCode,
    value: &T,
    allow_origin: Option<&str>,
    timeout: Duration,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    match tokio::time::timeout(
        timeout,
        write_json_response_cors(stream, status, value, allow_origin),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => bail!("timed out writing the video upload response"),
    }
}

/// Write a video-upload plain response under a bounded deadline.
async fn write_video_plain_response_bounded<S>(
    stream: &mut S,
    status: StatusCode,
    message: &str,
    allow_origin: Option<&str>,
    timeout: Duration,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    match tokio::time::timeout(
        timeout,
        write_plain_response_cors(stream, status, message, allow_origin),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => bail!("timed out writing the video upload response"),
    }
}

async fn write_response<S>(
    stream: &mut S,
    status: StatusCode,
    content_type: &str,
    body: &[u8],
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response_with_cors(stream, status, content_type, body, None).await
}

/// Like [`write_response`] with an optional `access-control-allow-origin`
/// header (plus `vary: origin`) for the video upload endpoint. The origin is
/// always a validated, reflected requester origin — never `*`.
async fn write_response_with_cors<S>(
    stream: &mut S,
    status: StatusCode,
    content_type: &str,
    body: &[u8],
    allow_origin: Option<&str>,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let reason = status.canonical_reason().unwrap_or("Error");
    let mut header = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\n",
        status.as_u16(),
        reason,
        content_type,
        body.len()
    );
    if let Some(origin) = allow_origin {
        header.push_str("access-control-allow-origin: ");
        header.push_str(origin);
        header.push_str("\r\nvary: origin\r\n");
    }
    header.push_str("connection: close\r\n\r\n");
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::{Value, json};
    use std::time::Instant;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, tungstenite::client::IntoClientRequest,
    };
    #[test]
    fn stale_snapshot_completion_preserves_newer_active_generation() {
        let identity = RuntimeIdentity::new("session-a", "incarnation-a");
        let mut active = Some(ActiveSnapshot {
            identity: identity.clone(),
            generation: 2,
            requested_cursor: 0,
            cancel: CancellationToken::new(),
            task_id: None,
        });
        let stale = SnapshotTaskResult {
            identity: identity.clone(),
            generation: 1,
            snapshot: json!({}),
        };
        assert!(take_matching_active_snapshot(&mut active, &stale).is_none());
        assert_eq!(active.as_ref().map(|snapshot| snapshot.generation), Some(2));

        let current = SnapshotTaskResult {
            identity,
            generation: 2,
            snapshot: json!({}),
        };
        assert_eq!(
            take_matching_active_snapshot(&mut active, &current)
                .map(|snapshot| snapshot.generation),
            Some(2)
        );
        assert!(active.is_none());
    }

    #[test]
    fn control_plane_idle_timeout_is_thirty_seconds() {
        assert_eq!(WS_IDLE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(DEFAULT_WS_TIMEOUTS.idle, Duration::from_secs(30));
    }

    #[test]
    fn parse_subscribe_frame_accepts_only_subscribe_type_and_reports_bad_frames() {
        let good = parse_subscribe_frame(r#"{"type":"subscribe","sessionId":"s1","cursor":7}"#);
        let frame = good
            .expect("subscribe frame recognized")
            .expect("frame decodes");
        assert_eq!(frame.session_id, "s1");
        assert_eq!(frame.cursor, 7);

        // Missing cursor defaults to 0 (authoritative snapshot replay).
        let default_cursor = parse_subscribe_frame(r#"{"type":"subscribe","sessionId":"s2"}"#);
        assert_eq!(
            default_cursor.expect("recognized").expect("decodes").cursor,
            0
        );

        // Non-subscribe types are not subscribe frames (None = ignore).
        assert!(parse_subscribe_frame(r#"{"type":"get_state","id":"x"}"#).is_none());
        assert!(parse_subscribe_frame("not json at all").is_none());

        // A subscribe frame failing field validation surfaces the error text.

        let invalid = parse_subscribe_frame(r#"{"type":"subscribe"}"#)
            .expect("recognized as subscribe attempt")
            .expect_err("missing sessionId must be an error");
        assert!(invalid.contains("invalid subscribe frame"), "{invalid}");
    }
    #[test]
    fn parse_subscribe_frame_rejects_oversized_identity_keys() {
        let uuid = "0198c0a6-7d3e-7cc0-9db1-3f6ce3a1b2c4";
        let accepted = parse_subscribe_frame(
            r#"{"type":"subscribe","sessionId":"0198c0a6-7d3e-7cc0-9db1-3f6ce3a1b2c4","runtimeIncarnation":"0198c0a6-7d3e-7cc0-9db1-3f6ce3a1b2c5","cursor":3}"#,
        )
        .expect("recognized")
        .expect("uuid-sized identities decode");
        assert_eq!(accepted.session_id, uuid);

        let oversized_session = "k".repeat(MAX_SUBSCRIBE_IDENTITY_CHARS + 1);
        let rejected = parse_subscribe_frame(&format!(
            r#"{{"type":"subscribe","sessionId":"{oversized_session}","cursor":0}}"#
        ))
        .expect("recognized as subscribe attempt")
        .expect_err("oversized sessionId must be rejected before becoming a pending key");
        assert!(
            rejected.contains("subscribe identity exceeds"),
            "{rejected}"
        );

        let oversized_incarnation = "i".repeat(MAX_SUBSCRIBE_IDENTITY_CHARS + 1);
        let rejected_incarnation = parse_subscribe_frame(&format!(
            r#"{{"type":"subscribe","sessionId":"{uuid}","runtimeIncarnation":"{oversized_incarnation}","cursor":0}}"#
        ))
        .expect("recognized as subscribe attempt")
        .expect_err("oversized runtimeIncarnation must be rejected");
        assert!(
            rejected_incarnation.contains("subscribe identity exceeds"),
            "{rejected_incarnation}"
        );
    }
    #[tokio::test]
    async fn duplicate_subscription_snapshot_shares_in_flight_result() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let lifecycle = manager.hold_subscription_snapshot_for_test().await;
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_millis(20),
                idle: Duration::from_secs(2),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        let subscribe = Message::Text(
            serde_json::json!({
                "type": "subscribe",
                "sessionId": session_id,
                "cursor": 0,
            })
            .to_string()
            .into(),
        );
        socket
            .send(subscribe.clone())
            .await
            .expect("send first subscription");
        socket
            .send(subscribe)
            .await
            .expect("send duplicate subscription");

        let held_deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        while tokio::time::Instant::now() < held_deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).expect("JSON response");
                    panic!("unexpected frame while snapshot held: {value}");
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("connection closed while snapshot held: {frame:?}")
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("socket error while snapshot held: {error}"),
                Ok(None) => panic!("socket closed while snapshot held"),
                Err(_) => {}
            }
        }

        drop(lifecycle);
        let snapshot = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("shared snapshot timeout")
                .expect("WebSocket closed before shared snapshot")
                .expect("read shared snapshot")
            {
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Message::Text(text) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).expect("JSON response");
                    match value["type"].as_str() {
                        Some("subscription_snapshot" | "session_resync") => break value,
                        Some("subscription_error") => {
                            panic!("duplicate subscribe must share, not error: {value}")
                        }
                        _ => {}
                    }
                }
                Message::Close(frame) => {
                    panic!("connection closed before shared snapshot: {frame:?}")
                }
                _ => {}
            }
        };
        assert_eq!(snapshot["sessionId"], session_id);
        assert_eq!(manager.protocol_snapshot_count_for_test(), 1);

        let extra_deadline = tokio::time::Instant::now() + Duration::from_millis(150);
        while tokio::time::Instant::now() < extra_deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).expect("JSON response");
                    panic!("second frame after shared snapshot: {value}");
                }
                Ok(Some(Ok(Message::Close(_)))) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("socket error after shared snapshot: {error}"),
                Ok(None) => break,
                Err(_) => {}
            }
        }

        socket
            .close(None)
            .await
            .expect("close duplicate subscription client");
        server
            .await
            .expect("server task")
            .expect("clean exit after client close");
        manager.shutdown().await;
    }

    #[test]
    fn ui_subscription_filter_requires_exact_runtime_identity() {
        let mut subscriptions = ConnectionSubscriptions::default();
        subscriptions.install(RuntimeIdentity::new("s1", "r1"), 0);
        subscriptions.install(RuntimeIdentity::new("s1", "r2"), 0);

        assert!(subscriptions.contains(Some("s1"), Some("r1")));
        assert!(!subscriptions.contains(Some("s1"), Some("stale")));
        assert!(
            !subscriptions.contains(Some("s1"), None),
            "legacy identity is ambiguous across incarnations"
        );
    }

    #[test]
    fn subscription_gate_forwards_sequences_in_order_and_skips_replays() {
        let mut gate = ConnectionSubscriptions::default();
        let identity = RuntimeIdentity::new("s1", "r1");
        gate.install(identity.clone(), 0);

        // In-order sequence advances the cursor and forwards.
        let event = |sequence: u64| json!({
            "sessionId":"s1",
            "runtimeIncarnation":"r1",
            "sequence":sequence
        });
        assert!(matches!(gate.decide(&event(1)), SubscriptionDecision::Forward));
        assert!(matches!(gate.decide(&event(2)), SubscriptionDecision::Forward));
        assert_eq!(gate.identities(), vec![(identity.clone(), 2)]);

        // Replay (sequence <= cursor) is skipped, cursor unchanged.
        assert!(matches!(gate.decide(&event(2)), SubscriptionDecision::Skip));
        assert!(matches!(gate.decide(&event(1)), SubscriptionDecision::Skip));
        assert_eq!(gate.identities(), vec![(identity, 2)]);
    }

    #[test]
    fn subscription_gate_rehydrates_on_sequence_gap_and_keeps_cursor() {
        let mut gate = ConnectionSubscriptions::default();
        let identity = RuntimeIdentity::new("s1", "r1");
        gate.install(identity.clone(), 5);

        // A gap (7 instead of 6) requires resync from the held cursor.
        let event = json!({"sessionId":"s1","runtimeIncarnation":"r1","sequence":7});
        match gate.decide(&event) {
            SubscriptionDecision::Gap { identity: gap_identity, cursor } => {
                assert_eq!(gap_identity, identity);
                assert_eq!(cursor, 5, "resync resumes from the held cursor");
            }
            other => panic!("gap must require resync, got {other:?}"),
        }
        // The cursor is NOT advanced by the gap decision.
        assert_eq!(gate.identities(), vec![(identity, 5)]);
    }

    #[test]
    fn subscription_gate_routes_by_session_and_treats_empty_registry_as_legacy_forward() {
        let mut gate = ConnectionSubscriptions::default();

        // Legacy contract: a client with NO subscriptions receives every
        // event (pre-multi-session fan-out compatibility).
        let no_sequence = json!({"sessionId":"s1"});
        assert!(matches!(
            gate.decide(&no_sequence),
            SubscriptionDecision::Forward
        ));

        // Once any subscription exists, events for un-subscribed sessions
        // are skipped and sequence-less events for subscribed sessions
        // forward (non-transcript control events).
        gate.install(RuntimeIdentity::new("s1", "r1"), 0);
        let other_session = json!({"sessionId":"other","sequence":1});
        assert!(matches!(
            gate.decide(&other_session),
            SubscriptionDecision::Skip
        ));
        let subscribed_no_sequence = json!({"sessionId":"s1"});
        assert!(matches!(
            gate.decide(&subscribed_no_sequence),
            SubscriptionDecision::Forward
        ));
        assert!(gate.contains(Some("s1"), Some("r1")));
        assert!(!gate.contains(Some("other"), None));
        // With a non-empty registry, an id-less event is NOT contained (the
        // bootstrap exception is handled before the gate, not here).
        assert!(!gate.contains(None, None));
    }

    #[test]
    fn unload_identity_removes_exact_incarnation_only() {
        let mut subscriptions = ConnectionSubscriptions::default();
        let current = RuntimeIdentity::new("s1", "r2");
        let stale = RuntimeIdentity::new("s1", "r1");
        subscriptions.install(current.clone(), 7);
        subscriptions.install(stale.clone(), 3);

        // A stale unload for the old incarnation must not delete the current
        // incarnation's subscription.
        subscriptions.unload_identity(&stale);
        assert!(subscriptions.contains(Some("s1"), Some("r2")));
        assert!(!subscriptions.contains(Some("s1"), Some("r1")));
        assert_eq!(subscriptions.cursor(&current), Some(7));

        // The matching unload removes the exact identity and its cursor.
        // The registry is empty afterwards, so `contains` — which treats an
        // empty registry as legacy forward-everything — cannot observe the
        // removal; the direct cursor lookup and emptiness are the proof.
        subscriptions.unload_identity(&current);
        assert_eq!(subscriptions.cursor(&current), None);
        assert!(subscriptions.states.is_empty());
    }

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
        open_collab_socket_with(connection, DEFAULT_WS_TIMEOUTS).await
    }

    async fn open_collab_socket_with(
        connection: CollabConnection,
        timeouts: WebSocketTimeouts,
    ) -> (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        JoinHandle<Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let protocol = "rpi-collab.test";
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            collab_websocket_connection(stream, raw, connection, protocol.to_owned(), timeouts)
                .await
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

    /// A real control-plane server state (manager with a faux primary) for
    /// driving `websocket_connection` end to end. The manager is never asked
    /// to dispatch: the tests below only exercise the upgrade and read loops.
    async fn ws_server_state() -> ServerState {
        use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
        let unique = uuid::Uuid::now_v7();
        let mut model = pi_ai::Model::default();
        model.id = format!("listen-model-{unique}");
        model.name = "Listen Test Model".into();
        model.api = format!("listen-api-{unique}");
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let model_key = (model.provider.clone(), model.id.clone());
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 8,
        });
        let cwd = tempfile::tempdir().expect("cwd").keep();
        let agent = tempfile::tempdir().expect("agent").keep();
        std::fs::write(
            agent.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(&cwd);
        resource_options.agent_dir = agent;
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
        let session = pi_coding::Session::new_with_additional_tools_filtered_and_discovery(
            pi_coding::SessionOptions {
                model,
                cwd: cwd.clone(),
                system_prompt: String::new(),
                thinking_level: pi_agent::ThinkingLevel::Off,
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
            pi_coding::ToolSelection::default(),
            pi_coding::ResourceDiscovery::Disabled,
        )
        .expect("session");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let session_dir = cwd.join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        session.set_session_dir(session_dir);
        session
            .start_new_recording()
            .expect("record listen fixture session");
        let application = pi_coding::Application::new(session).await;
        registration.unregister();
        pi_ai::replace_registered_models(&[model_key], Vec::new());
        let manager =
            SessionRuntimeManager::new(application, ExtensionUiAdapter::default(), None, None)
                .await
                .expect("session runtime manager");
        ServerState {
            manager: manager.clone(),
            collab: CollabService::new(manager),
            token: None,
            allow_insecure_remote: false,
            runner_codename: None,
            base_url: None,
            discovery: None,
            video_upload_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_VIDEO_UPLOADS,
            )),
            pdf_upload_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_PDF_UPLOADS,
            )),
            tmux_terminals: crate::modes::tmux::TmuxTerminalRegistry::new(),

            snapshot_overflow_observer: None,
            command_task_gate: None,
        }
    }

    /// An incomplete buffered HTTP request: the headers never terminate, so
    /// the WebSocket accept reads the partial prefix and then blocks on the
    /// live socket waiting for the rest — exactly the stall the handshake
    /// timeout must bound.
    fn stalled_upgrade_request(path: &str) -> RawRequest {
        let head = format!(
            "GET {path} HTTP/1.1\r\nhost: x\r\nupgrade: websocket\r\nconnection: Upgrade\r\n"
        );
        RawRequest {
            method: Method::GET,
            path: path.to_owned(),
            headers: HeaderMap::new(),
            raw_headers: head.into_bytes(),

            remainder: Vec::new(),
        }
    }
    async fn next_rpc_response(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("RPC response timeout")
                .expect("WebSocket closed before RPC response")
                .expect("read RPC response")
            {
                Message::Ping(payload) => socket
                    .send(Message::Pong(payload))
                    .await
                    .expect("send pong"),
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text).expect("JSON response");
                    if value["type"] == "response"
                        || value["type"] == "subscription_error"
                        || value["type"] == "runner_discovered"
                        || value["type"] == "runner_gone"
                    {
                        return value;
                    }
                }
                Message::Close(frame) => panic!("connection closed before RPC response: {frame:?}"),
                _ => {}
            }
        }
    }

    /// P0 regression: a `session_move` — success OR FAILURE — must NEVER
    /// close the client WebSocket. The failed move answers with its bounded
    /// failure response and the SAME socket keeps serving commands (a 1006
    /// here means the transport died with the command).
    #[tokio::test]
    async fn failed_session_move_keeps_the_transport_connection_alive() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;
        let session_id = manager.primary_session_id();

        // A failed move: the fixture manager has no session catalog storage,
        // so the move refuses at the catalog plane with a bounded failure.
        socket
            .send(Message::Text(
                json!({
                    "type":"session_move",
                    "id":"move-fails",
                    "sessionPath":"does-not-exist.jsonl",
                    "targetCwd":"/tmp/never-a-target"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send session_move");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["command"], json!("session_move"));
        assert_eq!(response["success"], json!(false));
        assert!(
            response["error"].is_string(),
            "the failed move answers with a bounded failure: {response}"
        );

        // The SAME transport must keep serving commands: a 1006 here means
        // the transport died with the failed move.
        socket
            .send(Message::Text(
                json!({"type":"get_state","id":"after-move","sessionId":session_id})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send get_state after the failed move");
        let after = next_rpc_response(&mut socket).await;
        assert_eq!(after["command"], json!("get_state"));
        assert_eq!(
            after["success"],
            json!(true),
            "the transport connection must survive a failed move: {after}"
        );

        socket.close(None).await.expect("close client");
        server.await.expect("server task").expect("server clean");
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn websocket_command_tasks_have_hard_bound_and_reserved_recovery_slot() {
        let mut state = ws_server_state().await;
        let manager = state.manager.clone();
        let session_id = manager.primary_session_id();
        let gate = Arc::new(CommandTaskGate::default());
        state.command_task_gate = Some(gate.clone());
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_secs(10),
                idle: Duration::from_secs(10),
                slow_client_grace: Duration::from_secs(2),
            },
        )
        .await;

        for id in 0..MAX_CONCURRENT_COMMANDS {
            socket
                .send(Message::Text(
                    json!({"type":"get_state","id":format!("ordinary-{id}"),"sessionId":session_id})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send ordinary command");
        }
        let ordinary_started = gate
            .started
            .acquire_many(MAX_CONCURRENT_COMMANDS as u32)
            .await
            .expect("ordinary tasks started");
        ordinary_started.forget();

        socket
            .send(Message::Text(
                json!({"type":"code_review_close","id":"recovery","sessionId":session_id})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send recovery command");
        gate.started
            .acquire()
            .await
            .expect("recovery task started")
            .forget();

        for id in 0..3 {
            socket
                .send(Message::Text(
                    json!({"type":"code_review_close","id":format!("bypass-{id}"),"sessionId":session_id})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send bypass flood");
        }
        for id in 0..3 {
            let response = next_rpc_response(&mut socket).await;
            assert_eq!(response["id"], format!("bypass-{id}"));
            assert_eq!(
                response["error"],
                format!("too many concurrent RPC commands (limit {MAX_SPAWNED_COMMANDS})")
            );
            assert_eq!(response["data"]["code"], "busy_capacity");
        }
        assert_eq!(
            gate.started.available_permits(),
            0,
            "bypass flood spawned beyond the hard bound"
        );

        gate.release.add_permits(MAX_SPAWNED_COMMANDS);
        let mut saw_recovery = false;
        for _ in 0..MAX_SPAWNED_COMMANDS {
            let response = next_rpc_response(&mut socket).await;
            if response["id"] == "recovery" {
                saw_recovery = true;
                assert_ne!(
                    response["error"],
                    format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
                    "reserved recovery command was rejected under ordinary saturation"
                );
            }
        }
        assert!(saw_recovery, "reserved recovery command never completed");

        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_allowance_is_consumed_only_by_sessionless_get_state() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;

        // Sessionful commands before the bootstrap get_state must not spend
        // the one-shot allowance (BUG-5 reload-recovery sequence).
        socket
            .send(Message::Text(
                json!({"type":"subscribe","sessionId":session_id,"cursor":0})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send subscribe");
        let snapshot = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("snapshot response timeout")
                .expect("WebSocket closed before snapshot")
                .expect("read snapshot response")
            {
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Message::Text(text) => {
                    break serde_json::from_str::<serde_json::Value>(&text)
                        .expect("snapshot JSON");
                }
                Message::Close(frame) => panic!("connection closed before snapshot: {frame:?}"),
                _ => {}
            }
        };
        assert!(
            matches!(
                snapshot["type"].as_str(),
                Some("subscription_snapshot" | "session_resync")
            ),
            "unexpected snapshot response: {snapshot}"
        );
        assert_eq!(snapshot["sessionId"], session_id);

        socket
            .send(Message::Text(
                json!({"type":"session_list","id":"list","scope":"current","sessionId":session_id})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send session_list");
        let listed = next_rpc_response(&mut socket).await;
        assert_eq!(listed["id"], "list");
        assert_eq!(listed["success"], true, "{}", listed["error"]);
        let primary_path = listed["data"]["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .find(|row| row["sessionId"] == session_id)
            .expect("primary session row")
            .get("path")
            .and_then(Value::as_str)
            .expect("primary session path")
            .to_owned();

        let snapshots_before_switch = manager.protocol_snapshot_count_for_test();
        let dispatch_started = Instant::now();
        socket
            .send(Message::Text(
                json!({"type":"switch_session","id":"switch","sessionPath":primary_path,"sessionId":session_id,"responseMode":"subscribe-handoff-v1"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send switch_session");
        let switched = next_rpc_response(&mut socket).await;
        let dispatch_elapsed = dispatch_started.elapsed();
        eprintln!("warm switch WebSocket dispatch: {dispatch_elapsed:?}");
        assert!(dispatch_elapsed < Duration::from_secs(10));
        assert_eq!(switched["id"], "switch");
        assert_eq!(switched["success"], true, "{}", switched["error"]);
        assert_eq!(switched["sessionId"], session_id);
        assert_eq!(switched["data"]["sessionId"], session_id);
        assert_eq!(switched["data"]["subscriptionRequired"], true);
        assert_eq!(switched["data"]["lifecycleResponseVersion"], 1);
        assert!(switched["data"]["cursor"].is_u64());
        assert!(switched["data"].get("state").is_none());
        assert!(switched["data"].get("messages").is_none());
        assert_eq!(
            manager.protocol_snapshot_count_for_test(),
            snapshots_before_switch,
            "warm switch handoff must not capture a transcript snapshot",
        );
        manager.release_subscription_snapshots_for_test(1);

        socket
            .send(Message::Text(
                json!({
                    "type":"subscribe",
                    "sessionId":session_id,
                    "cursor":switched["data"]["cursor"],
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("subscribe after switch handoff");
        let authoritative = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("authoritative snapshot timeout")
                .expect("WebSocket closed before authoritative snapshot")
                .expect("read authoritative snapshot")
            {
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.expect("pong"),
                Message::Text(text) => {
                    let frame: Value = serde_json::from_str(&text).expect("snapshot JSON");
                    if matches!(
                        frame["type"].as_str(),
                        Some("subscription_snapshot" | "session_resync")
                    ) {
                        break frame;
                    }
                }
                Message::Close(frame) => panic!("closed before authoritative snapshot: {frame:?}"),
                _ => {}
            }
        };
        assert_eq!(authoritative["sessionId"], session_id);
        assert!(authoritative["messages"].is_array());
        assert_eq!(
            manager.protocol_snapshot_count_for_test(),
            snapshots_before_switch + 1,
            "switch plus subscribe must capture the transcript exactly once",
        );
        socket
            .send(Message::Text(
                json!({"type":"switch_session","id":"legacy-switch","sessionPath":primary_path,"sessionId":session_id})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send legacy switch_session");
        let legacy_switch = next_rpc_response(&mut socket).await;
        assert_eq!(legacy_switch["id"], "legacy-switch");
        assert_eq!(legacy_switch["success"], true, "{}", legacy_switch["error"]);
        assert!(legacy_switch["data"]["state"].is_object());
        assert!(legacy_switch["data"]["messages"].is_array());
        assert!(legacy_switch["data"].get("subscriptionRequired").is_none());
        // The first sessionless get_state still succeeds and consumes the
        // allowance (the normal first-get-state flow).
        socket
            .send(Message::Text(
                json!({"type":"get_state","id":"bootstrap"}).to_string().into(),
            ))
            .await
            .expect("send bootstrap get_state");
        let first = next_rpc_response(&mut socket).await;
        assert_eq!(first["id"], "bootstrap");
        assert_eq!(first["success"], true, "{}", first["error"]);
        assert_eq!(first["data"]["sessionId"], session_id);

        // A second sessionless get_state is rejected fail-closed.
        socket
            .send(Message::Text(
                json!({"type":"get_state","id":"second"}).to_string().into(),
            ))
            .await
            .expect("send second sessionless get_state");
        let second = next_rpc_response(&mut socket).await;
        assert_eq!(second["id"], "second");
        assert_eq!(second["success"], false);
        assert_eq!(
            second["error"],
            "sessionId is required after WebSocket bootstrap"
        );

        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        manager.shutdown().await;
    }

    /// Regression for the Chromium 1006 defect: a client-initiated clean
    /// close(1000) must be answered with the echoed close frame (Chromium
    /// then reports code 1000 / wasClean true), never a bare TCP FIN, which
    /// every conformant client classifies as an abnormal 1006 close.
    #[tokio::test]
    async fn client_initiated_close_receives_echoed_close_frame_before_eof() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;

        // Round-trip one RPC so the connection carries real traffic before
        // the close, matching the observed 1006 sequence.
        socket
            .send(Message::Text(
                json!({"type": "get_state", "id": "pre-close"}).to_string().into(),
            ))
            .await
            .expect("send pre-close rpc");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["id"], "pre-close");

        socket
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "client done".into(),
            })))
            .await
            .expect("send client close");
        // The server must complete the close handshake: the next frame the
        // client observes is the echoed close frame (code 1000), not EOF.
        let echoed = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("close echo timeout")
                .expect("socket readable before close echo")
                .expect("frame ok")
            {
                Message::Close(frame) => break frame,
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await.expect("pong")
                }
                other => panic!("unexpected frame after client close: {other:?}"),
            }
        };
        let echoed = echoed.expect("close frame payload");
        assert_eq!(echoed.code, CloseCode::Normal);
        assert_eq!(echoed.reason.as_str(), "client done");

        server.await.expect("server task").expect("server exit");
        manager.shutdown().await;
    }

    /// A second close from the client is ignored by the tungstenite state
    /// machine (`ClosedByPeer` -> ConnectionClosed) and must not wedge or
    /// double-close the connection.
    #[tokio::test]
    async fn repeated_client_close_still_exits_cleanly() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;

        socket
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "first".into(),
            })))
            .await
            .expect("send first close");
        // The peer-level echo already moves the client to CloseAcknowledged;
        // a stray second close from the application must stay harmless.
        let echoed = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("close echo timeout")
            .expect("socket readable")
            .expect("frame ok");
        assert!(matches!(echoed, Message::Close(_)), "{echoed:?}");
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "duplicate".into(),
            })))
            .await;

        server.await.expect("server task").expect("server exit");
        manager.shutdown().await;
    }

    async fn open_control_socket_with(
        state: ServerState,
        timeouts: WebSocketTimeouts,
    ) -> (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            websocket_connection(stream, raw, state, None, timeouts).await
        });
        let request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("client request");
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect control WebSocket");
        (socket, server)
    }

    /// Catalog auth proof: on a tokened listener the `session_list` catalog
    /// is reachable only through an authenticated request (unauthenticated
    /// and wrong-token POSTs fail closed with 401 and zero rows), and every
    /// row carries exact identity including the primary's session id.
    #[tokio::test]
    async fn session_list_catalog_requires_authenticated_socket_and_rows_exact_identity() {
        let mut state = ws_server_state().await;
        let manager = state.manager.clone();
        let session_id = manager.primary_session_id();
        state.token = Some(Arc::from(b"listener-secret".as_slice()));
        let body = json!({"type":"session_list","scope":"current"}).to_string();
        let post = |authorization: Option<&str>| {
            let mut request = format!(
                "POST /rpc HTTP/1.1\r\nhost: listener.example\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
                body.len(),
            );
            if let Some(authorization) = authorization {
                request.push_str(&format!("authorization: {authorization}\r\n"));
            }
            request.push_str("\r\n");
            request.push_str(&body);
            request
        };
        async fn dispatch(state: ServerState, request: String) -> String {
            let (mut client, server) = tokio::io::duplex(64 * 1024);
            let task = tokio::spawn(async move { handle_connection(server, state).await });
            client
                .write_all(request.as_bytes())
                .await
                .expect("write request");
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.expect("read response");
            task.await
                .expect("connection task")
                .expect("connection handled");
            String::from_utf8_lossy(&response).into_owned()
        }

        let refused = dispatch(state.clone(), post(None)).await;
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        assert!(
            !refused.contains("sessions"),
            "unauthenticated catalog must not leak rows: {refused}"
        );

        let wrong = dispatch(state.clone(), post(Some("Bearer not-the-token"))).await;
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
        assert!(
            !wrong.contains("sessions"),
            "wrong-token catalog must not leak rows: {wrong}"
        );

        let authorized_body = dispatch(state.clone(), post(Some("Bearer listener-secret"))).await;
        assert!(authorized_body.starts_with("HTTP/1.1 200"), "{authorized_body}");
        let payload: Value = serde_json::from_str(
            authorized_body
                .split("\r\n\r\n")
                .nth(1)
                .expect("response body"),
        )
        .expect("JSON body");
        assert_eq!(payload["success"], true, "{payload}");
        let rows = payload["data"]["sessions"]
            .as_array()
            .expect("sessions array");
        for row in rows {
            assert!(
                row["sessionId"].as_str().is_some_and(|id| !id.is_empty()),
                "row identity must be an exact nonempty string: {row}"
            );
            assert!(
                row["path"].as_str().is_some_and(|path| !path.is_empty()),
                "row identity path: {row}"
            );
        }
        let primary = rows
            .iter()
            .find(|row| row["sessionId"].as_str() == Some(session_id.as_str()))
            .expect("primary catalog row");
        assert_eq!(primary["source"], "rpi");
        assert_eq!(primary["status"], "native");
        assert!(
            primary["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()),
            "{primary}"
        );
        manager.shutdown().await;
    }

    fn sample_discovered_runner() -> crate::team::DiscoveredRunner {
        crate::team::DiscoveredRunner {
            origin: "wss://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: "rpi-host-a".to_owned(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "wss".to_owned(),
            codename: Some("dev-box".to_owned()),
            last_seen_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis() as u64),
            failed_probes: 0,
            failed_since_ms: None,
        }
    }

    #[tokio::test]
    async fn get_discovered_runners_returns_http_origins_without_tokens() {
        let mut state = ws_server_state().await;
        let manager = state.manager.clone();
        let view = crate::team::DiscoveryView::for_test();
        view.upsert_runner(sample_discovered_runner());
        let mut codenameless = sample_discovered_runner();
        codenameless.origin = "wss://192.0.2.11:8766".to_owned();
        codenameless.host = "192.0.2.11".to_owned();
        codenameless.port = 8766;
        codenameless.instance = "rpi-host-b".to_owned();
        codenameless.codename = None;
        view.upsert_runner(codenameless);
        state.discovery = Some(view);
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;
        socket
            .send(Message::Text(
                json!({"type":"get_discovered_runners","id":"disc-1"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send discovery rpc");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["command"], "get_discovered_runners");
        assert_eq!(response["success"], true);
        let runners = response["data"]["runners"]
            .as_array()
            .expect("runners array");
        assert_eq!(runners.len(), 2);
        assert_eq!(runners[0]["origin"], "https://192.0.2.10:8765");
        assert_eq!(runners[0]["host"], "192.0.2.10");
        assert_eq!(runners[0]["port"], 8765);
        assert_eq!(runners[0]["label"], "dev box");
        assert_eq!(runners[0]["version"], "0.2.11");
        assert_eq!(runners[0]["scheme"], "wss");
        assert_eq!(runners[0]["codename"], "dev-box");
        let codenameless = runners
            .iter()
            .find(|row| row["host"] == "192.0.2.11")
            .expect("codenameless row");
        assert!(
            codenameless.get("codename").is_some_and(Value::is_null),
            "discovery rows expose an explicit null codename: {codenameless}"
        );
        let encoded = response.to_string();
        assert!(
            !encoded.contains("token"),
            "discovery payload must never carry tokens: {encoded}"
        );
        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_list_sessions_ws_is_machine_scoped_without_catalog_rows() {
        if !tmux_binary_available() {
            return;
        }
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;
        socket
            .send(Message::Text(
                json!({"type":"tmux_list_sessions","id":"t1"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send tmux_list_sessions");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["id"], "t1");
        assert_tmux_list_sessions_payload(&response);
        assert!(response.get("sessionId").is_none(), "{response}");

        socket
            .send(Message::Text(
                json!({
                    "type":"tmux_list_sessions",
                    "id":"t2",
                    "sessionId":"bogus-session-id"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send tmux_list_sessions with sessionId");
        let tagged = next_rpc_response(&mut socket).await;
        assert_eq!(tagged["id"], "t2");
        assert_tmux_list_sessions_payload(&tagged);
        assert_eq!(tagged["sessionId"], "bogus-session-id");

        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_terminal_ws_close_drops_attached_client() {
        if !tmux_binary_available() {
            return;
        }
        let name = format!(
            "pi-ws-term-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let selector = format!("={name}");
        let created = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-x", "80", "-y", "24", "-s", &name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("new-session");
        assert!(created.success(), "tmux new-session {name}");
        let state = ws_server_state().await;
        let terminals = state.tmux_terminals.clone();
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;
        socket
            .send(Message::Text(
                json!({
                    "type":"tmux_terminal_attach",
                    "id":"a1",
                    "session": name,
                    "cols":80,
                    "rows":24
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send attach");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(response["data"]["session"], name);
        assert_eq!(terminals.live_count(), 1);
        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if terminals.live_count() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("ws close left live terminals");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let still = std::process::Command::new("tmux")
            .args(["has-session", "-t", &selector])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("has-session");
        assert!(still.success(), "tmux session must survive ws close");
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &selector])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_terminal_ws_attach_response_precedes_output() {
        if !tmux_binary_available() {
            return;
        }
        let name = format!(
            "pi-ws-term-order-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let selector = format!("={name}");
        let created = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-x", "80", "-y", "24", "-s", &name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("new-session");
        assert!(created.success(), "tmux new-session {name}");
        let state = ws_server_state().await;
        let terminals = state.tmux_terminals.clone();
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;
        socket
            .send(Message::Text(
                json!({
                    "type":"tmux_terminal_attach",
                    "id":"a-order",
                    "session": name,
                    "cols":80,
                    "rows":24
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send attach");
        let mut saw_attach = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for attach response");
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, socket.next())
                .await
                .expect("attach ordering timeout")
                .expect("WebSocket closed before attach response")
                .expect("read attach ordering frame")
            {
                Message::Ping(payload) => socket
                    .send(Message::Pong(payload))
                    .await
                    .expect("send pong"),
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text).expect("JSON frame");
                    let kind = value["type"].as_str();
                    if kind == Some("tmux_terminal_output")
                        || kind == Some("tmux_terminal_exit")
                        || kind == Some("tmux_terminal_history")
                    {
                        panic!("tmux event overtook attach response: {value}");
                    }
                    if kind == Some("response") && value["id"] == "a-order" {
                        assert_eq!(value["success"], true, "{value}");
                        saw_attach = true;
                        break;
                    }
                }
                Message::Close(frame) => panic!("connection closed before attach response: {frame:?}"),
                _ => {}
            }
        }
        assert!(saw_attach);
        let mut saw_output = false;
        let output_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < output_deadline {
            match tokio::time::timeout(Duration::from_millis(500), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket.send(Message::Pong(payload)).await.expect("pong");
                }
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: Value = serde_json::from_str(&text).expect("JSON frame");
                    if value["type"] == "tmux_terminal_output" {
                        saw_output = true;
                        break;
                    }
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("closed before output: {frame:?}");
                }
                _ => {}
            }
        }
        assert!(saw_output, "expected tmux_terminal_output after attach response");
        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if terminals.live_count() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("ws close left live terminals");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &selector])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_terminal_forwarder_failure_cleans_terminal() {
        if !tmux_binary_available() {
            return;
        }
        let name = format!(
            "pi-ws-term-slow-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let selector = format!("={name}");
        let created = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-x", "80", "-y", "24", "-s", &name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("new-session");
        assert!(created.success(), "tmux new-session {name}");
        let state = ws_server_state().await;
        let terminals = state.tmux_terminals.clone();
        let manager = state.manager.clone();
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
            slow_client_grace: Duration::from_millis(200),
        };
        let (mut socket, server) = open_control_socket_with(state, timeouts).await;
        socket
            .send(Message::Text(
                json!({
                    "type":"tmux_terminal_attach",
                    "id":"a-slow",
                    "session": name,
                    "cols":80,
                    "rows":24
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send attach");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(terminals.live_count(), 1);
        let terminal_id = response["data"]["terminalId"]
            .as_str()
            .expect("terminalId")
            .to_owned();
        let payload = base64::engine::general_purpose::STANDARD.encode("x".repeat(4096));
        let flood = tokio::spawn(async move {
            for i in 0..80 {
                let frame = json!({
                    "type":"tmux_terminal_input",
                    "id": format!("in-{i}"),
                    "terminalId": terminal_id,
                    "data": payload
                });
                let send = socket.send(Message::Text(frame.to_string().into()));
                if tokio::time::timeout(Duration::from_millis(100), send)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            drop(socket);
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if terminals.live_count() == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("forwarder failure left live terminals");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), flood).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &selector])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_terminal_ws_input_preserves_receive_order() {
        if !tmux_binary_available() {
            return;
        }
        let name = format!(
            "pi-ws-term-order-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        );
        let selector = format!("={name}");
        // A login shell inherits the operator prompt and can redraw slowly
        // enough to coalesce later input into an earlier line. The order
        // check needs a shell that executes each line as it arrives.
        let created = std::process::Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "24",
                "-s",
                &name,
                "exec env -i PATH=/usr/bin:/bin TERM=dumb HOME=/tmp PS1='$ ' bash --noprofile --norc",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("new-session");
        assert!(created.success(), "tmux new-session {name}");
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
            slow_client_grace: Duration::from_secs(2),
        };
        let (mut socket, server) = open_control_socket_with(state, timeouts).await;
        socket
            .send(Message::Text(
                json!({
                    "type":"tmux_terminal_attach",
                    "id":"a-order",
                    "session": name,
                    "cols":80,
                    "rows":24
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send attach");
        let attached = next_rpc_response(&mut socket).await;
        assert_eq!(attached["success"], true, "{attached}");
        let terminal_id = attached["data"]["terminalId"]
            .as_str()
            .expect("terminalId")
            .to_owned();
        let enter = base64::engine::general_purpose::STANDARD.encode("\r");
        for i in 0..200u32 {
            let data = if i % 2 == 0 {
                base64::engine::general_purpose::STANDARD.encode(format!("printf MARK{:03}", i / 2))
            } else {
                enter.clone()
            };
            socket
                .send(Message::Text(
                    json!({
                        "type":"tmux_terminal_input",
                        "id": format!("in-{i}"),
                        "terminalId": terminal_id,
                        "data": data
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send input");
        }
        let mut seen = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10000);
        while seen < 200 {
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out collecting input responses seen={seen}");
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
                    if value["type"] != "response" {
                        continue;
                    }
                    assert_eq!(value["success"], true, "{value}");
                    assert_eq!(
                        value["id"].as_str().unwrap_or(""),
                        format!("in-{seen}"),
                        "input response out of WS order {value}"
                    );
                    seen += 1;
                }
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket.send(Message::Pong(payload)).await.expect("pong");
                }
                other => panic!("input response stream ended: {other:?}"),
            }
        }
        let wait_pane = Instant::now() + Duration::from_millis(10000);
        let mut last = String::new();
        loop {
            match crate::modes::tmux::pane_snapshot(&name, Some(500)) {
                Ok(snap) => {
                    last = snap.content;
                    let mut marks = Vec::new();
                    for i in 0..100u32 {
                        let mark = format!("MARK{i:03}");
                        if let Some(at) = last.find(&mark) {
                            marks.push((i, at));
                        }
                    }
                    if marks.len() >= 8
                        && marks.windows(2).all(|pair| {
                            pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1
                        })
                    {
                        break;
                    }
                }
                Err(error) => last = error,
            }
            if Instant::now() >= wait_pane {
                let sample: Vec<&str> = last
                    .lines()
                    .filter(|line| line.contains("MARK"))
                    .take(12)
                    .collect();
                panic!("ordered input missing from pane sample={sample:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        socket.close(None).await.expect("close");
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &selector])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_terminal_detach_accepted_when_ordinary_slots_saturated() {
        let mut state = ws_server_state().await;
        let manager = state.manager.clone();
        let session_id = manager.primary_session_id();
        let gate = Arc::new(CommandTaskGate::default());
        state.command_task_gate = Some(gate.clone());
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_secs(10),
                idle: Duration::from_secs(10),
                slow_client_grace: Duration::from_secs(2),
            },
        )
        .await;
        for id in 0..MAX_CONCURRENT_COMMANDS {
            socket
                .send(Message::Text(
                    json!({"type":"get_state","id":format!("ordinary-{id}"),"sessionId":session_id})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send ordinary command");
        }
        gate.started
            .acquire_many(MAX_CONCURRENT_COMMANDS as u32)
            .await
            .expect("ordinary tasks started")
            .forget();
        socket
            .send(Message::Text(
                json!({
                    "type":"tmux_terminal_detach",
                    "id":"detach-sat",
                    "terminalId":"missing-terminal"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send detach");
        let response = next_rpc_response(&mut socket).await;
        assert_eq!(response["id"], "detach-sat");
        assert_eq!(response["command"], "tmux_terminal_detach");
        assert_ne!(
            response["error"],
            format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
            "{response}"
        );
        assert_ne!(
            response["error"],
            format!("too many concurrent RPC commands (limit {MAX_SPAWNED_COMMANDS})"),
            "{response}"
        );
        gate.release.add_permits(MAX_CONCURRENT_COMMANDS);
        socket.close(None).await.expect("close");
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
        manager.shutdown().await;
    }

    fn tmux_binary_available() -> bool {
        std::process::Command::new("tmux")
            .arg("-V")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn assert_tmux_list_sessions_payload(response: &Value) {
        assert_eq!(response["command"], "tmux_list_sessions");
        assert_eq!(response["success"], true, "{response}");
        let sessions = response["data"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("sessions array: {response}"));
        for row in sessions {
            let obj = row
                .as_object()
                .unwrap_or_else(|| panic!("session row object: {row}"));
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["attached", "createdEpoch", "name", "paneCount", "windows"],
                "{row}"
            );
            assert!(row.get("sessionId").is_none(), "{row}");
            assert!(row.get("runtimeIncarnation").is_none(), "{row}");
            assert!(row.get("path").is_none(), "{row}");
        }
    }

    #[tokio::test]
    async fn websocket_pushes_runner_discovered_and_gone() {
        let mut state = ws_server_state().await;
        let manager = state.manager.clone();
        let view = crate::team::DiscoveryView::for_test();
        state.discovery = Some(view.clone());
        let (mut socket, server) = open_control_socket_with(state, DEFAULT_WS_TIMEOUTS).await;
        view.upsert_runner(sample_discovered_runner());
        let discovered = loop {
            let value = next_rpc_response(&mut socket).await;
            if value["type"] == "runner_discovered" {
                break value;
            }
        };
        assert_eq!(discovered["origin"], "https://192.0.2.10:8765");
        assert_eq!(discovered["codename"], "dev-box");
        assert_eq!(discovered["scheme"], "wss");
        assert_eq!(discovered["label"], "dev box");
        view.remove_origin("wss://192.0.2.10:8765");
        let gone = loop {
            let value = next_rpc_response(&mut socket).await;
            if value["type"] == "runner_gone" {
                break value;
            }
        };
        assert_eq!(gone["origin"], "wss://192.0.2.10:8765");
        socket.close(None).await.expect("close client");
        server
            .await
            .expect("server task")
            .expect("clean server exit");
        manager.shutdown().await;
    }

    #[test]
    fn discovery_event_json_maps_wss_to_https_origin() {
        let event = crate::team::DiscoveryEvent::RunnerDiscovered {
            origin: "wss://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            label: "dev box".to_owned(),
            version: Some("1.0.0".to_owned()),
            scheme: "wss".to_owned(),
            codename: Some("dev-box".to_owned()),
        };
        let value = discovery_event_json(&event);
        assert_eq!(value["type"], "runner_discovered");
        assert_eq!(value["origin"], "https://192.0.2.10:8765");
        assert_eq!(value["wsOrigin"], "wss://192.0.2.10:8765");
        assert_eq!(value["codename"], "dev-box");
    }

    #[test]
    fn discovery_event_json_exposes_null_codename_when_absent() {
        let event = crate::team::DiscoveryEvent::RunnerDiscovered {
            origin: "ws://192.0.2.11:8766".to_owned(),
            host: "192.0.2.11".to_owned(),
            port: 8766,
            label: "anon box".to_owned(),
            version: None,
            scheme: "ws".to_owned(),
            codename: None,
        };
        let value = discovery_event_json(&event);
        assert_eq!(value["type"], "runner_discovered");
        assert!(
            value.get("codename").is_some_and(Value::is_null),
            "discovery events expose an explicit null codename: {value}"
        );
    }

    #[test]
    fn runner_codename_resolution_forwards_override_and_stored_identity() {
        let home = tempfile::tempdir().expect("temp agent dir");
        // The override is what advertise receives verbatim.
        let codename = resolve_runner_codename(home.path(), Some("codename-one"), 8765)
            .expect("override resolves");
        assert_eq!(codename, "codename-one");
        // The stored codename survives port churn: a later advertise call
        // resolves exactly the name the earlier advertise published.
        let reused = resolve_runner_codename(home.path(), None, 8766).expect("stored identity");
        assert_eq!(reused, "codename-one");
    }

    #[test]
    fn runner_codename_resolution_is_nonfatal_on_invalid_override() {
        let home = tempfile::tempdir().expect("temp agent dir");
        assert_eq!(
            resolve_runner_codename(home.path(), Some("Not A Valid Codename"), 8765),
            None,
            "invalid override downgrades to None, never bails startup"
        );
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

    #[tokio::test]
    async fn tls_acceptor_accepts_explicit_pair_and_rejects_mixed_pair() {
        let directory = tempfile::tempdir().expect("certificate directory");
        let cert_path = directory.path().join("cert.pem");
        let key_path = directory.path().join("key.pem");
        let params =
            CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("certificate parameters");
        let signing_key = KeyPair::generate().expect("generate signing key");
        let certificate = params
            .self_signed(&signing_key)
            .expect("generate certificate");
        std::fs::write(&cert_path, certificate.pem()).expect("write certificate");
        std::fs::write(&key_path, signing_key.serialize_pem()).expect("write key");
        let bind = "127.0.0.1:0".parse().unwrap();

        make_tls_acceptor(Some(&cert_path), Some(&key_path), bind)
            .await
            .expect("explicit certificate pair must build an acceptor");
        for (cert, key) in [
            (Some(cert_path.as_path()), None),
            (None, Some(key_path.as_path())),
        ] {
            let error = match make_tls_acceptor(cert, key, bind).await {
                Ok(_) => panic!("mixed certificate pair must be rejected"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("--listen-cert and --listen-key must be provided together")
            );
        }
    }

    fn headers(input: &[u8]) -> std::result::Result<RawRequest, RequestError> {
        let end = find_header_end(input).expect("header terminator");
        parse_request_headers(&input[..end], input[end + 4..].to_vec())
    }

    #[test]
    fn auth_policy_keeps_loopback_open_and_requires_remote_tls_token() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token-file");
        std::fs::write(&token, b"fixture-value").unwrap();
        // Loopback is always permitted, tokenless or tokenized, under every
        // policy: the developer experience stays tokenless on the local host.
        for address in ["127.0.0.1", "::1"] {
            let address = address.parse().unwrap();
            for policy in [
                ListenAddressPolicy::LoopbackOnly,
                ListenAddressPolicy::AllowInsecureRemote,
                ListenAddressPolicy::AllowTlsRemote,
            ] {
                assert!(
                    load_auth_token(address, None, "--listen", policy)
                        .unwrap()
                        .is_none(),
                    "tokenless loopback must remain available under {policy:?}"
                );
                assert_eq!(
                    load_auth_token(address, Some(&token), "--listen", policy).unwrap(),
                    Some(b"fixture-value".to_vec()),
                    "token-authenticated loopback must remain available under {policy:?}"
                );
            }
        }
        for address in ["0.0.0.0", "::", "198.51.100.7", "8.8.8.8"] {
            let address = address.parse().unwrap();
            // LoopbackOnly refuses every non-loopback bind regardless of a
            // token: the policy is address-scoped, not token-gated.
            assert!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::LoopbackOnly
                )
                .is_err(),
                "LoopbackOnly must refuse a tokenized non-loopback bind"
            );
            // The explicit insecure-remote opt-in permits non-loopback binds
            // with a token (recommended) or without one, regardless of TLS.
            assert_eq!(
                load_auth_token(
                    address,
                    None,
                    "--listen",
                    ListenAddressPolicy::AllowInsecureRemote
                )
                .unwrap(),
                None
            );
            assert_eq!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::AllowInsecureRemote
                )
                .unwrap(),
                Some(b"fixture-value".to_vec())
            );
            // TLS is not plaintext, so a non-loopback TLS bind needs no
            // insecure-remote opt-in — but it requires a token: a tokenless
            // remote TLS listener would accept unauthenticated control-plane
            // commands from any network client, so the pre-bind guard fails
            // closed with a --listen-token-file hint (no token contents leak).
            match load_auth_token(
                address,
                None,
                "--listen",
                ListenAddressPolicy::AllowTlsRemote,
            ) {
                Ok(_) => panic!("tokenless non-loopback TLS must be rejected pre-bind"),
                Err(error) => {
                    let message = format!("{error:#}");
                    assert!(
                        message.contains(address.to_string().as_str()),
                        "TLS refusal must name the non-loopback address: {message}"
                    );
                    assert!(
                        message.contains("--listen-token-file"),
                        "TLS refusal must hint at --listen-token-file: {message}"
                    );
                    assert!(
                        !message.contains("fixture-value"),
                        "TLS refusal must not leak token contents: {message}"
                    );
                }
            }
            assert_eq!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::AllowTlsRemote
                )
                .unwrap(),
                Some(b"fixture-value".to_vec()),
                "AllowTlsRemote must load the token for a non-loopback bind"
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
        let parsed =
            headers(b"POST /rpc HTTP/1.1\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\n")
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
        assert!(authorized(&headers, Some(b"secret"), false));
        assert!(!authorized(&headers, Some(b"wrong"), false));
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic secret"),
        );
        assert!(!authorized(&headers, Some(b"secret"), false));
    }

    #[test]
    fn tokenless_listener_accepts_same_origin_browser_matching_host() {
        // Native clients (no Origin) are always allowed tokenless.
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, None, true));
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer ignored-without-token-policy"),
        );
        assert!(authorized(&headers, None, true));
        // A browser whose `http://` Origin authority matches the request's
        // Host connects; an unrelated cross-origin page does not.
        headers.insert(
            http::header::HOST,
            HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(authorized(&headers, None, true));
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!authorized(&headers, None, true));
        // Strict transport (ACP): every tokenless browser is rejected even
        // with a matching Host; natives still pass.
        let mut native = HeaderMap::new();
        assert!(authorized(&native, None, false));
        let mut browser = HeaderMap::new();
        browser.insert(
            http::header::HOST,
            HeaderValue::from_static("127.0.0.1:8765"),
        );
        browser.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(!authorized(&browser, None, false));
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

        assert!(matches!(
            next_collab_message(&mut socket).await,
            Message::Text(_)
        ));
        assert!(matches!(
            next_collab_message(&mut socket).await,
            Message::Binary(_)
        ));
        service.stop(&room_id).await.expect("stop room");
        assert_away_close(next_collab_message(&mut socket).await);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn stalled_collab_handshake_releases_participant_lease() {
        let (service, room_id, connection) = test_collab_connection().await;
        assert_eq!(service.status(Some(&room_id)).await[0].participants, 1);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            // The buffered request never terminates, so the upgrade blocks on
            // the live socket; the handshake timeout is the only release for
            // this task and the participant lease it already holds.
            let raw = stalled_upgrade_request("/collab/ws/test-room");
            collab_websocket_connection(
                stream,
                raw,
                connection,
                "rpi-collab.test".to_owned(),
                WebSocketTimeouts {
                    handshake: Duration::from_millis(150),
                    ping_interval: Duration::from_secs(3600),
                    idle: Duration::from_secs(3600),
                    slow_client_grace: Duration::from_secs(5),
                },
            )
            .await
        });
        // A connected-but-silent client: the handshake cannot complete and
        // the client never closes, so the server-side timeout must fire.
        let _client = TcpStream::connect(address)
            .await
            .expect("connect stalled client");

        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stalled handshake must release the connection task")
            .expect("server task")
            .expect_err("stalled handshake must time out instead of completing");
        assert_eq!(
            service.status(Some(&room_id)).await[0].participants,
            0,
            "the stalled guest's participant lease must be released"
        );
    }

    #[tokio::test]
    async fn stalled_ws_handshake_releases_connection_task() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let raw = stalled_upgrade_request("/ws");
            websocket_connection(
                stream,
                raw,
                state,
                None,
                WebSocketTimeouts {
                    handshake: Duration::from_millis(150),
                    ping_interval: Duration::from_secs(3600),
                    idle: Duration::from_secs(3600),
                    slow_client_grace: Duration::from_secs(5),
                },
            )
            .await
        });
        let _client = TcpStream::connect(address)
            .await
            .expect("connect stalled client");

        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stalled /ws handshake must release the connection task")
            .expect("server task")
            .expect_err("stalled /ws handshake must time out instead of completing");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn silent_control_plane_client_is_evicted() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let (_socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_millis(20),
                idle: Duration::from_millis(120),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("silent control client must be evicted")
            .expect("server task")
            .expect("idle close is clean");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn responding_control_plane_client_survives_idle_windows() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_millis(20),
                idle: Duration::from_millis(300),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("responding client was evicted: {frame:?}")
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error while responding: {error}"),
                Ok(None) => panic!("responding client disconnected"),
                Err(_) => {}
            }
        }
        assert!(
            !server.is_finished(),
            "server must still serve a responsive client"
        );
        socket.close(None).await.expect("close responsive client");
        server
            .await
            .expect("server task")
            .expect("clean exit after client close");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn blocked_subscription_snapshot_keeps_control_plane_liveness() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let lifecycle = manager.hold_subscription_snapshot_for_test().await;
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_millis(20),
                idle: Duration::from_millis(300),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "subscribe",
                    "sessionId": session_id,
                    "cursor": 0,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send blocked subscription");

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let mut ping_count = 0usize;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    ping_count += 1;
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("snapshot-blocked client was evicted: {frame:?}");
                }
                Ok(Some(Ok(Message::Text(text)))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).expect("JSON frame");
                    assert_ne!(
                        value["type"], "subscription_snapshot",
                        "snapshot completed before gate release"
                    );
                }

                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error while snapshot was blocked: {error}"),
                Ok(None) => panic!("snapshot-blocked client disconnected"),
                Err(_) => {}
            }
        }
        assert!(
            ping_count >= 2,
            "blocked snapshot must not stop periodic pings"
        );
        assert!(
            !server.is_finished(),
            "blocked snapshot must not stop the connection task"
        );

        drop(lifecycle);
        let snapshot = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("snapshot response timeout")
                .expect("WebSocket closed before snapshot")
                .expect("read snapshot response")
            {
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Message::Text(text) => {
                    break serde_json::from_str::<serde_json::Value>(&text).expect("snapshot JSON");
                }
                Message::Close(frame) => panic!("connection closed before snapshot: {frame:?}"),
                _ => {}
            }
        };
        assert!(
            matches!(
                snapshot["type"].as_str(),
                Some("subscription_snapshot" | "session_resync")
            ),
            "unexpected snapshot response: {snapshot}"
        );
        assert_eq!(snapshot["sessionId"], session_id);

        socket.close(None).await.expect("close snapshot client");
        server
            .await
            .expect("server task")
            .expect("clean exit after client close");
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn snapshot_window_event_is_delivered_once_after_snapshot() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let delivery = manager.hold_subscription_snapshot_delivery_for_test().await;
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_secs(1),
                idle: Duration::from_secs(5),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        socket
            .send(Message::Text(
                json!({
                    "type": "subscribe", "sessionId": session_id, "cursor": 0,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("subscribe");
        manager
            .wait_for_subscription_snapshot_capture_for_test()
            .await;

        let live = manager
            .publish_test_event(
                &session_id,
                pi_coding::ApplicationEvent::TurnStarted {
                    turn_id: "snapshot-window-turn".to_owned(),
                    model: None,
                },
            )
            .await;
        drop(delivery);

        let mut frames = Vec::new();
        while frames.len() < 2 {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("snapshot-window frame timeout")
                .expect("socket open")
                .expect("frame")
            {
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.expect("pong"),
                Message::Text(text) => {
                    frames.push(serde_json::from_str::<Value>(&text).expect("JSON frame"))
                }
                Message::Close(frame) => {
                    panic!("closed before snapshot-window delivery: {frame:?}")
                }
                _ => {}
            }
        }
        assert!(
            matches!(
                frames[0]["type"].as_str(),
                Some("subscription_snapshot" | "session_resync")
            ),
            "first frame: {}",
            frames[0]
        );
        assert_eq!(frames[1]["type"], live["type"]);
        assert_eq!(frames[1]["turnId"], "snapshot-window-turn");
        assert_eq!(frames[1]["sequence"].as_u64(), live["sequence"].as_u64());
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["turnId"] == "snapshot-window-turn")
                .count(),
            1
        );

        socket.close(None).await.expect("close");
        server.await.expect("server task").expect("server exit");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_unloaded_frame_is_delivered_once_and_prunes_exact_subscription() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let delivery = manager.hold_subscription_snapshot_delivery_for_test().await;
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_secs(10),
                idle: Duration::from_secs(10),
                slow_client_grace: Duration::from_secs(2),
            },
        )
        .await;
        socket
            .send(Message::Text(
                json!({
                    "type": "subscribe", "sessionId": session_id, "cursor": 0,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("subscribe");
        manager
            .wait_for_subscription_snapshot_capture_for_test()
            .await;

        // A live event lands in the snapshot window while the snapshot is
        // held, so the prune must also discard the window (no replay).
        manager
            .publish_test_event(
                &session_id,
                pi_coding::ApplicationEvent::TurnStarted {
                    turn_id: "unload-window-turn".to_owned(),
                    model: None,
                },
            )
            .await;

        // The runtime is unloaded: the server must forward the frame once
        // and prune the exact identity's subscription, window, pending
        // request, and active snapshot.
        let identity = manager
            .current_runtime_identity(&session_id)
            .await
            .expect("current identity");
        manager
            .events_tx()
            .send(json!({
                "type": "session_unloaded",
                "sessionId": identity.session_id,
                "runtimeIncarnation": identity.runtime_incarnation,
                "reason": "closed",
            }))
            .expect("send session_unloaded");

        // The unload frame is delivered exactly once, while the snapshot is
        // still held, so the prune provably precedes snapshot completion.
        let unload = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("unload frame timeout")
                .expect("socket open")
                .expect("frame")
            {
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.expect("pong"),
                Message::Text(text) => {
                    break serde_json::from_str::<Value>(&text).expect("JSON frame");
                }
                Message::Close(frame) => panic!("closed before session_unloaded: {frame:?}"),
                _ => {}
            }
        };
        assert_eq!(unload["type"], "session_unloaded");
        assert_eq!(unload["sessionId"], identity.session_id);
        assert_eq!(unload["runtimeIncarnation"], identity.runtime_incarnation);

        // Release the held snapshot: its completion must NOT resurrect the
        // pruned subscription (no snapshot frame, no window replay).
        drop(delivery);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), socket.next())
                .await
                .is_err(),
            "no frame may follow the unload: the snapshot must not resurrect the pruned subscription"
        );

        // A later event for the unloaded identity is no longer subscribed.
        manager
            .publish_test_event(
                &session_id,
                pi_coding::ApplicationEvent::TurnStarted {
                    turn_id: "unload-after-turn".to_owned(),
                    model: None,
                },
            )
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), socket.next())
                .await
                .is_err(),
            "events for the unloaded identity must not be delivered after the prune"
        );

        socket.close(None).await.expect("close");
        server.await.expect("server task").expect("server exit");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn snapshot_window_overflow_converges_without_missing_or_duplicate_events() {
        const EVENT_COUNT: u64 = 5;
        const MODEL_ID_BYTES: usize = 1024 * 1024;
        let mut state = ws_server_state().await;
        let manager = state.manager.clone();
        let observer = Arc::new(SnapshotOverflowObserver::default());
        state.snapshot_overflow_observer = Some(observer.clone());
        let delivery = manager.hold_subscription_snapshot_delivery_for_test().await;
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_secs(1),
                idle: Duration::from_secs(30),
                slow_client_grace: Duration::from_secs(30),
            },
        )
        .await;
        socket
            .send(Message::Text(
                json!({
                    "type": "subscribe",
                    "sessionId": session_id,
                    "cursor": 0,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("subscribe");
        manager
            .wait_for_subscription_snapshot_capture_for_test()
            .await;

        let first_sequence = manager
            .publish_test_event(
                &session_id,
                pi_coding::ApplicationEvent::TurnStarted {
                    turn_id: "snapshot-overflow-turn-1".to_owned(),
                    model: Some(pi_ai::Model {
                        id: "m".repeat(MODEL_ID_BYTES),
                        provider: "fixture".to_owned(),
                        ..pi_ai::Model::default()
                    }),
                },
            )
            .await["sequence"]
            .as_u64()
            .expect("first sequence");
        for identity in 2..=EVENT_COUNT {
            manager
                .publish_test_event(
                    &session_id,
                    pi_coding::ApplicationEvent::TurnStarted {
                        turn_id: format!("snapshot-overflow-turn-{identity}"),
                        model: Some(pi_ai::Model {
                            id: "m".repeat(MODEL_ID_BYTES),
                            provider: "fixture".to_owned(),
                            ..pi_ai::Model::default()
                        }),
                    },
                )
                .await;
        }
        tokio::time::timeout(Duration::from_secs(5), observer.wait())
            .await
            .expect("snapshot window overflow timeout");
        delivery.forget();
        manager.release_subscription_snapshots_for_test(2);

        let last_sequence = first_sequence + EVENT_COUNT - 1;
        let mut delivered = Vec::with_capacity(EVENT_COUNT as usize);
        let mut did_resubscribe = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while delivered.len() < EVENT_COUNT as usize {
            match tokio::time::timeout_at(deadline, socket.next())
                .await
                .expect("overflow convergence deadline")
                .expect("socket open")
                .expect("frame")
            {
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.expect("pong"),
                Message::Text(text) => {
                    let frame = serde_json::from_str::<Value>(&text).expect("JSON frame");
                    if frame["type"] == "session_resync_required" {
                        assert_eq!(frame["reason"], "overflow");
                        assert!(!did_resubscribe, "overflow requested more than one resync");
                        did_resubscribe = true;
                        socket
                            .send(Message::Text(
                                json!({
                                    "type": "subscribe",
                                    "sessionId": session_id,
                                    "runtimeIncarnation": frame["runtimeIncarnation"],
                                    "cursor": 0,
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .expect("resubscribe after overflow");
                    } else if matches!(
                        frame["type"].as_str(),
                        Some("subscription_snapshot" | "session_resync")
                    ) {
                        delivered.extend(
                            frame["events"]
                                .as_array()
                                .expect("snapshot events")
                                .iter()
                                .filter(|event| {
                                    event["turnId"].as_str().is_some_and(|turn_id| {
                                        turn_id.starts_with("snapshot-overflow-turn-")
                                    })
                                })
                                .cloned(),
                        );
                    } else if frame["turnId"]
                        .as_str()
                        .is_some_and(|turn_id| turn_id.starts_with("snapshot-overflow-turn-"))
                    {
                        delivered.push(frame);
                    }
                }
                Message::Close(frame) => panic!("closed before overflow convergence: {frame:?}"),
                _ => {}
            }
        }

        assert!(did_resubscribe, "overflow must request an exact resubscribe");
        let actual_sequences = delivered
            .iter()
            .map(|event| event["sequence"].as_u64().expect("event sequence"))
            .collect::<Vec<_>>();
        assert_eq!(
            actual_sequences,
            (first_sequence..=last_sequence).collect::<Vec<_>>()
        );
        let actual_identities = delivered
            .iter()
            .map(|event| event["turnId"].as_str().expect("turn identity").to_owned())
            .collect::<Vec<_>>();
        let expected_identities = (1..=EVENT_COUNT)
            .map(|identity| format!("snapshot-overflow-turn-{identity}"))
            .collect::<Vec<_>>();
        assert_eq!(actual_identities, expected_identities);

        socket.close(None).await.expect("close");
        server.await.expect("server task").expect("server exit");
        manager.shutdown().await;
    }

    async fn send_control_command(
        socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        command: Value,
    ) -> Value {
        socket
            .send(Message::Text(command.to_string().into()))
            .await
            .expect("send command");
        loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("control response timeout")
                .expect("socket open")
                .expect("frame")
            {
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.expect("pong"),
                Message::Text(text) => {
                    let response = serde_json::from_str::<Value>(&text).expect("JSON response");
                    if response["type"] == "response" {
                        return response;
                    }
                }
                Message::Close(frame) => panic!("closed before control response: {frame:?}"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn control_websocket_collaboration_start_status_stop() {
        let mut state = ws_server_state().await;
        state.base_url = Some("http://127.0.0.1:4321".to_owned());
        let manager = state.manager.clone();
        let session_id = manager.primary_session_id();
        let (mut socket, server) = open_control_socket_with(
            state,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_secs(1),
                idle: Duration::from_secs(5),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        let started = send_control_command(
            &mut socket,
            json!({
                "type": "collab_start", "id": "start", "sessionId": session_id,
            }),
        )
        .await;
        assert_eq!(started["success"], true, "{started}");
        let room_id = started["data"]["roomId"]
            .as_str()
            .expect("room id")
            .to_owned();

        let status = send_control_command(
            &mut socket,
            json!({
                "type": "collab_status", "id": "status", "sessionId": session_id, "roomId": room_id,
            }),
        )
        .await;
        assert_eq!(status["success"], true, "{status}");
        assert_eq!(status["data"]["rooms"].as_array().expect("rooms").len(), 1);

        let stopped = send_control_command(
            &mut socket,
            json!({
                "type": "collab_stop", "id": "stop", "sessionId": session_id, "roomId": room_id,
            }),
        )
        .await;
        assert_eq!(stopped["success"], true, "{stopped}");
        assert_eq!(stopped["data"]["stopped"], true);

        socket.close(None).await.expect("close");
        server.await.expect("server task").expect("server exit");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn silent_collab_guest_is_evicted_and_releases_participant_lease() {
        let (service, room_id, connection) = test_collab_connection().await;
        let (mut socket, server) = open_collab_socket_with(
            connection,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_millis(20),
                idle: Duration::from_millis(120),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        // The normal handshake, hello, and snapshot are unaffected.
        assert!(matches!(
            next_collab_message(&mut socket).await,
            Message::Text(_)
        ));
        assert!(matches!(
            next_collab_message(&mut socket).await,
            Message::Binary(_)
        ));
        assert_eq!(service.status(Some(&room_id)).await[0].participants, 1);

        // The guest goes silent. A tungstenite client auto-pongs every Ping it
        // reads (RFC 6455 5.5.2: tungstenite queues the Pong inside `read` and
        // flushes it on the next poll), so polling `socket.next()` here would
        // itself keep the guest "alive" and starve the watchdog — the per-read
        // timeout never elapses because pings arrive faster than it. Instead
        // the client stops reading entirely, modeling a dead network peer, and
        // we first wait for the server's idle watchdog to evict it and release
        // the participant lease. The bound is a safety ceiling; eviction lands
        // at `idle` (120ms), so it is not a mask for a product bug.
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("silent guest must be evicted within the idle window")
            .expect("server task")
            .expect("idle close is a clean server exit");
        assert_eq!(
            service.status(Some(&room_id)).await[0].participants,
            0,
            "the silent guest's participant lease must be released after the idle timeout"
        );

        // The eviction Close was written before the server task returned, so
        // the guest now receives a Close or a disconnect within a bounded
        // window. The server side has exited and dropped the connection/lease,
        // so draining the buffered pings cannot keep the lease alive. Skip the
        // pings and observe how the guest terminates: a Close carries the
        // policy/idle reason, and a read error or EOF is equally valid — once
        // the server has closed, tungstenite's auto-pong for a buffered ping
        // flushes into a half-closed socket and surfaces as a BrokenPipe,
        // which is the disconnect half of "receives close or disconnects".
        // The capacity contract (lease released) was already asserted above;
        // the timeout is a ceiling only, never the eviction signal.
        loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
                Ok(Some(Ok(Message::Close(Some(frame))))) => {
                    assert_eq!(frame.code, CloseCode::Policy);
                    assert!(
                        frame.reason.contains("idle"),
                        "unexpected idle close reason: {}",
                        frame.reason
                    );
                    break;
                }
                Ok(Some(Ok(Message::Close(None)))) | Ok(None) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) => continue,
                Err(_) => {
                    panic!("silent guest was neither closed nor disconnected within the bound")
                }
            }
        }
    }

    #[tokio::test]
    async fn responding_collab_guest_survives_idle_windows() {
        let (service, room_id, connection) = test_collab_connection().await;
        let (mut socket, server) = open_collab_socket_with(
            connection,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                ping_interval: Duration::from_millis(20),
                idle: Duration::from_millis(300),
                slow_client_grace: Duration::from_secs(5),
            },
        )
        .await;
        assert!(matches!(
            next_collab_message(&mut socket).await,
            Message::Text(_)
        ));
        assert!(matches!(
            next_collab_message(&mut socket).await,
            Message::Binary(_)
        ));

        // A healthy browser never sends application frames unprompted, but it
        // answers server pings with pongs at the protocol level (RFC 6455
        // 5.5.2). A guest that keeps answering stays connected across many
        // idle windows instead of losing its slot.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .expect("send pong");
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("responding guest was evicted: {frame:?}");
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error while responding: {error}"),
                Ok(None) => panic!("responding guest was disconnected"),
                Err(_) => {}
            }
        }
        assert_eq!(
            service.status(Some(&room_id)).await[0].participants,
            1,
            "a guest that answers pings must keep its participant slot"
        );
        assert!(
            !server.is_finished(),
            "server must still be serving the guest"
        );

        // Clean teardown: closing the client releases the lease.
        drop(socket);
        server
            .await
            .expect("server task")
            .expect("clean exit after client close");
        assert_eq!(service.status(Some(&room_id)).await[0].participants, 0);
    }

    #[test]
    fn outbound_write_timeout_adds_one_second_per_mib() {
        assert_eq!(OUTBOUND_QUEUE_BYTE_BUDGET, MAX_WS_OUTBOUND_MESSAGE_BYTES);
        assert!(OUTBOUND_QUEUE_BYTE_BUDGET > 24 * 1024 * 1024);
        let grace = Duration::from_secs(5);
        assert_eq!(outbound_write_timeout(0, grace), grace);
        assert_eq!(outbound_write_timeout(1024 * 1024 - 1, grace), grace);
        assert_eq!(
            outbound_write_timeout(1024 * 1024, grace),
            Duration::from_secs(6)
        );
        assert_eq!(
            outbound_write_timeout(24 * 1024 * 1024, grace),
            Duration::from_secs(29)
        );
    }

    #[tokio::test]
    async fn enqueue_json_admits_get_messages_above_old_24_mib_cap() {
        let (messages, mut rx) = mpsc::channel::<OutboundMessage>(1);
        let sender = OutboundSender {
            messages,
            bytes: Arc::new(Semaphore::new(OUTBOUND_QUEUE_BYTE_BUDGET)),
            encoder: Arc::new(Semaphore::new(1)),
        };
        let payload = serde_json::json!({
            "type": "response",
            "command": "get_messages",
            "data": { "blob": "x".repeat(25 * 1024 * 1024) },
        });
        enqueue_json(&sender, &payload, Duration::from_secs(30))
            .await
            .expect("payload above the old 24 MiB inbound cap must enqueue");
        let outbound = rx.recv().await.expect("queued get_messages frame");
        let bytes = outbound_message_bytes(&outbound.message);
        assert!(
            bytes > 24 * 1024 * 1024,
            "enqueued frame must exceed old cap: {bytes}"
        );
        assert!(bytes <= OUTBOUND_QUEUE_BYTE_BUDGET);
    }

    #[test]
    fn outbound_write_buffer_includes_websocket_header_slack() {
        assert!(OUTBOUND_FRAME_HEADER_SLACK >= 14);
        assert_eq!(OUTBOUND_QUEUE_BYTE_BUDGET, MAX_WS_OUTBOUND_MESSAGE_BYTES);
    }

    #[tokio::test]
    async fn enqueue_json_waits_beyond_grace_while_large_frame_holds_bytes() {
        let (messages, _rx) = mpsc::channel::<OutboundMessage>(1);
        let bytes = Arc::new(Semaphore::new(OUTBOUND_QUEUE_BYTE_BUDGET));
        let sender = OutboundSender {
            messages,
            bytes: bytes.clone(),
            encoder: Arc::new(Semaphore::new(1)),
        };
        let held = bytes
            .clone()
            .acquire_many_owned((25 * 1024 * 1024) as u32)
            .await
            .expect("hold a large in-flight frame");
        let grace = Duration::from_secs(5);
        let enqueuer = tokio::spawn({
            let sender = sender.clone();
            async move { enqueue_json(&sender, &serde_json::json!({ "ok": true }), grace).await }
        });
        tokio::time::sleep(Duration::from_millis(5500)).await;
        assert!(
            !enqueuer.is_finished(),
            "full-budget reservation must wait for the large in-flight frame past the 5s grace"
        );
        drop(held);
        enqueuer
            .await
            .expect("enqueuer task")
            .expect("must enqueue after large-frame permits release");
    }

    #[test]
    fn collaboration_browser_path_accepts_only_valid_room_ids() {
        assert_eq!(
            collab_room_id("/collab/ws/room-123_abc"),
            Some("room-123_abc")
        );
        assert_eq!(collab_room_id("/collab/ws/"), None);
        assert_eq!(collab_room_id("/collab/ws/room/child"), None);
        assert_eq!(collab_room_id("/collab/ws/room?secret=x"), None);
        assert_eq!(collab_room_id("/collab/ws/room%23fragment"), None);
    }

    #[test]
    fn advertised_origin_parser_accepts_strict_origins_and_normalizes_root() {
        for (input, expected) in [
            ("http://127.0.0.1:8765", "http://127.0.0.1:8765"),
            ("http://127.0.0.1:8765/", "http://127.0.0.1:8765"),
            ("https://collab.example", "https://collab.example"),
            ("https://collab.example:8443", "https://collab.example:8443"),
            ("https://[2001:db8::1]:8443", "https://[2001:db8::1]:8443"),
            ("https://[2001:db8::1]", "https://[2001:db8::1]"),
        ] {
            assert_eq!(
                parse_advertised_origin(input).expect("valid origin"),
                expected,
                "origin {input:?}"
            );
        }
    }

    #[test]
    fn advertised_origin_parser_rejects_credentials_paths_queries_and_bad_hosts() {
        for bad in [
            "",
            "ftp://collab.example",
            "//collab.example",
            "http://",
            "http:///path",
            "http://host/path",
            "http://host//",
            "http://host/?x=1",
            "http://host?query=1",
            "http://host#fragment",
            "http://user:pass@host",
            "http://ho st",
            "http://host:",
            "http://host:0",
            "http://host:99999",
            "http://host:port",
            "http://:8080",
            "http://a:b:8080",
            "http://[::1",
            "http://[]:8080",
            "http://[nope]:8080",
            "http://[::1]extra",
        ] {
            assert!(
                parse_advertised_origin(bad).is_err(),
                "accepted non-origin {bad:?}"
            );
        }
    }

    #[test]
    fn mdns_advertise_host_skips_loopback_and_keeps_lan_binds() {
        let loopback: SocketAddr = "127.0.0.1:8765".parse().expect("loopback");
        assert_eq!(mdns_advertise_host(loopback), None);
        let ipv6_loopback: SocketAddr = "[::1]:8765".parse().expect("ipv6 loopback");
        assert_eq!(mdns_advertise_host(ipv6_loopback), None);
        let lan: SocketAddr = "192.0.2.10:8765".parse().expect("documentation lan");
        assert_eq!(mdns_advertise_host(lan).as_deref(), Some("192.0.2.10"));
    }

    #[test]
    fn wildcard_binds_fail_closed_without_advertised_origin_and_loopback_stays_automatic() {
        // IPv4 and IPv6 wildcards: no origin -> fail closed (None); a
        // configured origin is used verbatim in both cases, whatever the
        // transport scheme.
        for address in ["0.0.0.0:4321", "[::]:4321"] {
            let bind: SocketAddr = address.parse().expect("wildcard bind");
            assert!(bind.ip().is_unspecified(), "{address} must be a wildcard");
            assert_eq!(
                advertised_base_url(bind, None, false),
                None,
                "{address} must fail closed without an advertised origin"
            );
            assert_eq!(
                advertised_base_url(bind, Some("https://lan.example:8443"), true),
                Some("https://lan.example:8443".to_owned()),
                "{address} must use the configured origin"
            );
        }
        // Loopback stays automatic, both stacks: https by default, http with
        // the explicit plaintext opt-out.
        for address in ["127.0.0.1:4321", "[::1]:4321"] {
            let bind: SocketAddr = address.parse().expect("loopback bind");
            assert!(bind.ip().is_loopback(), "{address} must be loopback");
            assert_eq!(
                advertised_base_url(bind, None, true),
                Some(format!("https://{bind}")),
                "TLS loopback {address} must synthesize https from the bound address"
            );
            assert_eq!(
                advertised_base_url(bind, None, false),
                Some(format!("http://{bind}")),
                "plaintext loopback {address} must synthesize http from the bound address"
            );
        }
        // A specific non-loopback bind keeps synthesizing from the address.
        let bind: SocketAddr = "198.51.100.7:4321".parse().expect("specific bind");
        assert_eq!(
            advertised_base_url(bind, None, true),
            Some("https://198.51.100.7:4321".to_owned())
        );
        assert_eq!(
            advertised_base_url(bind, None, false),
            Some("http://198.51.100.7:4321".to_owned())
        );
    }

    #[test]
    fn display_web_url_prefers_explicit_origin_then_concrete_bind() {
        // An explicit advertised origin always wins, even over a concrete
        // bind, and is used verbatim (scheme comes from the origin itself).
        let bind: SocketAddr = "198.51.100.7:4321".parse().expect("concrete bind");
        assert_eq!(
            display_web_url(bind, Some("https://lan.example:8443"), true, None),
            Some("https://lan.example:8443".to_owned()),
            "explicit origin must win over a concrete bind"
        );
        // A concrete bind synthesizes from the bound address and ignores any
        // discovered IP — the bound address is already directly reachable.
        let discovered: IpAddr = "203.0.113.10".parse().unwrap();
        assert_eq!(
            display_web_url(bind, None, true, Some(discovered)),
            Some("https://198.51.100.7:4321".to_owned()),
            "concrete bind must synthesize https from the bound address"
        );
        assert_eq!(
            display_web_url(bind, None, false, None),
            Some("http://198.51.100.7:4321".to_owned()),
            "plaintext concrete bind must synthesize http"
        );
    }

    #[test]
    fn display_web_url_wildcard_uses_discovered_ipv4_lan_ip() {
        // IPv4 wildcard + a discovered IPv4 LAN address -> scheme://ip:port.
        // The discovered IP is in TEST-NET-3 documentation space so the
        // test never names a real private address.
        let bind: SocketAddr = "0.0.0.0:4321".parse().expect("wildcard bind");
        let discovered: IpAddr = "203.0.113.10".parse().unwrap();
        assert_eq!(
            display_web_url(bind, None, true, Some(discovered)),
            Some("https://203.0.113.10:4321".to_owned()),
            "TLS wildcard must use the discovered LAN IP with the bound port"
        );
        assert_eq!(
            display_web_url(bind, None, false, Some(discovered)),
            Some("http://203.0.113.10:4321".to_owned()),
            "plaintext wildcard must use http with the discovered LAN IP"
        );
    }

    #[test]
    fn display_web_url_wildcard_formats_discovered_ipv6_with_brackets() {
        // IPv6 wildcard + a discovered IPv6 LAN address -> bracketed host.
        // 2001:db8::/32 is the IPv6 documentation prefix (RFC 3849).
        let bind: SocketAddr = "[::]:4321".parse().expect("ipv6 wildcard bind");
        let discovered: IpAddr = "2001:db8::10".parse().unwrap();
        assert_eq!(
            display_web_url(bind, None, true, Some(discovered)),
            Some("https://[2001:db8::10]:4321".to_owned()),
            "discovered IPv6 address must be bracketed in the URL"
        );
    }

    #[test]
    fn display_web_url_rejects_loopback_unspecified_and_multicast_discovered_ips() {
        // A wildcard bind must never turn an unusable discovered address
        // into a URL; each must yield None so the banner falls back to text.
        let bind: SocketAddr = "0.0.0.0:4321".parse().expect("wildcard bind");
        for bad in [
            "0.0.0.0",   // unspecified — the wildcard itself
            "127.0.0.1", // IPv4 loopback
            "224.0.0.1", // IPv4 multicast
            "::",        // IPv6 unspecified
            "::1",       // IPv6 loopback
            "ff02::1",   // IPv6 multicast
        ] {
            let ip: IpAddr = bad.parse().expect("parse bad discovered ip");
            assert_eq!(
                display_web_url(bind, None, true, Some(ip)),
                None,
                "discovered {bad:?} must be rejected, not rendered into a URL"
            );
        }
    }

    #[test]
    fn display_web_url_wildcard_without_usable_ip_yields_none_for_text_fallback() {
        // No discovered IP at all (no route / offline): the banner prints a
        // textual fallback, never an unreachable 0.0.0.0/:: URL.
        let bind4: SocketAddr = "0.0.0.0:4321".parse().expect("ipv4 wildcard");
        assert_eq!(
            display_web_url(bind4, None, true, None),
            None,
            "ipv4 wildcard with no discovery must yield None for the fallback"
        );
        assert_eq!(
            display_web_url(bind4, None, false, None),
            None,
            "plaintext ipv4 wildcard with no discovery must yield None"
        );
        let bind6: SocketAddr = "[::]:4321".parse().expect("ipv6 wildcard");
        assert_eq!(
            display_web_url(bind6, None, true, None),
            None,
            "ipv6 wildcard with no discovery must yield None for the fallback"
        );
    }

    #[tokio::test]
    async fn collab_host_fails_closed_on_wildcard_without_origin_and_uses_configured_origin() {
        let service = CollabService::with_runtime(CollabTestRuntime::new());

        // Wildcard bind without an advertised origin: starting a room must
        // fail closed with an actionable error naming the flag — never a
        // synthesized 0.0.0.0/:: link.
        let host = crate::collab_commands::CollabHost::new(service.clone(), None);
        let error = host
            .execute(crate::interactive_commands::CollabInvocation::Start)
            .await
            .expect_err("wildcard without advertised origin must fail closed");
        let message = error.to_string();
        assert!(
            message.contains("--listen-advertised-origin"),
            "fail-closed error must name the flag: {message}"
        );
        assert!(
            !message.contains("http://0.0.0.0") && !message.contains("http://[::]"),
            "fail-closed error must not synthesize wildcard links: {message}"
        );

        // Configured reachable origin: both the control and the view-only
        // link are built from it.
        let host = crate::collab_commands::CollabHost::new(
            service,
            Some("https://lan.example:8443".to_owned()),
        );
        let output = host
            .execute(crate::interactive_commands::CollabInvocation::Start)
            .await
            .expect("start with configured advertised origin");
        let control = output
            .lines()
            .find(|line| line.starts_with("Control link: "))
            .expect("control link line");
        let view = output
            .lines()
            .find(|line| line.starts_with("View-only link: "))
            .expect("view link line");
        assert!(
            control.starts_with("Control link: https://lan.example:8443/collab/ws/"),
            "control link must use the advertised origin: {output}"
        );
        assert!(
            view.starts_with("View-only link: https://lan.example:8443/collab/ws/"),
            "view link must use the advertised origin: {output}"
        );
        assert!(
            control.contains("#c=") && view.contains("#v="),
            "links must carry the role fragments: {output}"
        );
        assert!(
            !output.contains("0.0.0.0") && !output.contains("[::]"),
            "links must never contain the wildcard bind: {output}"
        );
    }

    #[tokio::test]
    async fn outbound_enqueue_waits_within_grace_and_times_out_without_drain() {
        let sender = || {
            let (messages, receiver) =
                mpsc::channel::<OutboundMessage>(OUTBOUND_QUEUE_CAPACITY);
            (
                OutboundSender {
                    messages,
                    bytes: Arc::new(Semaphore::new(OUTBOUND_QUEUE_BYTE_BUDGET)),
                    encoder: Arc::new(Semaphore::new(1)),
                },
                receiver,
            )
        };

        let (tx, rx) = sender();
        for _ in 0..OUTBOUND_QUEUE_CAPACITY {
            enqueue_message(&tx, Message::Text("fill".into()), Duration::from_secs(5))
                .await
                .expect("prefill must fit");
        }
        let started = tokio::time::Instant::now();
        let error = enqueue_message(
            &tx,
            Message::Text("overflow".into()),
            Duration::from_millis(150),
        )
        .await
        .expect_err("an undrained full queue must exhaust the grace and fail");
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(format!("{error:#}").contains("queue is full"), "{error:#}");
        drop(rx);

        let (tx, mut rx) = sender();
        for _ in 0..OUTBOUND_QUEUE_CAPACITY {
            enqueue_message(&tx, Message::Text("fill".into()), Duration::from_secs(5))
                .await
                .expect("prefill must fit");
        }
        let enqueuer_tx = tx.clone();
        let enqueuer = tokio::spawn(async move {
            enqueue_message(
                &enqueuer_tx,
                Message::Text("overflow".into()),
                Duration::from_secs(5),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(matches!(
            rx.recv().await,
            Some(OutboundMessage { message: Message::Text(_), .. })
        ));
        enqueuer
            .await
            .expect("enqueuer task")
            .expect("a slot freed within the grace must admit the overflow losslessly");
        let mut received = Vec::new();
        for _ in 0..OUTBOUND_QUEUE_CAPACITY {
            received.push(rx.recv().await.expect("queued message"));
        }
        assert_eq!(received.len(), OUTBOUND_QUEUE_CAPACITY);
        assert_eq!(
            received
                .iter()
                .filter(|outbound| matches!(&outbound.message, Message::Text(text) if text.as_str() == "overflow"))
                .count(),
            1,
        );
    }
    #[tokio::test]
    async fn outbound_byte_reservation_is_held_until_writer_consumes_or_drops() {
        let (messages, mut receiver) = mpsc::channel::<OutboundMessage>(2);
        let sender = OutboundSender {
            messages,
            bytes: Arc::new(Semaphore::new(4)),
            encoder: Arc::new(Semaphore::new(1)),
        };
        enqueue_message(
            &sender,
            Message::Text("four".into()),
            Duration::from_secs(1),
        )
        .await
        .expect("first frame fits byte budget");
        let blocked = enqueue_message(
            &sender,
            Message::Text("x".into()),
            Duration::from_millis(20),
        )
        .await
        .expect_err("queued bytes retain their reservation");
        assert!(format!("{blocked:#}").contains("byte budget"));

        drop(receiver.recv().await.expect("queued frame"));
        enqueue_message(
            &sender,
            Message::Text("x".into()),
            Duration::from_secs(1),
        )
        .await
        .expect("dropping consumed frame releases bytes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_json_encoders_share_byte_budget_and_close_preempts_waiters() {
        #[derive(Clone)]
        struct ObservedJson {
            payload: Arc<str>,
            serializations: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl Serialize for ObservedJson {
            fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                self.serializations
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                serializer.serialize_str(&self.payload)
            }
        }

        let (messages, receiver) = mpsc::channel::<OutboundMessage>(8);
        let sender = OutboundSender {
            messages,
            bytes: Arc::new(Semaphore::new(OUTBOUND_QUEUE_BYTE_BUDGET)),
            encoder: Arc::new(Semaphore::new(1)),
        };
        const ENCODER_PAYLOAD_BYTES: usize = 1024 * 1024;
        let serializations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let value = ObservedJson {
            payload: Arc::from("x".repeat(ENCODER_PAYLOAD_BYTES)),
            serializations: serializations.clone(),
        };
        let mut tasks = JoinSet::new();
        for _ in 0..8 {
            let sender = sender.clone();
            let value = value.clone();
            tasks.spawn(async move {
                enqueue_json(&sender, &value, Duration::from_secs(30)).await
            });
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            while serializations.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one encoder reaches serialization");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            serializations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a queued encoded String must keep every other task before serialization",
        );
        let held_encoded_bytes = OUTBOUND_QUEUE_BYTE_BUDGET - sender.bytes.available_permits();
        assert!(held_encoded_bytes <= OUTBOUND_QUEUE_BYTE_BUDGET);
        assert!(held_encoded_bytes >= ENCODER_PAYLOAD_BYTES);

        // This is the data-side action performed by stop_websocket_writer;
        // dedicated close control remains independent of both wait queues.
        sender.encoder.close();
        sender.bytes.close();
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(result) = tasks.join_next().await {
                let _ = result.expect("encoder task");
            }
        })
        .await
        .expect("close must promptly preempt encoder and byte waiters");
    }


    #[tokio::test]
    async fn slow_client_survives_transient_event_burst_without_policy_close() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        // Build the listener from a socket with a tiny send buffer (accepted
        // sockets inherit SO_SNDBUF), so a few frames congest the server side
        // of the connection and stall the writer between the client's reads.
        let listen_socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = listen_socket.set_send_buffer_size(4096);
        listen_socket
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind listener");
        let listener = listen_socket.listen(16).expect("listen");
        let address = listener.local_addr().expect("listener address");
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
            // Far longer than the client's read pacing: a transient stall is
            // not a permanently slow client and must not be evicted.
            slow_client_grace: Duration::from_secs(2),
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            websocket_connection(stream, raw, state, None, timeouts).await
        });
        // Connect with a tiny receive window so a few frames congest the
        // socket and stall the server's writer between reads.
        let socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = socket.set_recv_buffer_size(4096);
        let tcp = socket.connect(address).await.expect("connect tcp");
        let mut request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("client request");
        let (mut socket, _) = tokio_tungstenite::client_async(request, tcp)
            .await
            .expect("connect WebSocket");

        // A burst far beyond the 64-frame outbound queue, emitted as fast as
        // the fan-in broadcast allows while the client keeps reading.
        const BURST: usize = 200;
        let pad = "x".repeat(2000);
        let events_tx = manager.events_tx();
        let emitter = tokio::spawn(async move {
            for n in 0..BURST {
                let frame =
                    json!({"type": "burst_delta", "sessionId": "sess-1", "n": n, "pad": pad});
                if events_tx.send(frame).is_err() {
                    break;
                }
            }
        });

        // Read everything the server sends, pacing so the TCP window stays
        // mostly closed and the writer is stalled for most of the burst. On
        // the pre-fix code the first full-queue enqueue closed the connection
        // with 1008; with the grace the same transient stall must be absorbed
        // losslessly.
        let mut received = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while received < BURST && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(20), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame: Value = serde_json::from_str(text.as_str()).expect("frame json");
                    if frame.get("type").and_then(Value::as_str) == Some("burst_delta") {
                        received += 1;
                        // Pace the reads so the tiny TCP window stays mostly
                        // closed: the server writer stalls between reads for
                        // the whole burst, which is exactly the transient
                        // stall the grace must absorb.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("transient stall must not close the connection: {frame:?}");
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error during transient burst: {error}"),
                Ok(None) => panic!("connection dropped during transient burst"),
                Err(_) => {} // pacing tick: no frame was ready
            }
        }
        assert_eq!(
            received, BURST,
            "every burst delta must be delivered losslessly to a slow reader"
        );
        emitter.await.expect("emitter task");

        // The connection stayed up through the burst: close gracefully and
        // let the server exit cleanly.
        socket
            .close(None)
            .await
            .expect("close WebSocket gracefully");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task must exit after the client disconnects")
            .expect("server task must not panic")
            .expect("server result");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn inbound_fast_ack_command_is_answered_during_saturated_event_flood() {
        // A client that keeps READING (slowly, so the outbound queue stays
        // full for the whole burst) must still get its fast-ack commands
        // answered DURING the flood. With a `biased` select the application
        // event branches starved the inbound-read branch whenever the events
        // channel was continuously ready, so a mid-flood `session_list` poll
        // was not even read until the flood ended. That delays control replies
        // despite a healthy, provably-reading connection. The connection loop
        // must poll inbound reads fairly.
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        // Tiny send buffer + tiny receive window: the writer stalls between
        // the client's reads, keeping the outbound queue full for the whole
        // burst — the events channel therefore stays ready the entire time,
        // which is exactly the biased-select starvation condition.
        let listen_socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = listen_socket.set_send_buffer_size(4096);
        listen_socket
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind listener");
        let listener = listen_socket.listen(16).expect("listen");
        let address = listener.local_addr().expect("listener address");
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
            slow_client_grace: Duration::from_secs(2),
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            websocket_connection(stream, raw, state, None, timeouts).await
        });
        let socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = socket.set_recv_buffer_size(4096);
        let tcp = socket.connect(address).await.expect("connect tcp");
        let mut request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("client request");
        let (mut socket, _) = tokio_tungstenite::client_async(request, tcp)
            .await
            .expect("connect WebSocket");

        const BURST: usize = 200;
        let pad = "x".repeat(2000);
        let events_tx = manager.events_tx();
        let emitter = tokio::spawn(async move {
            for n in 0..BURST {
                let frame =
                    json!({"type": "burst_delta", "sessionId": "sess-1", "n": n, "pad": pad});
                if events_tx.send(frame).is_err() {
                    break;
                }
            }
        });

        // Drain the first 40 frames slowly, then send a fast-ack command
        // while the burst is still streaming. The response must arrive BEFORE
        // the flood ends — it must not wait for the whole burst to drain.
        let mut received = 0usize;
        let mut responded = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !responded && received < BURST && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(20), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame: Value = serde_json::from_str(text.as_str()).expect("frame json");
                    match frame.get("type").and_then(Value::as_str) {
                        Some("burst_delta") => {
                            received += 1;
                            if received == 40 {
                                socket
                                    .send(Message::Text(
                                        r#"{"id":"probe","type":"session_list","scope":"current"}"#
                                            .into(),
                                    ))
                                    .await
                                    .expect("send fast-ack command mid-flood");
                            }
                            // Pace the reads so the tiny TCP window stays
                            // mostly closed: the writer stalls between reads
                            // for the whole burst.
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        Some("response")
                            if frame.get("id").and_then(Value::as_str) == Some("probe") =>
                        {
                            responded = true;
                        }
                        _ => {}
                    }
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("flood must not close the connection: {frame:?}");
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error during flood: {error}"),
                Ok(None) => panic!("connection dropped during flood"),
                Err(_) => {} // pacing tick: no frame was ready
            }
        }
        assert!(
            responded,
            "a mid-flood fast-ack command must be answered (received {received}/{BURST} before timeout)"
        );
        assert!(
            received < BURST,
            "the fast-ack response must arrive BEFORE the flood ends, not after the whole burst drained (received {received}/{BURST})"
        );

        // Drain the remainder of the burst, then close gracefully.
        while received < BURST && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(20), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame: Value = serde_json::from_str(text.as_str()).expect("frame json");
                    if frame.get("type").and_then(Value::as_str) == Some("burst_delta") {
                        received += 1;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!("flood must not close the connection: {frame:?}");
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error during flood: {error}"),
                Ok(None) => panic!("connection dropped during flood"),
                Err(_) => {}
            }
        }
        assert_eq!(
            received, BURST,
            "every burst delta must be delivered losslessly"
        );
        emitter.await.expect("emitter task");
        socket
            .close(None)
            .await
            .expect("close WebSocket gracefully");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task must exit after the client disconnects")
            .expect("server task must not panic")
            .expect("server result");
        manager.shutdown().await;
    }

    fn push_liveness(
        tx: &mpsc::Sender<Message>,
        message: Message,
        received: usize,
        burst: usize,
    ) {
        match tx.try_send(message) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                panic!("liveness writer closed during flood after {received}/{burst}");
            }
        }
    }

    #[tokio::test]
    async fn inbound_pong_keeps_idle_alive_during_saturated_outbound_flood() {
        // A healthy client that keeps READING (each drain credits the next
        // emit, so the outbound queue stays saturated without a wall-clock
        // stall past slow-client grace) and keeps ANSWERING (manual pong +
        // explicit app pings) must NEVER be evicted with
        // "control plane connection idle" mid-flood. Pre-fix, the connection
        // loop could spend longer than the idle window inside blocking
        // outbound enqueues without polling `websocket_read`, so the idle
        // watchdog fired on a provably-alive connection — the loaded-switch
        // snapshot flood surfaced as a spurious 1006 in the browser. The
        // loop must re-arm the idle window from buffered inbound frames
        // around every outbound wait.
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        // Tiny send buffer + tiny receive window: the writer stalls between
        // the client's reads, keeping the outbound queue saturated — the
        // events branch blocks in enqueue_json for most of every iteration.
        let listen_socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = listen_socket.set_send_buffer_size(4096);
        listen_socket
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind listener");
        let listener = listen_socket.listen(16).expect("listen");
        let address = listener.local_addr().expect("listener address");
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_millis(20),
            idle: Duration::from_millis(300),
            slow_client_grace: Duration::from_secs(2),
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            websocket_connection(stream, raw, state, None, timeouts).await
        });
        let socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = socket.set_recv_buffer_size(4096);
        let tcp = socket.connect(address).await.expect("connect tcp");
        let mut request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("client request");
        let (socket, _) = tokio_tungstenite::client_async(request, tcp)
            .await
            .expect("connect WebSocket");
        // Independent read/write like production (3021). Combined send+read
        // wedged the 4KiB windows: ping send blocked the client's drain,
        // writer hit WriteStalledError (3110) and dropped TCP with no close
        // (3424), so the next send was ECONNRESET (104).
        let (mut sink, mut socket) = socket.split();
        let (liveness_tx, mut liveness_rx) = mpsc::channel::<Message>(32);
        let writer = tokio::spawn(async move {
            while let Some(message) = liveness_rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        const BURST: usize = 100;
        let pad = "x".repeat(2000);
        let events_tx = manager.events_tx();
        let (progress_tx, mut progress_rx) = mpsc::channel::<()>(BURST);
        let emitter = tokio::spawn(async move {
            for n in 0..BURST {
                let frame =
                    json!({"type": "burst_delta", "sessionId": "sess-1", "n": n, "pad": pad});
                if events_tx.send(frame).is_err() {
                    break;
                }
                if n + 1 >= OUTBOUND_QUEUE_CAPACITY && progress_rx.recv().await.is_none() {
                    break;
                }
            }
        });

        let mut received = 0usize;
        let mut answered = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        while (received < BURST || answered < 2) && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(20), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) {
                        if frame.get("type").and_then(Value::as_str) == Some("burst_delta") {
                            received += 1;
                            let _ = progress_tx.try_send(());
                            push_liveness(
                                &liveness_tx,
                                Message::Text(r#"{"type":"ping"}"#.into()),
                                received,
                                BURST,
                            );
                        }
                    }
                }
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    answered += 1;
                    push_liveness(&liveness_tx, Message::Pong(payload), received, BURST);
                }
                Ok(Some(Ok(Message::Close(frame)))) => {
                    panic!(
                        "a reading, answering client must not be evicted mid-flood: {frame:?} (received {received}/{BURST})"
                    );
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("read error during flood: {error}"),
                Ok(None) => panic!(
                    "connection dropped during flood after {received}/{BURST} frames"
                ),
                Err(_) => {
                    if received >= BURST {
                        push_liveness(
                            &liveness_tx,
                            Message::Text(r#"{"type":"ping"}"#.into()),
                            received,
                            BURST,
                        );
                    }
                }
            }
        }
        assert_eq!(
            received, BURST,
            "every burst delta must be delivered losslessly to the answering client"
        );
        assert!(
            answered >= 2,
            "the server must keep pinging during the flood (answered {answered})"
        );
        drop(progress_tx);
        emitter.await.expect("emitter task");
        drop(liveness_tx);
        writer.await.expect("client write task");
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server task must exit after the client disconnects")
            .expect("server task must not panic")
            .expect("server result");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn buffered_inbound_frame_receives_one_idle_credit() {
        let mut read = futures_util::stream::iter([Ok::<_, tokio_tungstenite::tungstenite::Error>(
            Message::Text(r#"{"type":"ping"}"#.into()),
        )]);
        let idle = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(idle);
        let initial_deadline = idle.as_ref().deadline();
        let mut pending = VecDeque::new();
        drain_buffered_inbound(
            &mut read,
            idle.as_mut(),
            Duration::from_millis(300),
            &mut pending,
        );
        let credited_deadline = idle.as_ref().deadline();
        assert!(credited_deadline < initial_deadline);
        assert_eq!(pending.len(), 1);
        assert!(pending.front().is_some_and(|(_, credited)| *credited));
        drain_buffered_inbound(
            &mut read,
            idle.as_mut(),
            Duration::from_millis(300),
            &mut pending,
        );
        assert_eq!(idle.as_ref().deadline(), credited_deadline);
        assert_eq!(pending.len(), 1);
    }

    #[tokio::test]
    async fn single_buffered_frame_rescues_idle_once_then_idle_close_fires() {
        // Credit accounting: exactly ONE buffered inbound frame gives the
        // connection exactly ONE idle window. The client sends a single app
        // ping during an event flood and then produces NO further inbound
        // traffic (no manual pongs; ping_interval is huge so a reading client
        // cannot auto-pong either). The frame must be consumed and ANSWERED
        // once (the pong reply arrives), and afterwards the idle watchdog
        // must still fire — a cached frame can never extend liveness.
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let listen_socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        listen_socket
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind listener");
        let listener = listen_socket.listen(16).expect("listen");
        let address = listener.local_addr().expect("listener address");
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_millis(300),
            slow_client_grace: Duration::from_secs(2),
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            websocket_connection(stream, raw, state, None, timeouts).await
        });
        let tcp = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect tcp");
        let mut request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("client request");
        let (mut socket, _) = tokio_tungstenite::client_async(request, tcp)
            .await
            .expect("connect WebSocket");

        let events_tx = manager.events_tx();
        for n in 0..100 {
            let frame = json!({"type": "burst_delta", "sessionId": "sess-1", "n": n});
            let _ = events_tx.send(frame);
        }
        // The single inbound frame of the whole connection.
        socket
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .expect("send the single app ping");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) {
                            if frame.get("type").and_then(Value::as_str) == Some("pong") {
                                return;
                            }
                        }
                    }
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(error))) => panic!("read error before pong: {error}"),
                    Ok(None) => panic!("connection closed before the ping was answered"),
                    Err(_) => {} // pacing tick
                }
            }
        })
        .await
        .expect("the single buffered frame must be consumed and answered");

        // No further inbound is possible (huge ping interval, client sends
        // nothing): the idle close MUST fire within roughly ONE idle window
        // (300ms) of the single credit, not merely within a loose bound.
        let close = tokio::time::timeout(Duration::from_millis(800), async {
            loop {
                match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                    Ok(Some(Ok(Message::Close(frame)))) => return frame,
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(error))) => panic!("read error before idle close: {error}"),
                    Ok(None) => panic!("connection dropped without an idle close"),
                    Err(_) => {}
                }
            }
        })
        .await
        .expect("the idle close must arrive after the single credit");
        let frame = close.expect("idle close carries a frame");
        assert_eq!(frame.code, CloseCode::Policy);
        assert!(
            frame.reason.contains("idle"),
            "unexpected close reason: {}",
            frame.reason
        );
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server task must exit after the idle close")
            .expect("server task must not panic")
            .expect("idle close is a clean server exit");
        manager.shutdown().await;
    }

    /// A transport whose writes are accepted-but-never-delivered once
    /// `stalled` is set: bytes are captured (so the test can observe exactly
    /// what the server attempted to write) while `poll_flush` never
    /// completes. Deterministically reproduces the production failure shape —
    /// a peer that stops reading so the kernel Send-Q wedges and no
    /// `websocket_write.send` can ever make progress — with no kernel
    /// buffers, no pacing, and no cross-test socket coupling.
    struct BlackHoledWrite<S> {
        inner: S,
        stalled: Arc<std::sync::atomic::AtomicBool>,
        captured: Arc<parking_lot::Mutex<Vec<u8>>>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for BlackHoledWrite<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for BlackHoledWrite<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if this.stalled.load(std::sync::atomic::Ordering::Relaxed) {
                // Bytes "accepted" by the transport but never delivered: the
                // peer's receive window never opens again, so no send can
                // ever complete — the exact stalled-write shape the
                // write-progress timeout must evict.
                this.captured.lock().extend_from_slice(buf);
                return Poll::Ready(Ok(buf.len()));
            }
            Pin::new(&mut this.inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if this.stalled.load(std::sync::atomic::Ordering::Relaxed) {
                // Never drains: only the write-progress timeout can wake the
                // stalled send, which is precisely the point.
                return Poll::Pending;
            }
            Pin::new(&mut this.inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    struct StallHarness {
        socket: WebSocketStream<tokio::io::DuplexStream>,
        server: tokio::task::JoinHandle<Result<()>>,
        stalled: Arc<std::sync::atomic::AtomicBool>,
        captured: Arc<parking_lot::Mutex<Vec<u8>>>,
        manager: Arc<SessionRuntimeManager>,
    }

    /// Open a control-plane WebSocket over a [`BlackHoledWrite`] duplex pair:
    /// the handshake completes normally, then the test can wedge the write
    /// side at will and observe every byte the server attempts.
    async fn open_stallable_control_socket(state: ServerState, grace: Duration) -> StallHarness {
        let manager = state.manager.clone();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let stalled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let transport = BlackHoledWrite {
            inner: server_io,
            stalled: stalled.clone(),
            captured: captured.clone(),
        };
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
            slow_client_grace: grace,
        };
        let server = tokio::spawn(async move {
            let mut transport = transport;
            let raw = read_http_request(&mut transport)
                .await
                .expect("read upgrade request");
            websocket_connection(transport, raw, state, None, timeouts).await
        });
        let request = "ws://listener.example/ws"
            .into_client_request()
            .expect("client request");
        let (socket, _) = tokio_tungstenite::client_async(request, client_io)
            .await
            .expect("client handshake");
        StallHarness {
            socket,
            server,
            stalled,
            captured,
            manager,
        }
    }

    #[tokio::test]
    async fn bounded_writer_keeps_fast_sends_lossless_and_ordered() {
        let state = ws_server_state().await;
        let harness = open_stallable_control_socket(state, Duration::from_secs(5)).await;
        let mut socket = harness.socket;

        // Fast path: the writer's per-frame write-progress bound must not
        // perturb ordinary sends — a ping is answered promptly...
        socket
            .send(Message::Text(r#"{"type":"ping"}"#.into()))
            .await
            .expect("send ping");
        let pong = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("pong within fast-path bound")
            .expect("socket open")
            .expect("read pong");
        assert_eq!(pong, Message::Text("{\"type\":\"pong\"}".into()));

        // ...and an event burst arrives in enqueue order (the writer is a
        // single FIFO consumer of the bounded outbound queue).
        const BURST: usize = 8;
        let events_tx = harness.manager.events_tx();
        for n in 0..BURST as u64 {
            events_tx
                .send(json!({"type": "burst_delta", "sessionId": "sess-1", "n": n}))
                .expect("emit event");
        }
        let mut order = Vec::new();
        while order.len() < BURST {
            let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("event within fast-path bound")
                .expect("socket open")
                .expect("read event");
            let Message::Text(text) = frame else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).expect("frame json");
            if value["type"] == "burst_delta" {
                order.push(value["n"].as_u64().expect("burst index"));
            }
        }
        assert_eq!(order, (0..BURST as u64).collect::<Vec<_>>());

        // Graceful client close: the connection exits cleanly, proving the
        // bounded send never interferes with the fast path.
        socket.close(None).await.expect("close gracefully");
        let result = tokio::time::timeout(Duration::from_secs(2), harness.server)
            .await
            .expect("server must exit after client close")
            .expect("server task must not panic");
        result.expect("graceful close must succeed");
        harness.manager.shutdown().await;
    }

    #[tokio::test]
    async fn stalled_socket_write_is_evicted_within_grace_with_policy_close() {
        let state = ws_server_state().await;
        let grace = Duration::from_millis(300);
        let harness = open_stallable_control_socket(state, grace).await;
        let mut socket = harness.socket;

        // Wedge the transport after the handshake: every subsequent byte is
        // accepted-but-never-delivered and flush never completes, so the
        // next writer send can make no progress at all — the frozen-Send-Q
        // shape from the live profile. Enqueue-side bounds cannot see this;
        // without the write-progress bound the writer would sit in send()
        // forever while the connection keeps enqueueing and re-serializing
        // snapshots for a client that never reads.
        harness
            .stalled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Exactly one event: it is the frame the writer dequeues and stalls
        // on. Any further enqueue would trip the enqueue-side slow-client
        // bound at the same instant (enqueue_json reserves the whole byte
        // budget, so one in-flight frame blocks the next reservation) and
        // race this test; a single frame makes the write-stall path the only
        // exit and the test deterministic.
        harness
            .manager
            .events_tx()
            .send(json!({"type": "burst_delta", "sessionId": "sess-1", "n": 0}))
            .expect("emit stalled event");

        let bound = grace + Duration::from_secs(1);
        let started = Instant::now();
        let result = tokio::time::timeout(bound, harness.server)
            .await
            .expect("connection must exit within the write grace");
        let elapsed = started.elapsed();
        let error = result
            .expect("server task must not panic")
            .expect_err("a stalled write must evict the connection");
        assert!(
            error.downcast_ref::<WriteStalledError>().is_some(),
            "stalled write must surface as the slow-client eviction, got: {error:#}"
        );
        assert!(
            format!("{error}").contains("client is not reading messages"),
            "eviction error must carry the slow-client 1008 policy wording: {error}"
        );
        assert!(
            elapsed < grace + Duration::from_secs(1),
            "eviction must stay within the write grace, took {elapsed:?}"
        );

        // The write-stall eviction shares the slow-client exit (identical
        // 1008 close construction at the connection exit); the frame itself
        // can never be observed here because the wedged transport cannot
        // carry it — the enqueue-side 1008 close is covered by the existing
        // full-queue eviction test.
        let captured = harness.captured.lock().clone();
        assert!(
            !captured.is_empty(),
            "the stalled data frame must have been accepted by the wedged transport"
        );
        // No unbounded retry: the single stalled data frame is all the
        // writer ever wrote into the wedged transport, and the connection
        // stopped enqueueing (the remaining flood was dropped with the
        // evicted connection).
        assert!(
            captured.len() < 2048,
            "writer must not retry unboundedly into a wedged transport; captured {} bytes",
            captured.len()
        );
        harness.manager.shutdown().await;
    }

    async fn run_session_download(raw: RawRequest, state: &ServerState) -> String {
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        handle_session_download(&mut server, raw, state)
            .await
            .expect("session download handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test]
    async fn session_download_is_one_shot_attachment_with_validated_origin_cors() {
        let state = ws_server_state().await;
        let download_id = state
            .manager
            .stage_session_download(
                Arc::<[u8]>::from(b"<!DOCTYPE html><p>selected</p>".to_vec()),
                "safe-session.html".into(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
            .expect("stage download");
        let request = format!(
            "GET {SESSION_DOWNLOAD_PREFIX}{download_id} HTTP/1.1\r\nhost: listener.example:5173\r\norigin: https://listener.example:5173\r\n\r\n"
        );
        let response =
            run_session_download(headers(request.as_bytes()).expect("request"), &state).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("content-type: text/html; charset=utf-8"),
            "{response}"
        );
        assert!(
            response.contains("content-disposition: attachment; filename=\"safe-session.html\""),
            "{response}"
        );
        assert!(
            response.contains("access-control-allow-origin: https://listener.example:5173"),
            "{response}"
        );
        assert!(
            response.ends_with("<!DOCTYPE html><p>selected</p>"),
            "{response}"
        );

        let reused =
            run_session_download(headers(request.as_bytes()).expect("reuse request"), &state).await;
        assert!(reused.starts_with("HTTP/1.1 404"), "{reused}");

        let cross_origin_id = state
            .manager
            .stage_session_download(
                Arc::<[u8]>::from(b"cross-origin".to_vec()),
                "cross-origin.html".into(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
            .expect("stage cross-origin download");
        let cross_origin_request = format!(
            "GET {SESSION_DOWNLOAD_PREFIX}{cross_origin_id} HTTP/1.1\r\nhost: listener.example\r\norigin: https://unrelated.example\r\n\r\n"
        );
        let cross_origin_response = run_session_download(
            headers(cross_origin_request.as_bytes()).expect("cross-origin request"),
            &state,
        )
        .await;
        assert!(
            cross_origin_response.starts_with("HTTP/1.1 200"),
            "{cross_origin_response}"
        );
        assert!(
            !cross_origin_response.contains("access-control-allow-origin"),
            "cross-origin capability response must not be readable by browser scripts: {cross_origin_response}"
        );
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_download_invalid_expired_and_malformed_origin_fail_closed() {
        let state = ws_server_state().await;
        let expired = state
            .manager
            .stage_session_download(
                Arc::<[u8]>::from(b"expired".to_vec()),
                "expired.html".into(),
                Instant::now() - Duration::from_millis(1),
            )
            .await
            .expect("stage expired");
        let expired_request = format!(
            "GET {SESSION_DOWNLOAD_PREFIX}{expired} HTTP/1.1\r\nhost: x\r\norigin: null\r\n\r\n"
        );
        let expired_response = run_session_download(
            headers(expired_request.as_bytes()).expect("expired request"),
            &state,
        )
        .await;
        assert!(
            expired_response.starts_with("HTTP/1.1 404"),
            "{expired_response}"
        );
        assert!(!expired_response.contains("access-control-allow-origin"));

        let invalid = "GET /download/session/not-a-uuid HTTP/1.1\r\nhost: x\r\n\r\n";
        let invalid_response = run_session_download(
            headers(invalid.as_bytes()).expect("invalid request"),
            &state,
        )
        .await;
        assert!(
            invalid_response.starts_with("HTTP/1.1 404"),
            "{invalid_response}"
        );
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_download_route_needs_no_bearer_token() {
        let mut state = ws_server_state().await;
        state.token = Some(Arc::from(b"listener-secret".as_slice()));
        let download_id = state
            .manager
            .stage_session_download(
                Arc::<[u8]>::from(b"<html>capability</html>".to_vec()),
                "capability.html".into(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
            .expect("stage capability download");
        let request = format!(
            "GET {SESSION_DOWNLOAD_PREFIX}{download_id} HTTP/1.1\r\nhost: listener.example\r\norigin: https://listener.example\r\n\r\n"
        );
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let task = tokio::spawn(async move { handle_connection(server, state).await });
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        task.await.expect("route task").expect("route response");
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("<html>capability</html>"), "{response}");
    }

    #[tokio::test]
    async fn session_download_response_write_is_bounded_for_non_reading_client() {
        // A client that never drains the socket must not pin the connection
        // task: the whole response (header + body + shutdown) write times out
        // and the handler returns an error instead of hanging forever.
        let (client, mut server) = tokio::io::duplex(16);
        let start = std::time::Instant::now();
        let download = crate::modes::session_runtime_manager::StagedSessionDownload {
            bytes: Arc::<[u8]>::from(b"<!DOCTYPE html><p>selected</p>".to_vec()),
            file_name: "safe-session.html".into(),
        };
        let result = write_session_download_response(
            &mut server,
            download,
            Some("https://web.example:5173"),
            Duration::from_millis(100),
        )
        .await;
        assert!(
            result.is_err(),
            "a non-reading client must time out the write"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("timed out writing the session download response"),
            "unexpected error"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "write timeout fired after {}ms",
            start.elapsed().as_millis()
        );
        drop(client);
    }

    #[tokio::test]
    async fn sustained_unread_client_is_still_evicted_after_grace() {
        let state = ws_server_state().await;
        let manager = state.manager.clone();
        let listen_socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = listen_socket.set_send_buffer_size(4096);
        listen_socket
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind listener");
        let listener = listen_socket.listen(16).expect("listen");
        let address = listener.local_addr().expect("listener address");
        let timeouts = WebSocketTimeouts {
            handshake: Duration::from_secs(2),
            ping_interval: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
            // A client that reads NOTHING for longer than the grace is a
            // genuinely slow client: bounded eviction (the 1008 decision)
            // must still fire.
            slow_client_grace: Duration::from_millis(300),
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let raw = read_http_request(&mut stream)
                .await
                .expect("read upgrade request");
            websocket_connection(stream, raw, state, None, timeouts).await
        });
        let socket = tokio::net::TcpSocket::new_v4().expect("new v4 socket");
        let _ = socket.set_recv_buffer_size(4096);
        let tcp = socket.connect(address).await.expect("connect tcp");
        let mut request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("client request");
        let (socket, _) = tokio_tungstenite::client_async(request, tcp)
            .await
            .expect("connect WebSocket");

        // Flood events while the client never reads: the queue fills and
        // stays full past the grace, so the connection must be torn down.
        const BURST: usize = 200;
        let pad = "x".repeat(2000);
        let events_tx = manager.events_tx();
        for n in 0..BURST {
            let frame = json!({"type": "burst_delta", "sessionId": "sess-1", "n": n, "pad": pad});
            let _ = events_tx.send(frame);
        }

        // Wait past the grace so the eviction decision fires (queue full,
        // no drain), then start reading: the close frame can only flush once
        // the client reads, and the writer's bounded close window (2s) must
        // still be open — so the server exits cleanly instead of aborting.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let drainer = tokio::spawn(async move {
            let mut socket = socket;
            loop {
                match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
                }
            }
        });
        let eviction = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("unread client must be evicted within the bounded grace")
            .expect("server task must not panic");
        // The writer's per-message send-progress timeout fires at the grace
        // even before the queue fills, so the eviction surfaces as the
        // WriteStalledError slow-client policy (same 1008 wording) rather
        // than a clean close — bounded either way.
        let error = eviction.expect_err("sustained unread client must be evicted");
        assert!(
            error.to_string().contains("client is not reading messages"),
            "eviction error must carry the slow-client 1008 policy wording: {error}"
        );
        drainer.await.ok();
        manager.shutdown().await;
    }

    // ---------------------------------------------------------------------
    // POST /upload/video — authenticated bounded upload + frame extraction.
    // ---------------------------------------------------------------------

    /// Build a parsed `POST /upload/video` request with the raw video bytes
    /// as the body and an `X-Video-Name` header. Extra headers are appended
    /// verbatim; a caller-supplied `host` replaces the default.
    fn video_request(body: &[u8], name: &str, extra_headers: &[(&str, &str)]) -> RawRequest {
        let has_host = extra_headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("host"));
        let mut head = format!(
            "POST /upload/video HTTP/1.1\r\n{}content-length: {}\r\nx-video-name: {}\r\n",
            if has_host { "" } else { "host: x\r\n" },
            body.len(),
            name
        );
        for (key, value) in extra_headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        headers(&bytes).expect("parse upload request")
    }

    /// Run `handle_video_upload` against a duplex pair and return the raw
    /// HTTP response text.
    async fn run_video_upload(raw: RawRequest, state: &ServerState) -> String {
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        handle_video_upload(&mut server, raw, state)
            .await
            .expect("upload handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    async fn video_upload_state() -> ServerState {
        let mut state = ws_server_state().await;
        state.token = Some(Arc::from(b"video-secret".as_slice()));
        state
    }

    fn response_json(response: &str) -> Value {
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or(response);
        serde_json::from_str(body).expect("response body is JSON")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn video_upload_success_returns_jpeg_frames_and_metadata() {
        use crate::video_extract::test_support::{fake_ffmpeg, video_bytes};
        use crate::video_extract::with_ffmpeg_program_async;

        let (_dir, script) = fake_ffmpeg();
        let body = video_bytes("VALID 00:00:12.34 1280x720");
        let raw = video_request(&body, "clip.mkv", &[]);
        let response = with_ffmpeg_program_async(script, async {
            let state = ws_server_state().await;
            run_video_upload(raw, &state).await
        })
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let parsed = response_json(&response);
        assert!(
            parsed["attachmentId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(parsed["name"], "clip.mkv");
        assert_eq!(parsed["container"], "mkv");
        assert_eq!(parsed["mimeType"], "video/x-matroska");
        assert_eq!(parsed["sizeBytes"].as_u64().unwrap() as usize, body.len());
        assert!((parsed["durationSeconds"].as_f64().unwrap() - 12.34).abs() < 1e-9);
        assert_eq!(parsed["frameCount"], 6);
        assert!(parsed["framesBase64Bytes"].as_u64().unwrap() > 0);
        let frames = parsed["frames"].as_array().expect("frames array");
        assert_eq!(frames.len(), 6);
        let mut previous_ts = -1.0;
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame["index"], index);
            assert_eq!(frame["mimeType"], "image/jpeg", "frame {index}");
            assert_eq!(frame["width"], 1);
            assert_eq!(frame["height"], 1);
            let data = frame["data"].as_str().expect("base64 data");
            assert!(!data.is_empty());
            let ts = frame["timestampSeconds"].as_f64().unwrap();
            assert!(ts > previous_ts, "frames must be chronological");
            previous_ts = ts;
        }
        let instruction = parsed["instruction"].as_str().expect("instruction");
        assert!(instruction.contains("clip.mkv"));
        assert!(instruction.contains("0.00s"));
        assert!(instruction.contains("2.06s"));
        assert!(
            !response.contains("pi-video-"),
            "response must not leak the work directory"
        );
        // Raw video never enters a content block: every frame is a JPEG.
        assert!(
            frames.iter().all(|frame| frame["mimeType"] == "image/jpeg"),
            "no video MIME may appear in the frames"
        );
    }

    #[tokio::test]
    async fn video_upload_requires_bearer_auth() {
        use crate::video_extract::test_support::video_bytes;
        use crate::video_extract::with_ffmpeg_program_async;

        let body = video_bytes("VALID 00:00:01.00 320x240");
        let state = video_upload_state().await;
        // No Authorization header while a token is configured.
        let raw = video_request(&body, "clip.mkv", &[]);
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");

        // Wrong token.
        let raw = video_request(
            &body,
            "clip.mkv",
            &[("authorization", "Bearer not-the-token")],
        );
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");

        // Correct bearer token passes auth; with ffmpeg overridden to a
        // missing binary the pipeline then fails with the actionable 503 —
        // proving auth was not the blocker (host-independent).
        let raw = video_request(
            &body,
            "clip.mkv",
            &[("authorization", "Bearer video-secret")],
        );
        let missing = std::env::temp_dir().join(format!("pi-no-ffmpeg-{}", uuid::Uuid::new_v4()));
        let response =
            with_ffmpeg_program_async(missing, async { run_video_upload(raw, &state).await }).await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    }

    #[tokio::test]
    async fn video_upload_validates_name_length_and_container_before_processing() {
        use crate::video_extract::test_support::video_bytes;

        let state = ws_server_state().await;
        let body = video_bytes("VALID 00:00:01.00 320x240");

        // Unsupported extension -> 415 before any body read. The raw name
        // may carry a client-side path and must never be echoed.
        let raw = video_request(&body, "../private/clip.txt", &[]);
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 415"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(error.contains("supported containers"), "{error}");
        assert!(
            !error.contains("private") && !error.contains("clip.txt"),
            "raw name must not be echoed: {error}"
        );

        // Missing content-length -> 411.
        let head = "POST /upload/video HTTP/1.1\r\nhost: x\r\nx-video-name: clip.mkv\r\n\r\n";
        let raw = headers(head.as_bytes()).expect("parse");
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 411"), "{response}");

        // Oversized content-length -> 413 before reading the body.
        let head = format!(
            "POST /upload/video HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\nx-video-name: clip.mkv\r\n\r\n",
            crate::video_extract::MAX_VIDEO_UPLOAD_BYTES + 1
        );
        let raw = headers(head.as_bytes()).expect("parse");
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 413"), "{response}");

        // Right extension, wrong container bytes -> 415 (no ffmpeg needed).
        let raw = video_request(b"definitely not a video", "clip.mkv", &[]);
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 415"), "{response}");

        // Missing name header -> 400.
        let head = format!(
            "POST /upload/video HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        let raw = headers(head.as_bytes()).expect("parse");
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    #[tokio::test]
    async fn video_upload_missing_ffmpeg_returns_actionable_503() {
        use crate::video_extract::test_support::video_bytes;
        use crate::video_extract::with_ffmpeg_program_async;

        let state = ws_server_state().await;
        let body = video_bytes("VALID 00:00:01.00 320x240");
        let raw = video_request(&body, "clip.mkv", &[]);
        let missing = std::env::temp_dir().join(format!("pi-no-ffmpeg-{}", uuid::Uuid::new_v4()));
        let response =
            with_ffmpeg_program_async(missing, async { run_video_upload(raw, &state).await }).await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(error.contains("ffmpeg"), "{error}");
        assert!(error.contains("install"), "actionable: {error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn video_upload_rejects_invalid_media_with_bounded_error() {
        use crate::video_extract::test_support::{fake_ffmpeg, video_bytes};
        use crate::video_extract::with_ffmpeg_program_async;

        let (_dir, script) = fake_ffmpeg();
        let body = video_bytes("CORRUPT");
        let raw = video_request(&body, "clip.mkv", &[]);
        let response = with_ffmpeg_program_async(script, async {
            let state = ws_server_state().await;
            run_video_upload(raw, &state).await
        })
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(error.contains("not a decodable video"), "{error}");
        assert!(
            !error.contains("pi-video-"),
            "error must not leak the work directory: {error}"
        );
    }

    /// Run the CORS preflight handler for a raw OPTIONS request head.
    async fn run_video_preflight(head: &str, state: &ServerState) -> String {
        let raw = headers(head.as_bytes()).expect("parse preflight");
        let (mut client, mut server) = tokio::io::duplex(1024);
        handle_video_upload_preflight(&mut server, raw, state)
            .await
            .expect("preflight handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[test]
    fn cors_origin_validation_accepts_http_hosts_and_rejects_junk() {
        assert_eq!(
            validated_cors_origin("http://127.0.0.1:8765"),
            Some("http://127.0.0.1:8765".to_owned())
        );
        assert_eq!(
            validated_cors_origin("https://LAN-IP.example"),
            Some("https://lan-ip.example".to_owned())
        );
        assert_eq!(
            validated_cors_origin("http://[::1]:8765"),
            Some("http://[::1]:8765".to_owned())
        );
        for bad in [
            "null",
            "file:///etc/passwd",
            "https://",
            "*",
            "http://host:abc",
            "http://user@host",
            "http://host/path",
            "http://host?query",
            "javascript:alert(1)",
            "http://",
        ] {
            assert!(
                validated_cors_origin(bad).is_none(),
                "{bad:?} must be rejected"
            );
        }
    }

    // ---------------------------------------------------------------------
    // GET /status — unauthenticated runner probe + CORS.
    // ---------------------------------------------------------------------

    /// Run `handle_status` against a duplex pair and return the raw HTTP
    /// response text.
    async fn run_status(raw: RawRequest, state: &ServerState) -> String {
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        handle_status(&mut server, raw, state)
            .await
            .expect("status handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Run `handle_status_preflight` against a duplex pair and return the
    /// raw HTTP response text.
    async fn run_status_preflight(head: &str, state: &ServerState) -> String {
        let raw = headers(head.as_bytes()).expect("parse preflight");
        let (mut client, mut server) = tokio::io::duplex(1024);
        handle_status_preflight(&mut server, raw, state)
            .await
            .expect("status preflight handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test]
    async fn status_reports_token_required_and_codename() {
        // Tokenless listener: tokenRequired false, codename null when no
        // runner identity is configured.
        let state = ws_server_state().await;
        let response = run_status(
            headers(b"GET /status HTTP/1.1\r\nhost: x\r\n\r\n").expect("parse status request"),
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let parsed = response_json(&response);
        assert_eq!(parsed["tokenRequired"], false);
        assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            parsed.get("codename").is_some_and(Value::is_null),
            "codename must be an explicit null without a runner identity: {parsed}"
        );
        assert!(
            !response
                .to_lowercase()
                .contains("access-control-allow-origin"),
            "a native client without an Origin gets no CORS headers: {response}"
        );
        state.manager.shutdown().await;

        // Tokened listener with a resolved codename: tokenRequired true and
        // the codename is exposed.
        let mut state = ws_server_state().await;
        state.token = Some(Arc::from(b"listener-secret".as_slice()));
        state.runner_codename = Some("alpha-one".to_owned());
        let response = run_status(
            headers(b"GET /status HTTP/1.1\r\nhost: x\r\n\r\n").expect("parse status request"),
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let parsed = response_json(&response);
        assert_eq!(parsed["tokenRequired"], true);
        assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(parsed["codename"], "alpha-one");
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn status_body_has_no_session_path_or_token_data() {
        let mut state = ws_server_state().await;
        state.token = Some(Arc::from(b"listener-secret".as_slice()));
        state.runner_codename = Some("alpha-one".to_owned());
        let response = run_status(
            headers(b"GET /status HTTP/1.1\r\nhost: x\r\n\r\n").expect("parse status request"),
            &state,
        )
        .await;
        let parsed = response_json(&response);
        let object = parsed.as_object().expect("status body is an object");
        let mut keys = object.keys().collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["codename", "tokenRequired", "version"],
            "the status body must contain exactly the three contract fields: {parsed}"
        );
        let encoded = response.to_string();
        // `tokenRequired` is the contract field name; the leak check targets
        // the configured token VALUE and any session/path data.
        for banned in ["session", "sessionId", "path", "listener-secret"] {
            assert!(
                !encoded.to_lowercase().contains(banned),
                "status response must not leak {banned:?}: {encoded}"
            );
        }
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn status_reflects_validated_origin_only() {
        let state = ws_server_state().await;
        // A validated cross-authority Origin is reflected verbatim.
        let response = run_status(
            headers(
                b"GET /status HTTP/1.1\r\nhost: x\r\norigin: http://127.0.0.1:5173\r\n\r\n",
            )
            .expect("parse status request"),
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://127.0.0.1:5173"),
            "{response}"
        );
        assert!(response.contains("vary: origin"), "{response}");
        assert!(
            !response.contains("access-control-allow-origin: *"),
            "no wildcard may ever be emitted: {response}"
        );

        // Malformed/null/file/wildcard origins are never reflected.
        for origin in [
            "null",
            "file:///etc/passwd",
            "https://",
            "*",
            "not-an-origin",
            "http://host:abc",
        ] {
            let head = format!("GET /status HTTP/1.1\r\nhost: x\r\norigin: {origin}\r\n\r\n");
            let response = run_status(
                headers(head.as_bytes()).expect("parse status request"),
                &state,
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 200"), "{origin}: {response}");
            assert!(
                !response
                    .to_lowercase()
                    .contains("access-control-allow-origin"),
                "{origin}: must never reflect"
            );
        }
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn status_preflight_allows_valid_cross_origin_get() {
        let state = ws_server_state().await;
        let head = "OPTIONS /status HTTP/1.1\r\nhost: x\r\norigin: http://127.0.0.1:5173\r\n\
                    access-control-request-method: GET\r\n\r\n";
        let response = run_status_preflight(head, &state).await;
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://127.0.0.1:5173"),
            "{response}"
        );
        assert!(
            response.contains("access-control-allow-methods: GET, OPTIONS"),
            "{response}"
        );
        assert!(response.contains("vary: origin"), "{response}");
        assert!(
            !response.contains("access-control-allow-origin: *"),
            "no wildcard may ever be emitted: {response}"
        );
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn status_preflight_rejects_malformed_origins_and_methods() {
        let state = ws_server_state().await;
        for origin in [
            "null",
            "file:///etc/passwd",
            "https://",
            "*",
            "not-an-origin",
            "http://host:abc",
        ] {
            let head = format!(
                "OPTIONS /status HTTP/1.1\r\nhost: x\r\norigin: {origin}\r\n\
                 access-control-request-method: GET\r\n\r\n"
            );
            let response = run_status_preflight(&head, &state).await;
            assert!(response.starts_with("HTTP/1.1 400"), "{origin}: {response}");
            assert!(
                !response
                    .to_lowercase()
                    .contains("access-control-allow-origin"),
                "{origin}: must never reflect"
            );
        }
        // Missing origin.
        let response = run_status_preflight(
            "OPTIONS /status HTTP/1.1\r\nhost: x\r\naccess-control-request-method: GET\r\n\r\n",
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        // Wrong requested method.
        let response = run_status_preflight(
            "OPTIONS /status HTTP/1.1\r\nhost: x\r\norigin: http://ok.example\r\n\
             access-control-request-method: POST\r\n\r\n",
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn video_upload_preflight_allows_valid_cross_origin_without_bearer() {
        // Cross-origin preflights carry no bearer (browsers only declare it),
        // so a token-configured listener must allow any valid origin here;
        // the actual POST is what enforces the token.
        let state = video_upload_state().await;
        let head = "OPTIONS /upload/video HTTP/1.1\r\nhost: x\r\norigin: http://127.0.0.1:5173\r\n\
                    access-control-request-method: POST\r\n\
                    access-control-request-headers: authorization, x-video-name, content-type\r\n\r\n";
        let response = run_video_preflight(head, &state).await;
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://127.0.0.1:5173"),
            "{response}"
        );
        assert!(
            response.contains("access-control-allow-methods: POST, OPTIONS"),
            "{response}"
        );
        assert!(
            response.contains(
                "access-control-allow-headers: authorization, x-video-name, content-type"
            ),
            "{response}"
        );
        assert!(
            response.contains("access-control-max-age: 600"),
            "{response}"
        );
        assert!(response.contains("vary: origin"), "{response}");

        // Tokenless listener reflects only the same-origin browser.
        let state = ws_server_state().await;
        let head = "OPTIONS /upload/video HTTP/1.1\r\nhost: 127.0.0.1:8765\r\n\
                    origin: http://127.0.0.1:8765\r\naccess-control-request-method: POST\r\n\r\n";
        let response = run_video_preflight(head, &state).await;
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://127.0.0.1:8765"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn video_upload_preflight_rejects_malformed_origins_and_headers() {
        // Tokened listener: malformed origins/headers are still rejected
        // before any reflection, independent of the token.
        let state = video_upload_state().await;
        for origin in [
            "null",
            "file:///etc/passwd",
            "https://",
            "*",
            "not-an-origin",
            "http://host:abc",
        ] {
            let head = format!(
                "OPTIONS /upload/video HTTP/1.1\r\nhost: x\r\norigin: {origin}\r\n\
                 access-control-request-method: POST\r\n\r\n"
            );
            let response = run_video_preflight(&head, &state).await;
            assert!(response.starts_with("HTTP/1.1 400"), "{origin}: {response}");
            assert!(
                !response
                    .to_lowercase()
                    .contains("access-control-allow-origin"),
                "{origin}: must never reflect"
            );
        }
        // Missing origin.
        let response = run_video_preflight(
            "OPTIONS /upload/video HTTP/1.1\r\nhost: x\r\naccess-control-request-method: POST\r\n\r\n",
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        // Wrong requested method.
        let response = run_video_preflight(
            "OPTIONS /upload/video HTTP/1.1\r\nhost: x\r\norigin: http://ok.example\r\n\
             access-control-request-method: GET\r\n\r\n",
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        // Disallowed requested header.
        let response = run_video_preflight(
            "OPTIONS /upload/video HTTP/1.1\r\nhost: x\r\norigin: http://ok.example\r\n\
             access-control-request-method: POST\r\naccess-control-request-headers: x-evil\r\n\r\n",
            &state,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(
            !response
                .to_lowercase()
                .contains("access-control-allow-headers"),
            "disallowed headers must not be advertised"
        );
    }

    #[tokio::test]
    async fn video_upload_post_reflects_validated_origin_only() {
        use crate::video_extract::test_support::video_bytes;
        use crate::video_extract::with_ffmpeg_program_async;

        let body = video_bytes("VALID 00:00:01.00 320x240");
        let missing = std::env::temp_dir().join(format!("pi-no-ffmpeg-{}", uuid::Uuid::new_v4()));

        // Cross-authority POST with a validated Origin AND the bearer token
        // is readable by that origin (the 503 proves the pipeline ran; CORS
        // headers present). Without the token the same request is refused.
        let state = video_upload_state().await;
        let raw = video_request(
            &body,
            "clip.mkv",
            &[
                ("origin", "http://other.example:9999"),
                ("authorization", "Bearer video-secret"),
            ],
        );
        let response = with_ffmpeg_program_async(missing.clone(), async {
            run_video_upload(raw, &state).await
        })
        .await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://other.example:9999"),
            "{response}"
        );
        assert!(response.contains("vary: origin"), "{response}");

        let raw = video_request(
            &body,
            "clip.mkv",
            &[
                ("host", "127.0.0.1:8765"),
                ("origin", "https://evil.example"),
            ],
        );
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert!(
            !response
                .to_lowercase()
                .contains("access-control-allow-origin"),
            "unauthenticated cross-origin must not be reflected: {response}"
        );

        // Tokenless same-origin browser keeps working and gets ACAO.
        let state = ws_server_state().await;
        let raw = video_request(
            &body,
            "clip.mkv",
            &[
                ("host", "127.0.0.1:8765"),
                ("origin", "http://127.0.0.1:8765"),
            ],
        );
        let response =
            with_ffmpeg_program_async(missing, async { run_video_upload(raw, &state).await }).await;
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://127.0.0.1:8765"),
            "{response}"
        );

        // Native client without an Origin gets no CORS headers at all.
        let raw = video_request(&body, "clip.mkv", &[]);
        let response = run_video_upload(raw, &state).await;
        assert!(
            !response
                .to_lowercase()
                .contains("access-control-allow-origin"),
            "{response}"
        );
    }

    #[test]
    fn video_name_percent_decode_is_bounded_and_literal() {
        assert_eq!(percent_decode_video_name("clip.mkv", 1024), "clip.mkv");
        assert_eq!(percent_decode_video_name("clip%2Emkv", 1024), "clip.mkv");
        assert_eq!(
            percent_decode_video_name("%E6%BC%94%E7%A4%BA.mkv", 1024),
            "演示.mkv"
        );
        // Invalid percent sequences stay literal.
        assert_eq!(percent_decode_video_name("bad%zz.mkv", 1024), "bad%zz.mkv");
        assert_eq!(percent_decode_video_name("%", 1024), "%");
        // Decode output is capped.
        let long = format!("{}.mkv", "a".repeat(500));
        assert!(percent_decode_video_name(&long, 64).len() <= 64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn video_upload_accepts_percent_encoded_name() {
        use crate::video_extract::test_support::{fake_ffmpeg, video_bytes};
        use crate::video_extract::with_ffmpeg_program_async;

        let (_dir, script) = fake_ffmpeg();
        let body = video_bytes("VALID 00:00:01.00 320x240");
        // The Web client sends encodeURIComponent(name); "clip%2Emkv"
        // decodes back to "clip.mkv" and must pass the container check.
        let raw = video_request(&body, "clip%2Emkv", &[]);
        let response = with_ffmpeg_program_async(script, async {
            let state = ws_server_state().await;
            run_video_upload(raw, &state).await
        })
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(response_json(&response)["name"], "clip.mkv");
    }

    #[tokio::test]
    async fn video_upload_rejects_when_concurrency_limit_reached() {
        use crate::video_extract::test_support::video_bytes;

        let mut state = ws_server_state().await;
        state.video_upload_permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let _held = state
            .video_upload_permits
            .clone()
            .try_acquire_owned()
            .expect("hold the only permit");
        let body = video_bytes("VALID 00:00:01.00 320x240");
        let raw = video_request(&body, "clip.mkv", &[]);
        let response = run_video_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 429"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(
            error.contains("too many concurrent video uploads"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn video_upload_response_write_is_bounded_for_slow_readers() {
        // A client that never drains the socket must not pin the connection
        // task: the (multi-MiB) frame response write times out.
        let (mut client, mut server) = tokio::io::duplex(16);
        let start = std::time::Instant::now();
        let result = write_video_json_response_bounded(
            &mut server,
            StatusCode::OK,
            &json!({"payload": "x".repeat(8192)}),
            None,
            Duration::from_millis(100),
        )
        .await;
        assert!(result.is_err(), "a stalled reader must time out the write");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "write timeout fired after {}ms",
            start.elapsed().as_millis()
        );
        drop(client);
    }

    // ---------------------------------------------------------------------
    // POST /upload/pdf — authenticated bounded upload + text extraction.
    // ---------------------------------------------------------------------

    fn pdf_request(body: &[u8], name: &str, extra_headers: &[(&str, &str)]) -> RawRequest {
        let has_host = extra_headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("host"));
        let mut head = format!(
            "POST /upload/pdf HTTP/1.1\r\n{}content-length: {}\r\nx-pdf-name: {}\r\n",
            if has_host { "" } else { "host: x\r\n" },
            body.len(),
            name
        );
        for (key, value) in extra_headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        headers(&bytes).expect("parse pdf upload request")
    }

    async fn run_pdf_upload(raw: RawRequest, state: &ServerState) -> String {
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        handle_pdf_upload(&mut server, raw, state)
            .await
            .expect("pdf upload handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read pdf response");
        String::from_utf8_lossy(&response).into_owned()
    }

    async fn run_pdf_preflight(head: &str, state: &ServerState) -> String {
        let raw = headers(head.as_bytes()).expect("parse pdf preflight");
        let (mut client, mut server) = tokio::io::duplex(1024);
        handle_pdf_upload_preflight(&mut server, raw, state)
            .await
            .expect("pdf preflight handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read pdf preflight");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pdf_upload_success_returns_extracted_text() {
        use crate::pdf_extract::test_support::{fake_pdftotext, pdf_bytes};
        use crate::pdf_extract::with_pdftotext_program_async;

        let (_dir, script) = fake_pdftotext();
        let body = pdf_bytes("VALID", "Certik findings body");
        let raw = pdf_request(&body, "report.pdf", &[]);
        let response = with_pdftotext_program_async(script, async {
            let state = ws_server_state().await;
            run_pdf_upload(raw, &state).await
        })
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let parsed = response_json(&response);
        assert!(
            parsed["attachmentId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(parsed["name"], "report.pdf");
        assert_eq!(parsed["mimeType"], "application/pdf");
        assert_eq!(parsed["text"], "Certik findings body\n");
        assert!(
            parsed["instruction"]
                .as_str()
                .is_some_and(|text| text.contains("[PDF attachment: report.pdf"))
        );
        assert!(
            !response.contains("pi-pdf-"),
            "response must not leak the work directory: {response}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pdf_upload_rejects_wrong_extension_before_body() {
        let state = ws_server_state().await;
        let raw = pdf_request(b"%PDF-1.7\n", "notes.txt", &[]);
        let response = run_pdf_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 415"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(error.contains("unsupported PDF"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pdf_upload_rejects_missing_header() {
        let state = ws_server_state().await;
        let body = b"%PDF-1.7\n";
        let head = format!(
            "POST /upload/pdf HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        let raw = headers(&bytes).expect("parse");
        let response = run_pdf_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(error.contains("X-Pdf-Name"), "{error}");
    }

    #[tokio::test]
    async fn pdf_upload_rejects_when_concurrency_limit_reached() {
        use crate::pdf_extract::test_support::pdf_bytes;

        let mut state = ws_server_state().await;
        state.pdf_upload_permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let _held = state
            .pdf_upload_permits
            .clone()
            .try_acquire_owned()
            .expect("hold the only pdf permit");
        let body = pdf_bytes("VALID", "body");
        let raw = pdf_request(&body, "report.pdf", &[]);
        let response = run_pdf_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 429"), "{response}");
        let parsed = response_json(&response);
        let error = parsed["error"].as_str().expect("error");
        assert!(
            error.contains("too many concurrent PDF uploads"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn pdf_preflight_allows_valid_cross_origin_post() {
        let state = video_upload_state().await;
        let head = "OPTIONS /upload/pdf HTTP/1.1\r\nhost: x\r\norigin: http://127.0.0.1:5173\r\n\
                    access-control-request-method: POST\r\n\
                    access-control-request-headers: authorization, x-pdf-name, content-type\r\n\r\n";
        let response = run_pdf_preflight(head, &state).await;
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert!(
            response.contains("access-control-allow-origin: http://127.0.0.1:5173"),
            "{response}"
        );
        assert!(
            response.contains("access-control-allow-headers: authorization, x-pdf-name, content-type"),
            "{response}"
        );
        state.manager.shutdown().await;
    }

    fn tmux_file_request(
        body: &[u8],
        name: &str,
        session: Option<&str>,
        extra_headers: &[(&str, &str)],
    ) -> RawRequest {
        let has_host = extra_headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("host"));
        let mut head = format!(
            "POST /upload/tmux-file HTTP/1.1\r\n{}content-length: {}\r\nx-file-name: {}\r\n",
            if has_host { "" } else { "host: x\r\n" },
            body.len(),
            name
        );
        if let Some(session) = session {
            head.push_str(&format!("x-tmux-session: {session}\r\n"));
        }
        for (key, value) in extra_headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        headers(&bytes).expect("parse tmux-file upload request")
    }

    async fn run_tmux_file_upload(raw: RawRequest, state: &ServerState) -> String {
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        handle_tmux_file_upload(&mut server, raw, state)
            .await
            .expect("tmux-file upload handler runs");
        drop(server);
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read tmux-file response");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tmux_file_rejects_missing_name_header() {
        let state = ws_server_state().await;
        let raw = headers(
            b"POST /upload/tmux-file HTTP/1.1\r\nhost: x\r\ncontent-length: 3\r\nx-tmux-session: s\r\n\r\nfn\n",
        )
        .expect("parse");
        let response = run_tmux_file_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("missing X-File-Name"), "{response}");
        state.manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tmux_file_rejects_missing_session_header() {
        let state = ws_server_state().await;
        let raw = tmux_file_request(b"fn main() {}", "main.rs", None, &[]);
        let response = run_tmux_file_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("missing X-Tmux-Session"), "{response}");
        state.manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tmux_file_rejects_invalid_session_name() {
        let state = ws_server_state().await;
        let raw = tmux_file_request(b"fn main() {}", "main.rs", Some("-hidden"), &[]);
        let response = run_tmux_file_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        state.manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tmux_file_rejects_declared_oversize() {
        let state = ws_server_state().await;
        let raw = headers(
            b"POST /upload/tmux-file HTTP/1.1\r\nhost: x\r\ncontent-length: 9000000\r\n\
              x-file-name: notes.txt\r\nx-tmux-session: agentlo\r\n\r\n",
        )
        .expect("parse");
        let response = run_tmux_file_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 413"), "{response}");
        state.manager.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tmux_file_rejects_png_magic_with_text_name() {
        let state = ws_server_state().await;
        let body = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let raw = tmux_file_request(&body, "main.rs", Some("agentlo"), &[]);
        let response = run_tmux_file_upload(raw, &state).await;
        assert!(response.starts_with("HTTP/1.1 415"), "{response}");
        state.manager.shutdown().await;
    }

    #[tokio::test]
    async fn tmux_file_preflight_allows_valid_cross_origin_post() {
        let state = video_upload_state().await;
        let raw = headers(
            b"OPTIONS /upload/tmux-file HTTP/1.1\r\nhost: x\r\norigin: http://127.0.0.1:5173\r\n\
              access-control-request-method: POST\r\n\
              access-control-request-headers: authorization, x-file-name, x-tmux-session, content-type\r\n\r\n",
        )
        .expect("parse");
        let (mut client, mut server) = tokio::io::duplex(1024);
        handle_tmux_file_preflight(&mut server, raw, &state)
            .await
            .expect("preflight");
        drop(server);
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.expect("read");
        let response = String::from_utf8_lossy(&response).into_owned();
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert!(response.contains("x-file-name"), "{response}");
        state.manager.shutdown().await;
    }

}
