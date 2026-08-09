//! Live collaboration room primitives: E2E-encrypted directional sequence
//! framing, room keys/capabilities, and join-link parsing/generation.
//!
//! # Threat model
//!
//! The collab transport (the `--listen` WebSocket relay) is untrusted for
//! payload confidentiality and integrity: every session payload (snapshot,
//! live transcript/tool/state events, guest commands) is sealed with
//! AES-256-GCM under a per-room role key before it touches the wire. The
//! relay sees only opaque ciphertext plus the non-secret frame header
//! (epoch, direction, sequence) it needs for routing and replay defense.
//!
//! Roles and keys:
//! - A room has two independent 32-byte keys: [`CollabRole::Control`]
//!   (writable guests: prompt/abort) and [`CollabRole::View`] (read-only
//!   guests). The host never reveals a key on the wire.
//! - Guests authenticate by presenting `base64url(SHA-256(key))` as the
//!   `rpi-collab.<cap>` WebSocket subprotocol. The relay stores only the
//!   capability hashes, so a browser can join with the key in the URL
//!   fragment without ever sending the key itself.
//!
//! # Nonce construction (uniqueness guarantee)
//!
//! Every connection derives two independent 32-byte keys from the room's
//! role key via HKDF-SHA256:
//! `conn_key = HKDF(ikm = role_key, salt = epoch, info = "collab-v1" || direction)`.
//! - `epoch`: 8 random bytes issued by the **host room** per connection and
//!   communicated in the plaintext `hello` frame. The host guarantees epochs
//!   are distinct across its connections, so every (room, role, direction,
//!   connection) uses a distinct key. Because the *encrypting* side never
//!   trusts a transport-supplied value for key/nonce construction, a forged
//!   hello can only break one connection (equivalent to a dropped connection
//!   — DoS, unavoidable), never cause key or nonce reuse.
//! - `direction`: the direction label in the HKDF info, so client→server and
//!   server→client never share a key even for the same epoch.
//!
//! Nonce (12 bytes) = `epoch_prefix(4) || seq_be(8)`. With per-connection
//! directional keys, a monotonic per-direction sequence alone guarantees
//! nonce uniqueness; the epoch prefix is defense in depth against any
//! derivation mistake.
//!
//! AAD = `room_id || direction(1) || seq_be(8)` — binds every frame to its
//! room, direction, and sequence, so cross-room replays, cross-role
//! injection, direction flips, and sequence tampering all fail tag
//! verification.
//!
//! # Frame layout
//!
//! One WebSocket binary message = one frame:
//! `header(17) = epoch_be(8) || direction(1) || seq_be(8)`, followed by the
//! AES-256-GCM ciphertext+tag (16-byte tag). The header is plaintext by
//! design (the relay needs epoch/direction/seq to route and to enforce
//! monotonicity); the tag authenticates it via the nonce and AAD.
//!
//! # Replay / out-of-order defense
//!
//! Each receiver keeps a strict monotonic [`ReceiveWindow`] per connection
//! and direction: only the exact next expected sequence is accepted.
//! Duplicates, replays, gaps, and reordered frames are rejected. Reconnect
//! starts a fresh connection with a fresh epoch, so captured frames from a
//! previous connection fail on the new nonce.
//!
//! # Links
//!
//! - Control link:  `http://host:port/collab/ws/<roomId>#c=<b64url(control key)>`
//! - View-only link: `http://host:port/collab/ws/<roomId>#v=<b64url(view key)>`
//!
//! The room id is in the path; the role key lives only in the URL fragment,
//! which browsers never send to the server.

use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use regex::Regex;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::constant_time::verify_slices_are_equal;
use ring::digest::{SHA256, digest};
use ring::hkdf::{HKDF_SHA256, Prk, Salt};
use ring::rand::{SecureRandom, SystemRandom};

/// AES-256 key length in bytes.
pub const KEY_LEN: usize = 32;

/// Random bytes per room id (`new_room_id`). 16 bytes → 22 base64url chars.
pub const ROOM_ID_BYTES: usize = 16;

/// Random bytes per connection epoch (8 → 64 bits; host-issued, unique per room).
pub const EPOCH_LEN: usize = 8;

/// Sequence counter length in bytes (u64, saturating).
pub const SEQ_LEN: usize = 8;

/// Direction tag length in bytes.
pub const DIRECTION_LEN: usize = 1;

/// Full nonce length (AES-GCM standard 96-bit nonce):
/// `epoch_prefix(4) || seq_be(8)`.
pub const NONCE_LEN: usize = 12;

/// Plaintext frame header length: `epoch(8) || direction(1) || seq(8)`.
pub const FRAME_HEADER_LEN: usize = EPOCH_LEN + DIRECTION_LEN + SEQ_LEN;

/// AES-GCM authentication tag length in bytes.
pub const TAG_LEN: usize = 16;

/// HKDF info label prefix for per-connection directional key derivation.
/// The full info is `"collab-v1" || direction_byte`; the epoch is the salt.
const HKDF_INFO_PREFIX: &[u8] = b"collab-v1";

/// Upper bound for one sealed frame (header + ciphertext + tag). Matches the
/// RPC control plane's message cap so collab payloads cannot exceed what the
/// same listener already admits; enforced by the relay and the codec.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum characters retained in any guest-visible string. Frame limits are
/// still the final transport bound; this prevents one recorder field or live
/// diagnostic from monopolizing the entire frame and browser render.
pub const MAX_PUBLIC_STRING_CHARS: usize = 64 * 1024;

