#!/usr/bin/env node
import {
  COMPACT_BUSY_COMPACTING,
  COMPACT_BUSY_STREAMING,
  COMPACT_SEND_BLOCKED,
  applyCompactionEvent,
  beginCompact,
  compactAdmission,
  compactBadgeLabel,
  compactSendAdmission,
  compactStatusText,
  compactView,
  emptyCompactBook,
  reconcileCompactCut,
  removeCompactRuntime,
  settleCompact,
} from '../src/compactProgress.ts';
import type { RuntimeIdentity } from '../src/sessionSwitch.ts';
import type {
  CompactionEndedEvent,
  CompactionStartedEvent,
  ExactRuntimeEventFrame,
} from '../src/types.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const identA: RuntimeIdentity = { sessionId: 'sess-a', runtimeIncarnation: 'inc-1' };
const identA2: RuntimeIdentity = { sessionId: 'sess-a', runtimeIncarnation: 'inc-2' };
const identB: RuntimeIdentity = { sessionId: 'sess-b', runtimeIncarnation: 'inc-1' };

function startEvent(sequence: number, identity = identA): CompactionStartedEvent {
  return {
    type: 'compaction_start',
    reason: 'threshold',
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    sequence,
  };
}

function endEvent(
  sequence: number,
  extra: Partial<CompactionEndedEvent> = {},
  identity = identA,
): CompactionEndedEvent {
  return {
    type: 'compaction_end',
    reason: 'threshold',
    aborted: false,
    willRetry: false,
    sessionId: identity.sessionId,
    runtimeIncarnation: identity.runtimeIncarnation,
    sequence,
    ...extra,
  };
}

function otherEvent(sequence: number): ExactRuntimeEventFrame {
  return { type: 'turn_start', ...identA, sequence };
}

{
  check('llm status is Compacting context…', compactStatusText('llm') === 'Compacting context…');
  check('snap status is Snapcompacting context…', compactStatusText('snap') === 'Snapcompacting context…');
  check('missing mode defaults to Compacting context…', compactStatusText(null) === 'Compacting context…');
  check('llm badge is compacting', compactBadgeLabel('llm') === 'compacting');
  check('snap badge is snapcompacting', compactBadgeLabel('snap') === 'snapcompacting');
  check('unknown runtime is not compacting', compactView(emptyCompactBook(), null).compacting === false);
}

{
  let book = beginCompact(emptyCompactBook(), identA, 'llm', 1);
  check('optimistic compacting is true before RPC settles', compactView(book, identA).compacting === true);
  check('optimistic mode is llm', compactView(book, identA).mode === 'llm');
  book = beginCompact(emptyCompactBook(), identA, 'snap', 1);
  check('snap dispatch is visible as snap', compactView(book, identA).mode === 'snap');
  check(
    'snap status text is distinct',
    compactStatusText(compactView(book, identA).mode) === 'Snapcompacting context…',
  );
}

{
  let book = beginCompact(emptyCompactBook(), identA, 'llm', 1);
  book = settleCompact(book, identA, 1);
  check('success finally clears compacting', compactView(book, identA).compacting === false);
  check('success finally drops mode', compactView(book, identA).mode === null);
  check('settled compact admits send', compactSendAdmission(compactView(book, identA).compacting).ok === true);

  const failed = settleCompact(beginCompact(emptyCompactBook(), identA, 'llm', 2), identA, 2);
  check('error/rejection path uses the same settle', compactView(failed, identA).compacting === false);

  // Cross-round ABA: generations come from the monotonic command counter, so a
  // superseded attempt's finally can never match a later round.
  const afterRound1 = settleCompact(beginCompact(emptyCompactBook(), identA, 'llm', 10), identA, 10);
  const round2 = beginCompact(afterRound1, identA, 'llm', 11);
  const staleFinally = settleCompact(round2, identA, 10);
  check('stale finally cannot clear the live compact', compactView(staleFinally, identA).compacting === true);
  check('stale finally keeps the live mode', compactView(staleFinally, identA).mode === 'llm');
  const finished = settleCompact(staleFinally, identA, 11);
  check('live generation still settles', compactView(finished, identA).compacting === false);

  // Incarnation isolation: a retired incarnation's finally cannot touch the
  // session's next runtime.
  const nextIncarnation = beginCompact(beginCompact(emptyCompactBook(), identA, 'llm', 20), identA2, 'llm', 21);
  const staleIncarnation = settleCompact(nextIncarnation, identA, 20);
  check('retired incarnation slot stays inactive', compactView(staleIncarnation, identA).compacting === false);
  check('next incarnation stays compacting', compactView(staleIncarnation, identA2).compacting === true);
  check(
    'invalidating the retired incarnation leaves the next one',
    compactView(removeCompactRuntime(staleIncarnation, identA), identA2).compacting === true,
  );
  check(
    'invalidating the active incarnation drops it',
    compactView(removeCompactRuntime(staleIncarnation, identA2), identA2).compacting === false,
  );
}

