#!/usr/bin/env node
// Host-scoped token storage regression for src/hostToken.ts — proves fresh
// loads use the serving page authority even when obsolete active-host state is
// present, tokens remain authority-scoped, and the legacy global token migrates
// ONLY to the initial authority. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Drives an in-memory StorageLike directly (no DOM) — behavioral assertions
// on the storage contract, not source strings.
import {
  LEGACY_TOKEN_KEY,
  hostTokenKey,
  loadTokenForAuthority,
  normalizeHostAuthority,
  saveTokenForAuthority,
  loadInitialAuthorityToken,
} from '../src/hostToken.ts';
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

// ---- hostTokenKey: per-authority, encoded (exact trimmed), distinct ----
{
  check('legacy key constant is rpi-web-token', LEGACY_TOKEN_KEY === 'rpi-web-token');
  check('scoped key encodes exact trimmed authority', hostTokenKey('127.0.0.1:8765') === `rpi-web-token:${encodeURIComponent('127.0.0.1:8765')}`);
  check('scoped keys for A and B differ', hostTokenKey('a:1') !== hostTokenKey('b:2'));
  check('scoped key encodes the colon', hostTokenKey('a:1') === 'rpi-web-token:a%3A1');
  check('trim applied before encoding', hostTokenKey('  a:1  ') === hostTokenKey('a:1'));
}


// ---- (1) save A, switch to B (absent) => connect pair B/empty, A stays only A ----
{
  const s = makeStorage();
  saveTokenForAuthority(s, 'hostA:1', 'tokenA');
  check('A saved under A scoped key', s.getItem(hostTokenKey('hostA:1')) === 'tokenA');
  // Loading B independently reads B's scoped token only (never legacy, never A).
  const bToken = loadTokenForAuthority(s, 'hostB:2');
  check('B loads empty (no B token) — connect pair B/empty', bToken === '');
  check('A token remains only under A key (not leaked to B)', s.getItem(hostTokenKey('hostA:1')) === 'tokenA');
  check('B scoped key absent', !s._has(hostTokenKey('hostB:2')));
  check('save never writes the legacy key', !s._has(LEGACY_TOKEN_KEY));
}

// ---- (2) save B independently; A/B restore respective; clearing B doesn't clear A ----
{
  const s = makeStorage();
  saveTokenForAuthority(s, 'hostA:1', 'tokenA');
  saveTokenForAuthority(s, 'hostB:2', 'tokenB');
  check('A restores A', loadTokenForAuthority(s, 'hostA:1') === 'tokenA');
  check('B restores B', loadTokenForAuthority(s, 'hostB:2') === 'tokenB');
  // Clear B: empty -> remove B scoped key.
  saveTokenForAuthority(s, 'hostB:2', '');
  check('B cleared (empty -> removed)', !s._has(hostTokenKey('hostB:2')));
  check('clearing B does NOT clear A', loadTokenForAuthority(s, 'hostA:1') === 'tokenA');
}

// ---- (3) legacy migrates ONLY to initial A; legacy removed; later B loads empty ----
{
  const s = makeStorage();
  s.setItem(LEGACY_TOKEN_KEY, 'legacyToken');
  // Initial load for authority A migrates legacy -> A scoped, removes legacy.
  const initial = loadInitialAuthorityToken(s, 'hostA:1');
  check('initial A load returns migrated legacy token', initial === 'legacyToken');
  check('legacy migrated to A scoped key', s.getItem(hostTokenKey('hostA:1')) === 'legacyToken');
  check('legacy key removed after migration', !s._has(LEGACY_TOKEN_KEY));
  // Later arbitrary B load reads scoped only -> empty (legacy gone).
  const bToken = loadTokenForAuthority(s, 'hostB:2');
  check('later B load empty (legacy is NOT a fallback)', bToken === '');
  check('A still has its migrated token', loadTokenForAuthority(s, 'hostA:1') === 'legacyToken');
}

// ---- (3b) scoped A precedence over legacy (scoped already present) ----
{
  const s = makeStorage();
  s.setItem(LEGACY_TOKEN_KEY, 'legacyToken');
  saveTokenForAuthority(s, 'hostA:1', 'scopedTokenA'); // scoped already set
  const initial = loadInitialAuthorityToken(s, 'hostA:1');
  check('scoped A chosen over legacy', initial === 'scopedTokenA');
  check('legacy removed even when scoped chosen', !s._has(LEGACY_TOKEN_KEY));
  check('scoped A unchanged (not overwritten by legacy)', s.getItem(hostTokenKey('hostA:1')) === 'scopedTokenA');
}

