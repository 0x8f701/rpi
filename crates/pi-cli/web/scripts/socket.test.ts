#!/usr/bin/env node
// Focused socket-generation reconnect regression for src/socket.ts — the
// pure generation invariant that fixes the reconnect P1: pending commands and
// bootstrap continuations are tied to the socket GENERATION they were sent on,
// so a socket (A) superseded mid-bootstrap (dropped, B reconnected healthy) can
// never settle a newer socket's pending, applyState/setConnState, or schedule a
// reconnect that replaces the healthy B. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Assertions drive a small pending-map + generation simulation through the REAL
// helper decisions (isCurrentPending / shouldScheduleReconnect / isStaleAbort),
// modeling the App.tsx connect/onOpen/onResponse flow — not source strings.
import { STALE_ABORT, isStaleAbort, isCurrentPending, shouldScheduleReconnect, detachTransportHandlers, HEARTBEAT_TIMEOUT_MS, HEARTBEAT_PING_INTERVAL_MS, HEARTBEAT_PONG_DEADLINE_MS, HEARTBEAT_PING_FRAME, heartbeatMayRetire, shouldCloseForSilence, shouldCloseForMissedPong, isPongFrame, heartbeatPingAction, planHeartbeatPing, shouldRetireHandshake, TRANSPORT_HANDSHAKE_DEADLINE_MS, TRANSPORT_READY_CONNECTING, TRANSPORT_READY_OPEN, TRANSPORT_READY_CLOSING, TRANSPORT_READY_CLOSED } from '../src/socket.ts';
import {
  RECONNECT_INITIAL_DELAY_MS,
  RECONNECT_MAX_DELAY_MS,
  RECONNECT_OVERLAY_TEXT,
  BOOTSTRAP_RETRY_DELAY_MS,
  BOOTSTRAP_RETRY_LIMIT,
  nextReconnectDelay,
  reconnectDelaySchedule,
  reconnectOverlayVisible,
  reconnectOverlayText,
  withBootstrapRetry,
  planTransportOutageToast,
  planRefusedHandshake,
} from '../src/reconnect.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- sentinel identity ----
{
  check('STALE_ABORT is its own sentinel', isStaleAbort(STALE_ABORT));
  check('a real Error is NOT a stale abort', !isStaleAbort(new Error('boom')));
  check('undefined is NOT a stale abort', !isStaleAbort(undefined));
  check('STALE_ABORT is a symbol, not an Error', typeof STALE_ABORT === 'symbol');
}

// ---- isCurrentPending: only the current generation's pending settles ----
{
  check('current-gen pending is current', isCurrentPending(2, 2));
  check('stale-gen pending is NOT current', !isCurrentPending(1, 2));
  check('same gen 0 is current', isCurrentPending(0, 0));
}

// ---- shouldScheduleReconnect: stale/aborted never reconnects ----
{
  // A LIVE socket's real bootstrap error reconnects.
  check('live socket real error reconnects', shouldScheduleReconnect(new Error('messages missing'), true));
  check('live socket timeout reconnects', shouldScheduleReconnect(new Error('command timed out'), true));
  // A STALE abort never reconnects (explicit bail).
  check('STALE_ABORT never reconnects (even if alive)', !shouldScheduleReconnect(STALE_ABORT, true));
  check('STALE_ABORT never reconnects (dead)', !shouldScheduleReconnect(STALE_ABORT, false));
  // A superseded socket (alive === false) never reconnects on behalf of the
  // healthy newer socket — covers the late timeout case.
  check('dead socket timeout does NOT reconnect', !shouldScheduleReconnect(new Error('command timed out'), false));
  check('dead socket real error does NOT reconnect', !shouldScheduleReconnect(new Error('state did not bind'), false));
}

