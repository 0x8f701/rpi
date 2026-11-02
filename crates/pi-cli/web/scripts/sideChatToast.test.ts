#!/usr/bin/env node
// The five catch handlers in App.tsx (~2180-2252) gate through the REAL
// production export shouldToastUnhandledCommandError (transportError.ts):
// transport failures (socket drop 1006, heartbeat silence close, immediate
// not-connected, ws.send failure, replaced transport) must produce ZERO
// side-chat toasts — the transport layer owns the single outage toast —
// while genuine RPC/business errors keep exactly ONE toast (owned by
// onResponse's toastOwned path) and bounded command timeouts keep exactly
// ONE toast from the handler itself. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// The five handlers are inline useCallbacks, so the suite drives a behavioral
// simulator (the same approach as socket.test.ts / pending.test.ts) over the
// REAL error-production paths — PendingRegistry drainExcept/drainAll/timeout
// shapes and the App-constructed `not connected` / `send failed:` tagged
// errors — and gates them with the REAL isTransportError classifier plus a
// transcription of the current handler predicate. The pre-fix predicates
// (`!err.rpc` only for side_chat_list, unconditional for the other four) are
// run over the SAME fixtures to prove the suite has teeth: they must fail the
// new contract (the run below prints the toast counts as evidence).
import { PendingRegistry, DEFAULT_COMMAND_TIMEOUT_MS, timeoutErrorMessage } from '../src/pending.ts';
import { shouldToastUnhandledCommandError } from '../src/transportError.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Deterministic clock + timer queue (same fixture as pending.test.ts). */
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
  now() {
    return this.time;
  }
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

/** The five side-chat operations with the toast prefix their handlers use and
 *  the PRE-FIX catch behavior from the App.tsx diff: side_chat_list gated on
 *  `!err.rpc` only; the other four toasted unconditionally. */
const OPS = [
  { type: 'side_chat_list', prefix: 'side chat: ', oldUnconditional: false },
  { type: 'side_chat_new', prefix: 'side chat new: ', oldUnconditional: true },
  { type: 'side_chat_switch', prefix: 'side chat switch: ', oldUnconditional: true },
  { type: 'side_chat_close', prefix: 'side chat close: ', oldUnconditional: true },
  { type: 'side_chat_prompt', prefix: 'side chat send failed: ', oldUnconditional: true },
];

/** The current handler gate — the REAL production export the five App.tsx
 *  side-chat catch handlers call (this suite binds production code, so a
 *  regression in the extracted predicate fails here directly). */
const newGate = (op, err) => shouldToastUnhandledCommandError(err);
/** The pre-fix handler behavior (per-op, from the diff). */
const oldGate = (op, err) => op.oldUnconditional || !err.rpc;

/**
 * The established transport-failure message family. Each spec reproduces the
 * error the App produces for that failure mode, through the REAL code paths:
 *  - drain specs: PendingRegistry.drainExcept with the exact close reason and
 *    toastOwned flag the App's onclose/replace paths pass (rpc=false for a
 *    dropped socket, rpc=true for the quiet replace drain).
 *  - direct specs: the Error + transport tag App's sendCommand constructs for
 *    an immediate not-connected or a ws.send throw.
 * outageToast counts the transport-layer outage toast the App owns for that
 * failure (one per drop, none for replaced/not-connected/send-failed).
 * rpcToast counts onResponse's toastOwned toast for RPC business failures.
 */
const TRANSPORT_FAILURES = [
  {
    kind: 'socket close 1006 drain',
    outageToast: 1,
    rpcToast: 0,
    produce(reject, registry, sched, op) {
      registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
      registry.drainExcept(2, 'connection closed (code 1006)', false, true);
    },
  },
  {
    kind: 'socket close 1008 drain',
    outageToast: 1,
    rpcToast: 0,
    produce(reject, registry, sched, op) {
      registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
      registry.drainExcept(2, 'connection closed (code 1008: policy violation)', false, true);
    },
  },
  {
    kind: 'heartbeat silence close drain',
    outageToast: 1,
    rpcToast: 0,
    produce(reject, registry, sched, op) {
      registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
      registry.drainExcept(2, 'connection closed (code 4000: heartbeat timeout)', false, true);
    },
  },
  {
    kind: 'transport replaced drain (rpc-tagged, quiet)',
    outageToast: 0,
    rpcToast: 0,
    produce(reject, registry, sched, op) {
      registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
      registry.drainExcept(2, 'connection replaced', true);
    },
  },
  {
    kind: 'immediate not connected (transport-tagged)',
    outageToast: 0,
    rpcToast: 0,
    produce(reject) {
      reject(Object.assign(new Error('not connected'), { transport: true }));
    },
  },
  {
    kind: 'ws.send failure (transport-tagged)',
    outageToast: 0,
    rpcToast: 0,
    produce(reject) {
      reject(Object.assign(new Error('send failed: socket is closed'), { transport: true }));
    },
  },
];

