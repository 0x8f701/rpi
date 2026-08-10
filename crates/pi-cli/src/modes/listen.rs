use std::{
    io,
    net::SocketAddr,
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
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use serde::Serialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
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
/// Server-side ping cadence for established collaboration guests. Browsers
/// answer pings automatically at the protocol level (RFC 6455 5.5.2), so a
/// healthy guest always yields incoming pongs; a silent scripted client does
/// not and is evicted by [`COLLAB_IDLE_TIMEOUT`].
const COLLAB_PING_INTERVAL: Duration = Duration::from_secs(5);
/// A collaboration guest that produces no incoming frame (not even a pong to
/// our pings) within this window is closed cleanly and its participant slot
/// released, so one silent client cannot hold one of the room's participant
/// slots indefinitely.
const COLLAB_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const CONTENT_TYPE_JSON: &str = "application/json";
const REMOTE_EXTENSION_UI_ERROR: &str = "remote interactive extension UI is disabled";

// Transport auth policy shared with the ACP WebSocket server (`ws_auth`);
// `MAX_CONNECTION_TASKS` stays publicly reachable through this module.
pub use super::ws_auth::MAX_CONNECTION_TASKS;
use super::ws_auth::{
    ListenAddressPolicy, authorized, constant_work_eq, load_auth_token, read_token_file,
    websocket_subprotocol,
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
    /// Cadence for server pings to established collaboration guests.
    collab_ping_interval: Duration,
    /// A collaboration guest sending no frame within this window is closed
    /// cleanly and its participant slot released.
    collab_idle: Duration,
}

const DEFAULT_WS_TIMEOUTS: WebSocketTimeouts = WebSocketTimeouts {
    handshake: READ_TIMEOUT,
    collab_ping_interval: COLLAB_PING_INTERVAL,
    collab_idle: COLLAB_IDLE_TIMEOUT,
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
/// stored under the standard agent home directory (`~/.pi/agent/`) so the
/// same certificate persists across restarts: a browser's one-time
/// acceptance of the self-signed certificate keeps working instead of
/// warning on every fresh pair.
const LISTEN_CERT_CACHE_NAME: &str = "listen-cert.pem";
const LISTEN_KEY_CACHE_NAME: &str = "listen-key.pem";

#[derive(Clone)]
pub struct ListenConfig {
    pub address: SocketAddr,
    pub token_file: Option<PathBuf>,
    /// Permit a non-loopback plaintext bind; a token file is optional
    /// (strongly recommended).
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
    /// certificate cached under `~/.pi/agent/`.
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
    /// Resolved advertised origin used to build collaboration links and to
    /// default `collab_start` requests that omit an explicit `baseUrl`.
    /// `None` for wildcard binds without an explicit advertised origin:
    /// link generation fails closed. Browser auth does not consult this —
    /// tokenless browsers are accepted only when same-origin against the
    /// request's own `Host` ([`authorized`]).
    base_url: Option<String>,
}

pub async fn start(
    application: Application,
    extension_ui: ExtensionUiAdapter,
    config: ListenConfig,
) -> Result<ListenHandle> {
    let policy = if config.allow_insecure_remote {
        ListenAddressPolicy::AllowPlaintextRemote
    } else if !config.plaintext {
        // TLS is the default transport; a non-loopback HTTPS listener is not
        // plaintext, so it needs no insecure-remote opt-in.
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
    let manager = SessionRuntimeManager::new(application, extension_ui, config.session_factory).await;
    let collab = CollabService::new(manager.clone());
    let state = ServerState {
        manager: manager.clone(),
        collab: collab.clone(),
        token: token.map(Arc::from),
        base_url: advertised_base_url(address, config.advertised_origin.as_deref(), tls_active),
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
                rest.strip_prefix(':').ok_or_else(|| {
                    anyhow!("--listen-advertised-origin has an invalid host")
                })?,
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

/// Cache directory for the auto-generated self-signed certificate
/// (`~/.pi/agent/`, the standard agent home layout).
fn listen_cert_cache_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("home directory is unavailable for the self-signed certificate cache")
        })?;
    Ok(PathBuf::from(home).join(".pi").join("agent"))
}

/// Pin a file or directory to owner-only permissions (the private key and
/// its cache directory). Self-signed key material is a credential; default
/// umask-derived permissions could expose it to other local users.
#[cfg(unix)]
fn ensure_owner_only_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", path.display()))
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
    // The cache directory holds the private key; pin it to owner-only (0700)
    // so a permissive umask cannot expose the key. `create_dir_all` is
    // idempotent, so this also hardens a directory created by older builds.
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating certificate cache directory {}", dir.display()))?;
    #[cfg(unix)]
    ensure_owner_only_permissions(&dir, 0o700)?;
    if tokio::fs::try_exists(&cert_path).await.unwrap_or(false)
        && tokio::fs::try_exists(&key_path).await.unwrap_or(false)
    {
        // Reuse path: a key written before the 0600 enforcement may still
        // carry permissive permissions; harden it here too.
        #[cfg(unix)]
        ensure_owner_only_permissions(&key_path, 0o600)?;
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
    params.distinguished_name.push(DnType::CommonName, "rpi-listener");
    let signing_key = KeyPair::generate().context("generating self-signed key pair")?;
    let cert = params
        .self_signed(&signing_key)
        .context("generating self-signed certificate")?;
    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();
    tokio::fs::write(&cert_path, cert_pem).await.with_context(|| {
        format!(
            "writing self-signed certificate to {}",
            cert_path.display()
        )
    })?;
    tokio::fs::write(&key_path, key_pem).await.with_context(|| {
        format!(
            "writing self-signed private key to {}",
            key_path.display()
        )
    })?;
    // The private key is a credential: owner-only (0600) on Unix.
    #[cfg(unix)]
    ensure_owner_only_permissions(&key_path, 0o600)?;
    Ok((cert_path, key_path))
}

/// Build the TLS acceptor for the control plane listener.
///
/// With an explicit `--listen-cert`/`--listen-key` pair (enforced at CLI
/// parse time) the PEM files are loaded as-is, e.g. a Let's Encrypt
/// fullchain and private key. Without them a self-signed certificate is
/// generated with rcgen and cached under `~/.pi/agent/` so the same
/// certificate persists across restarts. The handshake and HTTP/WebSocket
/// processing share the same stream type, so TLS termination slots in before
/// the request parser without changing anything downstream.
async fn make_tls_acceptor(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    bind: SocketAddr,
) -> Result<TlsAcceptor> {
    // rustls 0.23 requires an explicit CryptoProvider; install ring (already
    // in the dependency tree via rcgen/reqwest) before building ServerConfig.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let (cert_path, key_path) = match (cert_path, key_path) {
        (None, None) => self_signed_cert_paths(bind).await?,
        _ => bail!("--listen-cert and --listen-key must be provided together"),
    };
    let cert_pem = tokio::fs::read(&cert_path)
        .await
        .with_context(|| format!("reading TLS certificate {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(&key_path)
        .await
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
                let state = state.clone();
                // Complete the TLS handshake on the accept loop before spawning
                // the connection handler. TlsAcceptor::accept returns a future
                // that is not Send (it borrows the acceptor and the handshake
                // state machine across awaits), so it cannot live inside the
                // spawned future that JoinSet requires to be Send. Performing it
                // here yields a fully established TlsStream<TcpStream> — which is
                // Send — and the spawned handler stays Send. TLS handshake
                // failures (a plaintext client on the TLS port, a malformed
                // ClientHello) drop only that connection; the accept loop keeps
                // serving.
                if let Some(acceptor) = &tls {
                    match acceptor.accept(tcp_stream).await {
                        Ok(tls_stream) => {
                            connections.spawn(async move {
                                let _ = handle_connection(tls_stream, state).await;
                            });
                        }
                        Err(_) => continue,
                    }
                } else {
                    connections.spawn(async move {
                        let _ = handle_connection(tcp_stream, state).await;
                    });
                }
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
            return collab_websocket_connection(stream, raw, connection, protocol, DEFAULT_WS_TIMEOUTS).await;
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
        if !authorized(&raw.headers, state.token.as_deref(), true) && protocol.is_none()
        {
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
    if !authorized(&raw.headers, state.token.as_deref(), true) {
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
            let base_url = match base_url {
                Some(requested) => requested,
                None => match &state.base_url {
                    Some(advertised) => advertised.clone(),
                    None => {
                        return RpcResponse::failure(
                            id,
                            name,
                            "collaboration link generation requires an advertised origin: pass --listen-advertised-origin <URL> for wildcard binds (or an explicit baseUrl in the request)"
                                .to_owned(),
                        );
                    }
                },
            };
            match state.collab.start(session_id.as_deref(), &base_url).await {
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

async fn read_http_request<S>(stream: &mut S) -> std::result::Result<RawRequest, RequestError>
where
    S: AsyncRead + Unpin,
{
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
        ),
    )
    .await
    {
        Ok(result) => result.context("upgrading collaboration WebSocket")?,
        Err(_) => bail!("collaboration WebSocket handshake timed out"),
    };

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

    // Idle watchdog: the server pings the guest on a fixed cadence; browsers
    // answer automatically with pongs at the protocol level (RFC 6455
    // 5.5.2), so every live guest keeps producing incoming frames. A guest
    // that sends nothing — not even a pong — within `collab_idle` is closed
    // cleanly and its participant slot released instead of being pinned
    // indefinitely.
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + timeouts.collab_ping_interval,
        timeouts.collab_ping_interval,
    );
    // A prompt dispatch can outlast several intervals; catch up with a single
    // ping on the next aligned tick instead of bursting a backlog.
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle = tokio::time::sleep(timeouts.collab_idle);
    tokio::pin!(idle);
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
            _ = ping.tick() => {
                if write.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return Ok(());
                }
            }
            _ = &mut idle => {
                let _ = write.send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "collaboration connection idle".into(),
                }))).await;
                return Ok(());
            }
            incoming = read.next() => match incoming {
                Some(Ok(frame)) => {
                    match frame {
                        Message::Binary(frame) => {
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
                        Message::Ping(payload) => {
                            write.send(Message::Pong(payload)).await
                                .context("sending collaboration pong")?;
                        }
                        // Pongs are protocol-level replies to our pings (and
                        // the browser's automatic pong), never application
                        // data; a healthy guest may send them at any time.
                        Message::Pong(_) => {}
                        Message::Close(_) => return Ok(()),
                        Message::Text(_) | Message::Frame(_) => {
                            let _ = write.send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Unsupported,
                                reason: "encrypted binary messages required".into(),
                            }))).await;
                            return Ok(());
                        }
                    }
                    // Any inbound frame (a command, a ping, or the browser's
                    // automatic pong) proves the guest is alive; restart the
                    // idle window only after it is handled, so a long-running
                    // prompt dispatch never counts against a guest that is
                    // legitimately awaiting the response.
                    idle.as_mut().reset(tokio::time::Instant::now() + timeouts.collab_idle);
                }
                None => return Ok(()),
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
    let mut prefix = raw.raw_headers;
    prefix.extend_from_slice(&raw.remainder);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RPC_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_RPC_MESSAGE_BYTES))
        .max_write_buffer_size(2 * MAX_RPC_MESSAGE_BYTES);
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
        ),
    )
    .await
    {
        Ok(result) => result.context("upgrading control plane WebSocket")?,
        Err(_) => bail!("control plane WebSocket handshake timed out"),
    };

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

