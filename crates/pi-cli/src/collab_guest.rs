//! CLI guest client for live collaboration rooms (the `/join` side).
//!
//! Speaks the collab wire protocol directly against the host's
//! `--listen` server:
//!
//! 1. Connect to `ws(s)://host:port/collab/ws/<roomId>` presenting the
//!    `rpi-collab.<b64url(SHA-256(key))>` subprotocol. The role key never
//!    leaves the link fragment — the server sees only its capability hash.
//! 2. Receive the plaintext `hello` JSON (`{type:"hello",version,roomId,role,
//!    epoch}`) and derive the per-connection directional keys
//!    ([`pi_coding::collab::derive_connection_key`]).
//! 3. Exchange encrypted frames: server→guest `snapshot`/`event` payloads,
//!    guest→server `command` payloads (prompt/abort). All frames are
//!    AES-256-GCM sealed with strictly increasing sequence numbers; the
//!    receive window rejects replays and out-of-order frames, and a reconnect
//!    starts a fresh epoch so captured frames from a previous connection
//!    cannot be replayed.
//!
//! View-only links decrypt only the view stream; write commands are refused
//! locally before any frame is sealed (the relay enforces the same denial
//! host-authoritatively).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use pi_coding::collab::{
    CollabLink, FrameDirection, ReceiveWindow, SendCounter, capability, derive_connection_key,
    open_frame, parse_frame_header, seal_frame,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Message as WsMessage,
        client::IntoClientRequest,
        protocol::WebSocketConfig,
    },
};

/// `Sec-WebSocket-Protocol` prefix for collab guest authentication.
const SUBPROTOCOL_PREFIX: &str = "rpi-collab.";

/// Wire version carried in the hello exchange.
const HELLO_VERSION: u64 = 1;

/// Outbound queue capacity for one guest connection.
const OUTBOUND_QUEUE_CAPACITY: usize = 64;

/// Automatic reconnect attempts after an unexpected disconnect (each with
/// doubling backoff). A clean `/leave` or a server close stops immediately.
const RECONNECT_ATTEMPTS: usize = 5;

/// Base backoff between reconnect attempts, in milliseconds.
const RECONNECT_BASE_DELAY_MS: u64 = 400;

/// Commands a writable guest can send into the room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestCommand {
    /// Run a user prompt on the host session.
    Prompt(String),
    /// Abort the host's current run.
    Abort,
    /// Leave the room: closes the connection cleanly and stops reconnecting.
    Leave,
}

/// Default sink for the CLI/REPL command layer: renders decrypted guest
/// payloads as deterministic lines on stdout. The E2E suite asserts against
/// these line shapes.
#[derive(Default)]
pub struct PrintingGuestSink;

impl CollabGuestSink for PrintingGuestSink {
    fn on_snapshot(&mut self, snapshot: Value) {
        let entries = snapshot["entries"].as_array().map_or(0, Vec::len);
        let truncated = snapshot["truncated"].as_bool().unwrap_or(false);
        println!("[collab] history ({entries} entries{})", if truncated { ", truncated" } else { "" });
        for entry in snapshot["entries"].as_array().into_iter().flatten() {
            if let Some(text) = entry["message"]["content"]
                .as_array()
                .and_then(|blocks| blocks.first())
                .and_then(|block| block["text"].as_str())
            {
                println!("[collab] · {text}");
            }
        }
    }

    fn on_event(&mut self, event: Value) {
        let kind = event["type"].as_str().unwrap_or("event");
        println!("[collab] event {kind}");
        // Tool cards and assistant text are the interesting live content.
        if let Some(text) = event["text"].as_str() {
            println!("[collab] · {text}");
        }
        if let Some(message) = event["message"].as_str() {
            println!("[collab] · {message}");
        }
    }

    fn on_write_rejected(&mut self, command: &str) {
        println!("[collab] {command} rejected: view-only guests cannot write");
    }

    fn on_disconnect(&mut self, reason: &str) {
        println!("[collab] disconnected: {reason}");
    }
}

