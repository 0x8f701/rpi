#!/usr/bin/env node
// Pending-command lifecycle regression for src/pending.ts + the App.tsx wiring
// it encodes: per-command bounded timeout classes, immediate generation drain
// on socket close (truthful connection reason, never "timed out"), late
// response ignored after settlement, and the optimistic-bubble retention/
// removal contract. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Assertions drive the REAL registry/helpers with a fake clock + a mini App
// simulator (mirroring App.tsx sendCommand/onResponse/onclose/rejectPending
// Except) — not source strings.
import {
  PendingRegistry,
  commandTimeoutMs,
  timeoutErrorMessage,
  transportStaleMessage,
  DEFAULT_COMMAND_TIMEOUT_MS,
  LONG_OP_TIMEOUT_MS,
} from '../src/pending.ts';
import { isCurrentPending } from '../src/socket.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Deterministic clock + timer queue: advance() runs every timer due at or
 *  before now+ms in due order, then lands on the target time (timers armed by
 *  fired timers within the window also run). */
class FakeScheduler {
  time = 0;
  seq = 0;
  timers = new Map();
  setTimeout(fn, ms) {
    const id = ++this.seq;
    this.timers.set(id, { fn, at: this.time + ms });
    return id;
  }
  clearTimeout(timer) {
    this.timers.delete(timer);
  }
  now() { return this.time; }
  advance(ms) {
    const target = this.time + ms;
    for (;;) {
      let due = null;
      for (const [id, timer] of this.timers) {
        if (timer.at <= target && (due === null || timer.at < due.at || (timer.at === due.at && id < due.id))) {
          due = { id, at: timer.at };
        }
      }
      if (due === null) break;
      const timer = this.timers.get(due.id);
      this.timers.delete(due.id);
      this.time = due.at;
      timer.fn();
    }
    this.time = target;
  }
}

