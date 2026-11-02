#!/usr/bin/env node
// Focused protocol regression for src/protocol.ts — per-page command-id
// namespacing (reload cannot reuse c1/c2 against the resident manager
// ledger) and subscription-snapshot cut application (cursor-0 bootstrap
// after a completed turn must not replay pre-snapshot user/assistant
// events on top of the authoritative messages). Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import {
  MAX_COMMAND_ID,
  createPageClientNamespace,
  nextCommandId,
  planReconnectBootstrap,
  planReconnectDisplayedCatchup,
  planReconnectRoster,
  planSubscriptionSnapshot,
  reconnectSubscriptionIds,
  reconnectSubscribeCursor,
  reconnectSavedSwitchIsRedundant,
  planReconnectDisplayedResync,
  reconnectRefillFailureToast,
  applyReconnectRefillCatch,
  planReconnectRefillFailure,
  reconnectRefillFinallyApplies,
  planDisplayedIncarnationRebind,
  restoreReconnectCatalog,
  shouldPersistPrimaryPreference,
  bootstrapSavedSwitchNeedsRetry,
  commandRejectedBySwitchPending,
  planSessionSnapshotApply,
  stampSessionStateIncarnation,
  reconnectRefillStillOwned,
  adoptReconnectRefillHold,
  planReconnectRefillStep,
} from '../src/protocol.ts';
import { catalogRuntimeSessionId } from '../src/sessionPreference.ts';
import { wireModelKey } from '../src/modelSelect.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function bytesOf(...values: number[]): (n: number) => Uint8Array {
  return (n) => Uint8Array.from(values.slice(0, n));
}

// ---- page namespace uniqueness: two pages never mint the same ids ----
{
  const pageA = createPageClientNamespace(bytesOf(1, 2, 3, 4, 5, 6));
  const pageB = createPageClientNamespace(bytesOf(9, 8, 7, 6, 5, 4));
  check('two pages mint distinct namespaces', pageA !== pageB, `${pageA} vs ${pageB}`);
  check('namespace is non-empty', pageA.length > 0 && pageB.length > 0);

  const a1 = nextCommandId(pageA, 1);
  const a2 = nextCommandId(pageA, 2);
  const b1 = nextCommandId(pageB, 1);
  const b2 = nextCommandId(pageB, 2);
  check('same-page seq 1 and 2 differ', a1 !== a2);
  check('reload-equivalent seq 1 does not collide across pages', a1 !== b1, `${a1} vs ${b1}`);
  check('reload-equivalent seq 2 does not collide across pages', a2 !== b2, `${a2} vs ${b2}`);

  // Prior bug: sendCommand minted bare `c1`/`c2`. After a reload the same
  // ids hit the resident ledger as a different request.
  const priorIds: Record<string, true> = { c1: true, c2: true };
  check('namespaced ids are never the prior c1/c2 collision shape',
    ![a1, a2, b1, b2].some((id) => priorIds[id] === true),
    JSON.stringify([a1, a2, b1, b2]));
  check('namespaced id embeds the page namespace',
    a1.includes(pageA) && a1.startsWith('c') && a1.endsWith('-1'),
    a1);
}

// ---- ledger simulator: the prior bare-cN shape collides; namespaced ids do not ----
{
  type Ledger = Map<string, string>;
  function admit(ledger: Ledger, id: string, digest: string): 'execute' | 'replay' | 'conflict' {
    const existing = ledger.get(id);
    if (existing === undefined) {
      ledger.set(id, digest);
      return 'execute';
    }
    return existing === digest ? 'replay' : 'conflict';
  }

  const prior = new Map<string, string>();
  check('prior page seq 1 admits c1', admit(prior, 'c1', 'get_state') === 'execute');
  check('prior page seq 2 admits c2', admit(prior, 'c2', 'prompt') === 'execute');
  // Reload remints c1/c2 for a different request against the resident ledger.
  check('prior reload c1 collides with a different request',
    admit(prior, 'c1', 'get_messages') === 'conflict');
  check('prior reload c2 collides with a different request',
    admit(prior, 'c2', 'get_state') === 'conflict');

  const namespaced = new Map<string, string>();
  const firstPage = createPageClientNamespace(bytesOf(10, 20, 30, 40, 50, 60));
  const reloadPage = createPageClientNamespace(bytesOf(11, 21, 31, 41, 51, 61));
  check('first page seq 1 admits',
    admit(namespaced, nextCommandId(firstPage, 1), 'get_state') === 'execute');
  check('first page seq 2 admits',
    admit(namespaced, nextCommandId(firstPage, 2), 'prompt') === 'execute');
  check('reload page seq 1 does not collide',
    admit(namespaced, nextCommandId(reloadPage, 1), 'get_messages') === 'execute');
  check('reload page seq 2 does not collide',
    admit(namespaced, nextCommandId(reloadPage, 2), 'get_state') === 'execute');
  check('same-page retry of the same request replays',
    admit(namespaced, nextCommandId(firstPage, 1), 'get_state') === 'replay');
}

