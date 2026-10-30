#!/usr/bin/env node
// Pure exact-runtime session-switch reducer matrix. Covers identity, cursor
// order, warm/cold, stale generation, unload, and bounded view slots.
import {
  MAX_LOADED_SESSIONS,
  MAX_SWITCH_MS,
  validateSwitchMilestones,
  SWITCH_HANDOFF_RESPONSE_MODE,
  SWITCH_PENDING_ERROR,
  normalizeCatalogSource,
  normalizeCatalogStatus,
  acceptAuthoritativeCut,
  acceptSwitchHandoff,
  beginSessionSwitch,
  buildSubscribeFrame,
  buildSwitchCommand,
  emptyViewSlot,
  evictViewSlot,
  failSessionSwitch,
  parseRuntimeKey,
  planFastSessionSwitch,
  parseSubscriptionCut,
  parseSessionHandoffEnvelope,
  parseSwitchHandoff,
  parseSessionUnload,
  runtimeKey,
  type FastSwitchContext,
  type CatalogSource,
  type ReadyRuntime,
  type RuntimeIdentity,
  type SessionDisplayState,
  type SessionSwitchTarget,
  type SessionViewSlot,
  sameRuntime,
  unloadRuntime,
  upsertViewSlot,
  type SwitchHandoffV1,
  type SwitchAction,
  parseSubscriptionError,
  shouldRetrySnapshotInProgress,
  SNAPSHOT_IN_PROGRESS_CODE,
  warmContinuityNeedsHistoryConfirm,
  planWarmHistoryConfirm,
  warmHistoryConfirmIsCurrent,
  preserveLiveBindCursor,
  bindAuthorityCursor,
  acceptedCutLiveCursor,
  switchFailureShouldToast,
  AUTHORITATIVE_CUT_TIMEOUT_MS,
  planAuthoritativeCutTick,
} from '../src/sessionSwitch.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const A: RuntimeIdentity = { sessionId: 'sess-a', runtimeIncarnation: 'inc-a1' };
const B1: RuntimeIdentity = { sessionId: 'sess-b', runtimeIncarnation: 'inc-b1' };
const B2: RuntimeIdentity = { sessionId: 'sess-b', runtimeIncarnation: 'inc-b2' };
const C: RuntimeIdentity = { sessionId: 'sess-c', runtimeIncarnation: 'inc-c1' };

function target(identity: RuntimeIdentity, path = `${identity.sessionId}.jsonl`): SessionSwitchTarget {
  return {
    path,
    catalogSource: 'rpi',
    catalogSessionId: identity.sessionId,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: identity.runtimeIncarnation,
  };
}

/** Foreign catalog rows resolve to a DISTINCT native runtime id; the target
 *  carries only the catalog identity plus a null (unloaded) warm hint. */
function foreignTarget(catalogSessionId: string, path = `${catalogSessionId}.jsonl`): SessionSwitchTarget {
  return {
    path,
    catalogSource: 'codex',
    catalogSessionId,
    catalogStatus: 'foreign',
    catalogRuntimeIncarnation: null,
  };
}

function idle(displayed: RuntimeIdentity = A): SessionDisplayState {
  return {
    displayed,
    commandTarget: displayed,
    pending: null,
    nextGeneration: 1,
  };
}

function handoffWire(identity: RuntimeIdentity, cursor: number, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    catalogSource: 'rpi',
    catalogSessionId: identity.sessionId,
    catalogStatus: 'native',
    subscriptionRequired: true,
    lifecycleResponseVersion: 1,
    cursor,
    session: nativeSessionRow(identity),
    ...extra,
  };
}
function nativeSessionRow(identity: RuntimeIdentity): Record<string, unknown> {
  return {
    source: 'rpi',
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    cwd: `<workspace>/${identity.sessionId}`,
    path: `<workspace>/sessions/${identity.sessionId}.jsonl`,
    status: 'native',
    loaded: true,
    messageCount: 1,
  };
}


function handoff(identity: RuntimeIdentity, cursor: number, extra: Record<string, unknown> = {}): SwitchHandoffV1 {
  return parseSwitchHandoff(handoffWire(identity, cursor, extra), {
    catalogSource: 'rpi',
    catalogSessionId: identity.sessionId,
    catalogStatus: 'native',
  });
}

function actionTypes(actions: SwitchAction[]): string {
  return actions.map((action) => action.type).join(',');
}

function rejectHandoff(
  name: string,
  value: unknown,
  expected: Pick<SessionSwitchTarget, 'catalogSource' | 'catalogSessionId' | 'catalogStatus'> = {
    catalogSource: 'rpi',
    catalogSessionId: B1.sessionId,
    catalogStatus: 'native',
  },
): void {
  let threw = false;
  try {
    parseSwitchHandoff(value, expected);
  } catch {
    threw = true;
  }
  check(name, threw);
}

function rejectCut(name: string, value: unknown): void {
  let threw = false;
  try {
    parseSubscriptionCut(value);
  } catch {
    threw = true;
  }
  check(name, threw);
}
const validHandoff = {
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  catalogSource: 'rpi',
  catalogSessionId: B1.sessionId,
  catalogStatus: 'native',
  subscriptionRequired: true,
  lifecycleResponseVersion: 1,
  cursor: 12,
  session: nativeSessionRow(B1),
};

rejectHandoff('handoff rejects missing incarnation', { ...validHandoff, runtimeIncarnation: undefined });
rejectHandoff('handoff rejects empty incarnation', { ...validHandoff, runtimeIncarnation: '' });
rejectHandoff('handoff rejects wrong version', { ...validHandoff, lifecycleResponseVersion: 2 });
rejectHandoff('handoff rejects false subscriptionRequired', { ...validHandoff, subscriptionRequired: false });
rejectHandoff('handoff rejects missing cursor', { ...validHandoff, cursor: undefined });
rejectHandoff('handoff rejects negative cursor', { ...validHandoff, cursor: -1 });
rejectHandoff('handoff rejects non-finite cursor', { ...validHandoff, cursor: Number.NaN });
rejectHandoff('native handoff rejects distinct runtime session id', { ...validHandoff, sessionId: 'native-runtime-9' });
rejectHandoff('handoff rejects missing catalogSource', { ...validHandoff, catalogSource: undefined });
rejectHandoff('handoff rejects unknown catalogSource', { ...validHandoff, catalogSource: 'claude' });
// Canonical-only sources: the historic wire aliases are gone — no alias
// collapses onto a provider and no missing/empty value defaults to rpi.
rejectHandoff('handoff rejects the native source alias', { ...validHandoff, catalogSource: 'native' });
rejectHandoff('handoff rejects the primary alias', { ...validHandoff, catalogSource: 'primary' });
rejectHandoff('handoff rejects the grok/hyper alias', { ...validHandoff, catalogSource: 'grok/hyper' });
rejectHandoff('handoff rejects an empty catalogSource', { ...validHandoff, catalogSource: '' });
rejectHandoff('handoff rejects a padded catalogSource', { ...validHandoff, catalogSource: ' rpi ' });
rejectHandoff('handoff rejects an uppercase catalogSource', { ...validHandoff, catalogSource: 'RPI' });
rejectHandoff('handoff rejects wrong catalogSource', { ...validHandoff, catalogSource: 'pi' });
rejectHandoff('handoff rejects missing catalogSessionId', { ...validHandoff, catalogSessionId: undefined });
rejectHandoff('handoff rejects wrong catalogSessionId', { ...validHandoff, catalogSessionId: 'other' });
rejectHandoff('handoff rejects invalid catalogStatus', { ...validHandoff, catalogStatus: 'unknown' });
rejectHandoff('handoff rejects wrong catalogStatus', { ...validHandoff, catalogStatus: 'foreign' });
rejectHandoff('handoff rejects leftover state', { ...validHandoff, state: { sessionId: B1.sessionId } });
rejectHandoff('handoff rejects leftover messages', { ...validHandoff, messages: [] });
rejectHandoff('handoff rejects fractional cursor', { ...validHandoff, cursor: 1.5 });
rejectHandoff('handoff rejects unsafe cursor', { ...validHandoff, cursor: Number.MAX_SAFE_INTEGER + 1 });

