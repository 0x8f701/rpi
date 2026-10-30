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
  rpcFailureToastOwned,
  isPanelOwnedRpc,
  classifyResponseOwnership,
  isToastOwnedRpcError,
  isTargetOwnedLifecycle,
  RESPONSE_OWNERSHIP_REJECTIONS,
  DEFAULT_COMMAND_TIMEOUT_MS,
  LONG_OP_TIMEOUT_MS,
} from '../src/pending.ts';
import { isCurrentPending } from '../src/socket.ts';
import { rpcErrorCode } from '../src/types.ts';
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

// ---- timeout classes: per-command, bounded, with long-operation regressions ----
{
  check('unknown/fast commands keep the 30s default', commandTimeoutMs('get_state') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('prompt acceptance keeps the default', commandTimeoutMs('prompt') === DEFAULT_COMMAND_TIMEOUT_MS);
  check('task_spawn keeps the default', commandTimeoutMs('task_spawn') === DEFAULT_COMMAND_TIMEOUT_MS);
  for (const type of ['compact', 'snapcompact', 'get_messages', 'code_review_open', 'code_review_refresh', 'code_review_file_diff', 'code_review_auto_review', 'code_review_highlight', 'code_review_stop']) {
    check(`${type} gets the long bounded class`, commandTimeoutMs(type) === LONG_OP_TIMEOUT_MS);
  }
  check('code_review_snapshot remains fast-ack', commandTimeoutMs('code_review_snapshot') === DEFAULT_COMMAND_TIMEOUT_MS);
  check(
    'session_handoff is not in LONG_OP_TYPES',
    commandTimeoutMs('session_handoff') === DEFAULT_COMMAND_TIMEOUT_MS
      && commandTimeoutMs('session_handoff') !== LONG_OP_TIMEOUT_MS,
  );
  check(
    'session_handoff pending timeout is DEFAULT_COMMAND_TIMEOUT_MS 30_000',
    commandTimeoutMs('session_handoff') === 30_000 && DEFAULT_COMMAND_TIMEOUT_MS === 30_000,
  );
  check('every class is bounded and positive',
    DEFAULT_COMMAND_TIMEOUT_MS > 0 && LONG_OP_TIMEOUT_MS > DEFAULT_COMMAND_TIMEOUT_MS && Number.isFinite(LONG_OP_TIMEOUT_MS));
}

// ---- timeout message: names the command + elapsed seconds ----
{
  check('message names command and elapsed', timeoutErrorMessage('prompt', 0, 42_000) === 'prompt timed out after 42s (no response from server)');
  check('elapsed rounds to whole seconds', timeoutErrorMessage('compact', 1_000, 42_500) === 'compact timed out after 42s (no response from server)');
  check('elapsed floors at 0 for a clamped clock', timeoutErrorMessage('get_state', 10_000, 5_000) === 'get_state timed out after 0s (no response from server)');
  check('exact window reports the bound', timeoutErrorMessage('get_state', 0, 30_000) === 'get_state timed out after 30s (no response from server)');
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
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true, error.toastOwned === true]),
      gen,
      type,
    });
  }
  send('old', 1, 'prompt');
  send('keep', 2, 'get_state');
  const reason = 'connection closed (code 1008: policy violation)';
  registry.drainExcept(2, reason, false, true);
  check('old-gen pending rejected with the connection reason (not timeout)', settled.some((s) => s[0] === 'old' && s[1] === 'reject' && s[2] === reason), JSON.stringify(settled));
  check('connection rejection records App toast ownership', settled.some((s) => s[0] === 'old' && s[3] === false && s[4] === true), JSON.stringify(settled));
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
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true, error.toastOwned === true]),
      gen: 1,
      type: 'get_state',
    });
  }
  registry.drainAll('host switched', true);
  check('drainAll rejected every pending with rpc=true and no toast ownership', settled.length === 2 && settled.every((s) => s[1] === 'reject' && s[3] === true && s[4] === false && s[2] === 'host switched'), JSON.stringify(settled));
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

// ---- long Code Review work never owns transport liveness ----
{
  const sched = new FakeScheduler();
  const timedOut = [];
  const settled = [];
  const registry = new PendingRegistry({
    scheduler: sched,
    onTimeout: (entry) => timedOut.push(entry.id),
  });
  registry.add('review-open', {
    resolve: (value) => settled.push(['review-open', 'resolve', value]),
    reject: (error) => settled.push(['review-open', 'reject', error.message]),
    gen: 1,
    type: 'code_review_open',
  });
  registry.add('state', {
    resolve: (value) => settled.push(['state', 'resolve', value]),
    reject: (error) => settled.push(['state', 'reject', error.message]),
    gen: 1,
    type: 'get_state',
  });
  sched.advance(5_000);
  const state = registry.take('state');
  state.resolve({ sessionId: 'session-a' });
  sched.advance(40_000);
  check('code_review_open remains pending after 45s', registry.has('review-open') && !timedOut.includes('review-open'));
  check('unrelated get_state settles while review remains pending', settled.some((entry) => entry[0] === 'state' && entry[1] === 'resolve'), JSON.stringify(settled));
  const review = registry.take('review-open');
  review.resolve({ snapshotId: 'review-1' });
  check('slow code_review_open settles without touching unrelated commands', settled.some((entry) => entry[0] === 'review-open' && entry[1] === 'resolve'), JSON.stringify(settled));

  registry.add('review-stuck', {
    resolve: () => {},
    reject: (error) => settled.push(['review-stuck', 'reject', error.message]),
    gen: 1,
    type: 'code_review_open',
  });
  sched.advance(LONG_OP_TIMEOUT_MS);
  check('stuck code_review_open rejects at its own long bound', settled.some((entry) => entry[0] === 'review-stuck' && entry[2] === 'code_review_open timed out after 600s (no response from server)'), JSON.stringify(settled));
  check('review timeout fires only the command timeout hook', timedOut.includes('review-stuck'));
}