// ---- MAX_COMMAND_ID remains an upper bound on any real namespaced id ----
{
  const ns = createPageClientNamespace(bytesOf(255, 255, 255, 255, 255, 255));
  const long = nextCommandId(ns, Number('9'.repeat(12)));
  check('MAX_COMMAND_ID is at least as long as a realistic namespaced id',
    MAX_COMMAND_ID.length >= long.length,
    `max=${MAX_COMMAND_ID.length} real=${long.length}`);
  check('MAX_COMMAND_ID is not the bare c<N> stand-in',
    MAX_COMMAND_ID.includes('-') && MAX_COMMAND_ID.startsWith('c'));
}

const CUT_IDENTITY = { sessionId: 's1', runtimeIncarnation: 'inc-1' };

function event(sequence: number, type = 'turn_started') {
  return { type, sequence, ...CUT_IDENTITY };
}

function coldCut(watermark = 4) {
  return {
    type: 'subscription_snapshot' as const,
    reason: 'cold-full' as const,
    watermark,
    ...CUT_IDENTITY,
    state: {},
    messages: [{ message: { role: 'user', content: [{ type: 'text', text: 'ship it' }] } }],
    events: [],
  };
}

function rejects(name: string, fn: () => unknown): void {
  let threw = false;
  try {
    fn();
  } catch {
    threw = true;
  }
  check(name, threw);
}

{
  const plan = planSubscriptionSnapshot(0, coldCut());
  check('cold-full applies authoritative messages exactly once', plan.applyMessages === true
    && plan.replayEvents.length === 0);
  check('cold-full returns exact runtime identity', plan.identity.sessionId === CUT_IDENTITY.sessionId
    && plan.identity.runtimeIncarnation === CUT_IDENTITY.runtimeIncarnation);
  check('cold-full installs the cut watermark even from cursor zero', plan.nextCursor === 4);
}

{
  const plan = planSubscriptionSnapshot(100, {
    ...coldCut(2),
    type: 'session_resync',
    reason: 'resync',
  });
  check('resync replaces messages without replay', plan.applyMessages === true
    && plan.replayEvents.length === 0);
  check('resync may lower a stale prior-incarnation cursor', plan.nextCursor === 2);
}

{
  const plan = planSubscriptionSnapshot(10, {
    type: 'subscription_snapshot',
    reason: 'warm-continuity',
    watermark: 12,
    ...CUT_IDENTITY,
    state: {},
    events: [event(9), event(10), event(11), event(12, 'turn_terminal')],
    messages: [],
  });
  check('warm continuity ignores events at or behind cursor', plan.replayEvents.map((row) => row.sequence).join(',') === '11,12');
  check('warm continuity applies attested message base', plan.applyMessages === true);
  check('warm continuity advances exactly through watermark', plan.nextCursor === 12);
}

rejects('warm continuity rejects a gap after cursor', () => planSubscriptionSnapshot(10, {
  type: 'subscription_snapshot', reason: 'warm-continuity', watermark: 12,
  ...CUT_IDENTITY, state: {}, messages: [], events: [event(12)],
}));
rejects('warm continuity rejects replay ending before watermark', () => planSubscriptionSnapshot(10, {
  type: 'subscription_snapshot', reason: 'warm-continuity', watermark: 12,
  ...CUT_IDENTITY, state: {}, messages: [], events: [event(11)],
}));
rejects('warm continuity rejects cursor regression', () => planSubscriptionSnapshot(10, {
  type: 'subscription_snapshot', reason: 'warm-continuity', watermark: 9,
  ...CUT_IDENTITY, state: {}, messages: [], events: [],
}));
rejects('cut rejects event session mismatch', () => planSubscriptionSnapshot(0, {
  ...coldCut(1), events: [{ ...event(1), sessionId: 'other' }],
}));
rejects('cut rejects event incarnation mismatch', () => planSubscriptionSnapshot(0, {
  ...coldCut(1), events: [{ ...event(1), runtimeIncarnation: 'other' }],
}));
rejects('cut rejects missing event sequence', () => planSubscriptionSnapshot(0, {
  ...coldCut(1), events: [{ type: 'turn_started', ...CUT_IDENTITY }],
}));
rejects('cut rejects fractional watermark', () => planSubscriptionSnapshot(0, {
  ...coldCut(), watermark: 1.5,
}));
rejects('planner rejects fractional client cursor', () => planSubscriptionSnapshot(1.5, coldCut()));