const parsedHandoff = parseSwitchHandoff(validHandoff, {
  catalogSource: 'rpi',
  catalogSessionId: B1.sessionId,
  catalogStatus: 'native',
});
check('valid native handoff parses identity, catalog tuple, and cursor', parsedHandoff.sessionId === B1.sessionId
  && parsedHandoff.runtimeIncarnation === B1.runtimeIncarnation
  && parsedHandoff.catalogSource === 'rpi'
  && parsedHandoff.catalogSessionId === B1.sessionId
  && parsedHandoff.catalogStatus === 'native'
  && parsedHandoff.cursor === 12
  && parsedHandoff.subscriptionRequired === true
  && parsedHandoff.lifecycleResponseVersion === 1
  && parsedHandoff.session.sessionId === B1.sessionId);

// Native/foreign matrix: a native catalog row maps 1:1 to the runtime id; a
// foreign catalog id may bind a DISTINCT native runtime id, so only the
// catalog tuple is exact-checked for foreign targets.
const foreignRuntime: RuntimeIdentity = { sessionId: 'native-runtime-9', runtimeIncarnation: 'runtime-inc-9' };
const foreignHandoff = parseSwitchHandoff({
  ...handoffWire(foreignRuntime, 5),
  catalogSource: 'codex',
  catalogSessionId: 'codex-catalog-1',
  catalogStatus: 'foreign',
  session: nativeSessionRow(foreignRuntime),
}, { catalogSource: 'codex', catalogSessionId: 'codex-catalog-1', catalogStatus: 'foreign' });
check('foreign handoff accepts the distinct native runtime and authoritative row', foreignHandoff.sessionId === foreignRuntime.sessionId
  && foreignHandoff.catalogSessionId === 'codex-catalog-1'
  && foreignHandoff.catalogSource === 'codex'
  && foreignHandoff.catalogStatus === 'foreign'
  && foreignHandoff.session.sessionId === foreignRuntime.sessionId
  && foreignHandoff.session.runtimeIncarnation === foreignRuntime.runtimeIncarnation);
rejectHandoff('foreign handoff rejects a wrong catalog tuple', {
  ...handoffWire(foreignRuntime, 5),
  catalogSource: 'codex',
  catalogSessionId: 'other-catalog',
  catalogStatus: 'foreign',
  session: nativeSessionRow(foreignRuntime),
}, { catalogSource: 'codex', catalogSessionId: 'codex-catalog-1', catalogStatus: 'foreign' });
rejectHandoff('foreign handoff rejects a missing runtime session id', {
  ...handoffWire(foreignRuntime, 5),
  catalogSource: 'codex',
  catalogSessionId: 'codex-catalog-1',
  catalogStatus: 'foreign',
  session: nativeSessionRow(foreignRuntime),
  sessionId: undefined,
}, { catalogSource: 'codex', catalogSessionId: 'codex-catalog-1', catalogStatus: 'foreign' });
rejectHandoff('foreign handoff rejects a missing authoritative native row', {
  ...handoffWire(foreignRuntime, 5),
  catalogSource: 'codex',
  catalogSessionId: 'codex-catalog-1',
  session: undefined,
  catalogStatus: 'foreign',
}, { catalogSource: 'codex', catalogSessionId: 'codex-catalog-1', catalogStatus: 'foreign' });
rejectHandoff('foreign handoff rejects a native row with the wrong runtime id', {
  ...handoffWire(foreignRuntime, 5),
  catalogSource: 'codex',
  catalogSessionId: 'codex-catalog-1',
  catalogStatus: 'foreign',
  session: nativeSessionRow({ ...foreignRuntime, sessionId: 'wrong-runtime' }),
}, { catalogSource: 'codex', catalogSessionId: 'codex-catalog-1', catalogStatus: 'foreign' });

const switchCommand = buildSwitchCommand(target(B1, 'b.jsonl'), A.sessionId);
check('negotiated switch command sends the full catalog tuple', switchCommand.type === 'switch_session'
  && switchCommand.sessionPath === 'b.jsonl'
  && switchCommand.responseMode === SWITCH_HANDOFF_RESPONSE_MODE
  && switchCommand.sessionId === A.sessionId
  && switchCommand.catalogSource === 'rpi'
  && switchCommand.catalogSessionId === B1.sessionId
  && switchCommand.catalogStatus === 'native');
const foreignCommand = buildSwitchCommand(foreignTarget('codex-catalog-1'), A.sessionId);
check('foreign switch command sends its own normalized tuple', foreignCommand.catalogSource === 'codex'
  && foreignCommand.catalogSessionId === 'codex-catalog-1'
  && foreignCommand.catalogStatus === 'foreign');
check('switch command does not carry messages or incarnation', !('messages' in switchCommand)
  && !('state' in switchCommand) && !('runtimeIncarnation' in switchCommand));

// The normalizers are canonical-only at the boundary too: exactly the five
// provider keys and exactly the two statuses — everything else (aliases,
// casing, whitespace, empty, wrong types) rejects null and never forms a
// switch target.
const canonicalSources: CatalogSource[] = ['rpi', 'pi', 'omp', 'codex', 'grok'];
for (const source of canonicalSources) {
  check(`normalizeCatalogSource accepts the canonical ${source}`, normalizeCatalogSource(source) === source);
}
for (const rejectedSource of ['native', 'primary', 'grok/hyper', '', ' ', 'rpi ', 'RPI', 'claude', 'rpi\n']) {
  check(`normalizeCatalogSource rejects the non-canonical ${JSON.stringify(rejectedSource)}`,
    normalizeCatalogSource(rejectedSource) === null);
}
check('normalizeCatalogSource rejects non-string and missing values',
  normalizeCatalogSource(undefined) === null
    && normalizeCatalogSource(null) === null
    && normalizeCatalogSource(5) === null);
check('normalizeCatalogStatus accepts only the two exact statuses',
  normalizeCatalogStatus('native') === 'native' && normalizeCatalogStatus('foreign') === 'foreign');
for (const rejectedStatus of ['native ', 'NATIVE', 'loaded', true, undefined, null]) {
  check(`normalizeCatalogStatus rejects non-canonical ${JSON.stringify(rejectedStatus)}`,
    normalizeCatalogStatus(rejectedStatus) === null);
}

const subscribe = buildSubscribeFrame(B1, 12);
check('subscribe frame carries exact incarnation and cursor', subscribe.type === 'subscribe'
  && subscribe.sessionId === B1.sessionId
  && subscribe.runtimeIncarnation === B1.runtimeIncarnation
  && subscribe.cursor === 12);