/// Consumer of decrypted server payloads. Implementations render transcripts
/// (REPL lines, TUI panel) or record for tests.
pub trait CollabGuestSink: Send {
    /// Authoritative bounded history from the host recorder; also re-sent
    /// after every reconnect (the client must replace, not append).
    fn on_snapshot(&mut self, snapshot: Value);
    /// One live transcript/tool/state event.
    fn on_event(&mut self, event: Value);
    /// Encrypted host response to a guest prompt/abort command.
    fn on_response(&mut self, _response: Value) {}
    /// A write was refused locally because the link is view-only.
    fn on_write_rejected(&mut self, command: &str);
    /// The connection dropped (before any automatic reconnect attempt).
    fn on_disconnect(&mut self, reason: &str);
}

/// A joined guest session handle: keeps the background task plus the command
/// channel used to prompt/abort/leave.
pub struct CollabGuestHandle {
    pub role: pi_coding::collab::CollabRole,
    pub room_id: String,
    commands: mpsc::Sender<GuestCommand>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl CollabGuestHandle {
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.role.is_writable()
    }

    /// Ask the guest to run a prompt on the host. View-only links are
    /// refused locally (nothing is sealed or sent).
    pub async fn prompt(&self, message: String) -> Result<()> {
        self.commands
            .send(GuestCommand::Prompt(message))
            .await
            .map_err(|_| anyhow!("collab guest is no longer running"))
    }

    /// Ask the guest to abort the host's current run.
    pub async fn abort(&self) -> Result<()> {
        self.commands
            .send(GuestCommand::Abort)
            .await
            .map_err(|_| anyhow!("collab guest is no longer running"))
    }

    /// Leave the room and wait for the guest task to finish.
    pub async fn leave(self) -> Result<()> {
        let _ = self.commands.send(GuestCommand::Leave).await;
        match self.task.await {
            Ok(result) => result,
            Err(error) => Err(anyhow!(error).context("joining collab guest task")),
        }
    }
}

/// Derive the `ws(s)://host:port/collab/ws/<roomId>` URL from a join link,
/// dropping the fragment (the key) so it is never transmitted.
pub fn ws_url_from_link(link: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(link).map_err(|_| anyhow!("invalid collab link URL"))?;
    let scheme = match parsed.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => bail!("collab links must use http or https (got scheme {other:?})"),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("collab link has no host"))?;
    let mut ws_url = format!("{scheme}://{host}");
    if let Some(port) = parsed.port() {
        ws_url.push_str(&format!(":{port}"));
    }
    ws_url.push_str(parsed.path());
    Ok(ws_url)
}

/// Start a guest session in the background. The returned handle drives
/// prompt/abort/leave; decrypted payloads flow to `sink` on a dedicated task.
/// `original_link` is the join link as typed (its fragment holds the role
/// key; it is only used to derive the connection URL and is never sent).
pub fn spawn_guest(
    link: CollabLink,
    original_link: String,
    mut sink: Box<dyn CollabGuestSink>,
) -> CollabGuestHandle {
    let (commands_tx, commands_rx) = mpsc::channel::<GuestCommand>(16);
    let role = link.secret.role;
    let room_id = link.room_id.clone();
    let task = tokio::spawn(async move {
        run_guest(&link, &original_link, commands_rx, sink.as_mut()).await
    });
    CollabGuestHandle {
        role,
        room_id,
        commands: commands_tx,
        task,
    }
}

/// Run a guest connection loop, reconnecting on unexpected drops (bounded).
pub async fn run_guest(
    link: &CollabLink,
    original_link: &str,
    mut commands: mpsc::Receiver<GuestCommand>,
    sink: &mut dyn CollabGuestSink,
) -> Result<()> {
    let mut attempt = 0usize;
    loop {
        match run_connection(link, original_link, &mut commands, sink).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                sink.on_disconnect(&format!("{error:#}"));
                if attempt >= RECONNECT_ATTEMPTS {
                    return Err(error.context(format!(
                        "collab guest gave up after {attempt} reconnect attempts"
                    )));
                }
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(
                    RECONNECT_BASE_DELAY_MS * (1 << (attempt - 1)),
                ))
                .await;
            }
        }
    }
}

/// Per-connection crypto state, bound to the epoch issued by the host.
struct Connection {
    epoch: [u8; pi_coding::collab::EPOCH_LEN],
    s2c_key: [u8; pi_coding::collab::KEY_LEN],
    c2s_key: [u8; pi_coding::collab::KEY_LEN],
    send: SendCounter,
    receive: ReceiveWindow,
}