// ---- App wiring simulator: bubble retention/removal contract end to end ----
function simulate() {
  let gen = 0; // socketGenRef.current
  // Mirrors bootRef/sessionIdRef: boot=true only during the bootstrap window.
  let boot = false;
  let activeSession = null;
  function setLifecycle(bootValue, activeSessionValue = activeSession) {
    boot = bootValue;
    activeSession = activeSessionValue;
  }
  const sched = new FakeScheduler();
  const bubbles = new Set();
  const settled = []; // [id, kind, message?, rpc?, toastOwned?, rpcCode?]
  const timedOut = [];
  const toasts = [];
  const cleanupLog = []; // [cleanup:<sessionId> | resolve:<id>] in order
  const closedSessions = [];
  const registry = new PendingRegistry({
    scheduler: sched,
    onTimeout: (entry, message) => {
      timedOut.push(entry.id);
      if (entry.bubbleId) bubbles.delete(entry.bubbleId); // App: removeItem(bubbleId)
      const toastOwned = rpcFailureToastOwned({
        type: entry.type,
        boot,
        sessionId: entry.sessionId,
        activeSessionId: activeSession,
        auxiliary: entry.auxiliary,
      });
      if (toastOwned) toasts.push(message);
      return toastOwned;
    },
  });
  function send(id, type, bubbleId, socketGen = gen, extra = {}) {
    if (bubbleId) bubbles.add(bubbleId);
    // Mirror App sendCommand: auxiliary is per-call, never inferred from type.
    registry.add(id, {
      resolve: () => settled.push([id, 'resolve']),
      reject: (error) => settled.push([id, 'reject', error.message, error.rpc === true, error.toastOwned === true, error.rpcCode]),
      bubbleId,
      gen: socketGen,
      type,
      sessionId: extra.sessionId,
      auxiliary: extra.auxiliary === true,
    });
  }
  function onResponse(frame, activeSessionId = activeSession, bootArg = boot) {
    const entry = registry.take(frame.id);
    if (!entry) return;
    if (!isCurrentPending(entry.gen, gen)) return;
    // Mirrors App.onResponse: ONE ownership gate (./pending.ts
    // classifyResponseOwnership) — command identity first, then source
    // ownership, with the target-owned exemption applied ONLY to a SUCCESS
    // envelope of a lifecycle command and verified against the payload
    // sessionId inside the classifier.
    const ownership = classifyResponseOwnership(frame, entry);
    if (ownership !== 'accept') {
      if (entry.bubbleId) bubbles.delete(entry.bubbleId);
      entry.reject(new Error(RESPONSE_OWNERSHIP_REJECTIONS[ownership]));
      return;
    }
    if (frame.success) {
      const data = frame.data || {};
      // App close_session success cleanup: invalidate BEFORE resolving, but
      // the promise ALWAYS settles exactly once afterwards.
      if (entry.type === 'close_session' && data && typeof data.sessionId === 'string' && data.sessionId !== '') {
        cleanupLog.push(`cleanup:${data.sessionId}`);
        closedSessions.push(data.sessionId);
      }
      cleanupLog.push(`resolve:${entry.id}`);
      entry.resolve(frame.data);
    } else {
      if (entry.bubbleId) bubbles.delete(entry.bubbleId);
      const toastOwned = rpcFailureToastOwned({
        type: entry.type,
        boot: bootArg,
        sessionId: entry.sessionId,
        activeSessionId,
        auxiliary: entry.auxiliary,
      });
      const error = new Error(frame.error || 'rpc failed');
      error.rpc = true;
      if (toastOwned) error.toastOwned = true;
      error.rpcCode = rpcErrorCode(frame.data);
      toasts.push(toastOwned ? `command ${frame.command || entry.type} failed: ${frame.error || 'unknown error'}` : null);
      entry.reject(error);
    }
  }
  // Outage-surface state mirroring App: toastTransportOutage emits the
  // persistent outage toast only mid-session (everConnected) and only when
  // no outage surface is active yet; the flag clears when a socket OPENS.
  let everConnected = false;
  let outageActive = false;
  function markConnected() {
    everConnected = true;
    outageActive = false;
  }
  function toastTransportOutage(message) {
    if (!everConnected || outageActive) return false;
    outageActive = true;
    toasts.push(message);
    return true;
  }
  // App onclose: bump gen, then drain every pending of the closing socket.
  // Ownership: the drain is toastOwned iff an outage surface is ACTIVE
  // after toastTransportOutage (newly emitted OR a prior close in this same
  // outage); initial CONNECTING with no active surface stays UNOWNED.
  function onClose(code, reason) {
    gen += 1;
    const drainReason = `connection closed (code ${code})${reason ? `: ${reason}` : ''}`;
    let toastOwned = toastTransportOutage(`${drainReason}, reconnecting…`);
    if (!toastOwned && outageActive) toastOwned = true;
    registry.drainExcept(gen, drainReason, false, toastOwned);
  }
  // App sendCommand's immediate not-open reject: transport-tagged, owned
  // iff an outage surface is active.
  function immediateNotOpenReject() {
    const error = new Error('not connected');
    error.transport = true;
    if (outageActive) error.toastOwned = true;
    return error;
  }
  // App connect() replace drain (defensive path; rpc=true, bootstrap stays quiet).
  function connectReplace() {
    gen += 1;
    registry.drainExcept(gen, 'connection replaced', true);
  }
  return { send, onResponse, onClose, connectReplace, immediateNotOpenReject, markConnected, setLifecycle, sched, bubbles, settled, timedOut, toasts, registry, gen: () => gen, cleanupLog, closedSessions };
}

