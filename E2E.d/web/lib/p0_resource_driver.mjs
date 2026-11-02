// Standalone P0 session-acceptance driver.
// Injectable transport, clock, fs, and RSS helpers. Fail-closed JSON evidence.
// Does not import playwright or the shared user_mock_server.

import {
  captureResourceSnapshot,
  disconnectReleaseMeasures,
  listenerEstablishedConnectionCount,
  normalizeEvidencePath,
  sampleRssSlope,
  processTreeRssBytes,
  processTreeRssSnapshot,
  tcpConnectionCount,
  writeEvidenceJson,
} from './p0_resource_metrics.mjs';
import {
  DEFAULT_NATIVE_MARKER,
  DEFAULT_NATIVE_SESSION_ID,
  EXACT_NATIVE_SESSION_BYTES,
  loadNativeSession,
  nativeSessionPath,
  writeExactNativeSession,
  writeExactSelectedPressureSession,
} from './p0_native_session.mjs';

export const P0_RESOURCE_SCHEMA = 'p0-session-acceptance.v1';
// The one loaded-pool cap constant in the test corpus. Must equal
// MAX_LOADED_SESSIONS in crates/pi-cli/src/modes/session_runtime_manager.rs
// (128): fill past the cap retires exactly one idle LRU runtime, a busy
// runtime is never the victim, and an all-busy pool fails closed.
export const MAX_LOADED_RUNTIMES = 128;
export const SWITCH_HANDOFF_RESPONSE_MODE = 'subscribe-handoff-v1';
export const LIFECYCLE_RESPONSE_VERSION = 1;
export {
  DEFAULT_NATIVE_MARKER,
  DEFAULT_NATIVE_SESSION_ID,
  EXACT_NATIVE_SESSION_BYTES,
  nativeSessionPath,
};

const defaultNow = () => Date.now();
const defaultDelay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
export const DUAL_ISOLATION_WINDOW_MS = 250;
export const RSS_LIVE_SAMPLE_DEFAULTS = Object.freeze({
  intervalMs: 1000,
  minSamples: 5,
  maxSamples: 20,
});

// Bounded post-disconnect release window. Samples every 250ms for up to 120
// samples (30s worst case) and requires a strictly negative RSS delta vs the
// connected / H1 baseline that holds for 8 consecutive samples (2s) with no
// sample growing back above the baseline in between.
export const RSS_RELEASE_DEFAULTS = Object.freeze({
  intervalMs: 250,
  maxSamples: 120,
  confirmSamples: 8,
});
export const PRESSURE_MIN_BYTES = 1024 * 1024;
// Bounded connection-pressure installation window. Samples every 250ms for
// up to 120 samples (30s worst case) until a single sample shows both the
// RSS rise (rssBytes >= H0 + PRESSURE_MIN_BYTES) and the connection-owned
// signal +1 after the paused subscription exists. The signal is the fd
// count (fdSocketCount > fd0) when the fd tables are readable, else the
// listener-local-port established count (listenerEstablished >= conn0 + 1)
// when a listenerPort is supplied. Neither signal available = fail-closed.
export const PRESSURE_SAMPLE_DEFAULTS = Object.freeze({
  intervalMs: 250,
  maxSamples: 120,
});

export class P0ContractError extends Error {
  constructor(id, message) {
    super(`${id}: ${message}`);
    this.name = 'P0ContractError';
    this.contractId = id;
  }
}

export function runtimeIdentityFrom(sessionId, runtimeIncarnation) {
  if (typeof sessionId !== 'string' || sessionId === '') return null;
  if (typeof runtimeIncarnation !== 'string' || runtimeIncarnation === '') return null;
  return { sessionId, runtimeIncarnation };
}

export function parseHandoff(data, expectedSessionId = '') {
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    throw new P0ContractError('handoff', 'handoff must be an object');
  }
  if ('state' in data || 'messages' in data) {
    throw new P0ContractError('handoff', 'handoff must not carry state or messages');
  }
  const identity = runtimeIdentityFrom(data.sessionId, data.runtimeIncarnation);
  if (!identity) throw new P0ContractError('handoff', 'handoff RuntimeIdentity missing');
  if (expectedSessionId && identity.sessionId !== expectedSessionId) {
    throw new P0ContractError('handoff', `handoff sessionId ${identity.sessionId} !== ${expectedSessionId}`);
  }
  if (data.subscriptionRequired !== true) {
    throw new P0ContractError('handoff', 'subscriptionRequired must be true');
  }
  if (data.lifecycleResponseVersion !== LIFECYCLE_RESPONSE_VERSION) {
    throw new P0ContractError('handoff', 'lifecycleResponseVersion must be 1');
  }
  if (!Number.isFinite(data.cursor)) {
    throw new P0ContractError('handoff', 'handoff cursor missing');
  }
  return { ...identity, cursor: data.cursor, subscriptionRequired: true, lifecycleResponseVersion: 1 };
}

export function parseLifecycleIdentity(data, expectedSessionId = '') {
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    throw new P0ContractError('lifecycle', 'lifecycle payload must be an object');
  }
  const identity = runtimeIdentityFrom(data.sessionId, data.runtimeIncarnation);
  if (!identity) {
    throw new P0ContractError('lifecycle', 'lifecycle RuntimeIdentity missing');
  }
  if (expectedSessionId && identity.sessionId !== expectedSessionId) {
    throw new P0ContractError('lifecycle', `lifecycle sessionId ${identity.sessionId} !== ${expectedSessionId}`);
  }
  const cursor = Number.isFinite(data.cursor) ? data.cursor : 0;
  return { ...identity, cursor };
}

export function isAuthoritativeCut(frame, identity) {
  if (!frame || typeof frame !== 'object') return false;
  if (frame.type !== 'subscription_snapshot' && frame.type !== 'session_resync') return false;
  if (!identity) return false;
  return frame.sessionId === identity.sessionId
    && frame.runtimeIncarnation === identity.runtimeIncarnation;
}

export function classifyClose(code, {
  phase = '',
  locallyInitiated = false,
} = {}) {
  const numeric = Number(code);
  const recorded = {
    code: Number.isFinite(numeric) ? numeric : null,
    phase,
    locallyInitiated: locallyInitiated === true,
  };
  if (recorded.code === 1006) {
    return { ...recorded, unexpected: true, kind: 'abnormal-1006' };
  }
  if (recorded.code === 1000) {
    return { ...recorded, unexpected: locallyInitiated !== true, kind: 'normal' };
  }
  if (recorded.code === null) {
    return { ...recorded, unexpected: true, kind: 'unknown' };
  }
  return { ...recorded, unexpected: true, kind: 'non-1000' };
}

export function frameHasExactIdentity(frame, identity) {
  if (!frame || !identity) return false;
  if (typeof frame.sessionId !== 'string' || frame.sessionId === '') return false;
  if (typeof frame.runtimeIncarnation !== 'string' || frame.runtimeIncarnation === '') return false;
  return frame.sessionId === identity.sessionId
    && frame.runtimeIncarnation === identity.runtimeIncarnation;
}

export function payloadContainsMarker(payload, marker) {
  if (typeof marker !== 'string' || marker === '') return false;
  if (payload == null) return false;
  if (typeof payload === 'string') return payload.includes(marker);
  if (typeof payload !== 'object') return false;
  try {
    return JSON.stringify(payload).includes(marker);
  } catch {
    return false;
  }
}

export function nativeSessionEvidence(written, live = null) {
  if (!written) return live;
  return {
    bytes: written.bytes,
    sessionId: written.sessionId,
    marker: written.marker,
    timestamp: written.timestamp,
    header: written.header
      ? { type: written.header.type, version: written.header.version, id: written.header.id }
      : null,
    live,
  };
}

function attachSocket(ws, { onOpen, onMessage, onClose, onError }) {
  if (typeof ws.on === 'function') {
    if (onOpen) ws.on('open', onOpen);
    if (onMessage) ws.on('message', (raw) => onMessage(String(raw)));
    if (onClose) {
      ws.on('close', (code, reason) => {
        onClose(code, reason == null ? '' : String(reason));
      });
    }
    if (onError) ws.on('error', onError);
    return;
  }
  if (onOpen) ws.addEventListener('open', onOpen);
  if (onMessage) ws.addEventListener('message', (ev) => onMessage(String(ev.data)));
  if (onClose) {
    ws.addEventListener('close', (ev) => onClose(ev.code, ev.reason == null ? '' : String(ev.reason)));
  }
  if (onError) ws.addEventListener('error', onError);
}

let nextControlClientNs = 0;

export function createControlClient({
  url,
  token = '',
  WebSocketImpl,
  now = defaultNow,
  callTimeoutMs = 15000,
} = {}) {
  if (!WebSocketImpl) throw new P0ContractError('transport', 'WebSocketImpl is required');
  if (!url) throw new P0ContractError('transport', 'url is required');
  const protocols = token ? [`rpi-auth.${token}`] : [];
  const ws = protocols.length ? new WebSocketImpl(url, protocols) : new WebSocketImpl(url);
  const pending = new Map();
  const frames = [];
  const closes = [];
  const unexpectedCloses = [];
  const frameWaiters = new Set();
  const closeWaiters = new Set();
  let phase = 'connect';
  const clientNs = ++nextControlClientNs;
  let seq = 0;
  let opened = false;
  let locallyInitiatedClose = false;

  const classifyCurrentClose = (code) => classifyClose(code, {
    phase,
    locallyInitiated: locallyInitiatedClose,
  });

  const ready = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new P0ContractError('transport', 'websocket open timed out')), callTimeoutMs);
    attachSocket(ws, {
      onOpen() {
        opened = true;
        clearTimeout(timer);
        resolve();
      },
      onMessage(text) {
        let frame;
        try {
          frame = JSON.parse(text);
        } catch {
          return;
        }
        const entry = { tMs: now(), frame };
        frames.push(entry);
        if (frame && frame.type === 'response' && frame.id && pending.has(frame.id)) {
          const waiter = pending.get(frame.id);
          pending.delete(frame.id);
          waiter.resolve(frame);
        }
        for (const waiter of [...frameWaiters]) {
          if (waiter.predicate(frame, entry)) {
            frameWaiters.delete(waiter);
            waiter.resolve(frame);
          }
        }
      },
      onClose(code, reason) {
        const rec = {
          code,
          reason: String(reason || ''),
          tMs: now(),
          phase,
          locallyInitiated: locallyInitiatedClose,
        };
        closes.push(rec);
        const classified = classifyCurrentClose(code);
        const expected = locallyInitiatedClose
          && classified.code === 1000
          && classified.unexpected === false;
        rec.expected = expected;
        rec.kind = classified.kind;
        rec.unexpected = classified.unexpected;
        if (rec.unexpected) unexpectedCloses.push({ ...rec });
        for (const waiter of pending.values()) {
          waiter.reject(new P0ContractError('transport', `websocket closed (${code})`));
        }
        pending.clear();
        for (const waiter of closeWaiters) waiter.resolve(rec);
        closeWaiters.clear();
        for (const waiter of frameWaiters) {
          waiter.reject(new P0ContractError('transport', `websocket closed (${code})`));
        }
        frameWaiters.clear();
      },
      onError(err) {
        if (!opened) reject(err instanceof Error ? err : new P0ContractError('transport', String(err)));
      },
    });
  });

  const waitForClose = (timeoutMs = callTimeoutMs) => {
    if (closes.length) return Promise.resolve(closes[closes.length - 1]);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        closeWaiters.delete(waiter);
        reject(new P0ContractError('close', 'websocket close timed out'));
      }, timeoutMs);
      const waiter = {
        resolve(rec) {
          clearTimeout(timer);
          resolve(rec);
        },
      };
      closeWaiters.add(waiter);
    });
  };

  return {
    ws,
    ready,
    frames,
    closes,
    unexpectedCloses,
    setPhase(next) {
      phase = next;
    },
    phase() {
      return phase;
    },
    locallyInitiatedClose() {
      return locallyInitiatedClose;
    },
    async call(command) {
      await ready;
      const id = command.id || `p0-${clientNs}-${++seq}`;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          if (pending.delete(id)) reject(new P0ContractError('rpc', `timed out: ${command.type}`));
        }, callTimeoutMs);
        pending.set(id, {
          resolve: (frame) => {
            clearTimeout(timer);
            resolve(frame);
          },
          reject: (err) => {
            clearTimeout(timer);
            reject(err);
          },
        });
        ws.send(JSON.stringify({ ...command, id }));
      });
    },
    async send(obj) {
      const result = ws.send(JSON.stringify(obj));
      if (result && typeof result.then === 'function') await result;
    },
    waitFor(predicate, label, timeoutMs = callTimeoutMs, options = {}) {
      const afterIndex = Number.isInteger(options.afterIndex) ? options.afterIndex : 0;
      const afterMs = Number.isFinite(options.afterMs) ? options.afterMs : null;
      const matches = (frame, entry, index = frames.length - 1) => {
        if (index < afterIndex) return false;
        if (afterMs !== null && !(entry.tMs > afterMs)) return false;
        return predicate(frame, entry, index);
      };
      const existingIndex = frames.findIndex((entry, index) => matches(entry.frame, entry, index));
      if (existingIndex >= 0) return Promise.resolve(frames[existingIndex].frame);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          frameWaiters.delete(waiter);
          reject(new P0ContractError('wait', `${label} timed out`));
        }, timeoutMs);
        const waiter = {
          predicate: matches,
          resolve(frame) {
            clearTimeout(timer);
            resolve(frame);
          },
          reject(err) {
            clearTimeout(timer);
            reject(err);
          },
        };
        frameWaiters.add(waiter);
      });
    },
    waitForClose,
    async close(code = 1000, reason = '') {
      // Reserved codes (2-999, 1001-2999, 5000+, non-integers) are refused
      // before any state changes: a close that can never produce a clean
      // handshake must fail fast instead of hanging waitForClose.
      if (!Number.isInteger(code) || (code !== 1000 && (code < 3000 || code > 4999))) {
        throw new P0ContractError('close', `refusing reserved close code ${code}`);
      }
      if (new TextEncoder().encode(String(reason)).length > 123) {
        throw new P0ContractError('close', 'close reason exceeds the 123-byte limit');
      }
      locallyInitiatedClose = true;
      const closed = waitForClose();
      try {
        if (typeof ws.close === 'function') {
          const closing = ws.close(code, reason);
          if (closing && typeof closing.then === 'function') {
            await Promise.race([closed, closing.catch(() => {})]);
          }
        }
      } catch {
        /* already closed or page evaluate failed; waitForClose still fail-closes */
      }
      return closed;
    },
  };
}

