#!/usr/bin/env node
// Focused ReadyGate regression for src/socket.ts — the bounded, generation-
// safe ready gate that fixes the mount-before-WebSocket-OPEN `load failed:
// not connected` regression. Auto/background loads (sidebar + session panel
// `load()` on mount) wait on the gate for the current socket to reach OPEN
// instead of failing immediately; active actions bypass it and fail fast.
//
// The gate is pure (no DOM/WebSocket): a scheduler is injected so this node
// test drives time deterministically with a fake timer. Run via `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { ReadyGate, READY_GATE_TIMEOUT_MS } from '../src/socket.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Fake scheduler: records every armed timer and lets the test fire them in
 *  order (or by elapsed time) so the gate's bounded-timeout + notifyOpen/clear
 *  paths are exercised deterministically. */
type Timer = { fn: () => void; ms: number; id: number; fired: boolean };
class FakeScheduler {
  readonly timers: Timer[] = [];
  private nextId = 1;
  setTimeout(fn: () => void, ms: number): unknown {
    const t: Timer = { fn, ms, id: this.nextId++, fired: false };
    this.timers.push(t);
    return t;
  }
  clearTimeout(handle: unknown): void {
    const t = handle as Timer;
    if (!t) return;
    t.fired = true; // mark cancelled so fireAll/advance skip it
  }
  /** Fire every still-pending timer (notifyOpen/clear usually clear them). */
  fireAll(): void {
    for (const t of [...this.timers]) {
      if (!t.fired) {
        t.fired = true;
        t.fn();
      }
    }
  }
  /** Fire timers whose `ms` is <= elapsed, in id order, once each. */
  advance(elapsed: number): void {
    for (const t of [...this.timers]) {
      if (!t.fired && t.ms <= elapsed) {
        t.fired = true;
        t.fn();
      }
    }
  }
  pendingCount(): number {
    return this.timers.filter((t) => !t.fired).length;
  }
}

// ---- wait() registers a bounded waiter; notifyOpen resolves it ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  check('gate starts with no waiters', gate.size() === 0);
  let resolved = false;
  const p = gate.wait();
  p.then(() => { resolved = true; });
  check('wait() registers one waiter', gate.size() === 1);
  check('wait() arms a bounded timer', sched.pendingCount() === 1);
  check('wait() does not resolve before notifyOpen', !resolved);
  gate.notifyOpen();
  // Flush microtasks.
  await p;
  check('notifyOpen resolves the waiter', resolved);
  check('notifyOpen clears the waiter', gate.size() === 0);
  check('notifyOpen clears the bounded timer', sched.pendingCount() === 0);
}

// ---- notifyOpen resolves MULTIPLE concurrent waiters (poll + mount overlap) ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let r1 = false, r2 = false, r3 = false;
  const p1 = gate.wait().then(() => { r1 = true; });
  const p2 = gate.wait().then(() => { r2 = true; });
  const p3 = gate.wait().then(() => { r3 = true; });
  check('three waiters registered', gate.size() === 3);
  check('three bounded timers armed', sched.pendingCount() === 3);
  gate.notifyOpen();
  await Promise.all([p1, p2, p3]);
  check('notifyOpen resolves all three waiters', r1 && r2 && r3);
  check('all timers cleared after notifyOpen', sched.pendingCount() === 0 && gate.size() === 0);
}

// ---- bounded timeout: a waiter that is never opened rejects with not connected ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let rejected: Error | null = null;
  const p = gate.wait().catch((e: Error) => { rejected = e; });
  check('waiter armed with READY_GATE_TIMEOUT_MS', sched.timers[0]?.ms === READY_GATE_TIMEOUT_MS);
  // No notifyOpen — fire the bounded timer (simulates a permanently dead transport).
  sched.fireAll();
  await p;
  check('bounded timeout rejects with "not connected"', rejected !== null && rejected!.message === 'not connected');
  check('timed-out waiter is removed from the gate', gate.size() === 0);
}