{
  // Reported live defect: the accepted end must clear with no local RPC and no
  // confirming get_state; a later accepted start begins a new compaction.
  let book = applyCompactionEvent(emptyCompactBook(), startEvent(10));
  check('accepted start paints compacting', compactView(book, identA).compacting === true);
  check('accepted start defaults to llm mode', compactView(book, identA).mode === 'llm');
  book = applyCompactionEvent(book, endEvent(11));
  check('accepted end clears without local RPC or get_state', compactView(book, identA).compacting === false);
  check('accepted end drops the mode', compactView(book, identA).mode === null);
  check(
    'accepted end releases composer send',
    compactSendAdmission(compactView(book, identA).compacting).ok === true,
  );

  const endings: Array<[string, Partial<CompactionEndedEvent>]> = [
    ['success', {}],
    ['aborted', { aborted: true }],
    ['error', { errorMessage: 'boom' }],
    ['willRetry', { willRetry: true }],
  ];
  for (const [label, extra] of endings) {
    let ended = applyCompactionEvent(emptyCompactBook(), startEvent(20));
    ended = applyCompactionEvent(ended, endEvent(21, extra));
    check(`every accepted end settles: ${label}`, compactView(ended, identA).compacting === false);
  }

  let renewed = emptyCompactBook();
  renewed = applyCompactionEvent(renewed, startEvent(30));
  renewed = applyCompactionEvent(renewed, endEvent(31));
  renewed = applyCompactionEvent(renewed, startEvent(32));
  check('later accepted start begins a new compaction', compactView(renewed, identA).compacting === true);
  renewed = applyCompactionEvent(renewed, endEvent(33));
  check('second compaction settles', compactView(renewed, identA).compacting === false);
  renewed = applyCompactionEvent(renewed, startEvent(32));
  check('already-projected start cannot re-arm', compactView(renewed, identA).compacting === false);
  renewed = applyCompactionEvent(renewed, startEvent(29));
  check('older start cannot re-arm', compactView(renewed, identA).compacting === false);

  let snap = beginCompact(emptyCompactBook(), identA, 'snap', 40);
  snap = applyCompactionEvent(snap, startEvent(41));
  check('accepted start keeps an in-flight snap mode', compactView(snap, identA).mode === 'snap');
  snap = applyCompactionEvent(snap, endEvent(42));
  check('accepted end settles the snap round', compactView(snap, identA).compacting === false);
  check('accepted end clears snap mode', compactView(snap, identA).mode === null);

  const isolated = applyCompactionEvent(emptyCompactBook(), startEvent(50, identB));
  check(
    'another session is isolated from A',
    compactView(isolated, identB).compacting === true && compactView(isolated, identA).compacting === false,
  );
  check('another incarnation is isolated from A', compactView(isolated, identA).compacting === false);
}

{
  // Accepted cut: state baseline first, then the cut's ordered compaction
  // events; an end in the same cut beats a guard-lagged isCompacting=true.
  const baseline = reconcileCompactCut(emptyCompactBook(), identA, { isCompacting: true }, [], 5);
  check('cut baseline true adopts compaction', compactView(baseline, identA).compacting === true);
  check('cut baseline true defaults to llm', compactView(baseline, identA).mode === 'llm');

  const tailEnd = reconcileCompactCut(
    emptyCompactBook(),
    identA,
    { isCompacting: true },
    [startEvent(8), endEvent(9)],
    9,
  );
  check('cut tail end wins over the guard baseline', compactView(tailEnd, identA).compacting === false);

  const endOnly = reconcileCompactCut(
    emptyCompactBook(),
    identA,
    { isCompacting: true },
    [endEvent(9)],
    9,
  );
  check('cut end alone settles the guard baseline', compactView(endOnly, identA).compacting === false);

  const idleBaseline = reconcileCompactCut(emptyCompactBook(), identA, { isCompacting: false }, [], 5);
  check('cut baseline false keeps idle', compactView(idleBaseline, identA).compacting === false);

  const unackedLocal = reconcileCompactCut(
    beginCompact(emptyCompactBook(), identA, 'snap', 60),
    identA,
    { isCompacting: false },
    [],
    20,
  );
  check('cut false cannot erase an unacked local request', compactView(unackedLocal, identA).compacting === true);
  check('unacked local keeps its snap mode', compactView(unackedLocal, identA).mode === 'snap');

  let settled = applyCompactionEvent(emptyCompactBook(), startEvent(70));
  settled = applyCompactionEvent(settled, endEvent(71));
  const staleCut = reconcileCompactCut(settled, identA, { isCompacting: true }, [], 65);
  check('older cut baseline cannot re-arm a settled runtime', compactView(staleCut, identA).compacting === false);

  const newerCut = reconcileCompactCut(settled, identA, { isCompacting: true }, [], 72);
  check('newer authoritative cut adopts a real new compaction', compactView(newerCut, identA).compacting === true);

  const nonCompactFrames = reconcileCompactCut(
    emptyCompactBook(),
    identA,
    { isCompacting: true },
    [otherEvent(80)],
    80,
  );
  check('non-compaction cut frames are never projected', compactView(nonCompactFrames, identA).compacting === true);
  const afterIgnore = reconcileCompactCut(nonCompactFrames, identA, { isCompacting: false }, [], 80);
  check(
    'non-compaction frames leave no sequence claim, so the next cut baseline applies',
    compactView(afterIgnore, identA).compacting === false,
  );
}