let pageBridgeSeq = 0;

export function createPageWebSocketImpl(page) {
  if (!page || typeof page.evaluate !== 'function') {
    throw new P0ContractError('transport', 'page.evaluate is required for in-page WebSocket');
  }
  return class PageWebSocket {
    constructor(url, protocols = []) {
      this.url = url;
      this.protocols = protocols;
      // Every instance gets unique binding names and a unique in-page socket
      // slot. A page app, a second client, or a rebinding can therefore never
      // hijack this socket's callbacks or target its close/send at another
      // socket — the previously shared window.__p0ControlSocket and __p0*
      // bindings routed the second client's events into the first client and
      // left the first socket to die abnormally (1006) at page teardown.
      this.bridgeId = `p0Bridge${pageBridgeSeq += 1}`;
      this.handlers = { open: [], message: [], close: [], error: [] };
      this._closed = false;
      this._bound = this._bind();
      this._bound.catch((err) => {
        for (const handler of this.handlers.error) handler(err);
      });
    }
    on(event, handler) {
      if (this.handlers[event]) this.handlers[event].push(handler);
    }
    async _bind() {
      const expose = async (suffix, deliver) => {
        try {
          await page.exposeFunction(`${this.bridgeId}${suffix}`, deliver);
        } catch {
          /* binding already registered: only possible for this same instance
             rebinding after a navigation, and the existing binding still
             targets this instance's handlers */
        }
      };
      await expose(':OnOpen', () => {
        for (const handler of this.handlers.open) handler();
      });
      await expose(':OnMessage', (text) => {
        for (const handler of this.handlers.message) handler(text);
      });
      await expose(':OnClose', (code, reason) => {
        for (const handler of this.handlers.close) handler(code, reason == null ? '' : String(reason));
      });
      await expose(':OnError', (message) => {
        for (const handler of this.handlers.error) handler(new Error(String(message)));
      });
      await page.evaluate(({ id, url, protocols }) => {
        window.__p0Bridges = window.__p0Bridges || new Map();
        const socket = new WebSocket(url, protocols);
        window.__p0Bridges.set(id, socket);
        socket.addEventListener('open', () => window[id + ':OnOpen']());
        socket.addEventListener('message', (ev) => window[id + ':OnMessage'](String(ev.data)));
        socket.addEventListener('close', (ev) => window[id + ':OnClose'](ev.code, ev.reason == null ? '' : String(ev.reason)));
        socket.addEventListener('error', () => window[id + ':OnError']('page websocket error'));
      }, { id: this.bridgeId, url: this.url, protocols: this.protocols });
    }
    async send(data) {
      await this._bound;
      return page.evaluate(({ id, text }) => {
        const registry = window.__p0Bridges;
        const socket = registry ? registry.get(id) : null;
        if (!socket || socket.readyState !== WebSocket.OPEN) {
          throw new Error('page websocket is not open');
        }
        socket.send(text);
      }, { id: this.bridgeId, text: String(data) });
    }
    async close(code = 1000, reason = '') {
      if (this._closed) return this._closePromise;
      // Reserved close codes are refused locally instead of being emitted and
      // letting the in-page WebSocket API produce an opaque failure.
      if (!Number.isInteger(code) || (code !== 1000 && (code < 3000 || code > 4999))) {
        return Promise.reject(new P0ContractError('close', `refusing reserved close code ${code}`));
      }
      if (new TextEncoder().encode(String(reason)).length > 123) {
        return Promise.reject(new P0ContractError('close', 'close reason exceeds the 123-byte limit'));
      }
      this._closed = true;
      this._closePromise = (async () => {
        await this._bound;
        return page.evaluate(({ id, code, reason }) => new Promise((resolve) => {
          const registry = window.__p0Bridges;
          const socket = registry ? registry.get(id) : null;
          if (!socket || socket.readyState === WebSocket.CLOSED) {
            resolve({ bridged: false, code: null, reason: '' });
            return;
          }
          // Resolve only on the socket's actual close event so a following
          // page teardown can never race ahead of the clean close handshake.
          socket.addEventListener('close', (ev) => resolve({
            bridged: true,
            code: ev.code,
            reason: ev.reason == null ? '' : String(ev.reason),
          }));
          if (socket.readyState === WebSocket.CONNECTING) {
            // socket.close() during CONNECTING fails the connection (1006);
            // wait for the handshake to finish, then close cleanly.
            socket.addEventListener('open', () => socket.close(code, reason));
            socket.addEventListener('error', () => resolve({ bridged: false, code: null, reason: '' }));
            return;
          }
          if (socket.readyState === WebSocket.OPEN) socket.close(code, reason);
        }), { id: this.bridgeId, code, reason });
      })();
      return this._closePromise;
    }
  };
}

export function requireCleanLocalClose(rec, label = 'control') {
  if (!rec || rec.code !== 1000 || rec.locallyInitiated !== true || rec.unexpected === true) {
    throw new P0ContractError('close', `${label} expected locally initiated 1000, got ${JSON.stringify(rec)}`);
  }
  return rec;
}

export async function closeBrowserClients({ pages = [], page = null } = {}) {
  const targets = Array.isArray(pages) && pages.length ? pages : (page ? [page] : []);
  const errors = [];
  for (const target of targets) {
    if (!target) continue;
    if (typeof target.evaluate === 'function') {
      await target.evaluate(() => {
        const registry = window.__p0Bridges;
        if (!registry) return;
        for (const socket of registry.values()) {
          // Only OPEN sockets get close(): close() during CONNECTING fails
          // the connection (1006) instead of completing the handshake.
          if (socket && socket.readyState === WebSocket.OPEN) socket.close(1000, 'p0-disconnect');
        }
      }).catch(() => {});
    }
    let context = null;
    if (typeof target.context === 'function') {
      try { context = target.context(); } catch { context = null; }
    }
    if (typeof target.close === 'function') {
      try {
        await Promise.resolve(target.close());
      } catch (err) {
        errors.push(err);
      }
    }
    if (context && typeof context.close === 'function') {
      try {
        await Promise.resolve(context.close());
      } catch (err) {
        errors.push(err);
      }
    }
  }
  if (errors.length) throw errors[0];
}

export async function disconnectControlThenBrowser({
  clientA = null,
  clientB = null,
  pages = null,
  page = null,
  disconnectBrowserClients,
} = {}) {
  const control = [];
  if (clientA) {
    clientA.setPhase('disconnect');
    control.push(clientA.close(1000, 'p0-disconnect').then((rec) => ({ label: 'clientA', rec })));
  }
  if (clientB) {
    clientB.setPhase('disconnect');
    control.push(clientB.close(1000, 'p0-disconnect').then((rec) => ({ label: 'clientB', rec })));
  }
  const records = await Promise.all(control);
  for (const { label, rec } of records) requireCleanLocalClose(rec, label);
  const closer = typeof disconnectBrowserClients === 'function'
    ? disconnectBrowserClients
    : closeBrowserClients;
  await closer({ pages, page, clientA, clientB });
  return records.map(({ rec }) => rec);
}

// Raw Node WebSocket pressure client for the connection-pressure contract
// (shared by p0_resource_test.mjs and p0_release_probe.mjs). WebSocketImpl
// is required — the driver stays transport-agnostic and never imports 'ws'
// itself. Opens a plain WebSocket with the rpi-auth.<token> subprotocol,
// exposes ready / send / pause / destroy primitives, and leaves the
// underlying socket paused (no reads) after pause() so the paused-unread
// subscription holds connection-owned pressure. destroy() terminates the
// socket causally and awaits the close event.
export function createNodePressureClient({ url, token = '', WebSocketImpl }) {
  if (typeof WebSocketImpl !== 'function') {
    throw new P0ContractError('pressure', 'createNodePressureClient requires a WebSocketImpl');
  }
  const protocols = token ? [`rpi-auth.${token}`] : [];
  const ws = new WebSocketImpl(url, protocols);
  const ready = new Promise((resolve, reject) => {
    ws.once('open', resolve);
    ws.once('error', reject);
  });
  let closed = false;
  const closedPromise = new Promise((resolve) => {
    ws.once('close', (code, reason) => resolve({ code, reason: String(reason || '') }));
  });
  return {
    ready,
    async send(frame) {
      await ready;
      ws.send(JSON.stringify(frame));
    },
    async pause() {
      await ready;
      const socket = ws._socket;
      if (socket && typeof socket.pause === 'function') socket.pause();
    },
    async destroy() {
      if (closed) return closedPromise;
      closed = true;
      try {
        ws.terminate();
      } catch {
        /* already closed */
      }
      return closedPromise;
    },
  };
}