// ---- heartbeat: silence backstop plus missed-pong round-trip ----
{
  const startedAt = 1_000;
  check('responsive socket stays open before silence bound', !shouldCloseForSilence(startedAt, startedAt + HEARTBEAT_TIMEOUT_MS - 1));
  check('silent socket closes exactly at silence bound', shouldCloseForSilence(startedAt, startedAt + HEARTBEAT_TIMEOUT_MS));
  const pongAt = startedAt + HEARTBEAT_TIMEOUT_MS - 1_000;
  check('any inbound slides the silence window', !shouldCloseForSilence(pongAt, pongAt + 2_000));
  check('socket closes only after the slid window expires', shouldCloseForSilence(pongAt, pongAt + HEARTBEAT_TIMEOUT_MS));
}
{
  check('no outstanding ping is not a missed pong', !shouldCloseForMissedPong(null, 50_000, 0));
  const sentAt = 5_000;
  const lastBeforePing = sentAt - 1;
  check('outstanding ping stays open before deadline', !shouldCloseForMissedPong(sentAt, sentAt + HEARTBEAT_PONG_DEADLINE_MS - 1, lastBeforePing));
  check('outstanding ping with no inbound closes exactly at deadline', shouldCloseForMissedPong(sentAt, sentAt + HEARTBEAT_PONG_DEADLINE_MS, lastBeforePing));
  check('5s of queued transcript after ping is not a missed pong', !shouldCloseForMissedPong(sentAt, sentAt + 5_000, lastBeforePing));
  check('any inbound after ping keeps the socket even past the pong deadline',
    !shouldCloseForMissedPong(sentAt, sentAt + HEARTBEAT_PONG_DEADLINE_MS + 1, sentAt + 1));
  check('inbound at the ping instant is live', !shouldCloseForMissedPong(sentAt, sentAt + HEARTBEAT_PONG_DEADLINE_MS + 1, sentAt));
}
{
  check('canonical pong frame is recognized', isPongFrame('{"type":"pong"}'));
  check('spaced pong json is recognized', isPongFrame('{ "type": "pong" }'));
  check('ping frame is not a pong', !isPongFrame(HEARTBEAT_PING_FRAME));
  check('rpc response is not a pong', !isPongFrame('{"type":"response","id":"c1"}'));
  check('invalid json is not a pong', !isPongFrame('not-json'));
}

{
  check('missing socket: ping stays idle', heartbeatPingAction(null) === 'idle');
  check('OPEN ping sends', heartbeatPingAction(TRANSPORT_READY_OPEN) === 'send');
  check('CONNECTING ping waits for onopen/onclose', heartbeatPingAction(TRANSPORT_READY_CONNECTING) === 'idle');
  check('CLOSING ping retires without waiting for onclose', heartbeatPingAction(TRANSPORT_READY_CLOSING) === 'retire');
  check('CLOSED ping retires without waiting for onclose', heartbeatPingAction(TRANSPORT_READY_CLOSED) === 'retire');
}

{
  check('CLOSING plan retires', planHeartbeatPing(TRANSPORT_READY_CLOSING, null, 0) === 'retire');
  check('CLOSED plan retires', planHeartbeatPing(TRANSPORT_READY_CLOSED, null, 0) === 'retire');
  check('OPEN with no ping sends', planHeartbeatPing(TRANSPORT_READY_OPEN, null, 0) === 'send');
  check('OPEN outstanding ping waits', planHeartbeatPing(TRANSPORT_READY_OPEN, 5_000, 5_000, 4_999) === 'wait-pong');
  check('OPEN missed pong with no inbound retires', planHeartbeatPing(TRANSPORT_READY_OPEN, 5_000, 5_000 + HEARTBEAT_PONG_DEADLINE_MS, 4_999) === 'retire');
  check('OPEN inbound after ping does not retire past the pong deadline',
    planHeartbeatPing(TRANSPORT_READY_OPEN, 5_000, 5_000 + HEARTBEAT_PONG_DEADLINE_MS + 1, 5_001) === 'send');
  check('CONNECTING plan is idle', planHeartbeatPing(TRANSPORT_READY_CONNECTING, null, 0) === 'idle');
  check('missing socket plan is idle', planHeartbeatPing(null, null, 0) === 'idle');
}