// ---- timeout classes: per-command, bounded, with the fixed-30s regressions ----
{
  check('unknown/fast-ack commands keep the 30s default', commandTimeoutMs('get_state') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('prompt is a fast ACK (turn streams as events) -> default', commandTimeoutMs('prompt') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('steer -> default', commandTimeoutMs('steer') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('follow_up -> default', commandTimeoutMs('follow_up') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('task_spawn -> default', commandTimeoutMs('task_spawn') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('code_review_open is snapshot/fork only (no LLM) -> default', commandTimeoutMs('code_review_open') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('code_review_snapshot -> default', commandTimeoutMs('code_review_snapshot') === DEFAULT_COMMAND_TIMEOUT_MS);
  // The old fixed 30s failed REAL compactions (wait_for_idle + summarization).
  check('compact gets the long bounded class', commandTimeoutMs('compact') === LONG_OP_TIMEOUT_MS);
  check('snapcompact gets the long bounded class', commandTimeoutMs('snapcompact') === LONG_OP_TIMEOUT_MS);
  check('every class is bounded and positive',
    DEFAULT_COMMAND_TIMEOUT_MS > 0 && LONG_OP_TIMEOUT_MS > DEFAULT_COMMAND_TIMEOUT_MS && Number.isFinite(LONG_OP_TIMEOUT_MS));
}

// ---- timeout message: names the command + elapsed seconds ----
{
  check('message names command and elapsed', timeoutErrorMessage('prompt', 0, 42_000) === 'prompt timed out after 42s (no response from server)');
  check('elapsed rounds to whole seconds', timeoutErrorMessage('compact', 1_000, 42_500) === 'compact timed out after 42s (no response from server)');
  check('elapsed floors at 0 for a clamped clock', timeoutErrorMessage('get_state', 10_000, 5_000) === 'get_state timed out after 0s (no response from server)');
  check('exact window reports the bound', timeoutErrorMessage('get_state', 0, 30_000) === 'get_state timed out after 30s (no response from server)');
  // Transport-stale message: truthful about an unresponsive OPEN transport
  // (names the command + elapsed, says reconnecting) — never "command timed
  // out", which hides a dead-but-OPEN socket behind a per-command timeout.
  check('transport-stale message names command + elapsed + reconnecting',
    transportStaleMessage('prompt', 0, 30_000) === 'connection unresponsive after 30s (no ack for prompt); reconnecting');
  check('transport-stale elapsed rounds to whole seconds',
    transportStaleMessage('get_state', 1_000, 30_500) === 'connection unresponsive after 30s (no ack for get_state); reconnecting');
  check('transport-stale elapsed floors at 0 for a clamped clock',
    transportStaleMessage('prompt', 10_000, 5_000) === 'connection unresponsive after 0s (no ack for prompt); reconnecting');
}

// ---- registry primitives: timeout fires once, late response is a no-op ----
{
  const sched = new FakeScheduler();
  const timedOut = [];
  const registry = new PendingRegistry({ scheduler: sched, onTimeout: (e) => timedOut.push(e.id) });
  const settled = [];
  const resolve = (value) => settled.push(['resolve', value]);
  const reject = (error) => settled.push(['reject', error.message]);
  registry.add('c1', { resolve, reject, gen: 1, type: 'get_state' });
  check('registered pending is present', registry.has('c1') && registry.size === 1);
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('timeout fired the bubble hook (id passed)', timedOut.length === 1 && timedOut[0] === 'c1');
  check('timeout rejected with name + elapsed', settled.length === 1 && settled[0][0] === 'reject' && settled[0][1] === 'get_state timed out after 30s (no response from server)', JSON.stringify(settled));
  check('timed-out entry removed', !registry.has('c1') && registry.size === 0);
  // A LATE response after the timeout must be ignored (take -> undefined).
  check('late response after timeout is a no-op', registry.take('c1') === undefined);
  check('late response did not resolve the promise', settled.length === 1);
}

// ---- take() clears the timer: a settled command never times out later ----
{
  const sched = new FakeScheduler();
  const timedOut = [];
  const registry = new PendingRegistry({ scheduler: sched, onTimeout: (entry) => timedOut.push(entry.id) });
  const resolve = () => {};
  const reject = () => {};
  registry.add('c1', { resolve, reject, gen: 1, type: 'get_state' });
  const entry = registry.take('c1');
  check('take returns the entry for settlement', entry !== undefined && entry.type === 'get_state');
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('settled entry never fires its timeout', timedOut.length === 0 && !registry.has('c1'));
}

// ---- drainExcept: socket close/replace rejects + deletes, clears timers,
//      keeps bubbles (frame may have been delivered; retry owns semantics) ----
{
  const sched = new FakeScheduler();
  const timedOut = [];
  const registry = new PendingRegistry({ scheduler: sched, onTimeout: (e) => timedOut.push(e.id) });
  const settled = [];
  function send(id, gen, type) {
    registry.add(id, {
      resolve: (value) => settled.push([id, 'resolve', value]),
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true]),
      gen,
      type,
    });
  }
  send('old', 1, 'prompt');
  send('keep', 2, 'get_state');
  const reason = 'connection closed (code 1008: policy violation)';
  registry.drainExcept(2, reason);
  check('old-gen pending rejected with the connection reason (not timeout)', settled.some((s) => s[0] === 'old' && s[1] === 'reject' && s[2] === reason), JSON.stringify(settled));
  check('connection rejection surfaces to the send catch (rpc=false)', settled.some((s) => s[0] === 'old' && s[3] === false), JSON.stringify(settled));
  check('drained entry deleted', !registry.has('old'));
  check('keep-gen pending untouched', registry.has('keep') && registry.size === 1);
  // Timers cleared: advancing past the deadline must not double-reject.
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('drained timer cleared (no double reject)', settled.filter((s) => s[0] === 'old').length === 1, JSON.stringify(settled));
  check('drain never fires the bubble hook for the drained entry', !timedOut.includes('old'), JSON.stringify(timedOut));
}

// ---- drainAll: host switch rejects every pending as rpc (quiet catches) ----
{
  const sched = new FakeScheduler();
  const timedOut = [];
  const registry = new PendingRegistry({ scheduler: sched, onTimeout: (e) => timedOut.push(e.id) });
  const settled = [];
  for (const id of ['a', 'b']) {
    registry.add(id, {
      resolve: () => settled.push([id, 'resolve']),
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true]),
      gen: 1,
      type: 'get_state',
    });
  }
  registry.drainAll('host switched', true);
  check('drainAll rejected every pending with rpc=true', settled.length === 2 && settled.every((s) => s[1] === 'reject' && s[3] === true && s[2] === 'host switched'), JSON.stringify(settled));
  check('drainAll emptied the registry', registry.size === 0);
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('drainAll cleared timers (no double reject)', settled.length === 2);
  check('drainAll never fires the bubble hook', timedOut.length === 0);
}

// ---- regression: a legit 45s compaction must NOT time out (old fixed 30s did) ----
{
  const sched = new FakeScheduler();
  const timedOut = [];
  const registry = new PendingRegistry({ scheduler: sched, onTimeout: (e) => timedOut.push(e.id) });
  const settled = [];
  registry.add('compact-1', {
    resolve: (value) => settled.push(['resolve', value]),
    reject: (error) => settled.push(['reject', error.message]),
    gen: 1,
    type: 'compact',
  });
  sched.advance(45_000); // compaction legitimately takes minutes
  check('45s compaction still pending (long class)', registry.has('compact-1') && timedOut.length === 0, `timedOut=${JSON.stringify(timedOut)}`);
  const completed = registry.take('compact-1');
  completed.resolve({ report: 'done' });
  check('45s compaction settles successfully via its response', settled.length === 1 && settled[0][0] === 'resolve');
  // And the bound still holds: a truly stuck compaction DOES reject.
  const stuck = [];
  registry.add('compact-2', {
    resolve: () => {},
    reject: (error) => stuck.push(error.message),
    gen: 1,
    type: 'compact',
  });
  sched.advance(LONG_OP_TIMEOUT_MS);
  check('stuck compaction rejects at the long bound', stuck.length === 1 && stuck[0] === 'compact timed out after 600s (no response from server)', JSON.stringify(stuck));
}

// ---- transport-stale: current-gen fast-ack timeout fails closed ----
// Reproduced fact: the prompt frame is swallowed while the WebSocket stays OPEN; the
// 30s pending timer fires before the 60s liveness timer. A current-generation
// fast-ack timeout must be treated as an unresponsive transport, not a command
// stall: fire onTransportStale exactly once and reject with a truthful
// connection-unresponsive error so the App can close/replace the socket and
// reconnect through the existing onclose path.
{
  const sched = new FakeScheduler();
  const transportStale = [];
  const timedOut = [];
  let currentGen = 7;
  const registry = new PendingRegistry({
    scheduler: sched,
    isCurrentGen: (g) => g === currentGen,
    onTimeout: (e) => timedOut.push(e.id),
    onTransportStale: (e) => transportStale.push(e.id),
  });
  const settled = [];
  registry.add('c1', {
    resolve: (value) => settled.push(['resolve', value]),
    reject: (error) => settled.push(['reject', error.message]),
    bubbleId: 'u-c1',
    gen: currentGen,
    type: 'prompt',
  });
  check('transport-stale entry registered', registry.has('c1') && registry.size === 1);
  // Advance PAST the fast-ack bound but BEFORE the 60s liveness window: this
  // is exactly the reproduced window (30,021ms pending fire vs 60s heartbeat).
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('current-gen fast-ack timeout fires onTransportStale exactly once',
    transportStale.length === 1 && transportStale[0] === 'c1', JSON.stringify(transportStale));
  check('current-gen fast-ack timeout does NOT fire the plain onTimeout hook',
    timedOut.length === 0, JSON.stringify(timedOut));
  check('transport-stale rejects with a truthful connection-unresponsive message',
    settled.length === 1 && settled[0][0] === 'reject' &&
      settled[0][1] === 'connection unresponsive after 30s (no ack for prompt); reconnecting',
    JSON.stringify(settled));
  check('transport-stale rejection is NOT labeled a command timeout',
    !settled[0][1].includes('timed out'), JSON.stringify(settled));
  check('transport-stale entry removed (no double settlement)', !registry.has('c1') && registry.size === 0);
  // A LATE response after the transport-stale rejection must be a no-op.
  const beforeLate = settled.length;
  check('late response after transport-stale is a no-op', registry.take('c1') === undefined);
  check('late response did not double-settle the promise', settled.length === beforeLate);
}

// ---- long-op timeout stays bounded WITHOUT an eager transport close ----
// A legit compaction runs minutes; its 10-minute bound is a plain timeout,
// never onTransportStale, so a long operation never closes a healthy socket
// just because it ran long.
{
  const sched = new FakeScheduler();
  const transportStale = [];
  const timedOut = [];
  let currentGen = 1;
  const registry = new PendingRegistry({
    scheduler: sched,
    isCurrentGen: (g) => g === currentGen,
    onTimeout: (e) => timedOut.push(e.id),
    onTransportStale: (e) => transportStale.push(e.id),
  });
  const settled = [];
  registry.add('compact-1', {
    resolve: (value) => settled.push(['resolve', value]),
    reject: (error) => settled.push(['reject', error.message]),
    gen: currentGen,
    type: 'compact',
  });
  // A 45s compaction is still in flight (long class) — no transport-stale.
  sched.advance(45_000);
  check('45s compaction still pending (no transport-stale, no timeout)',
    registry.has('compact-1') && transportStale.length === 0 && timedOut.length === 0,
    `transportStale=${JSON.stringify(transportStale)} timedOut=${JSON.stringify(timedOut)}`);
  // The long bound fires as a PLAIN timeout — never onTransportStale.
  sched.advance(LONG_OP_TIMEOUT_MS - 45_000);
  check('stuck compaction rejects at the long bound as a PLAIN timeout',
    settled.length === 1 && settled[0][0] === 'reject' &&
      settled[0][1] === 'compact timed out after 600s (no response from server)',
    JSON.stringify(settled));
  check('long-op timeout NEVER fires onTransportStale (no eager transport close)',
    transportStale.length === 0, JSON.stringify(transportStale));
  check('long-op timeout fires the plain onTimeout hook', timedOut.length === 1 && timedOut[0] === 'compact-1', JSON.stringify(timedOut));
}

// ---- stale-generation fast-ack timeout CANNOT close the current transport ----
// A pending command whose socket was already replaced/closed must never close
// the CURRENT socket when its leftover timer fires: it rejects as a plain
// timeout and the newer, healthy socket is untouched.
{
  const sched = new FakeScheduler();
  const transportStale = [];
  const timedOut = [];
  let currentGen = 5; // the CURRENT healthy socket
  const registry = new PendingRegistry({
    scheduler: sched,
    isCurrentGen: (g) => g === currentGen,
    onTimeout: (e) => timedOut.push(e.id),
    onTransportStale: (e) => transportStale.push(e.id),
  });
  const settled = [];
  // Sent on the OLD (already-replaced) generation 4; current is 5.
  registry.add('stale', {
    resolve: (value) => settled.push(['resolve', value]),
    reject: (error) => settled.push(['reject', error.message]),
    gen: 4,
    type: 'prompt',
  });
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('stale-gen fast-ack timeout does NOT fire onTransportStale (no transport close)',
    transportStale.length === 0, JSON.stringify(transportStale));
  check('stale-gen fast-ack timeout fires the PLAIN onTimeout hook',
    timedOut.length === 1 && timedOut[0] === 'stale', JSON.stringify(timedOut));
  check('stale-gen timeout rejects as a plain command timeout',
    settled.length === 1 && settled[0][0] === 'reject' &&
      settled[0][1] === 'prompt timed out after 30s (no response from server)',
    JSON.stringify(settled));
  // The current generation is unchanged — a stale-gen timeout never touches
  // the healthy socket's state.
  check('stale-gen timeout leaves the current generation untouched', currentGen === 5);
}

// ---- healthy response cancels the transport-stale timer (no false close) ----
// A fast-ack command that IS answered in time must never trip onTransportStale:
// take() clears the timer, so a healthy connection is never misjudged dead.
{
  const sched = new FakeScheduler();
  const transportStale = [];
  const timedOut = [];
  let currentGen = 1;
  const registry = new PendingRegistry({
    scheduler: sched,
    isCurrentGen: (g) => g === currentGen,
    onTimeout: (e) => timedOut.push(e.id),
    onTransportStale: (e) => transportStale.push(e.id),
  });
  const settled = [];
  registry.add('healthy', {
    resolve: (value) => settled.push(['resolve', value]),
    reject: (error) => settled.push(['reject', error.message]),
    gen: currentGen,
    type: 'get_state',
  });
  // Answered at 5s (well inside the 30s fast-ack window): take() settles + clears.
  sched.advance(5_000);
  const entry = registry.take('healthy');
  check('healthy response take() returns the entry', entry !== undefined && entry.type === 'get_state');
  entry.resolve({ ok: 1 });
  check('healthy response resolves the promise', settled.length === 1 && settled[0][0] === 'resolve');
  // Advance past the fast-ack bound: the cleared timer must not fire.
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('healthy response cancelled the transport-stale timer (no false close)',
    transportStale.length === 0 && timedOut.length === 0,
    `transportStale=${JSON.stringify(transportStale)} timedOut=${JSON.stringify(timedOut)}`);
  check('cleared timer did not double-settle', settled.length === 1);
}

// ---- App wiring simulator: bubble retention/removal contract end to end ----
function simulate() {
  let gen = 0; // socketGenRef.current
  const sched = new FakeScheduler();
  const bubbles = new Set();
  const settled = []; // [id, kind, message?, rpc?]
  const timedOut = [];
  const registry = new PendingRegistry({
    scheduler: sched,
    onTimeout: (e) => {
      timedOut.push(e.id);
      if (e.bubbleId) bubbles.delete(e.bubbleId); // App: removeItem(bubbleId)
    },
  });
  function send(id, type, bubbleId, socketGen = gen) {
    if (bubbleId) bubbles.add(bubbleId);
    registry.add(id, {
      resolve: () => settled.push([id, 'resolve']),
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true]),
      bubbleId,
      gen: socketGen,
      type,
    });
  }
  // App onResponse, verbatim logic (take + generation guard + bubble/rpc).
  function onResponse(frame) {
    const entry = registry.take(frame.id);
    if (!entry) return;
    if (!isCurrentPending(entry.gen, gen)) return;
    if (frame.success) {
      entry.resolve(frame.data);
    } else {
      if (entry.bubbleId) bubbles.delete(entry.bubbleId);
      const error = new Error(frame.error || 'rpc failed');
      error.rpc = true;
      entry.reject(error);
    }
  }
  // App onclose: bump gen, then drain every pending of the closing socket.
  function onClose(code, reason) {
    gen += 1;
    const drainReason = `connection closed (code ${code})${reason ? `: ${reason}` : ''}`;
    registry.drainExcept(gen, drainReason);
  }
  // App connect() replace drain (defensive path; rpc=true, bootstrap stays quiet).
  function connectReplace() {
    gen += 1;
    registry.drainExcept(gen, 'connection replaced', true);
  }
  return { send, onResponse, onClose, connectReplace, sched, bubbles, settled, timedOut, registry, gen: () => gen };
}

