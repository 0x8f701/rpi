#!/usr/bin/env node
// Focused unit/syntax checks for the P0 session-acceptance harness.
// No rpi binary, no playwright, no long live run.

import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { assertNoOwnerStreamToPeer, assertBusyProtectedEviction, bootstrapPrimary, createControlClient, createNodePressureClient, createPageWebSocketImpl, fillEvictReopenPool, generateAndLoadExactSession, installConnectionOwnedPressure, isAuthoritativeCut, frameHasExactIdentity, MAX_LOADED_RUNTIMES, payloadContainsMarker, releaseConnectionOwnedPressure, requireCleanLocalClose, subscribeAuthoritative, openExactNativeSessionLive, nativeSessionEvidence, measureAuthoritativeInteractiveLatency, parseHandoff, parseLifecycleIdentity, P0ContractError, PRESSURE_MIN_BYTES, classifyClose, closeBrowserClients, disconnectControlThenBrowser, runConnectionPressureRelease, runDualClientBackgroundStream, runP0ResourceContract, sampleRssPlateauAndRelease, sampleRssUntilPlateau, sampleRssUntilRelease } from './p0_resource_driver.mjs';
import { captureResourceSnapshot, connectionFdSocketCount, disconnectReleaseMeasures, listenerEstablishedConnectionCount, normalizeEvidencePath, processTreeRssSnapshot, redactPathSubstrings, redactValue, sampleRssSlope, writeEvidenceJson } from './p0_resource_metrics.mjs';
import { EXACT_NATIVE_SESSION_BYTES, MAX_NATIVE_LINE_BYTES, MAX_SELECTED_CONTEXT_BYTES, NATIVE_MARKER_ENTRY_ID, inspectNativeParentChain, jsonlLine, nativeMessageUserText, nativeSessionPath, parseNativeSessionRecords } from './p0_native_session.mjs';

const here = dirname(fileURLToPath(import.meta.url));
let failed = 0;
function check(name, cond, detail = '') {
  if (cond) {
    console.log(`ok ${name}`);
    return;
  }
  failed += 1;
  console.error(`FAIL ${name}${detail ? `: ${detail}` : ''}`);
}

function memoryFs() {
  const files = new Map();
  return {
    files,
    async mkdir() {},
    async writeFile(path, data) {
      files.set(path, String(data));
    },
    async rename(from, to) {
      if (!files.has(from)) throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      files.set(to, files.get(from));
      files.delete(from);
    },
    async readFile(path) {
      if (!files.has(path)) throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      return files.get(path);
    },
  };
}

class FakeSocket {
  constructor() {
    this.handlers = { open: [], message: [], close: [], error: [], clientSend: [] };
    this.sent = [];
    queueMicrotask(() => this.emit('open'));
  }
  on(event, handler) {
    this.handlers[event].push(handler);
  }
  emit(event, ...args) {
    for (const handler of this.handlers[event] || []) handler(...args);
  }
  send(data) {
    const text = String(data);
    this.sent.push(text);
    this.emit('clientSend', text);
  }
  close(code = 1000, reason = '') {
    this.emit('close', code, reason);
  }
}

function parseSent(socket) {
  return socket.sent.map((text) => JSON.parse(text));
}

// Fake page adapter mirroring the playwright surface used by the in-page
// WebSocket bridge: exposeFunction (duplicate names throw, first wins),
// evaluate (runs in a fresh window world with an in-page WebSocket class),
// and page.close() teardown that abnormally (1006) kills still-open sockets.
// Used to prove the close-bridge behavior without a browser.
function fakePageWorld({ openMacroTask = false, peerAbandonsClose = false } = {}) {
  class BrowserWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    constructor(url, protocols = []) {
      this.url = url;
      this.protocols = protocols;
      this.readyState = 0;
      this.listeners = { open: [], message: [], close: [], error: [] };
      this.sent = [];
      world.created.push(this);
      const open = () => {
        if (this.readyState !== 0) return;
        this.readyState = 1;
        for (const handler of this.listeners.open) handler();
      };
      if (openMacroTask) setTimeout(open, 0);
      else queueMicrotask(open);
    }
    addEventListener(event, handler) {
      if (this.listeners[event]) this.listeners[event].push(handler);
    }
    emit(event, ...args) {
      for (const handler of this.listeners[event] || []) handler(...args);
    }
    send(text) {
      if (this.readyState !== 1) throw new Error('socket not open');
      this.sent.push(String(text));
    }
    deliver(text) {
      this.emit('message', { data: text });
    }
    close(code = 1000, reason = '') {
      // Spec: close() during CONNECTING fails the WebSocket connection —
      // the close event then reports 1006, never the requested code.
      if (this.readyState === 0) {
        this.readyState = 3;
        this.emit('close', { code: 1006, reason: '' });
        return;
      }
      if (this.readyState >= 2) return;
      queueMicrotask(() => {
        this.readyState = 3;
        // Peer abandoned the close handshake: instead of echoing the
        // requested code in a close frame it dropped the TCP connection.
        // Mirrors the live backend defect and must surface as 1006.
        this.emit('close', { code: peerAbandonsClose ? 1006 : code, reason: peerAbandonsClose ? '' : reason });
      });
    }
  }

  const world = { created: [] };
  const page = {
    world,
    bindings: new Map(),
    closed: false,
    sockets: () => world.created.filter(Boolean),
    async exposeFunction(name, fn) {
      if (page.bindings.has(name)) throw new Error(`Function "${name}" has been already registered`);
      page.bindings.set(name, fn);
      world[name] = fn;
    },
    evaluate(fn, arg) {
      if (page.closed) return Promise.reject(new Error('page closed'));
      // Playwright's evaluate always returns a promise, even for sync bodies.
      return Promise.resolve()
        .then(() => new Function('window', 'WebSocket', 'arg', `return (${fn})(arg);`)(world, BrowserWebSocket, arg));
    },
    async close() {
      for (const socket of world.created) {
        if (socket.readyState < 3) {
          socket.readyState = 3;
          socket.emit('close', { code: 1006, reason: '' });
        }
      }
      page.closed = true;
    },
  };
  return page;
}

function fakePageClient(page, { callTimeoutMs = 15000 } = {}) {
  return createControlClient({
    url: 'ws://127.0.0.1:9/ws',
    WebSocketImpl: createPageWebSocketImpl(page),
    callTimeoutMs,
    now: (() => {
      let t = 9_000;
      return () => (t += 1);
    })(),
  });
}

function respond(socket, request, data, extra = {}) {
  socket.emit('message', JSON.stringify({
    type: 'response',
    id: request.id,
    success: true,
    sessionId: extra.sessionId || data.sessionId,
    data,
  }));
}

function respondError(socket, request, message) {
  socket.emit('message', JSON.stringify({
    type: 'response',
    id: request.id,
    success: false,
    error: message,
  }));
}


function fakeClientWithPool(opts = {}) {
  const socket = new FakeSocket();
  const client = createControlClient({
    url: 'ws://127.0.0.1:9/ws',
    WebSocketImpl: class {
      constructor() {
        return socket;
      }
    },
    now: (() => {
      let t = 1_000;
      return () => (t += 1);
    })(),
  });
  const sessions = [];
  const subscribes = [];
  const stateSends = [];
  const pushSession = (id, loaded = true) => {
    const existing = sessions.find((row) => row.sessionId === id);
    if (existing) {
      existing.loaded = loaded;
      return existing;
    }
    const row = {
      sessionId: id,
      path: `<workspace>/sessions/${id}.jsonl`,
      loaded,
      runtimeIncarnation: `inc-${id}`,
    };
    sessions.push(row);
    return row;
  };
  pushSession('primary', true);

  socket.on('clientSend', (raw) => {
    const request = JSON.parse(String(raw));
    if (request.type === 'get_state') {
      const sid = request.sessionId || 'primary';
      stateSends.push(sid);
      respond(socket, request, { sessionId: sid, runtimeIncarnation: `inc-${sid}` });
      return;
    }
    if (request.type === 'session_list') {
      respond(socket, request, { sessions: sessions.map((row) => ({ ...row })) }, { sessionId: 'primary' });
      return;
    }
    if (request.type === 'new_session') {
      const id = `sess-${sessions.filter((row) => row.sessionId.startsWith('sess-') || row.sessionId === 'primary').length}`;
      const loaded = sessions.filter((row) => row.loaded).length;
      if (loaded >= MAX_LOADED_RUNTIMES) {
        // Production acquire_loaded_slot: never retire a busy runtime and
        // fail closed when every non-primary is busy. The malformed
        // busyServer modes model the violations the driver must reject.
        const victim = sessions.find((row) => row.loaded && row.sessionId !== 'primary'
          && (opts.busyServer === 'evictsBusy' || !row.busy));
        if (!victim) {
          if (opts.busyServer === 'acceptsAllBusy') {
            pushSession(id, true);
            respond(socket, request, {
              sessionId: id,
              runtimeIncarnation: `inc-${id}`,
              state: { sessionId: id },
              messages: [],
            });
            return;
          }
          respondError(socket, request, `too many busy sessions (limit ${MAX_LOADED_RUNTIMES}); finish or stop active work and retry`);
          return;
        }
        victim.loaded = false;
      }
      pushSession(id, true);
      respond(socket, request, {
        sessionId: id,
        runtimeIncarnation: `inc-${id}`,
        state: { sessionId: id },
        messages: [],
      });
      return;
    }
    if (request.type === 'switch_session') {
      const target = sessions.find((row) => row.path === request.sessionPath);
      if (target && !target.loaded) {
        if (opts.reopenMode === 'overCap') {
          // Malformed server: reopens the target without evicting a runtime.
          target.loaded = true;
        } else if (opts.reopenMode === 'double') {
          // Malformed server: evicts two idles, reloads the target, and adds
          // a fresh runtime — the loaded set changes by two, not one.
          const dropIds = sessions
            .filter((row) => row.loaded && row.sessionId !== 'primary' && row.sessionId !== target.sessionId)
            .slice(0, 2)
            .map((row) => row.sessionId);
          for (const dropId of dropIds) {
            const drop = sessions.find((row) => row.sessionId === dropId);
            if (drop) drop.loaded = false;
          }
          target.loaded = true;
          pushSession(`sess-${sessions.filter((row) => row.sessionId.startsWith('sess-') || row.sessionId === 'primary').length}`, true);
        } else if (opts.reopenMode !== 'noop') {
          const loaded = sessions.filter((row) => row.loaded).length;
          if (loaded >= MAX_LOADED_RUNTIMES) {
            const idle = sessions.find((row) => row.loaded && row.sessionId !== 'primary'
              && row.sessionId !== target.sessionId && !row.busy);
            if (idle) idle.loaded = false;
          }
          target.loaded = true;
        }
      }
      respond(socket, request, {
        sessionId: target ? target.sessionId : 'missing',
        runtimeIncarnation: target ? target.runtimeIncarnation : 'inc-missing',
        subscriptionRequired: true,
        lifecycleResponseVersion: 1,
        cursor: 4,
      });
      return;
    }
    if (request.type === 'subscribe') {
      const messages = opts.subscribeMessages !== undefined
        ? opts.subscribeMessages
        : (opts.nativeMarker
          ? [{ message: { role: 'user', content: [{ type: 'text', text: opts.nativeMarker }] } }]
          : []);
      subscribes.push({
        sessionId: request.sessionId,
        runtimeIncarnation: request.runtimeIncarnation,
        cursor: request.cursor || 0,
        reason: opts.subscribeReason || 'cold-full',
        messages,
      });
      socket.emit('message', JSON.stringify({
        type: 'subscription_snapshot',
        sessionId: request.sessionId,
        runtimeIncarnation: request.runtimeIncarnation,
        watermark: request.cursor || 0,
        reason: opts.subscribeReason || 'cold-full',
        state: {},
        messages,
        events: [],
      }));
      return;
    }
    if (request.type === 'prompt') {
      const target = sessions.find((row) => row.sessionId === (request.sessionId || 'primary'));
      if (target) target.busy = true;
      respond(socket, request, { accepted: true }, { sessionId: request.sessionId });
      socket.emit('message', JSON.stringify({
        type: 'message_delta',
        sessionId: request.sessionId,
        runtimeIncarnation: `inc-${request.sessionId}`,
        textContent: 'slow-a-done',
      }));
      return;
    }
    if (request.type === 'abort') {
      const target = sessions.find((row) => row.sessionId === (request.sessionId || 'primary'));
      if (target) target.busy = false;
      respond(socket, request, { accepted: true }, { sessionId: request.sessionId });
      return;
    }
  });
  return { client, socket, sessions, subscribes, stateSends };
}

