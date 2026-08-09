// Encrypted collaboration guest client — Web port of `pi_coding::collab`.
//
// Wire contract (locked with the CollabRuntimeService host over IRC):
// - Route:           /collab/ws/<roomId>
// - Subprotocol:     rpi-collab.<base64url-no-pad(SHA-256(role key))>
// - Hello (1st msg): TEXT JSON {type:"hello",version:1,roomId,role,epoch:
//                   base64url-no-pad(8 bytes)}. No 17-byte frame header,
//                   consumes NO sequence.
// - Encrypted frames: one binary WS message = header(17)
//                   epoch_be(8) || direction(1) || seq_be(8), then
//                   AES-256-GCM ciphertext+tag (tag last).
// - S2C window starts at seq 0: seq0 = {type:"snapshot",snapshot:{sessionId,
//                   truncated, entries:[SessionEntry...]}}, then
//                   {type:"event", event:<projected /ws event JSON>}, and sealed
//                   {type:"response", id?, command, success, error?} replies.
// - C2S window starts at seq 0 (control role only):
//                   {type:"command", command:"prompt"|"abort", message?, id?}
//                   with NO sessionId (the room is bound). View commands get an
//                   encrypted failure response, never dispatched.
// - Reconnect:       unexpected close + reopen; host issues a fresh epoch;
//                   both windows reset to 0; captured frames from the old
//                   connection fail decryption (different epoch => key).
// - Room stop:       Close 1001 with reason "collaboration room stopped" is
//                   terminal: cancel retry, reset crypto, wipe capability.
//
// # Secret handling
// The role key lives ONLY in the URL fragment. Browsers never send the
// fragment on the WS upgrade (or any request), and this module never copies
// it into storage, logs, or any outbound message — only its SHA-256 capability
// hash rides the subprotocol. Its decoded byte buffer has one owner and is
// zeroed on terminal teardown.
//
// # Typing note
// WebCrypto's `BufferSource` excludes `SharedArrayBuffer`-backed views. Every
// byte array here is constructed from a concrete `ArrayBuffer` (a fresh
// `new Uint8Array(n)`, a `TextEncoder` product, or a view over a WS
// `ArrayBuffer`), so they are typed `Uint8Array<ArrayBuffer>` and accepted by
// `crypto.subtle` without casts.

/** AES-256 key length in bytes. */
export const KEY_LEN = 32;
/** Random bytes per connection epoch (host-issued, 8 -> 64 bits). */
export const EPOCH_LEN = 8;
/** Sequence counter length in bytes (u64). */
export const SEQ_LEN = 8;
/** Direction tag length in bytes. */
export const DIRECTION_LEN = 1;
/** Plaintext frame header length: epoch(8) || direction(1) || seq(8). */
export const FRAME_HEADER_LEN = EPOCH_LEN + DIRECTION_LEN + SEQ_LEN; // 17
/** AES-GCM authentication tag length in bytes. */
export const TAG_LEN = 16;
/** Full nonce length: epoch_prefix(4) || seq_be(8). */
export const NONCE_LEN = 12;
/** Upper bound for one sealed frame (header + ciphertext + tag). */
export const MAX_FRAME_BYTES = 1024 * 1024;

/** Direction tag values (also the nonce/AAD direction byte). */
export const DIRECTION_CLIENT_TO_SERVER = 0x01;
export const DIRECTION_SERVER_TO_CLIENT = 0x02;

const HKDF_INFO_PREFIX = 'collab-v1';
const COLLAB_SUBPROTOCOL_PREFIX = 'rpi-collab.';
export const COLLAB_PATH_PREFIX = '/collab/ws/';
const RECONNECT_MAX_DELAY = 15000;
const ROOM_STOP_CLOSE_CODE = 1001;
const ROOM_STOP_CLOSE_REASON = 'collaboration room stopped';

/** A byte array guaranteed to be backed by a concrete `ArrayBuffer`, so it is
 *  a valid `BufferSource` for `crypto.subtle`. */
type Bytes = Uint8Array<ArrayBuffer>;

export type CollabRole = 'control' | 'view';

/** A parsed join link whose decoded capability has one in-memory owner. */
export class ParsedCollabLink {
  readonly roomId: string;
  readonly role: CollabRole;
  private readonly capability: CapabilityOwner;