// ---- a timed-out waiter's timer is NOT re-fired by a later notifyOpen ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let rejected = false;
  const p = gate.wait().catch(() => { rejected = true; });
  // Time out the waiter first.
  sched.advance(READY_GATE_TIMEOUT_MS);
  await p;
  check('waiter timed out (no open)', rejected);
  // A later open must not double-settle the already-rejected promise and must
  // not throw (notifyOpen iterates an empty waiter list).
  let threw = false;
  try { gate.notifyOpen(); } catch { threw = true; }
  check('later notifyOpen is a no-op (no throw)', !threw && gate.size() === 0);
}

// ---- clear() rejects every pending waiter (unmount) and clears timers ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let r1: Error | null = null, r2: Error | null = null;
  const p1 = gate.wait().catch((e: Error) => { r1 = e; });
  const p2 = gate.wait().catch((e: Error) => { r2 = e; });
  check('two waiters pending before clear', gate.size() === 2);
  gate.clear('unmount');
  await Promise.allSettled([p1, p2]);
  check('clear() rejects waiter 1 with the reason', r1 !== null && r1!.message === 'unmount');
  check('clear() rejects waiter 2 with the reason', r2 !== null && r2!.message === 'unmount');
  check('clear() empties the gate', gate.size() === 0);
  check('clear() cancels the bounded timers', sched.pendingCount() === 0);
  // A subsequent notifyOpen must not fire the cleared timers.
  let threw = false;
  try { gate.notifyOpen(); } catch { threw = true; }
  check('notifyOpen after clear is a no-op', !threw);
}

// ---- stale-socket guard: a waiter resolved by notifyOpen proceeds to a
//      sendCommand that re-checks readyState; the gate itself does NOT track
//      generation — the invariant is enforced by detach + readyState. Model
//      that contract: notifyOpen (called only from the CURRENT socket's
//      onOpen) resolves the waiter; a superseded socket's onopen is detached
//      and never calls notifyOpen, so it cannot trigger a load. ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  // Simulate socket A (CONNECTING): a mount load registers a waiter.
  let opened = false;
  const loadP = gate.wait().then(() => { opened = true; });
  check('mount load waits (socket A CONNECTING)', gate.size() === 1);
  // Socket A is superseded before it opens: its onopen is DETACHED, so it
  // never calls notifyOpen. The waiter stays pending (migrate to the new
  // socket — it does not reject on the gen bump).
  check('waiter still pending after A superseded (no notifyOpen)', gate.size() === 1);
  // Socket B opens: onOpen calls notifyOpen -> the migrated waiter resolves
  // and the load proceeds on the CURRENT socket (sendCommand re-checks
  // readyState at send time).
  gate.notifyOpen();
  await loadP;
  check('migrated waiter resolved on socket B open', opened);
  check('gate empty after the new open', gate.size() === 0);
}

// ---- bounded wait during a long outage: the timer fires before any open ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  const results: string[] = [];
  const p = gate.wait().then(() => results.push('resolved'), (e: Error) => results.push(`rejected:${e.message}`));
  // Advance just shy of the timeout: still pending, no permanent error yet.
  sched.advance(READY_GATE_TIMEOUT_MS - 1);
  check('waiter still pending 1ms before timeout', gate.size() === 1);
  // Cross the boundary: bounded reject (silently swallowed by the auto-load).
  sched.advance(READY_GATE_TIMEOUT_MS);
  await p;
  check('bounded timeout rejected after the deadline', results[0] === 'rejected:not connected');
}

// ---- a fresh waiter after a timeout/open is independent (no state leak) ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  const first = gate.wait().catch(() => {});
  sched.fireAll(); // first waiter times out
  await first;
  let secondResolved = false;
  const second = gate.wait().then(() => { secondResolved = true; });
  check('second waiter registered after first timed out', gate.size() === 1);
  gate.notifyOpen();
  await second;
  check('second waiter resolves on a later open', secondResolved);
}

console.log(`\nreadyGate.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);