{
  const parsed = parseSubscriptionError({
    type: 'subscription_error',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    error: 'subscription snapshot already in progress',
    code: SNAPSHOT_IN_PROGRESS_CODE,
    retryable: true,
  });
  check('parseSubscriptionError preserves snapshot_in_progress code',
    parsed.code === SNAPSHOT_IN_PROGRESS_CODE
      && parsed.sessionId === B1.sessionId
      && parsed.runtimeIncarnation === B1.runtimeIncarnation);
  check('shouldRetrySnapshotInProgress is typed-code only',
    shouldRetrySnapshotInProgress(SNAPSHOT_IN_PROGRESS_CODE, false) === true
      && shouldRetrySnapshotInProgress(SNAPSHOT_IN_PROGRESS_CODE, true) === false
      && shouldRetrySnapshotInProgress(undefined, false) === false
      && shouldRetrySnapshotInProgress('runtime_unavailable', false) === false);
}

const colonId: RuntimeIdentity = { sessionId: 'a:b', runtimeIncarnation: 'c:d' };
check('runtimeKey is length-prefixed, not delimiter concatenation', runtimeKey(colonId) === '3:a:b:3:c:d');
check('runtimeKey round-trips identities that contain colons', sameRuntime(parseRuntimeKey(runtimeKey(colonId)), colonId));
check('sameRuntime requires both sessionId and incarnation', sameRuntime(B1, B2) === false && sameRuntime(B1, B1) === true);

{
  const started = beginSessionSwitch(idle(A), target(B1));
  check('beginSessionSwitch increments generation', started.generation === 1 && started.state.nextGeneration === 2);
  check('beginSessionSwitch preserves displayed source', sameRuntime(started.state.displayed, A));
  check('beginSessionSwitch preserves commandTarget source', sameRuntime(started.state.commandTarget, A));
  check('beginSessionSwitch pending is opening with no target identity', started.state.pending?.phase === 'opening'
    && started.state.pending.targetIdentity === null
    && started.state.pending.responseCursor === null);
  check('beginSessionSwitch source is commandTarget, never displayed-only', sameRuntime(started.state.pending?.source, A));
  const coalesced = beginSessionSwitch(started.state, target(B1));
  check('same-target begin coalesces and does not increment generation', coalesced.coalesced === true
    && coalesced.generation === started.generation
    && coalesced.state.pending?.generation === started.generation
    && coalesced.state.nextGeneration === started.state.nextGeneration);
  const warm = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 10),
    { identity: B1, cursor: 10 },
  );
  const diverted = beginSessionSwitch(warm.state, target(C));
  check('pending warmDisplayed begin rolls back to source, not warm target', diverted.coalesced === false
    && diverted.replacedGeneration === started.generation
    && sameRuntime(diverted.state.displayed, A)
    && sameRuntime(diverted.state.commandTarget, A)
    && sameRuntime(diverted.state.pending?.source, A)
    && diverted.state.pending?.targetRequest.catalogSessionId === C.sessionId);
  // Same catalog tuple coalesces even when the incarnation hint changed
  // (LRU reopen / overlay refresh mid-flight); a different catalog tuple or
  // status never coalesces and supersedes the in-flight transaction.
  const staleHint: SessionSwitchTarget = { ...target(B1), catalogRuntimeIncarnation: 'inc-stale' };
  const coalescedAcrossHint = beginSessionSwitch(started.state, staleHint);
  check('same catalog tuple coalesces despite a different incarnation hint', coalescedAcrossHint.coalesced === true
    && coalescedAcrossHint.generation === started.generation);
  const superseded = beginSessionSwitch(started.state, foreignTarget(B1.sessionId));
  check('different catalog source/status supersedes the transaction', superseded.coalesced === false
    && superseded.replacedGeneration === started.generation
    && superseded.state.pending?.targetRequest.catalogStatus === 'foreign');
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const plan = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 42), undefined);
  check('cold handoff action order is install-cursor then subscribe', actionTypes(plan.actions) === 'install-cursor,subscribe');
  const install = plan.actions[0];
  const sub = plan.actions[1];
  check('install-cursor precedes subscribe', install?.type === 'install-cursor' && sub?.type === 'subscribe'
    && plan.actions.findIndex((action) => action.type === 'install-cursor')
      < plan.actions.findIndex((action) => action.type === 'subscribe'));
  check('cold handoff subscribes at cursor 0, never the handoff cursor', install?.type === 'install-cursor' && install.cursor === 0
    && sub?.type === 'subscribe' && sub.cursor === 0
    && sameRuntime(sub.identity, B1));
  check('cold handoff records the actual subscription cursor for the cut premise', plan.state.pending?.responseCursor === 0);
  check('cold handoff keeps source displayed', sameRuntime(plan.state.displayed, A) && plan.warm === false);
  check('cold handoff keeps commandTarget on source', sameRuntime(plan.state.commandTarget, A));
  check('cold handoff remains pending awaiting cut', plan.state.pending?.phase === 'awaiting-authoritative-cut'
    && plan.state.pending.warmDisplayed === false
    && sameRuntime(plan.state.pending.targetIdentity, B1));
}

{
  const started = beginSessionSwitch(idle(A), target(B2));
  const plan = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B2, 7),
    { identity: B1, cursor: 99 },
  );
  check('same sessionId different incarnation invalidates old then installs cursor then subscribe',
    actionTypes(plan.actions) === 'invalidate-runtime,install-cursor,subscribe');
  check('invalidate targets the retired exact incarnation, not session-id-only',
    plan.actions[0]?.type === 'invalidate-runtime' && sameRuntime(plan.actions[0].identity, B1));
  check('mismatched ready cache is cold', plan.warm === false && sameRuntime(plan.state.displayed, A));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const plan = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 10),
    { identity: B1, cursor: 10 },
  );
  check('warm handoff subscribes at the handoff cursor, not zero', plan.actions[0]?.type === 'install-cursor'
    && plan.actions[0].cursor === 10
    && plan.actions[1]?.type === 'subscribe' && plan.actions[1].cursor === 10
    && plan.state.pending?.responseCursor === 10);
  check('warm display paints target but stays pending', sameRuntime(plan.state.displayed, B1)
    && plan.state.pending?.warmDisplayed === true
    && plan.state.pending.phase === 'awaiting-authoritative-cut');
  check('warm display keeps commandTarget on source', sameRuntime(plan.state.commandTarget, A));
}

{
  // A→B→A defect fix: the READY cache can lag the handoff cursor while the
  // LIVE subscription for the target runtime is already caught up. Warm iff
  // the ready cursor OR the live cursor is at/past the handoff cursor.
  const started = beginSessionSwitch(idle(A), target(B1));
  const plan = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 20),
    { identity: B1, cursor: 19 },
  );
  check('stale ready cursor AND stale live cursor is cold', plan.warm === false
    && sameRuntime(plan.state.displayed, A)
    && !plan.actions.some((action) => action.type === 'display-warm'));
  check('stale-ready cold handoff still subscribes at cursor 0', plan.actions[0]?.type === 'install-cursor'
    && plan.actions[0].cursor === 0
    && plan.actions[1]?.type === 'subscribe' && plan.actions[1].cursor === 0);
}

{
  // The live cursor is caught up past the handoff cursor even though the
  // ready cache is stale → WARM, and the subscribe follows grok's formula:
  // max(ready.cursor, handoff.cursor) = the live-authoritative cursor.
  const started = beginSessionSwitch(idle(A), target(B1));
  const plan = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 20),
    { identity: B1, cursor: 19 },
    25,
  );
  check('stale ready cursor is warm when the live cursor is caught up', plan.warm === true
    && sameRuntime(plan.state.displayed, B1)
    && plan.state.pending?.warmDisplayed === true);
  check('warm-with-stale-ready subscribes at max(ready, handoff) = live', plan.actions[0]?.type === 'install-cursor'
    && plan.actions[0].cursor === 25
    && plan.actions[1]?.type === 'subscribe' && plan.actions[1].cursor === 25
    && plan.state.pending?.responseCursor === 25);
}