// ---- reconnect generation binds listener primary before stale sessions ----
{
  const plan = planReconnectBootstrap(
    ['old-a', 'old-b'],
    ['old-b', 'cursor-only'],
    'old-b',
  );
  check('reconnect first command is id-less get_state',
    plan.firstCommand.type === 'get_state' && !('sessionId' in plan.firstCommand),
    JSON.stringify(plan.firstCommand));
  check('old active B remains deferred and cannot emit before primary bind',
    plan.deferredSessionIds.join(',') === 'old-a,old-b,cursor-only',
    JSON.stringify(plan.deferredSessionIds));
}

{
  const primary = { sessionId: 'primary', runtimeIncarnation: 'inc-p' };
  const displayed = { sessionId: 'displayed', runtimeIncarnation: 'inc-d' };
  const other = planReconnectDisplayedCatchup(primary, displayed);
  check('displayed catchup refills the displayed session, not the listener primary',
    other.refillSessionId === displayed.sessionId
    && other.displayed.sessionId === displayed.sessionId);
  check('displayed catchup subscribes primary and displayed at the new generation',
    other.subscribe.length === 2
    && other.subscribe[0]?.sessionId === primary.sessionId
    && other.subscribe[1]?.sessionId === displayed.sessionId);
  const same = planReconnectDisplayedCatchup(primary, primary);
  check('displayed-equals-primary catchup still refills and subscribes that runtime once',
    same.refillSessionId === primary.sessionId
    && same.subscribe.length === 1
    && same.subscribe[0]?.runtimeIncarnation === primary.runtimeIncarnation);
  const missing = planReconnectDisplayedCatchup(primary, null);
  check('missing displayed catchup uses the primary',
    missing.refillSessionId === primary.sessionId && missing.subscribe.length === 1);
  check('reconnect subscribe keeps a stored watermark and treats missing as zero',
    reconnectSubscribeCursor(42) === 42
    && reconnectSubscribeCursor(0) === 0
    && reconnectSubscribeCursor(undefined) === 0
    && reconnectSubscribeCursor(-1) === 0);
  check('reconnect refill failure toast keeps last-good wording',
    reconnectRefillFailureToast(new Error('messages response missing messages'))
      === 'reconnect refill failed: messages response missing messages'
    && reconnectRefillFailureToast(new Error('  ')) === 'reconnect refill failed: request failed'
    && reconnectRefillFailureToast('socket closed') === 'reconnect refill failed: socket closed');
  {
    const stale = { kind: 'stale' };
    const toasts: string[] = [];
    let staleThrown = false;
    try {
      applyReconnectRefillCatch(stale, stale, (message) => { toasts.push(message); });
    } catch (error) {
      staleThrown = error === stale;
    }
    applyReconnectRefillCatch(new Error('messages response missing messages'), stale, (message) => {
      toasts.push(message);
    });
    check('refill catch rethrows stale and toasts every other rejection',
      staleThrown
      && toasts.join(',') === 'reconnect refill failed: messages response missing messages');
    check('busy_capacity refill catch stays silent',
      planReconnectRefillFailure({
        stillOwned: true,
        error: Object.assign(new Error('too many concurrent RPC commands (limit 16)'), { rpcCode: 'busy_capacity' }),
        staleAbort: stale,
      }) === 'silent');
    {
      const capacityToasts: string[] = [];
      applyReconnectRefillCatch(
        Object.assign(new Error('too many concurrent RPC commands (limit 16)'), { rpcCode: 'busy_capacity' }),
        stale,
        (message) => { capacityToasts.push(message); },
      );
      check('busy_capacity refill catch does not toast', capacityToasts.length === 0);
    }
    check('superseded refill catch is silent even for an ordinary rejection',
      planReconnectRefillFailure({
        stillOwned: false,
        error: new Error('messages response missing messages'),
        staleAbort: stale,
      }) === 'silent'
      && planReconnectRefillFailure({
        stillOwned: true,
        error: new Error('messages response missing messages'),
        staleAbort: stale,
      }) === 'toast-and-recover'
      && planReconnectRefillFailure({
        stillOwned: true,
        error: stale,
        staleAbort: stale,
      }) === 'rethrow-stale');
    check('refill finally applies only for the operation that still owns the slot',
      reconnectRefillFinallyApplies(1, 1) === true
      && reconnectRefillFinallyApplies(1, 2) === false
      && reconnectRefillFinallyApplies(0, 0) === false);
  }
  const reboundDisplayed = { sessionId: 'saved', runtimeIncarnation: 'inc-old' };
  const reincarnated = { sessionId: 'saved', runtimeIncarnation: 'inc-new' };
  const rebound = planDisplayedIncarnationRebind(
    { displayed: reboundDisplayed, commandTarget: reboundDisplayed },
    reincarnated,
  );
  check('displayed incarnation rebind emits the new identity for activation',
    rebound !== null
    && rebound.displayed.runtimeIncarnation === 'inc-new'
    && rebound.commandTarget.runtimeIncarnation === 'inc-new');
  check('matching displayed incarnation does not rebind',
    planDisplayedIncarnationRebind(
      { displayed: reboundDisplayed, commandTarget: reboundDisplayed },
      reboundDisplayed,
    ) === null);
  check('saved-target restore is redundant only when that loaded runtime is already displayed',
    reconnectSavedSwitchIsRedundant('saved', 'primary', 'saved', { loaded: true, runtimeIncarnation: 'inc-s' }) === true
    && reconnectSavedSwitchIsRedundant('saved', 'primary', 'saved') === false
    && reconnectSavedSwitchIsRedundant('saved', 'primary', 'primary', { loaded: true, runtimeIncarnation: 'inc-s' }) === false
    && reconnectSavedSwitchIsRedundant('primary', 'primary', 'primary', { loaded: true, runtimeIncarnation: 'inc-p' }) === false);
  check(
    'warm reconnect resyncs sidebar when bootstrap bound the listener primary',
    planReconnectDisplayedResync({ boundSessionId: 'primary', displayedSessionId: 'saved' }) === 'resync',
  );
  check(
    'warm reconnect keeps sidebar when bound session is already displayed',
    planReconnectDisplayedResync({ boundSessionId: 'saved', displayedSessionId: 'saved' }) === 'keep',
  );
  check(
    'cold bind with no displayed session does not resync',
    planReconnectDisplayedResync({ boundSessionId: 'primary', displayedSessionId: null }) === 'keep'
      && planReconnectDisplayedResync({ boundSessionId: 'primary', displayedSessionId: '' }) === 'keep',
  );
}