export async function requireRpcData(frame, label) {
  if (!frame || frame.type !== 'response') {
    throw new P0ContractError(label, `expected response, got ${JSON.stringify(frame && frame.type)}`);
  }
  if (frame.success !== true) {
    throw new P0ContractError(label, frame.error || 'rpc failed');
  }
  return frame.data || {};
}

const omittedBootstrapByClient = new WeakMap();

export async function bootstrapPrimary(client, { sessionId } = {}) {
  if (!sessionId && client && omittedBootstrapByClient.has(client)) {
    return omittedBootstrapByClient.get(client);
  }
  const command = { type: 'get_state' };
  if (sessionId) command.sessionId = sessionId;
  const frame = await client.call(command);
  const data = await requireRpcData(frame, 'bootstrap');
  const identity = runtimeIdentityFrom(data.sessionId, data.runtimeIncarnation)
    || runtimeIdentityFrom(frame.sessionId, data.runtimeIncarnation);
  if (!identity) {
    throw new P0ContractError('bootstrap', `get_state omitted RuntimeIdentity: ${JSON.stringify({ data, sessionId: frame.sessionId })}`);
  }
  const result = { identity, data, frame };
  if (!sessionId && client) omittedBootstrapByClient.set(client, result);
  return result;
}

export async function subscribeAuthoritative(client, identity, cursor, timeoutMs) {
  if (!identity) throw new P0ContractError('subscribe', 'RuntimeIdentity required');
  const afterIndex = Array.isArray(client.frames) ? client.frames.length : 0;
  await client.send({
    type: 'subscribe',
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    cursor,
  });
  return client.waitFor(
    (frame) => isAuthoritativeCut(frame, identity),
    `authoritative cut for ${identity.sessionId}`,
    timeoutMs,
    { afterIndex },
  );
}

export async function measureAuthoritativeInteractiveLatency({
  action,
  identity,
  cursor,
  client,
  now = defaultNow,
  timeoutMs = 15000,
} = {}) {
  if (typeof action !== 'function') {
    throw new P0ContractError('latency', 'action() is required');
  }
  const startedMs = now();
  const actionResult = await action();
  const target = identity
    || runtimeIdentityFrom(actionResult && actionResult.sessionId, actionResult && actionResult.runtimeIncarnation);
  const subCursor = cursor ?? (actionResult && actionResult.cursor);
  if (!target || !Number.isFinite(subCursor)) {
    throw new P0ContractError('latency', 'action did not yield RuntimeIdentity+cursor for subscribe');
  }
  const cut = await subscribeAuthoritative(client, target, subCursor, timeoutMs);
  const authoritativeMs = now();
  const latencyMs = authoritativeMs - startedMs;
  if (!Number.isFinite(latencyMs) || latencyMs < 0) {
    throw new P0ContractError('latency', `non-finite latency ${latencyMs}`);
  }
  return {
    startedMs,
    authoritativeMs,
    latencyMs,
    identity: target,
    cursor: subCursor,
    cutType: cut.type,
    reason: cut.reason || null,
  };
}

export function loadedIdsFromList(data) {
  const rows = Array.isArray(data && data.sessions) ? data.sessions : [];
  return rows
    .filter((row) => row && row.loaded === true && typeof row.sessionId === 'string')
    .map((row) => ({
      sessionId: row.sessionId,
      path: row.path,
      runtimeIncarnation: row.runtimeIncarnation || null,
    }));
}

// Fill the loaded pool to the production cap by opening fresh sessions from
// the primary. Fails closed the moment a server reports more loaded runtimes
// than the cap. Shared by the LRU-reopen contract and the busy-protection
// contract.
async function fillLoadedPool(client, { maxLoaded, timeoutMs }) {
  const boot = await bootstrapPrimary(client);
  const primary = boot.identity.sessionId;
  const created = [primary];
  const listLoaded = async () => {
    const frame = await client.call({ type: 'session_list', scope: 'all_projects', sessionId: primary });
    const data = await requireRpcData(frame, 'session_list');
    return { data, loaded: loadedIdsFromList(data) };
  };
  let listed = await listLoaded();
  while (listed.loaded.length < maxLoaded) {
    const frame = await client.call({ type: 'new_session', sessionId: created[created.length - 1] });
    const identity = parseLifecycleIdentity(await requireRpcData(frame, 'new_session'));
    created.push(identity.sessionId);
    await subscribeAuthoritative(client, identity, identity.cursor, timeoutMs);
    listed = await listLoaded();
    if (listed.loaded.length > maxLoaded) {
      throw new P0ContractError('pool', `loaded count ${listed.loaded.length} exceeded cap ${maxLoaded}`);
    }
  }
  if (listed.loaded.length !== maxLoaded) {
    throw new P0ContractError('pool', `loaded pool did not reach ${maxLoaded}: ${JSON.stringify(listed.loaded)}`);
  }
  return { primary, created, listLoaded };
}

// Stamp last_used via get_state (admit marks recency) in the exact given
// order so a following pool create must retire the expected idle LRU victim;
// FIFO/random/MRU orders cannot reproduce this sequence.
async function stampRecency(client, orderedIds) {
  for (const sid of orderedIds) {
    await requireRpcData(await client.call({ type: 'get_state', sessionId: sid }), `get_state:${sid}`);
  }
}

export async function fillEvictReopenPool(client, {
  maxLoaded = MAX_LOADED_RUNTIMES,
  timeoutMs = 15000,
} = {}) {
  client.setPhase('pool');
  const { primary, created, listLoaded } = await fillLoadedPool(client, { maxLoaded, timeoutMs });
  let listed = await listLoaded();
  const loadedAtCap = listed.loaded.map((row) => row.sessionId);
  if (!loadedAtCap.includes(primary)) {
    throw new P0ContractError('pool', `primary missing from loaded pool: ${JSON.stringify(loadedAtCap)}`);
  }

  const idle = loadedAtCap.filter((id) => id !== primary);
  if (idle.length < 2) {
    throw new P0ContractError('pool', `need >=2 idle non-primaries, got ${JSON.stringify(idle)}`);
  }
  const expectedOldest = idle[0];
  const recentIdle = idle[idle.length - 1];
  const recencyOrder = [
    expectedOldest,
    ...idle.filter((id) => id !== expectedOldest && id !== recentIdle),
    recentIdle,
    primary,
  ];
  await stampRecency(client, recencyOrder);

  const evicting = await client.call({ type: 'new_session', sessionId: primary });
  const replacement = parseLifecycleIdentity(await requireRpcData(evicting, 'new_session:evict'));
  await subscribeAuthoritative(client, replacement, replacement.cursor, timeoutMs);

  listed = await listLoaded();
  const loadedAfter = listed.loaded.map((row) => row.sessionId);
  if (loadedAfter.length !== maxLoaded) {
    throw new P0ContractError('evict', `loaded count after eviction ${loadedAfter.length} !== ${maxLoaded}`);
  }
  if (!loadedAfter.includes(replacement.sessionId) || !loadedAfter.includes(primary) || !loadedAfter.includes(recentIdle)) {
    throw new P0ContractError('evict', `LRU replacement pool mismatch: ${JSON.stringify({ loadedAfter, replacement: replacement.sessionId, primary, recentIdle })}`);
  }
  const retired = loadedAtCap.find((id) => !loadedAfter.includes(id));
  if (retired !== expectedOldest) {
    throw new P0ContractError('evict', `retired ${retired} !== expected idle LRU ${expectedOldest}`);
  }
  const retiredRow = (listed.data.sessions || []).find((row) => row && row.sessionId === retired);
  if (!retiredRow || retiredRow.loaded === true) {
    throw new P0ContractError('evict', `retired runtime lost durable catalog row: ${JSON.stringify(retiredRow)}`);
  }
  if (!retiredRow.path) {
    throw new P0ContractError('reopen', 'retired catalog row has no path');
  }

  const switched = await client.call({
    type: 'switch_session',
    sessionPath: retiredRow.path,
    sessionId: replacement.sessionId,
    responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
  });
  const handoff = parseHandoff(await requireRpcData(switched, 'switch_session:reopen'), retired);
  await subscribeAuthoritative(client, handoff, handoff.cursor, timeoutMs);
  listed = await listLoaded();
  if (listed.loaded.length !== maxLoaded) {
    throw new P0ContractError('reopen', `loaded count after reopen ${listed.loaded.length} !== ${maxLoaded}`);
  }
  const loadedAfterReopen = listed.loaded.map((row) => row.sessionId);
  const reopened = listed.loaded.find((row) => row.sessionId === retired);
  if (!reopened) {
    throw new P0ContractError('reopen', `retired session ${retired} did not re-enter the loaded pool`);
  }
  const replacedOnReopen = loadedAfter.filter((id) => !loadedAfterReopen.includes(id));
  if (replacedOnReopen.length !== 1) {
    throw new P0ContractError('reopen', `expected exactly one replacement, got ${JSON.stringify(replacedOnReopen)}`);
  }
  if (replacedOnReopen[0] === retired) {
    throw new P0ContractError('reopen', 'reopened session cannot be the replaced runtime');
  }

  return {
    primary,
    created,
    loadedAtCap,
    expectedOldest,
    recentIdle,
    retired,
    retiredPath: retiredRow.path,
    replacementId: replacement.sessionId,
    replacedOnReopen: replacedOnReopen[0],
    reopenedIdentity: handoff,
    loadedAfterReopen,
    loadedCountAfterReopen: listed.loaded.length,
  };
}