{
  // The live cursor BEHIND the handoff cursor cannot rescue a stale ready
  // cache: the cut would replay a range the client never received → cold.
  const started = beginSessionSwitch(idle(A), target(B1));
  const plan = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 20),
    { identity: B1, cursor: 19 },
    5,
  );
  check('stale ready cursor is cold when the live cursor is behind too', plan.warm === false
    && !plan.actions.some((action) => action.type === 'display-warm'));
}

rejectCut('cut parser rejects empty incarnation', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: '',
  watermark: 1,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [],
});
rejectCut('cut parser rejects warm-continuity without messages', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 1,
  reason: 'warm-continuity',
  state: {},
  events: [],
});
rejectCut('cut parser rejects cold-full without messages', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 1,
  reason: 'cold-full',
  state: {},
  events: [],
});
rejectCut('cut parser rejects event incarnation mismatch', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 2,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [{ type: 'turn_started', sequence: 2, sessionId: B1.sessionId, runtimeIncarnation: B2.runtimeIncarnation }],
});
rejectCut('cut parser rejects event sessionId mismatch', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 2,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [{ type: 'turn_started', sequence: 2, sessionId: 'other', runtimeIncarnation: B1.runtimeIncarnation }],
});
rejectCut('cut parser rejects missing event sequence', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 1,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [{ type: 'turn_started', sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation }],
});
rejectCut('cut parser rejects fractional event sequence', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 1,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [{ type: 'turn_started', sequence: 0.5, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation }],
});
rejectCut('cut parser rejects null event sequence', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 1,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [{ type: 'turn_started', sequence: null, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation }],
});
rejectCut('cut parser rejects null event type', {
  type: 'subscription_snapshot',
  sessionId: B1.sessionId,
  runtimeIncarnation: B1.runtimeIncarnation,
  watermark: 1,
  reason: 'cold-full',
  state: {},
  messages: [],
  events: [{ type: null, sequence: 1, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation }],
});

{
  const unload = parseSessionUnload({
    type: 'session_unloaded',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    reason: 'lru',
  });
  check('unload parser returns exact identity and reason', unload.sessionId === B1.sessionId
    && unload.runtimeIncarnation === B1.runtimeIncarnation
    && unload.reason === 'lru');
  let rejectedNull = false;
  let rejectedReason = false;
  try {
    parseSessionUnload({ type: 'session_unloaded', sessionId: B1.sessionId, runtimeIncarnation: null, reason: 'lru' });
  } catch {
    rejectedNull = true;
  }
  try {
    parseSessionUnload({ type: 'session_unloaded', sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation, reason: 'other' });
  } catch {
    rejectedReason = true;
  }
  check('unload parser rejects null incarnation', rejectedNull);
  check('unload parser rejects unknown reason', rejectedReason);
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 0), undefined);
  const cut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 4,
    reason: 'cold-full',
    state: { sessionId: B1.sessionId },
    messages: [{ role: 'user' }, { role: 'assistant' }],
    events: [],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, cut);
  check('matching cold-full applies messages once and binds authority', plan.settlesPromise === true
    && actionTypes(plan.actions) === 'apply-state,apply-messages,mark-ready,bind-authority,save-preference');
  check('cold-full apply-messages uses the cut array', plan.actions[1]?.type === 'apply-messages'
    && plan.actions[1].messages.length === 2);
  check('cold-full mark-ready carries the exact identity and watermark', plan.actions[2]?.type === 'mark-ready'
    && sameRuntime(plan.actions[2].identity, B1)
    && plan.actions[2].cursor === 4);
  check('mark-ready precedes bind-authority on a matching cut', plan.actions.findIndex((action) => action.type === 'mark-ready')
    < plan.actions.findIndex((action) => action.type === 'bind-authority'));
  check('matching cut sets displayed and commandTarget atomically', sameRuntime(plan.state.displayed, B1)
    && sameRuntime(plan.state.commandTarget, B1)
    && plan.state.pending === null);
}

{
  // Cold loaded target: handoff cursor > 0 with no ready cache subscribes at
  // 0, so the authoritative cut is a full cold-full snapshot whose events are
  // contiguous from the actual subscription cursor through the watermark.
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 7), undefined);
  check('cold loaded target handoff cursor>0 subscribes at 0', afterHandoff.actions[0]?.type === 'install-cursor'
    && afterHandoff.actions[0].cursor === 0
    && afterHandoff.actions[1]?.type === 'subscribe' && afterHandoff.actions[1].cursor === 0
    && afterHandoff.state.pending?.responseCursor === 0);
  const full = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 9,
    reason: 'cold-full',
    state: { sessionId: B1.sessionId },
    messages: [{ role: 'user' }],
    events: [
      { type: 'turn_started', sequence: 1, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_terminal', sequence: 2, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_started', sequence: 3, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_terminal', sequence: 4, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_started', sequence: 5, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_terminal', sequence: 6, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_started', sequence: 7, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_terminal', sequence: 8, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_started', sequence: 9, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
    ],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, full);
  check('cold loaded target full cut applies messages and marks ready', plan.settlesPromise === true
    && actionTypes(plan.actions) === 'apply-state,apply-messages,mark-ready,bind-authority,save-preference');
  check('cold loaded target cut premise is contiguous from cursor 0', plan.actions[1]?.type === 'apply-messages'
    && plan.actions[1].messages.length === 1
    && plan.actions[2]?.type === 'mark-ready' && plan.actions[2].cursor === 9);
}

