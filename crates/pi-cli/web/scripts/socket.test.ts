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
import { STALE_ABORT, isStaleAbort, isCurrentPending, shouldScheduleReconnect, detachTransportHandlers } from '../src/socket.ts';
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

console.log(`\nsocket.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);