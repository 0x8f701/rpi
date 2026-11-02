#!/usr/bin/env node
// Project channel + Broadcast contract for src/chat/team.ts (the single
// Project-channel controller per /tmp/chat-channel-broadcast-contract.md).
//
// Covers: per-Project channel state and target normalization, direct vs
// Broadcast target selection, exact-session broadcast fan-out through
// RunnerCommandClient.command with per-attempt provenance, partial failure,
// cancel/retry, one runner-event callback per mapped runner, subscribeSession
// exact-session fan-out, and bounded retained runs.
//
// Run through `npm run build`. Exit codes: 0 = every assertion held; 1 = a
// regression.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  insertMentionToken,
  resolveChannelRecipients,
  runnerMentionToken,
  trailingMentionQuery,
} from '../src/chat/chatMentions.ts';
import {
  composeWakeBrief,
  loadChannelLogs,
  saveChannelLogs,
  channelLogStorageKey,
  channelLogErrorMessage,
  MAX_CHANNEL_MESSAGE_CHARS,
} from '../src/chat/chatChannelLog.ts';
import {
  MAX_BROADCAST_RUNS_PER_PROJECT,
  TeamStore,
  activeProjectMembers,
  deriveBroadcastRunStatus,
  runnerSessionKey,
  type BroadcastAttempt,
  type BroadcastResponseCard,
  type BroadcastRun,
  type ChannelTarget,
  type ProjectChannelState,
  type TeamSnapshot,
} from '../src/chat/team.ts';
import type { ProjectRecord, ProjectRunnerMapping, RunnerCommandClient } from '../src/chat/mapping.ts';
import type { RunnerCommand } from '../src/chat/protocol.ts';
import type { RunnerView } from '../src/chat/runners.ts';
import type { RpcResponse } from '../src/types.ts';

const failures: string[] = [];
let assertions = 0;
let activeBlock = 'module initialization';

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${activeBlock}: ${name}${detail ? ` (${detail})` : ''}`);
}
function incarnationOf(sessionId: string): string {
  return `inc-${sessionId}`;
}

function project(id: string, label = `Project ${id}`, cwd = `/workspace/${id}`): ProjectRecord {
  return { id, label, cwd, createdAt: 1, updatedAt: 1 };
}

function runner(id: string, state: RunnerView['state'] = 'connected'): RunnerView {
  return {
    id,
    label: `Runner ${id}`,
    origin: `https://${id}.example`,
    authority: `${id}.example`,
    createdAt: 1,
    updatedAt: 1,
    connectionEpoch: 1,
    state,
    error: '',
    probe: { sessionId: `source-${id}`, runtimeIncarnation: incarnationOf(`source-${id}`), cwd: `/workspace/${id}`, sessionCount: 1 },
    status: { tokenRequired: false, version: '0.2.11', codename: null },
  };
}

function activeMapping(
  id: string,
  projectId: string,
  runnerId: string,
  sessionId: string,
  createdAt = 1,
): ProjectRunnerMapping {
  return {
    id,
    projectId,
    runnerId,
    operationId: `op-${id}`,
    sourceSessionId: `source-${runnerId}`,
    canaryId: `canary-${id}`,
    createId: `create-${id}`,
    sessionName: `Session ${id}`,
    createdAt,
    updatedAt: createdAt,
    state: 'active',
    sessionId,
    sessionPath: `/workspace/sessions/${sessionId}.jsonl`,
  };
}

function response(id: string | undefined, command: string, success: boolean, data?: unknown, error?: string, sessionId = 's'): RpcResponse {
  return { type: 'response', id, command, sessionId, success, data, error };
}

interface CommandCall {
  command: RunnerCommand;
  options: { commandId?: string; sourceSessionId?: string; allowBeforeProbe?: boolean; beforeSend?: () => void };
}

class FakeRunnerClient implements RunnerCommandClient {
  readonly calls: CommandCall[] = [];
  readonly views: RunnerView[] = [];
  promptResponses: Array<{ data?: unknown; error?: Error }> = [];
  abortResponses: Array<{ data?: unknown; error?: Error }> = [];
  promptIndex = 0;
  abortIndex = 0;
  beforeAbortSend: (() => void) | null = null;

  constructor(views: RunnerView[] = []) {
    this.views = views;
  }

  snapshot(): RunnerView[] {
    return this.views.map((view) => ({ ...view }));
  }

  async command(
    _runnerId: string,
    command: RunnerCommand,
    options: { commandId?: string; sourceSessionId?: string; allowBeforeProbe?: boolean; beforeSend?: () => void } = {},
  ): Promise<RpcResponse> {
    if (command.type === 'abort') this.beforeAbortSend?.();
    options.beforeSend?.();
    this.calls.push({ command, options });
    if (command.type === 'prompt') {
      const next = this.promptResponses[this.promptIndex] ?? { data: { turnId: `turn-${this.promptIndex + 1}` } };
      this.promptIndex += 1;
      if (next.error) throw next.error;
      return response(options.commandId, 'prompt', true, next.data, undefined, options.sourceSessionId);
    }
    if (command.type === 'abort') {
      const next = this.abortResponses[this.abortIndex] ?? { data: {} };
      this.abortIndex += 1;
      if (next.error) throw next.error;
      return response(options.commandId, 'abort', true, next.data, undefined, options.sourceSessionId);
    }
    return response(options.commandId, command.type, true, {});
  }
}

