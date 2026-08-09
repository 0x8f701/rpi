#!/usr/bin/env node
// Collaboration CLI/TUI-style guest E2E client.
//
// Implements the full collab wire protocol from crates/pi-coding/src/collab.rs:
// link parsing, SHA-256 capability hash subprotocol, HKDF-SHA256 per-connection
// directional key derivation, AES-256-GCM frame seal/open, WS connect with the
// rpi-collab.<cap> subprotocol, plaintext hello handshake, encrypted snapshot
// decryption, encrypted command sending (prompt/abort), live event capture,
// disconnect/rejoin with fresh epoch, ciphertext-absence-of-plaintext checks,
// host-path-absence checks, and host-only-lifecycle enforcement.
//
// Runtime commands (sent by the orchestrator shell, NOT this guest):
//   collab_start  POST /rpc  {"type":"collab_start","id":"...","baseUrl":"http://127.0.0.1:PORT"}
//     → {"type":"response","command":"collab_start","success":true,
//        "data":{"roomId":"...","sessionId":"...","controlLink":"...","viewLink":"..."}}
//   collab_status POST /rpc  {"type":"collab_status","id":"...","roomId":"..."}
//     → {"type":"response","command":"collab_status","success":true,
//        "data":{"rooms":[{"roomId","sessionId","participants","controlParticipants",
//                          "viewParticipants","participantLimit","running"}]}}
//   collab_stop   POST /rpc  {"type":"collab_stop","id":"...","roomId":"..."}
//     → {"type":"response","command":"collab_stop","success":true,
//        "data":{"stopped":true,"room":{...}}}
//
// Wire protocol (collab.rs + collab_service.rs):
//   WS route:     /collab/ws/<roomId>
//   Subprotocol:  rpi-collab.<base64url-no-pad(SHA-256(role-key))>
//   First frame:  TEXT JSON {"type":"hello","version":1,"roomId":"<id>","role":"control|view","epoch":"<b64url 8B>"}
//   Second frame: BINARY encrypted seq-0 snapshot
//   Subsequent:  BINARY encrypted events/responses (s2c), BINARY commands (c2s)
//   Frame layout: header(17: epoch_be(8)||dir(1)||seq_be(8)) || ciphertext || GCM-tag(16)
//   Direction:    0x01 = client→server, 0x02 = server→client
//   Nonce (12B):  epoch[0..4] || seq_be(8)
//   AAD:          room_id_bytes || direction_byte || seq_be(8)
//   Key derivation: HKDF-SHA256(ikm=role_key, salt=epoch, info="collab-v1"||direction_byte, 32)
//   Server payloads: {"type":"snapshot","snapshot":{sessionId,truncated,entries}}
//                    {"type":"event","event":{...}}
//                    {"type":"response","id","command":"prompt|abort","success":bool,"error"?}
//   Client commands: {"type":"command","command":"prompt","id","message"}
//                    {"type":"command","command":"abort","id"}
//
// Environment:
//   COLLAB_LINK        Join link (http://host:port/collab/ws/<roomId>#c=<key> or #v=<key>)
//   COLLAB_ROLE        "control" or "view" (must match the link fragment)
//   COLLAB_EVIDENCE    Evidence directory for output files
//   COLLAB_EXPECT      Comma-separated plaintext strings expected in the decrypted snapshot
//   COLLAB_EVENT_EXPECT Comma-separated plaintext strings expected in live decrypted events
//   COLLAB_HOST_PATH   Host workspace path that must NOT appear in the decrypted snapshot
//   COLLAB_PROMPT      Prompt text to send (default: "collab-e2e-prompt")
//   COLLAB_EVENT_TIMEOUT   Seconds to wait for live events after prompt (default: 15)
//   COLLAB_CONNECTED_MARKER Path written after the initial encrypted snapshot assertions
//   COLLAB_START_MARKER     Marker that releases role commands after every guest is connected
//   COLLAB_PHASE_TIMEOUT    Seconds to wait for the start marker (default: 45)
//   COLLAB_WAIT_STOP        "1" = after all tests, reconnect and wait for host stop close
//   COLLAB_STOP_TIMEOUT     Seconds to wait for stop close (default: 30)
// Exit: 0 = all assertions passed; 2 = one or more assertions failed.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { WebSocket } from 'ws';

// ── Constants (match crates/pi-coding/src/collab.rs) ──────────────────────

const KEY_LEN = 32;
const EPOCH_LEN = 8;
const FRAME_HEADER_LEN = 17;
const TAG_LEN = 16;
const DIR_C2S = 0x01;
const DIR_S2C = 0x02;
const HKDF_INFO_PREFIX = Buffer.from('collab-v1', 'ascii');

const evidenceDir = process.env.COLLAB_EVIDENCE || '.';
// ── Assertion helper ──────────────────────────────────────────────────────

const results = [];