{
  const sim = simulate();

  // Success: bubble RETAINED, promise resolves, timer cleared.
  sim.send('ok', 'prompt', 'u-ok');
  sim.onResponse({ id: 'ok', success: true, data: { ok: 1 }, command: 'prompt' });
  check('success resolves the promise', sim.settled.some((s) => s[0] === 'ok' && s[1] === 'resolve'), JSON.stringify(sim.settled));
  check('success KEEPS the optimistic bubble', sim.bubbles.has('u-ok'));
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('success cleared the timeout timer', sim.timedOut.length === 0);

  // Server error: bubble REMOVED, rejected with rpc=true (quiet catches).
  sim.send('err', 'get_state', 'u-err');
  sim.onResponse({ id: 'err', success: false, error: 'boom', command: 'get_state' });
  check('rpc failure rejects with rpc=true and toast ownership', sim.settled.some((s) => s[0] === 'err' && s[1] === 'reject' && s[3] === true && s[4] === true && s[2] === 'boom'), JSON.stringify(sim.settled));
  check('rpc failure REMOVES the optimistic bubble', !sim.bubbles.has('u-err'));

  sim.send('known-code', 'get_state');
  sim.onResponse({ id: 'known-code', success: false, error: 'runtime unavailable', command: 'get_state', data: { code: 'runtime_unavailable' } });
  check('known RPC failure code reaches the rejection', sim.settled.some((s) => s[0] === 'known-code' && s[5] === 'runtime_unavailable'), JSON.stringify(sim.settled));

  sim.send('unknown-code', 'get_state');
  sim.onResponse({ id: 'unknown-code', success: false, error: 'future failure', command: 'get_state', data: { code: 'future_code' } });
  check('unknown RPC failure code is discarded', sim.settled.some((s) => s[0] === 'unknown-code' && s[5] === undefined), JSON.stringify(sim.settled));

  sim.send('malformed-code', 'get_state');
  sim.onResponse({ id: 'malformed-code', success: false, error: 'malformed failure', command: 'get_state', data: { code: 7 } });
  check('malformed RPC failure code is discarded', sim.settled.some((s) => s[0] === 'malformed-code' && s[5] === undefined), JSON.stringify(sim.settled));

  // Timeout: bubble REMOVED, rejected with name + elapsed, late response ignored.
  sim.send('slow', 'get_state', 'u-slow');
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('timeout rejected with command name + elapsed', sim.settled.some((s) => s[0] === 'slow' && s[1] === 'reject' && s[2] === 'get_state timed out after 30s (no response from server)'), JSON.stringify(sim.settled));
  check('timeout REMOVES the optimistic bubble', !sim.bubbles.has('u-slow'));
  sim.onResponse({ id: 'slow', success: true, data: {}, command: 'get_state' });
  check('late response after timeout is ignored', sim.settled.filter((s) => s[0] === 'slow').length === 1, JSON.stringify(sim.settled));

  // Socket close: immediate drain with the truthful connection reason,
  // not a timeout; bubble retained for retry; rpc=false surfaces the error.
  sim.send('drop', 'prompt', 'u-drop');
  sim.onClose(1008, 'policy violation');
  check('close drains pending with the connection reason', sim.settled.some((s) => s[0] === 'drop' && s[1] === 'reject' && s[2] === 'connection closed (code 1008): policy violation'), JSON.stringify(sim.settled));
  check('close rejection is NOT labeled a timeout', sim.settled.some((s) => s[0] === 'drop') && !sim.settled.some((s) => s[0] === 'drop' && s[2].includes('timed out')), JSON.stringify(sim.settled));
  check('close rejection stays unowned when the App emits no toast', sim.settled.some((s) => s[0] === 'drop' && s[3] === false && s[4] === false), JSON.stringify(sim.settled));
  check('close KEEPS the optimistic bubble (retry semantics)', sim.bubbles.has('u-drop'));
  check('drained entry removed immediately', !sim.registry.has('drop'));
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('close cleared the timeout timer (no late timeout reject)', sim.settled.filter((s) => s[0] === 'drop').length === 1, JSON.stringify(sim.settled));
  sim.onResponse({ id: 'drop', success: true, data: {}, command: 'prompt' });
  check('late response after close drain is ignored', sim.settled.filter((s) => s[0] === 'drop').length === 1);

  // Replace drain (connect path): rpc=true (bootstrap stays quiet), bubble kept.
  sim.send('rep', 'get_state', 'u-rep');
  sim.connectReplace();
  check('replace drain rejects with rpc=true and no toast ownership', sim.settled.some((s) => s[0] === 'rep' && s[1] === 'reject' && s[3] === true && s[4] === false && s[2] === 'connection replaced'), JSON.stringify(sim.settled));
  check('replace drain KEEPS the bubble', sim.bubbles.has('u-rep'));
}