{
  // Nonpending background cut: a cut for a runtime with no matching pending
  // switch applies data and marks ready without touching displayed/command
  // target/pending, so a later warm handoff finds the ready cache.
  const idleState = idle(A);
  const background = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 6,
    reason: 'cold-full',
    state: { sessionId: B1.sessionId },
    messages: [{ role: 'user' }],
    events: [],
  });
  const plan = acceptAuthoritativeCut(idleState, 0, background);
  check('nonpending background cold cut applies data and marks ready', plan.settlesPromise === false
    && actionTypes(plan.actions) === 'apply-state,apply-messages,mark-ready'
    && plan.actions[2]?.type === 'mark-ready' && sameRuntime(plan.actions[2].identity, B1) && plan.actions[2].cursor === 6);
  check('background cut never changes displayed/commandTarget/pending', sameRuntime(plan.state.displayed, A)
    && sameRuntime(plan.state.commandTarget, A)
    && plan.state.pending === null);
  check('background cut never binds authority or saves preference', !plan.actions.some((action) =>
    action.type === 'bind-authority' || action.type === 'save-preference'));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 10),
    { identity: B1, cursor: 10 },
  );
  const cut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 12,
    reason: 'warm-continuity',
    state: { sessionId: B1.sessionId },
    messages: [],
    events: [
      { type: 'turn_started', sequence: 11, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_terminal', sequence: 12, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
    ],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, cut);
  check('matching warm-continuity applies attested messages then replay', plan.settlesPromise === true
    && plan.actions.some((action) => action.type === 'apply-messages')
    && actionTypes(plan.actions) === 'apply-state,apply-messages,replay-events,mark-ready,bind-authority,save-preference');
  check('warm-continuity replay is contiguous through watermark', plan.actions[2]?.type === 'replay-events'
    && plan.actions[2].events.map((event) => event.sequence).join(',') === '11,12');
  check('warm-continuity mark-ready advances the ready cache to the watermark', plan.actions[3]?.type === 'mark-ready'
    && sameRuntime(plan.actions[3].identity, B1)
    && plan.actions[3].cursor === 12);
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 4), undefined);
  const cut = parseSubscriptionCut({
    type: 'session_resync',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 8,
    reason: 'resync',
    state: {},
    messages: [{ role: 'user' }],
    events: [],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, cut);
  check('matching resync replaces messages once and binds', plan.settlesPromise === true
    && plan.actions.some((action) => action.type === 'apply-messages')
    && sameRuntime(plan.state.displayed, B1)
    && sameRuntime(plan.state.commandTarget, B1));
  check('matching resync marks ready at the watermark before binding', plan.actions.some((action) =>
    action.type === 'mark-ready' && sameRuntime(action.identity, B1) && action.cursor === 8)
    && plan.actions.findIndex((action) => action.type === 'mark-ready')
      < plan.actions.findIndex((action) => action.type === 'bind-authority'));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 0), undefined);
  const history = Array.from({ length: 295 }, (_, index) => ({ role: index % 2 === 0 ? 'user' : 'assistant' }));
  const cut = parseSubscriptionCut({
    type: 'session_resync',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 16918,
    reason: 'resync',
    state: {},
    messages: history,
    events: [
      { type: 'message_update', sequence: 16606, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'agent_settled', sequence: 16918, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
    ],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, cut);
  const applied = plan.actions.find((action) => action.type === 'apply-messages');
  check('gapped resync tail events still apply attested messages and bind', plan.settlesPromise === true
    && sameRuntime(plan.state.displayed, B1)
    && sameRuntime(plan.state.commandTarget, B1)
    && plan.state.pending === null
    && applied?.type === 'apply-messages'
    && applied.messages.length === 295
    && !plan.actions.some((action) => action.type === 'resubscribe-zero' || action.type === 'fail-pending'));
  const background = acceptAuthoritativeCut(idle(A), 0, cut);
  check('background gapped resync caches messages without stealing display or resubscribing',
    background.settlesPromise === false
      && sameRuntime(background.state.displayed, A)
      && background.actions.some((action) => action.type === 'apply-messages'
        && action.messages.length === 295)
      && background.actions.some((action) => action.type === 'mark-ready' && action.cursor === 16918)
      && !background.actions.some((action) => action.type === 'resubscribe-zero' || action.type === 'bind-authority'));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 0), undefined);
  const stale = acceptSwitchHandoff(afterHandoff.state, started.generation - 1, handoff(B2, 99), undefined);
  check('stale generation handoff is cache-only and cannot activate', stale.stale === true
    && stale.actions.length === 0
    && sameRuntime(stale.state.displayed, A)
    && stale.state.pending?.generation === started.generation);
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const plan = acceptSwitchHandoff(
    started.state,
    started.generation,
    handoff(B1, 10),
    { identity: B1, cursor: 10 },
  );
  check('ready cache exact for the handoff identity is accepted warm', plan.stale === false
    && plan.warm === true
    && sameRuntime(plan.state.pending?.targetIdentity, B1)
    && plan.state.pending?.phase === 'awaiting-authoritative-cut');
}

{
  // LRU reopen: the catalog hint still names the evicted incarnation while
  // the backend reopened the runtime under a fresh one. The handoff wins and
  // the mismatch NEVER fails the switch; the stale ready cache only means
  // cold (subscribe from 0) plus invalidation of the retired runtime.
  const staleHintTarget: SessionSwitchTarget = { ...target(B1), catalogRuntimeIncarnation: B1.runtimeIncarnation };
  const started = beginSessionSwitch(idle(A), staleHintTarget);
  const plan = acceptSwitchHandoff(started.state, started.generation, handoff(B2, 7), { identity: B1, cursor: 12 });
  check('stale catalog incarnation after LRU reopen succeeds cold', plan.stale === false
    && plan.warm === false
    && sameRuntime(plan.state.pending?.targetIdentity, B2)
    && plan.state.pending?.phase === 'awaiting-authoritative-cut'
    && sameRuntime(plan.state.displayed, A));
  check('LRU reopen invalidates the retired incarnation and subscribes at 0',
    actionTypes(plan.actions) === 'invalidate-runtime,install-cursor,subscribe'
    && plan.actions[0]?.type === 'invalidate-runtime' && sameRuntime(plan.actions[0].identity, B1)
    && plan.actions[1]?.type === 'install-cursor' && plan.actions[1].cursor === 0);
}

{
  // LRU reopen with an already-fresh ready cache: warmth keys on the ready
  // cache matching the handoff incarnation exactly, never on the catalog hint.
  const staleHintTarget: SessionSwitchTarget = { ...target(B1), catalogRuntimeIncarnation: B1.runtimeIncarnation };
  const started = beginSessionSwitch(idle(A), staleHintTarget);
  const plan = acceptSwitchHandoff(started.state, started.generation, handoff(B2, 7), { identity: B2, cursor: 9 });
  check('LRU reopen warms only when the ready cache matches the handoff incarnation',
    plan.warm === true
    && plan.actions[0]?.type === 'install-cursor' && plan.actions[0].cursor === 7
    && plan.actions[1]?.type === 'subscribe' && plan.actions[1].cursor === 7);
}

{
  const coldTarget: SessionSwitchTarget = {
    path: 'b.jsonl',
    catalogSource: 'rpi',
    catalogSessionId: B1.sessionId,
    catalogStatus: 'native',
    catalogRuntimeIncarnation: null,
  };
  const started = beginSessionSwitch(idle(A), coldTarget);
  const plan = acceptSwitchHandoff(started.state, started.generation, handoff(B2, 5), undefined);
  check('cold catalog row with null incarnation hint accepts the handoff runtime', plan.stale === false
    && plan.warm === false
    && actionTypes(plan.actions) === 'install-cursor,subscribe'
    && sameRuntime(plan.state.pending?.targetIdentity, B2)
    && plan.state.pending?.phase === 'awaiting-authoritative-cut');
}

{
  // Foreign catalog row: the backend binds a DISTINCT native runtime id and
  // a fresh incarnation the catalog never knew; the cut settles it warm or
  // cold purely through the runtime identity the handoff returned.
  const foreignRuntime: RuntimeIdentity = { sessionId: 'native-runtime-9', runtimeIncarnation: 'runtime-inc-9' };
  const started = beginSessionSwitch(idle(A), foreignTarget('codex-catalog-1'));
  const foreign = parseSwitchHandoff({
    ...handoffWire(foreignRuntime, 3),
    catalogSource: 'codex',
    catalogSessionId: 'codex-catalog-1',
    catalogStatus: 'foreign',
    session: nativeSessionRow(foreignRuntime),
  }, { catalogSource: 'codex', catalogSessionId: 'codex-catalog-1', catalogStatus: 'foreign' });
  const plan = acceptSwitchHandoff(started.state, started.generation, foreign, undefined);
  check('foreign switch installs the distinct native runtime cold', plan.stale === false
    && plan.warm === false
    && sameRuntime(plan.state.pending?.targetIdentity, foreignRuntime)
    && plan.actions[0]?.type === 'install-cursor' && plan.actions[0].cursor === 0);
  const cut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: foreignRuntime.sessionId,
    runtimeIncarnation: foreignRuntime.runtimeIncarnation,
    watermark: 3,
    reason: 'cold-full',
    state: { sessionId: foreignRuntime.sessionId },
    messages: [{ role: 'user' }],
    events: [],
  });
  const settled = acceptAuthoritativeCut(plan.state, started.generation, cut);
  check('foreign switch settles on its own runtime cut and preserves the catalog source',
    settled.settlesPromise === true
    && sameRuntime(settled.state.displayed, foreignRuntime)
    && sameRuntime(settled.state.commandTarget, foreignRuntime)
    && settled.actions.some((action) => action.type === 'save-preference' && action.sessionId === foreignRuntime.sessionId));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 0), undefined);
  const wrong = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B2.sessionId,
    runtimeIncarnation: B2.runtimeIncarnation,
    watermark: 3,
    reason: 'cold-full',
    state: { sessionId: B2.sessionId },
    messages: [{ role: 'user' }],
    events: [],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, wrong);
  check('wrong-incarnation cut updates cache only and cannot save preference', plan.settlesPromise === false
    && actionTypes(plan.actions) === 'apply-state,apply-messages,mark-ready'
    && !plan.actions.some((action) => action.type === 'save-preference' || action.type === 'bind-authority')
    && sameRuntime(plan.state.displayed, A)
    && sameRuntime(plan.state.commandTarget, A)
    && plan.state.pending !== null);
  check('background cut marks the exact runtime ready at its watermark', plan.actions[2]?.type === 'mark-ready'
    && sameRuntime(plan.actions[2].identity, B2)
    && plan.actions[2].cursor === 3);
}

