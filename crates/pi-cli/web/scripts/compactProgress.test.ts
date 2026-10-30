#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import {
  COMPACT_BUSY_COMPACTING,
  COMPACT_BUSY_STREAMING,
  COMPACT_SEND_BLOCKED,
  beginCompact,
  compactAdmission,
  compactBadgeLabel,
  compactSendAdmission,
  compactStatusText,
  reconcileCompacting,
  settleCompact,
  type CompactBook,
} from '../src/compactProgress.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function empty(): CompactBook {
  return { compacting: {}, mode: {}, token: {}, settled: {} };
}

{
  check('llm status is Compacting context…', compactStatusText('llm') === 'Compacting context…');
  check('snap status is Snapcompacting context…', compactStatusText('snap') === 'Snapcompacting context…');
  check('missing mode defaults to Compacting context…', compactStatusText(null) === 'Compacting context…');
  check('llm badge is compacting', compactBadgeLabel('llm') === 'compacting');
  check('snap badge is snapcompacting', compactBadgeLabel('snap') === 'snapcompacting');
}

{
  const started = beginCompact(empty(), 'sess-a', 'llm');
  check('optimistic compacting is true before RPC settles', started.book.compacting['sess-a'] === true);
  check('optimistic mode is llm', started.book.mode['sess-a'] === 'llm');
  check('token is positive', started.token === 1 && started.book.token['sess-a'] === 1);
  const snap = beginCompact(empty(), 'sess-a', 'snap');
  check('snap dispatch is visible as snap', snap.book.mode['sess-a'] === 'snap');
  check('snap status text is distinct', compactStatusText(snap.book.mode['sess-a']) === 'Snapcompacting context…');
}

{
  let book = beginCompact(empty(), 'sess-a', 'llm').book;
  const token = book.token['sess-a'];
  book = settleCompact(book, 'sess-a', token);
  check('success finally clears compacting', book.compacting['sess-a'] === false);
  check('success finally drops mode', book.mode['sess-a'] === undefined);
  check('success finally drops token', book.token['sess-a'] === undefined);

  const first = beginCompact(empty(), 'sess-a', 'llm');
  book = settleCompact(first.book, 'sess-a', first.token);
  check('error/timeout path uses the same settle', book.compacting['sess-a'] === false);

  const live = beginCompact(empty(), 'sess-a', 'llm');
  const stale = settleCompact(live.book, 'sess-a', live.token - 1);
  check('stale timeout does not clear a newer compact', stale.compacting['sess-a'] === true);
  check('stale timeout keeps the live token', stale.token['sess-a'] === live.token);

  const finished = settleCompact(live.book, 'sess-a', live.token);
  const lateTrue = reconcileCompacting(finished, 'sess-a', true);
  check('late server true cannot re-arm a settled compact', lateTrue.compacting['sess-a'] === false);
  check('late server true keeps the settlement marker', lateTrue.settled['sess-a'] === true);
  const confirmedIdle = reconcileCompacting(lateTrue, 'sess-a', false);
  check('authoritative false releases settlement suppression', confirmedIdle.settled['sess-a'] === undefined);
  const laterExternal = reconcileCompacting(confirmedIdle, 'sess-a', true);
  check('server compact can be adopted after false confirmation', laterExternal.compacting['sess-a'] === true);
}

{
  const streaming = compactAdmission({ streaming: true, compacting: false });
  check('streaming reject is actionable', !streaming.ok && streaming.message === COMPACT_BUSY_STREAMING);
  const compacting = compactAdmission({ streaming: false, compacting: true });
  check('duplicate compact reject is actionable', !compacting.ok && compacting.message === COMPACT_BUSY_COMPACTING);
  const idle = compactAdmission({ streaming: false, compacting: false });
  check('idle admits compact', idle.ok === true);
}

{
  let book = empty();
  book = reconcileCompacting(book, 'sess-a', true);
  check('get_state isCompacting true becomes visible', book.compacting['sess-a'] === true);
  check('server-started compact defaults to llm mode', book.mode['sess-a'] === 'llm');
  check('server-started compact has no local token', book.token['sess-a'] === undefined);
  book = reconcileCompacting(book, 'sess-a', false);
  check('get_state false clears when no local in-flight', book.compacting['sess-a'] === false);
  const started = beginCompact(empty(), 'sess-a', 'snap');
  const raced = reconcileCompacting(started.book, 'sess-a', false);
  check('stale get_state false cannot hide optimistic start', raced.compacting['sess-a'] === true);
  check('stale get_state false keeps snap mode', raced.mode['sess-a'] === 'snap');
}

{
  let book = beginCompact(empty(), 'sess-a', 'llm').book;
  book = beginCompact(book, 'sess-b', 'snap').book;
  check('A and B compact independently', book.compacting['sess-a'] === true && book.compacting['sess-b'] === true);
  check('A and B modes stay isolated', book.mode['sess-a'] === 'llm' && book.mode['sess-b'] === 'snap');
  book = settleCompact(book, 'sess-a', book.token['sess-a']);
  check('settling A leaves B compacting', book.compacting['sess-a'] === false && book.compacting['sess-b'] === true);
  check('settling A leaves B snap mode', book.mode['sess-b'] === 'snap');
}

{
  const blocked = compactSendAdmission(true);
  check('send/Enter blocked while compacting', !blocked.ok && blocked.message === COMPACT_SEND_BLOCKED);
  check('send admitted while idle', compactSendAdmission(false).ok === true);
}

{
  const source = fs.readFileSync(path.join(process.cwd(), 'src/App.tsx'), 'utf8');
  check('top compact badge remains', source.includes('id="compact-badge"'));
  check('transcript compact row removed', !source.includes('id="compact-progress"'));
  check('composer compact row removed', !source.includes('id="composer-compact-status"'));
  const sendDisabled = source.match(/disabled=\{([^}]*)\}/g)?.find((expr) => expr.includes('activeCompacting'));
  check('send disabled expression exists', sendDisabled != null);
  check('send remains disabled while compacting', sendDisabled != null && sendDisabled.includes('activeCompacting'));
  check('send disabled keeps streaming Stop precedence', sendDisabled != null && sendDisabled.includes('!stopArmed'));
  check('send disabled keeps transport precedence', sendDisabled != null && sendDisabled.includes("connState !== 'on'"));
  check('send label remains compact-mode aware', source.includes("? compactStatusText(activeCompactMode)"));
  const compactDispatch = source.slice(source.indexOf("if (action.type === 'compact')"), source.indexOf("if (action.type === 'skill')"));
  const compactSuccess = compactDispatch.slice(compactDispatch.indexOf('.then((data)'), compactDispatch.indexOf('.catch('));
  const compactFailure = compactDispatch.slice(compactDispatch.indexOf('.catch('));
  check('compact success refreshes the exact owning context meter', compactSuccess.includes('refreshContextUsage(targetSid)'));
  check('compact failure does not fake a context refresh', !compactFailure.includes('refreshContextUsage('));
}

console.log(`compactProgress.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  process.exit(1);
}