const MAX_PUBLIC_VALUE_DEPTH: usize = 32;
const MAX_PUBLIC_ARRAY_ITEMS: usize = 2_048;
const MAX_PUBLIC_OBJECT_FIELDS: usize = 512;
/// Absolute Unix, Windows drive, and UNC paths embedded in otherwise public
/// text, including paths immediately preceded by a colon label such as
/// `cwd:/private/file`. Schemed HTTP(S) URLs match the labeled form but are
/// explicitly preserved by [`public_text`].
static ABSOLUTE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)(^|[\s\"'`=({\[])(?:(?P<label>[A-Za-z_][A-Za-z0-9_-]*):)?(?P<path>(?:[A-Za-z]:[\\/]|/|\\\\)[^\s\"'`<>{}\[\](),;]*)"#,
    )
    .expect("valid collaboration absolute-path pattern")
});

/// Direction tag values (also the nonce/AAD direction byte).
const DIRECTION_CLIENT_TO_SERVER: u8 = 0x01;
const DIRECTION_SERVER_TO_CLIENT: u8 = 0x02;

/// Guest permission level, derived from which link secret a guest presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CollabRole {
    /// Writable guest: may prompt and abort, reads the control-key stream.
    Control,
    /// Read-only guest: receives the view-key stream, writes are rejected.
    View,
}

impl CollabRole {
    #[must_use]
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::Control)
    }
}

/// Frame direction, bound into the nonce and the AAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameDirection {
    ClientToServer,
    ServerToClient,
}

impl FrameDirection {
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::ClientToServer => DIRECTION_CLIENT_TO_SERVER,
            Self::ServerToClient => DIRECTION_SERVER_TO_CLIENT,
        }
    }

    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            DIRECTION_CLIENT_TO_SERVER => Some(Self::ClientToServer),
            DIRECTION_SERVER_TO_CLIENT => Some(Self::ServerToClient),
            _ => None,
        }
    }
}

/// The two independent role keys of one room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollabRoomKeys {
    pub control: [u8; KEY_LEN],
    pub view: [u8; KEY_LEN],
}

/// One role's secret: which key a link carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollabSecret {
    pub role: CollabRole,
    pub key: [u8; KEY_LEN],
}

/// A parsed or generated join link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollabLink {
    /// Room id as it appears in the URL path (`/collab/ws/<room_id>`).
    pub room_id: String,
    /// The role key carried in the URL fragment.
    pub secret: CollabSecret,
}

/// Generate a new random room id: 16 random bytes, base64url (no padding).
pub fn new_room_id() -> Result<String> {
    let mut bytes = [0u8; ROOM_ID_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("generating collab room id"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Generate a fresh pair of room keys (control + view).
pub fn generate_room_keys() -> Result<CollabRoomKeys> {
    let rng = SystemRandom::new();
    let mut control = [0u8; KEY_LEN];
    let mut view = [0u8; KEY_LEN];
    rng.fill(&mut control)
        .map_err(|_| anyhow!("generating collab control key"))?;
    rng.fill(&mut view)
        .map_err(|_| anyhow!("generating collab view key"))?;
    Ok(CollabRoomKeys { control, view })
}

/// Derive the capability hash presented to the relay as the
/// `rpi-collab.<cap>` WebSocket subprotocol. The relay stores only this
/// hash, never the key.
#[must_use]
pub fn capability(key: &[u8; KEY_LEN]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, key).as_ref());
    out
}

/// Constant-time capability comparison for relay-side authentication.
pub fn capability_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    verify_slices_are_equal(a, b).is_ok()
}

/// Build the 12-byte nonce `epoch_prefix(4) || seq_be(8)`.
#[must_use]
fn build_nonce(epoch: &[u8; EPOCH_LEN], seq: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..4].copy_from_slice(&epoch[..4]);
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

/// Build the AAD `room_id || direction(1) || seq_be(8)`.
#[must_use]
fn build_aad(room_id: &str, direction: FrameDirection, seq: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(room_id.len() + 1 + SEQ_LEN);
    aad.extend_from_slice(room_id.as_bytes());
    aad.push(direction.tag());
    aad.extend_from_slice(&seq.to_be_bytes());
    aad
}

/// Derive the per-connection directional key for one connection:
/// `HKDF-SHA256(ikm = role_key, salt = epoch, info = "collab-v1" || direction)`.
///
/// Distinct epochs (per connection) and direction labels (per direction)
/// yield distinct keys, so sequence monotonicity per (connection, direction)
/// is a complete nonce-uniqueness argument. The role key itself is bound by
/// using it as the HKDF input keying material. Output length is the SHA-256
/// digest length (32 bytes) — `HKDF_SHA256` doubles as the `KeyType`.
pub fn derive_connection_key(
    role_key: &[u8; KEY_LEN],
    epoch: &[u8; EPOCH_LEN],
    direction: FrameDirection,
) -> Result<[u8; KEY_LEN]> {
    let salt = Salt::new(HKDF_SHA256, epoch);
    let prk: Prk = salt.extract(role_key);
    let info: [&[u8]; 2] = [HKDF_INFO_PREFIX, &[direction.tag()]];
    let okm = prk
        .expand(&info, HKDF_SHA256)
        .map_err(|_| anyhow!("HKDF rejected the collab key material"))?;
    let mut key = [0u8; KEY_LEN];
    okm.fill(&mut key)
        .map_err(|_| anyhow!("HKDF failed to produce a collab connection key"))?;
    Ok(key)
}

/// An AEAD key bound to one room role key. `LessSafeKey` is fine here: the
/// key is used for many messages with fresh nonces (the standard AEAD usage
/// pattern) and never for handshake-derived keys.
fn aead_key(key: &[u8; KEY_LEN]) -> Result<LessSafeKey> {
    UnboundKey::new(&AES_256_GCM, key).map(LessSafeKey::new).map_err(|_| {
        anyhow!("AES-256-GCM rejected a collab room key (key must be exactly {KEY_LEN} bytes)")
    })
}

