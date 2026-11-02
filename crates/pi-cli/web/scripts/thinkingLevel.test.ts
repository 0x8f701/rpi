#!/usr/bin/env node
// Focused regression for src/thinkingLevel.ts (esbuild-bundled like
// scripts/goal.test.ts; exit 0 = all assertions held, 1 = regression).
// Run: esbuild scripts/thinkingLevel.test.ts --bundle --platform=node
// --format=esm --outfile=.web-test-build/thinkingLevel.test.mjs && node
// .web-test-build/thinkingLevel.test.mjs
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  EMPTY_THINKING_LEVEL_STATE,
  reconcileThinkingLevel,
  requestThinkingLevel,
  selectionAppliesNextTurn,
  setThinkingLevelCommand,
  settleThinkingRequest,
  thinkingCommandTargetSessionId,
  thinkingLevelClampMessage,
  thinkingLevelView,
} from '../src/thinkingLevel.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

{
  const warmHandoff = { commandTargetSessionId: 'A', displayedSessionId: 'B' };
  check('commandTarget wins over displayed', thinkingCommandTargetSessionId(warmHandoff, 'B') === 'A');
  check('commandTarget wins over stale sessionIdRef', thinkingCommandTargetSessionId(warmHandoff, 'A-old') === 'A');
  check('no display state falls back to sessionIdRef', thinkingCommandTargetSessionId(null, 'A') === 'A');
  check('empty commandTarget falls to displayed', thinkingCommandTargetSessionId({ commandTargetSessionId: '', displayedSessionId: 'B' }, 'A') === 'B');
  check('null commandTarget falls to displayed', thinkingCommandTargetSessionId({ commandTargetSessionId: null, displayedSessionId: 'B' }, null) === 'B');
  check('no authority at all resolves to empty', thinkingCommandTargetSessionId(null, '') === '');
  check('displayed-only authority is used before the bind', thinkingCommandTargetSessionId({ displayedSessionId: 'B' }, 'A') === 'B');
}

{
  const command = setThinkingLevelCommand('B', 'high');
  check('set_thinking_level command type', command.type === 'set_thinking_level');
  check('set_thinking_level command level', command.level === 'high');
  check('set_thinking_level command carries sessionId', command.sessionId === 'B');
}

{
  let state = EMPTY_THINKING_LEVEL_STATE;
  let decision = requestThinkingLevel(state, 'A', 'high');
  check('first selection sends exactly one frame', decision.send === true);
  state = decision.state;
  decision = requestThinkingLevel(state, 'A', 'high');
  check('identical re-selection while in flight is deduped', decision.send === false && decision.state === state);
  decision = requestThinkingLevel(state, 'A', 'low');
  check('different level while in flight still sends', decision.send === true);
  const settled = reconcileThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'medium');
  check('re-selecting the settled level never re-sends', requestThinkingLevel(settled, 'A', 'medium').send === false);
  check('level outside the known catalog never sends', requestThinkingLevel(state, 'A', 'bogus', ['off', 'low', 'high']).send === false);
  check('empty catalog does not block the selection', requestThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'high', []).send === true);
  check('empty level never sends', requestThinkingLevel(state, 'A', '').send === false);
  check('empty session never sends', requestThinkingLevel(state, '', 'high').send === false);
}

{
  const request = requestThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'high');
  check('selection is displayed immediately', thinkingLevelView(request.state).A === 'high');
  const racing = reconcileThinkingLevel(request.state, 'A', 'off');
  check('stale get_state during the round trip never overrides the selection', thinkingLevelView(racing).A === 'high');
  const settledHigh = settleThinkingRequest(request.state, 'A', 1, true, 'high');
  check('settled ack records the effective level', thinkingLevelView(settledHigh).A === 'high');
  const clamped = settleThinkingRequest(request.state, 'A', 1, true, 'off');
  check('clamped settle records the truthful effective level', thinkingLevelView(clamped).A === 'off');
  const bare = settleThinkingRequest(request.state, 'A', 1, true, null);
  check('bare ack clears the pending overlay', thinkingLevelView(bare).A === undefined);
  const restored = reconcileThinkingLevel(bare, 'A', 'high');
  check('queued get_state reconcile records the level after a bare ack', thinkingLevelView(restored).A === 'high');
}