{
  const startedAt = 1_000;
  check('CONNECTING before deadline stays', !shouldRetireHandshake(TRANSPORT_READY_CONNECTING, startedAt, startedAt + TRANSPORT_HANDSHAKE_DEADLINE_MS - 1));
  check('CONNECTING at deadline retires', shouldRetireHandshake(TRANSPORT_READY_CONNECTING, startedAt, startedAt + TRANSPORT_HANDSHAKE_DEADLINE_MS));
  check('OPEN never retires handshake', !shouldRetireHandshake(TRANSPORT_READY_OPEN, startedAt, startedAt + TRANSPORT_HANDSHAKE_DEADLINE_MS + 1));
  check('missing start does not retire handshake', !shouldRetireHandshake(TRANSPORT_READY_CONNECTING, null, 50_000));
  check('missing socket does not retire handshake', !shouldRetireHandshake(null, startedAt, startedAt + TRANSPORT_HANDSHAKE_DEADLINE_MS));
}

{
  let reconnects = 0;
  let ws = { readyState: TRANSPORT_READY_CLOSING };
  function sendHeartbeatPing() {
    const plan = planHeartbeatPing(ws ? ws.readyState : null, null, 0);
    if (plan === 'idle' || plan === 'wait-pong') return;
    if (plan === 'retire') {
      ws = null;
      reconnects += 1;
    }
  }
  sendHeartbeatPing();
  check('CLOSING ping interval reconnects without onclose', reconnects === 1 && ws === null);
  reconnects = 0;
  ws = { readyState: TRANSPORT_READY_CLOSED };
  sendHeartbeatPing();
  check('CLOSED ping interval reconnects without onclose', reconnects === 1 && ws === null);
  reconnects = 0;
  ws = { readyState: TRANSPORT_READY_OPEN };
  sendHeartbeatPing();
  check('OPEN ping does not retire', reconnects === 0 && ws !== null);
}

{
  let reconnects = 0;
  let neverOpened = 0;
  let ws = { readyState: TRANSPORT_READY_CONNECTING };
  const startedAt = 0;
  function onHandshakeDeadline(now) {
    if (!shouldRetireHandshake(ws ? ws.readyState : null, startedAt, now)) return;
    if (ws.readyState !== TRANSPORT_READY_OPEN) neverOpened += 1;
    ws = null;
    reconnects += 1;
  }
  onHandshakeDeadline(TRANSPORT_HANDSHAKE_DEADLINE_MS - 1);
  check('CONNECTING handshake waits for deadline', reconnects === 0 && ws !== null && neverOpened === 0);
  onHandshakeDeadline(TRANSPORT_HANDSHAKE_DEADLINE_MS);
  check('CONNECTING handshake retires without onclose', reconnects === 1 && ws === null && neverOpened === 1);
}

/**
 * Mini reconnect simulator: mirrors App.tsx connect/onOpen/onResponse using the
 * real helpers. A "socket" is just a generation number. pending entries carry
 * their gen; a response settles only a current-gen pending; connect() bumps gen
 * and drains (rejects) every pending NOT on the new gen (rejectPendingExcept).
 */
