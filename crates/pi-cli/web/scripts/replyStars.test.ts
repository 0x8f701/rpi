#!/usr/bin/env node
// Focused reply-star regression for src/replyStars.ts + the authoritative
// entryId projection in src/transcript.ts. Behavioral assertions only (in-memory
// StorageLike, no DOM), mirroring the sessionPreference/hostToken harness style.
// Registration in the package.json esbuild chain is owned by the main agent;
// until registered, run it the same way as sibling scripts: bundle with
// esbuild for node, then execute.
//
// Exit codes: 0 = every assertion held; 1 = a contract regressed.
import { loadReplyStars, saveReplyStars, deriveStarredReplies } from '../src/replyStars.ts';
import { messagesToItems } from '../src/transcript.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function makeStorage(store = new Map()) {
  return {
    getItem: (key) => (store.has(key) ? store.get(key) : null),
    setItem: (key, value) => { store.set(key, String(value)); },
    removeItem: (key) => { store.delete(key); },
    keys: () => [...store.keys()],
    _map: store,
  };
}

const scopeA = { authority: 'hostA:1', sessionId: 's1' };
const scopeA2 = { authority: 'hostA:1', sessionId: 's2' };
const scopeB = { authority: 'hostB:2', sessionId: 's1' };
const keyFor = (scope) =>
  `rpi-web-reply-stars:${encodeURIComponent(scope.authority.trim())}:${encodeURIComponent(scope.sessionId.trim())}`;

// ---- key shape + listener/session isolation (identical id strings) ----
{
  const storage = makeStorage();
  saveReplyStars(storage, scopeA, new Set(['entry-1']));
  check(
    'key follows the encoded authority:session scope convention',
    storage.keys().length === 1 && storage.keys()[0] === keyFor(scopeA),
    storage.keys().join(','),
  );
  check('reload from the same bucket sees the starred id', loadReplyStars(makeStorage(storage._map), scopeA).has('entry-1'));
  check('other listener authority isolated', loadReplyStars(storage, scopeB).size === 0);
  check('other session of the same authority isolated', loadReplyStars(storage, scopeA2).size === 0);

  // Same entry-id string starred in another session must not leak either way.
  saveReplyStars(storage, scopeA2, new Set(['entry-1']));
  check('same id in another session is its own star', loadReplyStars(storage, scopeA2).has('entry-1'));
  check('scoped buckets stay distinct', keyFor(scopeA) !== keyFor(scopeA2) && keyFor(scopeA) !== keyFor(scopeB));
  saveReplyStars(storage, scopeA2, new Set());
  check('unstarring one session leaves the other', loadReplyStars(storage, scopeA).has('entry-1') && loadReplyStars(storage, scopeA2).size === 0);

  // Trimmed scope maps to the same exact bucket; no origin-wide fallback.
  const trimmed = { authority: '  hostA:1 ', sessionId: ' s1 ' };
  check('scope trims to the exact bucket', loadReplyStars(storage, trimmed).has('entry-1'));
}

// ---- reload preserves; unstar removes; deterministic sorted ids ----
{
  const storage = makeStorage();
  saveReplyStars(storage, scopeA, new Set(['entry-2', 'entry-1']));
  check('payload is a sorted JSON id array (no bodies)', storage._map.get(keyFor(scopeA)) === '["entry-1","entry-2"]');
  const reload = makeStorage(storage._map);
  const loaded = loadReplyStars(reload, scopeA);
  check('reload restores both stars', loaded.size === 2 && loaded.has('entry-1') && loaded.has('entry-2'));

  saveReplyStars(reload, scopeA, new Set(['entry-2']));
  check('unstar persists across reload', !loadReplyStars(makeStorage(storage._map), scopeA).has('entry-1'));

  saveReplyStars(reload, scopeA, new Set());
  check('empty set removes the key (no empty bucket)', !storage._map.has(keyFor(scopeA)));
  check('empty reload reads as no stars', loadReplyStars(makeStorage(storage._map), scopeA).size === 0);
}

// ---- null storage: explicit empty load, action fails loudly ----
{
  check('null storage load is explicitly empty', loadReplyStars(null, scopeA).size === 0);
  let message = '';
  try {
    saveReplyStars(null, scopeA, new Set(['entry-1']));
  } catch (error) {
    message = error instanceof Error ? error.message : String(error);
  }
  check('null storage save throws (never fakes persistence)', /not available/.test(message) && /browser storage/.test(message), message);
  check('null storage save reports the scope', message.includes('hostA:1/s1'), message);
}