/// Parse the plaintext frame header: returns `(epoch, direction, seq, body)`.
///
/// The body is the remaining ciphertext+tag slice. Returns `None` for frames
/// shorter than the header, over the size cap, or carrying an unknown
/// direction tag.
#[must_use]
pub fn parse_frame_header(
    frame: &[u8],
) -> Option<([u8; EPOCH_LEN], FrameDirection, u64, &[u8])> {
    if frame.len() < FRAME_HEADER_LEN || frame.len() > MAX_FRAME_BYTES {
        return None;
    }
    let (header, body) = frame.split_at(FRAME_HEADER_LEN);
    let mut epoch = [0u8; EPOCH_LEN];
    epoch.copy_from_slice(&header[..EPOCH_LEN]);
    let direction = FrameDirection::from_tag(header[EPOCH_LEN])?;
    let seq = u64::from_be_bytes(
        header[EPOCH_LEN + DIRECTION_LEN..]
            .try_into()
            .expect("header tail is SEQ_LEN bytes"),
    );
    Some((epoch, direction, seq, body))
}

/// Seal one plaintext payload into a frame.
///
/// `key` is the per-connection directional key ([`derive_connection_key`]);
/// `seq` is the sender's next sequence for this connection/direction (see
/// [`SendCounter::next`]). Every call must use a fresh `seq` per key; callers
/// get this by construction (per-connection keys + monotonic counters).
pub fn seal_frame(
    key: &[u8; KEY_LEN],
    room_id: &str,
    direction: FrameDirection,
    epoch: &[u8; EPOCH_LEN],
    seq: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    if plaintext.len() + FRAME_HEADER_LEN + TAG_LEN > MAX_FRAME_BYTES {
        bail!(
            "collab frame payload exceeds the {MAX_FRAME_BYTES}-byte frame cap ({} bytes)",
            plaintext.len()
        );
    }
    let key = aead_key(key)?;
    let nonce = Nonce::try_assume_unique_for_key(&build_nonce(epoch, seq))
        .map_err(|_| anyhow!("built an invalid collab nonce"))?;
    let aad = Aad::from(build_aad(room_id, direction, seq));
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, aad, &mut in_out)
        .map_err(|_| anyhow!("collab frame encryption failed"))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + in_out.len());
    frame.extend_from_slice(epoch);
    frame.push(direction.tag());
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&in_out);
    Ok(frame)
}

/// Open a frame produced by [`seal_frame`].
///
/// The frame header is parsed and verified against the expected
/// `epoch`, `direction`, and `seq` (which the caller knows from its
/// connection context and/or [`parse_frame_header`]) before the body is
/// authenticated. Fails on any tampering — a flipped header byte is caught
/// by the epoch/direction/sequence match, a flipped body byte by the AEAD
/// tag, which additionally binds the room id, direction, and sequence.
pub fn open_frame(
    key: &[u8; KEY_LEN],
    room_id: &str,
    direction: FrameDirection,
    epoch: &[u8; EPOCH_LEN],
    seq: u64,
    frame: &[u8],
) -> Result<Vec<u8>> {
    let (header_epoch, header_direction, header_seq, body) = parse_frame_header(frame)
        .ok_or_else(|| anyhow!("collab frame has a malformed or oversized header"))?;
    if header_epoch != *epoch || header_direction != direction || header_seq != seq {
        bail!("collab frame header does not match the expected epoch/direction/sequence");
    }
    let key = aead_key(key)?;
    let nonce = Nonce::try_assume_unique_for_key(&build_nonce(epoch, seq))
        .map_err(|_| anyhow!("built an invalid collab nonce"))?;
    let aad = Aad::from(build_aad(room_id, direction, seq));
    let mut in_out = body.to_vec();
    let plaintext = key
        .open_in_place(nonce, aad, &mut in_out)
        .map_err(|_| anyhow!("collab frame authentication failed"))?;
    Ok(plaintext.to_vec())
}

/// Sender-side monotonic sequence counter (per connection, per direction).
///
/// Starts at 0 and hands out every value through `u64::MAX`, then refuses to
/// seal further frames instead of wrapping (a wrap would reuse nonces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendCounter {
    next: u64,
    exhausted: bool,
}

impl Default for SendCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl SendCounter {
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 0, exhausted: false }
    }

    /// The next sequence value, advancing the counter; `None` once the u64
    /// sequence space is exhausted (the connection must be closed).
    #[must_use]
    pub fn next(&mut self) -> Option<u64> {
        if self.exhausted {
            return None;
        }
        let value = self.next;
        if value == u64::MAX {
            self.exhausted = true;
        } else {
            self.next += 1;
        }
        Some(value)
    }
}

/// Receiver-side strict monotonic window: only the exact next expected
/// sequence is accepted. Duplicates, replays, gaps, and reordered frames are
/// rejected — WebSocket delivery is ordered, so any deviation is an attack
/// or a bug.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiveWindow {
    next_expected: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceError {
    /// The frame is older than or equal to the last accepted sequence
    /// (duplicate/replay).
    Replay,
    /// The frame skips ahead of the expected sequence (gap/out-of-order).
    OutOfOrder,
    /// The sequence space is exhausted.
    Exhausted,
}

impl ReceiveWindow {
    #[must_use]
    pub const fn new() -> Self {
        Self { next_expected: 0 }
    }

    /// The next sequence this window will accept.
    #[must_use]
    pub const fn next_expected(&self) -> u64 {
        self.next_expected
    }

