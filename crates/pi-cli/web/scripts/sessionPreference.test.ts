#!/usr/bin/env node
// Host-scoped session preference regression for src/sessionPreference.ts —
// proves a session saved for host A never restores on host B, storage
// exceptions degrade fail-soft, and the catalog selector prefers the saved
// session, else the first row, else null. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Drives an in-memory StorageLike directly (no DOM) — behavioral assertions
// on the storage/selector contract, not source strings.
import {
  sessionPreferenceKey,
  loadSessionPreference,
  saveSessionPreference,
  selectSessionFromCatalog,
} from '../src/sessionPreference.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function makeStorage() {
  const store = new Map();
  return {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => { store.set(k, String(v)); },
    removeItem: (k) => { store.delete(k); },
    _has: (k) => store.has(k),
  };
}

// ---- sessionPreferenceKey: per-authority, encoded (exact trimmed), distinct ----
{
  check(
    'scoped key encodes exact trimmed authority',
    sessionPreferenceKey('127.0.0.1:8765') === `rpi-web-session:${encodeURIComponent('127.0.0.1:8765')}`,
  );
  check('scoped keys for A and B differ', sessionPreferenceKey('hostA:1') !== sessionPreferenceKey('hostB:2'));
  check('scoped key encodes the colon', sessionPreferenceKey('hostA:1') === 'rpi-web-session:hostA%3A1');
  check('trim applied before encoding', sessionPreferenceKey('  hostA:1  ') === sessionPreferenceKey('hostA:1'));
}

// ---- authority isolation: save A, B stays empty; B independent; clearing B doesn't clear A ----
{
  const s = makeStorage();
  saveSessionPreference(s, 'hostA:1', 'sid-A');
  check('A saved under A scoped key', s.getItem(sessionPreferenceKey('hostA:1')) === 'sid-A');
  check('B loads empty (no cross-host preference)', loadSessionPreference(s, 'hostB:2') === '');
  check('A preference remains only under A key', s.getItem(sessionPreferenceKey('hostA:1')) === 'sid-A');
  check('B scoped key absent', !s._has(sessionPreferenceKey('hostB:2')));
  saveSessionPreference(s, 'hostB:2', 'sid-B');
  check('A restores A', loadSessionPreference(s, 'hostA:1') === 'sid-A');
  check('B restores B', loadSessionPreference(s, 'hostB:2') === 'sid-B');
  saveSessionPreference(s, 'hostB:2', '');
  check('clearing B removes B key', !s._has(sessionPreferenceKey('hostB:2')));
  check('clearing B does not clear A', loadSessionPreference(s, 'hostA:1') === 'sid-A');
}

// ---- restored listener keeps click-save and reload-load aligned ----
{
  const pageHost = 'localhost:8090';
  const connectionHost = '127.0.0.1:8090';
  const rows = [
    { sessionId: 's1', path: 'p1' },
    { sessionId: 's2', path: 'p2' },
  ];
  const s = makeStorage();
  saveSessionPreference(s, connectionHost, 's2');
  check('page host has no cross-listener preference', loadSessionPreference(s, pageHost) === '');
  const restored = selectSessionFromCatalog(rows, loadSessionPreference(s, connectionHost));
  check('restored listener selects the saved non-first session', restored !== null && restored.sessionId === 's2');
  check('preference stored under connection authority', s._has(sessionPreferenceKey(connectionHost)));
  check('preference not stored under page authority', !s._has(sessionPreferenceKey(pageHost)));
}

// ---- storage exceptions / null storage: degrade to ''/no-op, never throw ----
{
  const throwing = {
    getItem: () => { throw new Error('blocked'); },
    setItem: () => { throw new Error('quota'); },
    removeItem: () => { throw new Error('quota'); },
  };
  check('load with throwing storage returns empty', loadSessionPreference(throwing, 'hostA:1') === '');
  let threw = false;
  try { saveSessionPreference(throwing, 'hostA:1', 'sid-A'); } catch { threw = true; }
  check('save with throwing storage never throws', !threw);
  check('null storage loads empty', loadSessionPreference(null, 'hostA:1') === '');
  let threwNull = false;
  try { saveSessionPreference(null, 'hostA:1', 'sid-A'); } catch { threwNull = true; }
  check('null storage save never throws', !threwNull);
  check('empty save removes the key', (() => {
    const s = makeStorage();
    saveSessionPreference(s, 'hostA:1', 'sid-A');
    saveSessionPreference(s, 'hostA:1', '');
    return !s._has(sessionPreferenceKey('hostA:1'));
  })());
}

// ---- selector: saved match wins even when not the first row ----
{
  const rows = [
    { sessionId: 's1', path: 'p1' },
    { sessionId: 's2', path: 'p2' },
    { sessionId: 's3', path: 'p3' },
  ];
  const pick = selectSessionFromCatalog(rows, 's2');
  check('saved match selected (not first row)', pick !== null && pick.sessionId === 's2' && pick.path === 'p2');
}

// ---- selector: saved missing (or empty) -> first row ----
{
  const rows = [
    { sessionId: 's1', path: 'p1' },
    { sessionId: 's2', path: 'p2' },
  ];
  const pickMissing = selectSessionFromCatalog(rows, 'gone');
  check('saved missing -> first row', pickMissing !== null && pickMissing.sessionId === 's1' && pickMissing.path === 'p1');
  const pickEmpty = selectSessionFromCatalog(rows, '');
  check('no saved id -> first row', pickEmpty !== null && pickEmpty.sessionId === 's1');
  const pickBlank = selectSessionFromCatalog(rows, '   ');
  check('whitespace saved id -> first row', pickBlank !== null && pickBlank.sessionId === 's1');
}

// ---- selector: empty / unusable catalog -> null ----
{
  check('empty catalog -> null', selectSessionFromCatalog([], 's1') === null);
  check('undefined catalog -> null', selectSessionFromCatalog(undefined, 's1') === null);
  const junk = [
    { sessionId: '', path: 'p0' },
    null,
    { sessionId: 's1', path: '' },
  ];
  check('all-unusable catalog -> null', selectSessionFromCatalog(junk, 's1') === null);
  const mixed = [
    { sessionId: '', path: 'p0' },
    null,
    { sessionId: 's2', path: 'p2' },
  ];
  const pick = selectSessionFromCatalog(mixed, 's2');
  check('unusable rows skipped; saved match found', pick !== null && pick.sessionId === 's2' && pick.path === 'p2');
  const pickFirst = selectSessionFromCatalog(mixed, 'missing');
  check('unusable rows skipped; first usable returned', pickFirst !== null && pickFirst.sessionId === 's2');
}

console.log(`\nsessionPreference.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.map((f) => `  FAIL ${f}`).join('\n'));
  process.exit(1);
}
process.exit(0);