// Busy-runtime eviction protection, the remaining half of the production
// loaded-pool contract (acquire_loaded_slot): a pool create at the cap never
// retires a busy runtime — the next least-recently-used idle runtime is the
// victim instead — and when every non-primary runtime is busy the create
// FAILS with the bounded "too many busy sessions" error. A server that
// retires the busy runtime or accepts an all-busy create fails closed here.
// Requires the backend to surface an in-flight `prompt` as busy until an
// `abort` settles it.
export async function assertBusyProtectedEviction(client, {
  maxLoaded = MAX_LOADED_RUNTIMES,
  timeoutMs = 15000,
} = {}) {
  client.setPhase('pool');
  const { primary, listLoaded } = await fillLoadedPool(client, { maxLoaded, timeoutMs });
  const listed = await listLoaded();
  const loadedAtCap = listed.loaded.map((row) => row.sessionId);
  if (!loadedAtCap.includes(primary)) {
    throw new P0ContractError('busy', `primary missing from loaded pool: ${JSON.stringify(loadedAtCap)}`);
  }
  const idle = loadedAtCap.filter((id) => id !== primary);
  if (idle.length < 2) {
    throw new P0ContractError('busy', `need >=2 idle non-primaries, got ${JSON.stringify(idle)}`);
  }
  // Stamp recency so the busy runtime starts as the exact LRU victim a
  // busy-blind server would retire.
  const busyTarget = idle[0];
  const expectedVictim = idle[1];
  await stampRecency(client, [busyTarget, ...idle.slice(2), expectedVictim, primary]);

  await requireRpcData(
    await client.call({ type: 'prompt', message: 'p0-busy-pin', sessionId: busyTarget }),
    'prompt:busy-pin',
  );
  const created = await client.call({ type: 'new_session', sessionId: primary });
  const replacement = parseLifecycleIdentity(await requireRpcData(created, 'new_session:busy-evict'));
  await subscribeAuthoritative(client, replacement, replacement.cursor, timeoutMs);

  const afterCreate = await listLoaded();
  const loadedAfter = afterCreate.loaded.map((row) => row.sessionId);
  if (loadedAfter.length !== maxLoaded) {
    throw new P0ContractError('busy', `loaded count after busy-protected create ${loadedAfter.length} !== ${maxLoaded}`);
  }
  const retired = loadedAtCap.find((id) => !loadedAfter.includes(id));
  if (!retired || retired === busyTarget) {
    throw new P0ContractError('busy', `busy runtime ${busyTarget} was retired (victim ${retired})`);
  }
  if (retired !== expectedVictim) {
    throw new P0ContractError('busy', `retired ${retired} !== expected non-busy idle LRU ${expectedVictim}`);
  }
  const busyRow = (afterCreate.data.sessions || []).find((row) => row && row.sessionId === busyTarget);
  if (!busyRow || busyRow.loaded !== true) {
    throw new P0ContractError('busy', `busy runtime ${busyTarget} lost its loaded row after the create`);
  }

  const releaseBusy = async (ids, label) => {
    for (const sid of ids) {
      await requireRpcData(await client.call({ type: 'abort', sessionId: sid }), `${label}:${sid}`);
    }
  };
  await requireRpcData(await client.call({ type: 'abort', sessionId: busyTarget }), 'abort:busy-pin');
  const busyIds = (await listLoaded()).loaded.map((row) => row.sessionId).filter((id) => id !== primary);
  for (const sid of busyIds) {
    await requireRpcData(await client.call({ type: 'prompt', message: 'p0-busy-pin', sessionId: sid }), `prompt:all-busy:${sid}`);
  }
  let accepted = false;
  try {
    await requireRpcData(await client.call({ type: 'new_session', sessionId: primary }), 'new_session:all-busy');
    accepted = true;
  } catch (error) {
    if (!(error instanceof P0ContractError) || !/too many busy sessions/.test(error.message)) throw error;
  }
  if (accepted) {
    throw new P0ContractError('busy', 'server accepted a pool create while every non-primary runtime was busy (must fail closed)');
  }
  await releaseBusy(busyIds, 'abort:all-busy');
  return { primary, busyTarget, expectedVictim, retired, replacementId: replacement.sessionId };
}
export async function generateAndLoadExactSession(options = {}) {
  const written = await writeExactNativeSession(options);
  const loaded = await loadNativeSession(written.path, {
    expectedBytes: options.targetBytes || EXACT_NATIVE_SESSION_BYTES,
    expectedId: written.sessionId,
    expectedCwd: written.cwd,
    marker: written.marker,
    fsp: options.fsp,
  });
  if (loaded.bytes !== (options.targetBytes || EXACT_NATIVE_SESSION_BYTES)) {
    throw new P0ContractError('native-session', `loaded size ${loaded.bytes} mismatch`);
  }
  if (written.marker && loaded.selectedUserText !== written.marker) {
    throw new P0ContractError('native-session', `selected leaf user text !== marker ${written.marker}`);
  }
  if (written.marker && loaded.markerOnSelectedBranch !== true) {
    throw new P0ContractError('native-session', `marker ${written.marker} missing from selected branch`);
  }
  return {
    ...written,
    header: loaded.header,
    selectedLeafId: loaded.selectedLeafId,
    selectedBranchIds: loaded.selectedBranchIds,
    selectedMessageCount: loaded.selectedMessageCount,
    selectedContextBytes: loaded.selectedContextBytes,
    selectedUserText: loaded.selectedUserText,
    markerOnSelectedBranch: loaded.markerOnSelectedBranch,
  };
}

export async function openExactNativeSessionLive(client, written, {
  timeoutMs = 15000,
} = {}) {
  if (!client) throw new P0ContractError('native-session', 'live client is required');
  if (!written || !written.path || !written.sessionId) {
    throw new P0ContractError('native-session', 'generated native session is required');
  }
  client.setPhase('native-session');
  const boot = await bootstrapPrimary(client);
  const switched = await client.call({
    type: 'switch_session',
    sessionPath: written.path,
    sessionId: boot.identity.sessionId,
    responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
  });
  const handoff = parseHandoff(
    await requireRpcData(switched, 'switch_session:native'),
    written.sessionId,
  );
  const identity = {
    sessionId: handoff.sessionId,
    runtimeIncarnation: handoff.runtimeIncarnation,
  };
  const cut = await subscribeAuthoritative(client, identity, 0, timeoutMs);
  if (!frameHasExactIdentity(cut, identity)) {
    throw new P0ContractError('native-session', `authoritative cut identity mismatch: ${JSON.stringify({
      sessionId: cut.sessionId || null,
      runtimeIncarnation: cut.runtimeIncarnation || null,
    })}`);
  }
  if (cut.reason !== 'cold-full') {
    throw new P0ContractError('native-session', `authoritative cut reason ${cut.reason} !== cold-full`);
  }
  if (!Array.isArray(cut.messages)) {
    throw new P0ContractError('native-session', 'authoritative cut omitted messages');
  }
  if (!payloadContainsMarker(cut.messages, written.marker) && !payloadContainsMarker(cut, written.marker)) {
    throw new P0ContractError('native-session', `authoritative cut omitted marker ${written.marker}`);
  }
  return {
    identity,
    cursor: 0,
    handoffCursor: handoff.cursor,
    cutType: cut.type,
    reason: cut.reason,
    markerObserved: true,
    bytes: written.bytes,
    sessionId: written.sessionId,
    marker: written.marker,
    selectedLeafId: written.selectedLeafId || null,
    markerOnSelectedBranch: written.markerOnSelectedBranch === true,
  };
}

export function ownerStreamFrame(frame, owner, needle) {
  return frameHasExactIdentity(frame, owner) && payloadContainsMarker(frame, needle);
}

export async function assertNoOwnerStreamToPeer(client, owner, needle, {
  sinceIndex = 0,
  windowMs = DUAL_ISOLATION_WINDOW_MS,
} = {}) {
  if (!client) throw new P0ContractError('dual-client', 'peer client is required');
  const leaked = (frame) => ownerStreamFrame(frame, owner, needle);
  if (client.frames.slice(sinceIndex).some((entry) => leaked(entry.frame))) {
    throw new P0ContractError('dual-client', 'client B observed owner A stream');
  }
  try {
    await client.waitFor(leaked, 'peer isolation leak', windowMs, { afterIndex: sinceIndex });
  } catch (err) {
    if (err instanceof P0ContractError && err.contractId === 'wait') {
      return { isolated: true, windowMs };
    }
    throw err;
  }
  throw new P0ContractError('dual-client', 'client B observed owner A stream after A marker');
}

export async function runDualClientBackgroundStream({
  clientA,
  clientB,
  prompt = 'p0-slow-a',
  replyNeedle = 'slow-a-done',
  now = defaultNow,
  timeoutMs = 15000,
  isolationWindowMs = DUAL_ISOLATION_WINDOW_MS,
  runtimeA = null,
  runtimeB = null,
  pool = null,
} = {}) {
  if (!clientA || !clientB) throw new P0ContractError('dual-client', 'two clients are required');
  clientA.setPhase('dual-client');
  clientB.setPhase('dual-client');
  await clientA.ready;
  await clientB.ready;

  const listLoaded = async (client, sessionId) => {
    const frame = await client.call({
      type: 'session_list',
      scope: 'all_projects',
      sessionId,
    });
    const data = await requireRpcData(frame, 'session_list:dual');
    return loadedIdsFromList(data);
  };


  let owner;
  if (runtimeA) {
    owner = runtimeIdentityFrom(runtimeA.sessionId, runtimeA.runtimeIncarnation);
    if (!owner) throw new P0ContractError('dual-client', 'runtimeA omitted RuntimeIdentity');
  } else {
    const bootA = await bootstrapPrimary(clientA);
    owner = bootA.identity;
  }
  await subscribeAuthoritative(clientA, owner, 0, timeoutMs);

  let peer;
  if (runtimeB) {
    peer = runtimeIdentityFrom(runtimeB.sessionId, runtimeB.runtimeIncarnation);
    if (!peer) throw new P0ContractError('dual-client', 'runtimeB omitted RuntimeIdentity');
    if (runtimeB.path) {
      const switched = await clientB.call({
        type: 'switch_session',
        sessionPath: runtimeB.path,
        sessionId: owner.sessionId,
        responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
      });
      peer = parseHandoff(await requireRpcData(switched, 'switch_session:dual-b'), peer.sessionId);
    }
  } else {
    const listed = await listLoaded(clientB, owner.sessionId);
    const other = listed.find((row) => row.sessionId !== owner.sessionId && row.runtimeIncarnation);
    if (other) {
      const switched = await clientB.call({
        type: 'switch_session',
        sessionPath: other.path,
        sessionId: owner.sessionId,
        responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
      });
      peer = parseHandoff(await requireRpcData(switched, 'switch_session:dual-b'), other.sessionId);
    } else if (pool && pool.reopenedIdentity && pool.reopenedIdentity.sessionId !== owner.sessionId) {
      peer = pool.reopenedIdentity;
      if (pool.retiredPath) {
        const switched = await clientB.call({
          type: 'switch_session',
          sessionPath: pool.retiredPath,
          sessionId: owner.sessionId,
          responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
        });
        peer = parseHandoff(await requireRpcData(switched, 'switch_session:dual-b'), peer.sessionId);
      }
    } else {
      const created = await clientB.call({ type: 'new_session', sessionId: owner.sessionId });
      peer = parseLifecycleIdentity(await requireRpcData(created, 'new_session:dual-b'));
    }
  }
  if (!peer || !peer.sessionId || !peer.runtimeIncarnation) {
    throw new P0ContractError('dual-client', 'client B did not acquire a distinct runtime');
  }
  if (peer.sessionId === owner.sessionId && peer.runtimeIncarnation === owner.runtimeIncarnation) {
    throw new P0ContractError('dual-client', `A/B identities are not distinct: ${JSON.stringify({ owner, peer })}`);
  }
  await subscribeAuthoritative(clientB, peer, Number.isFinite(peer.cursor) ? peer.cursor : 0, timeoutMs);

  const startedMs = now();
  const bSeenBefore = clientB.frames.length;
  clientA.setPhase('background-stream');
  const promptFrame = await clientA.call({
    type: 'prompt',
    message: prompt,
    sessionId: owner.sessionId,
  });
  await requireRpcData(promptFrame, 'prompt:background');

  const streamFrame = await clientA.waitFor((frame) => {
    return ownerStreamFrame(frame, owner, replyNeedle);
  }, `background stream ${replyNeedle}`, timeoutMs);

  const isolation = await assertNoOwnerStreamToPeer(clientB, owner, replyNeedle, {
    sinceIndex: bSeenBefore,
    windowMs: isolationWindowMs,
  });

  return {
    clientAIdentity: owner,
    clientBIdentity: { sessionId: peer.sessionId, runtimeIncarnation: peer.runtimeIncarnation },
    distinctRuntimes: true,
    ownerIdentity: owner,
    streamObserved: true,
    streamNotDeliveredToB: true,
    isolationWindowMs: isolation.windowMs,
    backgroundPromptMs: now() - startedMs,
    streamType: streamFrame.type || null,
  };
}