impl Connection {
    fn new(link: &CollabLink, epoch: [u8; pi_coding::collab::EPOCH_LEN]) -> Result<Self> {
        let s2c_key = derive_connection_key(
            &link.secret.key,
            &epoch,
            FrameDirection::ServerToClient,
        )?;
        let c2s_key = derive_connection_key(
            &link.secret.key,
            &epoch,
            FrameDirection::ClientToServer,
        )?;
        Ok(Self {
            epoch,
            s2c_key,
            c2s_key,
            send: SendCounter::new(),
            receive: ReceiveWindow::new(),
        })
    }
}

async fn run_connection(
    link: &CollabLink,
    original_link: &str,
    commands: &mut mpsc::Receiver<GuestCommand>,
    sink: &mut dyn CollabGuestSink,
) -> Result<()> {
    let ws_url = ws_url_from_link(original_link)?;
    let capability = capability(&link.secret.key);
    let subprotocol = format!(
        "{SUBPROTOCOL_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(capability)
    );
    let tls = ws_url.starts_with("wss://");
    let mut request = ws_url
        .into_client_request()
        .map_err(|_| anyhow!("building collab WebSocket request"))?;
    request.headers_mut().insert(
        http::header::SEC_WEBSOCKET_PROTOCOL,
        http::HeaderValue::from_str(&subprotocol)
            .map_err(|_| anyhow!("collab subprotocol is not a valid header value"))?,
    );
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(pi_coding::collab::MAX_FRAME_BYTES);
    let (websocket, response) = connect_async_with_config(request, Some(config), tls)
        .await
        .map_err(|error| anyhow!("connecting to collab room: {error}"))?;
    // The server must echo the offered subprotocol (RFC 6455: at most one).
    let echoed = response
        .headers()
        .get(http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if echoed != subprotocol {
        bail!("collab server did not accept the guest subprotocol");
    }

    let (mut websocket_write, mut websocket_read) = websocket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<WsMessage>(OUTBOUND_QUEUE_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            websocket_write
                .send(message)
                .await
                .map_err(|error| anyhow!("sending collab frame: {error}"))?;
        }
        Ok::<(), anyhow::Error>(())
    });

    // The connection is not usable until the hello arrives.
    let mut connection: Option<Connection> = None;
    let exit = loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(GuestCommand::Prompt(message)) => {
                    if !link.secret.role.is_writable() {
                        sink.on_write_rejected("prompt");
                        continue;
                    }
                    let Some(conn) = connection.as_mut() else {
                        break Err(anyhow!("cannot prompt before the room hello"));
                    };
                    let payload = json!({
                        "type": "command",
                        "command": "prompt",
                        "message": message,
                        "id": uuid::Uuid::new_v4().to_string(),
                    });
                    if let Err(error) = seal_and_send(conn, link, &outbound_tx, &payload).await {
                        break Err(error);
                    }
                }
                Some(GuestCommand::Abort) => {
                    if !link.secret.role.is_writable() {
                        sink.on_write_rejected("abort");
                        continue;
                    }
                    let Some(conn) = connection.as_mut() else {
                        break Err(anyhow!("cannot abort before the room hello"));
                    };
                    let payload = json!({
                        "type": "command",
                        "command": "abort",
                        "id": uuid::Uuid::new_v4().to_string(),
                    });
                    if let Err(error) = seal_and_send(conn, link, &outbound_tx, &payload).await {
                        break Err(error);
                    }
                }
                Some(GuestCommand::Leave) | None => break Ok(()),
            },
            incoming = websocket_read.next() => match incoming {
                Some(Ok(WsMessage::Text(text))) => {
                    match &connection {
                        None => match parse_hello(&text, link) {
                            Ok(epoch) => connection = Some(Connection::new(link, epoch)?),
                            Err(error) => break Err(error),
                        },
                        Some(_) => break Err(anyhow!("unexpected plaintext message after hello")),
                    }
                }
                Some(Ok(WsMessage::Binary(frame))) => {
                    let Some(conn) = connection.as_mut() else {
                        break Err(anyhow!("encrypted frame before hello"));
                    };
                    if let Err(error) = handle_inbound_frame(conn, link, &frame, sink).await {
                        break Err(error);
                    }
                }
                Some(Ok(WsMessage::Ping(payload))) => {
                    if outbound_tx.send(WsMessage::Pong(payload)).await.is_err() {
                        break Err(anyhow!("collab outbound queue closed"));
                    }
                }
                Some(Ok(WsMessage::Pong(_) | WsMessage::Frame(_))) => {}
                Some(Ok(WsMessage::Close(_))) | None => break Ok(()),
                Some(Err(error)) => break Err(anyhow!("collab WebSocket error: {error}")),
            },
        }
    };

    writer.abort();
    exit
}