{
  const first = beginSessionSwitch(idle(A), target(B1));
  const firstHandoff = acceptSwitchHandoff(first.state, first.generation, handoff(B1, 0), undefined);
  const second = beginSessionSwitch(firstHandoff.state, target(C));
  const oldCut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 0,
    reason: 'cold-full',
    state: {},
    messages: [],
    events: [],
  });
  const plan = acceptAuthoritativeCut(second.state, first.generation, oldCut);
  check('superseded same-session cut cannot mutate replacement pending cache', plan.actions.length === 0
    && plan.settlesPromise === false
    && plan.state.pending?.generation === second.generation
    && plan.state.pending.targetIdentity === null);
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 5), undefined);
  const gap = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 8,
    reason: 'warm-continuity',
    state: {},
    messages: [],
    events: [
      { type: 'turn_started', sequence: 8, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
    ],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, gap);
  check('non-contiguous replay fails pending and keeps source', plan.settlesPromise === false
    && sameRuntime(plan.state.displayed, A)
    && sameRuntime(plan.state.commandTarget, A)
    && plan.state.pending === null
    && plan.actions.some((action) => action.type === 'fail-pending')
    && plan.actions.some((action) => action.type === 'resubscribe-zero'));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 10), undefined);
  const short = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 12,
    reason: 'warm-continuity',
    state: {},
    messages: [],
    events: [
      { type: 'turn_started', sequence: 11, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
    ],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, short);
  check('warm-continuity replay ending before watermark cannot bind', plan.settlesPromise === false
    && sameRuntime(plan.state.displayed, A)
    && plan.state.pending === null
    && !plan.actions.some((action) => action.type === 'bind-authority' || action.type === 'save-preference'));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 9), undefined);
  const regression = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 4,
    reason: 'warm-continuity',
    state: {},
    messages: [],
    events: [],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, regression);
  check('cursor regression fails pending and keeps source', plan.settlesPromise === false
    && sameRuntime(plan.state.displayed, A)
    && plan.state.pending === null);
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const failed = failSessionSwitch(started.state, started.generation);
  check('switch failure restores source displayed and command target', sameRuntime(failed.displayed, A)
    && sameRuntime(failed.commandTarget, A)
    && failed.pending === null);
  check('stale failSessionSwitch is a no-op', failSessionSwitch(started.state, 99).pending?.generation === started.generation);
  check('switch-pending error code is typed', SWITCH_PENDING_ERROR === 'switch-pending');
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const openingUnload = unloadRuntime(started.state, B1);
  check('opening unload with null targetIdentity does not fail pending by sessionId', openingUnload.failedPending === false
    && started.state.pending?.targetIdentity === null
    && openingUnload.state?.pending?.generation === started.generation);
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 0), undefined);
  const unloaded = unloadRuntime(afterHandoff.state, B1);
  check('unload of pending target fails the transaction and restores source', unloaded.failedPending === true
    && unloaded.state !== null
    && sameRuntime(unloaded.state.displayed, A)
    && sameRuntime(unloaded.state.commandTarget, A)
    && unloaded.state.pending === null
    && unloaded.droppedKey === runtimeKey(B1));
}

{
  const displayed = idle(B2);
  const unloaded = unloadRuntime(displayed, B1);
  check('exact unload of inc1 cannot drop current inc2 display', unloaded.failedPending === false
    && unloaded.state !== null
    && sameRuntime(unloaded.state.displayed, B2)
    && sameRuntime(unloaded.state.commandTarget, B2)
    && unloaded.droppedKey === runtimeKey(B1)
    && unloaded.droppedKey !== runtimeKey(B2));
}

{
  const unloaded = unloadRuntime(idle(B2), B2, A);
  check('displayed idle secondary unload restores live source', unloaded.failedPending === false
    && unloaded.state !== null
    && sameRuntime(unloaded.state.displayed, A)
    && sameRuntime(unloaded.state.commandTarget, A)
    && unloaded.state.pending === null
    && unloaded.droppedKey === runtimeKey(B2));
  check('displayed idle secondary unload does not leave commandTarget on the dead identity',
    unloaded.state !== null && !sameRuntime(unloaded.state.commandTarget, B2));
}



{
  const closed = unloadRuntime(idle(B2), B2, null);
  check('displayed unload without live fallback fails closed', closed.state === null
    && closed.failedPending === false
    && closed.actions.length === 0
    && closed.droppedKey === runtimeKey(B2));
}

{
  const slots = new Map<string, SessionViewSlot>();
  const displayed = A;
  const pendingState = beginSessionSwitch(idle(A), target(B1)).state;
  const pendingHandoff = acceptSwitchHandoff(pendingState, 1, handoff(B1, 0), undefined).state.pending;
  let current = slots;
  for (let i = 0; i < 9; i += 1) {
    const identity: RuntimeIdentity = { sessionId: `sess-${i}`, runtimeIncarnation: `inc-${i}` };
    current = upsertViewSlot(current, emptyViewSlot(identity, i), displayed, pendingHandoff);
  }
  current = upsertViewSlot(current, emptyViewSlot(A, 100), displayed, pendingHandoff);
  current = upsertViewSlot(current, emptyViewSlot(B1, 101), displayed, pendingHandoff);
  check('slot map stays bounded by the backend pool cap', current.size <= MAX_LOADED_SESSIONS);
  check('displayed slot is never evicted', current.has(runtimeKey(A)));
  check('pending target slot is never evicted', current.has(runtimeKey(B1)));
  const victim = evictViewSlot(current, displayed, pendingHandoff);
  check('evictViewSlot never names displayed or pending', victim !== runtimeKey(A) && victim !== runtimeKey(B1));
}