    /// Validate `seq` and advance the window on success. Rejects anything
    /// other than the exact expected value.
    pub fn accept(&mut self, seq: u64) -> Result<(), SequenceError> {
        if seq < self.next_expected {
            return Err(SequenceError::Replay);
        }
        if seq > self.next_expected {
            return Err(SequenceError::OutOfOrder);
        }
        self.next_expected = self
            .next_expected
            .checked_add(1)
            .ok_or(SequenceError::Exhausted)?;
        Ok(())
    }
}

/// Encode the URL path of a join link: `/collab/ws/<room_id>`.
#[must_use]
pub fn link_path(room_id: &str) -> String {
    format!("/collab/ws/{room_id}")
}

/// Generate a join link for one role.
///
/// `base_url` is the reachable listener origin (e.g. `http://127.0.0.1:3456`
/// or a LAN address). The room id goes in the path; the role key only in the
/// fragment, which browsers never transmit.
pub fn format_link(base_url: &str, room_id: &str, secret: &CollabSecret) -> String {
    let fragment = match secret.role {
        CollabRole::Control => "c",
        CollabRole::View => "v",
    };
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret.key);
    let base = base_url.trim_end_matches('/');
    format!("{base}{}#{fragment}={key}", link_path(room_id))
}

/// Parse a join link produced by [`format_link`].
///
/// Returns a room id + role key. Rejects malformed URLs, non-`/collab/ws/`
/// paths, missing/duplicate/unknown fragments, and keys of the wrong length.
pub fn parse_link(link: &str) -> Result<CollabLink> {
    let url = url::Url::parse(link).map_err(|_| anyhow!("invalid collab link URL"))?;
    let scheme = url.scheme();
    if !matches!(scheme, "http" | "https") {
        bail!("collab links must use http or https (got scheme {scheme:?})");
    }
    let path = url.path();
    let Some(room_id) = path.strip_prefix("/collab/ws/") else {
        bail!("collab link path must start with /collab/ws/");
    };
    if room_id.is_empty()
        || room_id.contains('/')
        || room_id.contains('?')
        || room_id.contains('#')
        || room_id.bytes().any(|b| !(0x21..=0x7e).contains(&b))
    {
        bail!("collab link has an invalid room id");
    }
    let fragment = url
        .fragment()
        .ok_or_else(|| anyhow!("collab link is missing its key fragment (#c=... or #v=...)"))?;
    let (role, encoded) = if let Some(encoded) = fragment.strip_prefix("c=") {

        (CollabRole::Control, encoded)
    } else if let Some(encoded) = fragment.strip_prefix("v=") {
        (CollabRole::View, encoded)
    } else {
        bail!("collab link fragment must start with c= (control) or v= (view)");
    };
    if encoded.is_empty() || encoded.contains('=') || encoded.contains('&') {
        bail!("collab link fragment must carry exactly one role key");
    }
    let key_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| anyhow!("collab link key is not valid base64url"))?;
    let key: [u8; KEY_LEN] = key_bytes
        .try_into()
        .map_err(|_| anyhow!("collab link key must decode to exactly {KEY_LEN} bytes (AES-256)"))?;
    Ok(CollabLink {
        room_id: room_id.to_owned(),
        secret: CollabSecret { role, key },
    })
}
/// Project arbitrary collaboration-visible JSON onto a bounded, recursively
/// redacted shape. This is the final server-side privacy boundary used for
/// both recorder snapshots and live events before encryption.
#[must_use]
pub fn public_value(value: &serde_json::Value) -> serde_json::Value {
    public_value_at(value, 0)
}

fn public_value_at(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    if depth >= MAX_PUBLIC_VALUE_DEPTH {
        return serde_json::Value::String("[truncated]".to_owned());
    }
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            value.clone()
        }
        serde_json::Value::String(text) => serde_json::Value::String(public_text(text)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .take(MAX_PUBLIC_ARRAY_ITEMS)
                .map(|item| public_value_at(item, depth + 1))
                .collect(),
        ),
        serde_json::Value::Object(fields) => {
            let mut projected = serde_json::Map::new();
            for (key, value) in fields.iter().take(MAX_PUBLIC_OBJECT_FIELDS) {
                if is_private_field(key) {
                    continue;
                }
                projected.insert(key.clone(), public_value_at(value, depth + 1));
            }
            serde_json::Value::Object(projected)
        }
    }
}

fn is_private_field(key: &str) -> bool {
    matches!(
        key,
        "details"
            | "data"
            | "fullOutputPath"
            | "full_output_path"
            | "textSignature"
            | "text_signature"
            | "thinkingSignature"
            | "thinking_signature"
            | "thoughtSignature"
            | "thought_signature"
            | "responseId"
            | "response_id"
            | "rawStopReason"
            | "raw_stop_reason"
    )
}

fn public_text(text: &str) -> String {
    let redacted = crate::redact::redact_secrets(text);
    let path_free = ABSOLUTE_PATH.replace_all(&redacted, |captures: &regex::Captures<'_>| {
        let boundary = captures.get(1).map_or("", |matched| matched.as_str());
        match captures.name("label").map(|matched| matched.as_str()) {
            Some(label) if label.eq_ignore_ascii_case("http") || label.eq_ignore_ascii_case("https") => {
                captures.get(0).expect("whole path match").as_str().to_owned()
            }
            Some(label) => format!("{boundary}{label}:[PATH]"),
            None => format!("{boundary}[PATH]"),
        }
    });
    if path_free.chars().count() <= MAX_PUBLIC_STRING_CHARS {
        return path_free.into_owned();
    }
    let mut bounded = path_free
        .chars()
        .take(MAX_PUBLIC_STRING_CHARS.saturating_sub(1))
        .collect::<String>();
    bounded.push('…');
    bounded
}