function reconcile(
  store: TeamStore,
  projects: readonly ProjectRecord[],
  mappings: readonly ProjectRunnerMapping[],
  views: readonly RunnerView[],
): TeamSnapshot {
  store.reconcileInputs(projects, mappings, views);
  return store.snapshot();
}

function channelOf(snapshot: TeamSnapshot, projectId: string): ProjectChannelState | null {
  return snapshot.channels?.[projectId] ?? null;
}

function cardOf(run: BroadcastRun, mappingId: string): BroadcastResponseCard | undefined {
  return run.responses.find((card) => card.mappingId === mappingId);
}

function attemptOf(card: BroadcastResponseCard | undefined): BroadcastAttempt | undefined {
  if (!card) return undefined;
  return card.attempts.find((row) => row.id === card.currentAttemptId) ?? card.attempts.at(-1);
}

function promptText(call: CommandCall): string {
  if (call.command.type !== 'prompt') return '';
  return typeof call.command.message === 'string' ? call.command.message : '';
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

async function main(): Promise<void> {
  activeBlock = 'channel projection and target normalization';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1'), project('p2')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
      activeMapping('m3', 'p2', 'a', 's3', 1),
    ];
    const snapshot = reconcile(store, projects, mappings, views);
    check('one channel per project', Object.keys(snapshot.channels ?? {}).length === 2);
    const p1 = channelOf(snapshot, 'p1');
    check('channel exists for p1', p1 !== null);
    check('channel defaults context pane to files', p1?.contextPane === 'files');
    check('two active mappings default the target to broadcast', p1?.target?.kind === 'broadcast');
    check('channel has no broadcast runs yet', (p1?.broadcastRuns ?? []).length === 0);
    check('selected project defaults to first', snapshot.selectedProjectId === 'p1');
    check('mobile pane defaults to projects', snapshot.mobilePane === 'projects');

    const members = activeProjectMembers('p1', mappings, views, snapshot.sessions);
    check('active members are ordered by createdAt then id', members.map((member) => member.mapping.id).join(',') === 'm1,m2');
    check('member carries the exact runner', members[0]?.runner.id === 'a');
    check('member carries the exact session', members[0]?.session.sessionId === 's1');
    check('nonactive mappings are excluded', activeProjectMembers('p2', mappings, views, snapshot.sessions).length === 1);
    store.dispose();
  }

  activeBlock = 'target selection and per-project retention';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1'), project('p2')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
      activeMapping('m3', 'p2', 'a', 's3', 1),
    ];
    reconcile(store, projects, mappings, views);
    store.selectTarget('p1', { kind: 'runner', mappingId: 'm2' });
    let snapshot = store.snapshot();
    const target = channelOf(snapshot, 'p1')?.target;
    check('runner target is retained per project', target?.kind === 'runner'
      && (target as { mappingId?: string } | null)?.mappingId === 'm2');
    const p2Target = channelOf(snapshot, 'p2')?.target;
    check('other project defaults a sole runner to the channel', p2Target?.kind === 'broadcast');

    store.selectTarget('p1', { kind: 'broadcast' });
    snapshot = store.snapshot();
    check('broadcast target is retained', channelOf(snapshot, 'p1')?.target?.kind === 'broadcast');

    store.selectProject('p2');
    snapshot = store.snapshot();
    check('selectProject switches the selected project', snapshot.selectedProjectId === 'p2');
    check('selectProject keeps per-project targets', channelOf(snapshot, 'p1')?.target?.kind === 'broadcast');

    store.selectTarget('p1', { kind: 'runner', mappingId: 'm1' });
    store.setContextPane('p1', 'jobs');
    snapshot = store.snapshot();
    check('context pane is retained per project', channelOf(snapshot, 'p1')?.contextPane === 'jobs');
    check('other project context pane stays files', channelOf(snapshot, 'p2')?.contextPane === 'files');

    store.setGoalOwner('p1', 'm2');
    store.setFileOwner('p1', 'm1');
    snapshot = store.snapshot();
    check('goal owner is retained', channelOf(snapshot, 'p1')?.goalOwnerMappingId === 'm2');
    check('file owner is retained', channelOf(snapshot, 'p1')?.fileOwnerMappingId === 'm1');

    store.setMobilePane('context');
    snapshot = store.snapshot();
    check('mobile pane is set', snapshot.mobilePane === 'context');
    store.dispose();
  }

  activeBlock = 'target normalization on mapping changes';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    let snapshot = store.snapshot();
    check('two active mappings default to broadcast', channelOf(snapshot, 'p1')?.target?.kind === 'broadcast');

    store.selectTarget('p1', { kind: 'runner', mappingId: 'm2' });
    const reduced = [mappings[0]!];
    reconcile(store, projects, reduced, views);
    snapshot = store.snapshot();
    const target = channelOf(snapshot, 'p1')?.target;
    check('removed target falls back to the channel', target?.kind === 'broadcast');

    reconcile(store, projects, [], views);
    snapshot = store.snapshot();
    check('zero active mappings clears the target', channelOf(snapshot, 'p1')?.target === null);
    store.dispose();
  }

  activeBlock = 'broadcast dispatch fan-out';
  {
    const views = [runner('a'), runner('b'), runner('c', 'offline')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
      activeMapping('m3', 'p1', 'c', 's3', 3),
    ];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', '  ship it  ');
    check('sendBroadcast returns a run', run.id !== '' && run.projectId === 'p1');
    check('prompt is the exact trimmed user string', run.prompt === 'ship it');
    check('run captures every active mapping in stable order', run.targetMappingIds.join(',') === 'm1,m2,m3');
    check('one response card per target', run.responses.length === 3);
    const promptCalls = client.calls.filter((call) => call.command.type === 'prompt');
    const briefA = promptText(promptCalls[0]!);
    const briefB = promptText(promptCalls[1]!);
    check('every connected target receives a wake brief', promptCalls.length === 2);
    check('wake brief names the runner as self', briefA.includes('You are @Runner-a in #Project-p1.')
      && briefB.includes('You are @Runner-b in #Project-p1.'));
    check('wake briefs differ per runner', briefA !== briefB);
    check('wake brief is not the raw human string', briefA !== 'ship it' && briefB !== 'ship it');
    check('wake brief includes the shared human line', briefA.includes('You: ship it') && briefB.includes('You: ship it'));
    check('wake brief lists the other teammate', briefA.includes('@Runner-b') && briefB.includes('@Runner-a'));
    check('prompt routes the exact mapped session', promptCalls.map((call) => call.options.sourceSessionId).join(',') === 's1,s2');
    const logAfterSend = channelOf(store.snapshot(), 'p1')?.channelMessages ?? [];
    check('send appends one human channel line', logAfterSend.length === 1
      && logAfterSend[0]?.kind === 'human'
      && logAfterSend[0]?.text === 'ship it'
      && logAfterSend[0]?.author === 'You');
    check('offline target sends nothing', !client.calls.some((call) => call.options.sourceSessionId === 's3' && call.command.type === 'prompt'));
    const offlineCard = cardOf(run, 'm3');
    check('offline target fails as precondition', attemptOf(offlineCard)?.status === 'failed'
      && attemptOf(offlineCard)?.failureKind === 'precondition');
    check('offline target error is bounded', (attemptOf(offlineCard)?.error ?? '').length <= 240);
    await flush();
    let snapshot = store.snapshot();
    const liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('run is retained in the channel', liveRun !== undefined);
    check('accepted turn binds the card', attemptOf(cardOf(liveRun!, 'm1'))?.turnId === 'turn-1'
      && attemptOf(cardOf(liveRun!, 'm2'))?.turnId === 'turn-2');
    check('accepted turn moves the card to streaming', attemptOf(cardOf(liveRun!, 'm1'))?.status === 'streaming');
    check('run status is running while any card streams', deriveBroadcastRunStatus(liveRun!) === 'running');
    store.dispose();
  }

  activeBlock = 'broadcast event routing and terminal settlement';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    // Same turn id across runners: isolation is by (runner, session, turn).
    client.promptResponses = [
      { data: { turnId: 'turn-1' } },
      { data: { turnId: 'turn-1' } },
    ];
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', 'ship it');
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_started', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', model: { id: 'model-a' } });
    store.applyRunnerEvent('a', {
      type: 'message_start',
      sessionId: 's1', runtimeIncarnation: incarnationOf('s1'),
      turnId: 'turn-1',
      itemId: 'a-msg-1',
      message: { role: 'assistant' },
    });
    store.applyRunnerEvent('a', {
      type: 'message_update',
      sessionId: 's1', runtimeIncarnation: incarnationOf('s1'),
      turnId: 'turn-1',
      itemId: 'a-msg-1',
      assistantMessageEvent: { type: 'text_delta', delta: 'streamed reply' },
    });
    store.applyRunnerEvent('b', { type: 'turn_started', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-1', model: { id: 'model-b' } });
    store.applyRunnerEvent('b', {
      type: 'message_start',
      sessionId: 's2', runtimeIncarnation: incarnationOf('s2'),
      turnId: 'turn-1',
      itemId: 'b-msg-1',
      message: { role: 'assistant' },
    });
    let snapshot = store.snapshot();
    let liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    let cardA = cardOf(liveRun!, 'm1');
    let cardB = cardOf(liveRun!, 'm2');
    check('same turn id across runners stays isolated', attemptOf(cardA)?.turnId === 'turn-1'
      && attemptOf(cardB)?.turnId === 'turn-1'
      && cardA !== cardB);
    check('turn_started binds the model label', attemptOf(cardA)?.modelLabel === 'model-a'
      && attemptOf(cardB)?.modelLabel === 'model-b');
    check('stream frame paints the card transcript', attemptOf(cardA)?.transcript.items.some((item) => item.id === 'a-msg-1') === true);
    check('runner b card has its own transcript', attemptOf(cardB)?.transcript.items.some((item) => item.id === 'b-msg-1') === true
      && !attemptOf(cardB)?.transcript.items.some((item) => item.id === 'a-msg-1'));

    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'succeeded' });
    snapshot = store.snapshot();
    liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    cardA = cardOf(liveRun!, 'm1');
    cardB = cardOf(liveRun!, 'm2');
    check('exact terminal settles only its card', attemptOf(cardA)?.status === 'succeeded'
      && attemptOf(cardB)?.status === 'streaming');
    check('run status is running while a sibling streams', deriveBroadcastRunStatus(liveRun!) === 'running');

    store.applyRunnerEvent('b', { type: 'turn_terminal', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-1', status: 'succeeded' });
    snapshot = store.snapshot();
    liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('all succeeded completes the run', deriveBroadcastRunStatus(liveRun!) === 'completed');
    check('completed run clears the active run id', channelOf(snapshot, 'p1')?.activeBroadcastRunId === null);
    const log = channelOf(snapshot, 'p1')?.channelMessages ?? [];
    check('succeeded assistant text lands on the shared log', log.length === 2
      && log[0]?.kind === 'human'
      && log[1]?.kind === 'runner'
      && log[1]?.author === 'Runner-a'
      && log[1]?.text === 'streamed reply'
      && log[1]?.mappingId === 'm1');
    store.dispose();
  }

  activeBlock = 'partial failure and run status mixture';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', 'ship it');
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'failed' });
    store.applyRunnerEvent('b', { type: 'turn_terminal', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-2', status: 'succeeded' });
    const snapshot = store.snapshot();
    const liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('failed terminal marks the card failed', attemptOf(cardOf(liveRun!, 'm1'))?.status === 'failed');
    check('sibling success is untouched', attemptOf(cardOf(liveRun!, 'm2'))?.status === 'succeeded');
    check('mixed terminal is partial', deriveBroadcastRunStatus(liveRun!) === 'partial');
    store.dispose();
  }

  activeBlock = 'second send blocked during a nonterminal run';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [activeMapping('m1', 'p1', 'a', 's1', 1)];
    reconcile(store, projects, mappings, views);
    const first = store.sendBroadcast('p1', 'first');
    let blocked = false;
    try {
      await store.sendBroadcast('p1', 'second');
    } catch {
      blocked = true;
    }
    check('second send is blocked while a run is nonterminal', blocked);
    await first;
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'succeeded' });
    const second = await store.sendBroadcast('p1', 'second');
    check('second send proceeds after the run settles', second.prompt === 'second');
    store.dispose();
  }

  activeBlock = 'cancel target with exact turn guard';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', 'ship it');
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_started', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1' });
    store.applyRunnerEvent('b', { type: 'turn_started', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-2' });
    await store.cancelBroadcastTarget('p1', run.id, 'm1');
    const aborts = client.calls.filter((call) => call.command.type === 'abort');
    check('cancel sends exactly one abort for the target', aborts.length === 1);
    check('abort routes the exact mapped session', aborts[0]?.options.sourceSessionId === 's1');
    check('abort carries a stable command id', (aborts[0]?.options.commandId ?? '').length > 0);
    let snapshot = store.snapshot();
    let liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('cancel marks the target cancelling', attemptOf(cardOf(liveRun!, 'm1'))?.status === 'cancelling');
    check('sibling card is untouched', attemptOf(cardOf(liveRun!, 'm2'))?.status === 'streaming');
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'aborted' });
    snapshot = store.snapshot();
    liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('aborted terminal settles the card cancelled', attemptOf(cardOf(liveRun!, 'm1'))?.status === 'cancelled');
    store.dispose();
  }

  activeBlock = 'cancel rechecks exact turn at send boundary';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [activeMapping('m1', 'p1', 'a', 's1', 1)];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', 'ship it');
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_started', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1' });
    client.beforeAbortSend = () => {
      store.applyRunnerEvent('a', { type: 'turn_started', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'newer-turn' });
    };
    await store.cancelBroadcastTarget('p1', run.id, 'm1');
    const aborts = client.calls.filter((call) => call.command.type === 'abort');
    const snapshot = store.snapshot();
    const liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('stale abort is rejected before the wire send', aborts.length === 0);
    check('ownership race marks only the old attempt interrupted', attemptOf(cardOf(liveRun!, 'm1'))?.status === 'interrupted'
      && attemptOf(cardOf(liveRun!, 'm1'))?.failureKind === 'protocol');
    store.dispose();
  }

  activeBlock = 'cancel remaining is independent';
  {
    const views = [runner('a'), runner('b'), runner('c')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
      activeMapping('m3', 'p1', 'c', 's3', 3),
    ];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', 'ship it');
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_started', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1' });
    store.applyRunnerEvent('b', { type: 'turn_started', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-2' });
    store.applyRunnerEvent('c', { type: 'turn_started', sessionId: 's3', runtimeIncarnation: incarnationOf('s3'), turnId: 'turn-3' });
    await store.cancelBroadcast('p1', run.id);
    const aborts = client.calls.filter((call) => call.command.type === 'abort');
    check('cancel remaining aborts every nonterminal card', aborts.length === 3
      && aborts.map((call) => call.options.sourceSessionId).sort().join(',') === 's1,s2,s3');
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'aborted' });
    store.applyRunnerEvent('b', { type: 'turn_terminal', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-2', status: 'aborted' });
    store.applyRunnerEvent('c', { type: 'turn_terminal', sessionId: 's3', runtimeIncarnation: incarnationOf('s3'), turnId: 'turn-3', status: 'aborted' });
    const snapshot = store.snapshot();
    const liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    check('all cancelled settles the run cancelled', deriveBroadcastRunStatus(liveRun!) === 'cancelled');
    store.dispose();
  }

  activeBlock = 'retry failed target';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [activeMapping('m1', 'p1', 'a', 's1', 1)];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', 'ship it');
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'failed' });
    await store.retryBroadcastTarget('p1', run.id, 'm1');
    const snapshot = store.snapshot();
    const liveRun = channelOf(snapshot, 'p1')?.broadcastRuns.find((row) => row.id === run.id);
    const card = cardOf(liveRun!, 'm1');
    check('retry appends a new attempt inside the same card', card?.attempts.length === 2);
    check('prior attempt is immutable', card?.attempts[0]?.status === 'failed');
    check('retry attempt is streaming with a new turn', attemptOf(card)?.status === 'streaming'
      && attemptOf(card)?.turnId === 'turn-2');
    check('retry uses a new prompt command id', card?.attempts[0]?.promptCommandId !== card?.attempts[1]?.promptCommandId);
    store.dispose();
  }

  activeBlock = 'bounded retained runs';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [activeMapping('m1', 'p1', 'a', 's1', 1)];
    reconcile(store, projects, mappings, views);
    for (let index = 0; index < MAX_BROADCAST_RUNS_PER_PROJECT + 3; index += 1) {
      await store.sendBroadcast('p1', `run ${index}`);
      await flush();
      store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: `turn-${index + 1}`, status: 'succeeded' });
    }
    const snapshot = store.snapshot();
    const channel = channelOf(snapshot, 'p1');
    check('retained runs are bounded', (channel?.broadcastRuns ?? []).length <= MAX_BROADCAST_RUNS_PER_PROJECT);
    check('newest runs are retained', (channel?.broadcastRuns.at(-1)?.prompt ?? '') === `run ${MAX_BROADCAST_RUNS_PER_PROJECT + 2}`);
    store.dispose();
  }

  activeBlock = 'channel mentions and targeted broadcast';
  {
    check('mention token collapses spaces', runnerMentionToken('Build runner') === 'Build-runner');
    check('plain draft is the team', resolveChannelRecipients('ship it', [
      { mappingId: 'm1', mention: 'alpha' },
      { mappingId: 'm2', mention: 'beta' },
    ]).kind === 'team');
    const mentioned = resolveChannelRecipients('@alpha ship it', [
      { mappingId: 'm1', mention: 'alpha' },
      { mappingId: 'm2', mention: 'beta' },
    ]);
    check('at-mention selects that runner', mentioned.kind === 'mentioned'
      && JSON.stringify(mentioned) === JSON.stringify({ kind: 'mentioned', mappingIds: ['m1'] }));
    check('unknown mention is unmatched', resolveChannelRecipients('@ghost hi', [
      { mappingId: 'm1', mention: 'alpha' },
    ]).kind === 'unmatched');
    check('trailing query after at', trailingMentionQuery('hello @al') === 'al');
    check('insert mention replaces the query', insertMentionToken('hello @al', 'alpha') === 'hello @alpha ');

    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    let rejected = '';
    try {
      await store.sendBroadcast('p1', 'no one', []);
    } catch (error) {
      rejected = error instanceof Error ? error.message : String(error);
    }
    check('empty recipient list is rejected', rejected === 'Broadcast recipients are required.');
    const run = await store.sendBroadcast('p1', '@alpha do this', ['m1']);
    check('targeted broadcast fans out to the named mapping only', run.targetMappingIds.join(',') === 'm1'
      && run.responses.length === 1);
    const promptCalls = client.calls.filter((call) => call.command.type === 'prompt');
    check('targeted broadcast prompts only that session', promptCalls.length === 1
      && promptCalls[0]?.options.sourceSessionId === 's1');
    check('targeted wake brief still names self not the raw mention string alone', promptText(promptCalls[0]!).includes('You are @Runner-a in #Project-p1.')
      && promptText(promptCalls[0]!).includes('You: @alpha do this'));
    store.dispose();
  }

  activeBlock = 'shared channel log wake brief and one-round handoff';
  {
    const only = composeWakeBrief({
      channelLabel: 'Team Room',
      selfMention: 'alpha',
      teammateMentions: [],
      messages: [],
    });
    check('solo wake names the channel and self', only.includes('You are @alpha in #Team-Room.')
      && only.includes('You are the only runner in this channel.')
      && only.includes('Reply in this channel. @ a teammate to hand off work.'));
    const withLog = composeWakeBrief({
      channelLabel: 'Team Room',
      selfMention: 'alpha',
      teammateMentions: ['beta'],
      messages: [
        { id: '1', at: 1, kind: 'human', mappingId: null, author: 'You', text: 'plan this' },
        { id: '2', at: 2, kind: 'runner', mappingId: 'm1', author: 'alpha', text: '@beta take the files' },
      ],
    });
    check('wake brief lists teammates and recent log', withLog.includes('Teammates: @beta.')
      && withLog.includes('You: plan this')
      && withLog.includes('@alpha: @beta take the files'));

    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    const run = await store.sendBroadcast('p1', '@Runner-a draft it', ['m1']);
    await flush();
    store.applyRunnerEvent('a', { type: 'turn_started', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1' });
    store.applyRunnerEvent('a', {
      type: 'message_start',
      sessionId: 's1', runtimeIncarnation: incarnationOf('s1'),
      turnId: 'turn-1',
      itemId: 'a-msg-1',
      message: { role: 'assistant' },
    });
    store.applyRunnerEvent('a', {
      type: 'message_update',
      sessionId: 's1', runtimeIncarnation: incarnationOf('s1'),
      turnId: 'turn-1',
      itemId: 'a-msg-1',
      assistantMessageEvent: { type: 'text_delta', delta: '@Runner-b take the files' },
    });
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'succeeded' });
    await flush();
    const promptCalls = client.calls.filter((call) => call.command.type === 'prompt');
    check('handoff wakes the mentioned teammate once', promptCalls.length === 2
      && promptCalls[0]?.options.sourceSessionId === 's1'
      && promptCalls[1]?.options.sourceSessionId === 's2');
    check('handoff does not append a second You line', (channelOf(store.snapshot(), 'p1')?.channelMessages.filter((row) => row.kind === 'human').length ?? 0) === 1);
    check('handoff brief includes the runner reply', promptText(promptCalls[1]!).includes('@Runner-a: @Runner-b take the files')
      && promptText(promptCalls[1]!).includes('You are @Runner-b in #Project-p1.'));
    const snapshot = store.snapshot();
    const runs = channelOf(snapshot, 'p1')?.broadcastRuns ?? [];
    check('handoff is a second run not a re-fan of the first', runs.length === 2
      && runs[0]?.id === run.id
      && runs[0]?.isHandoff === false
      && runs[1]?.isHandoff === true
      && runs[1]?.targetMappingIds.join(',') === 'm2');

    store.applyRunnerEvent('b', { type: 'turn_started', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-2' });
    store.applyRunnerEvent('b', {
      type: 'message_start',
      sessionId: 's2', runtimeIncarnation: incarnationOf('s2'),
      turnId: 'turn-2',
      itemId: 'b-msg-1',
      message: { role: 'assistant' },
    });
    store.applyRunnerEvent('b', {
      type: 'message_update',
      sessionId: 's2', runtimeIncarnation: incarnationOf('s2'),
      turnId: 'turn-2',
      itemId: 'b-msg-1',
      assistantMessageEvent: { type: 'text_delta', delta: '@Runner-a look at this too' },
    });
    store.applyRunnerEvent('b', { type: 'turn_terminal', sessionId: 's2', runtimeIncarnation: incarnationOf('s2'), turnId: 'turn-2', status: 'succeeded' });
    await flush();
    check('handoff run does not start a third wake', client.calls.filter((call) => call.command.type === 'prompt').length === 2);
    store.dispose();
  }

  activeBlock = 'channel log reloads from page-authority storage';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const storeMap: Record<string, string> = {};
    const memory = {
      getItem: (name: string) => (name in storeMap ? storeMap[name] : null),
      setItem: (name: string, value: string) => { storeMap[name] = value; },
      removeItem: (name: string) => { delete storeMap[name]; },
    };
    const authority = 'chat.example';
    const store = new TeamStore({ runners: client, storage: memory, pageAuthority: authority });
    reconcile(store, [project('p1')], [activeMapping('m1', 'p1', 'a', 's1', 1)], views);
    await store.sendBroadcast('p1', 'keep this line');
    await flush();
    store.applyRunnerEvent('a', {
      type: 'message_start',
      sessionId: 's1', runtimeIncarnation: incarnationOf('s1'),
      turnId: 'turn-1',
      itemId: 'a-msg-1',
      message: { role: 'assistant' },
    });
    store.applyRunnerEvent('a', {
      type: 'message_update',
      sessionId: 's1', runtimeIncarnation: incarnationOf('s1'),
      turnId: 'turn-1',
      itemId: 'a-msg-1',
      assistantMessageEvent: { type: 'text_delta', delta: 'held the line' },
    });
    store.applyRunnerEvent('a', { type: 'turn_terminal', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), turnId: 'turn-1', status: 'succeeded' });
    const raw = memory.getItem(channelLogStorageKey(authority)) ?? '';
    check('log is stored under the page authority key', raw.includes('"keep this line"') && raw.includes('"held the line"'));
    check('stored log does not include session ids', !raw.includes('"s1"') && !raw.includes('sessionId'));
    store.dispose();

    const restored = new TeamStore({ runners: client, storage: memory, pageAuthority: authority });
    reconcile(restored, [project('p1')], [activeMapping('m1', 'p1', 'a', 's1', 1)], views);
    const log = channelOf(restored.snapshot(), 'p1')?.channelMessages ?? [];
    check('reload restores the human and runner lines', log.length === 2
      && log[0]?.kind === 'human'
      && log[0]?.text === 'keep this line'
      && log[1]?.kind === 'runner'
      && log[1]?.text === 'held the line');
    restored.dispose();

    check('other authority does not see the log', Object.keys(loadChannelLogs(memory, 'other.example').logs).length === 0);
    const skippedVersion = loadChannelLogs({
      getItem: () => '{"version":2,"projects":{"p1":[]}}',
      setItem: () => {},
      removeItem: () => {},
    }, authority);
    check('unknown log version is empty without error', skippedVersion.ok && Object.keys(skippedVersion.logs).length === 0);
    const corrupt = loadChannelLogs({
      getItem: () => '{',
      setItem: () => {},
      removeItem: () => {},
    }, authority);
    check('corrupt log document propagates', !corrupt.ok && corrupt.error.kind === 'corrupt' && Object.keys(corrupt.logs).length === 0);
    const nonArrayProject = loadChannelLogs({
      getItem: () => JSON.stringify({ version: 1, projects: { p1: { id: 'msg-1' } } }),
      setItem: () => {},
      removeItem: () => {},
    }, authority);
    check('non-array project log is corrupt', !nonArrayProject.ok && nonArrayProject.error.kind === 'corrupt');
    const malformedRow = loadChannelLogs({
      getItem: () => JSON.stringify({ version: 1, projects: { p1: [{ id: 'msg-1' }] } }),
      setItem: () => {},
      removeItem: () => {},
    }, authority);
    check('malformed stored message row is corrupt', !malformedRow.ok && malformedRow.error.kind === 'corrupt');
    const readIo = loadChannelLogs({
      getItem: () => { throw new Error('blocked'); },
      setItem: () => {},
      removeItem: () => {},
    }, authority);
    check('unexpected log read errors propagate', !readIo.ok && readIo.error.kind === 'io');
    const quotaError = Object.assign(new Error('quota'), { name: 'QuotaExceededError' });
    const quotaSave = saveChannelLogs({
      getItem: () => null,
      setItem: () => { throw quotaError; },
      removeItem: () => {},
    }, authority, { p1: log });
    check('quota save errors propagate', !quotaSave.ok && quotaSave.error.kind === 'quota');
    const ioSave = saveChannelLogs({
      getItem: () => null,
      setItem: () => { throw new Error('disk'); },
      removeItem: () => {},
    }, authority, { p1: log });
    check('unexpected save errors propagate', !ioSave.ok && ioSave.error.kind === 'io');
    let threw = false;
    try {
      saveChannelLogs({
        getItem: () => { throw new Error('blocked'); },
        setItem: () => { throw new Error('quota'); },
        removeItem: () => { throw new Error('quota'); },
      }, authority, { p1: log });
    } catch {
      threw = true;
    }
    check('unavailable log storage save does not throw', threw === false);

    const quotaStoreMap: Record<string, string> = {};
    const quotaMemory = {
      getItem: (name: string) => (name in quotaStoreMap ? quotaStoreMap[name] : null),
      setItem: (_name: string, _value: string) => { throw quotaError; },
      removeItem: (name: string) => { delete quotaStoreMap[name]; },
    };
    const quotaStore = new TeamStore({ runners: client, storage: quotaMemory, pageAuthority: authority });
    reconcile(quotaStore, [project('p1')], [activeMapping('m1', 'p1', 'a', 's1', 1)], views);
    await quotaStore.sendBroadcast('p1', 'keep this line');
    const quotaChannel = channelOf(quotaStore.snapshot(), 'p1')?.channelMessages ?? [];
    check('save failure keeps last-good in-memory log', quotaChannel.length === 1 && quotaChannel[0]?.text === 'keep this line');
    check('save failure surfaces channel log error', quotaStore.snapshot().channelLogError === channelLogErrorMessage({ kind: 'quota' }));
    quotaStore.dispose();

    const corruptStore = new TeamStore({
      runners: client,
      storage: {
        getItem: () => '{',
        setItem: () => {},
        removeItem: () => {},
      },
      pageAuthority: authority,
    });
    check('corrupt load surfaces channel log error', corruptStore.snapshot().channelLogError === channelLogErrorMessage({ kind: 'corrupt' }));
    corruptStore.dispose();
  }

  activeBlock = 'live channel messages are char-bounded';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    reconcile(store, [project('p1')], [activeMapping('m1', 'p1', 'a', 's1', 1)], views);
    const long = 'x'.repeat(MAX_CHANNEL_MESSAGE_CHARS + 400);
    await store.sendBroadcast('p1', long);
    const text = channelOf(store.snapshot(), 'p1')?.channelMessages[0]?.text ?? '';
    check('oversized live message is truncated', text.length === MAX_CHANNEL_MESSAGE_CHARS + 2 && text.endsWith('\n…'));
    store.dispose();
  }

  activeBlock = 'subscribeSession exact-session fan-out';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [activeMapping('m1', 'p1', 'a', 's1', 1)];
    reconcile(store, projects, mappings, views);
    const received: unknown[] = [];
    const unsubscribe = store.subscribeSession('a', 's1', (frame) => { received.push(frame); });
    store.applyRunnerEvent('a', { type: 'message_start', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), itemId: 'x1', message: { role: 'assistant' } });
    check('exact-session listener receives the frame', received.length === 1);
    store.applyRunnerEvent('a', { type: 'message_start', sessionId: 'other', runtimeIncarnation: incarnationOf('other'), itemId: 'x2', message: { role: 'assistant' } });
    check('other-session frames are not fanned out', received.length === 1);
    unsubscribe();
    store.applyRunnerEvent('a', { type: 'message_start', sessionId: 's1', runtimeIncarnation: incarnationOf('s1'), itemId: 'x3', message: { role: 'assistant' } });
    check('unsubscribed listener is silent', received.length === 1);
    store.dispose();
  }

  activeBlock = 'one runner event callback per mapped runner';
  {
    const views = [runner('a'), runner('b')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [
      activeMapping('m1', 'p1', 'a', 's1', 1),
      activeMapping('m2', 'p1', 'b', 's2', 2),
    ];
    reconcile(store, projects, mappings, views);
    reconcile(store, projects, mappings, views);
    const snapshot = store.snapshot();
    check('reconcile does not duplicate sessions', Object.keys(snapshot.sessions).length === 2);
    check('session key is collision-safe', snapshot.sessions[runnerSessionKey('a', 's1')]?.sessionId === 's1');
    store.dispose();
  }

  activeBlock = 'dispose releases listeners';
  {
    const views = [runner('a')];
    const client = new FakeRunnerClient(views);
    const store = new TeamStore({ runners: client });
    const projects = [project('p1')];
    const mappings = [activeMapping('m1', 'p1', 'a', 's1', 1)];
    reconcile(store, projects, mappings, views);
    store.dispose();
    const snapshot = store.snapshot();
    check('dispose clears sessions', Object.keys(snapshot.sessions).length === 0);
    check('dispose clears channels', Object.keys(snapshot.channels ?? {}).length === 0);
  }

  activeBlock = 'channel composer slash + gear';
  {
    const here = dirname(fileURLToPath(import.meta.url));
    const channel = readFileSync(join(here, '../src/chat/BroadcastConversation.tsx'), 'utf8');
    const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
    check('Channel composer mounts CommandPicker', channel.includes('<CommandPicker') && channel.includes('className="chat-session__composer"'));
    check('Channel submit rejects /goal', channel.includes("slash.name === 'goal'")
      && channel.includes("'Open a runner session and use /goal there.'"));
    check('Channel submit rejects other slashes', channel.includes("`/${slash.name} is not available in chat.`"));
    check('Channel slash check runs before onSend', channel.includes('const slash = parseSlashLine(message);')
      && channel.indexOf('parseSlashLine(message)') < channel.indexOf('await onSend('));
    check('Channel rejects /clear like other slashes', channel.includes('parseSlashLine(message)')
      && channel.includes("`/${slash.name} is not available in chat.`")
      && !channel.includes('parseSupportedCommand'));
    check('ChatApp gear container is a dialog', app.includes('id="chat-project-gear"')
      && app.includes('role="dialog"')
      && app.includes('aria-label="Project settings"')
      && !app.includes('id="chat-project-gear" role="menu"'));

  }

  console.log(`\nchatChannels.test: ${assertions} assertions, ${failures.length} failure(s)`);
  if (failures.length > 0) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
  process.exit(0);
}

main().catch((error) => {
  console.error(`chatChannels.test (${activeBlock}): ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});