{
  const sim = simulate();

  // Success: bubble RETAINED, promise resolves, timer cleared.
  sim.send('ok', 'prompt', 'u-ok');
  sim.onResponse({ id: 'ok', success: true, data: { ok: 1 } });
  check('success resolves the promise', sim.settled.some((s) => s[0] === 'ok' && s[1] === 'resolve'), JSON.stringify(sim.settled));
  check('success KEEPS the optimistic bubble', sim.bubbles.has('u-ok'));
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('success cleared the timeout timer', sim.timedOut.length === 0);

  // Server error: bubble REMOVED, rejected with rpc=true (quiet catches).
  sim.send('err', 'get_state', 'u-err');
  sim.onResponse({ id: 'err', success: false, error: 'boom' });
  check('rpc failure rejects with rpc=true', sim.settled.some((s) => s[0] === 'err' && s[1] === 'reject' && s[3] === true && s[2] === 'boom'), JSON.stringify(sim.settled));
  check('rpc failure REMOVES the optimistic bubble', !sim.bubbles.has('u-err'));

  // Timeout: bubble REMOVED, rejected with name + elapsed, late response ignored.
  sim.send('slow', 'get_state', 'u-slow');
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('timeout rejected with command name + elapsed', sim.settled.some((s) => s[0] === 'slow' && s[1] === 'reject' && s[2] === 'get_state timed out after 30s (no response from server)'), JSON.stringify(sim.settled));
  check('timeout REMOVES the optimistic bubble', !sim.bubbles.has('u-slow'));
  sim.onResponse({ id: 'slow', success: true, data: {} });
  check('late response after timeout is ignored', sim.settled.filter((s) => s[0] === 'slow').length === 1, JSON.stringify(sim.settled));

  // Socket close: immediate drain with the truthful connection reason,
  // not a timeout; bubble retained for retry; rpc=false surfaces the error.
  sim.send('drop', 'prompt', 'u-drop');
  sim.onClose(1008, 'policy violation');
  check('close drains pending with the connection reason', sim.settled.some((s) => s[0] === 'drop' && s[1] === 'reject' && s[2] === 'connection closed (code 1008): policy violation'), JSON.stringify(sim.settled));
  check('close rejection is NOT labeled a timeout', sim.settled.some((s) => s[0] === 'drop') && !sim.settled.some((s) => s[0] === 'drop' && s[2].includes('timed out')), JSON.stringify(sim.settled));
  check('close rejection surfaces to the send catch (rpc=false)', sim.settled.some((s) => s[0] === 'drop' && s[3] === false), JSON.stringify(sim.settled));
  check('close KEEPS the optimistic bubble (retry semantics)', sim.bubbles.has('u-drop'));
  check('drained entry removed immediately', !sim.registry.has('drop'));
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('close cleared the timeout timer (no late timeout reject)', sim.settled.filter((s) => s[0] === 'drop').length === 1, JSON.stringify(sim.settled));
  sim.onResponse({ id: 'drop', success: true, data: {} });
  check('late response after close drain is ignored', sim.settled.filter((s) => s[0] === 'drop').length === 1);

  // Replace drain (connect path): rpc=true (bootstrap stays quiet), bubble kept.
  sim.send('rep', 'get_state', 'u-rep');
  sim.connectReplace();
  check('replace drain rejects with rpc=true', sim.settled.some((s) => s[0] === 'rep' && s[1] === 'reject' && s[3] === true && s[2] === 'connection replaced'), JSON.stringify(sim.settled));
  check('replace drain KEEPS the bubble', sim.bubbles.has('u-rep'));
}