export async function sampleRssUntilPlateau({
  pid,
  fsp,
  now = defaultNow,
  delay = defaultDelay,
  intervalMs = RSS_LIVE_SAMPLE_DEFAULTS.intervalMs,
  minSamples = RSS_LIVE_SAMPLE_DEFAULTS.minSamples,
  maxSamples = RSS_LIVE_SAMPLE_DEFAULTS.maxSamples,
  plateauToleranceBytes,
  plateauWindowSamples,
  maxDescendants,
} = {}) {
  if (!Number.isFinite(Number(pid))) {
    throw new P0ContractError('rss', 'numeric pid required');
  }
  const minCount = Math.max(2, Math.floor(Number(minSamples)) || RSS_LIVE_SAMPLE_DEFAULTS.minSamples);
  const maxCount = Math.max(minCount, Math.floor(Number(maxSamples)) || RSS_LIVE_SAMPLE_DEFAULTS.maxSamples);
  const plateauOptions = { plateauToleranceBytes, plateauWindowSamples };
  const captured = [];
  let summary = null;
  for (let index = 0; index < maxCount; index += 1) {
    if (index > 0) await delay(intervalMs);
    captured.push({
      tMs: now(),
      rssBytes: await processTreeRssBytes(pid, { fsp, maxDescendants }),
    });
    if (captured.length < minCount) continue;
    summary = await sampleRssSlope(captured, plateauOptions);
    if (summary.plateau && summary.plateau.detected === true) {
      return {
        sourcePid: Number(pid),
        ...summary,
        stoppedEarly: captured.length < maxCount,
        sampleCountTaken: captured.length,
      };
    }
  }
  summary = summary || await sampleRssSlope(captured, plateauOptions);
  return {
    sourcePid: Number(pid),
    ...summary,
    stoppedEarly: false,
    sampleCountTaken: captured.length,
  };
}
function pressureSubscribePaused(pressureClient, identity, cursor) {
  if (!pressureClient) {
    throw new P0ContractError('pressure', 'pressureClient.subscribePaused or pause/destroy primitives required');
  }
  if (typeof pressureClient.subscribePaused === 'function') {
    return pressureClient.subscribePaused(identity, cursor);
  }
  const canPause = typeof pressureClient.pause === 'function';
  const canDestroy = typeof pressureClient.destroy === 'function'
    || typeof pressureClient.close === 'function';
  const canSend = typeof pressureClient.subscribe === 'function'
    || typeof pressureClient.send === 'function'
    || typeof pressureClient.call === 'function';
  if (!canPause || !canDestroy || !canSend) {
    throw new P0ContractError('pressure', 'pressureClient.subscribePaused or pause/destroy primitives required');
  }
  return Promise.resolve(pressureClient.pause()).then(() => {
    if (typeof pressureClient.subscribe === 'function') {
      return pressureClient.subscribe(identity, cursor);
    }
    const frame = {
      type: 'subscribe',
      sessionId: identity.sessionId,
      runtimeIncarnation: identity.runtimeIncarnation,
      cursor,
    };
    if (typeof pressureClient.send === 'function') return pressureClient.send(frame);
    return pressureClient.call(frame);
  });
}

async function destroyPressureClient(pressureClient) {
  if (!pressureClient) return;
  if (typeof pressureClient.destroy === 'function') {
    await pressureClient.destroy();
    return;
  }
  if (typeof pressureClient.close === 'function') await pressureClient.close();
}


export async function sampleRssPlateauAndRelease({
  pid,
  page = null,
  postDisconnectPage,
  fsp,
  now,
  delay,
  disconnect,
  samples,
  intervalMs = RSS_LIVE_SAMPLE_DEFAULTS.intervalMs,
  minSamples = RSS_LIVE_SAMPLE_DEFAULTS.minSamples,
  maxSamples = RSS_LIVE_SAMPLE_DEFAULTS.maxSamples,
  release: releaseOptions = {},
  pressure: pressureOptions = null,
} = {}) {
  if (!Number.isFinite(Number(pid))) {
    throw new P0ContractError('rss', 'numeric pid required');
  }
  const fixedCount = Number.isFinite(Number(samples)) ? Math.floor(Number(samples)) : null;
  const connected = await captureResourceSnapshot({ page, pid, fsp, now });
  const slope = await sampleRssUntilPlateau({
    pid,
    fsp,
    now,
    delay,
    intervalMs,
    minSamples: fixedCount === null ? minSamples : fixedCount,
    maxSamples: fixedCount === null ? maxSamples : fixedCount,
  });
  if (!slope.plateau || slope.plateau.detected !== true) {
    throw new P0ContractError('rss', `plateau.detected !== true: ${JSON.stringify(slope.plateau)}`);
  }
  let installed = null;
  if (pressureOptions && pressureOptions.pressureClient) {
    installed = await installConnectionOwnedPressure({
      pid,
      fsp,
      now,
      delay,
      maxDescendants: releaseOptions.maxDescendants,
      ...pressureOptions,
    });
    await destroyPressureClient(installed.pressureClient);
  }
  // Causal ordering: without installed pressure the disconnect itself is the
  // causal cut and precedes the window; with installed pressure the destroy
  // of the pressure socket above is the only change before the window and
  // the unrelated disconnect is deferred until after the verdict (below),
  // so it can never cause or contribute to the release.
  if (installed === null && typeof disconnect === 'function') await disconnect();
  const disconnectedPage = postDisconnectPage !== undefined
    ? postDisconnectPage
    : (typeof disconnect === 'function' ? null : page);
  // Release verdict comes from the bounded sampling window (strictly
  // negative delta vs the connected baseline, no growth back inside the
  // confirmation window); the one-shot snapshot that follows is
  // evidence-only (browser metrics on the post-disconnect page) and never
  // decides release. When connection pressure was installed, the baseline
  // is H1 and the drop must cover the installed magnitude with an fd drop
  // and unchanged processCount.
  let released;
  try {
    released = await sampleRssUntilRelease({
      pid,
      baselineBytes: installed ? installed.h1 : connected.rssBytes,
      minReleaseBytes: installed ? installed.installedBytes : releaseOptions.minReleaseBytes,
      baselineFdSocketCount: installed
        ? (installed.signalMode === 'fd' ? installed.fd1 : undefined)
        : releaseOptions.baselineFdSocketCount,
      listenerPort: installed && installed.signalMode === 'listener' ? installed.listenerPort : undefined,
      baselineListenerEstablishedCount: installed && installed.signalMode === 'listener'
        ? installed.conn1
        : undefined,
      baselineProcessCount: installed ? installed.processCount : releaseOptions.baselineProcessCount,
      fsp,
      now,
      delay,
      intervalMs: releaseOptions.intervalMs,
      maxSamples: releaseOptions.maxSamples,
      confirmSamples: releaseOptions.confirmSamples,
      maxDescendants: releaseOptions.maxDescendants,
    });
  } catch (error) {
    if (error instanceof P0ContractError && error.rssEvidence) {
      error.rssEvidence.plateau = slope.plateau;
      error.rssEvidence.slopeBytesPerMs = slope.slopeBytesPerMs;
      if (installed) error.rssEvidence.pressure = {
        h0: installed.h0,
        h1: installed.h1,
        installedBytes: installed.installedBytes,
        fd0: installed.fd0,
        fd1: installed.fd1,
        signalMode: installed.signalMode,
        listenerPort: installed.listenerPort,
        conn0: installed.conn0,
        conn1: installed.conn1,
      };
    }
    throw error;
  }
  // With installed pressure the unrelated disconnect runs only now — after
  // the release verdict — so it can never cause or contribute to it.
  if (installed !== null && typeof disconnect === 'function') await disconnect();
  const disconnected = await captureResourceSnapshot({
    page: disconnectedPage,
    pid,
    fsp,
    now,
  });
  const result = {
    connected,
    disconnected,
    slope,
    plateau: slope.plateau,
    release: {
      ...released.measures,
      baselineBytes: released.baselineBytes,
      minReleaseBytes: released.minReleaseBytes,
      releasedAtSample: released.releasedAtSample,
      releaseElapsedMs: released.releaseElapsedMs,
      sampleCountTaken: released.sampleCountTaken,
      stoppedEarly: released.stoppedEarly,
      samples: released.samples,
    },
    postDisconnectPage: disconnectedPage,
  };
  if (installed) result.pressure = installed;
  return result;
}