// ---- malformed stored payloads reject rather than silently empty ----
{
  const bad = ['not json', '{"entry-1":true}', '[1]', '[""]', '[null]', '["ok",2]'];
  for (const payload of bad) {
    const storage = makeStorage();
    storage._map.set(keyFor(scopeA), payload);
    let message = '';
    try {
      loadReplyStars(storage, scopeA);
    } catch (error) {
      message = error instanceof Error ? error.message : String(error);
    }
    check(`malformed payload rejected (${payload})`, /malformed/.test(message) && /expected a JSON array of entry ids/.test(message), message);
    check(`malformed payload names its scope (${payload})`, message.includes('hostA:1/s1'), message);
  }
}

// ---- storage failures throw with context; last-good bytes survive ----
{
  const storage = makeStorage();
  saveReplyStars(storage, scopeA, new Set(['entry-1']));

  const readFail = makeStorage(storage._map);
  readFail.getItem = () => { throw new Error('SecurityError'); };
  let readMessage = '';
  try {
    loadReplyStars(readFail, scopeA);
  } catch (error) {
    readMessage = error instanceof Error ? error.message : String(error);
  }
  check('read failure throws instead of returning empty', /read failed/.test(readMessage) && /SecurityError/.test(readMessage), readMessage);
  check('read failure carries scope context', readMessage.includes('hostA:1/s1'), readMessage);

  const writeFail = makeStorage(storage._map);
  writeFail.setItem = () => { throw new Error('QuotaExceededError'); };
  let writeMessage = '';
  try {
    saveReplyStars(writeFail, scopeA, new Set(['entry-2']));
  } catch (error) {
    writeMessage = error instanceof Error ? error.message : String(error);
  }
  check('write failure throws with context', /save failed/.test(writeMessage) && /QuotaExceededError/.test(writeMessage), writeMessage);
  check('last-good stars survive a failed save', loadReplyStars(makeStorage(storage._map), scopeA).has('entry-1'));

  const removeFail = makeStorage(storage._map);
  removeFail.removeItem = () => { throw new Error('blocked'); };
  let removeMessage = '';
  try {
    saveReplyStars(removeFail, scopeA, new Set());
  } catch (error) {
    removeMessage = error instanceof Error ? error.message : String(error);
  }
  check('failed unstar throws instead of reporting success', /save failed/.test(removeMessage), removeMessage);
  check('failed unstar keeps the previous stars', loadReplyStars(makeStorage(storage._map), scopeA).has('entry-1'));
}

// ---- invalid scope never degrades to a shared bucket ----
{
  let message = '';
  try {
    loadReplyStars(makeStorage(), { authority: 'hostA:1', sessionId: '   ' });
  } catch (error) {
    message = error instanceof Error ? error.message : String(error);
  }
  check('empty session id rejected', /non-empty listener authority and session id/.test(message), message);
  let authorityMessage = '';
  try {
    saveReplyStars(makeStorage(), { authority: '', sessionId: 's1' }, new Set(['entry-1']));
  } catch (error) {
    authorityMessage = error instanceof Error ? error.message : String(error);
  }
  check('empty authority rejected', /non-empty listener authority and session id/.test(authorityMessage), authorityMessage);
}

