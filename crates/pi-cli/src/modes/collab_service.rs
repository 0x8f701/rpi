//! Host-owned encrypted collaboration rooms for the listen control plane.
//!
//! This module deliberately keeps the public surface small: callers can start,
//! inspect, and stop rooms through [`CollabService`]. The listener uses the
//! crate-private authenticated connection type to perform the plaintext hello
//! handshake and then carries only encrypted binary payloads.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine as _;
use pi_coding::collab::{
    CollabRole, CollabRoomKeys, CollabSecret, EPOCH_LEN, FrameDirection, KEY_LEN,
    MAX_FRAME_BYTES, ReceiveWindow, SendCounter, capability, capability_eq,
    derive_connection_key, format_link, generate_room_keys, new_room_id, open_frame,
    parse_frame_header, public_value, seal_frame,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{RwLock, broadcast, watch};

use super::{
    rpc::{RpcCommand, RpcResponse},
    session_runtime_manager::SessionRuntimeManager,
};

/// Maximum simultaneously active collaboration rooms in one listener.
pub const MAX_COLLAB_ROOMS: usize = 8;
/// Maximum live guests in one room.
pub const MAX_COLLAB_PARTICIPANTS: usize = 8;
/// Maximum recorder entries retained in the initial snapshot.
pub const MAX_COLLAB_SNAPSHOT_ENTRIES: usize = 2_048;
/// Maximum serialized recorder-entry bytes retained before envelope overhead.
pub const MAX_COLLAB_SNAPSHOT_BYTES: usize = MAX_FRAME_BYTES - 8 * 1024;
/// Maximum distinct connections a room may issue before it must be restarted.
pub const MAX_COLLAB_CONNECTION_EPOCHS: usize = 4_096;

const COLLAB_PROTOCOL_PREFIX: &str = "rpi-collab.";
const COLLAB_VERSION: u8 = 1;

/// Result returned by `collab_start`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabStartResult {
    pub room_id: String,
    pub session_id: String,
    pub control_link: String,
    pub view_link: String,
}

/// One room row returned by `collab_status`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CollabRoomInfo {
    pub room_id: String,
    pub session_id: String,
    pub participants: usize,
    pub control_participants: usize,
    pub view_participants: usize,
    pub participant_limit: usize,
    pub running: bool,
}