/// Validate the plaintext hello and return the issued epoch.
fn parse_hello(
    text: &str,
    link: &CollabLink,
) -> Result<[u8; pi_coding::collab::EPOCH_LEN]> {
    let hello: Value =
        serde_json::from_str(text).map_err(|_| anyhow!("malformed collab hello"))?;
    if hello["type"] != "hello" {
        bail!("expected a collab hello message");
    }
    if hello["version"].as_u64() != Some(HELLO_VERSION) {
        bail!("unsupported collab hello version");
    }
    if hello["roomId"].as_str() != Some(link.room_id.as_str()) {
        bail!("collab hello names a different room");
    }
    let role = match hello["role"].as_str() {
        Some("control") => pi_coding::collab::CollabRole::Control,
        Some("view") => pi_coding::collab::CollabRole::View,
        _ => bail!("collab hello carries an unknown role"),
    };
    if role != link.secret.role {
        bail!("collab server assigned a different role than the link grants");
    }
    let epoch_b64 = hello["epoch"]
        .as_str()
        .ok_or_else(|| anyhow!("collab hello is missing the epoch"))?;
    let epoch_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(epoch_b64)
        .map_err(|_| anyhow!("collab hello epoch is not valid base64url"))?;
    let epoch: [u8; pi_coding::collab::EPOCH_LEN] = epoch_bytes.try_into().map_err(|_| {
        anyhow!("collab hello epoch must decode to exactly {} bytes", pi_coding::collab::EPOCH_LEN)
    })?;
    Ok(epoch)
}

/// Decrypt one server→client frame, enforce strict ordering, and dispatch to
/// the sink. The receive window advances only after successful decryption.
async fn handle_inbound_frame(
    conn: &mut Connection,
    link: &CollabLink,
    frame: &[u8],
    sink: &mut dyn CollabGuestSink,
) -> Result<()> {
    let (epoch, direction, seq, _body) = parse_frame_header(frame)
        .ok_or_else(|| anyhow!("collab frame has a malformed header"))?;
    if epoch != conn.epoch || direction != FrameDirection::ServerToClient {
        bail!("collab frame header does not match this connection");
    }
    if seq != conn.receive.next_expected() {
        bail!("collab frame is out of order or a replay");
    }
    let plaintext = open_frame(
        &conn.s2c_key,
        &link.room_id,
        FrameDirection::ServerToClient,
        &conn.epoch,
        seq,
        frame,
    )
    .map_err(|_| anyhow!("collab frame authentication failed"))?;
    conn.receive.accept(seq).map_err(|error| anyhow!("collab receive window: {error:?}"))?;
    let payload: Value =
        serde_json::from_slice(&plaintext).map_err(|_| anyhow!("collab payload is not JSON"))?;
    match payload["type"].as_str() {
        Some("snapshot") => {
            sink.on_snapshot(payload.get("snapshot").cloned().unwrap_or(Value::Null));
        }
        Some("event") => {
            sink.on_event(payload.get("event").cloned().unwrap_or(Value::Null));
        }
        Some("response") => sink.on_response(payload),
        other => bail!("unknown collab payload type {other:?}"),
    }
    Ok(())
}