  constructor(roomId: string, role: CollabRole, key: Bytes) {
    this.roomId = roomId;
    this.role = role;
    this.capability = new CapabilityOwner(key);
  }

  acquireCapability(): CapabilityLease {
    return this.capability.acquire();
  }

  destroyCapability(): void {
    this.capability.destroy();
  }
}

/** The parsed link remains the sole owner of the decoded bytes. Guests lease
 * the same buffer, avoiding a second capability copy. A zero-delay release
 * lets React StrictMode's probe cleanup reacquire the lease before a real
 * unmount destroys it. Explicit room stop destroys it synchronously. */
class CapabilityOwner {
  private key: Bytes | null;
  private leaseCount = 0;
  private wipeTimer: number | null = null;

  constructor(key: Bytes) {
    this.key = key;
  }

  acquire(): CapabilityLease {
    if (!this.key) throw new Error('collaboration capability is unavailable');
    if (this.wipeTimer !== null) {
      window.clearTimeout(this.wipeTimer);
      this.wipeTimer = null;
    }
    this.leaseCount += 1;
    return new CapabilityLease(this, this.key);
  }

  release(): void {
    if (this.leaseCount > 0) this.leaseCount -= 1;
    if (this.leaseCount !== 0 || !this.key || this.wipeTimer !== null) return;
    this.wipeTimer = window.setTimeout(() => {
      this.wipeTimer = null;
      if (this.leaseCount === 0) this.destroy();
    }, 0);
  }

  destroy(): void {
    if (this.wipeTimer !== null) {
      window.clearTimeout(this.wipeTimer);
      this.wipeTimer = null;
    }
    this.key?.fill(0);
    this.key = null;
    this.leaseCount = 0;
  }
}

class CapabilityLease {
  private owner: CapabilityOwner | null;
  private key: Bytes | null;

  constructor(owner: CapabilityOwner, key: Bytes) {
    this.owner = owner;
    this.key = key;
  }

  bytes(): Bytes | null {
    return this.key;
  }

  release(): void {
    this.key = null;
    this.owner?.release();
    this.owner = null;
  }

  destroy(): void {
    this.key = null;
    this.owner?.destroy();
    this.owner = null;
  }
}

export interface CollabSnapshot {
  sessionId: string;
  truncated: boolean;
  entries: unknown[];
}

/** A projected /ws control-plane event frame (same shape the /ws listener
 *  already emits: message_start, tool_execution_start, run_failed, ...). */
export type CollabEventFrame = { type: string; [key: string]: unknown };

export interface CollabResponse {
  type: 'response';
  id?: string;
  command: string;
  success: boolean;
  error?: string;
  data?: unknown;
}

export type CollabConnState =
  | 'connecting'
  | 'connected'
  | 'reconnecting'
  | 'closed'
  | 'error';

export interface CollabGuestHandlers {
  onStatus: (state: CollabConnState) => void;
  onSnapshot: (snapshot: CollabSnapshot) => void;
  onEvent: (event: CollabEventFrame) => void;
  onResponse: (response: CollabResponse) => void;
  onError: (message: string) => void;
}

/* ------------------------------------------------------------------ *
 * base64url (no padding) helpers
 * ------------------------------------------------------------------ */