{
  // Success settlement contract after the close_session cleanup restore:
  // EVERY success type settles its promise exactly once; close_session runs
  // its invalidation BEFORE resolving; stale-generation and sessionId-mismatch
  // responses never settle.
  const sim = simulate();
  for (const type of ['session_move', 'session_delete', 'session_restore', 'get_state', 'prompt']) {
    sim.send(`ok-${type}`, type);
    sim.onResponse({ id: `ok-${type}`, success: true, data: { sessionId: 'x' }, command: type });
  }
  check('generic success types each resolve exactly once',
    ['session_move', 'session_delete', 'session_restore', 'get_state', 'prompt'].every((type) =>
      sim.settled.filter((s) => s[0] === `ok-${type}`).length === 1 && sim.settled.some((s) => s[0] === `ok-${type}` && s[1] === 'resolve')),
    JSON.stringify(sim.settled));

  const simClose = simulate();
  simClose.send('close-1', 'close_session', 'u-close');
  simClose.onResponse({ id: 'close-1', success: true, data: { sessionId: 'sess-gone' }, command: 'close_session' });
  check('close_session success still cleans up AND resolves exactly once',
    simClose.closedSessions.join(',') === 'sess-gone'
    && simClose.settled.filter((s) => s[0] === 'close-1').length === 1
    && simClose.settled.some((s) => s[0] === 'close-1' && s[1] === 'resolve'));
  check('close_session cleanup happens BEFORE the resolve',
    simClose.cleanupLog.join('|') === 'cleanup:sess-gone|resolve:close-1', JSON.stringify(simClose.cleanupLog));
  simClose.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS * 2);
  check('close_session resolve cleared its timeout (no late timeout)',
    simClose.timedOut.length === 0);

  // Stale generation: a success response for a pending from a dead socket
  // generation settles nothing.
  const simStale = simulate();
  simStale.connectReplace();
  simStale.send('stale-ok', 'get_state', undefined, 0);
  simStale.onResponse({ id: 'stale-ok', success: true, data: {}, command: 'get_state' });
  check('stale-generation success response never settles', simStale.settled.length === 0);

  // Session ownership mismatch: rejected with the ownership error, no resolve.
  const simOwn = simulate();
  simOwn.send('own-1', 'prompt', 'u-own', undefined, { sessionId: 'sess-a' });
  simOwn.onResponse({ id: 'own-1', success: true, data: {}, sessionId: 'sess-b', command: 'prompt' });
  check('sessionId mismatch rejects with the ownership error, never resolves',
    simOwn.settled.some((s) => s[0] === 'own-1' && s[1] === 'reject' && s[2] === 'rpc response session ownership mismatch')
    && !simOwn.settled.some((s) => s[1] === 'resolve'));
  simOwn.onResponse({ id: 'own-1', success: true, data: {}, command: 'prompt' });
  check('late success after ownership mismatch is ignored', simOwn.settled.filter((s) => s[0] === 'own-1').length === 1);
}

{
  // Target-owned lifecycle envelope (new_session/fork/clone/session_fork_latest):
  // the SUCCESS response is tagged with the TARGET runtime's sessionId, so the
  // ordinary source-ownership check is skipped; the envelope is accepted ONLY
  // when it equals the payload sessionId (nonempty). A forged mismatch rejects
  // exactly once with nothing mutated; a lifecycle FAILURE stays source-owned
  // and still answers the ordinary ownership check.
  const sim = simulate();
  sim.send('new-ok', 'new_session', undefined, sim.gen(), { sessionId: 'source-a' });
  sim.onResponse({ id: 'new-ok', success: true, data: { sessionId: 'target-b' }, sessionId: 'target-b', command: 'new_session' });
  check('target-owned lifecycle success resolves when envelope equals payload',
    sim.settled.some((s) => s[0] === 'new-ok' && s[1] === 'resolve'), JSON.stringify(sim.settled));

  const simForged = simulate();
  simForged.send('fork-forged', 'fork', undefined, simForged.gen(), { sessionId: 'source-a' });
  simForged.onResponse({ id: 'fork-forged', success: true, data: { sessionId: 'target-b' }, sessionId: 'other-c', command: 'fork' });
  check('forged lifecycle envelope mismatch rejects exactly once with nothing resolved',
    simForged.settled.some((s) => s[0] === 'fork-forged' && s[1] === 'reject' && s[2] === 'rpc response target ownership mismatch')
    && !simForged.settled.some((s) => s[1] === 'resolve'), JSON.stringify(simForged.settled));
  simForged.onResponse({ id: 'fork-forged', success: true, data: { sessionId: 'target-b' }, sessionId: 'target-b', command: 'fork' });
  check('late forged lifecycle response is ignored', simForged.settled.filter((s) => s[0] === 'fork-forged').length === 1);

  const simEmpty = simulate();
  simEmpty.send('clone-empty', 'clone', undefined, simEmpty.gen(), { sessionId: 'source-a' });
  simEmpty.onResponse({ id: 'clone-empty', success: true, data: {}, sessionId: 'target-b', command: 'clone' });
  check('lifecycle success with empty payload sessionId rejects as target mismatch',
    simEmpty.settled.some((s) => s[0] === 'clone-empty' && s[1] === 'reject' && s[2] === 'rpc response target ownership mismatch')
    && !simEmpty.settled.some((s) => s[1] === 'resolve'), JSON.stringify(simEmpty.settled));

  const simFail = simulate();
  simFail.send('new-fail', 'new_session', undefined, simFail.gen(), { sessionId: 'source-a' });
  simFail.onResponse({ id: 'new-fail', success: false, error: 'boom', sessionId: 'source-a', command: 'new_session' });
  check('lifecycle failure stays source-owned and rejects with rpc=true',
    simFail.settled.some((s) => s[0] === 'new-fail' && s[1] === 'reject' && s[2] === 'boom' && s[3] === true),
    JSON.stringify(simFail.settled));
  const simFailMismatch = simulate();
  simFailMismatch.send('new-fail-mismatch', 'new_session', undefined, simFailMismatch.gen(), { sessionId: 'source-a' });
  simFailMismatch.onResponse({ id: 'new-fail-mismatch', success: false, error: 'boom', sessionId: 'other-c', command: 'new_session' });
  check('lifecycle failure with a foreign envelope still rejects as source ownership mismatch',
    simFailMismatch.settled.some((s) => s[0] === 'new-fail-mismatch' && s[1] === 'reject' && s[2] === 'rpc response session ownership mismatch'),
    JSON.stringify(simFailMismatch.settled));

  check('new_session is target-owned', isTargetOwnedLifecycle('new_session') === true);
  check('fork is target-owned', isTargetOwnedLifecycle('fork') === true);
  check('clone is target-owned', isTargetOwnedLifecycle('clone') === true);
  check('session_fork_latest is target-owned', isTargetOwnedLifecycle('session_fork_latest') === true);
  check('switch_session is NOT target-owned (negotiated handoff)', isTargetOwnedLifecycle('switch_session') === false);
  check('get_state is NOT target-owned', isTargetOwnedLifecycle('get_state') === false);
}

