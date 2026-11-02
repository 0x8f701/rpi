#!/usr/bin/env node
// Focused ReadyGate regression for src/socket.ts — the bounded gate that keeps
// auto/background session loads behind authoritative WebSocket bootstrap.
// Raw OPEN is not enough: App releases the gate only after state/messages bind
// the active session id. The class is pure; these tests drive its bounded
// settlement and generation-agnostic waiter behavior with a fake scheduler.
// Run via `npm run build`.
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
 *  order (or by elapsed time) so bounded timeout/readiness paths are driven
 *  deterministically. */
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
  /** Fire every still-pending timer (`markReady`/`clear` usually cancel them). */
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

// ---- raw transport OPEN stays blocked; authoritative bootstrap releases ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  gate.markNotReady();
  check('gate starts with no waiters', gate.size() === 0);
  let resolved = false;
  const p = gate.wait();
  p.then(() => { resolved = true; });
  check('OPEN-but-unbound load registers one waiter', gate.size() === 1);
  check('OPEN-but-unbound load arms a bounded timer', sched.pendingCount() === 1);
  check('raw OPEN does not resolve before bootstrap completion', !resolved);
  gate.markReady();
  await p;
  check('bootstrap completion resolves the waiter', resolved);
  check('bootstrap completion clears the waiter', gate.size() === 0);
  check('bootstrap completion clears the bounded timer', sched.pendingCount() === 0);
  let immediate = false;
  await gate.wait().then(() => { immediate = true; });
  check('later loads resolve immediately while authoritative-ready', immediate);
  check('ready fast path does not arm a timer', sched.pendingCount() === 0);
}

// ---- markReady resolves MULTIPLE concurrent waiters (poll + mount overlap) ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let r1 = false, r2 = false, r3 = false;
  const p1 = gate.wait().then(() => { r1 = true; });
  const p2 = gate.wait().then(() => { r2 = true; });
  const p3 = gate.wait().then(() => { r3 = true; });
  check('three waiters registered', gate.size() === 3);
  check('three bounded timers armed', sched.pendingCount() === 3);
  gate.markReady();
  await Promise.all([p1, p2, p3]);
  check('markReady resolves all three waiters', r1 && r2 && r3);
  check('all timers cleared after markReady', sched.pendingCount() === 0 && gate.size() === 0);
}

// ---- bounded timeout: a waiter that is never opened rejects with not connected ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let rejected: Error | null = null;
  const p = gate.wait().catch((e: Error) => { rejected = e; });
  check('waiter armed with READY_GATE_TIMEOUT_MS', sched.timers[0]?.ms === READY_GATE_TIMEOUT_MS);
  // No markReady — fire the bounded timer (simulates a dead bootstrap).
  sched.fireAll();
  await p;
  check('bounded timeout rejects with "not connected"', rejected !== null && rejected!.message === 'not connected');
  check('timed-out waiter is removed from the gate', gate.size() === 0);
}

// ---- a timed-out waiter's timer is not re-fired by later readiness ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  let rejected = false;
  const p = gate.wait().catch(() => { rejected = true; });
  // Time out the waiter first.
  sched.advance(READY_GATE_TIMEOUT_MS);
  await p;
  check('waiter timed out (no open)', rejected);
  // Later readiness must not double-settle the already-rejected promise.
  let threw = false;
  try { gate.markReady(); } catch { threw = true; }
  check('later markReady is a no-op for the settled waiter', !threw && gate.size() === 0);
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
  // Subsequent readiness must not fire the cleared timers.
  let threw = false;
  try { gate.markReady(); } catch { threw = true; }
  check('markReady after clear is safe', !threw);
}

// ---- replacement connection resets readiness but preserves existing waiters ----
{
  const sched = new FakeScheduler();
  const gate = new ReadyGate(sched);
  gate.markReady();
  await gate.wait();
  gate.markNotReady();
  let bootstrapped = false;
  const loadP = gate.wait().then(() => { bootstrapped = true; });
  check('replacement raw OPEN remains blocked before authoritative bootstrap', gate.size() === 1 && !bootstrapped);
  // Connection A is superseded during bootstrap: its alive() guard prevents
  // markReady, so the waiter remains for connection B.
  gate.markNotReady();
  check('waiter survives stale bootstrap replacement', gate.size() === 1 && !bootstrapped);
  gate.markReady();
  await loadP;
  check('migrated waiter resolves after replacement bootstrap', bootstrapped);
  check('gate empty after replacement bootstrap', gate.size() === 0);
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
  gate.markReady();
  await second;
  check('second waiter resolves on a later open', secondResolved);
}

console.log(`\nreadyGate.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);