const BUSINESS_FAILURES = [
  {
    kind: 'rpc business error (onResponse-owned)',
    outageToast: 0,
    rpcToast: 1,
    produce(reject, registry, sched, op) {
      registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
      const entry = registry.take('c1');
      if (entry) entry.reject(Object.assign(new Error('side chat already exists'), { rpc: true, toastOwned: true }));
    },
  },
  {
    kind: 'rpc business error with transport-looking message',
    outageToast: 0,
    rpcToast: 1,
    produce(reject, registry, sched, op) {
      registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
      const entry = registry.take('c1');
      if (entry) entry.reject(Object.assign(new Error('connection closed: session not found'), { rpc: true, toastOwned: true }));
    },
  },
];

const TIMEOUT_FAILURE = {
  kind: 'bounded command timeout (server alive)',
  outageToast: 0,
  rpcToast: 0,
  produce(reject, registry, sched, op) {
    registry.add('c1', { resolve: () => {}, reject, gen: 1, type: op.type });
    sched.advance(DEFAULT_COMMAND_TIMEOUT_MS);
  },
};

/** Settle ONE side-chat command through the real production error path and
 *  count toasts the given catch predicate would raise. */
function settle(op, spec, gate) {
  const sched = new FakeScheduler();
  const registry = new PendingRegistry({ scheduler: sched, onTimeout: () => {} });
  let rejected = null;
  const reject = (error) => { rejected = error; };
  spec.produce(reject, registry, sched, op);
  if (!(rejected instanceof Error)) throw new Error(`settle(${op.type}, ${spec.kind}): no rejection produced`);
  const err = rejected;
  const catchToasts = gate(op, err) ? [`${op.prefix}${err.message}`] : [];
  return {
    err,
    catchCount: catchToasts.length,
    catchMessage: catchToasts[0] ?? null,
    total: catchToasts.length + spec.outageToast + spec.rpcToast,
  };
}

/** Run the full contract matrix under ONE candidate handler predicate.
 *  Returns the failures (empty = contract held) and per-op evidence. */