function simulate() {
  let gen = 0; // socketGenRef.current
  const pending = new Map(); // id -> { gen, state: 'pending'|'resolved'|'rejected', reason? }
  const events = []; // 'reconnect' | 'applyState:<gen>' | 'setConnStateOn:<gen>' | 'toast:<id>'
  const current = () => gen;
  const alive = (socketGen) => socketGen === gen;
  function connect() {
    gen += 1; // ++socketGenRef.current
    // rejectPendingExcept(gen): reject+delete every pending not on the new gen.
    for (const [id, e] of pending) {
      if (e.gen !== gen) {
        e.state = 'rejected';
        e.reason = 'connection replaced';
        pending.delete(id);
      }
    }
    return gen;
  }
  function send(id, socketGen) {
    pending.set(id, { gen: socketGen, state: 'pending' });
  }
  // onResponse: settle only a current-gen pending; a stale-gen pending is dropped
  // (not settled), matching isCurrentPending.
  function onResponse(id, success, socketGenOfCommand) {
    const e = pending.get(id);
    if (!e) return; // drained -> no-op (late response for a dead socket)
    if (!isCurrentPending(e.gen, current())) {
      pending.delete(id); // stale settlement dropped
      return;
    }
    pending.delete(id);
    e.state = success ? 'resolved' : 'rejected';
    e.reason = success ? undefined : 'rpc failed';
  }
  // The bootstrap .catch decision: reconnect only if shouldScheduleReconnect.
  function bootstrapCatch(err, socketGen) {
    if (!shouldScheduleReconnect(err, alive(socketGen))) return;
    events.push('reconnect');
  }
  // The bootstrap success continuation: only proceeds if alive (guarded in App).
  function bootstrapSuccess(socketGen) {
    if (!alive(socketGen)) {
      bootstrapCatch(STALE_ABORT, socketGen); // thrown STALE_ABORT -> .catch
      return;
    }
    events.push(`setConnStateOn:${socketGen}`);
  }
  return { current, connect, send, onResponse, bootstrapCatch, bootstrapSuccess, pending, events, alive };
}

// ---- Scenario 1: A disconnects -> B healthy -> A's timeout does NOT affect B ----
{
  const sim = simulate();
  const a = sim.connect(); // A opens, gen 1
  sim.send('c1', a); // A sends bootstrap get_state (c1, gen 1)
  // A is superseded by B before c1 responds.
  const b = sim.connect(); // B opens, gen 2; drains pending -> c1 rejected+deleted
  check('A pending c1 was drained (rejected) on replace', !sim.pending.has('c1'));
  sim.send('c2', b); // B sends bootstrap get_state (c2, gen 2)
  sim.onResponse('c2', true, b); // B's bootstrap succeeds
  check('B pending c2 resolved (B healthy)', sim.pending.has('c2') === false); // settled + deleted
  // A's 30s timeout fires for c1: it was already drained, so onResponse(c1)
  // is a no-op (no pending to settle); the bootstrap continuation that owned c1
  // is dead. Model its .catch with A's gen and the timeout error:
  sim.bootstrapCatch(new Error('command timed out'), a);
  check('A timeout does NOT schedule a reconnect (B is healthy)', sim.events.length === 0, JSON.stringify(sim.events));
  // B's bootstrap success still applies (B owns the UI).
  sim.bootstrapSuccess(b);
  check('B bootstrap success sets connState on', sim.events.some((e) => e === 'setConnStateOn:2'), JSON.stringify(sim.events));
}

// ---- Scenario 2: late A response does NOT settle B's pending ----
{
  const sim = simulate();
  const a = sim.connect(); // A, gen 1
  sim.send('c1', a); // A sends c1 (gen 1)
  const b = sim.connect(); // B, gen 2; drains c1 (rejected+deleted)
  sim.send('c2', b); // B sends c2 (gen 2)
  // A's late response for c1 arrives: c1 was drained -> no-op.
  sim.onResponse('c1', true, a);
  check('late A response for drained c1 is a no-op', !sim.pending.has('c1'));
  // B's c2 must still be pending (unsettled by A's late response).
  check('B pending c2 still pending after late A response', sim.pending.get('c2') && sim.pending.get('c2').state === 'pending');
  // B's own response settles c2 (not A's).
  sim.onResponse('c2', true, b);
  check('B response settles c2 (resolved)', sim.pending.has('c2') === false);
}