/// Plaintext server-issued connection hello. No capability or room key is
/// included; every subsequent application payload is encrypted.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollabHello {
    pub(crate) r#type: &'static str,
    pub(crate) version: u8,
    pub(crate) room_id: String,
    pub(crate) role: &'static str,
    pub(crate) epoch: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum CollabClientMessage {
    Command {
        #[serde(default)]
        id: Option<String>,
        command: CollabWritableCommand,
        #[serde(default)]
        message: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CollabWritableCommand {
    Prompt,
    Abort,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CollabServerMessage<'a> {
    Snapshot { snapshot: &'a Value },
    Event { event: &'a Value },
    Response {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        command: &'static str,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[async_trait]
pub(super) trait CollabRuntime: Send + Sync {
    fn events(&self) -> broadcast::Receiver<Value>;

    async fn snapshot(
        &self,
        session_id: Option<&str>,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<(String, Value)>;

    async fn dispatch(&self, command: RpcCommand, session_id: String) -> RpcResponse;
}

#[async_trait]
impl CollabRuntime for Arc<SessionRuntimeManager> {
    fn events(&self) -> broadcast::Receiver<Value> {
        SessionRuntimeManager::events(self)
    }

    async fn snapshot(
        &self,
        session_id: Option<&str>,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<(String, Value)> {
        self.collab_snapshot(session_id, max_entries, max_bytes).await
    }

    async fn dispatch(&self, command: RpcCommand, session_id: String) -> RpcResponse {
        SessionRuntimeManager::dispatch(self, command, Some(session_id)).await
    }
}

struct Room {
    room_id: String,
    session_id: String,
    keys: CollabRoomKeys,
    control_capability: [u8; 32],
    view_capability: [u8; 32],
    participants: AtomicUsize,
    control_participants: AtomicUsize,
    view_participants: AtomicUsize,
    epochs: Mutex<HashSet<[u8; EPOCH_LEN]>>,
    stopped: watch::Sender<bool>,
}

impl Room {
    fn role_for_capability(&self, presented: &[u8; 32]) -> Option<CollabRole> {
        if capability_eq(presented, &self.control_capability) {
            Some(CollabRole::Control)
        } else if capability_eq(presented, &self.view_capability) {
            Some(CollabRole::View)
        } else {
            None
        }
    }

    fn key(&self, role: CollabRole) -> &[u8; KEY_LEN] {
        match role {
            CollabRole::Control => &self.keys.control,
            CollabRole::View => &self.keys.view,
        }
    }

    fn acquire_participant(self: &Arc<Self>, role: CollabRole) -> Result<ParticipantLease> {
        let result = self.participants.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| (current < MAX_COLLAB_PARTICIPANTS).then_some(current + 1),
        );
        if result.is_err() {
            bail!(
                "collaboration room participant limit reached ({MAX_COLLAB_PARTICIPANTS})"
            );
        }
        match role {
            CollabRole::Control => self.control_participants.fetch_add(1, Ordering::AcqRel),
            CollabRole::View => self.view_participants.fetch_add(1, Ordering::AcqRel),
        };
        Ok(ParticipantLease {
            room: self.clone(),
            role,
        })
    }

    fn issue_epoch(&self) -> Result<[u8; EPOCH_LEN]> {
        let mut epochs = self.epochs.lock().expect("collaboration epoch lock");
        if epochs.len() >= MAX_COLLAB_CONNECTION_EPOCHS {
            bail!("collaboration room connection limit reached; restart the room");
        }
        loop {
            let uuid = uuid::Uuid::new_v4();
            let mut epoch = [0u8; EPOCH_LEN];
            epoch.copy_from_slice(&uuid.as_bytes()[..EPOCH_LEN]);
            if epochs.insert(epoch) {
                return Ok(epoch);
            }
        }
    }
}

struct ParticipantLease {
    room: Arc<Room>,
    role: CollabRole,
}

impl Drop for ParticipantLease {
    fn drop(&mut self) {
        self.room.participants.fetch_sub(1, Ordering::AcqRel);
        match self.role {
            CollabRole::Control => self.room.control_participants.fetch_sub(1, Ordering::AcqRel),
            CollabRole::View => self.room.view_participants.fetch_sub(1, Ordering::AcqRel),
        };
    }
}

/// Ephemeral registry and lifecycle owner for encrypted collaboration rooms.
#[derive(Clone)]
pub struct CollabService {
    runtime: Arc<dyn CollabRuntime>,
    rooms: Arc<RwLock<HashMap<String, Arc<Room>>>>,
    default_room: Arc<RwLock<Option<String>>>,
}

impl CollabService {
    pub(crate) fn new(manager: Arc<SessionRuntimeManager>) -> Self {
        Self::with_runtime(Arc::new(manager))
    }

    pub(super) fn with_runtime(runtime: Arc<dyn CollabRuntime>) -> Self {
        Self {
            runtime,
            rooms: Arc::new(RwLock::new(HashMap::new())),
            default_room: Arc::new(RwLock::new(None)),
        }
    }

    /// Start a room bound to the selected recorded session.
    pub async fn start(
        &self,
        session_id: Option<&str>,
        base_url: &str,
    ) -> Result<CollabStartResult> {
        validate_base_url(base_url)?;
        let (session_id, _) = self
            .runtime
            .snapshot(
                session_id,
                MAX_COLLAB_SNAPSHOT_ENTRIES,
                MAX_COLLAB_SNAPSHOT_BYTES,
            )
            .await
            .map_err(|_| anyhow!("collaboration requires an available recorded session"))?;
        let room_id = new_room_id().map_err(|_| anyhow!("creating collaboration room failed"))?;
        let keys = generate_room_keys().map_err(|_| anyhow!("creating collaboration room failed"))?;
        let control_capability = capability(&keys.control);
        let view_capability = capability(&keys.view);
        let (stopped, _) = watch::channel(false);
        let room = Arc::new(Room {
            room_id: room_id.clone(),
            session_id: session_id.clone(),
            keys: keys.clone(),
            control_capability,
            view_capability,
            participants: AtomicUsize::new(0),
            control_participants: AtomicUsize::new(0),
            view_participants: AtomicUsize::new(0),
            epochs: Mutex::new(HashSet::new()),
            stopped,
        });
        let mut rooms = self.rooms.write().await;
        if rooms.len() >= MAX_COLLAB_ROOMS {
            bail!("collaboration room limit reached ({MAX_COLLAB_ROOMS})");
        }
        rooms.insert(room_id.clone(), room);
        drop(rooms);
        *self.default_room.write().await = Some(room_id.clone());

        let control_link = format_link(
            base_url,
            &room_id,
            &CollabSecret {
                role: CollabRole::Control,
                key: keys.control,
            },
        );
        let view_link = format_link(
            base_url,
            &room_id,
            &CollabSecret {
                role: CollabRole::View,
                key: keys.view,
            },
        );
        Ok(CollabStartResult {
            room_id,
            session_id,
            control_link,
            view_link,
        })
    }
    /// Start the interactive host room, or reprint the current room without
    /// rotating its capability-bearing links.
    pub async fn start_default(&self, base_url: &str) -> Result<CollabStartResult> {
        if let Some(room_id) = self.default_room.read().await.clone()
            && let Some(room) = self.rooms.read().await.get(&room_id).cloned()
        {
            return Ok(start_result(&room, base_url));
        }
        self.start(None, base_url).await
    }

    /// Return the current interactive host room, if one is running.
    pub async fn default_status(&self) -> Option<CollabRoomInfo> {
        let room_id = self.default_room.read().await.clone()?;
        self.rooms.read().await.get(&room_id).map(|room| room_info(room))
    }

    /// Stop the current interactive host room.
    pub async fn stop_default(&self) -> Result<CollabRoomInfo> {
        let room_id = self
            .default_room
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no collaboration room is running"))?;
        self.stop(&room_id).await
    }

    /// Signal every room before listener teardown so connected guests receive
    /// the same graceful Away close path as an explicit room stop.
    pub async fn stop_all(&self) {
        let rooms = {
            let mut rooms = self.rooms.write().await;
            rooms.drain().map(|(_, room)| room).collect::<Vec<_>>()
        };
        for room in rooms {
            room.stopped.send_replace(true);
        }
        *self.default_room.write().await = None;
    }


    /// Return active rooms, optionally narrowed to one room id.
    pub async fn status(&self, room_id: Option<&str>) -> Vec<CollabRoomInfo> {
        let rooms = self.rooms.read().await;
        let mut rows = rooms
            .values()
            .filter(|room| room_id.is_none_or(|wanted| wanted == room.room_id))
            .map(|room| room_info(room.as_ref()))
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.room_id.cmp(&b.room_id));
        rows
    }

    /// Stop and remove one room. Existing connections observe the stop signal
    /// and close; future authentication fails.
    pub async fn stop(&self, room_id: &str) -> Result<CollabRoomInfo> {
        let room = self
            .rooms
            .write()
            .await
            .remove(room_id)
            .ok_or_else(|| anyhow!("unknown collaboration room"))?;
        room.stopped.send_replace(true);
        if self.default_room.read().await.as_deref() == Some(room_id) {
            *self.default_room.write().await = None;
        }
        let mut info = room_info(&room);
        info.running = false;
        Ok(info)
    }

    pub(crate) async fn authenticate(
        &self,
        room_id: &str,
        presented: &[u8; 32],
    ) -> Result<CollabConnection> {
        let room = self
            .rooms
            .read()
            .await
            .get(room_id)
            .cloned()
            .ok_or_else(|| anyhow!("collaboration authentication failed"))?;
        // Subscribe immediately after retaining the room. A concurrent stop
        // remains observable even if it ran before this receiver existed.
        let stopped = room.stopped.subscribe();
        if *stopped.borrow() {
            bail!("collaboration authentication failed");
        }
        let role = room
            .role_for_capability(presented)
            .ok_or_else(|| anyhow!("collaboration authentication failed"))?;
        let lease = room.acquire_participant(role)?;
        let epoch = room.issue_epoch()?;
        // Subscribe before reading the authoritative recorder tree so events
        // emitted during snapshot creation cannot be missed.
        let events = self.runtime.events();
        let (_, snapshot) = self
            .runtime
            .snapshot(
                Some(&room.session_id),
                MAX_COLLAB_SNAPSHOT_ENTRIES,
                MAX_COLLAB_SNAPSHOT_BYTES,
            )
            .await
            .map_err(|_| anyhow!("collaboration snapshot is unavailable"))?;
        if *stopped.borrow() {
            bail!("collaboration authentication failed");
        }
        let client_key = derive_connection_key(
            room.key(role),
            &epoch,
            FrameDirection::ClientToServer,
        )
        .map_err(|_| anyhow!("creating collaboration connection failed"))?;
        let server_key = derive_connection_key(
            room.key(role),
            &epoch,
            FrameDirection::ServerToClient,
        )
        .map_err(|_| anyhow!("creating collaboration connection failed"))?;
        Ok(CollabConnection {
            room,
            role,
            epoch,
            client_key,
            server_key,
            receive: ReceiveWindow::new(),
            send: SendCounter::new(),
            snapshot,
            events,
            stopped,
            runtime: self.runtime.clone(),
            _lease: lease,
        })
    }
}

/// Authenticated, per-connection crypto and stream state used only by listen.rs.
pub(crate) struct CollabConnection {
    room: Arc<Room>,
    role: CollabRole,
    epoch: [u8; EPOCH_LEN],
    client_key: [u8; KEY_LEN],
    server_key: [u8; KEY_LEN],
    receive: ReceiveWindow,
    send: SendCounter,
    snapshot: Value,
    pub(crate) events: broadcast::Receiver<Value>,
    pub(crate) stopped: watch::Receiver<bool>,
    runtime: Arc<dyn CollabRuntime>,
    _lease: ParticipantLease,
}

impl CollabConnection {
    pub(crate) fn hello(&self) -> CollabHello {
        CollabHello {
            r#type: "hello",
            version: COLLAB_VERSION,
            room_id: self.room.room_id.clone(),
            role: match self.role {
                CollabRole::Control => "control",
                CollabRole::View => "view",
            },
            epoch: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.epoch),
        }
    }

    pub(crate) fn snapshot_frame(&mut self) -> Result<Vec<u8>> {
        let snapshot = public_value(&self.snapshot);
        self.seal(&CollabServerMessage::Snapshot { snapshot: &snapshot })
    }

    pub(crate) fn event_matches_room(&self, event: &Value) -> bool {
        event.get("sessionId").and_then(Value::as_str) == Some(self.room.session_id.as_str())
    }

    pub(crate) fn event_frame(&mut self, event: &Value) -> Result<Vec<u8>> {
        let event = public_value(event);
        self.seal(&CollabServerMessage::Event { event: &event })
    }

    pub(crate) fn prepare_client_frame(&mut self, frame: &[u8]) -> Result<CollabPendingCommand> {
        if frame.len() > MAX_FRAME_BYTES {
            bail!("collaboration frame exceeds the size limit");
        }
        let (_, direction, sequence, _) = parse_frame_header(frame)
            .ok_or_else(|| anyhow!("invalid collaboration frame"))?;
        if direction != FrameDirection::ClientToServer
            || sequence != self.receive.next_expected()
        {
            bail!("invalid collaboration frame sequence");
        }
        let plaintext = open_frame(
            &self.client_key,
            &self.room.room_id,
            FrameDirection::ClientToServer,
            &self.epoch,
            sequence,
            frame,
        )
        .map_err(|_| anyhow!("collaboration frame authentication failed"))?;
        self.receive
            .accept(sequence)
            .map_err(|_| anyhow!("invalid collaboration frame sequence"))?;
        let message: CollabClientMessage = serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow!("invalid collaboration command"))?;
        let (id, name, command) = match message {
            CollabClientMessage::Command {
                id,
                command: CollabWritableCommand::Prompt,
                message: Some(message),
            } => (
                id,
                "prompt",
                RpcCommand::Prompt {
                    id: None,
                    message,
                    images: Vec::new(),
                    streaming_behavior: None,
                },
            ),
            CollabClientMessage::Command {
                id,
                command: CollabWritableCommand::Abort,
                message: None,
            } => (id, "abort", RpcCommand::Abort { id: None }),
            CollabClientMessage::Command { .. } => bail!("invalid collaboration command"),
        };
        Ok(CollabPendingCommand {
            runtime: self.runtime.clone(),
            session_id: self.room.session_id.clone(),
            id,
            name,
            command: self.role.is_writable().then_some(command),
        })
    }

    pub(crate) fn response_frame(&mut self, response: CollabCommandResponse) -> Result<Vec<u8>> {
        self.seal(&CollabServerMessage::Response {
            id: response.id,
            command: response.command,
            success: response.success,
            error: response.error,
        })
    }

    async fn handle_client_frame(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let pending = self.prepare_client_frame(frame)?;
        let response = pending.execute().await;
        self.response_frame(response)
    }

    fn seal(&mut self, message: &CollabServerMessage<'_>) -> Result<Vec<u8>> {
        let plaintext = serde_json::to_vec(message)
            .map_err(|_| anyhow!("serializing collaboration payload failed"))?;
        let sequence = self
            .send
            .next()
            .ok_or_else(|| anyhow!("collaboration sequence space exhausted"))?;
        seal_frame(
            &self.server_key,
            &self.room.room_id,
            FrameDirection::ServerToClient,
            &self.epoch,
            sequence,
            &plaintext,
        )
        .map_err(|_| anyhow!("encrypting collaboration payload failed"))
    }
}

pub(crate) struct CollabPendingCommand {
    runtime: Arc<dyn CollabRuntime>,
    session_id: String,
    id: Option<String>,
    name: &'static str,
    command: Option<RpcCommand>,
}

pub(crate) struct CollabCommandResponse {
    id: Option<String>,
    command: &'static str,
    success: bool,
    error: Option<String>,
}

impl CollabPendingCommand {
    pub(crate) async fn execute(self) -> CollabCommandResponse {
        let Some(command) = self.command else {
            return CollabCommandResponse {
                id: self.id,
                command: self.name,
                success: false,
                error: Some("view-only collaboration cannot issue commands".to_owned()),
            };
        };
        let response = self.runtime.dispatch(command, self.session_id).await;
        CollabCommandResponse {
            id: self.id,
            command: self.name,
            success: response.success,
            error: response.error,
        }
    }
}
fn start_result(room: &Room, base_url: &str) -> CollabStartResult {
    let control_link = format_link(
        base_url,
        &room.room_id,
        &CollabSecret { role: CollabRole::Control, key: room.keys.control },
    );
    let view_link = format_link(
        base_url,
        &room.room_id,
        &CollabSecret { role: CollabRole::View, key: room.keys.view },
    );
    CollabStartResult {
        room_id: room.room_id.clone(),
        session_id: room.session_id.clone(),
        control_link,
        view_link,
    }
}


fn room_info(room: &Room) -> CollabRoomInfo {
    CollabRoomInfo {
        room_id: room.room_id.clone(),
        session_id: room.session_id.clone(),
        participants: room.participants.load(Ordering::Acquire),
        control_participants: room.control_participants.load(Ordering::Acquire),
        view_participants: room.view_participants.load(Ordering::Acquire),
        participant_limit: MAX_COLLAB_PARTICIPANTS,
        running: true,
    }
}


pub(crate) fn capability_from_protocols(headers: &http::HeaderMap) -> Option<(String, [u8; 32])> {
    let mut matched = None;
    for value in headers.get_all(http::header::SEC_WEBSOCKET_PROTOCOL) {
        let Ok(value) = value.to_str() else { return None };
        for offered in value.split(',').map(str::trim) {
            let Some(encoded) = offered.strip_prefix(COLLAB_PROTOCOL_PREFIX) else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded) else {
                continue;
            };
            let Ok(capability) = <[u8; 32]>::try_from(bytes) else {
                continue;
            };
            if matched.is_some() {
                return None;
            }
            matched = Some((offered.to_owned(), capability));
        }
    }
    matched
}

fn validate_base_url(base_url: &str) -> Result<()> {
    if !(base_url.starts_with("http://") || base_url.starts_with("https://"))
        || base_url.contains('#')
        || base_url.contains('?')
        || base_url.chars().any(char::is_whitespace)
    {
        bail!("collaboration baseUrl must be an http(s) origin without query or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use pi_coding::collab::{parse_link, FrameDirection};
    use tokio::sync::{Mutex as AsyncMutex, Notify};

    struct FakeRuntime {
        events: broadcast::Sender<Value>,
        snapshot: Value,
        dispatched: AsyncMutex<Vec<(&'static str, String)>>,
    }

    impl FakeRuntime {
        fn new() -> Arc<Self> {
            let (events, _) = broadcast::channel(32);
            Arc::new(Self {
                events,
                snapshot: json!({
                    "sessionId": "session-1",
                    "truncated": false,
                    "entries": [{"type":"message","message":{"role":"user","content":"snapshot marker"}}]
                }),
                dispatched: AsyncMutex::new(Vec::new()),
            })
        }
    }
    struct SnapshotBarrierRuntime {
        events: broadcast::Sender<Value>,
        snapshot_calls: AtomicUsize,
        snapshot_entered: Notify,
        snapshot_release: Notify,
    }

    impl SnapshotBarrierRuntime {
        fn new() -> Arc<Self> {
            let (events, _) = broadcast::channel(32);
            Arc::new(Self {
                events,
                snapshot_calls: AtomicUsize::new(0),
                snapshot_entered: Notify::new(),
                snapshot_release: Notify::new(),
            })
        }
    }

    #[async_trait]
    impl CollabRuntime for SnapshotBarrierRuntime {
        fn events(&self) -> broadcast::Receiver<Value> {
            self.events.subscribe()
        }

        async fn snapshot(
            &self,
            _session_id: Option<&str>,
            _max_entries: usize,
            _max_bytes: usize,
        ) -> Result<(String, Value)> {
            if self.snapshot_calls.fetch_add(1, Ordering::SeqCst) > 0 {
                self.snapshot_entered.notify_one();
                self.snapshot_release.notified().await;
            }
            Ok((
                "session-1".to_owned(),
                json!({"sessionId":"session-1","truncated":false,"entries":[]}),
            ))
        }

        async fn dispatch(&self, command: RpcCommand, _session_id: String) -> RpcResponse {
            RpcResponse::success(None, command.command_name(), None)
        }
    }


    #[async_trait]
    impl CollabRuntime for FakeRuntime {
        fn events(&self) -> broadcast::Receiver<Value> {
            self.events.subscribe()
        }

        async fn snapshot(
            &self,
            _session_id: Option<&str>,
            _max_entries: usize,
            _max_bytes: usize,
        ) -> Result<(String, Value)> {
            Ok(("session-1".to_owned(), self.snapshot.clone()))
        }

        async fn dispatch(&self, command: RpcCommand, session_id: String) -> RpcResponse {

            let name = command.command_name();
            self.dispatched.lock().await.push((name, session_id));
            RpcResponse::success(None, name, None)
        }
    }
    #[tokio::test]
    async fn default_room_reuses_links_reports_status_and_stops() {
        let runtime = FakeRuntime::new();
        let service = CollabService::with_runtime(runtime);
        let first = service.start_default("http://127.0.0.1:4321").await.expect("start");
        let second = service.start_default("http://127.0.0.1:4321").await.expect("reprint");
        assert_eq!(first, second, "bare /collab must not rotate live capabilities");
        let status = service.default_status().await.expect("status");
        assert_eq!(status.room_id, first.room_id);
        assert_eq!(status.participants, 0);
        let stopped = service.stop_default().await.expect("stop");
        assert_eq!(stopped.room_id, first.room_id);
        assert!(!stopped.running);
        assert!(service.default_status().await.is_none());
        assert!(service.stop_default().await.is_err());
    }

    async fn room(runtime: Arc<FakeRuntime>) -> (CollabService, CollabStartResult) {
        let service = CollabService::with_runtime(runtime);
        let started = service
            .start(None, "http://127.0.0.1:4321")
            .await
            .expect("start room");
        (service, started)
    }

    fn role_capability(link: &str) -> ([u8; KEY_LEN], [u8; 32]) {
        let parsed = parse_link(link).expect("parse link");
        (parsed.secret.key, capability(&parsed.secret.key))
    }

    fn client_frame(
        connection: &CollabConnection,
        role_key: &[u8; KEY_LEN],
        sequence: u64,
        value: Value,
    ) -> Vec<u8> {
        let key = derive_connection_key(
            role_key,
            &connection.epoch,
            FrameDirection::ClientToServer,
        )
        .expect("client key");
        seal_frame(
            &key,
            &connection.room.room_id,
            FrameDirection::ClientToServer,
            &connection.epoch,
            sequence,
            &serde_json::to_vec(&value).expect("json"),
        )
        .expect("seal client")
    }

    fn open_server(
        connection: &CollabConnection,
        role_key: &[u8; KEY_LEN],
        sequence: u64,
        frame: &[u8],
    ) -> Value {
        let key = derive_connection_key(
            role_key,
            &connection.epoch,
            FrameDirection::ServerToClient,
        )
        .expect("server key");
        let plaintext = open_frame(
            &key,
            &connection.room.room_id,
            FrameDirection::ServerToClient,
            &connection.epoch,
            sequence,
            frame,
        )
        .expect("open server");
        serde_json::from_slice(&plaintext).expect("server json")
    }

    #[tokio::test]
    async fn capability_auth_assigns_control_and_view_roles() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let (_, control_cap) = role_capability(&started.control_link);
        let (_, view_cap) = role_capability(&started.view_link);
        let control = service.authenticate(&started.room_id, &control_cap).await.expect("control");
        let view = service.authenticate(&started.room_id, &view_cap).await.expect("view");
        assert_eq!(control.role, CollabRole::Control);
        assert_eq!(view.role, CollabRole::View);
        assert!(service.authenticate(&started.room_id, &[9; 32]).await.is_err());
    }

    #[tokio::test]
    async fn hard_room_and_participant_caps_are_enforced() {
        let runtime = FakeRuntime::new();
        let service = CollabService::with_runtime(runtime);
        let mut first = None;
        for index in 0..MAX_COLLAB_ROOMS {
            let started = service
                .start(None, "http://127.0.0.1:4321")
                .await
                .unwrap_or_else(|_| panic!("room {index}"));
            first.get_or_insert(started);
        }
        assert!(service.start(None, "http://127.0.0.1:4321").await.is_err());
        let first = first.expect("first room");
        let (_, cap) = role_capability(&first.view_link);
        let mut participants = Vec::new();
        for _ in 0..MAX_COLLAB_PARTICIPANTS {
            participants.push(service.authenticate(&first.room_id, &cap).await.expect("participant"));
        }
        assert!(service.authenticate(&first.room_id, &cap).await.is_err());
        drop(participants.pop());
        assert!(service.authenticate(&first.room_id, &cap).await.is_ok());
    }

    #[tokio::test]
    async fn snapshot_is_first_encrypted_payload_and_contains_no_plaintext() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let (key, cap) = role_capability(&started.control_link);
        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth");
        let frame = connection.snapshot_frame().expect("snapshot frame");
        assert!(!frame.windows(b"snapshot marker".len()).any(|w| w == b"snapshot marker"));
        let opened = open_server(&connection, &key, 0, &frame);
        assert_eq!(opened["type"], "snapshot");
        assert_eq!(opened["snapshot"]["sessionId"], "session-1");
    }

    #[tokio::test]
    async fn connection_rejects_replay_and_tamper_without_advancing_window() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let (key, cap) = role_capability(&started.control_link);
        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth");
        let frame = client_frame(
            &connection,
            &key,
            0,
            json!({"type":"command","command":"abort","id":"a"}),
        );
        connection.handle_client_frame(&frame).await.expect("first accepted");
        assert!(connection.handle_client_frame(&frame).await.is_err(), "replay rejected");

        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth two");
        let valid = client_frame(
            &connection,
            &key,
            0,
            json!({"type":"command","command":"abort"}),
        );
        let mut tampered = valid.clone();
        *tampered.last_mut().expect("ciphertext byte") ^= 1;
        assert!(connection.handle_client_frame(&tampered).await.is_err());
        connection.handle_client_frame(&valid).await.expect("valid after tamper");
    }

    #[tokio::test]
    async fn control_prompt_and_abort_route_to_bound_session() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime.clone()).await;
        let (key, cap) = role_capability(&started.control_link);
        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth");
        let prompt = client_frame(
            &connection,
            &key,
            0,
            json!({"type":"command","command":"prompt","message":"hello"}),
        );
        connection.handle_client_frame(&prompt).await.expect("prompt");
        let abort = client_frame(
            &connection,
            &key,
            1,
            json!({"type":"command","command":"abort"}),
        );
        connection.handle_client_frame(&abort).await.expect("abort");
        assert_eq!(
            *runtime.dispatched.lock().await,
            vec![("prompt", "session-1".to_owned()), ("abort", "session-1".to_owned())]
        );
    }

    #[tokio::test]
    async fn view_denial_happens_before_dispatch() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime.clone()).await;
        let (key, cap) = role_capability(&started.view_link);
        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth");
        let prompt = client_frame(
            &connection,
            &key,
            0,
            json!({"type":"command","command":"prompt","message":"blocked","id":"p"}),
        );
        let response = connection.handle_client_frame(&prompt).await.expect("encrypted denial");
        let opened = open_server(&connection, &key, 0, &response);
        assert_eq!(opened["success"], false);
        assert!(runtime.dispatched.lock().await.is_empty());
    }
    #[tokio::test]
    async fn live_event_frame_is_redacted_path_free_and_minimized() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let (key, cap) = role_capability(&started.view_link);
        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth");
        let _snapshot = connection.snapshot_frame().expect("snapshot consumes seq zero");
        let secret = ["s", "k", "-", "abcdefghijklmnop", "1234567890"].concat();
        let unix_path = ["/", "tmp", "/", "collab-private", "/", "output.log"].concat();
        let event = json!({
            "type": "tool_execution_end",
            "sessionId": "session-1",
            "toolCallId": "call-1",
            "result": {"content": [{"type": "text", "text": format!("token={secret} at {unix_path}")}], "details": {"path": &unix_path}},
            "fullOutputPath": &unix_path,
            "data": {"raw": &secret},
        });
        let frame = connection.event_frame(&event).expect("event frame");
        let opened = open_server(&connection, &key, 1, &frame);
        let encoded = serde_json::to_string(&opened).expect("encode opened event");
        assert!(!encoded.contains(&secret), "secret leaked: {encoded}");
        assert!(!encoded.contains(&unix_path), "path leaked: {encoded}");
        assert!(!encoded.contains("fullOutputPath"));
        assert!(!encoded.contains("details"));
        assert!(!encoded.contains("\"data\""));
        assert!(encoded.contains("[REDACTED]"));
        assert!(encoded.contains("[PATH]"));
    }

    #[tokio::test]
    async fn reconnect_gets_fresh_host_epoch() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let (_, cap) = role_capability(&started.control_link);
        let first = service.authenticate(&started.room_id, &cap).await.expect("first");
        let first_epoch = first.epoch;
        drop(first);
        let second = service.authenticate(&started.room_id, &cap).await.expect("second");
        assert_ne!(first_epoch, second.epoch);
    }

    #[tokio::test]
    async fn stop_during_authoritative_snapshot_rejects_authentication() {
        let runtime = SnapshotBarrierRuntime::new();
        let service = CollabService::with_runtime(runtime.clone());
        let started = service
            .start(None, "http://127.0.0.1:4321")
            .await
            .expect("start room");
        let (_, capability) = role_capability(&started.control_link);
        let authenticating = {
            let service = service.clone();
            let room_id = started.room_id.clone();
            tokio::spawn(async move { service.authenticate(&room_id, &capability).await })
        };

        runtime.snapshot_entered.notified().await;
        service.stop(&started.room_id).await.expect("stop room");
        runtime.snapshot_release.notify_one();

        let error = match authenticating.await.expect("authentication task") {
            Ok(_) => panic!("stopped room authenticated"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "collaboration authentication failed");
    }

    #[tokio::test]
    async fn stopped_value_persists_without_receivers() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let retained_room = service
            .rooms
            .read()
            .await
            .get(&started.room_id)
            .cloned()
            .expect("room");

        service.stop(&started.room_id).await.expect("stop room");

        assert!(*retained_room.stopped.subscribe().borrow());
    }

    #[tokio::test]
    async fn stop_removes_room_signals_connections_and_cleanup_drops_participants() {
        let runtime = FakeRuntime::new();
        let (service, started) = room(runtime).await;
        let (_, cap) = role_capability(&started.control_link);
        let mut connection = service.authenticate(&started.room_id, &cap).await.expect("auth");
        assert_eq!(service.status(Some(&started.room_id)).await[0].participants, 1);
        let stopped = service.stop(&started.room_id).await.expect("stop");
        assert_eq!(stopped.participants, 1);
        connection.stopped.changed().await.expect("stop signal");
        assert!(*connection.stopped.borrow());
        assert!(service.status(Some(&started.room_id)).await.is_empty());
        assert!(service.authenticate(&started.room_id, &cap).await.is_err());
        drop(connection);
    }

    #[test]
    fn websocket_protocol_parser_accepts_exactly_one_capability_hash() {
        let cap = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            http::HeaderValue::from_str(&format!("chat, rpi-collab.{cap}")).expect("header"),
        );
        let (protocol, parsed) = capability_from_protocols(&headers).expect("protocol");
        assert_eq!(protocol, format!("rpi-collab.{cap}"));
        assert_eq!(parsed, [7; 32]);
        headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            http::HeaderValue::from_str(&format!("rpi-collab.{cap}, rpi-collab.{cap}"))
                .expect("header"),
        );
        assert!(capability_from_protocols(&headers).is_none());
    }
}