function b64urlEncode(bytes: Bytes): string {
  let bin = '';
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function b64urlDecode(str: string): Bytes | null {
  // Reject any char outside the base64url alphabet up front so a malformed
  // fragment never silently decodes to the wrong key material.
  if (!/^[A-Za-z0-9_-]*$/.test(str)) return null;
  const padded = str.replace(/-/g, '+').replace(/_/g, '/');
  try {
    const bin = atob(padded);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

function utf8(s: string): Bytes {
  return new TextEncoder().encode(s);
}

function bytesEqual(a: Bytes, b: Bytes): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i] ^ b[i];
  return diff === 0;
}

/** Encode a u64 (≤ 2^53) as 8 big-endian bytes. */
function be64(seq: number): Bytes {
  const out = new Uint8Array(8);
  let v = BigInt(seq);
  for (let i = 7; i >= 0; i--) {
    out[i] = Number(v & 0xffn);
    v >>= 8n;
  }
  return out;
}

/** Read 8 big-endian bytes as a number (precise up to 2^53). */
function readBe64(bytes: Bytes, off: number): number {
  let v = 0n;
  for (let i = 0; i < 8; i++) v = (v << 8n) | BigInt(bytes[off + i]);
  return Number(v);
}

/* ------------------------------------------------------------------ *
 * Link parsing — mirrors `pi_coding::collab::parse_link`
 * ------------------------------------------------------------------ */

/** Parse the current document location for a collab guest link.
 *
 *  Pathname must be exactly `/collab/ws/<roomId>`; the fragment must be
 *  exactly one `c=<key>` or `v=<key>` with a 32-byte base64url-no-pad key.
 *  Returns null for anything else (the normal `/web` session UX). The key is
 *  read ONLY from `location.hash` — which browsers never transmit — and this
 *  function never persists or logs it. */
export function parseCollabLocation(loc: Location = location): ParsedCollabLink | null {
  const path = loc.pathname;
  if (!path.startsWith(COLLAB_PATH_PREFIX)) return null;
  const roomId = path.slice(COLLAB_PATH_PREFIX.length);
  // Room id rules (mirror Rust parse_link): non-empty, no `/` `?` `#`, visible
  // ASCII only.
  if (
    !roomId ||
    roomId.includes('/') ||
    roomId.includes('?') ||
    roomId.includes('#') ||
    ![...roomId].every((c) => {
      const b = c.charCodeAt(0);
      return b >= 0x21 && b <= 0x7e;
    })
  ) {
    return null;
  }
  const hash = loc.hash;
  if (!hash.startsWith('#')) return null;
  const frag = hash.slice(1);
  let role: CollabRole;
  let encoded: string;
  if (frag.startsWith('c=')) {
    role = 'control';
    encoded = frag.slice(2);
  } else if (frag.startsWith('v=')) {
    role = 'view';
    encoded = frag.slice(2);
  } else {
    return null;
  }
  // Exactly one role key: no `=`, no `&`, non-empty (mirror Rust).
  if (!encoded || encoded.includes('=') || encoded.includes('&')) return null;
  const keyBytes = b64urlDecode(encoded);
  if (!keyBytes || keyBytes.length !== KEY_LEN) return null;
  return new ParsedCollabLink(roomId, role, keyBytes);
}

/** True when the current location is a collab guest link. */
export function isCollabGuestMode(loc: Location = location): boolean {
  const link = parseCollabLocation(loc);
  if (!link) return false;
  link.destroyCapability();
  return true;
}

/* ------------------------------------------------------------------ *
 * Capability subprotocol — `rpi-collab.<base64url(SHA-256(key))>`
 * ------------------------------------------------------------------ */

async function capabilitySubprotocol(key: Bytes): Promise<string> {
  const hash = await crypto.subtle.digest('SHA-256', key);
  return COLLAB_SUBPROTOCOL_PREFIX + b64urlEncode(new Uint8Array(hash));
}

/* ------------------------------------------------------------------ *
 * Per-connection directional key — HKDF-SHA256
 *   ikm = role_key, salt = epoch, info = "collab-v1" || direction_tag
 * ------------------------------------------------------------------ */

async function deriveConnectionKey(
  roleKey: Bytes,
  epoch: Bytes,
  directionTag: number,
): Promise<CryptoKey> {
  const baseKey = await crypto.subtle.importKey('raw', roleKey, 'HKDF', false, ['deriveKey']);
  const info = new Uint8Array([...utf8(HKDF_INFO_PREFIX), directionTag]);
  return crypto.subtle.deriveKey(
    { name: 'HKDF', hash: 'SHA-256', salt: epoch, info },
    baseKey,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt'],
  );
}

/* ------------------------------------------------------------------ *
 * Frame seal / open — AES-256-GCM
 *   nonce = epoch_prefix(4) || seq_be(8)
 *   aad   = room_id || direction(1) || seq_be(8)
 * ------------------------------------------------------------------ */

function buildNonce(epoch: Bytes, seq: number): Bytes {
  const nonce = new Uint8Array(NONCE_LEN);
  nonce.set(epoch.subarray(0, 4), 0);
  nonce.set(be64(seq), 4);
  return nonce;
}

function buildAad(roomId: string, directionTag: number, seq: number): Bytes {
  const rid = utf8(roomId);
  const aad = new Uint8Array(rid.length + 1 + SEQ_LEN);
  aad.set(rid, 0);
  aad[rid.length] = directionTag;
  aad.set(be64(seq), rid.length + 1);
  return aad;
}

/** Seal one plaintext payload into a binary frame. */
export async function sealFrame(
  key: CryptoKey,
  roomId: string,
  directionTag: number,
  epoch: Bytes,
  seq: number,
  plaintext: Bytes,
): Promise<Bytes> {
  if (plaintext.length + FRAME_HEADER_LEN + TAG_LEN > MAX_FRAME_BYTES) {
    throw new Error('collab frame payload exceeds the frame cap');
  }
  const nonce = buildNonce(epoch, seq);
  const aad = buildAad(roomId, directionTag, seq);
  const ct = new Uint8Array(
    await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv: nonce, additionalData: aad, tagLength: 128 },
      key,
      plaintext,
    ),
  );
  const frame = new Uint8Array(FRAME_HEADER_LEN + ct.length);
  frame.set(epoch, 0);
  frame[EPOCH_LEN] = directionTag;
  frame.set(be64(seq), EPOCH_LEN + DIRECTION_LEN);
  frame.set(ct, FRAME_HEADER_LEN);
  return frame;
}