{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'saved', path: 'saved.jsonl', loaded: true, runtimeIncarnation: 'inc-s' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
    ],
    savedSessionId: 'saved',
    primarySessionId: 'primary',
    displayedSessionId: 'saved',
    displayedRuntimeIncarnation: 'inc-s',
    switchSession: async (selected) => {
      calls.push(`switch:primary->${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('already-displayed saved target skips a redundant bootstrap switch',
    target?.sessionId === 'saved' && calls.join(',') === 'subscribe:bg-a:inc-a',
    JSON.stringify(calls));
}

{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'saved', path: 'saved.jsonl', loaded: false, runtimeIncarnation: null },
    ],
    savedSessionId: 'saved',
    primarySessionId: 'primary',
    displayedSessionId: 'saved',
    displayedRuntimeIncarnation: 'inc-s',
    switchSession: async (selected) => {
      calls.push(`switch:primary->${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('displayed saved target with catalog loaded:false still negotiates switch',
    target?.sessionId === 'saved' && calls.join(',') === 'switch:primary->saved',
    JSON.stringify(calls));
}

{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'saved', path: 'saved.jsonl', loaded: true, runtimeIncarnation: 'inc-s-new' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
    ],
    savedSessionId: 'saved',
    primarySessionId: 'primary',
    displayedSessionId: 'saved',
    displayedRuntimeIncarnation: 'inc-s-old',
    switchSession: async (selected) => {
      calls.push(`switch:primary->${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('already-displayed saved target resubscribes when the catalog incarnation changed',
    target?.sessionId === 'saved'
    && calls.join(',') === 'subscribe:saved:inc-s-new,subscribe:bg-a:inc-a',
    JSON.stringify(calls));
}

// ---- reconnect catalog subscribes every loaded row with exact identity ----
{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'saved', path: 'saved.jsonl', loaded: true, runtimeIncarnation: 'inc-s' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
      { sessionId: 'bg-b', path: 'bg-b.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: 'saved',
    primarySessionId: 'primary',
    switchSession: async (selected) => {
      calls.push(`switch:primary->${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('catalog restore selects the saved row', target?.sessionId === 'saved', JSON.stringify(target));
  check('catalog restore switches saved target first', calls[0] === 'switch:primary->saved', JSON.stringify(calls));
  check('catalog restore subscribes every other loaded row exactly once',
    calls.slice(1).join(',') === 'subscribe:bg-a:inc-a,subscribe:bg-b:inc-b',
    JSON.stringify(calls));
}

// ---- saved target switch failure falls back to the bound primary ----
{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'saved', path: 'saved.jsonl', loaded: true, runtimeIncarnation: 'inc-s' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
    ],
    savedSessionId: 'saved',
    primarySessionId: 'primary',
    switchSession: async () => null,
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('failed switch still returns the saved target', target?.sessionId === 'saved', JSON.stringify(target));
  check('failed switch keeps primary active; the never-activated saved row still subscribes as loaded',
    calls.join(',') === 'subscribe:saved:inc-s,subscribe:bg-a:inc-a',
    JSON.stringify(calls));
}

// ---- empty preference keeps the listener-selected primary ----
{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'newest', path: 'newest.jsonl', loaded: false },
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
    ],
    savedSessionId: '',
    primarySessionId: 'primary',
    switchSession: async (selected) => {
      calls.push(`switch:${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('empty preference keeps the listener primary', target === null, JSON.stringify(target));
  check('empty preference never switches and still subscribes loaded background rows',
    calls.join(',') === 'subscribe:bg-a:inc-a', JSON.stringify(calls));
}

// ---- unloaded and malformed rows are excluded; primary is never re-subscribed ----
{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'unloaded', path: 'unloaded.jsonl', loaded: false, runtimeIncarnation: 'inc-u' },
      { sessionId: 'no-inc', path: 'no-inc.jsonl', loaded: true },
      { sessionId: 'empty-inc', path: 'empty-inc.jsonl', loaded: true, runtimeIncarnation: '' },
      { sessionId: '', path: 'empty-id.jsonl', loaded: true, runtimeIncarnation: 'inc-e' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
    ],
    savedSessionId: 'missing',
    primarySessionId: 'primary',
    switchSession: async () => null,
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('missing saved id falls back to the first catalog row', target?.sessionId === 'primary', JSON.stringify(target));
  check('unloaded/malformed rows never subscribe and primary is skipped',
    calls.join(',') === 'subscribe:bg-a:inc-a',
    JSON.stringify(calls));
}

// ---- duplicate sessionId rows dedupe by exact runtime identity, first row wins ----
{
  const calls: string[] = [];
  const target = await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'dup', path: 'dup-new.jsonl', loaded: true, runtimeIncarnation: 'inc-new' },
      { sessionId: 'dup', path: 'dup-stale.jsonl', loaded: true, runtimeIncarnation: 'inc-stale' },
      { sessionId: 'bg-a', path: 'bg-a.jsonl', loaded: true, runtimeIncarnation: 'inc-a' },
    ],
    savedSessionId: 'missing',
    primarySessionId: 'primary',
    switchSession: async () => null,
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('duplicate sessionId keeps the first catalog row incarnation',
    calls.join(',') === 'subscribe:dup:inc-new,subscribe:bg-a:inc-a',
    JSON.stringify(calls));
}

// ---- imported foreign + native rows subscribe one runtime identity ----
{
  const calls: string[] = [];
  await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      {
        sessionId: 'foreign-catalog-id',
        path: 'foreign.jsonl',
        status: 'already_imported',
        nativeSessionId: 'native-runtime-id',
        loaded: true,
        runtimeIncarnation: 'inc-native',
      },
      {
        sessionId: 'native-runtime-id',
        path: 'native.jsonl',
        status: 'native',
        loaded: true,
        runtimeIncarnation: 'inc-native',
      },
    ],
    savedSessionId: 'missing',
    primarySessionId: 'primary',
    switchSession: async () => null,
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}:${identity.runtimeIncarnation}`),
  });
  check('already-imported and native rows subscribe the native runtime exactly once',
    calls.join(',') === 'subscribe:native-runtime-id:inc-native',
    JSON.stringify(calls));
}