function assert(id, description, condition, details) {
  const passed = !!condition;
  // Retain details only on failure so passing logs never print failure phrases
  // or sensitive comparison values (keys, host paths, plaintext markers).
  const entry = { id, description, passed };
  if (!passed) entry.details = details != null ? String(details) : null;
  results.push(entry);
  const tag = passed ? 'PASS' : 'FAIL';
  if (passed) {
    console.error(`[collab-guest] ${tag} ${id}: ${description}`);
  } else {
    const detailSuffix = entry.details ? ` — ${entry.details.slice(0, 200)}` : '';
    console.error(`[collab-guest] ${tag} ${id}: ${description}${detailSuffix}`);
  }
}

// ── Crypto helpers ────────────────────────────────────────────────────────

function parseLink(link) {
  const url = new URL(link);
  if (!url.pathname.startsWith('/collab/ws/')) {
    throw new Error(`link path must start with /collab/ws/ (got ${url.pathname})`);
  }
  const roomId = decodeURIComponent(url.pathname.slice('/collab/ws/'.length));
  if (!roomId || roomId.includes('/') || roomId.includes('?') || roomId.includes('#')) {
    throw new Error(`invalid room id in link: ${roomId}`);
  }
  const fragment = url.hash.slice(1);
  let role, encoded;
  if (fragment.startsWith('c=')) {
    role = 'control';
    encoded = fragment.slice(2);
  } else if (fragment.startsWith('v=')) {
    role = 'view';
    encoded = fragment.slice(2);
  } else {
    throw new Error(`link fragment must start with c= or v= (got ${fragment})`);
  }
  if (!encoded || encoded.includes('=') || encoded.includes('&')) {
    throw new Error(`link fragment must carry exactly one role key`);
  }
  const key = Buffer.from(encoded, 'base64url');
  if (key.length !== KEY_LEN) {
    throw new Error(`link key must decode to ${KEY_LEN} bytes (got ${key.length})`);
  }
  const wsScheme = url.protocol === 'https:' ? 'wss:' : 'ws:';
  const wsUrl = `${wsScheme}//${url.host}/collab/ws/${encodeURIComponent(roomId)}`;
  return { roomId, role, key, encoded, wsUrl };
}

function sha256(buf) {
  return crypto.createHash('sha256').update(buf).digest();
}

function capabilityB64(key) {
  return sha256(key).toString('base64url');
}

function deriveKey(roleKey, epoch, dirByte) {
  const info = Buffer.concat([HKDF_INFO_PREFIX, Buffer.from([dirByte])]);
  const derived = crypto.hkdfSync('sha256', roleKey, epoch, info, KEY_LEN);
  return Buffer.from(derived);
}

function buildNonce(epoch, seq) {
  const nonce = Buffer.alloc(12);
  epoch.copy(nonce, 0, 0, 4);
  nonce.writeBigUInt64BE(BigInt(seq), 4);
  return nonce;
}

function buildAad(roomId, dirByte, seq) {
  const roomIdBytes = Buffer.from(roomId, 'ascii');
  const aad = Buffer.alloc(roomIdBytes.length + 1 + 8);
  roomIdBytes.copy(aad, 0);
  aad[roomIdBytes.length] = dirByte;
  aad.writeBigUInt64BE(BigInt(seq), roomIdBytes.length + 1);
  return aad;
}

function sealFrame(key, roomId, dirByte, epoch, seq, plaintext) {
  const nonce = buildNonce(epoch, seq);
  const aad = buildAad(roomId, dirByte, seq);
  const cipher = crypto.createCipheriv('aes-256-gcm', key, nonce, { authTagLength: TAG_LEN });
  cipher.setAAD(aad);
  const ciphertext = Buffer.concat([cipher.update(plaintext), cipher.final()]);
  const tag = cipher.getAuthTag();
  const header = Buffer.alloc(FRAME_HEADER_LEN);
  epoch.copy(header, 0);
  header[EPOCH_LEN] = dirByte;
  header.writeBigUInt64BE(BigInt(seq), EPOCH_LEN + 1);
  return Buffer.concat([header, ciphertext, tag]);
}

function openFrame(key, roomId, dirByte, epoch, seq, frame) {
  if (frame.length < FRAME_HEADER_LEN + TAG_LEN) {
    throw new Error(`frame too short (${frame.length} bytes)`);
  }
  const headerEpoch = frame.subarray(0, EPOCH_LEN);
  const headerDir = frame[EPOCH_LEN];
  const headerSeq = Number(frame.readBigUInt64BE(EPOCH_LEN + 1));
  if (!headerEpoch.equals(epoch) || headerDir !== dirByte || headerSeq !== seq) {
    throw new Error(`frame header mismatch: epoch=${headerEpoch.toString('hex')} dir=${headerDir} seq=${headerSeq} (expected epoch=${epoch.toString('hex')} dir=${dirByte} seq=${seq})`);
  }
  const body = frame.subarray(FRAME_HEADER_LEN);
  const tag = body.subarray(body.length - TAG_LEN);
  const ciphertext = body.subarray(0, body.length - TAG_LEN);
  const nonce = buildNonce(epoch, seq);
  const aad = buildAad(roomId, dirByte, seq);
  const decipher = crypto.createDecipheriv('aes-256-gcm', key, nonce, { authTagLength: TAG_LEN });
  decipher.setAAD(aad);
  decipher.setAuthTag(tag);
  return Buffer.concat([decipher.update(ciphertext), decipher.final()]);
}