/** Open a sealed frame. Verifies the header (epoch/direction/seq) against the
 *  expected connection context BEFORE authenticating the body; throws on any
 *  mismatch (tamper, replay, cross-room/role/direction). */
export async function openFrame(
  key: CryptoKey,
  roomId: string,
  directionTag: number,
  epoch: Bytes,
  seq: number,
  frame: Bytes,
): Promise<Bytes> {
  if (frame.length < FRAME_HEADER_LEN + TAG_LEN || frame.length > MAX_FRAME_BYTES) {
    throw new Error('collab frame has a malformed or oversized header');
  }
  const headerEpoch = frame.subarray(0, EPOCH_LEN);
  const headerDir = frame[EPOCH_LEN];
  const headerSeq = readBe64(frame, EPOCH_LEN + DIRECTION_LEN);
  if (headerDir !== directionTag || headerSeq !== seq || !bytesEqual(headerEpoch, epoch)) {
    throw new Error('collab frame header does not match the expected epoch/direction/sequence');
  }
  const body = frame.subarray(FRAME_HEADER_LEN);
  const nonce = buildNonce(epoch, seq);
  const aad = buildAad(roomId, directionTag, seq);
  return new Uint8Array(
    await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: nonce, additionalData: aad, tagLength: 128 },
      key,
      body,
    ),
  );
}

/* ------------------------------------------------------------------ *
 * Sequence counters — strict monotonic per connection/direction
 * ------------------------------------------------------------------ */

/** Sender-side monotonic counter (per connection, per direction). Saturates at
 *  Number.MAX_SAFE_INTEGER instead of wrapping (a wrap would reuse nonces). */
export class SendCounter {
  private next = 0;
  private exhausted = false;
  /** The next sequence value, or null once the sequence space is exhausted. */
  nextValue(): number | null {
    if (this.exhausted) return null;
    const value = this.next;
    if (value >= Number.MAX_SAFE_INTEGER) {
      this.exhausted = true;
    } else {
      this.next += 1;
    }
    return value;
  }
}

export type SequenceError = 'replay' | 'out-of-order' | 'exhausted';

/** Receiver-side strict monotonic window: only the exact next expected
 *  sequence is accepted. Duplicates, replays, gaps, and reordered frames are
 *  rejected (WebSocket delivery is ordered, so any deviation is an attack or
 *  a bug). */
export class ReceiveWindow {
  private nextExpected = 0;
  /** The next sequence this window will accept. */
  nextExpectedValue(): number {
    return this.nextExpected;
  }
  /** Validate `seq` and advance on success; return an error otherwise. */
  accept(seq: number): SequenceError | null {
    if (seq < this.nextExpected) return 'replay';
    if (seq > this.nextExpected) return 'out-of-order';
    if (this.nextExpected >= Number.MAX_SAFE_INTEGER) return 'exhausted';
    this.nextExpected += 1;
    return null;
  }
}

/* ------------------------------------------------------------------ *
 * CollabGuest — the connection lifecycle
 * ------------------------------------------------------------------ */