// ---- authoritative roster: loaded identities plan; malformed rosters fail closed ----
{
  const complete = planReconnectRoster([
    { sessionId: 'primary', path: 'p.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
    {
      sessionId: 'foreign-catalog-id',
      path: 'f.jsonl',
      status: 'already_imported',
      nativeSessionId: 'native-id',
      loaded: true,
      runtimeIncarnation: 'inc-n',
    },
    { sessionId: 'unloaded', path: 'u.jsonl', loaded: false, runtimeIncarnation: 'inc-u' },
    { sessionId: 'bg', path: 'b.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
  ]);
  check('roster maps every loaded row with the imported alias normalized',
    complete.authoritative.get('primary') === 'inc-p'
    && complete.authoritative.get('native-id') === 'inc-n'
    && complete.authoritative.get('bg') === 'inc-b'
    && !complete.authoritative.has('foreign-catalog-id')
    && !complete.authoritative.has('unloaded')
    && complete.authoritative.size === 3
    && complete.omissionSafe,
    JSON.stringify([...complete.authoritative]));

  const malformed = planReconnectRoster([
    { sessionId: 'primary', path: 'p.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
    { sessionId: 'broken', path: 'x.jsonl', loaded: true },
    { sessionId: '', path: 'e.jsonl', loaded: true, runtimeIncarnation: 'inc-e' },
  ]);
  check('a loaded row without a usable identity marks the roster unsafe for omission pruning',
    malformed.authoritative.get('primary') === 'inc-p'
    && malformed.authoritative.size === 1
    && malformed.omissionSafe === false,
    JSON.stringify([...malformed.authoritative]));

  const empty = planReconnectRoster([]);
  check('an empty roster can never authorize omission pruning',
    empty.authoritative.size === 0 && empty.omissionSafe === false);

  const conflicted = planReconnectRoster([
    { sessionId: 'dup', path: 'd1.jsonl', loaded: true, runtimeIncarnation: 'inc-1' },
    { sessionId: 'dup', path: 'd2.jsonl', loaded: true, runtimeIncarnation: 'inc-2' },
    { sessionId: 'bg', path: 'b.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
  ]);
  check('conflicting incarnations for the same runtime cannot authorize invalidation',
    conflicted.omissionSafe === false
    && !conflicted.authoritative.has('dup')
    && conflicted.authoritative.get('bg') === 'inc-b'
    && conflicted.authoritative.size === 1,
    JSON.stringify([...conflicted.authoritative]));
}

// ---- retirement runs before the saved-target switch and before any subscription ----
{
  const calls: string[] = [];
  await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'saved', path: 'saved.jsonl', loaded: true, runtimeIncarnation: 'inc-s' },
      { sessionId: 'bg', path: 'bg.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: 'saved',
    primarySessionId: 'primary',
    switchSession: async (selected) => {
      calls.push(`switch:${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}`),
    retireResident: (plan) => calls.push(
      `retire:${plan.omissionSafe ? 'complete' : 'unsafe'}:${plan.authoritative.size}`,
    ),
  });
  check('retirement precedes the saved-target switch and every subscription',
    calls[0] === 'retire:complete:3'
    && calls[1] === 'switch:saved'
    && calls[2] === 'subscribe:bg'
    && calls.length === 3,
    JSON.stringify(calls));
}

// ---- retirement still runs on the primary fallback path (failed switch) ----
{
  const calls: string[] = [];
  await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      { sessionId: 'bg', path: 'bg.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: 'missing',
    primarySessionId: 'primary',
    switchSession: async () => null,
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}`),
    retireResident: (plan) => calls.push(`retire:${plan.omissionSafe}:${plan.authoritative.size}`),
  });
  check('primary fallback still retires before subscriptions',
    calls[0] === 'retire:true:2' && calls[1] === 'subscribe:bg',
    JSON.stringify(calls));
}

// ---- imported saved target binds the native runtime id so subscribe skips it ----
{
  const calls: string[] = [];
  await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      {
        sessionId: 'foreign-catalog-id',
        path: 'foreign.jsonl',
        status: 'already_imported',
        nativeSessionId: 'native-id',
        nativePath: 'native.jsonl',
        loaded: true,
        runtimeIncarnation: 'inc-n',
      },
      { sessionId: 'bg', path: 'bg.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: 'foreign-catalog-id',
    primarySessionId: 'primary',
    switchSession: async (selected) => {
      calls.push(`switch:${selected.sessionId}`);
      return 'native-id';
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}`),
    retireResident: (plan) => calls.push(`retire:${plan.authoritative.size}`),
  });
  check('imported saved target skips the native runtime and still subscribes other loaded rows',
    calls.join(',') === 'retire:3,switch:foreign-catalog-id,subscribe:bg',
    JSON.stringify(calls));
}