/// Seal a guest→server command and enqueue it.
async fn seal_and_send(
    conn: &mut Connection,
    link: &CollabLink,
    outbound_tx: &mpsc::Sender<WsMessage>,
    payload: &Value,
) -> Result<()> {
    let seq = conn
        .send
        .next()
        .ok_or_else(|| anyhow!("collab send sequence exhausted"))?;
    let plaintext =
        serde_json::to_vec(payload).map_err(|_| anyhow!("serializing collab command"))?;
    let frame = seal_frame(
        &conn.c2s_key,
        &link.room_id,
        FrameDirection::ClientToServer,
        &conn.epoch,
        seq,
        &plaintext,
    )?;
    outbound_tx
        .send(WsMessage::Binary(frame.into()))
        .await
        .map_err(|_| anyhow!("collab outbound queue closed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(n: u8) -> [u8; pi_coding::collab::EPOCH_LEN] {
        [n; pi_coding::collab::EPOCH_LEN]
    }

    fn control_link(room: &str) -> CollabLink {
        CollabLink {
            room_id: room.to_owned(),
            secret: pi_coding::collab::CollabSecret {
                role: pi_coding::collab::CollabRole::Control,
                key: [7u8; pi_coding::collab::KEY_LEN],
            },
        }
    }

    fn view_link(room: &str) -> CollabLink {
        CollabLink {
            room_id: room.to_owned(),
            secret: pi_coding::collab::CollabSecret {
                role: pi_coding::collab::CollabRole::View,
                key: [9u8; pi_coding::collab::KEY_LEN],
            },
        }
    }

    #[test]
    fn ws_url_from_link_maps_schemes_and_drops_fragment() {
        let control = pi_coding::collab::CollabSecret {
            role: pi_coding::collab::CollabRole::Control,
            key: [1u8; 32],
        };
        let http = pi_coding::collab::format_link("http://127.0.0.1:4321", "roomA", &control);
        assert_eq!(
            ws_url_from_link(&http).expect("http link"),
            "ws://127.0.0.1:4321/collab/ws/roomA"
        );
        let https = pi_coding::collab::format_link("https://collab.example:8443", "roomB", &control);
        assert_eq!(
            ws_url_from_link(&https).expect("https link"),
            "wss://collab.example:8443/collab/ws/roomB"
        );
        // Default ports are not emitted redundantly.
        let default_port =
            pi_coding::collab::format_link("http://collab.example", "roomC", &control);
        assert_eq!(
            ws_url_from_link(&default_port).expect("default port link"),
            "ws://collab.example/collab/ws/roomC"
        );
        // The fragment (role key) is never part of the connection URL.
        assert!(!ws_url_from_link(&http).expect("http link").contains('#'));
        assert!(!ws_url_from_link(&http).expect("http link").contains("c="));
        // Non-http schemes and junk are rejected.
        assert!(ws_url_from_link("ftp://host/collab/ws/room#c=AAAA").is_err());
        assert!(ws_url_from_link("not a url").is_err());
    }

    #[test]
    fn hello_parse_accepts_valid_handshake() {
        let link = control_link("roomA");
        let hello = json!({
            "type": "hello",
            "version": 1,
            "roomId": "roomA",
            "role": "control",
            "epoch": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(epoch(3)),
        });
        assert_eq!(
            parse_hello(&hello.to_string(), &link).expect("hello"),
            epoch(3)
        );
    }

    #[test]
    fn hello_parse_rejects_mismatches_and_malformed_input() {
        let link = control_link("roomA");
        let good = json!({
            "type": "hello",
            "version": 1,
            "roomId": "roomA",
            "role": "control",
            "epoch": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(epoch(1)),
        });
        // Wrong room id.
        let mut wrong_room = good.clone();
        wrong_room["roomId"] = json!("roomB");
        assert!(parse_hello(&wrong_room.to_string(), &link).is_err());
        // Wrong role (server assigning view to a control link is a hard error).
        let mut wrong_role = good.clone();
        wrong_role["role"] = json!("view");
        assert!(parse_hello(&wrong_role.to_string(), &link).is_err());
        // Unsupported version.
        let mut wrong_version = good.clone();
        wrong_version["version"] = json!(2);
        assert!(parse_hello(&wrong_version.to_string(), &link).is_err());
        // Not a hello at all.
        assert!(parse_hello("{\"type\":\"bye\"}", &link).is_err());
        assert!(parse_hello("not json", &link).is_err());
        // Missing / malformed / short epoch.
        let mut no_epoch = good.clone();
        no_epoch["epoch"] = Value::Null;
        assert!(parse_hello(&no_epoch.to_string(), &link).is_err());
        let mut bad_epoch = good.clone();
        bad_epoch["epoch"] = json!("!!!");
        assert!(parse_hello(&bad_epoch.to_string(), &link).is_err());
        let mut short_epoch = good.clone();
        short_epoch["epoch"] = json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; 4]));
        assert!(parse_hello(&short_epoch.to_string(), &link).is_err());
    }

    #[test]
    fn roles_report_writability() {
        assert!(control_link("roomA").secret.role.is_writable());
        assert!(!view_link("roomA").secret.role.is_writable());
    }
}