/** Read a decrypted S2C payload as JSON. */
function parsePlaintext(pt: Bytes): Record<string, unknown> | null {
  try {
    return JSON.parse(new TextDecoder().decode(pt)) as Record<string, unknown>;
  } catch {
    return null;
  }
}

/** Encrypted collaboration guest connection.
 *
 *  Owns the WebSocket to `/collab/ws/<roomId>`, the host hello handshake, the
 *  per-connection directional HKDF keys, the strict S2C receive window, the C2S
 *  send counter (control only), and reconnect-with-fresh-epoch. The parsed link
 *  remains the sole owner of the role-key byte buffer; this guest holds a lease
 *  that terminal teardown destroys and ordinary unmount releases. Only the
 *  SHA-256 capability hash rides the subprotocol. */
export class CollabGuest {
  private readonly roomId: string;
  private readonly role: CollabRole;
  private capability: CapabilityLease | null;
  private handlers: CollabGuestHandlers;
  private ws: WebSocket | null = null;
  private epoch: Bytes | null = null;
  private s2cKey: CryptoKey | null = null;
  private c2sKey: CryptoKey | null = null;
  private s2cWindow = new ReceiveWindow();
  private c2sCounter = new SendCounter();
  private keysReady = false;
  private pendingFrames: Bytes[] = [];
  private delay = 1000;
  private retryTimer: number | null = null;
  private stopped = false;

  constructor(link: ParsedCollabLink, handlers: CollabGuestHandlers) {
    this.roomId = link.roomId;
    this.role = link.role;
    this.capability = link.acquireCapability();
    this.handlers = handlers;
  }

  /** Begin connecting. */
  start(): void {
    if (!this.capability) return;
    this.stopped = false;
    void this.connect();
  }