async function main() {
  check('jsonlLine ends with newline', jsonlLine({ type: 'session' }).endsWith('\n'));
  check('nativeSessionPath encodes cwd', nativeSessionPath({
    sessionsRoot: '<workspace>/sessions',
    cwd: '<workspace>/work',
    sessionId: 'abc',
    timestamp: '2026-08-31T00:00:00.000Z',
  }) === '<workspace>/sessions/--<workspace>-work--/2026-08-31T000000-abc.jsonl');

  check('handoff rejects snapshot payload', (() => {
    try {
      parseHandoff({ sessionId: 'a', runtimeIncarnation: 'r', subscriptionRequired: true, lifecycleResponseVersion: 1, cursor: 1, state: {} });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /state or messages/.test(err.message);
    }
  })());
  check('lifecycle identity accepts snapshot', parseLifecycleIdentity({
    sessionId: 'n1',
    runtimeIncarnation: 'r1',
    state: {},
    messages: [],
  }).sessionId === 'n1');
  check('1006 during live is unexpected', classifyClose(1006, { phase: 'pool' }).unexpected === true);
  check('1006 during disconnect phase stays unexpected', classifyClose(1006, { phase: 'disconnect' }).unexpected === true);
  check('locally initiated 1000 is expected', classifyClose(1000, { locallyInitiated: true }).unexpected === false);
  check('uninitiated 1000 stays unexpected', classifyClose(1000, { locallyInitiated: false }).unexpected === true);
  check('null code is unexpected', classifyClose(null).unexpected === true);
  check('isAuthoritativeCut accepts exact identity', isAuthoritativeCut(
    { type: 'subscription_snapshot', sessionId: 'p0', runtimeIncarnation: 'inc-7' },
    { sessionId: 'p0', runtimeIncarnation: 'inc-7' }) === true);
  check('isAuthoritativeCut rejects missing frame incarnation', isAuthoritativeCut(
    { type: 'subscription_snapshot', sessionId: 'p0' },
    { sessionId: 'p0', runtimeIncarnation: 'inc-7' }) === false);
  check('isAuthoritativeCut rejects missing identity incarnation', isAuthoritativeCut(
    { type: 'subscription_snapshot', sessionId: 'p0', runtimeIncarnation: 'inc-7' },
    { sessionId: 'p0' }) === false);
  check('isAuthoritativeCut rejects incarnation mismatch', isAuthoritativeCut(
    { type: 'subscription_snapshot', sessionId: 'p0', runtimeIncarnation: 'inc-8' },
    { sessionId: 'p0', runtimeIncarnation: 'inc-7' }) === false);
  check('isAuthoritativeCut rejects sessionId mismatch', isAuthoritativeCut(
    { type: 'subscription_snapshot', sessionId: 'p1', runtimeIncarnation: 'inc-7' },
    { sessionId: 'p0', runtimeIncarnation: 'inc-7' }) === false);
  check('frameHasExactIdentity rejects missing incarnation', frameHasExactIdentity(
    { sessionId: 'p0', textContent: 'slow-a-done' },
    { sessionId: 'p0', runtimeIncarnation: 'inc-7' }) === false);
  check('frameHasExactIdentity rejects stale incarnation', frameHasExactIdentity(
    { sessionId: 'p0', runtimeIncarnation: 'inc-stale', textContent: 'slow-a-done' },
    { sessionId: 'p0', runtimeIncarnation: 'inc-7' }) === false);
  check('payload marker alone does not earn stream delivery', (() => {
    const owner = { sessionId: 'primary', runtimeIncarnation: 'inc-primary' };
    const frame = { type: 'message_delta', sessionId: 'primary', textContent: 'slow-a-done' };
    return payloadContainsMarker(frame, 'slow-a-done') === true
      && frameHasExactIdentity(frame, owner) === false;
  })());

  const sparseSocket = new FakeSocket();
  const sparseClient = createControlClient({
    url: 'ws://127.0.0.1:9/ws',
    WebSocketImpl: class {
      constructor() {
        return sparseSocket;
      }
    },
  });
  sparseSocket.on('clientSend', (raw) => {
    const request = JSON.parse(String(raw));
    if (request.type === 'subscribe') {
      sparseSocket.emit('message', JSON.stringify({
        type: 'subscription_snapshot',
        sessionId: request.sessionId,
        watermark: request.cursor || 0,
        reason: 'cold-full',
        events: [],
      }));
    }
  });
  await sparseClient.ready;
  check('missing-incarnation snapshot times out authoritative wait', await (async () => {
    try {
      await subscribeAuthoritative(sparseClient, { sessionId: 'p0', runtimeIncarnation: 'inc-7' }, 1, 80);
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /timed out/.test(err.message);
    }
  })());
  check('omitted bootstrap second call served from cache without new get_state', await (async () => {
    const bootClient = fakeClientWithPool();
    await bootClient.client.ready;
    const bootOmitted1 = await bootstrapPrimary(bootClient.client);
    const bootOmitted2 = await bootstrapPrimary(bootClient.client);
    return bootOmitted2 === bootOmitted1 && bootClient.stateSends.length === 1;
  })());
  check('explicit sessionId bootstrap issues fresh get_state', await (async () => {
    const bootClient = fakeClientWithPool();
    await bootClient.client.ready;
    const bootOmitted = await bootstrapPrimary(bootClient.client);
    const bootExplicit = await bootstrapPrimary(bootClient.client, { sessionId: 'primary' });
    return bootExplicit !== bootOmitted
      && bootClient.stateSends.length === 2
      && bootClient.stateSends[1] === 'primary'
      && bootExplicit.identity.sessionId === 'primary'
      && bootExplicit.identity.runtimeIncarnation === 'inc-primary';
  })());
  check('cached omitted bootstrap returned again after explicit RPC', await (async () => {
    const bootClient = fakeClientWithPool();
    await bootClient.client.ready;
    const bootOmitted1 = await bootstrapPrimary(bootClient.client);
    await bootstrapPrimary(bootClient.client, { sessionId: 'primary' });
    const bootOmitted3 = await bootstrapPrimary(bootClient.client);
    return bootOmitted3 === bootOmitted1 && bootClient.stateSends.length === 2;
  })());
  check('driver close is locally initiated', await (async () => {
    const closed = fakeClientWithPool();
    await closed.client.ready;
    closed.client.setPhase('disconnect');
    const rec = await closed.client.close(1000, 'p0-disconnect');
    return closed.client.locallyInitiatedClose() === true
      && rec.locallyInitiated === true
      && rec.unexpected === false
      && closed.client.unexpectedCloses.length === 0;
  })());
  check('uninduced 1006 after disconnect phase is unexpected', await (async () => {
    const abrupt = fakeClientWithPool();
    await abrupt.client.ready;
    abrupt.client.setPhase('disconnect');
    abrupt.socket.emit('close', 1006, 'peer closed');
    return abrupt.client.unexpectedCloses.length === 1
      && abrupt.client.unexpectedCloses[0].code === 1006
      && abrupt.client.unexpectedCloses[0].phase === 'disconnect';
  })());

  // Request-id namespace: every createControlClient owns a module-wide
  // counter so two clients on the same session never both mint p0-1.
  // Auto ids increment per client; an explicit command.id is retained
  // unchanged so callers can retry the same idempotent request.
  check('two clients first auto ids never collide on p0-1', await (async () => {
    const a = fakeClientWithPool();
    const b = fakeClientWithPool();
    await Promise.all([a.client.ready, b.client.ready]);
    await a.client.call({ type: 'get_state' });
    await b.client.call({ type: 'get_state' });
    const idA = parseSent(a.socket).find((row) => row.type === 'get_state').id;
    const idB = parseSent(b.socket).find((row) => row.type === 'get_state').id;
    const parseAuto = (id) => {
      const match = /^(p0-\d+)-(\d+)$/.exec(id);
      return match ? { ns: match[1], n: Number(match[2]) } : null;
    };
    const parsedA = parseAuto(idA);
    const parsedB = parseAuto(idB);
    return parsedA && parsedB
      && parsedA.ns !== parsedB.ns
      && parsedA.n === 1
      && parsedB.n === 1
      && idA !== 'p0-1'
      && idB !== 'p0-1'
      && idA !== idB;
  })());
  check('same client auto ids increment', await (async () => {
    const one = fakeClientWithPool();
    await one.client.ready;
    await one.client.call({ type: 'get_state' });
    await one.client.call({ type: 'session_list', scope: 'all_projects' });
    const sent = parseSent(one.socket);
    const first = sent.find((row) => row.type === 'get_state');
    const second = sent.find((row) => row.type === 'session_list');
    const parseAuto = (id) => {
      const match = /^(p0-\d+)-(\d+)$/.exec(id);
      return match ? { ns: match[1], n: Number(match[2]) } : null;
    };
    const parsedFirst = parseAuto(first && first.id);
    const parsedSecond = parseAuto(second && second.id);
    return parsedFirst && parsedSecond
      && parsedFirst.ns === parsedSecond.ns
      && parsedFirst.n === 1
      && parsedSecond.n === 2
      && first.id !== second.id;
  })());
  check('explicit command.id is retained', await (async () => {
    const one = fakeClientWithPool();
    await one.client.ready;
    await one.client.call({ type: 'get_state', id: 'caller-fixed-id' });
    const sent = parseSent(one.socket).find((row) => row.type === 'get_state');
    return sent && sent.id === 'caller-fixed-id';
  })());
  check('explicit command.id retry stays idempotent', await (async () => {
    const one = fakeClientWithPool();
    await one.client.ready;
    await one.client.call({ type: 'get_state', id: 'caller-fixed-id' });
    await one.client.call({ type: 'session_list', scope: 'all_projects', id: 'caller-fixed-id' });
    const sent = parseSent(one.socket);
    return sent.length === 2 && sent.every((row) => row.id === 'caller-fixed-id');
  })());


  // ---- In-page WebSocket close bridge (fake page adapter, no browser) ----
  // Root cause of live locally-initiated 1006: the old bridge shared
  // window.__p0ControlSocket and four global __p0* bindings across every
  // PageWebSocket on the page. A second client's bindings failed silently, so
  // its socket events were routed into the first client, send/close targeted
  // the last-bound socket, the first socket was never closed by its owner and
  // died with 1006 at page teardown. The fixed bridge uses per-instance
  // binding names and a per-page bridge registry, and close resolves only on
  // the target socket's actual close event.

  check('page bridge: clean local close records 1000 and targets the exact socket', await (async () => {
    const pg = fakePageWorld();
    const client = fakePageClient(pg);
    await client.ready;
    const bridge = pg.sockets()[0];
    await client.send({ type: 'ping' });
    if (bridge.sent.length !== 1 || JSON.parse(bridge.sent[0]).type !== 'ping') return false;
    client.setPhase('disconnect');
    const rec = await client.close(1000, 'p0-disconnect');
    requireCleanLocalClose(rec, 'page');
    return rec.code === 1000 && rec.reason === 'p0-disconnect'
      && bridge.readyState === 3
      && client.unexpectedCloses.length === 0
      && pg.sockets().length === 1;
  })());

  // Live backend defect (2026-08-31 evidence web-p0-resource): the rpi
  // control plane never flushes tungstenite's auto-queued close-reply frame
  // (listen.rs breaks on Message::Close and drops the socket), so a clean
  // socket.close(1000) is answered with a bare TCP FIN and every conformant
  // client reports 1006. The bridge cannot fix that from the page; the 1006
  // must therefore stay fail-closed: recorded as locally initiated, phase
  // disconnect, unexpected — never waived.
  check('page bridge: peer abandoning the close handshake keeps 1006 fail-closed', await (async () => {
    const pg = fakePageWorld({ peerAbandonsClose: true });
    const client = fakePageClient(pg);
    await client.ready;
    client.setPhase('disconnect');
    const rec = await client.close(1000, 'p0-disconnect');
    const waived = (() => {
      try {
        requireCleanLocalClose(rec, 'page-abandon');
        return false;
      } catch {
        return true;
      }
    })();
    return rec.code === 1006 && rec.locallyInitiated === true
      && rec.phase === 'disconnect' && rec.unexpected === true
      && waived && client.unexpectedCloses.length === 1;
  })());

  check('page bridge: two clients on one page never cross callbacks or sockets', await (async () => {
    const pg = fakePageWorld();
    const a = fakePageClient(pg);
    const b = fakePageClient(pg);
    await Promise.all([a.ready, b.ready]);
    const [sockA, sockB] = pg.sockets();
    if (!sockA || !sockB || sockA === sockB) return false;
    // Each instance registered its own four bindings; none collided.
    if (pg.bindings.size !== 8) return false;
    // Socket events route only to the owning client.
    sockA.deliver(JSON.stringify({ tag: 'from-a' }));
    sockB.deliver(JSON.stringify({ tag: 'from-b' }));
    const tagsOf = (client) => client.frames.map((entry) => entry.frame.tag).join(',');
    if (tagsOf(a) !== 'from-a' || tagsOf(b) !== 'from-b') return false;
    // a.close(1000) closes only a's socket; b stays open and usable.
    a.setPhase('disconnect');
    const recA = await a.close(1000, 'p0-disconnect');
    requireCleanLocalClose(recA, 'page-a');
    if (sockA.readyState !== 3 || sockB.readyState !== 1) return false;
    await b.send({ type: 'still-open' });
    if (sockB.sent.length !== 1 || JSON.parse(sockB.sent[0]).type !== 'still-open') return false;
    b.setPhase('disconnect');
    const recB = await b.close(1000, 'p0-disconnect');
    requireCleanLocalClose(recB, 'page-b');
    return recB.code === 1000 && recB.reason === 'p0-disconnect'
      && a.unexpectedCloses.length === 0 && b.unexpectedCloses.length === 0;
  })());

  check('page bridge: separate pages never share bindings or sockets', await (async () => {
    const pgA = fakePageWorld();
    const pgB = fakePageWorld();
    const a = fakePageClient(pgA);
    const b = fakePageClient(pgB);
    await Promise.all([a.ready, b.ready]);
    const [sockA] = pgA.sockets();
    const [sockB] = pgB.sockets();
    sockB.deliver(JSON.stringify({ tag: 'page-b-only' }));
    if (a.frames.length !== 0) return false;
    a.setPhase('disconnect');
    const recA = await a.close(1000, 'p0-disconnect');
    requireCleanLocalClose(recA, 'page-a');
    if (sockA.readyState !== 3 || sockB.readyState !== 1) return false;
    await closeBrowserClients({ pages: [pgA, pgB] });
    return sockB.readyState === 3 && pgA.sockets().every((s) => s.readyState === 3);
  })());

  check('page bridge: page teardown after clean closes never records 1006', await (async () => {
    const pg = fakePageWorld();
    const client = fakePageClient(pg);
    await client.ready;
    client.setPhase('disconnect');
    const rec = await client.close(1000, 'p0-disconnect');
    requireCleanLocalClose(rec, 'page');
    await closeBrowserClients({ pages: [pg] });
    await pg.close();
    return client.closes.length === 1
      && client.closes[0].code === 1000
      && client.closes[0].locallyInitiated === true
      && client.unexpectedCloses.length === 0;
  })());

  check('page bridge: close before open waits for the handshake and still closes cleanly', await (async () => {
    const pg = fakePageWorld({ openMacroTask: true });
    const client = fakePageClient(pg);
    // close() races the in-page socket while it is still CONNECTING; the
    // bridge must wait for open — calling socket.close() during CONNECTING
    // fails the connection and reports 1006 instead of 1000.
    client.setPhase('disconnect');
    const rec = await client.close(1000, 'p0-disconnect');
    requireCleanLocalClose(rec, 'page-early');
    return rec.code === 1000 && rec.reason === 'p0-disconnect'
      && client.closes.length === 1
      && client.unexpectedCloses.length === 0;
  })());

  check('page bridge: reserved close codes are refused without touching the transport', await (async () => {
    const pg = fakePageWorld();
    const client = fakePageClient(pg);
    await client.ready;
    const bridge = pg.sockets()[0];
    let refused = 0;
    for (const code of [999, 1001, 1004, 1005, 1006, 2999, 5000, 1000.5]) {
      try {
        await client.close(code, 'must-refuse');
      } catch (err) {
        if (err instanceof P0ContractError && new RegExp(`refusing reserved close code ${code}$`).test(err.message)) refused += 1;
      }
    }
    return refused === 8
      && client.locallyInitiatedClose() === false
      && client.closes.length === 0
      && client.unexpectedCloses.length === 0
      && bridge.readyState === 1;
  })());

  check('page bridge: disconnectControlThenBrowser closes two page clients cleanly', await (async () => {
    const pg = fakePageWorld();
    const a = fakePageClient(pg);
    const b = fakePageClient(pg);
    await Promise.all([a.ready, b.ready]);
    const records = await disconnectControlThenBrowser({ clientA: a, clientB: b, pages: [pg] });
    return records.length === 2
      && records.every((rec) => rec.code === 1000 && rec.locallyInitiated === true && rec.unexpected === false)
      && pg.sockets().every((socket) => socket.readyState === 3)
      && a.unexpectedCloses.length === 0
      && b.unexpectedCloses.length === 0;
  })());
  const root = await mkdtemp(join(tmpdir(), 'p0-native-'));
  try {
    const filePath = join(root, 'p0.jsonl');
    const written = await generateAndLoadExactSession({
      filePath,
      cwd: '<workspace>/p0-work',
      sessionId: 'p0-native-20mib',
      marker: 'p0-native-20mib-marker',
    });
    const st = await stat(filePath);
    check('exact 20 MiB on disk', st.size === EXACT_NATIVE_SESSION_BYTES, `size=${st.size}`);
    check('header id matches', written.header.id === 'p0-native-20mib');
    const head = await readFile(filePath, 'utf8');
    check('v3 session header first line', head.startsWith('{"type":"session","version":3,'));
    check('marker present', head.includes('p0-native-20mib-marker'));
    check('file ends with newline', head.endsWith('\n') || (await readFile(filePath)).slice(-1)[0] === 0x0a);
    const records = parseNativeSessionRecords(head);
    const chain = inspectNativeParentChain(records, 'p0-native-20mib-marker');
    check('selected leaf is last non-label entry', chain.selectedLeafId === written.selectedLeafId && chain.selectedLeafId === NATIVE_MARKER_ENTRY_ID);
    check('marker is on selected branch', written.markerOnSelectedBranch === true && chain.markerOnSelectedBranch === true);
    check('selected leaf user text is exact marker', written.selectedUserText === 'p0-native-20mib-marker' && chain.selectedUserText === 'p0-native-20mib-marker');
    check('selected-branch context stays compact', chain.selectedMessageCount === 1 && chain.selectedContextBytes < MAX_SELECTED_CONTEXT_BYTES, `count=${chain.selectedMessageCount} bytes=${chain.selectedContextBytes}`);
    check('off-branch sibling is not selected leaf', written.offBranchId === 'm-off' && chain.selectedLeafId !== 'm-off');
    check('off-branch marker is not the selected-branch marker', !chain.selectedBranchIds.includes('m-off'));
    check('loader-shaped selected branch has context messages', written.selectedMessageCount >= 1);
    check('every JSONL line stays under 8 MiB', records.every((rec) => Buffer.byteLength(JSON.stringify(rec), 'utf8') < MAX_NATIVE_LINE_BYTES));
    const last = records[records.length - 1];
    check('last record is the compact marker leaf', last && last.id === NATIVE_MARKER_ENTRY_ID && nativeMessageUserText(last) === 'p0-native-20mib-marker');
    const projected = records.filter((rec) => rec.type === 'message' && chain.selectedBranchIds.includes(rec.id)).map((rec) => ({ message: rec.message }));
    check('public-message projection of selected branch is exact marker', JSON.stringify(projected).includes('p0-native-20mib-marker') && projected.length === 1 && nativeMessageUserText({ message: projected[0].message }) === 'p0-native-20mib-marker');
  } finally {
    await rm(root, { recursive: true, force: true });
  }

  const { client } = fakeClientWithPool();
  await client.ready;
  const pool = await fillEvictReopenPool(client);
  check(`pool fills ${MAX_LOADED_RUNTIMES}`, pool.loadedAtCap.length === MAX_LOADED_RUNTIMES, JSON.stringify(pool.loadedAtCap));
  check('evicts idle LRU', pool.retired === 'sess-1', `retired=${pool.retired}`);
  check('reopens retired path', pool.reopenedIdentity.sessionId === pool.retired);
  check(`loaded stays ${MAX_LOADED_RUNTIMES} after reopen`, pool.loadedAfterReopen.length === MAX_LOADED_RUNTIMES);
  check(`count after reopen remains ${MAX_LOADED_RUNTIMES}`, pool.loadedCountAfterReopen === MAX_LOADED_RUNTIMES, `count=${pool.loadedCountAfterReopen}`);
  check('exactly one runtime replaced on reopen', (() => {
    const replaced = pool.replacedOnReopen;
    return typeof replaced === 'string' && replaced !== ''
      && replaced !== pool.retired
      && !pool.loadedAfterReopen.includes(replaced)
      && pool.loadedAfterReopen.length === MAX_LOADED_RUNTIMES;
  })(), JSON.stringify({ replacedOnReopen: pool.replacedOnReopen, loadedAfterReopen: pool.loadedAfterReopen }));
  // Malformed servers: the reopen handshake must fail closed instead of
  // accepting a loaded count above the cap or a zero/multiple replacement.
  const poolContractError = async (expectReject, contractId = 'reopen') => {
    try {
      await expectReject();
      return false;
    } catch (err) {
      return err instanceof P0ContractError && err.contractId === contractId;
    }
  };
  const overCap = fakeClientWithPool({ reopenMode: 'overCap' });
  await overCap.client.ready;
  check('reopen above the loaded cap throws P0ContractError', await poolContractError(async () => {
    await fillEvictReopenPool(overCap.client);
  }));
  const noop = fakeClientWithPool({ reopenMode: 'noop' });
  await noop.client.ready;
  check('zero replacement on reopen throws P0ContractError', await poolContractError(async () => {
    await fillEvictReopenPool(noop.client);
  }));
  const doubled = fakeClientWithPool({ reopenMode: 'double' });
  await doubled.client.ready;
  check('multiple replacement on reopen throws P0ContractError', await poolContractError(async () => {
    await fillEvictReopenPool(doubled.client);
  }));

  // Busy protection (acquire_loaded_slot): the exact LRU victim is busy, so
  // the cap create must retire the NEXT idle LRU runtime, keep the busy one
  // loaded, and fail closed against servers that retire the busy runtime or
  // accept an all-busy create.
  const busyPool = fakeClientWithPool();
  await busyPool.client.ready;
  check('busy LRU runtime survives the cap create; next idle LRU is the victim; all-busy create fails closed', await (async () => {
    const busy = await assertBusyProtectedEviction(busyPool.client);
    return busy.busyTarget === 'sess-1' && busy.expectedVictim === 'sess-2'
      && busy.retired === busy.expectedVictim && busy.retired !== busy.busyTarget;
  })(), 'assertBusyProtectedEviction threw');
  const evictsBusy = fakeClientWithPool({ busyServer: 'evictsBusy' });
  await evictsBusy.client.ready;
  check('server retiring the busy runtime fails closed', await poolContractError(async () => {
    await assertBusyProtectedEviction(evictsBusy.client);
  }, 'busy'));
  const acceptsAllBusy = fakeClientWithPool({ busyServer: 'acceptsAllBusy' });
  await acceptsAllBusy.client.ready;
  check('server accepting an all-busy create fails closed', await poolContractError(async () => {
    await assertBusyProtectedEviction(acceptsAllBusy.client);
  }, 'busy'));

  const liveClient = fakeClientWithPool({ nativeMarker: 'p0-live-native-marker' });
  await liveClient.client.ready;
  const liveRoot = await mkdtemp(join(tmpdir(), 'p0-live-native-'));
  try {
    const liveFilePath = join(liveRoot, 'native.jsonl');
    const liveWritten = await generateAndLoadExactSession({
      filePath: liveFilePath,
      cwd: '<workspace>/live-native',
      sessionId: 'p0-live-native',
      marker: 'p0-live-native-marker',
    });
    liveClient.sessions.push({
      sessionId: liveWritten.sessionId,
      path: liveWritten.path,
      loaded: true,
      runtimeIncarnation: `inc-${liveWritten.sessionId}`,
    });
    const live = await openExactNativeSessionLive(liveClient.client, liveWritten, { timeoutMs: 5000 });
    check('live open observes marker', live.markerObserved === true && live.marker === 'p0-live-native-marker');
    check('live open exact identity', live.identity.sessionId === liveWritten.sessionId && live.identity.runtimeIncarnation === `inc-${liveWritten.sessionId}`);
    check('live open cut type snapshot', live.cutType === 'subscription_snapshot');
    check('cold live open subscribes at cursor 0', liveClient.subscribes.length === 1
      && liveClient.subscribes[0].cursor === 0
      && liveClient.subscribes[0].sessionId === liveWritten.sessionId
      && liveClient.subscribes[0].runtimeIncarnation === `inc-${liveWritten.sessionId}`
      && live.cursor === 0);
    check('cold open cut is cold-full with messages array and marker', liveClient.subscribes[0].reason === 'cold-full'
      && Array.isArray(liveClient.subscribes[0].messages)
      && live.reason === 'cold-full'
      && live.markerObserved === true
      && live.marker === 'p0-live-native-marker');
  } finally {
    await rm(liveRoot, { recursive: true, force: true });
  }

  // Missing-marker negative: the authoritative cut omits the expected
  // marker entirely (marker: null); the live open must fail closed with
  // the native-session contract error naming the marker.
  const liveClientMissing = fakeClientWithPool({ nativeMarker: null });
  await liveClientMissing.client.ready;
  const missingRoot = await mkdtemp(join(tmpdir(), 'p0-live-missing-'));
  try {
    const missingWritten = await generateAndLoadExactSession({
      filePath: join(missingRoot, 'native.jsonl'),
      cwd: '<workspace>/live-missing',
      sessionId: 'p0-live-missing',
      marker: 'p0-live-native-marker',
    });
    liveClientMissing.sessions.push({
      sessionId: missingWritten.sessionId,
      path: missingWritten.path,
      loaded: true,
      runtimeIncarnation: `inc-${missingWritten.sessionId}`,
    });
    check('live open fails when cut omits marker', await (async () => {
      try {
        await openExactNativeSessionLive(liveClientMissing.client, missingWritten, { timeoutMs: 1000 });
        return false;
      } catch (err) {
        return err instanceof P0ContractError
          && err.contractId === 'native-session'
          && /omitted marker/.test(err.message);
      }
    })());
  } finally {
    await rm(missingRoot, { recursive: true, force: true });
  }

  // Wrong-marker negative: the cut substitutes a marker that is NOT
  // derived from the expected one — no shared substring, so the substring
  // presence check cannot accidentally pass. The live open must still
  // fail closed on the strict marker contract.
  const wrongMarker = 'unrelated-wrong-marker-x7';
  check('wrong fake marker not derived from expected', wrongMarker !== 'p0-live-native-marker' && !wrongMarker.includes('p0-live-native-marker'));
  const liveClientWrong = fakeClientWithPool({ nativeMarker: wrongMarker });
  await liveClientWrong.client.ready;
  const wrongRoot = await mkdtemp(join(tmpdir(), 'p0-live-wrong-'));
  try {
    const wrongWritten = await generateAndLoadExactSession({
      filePath: join(wrongRoot, 'native.jsonl'),
      cwd: '<workspace>/live-wrong',
      sessionId: 'p0-live-wrong',
      marker: 'p0-live-native-marker',
    });
    liveClientWrong.sessions.push({
      sessionId: wrongWritten.sessionId,
      path: wrongWritten.path,
      loaded: true,
      runtimeIncarnation: `inc-${wrongWritten.sessionId}`,
    });
    check('live open fails on wrong marker', await (async () => {
      try {
        await openExactNativeSessionLive(liveClientWrong.client, wrongWritten, { timeoutMs: 1000 });
        return false;
      } catch (err) {
        return err instanceof P0ContractError
          && err.contractId === 'native-session'
          && /omitted marker/.test(err.message);
      }
    })());
  } finally {
    await rm(wrongRoot, { recursive: true, force: true });
  }

  // Cold-cut negatives for the live native open: the authoritative cut must
  // be reason cold-full and carry a messages array, independent of the
  // marker contract. Any other reason or a missing messages array fails the
  // open fail-closed.
  const reasonClient = fakeClientWithPool({ subscribeReason: 'cold-refresh', nativeMarker: 'p0-native-marker' });
  await reasonClient.client.ready;
  reasonClient.sessions.push({
    sessionId: 'cold-reason',
    path: '<workspace>/cold-reason.jsonl',
    loaded: true,
    runtimeIncarnation: 'inc-cold-reason',
  });
  check('non cold-full cut reason rejected', await (async () => {
    try {
      await openExactNativeSessionLive(reasonClient.client, {
        path: '<workspace>/cold-reason.jsonl',
        sessionId: 'cold-reason',
        marker: 'p0-native-marker',
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError
        && err.contractId === 'native-session'
        && /cold-full/.test(err.message);
    }
  })());
  const messagesClient = fakeClientWithPool({ subscribeMessages: null, nativeMarker: 'p0-native-marker' });
  await messagesClient.client.ready;
  messagesClient.sessions.push({
    sessionId: 'cold-messages',
    path: '<workspace>/cold-messages.jsonl',
    loaded: true,
    runtimeIncarnation: 'inc-cold-messages',
  });
  check('cut without messages array rejected', await (async () => {
    try {
      await openExactNativeSessionLive(messagesClient.client, {
        path: '<workspace>/cold-messages.jsonl',
        sessionId: 'cold-messages',
        marker: 'p0-native-marker',
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError
        && err.contractId === 'native-session'
        && /omitted messages/.test(err.message);
    }
  })());

  const latencyClient = fakeClientWithPool();
  await latencyClient.client.ready;
  const latency = await measureAuthoritativeInteractiveLatency({
    client: latencyClient.client,
    now: (() => {
      let t = 50;
      return () => (t += 7);
    })(),
    action: async () => parseHandoff({
      sessionId: 'primary',
      runtimeIncarnation: 'inc-primary',
      subscriptionRequired: true,
      lifecycleResponseVersion: 1,
      cursor: 2,
    }),
  });
  check('authoritative latency is finite', Number.isFinite(latency.latencyMs) && latency.latencyMs >= 0);
  check('authoritative cut type recorded', latency.cutType === 'subscription_snapshot');

  const dualA = fakeClientWithPool();
  const dualB = fakeClientWithPool();
  dualB.sessions.push({
    sessionId: 'sess-1',
    path: '<workspace>/sessions/sess-1.jsonl',
    loaded: true,
    runtimeIncarnation: 'inc-sess-1',
  });
  await dualA.client.ready;
  await dualB.client.ready;
  const dual = await runDualClientBackgroundStream({
    clientA: dualA.client,
    clientB: dualB.client,
    runtimeA: { sessionId: 'primary', runtimeIncarnation: 'inc-primary' },
    runtimeB: { sessionId: 'sess-1', runtimeIncarnation: 'inc-sess-1', path: '<workspace>/sessions/sess-1.jsonl' },
  });
  check('dual runtimes are distinct', dual.distinctRuntimes === true
    && dual.clientAIdentity.sessionId === 'primary'
    && dual.clientAIdentity.runtimeIncarnation === 'inc-primary'
    && dual.clientBIdentity.sessionId === 'sess-1'
    && dual.clientBIdentity.runtimeIncarnation === 'inc-sess-1'
    && dual.clientAIdentity.sessionId !== dual.clientBIdentity.sessionId);
  check('background stream observed', dual.streamObserved === true);
  check('dual owner identity exact', dual.ownerIdentity.sessionId === 'primary' && dual.ownerIdentity.runtimeIncarnation === 'inc-primary');
  check('background stream not delivered to B', dual.streamNotDeliveredToB === true);
  // Delayed B leak: the owner's stream frame arrives on client B inside the
  // isolation window, so assertNoOwnerStreamToPeer must reject (fail-closed)
  // instead of reporting isolation.
  const leakA = fakeClientWithPool();
  const leakB = fakeClientWithPool();
  await leakA.client.ready;
  await leakB.client.ready;
  const owner = { sessionId: 'primary', runtimeIncarnation: 'inc-primary' };
  check('delayed leak within isolation window fails', await (async () => {
    const bIndex = leakB.client.frames.length;
    const timer = setTimeout(() => {
      leakB.socket.emit('message', JSON.stringify({
        type: 'message_delta',
        sessionId: 'primary',
        runtimeIncarnation: 'inc-primary',
        textContent: 'slow-a-done',
      }));
    }, 10);
    try {
      await assertNoOwnerStreamToPeer(leakB.client, owner, 'slow-a-done', {
        sinceIndex: bIndex,
        windowMs: 250,
      });
      clearTimeout(timer);
      return false;
    } catch (err) {
      clearTimeout(timer);
      return err instanceof P0ContractError
        && err.contractId === 'dual-client'
        && /observed owner A stream/.test(err.message);
    }
  })());
  check('pre-existing leak before window fails immediately', await (async () => {
    leakB.socket.emit('message', JSON.stringify({
      type: 'message_delta',
      sessionId: 'primary',
      runtimeIncarnation: 'inc-primary',
      textContent: 'slow-a-done',
    }));
    const owner = { sessionId: 'primary', runtimeIncarnation: 'inc-primary' };
    const bIndex = leakB.client.frames.length - 1;
    try {
      await assertNoOwnerStreamToPeer(leakB.client, owner, 'slow-a-done', {
        sinceIndex: bIndex,
        windowMs: 250,
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError
        && err.contractId === 'dual-client'
        && /observed owner A stream/.test(err.message);
    }
  })());

  const procFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  let clock = 1000;
  const rss = await sampleRssPlateauAndRelease({
    pid: 4242,
    fsp: procFs,
    now: () => clock,
    delay: async (ms) => {
      clock += ms;
    },
    samples: 5,
    intervalMs: 10,
    disconnect: async () => {
      procFs.readFile = async (path) => {
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   1024 kB\n';
        if (path.includes('/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      };
    },
  });
  check('plateau detected on flat samples', rss.plateau.detected === true);
  check('disconnect RSS released', rss.release.released === true);
  check('disconnect delta negative', rss.release.rssDeltaBytes < 0);
  check('rss flat slope zero', rss.slope.slopeBytesPerMs === 0);

  // RSS sequence contract: a plateau that turns the 5-sample window flat only
  // at sample 6 must stop early with a count >= minSamples and < maxSamples;
  // the same sampling run must still measure a clean disconnect release.
  let seqStatusReads = 0;
  const procFsSeq = {
    async readFile(path) {
      if (path.endsWith('/status')) {
        seqStatusReads += 1;
        const kb = seqStatusReads === 1 ? 32 * 1024
          : seqStatusReads === 2 ? 0
            : seqStatusReads <= 7 ? 16 * 1024
              : 4 * 1024;
        return `Name:\trpi\nVmRSS:\t   ${kb} kB\n`;
      }
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  const maxSamplesSeq = 10;
  const rssSeq = await sampleRssPlateauAndRelease({
    pid: 4242,
    fsp: procFsSeq,
    now: () => 2500,
    delay: async () => {},
    intervalMs: 10,
    minSamples: 5,
    maxSamples: maxSamplesSeq,
    disconnect: async () => {},
  });
  check('rss plateau sequence stops early at sample 6', rssSeq.slope.stoppedEarly === true && rssSeq.slope.sampleCountTaken === 6);
  check('rss plateau count within [minSamples, maxSamples)', rssSeq.slope.sampleCountTaken >= 5 && rssSeq.slope.sampleCountTaken < maxSamplesSeq, `taken=${rssSeq.slope.sampleCountTaken}`);
  check('rss sequence plateau cut detected and released', rssSeq.plateau.detected === true && rssSeq.release.released === true);

  // RSS plateau early stop at min-sample count: a plateau already flat by the
  // first 5 samples must stop exactly at minSamples, never sampling more.
  let flatEarlyReads = 0;
  const procFsFlatEarly = {
    async readFile(path) {
      if (path.endsWith('/status')) {
        flatEarlyReads += 1;
        return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      }
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('rss flat plateau stops at min samples', await (async () => {
    const flatEarly = await sampleRssUntilPlateau({
      pid: 4242,
      fsp: procFsFlatEarly,
      now: () => 1000,
      delay: async () => {},
      intervalMs: 10,
      minSamples: 5,
      maxSamples: 10,
    });
    return flatEarly.stoppedEarly === true
      && flatEarly.sampleCountTaken === 5
      && flatEarly.plateau.detected === true
      && flatEarlyReads === 5;
  })());

  const slope = await sampleRssSlope([
    { tMs: 0, rssBytes: 100 },
    { tMs: 10, rssBytes: 100 },
    { tMs: 20, rssBytes: 100 },
    { tMs: 30, rssBytes: 100 },
    { tMs: 40, rssBytes: 100 },
  ]);
  check('zero slope plateau', slope.slopeBytesPerMs === 0 && slope.plateau.detected === true);
  const release = disconnectReleaseMeasures({ rssBytes: 50 }, { rssBytes: 10 });
  check('release helper fail-closed on growth', disconnectReleaseMeasures({ rssBytes: 10 }, { rssBytes: 50 }).released === false);
  check('release helper true on drop', release.released === true);
  // connectionFdSocketCount — per-process socket fd table (the
  // connection-owned signal, not the netns tcp table). Fake readdir/readlink
  // over /proc/<pid>/fd: socket:[ino] targets count, plain files and pipes
  // do not. Fail-closed: an unreadable fd table (EACCES readdir, missing
  // readdir/readlink on the fake fs) and an fd that vanished between readdir
  // and readlink (raced close mid-scan) both return null, so a release gate
  // can never read a phantom 0 or a stale partial sum.
  const fdFs = {
    async readdir(path) {
      if (path === '/proc/4242/fd') return ['0', '1', '2', '3', '4', '5'];
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readlink(path) {
      const targets = {
        '/proc/4242/fd/0': '/dev/null',
        '/proc/4242/fd/1': 'pipe:[123]',
        '/proc/4242/fd/2': 'socket:[456]',
        '/proc/4242/fd/3': 'socket:[789]',
        '/proc/4242/fd/4': '/home/u/work/sessions/x.jsonl',
        '/proc/4242/fd/5': 'anon_inode:[eventpoll]',
      };
      if (targets[path] !== undefined) return targets[path];
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('connectionFdSocketCount counts socket fds only', await connectionFdSocketCount(4242, { fsp: fdFs }) === 2);
  check('connectionFdSocketCount null when fd races close after readdir', await (async () => {
    const vanishFs = {
      async readdir() {
        return ['0', '1', '2'];
      },
      async readlink(path) {
        if (path.endsWith('/0')) return 'socket:[1]';
        if (path.endsWith('/1')) return 'socket:[2]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' }); // fd 2 closed mid-scan
      },
    };
    return (await connectionFdSocketCount(4242, { fsp: vanishFs })) === null;
  })());
  check('connectionFdSocketCount null on EACCES fd table', await (async () => {
    const deniedFs = {
      async readdir() {
        throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
      },
      async readlink() {
        return 'socket:[1]';
      },
    };
    return (await connectionFdSocketCount(4242, { fsp: deniedFs })) === null;
  })());
  check('connectionFdSocketCount null without readdir/readlink', await (async () => {
    const bareFs = { async readFile() { return ''; } };
    return (await connectionFdSocketCount(4242, { fsp: bareFs })) === null;
  })());
  check('connectionFdSocketCount null on non-numeric pid', await connectionFdSocketCount('nope', { fsp: fdFs }) === null);

  // ---- Connection-owned pressure contract (fd signal, listener fallback,
  // ---- neither) ----------------------------------------------------------
  // listenerEstablishedConnectionCount — established rows on the listener's
  // local port only (hex port match, st == 01); null when neither net/tcp
  // nor net/tcp6 is readable.
  const listenerTcpFs = {
    async readFile(path) {
      if (path === '/proc/4242/net/tcp') {
        return [
          '  sl local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt',
          '   0: 0100007F:1F90 0100007F:ABC0 01 00000000:00000000 00:00000000 00000000     0        0 1',
          '   1: 0100007F:0035 0100007F:ABC1 01 00000000:00000000 00:00000000 00000000     0        0 2',
        ].join('\n');
      }
      if (path === '/proc/4242/net/tcp6') throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('listenerEstablishedConnectionCount counts listener-port rows only', await listenerEstablishedConnectionCount(4242, 8080, { fsp: listenerTcpFs }) === 1);
  check('listenerEstablishedConnectionCount null when net tables unreadable', await listenerEstablishedConnectionCount(4242, 8080, { fsp: { async readFile() { throw Object.assign(new Error('EACCES'), { code: 'EACCES' }); } } }) === null);
  check('listenerEstablishedConnectionCount null on bad inputs', await listenerEstablishedConnectionCount(4242, 'nope', { fsp: listenerTcpFs }) === null
    && await listenerEstablishedConnectionCount('x', 8080, { fsp: listenerTcpFs }) === null);

  // installConnectionOwnedPressure — fd mode positive: the paused subscribe
  // raises the per-process socket fd count by exactly one (+1) and the RSS
  // by the pressure magnitude.
  let fdModeFds = ['0', '1', '2'];
  let fdModeRssKb = 4096;
  const fdModeFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${fdModeRssKb} kB\n`;
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readdir(path) {
      if (path === '/proc/4242/fd') return [...fdModeFds];
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readlink(path) {
      if (path === '/proc/4242/fd/1') return '/dev/null';
      if (path.startsWith('/proc/4242/fd/')) return 'socket:[7]';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  const fdModePressure = {
    async subscribePaused() {
      fdModeFds = ['0', '1', '2', '3'];
      fdModeRssKb = 4096 + 2048; // +2 MiB >= the 1 MiB pressure floor
    },
    async destroy() {
      fdModeFds = ['0', '1', '2'];
      fdModeRssKb = 4096;
    },
    async pause() {},
  };
  const fdInstalled = await installConnectionOwnedPressure({
    pid: 4242,
    pressureClient: fdModePressure,
    identity: { sessionId: 'p0-pressure-2mib', runtimeIncarnation: 'inc-pressure' },
    fsp: fdModeFs,
    now: () => 5000,
    delay: async () => {},
    maxSamples: 4,
    intervalMs: 1,
  });
  check('install observes fd signal +1 and RSS rise', fdInstalled.installed === true
    && fdInstalled.signalMode === 'fd'
    && fdInstalled.fd0 === 2 && fdInstalled.fd1 === 3
    && fdInstalled.h1 - fdInstalled.h0 === 2 * 1024 * 1024
    && fdInstalled.installedBytes === 2 * 1024 * 1024
    && fdInstalled.listenerPort === null
    && fdInstalled.conn0 === null && fdInstalled.conn1 === null);

  // installConnectionOwnedPressure — listener fallback: the fd table is
  // unreadable (EACCES) but the per-pid netns net/tcp exposes the listener
  // local port; the paused subscribe raises the established count on that
  // port by one (+1) and the RSS by the pressure magnitude.
  let listenerConns = 1;
  let listenerRssKb = 4096;
  const netTcpText = (conns) => [
    '  sl local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt',
    ...Array.from({ length: conns }, (_, i) => (
      `   ${i}: 0100007F:1F90 0100007F:AB${String(i).padStart(2, '0')} 01 00000000:00000000 00:00000000 00000000     0        0 ${i}`
    )),
  ].join('\n');
  const listenerFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${listenerRssKb} kB\n`;
      if (path.includes('/children')) return '';
      if (path.endsWith('/net/tcp')) return netTcpText(listenerConns);
      if (path.endsWith('/net/tcp6')) throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readdir() {
      throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
    },
    async readlink() {
      return 'socket:[1]';
    },
  };
  const listenerPressure = {
    async subscribePaused() {
      listenerConns = 2;
      listenerRssKb = 4096 + 2048;
    },
    async destroy() {
      listenerConns = 1;
      listenerRssKb = 4096;
    },
    async pause() {},
  };
  const listenerInstalled = await installConnectionOwnedPressure({
    pid: 4242,
    pressureClient: listenerPressure,
    identity: { sessionId: 'p0-pressure-2mib', runtimeIncarnation: 'inc-pressure' },
    fsp: listenerFs,
    now: () => 6000,
    delay: async () => {},
    maxSamples: 5,
    intervalMs: 1,
    listenerPort: 8080,
  });
  check('install falls back to listener-local-port signal +1', listenerInstalled.installed === true
    && listenerInstalled.signalMode === 'listener'
    && listenerInstalled.fd0 === null && listenerInstalled.fd1 === null
    && listenerInstalled.conn0 === 1 && listenerInstalled.conn1 === 2
    && listenerInstalled.listenerPort === 8080
    && listenerInstalled.h1 - listenerInstalled.h0 === 2 * 1024 * 1024);

  // installConnectionOwnedPressure — neither signal exists (fd unreadable
  // AND net/tcp unreadable, with and without a listenerPort): fail-closed.
  const neitherFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
    },
    async readdir() {
      throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
    },
    async readlink() {
      return 'socket:[1]';
    },
  };
  const pressureIdentity = { sessionId: 'p0-pressure-2mib', runtimeIncarnation: 'inc-pressure' };
  check('install fails closed when neither fd nor listener signal exists', await (async () => {
    try {
      await installConnectionOwnedPressure({
        pid: 4242,
        pressureClient: { async subscribePaused() {}, async pause() {} },
        identity: pressureIdentity,
        fsp: neitherFs,
        now: () => 7000,
        delay: async () => {},
        listenerPort: 8080,
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && err.contractId === 'pressure'
        && /connection signal unavailable \(fail-closed\)/.test(err.message)
        && /listenerPort=8080/.test(err.message);
    }
  })());
  check('install fails closed on unreadable fd table without listenerPort', await (async () => {
    try {
      await installConnectionOwnedPressure({
        pid: 4242,
        pressureClient: { async subscribePaused() {}, async pause() {} },
        identity: pressureIdentity,
        fsp: neitherFs,
        now: () => 7500,
        delay: async () => {},
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && err.contractId === 'pressure'
        && /connection signal unavailable \(fail-closed\)/.test(err.message);
    }
  })());
  check('install rejects a non-integer listenerPort fail-closed', await (async () => {
    try {
      await installConnectionOwnedPressure({
        pid: 4242,
        pressureClient: { async subscribePaused() {}, async pause() {} },
        identity: pressureIdentity,
        fsp: fdModeFs,
        now: () => 7600,
        delay: async () => {},
        listenerPort: 'not-a-port',
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /listenerPort must be a positive integer port/.test(err.message);
    }
  })());

  // releaseConnectionOwnedPressure — fd mode: the causal destroy drops the
  // fd count below fd1 and the RSS by the installed magnitude; every sample
  // keeps the listener probe as null in fd mode.
  const fdReleased = await releaseConnectionOwnedPressure({
    pid: 4242,
    h1: fdInstalled.h1,
    installedBytes: fdInstalled.installedBytes,
    fd1: fdInstalled.fd1,
    processCount1: fdInstalled.processCount,
    pressureClient: fdModePressure,
    fsp: fdModeFs,
    now: () => 8000,
    delay: async () => {},
    intervalMs: 1,
    maxSamples: 6,
    confirmSamples: 2,
  });
  check('release gates on fd decrease post-destroy', fdReleased.released === true
    && fdReleased.fd1 === 3
    && fdReleased.samples.every((s) => s.fdSocketCount < 3)
    && fdReleased.samples.every((s) => s.listenerEstablished === null)
    && fdReleased.magnitudeBytes === 2 * 1024 * 1024);

  // releaseConnectionOwnedPressure — listener fallback: the causal destroy
  // drops the established count on the listener port below conn1 and the
  // RSS by the installed magnitude.
  const listenerReleased = await releaseConnectionOwnedPressure({
    pid: 4242,
    h1: listenerInstalled.h1,
    installedBytes: listenerInstalled.installedBytes,
    listenerPort: 8080,
    conn1: listenerInstalled.conn1,
    processCount1: listenerInstalled.processCount,
    pressureClient: listenerPressure,
    fsp: listenerFs,
    now: () => 9000,
    delay: async () => {},
    intervalMs: 1,
    maxSamples: 6,
    confirmSamples: 2,
  });
  check('release gates on listener established decrease post-destroy', listenerReleased.released === true
    && listenerReleased.conn1 === 2
    && listenerReleased.listenerPort === 8080
    && listenerReleased.samples.every((s) => s.listenerEstablished < 2)
    && listenerReleased.magnitudeBytes === 2 * 1024 * 1024);

  // releaseConnectionOwnedPressure — listener hold negative: the connection
  // count never drops below conn1 (RSS releases), so the window fails
  // closed with reason listener-no-drop and the full sample series.
  let holdRssKb = 6144;
  const holdConns = 2; // never drops below conn1=2 despite the RSS release
  const holdFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${holdRssKb} kB\n`;
      if (path.includes('/children')) return '';
      if (path.endsWith('/net/tcp')) return netTcpText(holdConns);
      if (path.endsWith('/net/tcp6')) throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readdir() {
      throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
    },
    async readlink() {
      return 'socket:[1]';
    },
  };
  check('release fails closed when listener count never drops', await (async () => {
    try {
      await releaseConnectionOwnedPressure({
        pid: 4242,
        h1: 6144 * 1024,
        installedBytes: 2 * 1024 * 1024,
        listenerPort: 8080,
        conn1: 2,
        processCount1: 1,
        pressureClient: { async destroy() { holdRssKb = 4096; } },
        fsp: holdFs,
        now: () => 10000,
        delay: async () => {},
        intervalMs: 1,
        maxSamples: 4,
        confirmSamples: 2,
      });
      return false;
    } catch (err) {
      const samples = err && err.rssEvidence && err.rssEvidence.release && err.rssEvidence.release.samples;
      return err instanceof P0ContractError
        && err.rssEvidence.release.reason === 'listener-no-drop'
        && /listener-no-drop/.test(err.message)
        && Array.isArray(samples) && samples.length === 4
        && samples.every((s) => s.listenerEstablished === 2)
        && err.rssEvidence.release.conn1 === 2
        && err.rssEvidence.release.listenerPort === 8080;
    }
  })());

  // releaseConnectionOwnedPressure — neither signal: fail-closed before the
  // destroy hook runs.
  check('release fails closed without fd or listener signal', await (async () => {
    let destroyed = false;
    try {
      await releaseConnectionOwnedPressure({
        pid: 4242,
        h1: 6144 * 1024,
        installedBytes: 2 * 1024 * 1024,
        processCount1: 1,
        pressureClient: { async destroy() { destroyed = true; } },
        fsp: neitherFs,
        now: () => 10500,
        delay: async () => {},
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && err.contractId === 'rss'
        && /connection release signal missing \(fail-closed\)/.test(err.message)
        && destroyed === false;
    }
  })());

  // Causal ordering — with installed pressure the release window is sampled
  // while the unrelated disconnect (control clients / browser) is still
  // pending; the disconnect runs only after the verdict.
  check('pressure release sampled before the unrelated disconnect', await (async () => {
    let posRssKb = 4096;
    let posFds = ['0', '1', '2'];
    let posDisconnects = 0;
    const posFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${posRssKb} kB\n`;
        if (path.includes('/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd') return [...posFds];
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path === '/proc/4242/fd/0') return '/dev/null';
        if (path.startsWith('/proc/4242/fd/')) return 'socket:[21]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    const causal = await sampleRssPlateauAndRelease({
      pid: 4242,
      fsp: posFs,
      now: () => 13000,
      delay: async () => {},
      samples: 4,
      intervalMs: 10,
      pressure: {
        pressureClient: {
          async subscribePaused() {
            posFds = ['0', '1', '2', '3'];
            posRssKb = 6144; // +2 MiB installed
          },
          async pause() {},
          async destroy() {
            posFds = ['0', '1', '2']; // pressure socket gone
            posRssKb = 4096; // RSS released by the pressure destroy alone
          },
        },
        identity: pressureIdentity,
      },
      release: { maxSamples: 4, confirmSamples: 2, intervalMs: 1 },
      disconnect: async () => {
        posDisconnects += 1; // unrelated close, post-verdict only
      },
    });
    return posDisconnects === 1
      && causal.release.released === true
      && causal.release.rssDeltaBytes === -2 * 1024 * 1024
      && causal.release.samples.every((s) => s.fdSocketCount === 2)
      && causal.pressure.installed === true;
  })());

  // Causal ordering negative: destroying ONLY the pressure socket retains
  // the installed RSS (the still-connected unrelated clients pin it), so
  // the window must fail closed — even though the unrelated disconnect
  // after the verdict would release enough. The post-verdict disconnect
  // must never leak into the verdict.
  check('pressure release fails closed before the unrelated disconnect', await (async () => {
    let negRssKb = 4096;
    let negFds = ['0', '1', '2'];
    let negDisconnects = 0;
    const negFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${negRssKb} kB\n`;
        if (path.includes('/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd') return [...negFds];
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path === '/proc/4242/fd/0') return '/dev/null';
        if (path.startsWith('/proc/4242/fd/')) return 'socket:[22]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    try {
      await sampleRssPlateauAndRelease({
        pid: 4242,
        fsp: negFs,
        now: () => 14000,
        delay: async () => {},
        samples: 4,
        intervalMs: 10,
        pressure: {
          pressureClient: {
            async subscribePaused() {
              negFds = ['0', '1', '2', '3'];
              negRssKb = 6144;
            },
            async pause() {},
            async destroy() {
              negFds = ['0', '1', '2']; // socket destroyed, RSS retained
            },
          },
          identity: pressureIdentity,
        },
        release: { maxSamples: 4, confirmSamples: 2, intervalMs: 1 },
        disconnect: async () => {
          negDisconnects += 1;
          negRssKb = 4096; // would release enough — but only post-verdict
        },
      });
      return false;
    } catch (err) {
      const samples = err && err.rssEvidence && err.rssEvidence.release && err.rssEvidence.release.samples;
      const failedBeforeDisconnect = negDisconnects === 0
        && err instanceof P0ContractError
        && err.rssEvidence.release.released === false
        && err.rssEvidence.release.reason === 'delta0'
        && Array.isArray(samples) && samples.length === 4
        && samples.every((s) => s.rssBytes === 6144 * 1024)
        && samples.every((s) => s.fdSocketCount === 2);
      // Post-verdict, the same unrelated disconnect does release enough,
      // proving the fixture would have released had it run in-window.
      negDisconnects += 1;
      negRssKb = 4096;
      const after = await sampleRssUntilRelease({
        pid: 4242,
        baselineBytes: 6144 * 1024,
        minReleaseBytes: 2 * 1024 * 1024,
        fsp: negFs,
        now: () => 14500,
        delay: async () => {},
        intervalMs: 1,
        maxSamples: 4,
        confirmSamples: 2,
      });
      return failedBeforeDisconnect
        && negDisconnects === 1
        && after.released === true
        && after.magnitudeBytes === 2 * 1024 * 1024;
    }
  })());

  // runConnectionPressureRelease — fixed driver sequence: plateau →
  // installConnectionOwnedPressure → fail-closed installedBytes floor
  // (PRESSURE_MIN_BYTES) → destroyPressureClient(installed.pressureClient)
  // ONLY → sampleRssUntilRelease gated on installedBytes + fd/listener
  // signal drop + unchanged processCount → disconnect({pressure, release})
  // only on a retained verdict. Shared fd-mode fake proc-fs harness: the
  // paused subscribe raises the socket fd count +1 (fd0=2 → fd1=3) and the
  // RSS by `riseKb`; destroy optionally drops the fd and the RSS by
  // `dropKb`. Every phase transition is recorded on one ordered event log
  // ('subscribe', 'sample:<rssKb>' per /proc status read, 'destroy',
  // 'disconnect') so the causal ordering is assertable.
  const makeCausalHarness = ({ riseKb, dropKb, destroyDropsFd }) => {
    const state = { rssKb: 4096, fds: ['0', '1', '2'] };
    const events = [];
    let disconnectArgs = null;
    return {
      events,
      disconnectArgs: () => disconnectArgs,
      fsp: {
        async readFile(path) {
          if (path.endsWith('/status')) {
            events.push(`sample:${state.rssKb}`);
            return `Name:\trpi\nVmRSS:\t   ${state.rssKb} kB\n`;
          }
          if (path.includes('/children')) return '';
          throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
        },
        async readdir(path) {
          if (path === '/proc/4242/fd') return [...state.fds];
          throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
        },
        async readlink(path) {
          if (path === '/proc/4242/fd/1') return '/dev/null';
          if (path.startsWith('/proc/4242/fd/')) return 'socket:[31]';
          throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
        },
      },
      pressureClient: {
        async subscribePaused() {
          events.push('subscribe');
          state.fds = [...state.fds, '3'];
          state.rssKb += riseKb;
        },
        async pause() {},
        async destroy() {
          events.push('destroy');
          if (destroyDropsFd) state.fds = state.fds.slice(0, -1);
          state.rssKb -= dropKb;
        },
      },
      disconnect: async (args) => {
        events.push('disconnect');
        disconnectArgs = args;
      },
    };
  };
  const causalRun = (harness, { release = {}, install = {} } = {}) => runConnectionPressureRelease({
    pid: 4242,
    fsp: harness.fsp,
    now: () => 20000,
    delay: async () => {},
    plateau: { plateau: { detected: true }, slopeBytesPerMs: 0 },
    install: {
      pressureClient: harness.pressureClient,
      identity: pressureIdentity,
      maxSamples: 4,
      intervalMs: 1,
      ...install,
    },
    disconnect: harness.disconnect,
    release: { intervalMs: 1, maxSamples: 4, confirmSamples: 2, ...release },
  });

  // Positive fd mode: the pressure destroy alone releases the installed
  // magnitude; destroy precedes every release sample and the unrelated
  // disconnect runs exactly once, after the last sample, with the verdict.
  check('runConnectionPressureRelease orders destroy < samples < disconnect', await (async () => {
    const harness = makeCausalHarness({ riseKb: 2048, dropKb: 2048, destroyDropsFd: true });
    const result = await causalRun(harness);
    const destroyIdx = harness.events.indexOf('destroy');
    const disconnectIdx = harness.events.indexOf('disconnect');
    const subscribeIdx = harness.events.indexOf('subscribe');
    const sampleIdxs = harness.events
      .map((e, i) => [e, i])
      .filter(([e]) => e.startsWith('sample:'));
    const installSamples = sampleIdxs.filter(([, i]) => i > subscribeIdx && i < destroyIdx);
    const releaseSamples = sampleIdxs.filter(([, i]) => i > destroyIdx);
    return result.release.released === true
      && result.pressure.installed === true
      && result.pressure.installedBytes >= PRESSURE_MIN_BYTES
      && result.release.magnitudeBytes >= result.pressure.installedBytes
      && result.release.magnitudeBytes === 2 * 1024 * 1024
      && harness.events.filter((e) => e === 'destroy').length === 1
      && harness.events.filter((e) => e === 'disconnect').length === 1
      && installSamples.length >= 1
      && installSamples.every(([e]) => e === 'sample:6144') // installed level pre-destroy
      && releaseSamples.length >= 2 // confirmSamples consecutive
      && releaseSamples.every(([e]) => e === 'sample:4096') // released level post-destroy
      && releaseSamples.every(([, i]) => i < disconnectIdx) // disconnect after last sample
      && disconnectIdx === harness.events.length - 1 // nothing runs post-verdict
      && harness.disconnectArgs().pressure.installed === true
      && harness.disconnectArgs().release.released === true;
  })());

  // Small-drop negative: the destroy drops the RSS by less than the
  // installed magnitude — fail closed with reason small-drop; destroy ran
  // once, disconnect never.
  check('runConnectionPressureRelease fails closed on small drop', await (async () => {
    const harness = makeCausalHarness({ riseKb: 2048, dropKb: 1024, destroyDropsFd: true });
    try {
      await causalRun(harness);
      return false;
    } catch (err) {
      const evidence = err && err.rssEvidence;
      return err instanceof P0ContractError
        && err.contractId === 'rss'
        && evidence.release.reason === 'small-drop'
        && evidence.plateau.detected === true
        && evidence.slopeBytesPerMs === 0
        && evidence.pressure.h0 === 4096 * 1024
        && evidence.pressure.h1 === 6144 * 1024
        && evidence.pressure.installedBytes === 2 * 1024 * 1024
        && evidence.pressure.signalMode === 'fd'
        && evidence.pressure.fd0 === 2
        && evidence.pressure.fd1 === 3
        && evidence.pressure.processCount === 1
        && harness.events.filter((e) => e === 'destroy').length === 1
        && harness.events.every((e) => e !== 'disconnect');
    }
  })());

  // Sub-minimum install: a 64 KiB rise installs (above its own minBytes)
  // but violates the PRESSURE_MIN_BYTES floor — P0ContractError before any
  // destroy or sample of the release window.
  check('runConnectionPressureRelease rejects sub-minimum installedBytes', await (async () => {
    const harness = makeCausalHarness({ riseKb: 64, dropKb: 2048, destroyDropsFd: true });
    try {
      await causalRun(harness, { install: { minBytes: 64 * 1024 } });
      return false;
    } catch (err) {
      return err instanceof P0ContractError
        && err.contractId === 'rss'
        && /fail-closed/.test(err.message)
        && err.message.includes(`>= ${PRESSURE_MIN_BYTES}`)
        && harness.events.every((e) => e !== 'destroy')
        && harness.events.every((e) => e !== 'disconnect');
    }
  })());

  // fd-no-drop negative: the destroy releases the full RSS magnitude but
  // never removes the connection-owned fd, so the connection gate holds the
  // verdict closed — the RSS drop alone can never bless a release.
  check('runConnectionPressureRelease fails closed when fd never drops', await (async () => {
    const harness = makeCausalHarness({ riseKb: 2048, dropKb: 2048, destroyDropsFd: false });
    try {
      await causalRun(harness);
      return false;
    } catch (err) {
      const evidence = err && err.rssEvidence;
      return err instanceof P0ContractError
        && err.contractId === 'rss'
        && evidence.release.reason === 'fd-no-drop'
        && evidence.pressure.signalMode === 'fd'
        && evidence.pressure.fd1 === 3
        && evidence.pressure.installedBytes === 2 * 1024 * 1024
        && evidence.release.samples.every((s) => s.fdSocketCount === 3)
        && harness.events.filter((e) => e === 'destroy').length === 1
        && harness.events.every((e) => e !== 'disconnect');
    }
  })());

  // sampleRssUntilRelease — a lone half of the listener pair is a caller
  // error and fails closed before any sampling.
  check('release rejects listenerPort without a conn baseline', await (async () => {
    try {
      await sampleRssUntilRelease({
        pid: 4242,
        baselineBytes: 4096 * 1024,
        fsp: listenerFs,
        now: () => 1,
        delay: async () => {},
        listenerPort: 8080,
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /listener connection baseline missing/.test(err.message);
    }
  })());
  check('release rejects conn baseline without listenerPort', await (async () => {
    try {
      await sampleRssUntilRelease({
        pid: 4242,
        baselineBytes: 4096 * 1024,
        fsp: listenerFs,
        now: () => 1,
        delay: async () => {},
        baselineListenerEstablishedCount: 2,
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /listenerPort missing for listener connection gate/.test(err.message);
    }
  })());

  // createNodePressureClient — shared raw ws pressure client factory: the
  // auth subprotocol, paused raw socket, and causal terminate + close await.
  check('createNodePressureClient fails closed without WebSocketImpl', (() => {
    try {
      createNodePressureClient({ url: 'ws://127.0.0.1:9/ws' });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /WebSocketImpl/.test(err.message);
    }
  })());
  check('createNodePressureClient opens with auth subprotocol and pauses raw socket', await (async () => {
    let wsArgs = null;
    let pausedCount = 0;
    class FakeWS {
      constructor(u, protocols) {
        wsArgs = { u, protocols };
        this.events = {};
        this.sent = [];
        this.terminated = false;
        this._socket = { pause: () => { pausedCount += 1; } };
        // The factory registers once('open') synchronously right after
        // construction; a microtask fires it after that registration.
        queueMicrotask(() => { if (this.events.open) this.events.open(); });
      }
      once(ev, fn) { this.events[ev] = fn; }
      send(data) { this.sent.push(String(data)); }
      terminate() {
        this.terminated = true;
        if (this.events.close) this.events.close(1000, 'p0-destroy');
      }
    }
    const pc = createNodePressureClient({ url: 'ws://127.0.0.1:9/ws', token: 'tok', WebSocketImpl: FakeWS });
    await pc.ready;
    await pc.send({ type: 'subscribe', sessionId: 'p0-pressure-2mib' });
    await pc.pause();
    const closed = await pc.destroy();
    return wsArgs.u === 'ws://127.0.0.1:9/ws'
      && JSON.stringify(wsArgs.protocols) === JSON.stringify(['rpi-auth.tok'])
      && pausedCount === 1
      && closed.code === 1000 && closed.reason === 'p0-destroy';
  })());

  // processTreeRssSnapshot — fdSocketCount sums the per-process socket fd
  // counts over the whole tree (root + descendants), carries each process's
  // own count in `processes[].fdSockets`, and fails closed to null when ANY
  // discovered process's fd table is unreadable (root EACCES, descendant
  // EACCES, or a descendant fd table that raced exit between the status read
  // and the fd walk), when a discovered descendant's status is unreadable
  // (raced exit before the status read — its fd table is unknowable), when
  // any listed fd readlink misses after a readable fd directory, or when the
  // walk is truncated. A partial sum can never bless a release; RSS and
  // processCount stay truthful over what was readable either way.
  const treeFdFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.endsWith('/task/4242/children')) return '5001 5002\n';
      if (path.endsWith('/task/5001/children')) return '5003\n';
      if (path.endsWith('/task/5002/children')) return '';
      if (path.endsWith('/task/5003/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readdir(path) {
      if (path === '/proc/4242/fd') return ['0', '1'];
      if (path === '/proc/5001/fd') return ['0', '1', '2'];
      if (path === '/proc/5002/fd') return ['0'];
      if (path === '/proc/5003/fd') return ['0', '1'];
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readlink(path) {
      if (path === '/proc/4242/fd/0') return 'socket:[1]';
      if (path === '/proc/4242/fd/1') return '/dev/null';
      if (path === '/proc/5001/fd/0') return 'socket:[2]';
      if (path === '/proc/5001/fd/1') return 'socket:[3]';
      if (path === '/proc/5001/fd/2') return 'pipe:[9]';
      if (path === '/proc/5002/fd/0') return 'socket:[4]';
      if (path === '/proc/5003/fd/0') return 'socket:[5]';
      if (path === '/proc/5003/fd/1') return 'socket:[6]';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  const treeFdSnapshot = await processTreeRssSnapshot(4242, { fsp: treeFdFs, now: () => 7 });
  check('snapshot fdSocketCount sums tree socket fds', treeFdSnapshot.fdSocketCount === 6
    && treeFdSnapshot.processCount === 4
    && treeFdSnapshot.rssBytes === 4 * 4096 * 1024);
  check('snapshot processes carry per-pid fdSockets', (() => {
    const byPid = Object.fromEntries(treeFdSnapshot.processes.map((p) => [p.pid, p.fdSockets]));
    return byPid[4242] === 1 && byPid[5001] === 2 && byPid[5002] === 1 && byPid[5003] === 2;
  })());
  const deniedTreeFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readdir() {
      throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
    },
    async readlink() {
      return 'socket:[1]';
    },
  };
  check('snapshot fdSocketCount null when root fd table unreadable', await (async () => {
    const denied = await processTreeRssSnapshot(4242, { fsp: deniedTreeFs, now: () => 8 });
    return denied.fdSocketCount === null && denied.rssBytes === 4096 * 1024;
  })());
  check('snapshot fdSocketCount null without readdir/readlink', await (async () => {
    const bareTreeFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
        if (path.includes('/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    const bare = await processTreeRssSnapshot(4242, { fsp: bareTreeFs, now: () => 9 });
    return bare.fdSocketCount === null && bare.rssBytes === 4096 * 1024;
  })());
  check('snapshot fdSocketCount null on unreadable descendant fd table', await (async () => {
    const partialFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
        if (path.endsWith('/task/4242/children')) return '5001\n';
        if (path.endsWith('/task/5001/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd') return ['0'];
        if (path === '/proc/5001/fd') throw Object.assign(new Error('EACCES'), { code: 'EACCES' });
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path === '/proc/4242/fd/0') return 'socket:[1]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    const partial = await processTreeRssSnapshot(4242, { fsp: partialFs, now: () => 10 });
    return partial.fdSocketCount === null
      && partial.processCount === 2
      && partial.rssBytes === 2 * 4096 * 1024
      && partial.processes.find((p) => p.pid === 5001).fdSockets === null;
  })());
  check('snapshot fdSocketCount null when descendant fd table races exit', await (async () => {
    const racingFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
        if (path.endsWith('/task/4242/children')) return '5001\n';
        if (path.endsWith('/task/5001/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd') return ['0'];
        if (path === '/proc/5001/fd') throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path === '/proc/4242/fd/0') return 'socket:[1]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    const raced = await processTreeRssSnapshot(4242, { fsp: racingFs, now: () => 13 });
    return raced.fdSocketCount === null
      && raced.processCount === 2
      && raced.rssBytes === 2 * 4096 * 1024
      && raced.processes.find((p) => p.pid === 5001).fdSockets === null;
  })());
  check('snapshot fdSocketCount null when discovered descendant status unreadable', await (async () => {
    const statusMissFs = {
      async readFile(path) {
        if (path === '/proc/5001/status') throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' }); // raced exit mid-walk
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
        if (path.endsWith('/task/4242/children')) return '5001\n';
        if (path.endsWith('/task/5001/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd') return ['0'];
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path === '/proc/4242/fd/0') return 'socket:[1]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    const missed = await processTreeRssSnapshot(4242, { fsp: statusMissFs, now: () => 14 });
    return missed.fdSocketCount === null
      && missed.unreadableCount === 1
      && missed.processCount === 2
      && missed.rssBytes === 4096 * 1024
      && missed.processes.find((p) => p.pid === 5001).fdSockets === null;
  })());
  check('snapshot fdSocketCount null on truncated walk', await (async () => {
    const truncFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
        if (path.endsWith('/task/4242/children')) return '5001 5002\n';
        if (path.endsWith('/task/5001/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd' || path === '/proc/5001/fd') return ['0'];
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path.endsWith('/fd/0')) return 'socket:[1]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
    };
    const trunc = await processTreeRssSnapshot(4242, { fsp: truncFs, now: () => 15, maxDescendants: 2 });
    return trunc.fdSocketCount === null
      && trunc.truncated === true
      && trunc.rssBytes === 2 * 4096 * 1024
      && trunc.processes.find((p) => p.pid === 5001).fdSockets === 1;
  })());
  check('snapshot fdSocketCount null when descendant fd readlink races close', await (async () => {
    const fdRaceFs = {
      async readFile(path) {
        if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
        if (path.endsWith('/task/4242/children')) return '5001\n';
        if (path.endsWith('/task/5001/children')) return '';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readdir(path) {
        if (path === '/proc/4242/fd' || path === '/proc/5001/fd') return ['0'];
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
      },
      async readlink(path) {
        if (path === '/proc/4242/fd/0') return 'socket:[1]';
        throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' }); // 5001 fd 0 closed mid-scan
      },
    };
    const raced = await processTreeRssSnapshot(4242, { fsp: fdRaceFs, now: () => 16 });
    return raced.fdSocketCount === null
      && raced.truncated === false
      && raced.processes.find((p) => p.pid === 4242).fdSockets === 1
      && raced.processes.find((p) => p.pid === 5001).fdSockets === null;
  })());

  // captureResourceSnapshot — the combined sample carries fdSocketCount from
  // the tree snapshot (null when any discovered process fd table or status
  // is unreadable, a listed fd readlink misses, or the walk is truncated).
  const captureFdFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readdir(path) {
      if (path === '/proc/4242/fd') return ['0', '1'];
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
    async readlink(path) {
      if (path === '/proc/4242/fd/0') return 'socket:[1]';
      if (path === '/proc/4242/fd/1') return 'socket:[2]';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  const captureFd = await captureResourceSnapshot({ pid: 4242, fsp: captureFdFs, now: () => 11 });
  check('capture snapshot carries fdSocketCount', captureFd.fdSocketCount === 2 && captureFd.rssBytes === 4096 * 1024);
  const captureDenied = await captureResourceSnapshot({ pid: 4242, fsp: deniedTreeFs, now: () => 12 });
  check('capture snapshot fdSocketCount null on unreadable root', captureDenied.fdSocketCount === null && captureDenied.rssBytes === 4096 * 1024);
  let growthKb = 1024;
  const procFsGrowth = {
    async readFile(path) {
      if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${(growthKb += 9 * 1024)} kB\n`;
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('plateau throws on growth samples', await (async () => {
    try {
      await sampleRssPlateauAndRelease({
        pid: 4242,
        fsp: procFsGrowth,
        now: () => 1000,
        delay: async () => {},
        samples: 4,
        intervalMs: 10,
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /plateau\.detected/.test(err.message);
    }
  })());
  // Sustained growth: the slope never flattens, so sampling must run the full
  // maxSamples without an early stop, and the release hard gate must reject
  // the run as a non-plateau instead of reporting a plateau.
  let growMaxKb = 1024;
  const procFsGrowMax = {
    async readFile(path) {
      if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${(growMaxKb += 9 * 1024)} kB\n`;
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  const growMaxSamples = 9;
  let growClock = 0;
  const grown = await sampleRssUntilPlateau({
    pid: 4242,
    fsp: procFsGrowMax,
    now: () => growClock,
    delay: async (ms) => { growClock += ms; },
    intervalMs: 7,
    minSamples: 5,
    maxSamples: growMaxSamples,
  });
  check('sustained growth runs full max samples without plateau', grown.stoppedEarly === false
    && grown.sampleCountTaken === growMaxSamples
    && grown.plateau.detected === false
    && grown.slopeBytesPerMs > 0);
  check('growth through max rejected by plateau hard gate', await (async () => {
    try {
      await sampleRssPlateauAndRelease({
        pid: 4242,
        fsp: procFsGrowMax,
        now: () => 4000,
        delay: async () => {},
        intervalMs: 7,
        minSamples: 5,
        maxSamples: growMaxSamples,
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /plateau\.detected/.test(err.message);
    }
  })());
  const procFsFlat = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('release failure is fail-closed', await (async () => {
    try {
      await sampleRssPlateauAndRelease({
        pid: 4242,
        fsp: procFsFlat,
        now: () => 1000,
        delay: async () => {},
        samples: 4,
        intervalMs: 10,
        disconnect: async () => {},
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError && /released/.test(err.message);
    }
  })());

  // sampleRssUntilRelease — early-release positive: the tree dips strictly
  // below the connected baseline on the second sample and never grows back;
  // the window stops early, measures the negative delta against the
  // baseline, and carries tree thread counts plus a null tcp probe (fake fs
  // has no /proc/<pid>/net/tcp).
  let earlyReads = 0;
  const earlyFs = {
    async readFile(path) {
      if (path.endsWith('/status')) {
        earlyReads += 1;
        const kb = earlyReads === 1 ? 4096 : 1024;
        return `Name:\trpi\nVmRSS:\t   ${kb} kB\nThreads:\t17\n`;
      }
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  const releasedEarly = await sampleRssUntilRelease({
    pid: 4242,
    baselineBytes: 4096 * 1024,
    fsp: earlyFs,
    now: () => 1234,
    delay: async () => {},
    intervalMs: 5,
    maxSamples: 12,
    confirmSamples: 3,
  });
  check('release window stops early on strictly negative delta', releasedEarly.released === true
    && releasedEarly.releasedAtSample === 1
    && releasedEarly.sampleCountTaken === 4
    && releasedEarly.stoppedEarly === true
    && releasedEarly.measures.rssDeltaBytes === 1024 * 1024 - 4096 * 1024
    && releasedEarly.measures.rssAfterBytes === 1024 * 1024
    && releasedEarly.baselineBytes === 4096 * 1024);
  check('release window samples carry tree thread count', releasedEarly.samples.length === 4
    && releasedEarly.samples.every((s) => s.threadCount === 17)
    && releasedEarly.samples.every((s) => s.processCount === 1)
    && releasedEarly.samples.every((s) => s.tcpSockets === null));

  // sampleRssUntilRelease — no-release-at-max negative: a flat tree never
  // dips below the baseline; the window must run the full maxSamples and
  // fail closed carrying the full sample series (delta 0 is a no-release).
  let noneReads = 0;
  const noneFs = {
    async readFile(path) {
      if (path.endsWith('/status')) {
        noneReads += 1;
        return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      }
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('release window fails closed at max with full series', await (async () => {
    try {
      await sampleRssUntilRelease({
        pid: 4242,
        baselineBytes: 4096 * 1024,
        fsp: noneFs,
        now: () => 999,
        delay: async () => {},
        intervalMs: 1,
        maxSamples: 6,
        confirmSamples: 2,
      });
      return false;
    } catch (err) {
      const samples = err && err.rssEvidence && err.rssEvidence.release && err.rssEvidence.release.samples;
      return err instanceof P0ContractError
        && err.contractId === 'rss'
        && /released=false over 6 samples/.test(err.message)
        && /deltaKiB=\[0,0,0,0,0,0\]/.test(err.message)
        && err.rssEvidence.release.released === false
        && err.rssEvidence.release.baselineBytes === 4096 * 1024
        && err.rssEvidence.release.sampling.maxSamples === 6
        && Array.isArray(samples)
        && samples.length === 6
        && samples.every((s) => s.rssBytes === 4096 * 1024);
    }
  })());
  check('no-release window ran full maxSamples', noneReads === 6);

  // sampleRssUntilRelease — transient-dip negative: a dip followed by growth
  // back above the connected baseline is revoked (not a release); sampling
  // continues to maxSamples and fails closed with the revoking series.
  const dipKbByRead = [4096, 1024, 8192];
  let dipReads = 0;
  const dipFs = {
    async readFile(path) {
      if (path.endsWith('/status')) {
        dipReads += 1;
        const kb = dipKbByRead[dipReads - 1] !== undefined ? dipKbByRead[dipReads - 1] : 4096;
        return `Name:\trpi\nVmRSS:\t   ${kb} kB\n`;
      }
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('release window revokes transient dip on growth above baseline', await (async () => {
    try {
      await sampleRssUntilRelease({
        pid: 4242,
        baselineBytes: 4096 * 1024,
        fsp: dipFs,
        now: () => 77,
        delay: async () => {},
        intervalMs: 1,
        maxSamples: 7,
        confirmSamples: 2,
      });
      return false;
    } catch (err) {
      const samples = err && err.rssEvidence && err.rssEvidence.release && err.rssEvidence.release.samples;
      return err instanceof P0ContractError
        && /released=false over 7 samples/.test(err.message)
        && Array.isArray(samples)
        && samples[1].rssBytes === 1024 * 1024
        && samples[2].rssBytes === 8192 * 1024
        && samples.slice(3).every((s) => s.rssBytes === 4096 * 1024);
    }
  })());

  // sampleRssPlateauAndRelease failure evidence: when the release window
  // fails closed, the contract error carries the plateau verdict plus the
  // series so live evidence keeps the full diagnostic.
  check('release failure evidence carries plateau and series', await (async () => {
    try {
      await sampleRssPlateauAndRelease({
        pid: 4242,
        fsp: procFsFlat,
        now: () => 1000,
        delay: async () => {},
        samples: 4,
        intervalMs: 10,
        release: { maxSamples: 3 },
        disconnect: async () => {},
      });
      return false;
    } catch (err) {
      return err instanceof P0ContractError
        && err.rssEvidence
        && (err.rssEvidence.slopeBytesPerMs === null || typeof err.rssEvidence.slopeBytesPerMs === 'number')
        && err.rssEvidence.plateau.detected === true
        && err.rssEvidence.release.released === false
        && err.rssEvidence.release.samples.length === 3;
    }
  })());
  // Fake page that throws on ANY access once the browser client closes: the
  // post-disconnect RSS capture with postDisconnectPage === null must never
  // touch the closed page while still sampling the released process tree.
  let rssKb = 4096;
  let pageTouched = 0;
  let pageClosed = false;
  const closedPage = {
    context() {
      pageTouched += 1;
      if (pageClosed) throw new Error('closed page accessed after disconnect');
      return {
        async newCDPSession() {
          return {
            async send() {
              return { metrics: [] };
            },
            async detach() {},
          };
        },
      };
    },
  };
  const closedPageFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return `Name:\trpi\nVmRSS:\t   ${rssKb} kB\n`;
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('post-disconnect capture never touches closed page', await (async () => {
    const pageAware = await sampleRssPlateauAndRelease({
      pid: 4242,
      page: closedPage,
      postDisconnectPage: null,
      fsp: closedPageFs,
      now: (() => {
        let t = 3000;
        return () => (t += 10);
      })(),
      delay: async () => {},
      samples: 4,
      intervalMs: 10,
      disconnect: async () => {
        pageClosed = true;
        rssKb = 1024;
      },
    });
    return pageAware.postDisconnectPage === null
      && pageAware.release.released === true
      && pageTouched === 1; // connected capture only; closed page untouched
  })());
  check('closed page access fails fail-closed', await (async () => {
    try {
      await captureResourceSnapshot({ page: closedPage, pid: 4242, fsp: closedPageFs, now: () => 4000 });
      return false;
    } catch (err) {
      return pageTouched === 2 && /after disconnect/.test(err.message);
    }
  })());

  const evidenceFs = memoryFs();
  // Final causal-cut negatives (subscribeAuthoritative). A stale cut that
  // carries the exact identity but was preloaded into the frame history
  // before a new subscribe must NOT satisfy that subscribe; only a fresh
  // cut appended after the awaited send settles may. The thenable send
  // models the real transport: the server learns of the request when the
  // send settles, never before.
  const causalSocket = new FakeSocket();
  const causalClient = createControlClient({
    url: 'ws://127.0.0.1:9/ws',
    WebSocketImpl: class {
      constructor() {
        return causalSocket;
      }
    },
  });
  await causalClient.ready;
  const causalIdentity = { sessionId: 'p0-causal', runtimeIncarnation: 'inc-causal' };
  causalSocket.emit('message', JSON.stringify({
    type: 'subscription_snapshot',
    sessionId: causalIdentity.sessionId,
    runtimeIncarnation: causalIdentity.runtimeIncarnation,
    watermark: 0,
    reason: 'cold-full',
    events: [],
  }));
  let causalSubscribes = 0;
  causalSocket.send = (data) => {
    const text = String(data);
    causalSocket.sent.push(text);
    return new Promise((resolve) => {
      setTimeout(() => {
        const request = JSON.parse(text);
        causalSubscribes += 1;
        if (causalSubscribes === 2) {
          // Append the fresh cut strictly after the second send settles.
          setTimeout(() => {
            causalSocket.emit('message', JSON.stringify({
              type: 'subscription_snapshot',
              sessionId: request.sessionId,
              runtimeIncarnation: request.runtimeIncarnation,
              watermark: request.cursor || 0,
              reason: 'cold-refresh',
              events: [],
            }));
          }, 2);
        }
        resolve();
      }, 2);
    });
  };
  check('stale same-identity cut cannot satisfy new subscribe; awaited send then fresh cut does', await (async () => {
    // The preloaded cut sits at index 0, before this subscribe's afterIndex:
    // identical sessionId/runtimeIncarnation must not make it authoritative.
    let staleBlocked = false;
    try {
      await subscribeAuthoritative(causalClient, causalIdentity, 1, 80);
    } catch (err) {
      staleBlocked = err instanceof P0ContractError && /timed out/.test(err.message);
    }
    if (!staleBlocked) return false;
    // Second subscribe: the thenable send settles first, and only after
    // that does the server append the fresh cut; it must be accepted.
    const cut = await subscribeAuthoritative(causalClient, causalIdentity, 7, 300);
    return cut && cut.type === 'subscription_snapshot'
      && cut.reason === 'cold-refresh'
      && cut.watermark === 7
      && causalSubscribes === 2;
  })());

  // Browser-closer negatives (closeBrowserClients): page.close() and
  // context.close() rejections must propagate (fail-closed) instead of
  // masquerading as a clean disconnect.
  const pageCloseRejects = {
    async evaluate() {},
    async close() {
      throw new Error('page close exploded');
    },
    context() {
      return { async close() {} };
    },
  };
  const contextCloseRejects = {
    async close() {},
    context() {
      return { async close() { throw new Error('context close exploded'); } };
    },
  };
  check('page.close and context.close rejections propagate from closeBrowserClients', await (async () => {
    try {
      await closeBrowserClients({ pages: [pageCloseRejects] });
      return false;
    } catch (err) {
      if (!(err instanceof Error) || err.message !== 'page close exploded') return false;
    }
    try {
      await closeBrowserClients({ pages: [contextCloseRejects] });
      return false;
    } catch (err) {
      return err instanceof Error && err.message === 'context close exploded';
    }
  })());

  // Full disconnect negative: the browser-closer failure propagates out of
  // disconnectControlThenBrowser through the RSS disconnect hook — the
  // control clients are still closed cleanly first — and the post-disconnect
  // sample must never run (postSample stays false).
  const closingPageFs = {
    async readFile(path) {
      if (path.endsWith('/status')) return 'Name:\trpi\nVmRSS:\t   4096 kB\n';
      if (path.includes('/children')) return '';
      throw Object.assign(new Error('ENOENT'), { code: 'ENOENT' });
    },
  };
  check('disconnect rejects on browser close failure and skips post-RSS sample', await (async () => {
    const controlA = fakeClientWithPool();
    await controlA.client.ready;
    const controlB = fakeClientWithPool();
    await controlB.client.ready;
    let postSample = false;
    let disconnected = false;
    const rejectingPage = {
      evaluate: async () => {},
      async close() {
        throw new Error('page close exploded');
      },
      context() {
        return {
          async newCDPSession() {
            if (disconnected) postSample = true; // only a POST-disconnect capture may mark
            return { async send() { return { metrics: [] }; }, async detach() {} };
          },
          async close() {},
        };
      },
    };
    try {
      await sampleRssPlateauAndRelease({
        pid: 4242,
        page: rejectingPage,
        postDisconnectPage: rejectingPage,
        fsp: closingPageFs,
        now: (() => {
          let t = 8000;
          return () => (t += 10);
        })(),
        delay: async () => {},
        samples: 4,
        intervalMs: 10,
        disconnect: async () => {
          disconnected = true;
          await disconnectControlThenBrowser({
            clientA: controlA.client,
            clientB: controlB.client,
            pages: [rejectingPage],
          });
        },
      });
      return false; // the browser-close failure must propagate
    } catch (err) {
      return err instanceof Error
        && err.message === 'page close exploded'
        && postSample === false
        && controlA.client.closes.length === 1
        && controlA.client.closes[0].code === 1000
        && controlA.client.closes[0].locallyInitiated === true
        && controlA.client.unexpectedCloses.length === 0
        && controlB.client.closes.length === 1
        && controlB.client.closes[0].code === 1000;
    }
  })());
  const evidencePath = '<workspace>/p0-evidence.json';
  const unitRoot = await mkdtemp(join(tmpdir(), 'p0-contract-'));
  try {
    const nativePath = join(unitRoot, 'native.jsonl');
    const written = await runP0ResourceContract({
      evidencePath,
      nativeSession: { filePath: nativePath, cwd: '<workspace>/p0', sessionId: 'p0-native-20mib' },
      skipLiveTransport: true,
      fsp: evidenceFs,
      now: () => 1_700_000_000_000,
    });
    check('skipLiveTransport evidence ok', written.evidence.evidence.ok === true);
    check('evidence schema stamped', written.evidence.schema === 'p0-resource-metrics.v1');
    check('inner p0 schema present', written.evidence.evidence.schema === 'p0-session-acceptance.v1');
    check('native session evidence omits path/cwd', (() => {
      const ev = written.evidence.evidence.contracts.nativeSession;
      return !('path' in ev) && !('cwd' in ev) && !('filePath' in ev)
        && ev.live === null
        && ev.sessionId === 'p0-native-20mib'
        && ev.header && ev.header.type === 'session' && ev.header.version === 3
        && ev.header.id === 'p0-native-20mib' && Object.keys(ev.header).length === 3;
    })());
    check('nativeSessionEvidence prunes header and carries live', (() => {
      const ev = nativeSessionEvidence(
        { bytes: 21, sessionId: 's2', marker: 'm2', timestamp: 't2', path: '/p/s2.jsonl', cwd: '/p', header: { type: 'session', version: 3, id: 's2', cwd: '/p', timestamp: 't2' } },
        { sessionId: 's2', runtimeIncarnation: 'inc-2' },
      );
      return !('path' in ev) && !('cwd' in ev)
        && Object.keys(ev.header).length === 3 && ev.header.cwd === undefined
        && ev.live && ev.live.runtimeIncarnation === 'inc-2';
    })());
    check('secrets redacted in evidence helper', redactValue({ token: 'secret-value' }).token === '[REDACTED]');
    check('path-shaped evidence normalized', (() => {
      const redacted = redactValue({ cwd: '/home/x/work', retiredPath: '/tmp/a.jsonl' });
      return redacted.cwd === '<path:work>' && redacted.retiredPath === '<path:a.jsonl>';
    })());
    check('relative evidence paths pass through', normalizeEvidencePath('<workspace>/sessions/x.jsonl') === '<workspace>/sessions/x.jsonl');
    const missingPid = await writeEvidenceJson(join(unitRoot, 'pid-missing.json'), { probe: true }, { fsp: evidenceFs, now: () => 2 });
    check('writeEvidenceJson returns path', typeof missingPid.path === 'string');
    // Nested scalar failure message carrying TWO host-local paths: the raw
    // message keeps the exact diagnostic text, while the written evidence
    // normalizes both embedded paths so the artifact never leaks hosts.
    const leakFs = memoryFs();
    const leakPayload = {
      contracts: {
        pool: {
          failures: [
            {
              id: 'reopen',
              message: 'reopen failed: /home/u/work/p0-evidence.json and /home/u/other/token.json mismatch',
            },
          ],
        },
      },
    };
    const leakEv = await writeEvidenceJson('<workspace>/leak.json', leakPayload, { fsp: leakFs, now: () => 5 });
    const leakFailures = leakEv.evidence.evidence.contracts.pool.failures;
    check('nested failure message normalized in evidence', (() => {
      const msg = leakFailures[0].message;
      return typeof msg === 'string'
        && !msg.includes('/home/')
        && msg.includes('<path:p0-evidence.json>')
        && msg.includes('<path:token.json>');
    })(), `message=${leakFailures[0].message}`);
    check('nested failure keeps diagnostic text', (() => {
      const msg = leakFailures[0].message;
      return msg.includes('reopen failed:') && msg.includes(' and ') && msg.includes(' mismatch');
    })(), `message=${leakFailures[0].message}`);
    check('embedded scalar paths normalized with no host leakage', (() => {
      const raw = '/home/u/work/target/x.jsonl and /home/u/tmp/y.jsonl disagree';
      const norm = redactPathSubstrings(raw);
      return !norm.includes('/home/')
        && norm.includes('<path:x.jsonl>')
        && norm.includes('<path:y.jsonl>')
        && norm.includes(' disagree');
    })(), `normalized=${redactPathSubstrings('/home/u/work/target/x.jsonl and /home/u/tmp/y.jsonl disagree')}`);
  } finally {
    await rm(unitRoot, { recursive: true, force: true });
  }

  const py = spawn('python3', ['-m', 'py_compile', join(here, 'p0_resource_mock.py')], { stdio: ['ignore', 'pipe', 'pipe'] });
  const pyCode = await new Promise((resolve) => {
    py.on('close', resolve);
  });
  check('p0_resource_mock.py syntax', pyCode === 0);

  if (failed) {
    console.error(`p0-resource-unit: ${failed} failed`);
    process.exit(2);
  }
  console.log('p0-resource-unit: PASSED');
}

main().catch((err) => {
  console.error(`p0-resource-unit: FAIL: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