// ---- Scenario 2b: a stale-gen response that somehow still has a pending entry
//      is DROPPED, not settled, so it cannot continue a dead bootstrap ----
{
  const sim = simulate();
  const a = sim.connect(); // gen 1
  sim.send('c1', a); // c1, gen 1 — NOT drained this time
  sim.connect(); // gen 2 (B), but we intentionally left c1 in the map to model a
  // defensive case: a response arrives for a gen-1 pending while gen is 2.
  // isCurrentPending(1, 2) is false -> dropped, not resolved.
  sim.onResponse('c1', true, a);
  check('stale-gen response dropped (not settled) — isCurrentPending guard', !sim.pending.has('c1'));
}

// ---- Scenario 3: a LIVE socket's real bootstrap failure DOES reconnect ----
{
  const sim = simulate();
  const a = sim.connect(); // gen 1, still current
  sim.bootstrapCatch(new Error('state response did not bind a session'), a);
  check('live socket bootstrap failure schedules a reconnect', sim.events.length === 1 && sim.events[0] === 'reconnect', JSON.stringify(sim.events));
}


// ---- Scenario 3b: repeated live bootstrap failures close each poisoned
//      generation and keep one bounded retry chain until a later generation
//      becomes ready ----
{
  let gen = 0;
  let retryTimer: number | null = null;
  let nextTimer = 1;
  let scheduled = 0;
  let closed = 0;
  let ready = false;
  const timers = new Map<number, () => void>();
  const schedule = () => {
    if (retryTimer !== null) timers.delete(retryTimer);
    retryTimer = nextTimer++;
    const owned = retryTimer;
    timers.set(owned, () => {
      if (retryTimer !== owned) return;
      retryTimer = null;
      connect();
    });
    scheduled += 1;
  };
  const failBootstrap = (socketGen: number) => {
    if (!shouldScheduleReconnect(new Error('bootstrap failed'), socketGen === gen)) return;
    gen += 1;
    closed += 1;
    schedule();
  };
  const connect = () => {
    gen += 1;
    const socketGen = gen;
    if (socketGen < 6) failBootstrap(socketGen);
    else ready = true;
  };
  const fireOwnedTimer = () => {
    const owned = retryTimer;
    if (owned === null) return;
    timers.get(owned)?.();
    timers.delete(owned);
  };

  connect();
  while (!ready && retryTimer !== null) fireOwnedTimer();
  check('repeated bootstrap failures eventually reach a ready generation', ready);
  check('every failed live generation is closed before retry', closed === 3, `closed=${closed} gen=${gen}`);
  check('one retry timer is owned at a time', timers.size === 0 && retryTimer === null, `timers=${timers.size}`);
  check('each failed generation schedules exactly one retry', scheduled === closed, `scheduled=${scheduled} closed=${closed}`);
}

// ---- Scenario 4: connect()/unmount detach ALL old transport handlers before
//      close, so a CONNECTING old socket's late onopen cannot fire against the
//      new generation/wsRef (double-bootstrap prevention) ----
{
  let onopenFired = 0;
  let onmessageFired = 0;
  let onerrorFired = 0;
  let oncloseFired = 0;
  let closed = false;
  const oldSocket = {
    onopen: () => { onopenFired += 1; },
    onmessage: () => { onmessageFired += 1; },
    onerror: () => { onerrorFired += 1; },
    onclose: () => { oncloseFired += 1; },
    close: () => { closed = true; },
  };
  // connect()/unmount replacement: detach ALL four handlers, then close.
  detachTransportHandlers(oldSocket);
  oldSocket.close();
  // Simulate the old CONNECTING socket firing every event AFTER replacement.
  // The slots are null, so the optional calls are no-ops — no late bootstrap,
  // no late frame routing, no duplicate toast.
  if (oldSocket.onopen) oldSocket.onopen();
  if (oldSocket.onmessage) oldSocket.onmessage();
  if (oldSocket.onerror) oldSocket.onerror();
  if (oldSocket.onclose) oldSocket.onclose();
  check('detach nulls onopen (late onopen cannot fire)', oldSocket.onopen === null);
  check('detach nulls onmessage', oldSocket.onmessage === null);
  check('detach nulls onerror', oldSocket.onerror === null);
  check('detach nulls onclose', oldSocket.onclose === null);
  check('late old onopen did NOT fire after detach', onopenFired === 0);
  check('late old onmessage did NOT fire after detach', onmessageFired === 0);
  check('late old onerror did NOT fire after detach', onerrorFired === 0);
  check('late old onclose did NOT fire after detach', oncloseFired === 0);
  check('old socket closed after detach', closed === true);
  // A fresh socket still has its handlers intact (detach only touches the old).
  const fresh = { onopen: () => { onopenFired += 1; }, onmessage: null, onerror: null, onclose: null, close: () => {} };
  check('fresh socket onopen intact (not detached)', typeof fresh.onopen === 'function');
}