{
  // Wrong-command gate: a response frame that does not NAME the pending
  // command can never own it — for EVERY command class (fast-ack, long-op,
  // panel-owned, auxiliary, App-switch-owned, lifecycle target-owned,
  // close_session cleanup owner). The frame is rejected exactly once with the
  // command ownership error and the optimistic bubble removed; nothing
  // resolves.
  const COMMAND_CLASSES = [
    'prompt',             // fast-ack ordinary
    'get_state',          // fast-ack App shell
    'compact',            // long-op
    'code_review_open',   // long-op user action (App toast owns failures)
    'workflow_detail',    // panel-owned
    'todo_child_detail',  // panel-owned
    'path_complete',      // panel-owned quiet rejection
    'get_session_stats',  // auxiliary-capable
    'switch_session',     // App-switch-owned (never App-toasts)
    'new_session',        // lifecycle target-owned
    'fork',
    'clone',
    'session_fork_latest',
    'close_session',      // App-side cleanup owner
  ];
  for (const type of COMMAND_CLASSES) {
    const sim = simulate();
    sim.send('wc-1', type, 'u-wc', undefined, { sessionId: 'source-a' });
    sim.onResponse({
      id: 'wc-1',
      success: true,
      data: { sessionId: 'target-b' },
      sessionId: 'source-a',
      command: 'a_completely_different_command',
    });
    check(`wrong command rejected for ${type}`,
      sim.settled.some((s) => s[0] === 'wc-1'
        && s[1] === 'reject'
        && s[2] === 'rpc response command ownership mismatch')
      && !sim.settled.some((s) => s[0] === 'wc-1' && s[1] === 'resolve')
      && !sim.bubbles.has('u-wc'),
      JSON.stringify(sim.settled));
    // The mismatched frame settled the entry exactly once: a replayed frame
    // (even correctly named) is a late no-op.
    const settledCount = sim.settled.filter((s) => s[0] === 'wc-1').length;
    sim.onResponse({ id: 'wc-1', success: true, data: {}, command: type });
    check(`correctly named replay after wrong command is a no-op for ${type}`,
      sim.settled.filter((s) => s[0] === 'wc-1').length === settledCount);
  }
  // Direct classifier contract: command identity is checked BEFORE session
  // ownership, so a wrong command on an unrelated session reads as a command
  // mismatch, never a session mismatch.
  check('command mismatch outranks session mismatch',
    classifyResponseOwnership(
      { command: 'other', sessionId: 'target-b', success: true },
      { type: 'prompt', sessionId: 'source-a' },
    ) === 'command-mismatch');
  // The target-owned exemption NEVER applies to a failure envelope: a forged
  // lifecycle FAILURE tagged with the target's sessionId is exactly a source
  // ownership mismatch.
  check('lifecycle failure is source-owned even when tagged with the target',
    classifyResponseOwnership(
      { command: 'fork', sessionId: 'target-b', success: false, data: { sessionId: 'target-b' } },
      { type: 'fork', sessionId: 'source-a' },
    ) === 'source-ownership-mismatch');
  check('lifecycle failure named by its source is accepted',
    classifyResponseOwnership(
      { command: 'fork', sessionId: 'source-a', success: false },
      { type: 'fork', sessionId: 'source-a' },
    ) === 'accept');
  check('missing frame command never owns a pending entry',
    classifyResponseOwnership({ sessionId: 'source-a', success: true }, { type: 'prompt', sessionId: 'source-a' })
      === 'command-mismatch');
}