// Frontend-only exact-ready fast path: the pure plan must fast-path ONLY on a
// fully agreeing rpi/native tuple + hint + resident + view snapshot, and fail
// closed to the negotiated path on every partial match.
{
  const readyB: ReadyRuntime = { identity: B1, cursor: 7, socketGeneration: 3 };
  const fastContext = {
    ready: readyB,
    residentIncarnation: B1.runtimeIncarnation,
    viewRestorable: true,
    socketGeneration: 3,
  };
  const fast = planFastSessionSwitch(idle(A), target(B1), fastContext);
  check('exact-ready plan is fast with displayed+commandTarget on the ready identity',
    fast.kind === 'fast'
    && fast.kind === 'fast' && sameRuntime(fast.state.displayed, B1)
    && sameRuntime(fast.state.commandTarget, B1));
  if (fast.kind === 'fast') {
    check('exact-ready plan allocates no generation and keeps the counter',
      fast.state.pending === null && fast.state.nextGeneration === 1);
    check('exact-ready plan saves the preference and supersedes nothing',
      fast.actions.length === 1
      && fast.actions[0].type === 'save-preference'
      && fast.actions[0].sessionId === B1.sessionId
      && fast.supersededGeneration === null);
  }

  const rejects: Array<[string, SessionSwitchTarget, Partial<FastSwitchContext>]> = [
    ['plan rejects a missing ready record', target(B1), { ready: undefined }],
    ['plan rejects a non-rpi catalog source', { ...target(B1), catalogSource: 'pi' }, {}],
    ['plan rejects a foreign catalog status', { ...target(B1), catalogStatus: 'foreign' }, {}],
    ['plan rejects a catalog session mismatch', { ...target(B1), catalogSessionId: 'other' }, {}],
    ['plan rejects a null (unloaded) hint', { ...target(B1), catalogRuntimeIncarnation: null }, {}],
    ['plan rejects a stale hint incarnation', { ...target(B1), catalogRuntimeIncarnation: B2.runtimeIncarnation }, {}],
    ['plan rejects a resident overlay mismatch', target(B1), { residentIncarnation: B2.runtimeIncarnation }],
    ['plan rejects an unrestorable view slot', target(B1), { viewRestorable: false }],
    ['plan rejects an empty ready incarnation', target(B1), { ready: { identity: { sessionId: B1.sessionId, runtimeIncarnation: '' }, cursor: 7, socketGeneration: 3 } }],
    ['plan rejects a stale socket-generation stamp', target(B1), { socketGeneration: 4 }],
  ];
  for (const [name, variant, overrides] of rejects) {
    const plan = planFastSessionSwitch(idle(A), variant, { ...fastContext, ...overrides });
    check(name, plan.kind === 'negotiated');
  }

  // A pending transaction (cold, opening) is superseded: settled false by the
  // caller, its generation reported, and the new state is pending-free.
  const pendingState = beginSessionSwitch(idle(A), target(C)).state;
  const superseding = planFastSessionSwitch(pendingState, target(B1), fastContext);
  check('fast plan supersedes a pending cold transaction',
    superseding.kind === 'fast'
    && superseding.kind === 'fast' && superseding.supersededGeneration === pendingState.pending?.generation
    && superseding.state.pending === null
    && superseding.state.nextGeneration === pendingState.nextGeneration);

  // A pending transaction targeting the SAME catalog row coalesces: the plan
  // refuses to fast-supersede (and never false-settles the caller's own
  // in-flight switch); the negotiated begin coalesces it instead.
  const sameCatalogPending = beginSessionSwitch(idle(A), target(B1)).state;
  check('fast plan refuses to supersede a same-catalog pending',
    planFastSessionSwitch(sameCatalogPending, target(B1), fastContext).kind === 'negotiated');
  check('same-catalog pending with a hint-only variant still coalesces',
    planFastSessionSwitch(sameCatalogPending, { ...target(B1), catalogRuntimeIncarnation: B2.runtimeIncarnation }, fastContext).kind === 'negotiated');
}

// Exact-ready cut: zero wire frames and constant shape regardless of cache
// sizes; the negotiated path stays the only subscribe producer.
{
  const readyB: ReadyRuntime = { identity: B1, cursor: 7, socketGeneration: 9 };
  const fastContext = {
    ready: readyB,
    residentIncarnation: B1.runtimeIncarnation,
    viewRestorable: true,
    socketGeneration: 9,
  };
  const fast = planFastSessionSwitch(idle(A), target(B1), fastContext);
  check('exact-ready cut emits zero wire frames (no subscribe/switch in the plan surface)',
    fast.kind === 'fast'
    && !fast.actions.some((action) => action.type === 'subscribe')
    && !fast.actions.some((action) => action.type === 'resubscribe-zero')
    && !fast.actions.some((action) => action.type === 'install-cursor'));
  check('exact-ready cut performs no message/state application (no O(N) merge on the click path)',
    fast.kind === 'fast'
    && !fast.actions.some((action) => action.type === 'apply-messages')
    && !fast.actions.some((action) => action.type === 'apply-state')
    && !fast.actions.some((action) => action.type === 'replay-events'));
  check('exact-ready cut plan shape is constant: one save-preference action, no generation',
    fast.kind === 'fast' && fast.actions.length === 1 && fast.state.pending === null);

  const coldBegin = beginSessionSwitch(idle(A), target(B1));
  check('cold begin allocates exactly one generation and emits no frames itself',
    coldBegin.generation === 1 && coldBegin.state.pending?.phase === 'opening');
}

// Subsecond gate validator: every milestone duration must be a finite,
// nonnegative number strictly under MAX_SWITCH_MS; null and non-finite fail
// closed so an unobserved or NaN milestone can never pass the gate.
{
  const ok = validateSwitchMilestones({ activeMs: 12, transcriptMs: 40, readyMs: 80, pendingClearMs: 80 });
  check('validateSwitchMilestones accepts finite subsecond durations', ok.length === 0, JSON.stringify(ok));
  const bad = validateSwitchMilestones({
    unobserved: null,
    notFinite: Number.NaN,
    negative: -1,
    atGate: MAX_SWITCH_MS,
    beyondGate: 1500,
    okOne: 0,
  });
  check('validateSwitchMilestones rejects null, non-finite, negative, and >= gate',
    JSON.stringify(bad) === JSON.stringify(['unobserved=null', 'notFinite=NaN', 'negative=-1', 'atGate=1000', 'beyondGate=1500']),
    JSON.stringify(bad));
  check('the zero-duration boundary passes', validateSwitchMilestones({ zero: 0 }).length === 0);
}

// ---- parseSessionHandoffEnvelope: the session_handoff response contract ----
{
  const envelopeWire = handoffWire({ sessionId: 'handoff-new', runtimeIncarnation: 'handoff-inc' }, 0);
  const envelope = parseSessionHandoffEnvelope(envelopeWire);
  check('handoff envelope parses sessionId', envelope.sessionId === 'handoff-new');
  check('handoff envelope parses runtimeIncarnation', envelope.runtimeIncarnation === 'handoff-inc');
  check('handoff envelope parses catalogSessionId == sessionId', envelope.catalogSessionId === 'handoff-new');
  check('handoff envelope parses cursor 0', envelope.cursor === 0);
  const noticeEnvelope = parseSessionHandoffEnvelope({ ...handoffWire({ sessionId: 'h2', runtimeIncarnation: 'i2' }, 0), notice: 'seeded' });
  check('handoff envelope carries the notice', noticeEnvelope.notice === 'seeded');
}
{
  // The backend mints a NATIVE rpi row; any other source/status is malformed.
  const rejects: Array<[string, Record<string, unknown>]> = [
    ['foreign source', { ...handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0), catalogSource: 'codex' }],
    ['foreign status', { ...handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0), catalogStatus: 'foreign' }],
    ['sessionId != catalogSessionId', { ...handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0), catalogSessionId: 'other' }],
    ['missing subscriptionRequired', (() => { const w = handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0); delete (w as Record<string, unknown>).subscriptionRequired; return w; })()],
    ['wrong lifecycleResponseVersion', { ...handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0), lifecycleResponseVersion: 2 }],
    ['non-integer cursor', { ...handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0), cursor: -1 }],
    ['full snapshot smuggled in', { ...handoffWire({ sessionId: 'x', runtimeIncarnation: 'i' }, 0), state: {}, messages: [] }],
  ];
  for (const [name, wire] of rejects) {
    let threw = false;
    try {
      parseSessionHandoffEnvelope(wire);
    } catch {
      threw = true;
    }
    check(`handoff envelope rejects ${name}`, threw);
  }
  check('non-object envelope rejects', (() => { let t = false; try { parseSessionHandoffEnvelope('nope'); } catch { t = true; } return t; })());
}