// Bounded post-disconnect wait for real RSS release over the process tree.
// Samples {tMs, rssBytes, threadCount, processCount, fdSocketCount,
// listenerEstablished, tcpSockets}
// every intervalMs (default 250ms) for up to maxSamples samples (default 120 =
// 30s). Success requires a strictly negative RSS delta vs the connected / H1
// baseline that holds for confirmSamples consecutive samples (default 8 =
// 2s) with no sample in between growing back above the baseline — a
// transient dip alone is not a release, and delta == 0 is a no-release.
// When minReleaseBytes > 0 the drop magnitude must be >= that floor (the
// installed connection pressure) on every confirming sample. When fd1 /
// baselineFdSocketCount is finite the connection-owned fd count must stay
// strictly below it; when listenerPort + conn1 /
// baselineListenerEstablishedCount are supplied the established count on
// the listener's local port must drop strictly below conn1; when
// processCount1 / baselineProcessCount is finite
// processCount must stay unchanged (fail-closed if missing). H0 is never
// consulted for the verdict. Genuinely unreadable RSS samples are kept as
// rssBytes null and count as neither release nor growth. When the window
// ends without a release the P0ContractError fails closed and carries the
// full sample series on `err.rssEvidence` plus a compact delta summary in
// the message; the RSS tolerance itself is never weakened to pass a 0
// delta. Legacy callers that omit minReleaseBytes / fd / process gates keep
// the original strict-drop behavior (minReleaseBytes defaults to 0).
export async function sampleRssUntilRelease({
  pid,
  baselineBytes,
  h1,
  fsp,
  now = defaultNow,
  delay = defaultDelay,
  intervalMs = RSS_RELEASE_DEFAULTS.intervalMs,
  maxSamples = RSS_RELEASE_DEFAULTS.maxSamples,
  confirmSamples = RSS_RELEASE_DEFAULTS.confirmSamples,
  maxDescendants,
  minReleaseBytes,
  installedBytes,
  baselineFdSocketCount,
  fd1,
  baselineProcessCount,
  processCount1,
  listenerPort,
  baselineListenerEstablishedCount,
  conn1,
} = {}) {
  if (!Number.isFinite(Number(pid))) {
    throw new P0ContractError('rss', 'numeric pid required');
  }
  const baselineSource = baselineBytes !== undefined ? baselineBytes : h1;
  if (!Number.isFinite(baselineSource)) {
    throw new P0ContractError('rss', `connected RSS baseline missing (fail-closed): ${baselineSource}`);
  }
  const baseline = Number(baselineSource);
  const minReleaseRaw = minReleaseBytes !== undefined ? minReleaseBytes : installedBytes;
  const minRelease = Number.isFinite(Number(minReleaseRaw))
    ? Math.max(0, Math.floor(Number(minReleaseRaw)))
    : 0;
  const fdRaw = baselineFdSocketCount !== undefined ? baselineFdSocketCount : fd1;
  const processRaw = baselineProcessCount !== undefined ? baselineProcessCount : processCount1;
  const fdBaseline = Number.isFinite(Number(fdRaw)) ? Number(fdRaw) : null;
  const processBaseline = Number.isFinite(Number(processRaw)) ? Number(processRaw) : null;
  const processGate = processBaseline !== null;
  // Connection-owned fd gate: active only when an fd baseline (fd1) was
  // supplied by the install; absent = no fd gate, listener fallback only.
  const fdGate = fdBaseline !== null;
  // Listener-local-port connection gate: active only when BOTH the port and
  // the installed established-count baseline (conn1) are supplied; a lone
  // half of the pair is a caller error and fails closed.
  const listenerPortRaw = listenerPort !== undefined && listenerPort !== null
    ? Number(listenerPort)
    : null;
  if (listenerPortRaw !== null && (!Number.isInteger(listenerPortRaw) || listenerPortRaw <= 0)) {
    throw new P0ContractError('rss', `listenerPort must be a positive integer port (fail-closed): ${listenerPort}`);
  }
  const listenerBaselineRaw = baselineListenerEstablishedCount !== undefined ? baselineListenerEstablishedCount : conn1;
  const listenerBaseline = Number.isFinite(Number(listenerBaselineRaw))
    ? Number(listenerBaselineRaw)
    : null;
  if (listenerPortRaw !== null && listenerBaseline === null) {
    throw new P0ContractError('rss', `listener connection baseline missing (fail-closed): `
      + `listenerPort=${listenerPortRaw} conn1=${listenerBaselineRaw}`);
  }
  if (listenerBaseline !== null && listenerPortRaw === null) {
    throw new P0ContractError('rss', 'listenerPort missing for listener connection gate (fail-closed)');
  }
  const listenerGate = listenerBaseline !== null && listenerPortRaw !== null;
  const maxCount = Math.max(1, Math.floor(Number(maxSamples)) || RSS_RELEASE_DEFAULTS.maxSamples);
  const confirm = Math.max(1, Math.floor(Number(confirmSamples)) || RSS_RELEASE_DEFAULTS.confirmSamples);
  const step = Math.max(0, Math.floor(Number(intervalMs)) || RSS_RELEASE_DEFAULTS.intervalMs);
  const samples = [];
  let releasedAt = null;
  for (let index = 0; index < maxCount; index += 1) {
    if (index > 0) await delay(step);
    const snapshot = await processTreeRssSnapshot(pid, { fsp, maxDescendants });
    const listenerEstablished = listenerGate
      ? await listenerEstablishedConnectionCount(pid, listenerPortRaw, { fsp })
      : null;
    samples.push({
      tMs: now(),
      rssBytes: snapshot ? snapshot.rssBytes : null,
      threadCount: snapshot ? snapshot.threadCount : null,
      processCount: snapshot ? snapshot.processCount : null,
      fdSocketCount: snapshot ? snapshot.fdSocketCount : null,
      listenerEstablished,
      tcpSockets: await tcpConnectionCount(pid, { fsp }),
    });
    const current = samples[samples.length - 1];
    const rssFinite = Number.isFinite(current.rssBytes);
    const fdFinite = Number.isFinite(current.fdSocketCount);
    const processFinite = Number.isFinite(current.processCount);
    const listenerFinite = Number.isFinite(current.listenerEstablished);
    const drop = rssFinite ? baseline - current.rssBytes : null;
    const grew = rssFinite && current.rssBytes > baseline;
    const rssReleased = rssFinite && current.rssBytes < baseline && drop >= minRelease;
    const fdReleased = !fdGate || (fdFinite && current.fdSocketCount < fdBaseline);
    const listenerReleased = !listenerGate
      || (listenerFinite && current.listenerEstablished < listenerBaseline);
    const processOk = !processGate || (processFinite && current.processCount === processBaseline);
    const holds = rssReleased && fdReleased && listenerReleased && processOk;
    const revokes = grew
      || (fdGate && fdFinite && current.fdSocketCount >= fdBaseline)
      || (listenerGate && listenerFinite && current.listenerEstablished >= listenerBaseline)
      || (processGate && processFinite && current.processCount !== processBaseline)
      || (releasedAt !== null && minRelease > 0 && rssFinite && drop < minRelease)
      || (releasedAt !== null && fdGate && !fdReleased)
      || (releasedAt !== null && listenerGate && !listenerReleased)
      || (releasedAt !== null && processGate && !processOk);
    if (revokes) {
      releasedAt = null;
      continue;
    }
    if (releasedAt === null) {
      if (holds) releasedAt = samples.length - 1;
      continue;
    }
    if (samples.length - releasedAt >= confirm) break;
  }
  if (releasedAt === null) {
    const finite = samples.filter((s) => Number.isFinite(s.rssBytes)).map((s) => s.rssBytes);
    const deltasKiB = samples.map((s) => (Number.isFinite(s.rssBytes)
      ? Math.round((s.rssBytes - baseline) / 1024)
      : null));
    const last = samples.length ? samples[samples.length - 1] : {};
    const lastDrop = Number.isFinite(last.rssBytes) ? baseline - last.rssBytes : null;
    const maxRss = finite.length ? Math.max(...finite) : null;
    const fdNeverDropped = fdGate && samples.every((s) => (
      !Number.isFinite(s.fdSocketCount) || s.fdSocketCount >= fdBaseline
    ));
    const listenerNeverDropped = listenerGate && samples.every((s) => (
      !Number.isFinite(s.listenerEstablished) || s.listenerEstablished >= listenerBaseline
    ));
    const processChanged = processGate && samples.some((s) => (
      Number.isFinite(s.processCount) && s.processCount !== processBaseline
    ));
    let reason = 'no-release';
    if (maxRss !== null && maxRss > baseline) reason = 'regrowth';
    else if (processChanged) reason = 'process-change';
    else if (fdNeverDropped) reason = 'fd-no-drop';
    else if (listenerNeverDropped) reason = 'listener-no-drop';
    else if (lastDrop === 0 || (finite.length > 0 && finite.every((rss) => rss >= baseline))) reason = 'delta0';
    else if (lastDrop !== null && lastDrop < minRelease) reason = 'small-drop';
    const summary = `reason=${reason} baseline=${baseline}B minReleaseBytes=${minRelease}B `
      + `fd1=${fdBaseline} conn1=${listenerBaseline} listenerPort=${listenerGate ? listenerPortRaw : null} `
      + `processCount1=${processBaseline} samples=${samples.length} `
      + `min=${finite.length ? Math.min(...finite) : null}B `
      + `max=${finite.length ? Math.max(...finite) : null}B `
      + `last=${last.rssBytes}B lastFd=${last.fdSocketCount} `
      + `lastConn=${last.listenerEstablished} `
      + `lastProcessCount=${last.processCount} lastDrop=${lastDrop}B `
      + `deltaKiB=[${deltasKiB.join(',')}]`;
    const err = new P0ContractError('rss', `release not observed within window: released=false over `
      + `${samples.length} samples (maxSamples=${maxCount}, intervalMs=${step}, `
      + `confirmSamples=${confirm}); ${summary}`);
    err.rssEvidence = {
      release: {
        released: false,
        reason,
        baselineBytes: baseline,
        h1: baseline,
        minReleaseBytes: minRelease,
        installedBytes: minRelease,
        magnitudeBytes: lastDrop,
        fdSocketCount: last.fdSocketCount ?? null,
        listenerEstablished: last.listenerEstablished ?? null,
        processCount: last.processCount ?? null,
        baselineFdSocketCount: fdBaseline,
        baselineListenerEstablishedCount: listenerBaseline,
        listenerPort: listenerGate ? listenerPortRaw : null,
        conn1: listenerBaseline,
        baselineProcessCount: processBaseline,
        fd1: fdBaseline,
        processCount1: processBaseline,
        sampling: { intervalMs: step, maxSamples: maxCount, confirmSamples: confirm },
        samples,
      },
    };
    throw err;
  }
  const releasedSample = samples[releasedAt];
  const magnitudeBytes = Number.isFinite(releasedSample.rssBytes)
    ? baseline - releasedSample.rssBytes
    : null;
  return {
    sourcePid: Number(pid),
    released: true,
    releasedAtSample: releasedAt,
    sampleCountTaken: samples.length,
    stoppedEarly: samples.length < maxCount,
    baselineBytes: baseline,
    h1: baseline,
    minReleaseBytes: minRelease,
    installedBytes: minRelease,
    magnitudeBytes,
    fdSocketCount: releasedSample.fdSocketCount ?? null,
    baselineFdSocketCount: fdBaseline,
    baselineProcessCount: processBaseline,
    fd1: fdBaseline,
    listenerPort: listenerGate ? listenerPortRaw : null,
    conn1: listenerBaseline,
    baselineListenerEstablishedCount: listenerBaseline,
    listenerEstablished: releasedSample.listenerEstablished ?? null,
    processCount1: processBaseline,
    releaseElapsedMs: samples.length > 0 ? releasedSample.tMs - samples[0].tMs : 0,
    measures: disconnectReleaseMeasures({ rssBytes: baseline, tMs: samples[0].tMs }, releasedSample),
    samples,
  };
}

// Causal destroy of the connection-owned pressure client, then the gated
// release window against H1 sampled while every unrelated client (control
// clients, browser contexts, the listener's other connections) stays
// connected — the caller closes unrelated clients only after the verdict,
// so the release can never be caused by an unrelated disconnect. Caller
// supplies H1, installedBytes (>= 1MiB), processCount1, and the connection
// release signal from the install: fd1 (fd mode) or listenerPort + conn1
// (listener fallback). Neither signal is fail-closed. H0 is not used for
// the verdict. Returns measures / magnitude / fd / listener / process / the
// sample series from sampleRssUntilRelease.
export async function releaseConnectionOwnedPressure({
  pid,
  h1,
  baselineBytes,
  installedBytes,
  minReleaseBytes,
  fd1,
  processCount1,
  listenerPort,
  conn1,
  pressureClient,
  fsp,
  now = defaultNow,
  delay = defaultDelay,
  intervalMs,
  maxSamples,
  confirmSamples,
  maxDescendants,
} = {}) {
  const baseline = Number(baselineBytes !== undefined ? baselineBytes : h1);
  const installed = Number(minReleaseBytes !== undefined ? minReleaseBytes : installedBytes);
  if (!Number.isFinite(baseline)) {
    throw new P0ContractError('rss', `H1 baseline missing (fail-closed): ${baseline}`);
  }
  if (!Number.isFinite(installed) || installed < PRESSURE_MIN_BYTES) {
    throw new P0ContractError('rss', `installedBytes must be >= ${PRESSURE_MIN_BYTES} (got ${installed})`);
  }
  // Connection release signal (fail-closed): fd1 from the fd-mode install,
  // or the listener-local-port pair from the listener fallback. Neither
  // available means the caller lost the install signal and the release must
  // not run.
  const fdGateAvailable = Number.isFinite(Number(fd1));
  const connGateAvailable = Number.isInteger(Number(listenerPort)) && Number(listenerPort) > 0
    && Number.isFinite(Number(conn1));
  if (!fdGateAvailable && !connGateAvailable) {
    throw new P0ContractError('rss', `connection release signal missing (fail-closed): `
      + `fd1=${fd1} listenerPort=${listenerPort} conn1=${conn1}`);
  }
  if (!Number.isFinite(Number(processCount1))) {
    throw new P0ContractError('rss', `processCount1 missing (fail-closed): ${processCount1}`);
  }
  await destroyPressureClient(pressureClient);
  return sampleRssUntilRelease({
    pid,
    baselineBytes: baseline,
    minReleaseBytes: installed,
    fd1: fdGateAvailable ? Number(fd1) : undefined,
    listenerPort: fdGateAvailable ? undefined : Number(listenerPort),
    conn1: fdGateAvailable ? undefined : Number(conn1),
    processCount1: Number(processCount1),
    fsp,
    now,
    delay,
    intervalMs,
    maxSamples,
    confirmSamples,
    maxDescendants,
  });
}

