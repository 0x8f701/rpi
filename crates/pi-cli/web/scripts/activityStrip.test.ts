#!/usr/bin/env node
import {
  activityStripView,
  applyActivityStripEvent,
  applyTaskSpawnDetails,
  emptyActivityStripState,
  reconcileActivityStrip,
} from '../src/activityStrip.ts';

const failures: string[] = [];
let assertions = 0;
function check(name: string, condition: boolean, detail = '') {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const sid = 'session-a';
const turn = 'turn-a';
let state = emptyActivityStripState(sid);
check('idle has no row', activityStripView(state, 'on') === null);
state = applyActivityStripEvent(state, { type: 'turn_started', turnId: turn }, sid);
check('turn starts working', activityStripView(state, 'on')?.text === 'Working…');
state = applyActivityStripEvent(state, { type: 'message_update', turnId: turn, assistantMessageEvent: { type: 'thinking_delta', delta: 'x' } }, sid);
check('thinking replaces working', activityStripView(state, 'on')?.state === 'thinking');
state = applyActivityStripEvent(state, { type: 'job_updated', turnId: turn, job: { id: 'job-a', agentId: 'agent-a', agent: 'glm', parentId: 'Main', status: 'running' } }, sid);
check('child beats thinking', activityStripView(state, 'on')?.text === 'Waiting for glm…');
const childState = state;
state = applyActivityStripEvent(state, { type: 'job_updated', turnId: turn, job: { id: 'nested-job', agentId: 'nested-agent', agent: 'gpt', parentId: 'agent-a', status: 'running' } }, sid);
check('nested child cannot replace Main child', state === childState);
state = applyActivityStripEvent(state, { type: 'agent_updated', turnId: turn, agent: { id: 'agent-a', displayName: 'GLM reviewer', status: 'running' } }, sid);
check('authoritative agent name updates child', activityStripView(state, 'on')?.text === 'Waiting for GLM reviewer…');
state = applyActivityStripEvent(state, { type: 'tool_execution_start', turnId: turn, toolCallId: 'tool-a', toolName: 'read' }, sid);
check('tool beats child', activityStripView(state, 'on')?.text === 'Running read…');
check('reconnecting beats tool', activityStripView(state, 'reconnecting')?.state === 'reconnecting');
state = applyActivityStripEvent(state, { type: 'tool_execution_end', turnId: turn, toolCallId: 'other' }, sid);
check('wrong tool id cannot clear', activityStripView(state, 'on')?.state === 'tool');
state = applyActivityStripEvent(state, { type: 'tool_execution_end', turnId: turn, toolCallId: 'tool-a' }, sid);
check('matching tool clears to child', activityStripView(state, 'on')?.state === 'child');
state = applyActivityStripEvent(state, { type: 'job_updated', turnId: turn, job: { id: 'other', status: 'completed' } }, sid);
check('wrong job id cannot clear', activityStripView(state, 'on')?.state === 'child');
state = applyActivityStripEvent(state, { type: 'job_updated', turnId: turn, job: { id: 'job-a', status: 'completed' } }, sid);
check('matching child clears to thinking', activityStripView(state, 'on')?.state === 'thinking');
state = applyActivityStripEvent(state, { type: 'message_update', turnId: turn, assistantMessageEvent: { type: 'text_delta', delta: 'answer' } }, sid);
check('text clears thinking', activityStripView(state, 'on')?.state === 'working');
state = applyActivityStripEvent(state, { type: 'turn_terminal', turnId: turn, status: 'succeeded' }, sid);
check('success terminal removes row', activityStripView(state, 'on') === null);
const settled = state;
state = applyActivityStripEvent(state, { type: 'tool_execution_start', turnId: turn, toolCallId: 'late', toolName: 'write' }, sid);
check('late terminal tool cannot revive', state === settled);
state = applyActivityStripEvent(state, { type: 'job_updated', turnId: turn, job: { id: 'late-job', agent: 'glm', status: 'running' } }, sid);
check('late terminal job cannot revive', state === settled);
state = applyActivityStripEvent(state, { type: 'job_updated', job: { id: 'late-unscoped', agent: 'glm', status: 'running' } }, sid);
check('late unscoped job cannot revive', state === settled);
state = applyActivityStripEvent(state, { type: 'turn_started', turnId: 'turn-b' }, sid);
state = applyActivityStripEvent(state, { type: 'turn_terminal', turnId: 'turn-b', status: 'failed', message: 'provider failed' }, sid);
check('failed terminal removes row', activityStripView(state, 'on') === null);
const failedSettled = state;
state = applyActivityStripEvent(state, { type: 'run_failed', turnId: 'turn-b', message: 'late diagnostic' }, sid);
check('idle run failure cannot revive failed terminal', state === failedSettled);
state = applyActivityStripEvent(state, { type: 'turn_started', turnId: 'turn-c' }, sid);
check('next turn clears error', activityStripView(state, 'on')?.state === 'working');
const activeBeforeRunFailed = state;
state = applyActivityStripEvent(state, { type: 'run_failed', turnId: 'turn-c', message: 'fatal failure' }, sid);
check('run failure remains advisory until terminal', state === activeBeforeRunFailed);
const sessionB = reconcileActivityStrip(undefined, 'session-b', false);
check('session B stays idle', activityStripView(sessionB, 'on') === null);
const unownedStreaming = reconcileActivityStrip(undefined, sid, true);
check('streaming without active turn stays idle', activityStripView(unownedStreaming, 'on') === null);
check('idle connecting has no row', activityStripView(emptyActivityStripState(sid), 'connecting') === null);
check('idle reconnecting has no row', activityStripView(emptyActivityStripState(sid), 'reconnecting') === null);
check('watchdog stall pulses Reconnecting while conn stays on',
  activityStripView(state, 'on', true)?.state === 'reconnecting'
  && activityStripView(state, 'on', true)?.text === 'Reconnecting…');
check('stall without a turn still shows Reconnecting',
  activityStripView(emptyActivityStripState(sid), 'on', true)?.state === 'reconnecting');

let parallelTools = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-tools' }, sid);
parallelTools = applyActivityStripEvent(parallelTools, { type: 'tool_execution_start', turnId: 'turn-tools', toolCallId: 'tool-a', toolName: 'read' }, sid);
parallelTools = applyActivityStripEvent(parallelTools, { type: 'tool_execution_start', turnId: 'turn-tools', toolCallId: 'tool-b', toolName: 'bash' }, sid);
check('newest parallel tool is visible', activityStripView(parallelTools, 'on')?.text === 'Running bash…');
parallelTools = applyActivityStripEvent(parallelTools, { type: 'tool_execution_end', turnId: 'turn-tools', toolCallId: 'tool-b' }, sid);
check('ending newest tool reveals older running tool', activityStripView(parallelTools, 'on')?.text === 'Running read…');
parallelTools = applyActivityStripEvent(parallelTools, { type: 'tool_execution_end', turnId: 'turn-tools', toolCallId: 'tool-a' }, sid);
check('ending both tools returns to working', activityStripView(parallelTools, 'on')?.state === 'working');

let reverseTools = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-tools-reverse' }, sid);
reverseTools = applyActivityStripEvent(reverseTools, { type: 'tool_execution_start', turnId: 'turn-tools-reverse', toolCallId: 'tool-a', toolName: 'read' }, sid);
reverseTools = applyActivityStripEvent(reverseTools, { type: 'tool_execution_start', turnId: 'turn-tools-reverse', toolCallId: 'tool-b', toolName: 'bash' }, sid);
reverseTools = applyActivityStripEvent(reverseTools, { type: 'tool_execution_end', turnId: 'turn-tools-reverse', toolCallId: 'tool-a' }, sid);
check('ending older tool keeps newest running tool', activityStripView(reverseTools, 'on')?.text === 'Running bash…');

let parallelChildren = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-children' }, sid);
parallelChildren = applyActivityStripEvent(parallelChildren, { type: 'job_updated', turnId: 'turn-children', job: { id: 'job-a', agentId: 'agent-a', agent: 'glm', parentId: 'Main', status: 'running' } }, sid);
parallelChildren = applyActivityStripEvent(parallelChildren, { type: 'job_updated', turnId: 'turn-children', job: { id: 'job-b', agentId: 'agent-b', agent: 'gpt', parentId: 'Main', status: 'running' } }, sid);
check('newest Main child is visible', activityStripView(parallelChildren, 'on')?.text === 'Waiting for gpt…');
parallelChildren = applyActivityStripEvent(parallelChildren, { type: 'job_updated', turnId: 'turn-children', job: { id: 'job-b', status: 'completed' } }, sid);
check('ending newest child reveals older running child', activityStripView(parallelChildren, 'on')?.text === 'Waiting for glm…');
parallelChildren = applyActivityStripEvent(parallelChildren, { type: 'job_updated', turnId: 'turn-children', job: { id: 'job-a', status: 'completed' } }, sid);
check('ending both children returns to working', activityStripView(parallelChildren, 'on')?.state === 'working');

let reverseChildren = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-children-reverse' }, sid);
reverseChildren = applyActivityStripEvent(reverseChildren, { type: 'job_updated', turnId: 'turn-children-reverse', job: { id: 'job-a', agentId: 'agent-a', agent: 'glm', parentId: 'Main', status: 'running' } }, sid);
reverseChildren = applyActivityStripEvent(reverseChildren, { type: 'job_updated', turnId: 'turn-children-reverse', job: { id: 'job-b', agentId: 'agent-b', agent: 'gpt', parentId: 'Main', status: 'running' } }, sid);
reverseChildren = applyActivityStripEvent(reverseChildren, { type: 'job_updated', turnId: 'turn-children-reverse', job: { id: 'job-a', status: 'failed' } }, sid);
check('ending older child keeps newest running child', activityStripView(reverseChildren, 'on')?.text === 'Waiting for gpt…');
reverseChildren = applyActivityStripEvent(reverseChildren, { type: 'turn_terminal', turnId: 'turn-children-reverse', status: 'failed' }, sid);
check('terminal clears every remaining identity', activityStripView(reverseChildren, 'on') === null);
const beforeUnscopedActiveEvent = state;
state = applyActivityStripEvent(state, { type: 'tool_execution_start', toolCallId: 'unscoped-tool', toolName: 'bash' }, sid);
check('unscoped active event cannot mutate activity', state === beforeUnscopedActiveEvent);
const restoredA = reconcileActivityStrip(undefined, sid, true, 'turn-restored');
check('streaming snapshot restores working', activityStripView(restoredA, 'on')?.state === 'working');
const restoredWithChild = applyActivityStripEvent(
  restoredA,
  { type: 'job_updated', turnId: 'turn-restored', job: { id: 'job-restored', agentId: 'agent-restored', agent: 'glm', parentId: 'Main', status: 'running' } },
  sid,
);
check('reconnect job snapshot restores child wait', activityStripView(restoredWithChild, 'on')?.text === 'Waiting for glm…');
const restoredWithoutCurrentChild = reconcileActivityStrip(undefined, sid, true, 'turn-current');
check('streaming restore stays working without current-turn child provenance', activityStripView(restoredWithoutCurrentChild, 'on')?.state === 'working');
const wrongTurnSnapshot = applyActivityStripEvent(
  restoredWithChild,
  { type: 'job_updated', turnId: 'turn-stale', job: { id: 'job-stale', agent: 'gpt', status: 'running' } },
  sid,
);
check('reconnect job from stale turn ignored', wrongTurnSnapshot === restoredWithChild);
check('idle snapshot clears activity', activityStripView(reconcileActivityStrip(restoredA, sid, false), 'on') === null);
check('A activity survives isolated B snapshot', activityStripView(restoredWithChild, 'on')?.state === 'child');
const bounded = applyActivityStripEvent(
  applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-long' }, sid),
  { type: 'tool_execution_start', turnId: 'turn-long', toolCallId: 'long-tool', toolName: 'x'.repeat(300) },
  sid,
);
check('tool label bounded', (activityStripView(bounded, 'on')?.text.length ?? 0) < 140);

let manyTools = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-many-tools' }, sid);
for (let index = 0; index < 65; index += 1) {
  manyTools = applyActivityStripEvent(
    manyTools,
    { type: 'tool_execution_start', turnId: 'turn-many-tools', toolCallId: `tool-${index}`, toolName: `tool ${index}` },
    sid,
  );
}
check('tool identities retain the authoritative 64-item maximum', manyTools.tools.length === 64 && manyTools.at === undefined);
manyTools = applyActivityStripEvent(manyTools, { type: 'tool_execution_end', turnId: 'turn-many-tools', toolCallId: 'tool-63' }, sid);
check('settling newest retained tool preserves every older identity', manyTools.tools.length === 63 && manyTools.tools[0]?.id === 'tool-0');

let manyChildren = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-many-children' }, sid);
for (let index = 0; index < 64; index += 1) {
  manyChildren = applyActivityStripEvent(
    manyChildren,
    { type: 'job_updated', turnId: 'turn-many-children', job: { id: `job-${index}`, agentId: `agent-${index}`, agent: `agent ${index}`, parentId: 'Main', status: 'running' } },
    sid,
  );
}
check('full authoritative Main child batch retained', manyChildren.children.length === 64);
manyChildren = applyActivityStripEvent(manyChildren, { type: 'job_updated', turnId: 'turn-many-children', job: { id: 'job-0', status: 'completed' } }, sid);
check('settling oldest child preserves all other active jobs', manyChildren.children.length === 63 && manyChildren.children.at(-1)?.jobId === 'job-63');

let taskBatch = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-task-batch' }, sid);
taskBatch = applyTaskSpawnDetails(taskBatch, sid, 'turn-task-batch', [
  { jobId: 'batch-a', agentId: 'batch-agent-a', agent: 'glm', status: 'running' },
  { jobId: 'batch-b', agentId: 'batch-agent-b', agent: 'gpt', status: 'queued' },
]);
check('Task result projects every authoritative child identity', taskBatch.children.map((child) => child.jobId).join(',') === 'batch-a,batch-b');
taskBatch = applyActivityStripEvent(taskBatch, { type: 'job_updated', turnId: 'turn-task-batch', job: { id: 'batch-b', status: 'completed' } }, sid);
check('Task batch completion settles only matching child identity', taskBatch.children.length === 1 && taskBatch.children[0]?.jobId === 'batch-a');

let cancellingChild = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-cancelling' }, sid);
cancellingChild = applyActivityStripEvent(cancellingChild, { type: 'job_updated', turnId: 'turn-cancelling', job: { id: 'job-cancel', agentId: 'agent-cancel', agent: 'glm', parentId: 'Main', status: 'cancelling' } }, sid);
check('cancelling Main child stays on the strip', activityStripView(cancellingChild, 'on')?.text === 'Waiting for glm…');
cancellingChild = applyTaskSpawnDetails(cancellingChild, sid, 'turn-cancelling', [
  { jobId: 'job-drain', agentId: 'agent-drain', agent: 'gpt', status: 'cancelling' },
]);
check('cancelling Task spawn stays live', cancellingChild.children.some((child) => child.jobId === 'job-drain'));
const beforeTerminal = cancellingChild;
cancellingChild = applyActivityStripEvent(cancellingChild, { type: 'job_updated', turnId: 'turn-cancelling', job: { id: 'job-cancel', status: 'cancelled' } }, sid);
check('cancelled settles off the strip', !cancellingChild.children.some((child) => child.jobId === 'job-cancel') && cancellingChild !== beforeTerminal);
check('sibling cancelling child remains', cancellingChild.children.some((child) => child.jobId === 'job-drain'));

// Stop-tap regression (goal-stop "Saving reply…"): the optimistic idle reset
// (reconcileActivityStrip(state, sid, false, '')) must NOT orphan the live
// turn's typed identity. If the reset drops turnId, the real turn_terminal
// can no longer reach the terminal ledger, the settled-refill ownership
// check rejects the canonical-id hydration, and the aborted reply is stuck
// on "Saving reply…" forever.
let stopPending = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-stop' }, sid);
stopPending = reconcileActivityStrip(stopPending, sid, false, '');
check('stop-tap idle preserves the pending turn identity for its terminal', stopPending.turnId === 'turn-stop' && stopPending.isStreaming === false);
check('stop-tap idle still clears every activity visual (idle, no tools/children/thinking)', activityStripView(stopPending, 'on') === null && stopPending.tools.length === 0 && stopPending.children.length === 0 && stopPending.isThinking === false && stopPending.error === '');
const stopSettled = applyActivityStripEvent(stopPending, { type: 'turn_terminal', turnId: 'turn-stop', status: 'aborted' }, sid);
check('real terminal after a stop-tap idle lands on the terminal ledger', stopSettled.terminalTurnIds.includes('turn-stop') && stopSettled.turnId === '' && stopSettled.isStreaming === false && activityStripView(stopSettled, 'on') === null);
// An ALREADY-terminated turn resets exactly as before (no identity resurrection).
const terminatedTurn = applyActivityStripEvent(
  applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-done' }, sid),
  { type: 'turn_terminal', turnId: 'turn-done', status: 'succeeded' },
  sid,
);
const terminatedReset = reconcileActivityStrip(terminatedTurn, sid, false, '');
check('terminated turn still resets cleanly on an idle snapshot', terminatedReset.turnId === '' && terminatedReset.terminalTurnIds.includes('turn-done') && activityStripView(terminatedReset, 'on') === null);

console.log(`activityStrip.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  process.exit(1);
}