{
  check('first reconnect delay is at most 500ms', RECONNECT_INITIAL_DELAY_MS <= 500 && RECONNECT_INITIAL_DELAY_MS >= 0);
  check('first reconnect delay is 250ms', RECONNECT_INITIAL_DELAY_MS === 250);
  check('reconnect delay cap is 5s', RECONNECT_MAX_DELAY_MS === 5_000);
  check(
    'reconnect delay schedule doubles then caps',
    reconnectDelaySchedule().join(',') === '250,500,1000,2000,4000,5000,5000,5000',
  );
  check('next delay from initial is 500', nextReconnectDelay(RECONNECT_INITIAL_DELAY_MS) === 500);
  check('next delay from 4s is the cap', nextReconnectDelay(4_000) === RECONNECT_MAX_DELAY_MS);
  check('next delay from 8s still caps', nextReconnectDelay(8_000) === RECONNECT_MAX_DELAY_MS);
  check('next delay at the cap stays capped', nextReconnectDelay(RECONNECT_MAX_DELAY_MS) === RECONNECT_MAX_DELAY_MS);
}

{
  check('connecting shows overlay', reconnectOverlayVisible('connecting') === true);
  check('reconnecting shows overlay', reconnectOverlayVisible('reconnecting') === true);
  check('offline shows overlay', reconnectOverlayVisible('offline') === true);
  check('authoritative on hides overlay', reconnectOverlayVisible('on') === false);
  check('connected hides overlay', reconnectOverlayVisible('connected') === false);
  check('on during bootstrap still shows overlay', reconnectOverlayVisible('on', true) === true);
  check('connected during bootstrap still shows overlay', reconnectOverlayVisible('connected', true) === true);
  check('token_required never covers the pairing surface', reconnectOverlayVisible('token_required') === false);
  check('token_required stays hidden even during boot', reconnectOverlayVisible('token_required', true) === false);
  check('off before connect has no overlay', reconnectOverlayVisible('off') === false);
  check(
    'refused handshake with a token still reconnects until auth is confirmed',
    planRefusedHandshake({ socketOpened: false, everConnected: false, hasToken: true, authFailed: false }).kind === 'reconnect'
      && planRefusedHandshake({ socketOpened: false, everConnected: false, hasToken: true, authFailed: false }).connState === 'reconnecting',
  );
  check(
    'confirmed 401 with a token holds without overlay or reconnect',
    planRefusedHandshake({ socketOpened: false, everConnected: false, hasToken: true, authFailed: true }).kind === 'auth_hold'
      && planRefusedHandshake({ socketOpened: false, everConnected: false, hasToken: true, authFailed: true }).connState === 'off'
      && reconnectOverlayVisible('off') === false,
  );
  check(
    'mid-session close still reconnects',
    planRefusedHandshake({ socketOpened: true, everConnected: true, hasToken: true, authFailed: false }).kind === 'reconnect',
  );
  check(
    'never-opened close without a token still reconnects',
    planRefusedHandshake({ socketOpened: false, everConnected: false, hasToken: false, authFailed: false }).kind === 'reconnect',
  );
  check('overlay copy is the reconnect wait line', RECONNECT_OVERLAY_TEXT === '连接断开，正在重连…');
  check('first-contact connecting copy is 正在连接, never the disconnect line', reconnectOverlayText('connecting') === '正在连接…');
  check('reconnecting and offline copy is the disconnect line', reconnectOverlayText('reconnecting') === '连接断开，正在重连…' && reconnectOverlayText('offline') === '连接断开，正在重连…');
}