// ── Guest connection wrapper ─────────────────────────────────────────────

class GuestConnection {
  constructor(wsUrl, subprotocol) {
    this.wsUrl = wsUrl;
    this.subprotocol = subprotocol;
    this.ws = null;
    this.messages = [];
    this.closeInfo = null;
    this.openError = null;
  }

  connect(timeoutMs = 15000) {
    return new Promise((resolve, reject) => {
      this.ws = new WebSocket(this.wsUrl, [this.subprotocol]);
      let settled = false;
      const timer = setTimeout(() => {
        if (!settled) {
          settled = true;
          reject(new Error('WS open timeout'));
        }
      }, timeoutMs);

      this.ws.on('open', () => {
        clearTimeout(timer);
        if (!settled) {
          settled = true;
          resolve();
        }
      });
      this.ws.on('message', (data, isBinary) => {
        this.messages.push({ data: Buffer.from(data), isBinary });
      });
      this.ws.on('close', (code, reason) => {
        this.closeInfo = { code, reason: reason.toString() };
        clearTimeout(timer);
        if (!settled) {
          settled = true;
          reject(new Error(`WS closed before open (code ${code})`));
        }
      });
      this.ws.on('error', (err) => {
        clearTimeout(timer);
        if (!settled) {
          settled = true;
          this.openError = err;
          reject(err);
        }
      });
    });
  }

  async nextMessage(timeoutMs = 20000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (this.messages.length > 0) {
        return this.messages.shift();
      }
      if (this.closeInfo) {
        return { closed: true, ...this.closeInfo };
      }
      await new Promise((r) => setTimeout(r, 50));
    }
    throw new Error(`message timeout after ${timeoutMs}ms`);
  }

  send(data) {
    this.ws.send(data);
  }

  close() {
    try { this.ws.close(); } catch {}
  }

  get protocol() {
    return this.ws ? this.ws.protocol : '';
  }
}

// ── Main guest logic ─────────────────────────────────────────────────────