{
  const bind: SwitchAction[] = [{ type: 'bind-authority', identity: B1, cursor: 4 }];
  check('warm-continuity with empty messages and bind-authority needs confirm',
    warmContinuityNeedsHistoryConfirm('warm-continuity', [], bind) === true);
  check('warm-continuity with durable messages does not confirm',
    warmContinuityNeedsHistoryConfirm('warm-continuity', [{}], bind) === false);
  check('cold-full empty messages never confirm',
    warmContinuityNeedsHistoryConfirm('cold-full', [], bind) === false);
  check('warm-continuity without bind-authority does not confirm',
    warmContinuityNeedsHistoryConfirm('warm-continuity', [], []) === false);
  check('user and bootstrap switch failures toast',
    switchFailureShouldToast('user') === true && switchFailureShouldToast('bootstrap') === true);
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 4), {
    identity: B1,
    cursor: 4,
    socketGeneration: 1,
  });
  const cut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 4,
    reason: 'warm-continuity',
    state: { sessionId: B1.sessionId },
    messages: [],
    events: [],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, cut);
  const hold = planWarmHistoryConfirm(afterHandoff.state, plan, B1, 1);
  check('warm confirm hold keeps source pending until confirmation',
    hold !== null
      && afterHandoff.state.pending !== null
      && hold.source.pending !== null
      && hold.committed.pending === null
      && hold.preBindActions.every((action) => action.type !== 'mark-ready' && action.type !== 'bind-authority')
      && hold.commitActions.some((action) => action.type === 'mark-ready')
      && hold.commitActions.some((action) => action.type === 'bind-authority'));
  check('warm confirm is current only for the same generation identity and socket',
    hold !== null
      && warmHistoryConfirmIsCurrent(hold, afterHandoff.state, 1) === true
      && warmHistoryConfirmIsCurrent(hold, afterHandoff.state, 2) === false
      && warmHistoryConfirmIsCurrent(hold, hold.committed, 1) === false);
  const preserved = hold ? preserveLiveBindCursor(hold.commitActions, 9) : [];
  check('bind preserves an advanced live cursor over the cut watermark',
    bindAuthorityCursor(4, 9) === 9
    && bindAuthorityCursor(9, 4) === 9
    && preserved.some((action) => action.type === 'bind-authority' && action.cursor === 9)
    && preserved.some((action) => action.type === 'mark-ready' && action.cursor === 9));
}

{
  const started = beginSessionSwitch(idle(A), target(B1));
  const afterHandoff = acceptSwitchHandoff(started.state, started.generation, handoff(B1, 4), {
    identity: B1,
    cursor: 4,
    socketGeneration: 1,
  });
  const cut = parseSubscriptionCut({
    type: 'subscription_snapshot',
    sessionId: B1.sessionId,
    runtimeIncarnation: B1.runtimeIncarnation,
    watermark: 6,
    reason: 'warm-continuity',
    state: { sessionId: B1.sessionId },
    messages: [],
    events: [
      { type: 'turn_started', sequence: 5, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
      { type: 'turn_started', sequence: 6, sessionId: B1.sessionId, runtimeIncarnation: B1.runtimeIncarnation },
    ],
  });
  const plan = acceptAuthoritativeCut(afterHandoff.state, started.generation, cut);
  const hold = planWarmHistoryConfirm(afterHandoff.state, plan, B1, 1);
  check('replay tail advances the live cursor through the last accepted sequence',
    acceptedCutLiveCursor([], 4) === 4
    && acceptedCutLiveCursor(
      [{ sequence: 5 }, { sequence: 6 }],
      6,
    ) === 6);
  check('warm confirm pre-bind replays the nonempty tail and withholds mark-ready',
    hold !== null
    && hold.preBindActions.some((action) => action.type === 'replay-events'
      && action.events.map((event) => event.sequence).join(',') === '5,6')
    && hold.preBindActions.every((action) => action.type !== 'mark-ready' && action.type !== 'bind-authority')
    && hold.commitActions.some((action) => action.type === 'mark-ready' && action.cursor === 6));
}

// ---- App wiring pins: requestSessionHandoff consumes the envelope, cuts
// ---- authority, and passes the action to SessionPanel ----
{
  const { readFileSync } = await import('node:fs');
  const { dirname, join } = await import('node:path');
  const { fileURLToPath } = await import('node:url');
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  const handoffAt = appSource.indexOf('const requestSessionHandoff = useCallback');
  const seamEnd = appSource.indexOf('/* ---------------- connection ---------------- */', handoffAt);
  const handoffBlock = handoffAt >= 0 && seamEnd > handoffAt ? appSource.slice(handoffAt, seamEnd) : '';
  check('App sends session_handoff with the subscribe-handoff-v1 response mode',
    handoffBlock.includes("type: 'session_handoff'") && handoffBlock.includes('SWITCH_HANDOFF_RESPONSE_MODE'));
  check('App validates the envelope through the strict parser',
    handoffBlock.includes('parseSessionHandoffEnvelope(result)'));
  check('App binds display+commandTarget authority to the new session',
    handoffBlock.includes('displayed: identity') && handoffBlock.includes('commandTarget: identity'));
  check('App installs resident + cursor and subscribes the new runtime',
    handoffBlock.includes('subscribeRuntime(identity, 0)'));
  check('App surfaces streaming refusals as one error toast',
    handoffBlock.includes('toast(`session handoff failed: ${message}`, true)'));
  check('App passes the handoff action to SessionPanel',
    appSource.includes('onSessionHandoff={onSessionHandoff}'));
  /* P1-2: the global switchPending gate set at handoff start clears on BOTH
   * settle paths — unconditionally after the authoritative cut, and in the
   * refusal path (finally) — never only via the coincident-switch branch. */
  const gateCuts = handoffBlock.split('setSwitchPending(false)').length - 1;
  check('the switchPending gate clears unconditionally after the authoritative cut',
    gateCuts >= 2 && handoffBlock.includes('setSwitchPending(false);') && handoffBlock.includes('.finally(() => {'));
  check('the refusal path clears the gate too (never gated until reload)',
    handoffBlock.includes('Refusal path: no authority cut happened')
      && handoffBlock.includes('setSwitchPending(false);'));
  check('the gate is armed exactly once per handoff',
    handoffBlock.includes('setSwitchPending(true);'));
}

{
  check('cut tick holds while refill is in flight even past 15s',
    planAuthoritativeCutTick({
      now: 60_000,
      lastProgressAt: 0,
      timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
      refillInFlight: true,
    }) === 'hold');
  check('cut tick holds inside the silence window',
    planAuthoritativeCutTick({
      now: 14_999,
      lastProgressAt: 0,
      timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
      refillInFlight: false,
    }) === 'hold');
  check('cut tick fires after 15s of silence with no refill',
    planAuthoritativeCutTick({
      now: 15_000,
      lastProgressAt: 0,
      timeoutMs: AUTHORITATIVE_CUT_TIMEOUT_MS,
      refillInFlight: false,
    }) === 'fire');
}

console.log(`\nsessionSwitch.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);