// ---- (3c) no legacy + no scoped => initial empty; nothing migrated ----
{
  const s = makeStorage();
  const initial = loadInitialAuthorityToken(s, 'hostC:3');
  check('initial empty when no legacy and no scoped', initial === '');
  check('legacy absent', !s._has(LEGACY_TOKEN_KEY));
  check('scoped C absent (nothing migrated)', !s._has(hostTokenKey('hostC:3')));
}

// ---- (3d) failed scoped write retains legacy across StrictMode double mount ----
{
  const store = new Map([[LEGACY_TOKEN_KEY, 'legacyToken']]);
  let scopedWrites = 0;
  const s = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => {
      if (k === hostTokenKey('hostA:1')) {
        scopedWrites += 1;
        throw new Error('scoped storage blocked');
      }
    },
    removeItem: (k) => { store.delete(k); },
  };
  const firstMount = loadInitialAuthorityToken(s, 'hostA:1');
  const secondMount = loadInitialAuthorityToken(s, 'hostA:1');
  check('failed migration still returns legacy on first mount', firstMount === 'legacyToken');
  check('failed migration still returns legacy on StrictMode remount', secondMount === 'legacyToken');
  check('failed migration retains the only legacy copy', store.get(LEGACY_TOKEN_KEY) === 'legacyToken');
  check('failed migration never fabricates the scoped value', !store.has(hostTokenKey('hostA:1')));
  check('StrictMode remount retries the scoped write', scopedWrites === 2);
}

// ---- empty/whitespace authority gets its own bucket, distinct from a real host ----
{
  const s = makeStorage();
  saveTokenForAuthority(s, 'hostA:1', 'tokenA');
  saveTokenForAuthority(s, '   ', 'emptyHostToken');
  check('empty-host key distinct from real host', hostTokenKey('   ') !== hostTokenKey('hostA:1'));
  check('empty-host load returns its own value', loadTokenForAuthority(s, '   ') === 'emptyHostToken');
  check('real host unaffected by empty-host save', loadTokenForAuthority(s, 'hostA:1') === 'tokenA');
}

// ---- fresh-load authority contract: the page authority always boots ----
// A persisted selected-host pointer (rpi-web-selected-host) or recent-hosts
// value is advisory UI state. It must NEVER override the fresh-load boot
// authority, and the legacy token migration must therefore never target a
// non-page host's scoped key.
{
  const s = makeStorage();
  const pageAuthority = 'localhost:50003';
  const otherHost = '127.0.0.1:50003';
  s.setItem('rpi-web-selected-host', otherHost);
  check('persisted selected host does not change the page boot authority',
    normalizeHostAuthority(pageAuthority) === 'localhost:50003'
      && normalizeHostAuthority(pageAuthority) !== normalizeHostAuthority(s.getItem('rpi-web-selected-host')));
  s.setItem(LEGACY_TOKEN_KEY, 'pageToken');
  const bootToken = loadInitialAuthorityToken(s, pageAuthority);
  check('legacy migrates to the PAGE authority scoped key', bootToken === 'pageToken'
    && s.getItem(hostTokenKey(pageAuthority)) === 'pageToken');
  check('selected other host scoped key never written during page boot', !s._has(hostTokenKey(otherHost)));
  check('legacy key removed after successful page migration', !s._has(LEGACY_TOKEN_KEY));
  check('connect to the other host carries no token (no leak)', loadTokenForAuthority(s, otherHost) === '');
}

// ---- legacy + selected other host + failed scoped migration on the page ----
{
  const store = new Map();
  const pageAuthority = 'localhost:50003';
  const otherHost = '127.0.0.1:50003';
  store.set('rpi-web-selected-host', otherHost);
  store.set(LEGACY_TOKEN_KEY, 'pageToken');
  let pageScopedWrites = 0;
  const s = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => {
      if (k === hostTokenKey(pageAuthority)) {
        pageScopedWrites += 1;
        throw new Error('scoped storage blocked');
      }
      store.set(k, String(v));
    },
    removeItem: (k) => { store.delete(k); },
  };
  const bootToken = loadInitialAuthorityToken(s, pageAuthority);
  check('failed page migration still returns the legacy token for this boot', bootToken === 'pageToken');
  check('failed page migration never writes the OTHER host scoped key', !store.has(hostTokenKey(otherHost)));
  check('connect to the other host still carries no token', loadTokenForAuthority(s, otherHost) === '');
  check('legacy retained as the retry source', store.get(LEGACY_TOKEN_KEY) === 'pageToken');
  const retryToken = loadInitialAuthorityToken(s, pageAuthority);
  check('StrictMode remount retries the page scoped write', pageScopedWrites === 2 && retryToken === 'pageToken');
}

console.log(`\nhostToken.test: ${ran} assertions, ${failures.length} failure(s)`);

if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);