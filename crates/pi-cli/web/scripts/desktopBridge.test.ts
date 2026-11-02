import assert from 'node:assert/strict';
import { createDesktopActionRegistry, type DesktopActionKind, type DesktopBridge } from '../src/desktopBridge';

let listener: ((action: { kind: DesktopActionKind }) => void) | undefined;
let subscriptions = 0;
let unsubscriptions = 0;
const published: DesktopActionKind[][] = [];
const bridge: DesktopBridge = {
  onAction(handler) {
    assert.equal(listener, undefined, 'multiple owners share one native subscription');
    listener = handler;
    subscriptions += 1;
    return () => { listener = undefined; unsubscriptions += 1; };
  },
  setActions(kinds) { published.push(kinds); },
};
const registry = createDesktopActionRegistry(bridge);
const calls: string[] = [];
const workspace = registry.register({ settings: () => calls.push('settings'), 'new-workspace': () => calls.push('workspace') });
const session = registry.register({ attach: () => calls.push('attach'), stop: () => calls.push('stop') });
assert.deepEqual(published.at(-1), ['new-workspace', 'settings', 'attach', 'stop']);
assert.equal(subscriptions, 1);
listener!({ kind: 'settings' });
listener!({ kind: 'attach' });
listener!({ kind: 'stop' });
assert.deepEqual(calls, ['settings', 'attach', 'stop']);
for (const invalid of [null, {}, { kind: 'shell' }, { kind: '__proto__' }, { kind: 'constructor' }]) {
  listener!(invalid as { kind: DesktopActionKind });
}
assert.deepEqual(calls, ['settings', 'attach', 'stop'], 'invalid native payloads never dispatch');
const publicationsBeforeReplacement = published.length;
session.update({ attach: () => calls.push('fresh attach'), stop: () => calls.push('fresh stop') });
listener!({ kind: 'attach' });
assert.equal(calls.at(-1), 'fresh attach');
assert.equal(published.length, publicationsBeforeReplacement, 'handler replacement does not churn native capabilities');
const overlapping = registry.register({ attach: () => calls.push('foreground attach') });
const beforeOverlap = calls.length;
listener!({ kind: 'attach' });
assert.deepEqual(calls.slice(beforeOverlap), ['foreground attach'], 'one operation invokes exactly one owner');
overlapping.unsubscribe();
listener!({ kind: 'attach' });
assert.equal(calls.at(-1), 'fresh attach', 'remaining owner resumes after foreground cleanup');
session.update({ attach: () => calls.push('idle attach') });
assert.deepEqual(published.at(-1), ['new-workspace', 'settings', 'attach']);
const beforeIdleStop = calls.length;
listener!({ kind: 'stop' });
assert.equal(calls.length, beforeIdleStop, 'disabled capabilities have no dispatch target');
session.unsubscribe();
assert.deepEqual(published.at(-1), ['new-workspace', 'settings']);
assert.equal(unsubscriptions, 0, 'remaining workspace owner retains native subscription');
workspace.unsubscribe();
assert.deepEqual(published.at(-1), []);
assert.equal(unsubscriptions, 1);
assert.equal(listener, undefined);
workspace.unsubscribe();
workspace.update({ stop: () => calls.push('removed owner') });
assert.equal(unsubscriptions, 1, 'cleanup is idempotent and updates cannot resurrect an owner');
const empty = registry.register({});
assert.equal(subscriptions, 2, 'registry supports StrictMode cleanup and remount');
empty.unsubscribe();
assert.equal(unsubscriptions, 2);
console.log('desktop bridge behavioral tests passed');