// ---- messagesToItems: ONLY authoritative assistant row ids become entryId ----
const rows = [
  { id: 'entry-user', message: { role: 'user', content: [{ type: 'text', text: 'question' }] } },
  { id: 'entry-a1', message: { role: 'assistant', content: [{ type: 'text', text: 'same body' }] } },
  { id: 'entry-a2', message: { role: 'assistant', content: [{ type: 'text', text: 'same body' }] } },
  { message: { role: 'assistant', content: [{ type: 'text', text: 'no row id' }] } },
  { id: 'entry-fail', message: { role: 'assistant', content: [], stopReason: 'error', errorMessage: 'boom' } },
  { id: 'entry-tool-row', message: { role: 'assistant', content: [{ type: 'toolCall', id: 'call-1', name: 'Read', arguments: { path: 'a' } }] } },
];
{
  const items = messagesToItems(rows);
  const assistants = items.filter((item) => item.kind === 'assistant');
  check('assistant row ids become entryId', assistants.map((item) => item.entryId).join(',') === 'entry-a1,entry-a2,,entry-fail');
  check('entryId is the authoritative row id, not a fresh id', assistants[0].entryId === 'entry-a1' && assistants[0].id === 'entry-a1');
  const noId = assistants[2];
  check('assistant row without an id gets no entryId', !('entryId' in noId));
  check('failure assistant row with an id gets entryId', assistants[3].entryId === 'entry-fail');
  const userItem = items.find((item) => item.kind === 'user');
  check('user item never gets entryId', userItem !== undefined && !('entryId' in userItem));
  const toolCards = items.filter((item) => item.kind === 'toolCard');
  check('tool cards never get entryId', toolCards.length === 1 && !('entryId' in toolCards[0]));

  const starred = new Set(['entry-a1', 'entry-a2', 'entry-fail']);
  const derived = deriveStarredReplies(items, starred);
  check('derived order follows the transcript', derived.map((reply) => reply.entryId).join(',') === 'entry-a1,entry-a2,entry-fail');
  check('derived indexes point at the assistant items',
    derived.every((reply) => items[reply.transcriptIndex] === assistants.find((item) => item.entryId === reply.entryId)));
  check('identical reply bodies stay independently starred',
    derived[0].preview === 'same body' && derived[1].preview === 'same body' && derived[0].entryId !== derived[1].entryId);

  const one = deriveStarredReplies(items, new Set(['entry-a2']));
  check('unstarred replies dropped', one.length === 1 && one[0].entryId === 'entry-a2');
  check('starred id missing from items stays invisible', deriveStarredReplies(items, new Set(['entry-hidden'])).length === 0);
  const none = deriveStarredReplies(items, new Set());
  check('no stars derives no replies', none.length === 0);
}

// ---- derive: authoritative entryId + display itemId + transcript order ----
{
  const items = [
    { kind: 'user', id: 'u1', text: 'hi', optimistic: false },
    { kind: 'assistant', id: 'a-live-1', status: 'final', blocks: [{ type: 'text', text: 'dup' }], entryId: 'entry-x' },
    { kind: 'assistant', id: 'a-live-2', status: 'final', blocks: [{ type: 'text', text: 'dup' }], entryId: 'entry-y' },
    { kind: 'assistant', id: 'a-stream', status: 'streaming', blocks: [{ type: 'text', text: 'partial' }], entryId: 'entry-x' },
    { kind: 'assistant', id: 'a-plain', status: 'final', blocks: [{ type: 'text', text: 'unstarred' }] },
  ];
  const reversed = deriveStarredReplies(items, new Set(['entry-y', 'entry-x']));
  check('derive preserves transcript order regardless of set order', reversed.map((reply) => reply.entryId).join(',') === 'entry-x,entry-y');
  check('derive records ascending transcript indexes', reversed.map((reply) => reply.transcriptIndex).join(',') === '1,2');
  check('derive keeps the display item id, not the entry id', reversed.map((reply) => reply.itemId).join(',') === 'a-live-1,a-live-2');
  check('duplicate bodies yield distinct ids on both axes',
    reversed[0].entryId !== reversed[1].entryId && reversed[0].itemId !== reversed[1].itemId);
  check('streaming item is never a starred reply', reversed.length === 2);
  check('derived reply carries only preview data', Object.keys(reversed[0]).sort().join(',') === 'entryId,itemId,preview,transcriptIndex');
}

// ---- preview: plain text only, single line, bounded to 80 chars ----
{
  const items = messagesToItems([
    {
      id: 'entry-long',
      message: {
        role: 'assistant',
        content: [
          { type: 'thinking', thinking: 'SECRET-PLAN' },
          { type: 'text', text: `first line ${'x'.repeat(120)}\nsecond line` },
        ],
      },
    },
  ]);
  const reply = deriveStarredReplies(items, new Set(['entry-long']))[0];
  check('preview exists for the starred reply', reply !== undefined && reply.entryId === 'entry-long');
  check('preview excludes thinking blocks', !reply.preview.includes('SECRET-PLAN'));
  check('preview is a single line', !reply.preview.includes('\n') && !reply.preview.includes('second line'));
  check('preview is bounded to 80 chars with an ellipsis', reply.preview.length === 80 && reply.preview.endsWith('\u2026'), `${reply.preview.length}`);
  check('preview starts with the first plain line', reply.preview.startsWith('first line '));
}

console.log(`\nreplyStars.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.map((failure) => `  FAIL ${failure}`).join('\n'));
  process.exit(1);
}
process.exit(0);