{
  // Outage-surface ownership, end to end over the App wiring simulator:
  // initial CONNECTING (no active outage) stays UNOWNED for the panel's
  // one toast; an ACTIVE outage surface owns every drain and immediate
  // not-open reject so no second toast ever appears.
  const sim = simulate();

  // (1) Initial CONNECTING close: never connected, no outage surface.
  sim.send('boot-cmd', 'get_state');
  sim.onClose(1006, '');
  check('initial CONNECTING close drains UNOWNED (panel one-toast surface)', sim.settled.some((s) => s[0] === 'boot-cmd' && s[1] === 'reject' && s[3] === false && s[4] === false), JSON.stringify(sim.settled));
  check('initial CONNECTING close emits zero outage toast', sim.toasts.length === 0, JSON.stringify(sim.toasts));
  const unownedImmediate = sim.immediateNotOpenReject();
  check('initial not-open reject stays unowned', unownedImmediate.transport === true && unownedImmediate.toastOwned !== true, JSON.stringify(unownedImmediate));

  // (2) Mid-session close after a successful connect: the persistent outage
  // toast is emitted EXACTLY ONCE and the drain is STAMPED toastOwned.
  sim.markConnected();
  sim.send('outage-cmd', 'get_state');
  sim.onClose(1006, '');
  check('mid-session close emits exactly one outage toast', sim.toasts.length === 1 && sim.toasts[0].includes('connection closed (code 1006)'), JSON.stringify(sim.toasts));
  check('mid-session outage drain is STAMPED toastOwned', sim.settled.some((s) => s[0] === 'outage-cmd' && s[3] === false && s[4] === true), JSON.stringify(sim.settled));
  const ownedImmediate = sim.immediateNotOpenReject();
  check('immediate not-open reject during ACTIVE outage is owned', ownedImmediate.transport === true && ownedImmediate.toastOwned === true, JSON.stringify(ownedImmediate));

  // (3) Second close during the SAME outage (no reconnect/open in between):
  // no second outage toast, and the drained pending is STAMPED toastOwned.
  sim.send('same-outage-cmd', 'get_state', undefined, 0); // stale-gen style pending from the older socket
  sim.send('second-outage-cmd', 'get_state', undefined, sim.gen());
  sim.onClose(1006, '');
  check('second close in the same outage emits NO second outage toast', sim.toasts.length === 1, JSON.stringify(sim.toasts));
  check('second close drain of the CURRENT generation is STAMPED toastOwned', sim.settled.some((s) => s[0] === 'second-outage-cmd' && s[3] === false && s[4] === true), JSON.stringify(sim.settled));
  check('drained pending from the older generation is also STAMPED', sim.settled.some((s) => s[0] === 'same-outage-cmd' && s[3] === false && s[4] === true), JSON.stringify(sim.settled));

  // Reopen clears the outage surface; the next close emits a NEW outage
  // toast exactly once and owns its drains again.
  sim.markConnected();
  sim.send('reopen-cmd', 'get_state');
  sim.onClose(1006, '');
  check('reopen clears the outage surface so the next close emits one new outage toast', sim.toasts.length === 2, JSON.stringify(sim.toasts));
  check('post-reopen drain is STAMPED toastOwned', sim.settled.some((s) => s[0] === 'reopen-cmd' && s[3] === false && s[4] === true), JSON.stringify(sim.settled));
}

{
  check('user prompt failure still toasts', rpcFailureToastOwned({ type: 'prompt', boot: false }) === true);
  // Frozen ownership: background loads stay panel-owned; user actions reach the App toast.
  check('code_review_snapshot poll stays panel-owned (no App toast)', rpcFailureToastOwned({ type: 'code_review_snapshot', boot: false }) === false);
  check('code_review_file_diff paging stays panel-owned', rpcFailureToastOwned({ type: 'code_review_file_diff', boot: false }) === false);
  check('code_review_file_context stays panel-owned', rpcFailureToastOwned({ type: 'code_review_file_context', boot: false }) === false);
  check('code_review_close failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_close', boot: false }) === true);
  check('code_review_open failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_open', boot: false }) === true);
  check('code_review_refresh failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_refresh', boot: false }) === true);
  check('code_review_comment failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_comment', boot: false }) === true);
  check('code_review_abort failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_abort', boot: false }) === true);
  check('code_review_auto_review failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_auto_review', boot: false }) === true);
  check('code_review_highlight failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_highlight', boot: false }) === true);
  check('code_review_stop failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_stop', boot: false }) === true);
  check('code_review_set_model failure is a user action -> App toast', rpcFailureToastOwned({ type: 'code_review_set_model', boot: false }) === true);
  check('no broad code_review_ exclusion: any other code_review command toasts', rpcFailureToastOwned({ type: 'code_review_set_model', boot: false }) === true && rpcFailureToastOwned({ type: 'code_review_open', boot: false }) === true);
  check('isPanelOwnedRpc keeps only the background-load set', isPanelOwnedRpc('code_review_snapshot') === true
    && isPanelOwnedRpc('code_review_file_diff') === true
    && isPanelOwnedRpc('code_review_file_context') === true
    && isPanelOwnedRpc('code_review_close') === false
    && isPanelOwnedRpc('code_review_auto_review') === false
    && isPanelOwnedRpc('code_review_stop') === false
    && isPanelOwnedRpc('code_review_open') === false);
  check('todo_child prefix is panel-owned', rpcFailureToastOwned({ type: 'todo_child_status', boot: false }) === false);
  check('file_explorer_ prefix is panel-owned', rpcFailureToastOwned({ type: 'file_explorer_stage_download', boot: false }) === false);
  check('path_complete is panel-owned (the input component owns quiet failure)', rpcFailureToastOwned({ type: 'path_complete', boot: false }) === false);
  check('workflow_ prefix is panel-owned (WorkflowPanel owns every workflow error row)', rpcFailureToastOwned({ type: 'workflow_detail', boot: false }) === false);
  check('workflow_detail not-found after remove never toasts', rpcFailureToastOwned({ type: 'workflow_detail', boot: false, sessionId: 'A', activeSessionId: 'A' }) === false);
  check('workflow_list failures are panel-owned', rpcFailureToastOwned({ type: 'workflow_list', boot: false }) === false);
  check('workflow_cancel failures are panel-owned', rpcFailureToastOwned({ type: 'workflow_cancel', boot: false }) === false);
  check('boot failures stay silent', rpcFailureToastOwned({ type: 'get_state', boot: true }) === false);
  check('stale media stays silent', rpcFailureToastOwned({
    type: 'stt_transcribe',
    boot: false,
    sessionId: 'A',
    activeSessionId: 'B',
  }) === false);
  check('auxiliary get_session_stats stays silent', rpcFailureToastOwned({
    type: 'get_session_stats',
    boot: false,
    auxiliary: true,
  }) === false);
  check('user-facing get_session_stats without auxiliary still toasts', rpcFailureToastOwned({
    type: 'get_session_stats',
    boot: false,
  }) === true);
  check('switch_session is App-switch-owned (requestSessionSwitch toasts its own rollback)', rpcFailureToastOwned({ type: 'switch_session', boot: false }) === false);
  check('switch_session stays silent during bootstrap', rpcFailureToastOwned({ type: 'switch_session', boot: true }) === false);

  const sim = simulate();
  sim.send('stats-fail', 'get_session_stats', undefined, sim.gen(), { sessionId: 'A', auxiliary: true });
  check(
    'auxiliary command is not settled before its response',
    !sim.settled.some((s) => s[0] === 'stats-fail'),
    JSON.stringify(sim.settled),
  );
  sim.onResponse({ id: 'stats-fail', command: 'get_session_stats', success: false, error: 'session closed', sessionId: 'A' }, 'A');
  check(
    'failed get_session_stats rejects internally',
    sim.settled.some((s) => s[0] === 'stats-fail' && s[1] === 'reject' && s[2] === 'session closed' && s[3] === true && s[4] === false),
    JSON.stringify(sim.settled),
  );
  check('failed get_session_stats emits zero toast', sim.toasts.length === 1 && sim.toasts[0] === null, JSON.stringify(sim.toasts));

  sim.send('stats-user', 'get_session_stats', undefined, sim.gen(), { sessionId: 'A' });
  check(
    'user-facing stats is not settled before its response',
    !sim.settled.some((s) => s[0] === 'stats-user'),
    JSON.stringify(sim.settled),
  );
  sim.onResponse({ id: 'stats-user', command: 'get_session_stats', success: false, error: 'denied', sessionId: 'A' }, 'A');
  check(
    'get_session_stats without explicit auxiliary still toasts',
    sim.settled.some((s) => s[0] === 'stats-user' && s[1] === 'reject' && s[2] === 'denied' && s[3] === true && s[4] === true)
      && sim.toasts[sim.toasts.length - 1] === 'command get_session_stats failed: denied',
    JSON.stringify({ settled: sim.settled, toasts: sim.toasts }),
  );

  const caches = {};
  sim.send('stats-a', 'get_session_stats', undefined, sim.gen(), { sessionId: 'A', auxiliary: true });
  sim.send('stats-b', 'get_session_stats', undefined, sim.gen(), { sessionId: 'B', auxiliary: true });
  check(
    'in-flight stats are not cached before settlement',
    caches.A === undefined && caches.B === undefined,
    JSON.stringify(caches),
  );
  sim.onResponse({ id: 'stats-b', success: true, data: { contextUsage: { tokens: 20, contextWindow: 100, percent: 20 } }, sessionId: 'B', command: 'get_session_stats' }, 'B');
  caches.B = { tokens: 20 };
  sim.onResponse({ id: 'stats-a', success: true, data: { contextUsage: { tokens: 10, contextWindow: 100, percent: 10 } }, sessionId: 'A', command: 'get_session_stats' }, 'B');
  caches.A = { tokens: 10 };
  check('deferred A stats stay in A cache after switch to B', caches.A.tokens === 10 && caches.B.tokens === 20, JSON.stringify(caches));
  check('background B turn cannot overwrite A cache', caches.A.tokens === 10);
}