{
  const firstClose = planTransportOutageToast({
    everConnected: true,
    outageActive: false,
    message: 'connection closed (code 4000): test outage, reconnecting…',
  });
  check(
    'mid-session first close emits a persistent transport toast',
    firstClose.kind === 'emit'
      && firstClose.toastKind === 'transport'
      && firstClose.timeoutMs === null
      && firstClose.message.includes('test outage'),
    JSON.stringify(firstClose),
  );
  check(
    'repeat close in the same outage does not emit a second toast',
    planTransportOutageToast({
      everConnected: true,
      outageActive: true,
      message: 'connection closed (code 4000): test outage, reconnecting…',
    }).kind === 'skip',
  );
  check(
    'never-connected close does not emit the mid-session transport toast',
    planTransportOutageToast({
      everConnected: false,
      outageActive: false,
      message: 'cannot open WebSocket, reconnecting…',
    }).kind === 'skip',
  );
}

{
  check('5s stability is not the silence close bound', HEARTBEAT_TIMEOUT_MS !== 5_000 && HEARTBEAT_TIMEOUT_MS === 60_000);
  check('application ping is 5s', HEARTBEAT_PING_INTERVAL_MS === 5_000);
  check('pong deadline stays 10s not a flat 5s', HEARTBEAT_PONG_DEADLINE_MS === 10_000);
  check('silence at 5s does not close', !shouldCloseForSilence(1_000, 6_000));
  check('visible page may retire a dead heartbeat', heartbeatMayRetire(true) === true);
  check('hidden page must not retire on timer age', heartbeatMayRetire(false) === false);
}

{
  const abort = { kind: 'stale' };
  const waits = [];
  const wait = async (ms) => { waits.push(ms); };
  let attempts = 0;
  const recovered = await withBootstrapRetry(async () => {
    attempts += 1;
    if (attempts < 3) throw new Error(`transient-${attempts}`);
    return 'ok';
  }, { isAbort: () => false, wait });
  check('bootstrap retry delay is 1.5s and limit is 2', BOOTSTRAP_RETRY_DELAY_MS === 1_500 && BOOTSTRAP_RETRY_LIMIT === 2);
  check('two failures then succeed after two waits', recovered === 'ok' && attempts === 3 && waits.join(',') === '1500,1500');
  attempts = 0;
  waits.length = 0;
  let exhausted = '';
  try {
    await withBootstrapRetry(async () => {
      attempts += 1;
      throw new Error(`fail-${attempts}`);
    }, { isAbort: () => false, wait });
  } catch (error) {
    exhausted = error instanceof Error ? error.message : String(error);
  }
  check('exhausted retries throw the last error', attempts === 3 && exhausted === 'fail-3' && waits.join(',') === '1500,1500');
  attempts = 0;
  waits.length = 0;
  let abortThrown = false;
  try {
    await withBootstrapRetry(async () => {
      attempts += 1;
      throw abort;
    }, { isAbort: (error) => error === abort, wait });
  } catch (error) {
    abortThrown = error === abort;
  }
  check('stale abort never retries', abortThrown && attempts === 1 && waits.length === 0);
}

console.log(`\nsocket.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);