// ---- transport-stale end to end: fast-ack timeout closes the socket and
//      reconnects through the existing onclose path ----
// Mirrors App.tsx's onTransportStale + onclose wiring: a current-generation
// fast-ack timeout while OPEN fires onTransportStale, which closes the socket
// (code 4001); onclose then bumps the generation, drains remaining pending
// with the truthful connection reason (bubbles kept), and the reconnect is
// owned by that path. The timed-out command rejects with the truthful
// connection-unresponsive message (NOT a command timeout) and KEEPS its
// bubble (close/retry semantics).
const TRANSPORT_STALE_CLOSE_CODE = 4001;
function simulateTransportStale() {
  let gen = 0; // socketGenRef.current
  const sched = new FakeScheduler();
  const bubbles = new Set();
  const settled = []; // [id, kind, message?, rpc?]
  const timedOut = [];
  const transportStale = []; // ids that tripped onTransportStale (registry: <=1 per gen)
  const toasts = []; // { message, error } — one per proactive close
  const closed = []; // { code, reason } — sockets proactively closed by the hook
  const registry = new PendingRegistry({
    scheduler: sched,
    isCurrentGen: (g) => g === gen,
    onTimeout: (e) => {
      timedOut.push(e.id);
      if (e.bubbleId) bubbles.delete(e.bubbleId);
    },
    onTransportStale: (e) => {
      // App.tsx onTransportStale: the registry fires this AT MOST ONCE per
      // generation (it eagerly settles every other current-gen fast-ack
      // pending on the same dead socket with the same truthful message), so
      // the close + toast happen exactly once. Bubble is NOT removed here
      // (close/retry semantics, matching the onclose drain).
      transportStale.push(e.id);
      toasts.push({ message: 'connection unresponsive, reconnecting…', error: true });
      closed.push({ code: TRANSPORT_STALE_CLOSE_CODE, reason: 'connection unresponsive' });
    },
  });
  function send(id, type, bubbleId, socketGen = gen) {
    if (bubbleId) bubbles.add(bubbleId);
    registry.add(id, {
      resolve: () => settled.push([id, 'resolve']),
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true]),
      bubbleId,
      gen: socketGen,
      type,
    });
  }
  function onResponse(frame) {
    const entry = registry.take(frame.id);
    if (!entry) return;
    if (!isCurrentPending(entry.gen, gen)) return;
    if (frame.success) entry.resolve(frame.data);
    else {
      if (entry.bubbleId) bubbles.delete(entry.bubbleId);
      const error = new Error(frame.error || 'rpc failed');
      error.rpc = true;
      entry.reject(error);
    }
  }
  // App onclose: the proactive transport-stale close arrives here. Bump gen,
  // drain every pending of the closing socket with the truthful reason.
  function onClose(code, reason) {
    gen += 1;
    const drainReason = `connection closed (code ${code})${reason ? `: ${reason}` : ''}`;
    registry.drainExcept(gen, drainReason);
  }
  return { send, onResponse, onClose, sched, bubbles, settled, timedOut, transportStale, toasts, closed, registry, gen: () => gen };
}