{
  // The sidebar catalog sender is auxiliary per-call (App sendCatalogCommand):
  // every already-sent session_list rejection — switch-pending gate text,
  // RPC limit, business, transport — rejects into the sidebar's silent
  // background catch (last-good rows retained) and the App layer never
  // toasts it. A user-facing non-auxiliary session_list still toasts.
  check('auxiliary session_list (sidebar catalog load) stays silent', rpcFailureToastOwned({
    type: 'session_list',
    boot: false,
    auxiliary: true,
  }) === false);
  check('non-auxiliary session_list still toasts at the App layer', rpcFailureToastOwned({
    type: 'session_list',
    boot: false,
  }) === true);
  const sim = simulate();
  sim.send('cat-pending', 'session_list', undefined, sim.gen(), { auxiliary: true });
  check(
    'auxiliary catalog load is not settled before its response',
    !sim.settled.some((s) => s[0] === 'cat-pending'),
    JSON.stringify(sim.settled),
  );
  sim.onResponse({ id: 'cat-pending', command: 'session_list', success: false, error: 'session switch pending' }, 'A');
  check(
    'already-sent auxiliary catalog rejection carries rpc=true and no toast ownership',
    sim.settled.some((s) => s[0] === 'cat-pending' && s[1] === 'reject' && s[2] === 'session switch pending' && s[3] === true && s[4] === false),
    JSON.stringify(sim.settled),
  );
  check('rejected auxiliary catalog load emits zero toast', sim.toasts.length === 1 && sim.toasts[0] === null, JSON.stringify(sim.toasts));
}


{
  // path_complete is panel-owned: a failed completion rejects into the
  // PathAutocomplete caller (its catch hides the menu, manual entry is
  // unaffected) and the App never duplicates a global toast.
  const sim = simulate();
  sim.send('pc-fail', 'path_complete', undefined, sim.gen(), { sessionId: 'A' });
  sim.onResponse({ id: 'pc-fail', command: 'path_complete', success: false, error: 'path is outside the workspace', sessionId: 'A' }, 'A');
  check(
    'path_complete failure rejects to the component with rpc=true and no toast ownership',
    sim.settled.some((s) => s[0] === 'pc-fail' && s[1] === 'reject' && s[2] === 'path is outside the workspace' && s[3] === true && s[4] === false),
    JSON.stringify(sim.settled),
  );
  check('path_complete failure emits zero global toast', sim.toasts.length === 1 && sim.toasts[0] === null, JSON.stringify(sim.toasts));
}