{
  const request = requestThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'high');
  const failed = settleThinkingRequest(request.state, 'A', 1, false);
  check('failed selection reverts to the settled level', thinkingLevelView(failed).A === undefined);
  check('failed selection leaves no pending slot', selectionAppliesNextTurn(failed, 'A') === false);
  const staleFail = settleThinkingRequest(request.state, 'A', 2, false);
  check('stale failure mutates nothing', staleFail === request.state);
  const queued = requestThinkingLevel(request.state, 'A', 'low');
  const staleOk = settleThinkingRequest(queued.state, 'A', 1, true, 'high');
  check('stale settle records the applied level without touching the newer overlay', staleOk.settled.A === 'high' && thinkingLevelView(staleOk).A === 'low');
  check('rejected selection installs no overlay', thinkingLevelView(requestThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'bogus', ['off']).state).A === undefined);
}

{
  const first = requestThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'high');
  const settledState = settleThinkingRequest(first.state, 'A', 1, true, 'high');
  const second = requestThinkingLevel(settledState, 'A', 'low');
  check('seq is monotonic across the state lifetime, never reused', second.send === true
    && second.state.nextSeq === 3
    && second.state.pending.get('A')?.seq === 2);
  const late = settleThinkingRequest(second.state, 'A', 1, false);
  check('late stale settle never deletes the newer pending', late.pending.get('A')?.seq === 2 && late.pending.get('A')?.level === 'low');
}
{
  const established = reconcileThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'medium');
  const requested = requestThinkingLevel(established, 'A', 'high');
  const failed = settleThinkingRequest(requested.state, 'A', 1, false);
  check('failure reverts to the established level without reconcile masking', thinkingLevelView(failed).A === 'medium');
  check('failed selection leaves no pending slot', selectionAppliesNextTurn(failed, 'A') === false);
  const bootstrap = reconcileThinkingLevel(failed, 'A', 'medium');
  check('reconnect bootstrap keeps the established level', thinkingLevelView(bootstrap).A === 'medium' && reconcileThinkingLevel(bootstrap, 'A', 'medium') === bootstrap);
  check('empty push level is ignored', thinkingLevelView(reconcileThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', '')).A === undefined);
}

{
  const settled = reconcileThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'off');
  const streaming = requestThinkingLevel(settled, 'A', 'high');
  check('selection while streaming shows applies-next-turn', selectionAppliesNextTurn(streaming.state, 'A') === true);
  check('next-turn selection stays displayed', thinkingLevelView(streaming.state).A === 'high');
  const settledAfterTurn = settleThinkingRequest(streaming.state, 'A', 1, true, 'high');
  check('settle clears applies-next-turn', selectionAppliesNextTurn(settledAfterTurn, 'A') === false);
  check('no-op selection never reports applies-next-turn', selectionAppliesNextTurn(settled, 'A') === false);
}

{
  let state = EMPTY_THINKING_LEVEL_STATE;
  state = requestThinkingLevel(state, 'A', 'high').state;
  state = settleThinkingRequest(state, 'A', 1, true, 'high');
  state = requestThinkingLevel(state, 'B', 'low').state;
  const view = thinkingLevelView(state);
  check('session A keeps its level while B selects', view.A === 'high' && view.B === 'low');
  state = reconcileThinkingLevel(state, 'B', 'low');
  check('switch to B preserves A settled level', thinkingLevelView(state).A === 'high');
  state = requestThinkingLevel(state, 'B', 'medium').state;
  check('in-flight B selection leaves A untouched', thinkingLevelView(state).A === 'high' && thinkingLevelView(state).B === 'medium');
}