async function runGuest() {
  const link = process.env.COLLAB_LINK;
  const role = process.env.COLLAB_ROLE || '';
  fs.mkdirSync(evidenceDir, { recursive: true });
  if (!link) {
    assert('CG-FATAL', 'COLLAB_LINK is required', false, 'missing COLLAB_LINK');
    writeResults();
    return;
  }
  // COLLAB_EXPECT: decrypted snapshot / ciphertext checks only.
  const expectedTexts = (process.env.COLLAB_EXPECT || '').split(',').map(s => s.trim()).filter(Boolean);
  // COLLAB_EVENT_EXPECT: live-event stream checks (CG-12 / CG-07e), separate from snapshot markers.
  const eventExpectedTexts = (process.env.COLLAB_EVENT_EXPECT || '').split(',').map(s => s.trim()).filter(Boolean);
  const hostPath = process.env.COLLAB_HOST_PATH || '';
  const promptText = process.env.COLLAB_PROMPT || 'collab-e2e-prompt';
  const eventTimeout = parseInt(process.env.COLLAB_EVENT_TIMEOUT || '15', 10) * 1000;
  const connectedMarker = process.env.COLLAB_CONNECTED_MARKER || '';
  const startMarker = process.env.COLLAB_START_MARKER || '';
  const phaseTimeout = parseInt(process.env.COLLAB_PHASE_TIMEOUT || '45', 10) * 1000;
  const waitStop = process.env.COLLAB_WAIT_STOP === '1';
  const stopTimeout = parseInt(process.env.COLLAB_STOP_TIMEOUT || '30', 10) * 1000;

  // Binary-safe containment without echoing secrets into logs/details.
  const bufHasStr = (buf, str) => {
    if (!buf || !str) return false;
    return Buffer.isBuffer(buf)
      ? buf.includes(Buffer.from(str, 'utf8'))
      : Buffer.from(String(buf), 'binary').includes(Buffer.from(str, 'utf8'));
  };
  const bufHasBuf = (buf, needle) => {
    if (!buf || !needle) return false;
    return Buffer.isBuffer(buf) ? buf.includes(needle) : false;
  };

  // Parse the link.
  let parsed;
  try {
    parsed = parseLink(link);
  } catch (e) {
    assert('CG-LINK', 'join link parses to room id + role + 32-byte key', false, e.message);
    writeResults();
    return;
  }
  assert('CG-LINK', 'join link parses to room id + role + 32-byte key', true,
    `roomId=${parsed.roomId} role=${parsed.role}`);

  // Verify role matches the link.
  assert('CG-ROLE', 'COLLAB_ROLE matches link fragment role', parsed.role === role,
    `envRoleLen=${role.length} linkRole=${parsed.role}`);

  // Compute the capability hash subprotocol.
  const capB64 = capabilityB64(parsed.key);
  const subprotocol = `rpi-collab.${capB64}`;
  assert('CG-01', 'subprotocol uses SHA-256 capability hash (not raw key)',
    subprotocol === `rpi-collab.${capB64}` && !subprotocol.includes(parsed.encoded),
    'capability hash offered without raw role key');
  // ── Phase 1: Connect, hello, snapshot ──────────────────────────────────

  const conn = new GuestConnection(parsed.wsUrl, subprotocol);
  let epoch1 = null;
  let serverKey = null;
  let clientKey = null;
  let snapshotFrame = null;
  let snapshotPlain = null;

  try {
    await conn.connect(15000);
    assert('CG-02a', 'WS connects with rpi-collab subprotocol', true,
      `protocol=${conn.protocol}`);
    assert('CG-02b', 'server accepted the capability-hash subprotocol',
      conn.protocol === subprotocol, `got=${conn.protocol}`);

    // Receive hello (TEXT).
    const helloMsg = await conn.nextMessage(15000);
    if (helloMsg.closed) {
      assert('CG-03', 'hello is plaintext TEXT JSON with correct schema', false,
        `connection closed: code=${helloMsg.code} reason=${helloMsg.reason}`);
      writeResults();
      return;
    }
    let hello;
    try {
      const helloText = helloMsg.data.toString('utf8');
      hello = JSON.parse(helloText);
    } catch (e) {
      assert('CG-03', 'hello is plaintext TEXT JSON with correct schema', false, e.message);
      writeResults();
      return;
    }
    assert('CG-03', 'hello is plaintext TEXT JSON with correct schema',
      hello.type === 'hello' && hello.version === 1 &&
      hello.roomId === parsed.roomId &&
      hello.role === parsed.role &&
      typeof hello.epoch === 'string' && hello.epoch.length > 0,
      `type=${hello.type} version=${hello.version} role=${hello.role} epoch=${hello.epoch}`);

    epoch1 = Buffer.from(hello.epoch, 'base64url');
    assert('CG-03b', 'hello epoch is 8 bytes', epoch1.length === EPOCH_LEN,
      `epoch length=${epoch1.length}`);

    // Derive per-connection directional keys.
    clientKey = deriveKey(parsed.key, epoch1, DIR_C2S);
    serverKey = deriveKey(parsed.key, epoch1, DIR_S2C);

    // Receive snapshot (BINARY, seq 0).
    const snapMsg = await conn.nextMessage(15000);
    if (snapMsg.closed) {
      assert('CG-04', 'snapshot frame is binary and decrypts to valid JSON', false,
        `connection closed: ${snapMsg.reason}`);
      writeResults();
      return;
    }
    assert('CG-04a', 'snapshot frame is binary (not text)', snapMsg.isBinary === true);

    snapshotFrame = Buffer.isBuffer(snapMsg.data) ? snapMsg.data : Buffer.from(snapMsg.data);
    // Save raw ciphertext frame for evidence + ciphertext-absence check.
    fs.writeFileSync(path.join(evidenceDir, 'snapshot-raw.bin'), snapshotFrame);

    // Decrypt.
    try {
      const plaintext = openFrame(serverKey, parsed.roomId, DIR_S2C, epoch1, 0, snapshotFrame);
      snapshotPlain = plaintext.toString('utf8');
      fs.writeFileSync(path.join(evidenceDir, 'snapshot-decrypted.json'), snapshotPlain);
      assert('CG-04', 'snapshot frame decrypts to valid JSON', true);
    } catch (e) {
      assert('CG-04', 'snapshot frame decrypts to valid JSON', false, e.message);
      writeResults();
      return;
    }

    // Parse decrypted snapshot.
    let snapshotObj;
    try {
      snapshotObj = JSON.parse(snapshotPlain);
    } catch (e) {
      assert('CG-05', 'decrypted snapshot has {type:"snapshot",snapshot:{sessionId,entries}}',
        false, e.message);
      writeResults();
      return;
    }
    assert('CG-05', 'decrypted snapshot has {type:"snapshot",snapshot:{sessionId,entries}}',
      snapshotObj.type === 'snapshot' &&
      snapshotObj.snapshot &&
      typeof snapshotObj.snapshot.sessionId === 'string' &&
      Array.isArray(snapshotObj.snapshot.entries),
      `type=${snapshotObj.type} sessionId=${snapshotObj.snapshot?.sessionId} entries=${snapshotObj.snapshot?.entries?.length}`);

    // Check expected plaintext in decrypted snapshot (COLLAB_EXPECT only).
    const snapshotText = snapshotPlain;
    for (const text of expectedTexts) {
      const found = snapshotText.includes(text);
      assert(`CG-06-${text}`, `decrypted snapshot contains expected marker`,
        found, `found=${found}`);
    }

    // Check ciphertext absence of known snapshot plaintext (binary-safe).
    for (const text of expectedTexts) {
      const present = bufHasStr(snapshotFrame, text);
      assert(`CG-07-${text}`, `raw snapshot ciphertext does NOT contain expected marker`,
        !present, `present=${present}`);
    }

    // Role-key absence: booleans only — never echo keys.
    const encodedKeyPresent = bufHasStr(snapshotFrame, parsed.encoded);
    const rawKeyPresent = bufHasBuf(snapshotFrame, parsed.key);
    assert('CG-14a', 'raw ciphertext does NOT contain role key bytes',
      !encodedKeyPresent, `present=${encodedKeyPresent}`);
    assert('CG-14b', 'raw ciphertext does NOT contain role key raw bytes',
      !rawKeyPresent, `present=${rawKeyPresent}`);

    // Host path absence: booleans only — never echo the path.
    if (hostPath) {
      const hostPathPresent = snapshotText.includes(hostPath);
      assert('CG-08', 'decrypted snapshot does NOT contain the host workspace path',
        !hostPathPresent, `present=${hostPathPresent}`);
    } else {
      assert('CG-08', 'decrypted snapshot does NOT contain the host workspace path', true);
    }
    const snapshotPrivacyMarkers = [
      ['s', 'k-collabprivacy1234567890'].join(''),
      ['/', 'tmp/collab-private/workspace'].join(''),
    ];
    for (const marker of snapshotPrivacyMarkers) {
      assert(`CG-08b-${sha256(Buffer.from(marker)).subarray(0, 4).toString('hex')}`,
        'decrypted snapshot excludes raw privacy fixture secret/path',
        !snapshotText.includes(marker), `present=${snapshotText.includes(marker)}`);
    }

  } catch (e) {
    assert('CG-CONNECT', 'connection and snapshot phase completed', false, e.message);
    writeResults();
    return;
  }

  // Hold the initial connection open until the orchestrator has observed all
  // three participants. This makes the control prompt a deliberate live event
  // for the browser rather than a race with its initial snapshot.
  if (!connectedMarker || !startMarker) {
    assert('CG-PHASE', 'connection/start phase markers are configured', false,
      `connectedMarker=${Boolean(connectedMarker)} startMarker=${Boolean(startMarker)}`);
    conn.close();
    writeResults();
    return;
  }
  fs.writeFileSync(connectedMarker, 'connected\n');
  const phaseDeadline = Date.now() + phaseTimeout;
  while (!fs.existsSync(startMarker) && Date.now() < phaseDeadline) {
    if (conn.closeInfo) break;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  const phaseReleased = fs.existsSync(startMarker) && !conn.closeInfo;
  assert('CG-PHASE', 'role-command phase starts only after orchestrator release',
    phaseReleased, `released=${phaseReleased} closed=${Boolean(conn.closeInfo)}`);
  if (!phaseReleased) {
    conn.close();
    writeResults();
    return;
  }

  // ── Phase 2: Role-specific commands ────────────────────────────────────

  let s2cExpected = 1; // after snapshot (seq 0), next s2c is seq 1
  let c2sSeq = 0;

  // Decrypt one s2c frame with strict sequential seq. Out-of-order frames fail hard.
  const openNextS2c = (frame) => {
    const plain = openFrame(serverKey, parsed.roomId, DIR_S2C, epoch1, s2cExpected, frame);
    s2cExpected++;
    return plain;
  };

  if (role === 'control') {
    // Send prompt.
    const promptCmd = JSON.stringify({ type: 'command', command: 'prompt', id: 'cg-prompt', message: promptText });
    const promptFrame = sealFrame(clientKey, parsed.roomId, DIR_C2S, epoch1, c2sSeq, Buffer.from(promptCmd, 'utf8'));
    conn.send(promptFrame);

    // Collect events + response. Keep raw encrypted frames separate from decrypted events.
    let eventCount = 0;
    let eventTexts = [];
    const rawEventFrames = [];
    const eventDeadline = Date.now() + eventTimeout;
    let gotPromptResponse = false;
    let seqViolation = false;

    while (Date.now() < eventDeadline) {
      let msg;
      try {
        msg = await conn.nextMessage(5000);
      } catch {
        break;
      }
      if (msg.closed) {
        assert('CG-09', 'control prompt accepted (response success=true)', false,
          `connection closed: ${msg.reason}`);
        break;
      }
      if (!msg.isBinary) continue;

      const frameBuf = Buffer.isBuffer(msg.data) ? msg.data : Buffer.from(msg.data);
      let plain;
      try {
        plain = openNextS2c(frameBuf);
      } catch (e) {
        // Strict sequence/replay: do not accept out-of-order frames.
        seqViolation = true;
        assert('CG-SEQ', 's2c frames decrypt in strict sequence order', false,
          `seq=${s2cExpected} err=${e.message}`);
        break;
      }

      const text = plain.toString('utf8');
      let obj;
      try { obj = JSON.parse(text); } catch { continue; }

      if (obj.type === 'response' && obj.command === 'prompt') {
        gotPromptResponse = true;
        assert('CG-09', 'control prompt accepted (response success=true)',
          obj.success === true, `success=${obj.success} error=${obj.error || ''}`);
      } else if (obj.type === 'event') {
        eventCount++;
        eventTexts.push(text);
        rawEventFrames.push(frameBuf);
        fs.appendFileSync(path.join(evidenceDir, 'events.jsonl'), text + '\n');
        fs.appendFileSync(path.join(evidenceDir, 'events-raw.bin'), frameBuf);
      }

      if (gotPromptResponse) break;
    }

    if (!gotPromptResponse && !seqViolation) {
      assert('CG-09', 'control prompt accepted (response success=true)', false, 'no response received');
    }

    // Send abort.
    const abortCmd = JSON.stringify({ type: 'command', command: 'abort', id: 'cg-abort' });
    const abortFrame = sealFrame(clientKey, parsed.roomId, DIR_C2S, epoch1, c2sSeq + 1, Buffer.from(abortCmd, 'utf8'));
    conn.send(abortFrame);
    c2sSeq += 2;

    let gotAbortResponse = false;
    const abortDeadline = Date.now() + 10000;
    while (Date.now() < abortDeadline && !seqViolation) {
      let msg;
      try { msg = await conn.nextMessage(5000); } catch { break; }
      if (msg.closed) break;
      if (!msg.isBinary) continue;

      const frameBuf = Buffer.isBuffer(msg.data) ? msg.data : Buffer.from(msg.data);
      let plain;
      try {
        plain = openNextS2c(frameBuf);
      } catch (e) {
        seqViolation = true;
        assert('CG-SEQ', 's2c frames decrypt in strict sequence order', false,
          `seq=${s2cExpected} err=${e.message}`);
        break;
      }

      let obj;
      try { obj = JSON.parse(plain.toString('utf8')); } catch { continue; }

      if (obj.type === 'response' && obj.command === 'abort') {
        gotAbortResponse = true;
        assert('CG-10', 'control abort accepted (response success=true)',
          obj.success === true, `success=${obj.success} error=${obj.error || ''}`);
        break;
      } else if (obj.type === 'event') {
        eventCount++;
        eventTexts.push(plain.toString('utf8'));
        rawEventFrames.push(frameBuf);
        fs.appendFileSync(path.join(evidenceDir, 'events.jsonl'), plain.toString('utf8') + '\n');
        fs.appendFileSync(path.join(evidenceDir, 'events-raw.bin'), frameBuf);
      }
    }

    if (!gotAbortResponse && !seqViolation) {
      assert('CG-10', 'control abort accepted (response success=true)', false, 'no abort response');
    }

    // Assert we received at least one live event.
    assert('CG-11', 'live events received and decrypted after prompt',
      eventCount > 0, `eventCount=${eventCount}`);

    // CG-12: live events use COLLAB_EVENT_EXPECT only (not snapshot COLLAB_EXPECT).
    const allEventText = eventTexts.join('\n');
    if (eventExpectedTexts.length > 0) {
      for (const text of eventExpectedTexts) {
        const found = allEventText.includes(text);
        assert(`CG-12-${text}`, 'live event stream contains expected live marker',
          found, `found=${found} eventCount=${eventCount}`);
      }
    } else {
      assert('CG-12', 'live event stream contains expected content',
        eventCount > 0, `eventCount=${eventCount}`);
    }

    // CG-07e: expected live plaintext in decrypted events.jsonl, absent from raw encrypted frames.
    // Never treat decrypted events.jsonl as raw ciphertext evidence.
    // Live markers come only from COLLAB_EVENT_EXPECT (not snapshot COLLAB_EXPECT).
    const eventEvidencePath = path.join(evidenceDir, 'events.jsonl');
    const decryptedEvents = fs.existsSync(eventEvidencePath)
      ? fs.readFileSync(eventEvidencePath, 'utf8')
      : allEventText;
    const rawEventConcat = rawEventFrames.length > 0
      ? Buffer.concat(rawEventFrames)
      : (fs.existsSync(path.join(evidenceDir, 'events-raw.bin'))
        ? fs.readFileSync(path.join(evidenceDir, 'events-raw.bin'))
        : Buffer.alloc(0));

    if (eventExpectedTexts.length > 0) {
      for (const text of eventExpectedTexts) {
        const inDecrypted = decryptedEvents.includes(text);
        const inRaw = bufHasStr(rawEventConcat, text);
        assert(`CG-07e-${text}`, 'live marker in decrypted events and absent from raw encrypted frames',
          inDecrypted && !inRaw, `inDecrypted=${inDecrypted} inRaw=${inRaw}`);
      }
    } else {
      assert('CG-07e', 'raw encrypted event frames captured separately from decrypted events',
        rawEventConcat.length > 0 || eventCount === 0,
        `rawFrameBytes=${rawEventConcat.length} eventCount=${eventCount}`);
    }
    const livePrivacyMarkers = [
      ['s', 'k-collablive1234567890'].join(''),
      ['/', 'tmp/collab-live/private.txt'].join(''),
    ];
    for (const marker of livePrivacyMarkers) {
      assert(`CG-12p-${sha256(Buffer.from(marker)).subarray(0, 4).toString('hex')}`,
        'decrypted live events exclude raw privacy fixture secret/path',
        !allEventText.includes(marker), `present=${allEventText.includes(marker)}`);
    }

  } else {
    // View guest: attempt prompt, expect rejection.
    const promptCmd = JSON.stringify({ type: 'command', command: 'prompt', id: 'vg-prompt', message: promptText });
    const promptFrame = sealFrame(clientKey, parsed.roomId, DIR_C2S, epoch1, c2sSeq, Buffer.from(promptCmd, 'utf8'));
    conn.send(promptFrame);
    c2sSeq++;

    let gotResponse = false;
    let eventCount = 0;
    let eventTexts = [];
    const rawEventFrames = [];
    let seqViolation = false;
    const viewDeadline = Date.now() + eventTimeout;

    while (Date.now() < viewDeadline) {
      let msg;
      try { msg = await conn.nextMessage(10000); } catch { break; }
      if (msg.closed) {
        assert('CG-10v', 'view prompt rejected (success=false, error mentions view-only)', false,
          `connection closed: ${msg.reason}`);
        break;
      }
      if (!msg.isBinary) continue;

      const frameBuf = Buffer.isBuffer(msg.data) ? msg.data : Buffer.from(msg.data);
      let plain;
      try {
        plain = openNextS2c(frameBuf);
      } catch (e) {
        seqViolation = true;
        assert('CG-SEQ', 's2c frames decrypt in strict sequence order', false,
          `seq=${s2cExpected} err=${e.message}`);
        break;
      }

      let obj;
      try { obj = JSON.parse(plain.toString('utf8')); } catch { continue; }

      if (obj.type === 'response' && obj.command === 'prompt') {
        gotResponse = true;
        assert('CG-10v', 'view prompt rejected (success=false, error mentions view-only)',
          obj.success === false && (obj.error || '').includes('view-only'),
          `success=${obj.success} error=${obj.error || ''}`);
        break;
      } else if (obj.type === 'event') {
        eventCount++;
        const text = plain.toString('utf8');
        eventTexts.push(text);
        rawEventFrames.push(frameBuf);
        fs.appendFileSync(path.join(evidenceDir, 'events-view.jsonl'), text + '\n');
        fs.appendFileSync(path.join(evidenceDir, 'events-view-raw.bin'), frameBuf);
      }
    }

    if (!gotResponse && !seqViolation) {
      assert('CG-10v', 'view prompt rejected (success=false, error mentions view-only)',
        false, 'no rejection response received');
    }

    // Capture events from control guest's prompt (may arrive after our rejection).
    const captureDeadline = Date.now() + eventTimeout;
    while (Date.now() < captureDeadline && !seqViolation) {
      let msg;
      try { msg = await conn.nextMessage(3000); } catch { break; }
      if (msg.closed) break;
      if (!msg.isBinary) continue;

      const frameBuf = Buffer.isBuffer(msg.data) ? msg.data : Buffer.from(msg.data);
      let plain;
      try {
        plain = openNextS2c(frameBuf);
      } catch (e) {
        seqViolation = true;
        assert('CG-SEQ', 's2c frames decrypt in strict sequence order', false,
          `seq=${s2cExpected} err=${e.message}`);
        break;
      }

      let obj;
      try { obj = JSON.parse(plain.toString('utf8')); } catch { continue; }

      if (obj.type === 'event') {
        eventCount++;
        const text = plain.toString('utf8');
        eventTexts.push(text);
        rawEventFrames.push(frameBuf);
        fs.appendFileSync(path.join(evidenceDir, 'events-view.jsonl'), text + '\n');
        fs.appendFileSync(path.join(evidenceDir, 'events-view-raw.bin'), frameBuf);
      }
    }

    assert('CG-11v', 'view guest receives live events from control prompt',
      eventCount > 0, `eventCount=${eventCount}`);

    // CG-12v uses COLLAB_EVENT_EXPECT (live markers), not snapshot COLLAB_EXPECT.
    if (eventExpectedTexts.length > 0) {
      const allEventText = eventTexts.join('\n');
      for (const text of eventExpectedTexts) {
        const found = allEventText.includes(text);
        assert(`CG-12v-${text}`, 'view guest live events contain expected live marker',
          found, `found=${found} eventCount=${eventCount}`);
      }
    }
  }

  // ── Phase 3: Disconnect/rejoin with fresh epoch ─────────────────────────

  conn.close();
  await new Promise((r) => setTimeout(r, 500));

  let epoch2 = null;
  try {
    const conn2 = new GuestConnection(parsed.wsUrl, subprotocol);
    await conn2.connect(15000);
    const hello2 = await conn2.nextMessage(15000);
    if (!hello2.closed) {
      const hello2Obj = JSON.parse(hello2.data.toString('utf8'));
      epoch2 = Buffer.from(hello2Obj.epoch, 'base64url');
      assert('CG-13', 'disconnect and rejoin yields fresh epoch',
        epoch2.length === EPOCH_LEN && !epoch2.equals(epoch1),
        `epoch1=${epoch1.toString('hex')} epoch2=${epoch2.toString('hex')}`);

      // Receive and decrypt rejoin snapshot.
      const snap2 = await conn2.nextMessage(15000);
      if (!snap2.closed && snap2.isBinary) {
        const serverKey2 = deriveKey(parsed.key, epoch2, DIR_S2C);
        try {
          const plain2 = openFrame(serverKey2, parsed.roomId, DIR_S2C, epoch2, 0, snap2.data);
          const snap2Obj = JSON.parse(plain2.toString('utf8'));
          assert('CG-13b', 'rejoin snapshot decrypts with fresh-epoch key',
            snap2Obj.type === 'snapshot' && Array.isArray(snap2Obj.snapshot?.entries),
            `type=${snap2Obj.type}`);
        } catch (e) {
          assert('CG-13b', 'rejoin snapshot decrypts with fresh-epoch key', false, e.message);
        }
      } else {
        assert('CG-13b', 'rejoin snapshot decrypts with fresh-epoch key', false, 'no snapshot after rejoin');
      }
    } else {
      assert('CG-13', 'disconnect and rejoin yields fresh epoch', false, 'rejoin hello not received');
    }

    // ── Phase 4: Host-only lifecycle (invalid command → connection closed) ─
    if (epoch2) {
      const clientKey2 = deriveKey(parsed.key, epoch2, DIR_C2S);
      const serverKey2 = deriveKey(parsed.key, epoch2, DIR_S2C);
      const invalidCmd = JSON.stringify({ type: 'command', command: 'collab_stop', id: 'cg-invalid' });
      const invalidFrame = sealFrame(clientKey2, parsed.roomId, DIR_C2S, epoch2, 0, Buffer.from(invalidCmd, 'utf8'));
      conn2.send(invalidFrame);

      // The server should close the connection (invalid command → close).
      let gotClose = false;
      const closeDeadline = Date.now() + 10000;
      while (Date.now() < closeDeadline) {
        let msg;
        try { msg = await conn2.nextMessage(5000); } catch { break; }
        if (msg.closed) {
          gotClose = true;
          break;
        }
      }
      assert('CG-15', 'host-only lifecycle: invalid command (collab_stop) closes connection',
        gotClose, `connection not closed after invalid command`);

      try { conn2.close(); } catch {}
    }

    // ── Phase 5: Wait for host stop close ──────────────────────────────
    if (waitStop) {
      const conn3 = new GuestConnection(parsed.wsUrl, subprotocol);
      try {
        await conn3.connect(15000);
        const hello3 = await conn3.nextMessage(15000);
        if (!hello3.closed) {
          const epoch3 = Buffer.from(JSON.parse(hello3.data.toString('utf8')).epoch, 'base64url');
          // Consume snapshot.
          await conn3.nextMessage(15000);
          fs.writeFileSync(path.join(evidenceDir, 'stop-ready'), 'ready\n');
          // Wait for close (from collab_stop).
          let stopped = false;
          const stopDeadline = Date.now() + stopTimeout;
          while (Date.now() < stopDeadline) {
            let msg;
            try { msg = await conn3.nextMessage(5000); } catch { break; }
            if (msg.closed) {
              stopped = true;
              assert('CG-16', 'host stop closes guest connection (close frame received)',
                true, `code=${msg.code} reason=${msg.reason}`);
              break;
            }
          }
          if (!stopped) {
            assert('CG-16', 'host stop closes guest connection (close frame received)',
              false, 'no close within timeout');
          }
        }
      } catch (e) {
        assert('CG-16', 'host stop closes guest connection (close frame received)',
          false, e.message);
      }
      try { conn3.close(); } catch {}
    }
  } catch (e) {
    assert('CG-13', 'disconnect and rejoin yields fresh epoch', false, e.message);
  }

  writeResults();
}

function writeResults() {
  try {
    fs.mkdirSync(evidenceDir, { recursive: true });
  } catch {
    // best-effort; write may still succeed if dir already exists
  }
  const outPath = path.join(evidenceDir, 'guest-results.json');
  let payload;
  try {
    payload = JSON.stringify(results, null, 2);
  } catch {
    payload = JSON.stringify([{
      id: 'CG-FATAL',
      description: 'guest results serializable',
      passed: false,
      details: 'results serialization failed',
    }], null, 2);
  }
  try {
    fs.writeFileSync(outPath, payload);
  } catch (e) {
    console.error(`[collab-guest] failed to write guest-results.json: ${e.message}`);
  }
  const allPassed = Array.isArray(results) && results.length > 0 && results.every(r => r.passed);
  const passedCount = Array.isArray(results) ? results.filter(r => r.passed).length : 0;
  const totalCount = Array.isArray(results) ? results.length : 0;
  console.log(`[collab-guest] ${passedCount}/${totalCount} assertions passed`);
  if (!allPassed) {
    process.exitCode = 2;
  }
}

runGuest().catch((err) => {
  try {
    console.error(`[collab-guest] crashed: ${err && err.stack ? err.stack : err}`);
  } catch { /* ignore */ }
  try {
    assert('CG-FATAL', 'guest did not crash', false, err && err.message ? err.message : 'unknown error');
  } catch { /* ignore */ }
  try {
    writeResults();
  } catch (e) {
    try {
      fs.mkdirSync(evidenceDir, { recursive: true });
      fs.writeFileSync(
        path.join(evidenceDir, 'guest-results.json'),
        JSON.stringify([{
          id: 'CG-FATAL',
          description: 'guest did not crash',
          passed: false,
          details: e && e.message ? e.message : 'writeResults failed',
        }], null, 2),
      );
    } catch { /* last resort */ }
  }
  process.exit(2);
});