// Bounded wait for connection-owned pressure installation over the process
// tree. Observes H0 plus the connection signal baseline from the process
// tree after the caller has reached plateau, then creates the exact paused
// unread subscription via pressureClient.subscribePaused(identity, cursor)
// (or pause + subscribe/send + destroy/close primitives) and samples
// {tMs, rssBytes, fdSocketCount, listenerEstablished, processCount} every
// intervalMs (default 250ms) for up to maxSamples (default 120 = 30s) until
// a single sample shows BOTH the RSS rise
// (rssBytes >= H0 + PRESSURE_MIN_BYTES) and the connection signal +1:
// fdSocketCount > fd0 when the fd tables are readable, else
// listenerEstablished >= conn0 + 1 on the caller-supplied listenerPort
// (listenerEstablishedConnectionCount fallback). The driver stays
// transport-agnostic: the caller later supplies
// writeExactSelectedPressureSession output as pressureSession. The caller
// owns the returned pressureClient and destroys it later; no release logic
// lives here. When the window ends without the rise, or the fd signal is
// missing AND the listener fallback is unavailable or never increases, the
// P0ContractError fails closed and carries the full sample series on
// `err.pressureEvidence`.
export async function installConnectionOwnedPressure({
  pid,
  pressureClient,
  identity,
  fsp,
  now = defaultNow,
  delay = defaultDelay,
  intervalMs = PRESSURE_SAMPLE_DEFAULTS.intervalMs,
  maxSamples = PRESSURE_SAMPLE_DEFAULTS.maxSamples,
  minBytes = PRESSURE_MIN_BYTES,
  maxDescendants,
  cursor = 0,
  listenerPort = null,
} = {}) {
  if (!identity || !identity.sessionId || !identity.runtimeIncarnation) {
    throw new P0ContractError('pressure', 'RuntimeIdentity required');
  }
  if (!Number.isFinite(Number(pid))) {
    throw new P0ContractError('pressure', 'numeric pid required');
  }
  const riseBytes = Math.max(1, Math.floor(Number(minBytes)) || PRESSURE_MIN_BYTES);
  const maxCount = Math.max(1, Math.floor(Number(maxSamples)) || PRESSURE_SAMPLE_DEFAULTS.maxSamples);
  const step = Math.max(0, Math.floor(Number(intervalMs)) || PRESSURE_SAMPLE_DEFAULTS.intervalMs);
  // Connection signal selection (fail-closed): the per-pid fd table is the
  // connection-owned signal; when the fd table is unreadable (EACCES,
  // dumpable=0, fake fs without readdir/readlink) the listener-local-port
  // established count is the fallback, and when neither exists the install
  // fails closed instead of gating on a phantom signal.
  const listenerPortNumber = listenerPort === null || listenerPort === undefined
    ? null
    : Number(listenerPort);
  if (listenerPortNumber !== null && (!Number.isInteger(listenerPortNumber) || listenerPortNumber <= 0)) {
    throw new P0ContractError('pressure', `listenerPort must be a positive integer port (fail-closed): ${listenerPort}`);
  }
  const h0Snap = await processTreeRssSnapshot(pid, { fsp, maxDescendants });
  const h0 = h0Snap ? h0Snap.rssBytes : null;
  const fd0 = h0Snap ? h0Snap.fdSocketCount : null;
  let signalMode = null;
  let fdBaseline = null;
  let connBaseline = null;
  if (Number.isFinite(fd0)) {
    signalMode = 'fd';
    fdBaseline = Number(fd0);
  } else if (listenerPortNumber !== null) {
    connBaseline = await listenerEstablishedConnectionCount(pid, listenerPortNumber, { fsp });
    if (Number.isFinite(connBaseline)) signalMode = 'listener';
  }
  if (!Number.isFinite(h0) || signalMode === null) {
    throw new P0ContractError('pressure', `connection signal unavailable (fail-closed): `
      + `fd table unreadable (fd0=${fd0}) and no listener-local-port signal `
      + `(listenerPort=${listenerPortNumber}, established=${connBaseline}); h0=${h0}`);
  }
  const baseline = Number(h0);
  await pressureSubscribePaused(pressureClient, identity, cursor);
  const samples = [];
  let installedAt = null;
  for (let index = 0; index < maxCount; index += 1) {
    if (index > 0) await delay(step);
    const snapshot = await processTreeRssSnapshot(pid, { fsp, maxDescendants });
    const listenerEstablished = signalMode === 'listener'
      ? await listenerEstablishedConnectionCount(pid, listenerPortNumber, { fsp })
      : null;
    samples.push({
      tMs: now(),
      rssBytes: snapshot ? snapshot.rssBytes : null,
      fdSocketCount: snapshot ? snapshot.fdSocketCount : null,
      processCount: snapshot ? snapshot.processCount : null,
      listenerEstablished,
    });
    const current = samples[samples.length - 1];
    // Connection signal +1: fd mode requires fdSocketCount > fd0; listener
    // fallback requires the established count on the listener port to rise
    // by at least one over conn0.
    const signalRaised = signalMode === 'fd'
      ? Number.isFinite(current.fdSocketCount) && current.fdSocketCount > fdBaseline
      : Number.isFinite(current.listenerEstablished) && current.listenerEstablished >= connBaseline + 1;
    if (Number.isFinite(current.rssBytes)
      && current.rssBytes >= baseline + riseBytes
      && signalRaised) {
      installedAt = samples.length - 1;
      break;
    }
  }
  if (installedAt === null) {
    const finiteRss = samples.filter((s) => Number.isFinite(s.rssBytes)).map((s) => s.rssBytes);
    const finiteFd = samples.filter((s) => Number.isFinite(s.fdSocketCount)).map((s) => s.fdSocketCount);
    const rssDeltasKiB = samples.map((s) => (Number.isFinite(s.rssBytes)
      ? Math.round((s.rssBytes - baseline) / 1024)
      : null));
    const fdDeltas = samples.map((s) => (Number.isFinite(s.fdSocketCount)
      ? s.fdSocketCount - fdBaseline
      : null));
    const finiteConn = samples.filter((s) => Number.isFinite(s.listenerEstablished)).map((s) => s.listenerEstablished);
    const connDeltas = samples.map((s) => (Number.isFinite(s.listenerEstablished)
      ? s.listenerEstablished - connBaseline
      : null));
    const summary = `h0=${baseline}B signal=${signalMode} fd0=${fdBaseline} conn0=${connBaseline} `
      + `listenerPort=${listenerPortNumber} minBytes=${riseBytes}B `
      + `samples=${samples.length} rssMin=${finiteRss.length ? Math.min(...finiteRss) : null}B `
      + `rssMax=${finiteRss.length ? Math.max(...finiteRss) : null}B `
      + `fdMin=${finiteFd.length ? Math.min(...finiteFd) : null} `
      + `fdMax=${finiteFd.length ? Math.max(...finiteFd) : null} `
      + `connMin=${finiteConn.length ? Math.min(...finiteConn) : null} `
      + `connMax=${finiteConn.length ? Math.max(...finiteConn) : null} `
      + `rssDeltaKiB=[${rssDeltasKiB.join(',')}] fdDelta=[${fdDeltas.join(',')}] `
      + `connDelta=[${connDeltas.join(',')}]`;
    const err = new P0ContractError('pressure', `connection pressure not installed within window: `
      + `installed=false over ${samples.length} samples (maxSamples=${maxCount}, `
      + `intervalMs=${step}, minBytes=${riseBytes}, signal=${signalMode}); ${summary}`);
    err.pressureEvidence = {
      installed: false,
      h0: baseline,
      signalMode,
      fd0: fdBaseline,
      conn0: connBaseline,
      listenerPort: listenerPortNumber,
      minBytes: riseBytes,
      sampling: { intervalMs: step, maxSamples: maxCount },
      samples,
    };
    throw err;
  }
  const installedSample = samples[installedAt];
  return {
    sourcePid: Number(pid),
    installed: true,
    installedAtSample: installedAt,
    sampleCountTaken: samples.length,
    stoppedEarly: samples.length < maxCount,
    h0: baseline,
    h1: installedSample.rssBytes,
    installedBytes: installedSample.rssBytes - baseline,
    signalMode,
    fd0: signalMode === 'fd' ? fdBaseline : null,
    fd1: signalMode === 'fd' ? installedSample.fdSocketCount : null,
    listenerPort: signalMode === 'listener' ? listenerPortNumber : null,
    conn0: signalMode === 'listener' ? connBaseline : null,
    conn1: signalMode === 'listener' ? installedSample.listenerEstablished : null,
    processCount: installedSample.processCount,
    h0ProcessCount: h0Snap.processCount,
    pressureClient,
    samples,
  };
}

// Plateau → install paused unread subscribe → causal destroy → gated release.
// `plateau` may be a completed sampleRssUntilPlateau result or sampling
// options. `install` is forwarded to installConnectionOwnedPressure (must
// include pressureClient + identity). The causal destroy here targets ONLY
// installed.pressureClient — every unrelated client (control clients, browser
// contexts, the listener's other connections) stays connected through the
// whole release window, so an unrelated disconnect can never cause the
// release. The caller-supplied `disconnect` hook runs only after the release
// verdict is retained, receiving { pressure, release }; on the fail-closed
// no-release path the P0ContractError propagates with the full evidence and
// the caller owns unrelated-client teardown. Release is gated on
// magnitude >= installed.installedBytes (fail-closed below
// PRESSURE_MIN_BYTES), the connection signal drop (fd below fd1 or the
// listener established count below conn1), and unchanged processCount.
// Scout loaded=8 / same-incarnation is the caller's job.
export async function runConnectionPressureRelease({
  pid,
  fsp,
  now,
  delay,
  maxDescendants,
  plateau: plateauArg = {},
  install: installOptions = {},
  disconnect,
  release: releaseOptions = {},
} = {}) {
  if (!Number.isFinite(Number(pid))) {
    throw new P0ContractError('rss', 'numeric pid required');
  }
  let slope = plateauArg;
  if (!slope || !slope.plateau || slope.plateau.detected !== true) {
    slope = await sampleRssUntilPlateau({
      pid,
      fsp,
      now,
      delay,
      maxDescendants,
      ...plateauArg,
    });
  }
  if (!slope.plateau || slope.plateau.detected !== true) {
    throw new P0ContractError('rss', `plateau.detected !== true: ${JSON.stringify(slope.plateau)}`);
  }
  const installed = await installConnectionOwnedPressure({
    pid,
    fsp,
    now,
    delay,
    maxDescendants,
    ...installOptions,
  });
  const minReleaseBytes = Number(installed.installedBytes);
  if (!Number.isFinite(minReleaseBytes) || minReleaseBytes < PRESSURE_MIN_BYTES) {
    throw new P0ContractError('rss', `installedBytes must be >= ${PRESSURE_MIN_BYTES} `
      + `(fail-closed): ${installed.installedBytes}`);
  }
  // Causal destroy: only the connection-owned pressure client. The release
  // window below is sampled while every unrelated client stays connected.
  await destroyPressureClient(installed.pressureClient);
  let released;
  try {
    released = await sampleRssUntilRelease({
      pid,
      fsp,
      now,
      delay,
      maxDescendants,
      intervalMs: releaseOptions.intervalMs,
      maxSamples: releaseOptions.maxSamples,
      confirmSamples: releaseOptions.confirmSamples,
      baselineBytes: installed.h1,
      minReleaseBytes,
      baselineFdSocketCount: installed.signalMode === 'fd' ? installed.fd1 : undefined,
      listenerPort: installed.signalMode === 'listener' ? installed.listenerPort : undefined,
      baselineListenerEstablishedCount: installed.signalMode === 'listener' ? installed.conn1 : undefined,
      baselineProcessCount: installed.processCount,
    });
  } catch (error) {
    if (error instanceof P0ContractError && error.rssEvidence) {
      error.rssEvidence.plateau = slope.plateau;
      error.rssEvidence.slopeBytesPerMs = slope.slopeBytesPerMs;
      error.rssEvidence.pressure = {
        h0: installed.h0,
        h1: installed.h1,
        installedBytes: installed.installedBytes,
        signalMode: installed.signalMode,
        fd0: installed.fd0,
        fd1: installed.fd1,
        listenerPort: installed.listenerPort,
        conn0: installed.conn0,
        conn1: installed.conn1,
        processCount: installed.processCount,
      };
    }
    throw error;
  }
  // Release verdict retained; only now may unrelated clients disconnect.
  if (typeof disconnect === 'function') {
    await disconnect({ pressure: installed, release: released });
  }
  return {
    slope,
    plateau: slope.plateau,
    pressure: installed,
    release: released,
  };
}