function runContract(candidateGate) {
  const localFailures = [];
  let checks = 0;
  const t = (name, cond, detail) => {
    checks += 1;
    if (!cond) localFailures.push(`${name}${detail ? `: ${detail}` : ''}`);
  };
  const perOpTransport = {};
  const perOp = (op) => (perOpTransport[op.type] ??= { fresh: 0, old: 0 });

  for (const op of OPS) {
    for (const spec of TRANSPORT_FAILURES) {
      const fresh = settle(op, spec, candidateGate);
      const old = settle(op, spec, oldGate);
      perOp(op).fresh += fresh.catchCount;
      perOp(op).old += old.catchCount;
      t(`${op.type}: ${spec.kind} -> zero side-chat toasts`, fresh.catchCount === 0, `got ${fresh.catchCount}`);
      // Model the pre-fix predicate exactly (from the App.tsx diff): the
      // rpc-tagged replace drain was already quiet for side_chat_list's
      // `!err.rpc` gate, while the other four unconditional handlers toasted
      // it; every non-rpc transport reject toasted under all five.
      const rpcTagged = spec.kind.startsWith('transport replaced');
      const expectedOld = rpcTagged ? (op.oldUnconditional ? 1 : 0) : 1;
      t(`${op.type}: ${spec.kind} -> pre-fix predicate modeled exactly`, old.catchCount === expectedOld, `old ${old.catchCount}, expected ${expectedOld}`);
      if (spec.outageToast === 1) {
        t(
          `${op.type}: ${spec.kind} -> single transport outage toast, handler adds none`,
          fresh.total === 1 && old.total === 2,
          `new total ${fresh.total}, old total ${old.total}`,
        );
      } else if (spec.kind.startsWith('transport replaced')) {
        t(
          `${op.type}: ${spec.kind} -> fully quiet under the fix`,
          fresh.total === 0 && old.total === (op.oldUnconditional ? 1 : 0),
          `new total ${fresh.total}, old total ${old.total}`,
        );
      } else {
        t(`${op.type}: ${spec.kind} -> no toast at all under the fix`, fresh.total === 0, `new total ${fresh.total}`);
      }
    }
    for (const spec of BUSINESS_FAILURES) {
      const fresh = settle(op, spec, candidateGate);
      const old = settle(op, spec, oldGate);
      t(`${op.type}: ${spec.kind} -> catch adds nothing (onResponse owns it)`, fresh.catchCount === 0, `got ${fresh.catchCount}`);
      t(`${op.type}: ${spec.kind} -> exactly one toast total`, fresh.total === 1, `new total ${fresh.total}`);
      t(
        `${op.type}: ${spec.kind} -> pre-fix unconditional handler double-toasted`,
        old.catchCount === (op.oldUnconditional ? 1 : 0),
        `old ${old.catchCount}`,
      );
    }
    {
      const fresh = settle(op, TIMEOUT_FAILURE, candidateGate);
      const old = settle(op, TIMEOUT_FAILURE, oldGate);
      const expected = timeoutErrorMessage(op.type, 0, DEFAULT_COMMAND_TIMEOUT_MS);
      t(
        `${op.type}: timeout -> handler toasts exactly once with operation prefix`,
        fresh.catchCount === 1 && fresh.catchMessage === `${op.prefix}${expected}`,
        `got ${JSON.stringify(fresh.catchMessage)}`,
      );
      t(`${op.type}: timeout -> exactly one toast total`, fresh.total === 1, `new total ${fresh.total}`);
      t(`${op.type}: timeout -> pre-fix behavior unchanged (one toast)`, old.catchCount === 1, `old ${old.catchCount}`);
    }
  }
  {
    // Single-ownership proof: the 1006 drain marks the outage toast owned and
    // the fix must not raise a second toast on top of it.
    const op = OPS[0];
    const spec = TRANSPORT_FAILURES[0];
    const s = settle(op, spec, candidateGate);
    t('1006 drain marks toastOwned on the rejected error', s.err.toastOwned === true, `got ${s.err.toastOwned}`);
    t('1006 drain: owned transport toast is never duplicated by the handler', s.total === 1, `total ${s.total}`);
  }
  return { failures: localFailures, checks, perOpTransport };
}

const freshRun = runContract(newGate);
const oldRun = runContract(oldGate);

for (const f of freshRun.failures) failures.push(f);

// Evidence: toast counts per operation over the identical transport fixtures.
let oldTransportSum = 0;
let freshTransportSum = 0;
for (const op of OPS) {
  oldTransportSum += freshRun.perOpTransport[op.type].old;
  freshTransportSum += freshRun.perOpTransport[op.type].fresh;
}
console.log(
  `\nsideChatToast.test evidence: over ${OPS.length * TRANSPORT_FAILURES.length} transport rejections ` +
  `(${TRANSPORT_FAILURES.map((f) => f.kind).join('; ')}), the pre-fix predicates raised ` +
  `${oldTransportSum} side-chat toasts (${OPS.map((o) => `${o.type}=${freshRun.perOpTransport[o.type].old}`).join(', ')}); ` +
  `the fixed predicate raised ${freshTransportSum}.`,
);
console.log(`Old-predicate run fails the new contract on ${oldRun.failures.length} checks (${oldRun.checks} run); new-predicate run: ${freshRun.failures.length} failures (${freshRun.checks} checks).`);

check('fixed predicate: full side-chat toast contract holds', freshRun.failures.length === 0, `${freshRun.failures.length} failures`);
check('pre-fix predicate: fails the new contract (teeth proof)', oldRun.failures.length > 0, `${oldRun.failures.length} failures`);

console.log(`\nsideChatToast.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