  /** Tear down the component-owned connection. No further handler callbacks
   * fire. The capability lease is released and is wiped after the current task
   * unless React StrictMode immediately reacquires it for its probe remount. */
  stop(): void {
    this.stopped = true;
    this.handlers = noopHandlers;
    this.cancelRetry();
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      try {
        ws.close(1000, 'guest stopped');
      } catch {
        /* already closed */
      }
    }
    this.resetCrypto();
    this.capability?.release();
    this.capability = null;
  }

  /** Send a control command (prompt/abort). Control role only; throws for
   *  view guests (the composer must also disable them, but the protocol
   *  guarantees an encrypted failure response if a view frame ever reaches
   *  the host). `id` echoes back on the sealed response so the UI can drop an
   *  optimistic bubble on refusal. */
  async sendCommand(command: 'prompt' | 'abort', message?: string, id?: string): Promise<void> {
    if (this.role !== 'control') {
      throw new Error('view guests cannot issue commands');
    }
    if (!this.c2sKey || !this.epoch || !this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error('collab connection is not ready');
    }
    const seq = this.c2sCounter.nextValue();
    if (seq === null) {
      this.fail('sequence space exhausted');
      throw new Error('sequence space exhausted');
    }
    const payload: Record<string, unknown> = { type: 'command', command };
    if (command === 'prompt') payload.message = message ?? '';
    if (id) payload.id = id;
    const plaintext = utf8(JSON.stringify(payload));
    const frame = await sealFrame(
      this.c2sKey,
      this.roomId,
      DIRECTION_CLIENT_TO_SERVER,
      this.epoch,
      seq,
      plaintext,
    );
    // The socket may have closed during the async seal; drop silently.
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(frame);
  }

  // -- internal -----------------------------------------------------

  /** Reset all per-connection crypto state so captured frames from a previous
   *  connection can never replay (different epoch => different key => tag
   *  failure). Called on teardown and before every reconnect. */
  private resetCrypto(): void {
    this.epoch = null;
    this.s2cKey = null;
    this.c2sKey = null;
    this.keysReady = false;
    this.s2cWindow = new ReceiveWindow();
    this.c2sCounter = new SendCounter();
    this.pendingFrames = [];
  }

  private async connect(): Promise<void> {
    if (this.stopped) return;
    const capability = this.capability;
    const roleKey = capability?.bytes();
    if (!capability || !roleKey) return;
    this.emit('connecting');
    let subprotocol: string;
    try {
      subprotocol = await capabilitySubprotocol(roleKey);
    } catch {
      this.fail('cannot derive capability');
      return;
    }
    if (this.stopped || capability !== this.capability) return;
    const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
    // No fragment on the WS URL — the key stays in the browser only.
    const url = `${scheme}${location.host}${COLLAB_PATH_PREFIX}${this.roomId}`;
    let ws: WebSocket;
    try {
      ws = new WebSocket(url, subprotocol);
    } catch (err) {
      this.fail(`cannot open WebSocket: ${(err as Error).message}`);
      return;
    }
    ws.binaryType = 'arraybuffer';
    // Replace any prior socket.
    const old = this.ws;
    this.ws = ws;
    if (old) {
      old.onopen = null;
      old.onmessage = null;
      old.onclose = null;
      old.onerror = null;
      try {
        old.close(1000, 'replaced');
      } catch {
        /* already closed */
      }
    }
    ws.onopen = () => {
      // The relay must echo the requested subprotocol; a missing/mismatched
      // echo means it did not authenticate the capability — drop.
      if (ws.protocol !== subprotocol) {
        this.fail('relay did not accept the collab subprotocol');
        try {
          ws.close(1002, 'subprotocol mismatch');
        } catch {
          /* ignore */
        }
        return;
      }
      // Wait for the host hello (text) before switching to 'connected'.
    };
    ws.onmessage = (event: MessageEvent) => {
      if (typeof event.data === 'string') {
        this.onHello(event.data);
      } else if (event.data instanceof ArrayBuffer) {
        void this.onBinaryFrame(new Uint8Array(event.data));
      }
      // Blob never arrives: binaryType is 'arraybuffer'.
    };
    ws.onclose = (event: CloseEvent) => {
      if (event.target !== this.ws) return; // superseded
      this.ws = null;
      if (this.stopped) return;
      if (event.code === ROOM_STOP_CLOSE_CODE && event.reason === ROOM_STOP_CLOSE_REASON) {
        this.terminateRoomStop();
        return;
      }
      this.emit('reconnecting');
      this.resetCrypto();
      this.scheduleReconnect();
    };
    ws.onerror = () => {
      /* the close event carries the failure */
    };
  }

  /** Handle the plaintext host hello (TEXT JSON). Derive the directional keys
   *  and arm the S2C window; then drain any frames that arrived during the
   *  async derivation. */
  private onHello(data: string): void {
    if (this.stopped) return;
    let hello: { type?: string; version?: number; roomId?: string; role?: string; epoch?: string };
    try {
      hello = JSON.parse(data);
    } catch {
      this.fail('invalid collab hello');
      return;
    }
    if (hello.type !== 'hello' || hello.version !== 1) {
      this.fail('unsupported collab hello');
      return;
    }
    if (hello.roomId !== this.roomId) {
      this.fail('collab hello room id mismatch');
      return;
    }
    // The host echoes the role the capability granted; it must match the link.
    if (hello.role !== this.role) {
      this.fail('collab hello role mismatch');
      return;
    }
    const epochBytes = typeof hello.epoch === 'string' ? b64urlDecode(hello.epoch) : null;
    if (!epochBytes || epochBytes.length !== EPOCH_LEN) {
      this.fail('collab hello epoch is malformed');
      return;
    }
    this.resetCrypto();
    this.epoch = epochBytes;
    void this.deriveKeys();
  }

  private async deriveKeys(): Promise<void> {
    const capability = this.capability;
    const roleKey = capability?.bytes();
    const epoch = this.epoch;
    if (this.stopped || !capability || !roleKey || !epoch) return;
    let s2cKey: CryptoKey;
    let c2sKey: CryptoKey;
    try {
      s2cKey = await deriveConnectionKey(roleKey, epoch, DIRECTION_SERVER_TO_CLIENT);
      if (this.stopped || capability !== this.capability || epoch !== this.epoch) return;
      c2sKey = await deriveConnectionKey(roleKey, epoch, DIRECTION_CLIENT_TO_SERVER);
    } catch {
      this.fail('collab key derivation failed');
      return;
    }
    if (this.stopped || capability !== this.capability || epoch !== this.epoch) return;
    this.s2cKey = s2cKey;
    this.c2sKey = c2sKey;
    this.keysReady = true;
    this.emit('connected');
    // Drain frames buffered while keys were being derived (the snapshot
    // often lands immediately after the hello).
    const queued = this.pendingFrames;
    this.pendingFrames = [];
    for (const frame of queued) {
      await this.processFrame(frame);
    }
  }

  private async onBinaryFrame(frame: Bytes): Promise<void> {
    if (this.stopped) return;
    if (!this.keysReady) {
      // Hold the frame until the hello keys land; ordered WS delivery
      // guarantees the drain replays them in arrival order.
      this.pendingFrames.push(frame);
      return;
    }
    await this.processFrame(frame);
  }

  private async processFrame(frame: Bytes): Promise<void> {
    if (this.stopped || !this.s2cKey || !this.epoch) return;
    if (frame.length < FRAME_HEADER_LEN + TAG_LEN || frame.length > MAX_FRAME_BYTES) {
      this.fail('collab frame is malformed');
      return;
    }
    const dir = frame[EPOCH_LEN];
    if (dir !== DIRECTION_SERVER_TO_CLIENT) {
      this.fail('collab frame has an unexpected direction');
      return;
    }
    const seq = readBe64(frame, EPOCH_LEN + DIRECTION_LEN);
    const err = this.s2cWindow.accept(seq);
    if (err) {
      // Strict monotonicity: a duplicate, gap, or reorder is an attack or a
      // bug. Drop the connection and reconnect with a fresh epoch.
      this.fail(`collab sequence violation: ${err}`);
      return;
    }
    let plaintext: Bytes;
    try {
      plaintext = await openFrame(
        this.s2cKey,
        this.roomId,
        DIRECTION_SERVER_TO_CLIENT,
        this.epoch,
        seq,
        frame,
      );
    } catch {
      // Tag/header failure: tampering, replay across epochs, or a buggy host.
      // Never render unauthenticated bytes — reconnect.
      this.fail('collab frame authentication failed');
      return;
    }
    if (this.stopped) return;
    this.dispatch(plaintext);
  }

  private dispatch(plaintext: Bytes): void {
    const payload = parsePlaintext(plaintext);
    if (!payload) {
      this.fail('collab frame payload is not valid JSON');
      return;
    }
    switch (payload.type) {
      case 'snapshot': {
        const snap = payload.snapshot as CollabSnapshot | undefined;
        if (!snap || typeof snap !== 'object') {
          this.fail('collab snapshot is malformed');
          return;
        }
        this.handlers.onSnapshot(snap);
        break;
      }
      case 'event': {
        const ev = payload.event as CollabEventFrame | undefined;
        if (!ev || typeof ev !== 'object') return; // skip a bad event, don't drop
        this.handlers.onEvent(ev);
        break;
      }
      case 'response': {
        const resp = payload as unknown as CollabResponse;
        if (typeof resp.command !== 'string' || typeof resp.success !== 'boolean') return;
        this.handlers.onResponse(resp);
        break;
      }
      default:
        // Unknown payload kinds are ignored (forward-compat), never fatal.
        break;
    }
  }

  private cancelRetry(): void {
    if (this.retryTimer === null) return;
    window.clearTimeout(this.retryTimer);
    this.retryTimer = null;
  }

  private terminateRoomStop(): void {
    this.stopped = true;
    this.cancelRetry();
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
    }
    this.resetCrypto();
    this.capability?.destroy();
    this.capability = null;
    const handlers = this.handlers;
    this.handlers = noopHandlers;
    handlers.onStatus('closed');
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.retryTimer !== null) return;
    const delay = this.delay;
    this.delay = Math.min(this.delay * 2, RECONNECT_MAX_DELAY);
    this.retryTimer = window.setTimeout(() => {
      this.retryTimer = null;
      void this.connect();
    }, delay);
  }

  /** Fail the current connection: close the socket and reconnect with a fresh
   *  epoch. The hello contract guarantees a forged epoch only breaks this one
   *  connection (DoS), never key/nonce reuse. */
  private fail(message: string): void {
    if (this.stopped) return;
    this.handlers.onError(message);
    this.resetCrypto();
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      try {
        ws.close(4000, 'collab error');
      } catch {
        /* ignore */
      }
    }
    this.emit('reconnecting');
    this.scheduleReconnect();
  }

  private emit(state: CollabConnState): void {
    if (!this.stopped) this.handlers.onStatus(state);
  }
}

const noopHandlers: CollabGuestHandlers = {
  onStatus: () => {},
  onSnapshot: () => {},
  onEvent: () => {},
  onResponse: () => {},
  onError: () => {},
};