{
  class AppThinkingSeam {
    commandTarget = 'A';
    sessionIdRef = 'A';
    readonly frames: Array<{ sessionId: string; level: string }> = [];
    state = EMPTY_THINKING_LEVEL_STATE;
    levels = ['off', 'low', 'medium', 'high'];
    select(level: string): boolean {
      const target = thinkingCommandTargetSessionId(
        { commandTargetSessionId: this.commandTarget, displayedSessionId: this.sessionIdRef },
        this.sessionIdRef,
      );
      const decision = requestThinkingLevel(this.state, target, level, this.levels);
      this.state = decision.state;
      if (!decision.send) return false;
      this.frames.push(setThinkingLevelCommand(target, level));
      return true;
    }
  }
  const seam = new AppThinkingSeam();
  seam.sessionIdRef = 'B';
  seam.commandTarget = 'A';
  check('warm handoff selection routes to commandTarget A', seam.select('high') === true
    && seam.frames.length === 1
    && seam.frames[0].sessionId === 'A'
    && seam.frames[0].level === 'high');
  seam.select('high');
  check('duplicate selection adds no frame', seam.frames.length === 1);
  seam.commandTarget = 'B';
  seam.sessionIdRef = 'B';
  check('post-cut selection routes to the new commandTarget', seam.select('low') === true
    && seam.frames.length === 2
    && seam.frames[1].sessionId === 'B');
}

/* Clamp toast contract: a backend clamp envelope ({clamped:true, level,
 * message}) yields its message; every non-clamp/malformed envelope yields
 * null (a bare ack never toasts); and the settled state shows the CLAMPED
 * level — the exact pair App's onThinkingChange wires into toast(). ----- */
{
  const clampedEnvelope = {
    requested: 'medium',
    level: 'off',
    clamped: true,
    message: 'Thinking level medium unsupported by faux/faux-1 (reasoning disabled); using off',
  };
  check('clamp envelope yields its backend message',
    thinkingLevelClampMessage(clampedEnvelope)
      === 'Thinking level medium unsupported by faux/faux-1 (reasoning disabled); using off');
  check('non-clamped envelope yields null', thinkingLevelClampMessage({ ...clampedEnvelope, clamped: false }) === null);
  check('missing clamped flag yields null', thinkingLevelClampMessage({ level: 'off', message: 'x' }) === null);
  check('empty clamp message yields null', thinkingLevelClampMessage({ ...clampedEnvelope, message: '' }) === null);
  check('non-string clamp message yields null', thinkingLevelClampMessage({ ...clampedEnvelope, message: 7 }) === null);
  check('missing message yields null', thinkingLevelClampMessage({ clamped: true, level: 'off' }) === null);
  check('non-object response yields null', thinkingLevelClampMessage('clamped') === null && thinkingLevelClampMessage(null) === null);
  check('array response yields null', thinkingLevelClampMessage([clampedEnvelope]) === null);
  /* The user journey: the select optimistically shows 'medium', the clamp
   * settles to the truthful effective level, and the message toasts. */
  const request = requestThinkingLevel(EMPTY_THINKING_LEVEL_STATE, 'A', 'medium');
  check('select optimistically shows the requested level', thinkingLevelView(request.state).A === 'medium');
  const settled = settleThinkingRequest(request.state, 'A', 1, true, clampedEnvelope.level);
  check('the select shows the clamped value after the ack', thinkingLevelView(settled).A === 'off');
}

/* Wiring pin: App's onThinkingChange toasts the clamp message (non-error) for
 * the displayed session — a re-implementation that silently drops the clamp
 * message fails here. */
{
  const appSource = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../src/App.tsx'), 'utf8');
  const onChangeAt = appSource.indexOf('const onThinkingChange = useCallback');
  const onChangeEnd = appSource.indexOf('/* ---------------- effects ---------------- */', onChangeAt);
  const onChangeBody = onChangeAt >= 0 && onChangeEnd > onChangeAt ? appSource.slice(onChangeAt, onChangeEnd) : '';
  check('onThinkingChange wires the clamp message into a non-error toast',
    onChangeBody.includes('thinkingLevelClampMessage(response)')
      && onChangeBody.includes('if (clampMessage !== null) toast(clampMessage, false)'));
  check('onThinkingChange still settles to the backend effective level',
    onChangeBody.includes('settleThinkingRequest(thinkingLevelStateRef.current, target, request.seq, true, effective)'));
}

if (failures.length > 0) {
  console.error(`thinkingLevel: ${failures.length}/${ran} checks FAILED:\n- ${failures.join('\n- ')}`);
  process.exit(1);
}
console.log(`thinkingLevel: ${ran} checks passed`);