{
  // Guard-lag reconnect regression: the backend publishes compaction_end
  // before CompactionActivityGuard drops, so an accepted reconnect cut can
  // carry `isCompacting: true` AND the very end we already projected. The
  // cut's own end must win — a per-event "sequence <= cursor" skip would leave
  // the badge pulsing forever.
  let ended = applyCompactionEvent(emptyCompactBook(), startEvent(70));
  ended = applyCompactionEvent(ended, endEvent(71));
  check('precondition: the end settled the runtime', compactView(ended, identA).compacting === false);

  const restatedEnd = reconcileCompactCut(ended, identA, { isCompacting: true }, [endEvent(71)], 71);
  check(
    'already-ended book: re-stated end in the cut beats guard-lag true',
    compactView(restatedEnd, identA).compacting === false,
  );

  const restatedTail = reconcileCompactCut(
    ended,
    identA,
    { isCompacting: true },
    [startEvent(70), endEvent(71)],
    71,
  );
  check(
    'already-ended book: re-stated start+end tail stays idle',
    compactView(restatedTail, identA).compacting === false,
  );

  const emptySameCut = reconcileCompactCut(emptyCompactBook(), identA, { isCompacting: true }, [endEvent(71)], 71);
  check('empty book: the same cut is idle too', compactView(emptySameCut, identA).compacting === false);

  const newerStart = reconcileCompactCut(
    ended,
    identA,
    { isCompacting: true },
    [endEvent(71), startEvent(72)],
    72,
  );
  check('already-ended book: a start newer than the end still arms', compactView(newerStart, identA).compacting === true);

  const replayedStart = reconcileCompactCut(ended, identA, { isCompacting: false }, [startEvent(50)], 80);
  check(
    'already-projected start in a cut tail cannot re-arm',
    compactView(replayedStart, identA).compacting === false,
  );

  const staleWatermark = reconcileCompactCut(ended, identA, { isCompacting: true }, [endEvent(71)], 60);
  check('stale cut watermark cannot re-arm', compactView(staleWatermark, identA).compacting === false);

  // Counterexample 1: a cut re-stating an OLDER end must not clear the newer
  // start we already projected (end < base).
  let liveRound = applyCompactionEvent(emptyCompactBook(), startEvent(70));
  liveRound = applyCompactionEvent(liveRound, endEvent(71));
  liveRound = applyCompactionEvent(liveRound, startEvent(72));
  check('precondition: the newer start is live', compactView(liveRound, identA).compacting === true);
  const fullTail = reconcileCompactCut(
    liveRound,
    identA,
    { isCompacting: true },
    [startEvent(70), endEvent(71), startEvent(72)],
    72,
  );
  check(
    'older re-stated end cannot clear the newer start',
    compactView(fullTail, identA).compacting === true,
  );

  // Counterexample 2: the previous round's end (sequence == cursor) must not
  // clear a NEW local round dispatched after that end.
  const localRound = beginCompact(ended, identA, 'snap', 90);
  const oldEnd = reconcileCompactCut(localRound, identA, { isCompacting: false }, [endEvent(71)], 71);
  check(
    'previous round end cannot clear a newer local request',
    compactView(oldEnd, identA).compacting === true,
  );
  check('newer local snap mode survives the old end', compactView(oldEnd, identA).mode === 'snap');
  const oldEndTrue = reconcileCompactCut(localRound, identA, { isCompacting: true }, [endEvent(71)], 71);
  check(
    'previous round end under a true baseline keeps the local round',
    compactView(oldEndTrue, identA).compacting === true,
  );
  const freshEnd = reconcileCompactCut(localRound, identA, { isCompacting: true }, [endEvent(72)], 72);
  check('a fresh end still settles the live round', compactView(freshEnd, identA).compacting === false);
}

{
  const streaming = compactAdmission({ streaming: true, compacting: false });
  check('streaming reject is actionable', !streaming.ok && streaming.message === COMPACT_BUSY_STREAMING);
  const duplicate = compactAdmission({ streaming: false, compacting: true });
  check('duplicate compact reject is actionable', !duplicate.ok && duplicate.message === COMPACT_BUSY_COMPACTING);
  check('idle admits compact', compactAdmission({ streaming: false, compacting: false }).ok === true);
}

{
  const blocked = compactSendAdmission(true);
  check('send/Enter blocked while compacting', !blocked.ok && blocked.message === COMPACT_SEND_BLOCKED);
  check('send admitted while idle', compactSendAdmission(false).ok === true);
}

console.log(`compactProgress.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  process.exit(1);
}