{
  const sim = simulateTransportStale();
  const sched = sim.sched;

  // A prompt is sent on the current (OPEN) socket; the server swallows it.
  sim.send('p1', 'prompt', 'u-p1');
  // A second in-flight fast-ack on the same socket — collateral that would
  // otherwise fire its own onTransportStale moments later (duplicate close/
  // toast) or sit unbounded until the async onclose drain.
  sim.send('p2', 'get_state', 'u-p2');
  // A long-op on the same dead socket — it must NOT be eagerly settled by the
  // transport-stale path; the onclose drain owns it (connection reason, bubble
  // kept). This guards the "long op never eagerly reconnects" invariant.
  sim.send('c3', 'compact', 'u-c3');
  // Advance PAST the 30s fast-ack bound but BEFORE the 60s liveness window.
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  // The first fast-ack to time out trips onTransportStale EXACTLY ONCE and
  // closes the socket (code 4001). The registry then eagerly settles every
  // other current-gen fast-ack pending (p2) with the same truthful message —
  // onTransportStale never fires twice for one dead socket.
  check('transport-stale fired onTransportStale exactly once', sim.transportStale.length === 1, JSON.stringify(sim.transportStale));
  check('transport-stale closed the socket with code 4001',
    sim.closed.length === 1 && sim.closed[0].code === TRANSPORT_STALE_CLOSE_CODE, JSON.stringify(sim.closed));
  check('transport-stale toasted a truthful connection-unresponsive message exactly once',
    sim.toasts.length === 1 && sim.toasts[0].message === 'connection unresponsive, reconnecting…' && sim.toasts[0].error === true,
    JSON.stringify(sim.toasts));

  // Both fast-ack commands reject with the truthful connection-unresponsive
  // error (NOT a generic command timeout), and KEEP their bubbles (close/retry).
  const p1Reject = sim.settled.filter((s) => s[0] === 'p1');
  const p2Reject = sim.settled.filter((s) => s[0] === 'p2');
  check('transport-stale command rejected with the truthful connection-unresponsive message',
    p1Reject.length === 1 && p1Reject[0][1] === 'reject' &&
      p1Reject[0][2] === 'connection unresponsive after 30s (no ack for prompt); reconnecting',
    JSON.stringify(sim.settled));
  check('collateral fast-ack eagerly settled with the SAME truthful message (no duplicate callback)',
    p2Reject.length === 1 && p2Reject[0][1] === 'reject' &&
      p2Reject[0][2] === 'connection unresponsive after 30s (no ack for get_state); reconnecting',
    JSON.stringify(sim.settled));
  check('transport-stale rejection is NOT labeled a command timeout',
    p1Reject.length === 1 && !p1Reject[0][2].includes('timed out'), JSON.stringify(sim.settled));
  check('collateral rejection is NOT labeled a command timeout',
    p2Reject.length === 1 && !p2Reject[0][2].includes('timed out'), JSON.stringify(sim.settled));
  check('transport-stale command KEEPS its bubble (close/retry semantics)', sim.bubbles.has('u-p1'));
  check('collateral fast-ack KEEPS its bubble (close/retry semantics)', sim.bubbles.has('u-p2'));
  // No collateral fast-ack left unbounded awaiting onclose: the registry
  // settled p2 immediately (its timer is cleared, the entry is gone).
  check('collateral fast-ack entry removed immediately (no unbounded wait)', !sim.registry.has('p2'));

  // The long-op (c3) is NOT eagerly settled by the transport-stale path: it
  // stays pending for the onclose drain (its 10-minute bound is a legitimately
  // long class; the imminent close drains it with the connection reason).
  check('long-op collateral NOT eagerly settled by transport-stale (still pending for onclose)',
    sim.registry.has('c3'), JSON.stringify(sim.settled));

  // The proactive close now drives the existing onclose path: bump gen, drain
  // every remaining pending of the closing socket with the truthful connection
  // reason (bubbles kept), exactly like a real drop. Only the long-op remains.
  sim.onClose(TRANSPORT_STALE_CLOSE_CODE, 'connection unresponsive');
  const c3Reject = sim.settled.filter((s) => s[0] === 'c3');
  check('onclose drains the long-op collateral with the connection reason',
    c3Reject.length === 1 && c3Reject[0][1] === 'reject' &&
      c3Reject[0][2] === 'connection closed (code 4001): connection unresponsive',
    JSON.stringify(sim.settled));
  check('long-op collateral drain is NOT labeled a timeout',
    c3Reject.length === 1 && !c3Reject[0][2].includes('timed out'), JSON.stringify(sim.settled));
  check('long-op collateral drain KEEPS the bubble (retry semantics)', sim.bubbles.has('u-c3'));
  check('long-op collateral drain surfaces to the send catch (rpc=false)', c3Reject[0][3] === false, JSON.stringify(sim.settled));

  // Generation bumped: the closing socket's generation is superseded.
  check('onclose bumped the socket generation', sim.gen() === 1);
  // No double settlement: a late response for any command is a no-op.
  const beforeLate = sim.settled.length;
  sim.onResponse({ id: 'p1', success: true, data: {} });
  sim.onResponse({ id: 'p2', success: true, data: {} });
  sim.onResponse({ id: 'c3', success: true, data: {} });
  check('late responses after transport-stale + drain are no-ops', sim.settled.length === beforeLate, JSON.stringify(sim.settled));
  // The plain onTimeout hook never fired for any command (transport-stale and
  // drain own settlement; no bubble removal via the plain path).
  check('transport-stale path never fired the plain onTimeout hook', sim.timedOut.length === 0, JSON.stringify(sim.timedOut));
  // Advancing well past every bound must not double-settle (timers cleared).
  sched.advance(LONG_OP_TIMEOUT_MS);
  check('no late timer double-settles any command', sim.settled.length === beforeLate, JSON.stringify(sim.settled));
}