async fn serve_web_page<S>(stream: &mut S) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
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

async fn write_plain_response<S>(
    stream: &mut S,
    status: StatusCode,
    message: &str,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response(stream, status, "text/plain; charset=utf-8", message.as_bytes()).await
}

async fn write_json_response<T: Serialize, S>(
    stream: &mut S,
    status: StatusCode,
    value: &T,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value).context("serializing HTTP RPC response")?;
    write_response(stream, status, CONTENT_TYPE_JSON, &body).await
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
        open_collab_socket_with(connection, DEFAULT_WS_TIMEOUTS).await
    }

    async fn open_collab_socket_with(
        connection: CollabConnection,
        timeouts: WebSocketTimeouts,
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
            collab_websocket_connection(stream, raw, connection, protocol.to_owned(), timeouts).await
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
        let mut model = pi_ai::Model::default();
        model.id = "ws-stall-model".into();
        model.name = "ws-stall-model".into();
        model.api = "ws-stall-api".into();
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 4,
        });
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
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
        })
        .expect("session");
        let application = pi_coding::Application::new(session).await;
        registration.unregister();
        let manager = SessionRuntimeManager::new(
            application,
            ExtensionUiAdapter::default(),
            None,
        )
        .await;
        ServerState {
            manager: manager.clone(),
            collab: CollabService::new(manager),
            token: None,
            base_url: None,
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
    fn auth_policy_keeps_default_strict_and_allows_plaintext_and_tls_remote() {
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
            // The explicit plaintext opt-in permits non-loopback binds with a
            // token (recommended) or without one (tokenless LAN access).
            assert_eq!(
                load_auth_token(
                    address,
                    None,
                    "--listen",
                    ListenAddressPolicy::AllowPlaintextRemote,
                )
                .unwrap(),
                None
            );
            assert_eq!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::AllowPlaintextRemote,
                )
                .unwrap(),
                Some(b"fixture-value".to_vec())
            );
            // TLS is not plaintext: non-loopback binds pass without the
            // insecure-remote opt-in, with or without a token.
            assert_eq!(
                load_auth_token(
                    address,
                    None,
                    "--listen",
                    ListenAddressPolicy::AllowTlsRemote,
                )
                .unwrap(),
                None
            );
            assert_eq!(
                load_auth_token(
                    address,
                    Some(&token),
                    "--listen",
                    ListenAddressPolicy::AllowTlsRemote,
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

        assert!(matches!(next_collab_message(&mut socket).await, Message::Text(_)));
        assert!(matches!(next_collab_message(&mut socket).await, Message::Binary(_)));
        service.stop(&room_id).await.expect("stop room");
        assert_away_close(next_collab_message(&mut socket).await);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn stalled_collab_handshake_releases_participant_lease() {
        let (service, room_id, connection) = test_collab_connection().await;
        assert_eq!(service.status(Some(&room_id)).await[0].participants, 1);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind listener");
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
                    collab_ping_interval: Duration::from_secs(3600),
                    collab_idle: Duration::from_secs(3600),
                },
            )
            .await
        });
        // A connected-but-silent client: the handshake cannot complete and
        // the client never closes, so the server-side timeout must fire.
        let _client = TcpStream::connect(address).await.expect("connect stalled client");

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
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind listener");
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
                    collab_ping_interval: Duration::from_secs(3600),
                    collab_idle: Duration::from_secs(3600),
                },
            )
            .await
        });
        let _client = TcpStream::connect(address).await.expect("connect stalled client");

        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stalled /ws handshake must release the connection task")
            .expect("server task")
            .expect_err("stalled /ws handshake must time out instead of completing");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn silent_collab_guest_is_evicted_and_releases_participant_lease() {
        let (service, room_id, connection) = test_collab_connection().await;
        let (mut socket, server) = open_collab_socket_with(
            connection,
            WebSocketTimeouts {
                handshake: Duration::from_secs(2),
                collab_ping_interval: Duration::from_millis(20),
                collab_idle: Duration::from_millis(120),
            },
        )
        .await;
        // The normal handshake, hello, and snapshot are unaffected.
        assert!(matches!(next_collab_message(&mut socket).await, Message::Text(_)));
        assert!(matches!(next_collab_message(&mut socket).await, Message::Binary(_)));
        assert_eq!(service.status(Some(&room_id)).await[0].participants, 1);

        // The guest goes silent. A tungstenite client auto-pongs every Ping it
        // reads (RFC 6455 5.5.2: tungstenite queues the Pong inside `read` and
        // flushes it on the next poll), so polling `socket.next()` here would
        // itself keep the guest "alive" and starve the watchdog — the per-read
        // timeout never elapses because pings arrive faster than it. Instead
        // the client stops reading entirely, modeling a dead network peer, and
        // we first wait for the server's idle watchdog to evict it and release
        // the participant lease. The bound is a safety ceiling; eviction lands
        // at `collab_idle` (120ms), so it is not a mask for a product bug.
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
                collab_ping_interval: Duration::from_millis(20),
                collab_idle: Duration::from_millis(300),
            },
        )
        .await;
        assert!(matches!(next_collab_message(&mut socket).await, Message::Text(_)));
        assert!(matches!(next_collab_message(&mut socket).await, Message::Binary(_)));

        // A healthy browser never sends application frames unprompted, but it
        // answers server pings with pongs at the protocol level (RFC 6455
        // 5.5.2). A guest that keeps answering stays connected across many
        // idle windows instead of losing its slot.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket.send(Message::Pong(payload)).await.expect("send pong");
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
        assert!(!server.is_finished(), "server must still be serving the guest");

        // Clean teardown: closing the client releases the lease.
        drop(socket);
        server.await.expect("server task").expect("clean exit after client close");
        assert_eq!(service.status(Some(&room_id)).await[0].participants, 0);
    }

    #[test]
    fn collaboration_browser_path_accepts_only_valid_room_ids() {
        assert_eq!(collab_room_id("/collab/ws/room-123_abc"), Some("room-123_abc"));
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
}