/// Bounded public snapshot from a recorder-authoritative session tree.
///
/// Guests receive history straight from the backend recorder — the same
/// authoritative entries the host session is built from, never a frontend or
/// forwarder cache. The snapshot:
/// - carries the recorder's session id and the most recent `max_entries`
///   entries, oldest-first, while their serialized size stays under
///   `max_bytes` (the frame cap's spirit: a bounded, replayable history);
/// - never serializes the host filesystem path (`SessionHeader.cwd`) or any
///   other host-side metadata — only the session id and the entries;
/// - reports `truncated` when history was cut for size/entry limits.
///
/// The caller seals the returned JSON value into a frame like any other
/// payload. `max_entries == 0` yields an empty entry list.
pub fn public_snapshot(
    tree: &crate::SessionTree,
    max_entries: usize,
    max_bytes: usize,
) -> Result<serde_json::Value> {
    let mut retained = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for entry in tree.entries.iter().rev() {
        let raw = serde_json::to_value(entry)
            .map_err(|_| anyhow!("serializing a collab snapshot entry failed"))?;
        let projected = public_value(&raw);
        let serialized = serde_json::to_vec(&projected)
            .map_err(|_| anyhow!("serializing a collab snapshot entry failed"))?;
        if retained.len() >= max_entries || bytes.saturating_add(serialized.len()) > max_bytes {
            truncated = true;
            break;
        }
        bytes += serialized.len();
        retained.push(projected);
    }
    retained.reverse();
    Ok(serde_json::json!({
        "sessionId": tree.header.id,
        "truncated": truncated,
        "entries": retained,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_room() -> (String, CollabRoomKeys) {
        (new_room_id().expect("room id"), generate_room_keys().expect("keys"))
    }

    fn epoch(n: u8) -> [u8; EPOCH_LEN] {
        [n; EPOCH_LEN]
    }

    fn link_for(room: &str, keys: &CollabRoomKeys, role: CollabRole) -> String {
        let secret = CollabSecret {
            role,
            key: match role {
                CollabRole::Control => keys.control,
                CollabRole::View => keys.view,
            },
        };
        format_link("http://127.0.0.1:4321", room, &secret)
    }

    #[test]
    fn room_ids_are_url_safe_and_unique() {
        let first = new_room_id().expect("room id");
        let second = new_room_id().expect("room id");
        assert_eq!(first.len(), 22, "16 bytes -> 22 base64url chars");
        assert_ne!(first, second);
        assert!(first
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        // Room ids must survive URL round trips inside the path.
        let url = url::Url::parse(&format!("http://h/collab/ws/{first}")).expect("url");
        assert_eq!(url.path(), format!("/collab/ws/{first}"));
    }

    #[test]
    fn link_round_trips_for_both_roles() {
        for role in [CollabRole::Control, CollabRole::View] {
            let (room, keys) = test_room();
            let link = link_for(&room, &keys, role);
            let parsed = parse_link(&link).expect("parse link");
            assert_eq!(parsed.room_id, room);
            assert_eq!(parsed.secret.role, role);
            assert_eq!(
                parsed.secret.key,
                match role {
                    CollabRole::Control => keys.control,
                    CollabRole::View => keys.view,
                }
            );
        }
    }

    #[test]
    fn control_and_view_links_carry_distinct_secrets() {
        let (room, keys) = test_room();
        let control = parse_link(&link_for(&room, &keys, CollabRole::Control)).expect("control");
        let view = parse_link(&link_for(&room, &keys, CollabRole::View)).expect("view");
        assert_ne!(control.secret.key, view.secret.key);
        assert_ne!(control.secret.role, view.secret.role);
    }

    #[test]
    fn link_parse_rejects_malformed_input() {
        let (room, keys) = test_room();
        let good = link_for(&room, &keys, CollabRole::Control);
        // Wrong scheme.
        assert!(parse_link(&good.replace("http://", "ftp://")).is_err());
        // Missing fragment.
        assert!(parse_link(&good[..good.find('#').expect("fragment")]).is_err());
        // Unknown fragment key.
        assert!(parse_link(&format!("{good}#x=AAAA")).is_err());
        // Both fragment keys.
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(keys.control);
        assert!(parse_link(&format!("{good}#c={key}&v={key}")).is_err());
        // Wrong path.
        assert!(parse_link(&good.replace("/collab/ws/", "/other/")).is_err());
        // Key of the wrong length (16 bytes instead of 32).
        let short = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; 16]);
        assert!(parse_link(&format!("{good}#c={short}")).is_err());
        // Empty room id.
        assert!(parse_link("http://h/collab/ws/#c=AAAA").is_err());
        // Not a URL at all.
        assert!(parse_link("not a url").is_err());
    }

    #[test]
    fn capabilities_derive_deterministically_and_distinguish_keys() {
        let (_, keys) = test_room();
        let control_cap = capability(&keys.control);
        let view_cap = capability(&keys.view);
        assert_eq!(capability(&keys.control), control_cap, "deterministic");
        assert_ne!(control_cap, view_cap, "distinct keys -> distinct capabilities");
        assert!(capability_eq(&control_cap, &control_cap));
        assert!(!capability_eq(&control_cap, &view_cap));
        // A capability never leaks the key: it must not contain key bytes.
        let key_blob = keys.control.to_vec();
        let cap_blob = control_cap.to_vec();
        assert!(!cap_blob.windows(4).any(|w| key_blob.windows(4).any(|k| k == w)));
    }

    #[test]
    fn directional_hkdf_derives_distinct_keys_per_epoch_and_direction() {
        let (_, keys) = test_room();
        let epoch_a = epoch(1);
        let epoch_b = epoch(2);
        // Same role key, same direction, different epochs -> different keys.
        let a_c2s = derive_connection_key(&keys.control, &epoch_a, FrameDirection::ClientToServer)
            .expect("a c2s");
        let b_c2s = derive_connection_key(&keys.control, &epoch_b, FrameDirection::ClientToServer)
            .expect("b c2s");
        assert_ne!(a_c2s, b_c2s, "epoch must separate connection keys");
        // Same epoch, different directions -> different keys.
        let a_s2c = derive_connection_key(&keys.control, &epoch_a, FrameDirection::ServerToClient)
            .expect("a s2c");
        assert_ne!(a_c2s, a_s2c, "direction must separate connection keys");
        // Same epoch and direction, different role keys -> different keys.
        let view_c2s = derive_connection_key(&keys.view, &epoch_a, FrameDirection::ClientToServer)
            .expect("view c2s");
        assert_ne!(a_c2s, view_c2s, "role key must separate connection keys");
        // Derivation is deterministic for identical inputs.
        let a_c2s_again =
            derive_connection_key(&keys.control, &epoch_a, FrameDirection::ClientToServer)
                .expect("a c2s again");
        assert_eq!(a_c2s, a_c2s_again);
    }

    #[test]
    fn seal_open_round_trips_and_hides_plaintext() {
        let (room, keys) = test_room();
        let conn_key =
            derive_connection_key(&keys.control, &epoch(7), FrameDirection::ServerToClient)
                .expect("conn key");
        let plaintext = b"top secret collab payload with known marker";
        let frame = seal_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(7), 0, plaintext)
            .expect("seal");
        assert!(frame.len() > plaintext.len() + FRAME_HEADER_LEN);
        // Ciphertext contains no known plaintext.
        assert!(
            !frame.windows(plaintext.len()).any(|w| w == plaintext),
            "frame must not contain the plaintext"
        );
        assert!(
            !frame
                .windows(b"known marker".len())
                .any(|w| w == b"known marker"),
            "frame must not contain plaintext substrings"
        );
        let (parsed_epoch, direction, seq, body) = parse_frame_header(&frame).expect("header");
        assert_eq!(parsed_epoch, epoch(7));
        assert_eq!(direction, FrameDirection::ServerToClient);
        assert_eq!(seq, 0);
        assert_eq!(body.len() + FRAME_HEADER_LEN, frame.len());
        let opened = open_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(7), seq, &frame)
            .expect("open");
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn nonce_uniqueness_changes_ciphertext_per_seq_epoch_direction() {
        let (room, keys) = test_room();
        let payload = b"same payload";
        let s2c_a = derive_connection_key(&keys.control, &epoch(9), FrameDirection::ServerToClient)
            .expect("s2c a");
        // Different seq -> different nonce -> different ciphertext.
        let a = seal_frame(&s2c_a, &room, FrameDirection::ServerToClient, &epoch(9), 0, payload)
            .expect("a");
        let b = seal_frame(&s2c_a, &room, FrameDirection::ServerToClient, &epoch(9), 1, payload)
            .expect("b");
        assert_ne!(a, b);
        // Different epoch -> different key -> different ciphertext.
        let s2c_b = derive_connection_key(&keys.control, &epoch(10), FrameDirection::ServerToClient)
            .expect("s2c b");
        let c = seal_frame(&s2c_b, &room, FrameDirection::ServerToClient, &epoch(10), 0, payload)
            .expect("c");
        assert_ne!(a, c);
        // Different direction -> different key -> different ciphertext.
        let c2s_a = derive_connection_key(&keys.control, &epoch(9), FrameDirection::ClientToServer)
            .expect("c2s a");
        let d = seal_frame(&c2s_a, &room, FrameDirection::ClientToServer, &epoch(9), 0, payload)
            .expect("d");
        assert_ne!(a, d);
        // Different role key -> different key -> different ciphertext.
        let view_s2c = derive_connection_key(&keys.view, &epoch(9), FrameDirection::ServerToClient)
            .expect("view s2c");
        let e = seal_frame(&view_s2c, &room, FrameDirection::ServerToClient, &epoch(9), 0, payload)
            .expect("e");
        assert_ne!(a, e);
    }

    #[test]
    fn tamper_rejection_any_byte_flips() {
        let (room, keys) = test_room();
        let conn_key =
            derive_connection_key(&keys.control, &epoch(3), FrameDirection::ServerToClient)
                .expect("conn key");
        let frame = seal_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(3), 5, b"integrity protected")
            .expect("seal");
        for index in 0..frame.len() {
            let mut tampered = frame.clone();
            tampered[index] ^= 0x01;
            let result = open_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(3), 5, &tampered);
            assert!(result.is_err(), "byte flip at {index} must fail");
        }
    }

    #[test]
    fn truncation_rejected() {
        let (room, keys) = test_room();
        let conn_key =
            derive_connection_key(&keys.control, &epoch(3), FrameDirection::ServerToClient)
                .expect("conn key");
        let frame = seal_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(3), 0, b"payload")
            .expect("seal");
        for cut in 0..frame.len() {
            assert!(
                open_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(3), 0, &frame[..cut])
                    .is_err(),
                "truncation to {cut} bytes must fail"
            );
        }
        assert!(parse_frame_header(&frame[..FRAME_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn cross_room_rejection() {
        let (room_a, keys_a) = test_room();
        let (room_b, _) = test_room();
        let conn_key =
            derive_connection_key(&keys_a.control, &epoch(1), FrameDirection::ServerToClient)
                .expect("conn key");
        let frame = seal_frame(&conn_key, &room_a, FrameDirection::ServerToClient, &epoch(1), 0, b"room A payload")
            .expect("seal");
        // Same keys, different room: AAD mismatch must fail.
        assert!(
            open_frame(&conn_key, &room_b, FrameDirection::ServerToClient, &epoch(1), 0, &frame)
                .is_err(),
            "cross-room replay must be rejected"
        );
    }

    #[test]
    fn cross_role_and_cross_direction_rejection() {
        let (room, keys) = test_room();
        let control_s2c =
            derive_connection_key(&keys.control, &epoch(2), FrameDirection::ServerToClient)
                .expect("control s2c");
        let frame = seal_frame(&control_s2c, &room, FrameDirection::ServerToClient, &epoch(2), 0, b"control payload")
            .expect("seal");
        // Same room, view key instead of control key.
        let view_s2c = derive_connection_key(&keys.view, &epoch(2), FrameDirection::ServerToClient)
            .expect("view s2c");
        assert!(
            open_frame(&view_s2c, &room, FrameDirection::ServerToClient, &epoch(2), 0, &frame)
                .is_err(),
            "cross-role decryption must fail"
        );
        // Same control key, opposite direction.
        let control_c2s =
            derive_connection_key(&keys.control, &epoch(2), FrameDirection::ClientToServer)
                .expect("control c2s");
        assert!(
            open_frame(&control_c2s, &room, FrameDirection::ClientToServer, &epoch(2), 0, &frame)
                .is_err(),
            "direction flip must fail"
        );
    }

    #[test]
    fn wrong_epoch_and_wrong_seq_rejected() {
        let (room, keys) = test_room();
        let conn_key =
            derive_connection_key(&keys.control, &epoch(4), FrameDirection::ServerToClient)
                .expect("conn key");
        let frame = seal_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(4), 7, b"payload")
            .expect("seal");
        // A captured frame replayed under a different (new) epoch fails
        // because the connection key differs.
        let reconnected_key =
            derive_connection_key(&keys.control, &epoch(5), FrameDirection::ServerToClient)
                .expect("reconnected key");
        assert!(
            open_frame(&reconnected_key, &room, FrameDirection::ServerToClient, &epoch(5), 0, &frame)
                .is_err(),
            "replayed frame from another epoch must fail"
        );
        assert!(
            open_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(4), 8, &frame)
                .is_err(),
            "wrong sequence must fail"
        );
    }

    #[test]
    fn receive_window_accepts_exactly_in_order() {
        let mut window = ReceiveWindow::new();
        assert_eq!(window.next_expected(), 0);
        window.accept(0).expect("first frame");
        window.accept(1).expect("second frame");
        window.accept(2).expect("third frame");
        assert_eq!(window.next_expected(), 3);
    }

    #[test]
    fn receive_window_rejects_replays_gaps_and_reorder() {
        let mut window = ReceiveWindow::new();
        window.accept(0).expect("first");
        // Duplicate / replay of an accepted sequence.
        assert_eq!(window.accept(0), Err(SequenceError::Replay));
        // Skip-ahead (gap / reorder) is rejected, not buffered.
        assert_eq!(window.accept(2), Err(SequenceError::OutOfOrder));
        // Very old frames are replays.
        assert_eq!(window.accept(u64::MAX), Err(SequenceError::OutOfOrder));
        // The window is unchanged after rejections.
        assert_eq!(window.next_expected(), 1);
        window.accept(1).expect("second");
        assert_eq!(window.next_expected(), 2);
    }

    #[test]
    fn send_counter_is_monotonic_and_exhausts_without_wrapping() {
        let mut counter = SendCounter::new();
        assert_eq!(counter.next(), Some(0));
        assert_eq!(counter.next(), Some(1));
        assert_eq!(counter.next(), Some(2));
        // The final value is handed out once, then the counter is exhausted
        // instead of wrapping (a wrap would reuse nonces).
        let mut saturated = SendCounter { next: u64::MAX - 1, exhausted: false };
        assert_eq!(saturated.next(), Some(u64::MAX - 1));
        assert_eq!(saturated.next(), Some(u64::MAX));
        assert_eq!(saturated.next(), None);
    }

    #[test]
    fn frame_size_cap_is_enforced() {
        let (room, keys) = test_room();
        let conn_key =
            derive_connection_key(&keys.control, &epoch(6), FrameDirection::ServerToClient)
                .expect("conn key");
        let oversized = vec![0u8; MAX_FRAME_BYTES];
        assert!(
            seal_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(6), 0, &oversized)
                .is_err(),
            "payload that cannot fit under the frame cap must be refused"
        );
        // The cap boundary itself fits.
        let boundary = vec![0u8; MAX_FRAME_BYTES - FRAME_HEADER_LEN - TAG_LEN];
        let frame = seal_frame(&conn_key, &room, FrameDirection::ServerToClient, &epoch(6), 0, &boundary)
            .expect("boundary payload fits exactly");
        assert_eq!(frame.len(), MAX_FRAME_BYTES);
        assert!(parse_frame_header(&frame).is_some());
    }

    #[test]
    fn generated_epochs_are_unique() {
        // The host room must issue distinct epochs; 8 random bytes make
        // collisions negligible, and the room tracks issued epochs.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let mut epoch = [0u8; EPOCH_LEN];
            SystemRandom::new().fill(&mut epoch).expect("epoch");
            assert!(seen.insert(epoch), "generated epochs must be unique");
        }
    }

    fn recorded_tree(cwd: &std::path::Path, sessions: &std::path::Path, id: &str, turns: usize) -> crate::SessionTree {
        let recorder = crate::start_session_in(cwd, None, Some("off"), Some(sessions), Some(id), None)
            .expect("start recorder");
        for turn in 0..turns {
            recorder
                .record_message(&pi_ai::Message::user_text(format!("prompt {turn}"), turn as i64))
                .expect("record prompt");
        }
        recorder.persist_now().expect("persist");
        recorder.tree().expect("tree")
    }

    /// First text block of every message entry, in order (metadata entries
    /// such as the recorder's thinking-level change carry no `message`).
    fn message_texts(snapshot: &serde_json::Value) -> Vec<String> {
        snapshot["entries"]
            .as_array()
            .expect("entries array")
            .iter()
            .filter_map(|entry| {
                entry["message"]["content"]
                    .as_array()?
                    .first()?
                    .get("text")?
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn public_snapshot_is_bounded_and_path_free() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let tree = recorded_tree(cwd.path(), sessions.path(), "snap-bounded", 5);
        let snapshot = public_snapshot(&tree, 10, 1024 * 1024).expect("snapshot");
        assert_eq!(snapshot["sessionId"], "snap-bounded");
        assert_eq!(snapshot["truncated"], false);
        let entries = snapshot["entries"].as_array().expect("entries array");
        // 5 user prompts + the recorder's thinking-level change entry; the
        // message entries stay the authoritative 5.
        assert_eq!(entries.len(), 6, "5 turns + 1 thinking change entry");
        assert_eq!(
            message_texts(&snapshot),
            ["prompt 0", "prompt 1", "prompt 2", "prompt 3", "prompt 4"]
        );
        // The host filesystem path never appears anywhere in the payload.
        let rendered = serde_json::to_string(&snapshot).expect("render");
        assert!(
            !rendered.contains(cwd.path().to_str().expect("utf8 cwd")),
            "snapshot must exclude the host cwd"
        );
        assert!(!rendered.contains("cwd"), "snapshot must not carry cwd keys");
    }

    #[test]
    fn public_snapshot_truncates_by_entries_and_bytes() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let tree = recorded_tree(cwd.path(), sessions.path(), "snap-trunc", 5);
        // Entry cap keeps the most recent entries, oldest-first.
        let by_entries = public_snapshot(&tree, 3, 1024 * 1024).expect("snapshot");
        assert_eq!(by_entries["truncated"], true);
        assert_eq!(
            message_texts(&by_entries),
            ["prompt 2", "prompt 3", "prompt 4"]
        );
        // Byte cap bounds the payload: a budget that fits exactly the two
        // most recent entries keeps those two and truncates the rest.
        let all = public_snapshot(&tree, 100, 1024 * 1024).expect("all");
        let all_entries = all["entries"].as_array().expect("entries array");
        let tail_two_bytes: usize = all_entries
            .iter()
            .rev()
            .take(2)
            .map(|entry| serde_json::to_vec(entry).expect("entry size").len())
            .sum();
        let by_bytes = public_snapshot(&tree, 100, tail_two_bytes + 1).expect("snapshot");
        assert_eq!(by_bytes["truncated"], true);
        assert_eq!(
            message_texts(&by_bytes),
            ["prompt 3", "prompt 4"]
        );
        // Zero cap yields an empty bounded list.

        let empty = public_snapshot(&tree, 0, 1024).expect("snapshot");
        assert_eq!(empty["entries"].as_array().expect("entries array").len(), 0);
    }
    #[test]
    fn public_text_redacts_colon_labeled_absolute_paths() {
        let unix_path = ["/", "private", "/", "collab", "/", "history.json"].concat();
        let windows_path = ["C:", r"\", "Users", r"\", "alice", r"\", "history.json"].concat();
        let text = format!("cwd:{unix_path} path:{windows_path}");

        assert_eq!(public_text(&text), "cwd:[PATH] path:[PATH]");
    }

    #[test]
    fn public_text_preserves_http_urls() {
        let text = "docs=http://example.test/public/index.html secure:https://example.test/a/b?q=1";

        assert_eq!(public_text(text), text);
    }

    #[test]
    fn public_value_redacts_secrets_paths_private_fields_and_bounds() {
        let secret = ["s", "k", "-", "abcdefghijklmnop", "1234567890"].concat();
        let unix_path = ["/", "tmp", "/", "collab-private", "/", "output.log"].concat();
        let windows_path = ["C:", r"\", "Users", r"\", "alice", r"\", "output.log"].concat();
        let projected = public_value(&serde_json::json!({
            "message": format!("token={secret} wrote {unix_path} and {windows_path}"),
            "nested": {"details": {"secret": &secret}, "data": &unix_path},
            "fullOutputPath": &unix_path,
            "items": [format!("Bearer {secret}"), &unix_path],
            "long": "x".repeat(MAX_PUBLIC_STRING_CHARS + 50),
        }));
        let encoded = serde_json::to_string(&projected).expect("encode public value");
        assert!(!encoded.contains(&secret), "secret must be redacted: {encoded}");
        assert!(!encoded.contains(&unix_path), "Unix path must be removed: {encoded}");
        assert!(!encoded.contains("Users\\\\alice"), "Windows path must be removed: {encoded}");
        assert!(!encoded.contains("fullOutputPath"));
        assert!(!encoded.contains("details"));
        assert!(!encoded.contains("data"));
        assert!(encoded.contains("[REDACTED]"));
        assert!(encoded.contains("[PATH]"));
        assert!(projected["long"].as_str().expect("long string").chars().count() <= MAX_PUBLIC_STRING_CHARS);
    }
}