// ---- transport-stale isolation: a stale-gen fast-ack timeout does NOT close
//      the current socket (the newer, healthy generation is untouched) ----
{
  const sim = simulateTransportStale();
  const sched = sim.sched;
  // A healthy current socket exists at gen 0; bump to gen 1 to model a prior
  // socket already replaced, then send a leftover fast-ack on the OLD gen 0.
  sim.onClose(1000, 'replaced'); // gen -> 1, drains gen-0 pending (none yet)
  sim.send('leftover', 'prompt', 'u-leftover', 0); // stale gen 0
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('stale-gen fast-ack timeout does NOT fire onTransportStale', sim.transportStale.length === 0, JSON.stringify(sim.transportStale));
  check('stale-gen fast-ack timeout does NOT close the current socket', sim.closed.length === 0, JSON.stringify(sim.closed));
  check('stale-gen fast-ack timeout fires the PLAIN onTimeout hook', sim.timedOut.length === 1 && sim.timedOut[0] === 'leftover', JSON.stringify(sim.timedOut));
  check('stale-gen timeout rejects as a plain command timeout (truthful for a leftover)',
    sim.settled.some((s) => s[0] === 'leftover' && s[1] === 'reject' && s[2] === 'prompt timed out after 30s (no response from server)'),
    JSON.stringify(sim.settled));
  // The current generation is unchanged — the healthy socket was never touched.
  check('stale-gen timeout leaves the current generation untouched', sim.gen() === 1);
}

console.log(`\npending.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