// ---- a foreign saved target skips the distinct post-import runtime id ----
{
  const calls: string[] = [];
  await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      {
        sessionId: 'foreign-catalog-id',
        path: 'foreign.jsonl',
        source: 'codex',
        status: 'foreign',
        loaded: true,
        runtimeIncarnation: 'inc-f',
      },
      { sessionId: 'imported-runtime', path: 'native.jsonl', loaded: true, runtimeIncarnation: 'inc-n' },
      { sessionId: 'bg', path: 'bg.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: 'foreign-catalog-id',
    primarySessionId: 'primary',
    switchSession: async (selected) => {
      calls.push(`switch:${selected.sessionId}`);
      return 'imported-runtime';
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}`),
    retireResident: (plan) => calls.push(`retire:${plan.authoritative.size}`),
  });
  check('foreign saved target skips the bound runtime, not the catalog id',
    calls.join(',') === 'retire:4,switch:foreign-catalog-id,subscribe:foreign-catalog-id,subscribe:bg',
    JSON.stringify(calls));
}

{
  check('empty saved preference may persist the listener primary',
    shouldPersistPrimaryPreference('', null) === true);
  check('a saved id never persists the listener primary, even when restore returned null',
    shouldPersistPrimaryPreference('saved', null) === false);
  check('a restored saved row never persists the listener primary',
    shouldPersistPrimaryPreference('saved', { sessionId: 'saved', path: 'saved.jsonl' }) === false);
  check('failed switch of a saved target retries after ready',
    bootstrapSavedSwitchNeedsRetry('saved', false) === true);
  check('successful switch of a saved target does not retry',
    bootstrapSavedSwitchNeedsRetry('saved', true) === false);
  check('absent saved preference does not retry',
    bootstrapSavedSwitchNeedsRetry('', false) === false);
}

{
  const calls: string[] = [];
  const imported = {
    sessionId: 'chat-572ffbd33c99218e65f67417',
    path: 'omp.jsonl',
    status: 'already_imported',
    nativeSessionId: '01a08bfa-native',
    nativePath: 'native.jsonl',
    loaded: true,
    runtimeIncarnation: 'inc-n',
  };
  await restoreReconnectCatalog({
    rows: [
      { sessionId: '01a08bfa-native', path: 'native.jsonl', loaded: true, runtimeIncarnation: 'inc-n' },
      imported,
      { sessionId: 'bg', path: 'bg.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: 'chat-572ffbd33c99218e65f67417',
    primarySessionId: '01a08bfa-native',
    switchSession: async (selected) => {
      calls.push(`switch:${selected.sessionId}`);
      return selected.sessionId;
    },
    subscribeSession: (identity) => calls.push(`subscribe:${identity.sessionId}`),
  });
  check('already-imported display name does not switch or get_messages when native UUID is already primary',
    calls.join(',') === 'subscribe:bg',
    JSON.stringify(calls));

  const callsByNative: string[] = [];
  await restoreReconnectCatalog({
    rows: [
      { sessionId: 'primary', path: 'primary.jsonl', loaded: true, runtimeIncarnation: 'inc-p' },
      imported,
      { sessionId: 'bg', path: 'bg.jsonl', loaded: true, runtimeIncarnation: 'inc-b' },
    ],
    savedSessionId: '01a08bfa-native',
    primarySessionId: 'primary',
    switchSession: async (selected) => {
      callsByNative.push(`switch:${catalogRuntimeSessionId(selected)}`);
      return '01a08bfa-native';
    },
    subscribeSession: (identity) => callsByNative.push(`subscribe:${identity.sessionId}`),
  });
  check('saved native UUID restores the already-imported row via switch_session, never the display name as runtime id',
    callsByNative.join(',') === 'switch:01a08bfa-native,subscribe:bg',
    JSON.stringify(callsByNative));
}

{
  check('deferred refill get_messages is allowed after restore enters pending',
    commandRejectedBySwitchPending(true, { allowDuringSwitch: true }) === false
    && commandRejectedBySwitchPending(true, { auxiliary: true }) === false
    && commandRejectedBySwitchPending(true) === true
    && commandRejectedBySwitchPending(false) === false);
  const stayOnB = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: 'inc-a',
    residentIncarnation: 'inc-a',
    displayedSessionId: 'B',
    targetSessionId: 'A',
  });
  const reactivateA = planSessionSnapshotApply({
    shouldActivate: true,
    wireIncarnation: 'inc-a',
    residentIncarnation: 'inc-a',
    displayedSessionId: 'B',
    targetSessionId: 'A',
  });
  const staleA = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: 'inc-old',
    residentIncarnation: 'inc-new',
    displayedSessionId: 'B',
    targetSessionId: 'A',
  });
  check('Stop A then switch B rejects activating A and drops a superseded A snapshot',
    stayOnB.apply === true && stayOnB.activate === false
    && reactivateA.apply === true && reactivateA.activate === true
    && staleA.apply === false && staleA.activate === false);
  const missingResident = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: 'inc-old',
    residentIncarnation: undefined,
    displayedSessionId: 'B',
    targetSessionId: 'A',
  });
  const emptyResident = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: 'inc-old',
    residentIncarnation: '',
    displayedSessionId: 'B',
    targetSessionId: 'A',
  });
  check('background snapshot with a missing resident cannot recreate an invalidated identity',
    missingResident.apply === false && missingResident.activate === false
    && emptyResident.apply === false && emptyResident.activate === false);
  const nestedCutState = {
    sessionId: 'B',
    model: { provider: 'cliproxy', id: 'gpt-5.6-sol', name: 'GPT-5.6 Sol' },
    thinkingLevel: 'medium',
  };
  const nestedDropped = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: (nestedCutState as { runtimeIncarnation?: unknown }).runtimeIncarnation,
    residentIncarnation: 'inc-b',
    displayedSessionId: 'A',
    targetSessionId: 'B',
  });
  const stampedCut = stampSessionStateIncarnation(nestedCutState, 'inc-b') as {
    runtimeIncarnation?: unknown;
    model?: { provider?: string; id?: string };
    thinkingLevel?: string;
  };
  const nestedApply = planSessionSnapshotApply({
    shouldActivate: false,
    wireIncarnation: stampedCut.runtimeIncarnation,
    residentIncarnation: 'inc-b',
    displayedSessionId: 'A',
    targetSessionId: 'B',
  });
  check('subscription nested RpcSessionState without incarnation cannot apply',
    nestedDropped.apply === false && nestedDropped.activate === false);
  check('stamping the cut identity lets nested model and thinking apply without activating',
    nestedApply.apply === true && nestedApply.activate === false
      && stampedCut.runtimeIncarnation === 'inc-b'
      && wireModelKey(stampedCut.model) === 'cliproxy/gpt-5.6-sol'
      && stampedCut.thinkingLevel === 'medium');
  const getStateEnvelope = stampSessionStateIncarnation(
    { runtimeIncarnation: 'inc-old', model: { provider: 'cliproxy', id: 'kept' } },
    'inc-new',
  ) as { runtimeIncarnation?: unknown };
  check('stamp never overwrites a nonempty wire incarnation',
    getStateEnvelope.runtimeIncarnation === 'inc-old');
  const hold = { sessionId: 'A', runtimeIncarnation: 'inc-old', socketGeneration: 3 };
  check('refill hold is owned only for the exact incarnation on the same socket',
    reconnectRefillStillOwned(hold, 'inc-old', 3) === true
    && reconnectRefillStillOwned(hold, 'inc-new', 3) === false
    && reconnectRefillStillOwned(hold, 'inc-old', 4) === false
    && reconnectRefillStillOwned(hold, undefined, 3) === false);
  const adopted = adoptReconnectRefillHold(hold, 'A', 'inc-new', 3);
  check('same-session resident reincarnation on the same socket adopts the new hold',
    adopted.runtimeIncarnation === 'inc-new'
    && reconnectRefillStillOwned(adopted, 'inc-new', 3) === true
    && planReconnectRefillStep({
      hold: adopted,
      residentIncarnation: 'inc-new',
      socketGenerationNow: 3,
      wireIncarnation: 'inc-new',
      displayedSessionId: 'A',
    }) === 'apply');
  check('adopt leaves the hold unchanged for another session or socket generation',
    adoptReconnectRefillHold(hold, 'B', 'inc-new', 3).runtimeIncarnation === 'inc-old'
    && adoptReconnectRefillHold(hold, 'A', 'inc-new', 4).runtimeIncarnation === 'inc-old'
    && adoptReconnectRefillHold(hold, 'A', '', 3).runtimeIncarnation === 'inc-old');
  const deferredHold = { sessionId: 'sess-b', runtimeIncarnation: 'inc-old', socketGeneration: 1 };
  check('deferred refill crossing incarnation replacement rejects before apply',
    planReconnectRefillStep({
      hold: deferredHold,
      residentIncarnation: 'inc-old',
      socketGenerationNow: 1,
      wireIncarnation: 'inc-old',
      displayedSessionId: 'sess-b',
    }) === 'apply'
    && planReconnectRefillStep({
      hold: deferredHold,
      residentIncarnation: 'inc-new',
      socketGenerationNow: 1,
      wireIncarnation: 'inc-old',
      displayedSessionId: 'sess-b',
    }) === 'reject'
    && planReconnectRefillStep({
      hold: deferredHold,
      residentIncarnation: 'inc-new',
      socketGenerationNow: 1,
      wireIncarnation: 'inc-new',
      displayedSessionId: 'sess-b',
    }) === 'reject');
}

{
  const nested = {
    sessionId: 'B',
    model: { provider: 'cliproxy', id: 'gpt-5.6-sol' },
    thinkingLevel: 'medium',
  };
  const stamped = stampSessionStateIncarnation(nested, 'inc-b') as {
    sessionId?: string;
    runtimeIncarnation?: unknown;
    model?: { provider?: string; id?: string };
    thinkingLevel?: string;
  };
  check('nested state without incarnation is stamped and keeps model fields',
    stamped !== nested
      && stamped.runtimeIncarnation === 'inc-b'
      && stamped.sessionId === 'B'
      && stamped.thinkingLevel === 'medium'
      && wireModelKey(stamped.model) === 'cliproxy/gpt-5.6-sol'
      && !Object.prototype.hasOwnProperty.call(nested, 'runtimeIncarnation'));
  const envelope = { runtimeIncarnation: 'inc-old', model: { provider: 'cliproxy', id: 'kept' } };
  check('nonempty wire incarnation is not overwritten',
    stampSessionStateIncarnation(envelope, 'inc-new') === envelope
      && envelope.runtimeIncarnation === 'inc-old');
  const emptyIncarnation = { sessionId: 'B' };
  check('empty incarnation argument returns the input unchanged',
    stampSessionStateIncarnation(nested, '') === nested
      && stampSessionStateIncarnation(emptyIncarnation, '') === emptyIncarnation);
  const arrayState: unknown[] = [];
  check('non-object state is returned as-is',
    stampSessionStateIncarnation(null, 'inc-b') === null
      && stampSessionStateIncarnation(undefined, 'inc-b') === undefined
      && stampSessionStateIncarnation('state', 'inc-b') === 'state'
      && stampSessionStateIncarnation(7, 'inc-b') === 7
      && stampSessionStateIncarnation(arrayState, 'inc-b') === arrayState);
}

console.log(`\nprotocol.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