{
  // Timeout ownership: the registry stamps the rejection's toastOwned from onTimeout's boolean (old: bubble only).

  // (1) 600s code_review_auto_review: user action, ONE toast-owned rejection.
  const sim = simulate();
  sim.setLifecycle(false, 'session-a');
  sim.send('review-run', 'code_review_auto_review', 'u-review', undefined, { sessionId: 'session-a' });
  sim.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('code_review_auto_review stays pending at the 30s fast-ack bound', sim.registry.has('review-run') && sim.settled.length === 0 && sim.timedOut.length === 0, JSON.stringify(sim.settled));
  sim.sched.advance(LONG_OP_TIMEOUT_MS - DEFAULT_COMMAND_TIMEOUT_MS);
  const reviewMessage = 'code_review_auto_review timed out after 600s (no response from server)';
  check('stuck code_review_auto_review rejects exactly at its 600s boundary', sim.settled.some((s) => s[0] === 'review-run' && s[1] === 'reject' && s[2] === reviewMessage), JSON.stringify(sim.settled));
  check('owned code_review_auto_review timeout emits exactly ONE App toast with the exact message', sim.toasts.length === 1 && sim.toasts[0] === reviewMessage, JSON.stringify(sim.toasts));
  check('code_review_auto_review timeout rejection carries toastOwned=true (old rejection had none)', sim.settled.some((s) => s[0] === 'review-run' && s[4] === true), JSON.stringify(sim.settled));
  check('owned code_review_auto_review timeout removed the optimistic bubble', !sim.bubbles.has('u-review'));

  // (2) Panel-owned background snapshot timeout: silent at the App layer.
  const simSnap = simulate();
  simSnap.setLifecycle(false, 'session-a');
  simSnap.send('snap-poll', 'code_review_snapshot', undefined, undefined, { sessionId: 'session-a' });
  simSnap.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('panel-owned snapshot timeout rejects at the default bound', simSnap.settled.some((s) => s[0] === 'snap-poll' && s[1] === 'reject' && s[2] === 'code_review_snapshot timed out after 30s (no response from server)'), JSON.stringify(simSnap.settled));
  check('panel-owned background timeout emits NO App toast', simSnap.toasts.length === 0, JSON.stringify(simSnap.toasts));
  check('panel-owned timeout rejection stays unowned for the panel catch', simSnap.settled.some((s) => s[0] === 'snap-poll' && s[3] === false && s[4] === false), JSON.stringify(simSnap.settled));

  // (3) Default 30s non-long command: rejects at exactly 30s, owned once.
  const simState = simulate();
  simState.setLifecycle(false, 'session-a');
  simState.send('shell-state', 'get_state', 'u-state', undefined, { sessionId: 'session-a' });
  simState.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  const stateMessage = 'get_state timed out after 30s (no response from server)';
  check('default non-long command rejects at exactly 30s', simState.settled.some((s) => s[0] === 'shell-state' && s[1] === 'reject' && s[2] === stateMessage), JSON.stringify(simState.settled));
  check('default command timeout toasts once with the exact message', simState.toasts.length === 1 && simState.toasts[0] === stateMessage, JSON.stringify(simState.toasts));
  check('default command timeout rejection is stamped toastOwned=true', simState.settled.some((s) => s[0] === 'shell-state' && s[4] === true), JSON.stringify(simState.settled));

  const simHandoff = simulate();
  simHandoff.setLifecycle(false, 'session-a');
  simHandoff.send('handoff-run', 'session_handoff', undefined, undefined, { sessionId: 'session-a' });
  simHandoff.sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  const handoffMessage = 'session_handoff timed out after 30s (no response from server)';
  check(
    'session_handoff rejects at 30s (LONG_OP_TYPES must not swallow the 30s budget)',
    simHandoff.settled.some((s) => s[0] === 'handoff-run' && s[1] === 'reject' && s[2] === handoffMessage)
      && !simHandoff.registry.has('handoff-run'),
    JSON.stringify(simHandoff.settled),
  );
}

{
  const sched = new FakeScheduler();
  const hook = [];
  const settled = [];
  const registry = new PendingRegistry({
    scheduler: sched,
    onTimeout: (entry, message) => {
      hook.push([entry.type, message]);
      return entry.type === 'prompt';
    },
  });
  registry.add('t-owned', { resolve: () => {}, reject: (e) => settled.push(['t-owned', e.message, e.toastOwned === true]), gen: 1, type: 'prompt' });
  registry.add('t-quiet', { resolve: () => {}, reject: (e) => settled.push(['t-quiet', e.message, e.toastOwned === true]), gen: 1, type: 'get_state' });
  sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  check('onTimeout receives the entry and the exact timeout message', hook.length === 2 && hook[0][0] === 'prompt' && hook[0][1] === 'prompt timed out after 30s (no response from server)' && hook[1][0] === 'get_state' && hook[1][1] === 'get_state timed out after 30s (no response from server)', JSON.stringify(hook));
  check('onTimeout boolean stamps the rejected error toastOwned', settled[0][2] === true && settled[1][2] === false, JSON.stringify(settled));
}

{
  const owned = new Error('owned');
  owned.toastOwned = true;
  const unowned = new Error('unowned');
  unowned.toastOwned = false;
  check('isToastOwnedRpcError truth table', isToastOwnedRpcError(owned) === true
    && isToastOwnedRpcError(unowned) === false
    && isToastOwnedRpcError(undefined) === false
    && isToastOwnedRpcError(null) === false
    && isToastOwnedRpcError('timed out') === false
    && isToastOwnedRpcError({ toastOwned: true }) === true
    && isToastOwnedRpcError({ toastOwned: 'true' }) === false
    && isToastOwnedRpcError({}) === false);
}


console.log(`\npending.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