export async function runP0ResourceContract({
  evidencePath,
  nativeSession,
  pressureSession,
  clientFactory,
  pressureClientFactory,
  scoutClientFactory,
  pid = null,
  page = null,
  pages = null,
  disconnectBrowserClients,
  fsp,
  now = defaultNow,
  delay,
  dualPrompt = 'p0-slow-a',
  dualNeedle = 'slow-a-done',
  skipLiveTransport = false,
  listenerPort = null,
} = {}) {
  const failures = [];
  const contracts = {};
  const recordFail = (id, error) => {
    failures.push({ id, message: error instanceof Error ? error.message : String(error) });
  };

  let generatedNative = null;
  try {
    generatedNative = await generateAndLoadExactSession(nativeSession);
    contracts.nativeSession = nativeSessionEvidence(generatedNative, null);
  } catch (error) {
    recordFail('native-session', error);
  }

  let clientA = null;
  let clientB = null;
  let pool = null;
  let latency = null;
  let dual = null;
  let rss = null;
  let generatedPressure = null;
  let pressureLive = null;
  let pressureClient = null;
  let scout = null;
  let plateau = null;
  let released = null;
  let baselinesClosed = false;

  if (!skipLiveTransport) {
    if (typeof clientFactory !== 'function') {
      recordFail('transport', new P0ContractError('transport', 'clientFactory is required unless skipLiveTransport'));
    } else {
      try {
        clientA = await clientFactory('A');
        clientB = await clientFactory('B');
        await clientA.ready;
        await clientB.ready;
      } catch (error) {
        recordFail('transport', error);
      }
    }

    if (clientA && failures.every((row) => row.id !== 'transport')) {
      if (generatedNative) {
        try {
          const liveNative = await openExactNativeSessionLive(clientA, generatedNative);
          contracts.nativeSession = nativeSessionEvidence(generatedNative, liveNative);
        } catch (error) {
          recordFail('native-session', error);
        }
      }

      try {
        pool = await fillEvictReopenPool(clientA);
        contracts.pool = {
          ...pool,
          retiredPath: pool.retiredPath ? normalizeEvidencePath(pool.retiredPath) : pool.retiredPath,
        };
      } catch (error) {
        recordFail('pool', error);
      }

      try {
        clientA.setPhase('latency');
        const listed = await clientA.call({
          type: 'session_list',
          scope: 'all_projects',
          sessionId: pool ? pool.primary : undefined,
        });
        const data = await requireRpcData(listed, 'session_list:latency');
        const target = (data.sessions || []).find((row) => row && row.loaded === true && row.path);
        if (!target) throw new P0ContractError('latency', 'no loaded session path for switch');
        latency = await measureAuthoritativeInteractiveLatency({
          client: clientA,
          now,
          action: async () => {
            const switched = await clientA.call({
              type: 'switch_session',
              sessionPath: target.path,
              sessionId: target.sessionId,
              responseMode: SWITCH_HANDOFF_RESPONSE_MODE,
            });
            return parseHandoff(await requireRpcData(switched, 'switch_session:latency'), target.sessionId);
          },
        });
        contracts.latency = latency;
      } catch (error) {
        recordFail('latency', error);
      }

      if (clientB) {
        try {
          dual = await runDualClientBackgroundStream({
            clientA,
            clientB,
            prompt: dualPrompt,
            replyNeedle: dualNeedle,
            now,
            pool,
          });
          contracts.dualClient = dual;
        } catch (error) {
          recordFail('dual-client', error);
        }
      }
    }

    if (pid !== null && pid !== undefined) {
      try {
        if (!pressureSession) {
          throw new P0ContractError('pressure', 'pressureSession is required for the live RSS release contract');
        }
        if (typeof pressureClientFactory !== 'function') {
          throw new P0ContractError('pressure', 'pressureClientFactory is required for the live RSS release contract');
        }
        if (typeof scoutClientFactory !== 'function') {
          throw new P0ContractError('pressure', 'scoutClientFactory is required for the live RSS release contract');
        }
        if (!clientA || !clientB || !pool?.reopenedIdentity) {
          throw new P0ContractError('pressure', 'functional session contracts must pass before pressure installation');
        }

        await requireRpcData(await clientA.call({
          type: 'get_state',
          sessionId: pool.reopenedIdentity.sessionId,
        }), 'get_state:native-before-pressure');
        generatedPressure = await writeExactSelectedPressureSession(pressureSession);
        pressureLive = await openExactNativeSessionLive(clientA, generatedPressure);
        contracts.pressureSession = {
          sessionId: generatedPressure.sessionId,
          bytes: generatedPressure.bytes,
          selectedContextBytes: generatedPressure.selectedContextBytes,
          selectedMessageCount: generatedPressure.selectedMessageCount,
          live: pressureLive,
        };

        const listed = await clientA.call({
          type: 'session_list',
          scope: 'all_projects',
          sessionId: pool.primary,
        });
        const loadedPrePressure = loadedIdsFromList(await requireRpcData(listed, 'session_list:pressure'));
        if (loadedPrePressure.length !== MAX_LOADED_RUNTIMES) {
          throw new P0ContractError('pressure', `pressure open changed loaded pool: expected ${MAX_LOADED_RUNTIMES}, got ${loadedPrePressure.length}`);
        }

        plateau = await sampleRssUntilPlateau({
          pid,
          fsp,
          now,
          delay,
          intervalMs: RSS_LIVE_SAMPLE_DEFAULTS.intervalMs,
          minSamples: RSS_LIVE_SAMPLE_DEFAULTS.minSamples,
          maxSamples: RSS_LIVE_SAMPLE_DEFAULTS.maxSamples,
        });
        if (!plateau.plateau?.detected) {
          throw new P0ContractError('rss', `plateau.detected !== true: ${JSON.stringify(plateau.plateau)}`);
        }

        pressureClient = await pressureClientFactory();
        if (!pressureClient) throw new P0ContractError('pressure', 'pressureClientFactory returned no client');
        await pressureClient.ready;
        const installed = await installConnectionOwnedPressure({
          pid,
          pressureClient,
          identity: pressureLive.identity,
          fsp,
          now,
          delay,
          listenerPort,
        });
        // Causal ordering: destroy the pressure socket and sample the whole
        // release window while clientA/clientB/browser/control stay
        // connected; the unrelated close happens only after the verdict.
        released = await releaseConnectionOwnedPressure({
          pid,
          h1: installed.h1,
          installedBytes: installed.installedBytes,
          fd1: installed.signalMode === 'fd' ? installed.fd1 : undefined,
          listenerPort: installed.signalMode === 'listener' ? installed.listenerPort : undefined,
          conn1: installed.signalMode === 'listener' ? installed.conn1 : undefined,
          processCount1: installed.processCount,
          pressureClient: installed.pressureClient,
          fsp,
          now,
          delay,
        });
        pressureClient = null;
        // Release verdict settled; only now close the unrelated clients.
        await disconnectControlThenBrowser({
          clientA,
          clientB,
          pages,
          page,
          disconnectBrowserClients,
        });
        baselinesClosed = true;

        scout = await scoutClientFactory();
        if (!scout) throw new P0ContractError('pressure', 'scoutClientFactory returned no client');
        await scout.ready;
        const scoutBoot = await bootstrapPrimary(scout);
        const scoutListed = await scout.call({
          type: 'session_list',
          scope: 'all_projects',
          sessionId: scoutBoot.identity.sessionId,
        });
        const scoutSessions = loadedIdsFromList(await requireRpcData(scoutListed, 'session_list:scout'));
        if (scoutSessions.length !== MAX_LOADED_RUNTIMES) {
          throw new P0ContractError('pressure', `scout loaded !== ${MAX_LOADED_RUNTIMES} after release: got ${scoutSessions.length}`);
        }
        const pressureRow = scoutSessions.find((row) => row.sessionId === pressureLive.identity.sessionId);
        if (!pressureRow || !frameHasExactIdentity(pressureRow, pressureLive.identity)) {
          throw new P0ContractError('pressure', 'pressure runtime identity changed after connection release');
        }
        const nativeRow = scoutSessions.find((row) => row.sessionId === pool.reopenedIdentity.sessionId);
        if (!nativeRow || !frameHasExactIdentity(nativeRow, pool.reopenedIdentity)) {
          throw new P0ContractError('pressure', 'native runtime identity changed after connection release');
        }
        requireCleanLocalClose(await scout.close(1000, 'p0-scout-disconnect'), 'scout');
        scout = null;

        contracts.rss = {
          plateau: plateau.plateau,
          slopeBytesPerMs: plateau.slopeBytesPerMs,
          pressure: {
            installed: true,
            h0: installed.h0,
            h1: installed.h1,
            installedBytes: installed.installedBytes,
            signalMode: installed.signalMode,
            fd0: installed.fd0,
            fd1: installed.fd1,
            listenerPort: installed.listenerPort,
            conn0: installed.conn0,
            conn1: installed.conn1,
            processCount: installed.processCount,
            pressureSessionId: generatedPressure.sessionId,
            pressureIdentity: pressureLive.identity,
          },
          release: released,
          scout: {
            loadedCount: scoutSessions.length,
            nativeIdentity: pool.reopenedIdentity,
            pressureIdentity: pressureLive.identity,
          },
        };
      } catch (error) {
        recordFail('rss', error);
        const evidence = {};
        if (error && typeof error === 'object' && error.rssEvidence) Object.assign(evidence, error.rssEvidence);
        if (error && typeof error === 'object' && error.pressureEvidence) evidence.pressure = error.pressureEvidence;
        if (plateau?.plateau) {
          evidence.plateau = plateau.plateau;
          evidence.slopeBytesPerMs = plateau.slopeBytesPerMs;
        }
        contracts.rss = evidence;
      } finally {
        if (!baselinesClosed) {
          // Failure path: the verdict is settled (released=false), so the
          // unrelated close runs best-effort as cleanup.
          try {
            await disconnectControlThenBrowser({
              clientA,
              clientB,
              pages,
              page,
              disconnectBrowserClients,
            });
          } catch {}
        }
        if (pressureClient) {
          try { await destroyPressureClient(pressureClient); } catch {}
          pressureClient = null;
        }
        if (scout) {
          try { await scout.close(1000, 'p0-scout-disconnect'); } catch {}
          scout = null;
        }
      }
    } else {
      recordFail('rss', new P0ContractError('rss', 'pid missing (fail-closed)'));
    }

  }

  const unexpectedCloses = [
    ...(clientA ? clientA.unexpectedCloses : []),
    ...(clientB ? clientB.unexpectedCloses : []),
  ];
  const closeCodes = [
    ...(clientA ? clientA.closes : []),
    ...(clientB ? clientB.closes : []),
  ];
  if (unexpectedCloses.length) {
    recordFail('ws-close', new P0ContractError('ws-close', `unexpected closes: ${JSON.stringify(unexpectedCloses)}`));
  }

  const payload = {
    schema: P0_RESOURCE_SCHEMA,
    ok: failures.length === 0,
    contracts,
    failures,
    unexpectedCloses,
    closeCodes,
    recorded1006: closeCodes.filter((row) => row.code === 1006),
  };

  if (!evidencePath) {
    throw new P0ContractError('evidence', 'evidencePath is required (fail-closed)');
  }
  const written = await writeEvidenceJson(evidencePath, payload, { fsp, now });
  if (failures.length) {
    const err = new P0ContractError('p0-resource', failures.map((row) => `${row.id}: ${row.message}`).join('; '));
    err.evidence = written.evidence;
    throw err;
  }
  return written;
}
